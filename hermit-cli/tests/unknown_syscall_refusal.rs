/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::fs;
use std::path::Path;
use std::process::Command;

/// A guest syscall whose number the pinned table cannot name (462..=470 on
/// x86-64, past its last row, plus the unwired 999) is trapped to Detcore
/// (`UnknownSyscallPolicy::TrapToTool`) and never runs on the host:
///
/// - by default the guest receives the deterministic `ENOSYS` a kernel
///   without the number returns, for every such number, however new the
///   host kernel that would have implemented it;
/// - under `--strict` the first such call ends the run as a policy refusal
///   (exit 122) naming the number;
/// - the default answer is identical across repeated runs (`--verify-strict`).
#[test]
fn unknown_syscall_numbers_receive_enosys_or_a_strict_refusal() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("unknown-syscall-{}", std::process::id()));
    fs::create_dir_all(&build_root).expect("failed to create unknown-syscall build directory");

    let guest = build_root.join("unknown_syscall_enosys");
    let compile = Command::new("cc")
        .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(repository.join("tests/c/unknown_syscall_enosys.c"))
        .arg("-o")
        .arg(&guest)
        .output()
        .unwrap_or_else(|error| panic!("failed to compile unknown_syscall_enosys.c: {error}"));
    assert!(
        compile.status.success(),
        "failed to compile unknown_syscall_enosys.c\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr),
    );

    let run = |label: &str, hermit_args: &[&str]| {
        let mut command = Command::new("timeout");
        command
            .args(["--kill-after", "5s", "60s"])
            .arg(hermit_test::hermit_binary())
            .args(hermit_args)
            .arg(&guest);
        hermit_test::configure_guest_execution(&mut command);
        hermit_test::hermit_output(label, &mut command)
    };

    // (a) Default: every unknown number answers ENOSYS, and Detcore's log
    // shows the first one arriving as a trapped raw syscall.
    let default = run(
        "unknown-syscall default",
        &[
            "--log=trace",
            "--backend=ptrace",
            "run",
            "--base-env=minimal",
            "--",
        ],
    );
    assert!(
        default.status.success(),
        "default run failed\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        default.status,
        String::from_utf8_lossy(&default.stdout),
        String::from_utf8_lossy(&default.stderr),
    );
    let default_stdout = String::from_utf8_lossy(&default.stdout);
    assert!(
        default_stdout.contains("unknown-syscall-enosys-ok"),
        "default run omitted the success marker:\n{default_stdout}"
    );
    for nr in [462, 463, 464, 465, 466, 467, 468, 469, 470, 999] {
        assert!(
            default_stdout.contains(&format!("nr {nr} ret -1 errno 38")),
            "default run omitted the ENOSYS line for nr {nr}:\n{default_stdout}"
        );
    }
    let default_stderr = String::from_utf8_lossy(&default.stderr);
    assert!(
        default_stderr.contains("inbound syscall: mseal(0x0,"),
        "default run's log omitted the trapped mseal call"
    );

    // (b) Strict: the first unknown number ends the run as a policy
    // refusal naming it, before the guest can report any result.
    let strict = run(
        "unknown-syscall strict",
        &[
            "--log=debug",
            "--backend=ptrace",
            "run",
            "--strict",
            "--base-env=minimal",
            "--",
        ],
    );
    assert_eq!(
        strict.status.code(),
        Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
        "strict run status\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&strict.stdout),
        String::from_utf8_lossy(&strict.stderr),
    );
    let strict_stderr = String::from_utf8_lossy(&strict.stderr);
    assert!(
        strict_stderr.contains("unknown syscall 462 (mseal)"),
        "strict refusal did not name the first unknown number:\n{strict_stderr}"
    );
    assert!(
        !String::from_utf8_lossy(&strict.stdout).contains("unknown-syscall-enosys-ok"),
        "strict run let the guest finish"
    );

    // (c) The default ENOSYS answers are identical across repeated runs.
    let verify = run(
        "unknown-syscall verify",
        &[
            "--log=debug",
            "--backend=ptrace",
            "run",
            "--verify-strict",
            "--base-env=minimal",
            "--",
        ],
    );
    assert!(
        verify.status.success(),
        "verify run failed\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        verify.status,
        String::from_utf8_lossy(&verify.stdout),
        String::from_utf8_lossy(&verify.stderr),
    );
    let combined = [verify.stdout, verify.stderr].concat();
    assert!(
        String::from_utf8_lossy(&combined).contains("Determinism verified"),
        "verify run omitted the determinism marker"
    );
}

fn compile_c_guest(repository: &Path, source: &str, binary: &Path) {
    let compile = Command::new("cc")
        .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(repository.join(source))
        .arg("-o")
        .arg(binary)
        .output()
        .unwrap_or_else(|error| panic!("failed to compile {source}: {error}"));
    assert!(
        compile.status.success(),
        "failed to compile {source}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr),
    );
}

/// How the probe's single call ended natively: killed by a signal, or
/// returned (`ret`, plus the `errno` libc recorded for a `-1` return).
#[derive(Debug)]
enum NativeOutcome {
    Signaled(i32),
    Returned { ret: i64, errno: i32 },
}

fn native_uprobe_gap_outcome(probe: &Path, nr: i32) -> NativeOutcome {
    use std::os::unix::process::ExitStatusExt;

    // Run directly, without the timeout wrapper, so a fatal signal stays a
    // signal on the exit status. The probe child alone takes it.
    let native = Command::new(probe)
        .arg(nr.to_string())
        .output()
        .unwrap_or_else(|error| panic!("failed to run uprobe_gap_probe {nr} natively: {error}"));
    let stdout = String::from_utf8_lossy(&native.stdout);
    assert!(
        stdout.contains(&format!("uprobe-gap-before nr {nr}")),
        "native probe {nr} died before its call\nstatus: {}\nstdout:\n{stdout}\nstderr:\n{}",
        native.status,
        String::from_utf8_lossy(&native.stderr),
    );
    if let Some(signal) = native.status.signal() {
        return NativeOutcome::Signaled(signal);
    }
    let line = stdout
        .lines()
        .find(|line| line.starts_with(&format!("nr {nr} ret ")))
        .unwrap_or_else(|| {
            panic!(
                "native probe {nr} printed no result line\nstatus: {}\nstdout:\n{stdout}\nstderr:\n{}",
                native.status,
                String::from_utf8_lossy(&native.stderr),
            )
        });
    // Shape: "nr <nr> ret <ret> errno <errno> (<strerror>)".
    let fields: Vec<&str> = line.split_whitespace().collect();
    let parse = |index: usize, what: &str| {
        fields
            .get(index)
            .and_then(|field| field.parse::<i64>().ok())
            .unwrap_or_else(|| panic!("native probe {nr}: unparseable {what} in line {line:?}"))
    };
    NativeOutcome::Returned {
        ret: parse(3, "ret"),
        errno: i32::try_from(parse(5, "errno"))
            .unwrap_or_else(|_| panic!("native probe {nr}: errno out of range in line {line:?}")),
    }
}

/// Pinning test for the one gap in the policy above: on kernels that wire
/// 335 (`uretprobe`) and 336 (`uprobe`), entry code handles them before
/// seccomp/ptrace (see `KNOWN_TABLE_GAPS` in
/// `detcore/src/syscall_classification.rs`), so the ptrace backend cannot
/// intercept them, answer `ENOSYS`, or refuse them. The guest keeps the
/// kernel's native outcome under Hermit ptrace.
///
/// This test pins that observed behaviour as a tripwire, not as a goal:
/// it asserts the Hermit outcome equals the native outcome measured on
/// the same host, so it goes red the day a kernel starts letting
/// seccomp/ptrace see these numbers (Hermit would then answer `ENOSYS`,
/// or refuse under `--strict`, like any other unknown number) — which is
/// the moment the gap closes and the docs and classification must change.
///
/// On a kernel that does not wire a number (native outcome `ENOSYS`) the
/// gap cannot be exercised; that number is skipped with a loud line,
/// never silently.
#[test]
fn uprobe_gap_numbers_keep_their_native_outcome_under_ptrace() {
    use std::os::unix::process::ExitStatusExt;

    // 335/336 are x86-64 numbers; other tables wire different calls there.
    if !cfg!(target_arch = "x86_64") {
        eprintln!(
            "SKIP uprobe-gap: 335/336 are x86-64 syscall numbers; \
             nothing to pin on this architecture"
        );
        return;
    }
    const ENOSYS: i32 = 38;

    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("uprobe-gap-{}", std::process::id()));
    fs::create_dir_all(&build_root).expect("failed to create uprobe-gap build directory");
    let probe = build_root.join("uprobe_gap_probe");
    compile_c_guest(repository, "tests/c/uprobe_gap_probe.c", &probe);
    assert!(
        probe.is_file(),
        "compiled guest missing at {}",
        probe.display()
    );

    let run_hermit = |nr: i32| {
        let mut command = Command::new("timeout");
        command
            .args(["--kill-after", "5s", "60s"])
            .arg(hermit_test::hermit_binary())
            .args([
                "--log=debug",
                "--backend=ptrace",
                "run",
                "--base-env=minimal",
                "--",
            ])
            .arg(&probe)
            .arg(nr.to_string());
        hermit_test::configure_guest_execution(&mut command);
        hermit_test::hermit_output("uprobe-gap hermit", &mut command)
    };

    let mut checked = 0;
    let mut skipped = 0;
    for (nr, name) in [(335, "uretprobe"), (336, "uprobe")] {
        let native = native_uprobe_gap_outcome(&probe, nr);
        if let NativeOutcome::Returned {
            ret: -1,
            errno: ENOSYS,
        } = native
        {
            eprintln!(
                "SKIP uprobe-gap nr {nr} ({name}): native outcome is ENOSYS, so this \
                 kernel does not wire the number ahead of seccomp/ptrace and the \
                 ptrace gap cannot be exercised on this host"
            );
            skipped += 1;
            continue;
        }

        let hermit = run_hermit(nr);
        let hermit_stdout = String::from_utf8_lossy(&hermit.stdout);
        let hermit_stderr = String::from_utf8_lossy(&hermit.stderr);
        assert!(
            hermit_stdout.contains(&format!("uprobe-gap-before nr {nr}")),
            "hermit probe {nr} ({name}) never reached its call\nstatus: {}\nstdout:\n{hermit_stdout}\nstderr:\n{hermit_stderr}",
            hermit.status,
        );
        match native {
            NativeOutcome::Signaled(signal) => {
                // The guest died at the call natively (335: SIGILL outside a
                // uretprobe trampoline). Under Hermit it must die the same
                // way: no result line, and the run reports the signal
                // (128 + signal as an exit code, or the signal itself).
                assert!(
                    !hermit_stdout.contains(&format!("nr {nr} ret ")),
                    "hermit probe {nr} ({name}) returned from a call that kills \
                     natively with signal {signal}; the kernel may now let \
                     ptrace intercept it\nstdout:\n{hermit_stdout}\nstderr:\n{hermit_stderr}"
                );
                assert!(
                    hermit.status.code() == Some(128 + signal)
                        || hermit.status.signal() == Some(signal),
                    "hermit probe {nr} ({name}) should report native signal {signal}\nstatus: {}\nstdout:\n{hermit_stdout}\nstderr:\n{hermit_stderr}",
                    hermit.status,
                );
            }
            NativeOutcome::Returned { ret, errno } => {
                assert!(
                    hermit.status.success(),
                    "hermit probe {nr} ({name}) run failed\nstatus: {}\nstdout:\n{hermit_stdout}\nstderr:\n{hermit_stderr}",
                    hermit.status,
                );
                assert!(
                    hermit_stdout.contains(&format!("nr {nr} ret {ret} errno {errno}")),
                    "hermit probe {nr} ({name}) outcome differs from native \
                     (ret {ret} errno {errno}); the kernel may now let ptrace \
                     intercept it\nstdout:\n{hermit_stdout}\nstderr:\n{hermit_stderr}"
                );
            }
        }
        checked += 1;
    }
    eprintln!("uprobe-gap: pinned {checked} number(s) to their native outcome, skipped {skipped}");
}
