/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! End-to-end coverage for host interrupt totals in /sys/kernel/irq/*/per_cpu_count.

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::sync::MutexGuard;

static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());

struct ProgramCase {
    name: &'static str,
    candidates: &'static [&'static str],
    args: Vec<String>,
}

fn hermit_run_lock() -> MutexGuard<'static, ()> {
    HERMIT_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn first_active_irq_count_path() -> Option<(PathBuf, usize)> {
    let mut paths = fs::read_dir("/sys/kernel/irq")
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|irq| !irq.is_empty() && irq.bytes().all(|byte| byte.is_ascii_digit()))
        })
        .map(|entry| entry.path().join("per_cpu_count"))
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    paths.sort();

    paths.into_iter().find_map(|path| {
        let contents = fs::read_to_string(&path).ok()?;
        let fields = contents.trim_end().split(',').collect::<Vec<_>>();
        let values = fields
            .iter()
            .map(|field| field.parse::<u64>())
            .collect::<Result<Vec<_>, _>>()
            .ok()?;
        values
            .iter()
            .any(|value| *value > 0)
            .then_some((path, fields.len()))
    })
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
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log",
            "DEBUG",
            "run",
            "--backend=ptrace",
            "--strict",
            "--verify",
            "--panic-on-unsupported-syscalls",
            "--base-env=minimal",
            "--",
        ])
        .arg(&program)
        .args(&case.args);

    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"));
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

fn read_irq_counts(path: &Path) -> String {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command
        .args([
            "--log",
            "ERROR",
            "run",
            "--backend=ptrace",
            "--strict",
            "--panic-on-unsupported-syscalls",
            "--base-env=minimal",
            "--",
            "/bin/cat",
        ])
        .arg(path);
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"));
    assert!(
        output.status.success(),
        "IRQ count read failed ({rendered})\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout).expect("per_cpu_count should be UTF-8")
}

#[test]
fn irq_per_cpu_count_consumers_are_deterministic_under_strict_verify() {
    let Some((path, field_count)) = first_active_irq_count_path() else {
        return;
    };
    let _guard = hermit_run_lock();
    let contents = read_irq_counts(&path);
    let fields = contents.trim_end().split(',').collect::<Vec<_>>();
    assert_eq!(
        fields.len(),
        field_count,
        "per_cpu_count changed CPU column count"
    );
    assert!(
        fields.iter().all(|field| *field == "0"),
        "per_cpu_count retained host interrupt totals: {contents:?}"
    );

    let path = path.display().to_string();
    let cases = [
        ProgramCase {
            name: "awk per-CPU IRQ counts",
            candidates: &["/usr/bin/awk", "/bin/awk"],
            args: vec!["{ print }".to_owned(), path.clone()],
        },
        ProgramCase {
            name: "sed per-CPU IRQ counts",
            candidates: &["/bin/sed", "/usr/bin/sed"],
            args: vec!["-n".to_owned(), "p".to_owned(), path.clone()],
        },
        ProgramCase {
            name: "grep per-CPU IRQ counts",
            candidates: &["/usr/bin/grep", "/bin/grep"],
            args: vec![".".to_owned(), path],
        },
    ];

    for case in &cases {
        assert_l2(case);
    }
}
