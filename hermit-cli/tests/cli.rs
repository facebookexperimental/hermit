/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[path = "common/liteinst.rs"]
mod liteinst_runtime;

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::OnceLock;

static DBI_MMAP_GUEST: OnceLock<PathBuf> = OnceLock::new();
static DBI_EXEC_FAILURE_GUEST: OnceLock<PathBuf> = OnceLock::new();
static DBI_EXECVEAT_GUEST: OnceLock<PathBuf> = OnceLock::new();
static DBI_PID_GUEST: OnceLock<PathBuf> = OnceLock::new();
static DBI_WAIT_GUEST: OnceLock<PathBuf> = OnceLock::new();
static DBI_UNSUPPORTED_SYSCALL_GUEST: OnceLock<PathBuf> = OnceLock::new();
static DBI_SELF_SIGQUEUE_GUEST: OnceLock<PathBuf> = OnceLock::new();
static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());

fn hermit(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("failed to run hermit with {args:?}: {error}"))
}

fn hermit_with_stdin(args: &[&str], input: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("failed to run hermit with {args:?}: {error}"));
    child
        .stdin
        .take()
        .expect("hermit stdin should be piped")
        .write_all(input)
        .expect("failed to write hermit stdin");
    child
        .wait_with_output()
        .unwrap_or_else(|error| panic!("failed to wait for hermit with {args:?}: {error}"))
}

fn dbi_mmap_guest() -> &'static Path {
    DBI_MMAP_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("dbi-mmap");
        fs::create_dir_all(&build_root).expect("failed to create DBI mmap guest directory");
        let guest = build_root.join("dbi_mmap_exec");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/dbi_mmap_exec.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile DBI mmap guest");
        assert!(
            output.status.success(),
            "DBI mmap guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn dbi_exec_failure_guest() -> &'static Path {
    DBI_EXEC_FAILURE_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("dbi-exec-failure");
        fs::create_dir_all(&build_root).expect("failed to create DBI exec-failure guest directory");
        let guest = build_root.join("dbi_exec_failure");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/dbi_exec_failure.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile DBI exec-failure guest");
        assert!(
            output.status.success(),
            "DBI exec-failure guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn dbi_execveat_guest() -> &'static Path {
    DBI_EXECVEAT_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("dbi-execveat");
        fs::create_dir_all(&build_root).expect("failed to create DBI execveat guest directory");
        let guest = build_root.join("dbi_execveat_unsupported");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/dbi_execveat_unsupported.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile DBI execveat guest");
        assert!(
            output.status.success(),
            "DBI execveat guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn dbi_wait_guest() -> &'static Path {
    DBI_WAIT_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("dbi-wait");
        fs::create_dir_all(&build_root).expect("failed to create DBI wait guest directory");
        let guest = build_root.join("dbi_wait_lifecycle");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/dbi_wait_lifecycle.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile DBI wait guest");
        assert!(
            output.status.success(),
            "DBI wait guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

// TODO-HUMAN-REVIEW(PR-723): Review the DBI PID fixture build.
fn dbi_pid_guest() -> &'static Path {
    DBI_PID_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("dbi-pid");
        fs::create_dir_all(&build_root).expect("failed to create DBI PID guest directory");
        let guest = build_root.join("dbi_pid_virtualization");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/dbi_pid_virtualization.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile DBI PID guest");
        assert!(
            output.status.success(),
            "DBI PID guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-644): Review the DBI unsupported-syscall fixture build.
fn dbi_unsupported_syscall_guest() -> &'static Path {
    DBI_UNSUPPORTED_SYSCALL_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("dbi-unsupported-syscall");
        fs::create_dir_all(&build_root)
            .expect("failed to create DBI unsupported-syscall guest directory");
        let guest = build_root.join("dbi_unsupported_syscall");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/dbi_unsupported_syscall.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile DBI unsupported-syscall guest");
        assert!(
            output.status.success(),
            "DBI unsupported-syscall guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

// TODO-HUMAN-REVIEW(PR-1038): Review the DBI self-signal fixture build.
fn dbi_self_sigqueue_guest() -> &'static Path {
    DBI_SELF_SIGQUEUE_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("dbi-self-sigqueue");
        fs::create_dir_all(&build_root)
            .expect("failed to create DBI self-sigqueue guest directory");
        let guest = build_root.join("dbi_self_sigqueue");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/dbi_self_sigqueue.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile DBI self-sigqueue guest");
        assert!(
            output.status.success(),
            "DBI self-sigqueue guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn hermit_with_closed_stdin(args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: pre_exec closes only the child descriptor immediately before exec.
    unsafe {
        command.pre_exec(|| {
            if libc::close(libc::STDIN_FILENO) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
    command
        .output()
        .unwrap_or_else(|error| panic!("failed to run hermit with {args:?}: {error}"))
}

fn assert_success(output: &Output, args: &[&str]) {
    assert!(
        output.status.success(),
        "hermit {args:?} failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("hermit stdout should be UTF-8")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("hermit stderr should be UTF-8")
}

fn assert_failure_contains(output: &Output, expected: &[&str]) {
    assert_eq!(
        output.status.code(),
        Some(1),
        "unexpected status: {output:?}"
    );
    let stderr = stderr(output);
    for message in expected {
        assert!(
            stderr.contains(message),
            "missing {message:?} in:\n{stderr}"
        );
    }
    assert!(!stderr.contains("panicked"), "unexpected panic:\n{stderr}");
}

fn deny_syscall(command: &mut Command, syscall: libc::c_long) {
    // SAFETY: The callback makes only async-signal-safe syscalls before exec. The filter is an
    // allow-all policy except for the single syscall used by each capability-probe test.
    unsafe {
        command.pre_exec(move || {
            let mut filter = [
                libc::sock_filter {
                    code: 0x20, // BPF_LD | BPF_W | BPF_ABS
                    jt: 0,
                    jf: 0,
                    k: 0, // offsetof(seccomp_data, nr)
                },
                libc::sock_filter {
                    code: 0x15, // BPF_JMP | BPF_JEQ | BPF_K
                    jt: 0,
                    jf: 1,
                    k: syscall as u32,
                },
                libc::sock_filter {
                    code: 0x06, // BPF_RET | BPF_K
                    jt: 0,
                    jf: 0,
                    k: 0x0005_0000 | libc::EPERM as u32, // SECCOMP_RET_ERRNO
                },
                libc::sock_filter {
                    code: 0x06,
                    jt: 0,
                    jf: 0,
                    k: 0x7fff_0000, // SECCOMP_RET_ALLOW
                },
            ];
            let program = libc::sock_fprog {
                len: filter.len() as u16,
                filter: filter.as_mut_ptr(),
            };
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::prctl(
                libc::PR_SET_SECCOMP,
                libc::SECCOMP_MODE_FILTER,
                &program as *const libc::sock_fprog,
            ) == -1
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[test]
fn run_strict_flag_is_accepted_and_runs() {
    // Regression test for GH #12: `docs/Users.md` documents
    // `hermit run --strict ...`, and the CLI must accept that spelling and run
    // the guest to completion. Strict determinism is the default, so `--strict`
    // is a compatibility no-op over the defaults. `--max-timeslice=disabled`
    // and `--no-virtualize-cpuid` keep this runnable on hosts without accessible
    // PMU counters or CPUID faulting; neither weakens what `--strict` controls.
    let args = [
        "run",
        "--strict",
        "--max-timeslice=disabled",
        "--no-virtualize-cpuid",
        "--",
        "/bin/true",
    ];
    let output = hermit(&args);
    assert_success(&output, &args);
}

#[test]
fn verify_verbose_requires_verify() {
    let args = ["run", "--verify-verbose", "--", "/bin/true"];
    let output = hermit(&args);

    assert_eq!(output.status.code(), Some(2));
    let stderr = stderr(&output);
    assert!(
        stderr.contains("--verify-verbose"),
        "unexpected error:\n{stderr}"
    );
    assert!(stderr.contains("--verify"), "unexpected error:\n{stderr}");
    assert!(stderr.contains("required"), "unexpected error:\n{stderr}");
}

#[test]
fn run_rejects_unknown_backends_during_argument_parsing() {
    let args = ["run", "--backend", "unknown", "--", "/bin/true"];
    let output = hermit(&args);

    assert_eq!(output.status.code(), Some(2));
    let stderr = stderr(&output);
    assert!(
        stderr.contains("invalid value 'unknown'"),
        "unexpected error:\n{stderr}"
    );
    for backend in ["ptrace", "dbi", "kvm"] {
        assert!(
            stderr.contains(backend),
            "missing {backend:?} in:\n{stderr}"
        );
    }
}

#[test]
fn run_dbi_executes_integrated_backend() {
    let args = ["run", "--backend", "dbi", "--", "/bin/true"];
    let output = hermit(&args);
    assert_success(&output, &args);
}

#[test]
fn run_dbi_uses_the_requested_guest_environment() {
    let args = [
        "run",
        "--backend",
        "dbi",
        "--strict",
        "--base-env=empty",
        "--env=DBI_GUEST_ONLY=present",
        "--",
        "/usr/bin/env",
    ];
    let output = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .env("DBI_HOST_ONLY", "must-not-leak")
        .args(args)
        .output()
        .expect("failed to run DBI environment regression");

    assert_success(&output, &args);
    let stdout = stdout(&output);
    assert!(
        stdout.lines().any(|line| line == "DBI_GUEST_ONLY=present"),
        "DBI guest environment omitted the requested value:\n{stdout}",
    );
    assert!(
        !stdout
            .lines()
            .any(|line| line.starts_with("DBI_HOST_ONLY=")),
        "DBI guest inherited a host-only value:\n{stdout}",
    );
}

#[test]
fn run_dbi_verifies_simple_env_shebang() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create DBI env-shebang test directory");
    let script = directory.path().join("env-echo");
    fs::write(&script, b"#!/usr/bin/env echo\n")
        .expect("failed to write DBI env-shebang test script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755))
        .expect("failed to mark DBI env-shebang test script executable");
    let program = script
        .to_str()
        .expect("DBI env-shebang test path should be UTF-8");
    let args = [
        "run",
        "--backend",
        "dbi",
        "--strict",
        "--verify",
        "--",
        program,
    ];

    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), format!("{}\n", script.display()));
    assert!(
        stderr(&output).contains(":: Success: deterministic. Determinism verified."),
        "DBI determinism confirmation missing:\n{}",
        stderr(&output),
    );
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-644): Review ptrace verification warning delivery.
#[test]
fn run_ptrace_verify_reemits_unsupported_syscall_warning() {
    let program = dbi_unsupported_syscall_guest()
        .to_str()
        .expect("unsupported-syscall guest path should be UTF-8");
    let args = ["--log", "info", "run", "--verify", "--", program];
    let output = hermit(&args);
    assert_success(&output, &args);
    let warning = "syscalls pidfd_getfd used but not yet supported";
    assert_eq!(
        stderr(&output).matches(warning).count(),
        1,
        "verify did not re-emit exactly one aggregate warning:\n{}",
        stderr(&output)
    );
}
// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-644): Review DBI normal aggregation and strict failure coverage.
#[test]
fn run_dbi_aggregates_unsupported_syscalls_and_strict_rejects_them() {
    let program = dbi_unsupported_syscall_guest()
        .to_str()
        .expect("DBI unsupported-syscall guest path should be UTF-8");

    let normal_args = ["run", "--backend", "dbi", "--verify", "--", program];
    let normal = hermit(&normal_args);
    assert_success(&normal, &normal_args);
    assert_eq!(stdout(&normal), "dbi-unsupported-ok\n");
    let normal_stderr = stderr(&normal);
    let warning = "syscalls pidfd_getfd used but not yet supported";
    assert_eq!(
        normal_stderr.matches(warning).count(),
        1,
        "expected one aggregate warning:\n{normal_stderr}"
    );

    let tamper_args = ["run", "--backend", "dbi", "--", program, "report-tamper"];
    let tamper = hermit(&tamper_args);
    assert_success(&tamper, &tamper_args);
    assert_eq!(stdout(&tamper), "dbi-unsupported-report-tamper-ok\n");
    assert_eq!(
        stderr(&tamper).matches(warning).count(),
        1,
        "report tampering suppressed the aggregate warning:\n{}",
        stderr(&tamper)
    );

    let fork_tamper_args = [
        "run",
        "--backend",
        "dbi",
        "--",
        program,
        "fork-report-tamper",
    ];
    let fork_tamper = hermit(&fork_tamper_args);
    assert_success(&fork_tamper, &fork_tamper_args);
    assert_eq!(
        stdout(&fork_tamper),
        "dbi-unsupported-fork-report-tamper-ok\n"
    );
    assert_eq!(
        stderr(&fork_tamper).matches(warning).count(),
        1,
        "fork-child report tampering suppressed the aggregate warning:\n{}",
        stderr(&fork_tamper)
    );

    let strict_args = ["run", "--backend", "dbi", "--strict", "--", program];
    let strict = hermit(&strict_args);
    assert!(
        !strict.status.success(),
        "strict DBI unexpectedly succeeded:\n{}",
        stderr(&strict)
    );
    assert!(
        stderr(&strict).contains("unsupported syscall: pidfd_getfd"),
        "strict DBI failure omitted unsupported syscall:\n{}",
        stderr(&strict)
    );
    let normal_fork_args = ["run", "--backend", "dbi", "--verify", "--", program, "fork"];
    let normal_fork = hermit(&normal_fork_args);
    assert_success(&normal_fork, &normal_fork_args);
    assert_eq!(stdout(&normal_fork), "dbi-unsupported-fork-ok\n");
    assert_eq!(
        stderr(&normal_fork).matches(warning).count(),
        1,
        "fork-child warning was not aggregated exactly once:\n{}",
        stderr(&normal_fork)
    );

    let normal_fork_exec_args = [
        "run",
        "--backend",
        "dbi",
        "--verify",
        "--",
        program,
        "fork-exec",
    ];
    let normal_fork_exec = hermit(&normal_fork_exec_args);
    assert_success(&normal_fork_exec, &normal_fork_exec_args);
    assert_eq!(
        stdout(&normal_fork_exec),
        "dbi-unsupported-exec-ok\ndbi-unsupported-fork-exec-parent-ok\n"
    );
    assert_eq!(
        stderr(&normal_fork_exec).matches(warning).count(),
        1,
        "fork-exec warning was not aggregated exactly once:\n{}",
        stderr(&normal_fork_exec)
    );

    for mode in ["fork", "fork-exec", "fork-setsid-exec", "exec-empty"] {
        let args = ["run", "--backend", "dbi", "--strict", "--", program, mode];
        let output = hermit(&args);
        assert!(
            !output.status.success(),
            "strict DBI {mode} unexpectedly succeeded:\n{}",
            stderr(&output)
        );
        assert!(
            stderr(&output).contains("unsupported syscall"),
            "strict DBI {mode} omitted unsupported-syscall diagnostic:\n{}",
            stderr(&output)
        );
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-644): Review strict DBI teardown with a blocked stdin source.
#[test]
fn run_dbi_strict_returns_with_blocked_stdin_source() {
    let program = dbi_unsupported_syscall_guest()
        .to_str()
        .expect("DBI unsupported-syscall guest path should be UTF-8");
    let mut source = Command::new("sleep")
        .arg("30")
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to start blocked DBI stdin source");
    let args = ["run", "--backend", "dbi", "--strict", "--", program];
    let output = Command::new("timeout")
        .args(["--kill-after", "2s", "10s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(args)
        .stdin(source.stdout.take().expect("sleep stdout was not piped"))
        .output()
        .expect("failed to run strict DBI blocked-input regression");
    let _ = source.kill();
    let _ = source.wait();
    assert_ne!(output.status.code(), Some(124), "strict DBI hung on stdin");
    assert!(
        !output.status.success(),
        "strict DBI unexpectedly succeeded"
    );
    assert!(stderr(&output).contains("unsupported syscall"));
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-736): Review the real LiteInst Detcore CLI assertion.
#[test]
fn run_liteinst_verifies_detcore_backend() {
    liteinst_runtime::ensure_liteinst_runtime();
    let args = [
        "run",
        "--backend",
        "liteinst",
        "--strict",
        "--verify",
        "--",
        "/bin/echo",
        "liteinst-cli-ok",
    ];
    let output = hermit(&args);
    assert_success(&output, &args);
    assert_eq!(stdout(&output), "liteinst-cli-ok\n");
    let stderr = stderr(&output);
    assert!(
        stderr.contains("liteinst backend] Detcore Tool active"),
        "{stderr}"
    );
    assert!(
        stderr.contains("Success: deterministic. Determinism verified."),
        "{stderr}"
    );
    assert!(
        stderr.contains("LiteInst (reverie-liteinst LiteinstGuest<Detcore>)"),
        "{stderr}"
    );
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#679): validate the dedicated DBI diagnostic channel.
#[test]
fn run_dbi_keeps_diagnostics_out_of_guest_stderr() {
    let script = r#"set -euo pipefail; output=$(/bin/sh -c 'printf guest-stderr >&2' 2>&1); test "$output" = guest-stderr; printf 'isolated=%s\n' "$output""#;
    let args = [
        "run",
        "--backend",
        "dbi",
        "--strict",
        "--verify",
        "--",
        "/bin/bash",
        "-c",
        script,
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "isolated=guest-stderr\n");
    assert!(
        stderr(&output).contains(":: DBI path confirmed: DynamoRIO client reported tool=Detcore"),
        "DBI confirmation missing:\n{}",
        stderr(&output),
    );
}

#[test]
fn run_dbi_forwards_detcore_info_logs() {
    let args = [
        "--log",
        "INFO",
        "run",
        "--backend",
        "dbi",
        "--strict",
        "--",
        "/bin/true",
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    let stderr = stderr(&output);
    assert!(
        stderr.contains("INFO detcore") && stderr.contains("DETLOG [syscall]"),
        "DBI did not forward the Detcore INFO syscall stream:\n{stderr}",
    );
}

// TODO-HUMAN-REVIEW(PR-1038): Review DBI queued self-signal verification.
#[test]
fn run_dbi_verifies_queued_self_signals() {
    let program = dbi_self_sigqueue_guest()
        .to_str()
        .expect("DBI self-sigqueue guest path should be UTF-8");
    let args = [
        "run",
        "--backend",
        "dbi",
        "--strict",
        "--verify",
        "--",
        program,
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "dbi-self-sigqueue-ok\n");
    assert!(
        stderr(&output).contains(":: Success: deterministic. Determinism verified."),
        "DBI determinism confirmation missing:\n{}",
        stderr(&output),
    );
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#543): validate the explicit application-mmap DBI regression.
#[test]
fn run_dbi_verifies_application_mmap() {
    let program = dbi_mmap_guest()
        .to_str()
        .expect("DBI mmap guest path should be UTF-8");
    let args = ["run", "--backend", "dbi", "--verify", "--", program];
    let output = hermit(&args);
    assert_success(&output, &args);
    assert_eq!(stdout(&output), "dbi-mmap-exec-ok\n");
    assert!(
        stderr(&output).contains(":: DBI path confirmed: DynamoRIO client reported tool=Detcore"),
        "DBI confirmation missing:\n{}",
        stderr(&output),
    );
}

#[test]
fn run_dbi_verifies_process_wait_lifecycle() {
    let program = dbi_wait_guest()
        .to_str()
        .expect("DBI wait guest path should be UTF-8");
    let args = [
        "run",
        "--backend",
        "dbi",
        "--strict",
        "--verify",
        "--",
        program,
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(
        stdout(&output),
        "wait4=7 waitid=9 sigchld=observed reaped=2 cpu=zero\n"
    );
    assert!(
        stderr(&output).contains(":: Success: deterministic. Determinism verified."),
        "DBI determinism confirmation missing:\n{}",
        stderr(&output),
    );
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-723): Review DBI PID virtualization L2 coverage.
#[test]
fn run_dbi_virtualizes_process_identities() {
    let program = dbi_pid_guest()
        .to_str()
        .expect("DBI PID guest path should be UTF-8");
    let args = [
        "run",
        "--backend",
        "dbi",
        "--strict",
        "--verify",
        "--",
        program,
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(
        stdout(&output),
        concat!(
            "root pid=3 ppid=1 tid=3\n",
            "grandchild pid=5 ppid=4 tid=5\n",
            "child pid=4 ppid=3 tid=4\n",
            "child grandchild=5 waited=5 exit=5\n",
            "root child=4 waited=4 exit=6\n",
            "exec-child pid=6 ppid=3 tid=6\n",
            "exec-proc stat=6/3 status=6/3 tracer=1\n",
            "root exec=6 waited=6 exit=8\n",
            "waitid-child pid=7 ppid=3 tid=7\n",
            "root waitid=7 reported=7 exit=9\n",
            "root vfork=8 waited=8 exit=0 pid=3 tid=3\n",
            "vfork-exec-child pid=9 ppid=3 tid=9\n",
            "root vfork-exec=9 waited=9 exit=10 pid=3 tid=3\n",
        )
    );
    assert!(
        stderr(&output).contains(":: Success: deterministic. Determinism verified."),
        "DBI determinism confirmation missing:\n{}",
        stderr(&output),
    );
}

#[test]
fn run_dbi_verifies_shell_process_lifecycle() {
    let args = [
        "run",
        "--backend",
        "dbi",
        "--verify",
        "--",
        "/bin/sh",
        "-c",
        "/bin/echo hello; :",
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "hello\n");
    assert!(
        stderr(&output).contains(":: Success: deterministic. Determinism verified."),
        "DBI determinism confirmation missing:\n{}",
        stderr(&output),
    );
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#598): Confirm this captures the host-inherited O_NONBLOCK regression.
// TODO-HUMAN-REVIEW(#689): Confirm the split-write case protects partial-read semantics.
#[test]
fn run_dbi_verifies_pipe_backpressure() {
    let args = [
        "run",
        "--backend",
        "dbi",
        "--verify",
        "--",
        "/bin/bash",
        "-c",
        r#"{ printf "%4096s" x; for _ in {1..100000}; do :; done; printf "%1371s" y; } | wc -c"#,
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "5467\n");
    assert!(
        stderr(&output).contains(":: Success: deterministic. Determinism verified."),
        "DBI determinism confirmation missing:\n{}",
        stderr(&output),
    );
}

#[test]
fn run_dbi_recovers_after_failed_exec() {
    let program = dbi_exec_failure_guest()
        .to_str()
        .expect("DBI exec-failure guest path should be UTF-8");
    let args = [
        "run",
        "--backend",
        "dbi",
        "--strict",
        "--verify",
        "--",
        program,
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "recovered after failed exec\n");
    assert!(
        stderr(&output).contains(":: Success: deterministic. Determinism verified."),
        "DBI determinism confirmation missing:\n{}",
        stderr(&output),
    );
}
#[test]
fn run_dbi_rejects_unfollowed_execveat() {
    let program = dbi_execveat_guest()
        .to_str()
        .expect("DBI execveat guest path should be UTF-8");
    let args = [
        "run",
        "--backend",
        "dbi",
        "--strict",
        "--verify",
        "--",
        program,
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(
        stdout(&output),
        "execveat unsupported in root and fork child\n"
    );
    assert!(
        stderr(&output).contains(":: Success: deterministic. Determinism verified."),
        "DBI determinism confirmation missing:\n{}",
        stderr(&output),
    );
}

#[test]
fn run_kvm_executes_dynamic_guest() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--",
        "/bin/echo",
        "hello",
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "hello\n");
    assert!(
        !stderr(&output).contains("Hermit cannot use ptrace"),
        "kvm must not fall through to the ptrace backend:\n{}",
        stderr(&output),
    );
}

#[test]
fn run_kvm_awk_mincore_probe_terminates() {
    if !Path::new("/dev/kvm").exists() || !Path::new("/usr/bin/awk").exists() {
        return;
    }

    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--",
        "/usr/bin/awk",
        "BEGIN { print 42 }",
    ];
    let output = Command::new("timeout")
        .args(["--kill-after", "2s", "20s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(args)
        .output()
        .expect("failed to run the KVM awk mincore regression");

    assert_ne!(
        output.status.code(),
        Some(124),
        "KVM awk mincore probe hung"
    );
    assert_success(&output, &args);
    assert_eq!(stdout(&output), "42\n");
    assert!(
        stderr(&output).contains("Success: KVM guest output and exit status matched."),
        "KVM determinism confirmation missing:\n{}",
        stderr(&output),
    );
}

#[test]
fn run_kvm_resolves_bare_program_from_guest_path() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--base-env=minimal",
        "--",
        "echo",
        "from-kvm-path",
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "from-kvm-path\n");
}

#[test]
fn run_kvm_propagates_explicit_environment() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--base-env=empty",
        "--env=KVM_M3C=passed",
        "--",
        "/usr/bin/env",
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "KVM_M3C=passed\n");
}

#[test]
fn run_kvm_bash_process_substitution_is_deterministic() {
    if !Path::new("/dev/kvm").exists()
        || !Path::new("/bin/bash").exists()
        || !Path::new("/usr/bin/paste").exists()
        || !Path::new("/usr/bin/diff").exists()
    {
        return;
    }

    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--base-env=minimal",
        "--",
        "/bin/bash",
        "-c",
        r#"set -euo pipefail; /usr/bin/paste -d: <(printf "alpha\nbeta\n") <(printf "1\n2\n") | /usr/bin/diff -u <(printf "alpha:1\nbeta:2\n") -; printf "paste-ok\n""#,
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "paste-ok\n");
    assert!(stderr(&output).contains("Success: KVM guest output and exit status matched."));
}

#[test]
fn run_kvm_cpuid_policy_is_deterministic() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }
    let compiler = ["cc", "gcc", "clang"]
        .into_iter()
        .find(|program| {
            Command::new(program)
                .args(["-x", "c", "-fsyntax-only", "-"])
                .stdin(Stdio::null())
                .output()
                .is_ok_and(|output| output.status.success())
        })
        .expect("KVM CPUID regression requires cc, gcc, or clang on PATH");
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("kvm-cpuid");
    fs::create_dir_all(&build_root).expect("failed to create KVM CPUID guest directory");
    let binary = build_root.join("cpuid_probe");
    let compile = Command::new(compiler)
        .args(["-O2", "-g", "-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(repository.join("tests/backend-parity/fixtures/cpuid_probe.c"))
        .arg("-o")
        .arg(&binary)
        .output()
        .expect("failed to compile KVM CPUID guest");
    assert!(
        compile.status.success(),
        "KVM CPUID guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr),
    );

    let program = binary.to_str().expect("CPUID guest path should be UTF-8");
    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--base-env=minimal",
        "--",
        program,
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(
        stdout(&output),
        "CPUID-SUCCESS vendor=GenuineIntel signature=00000663\n"
    );
    assert!(stderr(&output).contains("Success: KVM guest output and exit status matched."));
}

#[test]
fn run_kvm_respects_workdir_for_relative_paths() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    let temp = tempfile::tempdir().expect("failed to create KVM cwd fixture");
    fs::write(temp.path().join("message.txt"), b"from-kvm-cwd\n")
        .expect("failed to write KVM cwd fixture");
    let workdir = temp
        .path()
        .to_str()
        .expect("temporary path should be UTF-8");
    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--tmp=/tmp",
        "--workdir",
        workdir,
        "--",
        "/bin/cat",
        "message.txt",
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "from-kvm-cwd\n");
}

#[test]
fn run_kvm_lists_host_directory_metadata() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    let temp = tempfile::tempdir().expect("failed to create KVM directory fixture");
    fs::write(temp.path().join("alpha.txt"), b"alpha\n")
        .expect("failed to write KVM directory fixture");
    fs::create_dir(temp.path().join("subdir")).expect("failed to create KVM subdirectory");
    std::os::unix::fs::symlink("alpha.txt", temp.path().join("alpha-link"))
        .expect("failed to create KVM symlink fixture");
    let workdir = temp
        .path()
        .to_str()
        .expect("temporary path should be UTF-8");
    let args = [
        "run",
        "--backend",
        "kvm",
        "--verify",
        "--base-env=minimal",
        "--tmp=/tmp",
        "--workdir",
        workdir,
        "--",
        "/bin/ls",
        "-ln",
        ".",
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    let listing = stdout(&output);
    let alpha = listing
        .lines()
        .find(|line| line.ends_with(" alpha.txt") && !line.contains(" -> "))
        .unwrap_or_else(|| panic!("missing file in:\n{listing}"));
    let alpha_fields: Vec<_> = alpha.split_whitespace().collect();
    assert!(alpha_fields[0].starts_with("-rw"), "bad file mode: {alpha}");
    assert_eq!(alpha_fields[4], "6", "bad file size: {alpha}");
    let subdir = listing
        .lines()
        .find(|line| line.ends_with(" subdir"))
        .unwrap_or_else(|| panic!("missing directory in:\n{listing}"));
    assert!(subdir.starts_with("d"), "bad directory type: {subdir}");
    let link = listing
        .lines()
        .find(|line| line.ends_with(" alpha-link -> alpha.txt"))
        .unwrap_or_else(|| panic!("missing symlink in:\n{listing}"));
    assert!(link.starts_with("l"), "bad symlink type: {link}");
}

#[test]
fn run_kvm_reads_host_file() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    let expected = fs::read_to_string("/etc/hostname").expect("failed to read host hostname");
    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--base-env=minimal",
        "--",
        "/bin/cat",
        "/etc/hostname",
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), expected);
}

#[test]
fn run_kvm_reads_standard_input() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--base-env=minimal",
        "--",
        "/bin/cat",
    ];
    let output = hermit_with_stdin(&args, b"hello\n");

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "hello\n");
}

#[test]
fn run_kvm_f_getfl_and_reads_standard_input() {
    if !Path::new("/dev/kvm").exists() || !Path::new("/usr/bin/perl").exists() {
        return;
    }

    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--base-env=minimal",
        "--",
        "/usr/bin/perl",
        "-MFcntl=F_GETFL",
        "-e",
        r#"defined(fcntl(STDIN, F_GETFL, 0)) or die "fcntl failed: $!\n"; my $line = <STDIN>; defined($line) && $line eq "hello\n" or die "stdin mismatch\n"; print "fcntl-stdin-ok\n";"#,
    ];
    let output = hermit_with_stdin(&args, b"hello\n");

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "fcntl-stdin-ok\n");
}

#[test]
fn run_kvm_verify_f_getfl_with_isolated_standard_input() {
    if !Path::new("/dev/kvm").exists() || !Path::new("/usr/bin/perl").exists() {
        return;
    }

    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--base-env=minimal",
        "--",
        "/usr/bin/perl",
        "-MFcntl=F_GETFL",
        "-e",
        r#"defined(fcntl(STDIN, F_GETFL, 0)) or die "fcntl failed: $!\n"; my $line = <STDIN>; !defined($line) or die "verify stdin was not isolated\n"; print "fcntl-verify-ok\n";"#,
    ];
    let output = hermit_with_stdin(&args, b"not-visible-during-capture\n");

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "fcntl-verify-ok\n");
    assert!(stderr(&output).contains("Success: KVM guest output and exit status matched."));
}

#[test]
fn run_kvm_verify_isolates_standard_input() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--base-env=minimal",
        "--",
        "/bin/cat",
    ];
    let output = hermit_with_stdin(&args, b"not-visible-during-capture\n");

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "");
}

#[test]
fn run_kvm_preserves_closed_standard_input() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--base-env=minimal",
        "--",
        "/bin/cat",
    ];
    let output = hermit_with_closed_stdin(&args);

    assert_eq!(
        output.status.code(),
        Some(1),
        "unexpected output: {output:?}"
    );
    assert_eq!(stdout(&output), "");
    assert!(
        stderr(&output)
            .to_ascii_lowercase()
            .contains("bad file descriptor")
    );
}

#[test]
fn run_kvm_verify_does_not_write_to_standard_input() {
    if !Path::new("/dev/kvm").exists() || !Path::new("/usr/bin/perl").exists() {
        return;
    }

    let temp = tempfile::tempdir().expect("failed to create stdin fixture");
    let path = temp.path().join("stdin");
    fs::write(&path, b"original-data").expect("failed to write stdin fixture");
    let stdin = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .expect("failed to open stdin fixture");
    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--base-env=minimal",
        "--",
        "/usr/bin/perl",
        "-MPOSIX",
        "-e",
        "POSIX::write(0, \"leak\", 4); exit 0",
    ];
    let output = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args(args)
        .stdin(Stdio::from(stdin))
        .output()
        .unwrap_or_else(|error| panic!("failed to run hermit with {args:?}: {error}"));

    assert_success(&output, &args);
    assert_eq!(fs::read(path).unwrap(), b"original-data");
}

#[test]
fn run_kvm_counts_standard_input() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--base-env=minimal",
        "--",
        "/usr/bin/wc",
    ];
    let output = hermit_with_stdin(&args, b"hello\n");

    assert_success(&output, &args);
    assert_eq!(
        stdout(&output).split_whitespace().collect::<Vec<_>>(),
        ["1", "1", "6"]
    );
}

#[test]
fn run_kvm_reports_hostname() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--base-env=minimal",
        "--",
        "/bin/hostname",
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "hermetic-container.local\n");
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#544): Confirm the host C compiler is acceptable for this KVM smoke guest.
#[test]
fn run_kvm_pipe_pipe2_and_getgroups_round_trip() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }
    let compiler = ["cc", "gcc", "clang"]
        .into_iter()
        .find(|program| {
            Command::new(program)
                .args(["-x", "c", "-fsyntax-only", "-"])
                .stdin(Stdio::null())
                .output()
                .is_ok_and(|output| output.status.success())
        })
        .expect("KVM syscall regression requires cc, gcc, or clang on PATH");

    let temp = tempfile::tempdir().expect("failed to create pipe guest directory");
    let source = temp.path().join("pipe_roundtrip.c");
    let binary = temp.path().join("pipe_roundtrip");
    fs::write(
        &source,
        br#"#define _GNU_SOURCE
#include <fcntl.h>
#include <grp.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static int roundtrip(int flags) {
    int fds[2];
    char buffer[3] = {0};
    int result = flags < 0 ? pipe(fds) : pipe2(fds, flags);
    if (result != 0) return 1;
    if (write(fds[1], "ok", 2) != 2) return 2;
    if (read(fds[0], buffer, 2) != 2) return 3;
    if (close(fds[0]) != 0 || close(fds[1]) != 0) return 4;
    return strcmp(buffer, "ok") != 0;
}

int main(void) {
    gid_t groups[1] = {0};
    if (roundtrip(-1) || roundtrip(O_CLOEXEC | O_NONBLOCK)) return 1;
    if (getgroups(0, NULL) != 1) return 5;
    if (getgroups(1, groups) != 1 || groups[0] != 65534) return 6;
    puts("kvm-syscalls-ok");
    return 0;
}
"#,
    )
    .expect("failed to write pipe guest");
    let compile = Command::new(compiler)
        .args(["-O2", "-Wall", "-Wextra", "-Werror", "-o"])
        .arg(&binary)
        .arg(&source)
        .output()
        .expect("failed to invoke C compiler");
    assert!(
        compile.status.success(),
        "failed to compile pipe guest: {}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let program = binary.to_str().expect("pipe guest path should be UTF-8");
    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--tmp=/tmp",
        "--base-env=minimal",
        "--",
        program,
    ];
    let output = hermit(&args);
    assert_success(&output, &args);
    assert_eq!(stdout(&output), "kvm-syscalls-ok\n");
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#544): Confirm 65534 remains the fixed container overflow group.
#[test]
fn run_kvm_reports_fixed_supplementary_groups() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    let kvm_args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--base-env=minimal",
        "--",
        "id",
        "-G",
    ];
    let kvm_output = hermit(&kvm_args);
    assert_success(&kvm_output, &kvm_args);
    assert_eq!(
        stdout(&kvm_output),
        "0 65534\n",
        "KVM must report its root-plus-overflow-group credential persona"
    );
}

#[test]
fn namespace_only_rejects_every_explicit_backend() {
    for backend in ["ptrace", "dbi", "kvm"] {
        let args = [
            "run",
            "--backend",
            backend,
            "--namespace-only",
            "--",
            "/bin/true",
        ];
        let output = hermit(&args);
        assert_eq!(output.status.code(), Some(2));
        let message = stderr(&output);
        assert!(
            message.contains("--backend"),
            "unexpected error:\n{message}"
        );
        assert!(
            message.contains("--namespace-only"),
            "unexpected error:\n{message}"
        );
    }
}

#[test]
fn backend_accepted_in_global_position() {
    // The global-position `--backend` (before the subcommand) must be threaded
    // through to `run` and reach the integrated DBI backend.
    let dbi_args = ["--backend", "dbi", "run", "--", "/bin/true"];
    let dbi = hermit(&dbi_args);

    assert_success(&dbi, &dbi_args);

    if Path::new("/dev/kvm").exists() {
        let args = ["--backend", "kvm", "run", "--", "/bin/true"];
        let kvm = hermit(&args);
        assert_success(&kvm, &args);
        assert!(
            !stderr(&kvm).contains("Hermit cannot use ptrace"),
            "global-position kvm should reach its dispatch:\n{}",
            stderr(&kvm),
        );
    }
}

#[test]
fn sabre_backend_validation_honors_command_scope() {
    let non_run = hermit(&["--backend", "sabre", "record", "list"]);
    assert_failure_contains(&non_run, &["SaBRe backend", "only through", "strace"]);

    let local_override = hermit(&[
        "--backend",
        "sabre",
        "run",
        "--backend",
        "ptrace",
        "--",
        "/definitely/missing/sabre-backend-override-test",
    ]);
    assert_failure_contains(&local_override, &["does not exist or is not accessible"]);
    assert!(!stderr(&local_override).contains("SaBRe backend"));

    let log = hermit(&[
        "--backend",
        "sabre",
        "--log",
        "info",
        "strace",
        "--",
        "/bin/true",
    ]);
    assert_failure_contains(&log, &["does not support --log or --log-file"]);
}

#[test]
fn sabre_rpc_socket_is_hidden_from_proc_environ() {
    let hermit_binary = Path::new(env!("CARGO_BIN_EXE_hermit"));
    let executable_dir = hermit_binary.parent().unwrap();
    let target_dir = executable_dir.parent().unwrap();
    let loader = target_dir.join("sabre/sabre");
    let plugin = executable_dir.join("libdetcore_sabre.so");
    if !loader.is_file() || !plugin.is_file() {
        return;
    }

    let _guard = HERMIT_RUN_LOCK.lock().unwrap();
    let args = [
        "run",
        "--backend",
        "sabre",
        "--strict",
        "--verify",
        "--base-env=minimal",
        "--",
        "/usr/bin/cat",
        "/proc/self/environ",
    ];
    let output = hermit(&args);
    assert_success(&output, &args);

    let guest_environment = stdout(&output);
    assert!(
        !guest_environment.contains("REVERIE_SABRE_HERMIT_RPC_SOCKET"),
        "private coordinator setting leaked through procfs: {guest_environment:?}"
    );
    assert!(
        stderr(&output).contains("Determinism verified"),
        "strict repeat verification did not complete:\n{}",
        stderr(&output)
    );
}

#[test]
fn global_position_rejects_unknown_backends() {
    let args = ["--backend", "unknown", "run", "--", "/bin/true"];
    let output = hermit(&args);
    assert_eq!(output.status.code(), Some(2));
    let stderr = stderr(&output);
    assert!(
        stderr.contains("invalid value 'unknown'"),
        "unexpected error:\n{stderr}"
    );
}

#[test]
fn namespace_only_rejects_global_position_backend() {
    let args = [
        "--backend",
        "ptrace",
        "run",
        "--namespace-only",
        "--",
        "/bin/true",
    ];
    let output = hermit(&args);
    let message = stderr(&output);
    assert!(
        message.contains("--backend"),
        "unexpected error:\n{message}"
    );
    assert!(
        message.contains("--namespace-only"),
        "unexpected error:\n{message}"
    );
}

#[test]
fn incompatible_run_modes_fail_during_argument_parsing() {
    let args = ["run", "--namespace-only", "--chaos", "/bin/true"];
    let output = hermit(&args);

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).expect("hermit stderr should be UTF-8");
    assert!(
        stderr.contains("--namespace-only"),
        "unexpected error:\n{stderr}"
    );
    assert!(stderr.contains("--chaos"), "unexpected error:\n{stderr}");
    assert!(
        stderr.contains("cannot be used with"),
        "unexpected error:\n{stderr}"
    );
}

#[test]
fn no_namespace_rejects_container_only_options() {
    let cases = [
        "--namespace-only",
        "--analyze-networking",
        "--mount=type=bind,source=/tmp,target=/tmp",
        "--bind=/tmp",
        "--network=local",
        "--network=host",
        "--tmp=/tmp/custom",
        "--replay-schedule-from=/tmp/schedule.json",
        "--replay-preemptions-from=/tmp/preemptions.json",
    ];

    for incompatible in cases {
        let args = ["run", "--no-namespace", incompatible, "/bin/true"];
        let output = hermit(&args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "hermit {args:?} unexpectedly ran"
        );

        let stderr = String::from_utf8(output.stderr).expect("hermit stderr should be UTF-8");
        assert!(
            stderr.contains("--no-namespace"),
            "unexpected error:\n{stderr}"
        );
        assert!(
            stderr.contains(incompatible.split_once("=").map_or(incompatible, |x| x.0)),
            "unexpected error:\n{stderr}"
        );
        assert!(
            stderr.contains("cannot be used with"),
            "unexpected error:\n{stderr}"
        );
    }
}

#[test]
fn no_namespace_runs_without_container_setup() {
    let _guard = HERMIT_RUN_LOCK.lock().unwrap();
    let args = [
        "run",
        "--no-namespace",
        "--max-timeslice=disabled",
        "--",
        "/bin/echo",
        "hello",
    ];
    let output = hermit(&args);
    assert_success(&output, &args);

    assert_eq!(stdout(&output), "hello\n");
    let stderr = String::from_utf8(output.stderr).expect("hermit stderr should be UTF-8");
    assert!(
        stderr.contains("WARNING: --no-namespace"),
        "unexpected stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("less deterministic"),
        "unexpected stderr:\n{stderr}"
    );
}

#[test]
fn no_namespace_preserves_affinity_for_run_and_verify() {
    let _guard = HERMIT_RUN_LOCK.lock().unwrap();

    let run_args = [
        "run",
        "--no-namespace",
        "--pin-threads",
        "--max-timeslice=disabled",
        "--",
        "/usr/bin/nproc",
    ];
    let output = hermit(&run_args);
    assert_success(&output, &run_args);
    assert_eq!(stdout(&output), "1\n");

    let verify_args = [
        "run",
        "--no-namespace",
        "--verify",
        "--pin-threads",
        "--max-timeslice=disabled",
        "--",
        "/bin/sh",
        "-c",
        "test $(nproc) -eq 1",
    ];
    let output = hermit(&verify_args);
    assert_success(&output, &verify_args);
}

#[test]
fn record_list_json_reports_an_empty_inventory() {
    let data_dir = tempfile::tempdir().expect("failed to create recording data directory");
    let output = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args(["record", "list", "--json", "--data-dir"])
        .arg(data_dir.path())
        .output()
        .expect("failed to run hermit record list");
    assert!(
        output.status.success(),
        "hermit record list failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("record list should emit JSON");
    assert_eq!(value, serde_json::json!([]));
}

#[test]
fn run_rejects_invalid_programs_with_actionable_errors() {
    let output = hermit(&["run", "--", "/definitely/missing/hermit-program"]);
    assert_failure_contains(
        &output,
        &["does not exist or is not accessible", "Check the path"],
    );

    let output = hermit(&["run", "--", "definitely-missing-hermit-program"]);
    assert_failure_contains(&output, &["Could not resolve program", "guest PATH"]);

    let temp = tempfile::tempdir().expect("failed to create program fixture directory");
    let non_executable = temp.path().join("non-executable");
    fs::write(&non_executable, "#!/bin/sh\nexit 0\n").expect("failed to write program fixture");

    let output = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args(["run", "--tmp=/tmp", "--"])
        .arg(&non_executable)
        .output()
        .expect("failed to run hermit");
    assert_failure_contains(&output, &["is not executable", "chmod +x"]);

    let output = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args(["run", "--tmp=/tmp", "--"])
        .arg(temp.path())
        .output()
        .expect("failed to run hermit");
    assert_failure_contains(&output, &["is a directory", "executable file"]);

    let bad_shebang = temp.path().join("bad-shebang");
    fs::write(&bad_shebang, "#!/definitely/missing/interpreter\n").expect("failed to write script");
    let mut permissions = fs::metadata(&bad_shebang)
        .expect("failed to stat script")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&bad_shebang, permissions).expect("failed to make script executable");

    let output = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args(["run", "--tmp=/tmp", "--"])
        .arg(&bad_shebang)
        .output()
        .expect("failed to run hermit");
    assert_failure_contains(
        &output,
        &["uses shebang interpreter", "does not exist", "#! line"],
    );
}

#[test]
fn run_rejects_invalid_configuration_without_panicking() {
    let output = hermit(&["run", "--no-virtualize-time", "--", "/bin/true"]);
    assert_failure_contains(
        &output,
        &["also requires --no-virtualize-metadata", "timestamps"],
    );

    let output = hermit(&["run", "--sched-sticky-random-param=-0.1", "--", "/bin/true"]);
    assert_failure_contains(&output, &["must be between 0 and 1", "received -0.1"]);
}

#[test]
fn run_rejects_a_missing_bind_source_before_mounting() {
    let output = hermit(&[
        "run",
        "--bind=/definitely/missing/hermit-test:/tmp/input",
        "--",
        "/bin/true",
    ]);
    assert_failure_contains(&output, &["--bind source", "does not exist", "correct"]);

    let output = hermit(&[
        "run",
        "--mount=type=bind,source=/definitely/missing/hermit-test,target=/tmp/input",
        "--",
        "/bin/true",
    ]);
    assert_failure_contains(&output, &["--mount source", "does not exist", "correct"]);
}

#[test]
fn run_reports_denied_ptrace_and_seccomp_capabilities() {
    for (syscall, expected) in [
        (
            libc::SYS_ptrace,
            ["cannot use ptrace", "PTRACE_TRACEME", "--namespace-only"],
        ),
        (
            libc::SYS_seccomp,
            [
                "cannot install",
                "SECCOMP_SET_MODE_FILTER",
                "--namespace-only",
            ],
        ),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
        command.args([
            "run",
            "--max-timeslice=disabled",
            "--no-virtualize-cpuid",
            "--",
            "/bin/true",
        ]);
        deny_syscall(&mut command, syscall);
        let output = command.output().expect("failed to run restricted hermit");
        assert_failure_contains(&output, &expected);
    }
}
