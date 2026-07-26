/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::ffi::OsStr;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

static HERMIT_RECORD_LOCK: Mutex<()> = Mutex::new(());
static WORKLOADS: OnceLock<Vec<Workload>> = OnceLock::new();

const BASELINE_RECORD_WORKLOADS: [&str; 10] = [
    "c_getpid",
    "c_ioctl_fioclex",
    "c_ioctl_siocethtool",
    "c_recvmsg_scm_rights_mmap",
    "c_ppoll_readv",
    "c_uname",
    "c_sysinfo",
    "c_wait_on_child",
    "c_nanosleep_parallel",
    "rs_clock_gettime",
];

const CARGO_RECORD_GUESTS: [&str; 15] = [
    "rustbin_clock_total_order",
    "rustbin_exit_group",
    "rustbin_sched_yield",
    "rustbin_futex_timeout",
    "rustbin_futex_wait_child",
    "rustbin_futex_wake_some",
    "rustbin_heap_ptrs",
    "rustbin_print_nanosleep_race",
    "rustbin_nanosleep",
    "rustbin_pipe_basics",
    "rustbin_poll",
    "rustbin_poll_spin",
    "rustbin_rdtsc",
    "rustbin_stack_ptr",
    "rustbin_thread_random",
];

#[derive(Debug)]
struct Workload {
    name: &'static str,
    path: PathBuf,
}

fn command_output(mut command: Command, label: &str) -> Output {
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {label}: {rendered}: {error}"));
    assert!(
        output.status.success(),
        "{label} failed: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output
}

fn hermit_record_lock() -> MutexGuard<'static, ()> {
    HERMIT_RECORD_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn compile_c(source: &Path, output: &Path) {
    let mut command = Command::new("cc");
    command
        .args(["-O0", "-g", "-pthread"])
        .arg(source)
        .arg("-o")
        .arg(output);
    command_output(command, "C record workload compilation");
}

// Reuse Cargo's Nix artifact so this test can compile the existing Rust guest
// without a generated manifest edit or a recursive Cargo invocation.
fn nix_rlibs() -> Vec<PathBuf> {
    let dependency_dir = std::env::current_exe()
        .expect("failed to locate the record/replay test binary")
        .parent()
        .expect("integration test binary should be inside Cargo's deps directory")
        .to_path_buf();
    let mut candidates = fs::read_dir(&dependency_dir)
        .expect("failed to read Cargo's dependency directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("libnix-") && name.ends_with(".rlib"))
        })
        .collect::<Vec<_>>();
    candidates.sort();
    assert!(
        !candidates.is_empty(),
        "Cargo did not build a Nix rlib in {}",
        dependency_dir.display()
    );
    candidates
}

fn compile_rust_clock(source: &Path, output: &Path) {
    let dependency_dir = std::env::current_exe()
        .expect("failed to locate the record/replay test binary")
        .parent()
        .expect("integration test binary should be inside Cargo's deps directory")
        .to_path_buf();
    let mut failures = Vec::new();

    for nix_rlib in nix_rlibs() {
        let mut command = Command::new("rustc");
        command
            .args(["--edition=2024", "-C", "debuginfo=1", "-L"])
            .arg(format!("dependency={}", dependency_dir.display()))
            .arg("--extern")
            .arg(format!("nix={}", nix_rlib.display()))
            .arg(source)
            .arg("-o")
            .arg(output);
        let rendered = format!("{command:?}");
        let result = command
            .output()
            .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"));
        if result.status.success() {
            return;
        }
        failures.push(format!(
            "{rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
            result.status,
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr),
        ));
    }

    panic!(
        "failed to compile the Rust clock_gettime workload with any Cargo-built Nix rlib:\n{}",
        failures.join("\n\n")
    );
}

fn cargo_record_workloads(repository: &Path) -> Vec<Workload> {
    let binary_directory = Path::new(env!("CARGO_BIN_EXE_hermit"))
        .parent()
        .expect("Hermit binary should have a parent directory");
    if CARGO_RECORD_GUESTS
        .iter()
        .any(|name| !binary_directory.join(name).is_file())
    {
        let mut command = Command::new(env!("CARGO"));
        command.current_dir(repository).args([
            "build",
            "-p",
            "hermetic_infra_hermit_tests",
            "--bins",
        ]);
        command_output(command, "Cargo record workload compilation");
    }

    CARGO_RECORD_GUESTS
        .iter()
        .map(|&name| {
            let path = binary_directory.join(name);
            assert!(
                path.is_file(),
                "missing Cargo record workload: {}",
                path.display()
            );
            Workload { name, path }
        })
        .collect()
}

fn workloads() -> &'static [Workload] {
    WORKLOADS.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("record-replay-workloads");
        fs::create_dir_all(&build_root).expect("failed to create workload build directory");

        let c_sources = [
            ("c_getpid", "getpid.c"),
            ("c_getsockopt_null", "getsockopt_null.c"),
            ("c_setsockopt_replay", "record_replay_setsockopt.c"),
            ("c_ioctl_fioclex", "ioctl_fioclex.c"),
            ("c_ioctl_siocethtool", "ioctl_siocethtool.c"),
            ("c_record_replay_fd_close", "record_replay_fd_close.c"),
            ("c_recvmsg_scm_rights_mmap", "recvmsg_scm_rights_mmap.c"),
            ("c_record_replay_file_state", "record_replay_file_state.c"),
            ("c_sigpipe_siginfo", "sigpipe_siginfo.c"),
            ("c_ppoll_readv", "ppoll_readv.c"),
            ("c_uname", "uname.c"),
            ("c_sysinfo", "sysinfo.c"),
            ("c_wait_on_child", "wait_on_child.c"),
            ("c_nanosleep_parallel", "nanosleep-par.c"),
        ];
        let mut workloads = c_sources
            .into_iter()
            .map(|(name, source_name)| {
                let path = build_root.join(name);
                compile_c(&repository.join("tests/c").join(source_name), &path);
                Workload { name, path }
            })
            .collect::<Vec<_>>();

        let clock_gettime = Workload {
            name: "rs_clock_gettime",
            path: build_root.join("rs_clock_gettime"),
        };
        compile_rust_clock(
            &repository.join("tests/rust/clock_gettime.rs"),
            &clock_gettime.path,
        );
        workloads.push(clock_gettime);
        workloads.extend(cargo_record_workloads(repository));
        workloads
    })
}

fn workload(name: &str) -> &Workload {
    workloads()
        .iter()
        .find(|workload| workload.name == name)
        .unwrap_or_else(|| panic!("unknown record/replay workload: {name}"))
}

fn record_replay_command(name: &str, program: &Path, args: &[&OsStr]) {
    let data_dir = tempfile::tempdir().expect("failed to create Hermit recording directory");
    // Bound replay as well as recording: --record-timeout only covers the first phase.
    let mut command = Command::new("timeout");
    command
        .env("HERMIT_MODE", "record")
        .args(["--kill-after=5s", "45s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["record", "start", "--verify", "--record-timeout=30"])
        .arg(format!("--data-dir={}", data_dir.path().display()))
        .arg("--")
        .arg(program)
        .args(args);
    let output = command_output(command, &format!("record/replay for {name}"));
    let combined_output = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined_output.contains("Success: replay matched recording."),
        "Hermit did not report deterministic replay for {name}:\n{combined_output}"
    );
}
fn record_then_replay_command(name: &str, program: &Path, args: &[&OsStr]) {
    let data_dir = tempfile::tempdir().expect("failed to create Hermit recording directory");
    let mut record = Command::new("timeout");
    record
        .args(["--kill-after=5s", "45s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=off", "record", "start", "--record-timeout=30"])
        .arg(format!("--data-dir={}", data_dir.path().display()))
        .arg("--")
        .arg(program)
        .args(args);
    let record_output = command_output(record, &format!("recording for {name}"));

    let mut replay = Command::new("timeout");
    replay
        .args(["--kill-after=5s", "45s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=off", "replay", "--autopilot"])
        .arg(format!("--data-dir={}", data_dir.path().display()));
    let replay_output = command_output(replay, &format!("replay for {name}"));

    assert_eq!(
        record_output.stdout, replay_output.stdout,
        "replayed guest stdout did not match the recording for {name}"
    );
}

fn record_replay(workload: &Workload) {
    record_replay_command(workload.name, &workload.path, &[]);
}

fn run_record_replay(name: &str) {
    let _guard = hermit_record_lock();
    record_replay(workload(name));
}

#[test]
fn record_strict_direct_cli_records_and_replays_echo() {
    let _guard = hermit_record_lock();
    let data_dir = tempfile::tempdir().expect("failed to create strict recording directory");

    let mut record = Command::new(env!("CARGO_BIN_EXE_hermit"));
    record
        .args(["--log=off", "record", "--strict", "--data-dir"])
        .arg(data_dir.path())
        .args(["--", "/bin/echo", "hello"]);
    let record_output = command_output(record, "strict direct CLI recording");
    assert_eq!(
        record_output.stdout, b"hello\n",
        "recorded guest stdout changed"
    );

    let mut replay = Command::new(env!("CARGO_BIN_EXE_hermit"));
    replay
        .args(["--log=off", "replay", "--autopilot", "--data-dir"])
        .arg(data_dir.path());
    let replay_output = command_output(replay, "strict direct CLI replay");
    assert_eq!(
        replay_output.stdout, b"hello\n",
        "replayed guest stdout did not match recording"
    );
}

#[test]
fn record_replay_matrix() {
    // Record/replay does not enable PMU-backed preemption, so these workloads
    // also run on GitHub-hosted runners without performance-counter access.
    let _guard = hermit_record_lock();
    for name in BASELINE_RECORD_WORKLOADS {
        record_replay(workload(name));
    }
}

#[test]
fn record_reopened_inherited_and_cloned_file_state() {
    run_record_replay("c_record_replay_file_state");
}

#[test]
fn record_find_directory_tree() {
    let _guard = hermit_record_lock();
    let tree = tempfile::tempdir().expect("failed to create find fixture directory");
    let nested = tree.path().join("nested");
    fs::create_dir(&nested).expect("failed to create nested find fixture directory");
    fs::write(tree.path().join("root.txt"), "root\n").expect("failed to write root find fixture");
    fs::write(nested.join("child.txt"), "child\n").expect("failed to write nested find fixture");

    let find = Path::new("/usr/bin/find");
    assert!(find.is_file(), "GNU find is missing at {}", find.display());
    record_replay_command(
        "find",
        find,
        &[
            tree.path().as_os_str(),
            OsStr::new("-type"),
            OsStr::new("f"),
            OsStr::new("-print"),
        ],
    );
}

#[test]
fn record_mkdir_and_rmdir_side_effects() {
    let _guard = hermit_record_lock();
    let shell = Path::new("/bin/bash");
    assert!(shell.is_file(), "bash is missing at {}", shell.display());

    record_replay_command(
        "mkdir-rmdir-side-effects",
        shell,
        &[
            OsStr::new("-c"),
            OsStr::new(
                "set -euo pipefail; root=/tmp/hermit-record-mkdir-side-effect; rm -rf \"$root\"; mkdir \"$root\"; rmdir \"$root\"; printf 'mkdir-rmdir-side-effect-ok\\n'",
            ),
        ],
    );
}

#[test]
fn record_nested_mkdir_side_effects() {
    let _guard = hermit_record_lock();
    let shell = Path::new("/bin/bash");
    assert!(shell.is_file(), "bash is missing at {}", shell.display());

    record_replay_command(
        "nested-mkdir-side-effects",
        shell,
        &[
            OsStr::new("-c"),
            OsStr::new(
                "set -euo pipefail; root=/tmp/hermit-record-nested-mkdir; rm -rf \"$root\"; mkdir -p \"$root/a/b\"; test -d \"$root/a/b\"; printf 'nested-mkdir-ok\\n'; rm -rf \"$root\"",
            ),
        ],
    );
}

#[test]
fn record_writable_filesystem_side_effects() {
    let _guard = hermit_record_lock();
    let shell = Path::new("/bin/bash");
    assert!(shell.is_file(), "bash is missing at {}", shell.display());

    record_replay_command(
        "writable-filesystem-side-effects",
        shell,
        &[
            OsStr::new("-c"),
            OsStr::new(
                "set -euo pipefail; root=/tmp/hermit-record-filesystem; rm -rf \"$root\"; mkdir \"$root\"; printf 'payload\\n' >\"$root/source\"; cp \"$root/source\" \"$root/copy\"; cmp \"$root/source\" \"$root/copy\"; mv \"$root/copy\" \"$root/moved\"; chmod 640 \"$root/moved\"; touch -t 200001010000 \"$root/moved\"; tar -cf \"$root/archive.tar\" -C \"$root\" moved; tar -tf \"$root/archive.tar\"; rm -rf \"$root\"; printf 'filesystem-side-effects-ok\\n'",
            ),
        ],
    );
}

#[test]
fn record_mkfifo_in_replay_tmp() {
    let _guard = hermit_record_lock();
    let shell = Path::new("/bin/bash");
    assert!(shell.is_file(), "bash is missing at {}", shell.display());

    record_replay_command(
        "mkfifo-in-replay-tmp",
        shell,
        &[
            OsStr::new("-c"),
            OsStr::new(
                "set -euo pipefail; fifo=/tmp/hermit-record-mkfifo; rm -f \"$fifo\"; mkfifo \"$fifo\"; stat -c '%F' \"$fifo\"; rm -f \"$fifo\"",
            ),
        ],
    );
}

/// Regression test for issue #19: a shell that forks and execs an external
/// binary must be able to re-exec that binary during replay. The replay chroot
/// previously contained only the root executable, so the forked child's
/// `execve` failed with `ENOENT` and the guest desynchronized (it took its
/// exec-failure path and issued an extra `newfstatat`).
#[test]
fn record_shell_forked_external_command() {
    let _guard = hermit_record_lock();

    let shell = [Path::new("/bin/bash"), Path::new("/usr/bin/bash")]
        .into_iter()
        .find(|path| path.is_file());
    let Some(shell) = shell else {
        eprintln!("bash is not installed; skipping shell fork/exec record coverage");
        return;
    };

    let true_bin = [Path::new("/bin/true"), Path::new("/usr/bin/true")]
        .into_iter()
        .find(|path| path.is_file())
        .expect("coreutils `true` is missing");

    // `cmd && cmd` forces bash to fork a child for the first command rather than
    // exec-optimizing it in place, so the child's execve exercises the chroot.
    let script = format!("{bin} && {bin}", bin = true_bin.display());
    record_replay_command(
        "shell-fork-exec",
        shell,
        &[OsStr::new("-c"), OsStr::new(&script)],
    );
}

/// Regression test for issue #535: replay must reproduce the SIGPIPE side
/// effect of a recorded write returning EPIPE. Returning the recorded errno
/// without executing the write left `yes` alive after `head` exited, causing
/// excess output and a replay hang.
#[test]
fn record_shell_sigpipe_pipeline() {
    let _guard = hermit_record_lock();

    let shell = [Path::new("/bin/sh"), Path::new("/usr/bin/sh")]
        .into_iter()
        .find(|path| path.is_file());
    let Some(shell) = shell else {
        eprintln!("sh is not installed; skipping SIGPIPE record coverage");
        return;
    };

    let yes = [Path::new("/usr/bin/yes"), Path::new("/bin/yes")]
        .into_iter()
        .find(|path| path.is_file());
    let head = [Path::new("/usr/bin/head"), Path::new("/bin/head")]
        .into_iter()
        .find(|path| path.is_file());
    let (Some(yes), Some(head)) = (yes, head) else {
        eprintln!("coreutils yes/head are not installed; skipping SIGPIPE record coverage");
        return;
    };

    let script = format!("{} | {} -n 1", yes.display(), head.display());
    record_replay_command(
        "shell-sigpipe-pipeline",
        shell,
        &[OsStr::new("-c"), OsStr::new(&script)],
    );
}

#[test]
fn record_shell_pipeline_stdout_matches() {
    let _guard = hermit_record_lock();

    let shell = Path::new("/bin/sh");
    assert!(
        shell.is_file(),
        "POSIX shell is missing at {}",
        shell.display()
    );
    let sort = [Path::new("/usr/bin/sort"), Path::new("/bin/sort")]
        .into_iter()
        .find(|path| path.is_file())
        .expect("coreutils sort is missing");
    let script = format!("printf 'b\\na\\n' | {}", sort.display());
    record_replay_command(
        "shell-pipeline-stdout",
        shell,
        &[OsStr::new("-c"), OsStr::new(&script)],
    );
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-696): Review same-executor replay output backpressure coverage.
#[test]
fn record_large_captured_output_does_not_deadlock() {
    let _guard = hermit_record_lock();

    let head = [Path::new("/usr/bin/head"), Path::new("/bin/head")]
        .into_iter()
        .find(|path| path.is_file())
        .expect("coreutils head is missing");
    record_replay_command(
        "large-captured-stdout",
        head,
        &[
            OsStr::new("-c"),
            OsStr::new("262144"),
            OsStr::new("/dev/zero"),
        ],
    );

    let shell = [Path::new("/bin/sh"), Path::new("/usr/bin/sh")]
        .into_iter()
        .find(|path| path.is_file())
        .expect("POSIX shell is missing");
    let script = format!("{} -c 262144 /dev/zero >&2", head.display());
    record_replay_command(
        "large-captured-stderr",
        shell,
        &[OsStr::new("-c"), OsStr::new(&script)],
    );
}

#[test]
fn record_shell_command_substitution_stdout_matches() {
    let _guard = hermit_record_lock();

    let shell = Path::new("/bin/sh");
    assert!(
        shell.is_file(),
        "POSIX shell is missing at {}",
        shell.display()
    );
    record_replay_command(
        "shell-command-substitution-stdout",
        shell,
        &[
            OsStr::new("-c"),
            OsStr::new("output=$(printf 'captured\\n'); printf '%s\\n' \"$output\""),
        ],
    );
}

#[test]
fn record_shell_redirected_stdout_stays_hidden() {
    let _guard = hermit_record_lock();

    let shell = Path::new("/bin/sh");
    assert!(
        shell.is_file(),
        "POSIX shell is missing at {}",
        shell.display()
    );
    record_replay_command(
        "shell-redirected-stdout",
        shell,
        &[OsStr::new("-c"), OsStr::new("printf FILE_ONLY >/dev/null")],
    );
}

#[test]
fn record_shell_original_output_aliases_and_swaps() {
    let _guard = hermit_record_lock();

    let shell = Path::new("/bin/sh");
    assert!(
        shell.is_file(),
        "POSIX shell is missing at {}",
        shell.display()
    );
    record_replay_command(
        "shell-output-aliases-and-swaps",
        shell,
        &[
            OsStr::new("-c"),
            OsStr::new(
                "exec 3>&1; printf ALIAS >&3; exec 1>&2 2>&3 3>&-; printf TO_STDERR; printf TO_STDOUT >&2",
            ),
        ],
    );
}

#[test]
fn record_curl_version() {
    let _guard = hermit_record_lock();
    let curl = [Path::new("/usr/bin/curl"), Path::new("/usr/local/bin/curl")]
        .into_iter()
        .find(|path| path.is_file());
    let Some(curl) = curl else {
        eprintln!("curl is not installed; skipping record/replay coverage");
        return;
    };

    record_replay_command("curl", curl, &[OsStr::new("--version")]);
}

#[test]
fn record_node_eventfd_epoll_sequence() {
    let _guard = hermit_record_lock();
    let node = [Path::new("/usr/bin/node"), Path::new("/usr/local/bin/node")]
        .into_iter()
        .find(|path| path.is_file());
    let Some(node) = node else {
        eprintln!("node is not installed; skipping eventfd/epoll record coverage");
        return;
    };

    // Node's worker wake order can change the DETLOG order while preserving the
    // recorded event stream, descriptor state, exit status, and guest output.
    record_then_replay_command(
        "node-eventfd-epoll-sequence",
        node,
        &[OsStr::new("-e"), OsStr::new("console.log(42)")],
    );
}

/// Regression test for the SQLite record/replay Mmap-event panic.
///
/// SQLite (via glibc's NSS/dynamic-linker path) issues a `recvmsg` carrying
/// `SCM_RIGHTS`. Before recvmsg was recorded/replayed symmetrically, the
/// `SyscallEvent` stream offset by one, so a later handler's `next_event!`
/// consumed the large file-backed `libsqlite3.so` `MmapEvent` (~650 KiB) and
/// panicked with "expected <X>, found Mmap(..)". The recvmsg record/replay fix
/// realigned the stream; this test exercises the real `sqlite3` binary
/// end-to-end so that regression is caught with the actual workload (the
/// synthetic `c_recvmsg_scm_rights_mmap` guest covers only the mechanism).
#[test]
fn record_sqlite_memory_query() {
    let _guard = hermit_record_lock();
    let sqlite3 = [
        Path::new("/usr/bin/sqlite3"),
        Path::new("/usr/local/bin/sqlite3"),
    ]
    .into_iter()
    .find(|path| path.is_file());
    let Some(sqlite3) = sqlite3 else {
        eprintln!("sqlite3 is not installed; skipping record/replay coverage");
        return;
    };

    record_replay_command(
        "sqlite",
        sqlite3,
        &[OsStr::new(":memory:"), OsStr::new("SELECT 1+1;")],
    );
}

#[test]
fn record_timeout_kills_guest_without_committing_partial_data() {
    let _guard = hermit_record_lock();
    let data_dir = tempfile::tempdir().expect("failed to create Hermit recording directory");
    let started = Instant::now();
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command
        .env("HERMIT_MODE", "record")
        .args(["record", "start", "--record-timeout=1"])
        .arg(format!("--data-dir={}", data_dir.path().display()))
        .args(["--", "/bin/sh", "-c", "while :; do :; done"]);
    let output = command.output().expect("failed to start timeout recording");

    assert!(
        !output.status.success(),
        "timed recording unexpectedly succeeded"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "record timeout took too long: {:?}",
        started.elapsed()
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Recording timed out after 1 seconds"),
        "missing timeout diagnostic:\n{stderr}"
    );
    assert!(
        !data_dir.path().join("last").exists(),
        "timed-out recording was committed"
    );
    let partials = fs::read_dir(data_dir.path().join("tmp"))
        .map(|entries| entries.filter_map(Result::ok).count())
        .unwrap_or(0);
    assert_eq!(partials, 0, "timed-out recording left partial data");
}

/// Builds a `hermit record start --record-timeout` command for a guest that
/// never exits on its own, so the deadline must terminate it.
fn timeout_recording_command(data_dir: &Path, timeout_secs: u32, guest: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command
        .env("HERMIT_MODE", "record")
        .arg("record")
        .arg("start")
        .arg(format!("--record-timeout={timeout_secs}"))
        .arg(format!("--data-dir={}", data_dir.display()))
        .arg("--")
        .args(guest);
    command
}

fn count_tmp_partials(data_dir: &Path) -> usize {
    fs::read_dir(data_dir.join("tmp"))
        .map(|entries| entries.filter_map(Result::ok).count())
        .unwrap_or(0)
}

/// End-to-end guard for the adversarial "inherited blocked SIGALRM" finding: a
/// parent with SIGALRM blocked must not be able to disable the recording
/// deadline. The precise arm/drop mask handling is covered by the
/// `recording_deadline_manages_sigalrm_mask` unit test; this test locks in the
/// observable guarantee that a blocked caller mask still yields a timeout.
#[test]
fn record_timeout_fires_even_when_sigalrm_is_blocked() {
    let _guard = hermit_record_lock();
    let data_dir = tempfile::tempdir().expect("failed to create Hermit recording directory");
    let started = Instant::now();
    let mut command = timeout_recording_command(
        data_dir.path(),
        1,
        &["/bin/sh", "-c", "while :; do :; done"],
    );
    // SAFETY: `pre_exec` runs in the forked child before exec; it only calls
    // async-signal-safe libc signal-mask functions and touches no shared state.
    unsafe {
        command.pre_exec(|| {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGALRM);
            if libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let output = command
        .output()
        .expect("failed to start timeout recording with SIGALRM blocked");

    assert!(
        !output.status.success(),
        "timed recording unexpectedly succeeded with SIGALRM blocked"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "record timeout did not fire with SIGALRM blocked: {:?}",
        started.elapsed()
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Recording timed out after 1 seconds"),
        "missing timeout diagnostic with SIGALRM blocked:\n{stderr}"
    );
    assert!(
        !data_dir.path().join("last").exists(),
        "timed-out recording was committed"
    );
}

/// A recording that times out must never disturb a previously committed
/// recording: `last` and the existing recording directory must be preserved.
#[test]
fn record_timeout_preserves_existing_last() {
    let _guard = hermit_record_lock();
    let data_dir = tempfile::tempdir().expect("failed to create Hermit recording directory");

    // Commit a successful baseline recording so `last` points at real data.
    let mut baseline = Command::new(env!("CARGO_BIN_EXE_hermit"));
    baseline
        .env("HERMIT_MODE", "record")
        .arg("record")
        .arg("start")
        .arg(format!("--data-dir={}", data_dir.path().display()))
        .args(["--", "/bin/true"]);
    let baseline_output = command_output(baseline, "baseline recording");
    let _ = baseline_output;

    let last_path = data_dir.path().join("last");
    let last_before =
        fs::read_to_string(&last_path).expect("baseline recording did not create last");
    assert!(!last_before.is_empty(), "baseline last pointer was empty");

    // Now run a recording that times out.
    let started = Instant::now();
    let mut command = timeout_recording_command(
        data_dir.path(),
        1,
        &["/bin/sh", "-c", "while :; do :; done"],
    );
    let output = command.output().expect("failed to start timeout recording");

    assert!(
        !output.status.success(),
        "timed recording unexpectedly succeeded"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "record timeout took too long: {:?}",
        started.elapsed()
    );
    let last_after = fs::read_to_string(&last_path)
        .expect("last pointer disappeared after a timed-out recording");
    assert_eq!(
        last_before, last_after,
        "timed-out recording overwrote the existing last pointer"
    );
    assert!(
        data_dir.path().join(last_after.trim()).is_dir(),
        "committed recording referenced by last was removed by a timed-out recording"
    );
    assert_eq!(
        count_tmp_partials(data_dir.path()),
        0,
        "timed-out recording left partial data"
    );
}

/// A guest that spawns a long-lived descendant must still be torn down by the
/// deadline. Exiting PID 1 collapses the recording namespace, so the whole
/// process tree dies and `record start` returns promptly instead of hanging on
/// the surviving descendant.
#[test]
fn record_timeout_terminates_descendant_processes() {
    let _guard = hermit_record_lock();
    let data_dir = tempfile::tempdir().expect("failed to create Hermit recording directory");
    let started = Instant::now();
    let mut command = timeout_recording_command(
        data_dir.path(),
        1,
        &["/bin/sh", "-c", "sleep 300 & while :; do :; done"],
    );
    let output = command
        .output()
        .expect("failed to start timeout recording with a descendant");

    assert!(
        !output.status.success(),
        "timed recording unexpectedly succeeded"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "a surviving descendant kept the timeout from returning: {:?}",
        started.elapsed()
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Recording timed out after 1 seconds"),
        "missing timeout diagnostic:\n{stderr}"
    );
    assert!(
        !data_dir.path().join("last").exists(),
        "timed-out recording was committed"
    );
    assert_eq!(
        count_tmp_partials(data_dir.path()),
        0,
        "timed-out recording left partial data"
    );
}

macro_rules! record_replay_tests {
    ($($test_name:ident => $workload_name:literal),+ $(,)?) => {
        $(
            #[test]
            fn $test_name() {
                run_record_replay($workload_name);
            }
        )+
    };
}

record_replay_tests! {
    record_c_getsockopt_null => "c_getsockopt_null",
    record_c_setsockopt_replay => "c_setsockopt_replay",
    record_c_fd_reuse_after_close => "c_record_replay_fd_close",
    record_c_sigpipe_siginfo => "c_sigpipe_siginfo",
    record_rs_clock_total_order => "rustbin_clock_total_order",
    record_rs_exit_group => "rustbin_exit_group",
    record_rs_sched_yield => "rustbin_sched_yield",
    record_rs_futex_timeout => "rustbin_futex_timeout",
    record_rs_futex_wait_child => "rustbin_futex_wait_child",
    record_rs_futex_wake_some => "rustbin_futex_wake_some",
    record_rs_heap_ptrs => "rustbin_heap_ptrs",
    record_rs_print_nanosleep_race => "rustbin_print_nanosleep_race",
    record_rs_nanosleep => "rustbin_nanosleep",
    record_rs_pipe_basics => "rustbin_pipe_basics",
    record_rs_poll => "rustbin_poll",
    record_rs_poll_spin => "rustbin_poll_spin",
    record_rs_rdtsc => "rustbin_rdtsc",
    record_rs_stack_ptr => "rustbin_stack_ptr",
    record_rs_thread_random => "rustbin_thread_random",
}
