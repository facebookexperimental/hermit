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
use std::io::Read as _;
use std::io::Seek as _;
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
            .mount(Mount::proc().allow_readonly_fallback());
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

    let guest = &workload("c_public_record_mount_stdio").path;

    let mut command = ReverieCommand::new(guest);
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
    let raw_ids = |key: &str| -> Vec<u64> {
        metadata[key]
            .as_array()
            .unwrap_or_else(|| panic!("recording metadata {key} must be an array"))
            .iter()
            .map(|id| {
                id.as_u64()
                    .unwrap_or_else(|| panic!("recording metadata {key} must hold raw mount IDs"))
            })
            .collect()
    };
    let captured_mountinfo_ids = raw_ids("mountinfo_mount_ids");
    let assignment_order = raw_ids("mount_id_assignment_order");
    // The guest's stdout is a pipe. pipefs is never listed in mountinfo, so
    // its guest-visible mnt_id must index an assignment that only fdinfo
    // provenance can have produced, and replay must rebuild it from there.
    let stdout_mount_id = section_contents(&captured, "STDOUT_FDINFO")
        .lines()
        .find_map(|line| line.strip_prefix("mnt_id:\t"))
        .expect("stdout fdinfo must contain mnt_id")
        .parse::<usize>()
        .expect("stdout fdinfo mnt_id must be decimal");
    let stdout_raw_mount_id = stdout_mount_id
        .checked_sub(1)
        .and_then(|index| assignment_order.get(index))
        .unwrap_or_else(|| {
            panic!(
                "stdout pipe mnt_id {stdout_mount_id} is not in the persisted assignment order {assignment_order:?}"
            )
        });
    assert!(
        !captured_mountinfo_ids.contains(stdout_raw_mount_id),
        "stdout pipe mnt_id {stdout_mount_id} maps to raw {stdout_raw_mount_id}, which mountinfo lists; \
         the recording did not persist the unlisted pipe assignment"
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
    let stdout_mount_id = stdout_mount_id.to_string();
    assert!(
        !mountinfo.lines().any(|line| {
            line.split(' ')
                .take(2)
                .any(|mount_id| mount_id == stdout_mount_id)
        }),
        "the stdout pipe's mnt_id {stdout_mount_id} must not name a mountinfo row or parent:\n{mountinfo}"
    );
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
            .mount(Mount::proc().allow_readonly_fallback());
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
    let guest = &workload("c_record_replay_forked_streams").path;

    let mut command = ReverieCommand::new(guest);
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

/// `--mount=type=tmpfs` gives the guest a tmpfs in a mount namespace the
/// replayer cannot name, as the E2E harness does for `/test`. Replay must
/// decide what lies inside the guest root from directory objects rather than
/// procfs link text. `rm -rf` removes entries relative to directory
/// descriptors; replay used to skip those removals and then fail the final
/// `rmdir` with ENOTEMPTY (https://github.com/rrnewton/hermit/issues/3592).
#[test]
fn record_replay_removes_a_tree_on_a_guest_private_tmpfs() {
    let data_dir = tempfile::tempdir().expect("failed to create Hermit recording directory");
    let mountpoint = tempfile::tempdir().expect("failed to create guest tmpfs mountpoint");
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after=5s", "45s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["record", "start", "--verify", "--record-timeout=30"])
        .arg(format!("--data-dir={}", data_dir.path().display()))
        .arg(format!(
            "--mount=type=tmpfs,target={}",
            mountpoint.path().display()
        ))
        .arg(format!("--workdir={}", mountpoint.path().display()))
        .args([
            "--",
            "/bin/sh",
            "-c",
            "mkdir -p w/sub/deeper && echo hi > w/sub/f && echo there > w/sub/deeper/g \
             && rm -rf w && test ! -e w && echo removed",
        ]);
    let output = command_output(command, "record/replay of rm -rf on a guest tmpfs");
    let combined_output = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined_output.contains("removed\n"),
        "the guest did not remove its tree:\n{combined_output}"
    );
    assert!(
        combined_output.contains("Success: replay matched recording."),
        "Hermit did not report matching replay:\n{combined_output}"
    );
    assert!(
        fs::read_dir(mountpoint.path())
            .expect("read host view of the mountpoint")
            .next()
            .is_none(),
        "the guest tmpfs leaked into the host mount namespace, so this test no \
         longer separates the two namespaces"
    );
}

/// The open half of https://github.com/rrnewton/hermit/issues/3592: replay
/// must open for real a file the guest creates on its private tmpfs. `flock`
/// creates its lock file with `O_CREAT` and then locks the descriptor; replay
/// refuses to fake a lock on a descriptor outside the replay root, so a
/// placeholder descriptor for the create makes replay fail.
#[test]
fn record_replay_locks_a_file_created_on_a_guest_private_tmpfs() {
    let data_dir = tempfile::tempdir().expect("failed to create Hermit recording directory");
    let mountpoint = tempfile::tempdir().expect("failed to create guest tmpfs mountpoint");
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after=5s", "45s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["record", "start", "--verify", "--record-timeout=30"])
        .arg(format!("--data-dir={}", data_dir.path().display()))
        .arg(format!(
            "--mount=type=tmpfs,target={}",
            mountpoint.path().display()
        ))
        .arg(format!("--workdir={}", mountpoint.path().display()))
        .args([
            "--",
            "/bin/sh",
            "-c",
            "mkdir -p locks && flock locks/l -c 'echo locked'",
        ]);
    let output = command_output(command, "record/replay of flock on a guest tmpfs");
    let combined_output = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined_output.contains("locked\n"),
        "the guest did not take its lock:\n{combined_output}"
    );
    assert!(
        combined_output.contains("Success: replay matched recording."),
        "Hermit did not report matching replay:\n{combined_output}"
    );
}

/// Records `script` with standard output `record_stdout`, then lets `prepare`
/// reset host state and replays the recording with `--autopilot` and standard
/// output `replay_stdout`, returning the replay's output.
fn record_then_replay(
    script: &str,
    record_stdout: Stdio,
    prepare: impl FnOnce(),
    replay_stdout: Stdio,
) -> Output {
    let data_dir = tempfile::tempdir().expect("failed to create Hermit recording directory");
    let mut record = Command::new("timeout");
    record
        .args(["--kill-after=5s", "45s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=off", "record", "start", "--record-timeout=30"])
        .arg(format!("--data-dir={}", data_dir.path().display()))
        .args(["--", "/bin/sh", "-c", script])
        .stdout(record_stdout);
    command_output(record, "recording");
    prepare();
    let mut replay = Command::new("timeout");
    replay
        .args(["--kill-after=5s", "45s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=off", "replay", "--autopilot"])
        .arg(format!("--data-dir={}", data_dir.path().display()))
        .stdout(replay_stdout)
        .stderr(Stdio::piped());
    replay.output().expect("failed to start replay")
}

/// `/proc/1/root` reads as `/`, a path inside the replay root, but the guest's
/// kernel follows it to the linked root. Replay must not let the guest re-run
/// an `O_CREAT` open through it, which would create a host file outside the
/// replay root.
#[test]
fn replay_does_not_create_a_host_file_through_proc_root() {
    let scratch = tempfile::tempdir().expect("failed to create scratch directory");
    let target = scratch.path().join("escaped");
    let script = format!("echo escaped > /proc/1/root{}", target.display());
    let output = record_then_replay(
        &script,
        Stdio::piped(),
        || {
            assert!(
                target.exists(),
                "recording did not reach the host file through /proc/1/root, so \
                 this test no longer exercises a link out of the guest root"
            );
            fs::remove_file(&target).expect("remove the recorded host file");
        },
        Stdio::piped(),
    );
    assert!(
        !target.exists(),
        "replay created {} outside the replay root; replay stderr:\n{}",
        target.display(),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// `/dev/fd/1` reaches replay's own standard output through the magic link
/// `/proc/self/fd/1`. A recorded `O_TRUNC` open of it must not truncate that
/// host file during replay. The recording's standard output is a regular file
/// too, so the recorded open is of a regular file, as replay sees.
///
/// Only truncation is checked. Replay on main writes the recorded "hi\n" at
/// the recorded offset 0 over the start of the sentinel, without truncating;
/// that is separate from the open decision and this test does not judge it.
#[test]
fn replay_does_not_truncate_its_stdout_through_dev_fd() {
    let scratch = tempfile::tempdir().expect("failed to create scratch directory");
    let replay_stdout = scratch.path().join("replay-stdout");
    fs::write(&replay_stdout, "sentinel\n").expect("write replay stdout sentinel");
    let stdout = OpenOptions::new()
        .append(true)
        .open(&replay_stdout)
        .expect("open replay stdout");
    let record_stdout =
        fs::File::create(scratch.path().join("record-stdout")).expect("create record stdout");
    let output = record_then_replay(
        "echo hi > /dev/fd/1",
        Stdio::from(record_stdout),
        || {},
        Stdio::from(stdout),
    );
    let content = fs::read_to_string(&replay_stdout).expect("read replay stdout");
    assert!(
        content.len() >= "sentinel\n".len(),
        "replay truncated its host stdout file: {content:?}; replay stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
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
            .mount(Mount::proc().allow_readonly_fallback());
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
    let guest = &workload("c_record_replay_deep_fork_chain").path;

    let mut command = ReverieCommand::new(guest);
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

#[path = "../../ci/record-replay-workloads.rs"]
pub mod record_workloads;
use record_workloads::Workload;

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

fn workloads() -> &'static [Workload] {
    WORKLOADS.get_or_init(|| {
        let prepared = std::env::var(record_workloads::PREPARED_ENV);
        let raw = match &prepared {
            Ok(raw) => Some(raw.as_str()),
            Err(std::env::VarError::NotPresent) => None,
            Err(error) => panic!("invalid prepared record workload environment: {error}"),
        };
        if let Some(workloads) = record_workloads::consume_prepared(
            std::env::var_os(record_workloads::REQUIRED_ENV).is_some(),
            raw,
        )
        .expect("record/replay prepared workload verification failed")
        {
            return workloads;
        }
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("record-replay-workloads");
        record_workloads::standalone(repository, &build_root, env!("CARGO"))
            .unwrap_or_else(|error| panic!("record/replay standalone preparation failed: {error}"))
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
    // Keep the basename "echo": a multicall coreutils, as in the pinned
    // validation root, picks the applet from argv[0] and refuses other names.
    let executable = fixture.path().join("echo");
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

/// The recording stores its network choice, and an autopilot replay of either
/// choice reproduces the recording.
#[test]
fn record_network_choice_is_stored_and_autopilot_replays_it() {
    let _guard = hermit_record_lock();
    for (flag, expected) in [(None, true), (Some("host"), false), (Some("local"), true)] {
        let data_dir = tempfile::tempdir().expect("failed to create recording directory");
        let mut record = Command::new(env!("CARGO_BIN_EXE_hermit"));
        record.args(["--log=off", "record", "start", "--record-timeout=30"]);
        if let Some(flag) = flag {
            record.args(["--network", flag]);
        }
        record
            .arg("--data-dir")
            .arg(data_dir.path())
            .args(["--", "/bin/echo", "hello"]);
        let record_output = command_output(record, "network-choice recording");
        assert_eq!(
            record_output.stdout, b"hello\n",
            "recorded stdout ({flag:?})"
        );

        let recording = fs::read_dir(data_dir.path())
            .expect("read recording directory")
            .map(|entry| entry.expect("recording entry").path())
            .find(|path| path.join("metadata.json").is_file())
            .expect("recording with metadata.json");
        let metadata: serde_json::Value = serde_json::from_slice(
            &fs::read(recording.join("metadata.json")).expect("read metadata.json"),
        )
        .expect("parse metadata.json");
        assert_eq!(
            metadata["local_networking"],
            serde_json::Value::Bool(expected),
            "stored network choice ({flag:?})"
        );

        let mut replay = Command::new(env!("CARGO_BIN_EXE_hermit"));
        replay
            .args(["--log=off", "replay", "--autopilot", "--data-dir"])
            .arg(data_dir.path());
        let replay_output = command_output(replay, "network-choice replay");
        assert_eq!(
            replay_output.stdout, b"hello\n",
            "replayed stdout ({flag:?})"
        );
    }
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
    let guest = &workload("c_mount_nscd_order").path;

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
        .arg(guest);
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

/// A successful mkdir proves that its parent directories existed at record
/// time, but a standalone replay starts from an empty chroot that holds none of
/// them. `git clone <src> <existing>/clone` is the real-world instance. This
/// pins the legacy `mkdir` syscall, which coreutils `mkdir` (without `-p`)
/// issues on x86_64; the mkdirat test below covers the descriptor-relative
/// form.
#[test]
fn replay_mkdir_beneath_a_directory_that_only_existed_at_record_time() {
    let _guard = hermit_record_lock();
    let shell = Path::new("/bin/bash");
    assert!(shell.is_file(), "bash is missing at {}", shell.display());
    let host = tempfile::tempdir().expect("failed to create pre-existing host directory");
    let cwd = host.path().join("cwd");
    fs::create_dir_all(cwd.join("relative-parent"))
        .expect("failed to create relative mkdir fixture");

    record_then_replay_command(
        "mkdir-beneath-record-time-directory",
        shell,
        &[
            OsStr::new("-c"),
            OsStr::new(
                "set -euo pipefail; mkdir \"$1/absolute\"; cd \"$1/cwd\"; mkdir relative-parent/relative; printf 'mkdir-beneath-record-time-directory-ok\\n'",
            ),
            OsStr::new("mkdir-fixture"),
            host.path().as_os_str(),
        ],
    );
    assert!(
        host.path().join("absolute").is_dir() && cwd.join("relative-parent/relative").is_dir(),
        "recording did not create the directories on the host"
    );
}

/// The mkdirat form of the test above: AT_FDCWD-relative after a chdir, and
/// relative to an opened directory, each followed by a second level through the
/// directory it created. Replay creates the parents of a mkdirat whose dirfd is
/// confined to the replay root, so it is injected and must reproduce the
/// recorded success.
#[test]
fn replay_mkdirat_beneath_a_directory_that_only_existed_at_record_time() {
    let _guard = hermit_record_lock();
    let host = tempfile::tempdir().expect("failed to create pre-existing host directory");
    fs::create_dir_all(host.path().join("cwd/relative-parent"))
        .expect("failed to create AT_FDCWD mkdirat fixture");
    fs::create_dir(host.path().join("dirfd-parent"))
        .expect("failed to create dirfd mkdirat fixture");

    record_then_replay_command(
        "mkdirat-beneath-record-time-directory",
        &workload("c_record_replay_mkdirat_parent").path,
        &[host.path().as_os_str()],
    );
    for created in [
        "cwd/relative-parent/at-fdcwd/child",
        "dirfd-parent/at-dirfd/child",
    ] {
        assert!(
            host.path().join(created).is_dir(),
            "recording did not create {created} on the host"
        );
    }
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

    // Node issues madvise(MADV_DONTNEED), which record/replay used to refuse
    // (https://github.com/rrnewton/hermit/issues/3537). Replay must match the
    // recording bitwise, not just in its stdout.
    canonical_record_replay_command(
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

/// faccessat/faccessat2, chdir/getcwd and the legacy path mutations (rename,
/// link, symlink, chmod, chown, lchown, mknod, rmdir) must replay from the
/// recording. The replay chroot lacks /etc/passwd and /usr/bin, so a live
/// query there answers differently and the guest's output diverges.
#[test]
fn record_path_queries_and_legacy_mutations() {
    let _guard = hermit_record_lock();
    // Host directories absent from the replay chroot: a replayed chdir that
    // stayed put would make the second round's link collide with the first.
    // "via" is a host symlink the replay chroot lacks, so only a replay that
    // enters the recorded directory resolves "via/.." to "first".
    let host_dirs = tempfile::tempdir().expect("failed to create host directories");
    let base = host_dirs.path();
    std::fs::create_dir_all(base.join("first/sub")).expect("failed to create first host directory");
    std::fs::create_dir(base.join("second")).expect("failed to create second host directory");
    std::os::unix::fs::symlink("first/sub", base.join("via"))
        .expect("failed to create host directory symlink");
    // A resolved working directory longer than the replayer's 512-byte
    // injection buffer, reached through a short symlink.
    let deep = (0..6).fold(base.join("long"), |path, _| path.join("d".repeat(100)));
    std::fs::create_dir_all(&deep).expect("failed to create deep host directory");
    std::os::unix::fs::symlink(&deep, base.join("longvia"))
        .expect("failed to create deep host directory symlink");
    canonical_record_replay_command(
        "path queries and legacy path mutations",
        &workload("c_record_replay_path_queries").path,
        &[base.as_os_str()],
    );
}

/// fsync, fdatasync, syncfs, fchmod, fchown, the xattr calls and zero-length
/// reads must replay from the recording
/// (https://github.com/rrnewton/hermit/issues/3591). The file lives in a host
/// directory the replay chroot lacks, so replay's descriptor is a placeholder
/// and a live call there answers EINVAL or EOPNOTSUPP. The directory carries
/// an attribute from before recording, which the replay root lacks.
#[test]
fn record_fd_metadata_calls() {
    let _guard = hermit_record_lock();
    let host_dir = tempfile::tempdir().expect("failed to create host directory");
    let dir = std::ffi::CString::new(host_dir.path().as_os_str().as_encoded_bytes())
        .expect("host directory path contains a NUL byte");
    // SAFETY: every pointer names a live NUL-terminated string, and the value
    // is the one byte before its terminator.
    let set = unsafe {
        libc::setxattr(
            dir.as_ptr(),
            c"user.pre".as_ptr(),
            c"p".as_ptr().cast(),
            1,
            0,
        )
    };
    assert_eq!(
        set,
        0,
        "failed to set user.pre on {:?}: {}",
        host_dir.path(),
        std::io::Error::last_os_error()
    );
    canonical_record_replay_command(
        "descriptor metadata calls",
        &workload("c_record_replay_fd_metadata").path,
        &[host_dir.path().as_os_str()],
    );
}

/// A working directory that was removed has no path: procfs names it
/// "<dir> (deleted)", which is not a directory replay could enter. Recording
/// must refuse such an fchdir loudly rather than store that text.
#[test]
fn record_refuses_a_removed_working_directory() {
    let _guard = hermit_record_lock();
    let data_dir = tempfile::tempdir().expect("failed to create recording directory");
    let guest = workload("c_record_replay_path_queries");

    let mut command = Command::new("timeout");
    command
        .args(["--kill-after=5s", "30s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["record", "start", "--data-dir"])
        .arg(data_dir.path())
        .arg("--")
        .arg(&guest.path)
        .arg("--removed-cwd");
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start removed-cwd recording: {error}"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // The refusal is a recorder panic, which exits with the internal-failure
    // status. Anything else, including timeout's 124 or a SIGKILL after a
    // hang, is not the refusal this test pins.
    assert_eq!(
        output.status.code(),
        Some(HERMIT_INTERNAL_FAILURE_EXIT),
        "removed-cwd recording did not refuse with the internal-failure status: {rendered}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("has been removed"),
        "removed-cwd recording did not name the removed directory:\n{stderr}"
    );
    assert!(
        !stdout.contains("removed-cwd-recorded"),
        "removed-cwd guest ran past the refused fchdir: {stdout}"
    );
}

/// select and pselect6 must replay the recorded result, fd sets, and remaining
/// timeout. Replay does not refill pipes, so before
/// https://github.com/rrnewton/hermit/issues/3569 a pipe that held data while
/// recording was reported ready at record time and not ready at replay.
fn record_select_mode(mode: &str) {
    let _guard = hermit_record_lock();
    canonical_record_replay_command(
        &format!("select {mode}"),
        &workload("c_record_replay_select").path,
        &[OsStr::new(mode)],
    );
}

#[test]
fn record_raw_select_ready_pipe() {
    record_select_mode("raw");
}

#[test]
fn record_glibc_select_ready_pipe() {
    record_select_mode("glibc");
}

#[test]
fn record_pselect_with_signal_mask_ready_pipe() {
    record_select_mode("pselect-mask");
}

#[test]
fn record_select_partial_fd_set_copyout() {
    record_select_mode("efault");
}

#[test]
fn record_select_negative_nfds_preserves_einval() {
    record_select_mode("einval");
}

#[test]
fn record_poll_and_ppoll_ready_pipe() {
    record_select_mode("poll");
}

/// The fork of `record_forked_child_writes_into_a_redirected_stdout_pipe`, with the child writing 128 KiB to the container's real stdout
/// through a descriptor its parent saved before redirecting its own stdout, as
/// `exec 3>&1; { head -c 131072 /dev/zero >&3; echo x; } | cat` does. Replay
/// emitted that captured output into the child's own copy of descriptor 1,
/// which is the pipe, and waited forever once the pipe was full
/// (https://github.com/rrnewton/hermit/issues/4006).
#[test]
fn record_forked_child_writes_to_a_saved_stdout_after_its_parent_redirected_it() {
    let _guard = hermit_record_lock();
    canonical_record_replay_command(
        "forked saved stdout",
        &workload("c_record_replay_forked_stdout_pipe").path,
        &[OsStr::new("saved-stdout")],
    );
}

/// A child forked after its parent redirected its own stdout into a pipe, as
/// flex does for each stage of its filter chain, writes 128 KiB into that pipe.
/// The recorder took each process's descriptor 1 at its creation as captured
/// output, so replay wrote the child's bytes into the live pipe while serving
/// the parent's reads from the recording, and the child waited forever once the
/// pipe was full (https://github.com/rrnewton/hermit/issues/3964).
#[test]
fn record_forked_child_writes_into_a_redirected_stdout_pipe() {
    let _guard = hermit_record_lock();
    canonical_record_replay_command(
        "forked stdout pipe",
        &workload("c_record_replay_forked_stdout_pipe").path,
        &[],
    );
}

/// Records, then replays, the guest with a regular-file stdout that the test
/// opened, and writes `TAIL` through the test's own copy of that description
/// after each run, so the file shows where the run left the description's
/// offset. Returns the recorded and the replayed file.
fn record_and_replay_into_a_shared_stdout_file(mode: &str) -> (Vec<u8>, Vec<u8>) {
    let guest = workload("c_record_replay_forked_stdout_pipe");
    let data_dir = tempfile::tempdir().expect("failed to create recording directory");
    let run = |subcommand: &[&str], what: &str| {
        let mut stdout = tempfile::tempfile().expect("failed to create regular stdout");
        let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
        command
            .arg("--log=off")
            .args(subcommand)
            .arg("--data-dir")
            .arg(data_dir.path());
        if subcommand[0] == "record" {
            command.arg("--").arg(&guest.path).arg(mode);
        }
        command.stdout(Stdio::from(
            stdout.try_clone().expect("failed to clone regular stdout"),
        ));
        command_output(command, what);
        stdout
            .write_all(b"TAIL")
            .expect("failed to write after the run");
        stdout.rewind().expect("failed to rewind regular stdout");
        let mut contents = Vec::new();
        stdout
            .read_to_end(&mut contents)
            .expect("failed to read regular stdout");
        contents
    };
    let recorded = run(&["record", "--strict"], &format!("record {mode}"));
    let replayed = run(&["replay", "--autopilot"], &format!("replay {mode}"));
    (recorded, replayed)
}

/// A captured write moves the offset of the root guest's stdout description
/// in replay exactly when it moved it in the recording, whichever process
/// wrote. Replay emits every captured write into the root's description, but
/// the recorder decided whether the write moved it against the writing
/// process's own creation-time descriptor 1 (review of
/// https://github.com/rrnewton/hermit/pull/4037):
/// - `last-writer`: a child forked after its parent redirected stdout into a
///   pipe writes `AAAA` through a saved copy of the root's description. Linux
///   leaves the offset at 4, so the file reads `AAAATAIL`; replay left it at 0
///   and the file read `TAIL`.
/// - `reopen`: the root opens its stdout file again, a second description of
///   the same file, as descriptor 1 and forks; the child writes `BBBBBBBB`
///   through it. The root's description stays at 0, so the file reads
///   `TAILBBBB`; replay moved it to 8 and the file read `BBBBBBBBTAIL`.
#[test]
fn record_forked_child_moves_the_root_stdout_offset_as_in_the_recording() {
    let _guard = hermit_record_lock();
    for (mode, expected) in [
        ("last-writer", b"AAAATAIL".as_slice()),
        ("reopen", b"TAILBBBB".as_slice()),
    ] {
        let (recorded, replayed) = record_and_replay_into_a_shared_stdout_file(mode);
        assert_eq!(
            String::from_utf8_lossy(&recorded),
            String::from_utf8_lossy(expected),
            "{mode}: the recording left the root's stdout offset where Linux does not"
        );
        assert_eq!(
            String::from_utf8_lossy(&replayed),
            String::from_utf8_lossy(&recorded),
            "{mode}: replay left the root's stdout offset where the recording did not"
        );
    }
}

#[test]
fn record_select_null_timeout_woken_by_thread() {
    record_select_mode("thread-wake");
}

/// A pselect6 recorded as a blocking call outside the schedule sleeps under its
/// own temporary mask, and the scheduler must arm its release barrier from that
/// mask. Taking the thread's ordinary mask instead, it armed the call for the
/// SIGALRM its timer sent although the call's mask blocks it, waited for a wake
/// Linux never delivers, and refused the recording with
/// `SignaledBackgroundRefusal` after 30 seconds
/// (https://github.com/rrnewton/hermit/issues/3963).
#[test]
fn record_pselect_whose_mask_blocks_the_scheduler_timer_signal() {
    record_select_mode("pselect-mask-blocks-alarm");
}

/// The same wait, eight times in one recording, with a writer that leaves
/// SIGALRM unblocked, so the kernel gives the timer's process-directed SIGALRM
/// to the writer while the waiter's pselect6 blocks it. Reading pselect6's mask
/// through an injected syscall before the call let recording give the signal to
/// the waiter in some runs, and replay refused 8 of 8 such recordings; a direct
/// read of guest memory replayed 8 of 8 (review of
/// https://github.com/rrnewton/hermit/pull/3989).
/// pselect6's `{ sigmask, sigsetsize }` wrapper on a PROT_WRITE page, which
/// Linux copies from but a tracer read of more than eight bytes cannot reach.
/// Reading the mask there failed, the scheduler took the ordinary mask, and
/// recording refused after 30 seconds at the release barrier (Codex review of
/// https://github.com/rrnewton/hermit/pull/3989).
#[test]
fn record_pselect_with_a_write_only_mask_wrapper() {
    record_select_mode("pselect-mask-writeonly-wrapper");
}

/// A sibling sets pselect6's mask to block SIGALRM after the waiter announces
/// the call and before the scheduler runs it. A mask read in the waiter's turn
/// was the old one, and recording refused after 30 seconds at the release
/// barrier (Codex review of https://github.com/rrnewton/hermit/pull/3989).
#[test]
fn record_pselect_whose_mask_a_sibling_rewrites_before_the_call_runs() {
    record_select_mode("pselect-mask-sibling-rewrite");
}

/// rt_sigtimedwait ends with EINTR for a caught, unblocked SIGCHLD from a
/// child's exit, and times out with EAGAIN when SIGCHLD is blocked or keeps its
/// default action; the guest checks each against Linux's result. Record and
/// replay returned EAGAIN for the caught case once their waits held SIGCHLD as
/// `hermit run` does (Codex review of
/// https://github.com/rrnewton/hermit/pull/3989).
#[test]
fn record_sigtimedwait_ends_for_a_caught_sigchld_from_a_child_exit() {
    for mode in ["caught", "blocked", "default"] {
        let _guard = hermit_record_lock();
        canonical_record_replay_command(
            &format!("sigtimedwait child exit {mode}"),
            &workload("c_record_replay_sigtimedwait_child_exit").path,
            &[OsStr::new(mode)],
        );
    }
}

#[test]
fn record_pselect_whose_mask_blocks_a_signal_a_sibling_takes() {
    record_select_mode("pselect-mask-shared-alarm");
}

/// The opposite idiom: the thread blocks SIGALRM and waits in pselect6 under a
/// temporary mask that unblocks it, so the timer's SIGALRM ends the wait with
/// EINTR and its handler runs before the call returns. Replay serves the call
/// from the recording and never installed the temporary mask, so the kernel
/// found the signal blocked, restarted the call, and replay stopped at the
/// repeated pselect6 where the recording has the guest's next call
/// (https://github.com/rrnewton/hermit/issues/3992).
#[test]
fn record_pselect_whose_mask_unblocks_the_scheduler_timer_signal() {
    record_select_mode("pselect-mask-unblocks-alarm");
}

/// The same wait, with a sibling adding SIGALRM to the call's mask buffer
/// while the call sleeps. Linux copied the mask at entry; replay read the
/// buffer again at the end of the served call, found SIGALRM blocked, and
/// stopped at the restarted pselect6 (review of
/// https://github.com/rrnewton/hermit/pull/4052).
#[test]
fn record_pselect_whose_mask_a_sibling_rewrites_while_it_sleeps() {
    record_select_mode("pselect-mask-unblocks-rewritten");
}

/// The unblocks wait on a stack with no writable memory below its red zone.
/// Replay delivers the signal through an injected pselect6 whose mask it must
/// stage somewhere: the stack scratch did not commit, replay skipped the
/// delivery, and the guest's restarted pselect6 no longer matched the
/// recording (follow-up to https://github.com/rrnewton/hermit/pull/4052).
/// Replay now stages it in the red zone, so the guest also checks that a
/// pattern it wrote to the 128 bytes below its stack pointer is intact after
/// the call (re-check of that follow-up).
#[test]
fn record_pselect_whose_mask_unblocks_the_timer_signal_on_a_tight_stack() {
    record_select_mode("pselect-mask-unblocks-tight-stack");
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

/// A guest pipe stays physically nonblocking under record/replay so that a
/// blocking read waits through the scheduler. Clearing the guest's O_NONBLOCK
/// with FIONBIO or F_SETFL must change only its view: forwarded to the kernel,
/// the next read waited as external I/O for a writer that could not run, and
/// the recording hung. The guest exits non-zero unless the empty read saw
/// EAGAIN, F_GETFL then showed the flag cleared, and the blocking read got the
/// writer's bytes.
#[test]
fn record_pipe_read_after_clearing_nonblocking_waits_for_its_writer() {
    let _guard = hermit_record_lock();
    record_replay_strict_command(
        "pipe-clear-nonblock",
        &workload("c_record_replay_pipe_clear_nonblock").path,
        &[],
    );
}

/// Records and then replays a guest that blocks reading a `kind` channel
/// (see `tests/c/record_replay_socketpair_blocking_read.c`) until the other
/// process writes to it, on a fresh channel per round: through the original
/// descriptor, through a dup with the original closed, and in a forked child
/// through its inherited descriptor. On a socketpair the dup round uses recv
/// and send, and the fork round recvmsg and sendmsg. With `clear`, `fionbio`
/// or `setfl`, each reader first turns O_NONBLOCK on and off again that way.
/// Both phases must exit 0 and print what Linux gives the guest: every
/// receive returns the writer's five bytes. The guest's own output is checked
/// because `--verify` compares a recording with its own replay, and that
/// comparison matches when the recording itself captured the wrong result.
/// Returns a description of each phase that failed.
fn blocking_read_failures(kind: &str, clear: Option<&str>) -> Vec<String> {
    let label = match clear {
        Some(clear) => format!("{kind} {clear}"),
        None => kind.to_owned(),
    };
    let (dup_receive, fork_receive) = if kind == "pipe" {
        ("read", "read")
    } else {
        ("recv", "recvmsg")
    };
    let args: Vec<&str> = std::iter::once(kind).chain(clear).collect();
    record_replay_output_failures(
        &label,
        &workload("c_record_replay_socketpair_blocking_read").path,
        &args,
        &format!(
            "{kind} original: read=5 errno=0 data=hello\n\
             {kind} dup: {dup_receive}=5 errno=0 data=hello\n\
             {kind} fork: {fork_receive}=5 errno=0 data=hello\n"
        ),
    )
}

/// Records `program` with `args` under `hermit record start` and then replays
/// it with `hermit replay --autopilot`. Each phase runs under a timeout, must
/// exit 0 and must print exactly `expected`. Returns a description, starting
/// with `label`, of each phase that failed.
fn record_replay_output_failures(
    label: &str,
    program: &Path,
    args: &[&str],
    expected: &str,
) -> Vec<String> {
    record_replay_output_failures_with_env(label, program, args, &[], expected).0
}

/// As `record_replay_output_failures`, with `envs` set for Hermit in both
/// phases. Also returns the stderr of each phase that ran, record first.
fn record_replay_output_failures_with_env(
    label: &str,
    program: &Path,
    args: &[&str],
    envs: &[(&str, &str)],
    expected: &str,
) -> (Vec<String>, Vec<String>) {
    record_replay_output_failures_with(
        label,
        program,
        args,
        &|command| {
            command.envs(envs.iter().copied());
        },
        &[],
        expected,
    )
}

/// As `record_replay_output_failures_with_env`, with `configure` applied to
/// the command of each phase.
fn record_replay_output_failures_with(
    label: &str,
    program: &Path,
    args: &[&str],
    configure: &dyn Fn(&mut Command),
    hermit_global: &[&str],
    expected: &str,
) -> (Vec<String>, Vec<String>) {
    let data_dir = tempfile::tempdir().expect("failed to create Hermit recording directory");
    let mut record = Command::new("timeout");
    configure(&mut record);
    record
        .args(["--kill-after=5s", "45s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(hermit_global)
        .args(["--log=off", "record", "start", "--record-timeout=30"])
        .arg(format!("--data-dir={}", data_dir.path().display()))
        .arg("--")
        .arg(program)
        .args(args);
    let mut replay = Command::new("timeout");
    configure(&mut replay);
    replay
        .args(["--kill-after=5s", "45s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(hermit_global)
        .args(["--log=off", "replay", "--autopilot"])
        .arg(format!("--data-dir={}", data_dir.path().display()));
    let mut failures = Vec::new();
    let mut stderrs = Vec::new();
    for (phase, mut command) in [("record", record), ("replay", replay)] {
        let output = command
            .output()
            .unwrap_or_else(|error| panic!("failed to start {label} {phase}: {error}"));
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if !output.status.success() || stdout != expected {
            failures.push(format!(
                "{label} {phase}: status {}\nstdout:\n{stdout}stderr:\n{stderr}",
                output.status,
            ));
            stderrs.push(stderr);
            // A replay of a failed recording says nothing more.
            break;
        }
        stderrs.push(stderr);
    }
    (failures, stderrs)
}

/// Control for the socketpair test below: a pipe read already waits for its
/// writer under record, because a pipe is container-internal.
#[test]
fn record_pipe_blocking_read_waits_for_forked_writer() {
    let _guard = hermit_record_lock();
    let failures = blocking_read_failures("pipe", None);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Both ends of a `socketpair(2)` are container-internal, like a pipe's, and
/// Hermit makes them physically nonblocking in the same way. Record used to
/// treat them as external I/O and issue the read once, so the guest saw
/// EAGAIN from a blocking read whenever its writer had not yet run. Every
/// socket type runs, so a failure reports all the types it affects.
#[test]
fn record_socketpair_blocking_read_waits_for_forked_writer() {
    let _guard = hermit_record_lock();
    let failures: Vec<String> = ["stream", "seqpacket", "dgram"]
        .into_iter()
        .flat_map(|kind| blocking_read_failures(kind, None))
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A guest that clears O_NONBLOCK on a socketpair endpoint, with FIONBIO as
/// Python's `socket.setblocking(True)` does or with a read-modify-write
/// F_SETFL, must still get a blocking read that waits for its writer. Record
/// used to forward the clear to the kernel for sockets, so the endpoint
/// became physically blocking while it was still classified internal, and
/// the next read tripped a debug assertion in Detcore (a hang in release
/// builds). `record_pipe_read_after_clearing_nonblocking_waits_for_its_writer`
/// is the pipe counterpart.
#[test]
fn record_socketpair_read_after_clearing_nonblocking_waits_for_its_writer() {
    let _guard = hermit_record_lock();
    let failures: Vec<String> = ["stream", "seqpacket", "dgram"]
        .into_iter()
        .flat_map(|kind| {
            ["fionbio", "setfl"]
                .into_iter()
                .flat_map(move |clear| blocking_read_failures(kind, Some(clear)))
        })
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A guest that accepts its own loopback TCP connections (see
/// `tests/c/record_replay_tcp_accept.c`). Record used to let `accept(2)` and
/// `accept4(2)` run live and logged nothing, so replay issued them against a
/// listener with no client and waited forever. Every round must print what
/// Linux gives the guest: the echoed bytes; the peer's address family, length
/// and host, in full and truncated to a 4-byte buffer that Linux must not
/// write past; the close-on-exec flag, both as `fcntl` reports it and as the
/// next descriptor of an exec'd child sees it; `SOCK_NONBLOCK`, through
/// `F_GETFL` and an `EAGAIN` read; and the `EAGAIN` and `EINVAL` failures.
#[test]
fn record_tcp_accept_replays_from_the_log() {
    let _guard = hermit_record_lock();
    let failures = record_replay_output_failures(
        "tcp-accept",
        &workload("c_record_replay_tcp_accept").path,
        &[],
        "client: go=2 sent=4 echo=ping\n\
         server accept: echoed=4 family=2 len=16 host=127.0.0.1\n\
         exec child: open fresh\n\
         client: go=2 sent=4 echo=ping\n\
         server accept4: echoed=4 cloexec=1\n\
         exec child: open reuse\n\
         client: go=2 sent=4 echo=ping\n\
         server truncated: echoed=4 family=2 len=16 untouched=1\n\
         client: go=2 sent=4 echo=ping\n\
         server nonblock: early=-1 errno=EAGAIN nonblock=1 echoed=4\n\
         server errors: empty=-1 errno=EAGAIN badflags=-1 errno=EINVAL\n",
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// What `accept4(2)` writes back at the edges (see
/// `tests/c/record_replay_accept_copyout.c`): address buffers and `*addrlen`
/// that end at, or straddle, an unmapped or read-only page. Record used to
/// read `*addrlen` and copy the address with plain memory accesses after the
/// call, so a legal short buffer next to an unmapped page turned a successful
/// accept into a recorded error, and an error Linux returned after writing
/// part of the guest's memory replayed without those bytes. An `*addrlen` on
/// a write-only page is legal, because on x86 Linux can read any writable user
/// page, while one on a `PROT_NONE` page faults. The last two cases call accept4
/// on a stack of the guest's own, one only 192 bytes above an unmapped page and
/// one whose zero `*addrlen` sits past the red zone: record used to borrow
/// guest stack below the red zone for the address, which faulted on the first
/// and overwrote the second's capacity. Recent Linux (7.1
/// and later) writes the length first and then the address, stopping at a
/// fault; record follows that order on every host, and each case's result,
/// length, copied prefix and untouched tail must match it in both phases. Linux
/// 7.0 and earlier copy the address first, so a native run there differs in the
/// two fault cases.
#[test]
fn record_accept_copies_the_peer_address_out_as_linux_does() {
    let _guard = hermit_record_lock();
    let failures = record_replay_output_failures(
        "accept-copyout",
        &workload("c_record_replay_accept_copyout").path,
        &[],
        "full: result=fd len=16 prefix=1 tail=1 nofd=1\n\
         zero: result=fd len=16 prefix=1 tail=1 nofd=1\n\
         truncated: result=fd len=16 prefix=1 tail=1 nofd=1\n\
         address-ends-at-page-4: result=fd len=16 prefix=1 tail=1 nofd=1\n\
         address-ends-at-page-1: result=fd len=16 prefix=1 tail=1 nofd=1\n\
         addrlen-ends-at-page: result=fd len=16 prefix=1 tail=1 nofd=1\n\
         addrlen-straddles-pages: result=fd len=16 prefix=1 tail=1 nofd=1\n\
         address-faults-after-4: result=EFAULT len=16 prefix=1 tail=1 nofd=1\n\
         addrlen-read-only: result=EFAULT len=16 prefix=1 tail=1 nofd=1\n\
         addrlen-unmapped: result=EFAULT len=-1 prefix=1 tail=1 nofd=1\n\
         addrlen-write-only: result=fd len=16 prefix=1 tail=1 nofd=1\n\
         addrlen-inaccessible: result=EFAULT len=-1 prefix=1 tail=1 nofd=1\n\
         addrlen-null: result=EFAULT len=-1 prefix=1 tail=1 nofd=1\n\
         negative: result=EINVAL len=-1 prefix=1 tail=1 nofd=1\n\
         tight-stack: result=fd len=16 prefix=1 tail=1 nofd=1\n\
         zero-addrlen-below-red-zone: result=fd len=16 prefix=1 tail=1 nofd=1\n",
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Linux reads `*addrlen` after `accept(2)` has waited for a connection, not
/// before. In `tests/c/record_replay_accept_copyout.c wait` one thread blocks
/// in accept with `*addrlen` 0 while another sets it to 16 and only then
/// connects. Record used to save the capacity before the wait, so the guest
/// got no address at all.
#[test]
fn record_accept_reads_the_address_capacity_after_the_wait() {
    let _guard = hermit_record_lock();
    let failures = record_replay_output_failures(
        "accept-wait",
        &workload("c_record_replay_accept_copyout").path,
        &["wait"],
        "wait: result=fd len=16 prefix=1 tail=1 connector=0\n",
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A thread cloned with `CLONE_THREAD` but without `CLONE_FILES` has a
/// descriptor table of its own (see `tests/c/record_replay_accept_copyout.c
/// private-table`). Record used to look up the socket such a thread accepted
/// in the leader's table, where the same number holds a decoy connection, and
/// so wrote the decoy's peer address into the guest's buffer. The accept must
/// get the connecting client's address, with and without an address buffer.
#[test]
fn record_accept_on_a_thread_with_its_own_fd_table_names_its_peer() {
    let _guard = hermit_record_lock();
    let program = &workload("c_record_replay_accept_copyout").path;
    let mut failures = record_replay_output_failures(
        "accept-private-table",
        program,
        &["private-table"],
        "private-table: result=fd fd=1 len=16 peer=1 decoy=1\n",
    );
    failures.extend(record_replay_output_failures(
        "accept-private-table-noaddr",
        program,
        &["private-table-noaddr"],
        "private-table-noaddr: result=fd fd=1 len=0 peer=1 decoy=1\n",
    ));
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Replay decides whether to reapply a recorded `setsockopt(2)` or
/// `shutdown(2)` by what the descriptor names: an accepted connection's
/// stand-in, a Unix socket, or another socket. For a thread with its own
/// descriptor table (see `tests/c/record_replay_accept_copyout.c
/// private-table-sockets`) that is the thread's table, where the number can
/// name something else than in the leader's. Replay used to look in the
/// leader's table, so it reapplied both calls to an accepted connection's
/// stand-in and failed, skipped a socketpair shutdown that a live `recvmmsg(2)`
/// then missed, and took a connected client socket for a Unix socket.
#[test]
fn replay_reapplies_socket_calls_against_the_calling_threads_own_fd_table() {
    let _guard = hermit_record_lock();
    let failures = record_replay_output_failures(
        "accept-private-table-sockets",
        &workload("c_record_replay_accept_copyout").path,
        &["private-table-sockets"],
        "private-table-sockets: accept_fd=1 setsockopt=0 shutdown=0 pair_fd=1 \
         pair_shutdown=0 pair_eof=1 client_fd=1 client_shutdown=0\n",
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// `shutdown(2)` on a socketpair endpoint must take effect on the live socket
/// in replay, because the receive that follows it runs live: Linux reports the
/// shutdown to `recvmmsg(2)` on the other end as one message of length 0
/// (see `tests/c/record_replay_socket_mmsg.c`). Replay used to return only the
/// recorded result of the shutdown, so the receive found the socket open and
/// failed with `EAGAIN`.
#[test]
fn replay_applies_a_socketpair_shutdown_before_a_live_recvmmsg() {
    let _guard = hermit_record_lock();
    let failures = record_replay_output_failures(
        "socketpair-mmsg",
        &workload("c_record_replay_socket_mmsg").path,
        &[],
        "socketpair: shutdown=0 recv=1 length=0 err=0\n",
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Record/replay serves an accepted connection's data from its log and stands
/// the connection in with a descriptor that carries none, so batched I/O that
/// would run live against it cannot be faithful. Record must stop and name the
/// call rather than let it reach that stand-in (see
/// `tests/c/record_replay_socket_mmsg.c`), and the guest must not print its
/// result line.
fn assert_recording_refuses_mmsg_on_an_accepted_connection(mode: &str, sysno: &str) {
    let _guard = hermit_record_lock();
    let data_dir = tempfile::tempdir().expect("failed to create recording directory");
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after=5s", "45s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["record", "start", "--record-timeout=30", "--data-dir"])
        .arg(data_dir.path())
        .arg("--")
        .arg(&workload("c_record_replay_socket_mmsg").path)
        .arg(mode);
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start the {sysno} recording: {error}"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_ne!(
        output.status.code(),
        Some(124),
        "{sysno} recording hung: {rendered}"
    );
    assert!(
        !output.status.success(),
        "{sysno} recording reported success: {rendered}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains(&format!("unsupported syscall: {sysno}")),
        "{sysno} recording did not name {sysno}:\n{stderr}"
    );
    assert!(
        !stdout.contains("accepted:"),
        "the guest ran past the refused {sysno}: {stdout}"
    );
}

#[test]
fn recording_refuses_sendmmsg_on_an_accepted_connection() {
    assert_recording_refuses_mmsg_on_an_accepted_connection("accepted", "sendmmsg");
}

#[test]
fn recording_refuses_recvmmsg_on_an_accepted_connection() {
    assert_recording_refuses_mmsg_on_an_accepted_connection("accepted-recv", "recvmmsg");
}

/// Replay's refusal of a recording in which an accepted connection holds a
/// different descriptor slot than it did during recording, as
/// `hermit-cli/src/replayer.rs` prints it.
const ACCEPT_SLOT_REFUSAL: &str = "replay refused: an accepted connection's \
    descriptor slot differs from the recording, because record does not log where a \
    backgrounded accept took its slot (https://github.com/rrnewton/hermit/issues/3880)";

/// One thread accepts loopback TCP connections while another opens and closes
/// descriptors (see `tests/c/record_replay_tcp_accept_threads.c`). Replay
/// serves each accept from the log and refuses to continue if the recorded
/// descriptor number is not the one free at that point. Replay runs the
/// accept in the background, as record did, and the scheduler used to commit
/// the other thread's turns while it ran, so where its stand-in landed among
/// that thread's calls depended on host timing and replays of one recording
/// disagreed: some finished and some refused. Every replay of the recording
/// must now end with the same exit status and output.
///
/// Record does not yet capture where among the other thread's calls the
/// recorded accept ran, so these replays may all refuse rather than all
/// succeed; https://github.com/rrnewton/hermit/issues/3880 tracks that. Replay
/// catches it at one of two points: at the accept, when its recorded slot is
/// taken, or at the other thread's open, when the accept's stand-in holds the
/// slot that open recorded. Both refuse with the same named message. The test
/// requires the recording to succeed, the replays to agree, and their common
/// outcome to be either the recorded output or that refusal. Any other
/// descriptor-slot divergence panics with a different message and fails it.
#[test]
fn replays_of_a_threaded_tcp_accept_recording_agree() {
    assert_threaded_accept_replays_agree(&[]);
}

/// As `replays_of_a_threaded_tcp_accept_recording_agree`, but the guest
/// accepts through a listener it received over `SCM_RIGHTS`. Detcore does not
/// track a received descriptor, so replay must hold the scheduler for the
/// accept because of the call itself, not because it knows the listener.
#[test]
fn replays_of_a_threaded_tcp_accept_recording_agree_for_a_received_listener() {
    assert_threaded_accept_replays_agree(&["scm-rights"]);
}

fn assert_threaded_accept_replays_agree(guest_args: &[&str]) {
    const REPLAYS: usize = 10;
    let _guard = hermit_record_lock();
    let data_dir = tempfile::tempdir().expect("failed to create Hermit recording directory");
    let mut record = Command::new("timeout");
    record
        .args(["--kill-after=5s", "45s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=off", "record", "start", "--record-timeout=30"])
        .arg(format!("--data-dir={}", data_dir.path().display()))
        .arg("--")
        .arg(&workload("c_record_replay_tcp_accept_threads").path)
        .args(guest_args);
    let recorded = command_output(record, "recording of the threaded accept workload");
    let recorded_stdout = String::from_utf8_lossy(&recorded.stdout).into_owned();
    assert_eq!(recorded_stdout, "threads: echoed=20 clients_ok=1\n");
    let outcomes: Vec<(Option<i32>, String, String)> = (0..REPLAYS)
        .map(|_| {
            let mut replay = Command::new("timeout");
            replay
                .args(["--kill-after=5s", "45s"])
                .arg(env!("CARGO_BIN_EXE_hermit"))
                .args(["--log=off", "replay", "--autopilot"])
                .arg(format!("--data-dir={}", data_dir.path().display()));
            let output = replay.output().expect("failed to start replay");
            (
                output.status.code(),
                String::from_utf8_lossy(&output.stdout).into_owned(),
                String::from_utf8_lossy(&output.stderr).into_owned(),
            )
        })
        .collect();
    let (first_status, first_stdout, _) = &outcomes[0];
    let disagreeing: Vec<String> = outcomes
        .iter()
        .enumerate()
        .filter(|(_, (status, stdout, _))| status != first_status || stdout != first_stdout)
        .map(|(index, (status, stdout, stderr))| {
            format!("replay {index}: status {status:?}\nstdout:\n{stdout}stderr:\n{stderr}")
        })
        .collect();
    assert!(
        disagreeing.is_empty(),
        "replay 0 exited {first_status:?} with stdout:\n{first_stdout}stderr:\n{}\n\
         {} of {REPLAYS} replays disagreed with it:\n{}",
        outcomes[0].2,
        disagreeing.len(),
        disagreeing.join("\n"),
    );
    let finished = *first_status == Some(0) && *first_stdout == recorded_stdout;
    let refused = *first_status == Some(HERMIT_INTERNAL_FAILURE_EXIT)
        && outcomes[0].2.contains(ACCEPT_SLOT_REFUSAL);
    assert!(
        finished || refused,
        "every replay exited {first_status:?}, which is neither the recorded output nor the \
         descriptor-order refusal:\nstdout:\n{first_stdout}stderr:\n{}",
        outcomes[0].2,
    );
    eprintln!(
        "all {REPLAYS} replays {}",
        if finished { "finished" } else { "refused" }
    );
}

/// Record's refusal of a close, `dup2`/`dup3` or `close_range` that would
/// close a listener a blocking accept still waits on, as Detcore prints it.
const ACCEPT_CLOSE_REFUSAL: &str = "hermit refused the recording: a descriptor was closed \
    while a blocking accept still waits on its listener";

/// Record's refusal of an accept whose listener was passed over `SCM_RIGHTS`
/// while it waited, as Detcore prints it.
const ACCEPT_EXPORT_REFUSAL: &str = "hermit refused the recording: a blocking accept's \
    listener was passed over SCM_RIGHTS while the accept waited";

/// Record's refusal of an accept whose listener's file status flags could
/// not be restored after its nonblocking attempt, as Detcore prints it.
const ACCEPT_RESTORE_REFUSAL: &str = "hermit refused the recording: an accept could not \
    restore its listener's file status flags";

/// What Detcore prints when `HERMIT_TEST_ACCEPT_BRACKET_FAULT=kill-after-set`
/// kills the accepting task while its listener is temporarily nonblocking.
const ACCEPT_BRACKET_KILL_NOTICE: &str =
    "accept bracket fault: killed the accepting task after the temporary O_NONBLOCK";

/// Records and replays `mode` of `tests/c/record_replay_accept_in_turn.c` and
/// returns a failure for each phase whose output is not `expected`.
fn accept_in_turn_failures(mode: &str, expected: &str) -> Vec<String> {
    record_replay_output_failures(
        mode,
        &workload("c_record_replay_accept_in_turn").path,
        &[mode],
        expected,
    )
}

/// Records `mode` of `tests/c/record_replay_accept_in_turn.c` with `envs` set
/// and requires the recording to stop with `refusal` on stderr, without
/// hanging and before the guest prints the line that names `mode`.
fn assert_accept_in_turn_recording_refuses(mode: &str, envs: &[(&str, &str)], refusal: &str) {
    assert_recording_refuses(
        mode,
        &workload("c_record_replay_accept_in_turn").path,
        &[mode],
        &|command| {
            command.envs(envs.iter().copied());
        },
        &[],
        refusal,
    );
}

/// Records `program` with `args` and `configure` applied to the command, and
/// requires the recording to stop with `refusal` on stderr, without hanging
/// and before the guest prints a line starting with `mode`.
fn assert_recording_refuses(
    mode: &str,
    program: &Path,
    args: &[&str],
    configure: &dyn Fn(&mut Command),
    hermit_global: &[&str],
    refusal: &str,
) {
    let data_dir = tempfile::tempdir().expect("failed to create recording directory");
    let mut command = Command::new("timeout");
    configure(&mut command);
    command
        .args(["--kill-after=5s", "45s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(hermit_global)
        .args(["--log=off", "record", "start", "--record-timeout=30"])
        .arg(format!("--data-dir={}", data_dir.path().display()))
        .arg("--")
        .arg(program)
        .args(args);
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start the {mode} recording: {error}"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_ne!(
        output.status.code(),
        Some(124),
        "{mode} recording hung: {rendered}"
    );
    assert!(
        !output.status.success(),
        "{mode} recording reported success: {rendered}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains(refusal),
        "{mode} recording did not refuse with {refusal:?}:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !stdout.contains(&format!("{mode}: ")),
        "the {mode} guest ran past the refusal: {stdout}"
    );
}

/// A blocking accept on a listener the container created runs in the
/// caller's turn, one nonblocking attempt at a time, until a connection
/// arrives, its `SO_RCVTIMEO` expires or a signal ends it (see
/// `tests/c/record_replay_accept_in_turn.c`). Record used to run it in the
/// background, so the kernel chose the accepted descriptor's number at a
/// host-timed point (https://github.com/rrnewton/hermit/issues/3880). Each
/// case must print, under record and under replay, exactly what it prints
/// natively; every case runs, so a failure reports all the cases it affects.
#[test]
fn record_replay_blocking_accept_in_turn_matches_linux() {
    let _guard = hermit_record_lock();
    let failures: Vec<String> = [
        (
            "late-connector",
            "late-connector: accept=fd echoed=4 helper=0\n",
        ),
        ("reset", "reset: accept=fd read=-1 ECONNRESET\n"),
        (
            "sibling-setfl",
            "sibling-setfl: accept=fd echoed=4 helper=0 nonblock-after=1\n",
        ),
        ("timeout-dup", "timeout-dup: original=EAGAIN dup=EAGAIN\n"),
        ("shutdown", "shutdown: shutdown=0 accept=EINVAL\n"),
        (
            "nonblocking",
            "nonblocking: empty=EAGAIN queued=fd conn-nonblock=1\n",
        ),
        ("accept-once", "accept-once: accept=fd echoed=4 client=1\n"),
    ]
    .into_iter()
    .flat_map(|(mode, expected)| accept_in_turn_failures(mode, expected))
    .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A handled signal ends a waiting accept as Linux ends it (signal(7)): an
/// untimed accept whose handler has `SA_RESTART` restarts and takes the
/// connection that comes later; without `SA_RESTART`, or on a listener with
/// `SO_RCVTIMEO`, it fails with `EINTR`. An `SO_RCVTIMEO` of `{LONG_MAX, 0}`,
/// which Linux stores as no timeout, restarts like an untimed accept.
#[test]
fn record_replay_blocking_accept_signals_match_linux() {
    let _guard = hermit_record_lock();
    let failures: Vec<String> = [
        (
            "signal-restart",
            "signal-restart: accept=fd echoed=4 handled=1 helper=0\n",
        ),
        (
            "signal-norestart",
            "signal-norestart: accept=EINTR echoed=-1 handled=1 helper=0\n",
        ),
        (
            "signal-timed-restart",
            "signal-timed-restart: accept=EINTR echoed=-1 handled=1 helper=0\n",
        ),
        (
            "signal-timed-norestart",
            "signal-timed-norestart: accept=EINTR echoed=-1 handled=1 helper=0\n",
        ),
        (
            "signal-unbounded-restart",
            "signal-unbounded-restart: accept=fd echoed=4 handled=1 helper=0\n",
        ),
    ]
    .into_iter()
    .flat_map(|(mode, expected)| accept_in_turn_failures(mode, expected))
    .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A blocking accept that nothing connects to never returns on Linux. Under
/// record it must keep waiting until `--record-timeout` ends the recording,
/// rather than return early or stall the host.
#[test]
fn record_blocking_accept_without_a_connector_waits_for_the_record_timeout() {
    let _guard = hermit_record_lock();
    let data_dir = tempfile::tempdir().expect("failed to create Hermit recording directory");
    let started = Instant::now();
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after=5s", "45s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=off", "record", "start", "--record-timeout=2"])
        .arg(format!("--data-dir={}", data_dir.path().display()))
        .arg("--")
        .arg(&workload("c_record_replay_accept_in_turn").path)
        .arg("unwakeable");
    let output = command.output().expect("failed to start the recording");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Hermit's deadline and the outer `timeout` both exit 124, so the elapsed
    // time and the deadline's message tell them apart.
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "recording hung past its deadline: {:?}",
        started.elapsed()
    );
    assert_eq!(
        output.status.code(),
        Some(124),
        "recording did not end with the deadline's status:\nstdout:\n{stdout}stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("Recording timed out after 2 seconds"),
        "the recording did not end at its deadline:\nstdout:\n{stdout}stderr:\n{stderr}"
    );
    assert_eq!(stdout, "", "the accept returned without a connection");
}

/// Unit coverage of the guard behind the accept bracket lives in Detcore
/// (`accept_bracket_guard_restores_on_drop`). Here, a forked child waits in
/// accept on its parent's listener, and the test-only fault hook kills it just
/// after Detcore made the shared description nonblocking for one attempt. The
/// guard must restore the flags through Detcore's own copy of the listener, so
/// the parent sees a blocking listener and its own accept waits for a delayed
/// connector. Replay kills the child at the same point.
#[test]
fn record_accept_bracket_kill_restores_a_forked_listener() {
    let _guard = hermit_record_lock();
    let (failures, stderrs) = record_replay_output_failures_with_env(
        "bracket-kill",
        &workload("c_record_replay_accept_in_turn").path,
        &["bracket-kill"],
        &[("HERMIT_TEST_ACCEPT_BRACKET_FAULT", "kill-after-set")],
        "bracket-kill: child-killed=1 nonblock=0 accept=fd echoed=4 client=1\n",
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert!(
        stderrs[0].contains(ACCEPT_BRACKET_KILL_NOTICE),
        "the fault hook never killed the accepting child during record:\n{}",
        stderrs[0]
    );
}

/// When the accept bracket cannot restore its listener's flags, the shared
/// description would stay nonblocking for every other alias, so record must
/// stop and say why.
#[test]
fn recording_refuses_a_failed_accept_bracket_restore() {
    let _guard = hermit_record_lock();
    assert_accept_in_turn_recording_refuses(
        "accept-once",
        &[("HERMIT_TEST_ACCEPT_BRACKET_FAULT", "restore-fails")],
        ACCEPT_RESTORE_REFUSAL,
    );
}

/// Once a listener has been passed over `SCM_RIGHTS`, a process outside
/// Detcore's view may hold it, so the bracket must not change its flags
/// again. An accept already waiting on it must refuse by name at its next
/// attempt, whether the listener went in a `sendmsg`, in the second message
/// of a `sendmmsg`, or in a `sendmsg` that failed (Linux may still have
/// queued it, and both phases must decide alike).
#[test]
fn recording_refuses_an_accept_whose_listener_is_exported_mid_wait() {
    let _guard = hermit_record_lock();
    for mode in [
        "export-sendmsg",
        "export-sendmmsg-second",
        "export-failed-send",
    ] {
        assert_accept_in_turn_recording_refuses(mode, &[], ACCEPT_EXPORT_REFUSAL);
    }
}

/// Each waiting accept guards its listener with its own entry. When one of
/// two waiters returns, a close must still be refused because of the other;
/// once both have returned, the close goes ahead.
#[test]
fn record_accept_close_guard_is_per_operation() {
    let _guard = hermit_record_lock();
    assert_accept_in_turn_recording_refuses("close-one-left", &[], ACCEPT_CLOSE_REFUSAL);
    let failures = accept_in_turn_failures(
        "close-both-done",
        "close-both-done: close=0 clients=0,0 accepted=1,1\n",
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A process that shares the waiter's descriptor table (`CLONE_FILES`, not a
/// thread) and then execs gets a table of its own, without the waiter's
/// guards, so it may close its copy of the listener. The waiter's table
/// keeps its guard: a later close there is refused.
#[test]
fn record_accept_close_guard_does_not_follow_exec() {
    let _guard = hermit_record_lock();
    let failures = accept_in_turn_failures(
        "exec-sharer-close",
        "close-helper: open=1 close=0\n\
         exec-sharer-close: sharer=1 close=skipped helper=0 accepted=1\n",
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert_accept_in_turn_recording_refuses("exec-sharer-then-close", &[], ACCEPT_CLOSE_REFUSAL);
}

/// A sharer whose exec fails still shares the waiter's table, guards
/// included, so its close of the listener must be refused.
#[test]
fn record_accept_close_guard_survives_a_failed_exec() {
    let _guard = hermit_record_lock();
    assert_accept_in_turn_recording_refuses("failed-exec-close", &[], ACCEPT_CLOSE_REFUSAL);
}

/// Record's refusal of an accept whose listener Detcore cannot copy or make
/// nonblocking for one attempt, as Detcore prints it.
const ACCEPT_SET_REFUSAL: &str = "hermit refused the recording: an accept could not make \
    its listener nonblocking for one attempt";

/// Each case queues its connection before the accept, so none depends on a
/// timer, a signal or another thread, and each must print under record and
/// replay what it prints natively (see `tests/c/record_replay_accept_in_turn.c`):
/// - a tight stack, and an address length just below the red zone with a
///   capacity of 0: record used to read the listener's `SO_RCVTIMEO` through a
///   `getsockopt` whose buffers it pushed below the guest's stack pointer, so
///   the first accept failed with `EFAULT` and the second wrote an address
///   Linux leaves alone;
/// - an accept on a thread other than the leader;
/// - a negative `SO_RCVTIMEO`, which Linux takes as an immediate timeout but
///   reads back as no timeout at all, so record used to wait forever;
/// - an `SO_RCVTIMEO` a forked child set on the listener it shares with its
///   parent, which the parent's accept must wait by (Detcore models the
///   timeout per open file description, from the `setsockopt` calls it sees).
#[test]
fn record_replay_accept_entry_cases_match_linux() {
    let _guard = hermit_record_lock();
    let failures: Vec<String> = [
        (
            "scratch-tight",
            "scratch-tight: accept=fd len=16 changed=1\n",
        ),
        (
            "scratch-alias",
            "scratch-alias: accept=fd len=16 changed=0\n",
        ),
        ("worker-queued", "worker-queued: accept=fd nonleader=1\n"),
        ("negative-timeout", "negative-timeout: accept=EAGAIN\n"),
        (
            "timeout-set-by-child",
            "timeout-set-by-child: set=1 accept=EAGAIN\n",
        ),
    ]
    .into_iter()
    .flat_map(|(mode, expected)| accept_in_turn_failures(mode, expected))
    .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Makes every `pidfd_open(pid, PIDFD_THREAD)` in this process and its
/// descendants fail with `EINVAL`, as Linux before 6.9 does, and lets every
/// other system call through.
/// Global options for a hermit whose test installs a seccomp filter in it:
/// the filter is inherited, which hermit refuses unless told to ignore it.
const UNDER_HOST_FILTER: &[&str] = &["--unsafe-ignore-host-seccomp"];

fn deny_thread_pidfds(command: &mut Command) {
    const PIDFD_THREAD: u32 = libc::O_EXCL as u32;
    const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;
    // SAFETY: the closure makes only prctl calls, which are async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            let filter = [
                // Kill any other architecture.
                libc::sock_filter {
                    code: 0x20,
                    jt: 0,
                    jf: 0,
                    k: 4,
                },
                libc::sock_filter {
                    code: 0x15,
                    jt: 1,
                    jf: 0,
                    k: AUDIT_ARCH_X86_64,
                },
                libc::sock_filter {
                    code: 0x06,
                    jt: 0,
                    jf: 0,
                    k: libc::SECCOMP_RET_KILL_PROCESS,
                },
                // pidfd_open(_, PIDFD_THREAD) fails with EINVAL.
                libc::sock_filter {
                    code: 0x20,
                    jt: 0,
                    jf: 0,
                    k: 0,
                },
                libc::sock_filter {
                    code: 0x15,
                    jt: 0,
                    jf: 3,
                    k: libc::SYS_pidfd_open as u32,
                },
                libc::sock_filter {
                    code: 0x20,
                    jt: 0,
                    jf: 0,
                    k: 24,
                },
                libc::sock_filter {
                    code: 0x15,
                    jt: 0,
                    jf: 1,
                    k: PIDFD_THREAD,
                },
                libc::sock_filter {
                    code: 0x06,
                    jt: 0,
                    jf: 0,
                    k: libc::SECCOMP_RET_ERRNO | libc::EINVAL as u32,
                },
                libc::sock_filter {
                    code: 0x06,
                    jt: 0,
                    jf: 0,
                    k: libc::SECCOMP_RET_ALLOW,
                },
            ];
            let program = libc::sock_fprog {
                len: filter.len() as u16,
                filter: filter.as_ptr() as *mut libc::sock_filter,
            };
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                || libc::prctl(
                    libc::PR_SET_SECCOMP,
                    libc::SECCOMP_MODE_FILTER,
                    &program as *const libc::sock_fprog,
                ) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// On Linux before 6.9 there is no thread pidfd. Detcore's copy of a
/// listener a non-leader thread accepts on must then come through the
/// leader's pidfd, but only when Linux confirms (`KCMP_FILES`) that the thread
/// shares the leader's table: a thread sharing it accepts as on Linux, and a
/// thread with a table of its own is refused by name, never served from the
/// leader's table. Simulated by failing `PIDFD_THREAD` opens with seccomp.
#[test]
fn record_accept_on_a_non_leader_without_thread_pidfds() {
    let _guard = hermit_record_lock();
    let (failures, _) = record_replay_output_failures_with(
        "worker-queued without PIDFD_THREAD",
        &workload("c_record_replay_accept_in_turn").path,
        &["worker-queued"],
        &deny_thread_pidfds,
        UNDER_HOST_FILTER,
        "worker-queued: accept=fd nonleader=1\n",
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert_recording_refuses(
        "private-table",
        &workload("c_record_replay_accept_copyout").path,
        &["private-table"],
        &deny_thread_pidfds,
        UNDER_HOST_FILTER,
        ACCEPT_SET_REFUSAL,
    );
}

/// A listener passed over `SCM_RIGHTS` must not be made nonblocking again,
/// even when Detcore cannot read the message's control buffer, which Linux
/// reads all the same: here it is write-only. Under the fault hook, a bracket
/// at the accept that follows reports a failed restore, so the recording
/// refuses if the export scan let the listener through.
#[test]
fn record_accept_after_an_unreadable_scm_rights_control_keeps_mains_path() {
    let _guard = hermit_record_lock();
    let (failures, _) = record_replay_output_failures_with_env(
        "export-write-only",
        &workload("c_record_replay_accept_in_turn").path,
        &["export-write-only"],
        &[("HERMIT_TEST_ACCEPT_BRACKET_FAULT", "restore-fails")],
        "export-write-only: sent=1 accept=fd\n",
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// An accept checks its listener for an export before every attempt, the
/// first included: the request for its first turn yields, so a thread
/// already runnable can pass the listener over `SCM_RIGHTS` after the accept
/// found it eligible. The guest yields N times before exporting; swept over
/// N, every run either accepts as Linux does (the export came before the
/// accept's entry, or after its first attempt took the queued connection) or
/// refuses by name, and at least one N lands in the window and refuses.
#[test]
fn record_accept_refuses_an_export_before_its_first_attempt() {
    let _guard = hermit_record_lock();
    let program = &workload("c_record_replay_accept_in_turn").path;
    let mut refused = Vec::new();
    let mut failures = Vec::new();
    for spins in 0..8 {
        let spins = spins.to_string();
        let data_dir = tempfile::tempdir().expect("failed to create recording directory");
        let mut command = Command::new("timeout");
        command
            .args(["--kill-after=5s", "45s"])
            .arg(env!("CARGO_BIN_EXE_hermit"))
            .args(["--log=off", "record", "start", "--record-timeout=30"])
            .arg(format!("--data-dir={}", data_dir.path().display()))
            .arg("--")
            .arg(program)
            .args(["export-at-entry", &spins]);
        let output = command.output().expect("failed to start the recording");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if output.status.success() && stdout == "export-at-entry: accept=fd export=0\n" {
            continue;
        }
        if output.status.code() == Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT)
            && stderr.contains(ACCEPT_EXPORT_REFUSAL)
            && !stdout.contains("export-at-entry: ")
        {
            refused.push(spins);
            continue;
        }
        failures.push(format!(
            "N={spins}: status {}\nstdout:\n{stdout}stderr:\n{stderr}",
            output.status
        ));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert!(
        !refused.is_empty(),
        "no N in 0..8 exported the listener between the accept's entry and its first attempt"
    );
}

/// Replay's refusal of a recording whose rejoin log it cannot follow, as
/// Detcore prints it.
const REJOIN_REFUSAL: &str = "hermit replay refused to continue: ";

/// Replay requires the recording's rejoin log whether or not the run
/// backgrounds a call, and requires the run to reach every readmission it
/// logs: a quiet recording (`/bin/true`, no backgrounded call) must refuse by
/// name when its log is missing or carries a readmission past the run's end,
/// and replay unchanged.
#[test]
fn replay_refuses_a_missing_or_unexhausted_rejoin_log() {
    let _guard = hermit_record_lock();
    let data_dir = tempfile::tempdir().expect("failed to create Hermit recording directory");
    let mut record = Command::new("timeout");
    record
        .args(["--kill-after=5s", "45s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=off", "record", "start", "--record-timeout=30"])
        .arg(format!("--data-dir={}", data_dir.path().display()))
        .args(["--", "/bin/true"]);
    command_output(record, "the /bin/true recording");
    let recording_id =
        fs::read_to_string(data_dir.path().join("last")).expect("recording did not publish its ID");
    let log = data_dir.path().join(recording_id.trim()).join("rejoins");
    let recorded = fs::read_to_string(&log).expect("the recording has no rejoin log");
    assert_eq!(
        recorded, "hermit-rejoins 1\n",
        "the quiet recording logged a readmission"
    );

    let replay = || {
        let mut replay = Command::new("timeout");
        replay
            .args(["--kill-after=5s", "45s"])
            .arg(env!("CARGO_BIN_EXE_hermit"))
            .args(["--log=off", "replay", "--autopilot"])
            .arg(format!("--data-dir={}", data_dir.path().display()));
        replay.output().expect("failed to start the replay")
    };
    let intact = replay();
    assert!(
        intact.status.success(),
        "the intact recording did not replay: {}",
        String::from_utf8_lossy(&intact.stderr)
    );

    let mut failures = Vec::new();
    for (case, contents, reason) in [
        ("missing", None, "rejoin log cannot be read"),
        (
            "extra tail",
            Some("hermit-rejoins 1\n999999999 0 3:1\n"),
            "1 logged readmissions not replayed, the first at turn 999999999",
        ),
    ] {
        match contents {
            None => fs::remove_file(&log).expect("cannot remove the rejoin log"),
            Some(contents) => fs::write(&log, contents).expect("cannot rewrite the rejoin log"),
        }
        let output = replay();
        let stderr = String::from_utf8_lossy(&output.stderr);
        if output.status.code() != Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT)
            || !stderr.contains(REJOIN_REFUSAL)
            || !stderr.contains(reason)
        {
            failures.push(format!(
                "{case}: replay exited {:?} without refusing with {reason:?}:\n{stderr}",
                output.status.code()
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The scheduler records that place a guest's SIGCHLD in a ptrace log: every
/// committed turn, the delivery of each inbound SIGCHLD, and the synthetic
/// "Alarm fired" send.
fn sigchld_boundary_records(log: &Path) -> Vec<String> {
    let text = fs::read_to_string(log)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", log.display()));
    text.lines()
        .filter_map(|line| {
            if let Some(start) = line.find("COMMIT turn ") {
                let record = &line[start..];
                // "COMMIT turn N, dettid D using ...": keep the turn and thread.
                let end = record
                    .match_indices(", dettid ")
                    .next()
                    .and_then(|(at, separator)| {
                        record[at + separator.len()..]
                            .find(|c: char| !c.is_ascii_digit())
                            .map(|digits| at + separator.len() + digits)
                    })
                    .unwrap_or(record.len());
                Some(record[..end].to_string())
            } else if line.contains("signal (#") && line.ends_with(" SIGCHLD") {
                line.find("[dtid ").map(|start| line[start..].to_string())
            } else {
                line.find("Alarm fired")
                    .map(|start| line[start..].to_string())
            }
        })
        .collect()
}

/// SIGCHLD Phase A on the recorded path. A child exits while its parent leaves
/// SIGCHLD at SIG_DFL and unblocked, once while the parent is blocked in a
/// recorded pipe read and once while it waits in waitpid. Linux discards that
/// notification, so ptrace Hermit sends no synthetic SIGCHLD ("Alarm fired")
/// and instead holds turns until the kernel publishes the exit; the kernel's
/// own SIGCHLD then reaches Detcore as an inbound signal. Recorded reads
/// inject directly, while replay supplies their recorded results, so the
/// delivery point cannot be inferred from the recording alone: the replay
/// must receive each SIGCHLD between the same committed turns, on the same
/// thread, with no extra turn.
#[test]
fn replay_receives_an_ignored_sigchld_at_the_recorded_boundary() {
    let _guard = hermit_record_lock();
    let program = &workload("c_record_replay_sigchld_ignored_boundary").path;
    let data_dir = tempfile::tempdir().expect("failed to create Hermit recording directory");
    let logs = tempfile::tempdir().expect("failed to create log directory");
    let record_log = logs.path().join("record.log");
    let replay_log = logs.path().join("replay.log");

    let mut record = Command::new("timeout");
    record
        .args(["--kill-after=5s", "45s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=info", "--backend=ptrace"])
        .arg(format!("--log-file={}", record_log.display()))
        .args(["record", "start", "--strict", "--record-timeout=30"])
        .arg(format!("--data-dir={}", data_dir.path().display()))
        .arg("--")
        .arg(program);
    let recorded = command_output(record, "recording of the ignored-SIGCHLD workload");

    let mut replay = Command::new("timeout");
    replay
        .args(["--kill-after=5s", "45s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .arg("--log=info")
        .arg(format!("--log-file={}", replay_log.display()))
        .args(["replay", "--autopilot"])
        .arg(format!("--data-dir={}", data_dir.path().display()));
    let replayed = command_output(replay, "replay of the ignored-SIGCHLD workload");

    // Linux's output: the read returns the writer's byte after the reap, with
    // no EINTR, and nothing is ever pending.
    let expected = "read=1 byte=w errno=- reaped_before_return=1 sigchld_pending=0\n\
                    waitpid=child status=3 sigchld_pending=0\n";
    assert_eq!(String::from_utf8_lossy(&recorded.stdout), expected);
    assert_eq!(String::from_utf8_lossy(&replayed.stdout), expected);

    let recorded_boundaries = sigchld_boundary_records(&record_log);
    let replayed_boundaries = sigchld_boundary_records(&replay_log);
    for (label, records) in [
        ("recording", &recorded_boundaries),
        ("replay", &replayed_boundaries),
    ] {
        let inbound = records
            .iter()
            .filter(|record| record.contains("handling inbound signal"))
            .count();
        assert_eq!(
            inbound, 2,
            "the {label} must receive the kernel's SIGCHLD once per child: {records:#?}"
        );
        assert!(
            !records
                .iter()
                .any(|record| record.starts_with("Alarm fired")),
            "the {label} sent a synthetic SIGCHLD for an ignored notification: {records:#?}"
        );
    }
    assert_eq!(
        recorded_boundaries, replayed_boundaries,
        "the replay received SIGCHLD at a different scheduler boundary"
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
    record_rs_select => "rustbin_select",
    record_rs_stack_ptr => "rustbin_stack_ptr",
    record_rs_thread_random => "rustbin_thread_random",
}

#[path = "record_replay/network_trace.rs"]
mod network_trace;
