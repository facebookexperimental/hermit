/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::PathBuf;
use std::process::Command;

const RANDOM_UUID: &str = "/proc/sys/kernel/random/uuid";

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
fn random_uuid_consumers_verify() {
    let cases = [
        ProgramCase {
            name: "cat random UUID",
            candidates: &["/usr/bin/cat", "/bin/cat"],
            args: &[RANDOM_UUID],
        },
        ProgramCase {
            name: "awk random UUID",
            candidates: &["/usr/bin/awk", "/bin/awk"],
            args: &["{print $1}", RANDOM_UUID],
        },
        ProgramCase {
            name: "sed random UUID",
            candidates: &["/usr/bin/sed", "/bin/sed"],
            args: &["-n", "s/-/:/gp", RANDOM_UUID],
        },
    ];

    let two_reads = ProgramCase {
        name: "two random UUID reads",
        candidates: &["/usr/bin/cat", "/bin/cat"],
        args: &[RANDOM_UUID, RANDOM_UUID],
    };
    let initial = hermit_output(&two_reads, false);
    assert!(
        initial.status.success(),
        "failed to read a deterministic random UUID: {}",
        String::from_utf8_lossy(&initial.stderr)
    );
    let text = std::str::from_utf8(&initial.stdout).expect("random UUIDs should be UTF-8");
    let uuids = text.lines().collect::<Vec<_>>();
    assert_eq!(uuids.len(), 2);
    assert_ne!(uuids[0], uuids[1], "separate reads should remain unique");
    for uuid in uuids {
        assert_eq!(uuid.len(), 36);
        assert_eq!(uuid.as_bytes()[14], b'4');
        assert!(matches!(uuid.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));
    }

    for case in &cases {
        assert_l2(case);
    }
}
