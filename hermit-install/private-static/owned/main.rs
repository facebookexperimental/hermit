//! One fresh maintained source/archive owner and one owned diagnostic link.
//! Expected join refusals are retained before independent final inspections.
mod factory_controls;
mod native_leaf_controls;
mod optional_controls;
mod ordinary_controls;
mod structural_observation;
mod terminal_observation;
mod view_leaf_controls;
use std::path::{Path, PathBuf};
use std::time::Duration;
use reverie_liteinst_artifact::private_static::{SourceExpectation, VerifiedStaticSource};
use reverie_liteinst_artifact::private_static::selected_initializers::{InitializerLinkInputs, implementation_sha256, NativeLeafInputs, native_leaf_implementation_sha256};
use reverie_liteinst_artifact::{FileIdentity, Runner, qualifier_source_sha256, sha256, write_new};

fn run() -> Result<(), String> {
    let a: Vec<String> = std::env::args().collect();
    if a.len() != 31 {
        return Err("expected H H_HEAD RUNTIME_R_PIN CARGO RUSTC TARGET LOGS TOOL_R_HEAD QUALIFIER_SHA MODULE_SHA LLD SCRIPT LIBC LIBUNWIND MEMCPY_SOURCE MUSL_AUTHORITY SSP_SOURCE OUTPUT PREPARED_ARCHIVE_SHA PREPARED_ARCHIVE_BYTES OBJDUMP MEMSET_SOURCE SET_THREAD_AREA_SOURCE NATIVE_LEAF_MODULE_SHA OPTIONAL_STD_WEAK OPTIONAL_STD_DESTRUCTORS OPTIONAL_MUSL_DL_ITERATE ORDINARY_GATE_MODULE_SHA VIEW_SITE_MODULE_SHA VIEW_TABLE_MODULE_SHA".into());
    }
    if qualifier_source_sha256() != a[9] || implementation_sha256() != a[10] {
        return Err("compiled qualifier/module differs from independently bound tool source".into());
    }
    if native_leaf_implementation_sha256() != a[24] {
        return Err("compiled native leaf module differs from bound actual tool source".into());
    }
    if reverie_liteinst_artifact::private_static::selected_initializers::ordinary_gate_implementation_sha256() != a[28] {
        return Err("compiled ordinary gate module differs from bound actual tool source".into());
    }
    if reverie_liteinst_artifact::private_static::selected_initializers::view_site_implementation_sha256() != a[29]
        || reverie_liteinst_artifact::private_static::selected_initializers::view_table_implementation_sha256() != a[30] {
        return Err("compiled View leaf modules differ from independently bound tool source".into());
    }
    let expected_bytes: u64 = a[20].parse().map_err(|_| "invalid archive byte count")?;
    let expected = SourceExpectation {
        workspace: Path::new(&a[1]), workspace_head: &a[2], reverie_pin: &a[3],
        cargo: Path::new(&a[4]), rustc: Path::new(&a[5]), target_dir: Path::new(&a[6]),
    };
    let logs = PathBuf::from(&a[7]);
    let mut runner = Runner::new(logs.clone(), Duration::from_secs(2400), 128 * 1024 * 1024)?;
    let mut terminal_observed = false;
    let mut terminal_detail = String::from("NOT_REACHED");
    let mut terminal_value = None;
    let mut terminal_files = Vec::new();
    let mut view_leaf_report = None;
    let mut view_leaf_files = Vec::new();
    let mut view_leaf_observed = false;
    let mut view_leaf_detail = String::from("NOT_REACHED");
    let mut joined = false;
    let mut selected_proof = None;
    let mut join_detail = String::from("NOT_REACHED");
    let mut archive_detail = String::new();
    let mut census_observed = false;
    let mut census_detail = String::from("NOT_REACHED");
    let mut census_files = Vec::new();
    let mut leaf_files = Vec::new();
    let mut leaves_observed = false;
    let mut leaf_detail = String::from("NOT_REACHED");
    let mut optional_files = Vec::new();
    let mut optional_observed = false;
    let mut optional_detail = String::from("NOT_REACHED");
    let mut optional_proof = None;
    let mut structural = None;
    let mut structural_files = Vec::new();
    let mut ordinary_files = Vec::new();
    let mut ordinary_observed = false;
    let mut ordinary_detail = String::from("NOT_REACHED");
    let body = VerifiedStaticSource::with_resolved(&expected, &mut runner, |source, runner| {
        let archive = source.build_archive(runner)?;
        archive.verify()?;
        let actual = archive.file();
        if actual.sha256 != a[19] || std::fs::metadata(&actual.path).map_err(|e| e.to_string())?.len() != expected_bytes {
            return Err("fresh owned archive differs from actual prepared physical inventory".into());
        }
        archive_detail = format!("ARCHIVE_PATH={}\nARCHIVE_SHA256={}\nARCHIVE_BYTES={}\nMETADATA_SHA256={}\nCARGO_MESSAGES_SHA256={}\n",
            actual.path.display(), actual.sha256, expected_bytes, archive.metadata_sha256(), archive.cargo_messages_sha256());
        let optional_inputs = optional_controls::Inputs::prepare(Path::new(&a[12]),
            [Path::new(&a[25]), Path::new(&a[26]), Path::new(&a[27])], &logs)?;
        optional_files.extend(optional_inputs.files());
        let supporting = [Path::new(&a[14])];
        let input = InitializerLinkInputs {
            lld: Path::new(&a[11]), script: &optional_inputs.script.path, libc: Path::new(&a[13]),
            supporting_archives: &supporting, memcpy_source: Path::new(&a[15]),
            musl_source_authority: Path::new(&a[16]), stack_chk_source: Path::new(&a[17]),
            output_directory: Path::new(&a[18]),
        };
        let controls = factory_controls::negatives(source, &archive, &expected, &input, runner,
            &Path::new(&a[18]).with_extension("negative-inputs"))?;
        if controls != 5 { return Err("actual five factory controls incomplete".into()); }
        // The sole positive LLD invocation remains inside this same source and
        // its actual StaticArchive owner. No old JSON can construct either.
        match source.link_and_join_initializer_definitions(&archive, &input, runner) {
            Ok(selected) => match selected.verify() {
                Ok(()) => { joined = true; join_detail = format!("Ok({selected:#?})"); selected_proof = Some(selected); }
                Err(error) => { join_detail = format!("POST_JOIN_CUSTODY_REFUSED: {error}"); }
            },
            Err(error) => { join_detail = format!("LINK_OR_SELECTED_JOIN_REFUSED: {error}"); }
        }
        // Finite terminal data is independent of optional/native/census results.
        // It requires this actual selected proof; absence is a named refusal.
        let terminal = (|| {
            let selected = selected_proof.as_ref().ok_or("terminal observation requires actual successful selected join")?;
            let elf = FileIdentity::read(&input.output_directory.join("initializer-definitions.not-qualified.elf"))?;
            let map = FileIdentity::read(&input.output_directory.join("initializer-definitions.map"))?;
            let objdump = FileIdentity::read(Path::new(&a[21]))?;
            terminal_observation::observe(source, &archive, selected, &elf, &map,
                &objdump, runner, &logs, &mut terminal_files)
        })();
        match terminal {
            Ok(value) => { terminal_observed = true; terminal_value = Some(value);
                terminal_detail = "FINITE_OBSERVATION_ONLY_NOT_PROVEN".into(); }
            Err(error) => { terminal_detail = format!("TERMINAL_OBSERVER_REFUSED: {error}"); }
        }
        // Reuse the actual selected token from the sole link. Leaf errors are
        // independent of later optional/native/census/final inspection work.
        let view_attempt = (|| {
            let selected = selected_proof.as_ref().ok_or("View leaves require this actual successful selected join")?;
            let elf = FileIdentity::read(&input.output_directory.join("initializer-definitions.not-qualified.elf"))?;
            let map = FileIdentity::read(&input.output_directory.join("initializer-definitions.map"))?;
            let mut outcome = view_leaf_controls::observe_and_controls(source, &archive,
                selected, &elf, &map, &optional_inputs.script, runner, &logs);
            outcome.report["tool_R_head"] = serde_json::json!(a[8]);
            outcome.report["runtime_H_head"] = serde_json::json!(a[2]);
            outcome.report["runtime_R_pin"] = serde_json::json!(a[3]);
            outcome.report["site_module_sha256"] = serde_json::json!(a[29]);
            outcome.report["table_module_sha256"] = serde_json::json!(a[30]);
            outcome.report["helper_source_sha256"] = serde_json::json!(sha256(include_bytes!("view_leaf_controls.rs")));
            view_leaf_observed = outcome.report["state"] == "ACTUAL_LEAF_FACTORY_CONTROLS_ONLY_NOT_QUALIFIED";
            view_leaf_report = Some(outcome.report);
            view_leaf_files.extend(outcome.files);
            // Never continue after failed restore. with_resolved still closes
            // the actual source on this Err; no later consumer runs.
            if outcome.restoration_failed {
                let _ = view_leaf_controls::retain_report(&logs,
                    view_leaf_report.as_ref().ok_or("View leaf report absent")?, &mut view_leaf_files);
                return Ok::<bool,String>(true);
            }
            Ok(false)
        })();
        match view_attempt {
            Ok(true) => {
                view_leaf_detail = "RESTORATION_UNCONFIRMED_FATAL".into();
                return Err("View leaf script restore unconfirmed; later consumers forbidden".into());
            }
            Ok(false) => { view_leaf_detail = if view_leaf_observed {
                "ACTUAL_LEAF_FACTORY_CONTROLS_ONLY_NOT_QUALIFIED".into()
            } else { "VIEW_LEAF_CONTROL_REFUSED".into() }; }
            Err(error) => { view_leaf_observed = false; view_leaf_detail = format!("VIEW_LEAF_ATTEMPT_REFUSED: {error}"); }
        }
        // A full actual producer-ledger positive is required. No JSON/private
        // four-file token substitutes for the selected definition returned above.
        let optional = match selected_proof.as_ref() {
            Some(selected) => optional_controls::observe_and_controls(source, &archive,
                selected, &optional_inputs, Path::new(&a[21]), runner, &logs,
                &mut optional_files, &mut optional_proof),
            None => Err("optional observation requires this actual successful selected join".into()),
        };
        match optional {
            Ok(()) => { optional_observed = true; optional_detail = "OBSERVED_WITH_TWO_RETURNED_LEDGER_CONTROLS_NOT_PROVEN".into(); }
            Err(error) => { optional_detail = format!("OPTIONAL_OBSERVATION_OR_RETURNED_LEDGER_CONTROL_REFUSED: {error}"); }
        }
        // Ordinary provider identity and complete caller transfer observation
        // share this same actual source/archive/ELF/map, independently of the
        // optional result. A transfer_refusal remains explicit NOT_PROVEN data.
        let ordinary = (|| {
            let elf = FileIdentity::read(&input.output_directory.join("initializer-definitions.not-qualified.elf"))?;
            let map = FileIdentity::read(&input.output_directory.join("initializer-definitions.map"))?;
            let objdump = FileIdentity::read(Path::new(&a[21]))?;
            ordinary_controls::observe_and_control(source, &archive, &elf, &map,
                &objdump, runner, &logs, &mut ordinary_files)
        })();
        match ordinary {
            Ok(()) => { ordinary_observed = true; ordinary_detail = "OBSERVED_NOT_PROVEN".into(); }
            Err(error) => { ordinary_detail = format!("ORDINARY_PROVIDER_OR_CALLER_DIAGNOSTIC_REFUSED: {error}"); }
        }
        // Independent of selected-initializer joining. These calls cannot link
        // or construct source/archive authority; each uses this SAME owner.
        let leaves = (|| {
            let elf = FileIdentity::read(&input.output_directory.join("initializer-definitions.not-qualified.elf"))?;
            let map = FileIdentity::read(&input.output_directory.join("initializer-definitions.map"))?;
            leaf_files.extend([elf.clone(), map.clone()]);
            let native = NativeLeafInputs {
                memcpy: reverie_liteinst_artifact::private_machine_diagnostic::NativeMemcpyInputs {
                    archive: input.libc, source: input.memcpy_source,
                    source_authority: input.musl_source_authority, link_map: &map.path,
                }, memset_source: Path::new(&a[22]), set_thread_area_source: Path::new(&a[23]),
            };
            native_leaf_controls::observe_and_controls(source, &archive, &expected,
                &native, &elf, &map, runner, &logs, &mut leaf_files)
        })();
        match leaves {
            Ok(()) => { leaves_observed = true; leaf_detail = "OBSERVED_WITH_THREE_CLOSE_CONTROLS_NOT_PROVEN".into(); }
            Err(error) => { leaf_detail = format!("NATIVE_LEAVES_OR_CLOSE_CONTROLS_REFUSED: {error}"); }
        }
        // Explicit structural alternative uses the actual retained optional
        // object before this source/archive transaction closes. Existing
        // ordinary/native controls above are unchanged and still mandatory.
        let structural_attempt = (|| {
            let elf = FileIdentity::read(&input.output_directory.join("initializer-definitions.not-qualified.elf"))?;
            let map = FileIdentity::read(&input.output_directory.join("initializer-definitions.map"))?;
            let objdump = FileIdentity::read(Path::new(&a[21]))?;
            let observation = optional_proof.as_ref().ok_or("missing actual same-owner optional observation")?;
            let native = reverie_liteinst_artifact::private_machine_diagnostic::NativeMemcpyInputs {
                archive:input.libc,source:input.memcpy_source,
                source_authority:input.musl_source_authority,link_map:&map.path,
            };
            let selected = selected_proof.as_ref().ok_or("missing actual same-owner selected definitions")?;
            let value = structural_observation::observe(source,&archive,&elf,&map,
                selected,observation,&native,&objdump,runner)?;
            structural_files.extend([elf,map,objdump]);
            structural_observation::retain_diagnostic_files(&value,&mut structural_files)?;
            Ok::<_,String>(value)
        })();
        structural = Some(match structural_attempt {
            Ok(value)=>value,
            Err(error)=>serde_json::json!({"attempt_error":error}),
        });
        // Observation only, under the SAME actual source/archive. Attempt
        // even when selected-definition joining refused but left real output.
        let census = (|| {
            let elf = FileIdentity::read(&input.output_directory.join("initializer-definitions.not-qualified.elf"))?;
            let map = FileIdentity::read(&input.output_directory.join("initializer-definitions.map"))?;
            let objdump = FileIdentity::read(Path::new(&a[21]))?;
            census_files = vec![elf, map, objdump];
            let observed = source.inspect_private_startup_census(&archive,
                &census_files[0], &census_files[1], &census_files[2], runner)?;
            let json_bytes = observed.to_json_pretty()?;
            let expected_json_sha = sha256(&json_bytes);
            let json_path = logs.join("startup-census.not-proven.json");
            write_new(&json_path, &json_bytes)?;
            let json_identity = FileIdentity::read(&json_path)?;
            if json_identity.sha256 != expected_json_sha {
                return Err("census JSON differs from actual serialized observation".into());
            }
            census_files.push(json_identity);
            for file in &census_files { file.verify()?; }
            Ok::<(), String>(())
        })();
        match census {
            Ok(()) => { census_observed = true; census_detail = "OBSERVED_NOT_PROVEN".into(); }
            Err(error) => { census_detail = format!("CENSUS_REFUSED: {error}"); }
        }
        // Existing consumers above observe the original exact2 source streams.
        // Now resolve ONE further genuine source owner for TWO leaf admissions.
        let foreign_leaves = (|| {
            let selected=selected_proof.as_ref().ok_or("View foreign controls require actual selected join")?;
            let elf=FileIdentity::read(&input.output_directory.join("initializer-definitions.not-qualified.elf"))?;
            let map=FileIdentity::read(&input.output_directory.join("initializer-definitions.map"))?;
            let report=view_leaf_report.as_mut().ok_or("View six-control report absent")?;
            view_leaf_controls::foreign_owner_controls(source,&archive,selected,&expected,
                &elf,&map,input.lld,runner,report,&view_leaf_files)?;
            view_leaf_observed=report["state"]=="ACTUAL_LEAF_FACTORY_CONTROLS_ONLY_NOT_QUALIFIED";
            view_leaf_controls::retain_report(&logs,report,&mut view_leaf_files)?;
            Ok::<(),String>(())
        })();
        match foreign_leaves {
            Ok(())=>{view_leaf_detail=if view_leaf_observed{"ACTUAL_LEAF_FACTORY_CONTROLS_ONLY_NOT_QUALIFIED".into()}else{"VIEW_LEAF_CONTROL_REFUSED".into()};}
            Err(error)=>{view_leaf_observed=false;view_leaf_detail=format!("VIEW_FOREIGN_OWNER_OR_REPORT_REFUSED: {error}");}
        }
        // A census error is retained independently; it cannot prevent the
        // enclosing factory's final source close or external ELF inspection.
        // Preserve a join refusal as data so source final-close must still
        // independently succeed. The enclosing Python caller retains failure
        // and inspects any actual ELF; it never treats this as qualification.
        archive.verify()?;
        Ok(())
    });
    let detail = write_new(&logs.join("owned-result.debug"), format!("{archive_detail}SELECTED_JOIN={join_detail}\nTERMINAL_OBSERVER={terminal_detail}\nSTARTUP_CENSUS={census_detail}\nNATIVE_LEAVES={leaf_detail}\nOPTIONAL_ZERO={optional_detail}\nORDINARY_GATE={ordinary_detail}\nEXPLICIT_STRUCTURAL_ATTEMPT={structural:#?}\nVIEW_LEAVES={view_leaf_detail}\nSOURCE_FACTORY_RESULT={body:?}\n").as_bytes());
    let commands = write_new(&logs.join("commands.debug"), format!("{:#?}\n", runner.commands).as_bytes());
    if body.is_err() || detail.is_err() || commands.is_err() {
        return Err(format!("source={body:?}; evidence={detail:?}; commands={commands:?}"));
    }
    // Retain the owned selected proof across source final-close. Bracket the
    // exported data identities with its existing full file-custody checks.
    for file in census_files.iter().chain(&leaf_files).chain(&optional_files).chain(&ordinary_files).chain(&structural_files).chain(&terminal_files).chain(&view_leaf_files) { file.verify()?; }
    if let Some(proof) = &selected_proof { proof.verify()?; }
    if let Some(proof) = &optional_proof { proof.verify()?; }
    let mut outputs = Vec::new();
    for (role, name) in [("ELF", "initializer-definitions.not-qualified.elf"), ("MAP", "initializer-definitions.map")] {
        let path = Path::new(&a[18]).join(name);
        if path.exists() { outputs.push((role, FileIdentity::read(&path)?)); }
    }
    for file in census_files.iter().chain(&leaf_files).chain(&optional_files).chain(&ordinary_files).chain(&structural_files).chain(&terminal_files).chain(&view_leaf_files) { file.verify()?; }
    if let Some(proof) = &selected_proof { proof.verify()?; }
    if let Some(proof) = &optional_proof { proof.verify()?; }
    for (role, file) in &outputs {
        file.verify()?;
        println!("ACTUAL_{role}_PATH={}\nACTUAL_{role}_SHA256={}", file.path.display(), file.sha256);
    }
    if let Some(json) = census_files.get(3) {
        json.verify()?;
        println!("STARTUP_CENSUS_JSON_SHA256={}", json.sha256);
    }
    println!("{archive_detail}TOOL_R_HEAD={}\nRUNTIME_R_PIN={}\nSOURCE_STATE=SOURCE_CLOSED_LINK_OBSERVATION_ONLY", a[8], a[3]);
    println!("OWNED_JOIN={}\nFINAL_INSPECTION=EXTERNAL_REQUIRED\nQUALIFICATION=NOT_PROVEN\nPRE_FS_CLOSURE=NOT_PROVEN\nREADINESS=NOT_PROVEN\nGUESTS_EXECUTED=0\nTESTS_EXECUTED=0", if joined { "SELECTED_DEFINITIONS_ONLY" } else { "REFUSED" });
    println!("STARTUP_CENSUS={}\nPRE_FS_CENSUS_QUALIFICATION=NOT_PROVEN", if census_observed { "OBSERVED_NOT_PROVEN" } else { "REFUSED" });
    println!("NATIVE_LEAVES={}\nNATIVE_LEAF_QUALIFICATION=NOT_PROVEN", if leaves_observed { "OBSERVED_WITH_THREE_CLOSE_CONTROLS_NOT_PROVEN" } else { "REFUSED" });
    for file in &leaf_files {
        file.verify()?;
        println!("NATIVE_LEAF_RETAINED_FILE={}\t{}", file.sha256, file.path.display());
    }
    println!("OPTIONAL_ZERO={}\nOPTIONAL_ZERO_QUALIFICATION=NOT_PROVEN", if optional_observed { "OBSERVED_WITH_TWO_RETURNED_LEDGER_CONTROLS_NOT_PROVEN" } else { "REFUSED" });
    let mut optional_paths = std::collections::BTreeSet::new();
    for file in &optional_files {
        file.verify()?;
        if optional_paths.insert(file.path.clone()) {
            println!("OPTIONAL_ZERO_RETAINED_FILE={}\t{}", file.sha256, file.path.display());
        }
    }
    if let Some(json) = optional_files.iter().find(|f| f.path.file_name().is_some_and(|n| n == "optional-zero.not-proven.debug")) {
        json.verify()?;
        println!("OPTIONAL_ZERO_DEBUG_SHA256={}", json.sha256);
    }
    println!("ORDINARY_GATE={}\nORDINARY_GATE_QUALIFICATION=NOT_PROVEN", if ordinary_observed { "OBSERVED_NOT_PROVEN" } else { "REFUSED" });
    let mut ordinary_paths = std::collections::BTreeSet::new();
    for file in &ordinary_files {
        file.verify()?;
        if ordinary_paths.insert(file.path.clone()) {
            println!("ORDINARY_GATE_RETAINED_FILE={}\t{}", file.sha256, file.path.display());
        }
    }
    if let Some(json) = ordinary_files.iter().find(|f| f.path.file_name().is_some_and(|n| n == "ordinary-gate.not-proven.json")) {
        json.verify()?;
        println!("ORDINARY_GATE_JSON_SHA256={}", json.sha256);
    }
    println!("TERMINAL_OBSERVER={}\nTERMINAL_QUALIFICATION=NOT_PROVEN", if terminal_observed { "FINITE_OBSERVATION_ONLY_NOT_PROVEN" } else { "REFUSED" });
    if let Some(file) = terminal_files.iter().find(|file| file.path.file_name().is_some_and(|n| n == "terminal-observer.not-proven.json")) {
        file.verify()?;
        println!("TERMINAL_OBSERVER_JSON_SHA256={}", file.sha256);
    }
    // Emit only after actual source final-close and full retained-file checks.
    println!("VIEW_LEAF_CONTROLS={}", if view_leaf_observed {
        "ACTUAL_LEAF_FACTORY_CONTROLS_ONLY_NOT_QUALIFIED"
    } else { "REFUSED" });
    let view_rows = view_leaf_report.as_ref().and_then(|v|v["roles"].as_array())
        .map(|roles|roles.iter().filter_map(|v|v["controls"].as_array()).map(Vec::len).sum::<usize>()).unwrap_or(0);
    println!("VIEW_LEAF_CONTROL_ROWS={view_rows}");
    for role in ["site", "table"] {
        let state = view_leaf_report.as_ref().and_then(|v|v["roles"].as_array())
            .and_then(|roles|roles.iter().find(|v|v["role"]==role))
            .and_then(|v|v["state"].as_str()).unwrap_or("NOT_REACHED");
        println!("VIEW_{}_LEAF={state}", role.to_ascii_uppercase());
    }
    if let Some(file) = view_leaf_files.iter().find(|f|f.path.file_name().is_some_and(|n|n=="view-leaf-controls.not-qualified.json")) {
        file.verify()?;
        println!("VIEW_LEAF_REPORT_SHA256={}",file.sha256);
    }
    if !terminal_observed || !joined || !census_observed || !leaves_observed || !optional_observed || !ordinary_observed || !view_leaf_observed {
        return Err(format!("terminal={terminal_detail}; selected={join_detail}; census={census_detail}; leaves={leaf_detail}; optional={optional_detail}; ordinary={ordinary_detail}; view_leaves={view_leaf_detail}"));
    }
    let structural = structural.as_ref().ok_or("structural attempt absent")?;
    structural_observation::require_complete(structural)?;
    let executable = FileIdentity::read(&std::env::current_exe().map_err(|e|e.to_string())?)?;
    let receipt = serde_json::json!({
        "schema":1,"state":"SOURCE_CLOSED_EXPLICIT_STRUCTURAL_OBSERVATION_ONLY",
        "tool_R_head":a[8],"runtime_H_head":a[2],"runtime_R_pin":a[3],
        "qualifier_sha256":a[9],"initializer_module_sha256":a[10],
        "native_leaf_module_sha256":a[24],"ordinary_gate_module_sha256":a[28],
        "view_leaf_controls":view_leaf_report.as_ref().ok_or("View leaf controls absent")?,
        "view_site_module_sha256":a[29],"view_table_module_sha256":a[30],
        "view_leaf_helper_source_sha256":sha256(include_bytes!("view_leaf_controls.rs")),
        "driver_executable":executable,
        "main_source_sha256":sha256(include_bytes!("main.rs")),
        "structural_source_sha256":sha256(include_bytes!("structural_observation.rs")),
        "observation":structural,
        "terminal_observation":terminal_value.as_ref().ok_or("terminal observation absent")?,
        "terminal_source_sha256":sha256(include_bytes!("terminal_observation.rs")),
        "source_factory_close":"PASS","archive_close":"PASS",
        "qualification":"NOT_PROVEN","pre_fs_closure":"NOT_PROVEN",
        "readiness":"NOT_PROVEN","guest_release":false,"guests_executed":0,
    });
    let receipt_path=logs.join("explicit-structural.source-closed.json");
    let receipt_bytes=serde_json::to_vec_pretty(&receipt).map_err(|e|e.to_string())?;
    write_new(&receipt_path,&receipt_bytes)?;
    let receipt_file=FileIdentity::read(&receipt_path)?;
    if receipt_file.sha256!=sha256(&receipt_bytes) {return Err("structural receipt write changed".into());}
    for (_, file) in &outputs { file.verify()?; }
    for file in census_files.iter().chain(&leaf_files).chain(&optional_files).chain(&ordinary_files).chain(&structural_files).chain(&terminal_files).chain(&view_leaf_files) { file.verify()?; }
    if let Some(proof) = &selected_proof { proof.verify()?; }
    if let Some(proof) = &optional_proof { proof.verify()?; }
    receipt_file.verify()?;executable.verify()?;
    println!("EXPLICIT_STRUCTURAL_SOURCE_CLOSED_SHA256={}",receipt_file.sha256);
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("OWNED_STATIC_LINK_REFUSED: {error}");
        std::process::exit(1);
    }
}
