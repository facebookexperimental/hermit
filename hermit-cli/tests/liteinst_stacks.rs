//! Required fixed placement of the existing signal/continuation stacks.
//! These observations do not claim all-entry, TLS or interior write isolation.

#[allow(dead_code)]
#[path = "../../reverie/scripts/m1_artifact.rs"]
mod artifact;
#[path = "common/cold_bundle.rs"]
mod cold_bundle;
#[path = "common/cold_constructor.rs"]
mod cold_constructor;
#[allow(dead_code)]
#[path = "../../reverie/scripts/m1_contract.rs"]
mod contract;
#[allow(dead_code)]
#[path = "common/hermit_binary.rs"]
mod hermit_binary;
#[path = "common/m1_bundle.rs"]
mod m1_bundle;
#[path = "common/m2_bundle.rs"]
mod m2_bundle;
#[path = "../../reverie/reverie-liteinst/tests/support/stack_observation.rs"]
mod stack_observation;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::process::Command;

use artifact::ConsumerExpectation;
use artifact::FileIdentity;
use artifact::PackageIdentity;
use artifact::Result;
use artifact::RuntimeReceipt;
use contract::CapturedRun;
use contract::HeldInputs;
use m1_bundle::Leaf;
use serde_json::json;
use stack_observation::Continuation;

const EPOCH: &str = "2026-10-09T00:00:00Z";

fn require(ok: bool, message: &str) -> Result<()> {
    if ok { Ok(()) } else { Err(message.into()) }
}

fn validate_allocator_bundle(bundle: &m1_bundle::Bundle) -> Result<()> {
    require(
        bundle.schema == 1 && bundle.executed_tests == 0 && !bundle.full_m1_pass_claimed,
        "fixture producer may not claim executed tests or full M1",
    )?;
    require(
        bundle.reverie_head == env!("HERMIT_REVERIE_PIN"),
        "fixture/binary Reverie pin differs",
    )?;
    require(
        bundle.hermit_lock_sha256 == artifact::sha256(include_bytes!("../../Cargo.lock"))
            && bundle.nested_lock_sha256
                == artifact::sha256(include_bytes!("../../liteinst-runtime-build/Cargo.lock")),
        "fixture locks differ from the compiled test inputs",
    )?;
    let oracles = BTreeMap::from([
        ("rust_exports".into(), artifact::FROZEN_RUST_ORACLE.into()),
        ("c_workload".into(), artifact::FROZEN_C_ORACLE.into()),
    ]);
    require(
        bundle.oracle_sha256 == oracles,
        "frozen export/workload oracle identities differ",
    )?;
    require(
        bundle.config_fingerprint == detcore_model::config::config_wire_fingerprint(),
        "Detcore config fingerprint differs",
    )?;
    require(
        bundle.producer_sha256
            == artifact::sha256(include_bytes!(
                "../../ci/build-liteinst-allocator-fixtures.rs"
            ))
            && artifact::file_sha256(&bundle.producer_source)? == bundle.producer_sha256,
        "fixture producer differs from the compiled source",
    )?;
    artifact::verify_guest_receipt(
        &bundle.guest_receipt,
        &bundle.guest,
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .ok_or("H root absent")?
            .join("reverie/reverie-liteinst/tests/fixtures/m1_allocator_guest.c"),
        artifact::FROZEN_C_ORACLE,
    )?;
    Ok(())
}

fn required_bundle() -> Result<m2_bundle::Bundle> {
    let pointer = std::env::var_os("HERMIT_M2_STACK_FIXTURE_BUNDLE")
        .ok_or("required HERMIT_M2_STACK_FIXTURE_BUNDLE pointer is absent")?;
    let manifest =
        fs::read_to_string(artifact::real_file(Path::new(&pointer))?).map_err(|e| e.to_string())?;
    let bundle: m2_bundle::Bundle = serde_json::from_slice(
        &fs::read(artifact::real_file(Path::new(manifest.trim()))?).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("typed M2 fixture bundle: {e}"))?;
    require(
        bundle.schema == 1 && bundle.executed_tests == 0 && !bundle.full_stack_isolation_claimed,
        "M2 producer may not claim executed tests or full isolation",
    )?;
    validate_allocator_bundle(&bundle.allocator)?;
    require(
        bundle.guest_source_sha256 == m2_bundle::FROZEN_C
            && bundle.stack_oracle_sha256 == m2_bundle::FROZEN_ORACLE
            && artifact::sha256(include_bytes!(
                "../../reverie/reverie-liteinst/tests/fixtures/m2_stack_guest.c"
            )) == m2_bundle::FROZEN_C
            && artifact::sha256(include_bytes!(
                "../../reverie/reverie-liteinst/tests/support/stack_observation.rs"
            )) == m2_bundle::FROZEN_ORACLE,
        "compiled M2 C/comparator differ from the frozen source",
    )?;
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("H root absent")?;
    require(
        artifact::file_sha256(
            &root.join("reverie/reverie-liteinst/tests/support/stack_observation.rs"),
        )? == m2_bundle::FROZEN_ORACLE,
        "current shared M2 comparator differs",
    )?;
    artifact::verify_guest_receipt(
        &bundle.guest_receipt,
        &bundle.guest,
        &root.join("reverie/reverie-liteinst/tests/fixtures/m2_stack_guest.c"),
        m2_bundle::FROZEN_C,
    )?;
    Ok(bundle)
}

fn qualify(bundle: &m1_bundle::Bundle, standalone: bool) -> Result<(Leaf, RuntimeReceipt)> {
    let leaf = if standalone {
        &bundle.standalone
    } else {
        &bundle.detcore
    };
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("H root absent")?;
    let receipt: RuntimeReceipt = serde_json::from_slice(
        &fs::read(artifact::real_file(&leaf.receipt)?).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let pin = env!("HERMIT_REVERIE_PIN");
    let source = format!("git+https://github.com/rrnewton/reverie.git?rev={pin}#{pin}");
    let (name, target, manifest, package_source, id, initializer) = if standalone {
        require(
            fs::read_to_string(format!("{}.revision", leaf.runtime.display()))
                .map_err(|e| e.to_string())?
                .trim()
                == pin,
            "standalone revision sidecar differs",
        )?;
        require(
            receipt.cargo_artifact.package.source.as_deref() == Some(source.as_str()),
            "standalone exact Git source differs",
        )?;
        let manifest = artifact::real_file(&receipt.cargo_artifact.package.manifest)?;
        require(
            manifest.file_name().is_some_and(|n| n == "Cargo.toml")
                && manifest
                    .parent()
                    .and_then(Path::file_name)
                    .is_some_and(|n| n == "reverie-liteinst-preload"),
            "standalone selected package is not the real preload leaf",
        )?;
        (
            "reverie-liteinst-preload",
            "reverie_liteinst_preload",
            manifest,
            Some(source.clone()),
            format!(
                "git+https://github.com/rrnewton/reverie.git?rev={pin}#reverie-liteinst-preload@0.4.1"
            ),
            artifact::STANDALONE_ROOT,
        )
    } else {
        let manifest = artifact::real_file(&root.join("detcore-liteinst/Cargo.toml"))?;
        (
            "detcore-liteinst",
            "detcore_liteinst",
            manifest,
            None,
            format!(
                "path+file://{}#0.4.0",
                root.join("detcore-liteinst").display()
            ),
            "detcore_liteinst_initialize",
        )
    };
    let package = PackageIdentity {
        id,
        name: name.into(),
        manifest: manifest.clone(),
        source: package_source,
        target: target.into(),
        kind: vec!["cdylib".into()],
        crate_types: vec!["cdylib".into()],
        target_source: artifact::real_file(
            &manifest
                .parent()
                .ok_or("leaf directory absent")?
                .join("src/lib.rs"),
        )?,
    };
    let expected_profile = json!({"opt_level":"3", "debuginfo":0, "debug_assertions":!standalone,
        "overflow_checks":!standalone, "test":false});
    let profile_name = if standalone { "release" } else { "validate" };
    require(
        leaf.profile_name == profile_name && leaf.cargo_profile == expected_profile,
        "leaf profile differs from required diagnostic producer",
    )?;
    let features = vec!["allocator-fixture".into()];
    let receipt = artifact::verify_runtime_receipt(
        &leaf.receipt,
        &leaf.runtime,
        &ConsumerExpectation {
            package: &package,
            features: &features,
            profile_name,
            profile: &expected_profile,
            initializer,
            source_head: &bundle.hermit_head,
            source_tree: &bundle.hermit_tree,
            source_lock_sha256: if standalone {
                &bundle.nested_lock_sha256
            } else {
                &bundle.hermit_lock_sha256
            },
            oracle_sha256: &bundle.oracle_sha256,
        },
    )?;
    Ok((leaf.clone(), receipt))
}

fn retain(directory: &Path, label: &str, run: &CapturedRun) -> Result<()> {
    artifact::write_new(&directory.join(format!("{label}.stdout")), &run.stdout)?;
    artifact::write_new(&directory.join(format!("{label}.stderr")), &run.stderr)?;
    artifact::write_new(&directory.join(format!("{label}.json")), &serde_json::to_vec_pretty(&json!({
        "stdin":run.input, "status_code":run.status.and_then(|status| status.code()),
        "status":format!("{:?}",run.status), "capture_end":format!("{:?}",run.end),
        "elapsed_ns":run.elapsed.as_nanos(), "output_truncated":run.output_truncated,
        "program":run.launch.program, "arguments":run.launch.arguments, "cwd":run.launch.current_dir,
        "held_environment":run.launch.held.environment, "held_policy":run.launch.held.policy,
        "execution_claim":"raw actual stack trial; placement oracle evaluated separately",
    })).map_err(|e| e.to_string())?)
}

// Producer identities are untrusted until independently matched to this clean
// checkout. Read Git with closed stdin and the same bounded capture machinery.
fn source_guard(
    bundle: &m1_bundle::Bundle,
    root: &Path,
    evidence: &Path,
    phase: &str,
) -> Result<()> {
    let mut environment = vec![
        (
            "PATH".into(),
            std::env::var_os("PATH").ok_or("Git PATH absent")?,
        ),
        ("LC_ALL".into(), "C".into()),
        ("GIT_OPTIONAL_LOCKS".into(), "0".into()),
    ];
    // The pinned-root wrapper supplies its Git safe-directory configuration.
    // Repository-location variables and replacement-object controls are never
    // inherited; all repositories and revisions below are explicit.
    for name in ["HOME", "GIT_CONFIG_GLOBAL", "GIT_CONFIG_SYSTEM"] {
        if let Some(value) = std::env::var_os(name) {
            environment.push((name.into(), value));
        }
    }
    let held = HeldInputs {
        environment,
        policy: b"independent clean source identity; closed stdin".to_vec(),
    };
    let read = |repo: &Path, name: &str, args: &[&str]| -> Result<String> {
        let mut command = Command::new("git");
        command
            .arg("--no-replace-objects")
            .args(args)
            .current_dir(repo);
        let label = format!("{phase}-source-{name}");
        let run = contract::run_without_input(command, &held).map_err(|failure| {
            for (index, capture) in failure.captures.iter().enumerate() {
                let _ = retain(evidence, &format!("{label}-failure-{index}"), capture);
            }
            failure.to_string()
        })?;
        retain(evidence, &label, &run)?;
        require(
            run.input.is_none()
                && run.status.is_some_and(|status| status.success())
                && run.end == contract::CaptureEnd::Complete
                && !run.output_truncated,
            "independent Git source read failed",
        )?;
        String::from_utf8(run.stdout).map_err(|e| e.to_string())
    };
    let h = read(
        root,
        "hermit",
        &["rev-parse", "HEAD", "HEAD^{tree}", "HEAD:reverie"],
    )?;
    let values: Vec<_> = h.lines().collect();
    require(
        values
            == [
                bundle.hermit_head.as_str(),
                bundle.hermit_tree.as_str(),
                env!("HERMIT_REVERIE_PIN"),
            ],
        "current Hermit HEAD/tree/gitlink differs from qualified producer or compiled pin",
    )?;
    require(
        read(
            root,
            "hermit-clean",
            &["status", "--porcelain=v1", "--untracked-files=all"],
        )?
        .is_empty(),
        "Hermit source must remain clean during actual fixture execution",
    )?;
    let rv = root.join("reverie");
    let identity = read(&rv, "reverie", &["rev-parse", "HEAD", "HEAD^{tree}"])?;
    let values: Vec<_> = identity.lines().collect();
    require(
        bundle.reverie_head == env!("HERMIT_REVERIE_PIN")
            && values == [env!("HERMIT_REVERIE_PIN"), bundle.reverie_tree.as_str()],
        "initialized Reverie HEAD/tree differs from compiled pin or producer",
    )?;
    require(
        read(
            &rv,
            "reverie-clean",
            &["status", "--porcelain=v1", "--untracked-files=all"],
        )?
        .is_empty(),
        "initialized Reverie source must remain clean during actual fixture execution",
    )
}
#[derive(Clone, Copy)]
struct Case {
    label: &'static str,
    standalone: bool,
    alt_stack: bool,
    inert: bool,
}

fn actual_stack(case: Case) -> Result<()> {
    let bundle = required_bundle()?;
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("H root absent")?;
    let parent = root.join("target/ci/m2-stack-results/build");
    fs::create_dir_all(&parent).map_err(|e| e.to_string())?;
    let evidence = tempfile::Builder::new()
        .prefix(&format!("{}-{}-", case.label, std::process::id()))
        .tempdir_in(&parent)
        .map_err(|e| e.to_string())?
        .keep();
    eprintln!("M2 {} evidence {}", case.label, evidence.display());
    source_guard(&bundle.allocator, root, &evidence, "before")?;
    let mut observation_passed = false;
    let mut placement_evaluated = false;
    let mut completed_report = None;
    let result = (|| -> Result<()> {
        let (leaf, receipt) = qualify(&bundle.allocator, case.standalone)?;
        let guest = FileIdentity::read(&bundle.guest)?;
        let cwd = bundle.guest.parent().ok_or("M2 guest directory absent")?;
        let mut command = Command::new(if case.standalone {
            &bundle.guest
        } else {
            hermit_binary::hermit_binary()
        });
        command.current_dir(cwd);
        let mut environment = BTreeMap::<OsString, OsString>::from([
            (
                "PATH".into(),
                std::env::var_os("PATH").ok_or("pinned PATH absent")?,
            ),
            ("LC_ALL".into(), "C".into()),
            ("LD_BIND_NOW".into(), "1".into()),
        ]);
        let mode = if case.inert { "inert" } else { "observe" };
        let continuation = if case.standalone {
            Continuation::Absent
        } else {
            Continuation::Reached
        };
        if case.standalone {
            require(
                std::env::var_os("LD_PRELOAD").is_none(),
                "test process may not inherit an unqualified preload",
            )?;
            require(
                std::env::var_os("REVERIE_LITEINST_PRELOAD").as_deref()
                    == Some(leaf.runtime.as_os_str()),
                "actual standalone selector differs from the qualified leaf",
            )?;
            if case.inert {
                // The same genuine constructor-bearing DSO, with no active Tool
                // selector. It must not reserve a region or register a stack.
                environment.insert("LD_PRELOAD".into(), leaf.runtime.as_os_str().to_owned());
            } else {
                reverie_liteinst::configure_command(
                    &mut command,
                    reverie_liteinst::PreloadTool::Strace,
                )
                .map_err(|e| e.to_string())?;
                for (name, value) in command.get_envs() {
                    environment.insert(
                        name.to_owned(),
                        value
                            .ok_or("launcher removed a required environment value")?
                            .to_owned(),
                    );
                }
                require(
                    environment.get(&OsString::from("LD_PRELOAD"))
                        == Some(&leaf.runtime.as_os_str().to_owned()),
                    "public standalone launcher did not select exactly the qualified DSO",
                )?;
            }
            environment.insert("REVERIE_LITEINST_SITE_PATCHING".into(), "1".into());
            environment.insert(
                "REVERIE_LITEINST_ALT_STACK".into(),
                if case.alt_stack { "1" } else { "0" }.into(),
            );
            command.arg(mode).arg(&leaf.runtime);
        } else {
            require(
                !case.inert && case.alt_stack,
                "Detcore reached row must require its real altstack",
            )?;
            environment.insert(
                "HERMIT_LITEINST_TOOL_RUNTIME".into(),
                leaf.runtime.as_os_str().to_owned(),
            );
            command
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
                .arg(mode)
                .arg(&leaf.runtime);
        }
        // No calibration is part of the held environment: this original 0f05
        // cross-line write must take the existing unpublished native adapter.
        require(
            !environment.contains_key(&OsString::from(
                "REVERIE_LITEINST_STRADDLER_STALENESS_TICKS",
            )),
            "M2 marker may not inherit split-word calibration",
        )?;
        let executable = FileIdentity::read(Path::new(command.get_program()))?;
        let held = HeldInputs {
            environment: environment.into_iter().collect(),
            policy: serde_json::to_vec(&json!({"case":case.label,"mode":mode,"alt_stack":case.alt_stack,
                "site_patching":case.standalone,"continuation":if case.standalone {"absent"} else {"required"},
                "runtime_sha256":receipt.artifact.sha256,"guest_sha256":guest.sha256,"executable_sha256":executable.sha256,
                "epoch":EPOCH,"seed":0,"sched_seed":0,"fixed_region_required":case.alt_stack,
                "full_entry_isolation_claimed":false,"interior_write_protection_claimed":false})).map_err(|e| e.to_string())?,
        };
        let run = contract::run_without_input(command, &held).map_err(|failure| {
            for (index, capture) in failure.captures.iter().enumerate() {
                let _ = retain(&evidence, &format!("guest-failure-{index}"), capture);
            }
            failure.to_string()
        })?;
        retain(&evidence, "guest", &run)?;
        require(
            run.input.is_none()
                && run.status.is_some_and(|status| status.success())
                && run.end == contract::CaptureEnd::Complete
                && !run.output_truncated,
            "M2 child did not complete successfully; no placement credit",
        )?;
        if !case.standalone {
            let selection = b"hermit: [in-guest-trap] selected: the guest preload is to host the Detcore Tool, with syscall site patching off (REVERIE_LITEINST_SITE_PATCHING=0)";
            require(
                run.stderr.windows(selection.len()).any(|w| w == selection),
                "actual Hermit did not report the required trap/site0 Detcore path",
            )?;
        }
        let observation = stack_observation::parse(&run.stdout)?;
        stack_observation::verify_observation(
            &observation,
            &leaf.runtime,
            mode,
            case.alt_stack,
            continuation,
        )?;
        observation_passed = true;
        if case.alt_stack {
            placement_evaluated = true;
            stack_observation::verify_fixed_stack_placement(&observation, true, continuation)?;
        }
        guest.verify()?;
        executable.verify()?;
        receipt.artifact.verify()?;
        qualify(&bundle.allocator, case.standalone)?;
        artifact::verify_guest_receipt(
            &bundle.guest_receipt,
            &bundle.guest,
            &root.join("reverie/reverie-liteinst/tests/fixtures/m2_stack_guest.c"),
            m2_bundle::FROZEN_C,
        )?;
        completed_report = Some(json!({
            "case":case.label,"actual_child_exit":0,"observation_passed":true,
            "fixed_placement_evaluated":case.alt_stack,"fixed_placement_passed":if case.alt_stack {Some(true)} else {None},
            "actual_marker_ip":observation.marker_ip,"actual_registered_altstack":observation.after.alt_sp,
            "actual_registered_altstack_size":observation.after.alt_size,"actual_reached_rsp":observation.after.reached_rsp,
            "full_entry_isolation_claimed":false,"guest_tls_isolation_claimed":false,"interior_write_protection_claimed":false,
        }));
        Ok(())
    })();
    // Even a literal failing observation closes the independent clean-source
    // guard. No missing/failed actual run receives completed placement credit.
    source_guard(&bundle.allocator, root, &evidence, "after")?;
    if let Some(report) = completed_report {
        artifact::write_new(
            &evidence.join("observation.json"),
            &serde_json::to_vec_pretty(&report).map_err(|e| e.to_string())?,
        )?;
    }
    if let Err(error) = &result {
        artifact::write_new(
            &evidence.join("failure.json"),
            &serde_json::to_vec_pretty(&json!({
                "error":error,"observation_passed":observation_passed,
                "placement_evaluated":placement_evaluated,
                "placement_passed":if placement_evaluated {Some(false)} else {None},
                "full_entry_isolation_claimed":false,
            }))
            .map_err(|e| e.to_string())?,
        )?;
    }
    result.map_err(|error| format!("{error}; M2 raw evidence {}", evidence.display()))
}

#[test]
fn genuine_standalone_registers_fixed_guarded_altstack_without_continuation() -> Result<()> {
    actual_stack(Case {
        label: "standalone-alt1",
        standalone: true,
        alt_stack: true,
        inert: false,
    })
}

#[test]
fn standalone_altstack_disabled_receives_no_owned_stack_credit() -> Result<()> {
    actual_stack(Case {
        label: "standalone-alt0",
        standalone: true,
        alt_stack: false,
        inert: false,
    })
}

#[test]
fn inert_preload_does_not_reserve_or_register_runtime_stacks() -> Result<()> {
    actual_stack(Case {
        label: "standalone-inert",
        standalone: true,
        alt_stack: false,
        inert: true,
    })
}

#[test]
fn genuine_detcore_reaches_fixed_guarded_callback_and_altstack() -> Result<()> {
    actual_stack(Case {
        label: "detcore-trap-alt1",
        standalone: false,
        alt_stack: true,
        inert: false,
    })
}
