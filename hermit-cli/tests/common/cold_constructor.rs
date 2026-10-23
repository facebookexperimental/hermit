//! Additional actual Detcore constructor-placement consumer. The existing M2
//! workload and comparator remain required. Samples below attest placement;
//! they do not claim caller-window equality or universal first-entry isolation.

use serde::Deserialize;
use serde::Serialize;

use super::*;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Record {
    layout_version: u64,
    phase: u64,
    owner_tid: u64,
    extent_start: u64,
    extent_end: u64,
    bottom: u64,
    top: u64,
    caller_rsp: u64,
    adoption_entry: u64,
    body_entry: u64,
    first_rust_sample_rsp: u64,
    body_call_sample_rsp: u64,
    control_address: u64,
    record_address: u64,
    region_base: u64,
    region_end: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Observation {
    schema: u64,
    owner: String,
    owner_base: u64,
    query_result: i32,
    record: Record,
}

fn symbol_address(bytes: &[u8], name: &str, bias: u64) -> Result<u64> {
    let elf = goblin::elf::Elf::parse(bytes).map_err(|e| e.to_string())?;
    let definitions: std::collections::BTreeSet<_> = elf
        .syms
        .iter()
        .filter(|symbol| elf.strtab.get_at(symbol.st_name) == Some(name))
        .filter(|symbol| {
            symbol.st_type() == goblin::elf::sym::STT_FUNC
                && symbol.st_size != 0
                && symbol.st_shndx != goblin::elf::section_header::SHN_UNDEF as usize
        })
        .map(|symbol| (symbol.st_value, symbol.st_size))
        .collect();
    require(
        definitions.len() == 1,
        "cold entry symbol is absent or ambiguous",
    )?;
    let (address, size) = *definitions.first().ok_or("cold symbol absent")?;
    require(
        elf.program_headers.iter().any(|segment| {
            segment.p_type == goblin::elf::program_header::PT_LOAD
                && segment.is_executable()
                && address >= segment.p_vaddr
                && address.checked_add(size).is_some_and(|end| {
                    segment
                        .p_vaddr
                        .checked_add(segment.p_filesz)
                        .is_some_and(|top| end <= top)
                })
        }),
        "cold entry has no complete executable file-backed interval",
    )?;
    bias.checked_add(address)
        .ok_or_else(|| "cold entry address overflow".into())
}

fn competing_constructor_export(bytes: &[u8]) -> Result<artifact::SymbolEvidence> {
    use goblin::elf::header;
    use goblin::elf::program_header;
    use goblin::elf::section_header;
    use goblin::elf::sym;

    let elf = goblin::elf::Elf::parse(bytes).map_err(|e| e.to_string())?;
    require(
        elf.is_64
            && elf.little_endian
            && elf.header.e_machine == header::EM_X86_64
            && matches!(elf.header.e_type, header::ET_EXEC | header::ET_DYN),
        "competing constructor guest is not an x86-64 executable",
    )?;
    let mut definitions = elf.dynsyms.iter().filter(|symbol| {
        elf.dynstrtab.get_at(symbol.st_name) == Some("detcore_liteinst_initialize")
    });
    let symbol = definitions
        .next()
        .ok_or("actual cold guest has no competing dynamic constructor export")?;
    require(
        definitions.next().is_none()
            && symbol.st_bind() == sym::STB_GLOBAL
            && symbol.st_type() == sym::STT_FUNC
            && symbol.st_visibility() == sym::STV_DEFAULT
            && symbol.st_shndx != section_header::SHN_UNDEF as usize
            && symbol.st_size != 0,
        "actual competing constructor export is not unique defined GLOBAL/DEFAULT FUNC",
    )?;
    let section = elf
        .section_headers
        .get(symbol.st_shndx)
        .ok_or("actual competing constructor section is absent")?;
    let end = symbol
        .st_value
        .checked_add(symbol.st_size)
        .ok_or("actual competing constructor extent overflow")?;
    require(
        section.sh_type == section_header::SHT_PROGBITS
            && section.sh_flags & u64::from(section_header::SHF_EXECINSTR) != 0
            && symbol.st_value >= section.sh_addr
            && section
                .sh_addr
                .checked_add(section.sh_size)
                .is_some_and(|top| end <= top),
        "actual competing constructor has no complete executable section",
    )?;
    let section_offset = section
        .sh_offset
        .checked_add(symbol.st_value - section.sh_addr)
        .ok_or("actual competing constructor file offset overflow")?;
    let mut mappings = elf.program_headers.iter().filter(|segment| {
        segment.p_type == program_header::PT_LOAD
            && segment.is_executable()
            && symbol.st_value >= segment.p_vaddr
            && segment
                .p_vaddr
                .checked_add(segment.p_filesz)
                .is_some_and(|top| end <= top)
    });
    let mapping = mappings
        .next()
        .ok_or("actual competing constructor has no executable file backing")?;
    let offset = mapping
        .p_offset
        .checked_add(symbol.st_value - mapping.p_vaddr)
        .ok_or("actual competing constructor loaded offset overflow")?;
    let file_end = offset
        .checked_add(symbol.st_size)
        .ok_or("actual competing constructor file extent overflow")?;
    require(
        mappings.next().is_none()
            && offset == section_offset
            && usize::try_from(offset)
                .ok()
                .zip(usize::try_from(file_end).ok())
                .is_some_and(|(start, end)| bytes.get(start..end).is_some()),
        "actual competing constructor file identity is ambiguous or truncated",
    )?;
    Ok(artifact::SymbolEvidence {
        name: "detcore_liteinst_initialize".into(),
        address: symbol.st_value,
        size: symbol.st_size,
        binding: symbol.st_bind(),
        visibility: symbol.st_visibility(),
    })
}

fn verify_record(record: &Record, stacks: &stack_observation::Query) -> Result<()> {
    use stack_observation::CALLBACK_BYTES;
    use stack_observation::CONTROL_BYTES;
    use stack_observation::PAGE_BYTES;
    use stack_observation::REGION_BASE;
    use stack_observation::REGION_END;
    let extent_start = REGION_BASE + CONTROL_BYTES;
    let bottom = extent_start + PAGE_BYTES;
    let top = bottom + CALLBACK_BYTES;
    let extent_end = top + PAGE_BYTES;
    require(
        record.layout_version == 1
            && record.phase == 4
            && record.owner_tid != 0
            && record.owner_tid == stacks.current_tid,
        "actual cold owner did not complete on this thread",
    )?;
    require(
        record.region_base == REGION_BASE
            && record.region_end == REGION_END
            && record.extent_start == extent_start
            && record.extent_end == extent_end
            && record.bottom == bottom
            && record.top == top
            && record.control_address == REGION_BASE + PAGE_BYTES
            && record.record_address >= record.control_address
            && record.record_address.is_multiple_of(8)
            && record
                .record_address
                .checked_add(128)
                .is_some_and(|end| end <= extent_start),
        "cold owner layout differs from the fixed guarded bootstrap extent",
    )?;
    require(
        (bottom..top).contains(&record.first_rust_sample_rsp)
            && (bottom..top).contains(&record.body_call_sample_rsp),
        "actual constructor stack sample is outside its bootstrap lease",
    )?;
    require(
        record.caller_rsp != 0
            && record.caller_rsp % 16 == 8
            && !(REGION_BASE..REGION_END).contains(&record.caller_rsp),
        "cold caller stack is absent, misaligned or aliases Tool storage",
    )?;
    for (start, end) in [
        (
            stacks.alt_sp.checked_sub(PAGE_BYTES),
            stacks
                .alt_sp
                .checked_add(stacks.alt_size)
                .and_then(|n| n.checked_add(PAGE_BYTES)),
        ),
        (
            stacks.continuation_bottom.checked_sub(PAGE_BYTES),
            stacks.continuation_top.checked_add(PAGE_BYTES),
        ),
    ] {
        let (start, end) = start.zip(end).ok_or("later stack extent overflow")?;
        require(
            start < end && (end <= extent_start || start >= extent_end),
            "later stack or guard aliases the retained bootstrap extent",
        )?;
    }
    Ok(())
}

fn required_cold_bundle(root: &Path) -> Result<cold_bundle::Bundle> {
    let pointer = std::env::var_os("HERMIT_COLD_STACK_FIXTURE_BUNDLE")
        .ok_or("required HERMIT_COLD_STACK_FIXTURE_BUNDLE pointer is absent")?;
    let manifest =
        fs::read_to_string(artifact::real_file(Path::new(&pointer))?).map_err(|e| e.to_string())?;
    let bundle: cold_bundle::Bundle = serde_json::from_slice(
        &fs::read(artifact::real_file(Path::new(manifest.trim()))?).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("typed constructor fixture bundle: {e}"))?;
    require(
        bundle.schema == 1
            && bundle.executed_tests == 0
            && !bundle.full_constructor_byte_isolation_claimed,
        "cold producer may not claim execution or byte isolation",
    )?;
    let stacks = required_bundle()?;
    require(
        serde_json::to_value(&bundle.stacks).map_err(|e| e.to_string())?
            == serde_json::to_value(&stacks).map_err(|e| e.to_string())?,
        "cold and required M2 bundles must name the same qualified leaves",
    )?;
    require(
        bundle.guest_source_sha256
            == artifact::sha256(include_bytes!("../fixtures/liteinst_constructor.c"))
            && bundle.consumer_sha256 == artifact::sha256(include_bytes!("cold_constructor.rs"))
            && bundle.consumer_sha256
                == artifact::file_sha256(
                    &root.join("hermit-cli/tests/common/cold_constructor.rs"),
                )?,
        "cold fixture or comparator differs from its compiled source",
    )?;
    artifact::verify_guest_receipt(
        &bundle.guest_receipt,
        &bundle.guest,
        &root.join("hermit-cli/tests/fixtures/liteinst_constructor.c"),
        &bundle.guest_source_sha256,
    )?;
    Ok(bundle)
}

#[test]
fn genuine_detcore_constructor_uses_retained_disjoint_bootstrap_stack() -> Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("H root absent")?;
    let bundle = required_cold_bundle(root)?;
    let parent = root.join("target/ci/cold-stack-results/build");
    fs::create_dir_all(&parent).map_err(|e| e.to_string())?;
    let evidence = tempfile::Builder::new()
        .prefix("detcore-constructor-")
        .tempdir_in(&parent)
        .map_err(|e| e.to_string())?
        .keep();
    eprintln!(
        "Detcore constructor placement evidence {}",
        evidence.display()
    );
    source_guard(&bundle.stacks.allocator, root, &evidence, "before")?;
    let result = (|| -> Result<()> {
        let (leaf, receipt) = qualify(&bundle.stacks.allocator, false, &evidence, "before")?;
        let guest = FileIdentity::read(&bundle.guest)?;
        let guest_bytes = fs::read(&guest.path).map_err(|e| e.to_string())?;
        require(
            artifact::sha256(&guest_bytes) == guest.sha256,
            "competing export was not read from the held actual guest",
        )?;
        let competing_export = competing_constructor_export(&guest_bytes)?;
        let detcore = receipt
            .qualification
            .detcore_constructor
            .as_ref()
            .ok_or("qualified cold Detcore runtime lacks actual constructor evidence")?;
        let runtime_bytes = fs::read(&leaf.runtime).map_err(|e| e.to_string())?;
        require(
            artifact::sha256(&runtime_bytes) == receipt.artifact.sha256
                && detcore.protected_initializer.size == 16
                && detcore.protected_initializer.binding == goblin::elf::sym::STB_GLOBAL
                && detcore.protected_initializer.visibility == goblin::elf::sym::STV_PROTECTED,
            "cold Detcore evidence is not bound to the actual protected image",
        )?;
        for (role, name) in [
            (
                &detcore.protected_initializer,
                "detcore_liteinst_initialize",
            ),
            (&detcore.body, "detcore_liteinst_initialize_body"),
            (&detcore.bridge, "reverie_inguest_constructor_entry"),
            (&detcore.prepare, "reverie_inguest_constructor_adopt_entry"),
        ] {
            require(
                role.name == name && role.address == symbol_address(&runtime_bytes, name, 0)?,
                "cold Detcore evidence does not identify its actual root/body/bridge/adoption",
            )?;
        }
        artifact::write_new(
            &evidence.join("constructor-preconditions.json"),
            &serde_json::to_vec_pretty(&json!({
                "guest_sha256":guest.sha256,"competing_export":competing_export,
                "runtime_sha256":receipt.artifact.sha256,"detcore_constructor":detcore,
                "execution_claimed":false,
            }))
            .map_err(|e| e.to_string())?,
        )?;
        let executable = FileIdentity::read(hermit_binary::hermit_binary())?;
        let cwd = bundle.guest.parent().ok_or("cold guest directory absent")?;
        let mut command = Command::new(&executable.path);
        command
            .current_dir(cwd)
            .args([
                "--log=warn",
                "--backend=in-guest-trap",
                "run",
                &format!("--epoch={EPOCH}"),
                "--seed=0",
                "--sched-seed=0",
                "--max-timeslice=disabled",
                "--strict",
                "--base-env=minimal",
                "--env=LD_BIND_NOW=1",
                "--env=REVERIE_LITEINST_ALT_STACK=1",
                "--env=REVERIE_LITEINST_SITE_PATCHING=0",
            ])
            .arg(format!("--workdir={}", cwd.display()))
            .arg("--")
            .arg(&bundle.guest)
            .arg("observe")
            .arg(&leaf.runtime);
        let held = HeldInputs {
            environment: vec![
                (
                    "PATH".into(),
                    std::env::var_os("PATH").ok_or("pinned PATH absent")?,
                ),
                ("LC_ALL".into(), "C".into()),
                ("LD_BIND_NOW".into(), "1".into()),
                (
                    "HERMIT_LITEINST_TOOL_RUNTIME".into(),
                    leaf.runtime.as_os_str().to_owned(),
                ),
            ],
            policy: serde_json::to_vec(&json!({"runtime_sha256":receipt.artifact.sha256,
                "guest_sha256":guest.sha256,"executable_sha256":executable.sha256,
                "epoch":EPOCH,"seed":0,"sched_seed":0,"site_patching":false,
                "claim":"actual constructor samples and stack nonaliasing only",
                "caller_window_equality_claimed":false}))
            .map_err(|e| e.to_string())?,
        };
        let run = contract::run_without_input(command, &held).map_err(|failure| {
            for (i, capture) in failure.captures.iter().enumerate() {
                let _ = retain(&evidence, &format!("guest-failure-{i}"), capture);
            }
            failure.to_string()
        })?;
        retain(&evidence, "guest", &run)?;
        require(
            run.input.is_none()
                && run.status.is_some_and(|s| s.success())
                && run.end == contract::CaptureEnd::Complete
                && !run.output_truncated,
            "actual cold child did not complete; no placement credit",
        )?;
        let selection = b"hermit: [in-guest-trap] selected: the guest preload is to host the Detcore Tool, with syscall site patching off (REVERIE_LITEINST_SITE_PATCHING=0)";
        require(
            run.stderr.windows(selection.len()).any(|w| w == selection),
            "actual trap/site0 selection absent",
        )?;
        let mut lines = run.stdout.split_inclusive(|byte| *byte == b'\n');
        let stacks = stack_observation::parse(lines.next().ok_or("M2 observation absent")?)?;
        let cold: Observation =
            serde_json::from_slice(lines.next().ok_or("cold observation absent")?)
                .map_err(|e| e.to_string())?;
        require(
            lines.next().is_none(),
            "unexpected bytes after actual observations",
        )?;
        stack_observation::verify_observation(
            &stacks,
            &leaf.runtime,
            "observe",
            true,
            Continuation::Reached,
        )?;
        stack_observation::verify_fixed_stack_placement(&stacks, true, Continuation::Reached)?;
        require(
            cold.schema == 1
                && cold.query_result == 0
                && cold.owner_base != 0
                && leaf.runtime.to_str() == Some(cold.owner.as_str()),
            "cold query/owner failed",
        )?;
        let bytes = fs::read(&leaf.runtime).map_err(|e| e.to_string())?;
        require(
            cold.record.adoption_entry
                == symbol_address(
                    &bytes,
                    "reverie_inguest_constructor_adopt_entry",
                    cold.owner_base,
                )?
                && cold.record.body_entry
                    == symbol_address(&bytes, "detcore_liteinst_initialize_body", cold.owner_base)?,
            "cold record does not identify the actual linked adoption/body",
        )?;
        verify_record(&cold.record, &stacks.after)?;
        guest.verify()?;
        executable.verify()?;
        receipt.artifact.verify()?;
        qualify(&bundle.stacks.allocator, false, &evidence, "after")?;
        required_cold_bundle(root)?;
        artifact::write_new(&evidence.join("placement.json"), &serde_json::to_vec_pretty(&json!({
            "actual_child_exit":0,"sampled_placement_passed":true,"nonaliasing_passed":true,
            "record":cold.record,"actual_reached_rsp":stacks.after.reached_rsp,
            "actual_altstack":stacks.after.alt_sp,"full_constructor_byte_isolation_claimed":false,
            "exact_first_instruction_observed":false,"interior_write_protection_claimed":false,
        })).map_err(|e| e.to_string())?)?;
        Ok(())
    })();
    source_guard(&bundle.stacks.allocator, root, &evidence, "after")?;
    if let Err(error) = &result {
        artifact::write_new(&evidence.join("failure.json"), &serde_json::to_vec_pretty(&json!({
            "error":error,"placement_passed":false,"full_constructor_byte_isolation_claimed":false,
        })).map_err(|e| e.to_string())?)?;
    }
    result.map_err(|e| format!("{e}; constructor evidence {}", evidence.display()))
}

fn synthetic_control() -> (Record, stack_observation::Query) {
    use stack_observation::CALLBACK_BYTES;
    use stack_observation::CONTROL_BYTES;
    use stack_observation::PAGE_BYTES;
    use stack_observation::REGION_BASE;
    use stack_observation::REGION_END;
    let start = REGION_BASE + CONTROL_BYTES;
    let bottom = start + PAGE_BYTES;
    let top = bottom + CALLBACK_BYTES;
    let end = top + PAGE_BYTES;
    (
        Record {
            layout_version: 1,
            phase: 4,
            owner_tid: 3,
            extent_start: start,
            extent_end: end,
            bottom,
            top,
            caller_rsp: 0x7fff_ffff_ca08,
            adoption_entry: 1,
            body_entry: 2,
            first_rust_sample_rsp: top - 64,
            body_call_sample_rsp: top - 128,
            control_address: REGION_BASE + PAGE_BYTES,
            record_address: REGION_BASE + 256 * 1024,
            region_base: REGION_BASE,
            region_end: REGION_END,
        },
        stack_observation::Query {
            current_tid: 3,
            alt_sp: end + PAGE_BYTES,
            alt_size: 65536,
            continuation_bottom: end + 65536 + 3 * PAGE_BYTES,
            continuation_top: end + 65536 + 3 * PAGE_BYTES + CALLBACK_BYTES,
            ..Default::default()
        },
    )
}

#[test]
fn caller_stack_samples_never_receive_constructor_placement_credit() -> Result<()> {
    let (mut record, stacks) = synthetic_control();
    verify_record(&record, &stacks)?;
    // The genuine baseline's first Rust entry and post-prologue RSPs.
    record.first_rust_sample_rsp = 0x7fff_ffff_ca08;
    record.body_call_sample_rsp = 0x7fff_ffff_ad50;
    require(
        verify_record(&record, &stacks).err().as_deref()
            == Some("actual constructor stack sample is outside its bootstrap lease"),
        "baseline caller-stack samples were accepted",
    )
}

#[test]
fn later_guard_cannot_alias_the_retained_bootstrap_extent() -> Result<()> {
    let (record, mut stacks) = synthetic_control();
    verify_record(&record, &stacks)?;
    // Interiors are disjoint, but the next stack's lower guard overlaps the
    // bootstrap's upper guard. Full extents, not only live RSPs, must differ.
    stacks.alt_sp = record.extent_end;
    require(
        verify_record(&record, &stacks).err().as_deref()
            == Some("later stack or guard aliases the retained bootstrap extent"),
        "overlapping guard extent was accepted",
    )
}
