/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::process::Output;
use std::sync::Mutex;

static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());

fn hermit(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args(args)
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
fn top_level_help_lists_user_facing_commands() {
    let args = ["--help"];
    let output = hermit(&args);
    assert_success(&output, &args);
    let help = stdout(&output);

    assert!(help.contains("Usage: hermit [OPTIONS] <COMMAND>"));
    for command in ["run", "record", "replay", "log-diff", "analyze", "bisect"] {
        assert!(help.contains(command), "missing {command:?} in:\n{help}");
    }
}

#[test]
fn bisect_help_describes_schedule_endpoints() {
    let args = ["bisect", "--help"];
    let output = hermit(&args);
    assert_success(&output, &args);
    let help = stdout(&output);

    assert!(help.contains("--good <SCHEDULE>"));
    assert!(help.contains("--bad <SCHEDULE>"));
    assert!(help.contains("--target-exit-code"));
    assert!(help.contains("--report-file"));
    assert!(help.contains("<RUN_ARGS>..."));
}

#[test]
fn replay_help_accepts_optional_recording_id() {
    let args = ["replay", "--help"];
    let output = hermit(&args);
    assert_success(&output, &args);
    let help = stdout(&output);

    assert!(help.contains("Usage: hermit replay [OPTIONS] [ID]"));
    assert!(help.contains("--autopilot"));
    assert!(help.contains("--data-dir <DIR>"));
    assert!(help.contains("--gdbserver-port"));
}

#[test]
fn run_help_exposes_determinism_modes() {
    let args = ["run", "--help"];
    let output = hermit(&args);
    assert_success(&output, &args);
    let help = stdout(&output);

    for option in [
        "--strict",
        "--sequentialize-threads",
        "--chaos",
        "--verify",
        "--verify-verbose",
        "--record-preemptions",
        "--replay-preemptions-from",
        "--preemption-timeout",
        "--backend <BACKEND>",
        "ptrace",
        "dbi",
        "kvm",
        "Bare names are resolved using the guest PATH",
        "hidden by Hermit's isolated `/tmp`",
        "without ptrace, seccomp interception, or determinization",
        "--no-namespace",
        "--core-only",
    ] {
        assert!(help.contains(option), "missing {option:?} in run help");
    }
}

#[test]
fn run_strict_flag_is_accepted_and_runs() {
    // Regression test for GH #12: `docs/Users.md` documents
    // `hermit run --strict ...`, and the CLI must accept that spelling and run
    // the guest to completion. Strict determinism is the default, so `--strict`
    // is a compatibility no-op over the defaults. `--preemption-timeout=disabled`
    // and `--no-virtualize-cpuid` keep this runnable on hosts without accessible
    // PMU counters or CPUID faulting; neither weakens what `--strict` controls.
    let args = [
        "run",
        "--strict",
        "--preemption-timeout=disabled",
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
fn run_fails_closed_for_unintegrated_backends() {
    // DBI has no Hermit dispatch, so it must fail closed at the availability
    // probe. (KVM is now integrated; see `run_kvm_reaches_backend_then_fails_execution`.)
    let output = hermit(&["run", "--backend", "dbi", "--", "/bin/true"]);
    assert_failure_contains(&output, &["backend `dbi` is unavailable"]);
    assert!(
        !stderr(&output).contains("Hermit cannot use ptrace"),
        "dbi should fail before ptrace capability probing"
    );
}

#[test]
fn run_kvm_reaches_backend_then_fails_execution() {
    // The KVM backend is wired into hermit-cli via reverie-kvm, but reverie-kvm
    // does not yet implement a Linux execution personality, so an explicit
    // `--backend kvm` reaches the backend dispatch and then fails with an
    // accurate, program-specific error instead of the generic availability
    // message used for unintegrated backends. The exact wording depends on
    // whether /dev/kvm is accessible on the test host, so assert on the stable
    // prefix common to both outcomes.
    let args = ["run", "--backend", "kvm", "--", "/bin/true"];
    let output = hermit(&args);

    assert_failure_contains(&output, &["the KVM backend cannot run"]);
    let stderr = stderr(&output);
    assert!(
        !stderr.contains("is unavailable"),
        "kvm should reach its dispatch, not the generic availability probe:\n{stderr}"
    );
    assert!(
        !stderr.contains("Hermit cannot use ptrace"),
        "kvm must not fall through to the ptrace backend:\n{stderr}"
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
    // through to `run`. Probe with the unintegrated DBI backend so the value's
    // effect is observable without depending on ptrace capabilities being
    // available.
    let dbi = hermit(&["--backend", "dbi", "run", "--", "/bin/true"]);
    assert_failure_contains(&dbi, &["backend `dbi` is unavailable"]);

    // KVM is integrated (via reverie-kvm) but has no Linux execution personality
    // yet, so the global-position value must still reach the KVM dispatch and
    // produce its accurate, program-specific error.
    let kvm = hermit(&["--backend", "kvm", "run", "--", "/bin/true"]);
    assert_failure_contains(&kvm, &["the KVM backend cannot run"]);
    assert!(
        !stderr(&kvm).contains("is unavailable"),
        "global-position kvm should reach its dispatch:\n{}",
        stderr(&kvm)
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
        "--preemption-timeout=disabled",
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
        "--preemption-timeout=disabled",
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
        "--preemption-timeout=disabled",
        "--",
        "/bin/sh",
        "-c",
        "test $(nproc) -eq 1",
    ];
    let output = hermit(&verify_args);
    assert_success(&output, &verify_args);
}

#[test]
fn record_help_lists_management_commands() {
    let args = ["record", "--help"];
    let output = hermit(&args);
    assert_success(&output, &args);
    let help = stdout(&output);

    assert!(help.contains("Usage: hermit record <COMMAND>"));
    for command in ["list", "rm", "clean", "start"] {
        assert!(help.contains(command), "missing {command:?} in:\n{help}");
    }
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
            "--preemption-timeout=disabled",
            "--no-virtualize-cpuid",
            "--",
            "/bin/true",
        ]);
        deny_syscall(&mut command, syscall);
        let output = command.output().expect("failed to run restricted hermit");
        assert_failure_contains(&output, &expected);
    }
}
