/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The complete record/replay WORKLOADS preparation contract.
//! Included by both the official producer and its test-harness consumer.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

pub const PREPARED_ENV: &str = "HERMIT_PREPARED_RECORD_WORKLOADS";
pub const REQUIRED_ENV: &str = "HERMIT_PREPARED_NEXTEST_REQUIRED";
pub const PACKAGE: &str = "hermetic_infra_hermit_tests";
pub const C_FLAGS: [&str; 3] = ["-O0", "-g", "-pthread"];

pub const C_SOURCES: [(&str, &str); 26] = [
    ("c_getpid", "tests/c/getpid.c"),
    ("c_getsockopt_null", "tests/c/getsockopt_null.c"),
    ("c_setsockopt_replay", "tests/c/record_replay_setsockopt.c"),
    ("c_ioctl_fioclex", "tests/c/ioctl_fioclex.c"),
    ("c_ioctl_siocethtool", "tests/c/ioctl_siocethtool.c"),
    (
        "c_record_replay_fd_close",
        "tests/c/record_replay_fd_close.c",
    ),
    ("c_pidfd_open_self", "tests/c/pidfd_open_self.c"),
    ("c_pidfd_poll_self", "tests/c/pidfd_poll_self.c"),
    (
        "c_recvmsg_scm_rights_mmap",
        "tests/c/recvmsg_scm_rights_mmap.c",
    ),
    (
        "c_record_replay_file_state",
        "tests/c/record_replay_file_state.c",
    ),
    (
        "c_record_replay_poll_partial_copyout",
        "tests/c/record_replay_poll_partial_copyout.c",
    ),
    (
        "c_record_replay_execveat_paths",
        "tests/c/record_replay_execveat_paths.c",
    ),
    (
        "c_record_replay_mkdir_eexist",
        "tests/c/record_replay_mkdir_eexist.c",
    ),
    ("c_clock_exec_continuity", "tests/c/clock_exec_continuity.c"),
    ("c_lseek_seek_cur", "tests/c/record_replay_lseek_seek_cur.c"),
    (
        "c_timerslack_proc_record_replay",
        "tests/c/timerslack_proc_record_replay.c",
    ),
    ("c_sigpipe_siginfo", "tests/c/sigpipe_siginfo.c"),
    ("c_ppoll_readv", "tests/c/ppoll_readv.c"),
    ("c_uname", "tests/c/uname.c"),
    ("c_sysinfo", "tests/c/sysinfo.c"),
    (
        "c_proc_fdinfo_mount_classes",
        "tests/c/proc_fdinfo_mount_classes.c",
    ),
    ("c_wait_on_child", "tests/c/wait_on_child.c"),
    ("c_nanosleep_parallel", "tests/c/nanosleep-par.c"),
    (
        "c_ftruncate_ignore_output_error",
        "tests/c/ftruncate_ignore_output_error.c",
    ),
    (
        "c_write_ignore_output_error",
        "tests/c/write_ignore_output_error.c",
    ),
    ("c_unsupported_syscall", "tests/c/dbt_unsupported_syscall.c"),
];

// Alias, Cargo target, repository-relative source. The clock now uses the
// declared Cargo dev profile, rather than the former manual debuginfo=1 build.
pub const RUST_SOURCES: [(&str, &str, &str); 16] = [
    (
        "rs_clock_gettime",
        "rustbin_clock_gettime",
        "tests/rust/clock_gettime.rs",
    ),
    (
        "rustbin_clock_total_order",
        "rustbin_clock_total_order",
        "tests/rust/clock_total_order.rs",
    ),
    (
        "rustbin_exit_group",
        "rustbin_exit_group",
        "tests/rust/exit_group.rs",
    ),
    (
        "rustbin_sched_yield",
        "rustbin_sched_yield",
        "tests/rust/sched_yield.rs",
    ),
    (
        "rustbin_futex_timeout",
        "rustbin_futex_timeout",
        "tests/rust/futex_timeout.rs",
    ),
    (
        "rustbin_futex_wait_child",
        "rustbin_futex_wait_child",
        "tests/rust/futex_wait_child.rs",
    ),
    (
        "rustbin_futex_wake_some",
        "rustbin_futex_wake_some",
        "tests/rust/futex_wake_some.rs",
    ),
    (
        "rustbin_heap_ptrs",
        "rustbin_heap_ptrs",
        "tests/rust/heap_ptrs.rs",
    ),
    (
        "rustbin_print_nanosleep_race",
        "rustbin_print_nanosleep_race",
        "tests/rust/print_nanosleep_race.rs",
    ),
    (
        "rustbin_nanosleep",
        "rustbin_nanosleep",
        "tests/rust/nanosleep.rs",
    ),
    (
        "rustbin_pipe_basics",
        "rustbin_pipe_basics",
        "tests/rust/pipe_basics.rs",
    ),
    ("rustbin_poll", "rustbin_poll", "tests/rust/poll.rs"),
    (
        "rustbin_poll_spin",
        "rustbin_poll_spin",
        "tests/rust/poll_spin.rs",
    ),
    ("rustbin_rdtsc", "rustbin_rdtsc", "tests/rust/rdtsc.rs"),
    (
        "rustbin_stack_ptr",
        "rustbin_stack_ptr",
        "tests/rust/stack_ptr.rs",
    ),
    (
        "rustbin_thread_random",
        "rustbin_thread_random",
        "tests/rust/thread_random.rs",
    ),
];

#[derive(Debug)]
pub struct Workload {
    pub name: &'static str,
    pub path: PathBuf,
}

pub fn names() -> impl Iterator<Item = &'static str> {
    C_SOURCES
        .into_iter()
        .map(|(name, _)| name)
        .chain(RUST_SOURCES.into_iter().map(|(name, _, _)| name))
}

pub fn source(name: &str) -> Option<&'static str> {
    C_SOURCES
        .into_iter()
        .find_map(|(alias, path)| (alias == name).then_some(path))
        .or_else(|| {
            RUST_SOURCES
                .into_iter()
                .find_map(|(alias, _, path)| (alias == name).then_some(path))
        })
}

pub fn executable(path: &Path, directory: &Path) -> Result<(), String> {
    if !path.is_absolute()
        || !path.starts_with(directory)
        || path
            .canonicalize()
            .map_err(|e| format!("missing workload {}: {e}", path.display()))?
            != path
    {
        return Err(format!(
            "noncanonical or misplaced record workload: {}",
            path.display()
        ));
    }
    let metadata = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !metadata.is_file() || metadata.mode() & 0o111 == 0 {
        return Err(format!(
            "record workload is not a regular executable: {}",
            path.display()
        ));
    }
    Ok(())
}

/// A generation witness supplements the official producer's SHA256 checks.
/// Checked once when WORKLOADS initializes. The official caller holds its shared
/// preparation lock through child exit, excluding cooperative producers. This
/// does not provide a per-guest check or exclude uncoordinated path mutations.
#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Generation {
    dev: u64,
    ino: u64,
    size: u64,
    mode: u32,
    mtime: i64,
    mtime_nsec: i64,
    ctime: i64,
    ctime_nsec: i64,
}

impl Generation {
    fn read(path: &Path) -> Result<Self, String> {
        executable(path, Path::new("/"))?;
        let m = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
        Ok(Self {
            dev: m.dev(),
            ino: m.ino(),
            size: m.len(),
            mode: m.mode(),
            mtime: m.mtime(),
            mtime_nsec: m.mtime_nsec(),
            ctime: m.ctime(),
            ctime_nsec: m.ctime_nsec(),
        })
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PreparedWorkload {
    name: String,
    path: PathBuf,
    generation: Generation,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    schema: u32,
    // A sequence deliberately preserves duplicate names for rejection.
    workloads: Vec<PreparedWorkload>,
}

pub fn prepared_envelope(paths: &BTreeMap<String, PathBuf>) -> Result<String, String> {
    if paths.keys().map(String::as_str).collect::<BTreeSet<_>>() != names().collect() {
        return Err(
            "prepared record workload population must contain exactly all 42 aliases".into(),
        );
    }
    let workloads = paths
        .iter()
        .map(|(name, path)| {
            Ok(PreparedWorkload {
                name: name.clone(),
                path: path.clone(),
                generation: Generation::read(path)?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    serde_json::to_string(&Envelope {
        schema: 1,
        workloads,
    })
    .map_err(|e| e.to_string())
}

pub fn consume_prepared(
    required: bool,
    raw: Option<&str>,
) -> Result<Option<Vec<Workload>>, String> {
    let Some(raw) = raw else {
        return if required {
            Err(format!(
                "official record/replay execution requires {PREPARED_ENV}"
            ))
        } else {
            Ok(None)
        };
    };
    let envelope: Envelope =
        serde_json::from_str(raw).map_err(|e| format!("invalid {PREPARED_ENV}: {e}"))?;
    if envelope.schema != 1 {
        return Err("unsupported prepared record workload schema".into());
    }
    let mut paths = BTreeMap::new();
    for entry in envelope.workloads {
        if entry.generation != Generation::read(&entry.path)? {
            return Err(format!("prepared record workload changed: {}", entry.name));
        }
        if paths.insert(entry.name, entry.path).is_some() {
            return Err("duplicate prepared record workload alias".into());
        }
    }
    if paths.keys().map(String::as_str).collect::<BTreeSet<_>>() != names().collect() {
        return Err(
            "prepared record workload population must contain exactly all 42 aliases".into(),
        );
    }
    Ok(Some(
        names()
            .map(|name| Workload {
                name,
                path: paths.remove(name).expect("checked population"),
            })
            .collect(),
    ))
}

/// Reject hidden fixture compilation in prepared mode, including callers
/// outside WORKLOADS which the current official matrix does not select.
pub fn require_standalone() -> Result<(), String> {
    if std::env::var_os(REQUIRED_ENV).is_some() || std::env::var_os(PREPARED_ENV).is_some() {
        return Err("record/replay fixture compilation is forbidden in prepared execution".into());
    }
    Ok(())
}

fn field<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| format!("Cargo record metadata lacks {key}"))
}

/// Use Cargo's actual package, target, source and executable identities. An
/// apparently usable stale executable is never a substitute for a successful build.
pub fn cargo_executables(
    events: &str,
    cargo: &Value,
    repository: &Path,
    target: &Path,
) -> Result<BTreeMap<String, PathBuf>, String> {
    let packages = cargo["packages"]
        .as_array()
        .ok_or("missing Cargo packages")?
        .iter()
        .filter(|p| p["name"].as_str() == Some(PACKAGE))
        .collect::<Vec<_>>();
    if packages.len() != 1 {
        return Err("record Cargo package is missing or ambiguous".into());
    }
    let package = packages[0];
    if package["source"] != Value::Null
        || Path::new(field(package, "manifest_path")?) != repository.join("tests/Cargo.toml")
    {
        return Err("record Cargo package is not this repository's declared guest package".into());
    }
    let package_id = field(package, "id")?;
    let targets = package["targets"]
        .as_array()
        .ok_or("missing Cargo targets")?;
    for (_, name, source) in RUST_SOURCES {
        let matches = targets
            .iter()
            .filter(|t| t["name"].as_str() == Some(name))
            .collect::<Vec<_>>();
        if matches.len() != 1
            || matches[0]["kind"] != serde_json::json!(["bin"])
            || Path::new(field(matches[0], "src_path")?) != repository.join(source)
        {
            return Err(format!(
                "record Cargo target/source is missing or ambiguous: {name}"
            ));
        }
    }
    let mut result = BTreeMap::new();
    let mut finished = false;
    for line in events.lines() {
        if finished {
            return Err("Cargo record events follow build-finished".into());
        }
        let event: Value =
            serde_json::from_str(line).map_err(|e| format!("invalid Cargo record event: {e}"))?;
        if event["reason"] == "build-finished" {
            if event["success"] != true {
                return Err("Cargo record build did not succeed".into());
            }
            finished = true;
        }
        if event["reason"] != "compiler-artifact" || event["package_id"] != package_id {
            continue;
        }
        let name = field(&event["target"], "name")?;
        let Some((alias, _, source)) = RUST_SOURCES
            .into_iter()
            .find(|(_, target, _)| *target == name)
        else {
            continue;
        };
        if event["target"]["kind"] != serde_json::json!(["bin"])
            || event["profile"]["test"] != false
            || Path::new(field(&event["target"], "src_path")?) != repository.join(source)
        {
            return Err(format!(
                "record Cargo artifact has wrong target/source/profile: {name}"
            ));
        }
        let path = PathBuf::from(field(&event, "executable")?);
        executable(&path, target)?;
        if result.insert(alias.into(), path).is_some() {
            return Err(format!("duplicate record Cargo artifact: {name}"));
        }
    }
    if !finished
        || result.keys().map(String::as_str).collect::<BTreeSet<_>>()
            != RUST_SOURCES.into_iter().map(|(name, _, _)| name).collect()
    {
        return Err("Cargo did not complete all 16 record workload artifacts".into());
    }
    Ok(result)
}

#[derive(Debug)]
pub struct BuildError {
    pub message: String,
    pub status: u8,
}
impl From<String> for BuildError {
    fn from(message: String) -> Self {
        Self { message, status: 2 }
    }
}
impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.message.fmt(f)
    }
}

fn output(mut command: Command, log: &Path) -> Result<Vec<u8>, BuildError> {
    let rendered = format!("{command:?}");
    fs::write(log.with_extension("command"), &rendered).map_err(|e| e.to_string())?;
    let output = command
        .output()
        .map_err(|e| format!("cannot start {rendered}: {e}"))?;
    fs::write(log.with_extension("stdout"), &output.stdout).map_err(|e| e.to_string())?;
    fs::write(log.with_extension("stderr"), &output.stderr).map_err(|e| e.to_string())?;
    let status = output
        .status
        .code()
        .unwrap_or_else(|| 128 + output.status.signal().unwrap_or(1));
    fs::write(log.with_extension("status"), status.to_string()).map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(BuildError {
            status: u8::try_from(status).unwrap_or(1),
            message: format!(
                "record workload compilation failed: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ),
        });
    }
    Ok(output.stdout)
}

pub fn c_compiler() -> Result<PathBuf, String> {
    for directory in
        std::env::split_paths(&std::env::var_os("PATH").ok_or("missing compiler PATH")?)
    {
        let path = directory.join("cc");
        if path.is_file() {
            let path = path.canonicalize().map_err(|e| e.to_string())?;
            executable(&path, Path::new("/"))?;
            return Ok(path);
        }
    }
    Err("cannot find the record workload C compiler cc".into())
}

pub fn compile_c_workloads(
    repository: &Path,
    directory: &Path,
    compiler: &Path,
) -> Result<BTreeMap<String, PathBuf>, BuildError> {
    let mut result = BTreeMap::new();
    for (name, source) in C_SOURCES {
        let path = directory.join(name);
        let mut command = Command::new(compiler);
        command
            .current_dir(repository)
            .args(C_FLAGS)
            .arg(repository.join(source))
            .arg("-o")
            .arg(&path);
        output(command, &directory.join(name))?;
        executable(&path, directory)?;
        result.insert(name.into(), path);
    }
    Ok(result)
}

pub fn copy_cargo_workloads(
    paths: BTreeMap<String, PathBuf>,
    directory: &Path,
) -> Result<BTreeMap<String, PathBuf>, String> {
    paths
        .into_iter()
        .map(|(name, source)| {
            let destination = directory.join(&name);
            fs::copy(&source, &destination)
                .map_err(|e| format!("cannot retain Cargo record workload {name}: {e}"))?;
            executable(&destination, directory)?;
            Ok((name, destination))
        })
        .collect()
}

pub fn standalone(
    repository: &Path,
    directory: &Path,
    cargo: &str,
) -> Result<Vec<Workload>, BuildError> {
    require_standalone()?;
    // A failed compilation never reuses an earlier successful population.
    fs::create_dir(directory).map_err(|e| e.to_string())?;
    let mut metadata = Command::new(cargo);
    metadata
        .current_dir(repository)
        .args(["metadata", "--format-version=1", "--locked"]);
    let metadata: Value =
        serde_json::from_slice(&output(metadata, &directory.join("cargo-metadata"))?)
            .map_err(|e| e.to_string())?;
    let mut build = Command::new(cargo);
    build.current_dir(repository).args([
        "build",
        "--locked",
        "-p",
        PACKAGE,
        "--bins",
        "--message-format=json",
    ]);
    let events = output(build, &directory.join("cargo-build"))?;
    let events = std::str::from_utf8(&events).map_err(|e| e.to_string())?;
    let rust = cargo_executables(
        events,
        &metadata,
        repository,
        Path::new(field(&metadata, "target_directory")?),
    )?;
    let mut paths = compile_c_workloads(repository, directory, &c_compiler()?)?;
    paths.extend(copy_cargo_workloads(rust, directory)?);
    consume_prepared(false, Some(&prepared_envelope(&paths)?))?
        .ok_or_else(|| "missing standalone workload population".to_owned().into())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use super::*;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "hermit-record-preparation-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn executable(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            path
        }
        fn paths(&self) -> BTreeMap<String, PathBuf> {
            names()
                .map(|name| (name.into(), self.executable(name)))
                .collect()
        }
        fn cargo(&self) -> (Value, Vec<Value>) {
            let targets = RUST_SOURCES
                .into_iter()
                .map(|(_, name, source)| {
                    serde_json::json!({
                        "name":name, "kind":["bin"], "src_path":self.0.join(source),
                    })
                })
                .collect::<Vec<_>>();
            let metadata = serde_json::json!({ "target_directory":self.0, "packages":[{
                "name":PACKAGE, "id":"real-guest-package", "source":null,
                "manifest_path":self.0.join("tests/Cargo.toml"), "targets":targets,
            }]});
            let mut events = targets
                .into_iter()
                .map(|target| {
                    serde_json::json!({
                        "reason":"compiler-artifact", "package_id":"real-guest-package",
                        "executable": self.executable(target["name"].as_str().unwrap()),
                        "target":target, "profile":{"test":false,"debuginfo":2},
                    })
                })
                .collect::<Vec<_>>();
            events.push(serde_json::json!({"reason":"build-finished","success":true}));
            (metadata, events)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn jsonl(events: &[Value]) -> String {
        events.iter().map(|event| format!("{event}\n")).collect()
    }

    #[test]
    fn cargo_metadata_drives_exported_paths_and_preserves_clock_alias() {
        let fixture = Fixture::new();
        let (cargo, mut events) = fixture.cargo();
        let nested = fixture.0.join("build/guest/opaque/out");
        fs::create_dir_all(&nested).unwrap();
        let artifact = nested.join("clock-output");
        fs::rename(events[0]["executable"].as_str().unwrap(), &artifact).unwrap();
        events[0]["executable"] = serde_json::json!(artifact);
        let paths = cargo_executables(&jsonl(&events), &cargo, &fixture.0, &fixture.0).unwrap();
        assert_eq!(paths.len(), 16);
        assert_eq!(paths["rs_clock_gettime"], artifact);
        assert!(!paths.contains_key("rustbin_clock_gettime"));
    }

    #[test]
    fn cargo_artifact_contract_rejects_wrong_package_source_kind_profile_and_population() {
        let fixture = Fixture::new();
        let (cargo, events) = fixture.cargo();
        for (pointer, bad) in [
            ("/package_id", serde_json::json!("other-package")),
            ("/target/name", serde_json::json!("other-target")),
            (
                "/target/src_path",
                serde_json::json!(fixture.0.join("other.rs")),
            ),
            ("/target/kind", serde_json::json!(["lib"])),
            ("/profile/test", serde_json::json!(true)),
            ("/executable", Value::Null),
        ] {
            let mut changed = events.clone();
            *changed[0].pointer_mut(pointer).unwrap() = bad;
            assert!(
                cargo_executables(&jsonl(&changed), &cargo, &fixture.0, &fixture.0).is_err(),
                "accepted {pointer}"
            );
        }
        let mut duplicate = events.clone();
        duplicate.insert(0, events[0].clone());
        assert!(cargo_executables(&jsonl(&duplicate), &cargo, &fixture.0, &fixture.0).is_err());
        let mut missing = events.clone();
        missing.remove(0);
        assert!(cargo_executables(&jsonl(&missing), &cargo, &fixture.0, &fixture.0).is_err());
        for pointer in [
            "/packages/0/id",
            "/packages/0/manifest_path",
            "/packages/0/targets/0/src_path",
        ] {
            let mut changed = cargo.clone();
            *changed.pointer_mut(pointer).unwrap() = serde_json::json!("forged");
            assert!(
                cargo_executables(&jsonl(&events), &changed, &fixture.0, &fixture.0).is_err(),
                "accepted {pointer}"
            );
        }
        let mut duplicate = cargo.clone();
        duplicate["packages"]
            .as_array_mut()
            .unwrap()
            .push(cargo["packages"][0].clone());
        assert!(cargo_executables(&jsonl(&events), &duplicate, &fixture.0, &fixture.0).is_err());
    }

    #[test]
    fn cargo_events_require_a_complete_successful_build_even_with_existing_artifacts() {
        let fixture = Fixture::new();
        let (cargo, events) = fixture.cargo();
        let mut failed = events.clone();
        failed.last_mut().unwrap()["success"] = serde_json::json!(false);
        for text in [
            jsonl(&events[..16]),
            jsonl(&failed),
            format!("{}{{", jsonl(&events)),
            "{}\n".into(),
            format!("{}{}", jsonl(&events), jsonl(&events)),
        ] {
            assert!(cargo_executables(&text, &cargo, &fixture.0, &fixture.0).is_err());
        }
    }

    #[test]
    fn artifact_paths_reject_symlinks_nonexecutables_and_outside_target() {
        let fixture = Fixture::new();
        let (cargo, mut events) = fixture.cargo();
        let path = PathBuf::from(events[0]["executable"].as_str().unwrap());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(cargo_executables(&jsonl(&events), &cargo, &fixture.0, &fixture.0).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let link = fixture.0.join("alias");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        events[0]["executable"] = serde_json::json!(link);
        assert!(cargo_executables(&jsonl(&events), &cargo, &fixture.0, &fixture.0).is_err());
        events[0]["executable"] = serde_json::json!(path);
        assert!(
            cargo_executables(
                &jsonl(&events),
                &cargo,
                &fixture.0,
                &fixture.0.join("other-target")
            )
            .is_err()
        );
    }

    #[test]
    fn prepared_consumer_requires_every_alias_and_never_falls_back_on_invalid_data() {
        let fixture = Fixture::new();
        let paths = fixture.paths();
        let raw = prepared_envelope(&paths).unwrap();
        let workloads = consume_prepared(true, Some(&raw)).unwrap().unwrap();
        assert_eq!(
            workloads.iter().map(|w| w.name).collect::<Vec<_>>(),
            names().collect::<Vec<_>>()
        );
        assert!(consume_prepared(false, None).unwrap().is_none());
        assert!(consume_prepared(true, None).is_err());
        let envelope: Value = serde_json::from_str(&raw).unwrap();
        let mut missing = envelope.clone();
        missing["workloads"].as_array_mut().unwrap().pop();
        let mut duplicate = envelope.clone();
        duplicate["workloads"]
            .as_array_mut()
            .unwrap()
            .push(envelope["workloads"][0].clone());
        let mut extra = envelope.clone();
        extra["workloads"][0]["name"] = serde_json::json!("unknown");
        let mut schema = envelope.clone();
        schema["schema"] = serde_json::json!(2);
        for input in [
            missing.to_string(),
            duplicate.to_string(),
            extra.to_string(),
            schema.to_string(),
            "{".into(),
            "{}".into(),
            format!("{}tail", raw),
        ] {
            assert!(consume_prepared(true, Some(&input)).is_err());
            assert!(consume_prepared(false, Some(&input)).is_err());
        }
    }

    #[test]
    fn prepared_consumer_refuses_replaced_or_modified_bytes_at_the_same_path() {
        let fixture = Fixture::new();
        let paths = fixture.paths();
        let raw = prepared_envelope(&paths).unwrap();
        fs::write(&paths["c_getpid"], "#!/bin/sh\nexit 1\n").unwrap();
        assert!(consume_prepared(true, Some(&raw)).is_err());
        let raw = prepared_envelope(&paths).unwrap();
        let replacement = fixture.executable("replacement");
        fs::rename(replacement, &paths["c_getpid"]).unwrap();
        assert!(consume_prepared(true, Some(&raw)).is_err());
    }

    #[test]
    fn real_c_compiler_failure_preserves_diagnostics_and_does_not_accept_stale_output() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.0.join("tests/c")).unwrap();
        fs::write(
            fixture.0.join("tests/c/getpid.c"),
            "#error deliberate_compiler_refusal\n",
        )
        .unwrap();
        let stale = fixture.executable("c_getpid");
        let before = fs::read(&stale).unwrap();
        let error =
            compile_c_workloads(&fixture.0, &fixture.0, &c_compiler().unwrap()).unwrap_err();
        assert_eq!(error.status, 1);
        assert!(error.message.contains("deliberate_compiler_refusal"));
        assert_eq!(fs::read(stale).unwrap(), before);
        assert!(
            fs::read_to_string(fixture.0.join("c_getpid.stderr"))
                .unwrap()
                .contains("deliberate_compiler_refusal")
        );
        assert_eq!(
            fs::read_to_string(fixture.0.join("c_getpid.status")).unwrap(),
            "1"
        );
    }
}
