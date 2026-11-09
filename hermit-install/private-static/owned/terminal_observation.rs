//! Same-owner finite terminal observations. This cannot admit execution.
use std::path::Path;
use reverie_liteinst_artifact::{FileIdentity, Runner, sha256, write_new};
use reverie_liteinst_artifact::private_static::{StaticArchive, VerifiedStaticSource};
use reverie_liteinst_artifact::private_static::selected_initializers::SelectedInitializerDefinitions;
use serde_json::Value;
const OPEN: [&str; 6] = [
    "GENERIC_PREPARATION_REJECTION_UNCHANGED",
    "UD2_UNREACHABILITY_REQUIRES_EXCEPTION_PREDECESSOR_PROOF",
    "PANIC_CANNOT_UNWIND_AND_OTHER_CALLEES_NOT_PROVEN",
    "HOST_GATE_FILTER_SIGNAL_WAIT_AND_CANCEL_NOT_PROVEN",
    "OBSERVER_SUCCESS_PREDECESSOR_NOT_PROVEN",
    "PRE_FS_CLOSURE_AND_ENTRY_ADMISSION_NOT_PROVEN",
];
fn need(ok: bool, message: &str) -> Result<(), String> {
    if ok { Ok(()) } else { Err(message.into()) }
}
pub fn observe(source: &VerifiedStaticSource, archive: &StaticArchive,
    selected: &SelectedInitializerDefinitions, elf: &FileIdentity,
    map: &FileIdentity, objdump: &FileIdentity, runner: &mut Runner,
    logs: &Path, files: &mut Vec<FileIdentity>) -> Result<Value, String> {
    files.extend([elf.clone(), map.clone(), objdump.clone()]);
    let mut captured = None;
    // Only the actual private source-owned factory can create this value.
    // Serialization exports observations; it never reconstructs proof input.
    let result = source.with_mapped_terminal_observation(archive, selected,
        objdump, runner, |observed, _| {
            let value = serde_json::to_value(observed).map_err(|e| e.to_string())?;
            need(value["scope"] == "FINITE_TERMINAL_OBSERVER_STATIC_OBSERVATIONS_ONLY",
                "terminal observation scope differs")?;
            need(value["remaining"] == serde_json::json!(OPEN),
                "terminal observation lost explicit open obligations")?;
            let definitions = value["definitions"].as_array().ok_or("terminal definitions absent")?;
            need(definitions.len() == 5, "terminal definition count differs")?;
            for row in definitions {
                let row = row.as_array().ok_or("terminal definition tuple absent")?;
                need(row.len() == 3 && row[0].as_u64().is_some_and(|v| v != 0)
                    && row[1].as_u64().is_some_and(|v| v != 0)
                    && row[2].as_str().is_some_and(|v| v.len() == 64
                        && v.bytes().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())),
                    "terminal definition identity malformed")?;
            }
            let trap = value["observer_int3"].as_u64().ok_or("terminal observer address absent")?;
            need(trap != 0 && trap.checked_add(1) == value["expected_post_rip"].as_u64()
                && value["ud2"].as_u64().is_some_and(|v| v != 0),
                "terminal trap/post address relation differs")?;
            let disassembly: FileIdentity = serde_json::from_value(value["disassembly"].clone())
                .map_err(|e| e.to_string())?;
            disassembly.verify()?;
            files.push(disassembly);
            let bytes = serde_json::to_vec_pretty(&value).map_err(|e| e.to_string())?;
            let path = logs.join("terminal-observer.not-proven.json");
            write_new(&path, &bytes)?;
            let file = FileIdentity::read(&path)?;
            need(file.sha256 == sha256(&bytes), "terminal serialized bytes changed")?;
            files.push(file);
            captured = Some(value);
            Ok(())
        });
    // Do not bypass file/archive closing when the callback or factory refused.
    let archive_close = archive.verify();
    let selected_close = selected.verify();
    let file_errors = files.iter().filter_map(|file| file.verify().err()).collect::<Vec<_>>();
    if result.is_err() || archive_close.is_err() || selected_close.is_err() || !file_errors.is_empty() {
        return Err(format!("terminal={result:?}; archive_close={archive_close:?}; selected_close={selected_close:?}; file_close={file_errors:?}"));
    }
    captured.ok_or("terminal positive callback not reached".into())
}
