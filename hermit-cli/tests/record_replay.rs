/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::ffi::OsStr;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

use hermit::HERMIT_INTERNAL_FAILURE_EXIT;
use reverie::process::Command as ReverieCommand;
use reverie::process::Mount;
use reverie::process::Namespace;

static HERMIT_RECORD_LOCK: Mutex<()> = Mutex::new(());
static WORKLOADS: OnceLock<Vec<Workload>> = OnceLock::new();

#[test]
fn public_record_entry_points_do_not_start_a_nested_runtime() {
    let data = tempfile::tempdir().expect("create recording directory");
    let missing = "/definitely/missing/hermit-public-record-entry";

    let record_error = hermit::record_to(ReverieCommand::new(missing), data.path())
        .expect_err("missing executable should be reported");
    assert!(
        !format!("{record_error:#}").contains("Cannot start a runtime from within a runtime"),
        "record_to created a nested Tokio runtime: {record_error:#}"
    );

    let output_error = hermit::record_with_output(ReverieCommand::new(missing), data.path())
        .expect_err("missing executable should be reported");
    assert!(
        !format!("{output_error:#}").contains("Cannot start a runtime from within a runtime"),
        "record_with_output created a nested Tokio runtime: {output_error:#}"
    );
}

#[test]
fn public_record_uses_the_completed_command_namespace_and_stdio() {
    const INNER: &str = "HERMIT_PUBLIC_RECORD_REPLAY_INNER";
    if std::env::var_os(INNER).is_none() {
        let mut command = ReverieCommand::new(std::env::current_exe().expect("find test binary"));
        command
            .args([
                "--exact",
                "public_record_uses_the_completed_command_namespace_and_stdio",
                "--nocapture",
            ])
            .env(INNER, "1")
            .map_root()
            .unshare(Namespace::MOUNT | Namespace::PID)
            .mount(Mount::proc());
        let output = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build namespace test runtime")
            .block_on(command.output())
            .expect("launch public record/replay namespace");
        assert_eq!(
            output.status,
            reverie::process::ExitStatus::Exited(0),
            "public API record/replay failed in its user namespace:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let _guard = hermit_record_lock();
    let data = tempfile::tempdir().expect("create recording directory");
    let files = tempfile::tempdir().expect("create command mount directory");
    let source = files.path().join("source");
    let target = files.path().join("target");
    fs::write(&source, b"mounted-content\n").expect("write mount source");
    fs::write(&target, b"unmounted-content\n").expect("write mount target");

    let guest = files.path().join("public-record-mount-stdio");
    compile_c(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli must be inside the repository")
            .join("tests/c/public_record_mount_stdio.c"),
        &guest,
    );

    let mut command = ReverieCommand::new(&guest);
    command
        .arg(&target)
        .map_root()
        .mount(Mount::bind(&source, &target))
        .stdin(reverie::process::Stdio::null());

    let recording =
        hermit::record_with_output(command, data.path()).expect("public recording should run");
    assert_eq!(recording.status, reverie::process::ExitStatus::Exited(0));
    let captured = String::from_utf8(recording.stdout).expect("recording stdout must be UTF-8");
    let metadata: serde_json::Value = serde_json::from_slice(
        &fs::read(data.path().join("metadata.json")).expect("read recording metadata"),
    )
    .expect("parse recording metadata");
    assert_eq!(metadata["mountinfo_mount_ids_captured"], true);
    assert!(
        metadata["mountinfo_mount_ids"]
            .as_array()
            .is_some_and(|ids| !ids.is_empty()),
        "completed recording mountinfo order was not persisted"
    );
    assert!(
        metadata["fdinfo_unlisted_mount_ids"]
            .as_array()
            .is_some_and(|ids| !ids.is_empty()),
        "recording-time pipe mount identity was not persisted"
    );
    let replay = hermit::replay_with_output(data.path()).expect("public recording should replay");
    assert_eq!(replay.status, reverie::process::ExitStatus::Exited(0));
    assert_eq!(replay.stdout, captured.as_bytes());

    let mountinfo = section_contents(&captured, "MOUNTINFO");
    let fdinfo = section_contents(&captured, "FDINFO");
    let fdinfo_mount_id = fdinfo
        .lines()
        .find_map(|line| line.strip_prefix("mnt_id:\t"))
        .expect("fdinfo must contain mnt_id");
    assert!(mountinfo.lines().any(|line| {
        line.split_once(' ')
            .is_some_and(|(mount_id, _)| mount_id == fdinfo_mount_id)
    }));
    assert!(captured.contains("mounted-content\n"));
    assert_eq!(section_contents(&captured, "STDIN"), "");
    assert!(!captured.contains("unmounted-content"));
}

#[test]
fn public_record_replay_preserves_distinct_forked_child_streams() {
    const INNER: &str = "HERMIT_FORKED_STREAM_RECORD_REPLAY_INNER";
    if std::env::var_os(INNER).is_none() {
        let mut command = ReverieCommand::new(std::env::current_exe().expect("find test binary"));
        command
            .args([
                "--exact",
                "public_record_replay_preserves_distinct_forked_child_streams",
                "--nocapture",
            ])
            .env(INNER, "1")
            .map_root()
            .unshare(Namespace::MOUNT | Namespace::PID)
            .mount(Mount::proc());
        let output = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build namespace test runtime")
            .block_on(command.output())
            .expect("launch forked-stream record/replay namespace");
        assert_eq!(
            output.status,
            reverie::process::ExitStatus::Exited(0),
            "forked-stream record/replay failed in its user namespace:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let _guard = hermit_record_lock();
    let data = tempfile::tempdir().expect("create recording directory");
    let build = tempfile::tempdir().expect("create guest build directory");
    let guest = build.path().join("record-replay-forked-streams");
    compile_c(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli must be inside the repository")
            .join("tests/c/record_replay_forked_streams.c"),
        &guest,
    );

    let mut command = ReverieCommand::new(&guest);
    command.map_root();
    let recording =
        hermit::record_with_output(command, data.path()).expect("forked guest should record");
    assert_eq!(recording.status, reverie::process::ExitStatus::Exited(0));
    assert_eq!(recording.stdout, b"first-child\nsecond-child\nparent\n");

    let thread_dir = data.path().join("thread");
    let streams = fs::read_dir(&thread_dir)
        .expect("read thread streams")
        .map(|entry| entry.expect("read stream entry"))
        .filter(|entry| !entry.file_name().to_string_lossy().ends_with(".debug"))
        .collect::<Vec<_>>();
    assert_eq!(streams.len(), 3, "root and both child streams must survive");
    for stream in streams {
        let name = stream.file_name();
        let name = name.to_string_lossy();
        assert!(
            name.starts_with("stream-"),
            "unexpected stream name: {name}"
        );
        assert_eq!(name.len(), "stream-".len() + 64);
        assert!(stream.metadata().expect("read stream metadata").len() > 0);
        assert!(
            fs::metadata(thread_dir.join(format!("{name}.debug")))
                .expect("read debug stream metadata")
                .len()
                > 0
        );
    }

    let replay = hermit::replay_with_output(data.path()).expect("forked guest should replay");
    assert_eq!(replay.status, reverie::process::ExitStatus::Exited(0));
    assert_eq!(replay.stdout, recording.stdout);
}

#[test]
fn public_record_replay_handles_a_deep_serial_fork_chain() {
    const INNER: &str = "HERMIT_DEEP_FORK_STREAM_RECORD_REPLAY_INNER";
    if std::env::var_os(INNER).is_none() {
        let mut command = ReverieCommand::new(std::env::current_exe().expect("find test binary"));
        command
            .args([
                "--exact",
                "public_record_replay_handles_a_deep_serial_fork_chain",
                "--nocapture",
            ])
            .env(INNER, "1")
            .map_root()
            .unshare(Namespace::MOUNT | Namespace::PID)
            .mount(Mount::proc());
        let output = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build namespace test runtime")
            .block_on(command.output())
            .expect("launch deep-fork record/replay namespace");
        assert_eq!(
            output.status,
            reverie::process::ExitStatus::Exited(0),
            "deep-fork record/replay failed in its user namespace:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let _guard = hermit_record_lock();
    let data = tempfile::tempdir().expect("create recording directory");
    let build = tempfile::tempdir().expect("create guest build directory");
    let guest = build.path().join("record-replay-deep-fork-chain");
    compile_c(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli must be inside the repository")
            .join("tests/c/record_replay_deep_fork_chain.c"),
        &guest,
    );

    let mut command = ReverieCommand::new(&guest);
    command.map_root();
    let recording =
        hermit::record_with_output(command, data.path()).expect("deep fork chain should record");
    assert_eq!(recording.status, reverie::process::ExitStatus::Exited(0));
    let output = String::from_utf8(recording.stdout.clone()).expect("guest output should be UTF-8");
    assert!(output.starts_with("leaf-125\n"));
    assert!(output.ends_with("parent-0\n"));
    assert_eq!(output.lines().count(), 126);

    let entries = fs::read_dir(data.path().join("thread"))
        .expect("read thread streams")
        .map(|entry| entry.expect("read stream entry"))
        .collect::<Vec<_>>();
    assert_eq!(
        entries.len(),
        252,
        "every process needs data and debug streams"
    );
    assert!(entries.iter().all(|entry| {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        name.len() <= 255 && entry.metadata().is_ok_and(|metadata| metadata.len() > 0)
    }));

    let replay = hermit::replay_with_output(data.path()).expect("deep fork chain should replay");
    assert_eq!(replay.status, reverie::process::ExitStatus::Exited(0));
    assert_eq!(replay.stdout, recording.stdout);
}

fn section_contents<'a>(output: &'a str, name: &str) -> &'a str {
    let start_marker = format!("__{name}__\n");
    let end_marker = format!("__END_{name}__\n");
    output
        .split_once(&start_marker)
        .and_then(|(_, rest)| rest.split_once(&end_marker))
        .map(|(contents, _)| contents)
        .unwrap_or_else(|| panic!("missing {name} section in public recording output"))
}

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

fn assert_recordings_equal_while_host_mountinfo_stable(
    mut record: impl FnMut(&str) -> Vec<u8>,
    label: &str,
) -> Vec<u8> {
    for attempt in 1..=3 {
        let before_first = fs::read("/proc/self/mountinfo").expect("read host mountinfo");
        let first = record("first independent mountinfo recording");
        let after_first = fs::read("/proc/self/mountinfo").expect("read host mountinfo");
        let before_second = fs::read("/proc/self/mountinfo").expect("read host mountinfo");
        let second = record("second independent mountinfo recording");
        let after_second = fs::read("/proc/self/mountinfo").expect("read host mountinfo");
        if before_first == after_first
            && after_first == before_second
            && before_second == after_second
        {
            let first_without_devices = mountinfo_without_device_column(&first)
                .unwrap_or_else(|error| panic!("{label}: first recording: {error}"));
            let second_without_devices = mountinfo_without_device_column(&second)
                .unwrap_or_else(|error| panic!("{label}: second recording: {error}"));
            assert_eq!(
                first_without_devices,
                second_without_devices,
                "{label}: recording output differed outside mountinfo's device column while the host mount table was stable; {}",
                first_mountinfo_row_difference(&first_without_devices, &second_without_devices)
            );
            return first;
        }
        let host_change = [
            ("before first", &before_first, "after first", &after_first),
            ("after first", &after_first, "before second", &before_second),
            (
                "before second",
                &before_second,
                "after second",
                &after_second,
            ),
        ]
        .into_iter()
        .find(|(_, left, _, right)| left != right)
        .map(|(left_label, left, right_label, right)| {
            format!(
                "{left_label} versus {right_label}: {}",
                first_mountinfo_row_difference(left, right)
            )
        });
        if attempt == 3 {
            panic!(
                "{label}: host /proc/self/mountinfo changed around all three recording pairs; last observed change: {}",
                host_change.as_deref().unwrap_or("unavailable")
            );
        }
    }
    unreachable!()
}

fn mountinfo_without_device_column(contents: &[u8]) -> Result<Vec<u8>, &'static str> {
    detcore_model::procfs::parse_mountinfo(contents).ok_or("malformed mountinfo")?;
    let mut normalized = Vec::with_capacity(contents.len());
    for row in contents.split_inclusive(|byte| *byte == b'\n') {
        if row.is_empty() {
            continue;
        }
        let mut spaces = row
            .iter()
            .enumerate()
            .filter_map(|(index, byte)| (*byte == b' ').then_some(index));
        let _after_mount_id = spaces.next().ok_or("mountinfo row has no parent ID")?;
        let device_start = spaces.next().ok_or("mountinfo row has no device field")? + 1;
        let device_end = spaces.next().ok_or("mountinfo row has no root field")?;
        normalized.extend_from_slice(&row[..device_start]);
        normalized.extend_from_slice(b"<major:minor>");
        normalized.extend_from_slice(&row[device_end..]);
    }
    Ok(normalized)
}

fn first_mountinfo_row_difference(left: &[u8], right: &[u8]) -> String {
    let mut left_rows = left.split(|byte| *byte == b'\n');
    let mut right_rows = right.split(|byte| *byte == b'\n');
    for row_index in 0.. {
        let left = left_rows.next();
        let right = right_rows.next();
        if left != right {
            return format!(
                "row {row_index}: {:?} -> {:?}",
                left.map(String::from_utf8_lossy),
                right.map(String::from_utf8_lossy)
            );
        }
    }
    unreachable!("different mountinfo byte strings must have a differing row")
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
            ("c_pidfd_open_self", "pidfd_open_self.c"),
            ("c_pidfd_poll_self", "pidfd_poll_self.c"),
            ("c_recvmsg_scm_rights_mmap", "recvmsg_scm_rights_mmap.c"),
            ("c_record_replay_file_state", "record_replay_file_state.c"),
            (
                "c_record_replay_poll_partial_copyout",
                "record_replay_poll_partial_copyout.c",
            ),
            (
                "c_record_replay_execveat_paths",
                "record_replay_execveat_paths.c",
            ),
            (
                "c_record_replay_mkdir_eexist",
                "record_replay_mkdir_eexist.c",
            ),
            ("c_clock_exec_continuity", "clock_exec_continuity.c"),
            ("c_lseek_seek_cur", "record_replay_lseek_seek_cur.c"),
            (
                "c_timerslack_proc_record_replay",
                "timerslack_proc_record_replay.c",
            ),
            ("c_sigpipe_siginfo", "sigpipe_siginfo.c"),
            ("c_ppoll_readv", "ppoll_readv.c"),
            ("c_uname", "uname.c"),
            ("c_sysinfo", "sysinfo.c"),
            ("c_proc_fdinfo_mount_classes", "proc_fdinfo_mount_classes.c"),
            ("c_wait_on_child", "wait_on_child.c"),
            ("c_nanosleep_parallel", "nanosleep-par.c"),
            (
                "c_ftruncate_ignore_output_error",
                "ftruncate_ignore_output_error.c",
            ),
            ("c_write_ignore_output_error", "write_ignore_output_error.c"),
            ("c_unsupported_syscall", "dbt_unsupported_syscall.c"),
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
    record_replay_command_with_policy(name, program, args, false);
}

fn record_replay_strict_command(name: &str, program: &Path, args: &[&OsStr]) {
    record_replay_command_with_policy(name, program, args, true);
}

fn record_replay_command_with_policy(
    name: &str,
    program: &Path,
    args: &[&OsStr],
    verify_strict: bool,
) {
    let data_dir = tempfile::tempdir().expect("failed to create Hermit recording directory");
    let verdict = data_dir.path().join("verdict.json");
    // Bound replay as well as recording: --record-timeout only covers the first phase.
    let mut command = Command::new("timeout");
    command
        .env("HERMIT_MODE", "record")
        .args(["--kill-after=5s", "45s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["record", "start", "--verify"]);
    if verify_strict {
        command
            .args(["--strict", "--verify-strict"])
            .arg(format!("--verify-json={}", verdict.display()));
    }
    command
        .arg("--record-timeout=30")
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
    if verify_strict {
        let report: serde_json::Value = serde_json::from_slice(
            &fs::read(&verdict).expect("strict record/replay verdict was not written"),
        )
        .expect("strict record/replay verdict is valid JSON");
        assert_eq!(report["verified"], true, "record/replay did not verify");
        assert_eq!(
            report["bitwise_parity"], true,
            "record/replay did not establish canonical parity"
        );
    }
}

fn canonical_record_replay_command(name: &str, program: &Path, args: &[&OsStr]) {
    let data_dir = tempfile::tempdir().expect("failed to create Hermit recording directory");
    let verdict_dir = tempfile::tempdir().expect("failed to create verification directory");
    let verdict_path = verdict_dir.path().join("verify.json");
    let mut command = Command::new("timeout");
    command
        .env("HERMIT_MODE", "record")
        .args(["--kill-after=5s", "45s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=info",
            "--backend=ptrace",
            "record",
            "start",
            "--strict",
            "--verify",
            "--verify-strict",
            "--record-timeout=30",
        ])
        .arg(format!("--verify-json={}", verdict_path.display()))
        .arg(format!("--data-dir={}", data_dir.path().display()))
        .arg("--")
        .arg(program)
        .args(args);
    let output = command_output(command, &format!("canonical record/replay for {name}"));
    let combined_output = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined_output.contains("Success: replay matched recording."),
        "Hermit did not report deterministic replay for {name}:\n{combined_output}"
    );

    let report: serde_json::Value = serde_json::from_slice(
        &fs::read(&verdict_path).expect("canonical record/replay omitted verify JSON"),
    )
    .expect("canonical record/replay verify JSON was invalid");
    assert_eq!(report["verdict"], serde_json::json!("matched"));
    assert_eq!(report["bitwise_parity"], serde_json::json!(true));
    assert!(
        report["compared_log_messages"]["left"]
            .as_u64()
            .is_some_and(|count| count > 0)
    );
    assert!(
        report["compared_log_messages"]["right"]
            .as_u64()
            .is_some_and(|count| count > 0)
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

fn record_then_mutate_and_replay_command<F>(
    name: &str,
    program: &Path,
    args: &[&OsStr],
    record_current_dir: Option<&Path>,
    mutate_host_paths: F,
) where
    F: FnOnce(&Path),
{
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
    if let Some(current_dir) = record_current_dir {
        record.current_dir(current_dir);
    }
    let record_output = command_output(record, &format!("recording for {name}"));

    let recording_id =
        fs::read_to_string(data_dir.path().join("last")).expect("recording did not publish its ID");
    let recording_dir = data_dir.path().join(recording_id.trim());
    mutate_host_paths(&recording_dir);

    let mut replay = Command::new("timeout");
    replay
        .args(["--kill-after=5s", "45s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=off", "replay", "--autopilot"])
        .arg(format!("--data-dir={}", data_dir.path().display()));
    let replay_output = command_output(replay, &format!("replay for {name}"));

    assert_eq!(
        record_output.stdout, replay_output.stdout,
        "replayed guest stdout did not match after host-path mutation for {name}"
    );
}

#[test]
fn record_rejects_initial_executable_without_shebang() {
    let _guard = hermit_record_lock();
    let fixture = tempfile::tempdir().expect("failed to create ENOEXEC fixture");
    let executable = fixture.path().join("missing-shebang");
    fs::write(&executable, "printf 'must-not-run\n'\n").expect("failed to write ENOEXEC fixture");
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))
        .expect("failed to mark ENOEXEC fixture executable");
    let data_dir = tempfile::tempdir().expect("failed to create recording directory");

    let output = Command::new("timeout")
        .args(["--kill-after=5s", "45s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=off", "record", "start", "--record-timeout=30"])
        .arg(format!("--data-dir={}", data_dir.path().display()))
        .arg("--")
        .arg(&executable)
        .output()
        .expect("failed to start ENOEXEC recording");

    assert!(
        !output.status.success(),
        "ENOEXEC recording unexpectedly succeeded"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("does not support execvpe shell fallback")
            && stderr.contains("add an explicit shebang"),
        "ENOEXEC recording did not explain the unsupported fallback:
{stderr}"
    );
    assert!(
        !data_dir.path().join("last").exists(),
        "failed ENOEXEC recording was published as replayable"
    );
}

#[test]
fn replay_bootstrap_uses_snapshot_after_original_executable_is_removed() {
    let _guard = hermit_record_lock();
    let fixture = tempfile::tempdir().expect("failed to create bootstrap replay fixture");
    let executable = fixture.path().join("ephemeral-echo");
    fs::copy("/bin/echo", &executable).expect("failed to copy ephemeral executable");
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))
        .expect("failed to mark ephemeral executable executable");
    let removed = fixture.path().join("removed-echo");

    record_then_mutate_and_replay_command(
        "removed-bootstrap-executable",
        &executable,
        &[OsStr::new("snapshot-bootstrap")],
        None,
        |_| {
            fs::rename(&executable, &removed).expect("failed to remove original executable path");
        },
    );
}

#[test]
fn replay_bootstrap_uses_recorded_custom_interpreter_after_host_mutation() {
    let _guard = hermit_record_lock();
    let fixture = tempfile::tempdir().expect("failed to create interpreter replay fixture");
    let interpreter = fixture.path().join("ephemeral-interpreter");
    fs::copy("/bin/sh", &interpreter).expect("failed to copy custom interpreter");
    fs::set_permissions(&interpreter, fs::Permissions::from_mode(0o755))
        .expect("failed to mark custom interpreter executable");

    let script = fixture.path().join("ephemeral-script");
    fs::write(
        &script,
        format!("#!{}\nprintf '%s\\n' \"$0\"\n", interpreter.display()),
    )
    .expect("failed to write custom-interpreter script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755))
        .expect("failed to mark custom-interpreter script executable");

    let removed_interpreter = fixture.path().join("removed-interpreter");
    let removed_script = fixture.path().join("removed-script");
    record_then_mutate_and_replay_command(
        "removed-bootstrap-interpreter",
        &script,
        &[],
        None,
        |_| {
            fs::rename(&script, &removed_script).expect("failed to remove original script path");
            fs::rename(&interpreter, &removed_interpreter)
                .expect("failed to remove original interpreter path");
        },
    );
}

#[test]
fn replay_bootstrap_records_relative_interpreter_from_symlink_cwd() {
    let _guard = hermit_record_lock();
    let fixture = tempfile::tempdir().expect("failed to create relative-interpreter fixture");
    let real_cwd = fixture.path().join("real-cwd");
    fs::create_dir(&real_cwd).expect("failed to create real guest cwd");
    let linked_cwd = fixture.path().join("linked-cwd");
    std::os::unix::fs::symlink("real-cwd", &linked_cwd)
        .expect("failed to create guest cwd symlink");

    let interpreter = real_cwd.join("interp");
    fs::copy("/bin/sh", &interpreter).expect("failed to copy relative interpreter");
    fs::set_permissions(&interpreter, fs::Permissions::from_mode(0o755))
        .expect("failed to mark relative interpreter executable");
    let script = real_cwd.join("script");
    fs::write(&script, "#!interp\nprintf 'relative-interpreter\\n'\n")
        .expect("failed to write relative-interpreter script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755))
        .expect("failed to mark relative-interpreter script executable");
    let program = linked_cwd.join("script");
    let removed_interpreter = real_cwd.join("removed-interpreter");

    record_then_mutate_and_replay_command(
        "relative-interpreter-symlink-cwd",
        &program,
        &[],
        Some(&real_cwd),
        |recording_dir| {
            let metadata_path = recording_dir.join("metadata.json");
            let mut metadata: serde_json::Value = serde_json::from_reader(
                fs::File::open(&metadata_path).expect("failed to open recorded metadata"),
            )
            .expect("failed to parse recorded metadata");
            metadata["current_dir"] =
                serde_json::Value::String(linked_cwd.to_string_lossy().into_owned());
            serde_json::to_writer_pretty(
                fs::File::create(&metadata_path).expect("failed to rewrite recorded metadata"),
                &metadata,
            )
            .expect("failed to serialize equivalent symlink cwd");
            fs::rename(&interpreter, &removed_interpreter)
                .expect("failed to remove relative interpreter path");
        },
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
fn record_proc_mountinfo_replays_the_captured_read_buffer() {
    let _guard = hermit_record_lock();
    // The inner Recorder stores the raw kernel bytes in ReadV2. The inner
    // Replayer writes those exact bytes back, and the outer Detcore layer then
    // reapplies the recording-time provenance stored in metadata. This checks
    // the whole record-to-replay transport; the next test separately checks
    // independent recordings against different private source paths.
    record_then_replay_command(
        "proc mountinfo captured read buffer",
        Path::new("/bin/cat"),
        &[OsStr::new("/proc/self/mountinfo")],
    );
}

#[test]
fn record_proc_fdinfo_reuses_the_recording_mountinfo_identity_map() {
    let _guard = hermit_record_lock();
    record_then_replay_command(
        "proc fdinfo mount identity",
        Path::new("/bin/cat"),
        &[OsStr::new("/proc/self/fdinfo/1")],
    );
}

#[test]
fn record_mount_namespace_fdinfo_replays_the_observed_unlisted_identity() {
    let _guard = hermit_record_lock();
    let guest = workload("c_proc_fdinfo_mount_classes");
    record_then_replay_command(
        "mount namespace fdinfo identity",
        &guest.path,
        &[OsStr::new("--mount-namespace-only")],
    );
}

#[test]
fn independent_mountinfo_recordings_are_canonical() {
    let _guard = hermit_record_lock();

    let record_once = |label: &str| {
        let data_dir = tempfile::tempdir().expect("recording data directory");
        let host_tmpdir = tempfile::tempdir().expect("recording host TMPDIR");
        let mut command = Command::new("timeout");
        command
            .env("TMPDIR", host_tmpdir.path())
            .args(["--kill-after=5s", "45s"])
            .arg(env!("CARGO_BIN_EXE_hermit"))
            .args(["--log=off", "record", "start", "--strict"])
            .arg(format!("--data-dir={}", data_dir.path().display()))
            .args(["--", "/bin/cat", "/proc/self/mountinfo"]);
        command_output(command, label).stdout
    };

    let first = assert_recordings_equal_while_host_mountinfo_stable(
        record_once,
        "independent mountinfo recordings",
    );
    let text = std::str::from_utf8(&first).expect("mountinfo should be UTF-8");
    assert!(text.contains("/tmpvol/.hermit/etc/group"));
    assert!(!text.contains("/.tmp"));
}

#[test]
fn independent_mountinfo_comparison_ignores_only_the_device_column() {
    let baseline = b"10 1 0:42 / /proc rw,nosuid shared:7 - proc proc rw,nodev\n";
    let other_device = b"10 1 0:99 / /proc rw,nosuid shared:7 - proc proc rw,nodev\n";
    let normalized = mountinfo_without_device_column(baseline).expect("valid baseline row");
    assert_eq!(
        normalized,
        mountinfo_without_device_column(other_device).expect("valid alternate device row")
    );

    for changed in [
        b"11 1 0:42 / /proc rw,nosuid shared:7 - proc proc rw,nodev\n" as &[u8],
        b"10 2 0:42 / /proc rw,nosuid shared:7 - proc proc rw,nodev\n",
        b"10 1 0:42 /sub /proc rw,nosuid shared:7 - proc proc rw,nodev\n",
        b"10 1 0:42 / /other rw,nosuid shared:7 - proc proc rw,nodev\n",
        b"10 1 0:42 / /proc ro,nosuid shared:7 - proc proc rw,nodev\n",
        b"10 1 0:42 / /proc rw,nosuid master:7 - proc proc rw,nodev\n",
        b"10 1 0:42 / /proc rw,nosuid shared:7 - sysfs proc rw,nodev\n",
        b"10 1 0:42 / /proc rw,nosuid shared:7 - proc none rw,nodev\n",
        b"10 1 0:42 / /proc rw,nosuid shared:7 - proc proc ro,nodev\n",
    ] {
        assert_ne!(
            normalized,
            mountinfo_without_device_column(changed).expect("valid changed row"),
            "a non-device mountinfo field was incorrectly ignored: {}",
            String::from_utf8_lossy(changed)
        );
    }
    assert!(
        mountinfo_without_device_column(
            b"10 1 0:42 / /proc rw,nosuid shared:7 + proc proc rw,nodev\n"
        )
        .is_err(),
        "a changed mountinfo separator must fail strict parsing"
    );
}

#[test]
fn record_start_preserves_ordered_nested_user_mounts() {
    let _guard = hermit_record_lock();
    let data_dir = tempfile::tempdir().expect("recording data directory");
    let parent_source = tempfile::tempdir().expect("create parent mount source");
    let child_source = tempfile::tempdir().expect("create child mount source");
    let targets = tempfile::tempdir().expect("create mount targets");
    let parent_target = targets.path().join("stack");
    let child_target = parent_target.join("child");
    fs::create_dir(parent_source.path().join("child")).expect("create covered child path");
    fs::create_dir_all(&child_target).expect("create nested mount targets");

    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command
        .args(["--log=off", "record", "start", "--strict"])
        .arg(format!("--data-dir={}", data_dir.path().display()))
        .arg(format!(
            "--mount=type=bind,source={},target={}",
            child_source.path().display(),
            child_target.display()
        ))
        .arg(format!(
            "--mount=type=bind,source={},target={}",
            parent_source.path().display(),
            parent_target.display()
        ))
        .args(["--", "/bin/cat", "/proc/self/mountinfo"]);
    let output = command_output(command, "recording ordered nested mounts");
    let text = std::str::from_utf8(&output.stdout).expect("mountinfo should be UTF-8");
    for target in [&child_target, &parent_target] {
        assert!(
            text.lines()
                .any(|line| line.split(' ').nth(4) == target.to_str()),
            "record planning dropped {}:\n{text}",
            target.display()
        );
    }
}

#[test]
fn record_start_ordered_var_then_nscd_keeps_run_nscd_hardening() {
    let _guard = hermit_record_lock();
    if !PathBuf::from("/var/run/nscd").is_dir()
        || fs::canonicalize("/var/run").ok() != fs::canonicalize("/run").ok()
    {
        return;
    }

    let data_dir = tempfile::tempdir().expect("recording data directory");
    let user_var = tempfile::tempdir().expect("create user /var source");
    fs::create_dir_all(user_var.path().join("run/nscd")).expect("create user /var nscd path");
    fs::write(user_var.path().join("run/nscd/from-var"), b"from-var\n").expect("write /var marker");
    let later_nscd = tempfile::tempdir().expect("create later nscd source");
    fs::write(later_nscd.path().join("from-later"), b"from-later\n").expect("write later marker");
    let guest = Path::new(env!("CARGO_BIN_EXE_hermit"))
        .parent()
        .expect("Hermit binary should have a parent directory")
        .join("mount-nscd-order-round8");
    compile_c(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli must be inside the repository")
            .join("tests/c/mount_nscd_order.c"),
        &guest,
    );

    let mut record = Command::new(env!("CARGO_BIN_EXE_hermit"));
    record
        .args(["--log=off", "record", "start", "--strict"])
        .arg(format!("--data-dir={}", data_dir.path().display()))
        .arg(format!(
            "--mount=type=bind,source={},target=/var",
            user_var.path().display()
        ))
        .arg(format!(
            "--mount=type=bind,source={},target=/var/run/nscd",
            later_nscd.path().display()
        ))
        .arg("--")
        .arg(&guest);
    let recorded = command_output(record, "record ordered /var and nscd mounts");
    let text = std::str::from_utf8(&recorded.stdout).expect("guest output should be UTF-8");
    assert!(
        text.starts_with("from-later\n"),
        "later user mount was absent: {text}"
    );
    assert!(
        text.lines().any(|line| {
            line.split(' ').nth(4) == Some("/run/nscd") && line.contains("/tmpvol/.hermit/run/nscd")
        }),
        "record planning removed the /run/nscd hardening mount:\n{text}"
    );

    let mut replay = Command::new(env!("CARGO_BIN_EXE_hermit"));
    replay
        .args(["--log=off", "replay", "--autopilot", "--data-dir"])
        .arg(data_dir.path());
    let replayed = command_output(replay, "replay ordered /var and nscd mounts");
    assert_eq!(replayed.stdout, recorded.stdout);
}

#[test]
fn replay_output_sink_failure_aborts_once_without_guest_retry() {
    let _guard = hermit_record_lock();
    let data_dir = tempfile::tempdir().expect("failed to create recording directory");
    let guest = workload("c_write_ignore_output_error");

    let mut record = Command::new(env!("CARGO_BIN_EXE_hermit"));
    record
        .args(["--log=off", "record", "--strict", "--data-dir"])
        .arg(data_dir.path())
        .arg("--")
        .arg(&guest.path);
    let record_output = command_output(record, "recording output-sink failure fixture");
    assert_eq!(record_output.stdout, b"captured-output\n");

    let mut control = Command::new(env!("CARGO_BIN_EXE_hermit"));
    control
        .args(["--log=off", "replay", "--autopilot", "--data-dir"])
        .arg(data_dir.path());
    let control_output = command_output(control, "successful replay-output control");
    assert_eq!(control_output.stdout, record_output.stdout);

    let full = OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .expect("/dev/full is required for replay output failure coverage");
    let started = Instant::now();
    let mut replay = Command::new("timeout");
    replay
        .args(["--kill-after=2s", "10s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=off", "replay", "--autopilot", "--data-dir"])
        .arg(data_dir.path())
        .stdout(Stdio::from(full));
    let rendered = format!("{replay:?}");
    let replay_output = replay
        .output()
        .unwrap_or_else(|error| panic!("failed to start replay: {rendered}: {error}"));
    let elapsed = started.elapsed();
    let stderr = String::from_utf8_lossy(&replay_output.stderr);

    assert_eq!(
        replay_output.status.code(),
        Some(HERMIT_INTERNAL_FAILURE_EXIT),
        "replay output failure did not terminate as one tool error: {rendered}\n\
         elapsed: {elapsed:?}\nstderr:\n{stderr}"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "replay output failure retried until the watchdog: {elapsed:?}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("No space left on device"),
        "replay did not report the output sink cause:\n{stderr}"
    );
    assert_eq!(
        stderr
            .lines()
            .filter(|line| line.contains("Error:"))
            .count(),
        1,
        "replay output failure was not reported exactly once:\n{stderr}"
    );
    assert!(
        !stderr.contains("panicked") && !stderr.contains("desync") && !stderr.contains("expected"),
        "replay output failure escaped as a panic or stream divergence:\n{stderr}"
    );
}

#[test]
fn replay_captured_output_ftruncate_failure_aborts_without_panicking() {
    let _guard = hermit_record_lock();
    let data_dir = tempfile::tempdir().expect("failed to create recording directory");
    let guest = workload("c_ftruncate_ignore_output_error");
    let mut recorded_stdout = tempfile::tempfile().expect("failed to create regular stdout");
    recorded_stdout
        .write_all(b"must be truncated")
        .expect("failed to seed regular stdout");

    let mut record = Command::new(env!("CARGO_BIN_EXE_hermit"));
    record
        .args(["--log=off", "record", "--strict", "--data-dir"])
        .arg(data_dir.path())
        .arg("--")
        .arg(&guest.path)
        .stdout(Stdio::from(
            recorded_stdout
                .try_clone()
                .expect("failed to clone regular stdout"),
        ));
    command_output(record, "recording captured-output ftruncate fixture");
    assert_eq!(
        recorded_stdout.metadata().unwrap().len(),
        0,
        "recording did not exercise a successful ftruncate on captured stdout"
    );

    let mut control_stdout = tempfile::tempfile().expect("failed to create replay stdout");
    control_stdout
        .write_all(b"must also be truncated")
        .expect("failed to seed replay stdout");
    let mut control = Command::new(env!("CARGO_BIN_EXE_hermit"));
    control
        .args(["--log=off", "replay", "--autopilot", "--data-dir"])
        .arg(data_dir.path())
        .stdout(Stdio::from(
            control_stdout
                .try_clone()
                .expect("failed to clone replay stdout"),
        ));
    command_output(
        control,
        "successful captured-output ftruncate replay control",
    );
    assert_eq!(
        control_stdout.metadata().unwrap().len(),
        0,
        "replay did not reproduce ftruncate on a compatible captured stdout"
    );

    let started = Instant::now();
    let mut replay = Command::new("timeout");
    replay
        .args(["--kill-after=2s", "10s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=off", "replay", "--autopilot", "--data-dir"])
        .arg(data_dir.path());
    let rendered = format!("{replay:?}");
    let replay_output = replay
        .output()
        .unwrap_or_else(|error| panic!("failed to start replay: {rendered}: {error}"));
    let elapsed = started.elapsed();
    let stderr = String::from_utf8_lossy(&replay_output.stderr);

    assert_eq!(
        replay_output.status.code(),
        Some(HERMIT_INTERNAL_FAILURE_EXIT),
        "captured-output ftruncate failure did not terminate as one tool error: {rendered}\n\
         elapsed: {elapsed:?}\nstderr:\n{stderr}"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "captured-output ftruncate failure reached the watchdog: {elapsed:?}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("Invalid argument"),
        "replay did not report the host ftruncate cause:\n{stderr}"
    );
    assert_eq!(
        stderr
            .lines()
            .filter(|line| line.contains("Error:"))
            .count(),
        1,
        "captured-output ftruncate failure was not reported exactly once:\n{stderr}"
    );
    assert!(
        !stderr.contains("panicked") && !stderr.contains("desync") && !stderr.contains("expected"),
        "captured-output ftruncate escaped as a panic or stream divergence:\n{stderr}"
    );
}

#[test]
fn recording_rejects_an_unsupported_syscall_by_name() {
    let _guard = hermit_record_lock();
    let data_dir = tempfile::tempdir().expect("failed to create recording directory");
    let guest = workload("c_unsupported_syscall");

    let mut command = Command::new("timeout");
    command
        .args(["--kill-after=5s", "30s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["record", "start", "--data-dir"])
        .arg(data_dir.path())
        .arg("--")
        .arg(&guest.path);
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start unsupported recording: {error}"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_ne!(
        output.status.code(),
        Some(124),
        "unsupported recording hung: {rendered}"
    );
    assert!(
        !output.status.success(),
        "unsupported recording reported success: {rendered}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("unsupported syscall: restart_syscall"),
        "unsupported recording did not name restart_syscall:\n{stderr}"
    );
    assert!(
        !stdout.contains("dbt-unsupported-ok"),
        "unsupported guest published its former success marker: {stdout}"
    );
}

#[test]
fn replay_rejects_an_unsupported_syscall_by_name() {
    let _guard = hermit_record_lock();
    let data_dir = tempfile::tempdir().expect("failed to create replay directory");
    let guest = workload("c_unsupported_syscall");

    // Record the same executable on a supported branch. Rewriting only the
    // recorded argv then makes replay take its unsupported branch without
    // requiring a fail-open recording mode to manufacture the fixture.
    let mut record = Command::new(env!("CARGO_BIN_EXE_hermit"));
    record
        .args(["--log=off", "record", "--data-dir"])
        .arg(data_dir.path())
        .arg("--")
        .arg(&guest.path)
        .arg("replay-control");
    let record_output = command_output(record, "supported replay-control recording");
    assert_eq!(
        record_output.stdout, b"dbt-supported-replay-control\n",
        "recording did not exercise the supported control branch"
    );

    let recording_id = fs::read_to_string(data_dir.path().join("last"))
        .expect("recording did not publish its last ID");
    let metadata_path = data_dir
        .path()
        .join(recording_id.trim())
        .join("metadata.json");
    let mut metadata: serde_json::Value = serde_json::from_reader(
        fs::File::open(&metadata_path).expect("failed to open replay metadata"),
    )
    .expect("failed to parse replay metadata");
    // Keep argc and the argument length identical so the initial stack layout
    // and dynamic-loader syscall pointers still match the recording. Only the
    // branch selected by the argument contents changes.
    metadata["args"] = serde_json::json!(["replay-failure"]);
    serde_json::to_writer_pretty(
        fs::File::create(&metadata_path).expect("failed to rewrite replay metadata"),
        &metadata,
    )
    .expect("failed to serialize replay metadata");

    let mut replay = Command::new("timeout");
    replay
        .args(["--kill-after=5s", "30s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["replay", "--autopilot", "--data-dir"])
        .arg(data_dir.path());
    let rendered = format!("{replay:?}");
    let output = replay
        .output()
        .unwrap_or_else(|error| panic!("failed to start unsupported replay: {error}"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_ne!(
        output.status.code(),
        Some(124),
        "unsupported replay hung: {rendered}"
    );
    assert!(
        !output.status.success(),
        "unsupported replay reported success: {rendered}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("unsupported syscall: restart_syscall"),
        "unsupported replay did not name restart_syscall:\n{stderr}"
    );
    assert!(
        !stdout.contains("dbt-unsupported-ok"),
        "unsupported replay published its former success marker: {stdout}"
    );
}

#[test]
fn record_replay_matrix() {
    // Record/replay does not enable PMU-backed preemption, so these workloads
    // also run on GitHub-managed portable runners without performance-counter access.
    let _guard = hermit_record_lock();
    for name in BASELINE_RECORD_WORKLOADS {
        record_replay(workload(name));
    }
}

#[test]
fn record_reopened_inherited_and_cloned_file_state() {
    run_record_replay("c_record_replay_file_state");
}

/// Regression test for the record/replay regular-file `lseek(SEEK_CUR)` bug.
///
/// Detcore's `handle_lseek` live-injected a seek on a non-procfs (regular-file)
/// descriptor instead of routing it through the record/replay strategy. On
/// replay the descriptor is a virtual placeholder whose kernel position never
/// advances (reads are served from the log), so `lseek(fd, -N, SEEK_CUR)`
/// returned 0 rather than the recorded offset -- the exact pattern glibc's
/// `__tzfile_read` uses to rewind `/etc/localtime`. The wrong offset injected
/// an extra read and desynchronized replay at `replayer/mod.rs`.
///
/// The fixture is created by the harness rather than by the guest, so it is not
/// in the replay root and is served as a virtual placeholder on replay -- the
/// descriptor shape that triggered the bug. Without the fix this aborts replay
/// with a divergence panic; with it, record and replay stdout match.
#[test]
fn record_regular_file_lseek_seek_cur() {
    let _guard = hermit_record_lock();
    let fixture_dir = tempfile::tempdir().expect("failed to create lseek fixture directory");
    let fixture = fixture_dir.path().join("fixture.bin");
    let bytes: Vec<u8> = (0..1000u32).map(|i| ((i * 37 + 13) % 256) as u8).collect();
    fs::write(&fixture, &bytes).expect("failed to write lseek fixture");
    record_replay_command(
        "regular-file-lseek-seek-cur",
        &workload("c_lseek_seek_cur").path,
        &[fixture.as_os_str()],
    );
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

/// Exercises the replay-only distinction between an EEXIST directory and an
/// EEXIST file/symlink, including Linux's symlink-before-`..` resolution order
/// and mkdirat's absolute-path rule that ignores an unusable dirfd.
#[test]
fn record_mkdir_eexist_materialization_semantics() {
    let _guard = hermit_record_lock();

    let basic = tempfile::tempdir().expect("failed to create mkdir EEXIST fixture");
    let existing_directory = basic.path().join("existing-directory");
    let existing_file = basic.path().join("existing-file");
    let existing_link = basic.path().join("existing-link");
    let new_directory = basic.path().join("new-directory");
    let missing_child = basic.path().join("missing-parent/child");
    fs::create_dir(&existing_directory).expect("failed to create existing directory fixture");
    fs::write(&existing_file, b"file\n").expect("failed to create existing file fixture");
    std::os::unix::fs::symlink("existing-file", &existing_link)
        .expect("failed to create existing symlink fixture");

    let walk = tempfile::tempdir().expect("failed to create symlink walk fixture");
    fs::create_dir_all(walk.path().join("real/deep"))
        .expect("failed to create symlink target fixture");
    fs::create_dir(walk.path().join("real/target"))
        .expect("failed to create symlink parent target fixture");
    let walk_link = walk.path().join("link");
    let walk_path = walk.path().join("link/../target");

    let unconfined = tempfile::tempdir().expect("failed to create unconfined dirfd fixture");
    fs::create_dir(unconfined.path().join("relative-existing"))
        .expect("failed to create relative mkdirat fixture");
    let absolute = tempfile::tempdir().expect("failed to create absolute mkdirat fixture");
    let absolute_directory = absolute.path().join("absolute-existing");
    fs::create_dir(&absolute_directory).expect("failed to create absolute mkdirat directory");

    record_replay_command(
        "mkdir-eexist-materialization-semantics",
        &workload("c_record_replay_mkdir_eexist").path,
        &[
            basic.path().as_os_str(),
            existing_directory.as_os_str(),
            existing_file.as_os_str(),
            existing_link.as_os_str(),
            walk.path().as_os_str(),
            walk_link.as_os_str(),
            walk_path.as_os_str(),
            unconfined.path().as_os_str(),
            absolute_directory.as_os_str(),
            new_directory.as_os_str(),
            missing_child.as_os_str(),
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

/// A relative child script is resolved against the recording-time guest cwd,
/// but the replayed `execveat` must retain the original relative pathname. The
/// recorder therefore snapshots the resolved script and its shebang/ELF
/// interpreter chain at the corresponding absolute guest destination.
#[test]
fn record_shell_relative_child_script() {
    let _guard = hermit_record_lock();
    let fixture = tempfile::tempdir().expect("failed to create relative exec fixture");
    let script = fixture.path().join("relative-child.sh");
    fs::write(&script, b"#!/bin/sh\nprintf 'relative-child-ok\\n'\n")
        .expect("failed to write relative child script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755))
        .expect("failed to mark relative child script executable");

    let data_dir = tempfile::tempdir().expect("failed to create Hermit recording directory");
    let mut command = Command::new("timeout");
    command
        .current_dir(fixture.path())
        .env("HERMIT_MODE", "record")
        .args(["--kill-after=5s", "45s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["record", "start", "--verify", "--record-timeout=30"])
        .arg(format!("--data-dir={}", data_dir.path().display()))
        .args(["--", "/bin/sh", "-c", "./relative-child.sh"]);
    let output = command_output(command, "record/replay for relative child script");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("Success: replay matched recording."),
        "missing replay parity verdict:\n{combined}"
    );
}

/// Linux follows an encountered symlink before applying a later `..`. A
/// lexical collapse would incorrectly stage `a/prog`; the actual target here is
/// `resolved/prog`.
#[test]
fn record_exec_symlink_before_parent_resolution() {
    let _guard = hermit_record_lock();
    let fixture = tempfile::tempdir().expect("failed to create exec path fixture");
    fs::create_dir_all(fixture.path().join("a")).unwrap();
    fs::create_dir_all(fixture.path().join("resolved/deep")).unwrap();
    let program = fixture.path().join("resolved/prog");
    fs::write(&program, b"#!/bin/sh\nprintf 'symlink-parent-ok\\n'\n").unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("../resolved/deep", fixture.path().join("a/link")).unwrap();

    let data_dir = tempfile::tempdir().expect("failed to create Hermit recording directory");
    let mut command = Command::new("timeout");
    command
        .current_dir(fixture.path())
        .env("HERMIT_MODE", "record")
        .args(["--kill-after=5s", "45s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["record", "start", "--verify", "--record-timeout=30"])
        .arg(format!("--data-dir={}", data_dir.path().display()))
        .args(["--", "/bin/sh", "-c", "./a/link/../prog"]);
    let output = command_output(command, "record/replay for symlink-before-parent exec");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("Success: replay matched recording."),
        "missing replay parity verdict:\n{combined}"
    );
}

/// Failed execs must not prepopulate a later pathname. The same pathname is
/// then successfully executed twice with different contents, proving snapshots
/// are associated with the matching successful event rather than globally.
#[test]
fn record_exec_failure_then_temporal_path_reuse() {
    let _guard = hermit_record_lock();
    let fixture = tempfile::tempdir().expect("failed to create exec chronology fixture");
    let data_dir = tempfile::tempdir().expect("failed to create Hermit recording directory");
    let script = r#"set -eu
if ./later 2>/dev/null; then exit 91; fi
printf '%s\n' '#!/bin/sh' "printf 'first-image\\n'" > later
chmod 755 later
./later
printf '%s\n' '#!/bin/sh' "printf 'second-image\\n'" > later
chmod 755 later
./later
"#;
    let mut command = Command::new("timeout");
    command
        .current_dir(fixture.path())
        .env("HERMIT_MODE", "record")
        .args(["--kill-after=5s", "45s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["record", "start", "--verify", "--record-timeout=30"])
        .arg(format!("--data-dir={}", data_dir.path().display()))
        .args(["--", "/bin/sh", "-c", script]);
    let output = command_output(command, "record/replay for temporal exec path reuse");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("Success: replay matched recording."),
        "missing replay parity verdict:\n{combined}"
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

/// Regression test for issue #862: `pidfd_open` was tracked in Detcore
/// (`add_fd(.., FdType::Pidfd)`) but the record/replay tool layer had no
/// `Syscall::PidfdOpen` arm, so under record/replay it fell through to live
/// injection — the returned pidfd was neither recorded nor recreated/validated
/// on replay. That left the deterministic replay unenforced and exposed the
/// Detcore descriptor model to fd-allocation or target-lifetime drift.
///
/// These guests open a pidfd and then perform a *modeled* descriptor operation
/// on it (`fcntl(F_GETFD)` and a zero-timeout `poll`), so a divergence between
/// the recorded and replayed pidfd would surface as a record/replay mismatch.
/// The `--verify` path records then replays and asserts the two agree, which is
/// the record/replay witness the earlier `hermit run --verify`-only coverage
/// lacked (that path never exercises the recorder/replayer at all).
#[test]
fn record_pidfd_open_modeled_descriptor_ops() {
    let _guard = hermit_record_lock();
    record_replay(workload("c_pidfd_open_self"));
    record_replay(workload("c_pidfd_poll_self"));
}

#[test]
fn record_poll_partial_revents_copyout() {
    let _guard = hermit_record_lock();
    canonical_record_replay_command(
        "poll partial revents copyout",
        &workload("c_record_replay_poll_partial_copyout").path,
        &[OsStr::new("poll")],
    );
}

#[test]
fn record_ppoll_partial_revents_and_timeout_copyout() {
    let _guard = hermit_record_lock();
    canonical_record_replay_command(
        "ppoll partial revents and timeout copyout",
        &workload("c_record_replay_poll_partial_copyout").path,
        &[OsStr::new("ppoll")],
    );
}

#[test]
fn record_poll_invalid_nfds_preserves_einval() {
    let _guard = hermit_record_lock();
    canonical_record_replay_command(
        "poll and ppoll invalid nfds",
        &workload("c_record_replay_poll_partial_copyout").path,
        &[OsStr::new("invalid-nfds")],
    );
}

/// Replayer substitutes an eventfd for this proc descriptor. The Detcore
/// procfs layer must bind the live task incarnation named by an absolute or
/// AT_FDCWD-relative path rather than the placeholder inode. Zero-length
/// read/pread and pre-snapshot lseek must remain entirely virtual, then one
/// timer-slack scalar must compose across proc read/write and prctl access in
/// both phases.
#[test]
fn record_timer_slack_proc_read_write() {
    let _guard = hermit_record_lock();
    record_replay_strict_command(
        "timer-slack-proc-read-write",
        &workload("c_timerslack_proc_record_replay").path,
        &[],
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
    record_c_execveat_paths => "c_record_replay_execveat_paths",
    record_c_sigpipe_siginfo => "c_sigpipe_siginfo",
    record_c_clock_exec_continuity => "c_clock_exec_continuity",
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
