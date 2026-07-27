/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

const STATS_ROOT: &str = "/sys/kernel/mm/transparent_hugepage/hugepages-2048kB/stats";
const ANON_FAULT_ALLOC: &str =
    "/sys/kernel/mm/transparent_hugepage/hugepages-2048kB/stats/anon_fault_alloc";
const NR_ANON: &str = "/sys/kernel/mm/transparent_hugepage/hugepages-2048kB/stats/nr_anon";
const SPLIT_DEFERRED: &str =
    "/sys/kernel/mm/transparent_hugepage/hugepages-2048kB/stats/split_deferred";

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
        .unwrap_or_else(|| panic!("{} requires one of {:?}", case.name, case.candidates))
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
            "--strict",
            "--verify",
            "--no-virtualize-cpuid",
            "--max-timeslice=disabled",
            "--",
        ])
        .arg(program)
        .args(case.args);

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

#[test]
fn transparent_hugepage_stat_consumers_verify() {
    if !Path::new(STATS_ROOT).is_dir() {
        eprintln!("skipping: this host does not expose per-size THP statistics");
        return;
    }

    let cases = [
        ProgramCase {
            name: "cat THP allocation count",
            candidates: &["/usr/bin/cat", "/bin/cat"],
            args: &[ANON_FAULT_ALLOC],
        },
        ProgramCase {
            name: "awk deferred THP split count",
            candidates: &["/usr/bin/awk", "/bin/awk"],
            args: &["{print $1}", SPLIT_DEFERRED],
        },
        ProgramCase {
            name: "sed anonymous THP gauge",
            candidates: &["/usr/bin/sed", "/bin/sed"],
            args: &["-n", "1p", NR_ANON],
        },
    ];

    for case in &cases {
        if !Path::new(case.args.last().expect("case must include a counter path")).is_file() {
            eprintln!("skipping {}: counter path is unavailable", case.name);
            continue;
        }
        assert_l2(case);
    }
}
