/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! End-to-end coverage for the absent `/proc/self/numa_maps`.
//!
//! Hermit presents a kernel built without NUMA
//! (https://github.com/rrnewton/hermit/issues/5269): Linux registers
//! `numa_maps` in a task's directory only under `CONFIG_NUMA`
//! (fs/proc/base.c), so the file does not exist at any `--proc` level —
//! every lookup is ENOENT and no listing names it, whatever the host's
//! procfs holds. (This file used to pin the sanitized contents of the
//! file; its name is kept because the validation selections name the
//! binary.)

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::sync::MutexGuard;

static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());

struct ProgramCase {
    name: &'static str,
    candidates: &'static [&'static str],
    args: &'static [&'static str],
}

fn hermit_run_lock() -> MutexGuard<'static, ()> {
    HERMIT_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn required_program(case: &ProgramCase) -> PathBuf {
    case.candidates
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .unwrap_or_else(|| {
            panic!(
                "required program {} is missing; expected one of {:?}",
                case.name, case.candidates
            )
        })
}

fn assert_l2(case: &ProgramCase) {
    let program = required_program(case);
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after", "10s", "90s"])
        .arg(hermit_test::hermit_binary())
        .args([
            "--log",
            "DEBUG",
            "--backend=ptrace",
            "run",
            "--strict",
            "--proc=standard",
            "--verify",
            "--panic-on-unsupported-syscalls",
            "--base-env=minimal",
            "--",
        ])
        .arg(&program)
        .args(case.args);

    hermit_test::configure_guest_execution(&mut command);
    let rendered = format!("{command:?}");
    let output = hermit_test::hermit_output(&rendered, &mut command);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "{} failed strict verification ({rendered})\nstatus: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        case.name,
        output.status,
    );
    assert!(
        stdout.contains("Determinism verified") || stderr.contains("Determinism verified"),
        "{} omitted Hermit's verification marker ({rendered})\nstdout:\n{stdout}\nstderr:\n{stderr}",
        case.name,
    );
}

/// One plain run: `cat /proc/self/numa_maps` fails, with ENOENT's text,
/// at the standard level (the level that used to list the file).
fn assert_cat_fails_with_enoent() {
    let mut command = Command::new(hermit_test::hermit_binary());
    command.args([
        "--log",
        "ERROR",
        "--backend=ptrace",
        "run",
        "--strict",
        "--proc=standard",
        "--panic-on-unsupported-syscalls",
        "--base-env=minimal",
        "--",
        "/bin/cat",
        "/proc/self/numa_maps",
    ]);
    hermit_test::configure_guest_execution(&mut command);
    let rendered = format!("{command:?}");
    let output = hermit_test::hermit_output(&rendered, &mut command);
    assert!(
        !output.status.success(),
        "numa_maps read succeeded ({rendered})\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("No such file or directory"),
        "numa_maps read did not fail with ENOENT ({rendered}):\n{combined}"
    );
}

#[test]
fn numa_maps_consumers_are_deterministic_under_strict_verify() {
    let _guard = hermit_run_lock();
    assert_cat_fails_with_enoent();

    let cases = [
        ProgramCase {
            name: "numa_maps absent by every spelling",
            candidates: &["/bin/sh", "/usr/bin/sh"],
            args: &[
                "-c",
                "test ! -e /proc/self/numa_maps && test ! -e /proc/thread-self/numa_maps && \
                 test ! -e /proc/$$/numa_maps && test -e /proc/self/smaps",
            ],
        },
        ProgramCase {
            name: "process listing omits numa_maps",
            candidates: &["/bin/sh", "/usr/bin/sh"],
            args: &[
                "-c",
                "! ls /proc/self | grep -qx numa_maps && ls /proc/self | grep -qx smaps && \
                 ls /proc/self | grep -qx smaps_rollup",
            ],
        },
        ProgramCase {
            name: "awk and sed find no numa_maps",
            candidates: &["/bin/sh", "/usr/bin/sh"],
            args: &[
                "-c",
                "if awk '{ print }' /proc/self/numa_maps 2>/dev/null; then exit 1; fi; \
                 if sed -n p /proc/self/numa_maps 2>/dev/null; then exit 1; fi",
            ],
        },
    ];

    for case in &cases {
        assert_l2(case);
    }
}
