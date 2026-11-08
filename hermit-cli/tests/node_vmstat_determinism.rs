/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! End-to-end coverage for the absent NUMA node directory in sysfs.
//!
//! Hermit presents a kernel built without NUMA
//! (https://github.com/rrnewton/hermit/issues/5269): Linux registers no
//! node subsystem then, so `/sys/devices/system/node` and `/sys/bus/node`
//! do not exist at all — not even as empty directories — and no lookup
//! below them finds anything, whatever the host's sysfs holds. (This file
//! used to pin the zeroed counters of the one virtual node's `vmstat`;
//! its name is kept because the validation selections name the binary.)

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::sync::MutexGuard;

static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());
const NODE_VMSTAT: &str = "/sys/devices/system/node/node0/vmstat";

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
            "--backend",
            "ptrace",
            "run",
            "--strict",
            "--verify",
            "--verify-logs",
            "--panic-on-unsupported-syscalls",
            "--base-env",
            "minimal",
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

#[test]
fn node_vmstat_consumers_are_deterministic_under_strict_verify() {
    let _guard = hermit_run_lock();
    assert!(
        Path::new(NODE_VMSTAT).is_file(),
        "{NODE_VMSTAT} is required for the portable regression"
    );

    let cases = [
        ProgramCase {
            name: "node directories absent",
            candidates: &["/bin/sh", "/usr/bin/sh"],
            args: &[
                "-c",
                "test ! -e /sys/devices/system/node && test ! -e /sys/bus/node && \
                 test ! -e /sys/devices/system/node/node0/vmstat && \
                 test ! -e /sys/bus/node/devices/node0",
            ],
        },
        ProgramCase {
            name: "system listing omits node",
            candidates: &["/bin/sh", "/usr/bin/sh"],
            args: &[
                "-c",
                "! ls /sys/devices/system | grep -qx node && \
                 ls /sys/devices/system | grep -qx cpu && \
                 ! ls /sys/bus | grep -qx node && ls /sys/bus | grep -qx cpu",
            ],
        },
        ProgramCase {
            name: "cat node vmstat fails",
            candidates: &["/bin/sh", "/usr/bin/sh"],
            args: &[
                "-c",
                "if cat /sys/devices/system/node/node0/vmstat 2>/dev/null; then exit 1; fi; \
                 if cat /sys/devices/system/node/node0/numastat 2>/dev/null; then exit 1; fi",
            ],
        },
    ];

    for case in &cases {
        assert_l2(case);
    }
}
