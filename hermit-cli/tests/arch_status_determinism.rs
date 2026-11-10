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
use std::path::PathBuf;
use std::process::Command;

struct ProgramCase {
    name: &'static str,
    candidates: &'static [&'static str],
    args: &'static [&'static str],
}

fn required_program(case: &ProgramCase) -> PathBuf {
    case.candidates
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("required program {} is missing", case.name))
}

fn assert_l2(case: &ProgramCase) {
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after", "5s", "90s"])
        .arg(hermit_test::hermit_binary())
        .args([
            "--log",
            "DEBUG",
            "--backend=ptrace",
            "run",
            "--strict",
            "--proc=standard",
            "--verify",
            "--base-env=minimal",
            "--",
        ])
        .arg(required_program(case))
        .args(case.args);
    hermit_test::configure_guest_execution(&mut command);
    let output = hermit_test::hermit_output(&format!("{command:?}"), &mut command);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "{} failed strict verification\nstdout:\n{stdout}\nstderr:\n{stderr}",
        case.name
    );
    assert!(
        stdout.contains("Determinism verified") || stderr.contains("Determinism verified"),
        "{} omitted verification marker\nstdout:\n{stdout}\nstderr:\n{stderr}",
        case.name
    );
}

/// Whether Hermit's ptrace backend answers the guest's CPUID instruction from
/// Detcore's table on this host, as `hermit host-capabilities --json` reports
/// it: ptrace intercepts the instruction only through CPUID faulting
/// (`arch_prctl(ARCH_SET_CPUID, 0)`). An answer that cannot be read stops the
/// test by name: the expectation below depends on it.
fn ptrace_intercepts_cpuid() -> bool {
    let mut command = Command::new(hermit_test::hermit_binary());
    command.args(["host-capabilities", "--json"]);
    let output = hermit_test::hermit_output(&format!("{command:?}"), &mut command);
    assert!(
        output.status.success(),
        "hermit host-capabilities --json failed, so whether the guest sees the virtual CPU \
         is unknown: {output:?}"
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)
        .expect("hermit host-capabilities --json printed no JSON");
    report["host_capabilities"]["cpuid-faulting"]["present"]
        .as_bool()
        .unwrap_or_else(|| {
            panic!(
                "hermit host-capabilities --json reports no cpuid-faulting.present, so whether \
                 the guest sees the virtual CPU is unknown: {report}"
            )
        })
}

/// The guest's `arch_status` names the CPU the guest's CPUID instruction
/// reports, as Linux's `proc_pid_arch_status` names the CPU it runs on
/// (it prints its line only for a CPU with AVX-512F). Where Hermit answers
/// that instruction from Detcore's table (on ptrace, where the host has CPUID
/// faulting), the guest's CPU is Hermit's virtual one, which has no AVX-512F,
/// so the file is empty whatever the host's CPU is. Where it cannot, the guest
/// sees the host's CPU and the host's file, its value sanitized:
/// `AVX512_elapsed_ms:\t0` on a host whose CPU has AVX-512F, nothing on
/// another. The test derives which from the host, and checks it; its
/// consumers then pass strict verification either way. A host kernel with no
/// arch_status file at all (before Linux 5.4, or not x86-64) is named as not
/// applicable.
#[test]
fn arch_status_consumers_are_deterministic_under_strict_verify() {
    let test = "arch_status_consumers_are_deterministic_under_strict_verify";
    let path = Path::new("/proc/self/arch_status");
    let Ok(host_contents) = fs::read_to_string(path) else {
        eprintln!(
            "{test}: NOT APPLICABLE: this host's kernel has no readable /proc/self/arch_status"
        );
        return;
    };
    let host_avx512 = host_contents.contains("AVX512_elapsed_ms:");
    let intercepts = ptrace_intercepts_cpuid();
    let expected = if intercepts || !host_avx512 {
        ""
    } else {
        "AVX512_elapsed_ms:\t0\n"
    };
    eprintln!(
        "{test}: the host's CPU has AVX-512F: {host_avx512}; ptrace answers CPUID from \
         Detcore's table: {intercepts}; the guest's arch_status must be {expected:?}"
    );

    let cat = ProgramCase {
        name: "cat",
        candidates: &["/usr/bin/cat", "/bin/cat"],
        args: &["/proc/self/arch_status"],
    };
    let mut command = Command::new(hermit_test::hermit_binary());
    command
        .args([
            "--log=ERROR",
            "--backend=ptrace",
            "run",
            "--strict",
            "--proc=standard",
            "--base-env=minimal",
            "--",
        ])
        .arg(required_program(&cat))
        .args(cat.args);
    hermit_test::configure_guest_execution(&mut command);
    let snapshot = hermit_test::hermit_output(&format!("{command:?}"), &mut command);
    assert!(snapshot.status.success());
    let text = String::from_utf8(snapshot.stdout).expect("arch_status should be UTF-8");
    assert_eq!(
        text, expected,
        "the guest's arch_status must name the CPU its CPUID instruction reports"
    );

    for case in [
        cat,
        ProgramCase {
            name: "awk",
            candidates: &["/usr/bin/awk", "/bin/awk"],
            args: &[
                "-F",
                "\t",
                "/^AVX512_elapsed_ms:/ { print $2 }",
                "/proc/self/arch_status",
            ],
        },
        ProgramCase {
            name: "sed",
            candidates: &["/usr/bin/sed", "/bin/sed"],
            args: &[
                "-n",
                "s/^AVX512_elapsed_ms:[[:space:]]*//p",
                "/proc/self/arch_status",
            ],
        },
    ] {
        assert_l2(&case);
    }
}
