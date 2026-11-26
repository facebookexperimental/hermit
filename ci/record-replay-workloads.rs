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
use std::sync::OnceLock;
use std::time::Duration;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

pub const PREPARED_ENV: &str = "HERMIT_PREPARED_RECORD_WORKLOADS";
pub const REQUIRED_ENV: &str = "HERMIT_PREPARED_NEXTEST_REQUIRED";
pub const PACKAGE: &str = "hermetic_infra_hermit_tests";
pub const C_FLAGS: [&str; 3] = ["-O0", "-g", "-pthread"];

pub const C_SOURCES: [(&str, &str); 50] = [
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
    (
        "c_record_replay_chaos_race",
        "tests/c/record_replay_chaos_race.c",
    ),
    (
        "c_record_replay_scm_rights",
        "tests/c/record_replay_scm_rights.c",
    ),
    (
        "c_record_replay_shared_mappings",
        "tests/c/record_replay_shared_mappings.c",
    ),
];

// Alias, Cargo target, repository-relative source. The clock now uses the
// declared Cargo dev profile, rather than the former manual debuginfo=1 build.
pub const RUST_SOURCES: [(&str, &str, &str); 26] = [
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
    (
        "rustbin_bind_connect_race",
        "rustbin_bind_connect_race",
        "tests/rust/bind_connect_race.rs",
    ),
    (
        "rustbin_futex_and_print",
        "rustbin_futex_and_print",
        "tests/rust/futex_and_print.rs",
    ),
    (
        "rustbin_interrogate_tty",
        "rustbin_interrogate_tty",
        "tests/rust/interrogate_tty.rs",
    ),
    (
        "rustbin_mem_race",
        "rustbin_mem_race",
        "tests/rust/mem_race.rs",
    ),
    (
        "rustbin_network_hello_world",
        "rustbin_network_hello_world",
        "tests/rust/network_hello_world.rs",
    ),
    (
        "rustbin_print_clock_nanosleep_monotonic_abs_race",
        "rustbin_print_clock_nanosleep_monotonic_abs_race",
        "tests/rust/print_clock_nanosleep_monotonic_abs_race.rs",
    ),
    (
        "rustbin_print_clock_nanosleep_monotonic_race",
        "rustbin_print_clock_nanosleep_monotonic_race",
        "tests/rust/print_clock_nanosleep_monotonic_race.rs",
    ),
    (
        "rustbin_print_clock_nanosleep_realtime_abs_race",
        "rustbin_print_clock_nanosleep_realtime_abs_race",
        "tests/rust/print_clock_nanosleep_realtime_abs_race.rs",
    ),
    (
        "rustbin_socketpair",
        "rustbin_socketpair",
        "tests/rust/socketpair.rs",
    ),
];

#[derive(Debug)]
pub struct Workload {
    pub name: &'static str,
    pub path: PathBuf,
    sha256: String,
    content: OnceLock<Result<(), String>>,
}

impl Workload {
    /// Whether the workload's bytes are the ones its producer hashed. The first
    /// call in a process hashes them; later calls give its answer. A case calls
    /// it for each workload it takes, so a process hashes only those.
    pub fn check_content(&self) -> Result<(), String> {
        self.content
            .get_or_init(|| {
                let now = sha256(&self.path)?;
                if now != self.sha256 {
                    return Err(format!(
                        "prepared record workload changed: {}: content changed: prepared sha256 {}, now {now}",
                        self.name, self.sha256
                    ));
                }
                Ok(())
            })
            .clone()
    }
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

/// A generation witness, checked for every workload once when WORKLOADS
/// initializes; each workload's bytes are compared with its SHA256 when a case
/// first takes it (`Workload::check_content`). The official caller holds its
/// shared preparation lock through child exit, excluding cooperative producers.
/// This does not provide a per-guest check or exclude uncoordinated path
/// mutations.
/// It leaves out the change time: a link made to the file or removed from it
/// changes that with the link count, and a build executor that gives several
/// concurrent actions one input as hard links of one file does both while a
/// workload runs, whose bytes, inode, size and modification time stay. A
/// rewrite that puts the times back changes none of it either, which the
/// SHA256 sees.
/// The owner and group are the ids the reading process's user namespace maps
/// them to, so they are compared only where the consumer reads them in the
/// producer's user namespace ([`Generation::agrees`]).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Generation {
    dev: u64,
    ino: u64,
    size: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    mtime: i64,
    mtime_nsec: i64,
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
            uid: m.uid(),
            gid: m.gid(),
            mtime: m.mtime(),
            mtime_nsec: m.mtime_nsec(),
        })
    }

    /// Whether `now`, the witness read again, is the generation the producer
    /// witnessed (`self`). Every field counts where `same_user_namespace`;
    /// elsewhere all but the owner and group, which stat reports as the ids
    /// the reader's user namespace maps them to: a case that re-runs itself as
    /// root in a user namespace of its own sees its owner's files owned by 0.
    fn agrees(&self, now: &Self, same_user_namespace: bool) -> bool {
        if same_user_namespace {
            return self == now;
        }
        Self {
            uid: now.uid,
            gid: now.gid,
            ..*self
        } == *now
    }
}

/// A user namespace, by the device and inode of its nsfs file.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct UserNamespace {
    dev: u64,
    ino: u64,
}

impl UserNamespace {
    /// The user namespace this process is in.
    fn of_this_process() -> Result<Self, String> {
        let m = fs::metadata("/proc/self/ns/user").map_err(|e| {
            format!("cannot identify this process's user namespace: /proc/self/ns/user: {e}")
        })?;
        Ok(Self {
            dev: m.dev(),
            ino: m.ino(),
        })
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PreparedWorkload {
    name: String,
    path: PathBuf,
    sha256: String,
    generation: Generation,
}

/// The envelope schema this module writes and reads. Schema 1 had no SHA256,
/// so a consumer cannot check its bytes and refuses it.
const SCHEMA: u32 = 2;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    schema: u32,
    /// The user namespace the producer read the generation witnesses in.
    user_namespace: UserNamespace,
    // A sequence deliberately preserves duplicate names for rejection.
    workloads: Vec<PreparedWorkload>,
}

/// A workload as its producer hands it over: where it is and the SHA256, in
/// lowercase hex, of the bytes the producer prepared there.
#[derive(Clone, Debug)]
pub struct Prepared {
    pub path: PathBuf,
    pub sha256: String,
}

pub fn prepared_envelope(workloads: &BTreeMap<String, Prepared>) -> Result<String, String> {
    if workloads
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>()
        != names().collect()
    {
        return Err(
            "prepared record workload population must contain exactly all 52 aliases".into(),
        );
    }
    let workloads = workloads
        .iter()
        .map(|(name, prepared)| {
            check_sha256(name, &prepared.sha256)?;
            Ok(PreparedWorkload {
                name: name.clone(),
                path: prepared.path.clone(),
                sha256: prepared.sha256.clone(),
                generation: Generation::read(&prepared.path)?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    serde_json::to_string(&Envelope {
        schema: SCHEMA,
        user_namespace: UserNamespace::of_this_process()?,
        workloads,
    })
    .map_err(|e| e.to_string())
}

/// The workloads of a prepared envelope, each still at its witnessed
/// generation, with the SHA256 `Workload::check_content` compares its bytes
/// with.
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
    let invalid = |e: serde_json::Error| format!("invalid {PREPARED_ENV}: {e}");
    let probe: Value = serde_json::from_str(raw).map_err(invalid)?;
    if probe["schema"] != SCHEMA {
        return Err(format!(
            "unsupported prepared record workload schema {}: this consumer reads schema {SCHEMA}, whose workloads carry their SHA256",
            probe["schema"]
        ));
    }
    // From the text, not `probe`: a `Value` keeps the last of a member given
    // twice, which the typed read refuses.
    let envelope: Envelope = serde_json::from_str(raw).map_err(invalid)?;
    let same_user_namespace = UserNamespace::of_this_process()? == envelope.user_namespace;
    let mut paths = BTreeMap::new();
    for entry in envelope.workloads {
        check_sha256(&entry.name, &entry.sha256)?;
        let now = Generation::read(&entry.path)?;
        if !entry.generation.agrees(&now, same_user_namespace) {
            return Err(format!(
                "prepared record workload changed: {}: prepared {:?}, now {now:?}{}",
                entry.name,
                entry.generation,
                if same_user_namespace {
                    ""
                } else {
                    " (owner and group not compared: read in another user namespace than the producer's)"
                }
            ));
        }
        if paths
            .insert(entry.name, (entry.path, entry.sha256))
            .is_some()
        {
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
            .map(|name| {
                let (path, sha256) = paths.remove(name).expect("checked population");
                Workload {
                    name,
                    path,
                    sha256,
                    content: OnceLock::new(),
                }
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

/// The Cargo command `standalone` runs in `repository`, pinned to the
/// toolchain of `cargo` itself: `RUSTC` names the rustc in `cargo`'s own
/// directory. Cargo takes its rustc from `RUSTC`, else `build.rustc`, else
/// PATH, and under rustup PATH holds a proxy that picks a toolchain from
/// `RUSTUP_TOOLCHAIN` or, without it, by directory. With `RUSTUP_TOOLCHAIN`
/// unset (the test binary run directly, not under a rustup `cargo`), a
/// registry dependency that ships its own `rust-toolchain` (never-say-never
/// 6.6.666 pins "1.14.0") was compiled with a toolchain nobody installed
/// ("toolchain '1.14.0-x86_64-unknown-linux-gnu' is not installed").
///
/// The rule is cargo's own toolchain. The pin is real when `cargo` is a
/// toolchain's own binary, which `env!("CARGO")` is under rustup (Cargo
/// records its own executable). Where `cargo` is the rustup proxy, the rustc
/// beside it is the proxy too and pins nothing; that case works because the
/// proxy `cargo` exports `RUSTUP_TOOLCHAIN`. The pin overrides a `RUSTC`
/// the caller set. Refused by name: a `cargo` that is not an absolute path
/// (Buck passes the bare `cargo`), which names no toolchain directory, and
/// a layout with no rustc beside `cargo` (a separately installed cargo),
/// which before used whatever rustc PATH held. Validation never reaches
/// this: it sets `HERMIT_PREPARED_NEXTEST_REQUIRED`, which
/// `require_standalone` refuses.
fn standalone_cargo(cargo: &str, repository: &Path) -> Result<Command, String> {
    if !Path::new(cargo).is_absolute() {
        return Err(format!(
            "cannot pin the record workload build to the toolchain of {cargo}: not an absolute path, so it names no toolchain directory"
        ));
    }
    let rustc = Path::new(cargo).with_file_name("rustc");
    if !rustc.is_file() {
        return Err(format!(
            "cannot pin the record workload build to the toolchain of {cargo}: no rustc beside it at {}",
            rustc.display()
        ));
    }
    let mut command = Command::new(cargo);
    command.current_dir(repository).env("RUSTC", rustc);
    Ok(command)
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
    standalone_ungated(repository, build_root, cargo)
}

/// `standalone` past its `require_standalone` gate, so that a test can drive
/// its Cargo commands with a stand-in `cargo`.
fn standalone_ungated(
    repository: &Path,
    build_root: &Path,
    cargo: &str,
) -> Result<Vec<Workload>, BuildError> {
    fs::create_dir_all(build_root).map_err(|e| e.to_string())?;
    // Not the process id alone: tests that re-run themselves in a new PID
    // namespace are all pid 2 there, so two of them running at once named the
    // same directory and one's cleanup deleted the other's Cargo logs mid-build
    // ("No such file or directory"). The wall-clock nanoseconds keep the name
    // unique across namespaces.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let logs = build_root.join(format!("{CARGO_LOGS_PREFIX}{}-{nanos}", std::process::id()));
    fs::create_dir_all(&logs).map_err(|e| e.to_string())?;
    let logs = Staging(logs);
    let mut metadata = standalone_cargo(cargo, repository)?;
    metadata.args(["metadata", "--format-version=1", "--locked"]);
    let metadata: Value =
        serde_json::from_slice(&output(metadata, &logs.0.join("cargo-metadata"))?)
            .map_err(|e| e.to_string())?;
    let mut build = standalone_cargo(cargo, repository)?;
    build.args([
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
    // The staging directory is renamed into place, so the Staging guard's
    // cleanup at scope end can only find a path that no longer exists:
    // let it drop normally (and free its path allocation) instead of
    // forgetting it, which leaked the allocation on every build.
    let workloads = names()
        .map(|name| {
            let path = population.join(name);
            Ok((
                name.to_owned(),
                Prepared {
                    sha256: sha256(&path)?,
                    path,
                },
            ))
        })
        .collect::<Result<BTreeMap<_, _>, String>>()?;
    let raw = prepared_envelope(&workloads)?;
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
    Ok(hex(&hasher.finalize()))
}

/// The SHA-256, in hex, of the bytes of the file at `path`.
pub fn sha256(path: &Path) -> Result<String, String> {
    use sha2::Digest as _;
    let bytes = fs::read(path)
        .map_err(|e| format!("cannot read record workload {}: {e}", path.display()))?;
    Ok(hex(&sha2::Sha256::digest(bytes)))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Refuses a workload's SHA256 unless it is 64 lowercase hex digits, as
/// `sha256` writes it.
fn check_sha256(name: &str, sha256: &str) -> Result<(), String> {
    if sha256.len() == 64
        && sha256
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Ok(());
    }
    Err(format!(
        "prepared record workload {name} has no SHA256: {sha256:?}"
    ))
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
        /// A stand-in C compiler, for tests of how a population is shared,
        /// rebuilt and pruned rather than of compiling, as `executable`
        /// stands in for the Cargo-built guests. It answers exactly
        /// `--version`, which the population key reads, with a fixed line,
        /// and for exactly `C_FLAGS... SOURCE -o OUTPUT` it installs SOURCE as
        /// an executable OUTPUT, so each workload costs one small process
        /// instead of a compiler pipeline. Any other invocation fails and
        /// names all of its arguments.
        ///
        /// The test executes this file, so this process never holds it open
        /// for writing: a sibling test's spawn would inherit that descriptor
        /// and the exec would fail with ETXTBSY (see
        /// ci/manifest-plan/src/exec_safe_fs.rs, which the other test target
        /// that includes this module does not have). The script is written
        /// under a staging name, and an `install` child creates the
        /// executable.
        fn compiler(&self) -> PathBuf {
            let flags = C_FLAGS
                .iter()
                .enumerate()
                .map(|(index, flag)| format!("[ \"${{{}}}\" = '{flag}' ]", index + 1))
                .collect::<Vec<_>>()
                .join(" && ");
            let script = format!(
                "#!/bin/sh\n\
                 if [ $# -eq 1 ] && [ \"$1\" = --version ]; then echo 'stand-in cc 1'; exit 0; fi\n\
                 if [ $# -eq {total} ] && {flags} && [ \"${{{minus_o}}}\" = -o ]; then\n\
                 exec install -m 755 -- \"${{{source}}}\" \"${{{output}}}\"\n\
                 fi\n\
                 echo \"stand-in cc: expected --version or {expected} SOURCE -o OUTPUT, got: $*\" >&2\n\
                 exit 2\n",
                total = C_FLAGS.len() + 3,
                source = C_FLAGS.len() + 1,
                minus_o = C_FLAGS.len() + 2,
                output = C_FLAGS.len() + 3,
                expected = C_FLAGS.join(" "),
            );
            self.installed("stand-in-cc", &script)
        }
        /// An executable `script` at `relative`, created by an `install`
        /// child process rather than written by this process, so no thread
        /// of this multi-threaded test process can hold it open for writing
        /// when it is executed (ETXTBSY).
        fn installed(&self, relative: &str, script: &str) -> PathBuf {
            let path = self.0.join(relative);
            let staging = self.0.join(format!("{relative}.staging"));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&staging, script).unwrap();
            let status = Command::new("install")
                .args(["-m", "755", "-T", "--"])
                .arg(&staging)
                .arg(&path)
                .status()
                .unwrap();
            assert!(status.success(), "install of {relative}: {status}");
            fs::remove_file(&staging).unwrap();
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

    /// The envelope of `paths`, each with the SHA256 of its bytes now.
    fn envelope(paths: &BTreeMap<String, PathBuf>) -> String {
        prepared_envelope(&prepared(paths)).unwrap()
    }

    fn prepared(paths: &BTreeMap<String, PathBuf>) -> BTreeMap<String, Prepared> {
        paths
            .iter()
            .map(|(name, path)| {
                let sha256 = sha256(path).unwrap();
                let path = path.clone();
                (name.clone(), Prepared { path, sha256 })
            })
            .collect()
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
        assert_eq!(paths.len(), 26);
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
        let raw = envelope(&paths);
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
        schema["schema"] = serde_json::json!(SCHEMA + 1);
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
        let raw = envelope(&paths);
        fs::write(&paths["c_getpid"], "#!/bin/sh\nexit 1\n").unwrap();
        assert!(consume_prepared(true, Some(&raw)).is_err());
        let raw = envelope(&paths);
        let replacement = fixture.executable("replacement");
        fs::rename(replacement, &paths["c_getpid"]).unwrap();
        assert!(consume_prepared(true, Some(&raw)).is_err());
    }

    /// A link made to a prepared workload, or removed from it, changes its
    /// change time and link count, not the workload, as another action's hard
    /// link of a shared input does; a write still changes its witness.
    #[test]
    fn prepared_consumer_keeps_a_workload_another_link_was_made_to() {
        let fixture = Fixture::new();
        let paths = fixture.paths();
        let raw = envelope(&paths);
        let link = fixture.0.join("another-actions-link");
        let taken = |raw: &str| {
            let workloads = consume_prepared(true, Some(raw))?.unwrap();
            workloads
                .iter()
                .find(|w| w.name == "c_getpid")
                .unwrap()
                .check_content()
        };
        fs::hard_link(&paths["c_getpid"], &link).unwrap();
        assert_eq!(taken(&raw), Ok(()));
        fs::remove_file(link).unwrap();
        assert_eq!(taken(&raw), Ok(()));
        fs::File::options()
            .write(true)
            .open(&paths["c_getpid"])
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH)
            .unwrap();
        let refusal = consume_prepared(true, Some(&raw)).unwrap_err();
        assert!(
            refusal.starts_with("prepared record workload changed: c_getpid: prepared "),
            "{refusal}"
        );
    }

    /// A file renamed over a prepared workload changes its witness by the
    /// inode alone when it has the workload's bytes, mode and times.
    #[test]
    fn prepared_consumer_refuses_a_replacement_with_the_workloads_times() {
        let fixture = Fixture::new();
        let paths = fixture.paths();
        let raw = envelope(&paths);
        let original = &paths["c_getpid"];
        let replacement = fixture.executable("replacement");
        let times = fs::metadata(original).unwrap();
        fs::File::options()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_times(
                fs::FileTimes::new()
                    .set_accessed(times.accessed().unwrap())
                    .set_modified(times.modified().unwrap()),
            )
            .unwrap();
        fs::rename(replacement, original).unwrap();
        let refusal = consume_prepared(true, Some(&raw)).unwrap_err();
        assert!(
            refusal.starts_with("prepared record workload changed: c_getpid: prepared "),
            "{refusal}"
        );
    }

    /// Other bytes of the same size written in place, with the access and
    /// modification times put back, leave the whole generation witness as it
    /// was: only the SHA256 tells them apart, when a case takes the workload.
    #[test]
    fn prepared_consumer_refuses_other_bytes_of_the_same_size_and_times() {
        let fixture = Fixture::new();
        let paths = fixture.paths();
        let raw = envelope(&paths);
        let path = &paths["c_getpid"];
        let witnessed = Generation::read(path).unwrap();
        let times = fs::metadata(path).unwrap();
        fs::write(path, "#!/bin/sh\nexit 1\n").unwrap();
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(
                fs::FileTimes::new()
                    .set_accessed(times.accessed().unwrap())
                    .set_modified(times.modified().unwrap()),
            )
            .unwrap();
        assert_eq!(Generation::read(path).unwrap(), witnessed);
        let workloads = consume_prepared(true, Some(&raw)).unwrap().unwrap();
        let [changed, unchanged] = ["c_getpid", "c_getsockopt_null"]
            .map(|name| workloads.iter().find(|w| w.name == name).unwrap());
        let refusal = changed.check_content().unwrap_err();
        assert!(
            refusal.starts_with(
                "prepared record workload changed: c_getpid: content changed: prepared sha256 "
            ),
            "{refusal}"
        );
        assert_eq!(unchanged.check_content(), Ok(()));
    }

    /// An envelope without a workload's SHA256 cannot have its bytes checked:
    /// the consumer refuses schema 1, which had none, and a schema 2 envelope
    /// that leaves one out, and the producer writes neither.
    #[test]
    fn prepared_consumer_refuses_an_envelope_without_a_workloads_sha256() {
        let fixture = Fixture::new();
        let paths = fixture.paths();
        let envelope: Value = serde_json::from_str(&envelope(&paths)).unwrap();
        let mut schema_1 = envelope.clone();
        schema_1["schema"] = serde_json::json!(1);
        for workload in schema_1["workloads"].as_array_mut().unwrap() {
            workload.as_object_mut().unwrap().remove("sha256");
        }
        let refusal = consume_prepared(true, Some(&schema_1.to_string())).unwrap_err();
        assert!(
            refusal.starts_with("unsupported prepared record workload schema 1: "),
            "{refusal}"
        );
        let mut unhashed = envelope.clone();
        unhashed["workloads"][0]
            .as_object_mut()
            .unwrap()
            .remove("sha256");
        let refusal = consume_prepared(true, Some(&unhashed.to_string())).unwrap_err();
        assert!(refusal.contains("missing field `sha256`"), "{refusal}");
        let mut workloads = prepared(&paths);
        let name = envelope["workloads"][0]["name"].as_str().unwrap();
        for sha256 in ["", "C0FFEE", "A".repeat(64).as_str()] {
            workloads.get_mut("c_getpid").unwrap().sha256 = sha256.into();
            assert_eq!(
                prepared_envelope(&workloads).unwrap_err(),
                format!("prepared record workload c_getpid has no SHA256: {sha256:?}")
            );
            let mut malformed = envelope.clone();
            malformed["workloads"][0]["sha256"] = serde_json::json!(sha256);
            for required in [true, false] {
                assert_eq!(
                    consume_prepared(required, Some(&malformed.to_string())).unwrap_err(),
                    format!("prepared record workload {name} has no SHA256: {sha256:?}")
                );
            }
        }
    }

    /// A member given twice is refused wherever it is, in either mode, not read
    /// as its last value: at the root (the producer's user namespace included),
    /// in a workload and in its generation witness.
    #[test]
    fn prepared_consumer_refuses_a_member_given_twice() {
        let fixture = Fixture::new();
        let raw = envelope(&fixture.paths());
        let parsed: Value = serde_json::from_str(&raw).unwrap();
        let first = &parsed["workloads"][0];
        let member = |key: &str| format!("\"{key}\":{}", first[key]);
        let generation = {
            let start = raw.find("\"generation\":").unwrap();
            let end = start + raw[start..].find('}').unwrap() + 1;
            raw[start..end].to_owned()
        };
        let ino = format!("\"ino\":{},", first["generation"]["ino"]);
        let user_namespace = format!("\"user_namespace\":{}", parsed["user_namespace"]);
        for (field, once, twice) in [
            (
                "schema",
                "{\"schema\":2,".to_owned(),
                "{\"schema\":1,\"schema\":2,".to_owned(),
            ),
            (
                "user_namespace",
                user_namespace.clone(),
                format!("{user_namespace},{user_namespace}"),
            ),
            (
                "workloads",
                "\"workloads\":[".to_owned(),
                "\"workloads\":[],\"workloads\":[".to_owned(),
            ),
            (
                "name",
                member("name"),
                format!("\"name\":\"c_getpid\",{}", member("name")),
            ),
            (
                "path",
                member("path"),
                format!("\"path\":\"/elsewhere\",{}", member("path")),
            ),
            (
                "sha256",
                member("sha256"),
                format!("{},{}", member("sha256"), member("sha256")),
            ),
            (
                "generation",
                generation.clone(),
                format!("{generation},{generation}"),
            ),
            ("ino", ino.clone(), format!("\"ino\":0,{ino}")),
        ] {
            let duplicate = raw.replacen(&once, &twice, 1);
            assert_ne!(duplicate, raw, "{field} was not given twice");
            for required in [true, false] {
                let refusal = consume_prepared(required, Some(&duplicate)).unwrap_err();
                assert!(
                    refusal.starts_with(&format!(
                        "invalid {PREPARED_ENV}: duplicate field `{field}`"
                    )),
                    "{field}: {refusal}"
                );
            }
        }
    }

    /// The owner and group count only where the consumer reads them in the
    /// producer's user namespace; every other field counts in any. Here the
    /// envelope's namespace or witness is edited, so this process stands for
    /// a consumer elsewhere; a real case in its own user namespace is
    /// record_replay's
    /// `a_case_in_its_own_user_namespace_runs_only_the_workloads_its_producer_witnessed`.
    #[test]
    fn prepared_consumer_compares_the_owner_and_group_only_in_the_producers_user_namespace() {
        let fixture = Fixture::new();
        let raw = envelope(&fixture.paths());
        let parsed: Value = serde_json::from_str(&raw).unwrap();
        let index = parsed["workloads"]
            .as_array()
            .unwrap()
            .iter()
            .position(|w| w["name"] == "c_getpid")
            .unwrap();
        let edited = |elsewhere: bool, field: &str| {
            let mut edited = parsed.clone();
            if elsewhere {
                let ino = edited["user_namespace"]["ino"].as_u64().unwrap();
                edited["user_namespace"]["ino"] = serde_json::json!(ino + 1);
            }
            if !field.is_empty() {
                let value = edited["workloads"][index]["generation"][field]
                    .as_u64()
                    .unwrap();
                edited["workloads"][index]["generation"][field] = serde_json::json!(value + 1);
            }
            edited.to_string()
        };
        for elsewhere in [false, true] {
            assert!(
                consume_prepared(true, Some(&edited(elsewhere, "")))
                    .unwrap()
                    .is_some(),
                "unchanged, elsewhere {elsewhere}"
            );
        }
        for field in ["uid", "gid"] {
            let refusal = consume_prepared(true, Some(&edited(false, field))).unwrap_err();
            assert!(
                refusal.starts_with("prepared record workload changed: c_getpid: prepared "),
                "{field} in the producer's user namespace: {refusal}"
            );
            assert!(
                consume_prepared(true, Some(&edited(true, field)))
                    .unwrap()
                    .is_some(),
                "{field} in another user namespace"
            );
        }
        for field in ["dev", "ino", "size", "mode", "mtime", "mtime_nsec"] {
            for elsewhere in [false, true] {
                let refusal = consume_prepared(true, Some(&edited(elsewhere, field))).unwrap_err();
                assert!(
                    refusal.starts_with("prepared record workload changed: c_getpid: prepared "),
                    "{field}, elsewhere {elsewhere}: {refusal}"
                );
            }
        }
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

    /// How many threads of this process wait in flock(2) for the lock file
    /// `path`: the "->" lines of /proc/locks with this process's pid and the
    /// file's inode. The device field is not compared: on btrfs /proc/locks
    /// names the filesystem's device while stat(2) names the subvolume's, as
    /// `flock_waiter_on` in ci/manifest-plan/src/nextest_binaries.rs records.
    fn flock_waiters_on(path: &Path) -> usize {
        let inode = format!(":{}", fs::metadata(path).unwrap().ino());
        let pid = std::process::id().to_string();
        fs::read_to_string("/proc/locks")
            .unwrap()
            .lines()
            .filter(|line| {
                let fields = line.split_whitespace().collect::<Vec<_>>();
                fields.get(1) == Some(&"->")
                    && fields.get(2) == Some(&"FLOCK")
                    && fields.get(5) == Some(&pid.as_str())
                    && fields.get(6).is_some_and(|file| file.ends_with(&inode))
            })
            .count()
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
    /// Every build makes the whole population with the stand-in compiler
    /// (`Fixture::compiler`), which installs each source as its executable.
    /// The test holds the lock until both preparations wait for it, so they
    /// always contend, however short a build is. The real compiler's failure
    /// path is covered by
    /// `real_c_compiler_failure_preserves_diagnostics_and_does_not_accept_stale_output`,
    /// and a successful compile of all 50 real sources by every producer run.
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
        // The stand-in makes every C workload of the population at a small
        // fraction of the real compiler's kernel and CPU cost.
        let compiler = fixture.compiler();
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

        // Hold the preparation lock while both preparations start, and
        // release it only once /proc/locks shows both of them waiting for it.
        let lock_path = build_root.join("lock");
        let both = std::thread::scope(|scope| {
            let held = fs::File::create(&lock_path).unwrap();
            held.lock().unwrap();
            let first = scope.spawn(prepare);
            let second = scope.spawn(prepare);
            let deadline = std::time::Instant::now() + Duration::from_secs(60);
            while flock_waiters_on(&lock_path) < 2 {
                assert!(
                    !first.is_finished() && !second.is_finished(),
                    "a preparation finished while the test held the lock"
                );
                assert!(
                    std::time::Instant::now() < deadline,
                    "the two preparations did not both wait for the lock within 60 s"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            // Closing the descriptor releases the lock.
            drop(held);
            [first.join().unwrap(), second.join().unwrap()]
        });
        let populations = directories(&build_root)
            .into_iter()
            .filter(|name| name.starts_with(POPULATION_PREFIX))
            .collect::<Vec<_>>();
        assert_eq!(populations.len(), 1, "{:?}", directories(&build_root));
        assert_eq!(both[0], both[1], "the population was built twice");
        assert_eq!(prepare(), both[0], "the population was not reused");
        // A population whose envelope predates the SHA256 is built again.
        let envelope = build_root.join(&populations[0]).join(ENVELOPE);
        let mut schema_1: Value =
            serde_json::from_str(&fs::read_to_string(&envelope).unwrap()).unwrap();
        schema_1["schema"] = serde_json::json!(1);
        for workload in schema_1["workloads"].as_array_mut().unwrap() {
            workload.as_object_mut().unwrap().remove("sha256");
        }
        fs::write(&envelope, schema_1.to_string()).unwrap();
        prepare();
        let rebuilt: Value = serde_json::from_str(&fs::read_to_string(&envelope).unwrap()).unwrap();
        assert_eq!(
            rebuilt["schema"], SCHEMA,
            "a schema 1 population was reused"
        );
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

    /// `standalone_cargo`'s command carries RUSTC naming the rustc beside an
    /// absolute `cargo`: a stand-in `cargo`, run with RUSTUP_TOOLCHAIN
    /// removed from its environment, prints the RUSTC it receives. That both
    /// of `standalone`'s commands are built this way is
    /// `standalone_runs_both_cargo_commands_pinned`.
    #[test]
    fn standalone_cargo_pins_the_rustc_beside_cargo() {
        let fixture = Fixture::new();
        let cargo = fixture.installed("toolchain/bin/cargo", "#!/bin/sh\nprintf '%s' \"$RUSTC\"\n");
        let rustc = fixture.installed("toolchain/bin/rustc", "#!/bin/sh\nexit 0\n");
        let output = standalone_cargo(cargo.to_str().unwrap(), &fixture.0)
            .unwrap()
            .env_remove("RUSTUP_TOOLCHAIN")
            .output()
            .unwrap();
        assert!(output.status.success(), "stand-in cargo: {output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            rustc.to_str().unwrap()
        );
    }

    /// A `cargo` with no rustc beside it cannot be pinned, and
    /// `standalone_cargo` refuses it by name rather than letting PATH choose
    /// a rustc.
    #[test]
    fn standalone_cargo_refuses_a_cargo_without_rustc_beside_it() {
        let fixture = Fixture::new();
        let cargo = fixture.installed("lonely/bin/cargo", "#!/bin/sh\nexit 0\n");
        let error = standalone_cargo(cargo.to_str().unwrap(), &fixture.0).unwrap_err();
        let expected = fixture.0.join("lonely/bin/rustc");
        assert!(
            error.contains(expected.to_str().unwrap()),
            "the refusal names the missing rustc: {error}"
        );
    }

    /// A `cargo` that is not an absolute path (Buck passes the bare
    /// `cargo`) names no toolchain directory to take rustc from, so
    /// `standalone_cargo` refuses it for that reason, whatever files the
    /// working directory holds.
    #[test]
    fn standalone_cargo_refuses_a_cargo_that_is_not_an_absolute_path() {
        for cargo in ["cargo", "toolchain/bin/cargo"] {
            let error = standalone_cargo(cargo, Path::new("/")).unwrap_err();
            assert!(
                error.contains("not an absolute path"),
                "{cargo}: the refusal names the reason: {error}"
            );
        }
    }

    /// Both of `standalone`'s Cargo commands (`cargo metadata`, then `cargo
    /// build`) carry the pinned RUSTC. The stand-in `cargo` records
    /// "<subcommand> <RUSTC>", answers `metadata` with an empty JSON object
    /// and fails `build`, which ends the preparation before anything is
    /// compiled.
    #[test]
    fn standalone_runs_both_cargo_commands_pinned() {
        let fixture = Fixture::new();
        let calls = fixture.0.join("calls");
        let cargo = fixture.installed(
            "toolchain/bin/cargo",
            &format!(
                "#!/bin/sh\nprintf '%s %s\\n' \"$1\" \"$RUSTC\" >> '{}'\n\
                 if [ \"$1\" = metadata ]; then printf '{{}}'; exit 0; fi\nexit 1\n",
                calls.display()
            ),
        );
        let rustc = fixture.installed("toolchain/bin/rustc", "#!/bin/sh\nexit 0\n");
        let error = standalone_ungated(
            &fixture.0,
            &fixture.0.join("build-root"),
            cargo.to_str().unwrap(),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("record workload compilation failed"),
            "the stand-in build fails: {error}"
        );
        let rustc = rustc.to_str().unwrap();
        assert_eq!(
            fs::read_to_string(&calls).unwrap(),
            format!("metadata {rustc}\nbuild {rustc}\n")
        );
    }
}
