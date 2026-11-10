// Copyright (c) Meta Platforms, Inc. and affiliates.
// This source code is licensed under the BSD-style license found in LICENSE.

//! Explicit catalog-owned static CLI selection. This module grants no backend
//! capability: the selected CLI's real adapter still admits its engine cohort.

use std::collections::BTreeMap;
use std::fs::File;
use std::fs::{self};
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutableIdentity {
    pub backend: String,
    pub producer_backend: String,
    pub executable: PathBuf,
    pub sha256: String,
    pub binary_build_sha: String,
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime: i64,
    pub mtime_nsec: i64,
    pub ctime: i64,
    pub ctime_nsec: i64,
}

impl ExecutableIdentity {
    fn matches(&self, metadata: &fs::Metadata) -> bool {
        metadata.is_file()
            && metadata.mode() & 0o111 != 0
            && self.dev == metadata.dev()
            && self.ino == metadata.ino()
            && self.size == metadata.size()
            && self.mtime == metadata.mtime()
            && self.mtime_nsec == metadata.mtime_nsec()
            && self.ctime == metadata.ctime()
            && self.ctime_nsec == metadata.ctime_nsec()
    }

    pub fn validate(&self) -> Result<(), String> {
        let canonical = |text: &str, length| {
            text.len() == length
                && text
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        };
        if self.backend.is_empty()
            || self.producer_backend.is_empty()
            || !self.executable.is_absolute()
            || !canonical(&self.sha256, 64)
            || !(7..=40).contains(&self.binary_build_sha.len())
            || !self.binary_build_sha.bytes().all(|c| c.is_ascii_hexdigit())
            || self.size == 0
        {
            return Err("malformed static executable identity".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct HeldExecutable {
    pub identity: ExecutableIdentity,
    #[serde(skip)]
    file: Arc<File>,
}

impl HeldExecutable {
    pub fn open(identity: ExecutableIdentity) -> Result<Self, String> {
        identity.validate()?;
        if fs::canonicalize(&identity.executable).map_err(|e| e.to_string())? != identity.executable
            || fs::symlink_metadata(&identity.executable)
                .map_err(|e| e.to_string())?
                .file_type()
                .is_symlink()
        {
            return Err("static executable must name its canonical regular file".into());
        }
        let mut file = File::open(&identity.executable).map_err(|e| e.to_string())?;
        if !identity.matches(&file.metadata().map_err(|e| e.to_string())?) {
            return Err("static executable metadata differs from its producer binding".into());
        }
        let mut elf = [0_u8; 4];
        file.read_exact(&mut elf).map_err(|e| e.to_string())?;
        if elf != *b"\x7fELF" {
            return Err("static executable is not an actual ELF".into());
        }
        file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
        let mut hash = Sha256::new();
        let mut bytes = [0_u8; 65536];
        loop {
            let n = file.read(&mut bytes).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            hash.update(&bytes[..n]);
        }
        let actual = format!("{:x}", hash.finalize());
        if actual != identity.sha256 {
            return Err("static executable hash differs from its producer binding".into());
        }
        let held = Self {
            identity,
            file: Arc::new(file),
        };
        held.recheck()?;
        Ok(held)
    }

    pub fn recheck(&self) -> Result<(), String> {
        if fs::canonicalize(&self.identity.executable).map_err(|e| e.to_string())?
            != self.identity.executable
            || !self
                .identity
                .matches(&self.file.metadata().map_err(|e| e.to_string())?)
            || !self.identity.matches(
                &fs::symlink_metadata(&self.identity.executable).map_err(|e| e.to_string())?,
            )
        {
            return Err(
                "static executable path or held inode changed during the invocation".into(),
            );
        }
        Ok(())
    }
}

/// Measure an actual producer-selected ELF. The runner separately probes its
/// typed version before any cell and holds/rechecks this exact inode.
pub fn measure(
    backend: String,
    producer_backend: String,
    executable: PathBuf,
    binary_build_sha: String,
) -> Result<HeldExecutable, String> {
    let executable = fs::canonicalize(executable).map_err(|e| e.to_string())?;
    let before = fs::metadata(&executable).map_err(|e| e.to_string())?;
    let mut file = File::open(&executable).map_err(|e| e.to_string())?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 65536];
    loop {
        let n = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    let identity = ExecutableIdentity {
        backend,
        producer_backend,
        executable,
        binary_build_sha,
        sha256: format!("{:x}", hash.finalize()),
        dev: before.dev(),
        ino: before.ino(),
        size: before.size(),
        mtime: before.mtime(),
        mtime_nsec: before.mtime_nsec(),
        ctime: before.ctime(),
        ctime_nsec: before.ctime_nsec(),
    };
    // HeldExecutable validates the real ELF and both source and held metadata,
    // including every component measured before hashing. No path-only credit.
    HeldExecutable::open(identity)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogBindings {
    schema: u32,
    bindings: Vec<ExecutableIdentity>,
}

pub fn read_bindings(path: Option<&Path>) -> Result<BTreeMap<String, HeldExecutable>, String> {
    let Some(path) = path else {
        return Ok(BTreeMap::new());
    };
    let input: CatalogBindings =
        serde_json::from_slice(&fs::read(path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    if input.schema != 1 || input.bindings.is_empty() {
        return Err("unknown or empty executable-binding catalog".into());
    }
    let mut result = BTreeMap::new();
    for identity in input.bindings {
        let key = identity.backend.clone();
        if result
            .insert(key, HeldExecutable::open(identity)?)
            .is_some()
        {
            return Err("duplicate static backend executable binding".into());
        }
    }
    Ok(result)
}

/// Translate only producer-owned command positions. Guest argv is preserved
/// separately and never searched/replaced. Return the original selector form
/// when normalizing a retained static invocation for the existing validator.
pub fn static_argv(
    argv: &[String],
    original: &Path,
    guest_argv: &[String],
    identity: &ExecutableIdentity,
) -> Result<Vec<String>, String> {
    identity.validate()?;
    let old = original.to_string_lossy();
    let mut result = argv.to_vec();
    // VERIFY_PAIR_SCRIPT names the same binary for comparison and physical
    // work. Its arguments before the guest separator are the command domain.
    if !result.ends_with(guest_argv) {
        return Err("static binding guest suffix mismatch".into());
    }
    let mut guest_start = result.len() - guest_argv.len();
    let mut changed = 0;
    let mut selected = 0;
    let mut i = 0;
    while i < guest_start.min(result.len()) {
        if result[i].as_str() == old.as_ref() {
            result[i] = identity.executable.to_string_lossy().into_owned();
            changed += 1;
            if result
                .get(i + 1..i + 5)
                .is_some_and(|x| x == ["--log", "info", "--backend", identity.backend.as_str()])
            {
                result.drain(i + 3..i + 5);
                selected += 1;
                guest_start -= 2;
            }
        }
        i += 1;
    }
    if changed == 0 || selected != 1 {
        return Err("static binding did not select an actual runner command".into());
    }
    Ok(result)
}

pub fn selector_argv(
    argv: &[String],
    guest_argv: &[String],
    identity: &ExecutableIdentity,
) -> Result<Vec<String>, String> {
    identity.validate()?;
    let executable = identity.executable.to_string_lossy();
    let mut result = argv.to_vec();
    if !result.ends_with(guest_argv) {
        return Err("retained static guest suffix mismatch".into());
    }
    let mut limit = result.len() - guest_argv.len();
    let mut selected = 0;
    let mut i = 0;
    while i < limit.min(result.len()) {
        if result[i].as_str() == executable.as_ref()
            && result
                .get(i + 1..i + 4)
                .is_some_and(|x| x == ["--log", "info", "run"] || x == ["--log", "info", "record"])
        {
            result.splice(i + 3..i + 3, ["--backend".into(), identity.backend.clone()]);
            selected += 1;
            limit += 2;
            i += 2;
        }
        i += 1;
    }
    if selected != 1 {
        return Err("retained static invocation does not use its measured executable".into());
    }
    Ok(result)
}

#[cfg(test)]
pub(crate) fn current_test_binding(backend: &str) -> HeldExecutable {
    measure(
        backend.into(),
        backend.into(),
        std::env::current_exe().unwrap(),
        "b".repeat(40),
    )
    .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn identity() -> ExecutableIdentity {
        ExecutableIdentity {
            backend: "candidate".into(),
            producer_backend: "physical-owner".into(),
            executable: "/catalog/entry".into(),
            sha256: "a".repeat(64),
            binary_build_sha: "b".repeat(40),
            dev: 1,
            ino: 2,
            size: 3,
            mtime: 4,
            mtime_nsec: 5,
            ctime: 6,
            ctime_nsec: 7,
        }
    }
    #[test]
    fn static_binding_changes_only_actual_command_and_roundtrips_the_existing_validator_shape() {
        let original: Vec<String> = [
            "/core/hermit",
            "--log",
            "info",
            "--backend",
            "candidate",
            "run",
            "--strict",
            "--",
            "/core/hermit",
            "--backend",
            "candidate",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let actual = static_argv(
            &original,
            Path::new("/core/hermit"),
            &original[8..],
            &identity(),
        )
        .unwrap();
        assert_eq!(actual[0], "/catalog/entry");
        assert_eq!(&actual[actual.len() - 3..], &original[original.len() - 3..]);
        assert_eq!(
            selector_argv(&actual, &original[8..], &identity()).unwrap()[1..],
            original[1..]
        );
    }
    #[test]
    fn measured_elf_identity_rejects_each_changed_artifact_component() {
        let actual = super::current_test_binding("fixture");
        actual.recheck().unwrap();
        for component in 0..9 {
            let mut id = actual.identity.clone();
            match component {
                0 => id.dev += 1,
                1 => id.ino += 1,
                2 => id.size += 1,
                3 => id.mtime += 1,
                4 => id.mtime_nsec += 1,
                5 => id.ctime += 1,
                6 => id.ctime_nsec += 1,
                7 => id.sha256 = "0".repeat(64),
                _ => id.executable = PathBuf::from("/missing/static-entry"),
            }
            assert!(
                HeldExecutable::open(id).is_err(),
                "accepted changed component {component}"
            );
        }
    }
    #[test]
    fn duplicate_catalog_identity_is_refused_even_for_the_same_actual_elf() {
        let actual = super::current_test_binding("fixture");
        let path = std::env::temp_dir().join(format!(
            "hermit-binding-duplicate-{}.json",
            std::process::id()
        ));
        let mut out = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        use std::io::Write;
        out.write_all(&serde_json::to_vec(&serde_json::json!({"schema":1,"bindings":[actual.identity.clone(),actual.identity]})).unwrap()).unwrap();
        drop(out);
        let result = read_bindings(Some(&path));
        fs::remove_file(path).unwrap();
        assert!(result.unwrap_err().contains("duplicate"));
    }
    #[test]
    fn malformed_and_unprovided_identity_refuse_before_launch() {
        let mut id = identity();
        id.sha256 = "unknown".into();
        assert!(id.validate().is_err());
        assert!(read_bindings(Some(Path::new("/definitely/not/a/binding"))).is_err());
        assert!(HeldExecutable::open(identity()).is_err());
    }
}
