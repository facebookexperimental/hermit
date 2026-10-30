/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[path = "common/hermit_binary.rs"]
mod hermit_test;
#[path = "common/proc_locks_lease.rs"]
mod proc_locks_lease;

use std::env;
use std::ffi::CString;
use std::ffi::OsStr;
use std::fs;
use std::fs::File;
use std::io::ErrorKind;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs as unix_fs;
use std::os::unix::fs::MetadataExt;
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

use proc_locks_lease::*;

struct ProcLocksSnapshotLease {
    _file: File,
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
    validate_proc_locks_host_identity(&file, env::var_os("HERMIT_PROC_LOCKS_LEASE_ID").as_deref())
        .expect("bind proc-locks snapshot lease to the host inode");
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
    let metadata = safe_file.metadata().expect("stat host identity fixture");
    let identity = format!("{}:{}", metadata.dev(), metadata.ino());
    validate_proc_locks_host_identity(&safe_file, None)
        .expect("native lease needs no mount identity");
    validate_proc_locks_host_identity(&safe_file, Some(OsStr::new(&identity)))
        .expect("accept exact host inode");
    let other_file = File::create(safe.join("different-inode")).expect("create distinct inode");
    assert_eq!(
        validate_proc_locks_host_identity(&other_file, Some(OsStr::new(&identity)))
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidData
    );
    for malformed in [
        "",
        "1",
        "1:2:3",
        "+1:2",
        "1:",
        "x:2",
        "18446744073709551616:2",
    ] {
        assert_eq!(
            validate_proc_locks_host_identity(&safe_file, Some(OsStr::new(malformed)))
                .unwrap_err()
                .kind(),
            ErrorKind::InvalidInput
        );
    }
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

    // The manifest consumer uses this same inode with an absolute deadline;
    // neither contention nor retry may start a fresh wait budget.
    fs::remove_file(&lease_path).unwrap();
    let open = || open_proc_locks_snapshot_lease_file(&safe, expected_uid).unwrap();
    let first = lock_until(open(), Instant::now() + Duration::from_secs(1)).unwrap();
    let deadline = Instant::now() + Duration::from_millis(20);
    assert_eq!(
        lock_until(open(), deadline).unwrap_err().kind(),
        ErrorKind::TimedOut
    );
    assert!(Instant::now() >= deadline);
    drop(first);
    assert_eq!(
        lock_until(open(), deadline).unwrap_err().kind(),
        ErrorKind::TimedOut
    );
    let released = lock_until(open(), Instant::now() + Duration::from_secs(1)).unwrap();
    drop(released);
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

/// Run `guest` under `hermit --backend=<backend> run --strict`, and with
/// `verify_json`, as the L2 check: `--verify --verify-strict` writing its
/// report there, at log level info.
fn run_guest_on(
    backend: &str,
    guest: &Path,
    args: &[&OsStr],
    verify_json: Option<&Path>,
) -> Output {
    run_guest_on_with(backend, &["--strict"], guest, args, verify_json)
}

/// [`run_guest_on`] with `run_flags` in place of `--strict`.
fn run_guest_on_with(
    backend: &str,
    run_flags: &[&str],
    guest: &Path,
    args: &[&OsStr],
    verify_json: Option<&Path>,
) -> Output {
    let mut command = guest_command(backend, run_flags, guest, args, verify_json);
    hermit_test::hermit_output(&format!("{command:?}"), &mut command)
}

/// The command [`run_guest_on_with`] runs.
fn guest_command(
    backend: &str,
    run_flags: &[&str],
    guest: &Path,
    args: &[&OsStr],
    verify_json: Option<&Path>,
) -> Command {
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after", "5s", "90s"])
        .arg(hermit_test::hermit_binary());
    if verify_json.is_some() {
        command.arg("--log=info");
    }
    command
        .arg(format!("--backend={backend}"))
        .arg("run")
        .args(run_flags)
        .arg("--proc=standard");
    if let Some(report) = verify_json {
        command
            .args(["--verify", "--verify-strict", "--verify-json"])
            .arg(report);
    }
    command
        .args([
            "--no-virtualize-cpuid",
            "--max-timeslice=disabled",
            "--panic-on-unsupported-syscalls",
            "--base-env=minimal",
        ])
        .arg("--")
        .arg(guest)
        .args(args);
    hermit_test::configure_guest_execution(&mut command);
    command
}

fn run_guest(guest: &Path, verify_json: Option<&Path>) -> Output {
    run_guest_on("ptrace", guest, &[], verify_json)
}

/// Require the `--verify-strict` report at `report` to show bitwise parity
/// over a nonzero, equal number of compared INFO messages: the L2 check, which
/// the default `--verify` comparator (`Stripped`) cannot establish.
fn assert_bitwise_parity(label: &str, report: &Path, output: &Output) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "{label} failed strict verification\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let report: serde_json::Value = serde_json::from_slice(
        &fs::read(report)
            .unwrap_or_else(|error| panic!("{label}: no verification report: {error}")),
    )
    .unwrap_or_else(|error| panic!("{label}: verification report is not JSON: {error}"));
    assert_eq!(
        report["bitwise_parity"],
        serde_json::Value::Bool(true),
        "{label}: {report}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let compared = &report["compared_log_messages"];
    assert!(
        compared["left"].as_u64().is_some_and(|count| count > 0)
            && compared["left"] == compared["right"],
        "{label}: verification compared no INFO messages: {report}"
    );
}

#[test]
fn proc_locks_consumers_are_deterministic_under_strict_verify() {
    // Linux exposes OFD locks from other PID namespaces because those locks
    // have no owning PID. Detcore now drops every row the guest does not own
    // (`host_locks_stay_out_of_the_guests_proc_locks_*` below is the control),
    // so this lease no longer decides the result; it stays until its
    // validation wiring is retired. The host-side BSD flock is PID-owned, so
    // it is hidden from the guest snapshot and released if the test process
    // dies.
    let _lease = acquire_proc_locks_snapshot_lease();
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("proc-locks-{}", std::process::id()));

    for (name, api) in [("fcntl", "1"), ("lockf", "2"), ("ofd-fcntl", "3")] {
        let guest = build_guest(repository, &build_root, name, api);
        let strict = run_guest(&guest, None);
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

        let report = build_root.join(format!("{name}-verify.json"));
        let output = run_guest(&guest, Some(&report));
        assert_bitwise_parity(name, &report, &output);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("proc-locks-virtual-graph-and-aliases-ok"),
            "{name}: the verified run omitted its marker\nstdout:\n{stdout}"
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
            let report = root.join(format!("worker-{}-verify.json", worker.to_string_lossy()));
            let output = run_guest(&guest, Some(&report));
            assert_bitwise_parity(
                &format!("concurrent proc-locks worker {}", worker.to_string_lossy()),
                &report,
                &output,
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
    let mut loud_holder =
        spawn_proc_locks_worker(root.path(), &placeholder_guest, 1, LOUD_HOLDER_MODE);
    wait_for_workers(root.path(), 2);

    // Wait for worker 0 first on purpose. If worker 1 wrote to an undrained
    // pipe while holding the lease, this ordering would deadlock worker 0.
    let (waiter_success, waiter_stdout, waiter_stderr) = finish_proc_locks_worker(waiter);
    // Observe the holder without adding a blocking wait to the failed waiter
    // path. These are current regular-file diagnostics, not an inferred exit.
    let holder_status = loud_holder
        .child
        .try_wait()
        .expect("observe loud-holder status");
    let holder_stdout_now = fs::read(&loud_holder.stdout).expect("read current holder stdout");
    let holder_stderr_now = fs::read(&loud_holder.stderr).expect("read current holder stderr");
    assert!(
        waiter_success,
        "waiter failed\nstdout:\n{}\nstderr:\n{}\nholder status now: {:?}\nholder stdout now:\n{}\nholder stderr now:\n{}",
        String::from_utf8_lossy(&waiter_stdout),
        String::from_utf8_lossy(&waiter_stderr),
        holder_status,
        String::from_utf8_lossy(&holder_stdout_now),
        String::from_utf8_lossy(&holder_stderr_now)
    );
    let (holder_success, _, holder_stderr) = finish_proc_locks_worker(loud_holder);
    assert!(!holder_success, "loud holder unexpectedly succeeded");
    assert!(
        holder_stderr.len() > 64 * 1024,
        "loud holder did not exceed a typical pipe capacity"
    );
}

/// A guest that takes one POSIX lock and one OFD lock on files of its own,
/// opens (and never locks) a file the host locks, and writes `/proc/locks`
/// verbatim to stdout, several times.
const HOST_LOCKS_GUEST_SOURCE: &str = r#"
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static int open_own(const char* directory, const char* name) {
  char path[4096];
  if (snprintf(path, sizeof(path), "%s/%s", directory, name) >= (int)sizeof(path)) {
    return -1;
  }
  return open(path, O_CREAT | O_RDWR | O_TRUNC, 0600);
}

static int print_snapshot(int index) {
  static char buffer[256 * 1024];
  int fd = open("/proc/locks", O_RDONLY);
  if (fd < 0) {
    return 1;
  }
  size_t used = 0;
  for (;;) {
    ssize_t count = read(fd, buffer + used, sizeof(buffer) - used);
    if (count < 0 && errno == EINTR) {
      continue;
    }
    if (count < 0 || (count > 0 && used + (size_t)count == sizeof(buffer))) {
      close(fd);
      return 1;
    }
    if (count == 0) {
      break;
    }
    used += (size_t)count;
  }
  close(fd);
  printf("--- snapshot %d ---\n", index);
  return fwrite(buffer, 1, used, stdout) == used ? 0 : 1;
}

int main(int argc, char** argv) {
  if (argc != 4) {
    return 2;
  }
  int shared = open(argv[2], O_RDONLY);
  int posix = open_own(argv[1], "own-posix");
  int ofd = open_own(argv[1], "own-ofd");
  if (shared < 0 || posix < 0 || ofd < 0) {
    return 1;
  }
  struct flock posix_lock = {.l_type = F_WRLCK, .l_whence = SEEK_SET};
  struct flock ofd_lock = {.l_type = F_WRLCK, .l_whence = SEEK_SET};
  if (fcntl(posix, F_SETLK, &posix_lock) != 0 || fcntl(ofd, F_OFD_SETLK, &ofd_lock) != 0) {
    return 1;
  }
  int snapshots = atoi(argv[3]);
  for (int index = 0; index < snapshots; index++) {
    if (print_snapshot(index) != 0) {
      return 1;
    }
  }
  return fflush(stdout) == 0 ? 0 : 1;
}
"#;

/// Snapshots each guest run takes, to give the toggling host locks many
/// chances to appear.
const HOST_LOCKS_SNAPSHOTS: &str = "8";

/// The host's locks for the control: an OFD write lock and a POSIX write lock
/// (on separate ranges) and a flock on a file the guest opens but never
/// locks, and an OFD write lock on a file the guest never sees. The test
/// process takes them outside the guest's PID namespace.
struct HostLocks {
    shared: File,
    shared_flock: File,
    unseen: File,
}

/// `F_OFD_SETLK` or `F_SETLK` of `kind` (`F_WRLCK` or `F_UNLCK`) on
/// `[start, start + len)`.
fn record_lock(file: &File, command: libc::c_int, kind: libc::c_int, start: i64, len: i64) {
    // SAFETY: an all-zero flock is a valid value; the fields set below are
    // the ones fcntl reads.
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = kind as libc::c_short;
    lock.l_whence = libc::SEEK_SET as libc::c_short;
    lock.l_start = start;
    lock.l_len = len;
    // SAFETY: `file` owns a valid descriptor and `lock` outlives the call.
    let result = unsafe { libc::fcntl(file.as_raw_fd(), command, &lock as *const libc::flock) };
    assert_eq!(
        result,
        0,
        "host lock command {command} type {kind}: {}",
        std::io::Error::last_os_error()
    );
}

impl HostLocks {
    fn open(shared: &Path, unseen: &Path) -> Self {
        let open = |path: &Path| {
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .unwrap_or_else(|error| panic!("open host lock file {}: {error}", path.display()))
        };
        Self {
            shared: open(shared),
            shared_flock: open(shared),
            unseen: open(unseen),
        }
    }

    fn set(&self, held: bool) {
        let kind = if held { libc::F_WRLCK } else { libc::F_UNLCK };
        record_lock(&self.shared, libc::F_OFD_SETLK, kind, 0, 1);
        record_lock(&self.shared, libc::F_SETLK, kind, 1, 1);
        record_lock(&self.unseen, libc::F_OFD_SETLK, kind, 0, 0);
        let operation = if held {
            libc::LOCK_EX | libc::LOCK_NB
        } else {
            libc::LOCK_UN
        };
        // SAFETY: `shared_flock` owns a valid descriptor.
        let result = unsafe { libc::flock(self.shared_flock.as_raw_fd(), operation) };
        assert_eq!(result, 0, "host flock: {}", std::io::Error::last_os_error());
    }
}

/// The inode of `path`, as `/proc/locks` prints it at the end of the object.
fn inode_suffix(path: &Path) -> String {
    format!(
        ":{} ",
        fs::metadata(path).expect("stat host lock file").ino()
    )
}

/// Whether the host's own `/proc/locks` shows an OFD lock on `path`. The
/// control is real only if it does: a host lock nothing could see would
/// prove nothing about the guest's view.
///
/// The host's file is longer than a page here, and the kernel returns at most
/// about a page per read, restarting its walk at an index each time; a lock
/// another host process drops ahead of that index between two reads makes the
/// next read skip a row, which can be this one. The lock is held throughout,
/// so a read that misses it raced; read again, a bounded number of times.
fn host_sees_ofd_lock(path: &Path) -> bool {
    let suffix = inode_suffix(path);
    (0..20).any(|_| {
        fs::read_to_string("/proc/locks")
            .expect("read the host's /proc/locks")
            .lines()
            .any(|line| line.contains("OFDLCK") && line.contains(" -1 ") && line.contains(&suffix))
    })
}

/// The guest's snapshots: each must hold exactly its two rows, its own OFD
/// lock (owner -1) and its own POSIX lock, and nothing of the host's.
fn assert_only_guest_rows(label: &str, stdout: &str) {
    let snapshots = stdout.split("--- snapshot ").skip(1).collect::<Vec<_>>();
    assert_eq!(
        snapshots.len().to_string(),
        HOST_LOCKS_SNAPSHOTS,
        "{label}: the guest printed {} snapshots\n{stdout}",
        snapshots.len()
    );
    for snapshot in snapshots {
        let rows = snapshot.lines().skip(1).collect::<Vec<_>>();
        assert_eq!(
            rows.len(),
            2,
            "{label}: expected only the guest's two rows\n{stdout}"
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.contains(" OFDLCK ") && row.contains(" WRITE -1 "))
                .count(),
            1,
            "{label}: the guest's own OFD lock is not shown exactly once\n{stdout}"
        );
        assert_eq!(
            rows.iter().filter(|row| row.contains(" POSIX ")).count(),
            1,
            "{label}: the guest's own POSIX lock is not shown exactly once\n{stdout}"
        );
    }
}

/// A directory for the file the guest never sees, removed when dropped: a
/// fresh one under `/dev/shm`, another filesystem than the guest's files,
/// where the host lets the test create one, else one beside the guest's files
/// (which the guest never opens either). Says which on stderr, so a run that
/// fell back does not pass as if it had covered another filesystem.
fn unseen_directory(fallback: &Path) -> tempfile::TempDir {
    let (directory, class) = match tempfile::Builder::new()
        .prefix("hermit-proc-locks-unseen-")
        .tempdir_in("/dev/shm")
    {
        Ok(directory) => (directory, "/dev/shm"),
        Err(error) => {
            eprintln!("cannot create a directory under /dev/shm ({error}); falling back");
            (
                tempfile::Builder::new()
                    .prefix("unseen-")
                    .tempdir_in(fallback)
                    .expect("create a directory for the unseen lock file"),
                "test tempdir",
            )
        }
    };
    eprintln!("unseen host lock file location: {class}");
    directory
}

/// Pin this thread, and so the Hermit runs it starts and the threads it
/// spawns, to the highest-numbered CPU it may run on.
///
/// The kernel walks `/proc/locks` one CPU's list after another, in CPU order,
/// each newest lock first, and each `read` restarts that walk at the index
/// where the last one stopped. A guest pinned to the last CPU therefore holds
/// the last row of the walk (its first lock, unless an older lock is still on
/// that CPU's list), and any lock the host adds between Detcore's capture reads
/// makes the closing read print that row again. Unpinned, the guest's rows
/// land there only when Hermit happens to pick that CPU, and the race showed
/// up once in a loaded validation run; pinned, 24 of 30 runs caught it before
/// the capture dropped those bytes.
fn pin_to_the_last_allowed_cpu() {
    // A CPU can leave the allowed set between the two calls; read it again.
    for _ in 0..3 {
        let mut allowed = allowed_cpu_mask();
        let word_bits = u64::BITS as usize;
        let last = (0..allowed.len() * word_bits)
            .rev()
            .find(|cpu| allowed[cpu / word_bits] & (1 << (cpu % word_bits)) != 0)
            .expect("this thread may run on no CPU");
        allowed.fill(0);
        allowed[last / word_bits] = 1 << (last % word_bits);
        // SAFETY: the kernel reads `allowed.len() * 8` bytes from `allowed`,
        // which owns them; glibc passes a mask of any size through.
        let result = unsafe {
            libc::sched_setaffinity(
                0,
                allowed.len() * std::mem::size_of::<u64>(),
                allowed.as_ptr().cast::<libc::cpu_set_t>(),
            )
        };
        if result == 0 {
            return;
        }
        let error = std::io::Error::last_os_error();
        assert_eq!(
            error.raw_os_error(),
            Some(libc::EINVAL),
            "sched_setaffinity to CPU {last}: {error}"
        );
    }
    panic!("the last allowed CPU kept leaving the allowed set");
}

/// This thread's allowed CPUs, as a bit mask in 64-bit words, sized for the
/// kernel's CPU count however large: the kernel refuses (`EINVAL`) a mask
/// smaller than its possible CPUs, so the mask doubles until it fits.
fn allowed_cpu_mask() -> Vec<u64> {
    let mut words = 16;
    loop {
        let mut mask = vec![0_u64; words];
        // SAFETY: the kernel writes at most `words * 8` bytes into `mask`,
        // which owns them; glibc passes a mask of any size through.
        let result = unsafe {
            libc::sched_getaffinity(
                0,
                words * std::mem::size_of::<u64>(),
                mask.as_mut_ptr().cast::<libc::cpu_set_t>(),
            )
        };
        if result == 0 {
            return mask;
        }
        let error = std::io::Error::last_os_error();
        assert!(
            error.raw_os_error() == Some(libc::EINVAL) && words < 1 << 16,
            "sched_getaffinity with {words} words: {error}"
        );
        words *= 2;
    }
}

/// Host locks taken outside the guest change nothing it sees in
/// `/proc/locks`, on `backend`:
/// 1. `run --strict` prints the same bytes with the host's locks held as
///    without them;
/// 2. `run --strict --verify --verify-strict` reaches bitwise parity while a
///    host thread takes and drops the locks about every millisecond;
/// 3. the guest's own OFD and POSIX rows are still there.
fn host_locks_stay_out_of_the_guests_proc_locks(backend: &str) {
    pin_to_the_last_allowed_cpu();
    let root = tempfile::Builder::new()
        .prefix("proc-locks-host-control-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("create host-lock control directory");
    let source = root.path().join("host-locks-guest.c");
    fs::write(&source, HOST_LOCKS_GUEST_SOURCE).expect("write host-lock guest source");
    let guest = root.path().join("host-locks-guest");
    let compile = Command::new("cc")
        .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(&source)
        .arg("-o")
        .arg(&guest)
        .output()
        .expect("start cc for the host-lock guest");
    assert!(
        compile.status.success(),
        "failed to compile the host-lock guest:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );
    let own = root.path().join("own");
    fs::create_dir(&own).expect("create the guest's own lock directory");
    let shared = root.path().join("shared");
    fs::write(&shared, b"shared\n").expect("create the shared lock file");
    let unseen_directory = unseen_directory(root.path());
    let unseen = unseen_directory.path().join("unseen");
    fs::write(&unseen, b"unseen\n").expect("create the unseen lock file");
    let args = [
        own.as_os_str(),
        shared.as_os_str(),
        OsStr::new(HOST_LOCKS_SNAPSHOTS),
    ];
    let run = |label: &str, verify_json: Option<&Path>| {
        let output = run_guest_on(backend, &guest, &args, verify_json);
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        if verify_json.is_none() {
            assert!(
                output.status.success(),
                "{backend} {label}: run failed\nstdout:\n{stdout}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        (output, stdout)
    };

    let (_, unlocked) = run("without host locks", None);
    assert_only_guest_rows(&format!("{backend} without host locks"), &unlocked);

    let host = HostLocks::open(&shared, &unseen);
    host.set(true);
    assert!(
        host_sees_ofd_lock(&shared) && host_sees_ofd_lock(&unseen),
        "the host's /proc/locks does not show the control's OFD locks"
    );
    let (_, locked) = run("with host locks held", None);
    host.set(false);
    assert_eq!(
        locked, unlocked,
        "{backend}: host locks held outside the guest changed its /proc/locks"
    );

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let toggles = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let toggler = {
        let (stop, toggles) = (stop.clone(), toggles.clone());
        thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                host.set(true);
                thread::sleep(Duration::from_micros(500));
                host.set(false);
                thread::sleep(Duration::from_micros(500));
                toggles.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        })
    };
    let report = root.path().join("verify.json");
    let (verified, verified_stdout) = run("verify while host locks toggle", Some(&report));
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    toggler.join().expect("host lock toggler panicked");
    assert!(
        toggles.load(std::sync::atomic::Ordering::Relaxed) > 0,
        "the host never toggled its locks during the verified run"
    );
    assert_bitwise_parity(
        &format!("{backend} with toggling host locks"),
        &report,
        &verified,
    );
    assert_eq!(
        verified_stdout, unlocked,
        "{backend}: toggling host locks changed the verified run's /proc/locks"
    );
}

#[test]
fn host_locks_stay_out_of_the_guests_proc_locks_on_ptrace() {
    host_locks_stay_out_of_the_guests_proc_locks("ptrace");
}

/// Whether this test binary cannot run DBT: built without the `dbt` feature.
/// With `HERMIT_REQUIRE_DBT` set that is a failure rather than a skip.
fn dbt_unavailable() -> bool {
    if cfg!(feature = "dbt") {
        return false;
    }
    assert!(
        env::var_os("HERMIT_REQUIRE_DBT").is_none(),
        "HERMIT_REQUIRE_DBT is set, but this test binary was built without the dbt \
         feature, so it cannot exercise DBT"
    );
    eprintln!("skipping: built without the dbt feature");
    true
}

#[test]
fn host_locks_stay_out_of_the_guests_proc_locks_on_dbt() {
    if dbt_unavailable() {
        return;
    }
    host_locks_stay_out_of_the_guests_proc_locks("dbt");
}

/// A guest that runs one lock scenario, chosen by its first argument, and
/// prints `/proc/locks` verbatim under a `--- label ---` line at each point the
/// scenario names:
/// - `unlock FILE`: an OFD write lock on byte 0 of FILE, then its release;
/// - `fork DIR`: the parent takes an OFD lock and a flock, forks, closes both
///   descriptors while the child holds its copies and a POSIX lock of its own,
///   then the child exits;
/// - `transfer DIR`: the parent OFD-locks a file it opened after forking,
///   sends the descriptor over `SCM_RIGHTS` and closes it; the child holds it,
///   then closes it;
/// - `in-flight-sender-closes DIR` and `in-flight-sender-exits DIR`: the
///   child OFD-locks a file, sends the descriptor and closes it or exits
///   before the parent receives it; the parent looks while the message is in
///   flight, after it receives it, and after it closes it;
/// - `write-only DIR`: an OFD lock whose `struct flock` is on a page mapped
///   `PROT_WRITE` only, which the kernel reads and a tracer's
///   `process_vm_readv` cannot;
/// - `exhaust DIR`: an OFD lock after the guest has mapped its whole address
///   space away, which leaves Detcore no page to read the descriptor's fdinfo
///   through.
const LOCK_SCENARIOS_SOURCE: &str = r#"
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/futex.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

static void die(const char* what) {
  fprintf(stderr, "scenario: %s: %s\n", what, strerror(errno));
  exit(1);
}

static int open_in(const char* directory, const char* name) {
  char path[4096];
  if (snprintf(path, sizeof(path), "%s/%s", directory, name) >= (int)sizeof(path)) {
    die("path");
  }
  int fd = open(path, O_CREAT | O_RDWR | O_TRUNC, 0600);
  if (fd < 0) {
    die(name);
  }
  return fd;
}

static void snapshot(const char* label) {
  static char buffer[256 * 1024];
  int fd = open("/proc/locks", O_RDONLY);
  if (fd < 0) {
    die("open /proc/locks");
  }
  size_t used = 0;
  for (;;) {
    ssize_t count = read(fd, buffer + used, sizeof(buffer) - used);
    if (count < 0 && errno == EINTR) {
      continue;
    }
    if (count < 0) {
      die("read /proc/locks");
    }
    if (count == 0) {
      break;
    }
    used += (size_t)count;
    if (used == sizeof(buffer)) {
      die("snapshot too large");
    }
  }
  close(fd);
  printf("--- %s ---\n", label);
  if (fwrite(buffer, 1, used, stdout) != used || fflush(stdout) != 0) {
    die("write snapshot");
  }
}

static void ofd(int fd, short type, off_t start, off_t length) {
  struct flock lock = {.l_type = type, .l_whence = SEEK_SET, .l_start = start, .l_len = length};
  if (fcntl(fd, F_OFD_SETLK, &lock) != 0) {
    die("F_OFD_SETLK");
  }
}

static void wait_for(pid_t pid) {
  int status = 0;
  if (waitpid(pid, &status, 0) != pid || !WIFEXITED(status) || WEXITSTATUS(status) != 0) {
    die("child");
  }
}

static void byte_in(int fd) {
  char byte = 0;
  if (read(fd, &byte, 1) != 1) {
    die("read byte");
  }
}

static void byte_out(int fd) {
  if (write(fd, "x", 1) != 1) {
    die("write byte");
  }
}

static int unlock_scenario(const char* path) {
  int fd = open(path, O_RDWR);
  if (fd < 0) {
    die("open shared file");
  }
  ofd(fd, F_WRLCK, 0, 1);
  snapshot("held");
  ofd(fd, F_UNLCK, 0, 1);
  snapshot("released");
  return 0;
}

static int fork_scenario(const char* directory) {
  int ofd_file = open_in(directory, "fork-ofd");
  int flock_file = open_in(directory, "fork-flock");
  int posix_file = open_in(directory, "fork-posix");
  ofd(ofd_file, F_WRLCK, 0, 0);
  if (flock(flock_file, LOCK_EX | LOCK_NB) != 0) {
    die("flock");
  }
  int ready[2];
  int go[2];
  if (pipe(ready) != 0 || pipe(go) != 0) {
    die("pipe");
  }
  pid_t child = fork();
  if (child < 0) {
    die("fork");
  }
  if (child == 0) {
    struct flock lock = {.l_type = F_WRLCK, .l_whence = SEEK_SET};
    if (fcntl(posix_file, F_SETLK, &lock) != 0) {
      die("F_SETLK");
    }
    byte_out(ready[1]);
    byte_in(go[0]);
    _exit(0);
  }
  byte_in(ready[0]);
  close(ofd_file);
  close(flock_file);
  snapshot("parent-closed");
  byte_out(go[1]);
  wait_for(child);
  snapshot("child-exited");
  return 0;
}

static int open_path(const char* path, int flags) {
  int fd = open(path, flags | O_CREAT, 0600);
  if (fd < 0) {
    die(path);
  }
  return fd;
}

// The parent locks `path` (opened after forking, so only it holds it), sends
// the descriptor to the child and closes its own, either after the child's
// first snapshot or, with `sender_closes_first`, before it; the child then
// closes the one it received.
static int transfer_scenario(const char* path, short type, int sender_closes_first) {
  int pair[2];
  if (socketpair(AF_UNIX, SOCK_STREAM, 0, pair) != 0) {
    die("socketpair");
  }
  pid_t child = fork();
  if (child < 0) {
    die("fork");
  }
  if (child == 0) {
    close(pair[0]);
    char byte = 0;
    char control[CMSG_SPACE(sizeof(int))];
    struct iovec vector = {.iov_base = &byte, .iov_len = 1};
    struct msghdr message = {
        .msg_iov = &vector,
        .msg_iovlen = 1,
        .msg_control = control,
        .msg_controllen = sizeof(control),
    };
    if (recvmsg(pair[1], &message, 0) != 1) {
      die("recvmsg");
    }
    struct cmsghdr* header = CMSG_FIRSTHDR(&message);
    if (header == NULL || header->cmsg_type != SCM_RIGHTS) {
      die("no descriptor");
    }
    int received;
    memcpy(&received, CMSG_DATA(header), sizeof(received));
    if (sender_closes_first) {
      byte_in(pair[1]);
      snapshot("transferred");
    } else {
      snapshot("transferred");
      byte_out(pair[1]);
      byte_in(pair[1]);
    }
    close(received);
    snapshot("closed");
    _exit(0);
  }
  close(pair[1]);
  int file = open_path(path, O_RDWR);
  ofd(file, type, 0, 1);
  char byte = 'x';
  char control[CMSG_SPACE(sizeof(int))];
  memset(control, 0, sizeof(control));
  struct iovec vector = {.iov_base = &byte, .iov_len = 1};
  struct msghdr message = {
      .msg_iov = &vector,
      .msg_iovlen = 1,
      .msg_control = control,
      .msg_controllen = sizeof(control),
  };
  struct cmsghdr* header = CMSG_FIRSTHDR(&message);
  header->cmsg_level = SOL_SOCKET;
  header->cmsg_type = SCM_RIGHTS;
  header->cmsg_len = CMSG_LEN(sizeof(int));
  memcpy(CMSG_DATA(header), &file, sizeof(file));
  if (sendmsg(pair[0], &message, 0) != 1) {
    die("sendmsg");
  }
  if (!sender_closes_first) {
    byte_in(pair[0]);
  }
  close(file);
  byte_out(pair[0]);
  wait_for(child);
  return 0;
}

// The release paths, each with a shared read lock on byte 0 of `path`, which
// the host holds too, identically, for the whole run.
static int stale_unlock_scenario(const char* path) {
  int fd = open_path(path, O_RDWR);
  ofd(fd, F_RDLCK, 0, 1);
  snapshot("held");
  ofd(fd, F_UNLCK, 0, 1);
  snapshot("released");
  return 0;
}

static int stale_reap_scenario(const char* path) {
  fflush(stdout);
  pid_t child = fork();
  if (child < 0) {
    die("fork");
  }
  if (child == 0) {
    ofd(open_path(path, O_RDWR), F_RDLCK, 0, 1);
    _exit(0);
  }
  wait_for(child);
  snapshot("reaped");
  return 0;
}

static int stale_fork_scenario(const char* path) {
  int fd = open_path(path, O_RDWR);
  ofd(fd, F_RDLCK, 0, 1);
  int go[2];
  if (pipe(go) != 0) {
    die("pipe");
  }
  fflush(stdout);
  pid_t child = fork();
  if (child < 0) {
    die("fork");
  }
  if (child == 0) {
    byte_in(go[0]);
    snapshot("parent-closed");
    _exit(0);
  }
  close(fd);
  byte_out(go[1]);
  wait_for(child);
  snapshot("child-exited");
  return 0;
}

static int stale_exec_scenario(const char* path, const char* self) {
  int fd = open_path(path, O_RDWR | O_CLOEXEC);
  ofd(fd, F_RDLCK, 0, 1);
  fflush(stdout);
  execl(self, self, "after-exec", path, (char*)NULL);
  die("execl");
  return 1;
}

static int failed_exec_scenario(const char* path) {
  int fd = open_path(path, O_RDWR | O_CLOEXEC);
  ofd(fd, F_RDLCK, 0, 1);
  execl("/nonexistent/hermit-proc-locks-helper", "helper", (char*)NULL);
  snapshot("exec-failed");
  return 0;
}

static void send_descriptor(int socket, int fd) {
  char byte = 'x';
  char control[CMSG_SPACE(sizeof(int))];
  memset(control, 0, sizeof(control));
  struct iovec vector = {.iov_base = &byte, .iov_len = 1};
  struct msghdr message = {
      .msg_iov = &vector,
      .msg_iovlen = 1,
      .msg_control = control,
      .msg_controllen = sizeof(control),
  };
  struct cmsghdr* header = CMSG_FIRSTHDR(&message);
  header->cmsg_level = SOL_SOCKET;
  header->cmsg_type = SCM_RIGHTS;
  header->cmsg_len = CMSG_LEN(sizeof(int));
  memcpy(CMSG_DATA(header), &fd, sizeof(fd));
  if (sendmsg(socket, &message, 0) != 1) {
    die("sendmsg");
  }
}

static int receive_descriptor(int socket) {
  char byte = 0;
  char control[CMSG_SPACE(sizeof(int))];
  struct iovec vector = {.iov_base = &byte, .iov_len = 1};
  struct msghdr message = {
      .msg_iov = &vector,
      .msg_iovlen = 1,
      .msg_control = control,
      .msg_controllen = sizeof(control),
  };
  if (recvmsg(socket, &message, 0) != 1) {
    die("recvmsg");
  }
  struct cmsghdr* header = CMSG_FIRSTHDR(&message);
  if (header == NULL || header->cmsg_type != SCM_RIGHTS) {
    die("no descriptor");
  }
  int received;
  memcpy(&received, CMSG_DATA(header), sizeof(received));
  return received;
}

// The sender (the child) locks `path`, opened after forking so that only it
// holds it, sends the descriptor, and lets go of its own before the
// receiver takes the message: it closes it, or with `sender_exits` exits
// holding it. The open file and its OFD lock ride the message meanwhile, as
// Linux's in-flight reference holds them; the receiver then takes the
// descriptor and closes it.
static int in_flight_scenario(const char* path, int sender_exits) {
  int pair[2];
  int closed[2];
  if (socketpair(AF_UNIX, SOCK_STREAM, 0, pair) != 0 || pipe(closed) != 0) {
    die("socketpair");
  }
  pid_t child = fork();
  if (child < 0) {
    die("fork");
  }
  if (child == 0) {
    close(pair[0]);
    close(closed[0]);
    int file = open_path(path, O_RDWR);
    ofd(file, F_WRLCK, 0, 1);
    send_descriptor(pair[1], file);
    if (sender_exits) {
      _exit(0);
    }
    close(file);
    byte_out(closed[1]);
    byte_in(pair[1]);
    _exit(0);
  }
  close(pair[1]);
  close(closed[1]);
  if (sender_exits) {
    wait_for(child);
  } else {
    byte_in(closed[0]);
  }
  snapshot("in-flight");
  int received = receive_descriptor(pair[0]);
  snapshot("received");
  close(received);
  snapshot("closed");
  if (!sender_exits) {
    byte_out(pair[0]);
    wait_for(child);
  }
  return 0;
}

// The parent locks and sends its descriptor to the child, which unlocks
// through the one it received while the parent keeps its own open: the
// shared description, and so the kernel, holds no lock.
static int stale_receiver_unlock_scenario(const char* path) {
  int pair[2];
  if (socketpair(AF_UNIX, SOCK_STREAM, 0, pair) != 0) {
    die("socketpair");
  }
  fflush(stdout);
  pid_t child = fork();
  if (child < 0) {
    die("fork");
  }
  if (child == 0) {
    close(pair[0]);
    int received = receive_descriptor(pair[1]);
    ofd(received, F_UNLCK, 0, 1);
    byte_out(pair[1]);
    byte_in(pair[1]);
    _exit(0);
  }
  close(pair[1]);
  int file = open_path(path, O_RDWR);
  ofd(file, F_RDLCK, 0, 1);
  send_descriptor(pair[0], file);
  byte_in(pair[0]);
  snapshot("receiver-unlocked");
  byte_out(pair[0]);
  wait_for(child);
  return 0;
}

// The parent's locked descriptor is the only reference to its description;
// `dup2` over it with a descriptor received from the child closes that
// description, and with it the lock. The call's result is not checked:
// Detcore answers EBADF on ptrace, where it keeps no model of the received
// source, though the kernel replaced the slot.
static int stale_dup_over_scenario(const char* path) {
  int pair[2];
  if (socketpair(AF_UNIX, SOCK_STREAM, 0, pair) != 0) {
    die("socketpair");
  }
  fflush(stdout);
  pid_t child = fork();
  if (child < 0) {
    die("fork");
  }
  if (child == 0) {
    close(pair[0]);
    int null = open("/dev/null", O_RDONLY);
    if (null < 0) {
      die("open /dev/null");
    }
    send_descriptor(pair[1], null);
    byte_in(pair[1]);
    _exit(0);
  }
  close(pair[1]);
  int file = open_path(path, O_RDWR);
  ofd(file, F_RDLCK, 0, 1);
  int received = receive_descriptor(pair[0]);
  (void)dup2(received, file);
  snapshot("replaced");
  byte_out(pair[0]);
  wait_for(child);
  return 0;
}

// The child shares the parent's descriptor table (CLONE_FILES), from before
// any lock. The parent locks; the child closes the table's only slot naming
// the description, which releases the lock.
static int stale_shared_table_scenario(const char* path) {
  int ready[2];
  int go[2];
  if (pipe(ready) != 0 || pipe(go) != 0) {
    die("pipe");
  }
  fflush(stdout);
  long child = syscall(SYS_clone, CLONE_FILES | SIGCHLD, NULL, NULL, NULL, NULL);
  if (child < 0) {
    die("clone");
  }
  if (child == 0) {
    int fd = -1;
    if (read(go[0], &fd, sizeof(fd)) != sizeof(fd)) {
      _exit(1);
    }
    if (close(fd) != 0) {
      _exit(1);
    }
    byte_out(ready[1]);
    _exit(0);
  }
  int fd = open_path(path, O_RDWR);
  ofd(fd, F_RDLCK, 0, 1);
  if (write(go[1], &fd, sizeof(fd)) != sizeof(fd)) {
    die("write fd");
  }
  byte_in(ready[0]);
  snapshot("child-closed");
  wait_for((pid_t)child);
  return 0;
}

static volatile int private_thread_status = 1;

// A thread with a descriptor table of its own (CLONE_THREAD without
// CLONE_FILES) opens and locks the file, then exits: its table, and the lock,
// go with it.
static int private_table_thread(void* path) {
  int fd = open((const char*)path, O_RDWR);
  struct flock lock = {.l_type = F_RDLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 1};
  private_thread_status = (fd >= 0 && fcntl(fd, F_OFD_SETLK, &lock) == 0) ? 0 : 2;
  return 0;
}

static int stale_private_table_scenario(const char* path) {
  static char stack[256 * 1024] __attribute__((aligned(16)));
  // CLONE_CHILD_SETTID sets it, CLONE_CHILD_CLEARTID clears it and wakes the
  // waiter when the thread has exited.
  static volatile int thread_id = 1;
  pid_t created = clone(private_table_thread, stack + sizeof(stack),
                        CLONE_VM | CLONE_SIGHAND | CLONE_THREAD | CLONE_CHILD_SETTID |
                            CLONE_CHILD_CLEARTID,
                        (void*)path, NULL, NULL, (pid_t*)&thread_id);
  if (created < 0) {
    die("clone");
  }
  for (;;) {
    int current = thread_id;
    if (current == 0) {
      break;
    }
    syscall(SYS_futex, (int*)&thread_id, FUTEX_WAIT, current, NULL, NULL, 0);
  }
  if (private_thread_status != 0) {
    die("thread lock");
  }
  snapshot("thread-exited");
  return 0;
}

// Inside a chroot whose /proc is an ordinary tree, the guest plants an
// "fdinfo" for its own locking descriptor whose lock line is the host's lock
// (`object` from `<jail>/object`), takes a real write lock on another file,
// and reads /proc/locks through a descriptor it opened on the real procfs
// before the chroot. Detcore must not take the planted line as the guest's.
static int fake_proc_scenario(const char* jail) {
  int locks = open("/proc/locks", O_RDONLY);
  if (locks < 0) {
    die("open /proc/locks");
  }
  char path[4096];
  if (snprintf(path, sizeof(path), "%s/object", jail) >= (int)sizeof(path)) {
    die("path");
  }
  char object[256];
  int object_file = open(path, O_RDONLY);
  ssize_t length = object_file < 0 ? -1 : read(object_file, object, sizeof(object) - 1);
  if (length <= 0) {
    die("read object");
  }
  object[length] = 0;
  close(object_file);
  if (chdir(jail) != 0 || chroot(".") != 0) {
    die("chroot");
  }
  int own = open("/own", O_CREAT | O_RDWR, 0600);
  if (own < 0) {
    die("open /own");
  }
  if ((mkdir("/proc", 0755) != 0 && errno != EEXIST) ||
      (mkdir("/proc/thread-self", 0755) != 0 && errno != EEXIST) ||
      (mkdir("/proc/thread-self/fdinfo", 0755) != 0 && errno != EEXIST)) {
    die("mkdir fake proc");
  }
  char fake[64];
  if (snprintf(fake, sizeof(fake), "/proc/thread-self/fdinfo/%d", own) >= (int)sizeof(fake)) {
    die("fake path");
  }
  FILE* planted = fopen(fake, "w");
  if (planted == NULL ||
      fprintf(planted, "pos:\t0\nflags:\t02\nmnt_id:\t1\nino:\t1\nlock:\t1: OFDLCK ADVISORY  READ -1 %s 0 0\n",
              object) < 0 ||
      fclose(planted) != 0) {
    die("plant fdinfo");
  }
  ofd(own, F_WRLCK, 0, 1);
  static char buffer[256 * 1024];
  size_t used = 0;
  for (;;) {
    ssize_t count = pread(locks, buffer + used, sizeof(buffer) - used, (off_t)used);
    if (count < 0 && errno == EINTR) {
      continue;
    }
    if (count < 0) {
      die("read /proc/locks");
    }
    if (count == 0) {
      break;
    }
    used += (size_t)count;
    if (used == sizeof(buffer)) {
      die("snapshot too large");
    }
  }
  printf("--- after-fake ---\n");
  if (fwrite(buffer, 1, used, stdout) != used || fflush(stdout) != 0) {
    die("write snapshot");
  }
  return 0;
}

// An OFD lock, then vfork; the child exits at once. DBT runs a vfork child's
// calls natively, so it refuses this vfork rather than let the parent's lock
// record go stale.
static int ofd_then_vfork_scenario(const char* path) {
  int fd = open_path(path, O_RDWR);
  ofd(fd, F_RDLCK, 0, 1);
  fflush(stdout);
  pid_t child = vfork();
  if (child < 0) {
    die("vfork");
  }
  if (child == 0) {
    _exit(0);
  }
  wait_for(child);
  snapshot("after-vfork");
  return 0;
}

// The parent opens the file and forks before any lock; the child locks it
// (an OFD read lock, or with `flock`, an exclusive flock) and exits while the
// parent keeps its descriptor.
static int lock_after_fork_scenario(const char* path, int use_flock) {
  int fd = open_path(path, O_RDWR);
  int ready[2];
  int go[2];
  if (pipe(ready) != 0 || pipe(go) != 0) {
    die("pipe");
  }
  fflush(stdout);
  pid_t child = fork();
  if (child < 0) {
    die("fork");
  }
  if (child == 0) {
    if (use_flock) {
      if (flock(fd, LOCK_EX | LOCK_NB) != 0) {
        die("flock");
      }
    } else {
      ofd(fd, F_RDLCK, 0, 1);
    }
    byte_out(ready[1]);
    byte_in(go[0]);
    _exit(0);
  }
  byte_in(ready[0]);
  snapshot("child-holds");
  byte_out(go[1]);
  wait_for(child);
  snapshot("after-child");
  return 0;
}

// The parent opens `path` and forks before any lock; the child takes an OFD
// read lock on byte 0 (with `use_flock`, a shared flock) and stays alive, so only the child's split copy of the
// description's model recorded the lock and the child's table keeps it the
// guest's in the `/proc/locks` ledger. The parent then vforks, and the vfork
// child unlocks it; the parent snapshots before letting the child exit.
static int lock_after_fork_vfork_unlock_scenario(const char* path, int use_flock) {
  int fd = open_path(path, O_RDWR);
  int ready[2];
  int go[2];
  if (pipe(ready) != 0 || pipe(go) != 0) {
    die("pipe");
  }
  fflush(stdout);
  pid_t child = fork();
  if (child < 0) {
    die("fork");
  }
  if (child == 0) {
    if (use_flock) {
      if (flock(fd, LOCK_SH | LOCK_NB) != 0) {
        die("flock");
      }
    } else {
      ofd(fd, F_RDLCK, 0, 1);
    }
    byte_out(ready[1]);
    byte_in(go[0]);
    _exit(0);
  }
  byte_in(ready[0]);
  snapshot("child-holds");
  pid_t unlocker = vfork();
  if (unlocker < 0) {
    die("vfork");
  }
  if (unlocker == 0) {
    if (use_flock) {
      _exit(flock(fd, LOCK_UN) == 0 ? 0 : 3);
    }
    struct flock lock = {.l_type = F_UNLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 1};
    if (fcntl(fd, F_OFD_SETLK, &lock) == 0) {
      _exit(0);
    }
    _exit(errno == ENOLCK ? 10 : 3);
  }
  int status = 0;
  if (waitpid(unlocker, &status, 0) != unlocker || !WIFEXITED(status) ||
      (WEXITSTATUS(status) != 0 && WEXITSTATUS(status) != 10)) {
    die("unlocker");
  }
  // 10: the unlock was refused with ENOLCK and the lock is still held (DBT
  // under --allow-unsupported-syscalls). Reported on stderr, outside the
  // snapshots.
  fprintf(stderr, "unlocker-status=%d\n", WEXITSTATUS(status));
  snapshot("after-vfork-unlock");
  byte_out(go[1]);
  wait_for(child);
  return 0;
}

static int shared_flock_scenario(const char* path) {
  if (flock(open_path(path, O_RDWR), LOCK_SH | LOCK_NB) != 0) {
    die("flock");
  }
  snapshot("shared");
  return 0;
}

static int write_only_scenario(const char* directory) {
  int file = open_in(directory, "write-only");
  struct flock* lock = mmap(NULL, 4096, PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (lock == MAP_FAILED) {
    die("mmap");
  }
  *lock = (struct flock){.l_type = F_WRLCK, .l_whence = SEEK_SET};
  if (fcntl(file, F_OFD_SETLK, lock) != 0) {
    die("F_OFD_SETLK");
  }
  snapshot("write-only");
  return 0;
}

static int exhaust_scenario(const char* directory) {
  int file = open_in(directory, "exhaust");
  // Map the whole address space away, in PROT_NONE reservations that cost no
  // memory, largest first. Hermit's resource limits are virtual, so
  // RLIMIT_AS cannot do this.
  for (size_t size = (size_t)1 << 46; size >= 4096; size /= 2) {
    while (mmap(NULL, size, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0) !=
           MAP_FAILED) {
    }
  }
  static const char exhausted[] = "address space exhausted\n";
  if (write(1, exhausted, sizeof(exhausted) - 1) < 0) {
    return 2;
  }
  struct flock lock = {.l_type = F_WRLCK, .l_whence = SEEK_SET};
  int result = fcntl(file, F_OFD_SETLK, &lock);
  static const char reached[] = "the lock returned\n";
  if (write(1, reached, sizeof(reached) - 1) < 0 || result != 0) {
    return 2;
  }
  return 0;
}

// With `read_after`, the guest then frees two slots and reads /proc/locks.
static int descriptors_scenario(const char* directory, int read_after) {
  int file = open_in(directory, "descriptors");
  // Fill the descriptor table up to the soft limit the test set for Hermit,
  // which the guest inherits; Hermit's resource limits are virtual, so the
  // guest's own setrlimit could not lower the kernel's.
  int last = file;
  for (;;) {
    int next = dup(file);
    if (next < 0) {
      break;
    }
    last = next;
  }
  if (errno != EMFILE || last - 1 <= file) {
    die("dup");
  }
  static const char exhausted[] = "descriptors exhausted\n";
  if (write(1, exhausted, sizeof(exhausted) - 1) < 0) {
    return 2;
  }
  struct flock lock = {.l_type = F_WRLCK, .l_whence = SEEK_SET};
  int result = fcntl(file, F_OFD_SETLK, &lock);
  static const char reached[] = "the lock returned\n";
  if (write(1, reached, sizeof(reached) - 1) < 0 || result != 0) {
    return 2;
  }
  if (read_after) {
    if (close(last) != 0 || close(last - 1) != 0) {
      die("close");
    }
    snapshot("after-lock");
  }
  return 0;
}

int main(int argc, char** argv) {
  if (argc != 3) {
    return 2;
  }
  const char* mode = argv[1];
  const char* where = argv[2];
  if (strcmp(mode, "unlock") == 0) {
    return unlock_scenario(where);
  }
  if (strcmp(mode, "fork") == 0) {
    return fork_scenario(where);
  }
  if (strcmp(mode, "transfer") == 0) {
    char path[4096];
    if (snprintf(path, sizeof(path), "%s/transfer-ofd", where) >= (int)sizeof(path)) {
      die("path");
    }
    return transfer_scenario(path, F_WRLCK, 0);
  }
  if (strcmp(mode, "transfer-sender-first") == 0) {
    char path[4096];
    if (snprintf(path, sizeof(path), "%s/transfer-ofd", where) >= (int)sizeof(path)) {
      die("path");
    }
    return transfer_scenario(path, F_WRLCK, 1);
  }
  if (strcmp(mode, "in-flight-sender-closes") == 0 ||
      strcmp(mode, "in-flight-sender-exits") == 0) {
    char path[4096];
    if (snprintf(path, sizeof(path), "%s/in-flight-ofd", where) >= (int)sizeof(path)) {
      die("path");
    }
    return in_flight_scenario(path, strcmp(mode, "in-flight-sender-exits") == 0);
  }
  if (strcmp(mode, "write-only") == 0) {
    return write_only_scenario(where);
  }
  if (strcmp(mode, "exhaust") == 0) {
    return exhaust_scenario(where);
  }
  if (strcmp(mode, "descriptors") == 0) {
    return descriptors_scenario(where, 0);
  }
  if (strcmp(mode, "descriptors-read") == 0) {
    return descriptors_scenario(where, 1);
  }
  if (strcmp(mode, "stale-unlock") == 0) {
    return stale_unlock_scenario(where);
  }
  if (strcmp(mode, "stale-reap") == 0) {
    return stale_reap_scenario(where);
  }
  if (strcmp(mode, "stale-fork") == 0) {
    return stale_fork_scenario(where);
  }
  if (strcmp(mode, "stale-transfer") == 0) {
    return transfer_scenario(where, F_RDLCK, 0);
  }
  if (strcmp(mode, "stale-exec") == 0) {
    return stale_exec_scenario(where, argv[0]);
  }
  if (strcmp(mode, "stale-receiver-unlock") == 0) {
    return stale_receiver_unlock_scenario(where);
  }
  if (strcmp(mode, "stale-dup-over") == 0) {
    return stale_dup_over_scenario(where);
  }
  if (strcmp(mode, "stale-shared-table") == 0) {
    return stale_shared_table_scenario(where);
  }
  if (strcmp(mode, "stale-private-table") == 0) {
    return stale_private_table_scenario(where);
  }
  if (strcmp(mode, "ofd-then-vfork") == 0) {
    return ofd_then_vfork_scenario(where);
  }
  if (strcmp(mode, "fake-proc") == 0) {
    return fake_proc_scenario(where);
  }
  if (strcmp(mode, "after-exec") == 0) {
    snapshot("after-exec");
    return 0;
  }
  if (strcmp(mode, "failed-exec") == 0) {
    return failed_exec_scenario(where);
  }
  if (strcmp(mode, "ofd-after-fork") == 0) {
    return lock_after_fork_scenario(where, 0);
  }
  if (strcmp(mode, "flock-after-fork") == 0) {
    return lock_after_fork_scenario(where, 1);
  }
  if (strcmp(mode, "shared-flock") == 0) {
    return shared_flock_scenario(where);
  }
  if (strcmp(mode, "ofd-after-fork-vfork-unlock") == 0) {
    return lock_after_fork_vfork_unlock_scenario(where, 0);
  }
  if (strcmp(mode, "flock-after-fork-vfork-unlock") == 0) {
    return lock_after_fork_vfork_unlock_scenario(where, 1);
  }
  return 2;
}
"#;

/// Compile `source` as `name` in `directory` with the flags the C corpus uses.
fn compile_embedded_guest(directory: &Path, name: &str, source: &str) -> PathBuf {
    let source_path = directory.join(format!("{name}.c"));
    fs::write(&source_path, source).expect("write embedded guest source");
    let guest = directory.join(name);
    let compile = Command::new("cc")
        .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(&source_path)
        .arg("-o")
        .arg(&guest)
        .output()
        .unwrap_or_else(|error| panic!("start cc for {name}: {error}"));
    assert!(
        compile.status.success(),
        "failed to compile {name}:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );
    guest
}

/// The lock classes of each `--- label ---` section of a scenario's output,
/// in order, each section's classes sorted.
fn sections(stdout: &str) -> Sections {
    let mut sections: Sections = Vec::new();
    for line in stdout.lines() {
        if let Some(label) = line
            .strip_prefix("--- ")
            .and_then(|line| line.strip_suffix(" ---"))
        {
            sections.push((label.to_owned(), Vec::new()));
        } else if let Some((_, classes)) = sections.last_mut() {
            let mut fields = line.split_whitespace().skip(1);
            let class = fields
                .find(|field| *field != "->")
                .unwrap_or("?")
                .to_owned();
            classes.push(class);
        }
    }
    for (_, classes) in &mut sections {
        classes.sort();
    }
    sections
}

/// A scenario's output: the lock classes of each labelled section.
type Sections = Vec<(String, Vec<String>)>;

fn expected(sections: Vec<(&str, Vec<&str>)>) -> Sections {
    sections
        .into_iter()
        .map(|(label, classes)| {
            (
                label.to_owned(),
                classes.into_iter().map(str::to_owned).collect(),
            )
        })
        .collect()
}

/// The guest's own OFD and flock rows follow its open file descriptions on
/// `backend`, at L2 (`--verify --verify-strict`, bitwise parity):
/// - after an unlock, nothing of the guest's remains, and a host OFD lock on
///   the same file (another range) is never shown, before or after: the
///   scenario Detcore got wrong when it kept a file alive from host rows;
/// - an OFD lock and a flock a forked child inherited stay shown after the
///   parent closes its descriptors, with the child's POSIX lock, and go when
///   the child exits;
/// - an OFD lock sent over `SCM_RIGHTS` is shown while both hold it, and
///   goes once both have closed it; after the sender's close alone it stays
///   shown, as Linux shows it, because Detcore models the receiver's
///   descriptor on every backend (recorded as it arrives on ptrace,
///   `Detcore::record_received_descriptors`; discovered on DBT);
/// - it stays shown while the message is in flight after the sender closed
///   its descriptor or exited, as Linux's in-flight reference keeps the open
///   file and its lock (https://github.com/rrnewton/hermit/issues/4930), then
///   while the receiver holds it, and goes with the receiver's close; on DBT,
///   which queues nothing, it is not shown in flight (a named residual);
/// - an OFD lock whose argument is on a write-only page is shown.
fn lock_rows_follow_the_guests_descriptions(backend: &str) {
    let root = tempfile::Builder::new()
        .prefix("proc-locks-scenarios-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("create lock-scenario directory");
    let guest = compile_embedded_guest(root.path(), "lock-scenarios", LOCK_SCENARIOS_SOURCE);
    let shared = root.path().join("shared");
    fs::write(&shared, b"shared\n").expect("create the shared lock file");
    // The host holds a shared OFD lock on byte 10 of the file the guest locks
    // byte 0 of, for the whole run.
    let host = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&shared)
        .expect("open the shared lock file");
    record_lock(&host, libc::F_OFD_SETLK, libc::F_RDLCK, 10, 1);
    assert!(
        host_sees_ofd_lock(&shared),
        "the host's /proc/locks does not show the control's OFD lock"
    );
    let own = root.path().join("own");
    fs::create_dir(&own).expect("create the guest's own lock directory");
    // The sender closes before the receiver looks. The receiver's descriptor
    // is modelled and registered on reception on every backend, so the row
    // survives the sender's close, as on Linux.
    let after_sender_close = vec!["OFDLCK"];
    // In flight, the message holds the open file and its lock where Detcore
    // queues the descriptions a send carries (ptrace,
    // `Detcore::queue_sent_rights`). DBT discovers received descriptors
    // lazily and queues nothing, so there the lock leaves the rows with the
    // sender's close or exit and comes back with the receipt: a named
    // residual of the in-guest backends.
    let in_flight = || {
        let in_flight = if backend == "dbt" {
            vec![]
        } else {
            vec!["OFDLCK"]
        };
        expected(vec![
            ("in-flight", in_flight),
            ("received", vec!["OFDLCK"]),
            ("closed", vec![]),
        ])
    };
    let scenarios: [(&str, &OsStr, Sections); 7] = [
        (
            "unlock",
            shared.as_os_str(),
            expected(vec![("held", vec!["OFDLCK"]), ("released", vec![])]),
        ),
        (
            "fork",
            own.as_os_str(),
            expected(vec![
                ("parent-closed", vec!["FLOCK", "OFDLCK", "POSIX"]),
                ("child-exited", vec![]),
            ]),
        ),
        (
            "transfer",
            own.as_os_str(),
            expected(vec![("transferred", vec!["OFDLCK"]), ("closed", vec![])]),
        ),
        (
            "write-only",
            own.as_os_str(),
            expected(vec![("write-only", vec!["OFDLCK"])]),
        ),
        (
            "transfer-sender-first",
            own.as_os_str(),
            expected(vec![
                ("transferred", after_sender_close),
                ("closed", vec![]),
            ]),
        ),
        ("in-flight-sender-closes", own.as_os_str(), in_flight()),
        ("in-flight-sender-exits", own.as_os_str(), in_flight()),
    ];
    for (scenario, argument, want) in scenarios {
        let label = format!("{backend} {scenario}");
        let report = root.path().join(format!("{scenario}-verify.json"));
        let output = run_guest_on(
            backend,
            &guest,
            &[OsStr::new(scenario), argument],
            Some(&report),
        );
        assert_bitwise_parity(&label, &report, &output);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(sections(&stdout), want, "{label}\nstdout:\n{stdout}");
    }
    drop(host);
}

#[test]
fn lock_rows_follow_the_guests_descriptions_on_ptrace() {
    lock_rows_follow_the_guests_descriptions("ptrace");
}

#[test]
fn lock_rows_follow_the_guests_descriptions_on_dbt() {
    if dbt_unavailable() {
        return;
    }
    lock_rows_follow_the_guests_descriptions("dbt");
}

/// A guest that has used up its address space leaves Detcore no page to read
/// an OFD-locking descriptor's fdinfo through. The lock is Linux's to grant
/// and the guest goes on, the same way every time; only a later
/// `/proc/locks` read would refuse, since that description's locks are
/// unknown (`procfs::locks::LockLedger::unknown`).
#[test]
fn an_ofd_lock_without_address_space_left_goes_on() {
    let root = tempfile::Builder::new()
        .prefix("proc-locks-exhaust-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("create exhaust directory");
    let guest = compile_embedded_guest(root.path(), "lock-scenarios", LOCK_SCENARIOS_SOURCE);
    let own = root.path().join("own");
    fs::create_dir(&own).expect("create the guest's own lock directory");
    for attempt in 0..2 {
        let output = run_guest_on(
            "ptrace",
            &guest,
            &[OsStr::new("exhaust"), own.as_os_str()],
            None,
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(0),
            "attempt {attempt}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        assert!(
            stdout.contains("address space exhausted") && stdout.contains("the lock returned"),
            "attempt {attempt}: the guest's lock did not return\nstdout:\n{stdout}"
        );
    }
}

/// Run the lock-scenarios guest in `mode` on ptrace with the soft descriptor
/// limit Hermit starts with, which the guest inherits, lowered so the guest
/// fills its table in a few hundred calls.
fn run_with_a_full_descriptor_table(mode: &str) -> Output {
    use std::os::unix::process::CommandExt;

    /// Comfortably above what Hermit's own processes use, far below the
    /// default limit.
    const SOFT_LIMIT: libc::rlim_t = 512;
    let root = tempfile::Builder::new()
        .prefix("proc-locks-descriptors-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("create descriptors directory");
    let guest = compile_embedded_guest(root.path(), "lock-scenarios", LOCK_SCENARIOS_SOURCE);
    let own = root.path().join("own");
    fs::create_dir(&own).expect("create the guest's own lock directory");
    let mut command = guest_command(
        "ptrace",
        &["--strict"],
        &guest,
        &[OsStr::new(mode), own.as_os_str()],
        None,
    );
    // SAFETY: the closure calls only getrlimit and setrlimit, which are
    // async-signal-safe, on a local value.
    unsafe {
        command.pre_exec(|| {
            let mut limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            limit.rlim_cur = limit.rlim_cur.min(SOFT_LIMIT);
            if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    hermit_test::hermit_output(&format!("{command:?}"), &mut command)
}

/// A guest whose descriptor table is full leaves Detcore no descriptor to read
/// an OFD-locking descriptor's fdinfo through: the open fails with EMFILE.
/// Linux grants the lock, and a guest that never reads `/proc/locks` runs to
/// its end.
#[test]
fn a_full_descriptor_table_lock_goes_on_without_a_proc_locks_read() {
    for attempt in 0..2 {
        let output = run_with_a_full_descriptor_table("descriptors");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(0),
            "attempt {attempt}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        assert!(
            stdout.contains("descriptors exhausted") && stdout.contains("the lock returned"),
            "attempt {attempt}: the guest's lock did not return\nstdout:\n{stdout}"
        );
    }
}

/// After that lock, a `/proc/locks` read cannot tell the description's rows
/// from a host's, since its locks are unknown: the read stops the run, by
/// name, with the policy-refusal exit, the same way every time, rather than
/// taking the failure to mean the description holds no lock.
#[test]
fn a_full_descriptor_table_refuses_the_next_proc_locks_read() {
    for attempt in 0..2 {
        let output = run_with_a_full_descriptor_table("descriptors-read");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
            "attempt {attempt}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        assert!(
            stdout.contains("the lock returned") && !stdout.contains("--- after-lock ---"),
            "attempt {attempt}: the guest did not stop at its /proc/locks read\nstdout:\n{stdout}"
        );
        assert!(
            stderr.contains("/proc/locks: after fcntl on fd")
                && stderr.contains("could not open that descriptor's fdinfo")
                && stderr.contains("EMFILE"),
            "attempt {attempt}: the refusal does not name its reason\nstderr:\n{stderr}"
        );
    }
}

/// A recording in the format before the descriptor-table ledger (labelled
/// 0x16f, and 0x171 after the R2b-2 stage 3 revert's relabel) would replay
/// different `/proc/locks` bytes, since replay recomputes which recorded rows
/// are the guest's, and one from before the procfs anchor (0x172, 0x173) or
/// from before its entry recheck (0x176, 0x177) holds a different fdinfo
/// read, and one from before the capture dropped only repeated rows (0x180,
/// 0x17f, 0x17e, 0x17d, 0x17c) would replay a capture that caught the host's
/// race to different bytes, so the version gate refuses each label; the same
/// recording at the current version replays.
#[test]
fn recordings_from_before_the_lock_ledger_are_refused_by_the_version_gate() {
    let root = tempfile::Builder::new()
        .prefix("proc-locks-version-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("create version directory");
    let guest = compile_embedded_guest(root.path(), "lock-scenarios", LOCK_SCENARIOS_SOURCE);
    let shared = root.path().join("shared");
    fs::write(&shared, b"shared\n").expect("create the shared lock file");
    let data_dir = root.path().join("recording");
    fs::create_dir(&data_dir).expect("create the recording directory");
    let mut record = Command::new("timeout");
    record
        .args(["--kill-after", "5s", "90s"])
        .arg(hermit_test::hermit_binary())
        .args(["--log=off", "record", "start"])
        .arg(format!("--data-dir={}", data_dir.display()))
        .args(["--proc=standard", "--base-env=minimal", "--"])
        .arg(&guest)
        .arg("unlock")
        .arg(&shared);
    hermit_test::configure_guest_execution(&mut record);
    let recorded = hermit_test::hermit_output(&format!("{record:?}"), &mut record);
    assert!(
        recorded.status.success(),
        "recording the unlock scenario failed\nstderr:\n{}",
        String::from_utf8_lossy(&recorded.stderr)
    );
    let replay = || {
        let mut replay = Command::new("timeout");
        replay
            .args(["--kill-after", "5s", "90s"])
            .arg(hermit_test::hermit_binary())
            .args(["--log=off", "replay", "--autopilot"])
            .arg(format!("--data-dir={}", data_dir.display()));
        hermit_test::configure_guest_execution(&mut replay);
        hermit_test::hermit_output(&format!("{replay:?}"), &mut replay)
    };
    let accepted = replay();
    assert!(
        accepted.status.success(),
        "the current-version recording did not replay, so the refusal below would prove \
         nothing\nstderr:\n{}",
        String::from_utf8_lossy(&accepted.stderr)
    );
    let metadata_path = fs::read_dir(&data_dir)
        .expect("read the recording directory")
        .map(|entry| {
            entry
                .expect("read a recording entry")
                .path()
                .join("metadata.json")
        })
        .find(|candidate| candidate.is_file())
        .expect("no recording metadata.json");
    let text = fs::read_to_string(&metadata_path).expect("read the recording metadata");
    let mut metadata: serde_json::Value =
        serde_json::from_str(&text).expect("recording metadata is not JSON");
    assert_eq!(
        metadata["version"], 0x198,
        "RECORD_VERSION moved; point this test at the current version and the ledger's predecessor"
    );
    // 0x180, 0x17f, 0x17e, 0x17d and 0x17c keep, or drop by length alone, the
    // rows a closing read repeats; 0x177 (dbt's) and 0x176 do not recheck the
    // entry as procfs; 0x173 and 0x172 read the fdinfo by path, without the
    // procfs anchor.
    for older in [
        0x180, 0x17f, 0x17e, 0x17d, 0x17c, 0x177, 0x176, 0x173, 0x172, 0x171, 0x16f,
    ] {
        metadata["version"] = serde_json::json!(older);
        fs::write(
            &metadata_path,
            serde_json::to_vec(&metadata).expect("serialize the metadata"),
        )
        .expect("rewrite the recording metadata");
        let refused = replay();
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert!(
            !refused.status.success() && stderr.contains("Version mismatch"),
            "a {older:#x} recording was not refused by the version gate\nstderr:\n{stderr}"
        );
    }
}

/// A recording of the lock scenarios replays to the same `/proc/locks`, at the
/// canonical comparison, while the host holds a lock on the guest's file: the
/// fdinfo reads Detcore makes after each lock call are recorded, and replay,
/// which takes no real lock, matches the recorded rows against the recorded
/// fdinfo.
#[test]
fn lock_scenarios_replay_from_their_recordings() {
    let root = tempfile::Builder::new()
        .prefix("proc-locks-record-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("create record directory");
    let guest = compile_embedded_guest(root.path(), "lock-scenarios", LOCK_SCENARIOS_SOURCE);
    let shared = root.path().join("shared");
    fs::write(&shared, b"shared\n").expect("create the shared lock file");
    let host = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&shared)
        .expect("open the shared lock file");
    record_lock(&host, libc::F_OFD_SETLK, libc::F_RDLCK, 10, 1);
    let own = root.path().join("own");
    fs::create_dir(&own).expect("create the guest's own lock directory");
    for (scenario, argument) in [
        ("unlock", shared.as_os_str()),
        ("fork", own.as_os_str()),
        ("transfer", own.as_os_str()),
    ] {
        let report = root.path().join(format!("{scenario}-record.json"));
        let mut command = Command::new("timeout");
        command
            .args(["--kill-after", "5s", "90s"])
            .arg(hermit_test::hermit_binary())
            .args([
                "--log=info",
                "record",
                "start",
                "--verify",
                "--verify-strict",
                "--verify-json",
            ])
            .arg(&report)
            .args(["--proc=standard", "--base-env=minimal", "--"])
            .arg(&guest)
            .arg(scenario)
            .arg(argument);
        hermit_test::configure_guest_execution(&mut command);
        let output = hermit_test::hermit_output(&format!("{command:?}"), &mut command);
        assert_bitwise_parity(&format!("record {scenario}"), &report, &output);
    }
    drop(host);
}

/// The release paths of `procfs::locks::LockLedger` keep no stale entry, on
/// `backend`, at L2: the host holds a shared OFD read lock on byte 0 of a file
/// for the whole run, and the guest takes the identical lock there. While the
/// guest holds it, the snapshot shows one row; after each way the guest lets
/// go of it, the host's identical row must not appear:
/// - an unlock;
/// - a child that locks and exits, once reaped;
/// - a parent that closes after forking, once the child exits too;
/// - a description sent over `SCM_RIGHTS`, once sender and receiver closed it;
/// - a close-on-exec descriptor, after a successful exec;
/// - an unlock through a descriptor received over `SCM_RIGHTS` while the
///   sender keeps its own;
/// - `dup2` over the only slot, from a received source;
/// - on ptrace, a close through a table shared with `CLONE_FILES` from
///   before the lock, and the exit of a thread with a private table that
///   took it.
///
/// The lock stays shown after a failed exec. Two named residuals are
/// pinned too: a lock a child takes on a description its parent opened before
/// forking is shown while the child lives and hidden after it exits (OFD), or
/// after it is reaped (flock, which Linux itself hides once its taker's pid is
/// freed in a PID namespace).
fn stale_ledger_entries_admit_no_host_row(backend: &str) {
    let root = tempfile::Builder::new()
        .prefix("proc-locks-stale-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("create stale-entry directory");
    let guest = compile_embedded_guest(root.path(), "lock-scenarios", LOCK_SCENARIOS_SOURCE);
    let shared = root.path().join("stale");
    fs::write(&shared, b"stale\n").expect("create the stale-entry lock file");
    let host = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&shared)
        .expect("open the stale-entry lock file");
    record_lock(&host, libc::F_OFD_SETLK, libc::F_RDLCK, 0, 1);
    assert!(
        host_sees_ofd_lock(&shared),
        "the host's /proc/locks does not show the control's OFD lock"
    );
    let flock_file = root.path().join("flock-after-fork");
    fs::write(&flock_file, b"flock\n").expect("create the flock file");
    let mut scenarios: Vec<(&str, &OsStr, Sections)> = vec![
        (
            "stale-unlock",
            shared.as_os_str(),
            expected(vec![("held", vec!["OFDLCK"]), ("released", vec![])]),
        ),
        (
            "stale-reap",
            shared.as_os_str(),
            expected(vec![("reaped", vec![])]),
        ),
        (
            "stale-fork",
            shared.as_os_str(),
            expected(vec![
                ("parent-closed", vec!["OFDLCK"]),
                ("child-exited", vec![]),
            ]),
        ),
        (
            "stale-transfer",
            shared.as_os_str(),
            expected(vec![("transferred", vec!["OFDLCK"]), ("closed", vec![])]),
        ),
        (
            "stale-exec",
            shared.as_os_str(),
            expected(vec![("after-exec", vec![])]),
        ),
        (
            "failed-exec",
            shared.as_os_str(),
            expected(vec![("exec-failed", vec!["OFDLCK"])]),
        ),
        (
            "ofd-after-fork",
            shared.as_os_str(),
            expected(vec![
                ("child-holds", vec!["OFDLCK"]),
                ("after-child", vec![]),
            ]),
        ),
        (
            "flock-after-fork",
            flock_file.as_os_str(),
            expected(vec![
                ("child-holds", vec!["FLOCK"]),
                ("after-child", vec![]),
            ]),
        ),
        // An unlock through a received descriptor, while the sender keeps its
        // own: no lock on that file is shown again (`LockLedger::alias_inode`).
        (
            "stale-receiver-unlock",
            shared.as_os_str(),
            expected(vec![("receiver-unlocked", vec![])]),
        ),
        // dup2 over the locked descriptor's only slot, from a received source.
        (
            "stale-dup-over",
            shared.as_os_str(),
            expected(vec![("replaced", vec![])]),
        ),
    ];
    if backend == "ptrace" {
        // DBT refuses CLONE_FILES without CLONE_THREAD; on ptrace a shared
        // table and a private-table thread make their process's locks
        // uncounted (`LockLedger::mark_opaque`).
        scenarios.push((
            "stale-shared-table",
            shared.as_os_str(),
            expected(vec![("child-closed", vec![])]),
        ));
        scenarios.push((
            "stale-private-table",
            shared.as_os_str(),
            expected(vec![("thread-exited", vec![])]),
        ));
    }
    for (scenario, argument, want) in scenarios {
        let label = format!("{backend} {scenario}");
        let report = root.path().join(format!("{scenario}-verify.json"));
        let output = run_guest_on(
            backend,
            &guest,
            &[OsStr::new(scenario), argument],
            Some(&report),
        );
        assert_bitwise_parity(&label, &report, &output);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(sections(&stdout), want, "{label}\nstdout:\n{stdout}");
    }
    drop(host);
}

#[test]
fn stale_ledger_entries_admit_no_host_row_on_ptrace() {
    stale_ledger_entries_admit_no_host_row("ptrace");
}

#[test]
fn stale_ledger_entries_admit_no_host_row_on_dbt() {
    if dbt_unavailable() {
        return;
    }
    stale_ledger_entries_admit_no_host_row("dbt");
}

/// A guest that chroots into a tree whose `/proc` is ordinary directories can
/// plant an "fdinfo" for its own locking descriptor whose lock line is a host
/// lock, field for field. Detcore reads a description's fdinfo only through
/// an anchor proved to be procfs, so the planted line is not taken as the
/// guest's lock and the host's identical `/proc/locks` row stays out (L2,
/// ptrace). The guest's own lock is hidden too: its `/proc` is not procfs.
#[test]
fn a_fake_proc_under_chroot_admits_no_host_row() {
    let root = tempfile::Builder::new()
        .prefix("proc-locks-fake-proc-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("create fake-proc directory");
    let guest = compile_embedded_guest(root.path(), "lock-scenarios", LOCK_SCENARIOS_SOURCE);
    let held = root.path().join("held");
    fs::write(&held, b"held\n").expect("create the host-locked file");
    let host = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&held)
        .expect("open the host-locked file");
    record_lock(&host, libc::F_OFD_SETLK, libc::F_RDLCK, 0, 1);
    // The host lock's object, exactly as /proc/locks prints it.
    let suffix = inode_suffix(&held);
    let object = fs::read_to_string("/proc/locks")
        .expect("read the host's /proc/locks")
        .lines()
        .filter(|line| line.contains("OFDLCK") && line.contains(" -1 ") && line.contains(&suffix))
        .find_map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            let owner = fields.iter().position(|field| *field == "-1")?;
            fields.get(owner + 1).map(|object| object.to_string())
        })
        .expect("the host's /proc/locks does not show the control's OFD lock");
    let jail = root.path().join("jail");
    fs::create_dir(&jail).expect("create the jail");
    fs::write(jail.join("object"), object.as_bytes()).expect("write the lock object");
    // The jail's tree exists before the first run, so the guest's calls that
    // make it (mkdir, open with O_CREAT) answer the same in both runs of the
    // L2 check.
    fs::create_dir_all(jail.join("proc/thread-self/fdinfo")).expect("create the fake /proc");
    fs::write(jail.join("own"), b"").expect("create the jail's own file");
    let report = root.path().join("fake-proc-verify.json");
    let output = run_guest_on(
        "ptrace",
        &guest,
        &[OsStr::new("fake-proc"), jail.as_os_str()],
        Some(&report),
    );
    assert_bitwise_parity("ptrace fake-proc", &report, &output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        sections(&stdout),
        expected(vec![("after-fake", vec![])]),
        "the planted fdinfo admitted a host row\n{stdout}"
    );
    drop(host);
}

/// The exit status of a run DBT's runtime refuses at a process lifecycle
/// call, as hermit-cli's flock_exclusion tests require it too.
const DBT_REFUSAL_EXIT: i32 = 101;

/// DBT runs a `vfork` child's calls natively, where Detcore would not see an
/// OFD lock or unlock it makes, so it refuses a `vfork` while a description
/// may hold an OFD lock; ptrace sees every call and runs it, showing the
/// parent's lock after the child exits.
#[test]
fn a_vfork_after_an_ofd_lock_is_refused_on_dbt_and_runs_on_ptrace() {
    let root = tempfile::Builder::new()
        .prefix("proc-locks-vfork-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("create vfork directory");
    let guest = compile_embedded_guest(root.path(), "lock-scenarios", LOCK_SCENARIOS_SOURCE);
    let file = root.path().join("vfork");
    fs::write(&file, b"vfork\n").expect("create the vfork lock file");
    let report = root.path().join("ptrace-verify.json");
    let output = run_guest_on(
        "ptrace",
        &guest,
        &[OsStr::new("ofd-then-vfork"), file.as_os_str()],
        Some(&report),
    );
    assert_bitwise_parity("ptrace ofd-then-vfork", &report, &output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        sections(&stdout),
        expected(vec![("after-vfork", vec!["OFDLCK"])]),
        "{stdout}"
    );
    if dbt_unavailable() {
        return;
    }
    // At log level info, where DBT prints its lifecycle refusals.
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after", "5s", "90s"])
        .arg(hermit_test::hermit_binary())
        .args([
            "--log=info",
            "--backend=dbt",
            "run",
            "--strict",
            "--proc=standard",
            "--no-virtualize-cpuid",
            "--max-timeslice=disabled",
            "--panic-on-unsupported-syscalls",
            "--base-env=minimal",
            "--",
        ])
        .arg(&guest)
        .arg("ofd-then-vfork")
        .arg(&file);
    hermit_test::configure_guest_execution(&mut command);
    let output = hermit_test::hermit_output(&format!("{command:?}"), &mut command);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.code() == Some(DBT_REFUSAL_EXIT)
            && stderr.contains(
                "detcore-dbt: refusing vfork/CLONE_VFORK while an open file description may hold an OFD lock"
            )
            && !stdout.contains("--- after-vfork ---"),
        "DBT ran a vfork child after an OFD lock\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

/// A sibling's OFD lock, then a copied child's native unlock of it, with a
/// host process holding the identical OFD read lock the whole time. The
/// parent's up-front OFD vfork guard cannot see a lock a forked child took
/// through its own copy of the description's model. Natively on DBT the
/// copied child's unlock would leave the `/proc/locks` ledger claiming the
/// guest's lock, which admits the host's identical row (OFD rows print owner
/// -1 in every namespace). DBT never runs the copied child's `F_OFD_SETLK`:
/// under `--strict` it ends the run by name before any later snapshot; under
/// the `--allow-unsupported-syscalls` opt-out it names the refusal and
/// answers ENOLCK, so the lock is still held, and still the guest's, in the
/// later snapshot. ptrace sees the unlock and runs the scenario at L2 with
/// nothing shown after it.
#[test]
fn a_copied_childs_ofd_unlock_is_refused_on_dbt_and_seen_on_ptrace() {
    let root = tempfile::Builder::new()
        .prefix("proc-locks-sibling-ofd-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("create sibling-ofd directory");
    let guest = compile_embedded_guest(root.path(), "lock-scenarios", LOCK_SCENARIOS_SOURCE);
    let file = root.path().join("sibling-ofd");
    fs::write(&file, b"sibling\n").expect("create the sibling-ofd lock file");
    let host = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&file)
        .expect("open the sibling-ofd lock file");
    record_lock(&host, libc::F_OFD_SETLK, libc::F_RDLCK, 0, 1);
    assert!(
        host_sees_ofd_lock(&file),
        "the host's /proc/locks does not show the control's OFD lock"
    );
    let report = root.path().join("ptrace-verify.json");
    let output = run_guest_on(
        "ptrace",
        &guest,
        &[OsStr::new("ofd-after-fork-vfork-unlock"), file.as_os_str()],
        Some(&report),
    );
    assert_bitwise_parity("ptrace ofd-after-fork-vfork-unlock", &report, &output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        sections(&stdout),
        expected(vec![
            ("child-holds", vec!["OFDLCK"]),
            ("after-vfork-unlock", vec![]),
        ]),
        "{stdout}"
    );
    if dbt_unavailable() {
        drop(host);
        return;
    }
    // At log level info, where DBT prints its refusals.
    for (policy, refused_with_errno) in [
        (&["--strict", "--panic-on-unsupported-syscalls"][..], false),
        (&["--allow-unsupported-syscalls"][..], true),
    ] {
        let mut command = Command::new("timeout");
        command
            .args(["--kill-after", "5s", "90s"])
            .arg(hermit_test::hermit_binary())
            .args(["--log=info", "--backend=dbt", "run"])
            .args(policy)
            .args([
                "--proc=standard",
                "--no-virtualize-cpuid",
                "--max-timeslice=disabled",
                "--base-env=minimal",
                "--",
            ])
            .arg(&guest)
            .arg("ofd-after-fork-vfork-unlock")
            .arg(&file);
        hermit_test::configure_guest_execution(&mut command);
        let output = hermit_test::hermit_output(&format!("{command:?}"), &mut command);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let refusal =
            stderr.contains("detcore-dbt: refusing fcntl F_OFD_SETLK/F_OFD_SETLKW in copied child");
        if refused_with_errno {
            assert!(
                output.status.success()
                    && refusal
                    && stderr.contains("unlocker-status=10")
                    && sections(&stdout)
                        == expected(vec![
                            ("child-holds", vec!["OFDLCK"]),
                            ("after-vfork-unlock", vec!["OFDLCK"]),
                        ]),
                "{policy:?}: DBT did not refuse a copied child's OFD unlock with ENOLCK\nstdout:\n{stdout}\nstderr:\n{stderr}"
            );
        } else {
            assert!(
                output.status.code() == Some(DBT_REFUSAL_EXIT)
                    && refusal
                    && stdout.contains("--- child-holds ---")
                    && !stdout.contains("--- after-vfork-unlock ---"),
                "{policy:?}: DBT let a copied child unlock a sibling's OFD lock natively\nstdout:\n{stdout}\nstderr:\n{stderr}"
            );
        }
    }
    drop(host);
}

/// Codex's flock control for the same shape: a forked sibling takes
/// `LOCK_SH|LOCK_NB` through Detcore after the fork and stays alive, so the
/// `/proc/locks` ledger records the description's flock; the parent vforks
/// and the vfork child runs `LOCK_UN` on its inherited descriptor; a host
/// process holds the identical shared flock throughout. Under
/// `--no-namespace` the guest's procfs shows the host's flock row. On DBT the
/// copied child's unlock is native, so the ledger's record of the sibling's
/// flock goes stale; a flock row is kept only by its taker's pid, so the
/// host's row stays out anyway. On ptrace the unlock is mediated; the same
/// shape checks the owner rule there. Both backends show the sibling's flock
/// (its taker is a guest process) and then nothing.
///
/// Relaxation: `--strict` refuses `--no-namespace`, so these runs are not
/// strict, and their task ids are the host's; the sections are compared,
/// not logs.
#[test]
fn a_stale_flock_record_admits_no_host_row_without_a_namespace() {
    let root = tempfile::Builder::new()
        .prefix("proc-locks-sibling-flock-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("create sibling-flock directory");
    let guest = compile_embedded_guest(root.path(), "lock-scenarios", LOCK_SCENARIOS_SOURCE);
    let file = root.path().join("sibling-flock");
    fs::write(&file, b"sibling\n").expect("create the sibling-flock file");
    let host = File::open(&file).expect("open the sibling-flock file");
    // SAFETY: `host` owns a valid descriptor.
    assert_eq!(
        unsafe { libc::flock(host.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) },
        0,
        "host flock: {}",
        std::io::Error::last_os_error()
    );
    let suffix = inode_suffix(&file);
    assert!(
        fs::read_to_string("/proc/locks")
            .expect("read the host's /proc/locks")
            .lines()
            .any(|line| line.contains("FLOCK") && line.contains(&suffix)),
        "the host's /proc/locks does not show the control's flock"
    );
    let mut backends = vec!["ptrace"];
    if !dbt_unavailable() {
        backends.push("dbt");
    }
    for backend in backends {
        let output = run_guest_on_with(
            backend,
            &["--no-namespace"],
            &guest,
            &[
                OsStr::new("flock-after-fork-vfork-unlock"),
                file.as_os_str(),
            ],
            None,
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "{backend}: run failed\nstdout:\n{stdout}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            sections(&stdout),
            expected(vec![
                ("child-holds", vec!["FLOCK"]),
                ("after-vfork-unlock", vec![]),
            ]),
            "{backend}: the host's identical flock reached the guest\n{stdout}"
        );
    }
    drop(host);
}

/// Under `--no-namespace` the guest sees a host process's flock rows too.
/// When the guest and the host hold identical shared flocks on one file, the
/// guest's row is kept by its taker's pid and the host's, whose pid names no
/// guest process, is not: one row, not two, whether the host holds its flock
/// or not.
#[test]
fn identical_shared_flocks_show_the_guests_row_once() {
    let root = tempfile::Builder::new()
        .prefix("proc-locks-shared-flock-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("create shared-flock directory");
    let guest = compile_embedded_guest(root.path(), "lock-scenarios", LOCK_SCENARIOS_SOURCE);
    let file = root.path().join("shared-flock");
    fs::write(&file, b"flock\n").expect("create the shared-flock file");
    // Relaxation: `--strict` refuses `--no-namespace`, so these runs are not
    // strict. Without a PID namespace the guest's task ids are the host's, so
    // two runs' logs differ by design; the outputs are compared instead. The
    // run still goes through Detcore's /proc/locks filter, which is what is
    // under test.
    let run = |label: &str| {
        let output = run_guest_on_with(
            "ptrace",
            &["--no-namespace"],
            &guest,
            &[OsStr::new("shared-flock"), file.as_os_str()],
            None,
        );
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        assert!(
            output.status.success(),
            "{label}: run failed\nstdout:\n{stdout}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        stdout
    };
    let alone = run("without the host's flock");
    let host = File::open(&file).expect("open the shared-flock file");
    // SAFETY: `host` owns a valid descriptor.
    assert_eq!(
        unsafe { libc::flock(host.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) },
        0,
        "host flock: {}",
        std::io::Error::last_os_error()
    );
    let with_host = run("with the host's identical flock");
    drop(host);
    assert_eq!(
        sections(&alone),
        expected(vec![("shared", vec!["FLOCK"])]),
        "{alone}"
    );
    assert_eq!(
        with_host, alone,
        "the host's identical flock reached the guest"
    );
}
