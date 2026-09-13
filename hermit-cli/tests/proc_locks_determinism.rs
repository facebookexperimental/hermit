/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::env;
use std::ffi::CString;
use std::ffi::OsStr;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::ErrorKind;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs as unix_fs;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;
use std::thread;
use std::time::Duration;
use std::time::Instant;

struct ProcLocksSnapshotLease {
    _file: File,
}

const PROC_LOCKS_LEASE_NAME: &str = "hermit-proc-locks-determinism.lock";

fn effective_uid() -> u32 {
    // SAFETY: `geteuid` takes no arguments and has no memory-safety preconditions.
    unsafe { libc::geteuid() }
}

fn validated_runtime_directory(
    configured: Option<&OsStr>,
    fallback: &Path,
    expected_uid: u32,
) -> io::Result<PathBuf> {
    let configured = configured.map(Path::new);
    let directory = match configured {
        Some(path) if !path.as_os_str().is_empty() && path.is_absolute() => path,
        _ => fallback,
    };
    if !directory.is_absolute() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "proc-locks runtime directory is not absolute",
        ));
    }
    let metadata = fs::symlink_metadata(directory)?;
    if !metadata.file_type().is_dir() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "proc-locks runtime path is not a directory",
        ));
    }
    if metadata.uid() != expected_uid {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            "proc-locks runtime directory is not owned by the current user",
        ));
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            "proc-locks runtime directory is accessible by another user",
        ));
    }
    Ok(directory.to_path_buf())
}

fn open_proc_locks_snapshot_lease_file(
    runtime_directory: &Path,
    expected_uid: u32,
) -> io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(runtime_directory.join(PROC_LOCKS_LEASE_NAME))?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "proc-locks snapshot lease is not a regular file",
        ));
    }
    if metadata.uid() != expected_uid {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            "proc-locks snapshot lease is not owned by the current user",
        ));
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            "proc-locks snapshot lease is accessible by another user",
        ));
    }
    Ok(file)
}

fn acquire_proc_locks_snapshot_lease() -> ProcLocksSnapshotLease {
    let expected_uid = effective_uid();
    let fallback = PathBuf::from(format!("/run/user/{expected_uid}"));
    let configured = env::var_os("XDG_RUNTIME_DIR");
    let runtime_directory =
        validated_runtime_directory(configured.as_deref(), &fallback, expected_uid)
            .expect("validate proc-locks runtime directory");
    let file = open_proc_locks_snapshot_lease_file(&runtime_directory, expected_uid)
        .expect("open proc-locks snapshot lease");
    loop {
        // SAFETY: `file` owns this valid descriptor and remains alive in the
        // returned guard. BSD flock is released by the kernel if the process dies.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
            break;
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != ErrorKind::Interrupted {
            panic!("acquire proc-locks snapshot lease: {error}");
        }
    }
    ProcLocksSnapshotLease { _file: file }
}

#[test]
fn proc_locks_snapshot_lease_rejects_unsafe_paths() {
    let expected_uid = effective_uid();
    let root = tempfile::Builder::new()
        .prefix("proc-locks-lease-safety-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("create proc-locks lease safety directory");
    let safe = root.path().join("safe");
    fs::create_dir(&safe).expect("create private runtime directory");
    fs::set_permissions(&safe, fs::Permissions::from_mode(0o700))
        .expect("make runtime directory private");

    assert_eq!(
        validated_runtime_directory(Some(safe.as_os_str()), &safe, expected_uid)
            .expect("accept safe runtime directory"),
        safe
    );
    for configured in [None, Some(OsStr::new("")), Some(OsStr::new("relative"))] {
        assert_eq!(
            validated_runtime_directory(configured, &safe, expected_uid)
                .expect("fall back from an absent or non-absolute runtime directory"),
            safe
        );
    }

    let unsafe_directory = root.path().join("unsafe");
    fs::create_dir(&unsafe_directory).expect("create unsafe runtime directory");
    fs::set_permissions(&unsafe_directory, fs::Permissions::from_mode(0o755))
        .expect("make runtime directory unsafe");
    assert_eq!(
        validated_runtime_directory(Some(unsafe_directory.as_os_str()), &safe, expected_uid)
            .expect_err("reject group/world-accessible runtime directory")
            .kind(),
        ErrorKind::PermissionDenied
    );

    let runtime_symlink = root.path().join("runtime-symlink");
    unix_fs::symlink(&safe, &runtime_symlink).expect("create runtime-directory symlink fixture");
    assert_eq!(
        validated_runtime_directory(Some(runtime_symlink.as_os_str()), &safe, expected_uid)
            .expect_err("reject symlink runtime directory")
            .kind(),
        ErrorKind::InvalidInput
    );

    let safe_file = open_proc_locks_snapshot_lease_file(&safe, expected_uid)
        .expect("accept private regular lease file");
    assert_eq!(
        safe_file.metadata().expect("stat safe lease file").mode() & 0o077,
        0
    );
    drop(safe_file);

    let lease_path = safe.join(PROC_LOCKS_LEASE_NAME);
    fs::set_permissions(&lease_path, fs::Permissions::from_mode(0o666))
        .expect("make existing lease file unsafe");
    assert_eq!(
        open_proc_locks_snapshot_lease_file(&safe, expected_uid)
            .expect_err("reject group/world-accessible lease file")
            .kind(),
        ErrorKind::PermissionDenied
    );

    fs::remove_file(&lease_path).expect("remove unsafe lease fixture");
    let fifo_path = CString::new(lease_path.as_os_str().as_bytes())
        .expect("lease fixture path must not contain NUL");
    // SAFETY: `fifo_path` is a valid NUL-terminated path and mode has no
    // additional preconditions.
    assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);
    assert_eq!(
        open_proc_locks_snapshot_lease_file(&safe, expected_uid)
            .expect_err("reject openable nonregular lease file")
            .kind(),
        ErrorKind::InvalidInput
    );
    fs::remove_file(&lease_path).expect("remove nonregular lease fixture");

    let symlink_target = safe.join("symlink-target");
    fs::write(&symlink_target, b"").expect("create lease symlink target");
    unix_fs::symlink(&symlink_target, &lease_path).expect("create lease symlink fixture");
    assert!(
        open_proc_locks_snapshot_lease_file(&safe, expected_uid).is_err(),
        "symlink lease path must be refused"
    );
}

fn build_guest(repository: &Path, build_root: &Path, name: &str, api: &str) -> std::path::PathBuf {
    fs::create_dir_all(build_root).expect("failed to create proc-locks build directory");
    let guest = build_root.join(name);
    let compile = Command::new("cc")
        .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(format!("-DLOCK_API={api}"))
        .arg(repository.join("tests/c/proc_locks.c"))
        .arg("-o")
        .arg(&guest)
        .output()
        .unwrap_or_else(|error| panic!("failed to compile {name}: {error}"));
    assert!(
        compile.status.success(),
        "failed to compile {name}:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );
    guest
}

fn run_guest(guest: &Path, verify: bool) -> Output {
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after", "5s", "90s"])
        .arg(hermit_test::hermit_binary());
    if verify {
        command.args(["--log", "DEBUG"]);
    }
    command.args(["run", "--backend=ptrace", "--strict"]);
    if verify {
        command.arg("--verify");
    }
    command
        .args([
            "--no-virtualize-cpuid",
            "--max-timeslice=disabled",
            "--panic-on-unsupported-syscalls",
            "--base-env=minimal",
        ])
        .arg("--")
        .arg(guest);
    hermit_test::configure_guest_execution(&mut command);
    command.output().expect("failed to run proc-locks guest")
}

#[test]
fn proc_locks_consumers_are_deterministic_under_strict_verify() {
    // Linux intentionally exposes OFD locks from other PID namespaces because
    // those locks have no owning PID. Keep this whole-snapshot test exclusive
    // across test processes. The host-side BSD flock is PID-owned, so it is
    // hidden from the guest snapshot and released if the test process dies.
    let _lease = acquire_proc_locks_snapshot_lease();
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("proc-locks");

    for (name, api) in [("fcntl", "1"), ("lockf", "2"), ("ofd-fcntl", "3")] {
        let guest = build_guest(repository, &build_root, name, api);
        let strict = run_guest(&guest, false);
        let strict_out = String::from_utf8_lossy(&strict.stdout);
        let strict_err = String::from_utf8_lossy(&strict.stderr);
        assert!(
            strict.status.success(),
            "{name} failed strict run\nstdout:\n{strict_out}\nstderr:\n{strict_err}"
        );
        assert!(
            strict_out.contains("proc-locks-virtual-graph-and-aliases-ok"),
            "{name}: content/alias probe omitted its marker\nstdout:\n{strict_out}\nstderr:\n{strict_err}"
        );

        let output = run_guest(&guest, true);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "{name} failed strict verification\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        assert!(
            stdout.contains("Determinism verified") || stderr.contains("Determinism verified"),
            "{name} omitted verification marker\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }
}

const CONCURRENT_WORKER: &str = "HERMIT_PROC_LOCKS_CONCURRENT_WORKER";
const CONCURRENT_MODE: &str = "HERMIT_PROC_LOCKS_CONCURRENT_MODE";
const CONCURRENT_ROOT: &str = "HERMIT_PROC_LOCKS_CONCURRENT_ROOT";
const CONCURRENT_GUEST: &str = "HERMIT_PROC_LOCKS_CONCURRENT_GUEST";
const VERIFY_MODE: &str = "verify";
const LOUD_HOLDER_MODE: &str = "loud-holder";
const WAITER_MODE: &str = "waiter";

struct ProcLocksWorker {
    index: usize,
    child: Child,
    stdout: PathBuf,
    stderr: PathBuf,
}

fn spawn_proc_locks_worker(root: &Path, guest: &Path, index: usize, mode: &str) -> ProcLocksWorker {
    let stdout = root.join(format!("worker-{index}-{mode}.stdout"));
    let stderr = root.join(format!("worker-{index}-{mode}.stderr"));
    let child =
        Command::new(env::current_exe().expect("locate proc-locks integration-test binary"))
            .env(CONCURRENT_WORKER, index.to_string())
            .env(CONCURRENT_MODE, mode)
            .env(CONCURRENT_ROOT, root)
            .env(CONCURRENT_GUEST, guest)
            .args([
                "--exact",
                "concurrent_proc_locks_consumers_are_serialized_across_processes",
                "--nocapture",
            ])
            .stdout(Stdio::from(
                File::create(&stdout).expect("create proc-locks worker stdout"),
            ))
            .stderr(Stdio::from(
                File::create(&stderr).expect("create proc-locks worker stderr"),
            ))
            .spawn()
            .unwrap_or_else(|error| panic!("start concurrent proc-locks worker {index}: {error}"));
    ProcLocksWorker {
        index,
        child,
        stdout,
        stderr,
    }
}

fn wait_for_workers(root: &Path, workers: usize) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !(0..workers).all(|worker| root.join(format!("ready-{worker}")).exists()) {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for concurrent proc-locks workers to become ready"
        );
        thread::sleep(Duration::from_millis(10));
    }
    fs::write(root.join("go"), b"go\n").expect("release concurrent proc-locks workers");
}

fn finish_proc_locks_worker(worker: ProcLocksWorker) -> (bool, Vec<u8>, Vec<u8>) {
    let status = worker
        .child
        .wait_with_output()
        .unwrap_or_else(|error| panic!("wait for proc-locks worker {}: {error}", worker.index))
        .status;
    let stdout = fs::read(&worker.stdout)
        .unwrap_or_else(|error| panic!("read proc-locks worker {} stdout: {error}", worker.index));
    let stderr = fs::read(&worker.stderr)
        .unwrap_or_else(|error| panic!("read proc-locks worker {} stderr: {error}", worker.index));
    (status.success(), stdout, stderr)
}

fn run_proc_locks_worker() -> bool {
    let Some(worker) = env::var_os(CONCURRENT_WORKER) else {
        return false;
    };
    let mode = env::var(CONCURRENT_MODE).expect("concurrent proc-locks worker lacks mode");
    let root = PathBuf::from(
        env::var_os(CONCURRENT_ROOT).expect("concurrent proc-locks worker lacks root"),
    );
    fs::write(
        root.join(format!("ready-{}", worker.to_string_lossy())),
        b"ready\n",
    )
    .expect("publish concurrent proc-locks worker readiness");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !root.join("go").exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting to release concurrent proc-locks workers"
        );
        thread::sleep(Duration::from_millis(10));
    }

    match mode.as_str() {
        VERIFY_MODE => {
            let guest = PathBuf::from(
                env::var_os(CONCURRENT_GUEST).expect("concurrent proc-locks worker lacks guest"),
            );
            let _lease = acquire_proc_locks_snapshot_lease();
            let output = run_guest(&guest, true);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success(),
                "concurrent proc-locks worker {} failed strict verification\n\
                 stdout:\n{stdout}\nstderr:\n{stderr}",
                worker.to_string_lossy()
            );
            assert!(
                stdout.contains("Determinism verified") || stderr.contains("Determinism verified"),
                "concurrent proc-locks worker {} omitted verification marker\n\
                 stdout:\n{stdout}\nstderr:\n{stderr}",
                worker.to_string_lossy()
            );
        }
        LOUD_HOLDER_MODE => {
            let _lease = acquire_proc_locks_snapshot_lease();
            fs::write(root.join("holder-ready"), b"ready\n")
                .expect("publish loud holder readiness");
            let mut stderr = std::io::stderr().lock();
            stderr
                .write_all(&vec![b'x'; 128 * 1024])
                .expect("write loud worker output");
            stderr.flush().expect("flush loud worker output");
            panic!("intentional loud-holder failure");
        }
        WAITER_MODE => {
            while !root.join("holder-ready").exists() {
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for loud proc-locks holder"
                );
                thread::sleep(Duration::from_millis(10));
            }
            let _lease = acquire_proc_locks_snapshot_lease();
        }
        _ => panic!("unknown concurrent proc-locks worker mode {mode:?}"),
    }
    true
}

#[test]
fn concurrent_proc_locks_consumers_are_serialized_across_processes() {
    const WORKERS: usize = 3;
    if run_proc_locks_worker() {
        return;
    }

    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let root = tempfile::Builder::new()
        .prefix("proc-locks-cross-process-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("create concurrent proc-locks control directory");
    let guest = build_guest(repository, root.path(), "ofd-fcntl", "3");
    let children = (0..WORKERS)
        .map(|worker| spawn_proc_locks_worker(root.path(), &guest, worker, VERIFY_MODE))
        .collect::<Vec<_>>();
    wait_for_workers(root.path(), WORKERS);

    for worker in children {
        let index = worker.index;
        let (success, stdout, stderr) = finish_proc_locks_worker(worker);
        assert!(
            success,
            "proc-locks worker {index} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        );
    }
}

#[test]
fn loud_later_worker_cannot_deadlock_output_collection() {
    let root = tempfile::Builder::new()
        .prefix("proc-locks-loud-worker-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("create loud-worker control directory");
    let placeholder_guest = root.path().join("unused-guest");
    let waiter = spawn_proc_locks_worker(root.path(), &placeholder_guest, 0, WAITER_MODE);
    let loud_holder = spawn_proc_locks_worker(root.path(), &placeholder_guest, 1, LOUD_HOLDER_MODE);
    wait_for_workers(root.path(), 2);

    // Wait for worker 0 first on purpose. If worker 1 wrote to an undrained
    // pipe while holding the lease, this ordering would deadlock worker 0.
    let (waiter_success, waiter_stdout, waiter_stderr) = finish_proc_locks_worker(waiter);
    assert!(
        waiter_success,
        "waiter failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&waiter_stdout),
        String::from_utf8_lossy(&waiter_stderr)
    );
    let (holder_success, _, holder_stderr) = finish_proc_locks_worker(loud_holder);
    assert!(!holder_success, "loud holder unexpectedly succeeded");
    assert!(
        holder_stderr.len() > 64 * 1024,
        "loud holder did not exceed a typical pipe capacity"
    );
}
