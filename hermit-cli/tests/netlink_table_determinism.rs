/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::PathBuf;
use std::process::Command;

const NETLINK_TABLE: &str = "/proc/net/netlink";

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

fn hermit_output(case: &ProgramCase, verify: bool) -> std::process::Output {
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after", "10s", "90s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log", "DEBUG", "run", "--strict"]);
    if verify {
        command.arg("--verify");
    }
    command
        .arg("--")
        .arg(required_program(case))
        .args(case.args);

    let rendered = format!("{command:?}");
    command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"))
}

fn assert_l2(case: &ProgramCase) {
    let output = hermit_output(case, true);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "{} failed strict verification\nstatus: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        case.name,
        output.status,
    );
    assert!(
        stdout.contains("Determinism verified") || stderr.contains("Determinism verified"),
        "{} omitted Hermit's verification marker\nstdout:\n{stdout}\nstderr:\n{stderr}",
        case.name,
    );
}

#[test]
fn netlink_table_consumers_verify() {
    let cases = [
        ProgramCase {
            name: "cat netlink table",
            candidates: &["/usr/bin/cat", "/bin/cat"],
            args: &[NETLINK_TABLE],
        },
        ProgramCase {
            name: "awk netlink identities",
            candidates: &["/usr/bin/awk", "/bin/awk"],
            args: &["NR > 1 {print $1, $10}", NETLINK_TABLE],
        },
        ProgramCase {
            name: "sed netlink rows",
            candidates: &["/usr/bin/sed", "/bin/sed"],
            args: &["-n", "2,4p", NETLINK_TABLE],
        },
    ];

    let initial = hermit_output(&cases[0], false);
    assert!(
        initial.status.success(),
        "failed to read normalized netlink table: {}",
        String::from_utf8_lossy(&initial.stderr)
    );
    let table = std::str::from_utf8(&initial.stdout).expect("netlink table should be UTF-8");
    let mut lines = table.lines();
    assert_eq!(
        lines.next(),
        Some(
            "sk               Eth Pid        Groups   Rmem     Wmem     Dump  Locks    Drops    Inode"
        )
    );
    let rows = lines.collect::<Vec<_>>();
    assert!(
        !rows.is_empty(),
        "netlink table should contain kernel sockets"
    );
    for row in &rows {
        let fields = row.split_whitespace().collect::<Vec<_>>();
        assert_eq!(fields.len(), 10);
        assert_eq!(fields[0], "0000000000000000");
        assert_eq!(fields[9], "0");
    }

    for case in &cases {
        assert_l2(case);
    }
}
