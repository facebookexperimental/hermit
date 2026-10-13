// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
// Licensed under the BSD-style license in the LICENSE file.

//! timerfd on Detcore's virtual clock, end to end: each mode of
//! `tests/fixtures/timerfd_virtual_time.c` checks that a timer wakes when it
//! must and not before, and strict verification requires both runs to match
//! bit for bit. Calls that would wait in the host on a timerfd only Detcore
//! makes readable, and a timerfd the host never makes readable, end the run
//! with a named refusal.

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use detcore_model::HERMIT_POLICY_REFUSAL_EXIT;
use hermit::canonical_verdict::VerificationReport;

use super::kvm_cancellation::bounded_command_with_timeout;
use super::kvm_cancellation::bounded_read;

const MIB: u64 = 1024 * 1024;

/// The fixture, compiled once per test process.
fn guest() -> &'static Path {
    static GUEST: OnceLock<PathBuf> = OnceLock::new();
    GUEST.get_or_init(|| {
        let root = tempfile::Builder::new()
            .prefix("timerfd-virtual-time-")
            .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .expect("timerfd fixture directory")
            .keep();
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/timerfd_virtual_time.c");
        let guest = root.join("program");
        let compile = root.join("compile");
        let status = bounded_command_with_timeout(
            Command::new("cc")
                .args([
                    "-std=gnu11",
                    "-O2",
                    "-g",
                    "-Wall",
                    "-Wextra",
                    "-Wpedantic",
                    "-Wformat=2",
                    "-Werror",
                    "-fno-pie",
                    "-no-pie",
                ])
                .arg(&fixture)
                .arg("-o")
                .arg(&guest),
            &compile,
            Duration::from_secs(20),
        );
        assert!(status.success(), "timerfd fixture compilation failed");
        guest
    })
}

fn require_kvm(backend: &str) {
    if backend == "kvm" {
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/kvm")
            .expect("selected KVM timerfd regression requires /dev/kvm; absence is not a pass");
    }
}

fn run_dir(backend: &str, mode: &str) -> PathBuf {
    let directory = tempfile::Builder::new()
        .prefix(&format!("timerfd-{backend}-{mode}-"))
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("timerfd run directory")
        .keep();
    eprintln!(
        "{backend} timerfd {mode} artifacts retained at {}",
        directory.display()
    );
    directory
}

/// Whether `actual` matches `expected` line for line, where `{n}` in an
/// expected line stands for a decimal number (counts and remaining times are
/// functions of virtual time, which the bitwise comparison pins run to run).
fn matches(expected: &str, actual: &str) -> bool {
    let pattern = format!("^{}$", regex::escape(expected).replace(r"\{n\}", r"[0-9]+"));
    regex::Regex::new(&pattern)
        .expect("expected-output pattern")
        .is_match(actual)
}

/// Run `mode` under strict verification and require a bitwise match with
/// stdout matching `expected`.
pub(super) fn verify(backend: &str, mode: &str, expected: &str) {
    let _lock = super::hermit_run_guard();
    require_kvm(backend);
    let directory = run_dir(backend, mode);
    let report_path = directory.join("verification.json");
    let args = [
        "--log=info",
        "--backend",
        backend,
        "run",
        "--base-env=minimal",
        "--strict",
        "--verify-strict",
        "--verify",
        "--verify-json",
        report_path.to_str().unwrap(),
        "--mount=type=tmpfs,target=/test",
        "--workdir=/test",
        "--",
        guest().to_str().unwrap(),
        mode,
    ];
    let mut command = super::hermit_command(&args);
    command.env("HERMIT_LOG_MAX_BYTES", (64 * MIB).to_string());
    let status = bounded_command_with_timeout(&mut command, &directory, Duration::from_secs(57));
    let stdout =
        String::from_utf8_lossy(&bounded_read(&directory.join("stdout"), 16 * MIB)).into_owned();
    let stderr =
        String::from_utf8_lossy(&bounded_read(&directory.join("stderr"), 16 * MIB)).into_owned();
    assert_eq!(
        status.code(),
        Some(0),
        "{backend} timerfd {mode} status\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        matches(expected, &stdout),
        "{backend} timerfd {mode} stdout\nexpected (pattern):\n{expected}\nactual:\n{stdout}"
    );
    let report = VerificationReport::from_json_slice(&bounded_read(&report_path, 16 * MIB))
        .expect("complete typed verification report");
    report
        .require_canonical_match()
        .expect("full nonempty canonical INFO match");
    report
        .require_exact_output_match()
        .expect("exact two-run status/stdout/stderr match");
}

/// Run `mode` once under `--strict` with `env` set, and require the named
/// refusal `reason` and Hermit's refusal status.
pub(super) fn refused(backend: &str, mode: &str, env: Option<(&str, &str)>, reason: &str) {
    let _lock = super::hermit_run_guard();
    require_kvm(backend);
    let directory = run_dir(backend, mode);
    let args = [
        "--backend",
        backend,
        "run",
        "--base-env=minimal",
        "--strict",
        "--mount=type=tmpfs,target=/test",
        "--workdir=/test",
        "--",
        guest().to_str().unwrap(),
        mode,
    ];
    let mut command = super::hermit_command(&args);
    if let Some((name, value)) = env {
        command.env(name, value);
    }
    let status = bounded_command_with_timeout(&mut command, &directory, Duration::from_secs(57));
    let stderr =
        String::from_utf8_lossy(&bounded_read(&directory.join("stderr"), 16 * MIB)).into_owned();
    assert_eq!(
        status.code(),
        Some(HERMIT_POLICY_REFUSAL_EXIT),
        "{backend} timerfd {mode} must end on a refusal\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains(reason)
            && stderr.contains("https://github.com/rrnewton/hermit/issues/1923"),
        "{backend} timerfd {mode} refusal must name {reason:?}\nstderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("EIO"),
        "a refusal, not an errno\nstderr:\n{stderr}"
    );
}

/// A host signal pending when Detcore arms an expired timerfd interrupts the
/// injection, which retries: the run completes, the guest's caught SIGCONT
/// handler runs, it sees every expiration, and no call fails. That holds on
/// ptrace, where an injection can meet a pending host signal. On KVM guest ids
/// name no host thread, so the hook sends nothing and the run is unchanged.
pub(super) fn signal_during_publish(backend: &str) {
    let _lock = super::hermit_run_guard();
    require_kvm(backend);
    let directory = run_dir(backend, "signal-before-arm");
    let args = [
        "--backend",
        backend,
        "run",
        "--base-env=minimal",
        "--strict",
        "--mount=type=tmpfs,target=/test",
        "--workdir=/test",
        "--",
        guest().to_str().unwrap(),
        "catchup-sigcont",
    ];
    let mut command = super::hermit_command(&args);
    command.env("HERMIT_TEST_TIMERFD_FAULT", "signal-before-arm");
    let status = bounded_command_with_timeout(&mut command, &directory, Duration::from_secs(57));
    let stdout =
        String::from_utf8_lossy(&bounded_read(&directory.join("stdout"), 16 * MIB)).into_owned();
    let stderr =
        String::from_utf8_lossy(&bounded_read(&directory.join("stderr"), 16 * MIB)).into_owned();
    assert_eq!(
        status.code(),
        Some(0),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let interrupted = backend == "ptrace";
    let expected = format!("{CATCHUP}sigcont caught={}\n", u8::from(interrupted));
    assert!(
        matches(&expected, &stdout),
        "every expiration still counted\nexpected (pattern):\n{expected}\nstdout:\n{stdout}"
    );
    assert_eq!(
        stderr.contains("a host signal interrupted arming timerfd"),
        interrupted,
        "{backend}: the fault must interrupt the first arming attempt exactly on ptrace\nstderr:\n{stderr}"
    );
}

pub(super) const CATCHUP: &str = "catchup count={n} remaining_ns={n}\ncatchup disarmed ok\n";
pub(super) const ABSTIME: &str = "abstime realtime ok\nabstime monotonic ok\nabstime boottime ok\n";
pub(super) const NONBLOCK: &str = "nonblock ok\n";
pub(super) const EPOLL_PWAIT2: &str = "epoll_pwait2 ok\n";
pub(super) const ZERO_TIMEOUT: &str = "zero-timeout ok\n";
pub(super) const WAITS: &str =
    "waits poll ok\nwaits ppoll ok\nwaits select ok\nwaits pselect6 ok\nwaits epoll_wait ok\n";
pub(super) const MASKED_WAITS: &str =
    "masked-waits epoll_pwait ok\nmasked-waits ppoll ok\nmasked-waits pselect6 ok\n";
pub(super) const MASKED_PPOLL_EXPIRED: &str = "masked-ppoll-expired ok\n";
pub(super) const EPOLLET: &str = "epollet order 1 2 0\nepollet ok\n";
pub(super) const DUP_FORK: &str =
    "dup-fork child count={n}\ndup-fork parent count={n}\ndup-fork ok\n";
pub(super) const EXEC: &str = "exec ok\n";
pub(super) const READV: &str = "readv ok split={n}\n";
pub(super) const SIGCHLD_STORM: &str = "sigchld-storm total={n}\n";
