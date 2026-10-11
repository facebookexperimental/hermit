// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

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
use std::time::Duration;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

pub const PREPARED_ENV: &str = "HERMIT_PREPARED_RECORD_WORKLOADS";
pub const REQUIRED_ENV: &str = "HERMIT_PREPARED_NEXTEST_REQUIRED";
pub const PACKAGE: &str = "hermetic_infra_hermit_tests";
pub const C_FLAGS: [&str; 3] = ["-O0", "-g", "-pthread"];

pub const C_SOURCES: [(&str, &str); 47] = [
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
    ("c_record_replay_select", "tests/c/record_replay_select.c"),
    (
        "c_record_replay_forked_stdout_pipe",
        "tests/c/record_replay_forked_stdout_pipe.c",
    ),
    (
        "c_record_replay_sigtimedwait_child_exit",
        "tests/c/record_replay_sigtimedwait_child_exit.c",
    ),
    (
        "c_record_replay_sigsuspend_timer_signal",
        "tests/c/record_replay_sigsuspend_timer_signal.c",
    ),
    (
        "c_record_replay_execveat_paths",
        "tests/c/record_replay_execveat_paths.c",
    ),
    (
        "c_record_replay_path_queries",
        "tests/c/record_replay_path_queries.c",
    ),
    (
        "c_record_replay_fd_metadata",
        "tests/c/record_replay_fd_metadata.c",
    ),
    (
        "c_record_replay_mkdir_eexist",
        "tests/c/record_replay_mkdir_eexist.c",
    ),
    (
        "c_record_replay_mkdirat_parent",
        "tests/c/record_replay_mkdirat_parent.c",
    ),
    (
        "c_record_replay_pipe_clear_nonblock",
        "tests/c/record_replay_pipe_clear_nonblock.c",
    ),
    (
        "c_record_replay_socketpair_blocking_read",
        "tests/c/record_replay_socketpair_blocking_read.c",
    ),
    (
        "c_record_replay_tcp_accept",
        "tests/c/record_replay_tcp_accept.c",
    ),
    (
        "c_record_replay_tcp_accept_threads",
        "tests/c/record_replay_tcp_accept_threads.c",
    ),
    (
        "c_record_replay_accept_copyout",
        "tests/c/record_replay_accept_copyout.c",
    ),
    (
        "c_record_replay_socket_mmsg",
        "tests/c/record_replay_socket_mmsg.c",
    ),
    (
        "c_record_replay_accept_in_turn",
        "tests/c/record_replay_accept_in_turn.c",
    ),
    (
        "c_record_replay_sigchld_ignored_boundary",
        "tests/c/record_replay_sigchld_ignored_boundary.c",
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
    (
        "c_network_replay_tcp_bracket",
        "tests/c/network_replay_tcp_bracket.c",
    ),
    (
        "c_localhost_http_server",
        "tests/compat/localhost_http_server.c",
    ),
    (
        "c_public_record_mount_stdio",
        "tests/c/public_record_mount_stdio.c",
    ),
    (
        "c_record_replay_forked_streams",
        "tests/c/record_replay_forked_streams.c",
    ),
    (
        "c_record_replay_deep_fork_chain",
        "tests/c/record_replay_deep_fork_chain.c",
    ),
    ("c_mount_nscd_order", "tests/c/mount_nscd_order.c"),
];

// Alias, Cargo target, repository-relative source. The clock now uses the
// declared Cargo dev profile, rather than the former manual debuginfo=1 build.
pub const RUST_SOURCES: [(&str, &str, &str); 17] = [
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
    ("rustbin_select", "rustbin_select", "tests/rust/select.rs"),
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
            "prepared record workload population must contain exactly all 52 aliases".into(),
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
            "prepared record workload population must contain exactly all 52 aliases".into(),
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

/// Prepares the workloads outside the official producer, in a population
/// under `build_root` that every test process of this target directory
/// shares (<https://github.com/rrnewton/hermit/issues/3946>).
///
/// A population is keyed by everything that decides its bytes: the C
/// compiler's identity and flags, each C source, and each Cargo-built guest.
/// The first process with a new key builds it into a staging directory and
/// renames it into place; later processes with the same key reuse it. An
/// exclusive lock on `build_root/lock` covers the whole preparation, so at
/// most one process builds at a time. A failed build removes its staging
/// directory, and populations and legacy per-process generations untouched
/// for `PRUNE_AFTER` are removed (a process with an older key may still run).
pub fn standalone(
    repository: &Path,
    build_root: &Path,
    cargo: &str,
) -> Result<Vec<Workload>, BuildError> {
    require_standalone()?;
    fs::create_dir_all(build_root).map_err(|e| e.to_string())?;
    let logs = build_root.join(format!("{CARGO_LOGS_PREFIX}{}", std::process::id()));
    fs::create_dir_all(&logs).map_err(|e| e.to_string())?;
    let logs = Staging(logs);
    let mut metadata = Command::new(cargo);
    metadata
        .current_dir(repository)
        .args(["metadata", "--format-version=1", "--locked"]);
    let metadata: Value =
        serde_json::from_slice(&output(metadata, &logs.0.join("cargo-metadata"))?)
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
    let events = output(build, &logs.0.join("cargo-build"))?;
    let events = std::str::from_utf8(&events).map_err(|e| e.to_string())?;
    let rust = cargo_executables(
        events,
        &metadata,
        repository,
        Path::new(field(&metadata, "target_directory")?),
    )?;
    drop(logs);
    prepare_population(repository, build_root, &c_compiler()?, rust)
}

/// The shared population for C workloads compiled with `compiler` from
/// `repository` and the Cargo-built guests `rust` (see `standalone`).
fn prepare_population(
    repository: &Path,
    build_root: &Path,
    compiler: &Path,
    rust: BTreeMap<String, PathBuf>,
) -> Result<Vec<Workload>, BuildError> {
    let lock = fs::File::create(build_root.join("lock")).map_err(|e| e.to_string())?;
    lock.lock()
        .map_err(|e| format!("cannot lock {}: {e}", build_root.display()))?;
    // Only the lock holder builds, so any staging directory is a dead one's.
    prune(
        build_root,
        |name| name.starts_with(STAGING_PREFIX),
        Duration::ZERO,
    )?;
    let population = build_root.join(format!(
        "{POPULATION_PREFIX}{}",
        population_key(repository, compiler, &rust)?
    ));
    let envelope = population.join(ENVELOPE);
    if let Ok(raw) = fs::read_to_string(&envelope) {
        if let Ok(Some(workloads)) = consume_prepared(false, Some(&raw)) {
            touch(&envelope)?;
            prune_stale(build_root, &population)?;
            return Ok(workloads);
        }
        // Changed or incomplete: build it again.
        fs::remove_dir_all(&population).map_err(|e| e.to_string())?;
    }

    let staging = Staging(build_root.join(format!("{STAGING_PREFIX}{}", std::process::id())));
    // A failed compilation never reuses an earlier successful population.
    fs::create_dir(&staging.0).map_err(|e| e.to_string())?;
    compile_c_workloads(repository, &staging.0, compiler)?;
    copy_cargo_workloads(rust, &staging.0)?;
    if population.exists() {
        fs::remove_dir_all(&population).map_err(|e| e.to_string())?;
    }
    fs::rename(&staging.0, &population).map_err(|e| e.to_string())?;
    std::mem::forget(staging);
    let paths = names()
        .map(|name| (name.to_owned(), population.join(name)))
        .collect::<BTreeMap<_, _>>();
    let raw = prepared_envelope(&paths)?;
    let partial = population.join(format!("{ENVELOPE}.partial"));
    fs::write(&partial, &raw).map_err(|e| e.to_string())?;
    fs::rename(&partial, &envelope).map_err(|e| e.to_string())?;
    prune_stale(build_root, &population)?;
    consume_prepared(false, Some(&raw))?
        .ok_or_else(|| "missing standalone workload population".to_owned().into())
}

/// Directory-name prefixes under a standalone build root.
const POPULATION_PREFIX: &str = "population-";
const STAGING_PREFIX: &str = "staging-";
const CARGO_LOGS_PREFIX: &str = "cargo-logs-";
const LEGACY_PREFIX: &str = "generation-";

/// A complete population's prepared envelope, written last.
const ENVELOPE: &str = "envelope.json";

/// How long an unused population or legacy generation is kept.
const PRUNE_AFTER: Duration = Duration::from_secs(3600);

/// Removes a staging directory unless it was renamed into place.
struct Staging(PathBuf);

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The SHA-256, in hex, of everything that decides a population's bytes.
fn population_key(
    repository: &Path,
    compiler: &Path,
    rust: &BTreeMap<String, PathBuf>,
) -> Result<String, String> {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    let mut field = |label: &str, bytes: &[u8]| {
        hasher.update((label.len() as u64).to_le_bytes());
        hasher.update(label.as_bytes());
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    };
    let version = Command::new(compiler)
        .arg("--version")
        .output()
        .map_err(|e| format!("cannot run {} --version: {e}", compiler.display()))?;
    field("compiler", compiler.as_os_str().as_encoded_bytes());
    field("compiler-version", &version.stdout);
    field("c-flags", C_FLAGS.join(" ").as_bytes());
    for (name, source) in C_SOURCES {
        let bytes = fs::read(repository.join(source))
            .map_err(|e| format!("cannot read record workload source {source}: {e}"))?;
        field(name, &bytes);
    }
    for (name, path) in rust {
        let bytes =
            fs::read(path).map_err(|e| format!("cannot read Cargo record workload {name}: {e}"))?;
        field(name, &bytes);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Marks a population as used now.
fn touch(path: &Path) -> Result<(), String> {
    fs::File::options()
        .append(true)
        .open(path)
        .and_then(|file| file.set_modified(std::time::SystemTime::now()))
        .map_err(|e| format!("cannot touch {}: {e}", path.display()))
}

/// Removes populations other than `current`, legacy generations and Cargo
/// log directories (a killed process leaves its own) unused for
/// `PRUNE_AFTER`.
fn prune_stale(build_root: &Path, current: &Path) -> Result<(), String> {
    let current = current.file_name().and_then(std::ffi::OsStr::to_str);
    prune(
        build_root,
        |name| {
            (name.starts_with(POPULATION_PREFIX) && Some(name) != current)
                || name.starts_with(LEGACY_PREFIX)
                || name.starts_with(CARGO_LOGS_PREFIX)
        },
        PRUNE_AFTER,
    )
}

/// Removes each directory in `build_root` whose name `select` accepts and
/// whose newest of its own and its envelope's modification time is at least
/// `age` old.
fn prune(build_root: &Path, select: impl Fn(&str) -> bool, age: Duration) -> Result<(), String> {
    let now = std::time::SystemTime::now();
    for entry in fs::read_dir(build_root).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        if !name.to_str().is_some_and(&select) || !entry.file_type().is_ok_and(|kind| kind.is_dir())
        {
            continue;
        }
        let path = entry.path();
        let used = [path.clone(), path.join(ENVELOPE)]
            .iter()
            .filter_map(|path| fs::metadata(path).and_then(|m| m.modified()).ok())
            .max();
        if used.is_some_and(|used| now.duration_since(used).unwrap_or_default() < age) {
            continue;
        }
        fs::remove_dir_all(&path).map_err(|e| format!("cannot prune {}: {e}", path.display()))?;
    }
    Ok(())
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
        assert_eq!(paths.len(), 17);
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

    /// The directories in `build_root`, sorted.
    fn directories(build_root: &Path) -> Vec<String> {
        let mut names = fs::read_dir(build_root)
            .unwrap()
            .map(|entry| entry.unwrap())
            .filter(|entry| entry.file_type().unwrap().is_dir())
            .map(|entry| entry.file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    fn age(path: &Path, by: Duration) {
        let then = std::time::SystemTime::now() - by;
        for path in [path.to_owned(), path.join(ENVELOPE)] {
            if path.exists() {
                fs::File::open(&path).unwrap().set_modified(then).unwrap();
            }
        }
    }

    /// Two preparations at once, each with its own lock description as two
    /// test processes have, leave one population, built once; a source change
    /// makes a second; populations and legacy generations unused for
    /// `PRUNE_AFTER` are pruned (<https://github.com/rrnewton/hermit/issues/3946>).
    #[test]
    fn standalone_population_is_shared_across_processes_and_pruned() {
        let fixture = Fixture::new();
        for (_, source) in C_SOURCES {
            let path = fixture.0.join(source);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "int main(void) { return 0; }\n").unwrap();
        }
        let rust = RUST_SOURCES
            .into_iter()
            .map(|(alias, _, _)| (alias.to_owned(), fixture.executable(alias)))
            .collect::<BTreeMap<_, _>>();
        let build_root = fixture.0.join("build-root");
        fs::create_dir(&build_root).unwrap();
        let legacy_old = build_root.join(format!("{LEGACY_PREFIX}1-1"));
        let legacy_new = build_root.join(format!("{LEGACY_PREFIX}2-2"));
        fs::create_dir(&legacy_old).unwrap();
        fs::create_dir(&legacy_new).unwrap();
        age(&legacy_old, PRUNE_AFTER * 2);
        let compiler = c_compiler().unwrap();
        let inode = |path: &Path| fs::metadata(path).unwrap().ino();
        // Each workload's path and the inode it had when its preparation
        // returned: a rebuild replaces the files.
        let prepare = || {
            prepare_population(&fixture.0, &build_root, &compiler, rust.clone())
                .unwrap()
                .into_iter()
                .map(|workload| {
                    let inode = inode(&workload.path);
                    (workload.path, inode)
                })
                .collect::<Vec<_>>()
        };

        let both = std::thread::scope(|scope| {
            let first = scope.spawn(prepare);
            let second = scope.spawn(prepare);
            [first.join().unwrap(), second.join().unwrap()]
        });
        let populations = directories(&build_root)
            .into_iter()
            .filter(|name| name.starts_with(POPULATION_PREFIX))
            .collect::<Vec<_>>();
        assert_eq!(populations.len(), 1, "{:?}", directories(&build_root));
        assert_eq!(both[0], both[1], "the population was built twice");
        assert_eq!(prepare(), both[0], "the population was not reused");
        assert_eq!(
            directories(&build_root),
            [format!("{LEGACY_PREFIX}2-2"), populations[0].clone()],
            "the old legacy generation and every staging directory are gone"
        );

        fs::write(
            fixture.0.join(C_SOURCES[0].1),
            "int main(void) { return 1; }\n",
        )
        .unwrap();
        let changed = prepare();
        let first = build_root.join(&populations[0]);
        assert!(!changed[0].0.starts_with(&first));
        assert_eq!(
            directories(&build_root).len(),
            3,
            "a recent population stays"
        );
        age(&first, PRUNE_AFTER * 2);
        age(&legacy_new, PRUNE_AFTER * 2);
        assert_eq!(prepare(), changed, "the population is reused");
        assert_eq!(
            directories(&build_root),
            [changed[0]
                .0
                .parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()],
            "unused populations and generations are pruned"
        );
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
