//! Actual source-owned observations and callback/closing controls, not authority.
//! The lane must exclusively own Cargo artifacts while the mutation is active.
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::panic::{catch_unwind, AssertUnwindSafe};
use reverie_liteinst_artifact::{FileIdentity, Runner, sha256, write_new};
use reverie_liteinst_artifact::private_static::{SourceExpectation, StaticArchive, VerifiedStaticSource};
use reverie_liteinst_artifact::private_static::selected_initializers::NativeLeafInputs;
use serde_json::Value;

fn need(ok: bool, message: &str) -> Result<(), String> {
    if ok { Ok(()) } else { Err(message.into()) }
}

// Observation of genuine fresh streams only. This creates no StaticArchive or
// source identity; the existing factory already selected/retained the outputs.
fn selected_build_script(archive: &StaticArchive, expected: &SourceExpectation<'_>,
    runner: &Runner) -> Result<(FileIdentity, Vec<FileIdentity>), String> {
    let builds: Vec<_> = runner.commands.iter().filter(|c|
        c.program.as_str() == expected.cargo.to_string_lossy().as_ref() && c.status == 0 &&
        c.cwd == expected.workspace && c.arguments.first().map(String::as_str) == Some("rustc") &&
        c.stdout.sha256 == archive.cargo_messages_sha256()).collect();
    need(builds.len() == 1, "leaf control needs one actual fresh archive Cargo stream")?;
    let metadata: Vec<_> = runner.commands.iter().filter(|c|
        c.program.as_str() == expected.cargo.to_string_lossy().as_ref() && c.status == 0 &&
        c.cwd == expected.workspace && c.arguments.first().map(String::as_str) == Some("metadata") &&
        c.stdout.sha256 == archive.metadata_sha256()).collect();
    need(metadata.len() == 2, "leaf control needs both actual equal-metadata source streams")?;
    let mut files = vec![builds[0].stdout.clone()];
    files.extend(metadata.iter().map(|c| c.stdout.clone()));
    for f in &files { f.verify()?; }
    for f in &files {
        need(std::fs::metadata(&f.path).map_err(|e|e.to_string())?.len() <= 128 * 1024 * 1024,
            "leaf control source stream exceeds bound")?;
    }
    let raw = std::fs::read(&files[0].path).map_err(|e| e.to_string())?;
    need(raw.len() <= 128 * 1024 * 1024 && sha256(&raw) == archive.cargo_messages_sha256(),
        "leaf control fresh Cargo stream changed/exceeds bound")?;
    let meta_raw = std::fs::read(&files[1].path).map_err(|e| e.to_string())?;
    need(meta_raw.len() <= 128 * 1024 * 1024 && sha256(&meta_raw) == archive.metadata_sha256(),
        "leaf control actual metadata changed/exceeds bound")?;
    let meta: Value = serde_json::from_slice(&meta_raw).map_err(|e| e.to_string())?;
    let manifest = expected.workspace.join("detcore-liteinst/Cargo.toml");
    let packages = meta["packages"].as_array().ok_or("actual metadata packages absent")?;
    let packages: Vec<_> = packages.iter().filter(|p| p["name"] == "detcore-liteinst" &&
        p["manifest_path"].as_str() == manifest.to_str() && p["source"].is_null()).collect();
    need(packages.len() == 1, "leaf control actual runtime package ambiguous")?;
    let package = packages[0];
    let targets = package["targets"].as_array().ok_or("actual runtime targets absent")?;
    let targets: Vec<_> = targets.iter().filter(|t|
        t["kind"] == serde_json::json!(["custom-build"]) &&
        t["crate_types"] == serde_json::json!(["bin"])).collect();
    need(targets.len() == 1, "leaf control requires one actual custom-build target")?;
    let rows: Vec<Value> = raw.split(|b| *b == b'\n').filter(|s| !s.is_empty())
        .map(serde_json::from_slice).collect::<Result<_, _>>().map_err(|e| e.to_string())?;
    let finished: Vec<_> = rows.iter().filter(|r| r["reason"] == "build-finished")
        .map(|r| r["success"].as_bool()).collect();
    need(finished == [Some(true)], "leaf control archive stream did not finish once")?;
    let scripts: Vec<_> = rows.iter().filter(|r| r["reason"] == "compiler-artifact" &&
        r["package_id"] == package["id"] && r["manifest_path"].as_str() == manifest.to_str() &&
        r["target"] == *targets[0]).collect();
    need(scripts.len() == 1, "leaf control actual selected build script absent/duplicate")?;
    let row = scripts[0];
    need(row["features"] == serde_json::json!(["exec-native-injection", "exec-private-init", "loader-private-runtime"]) &&
        row["profile"]["test"] == false && row["executable"].is_null(),
        "leaf control actual script feature/profile/executable differs")?;
    let names = row["filenames"].as_array().ok_or("actual script filenames absent")?;
    need(names.len() == 1, "leaf control actual script output ambiguous")?;
    let path = PathBuf::from(names[0].as_str().ok_or("actual script filename not string")?);
    need(path.is_absolute() && path.starts_with(expected.target_dir) &&
        path.canonicalize().map_err(|e| e.to_string())? == path &&
        std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?.is_file(),
        "leaf control script outside sole target/indirect/nonregular")?;
    let selected = FileIdentity::read(&path)?;
    archive.verify()?;
    for f in &files { f.verify()?; }
    Ok((selected, files))
}

// Retain the actual open inode and original bytes before the first write. No
// rename, permission change, deletion, fresh script execution, or Cargo command
// occurs while altered. Explicit restore runs even on callback panic; Drop is
// a last best-effort restoration guard for unwinding. SIGKILL/power loss cannot
// execute Rust cleanup: the retained backup/journal must then be used by root.
struct RestoreScript {
    file: File,
    identity: FileIdentity,
    bytes: Vec<u8>,
    metadata: (u64, u64, u32, u64),
    backup: FileIdentity,
    attempted: bool,
}
impl RestoreScript {
    fn new(identity: FileIdentity, logs: &Path) -> Result<Self, String> {
        identity.verify()?;
        let mut file = OpenOptions::new().read(true).write(true).open(&identity.path)
            .map_err(|e| e.to_string())?;
        let m = file.metadata().map_err(|e| e.to_string())?;
        need(m.is_file() && !m.permissions().readonly() && m.len() > 0 && m.len() <= 64 * 1024 * 1024,
            "selected script is not bounded writable nonempty regular file")?;
        let mut bytes = Vec::new();
        (&mut file).take(64 * 1024 * 1024 + 1).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
        need(bytes.len() as u64 == m.len() && sha256(&bytes) == identity.sha256,
            "selected script bytes changed before backup")?;
        let backup_path = logs.join("native-leaf-build-script.original.bin");
        write_new(&backup_path, &bytes)?;
        let backup = FileIdentity::read(&backup_path)?;
        need(backup.sha256 == identity.sha256, "selected script backup mismatch")?;
        write_new(&logs.join("native-leaf-build-script.journal.json"),
            &serde_json::to_vec_pretty(&serde_json::json!({
                "state":"BACKED_UP_NOT_YET_MUTATED", "path":identity.path,
                "sha256":identity.sha256, "bytes":bytes.len(), "dev":m.dev(),
                "ino":m.ino(), "mode":m.mode(), "nlink":m.nlink(), "backup":backup.path,
                "scope":"EXCLUSIVE_LANE_ONE_BYTE_CALLBACK_CONTROL_NO_EXECUTION"})).map_err(|e|e.to_string())?)?;
        let result = Self { file, identity, bytes, metadata:(m.dev(),m.ino(),m.mode(),m.nlink()), backup, attempted:false };
        result.verify_restored()?;
        Ok(result)
    }
    fn same_inode(&self) -> Result<(), String> {
        let path = std::fs::symlink_metadata(&self.identity.path).map_err(|e| e.to_string())?;
        let held = self.file.metadata().map_err(|e| e.to_string())?;
        for m in [&path, &held] {
            need(m.is_file() && (m.dev(),m.ino(),m.mode(),m.nlink()) == self.metadata,
                "selected script retained inode/mode/link custody changed")?;
        }
        Ok(())
    }
    fn verify_restored(&self) -> Result<(), String> {
        self.same_inode()?; self.backup.verify()?; self.identity.verify()?;
        need(self.file.metadata().map_err(|e|e.to_string())?.len() == self.bytes.len() as u64,
            "restored script length differs")
    }
    fn alter(&mut self) -> Result<(), String> {
        self.verify_restored()?;
        self.attempted = true; // before even a partial write can occur
        self.file.seek(SeekFrom::Start(0)).map_err(|e|e.to_string())?;
        self.file.write_all(&[self.bytes[0] ^ 1]).map_err(|e|e.to_string())?;
        self.file.sync_all().map_err(|e|e.to_string())?;
        self.same_inode()?;
        need(FileIdentity::read(&self.identity.path)?.sha256 != self.identity.sha256,
            "actual selected script alteration not observed")
    }
    fn restore(&mut self) -> Result<(), String> {
        if self.attempted {
            self.same_inode()?;
            self.file.seek(SeekFrom::Start(0)).map_err(|e|e.to_string())?;
            self.file.write_all(&self.bytes).map_err(|e|e.to_string())?;
            self.file.set_len(self.bytes.len() as u64).map_err(|e|e.to_string())?;
            self.file.sync_all().map_err(|e|e.to_string())?;
        }
        self.verify_restored()?;
        self.attempted = false;
        Ok(())
    }
}
impl Drop for RestoreScript {
    fn drop(&mut self) {
        if self.attempted {
            if let Err(error) = self.restore() {
                eprintln!("NATIVE_LEAF_SCRIPT_RESTORE_FAILED: {error}; backup={}", self.backup.path.display());
            }
        }
    }
}

pub fn observe_and_controls(source:&VerifiedStaticSource, archive:&StaticArchive,
    expected:&SourceExpectation<'_>, inputs:&NativeLeafInputs<'_>, elf:&FileIdentity,
    map:&FileIdentity, runner:&mut Runner, logs:&Path, files:&mut Vec<FileIdentity>) -> Result<(),String> {
    let (script, stream_files) = selected_build_script(archive, expected, runner)?;
    files.extend(stream_files);
    // Preserve successful closure and callback-error closure as distinct facts.
    let mut reached = false;
    source.with_native_leaf_definitions(archive, inputs, elf, map, runner, |observed| {
        reached = true;
        let bytes = format!("{observed:#?}\nSCOPE=IDENTITIES_ONLY_NOT_SYSCALL_ENTRY_OR_READY\n").into_bytes();
        let path = logs.join("native-leaves.not-proven.debug");
        write_new(&path, &bytes)?;
        let file = FileIdentity::read(&path)?;
        need(file.sha256 == sha256(&bytes), "leaf observation serialization identity differs")?;
        files.push(file);
        // Serialize the same live callback value; the existing retained-file
        // ledger and all subsequent owner closes cover this DATA record.
        let bytes = serde_json::to_vec_pretty(observed).map_err(|e| e.to_string())?;
        let path = logs.join("native-leaves.not-proven.json");
        write_new(&path, &bytes)?;
        let file = FileIdentity::read(&path)?;
        need(file.sha256 == sha256(&bytes), "native JSON observation identity differs")?;
        files.push(file);
        Ok(())
    })?;
    need(reached, "native leaf success callback not reached")?;
    archive.verify()?; elf.verify()?; map.verify()?;
    println!("NATIVE_LEAF_SUCCESS_CLOSE=OBSERVED_NOT_PROVEN");
    let sentinel = "actual native leaf callback error control";
    let mut reached = false;
    let callback_error = source.with_native_leaf_definitions(archive, inputs, elf, map, runner, |_| {
        reached = true; Err::<(),String>(sentinel.into())
    });
    need(reached && matches!(&callback_error, Err(e) if e == sentinel),
        &format!("native leaf callback error/close differs: {callback_error:?}"))?;
    archive.verify()?; elf.verify()?; map.verify()?;
    println!("NATIVE_LEAF_CALLBACK_ERROR_CLOSE=EXACT_ERROR_RESTORED_INPUTS");
    let mut guard = RestoreScript::new(script, logs)?;
    let mut reached = false;
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        source.with_native_leaf_definitions(archive, inputs, elf, map, runner, |_| {
            reached = true;
            guard.alter()?;
            Ok(())
        })
    }));
    // Finally: restore BEFORE checking the observed closing result, before any
    // further Cargo operation, and before the enclosing factory source close.
    let restore = guard.restore();
    let restore_file = logs.join("native-leaf-build-script.restore.debug");
    let outcome_text = match &outcome { Ok(result) => format!("{result:?}"), Err(_) => "CALLBACK_PANICKED".into() };
    let detail = format!("RESULT={outcome_text}\nRESTORE={restore:?}\nCALLBACK_REACHED={reached}\nORIGINAL={:?}\n", guard.identity);
    let receipt = write_new(&restore_file, detail.as_bytes());
    restore?; receipt?;
    files.push(guard.identity.clone()); files.push(guard.backup.clone());
    files.push(FileIdentity::read(&restore_file)?);
    files.push(FileIdentity::read(&logs.join("native-leaf-build-script.journal.json"))?);
    archive.verify()?; elf.verify()?; map.verify()?; guard.verify_restored()?;
    let exact = format!("static source transaction close: file identity changed: {}", guard.identity.path.display());
    match outcome {
        Ok(Err(error)) if reached && error == exact => {},
        _ => return Err(format!("native leaf actual archive close control refused elsewhere: {outcome_text}")),
    }
    println!("NATIVE_LEAF_ARCHIVE_CLOSE=EXACT_SELECTED_SCRIPT_CHANGE_REFUSAL_RESTORED");
    for file in files { file.verify()?; }
    Ok(())
}
