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

const STDOUT_LINK: &str = "/proc/self/fd/1";

/// The line Hermit prints when it runs the guest under in-guest LiteInst.
#[cfg(feature = "liteinst")]
const IN_GUEST_SELECTED: &str =
    "hermit: [liteinst in-guest] selected: the guest preload is to host the Detcore Tool";

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
        .arg(hermit_test::hermit_binary())
        .args(["--log", "DEBUG", "--backend=ptrace", "run", "--strict"]);
    if verify {
        command.arg("--verify");
    }
    command
        .arg("--")
        .arg(required_program(case))
        .args(case.args);
    hermit_test::configure_guest_execution(&mut command);

    let rendered = format!("{command:?}");
    hermit_test::hermit_output(&rendered, &mut command)
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
fn proc_fd_link_consumers_verify() {
    let cases = [
        ProgramCase {
            name: "readlink stdout pipe",
            candidates: &["/usr/bin/readlink", "/bin/readlink"],
            args: &[STDOUT_LINK],
        },
        ProgramCase {
            name: "ls stdout pipe",
            candidates: &["/usr/bin/ls", "/bin/ls"],
            args: &["-l", STDOUT_LINK],
        },
        ProgramCase {
            name: "stat stdout pipe",
            candidates: &["/usr/bin/stat", "/bin/stat"],
            args: &["-c", "%N", STDOUT_LINK],
        },
    ];

    let initial = hermit_output(&cases[0], false);
    assert!(
        initial.status.success(),
        "failed to read normalized stdout link: {}",
        String::from_utf8_lossy(&initial.stderr)
    );
    let target = std::str::from_utf8(&initial.stdout)
        .expect("stdout link should be UTF-8")
        .trim();
    let inode = target
        .strip_prefix("pipe:[")
        .and_then(|value| value.strip_suffix(']'))
        .expect("stdout link should retain the Linux pipe shape");
    assert!(
        !inode.is_empty() && inode.bytes().all(|byte| byte.is_ascii_digit()),
        "stdout link should contain a deterministic decimal inode: {target}"
    );

    for case in &cases {
        assert_l2(case);
    }
}

#[test]
fn proc_fd_link_aliases_and_truncation_verify() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("proc-fd-link-aliases");
    fs::create_dir_all(&build_root).expect("failed to create proc-fd link build directory");
    let guest = build_root.join("proc-fd-link-aliases");
    let compile = Command::new("cc")
        .args(["-O2", "-std=gnu11", "-Wall", "-Wextra", "-Werror"])
        .arg(repository.join("tests/c/proc_fd_link_aliases.c"))
        .arg("-o")
        .arg(&guest)
        .output()
        .expect("failed to start C compiler");
    assert!(
        compile.status.success(),
        "failed to compile proc-fd link guest:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let case = ProgramCase {
        name: "proc-fd link aliases",
        candidates: &[],
        args: &[],
    };
    // The guest reads the links of a file it creates, not of its stdout pipe:
    // c7531a837a65 ("Stabilize proc-fd alias parity fixture", 2026-08-01)
    // changed it so that harness capture names could not become guest output,
    // and left this expectation naming the old pipe target.
    let expected = concat!(
        "canonical=/tmp/hermit-proc-fd-link-aliases\n",
        "truncated=/tmp/hermi\n",
        "numeric=/tmp/hermit-proc-fd-link-aliases\n",
        "dev-fd=/tmp/hermit-proc-fd-link-aliases\n",
        "lexical=/tmp/hermit-proc-fd-link-aliases\n",
        "readlinkat=/tmp/hermit-proc-fd-link-aliases\n",
    );
    for backend in ["ptrace", "dbt"] {
        let mut command = Command::new("timeout");
        command
            .args(["--kill-after", "10s", "90s"])
            .arg(hermit_test::hermit_binary())
            .args(["--log", "DEBUG"])
            .arg(format!("--backend={backend}"))
            .arg("run")
            .args(["--strict", "--verify", "--"])
            .arg(&guest);
        hermit_test::configure_guest_execution(&mut command);
        let output = hermit_test::hermit_output(&format!("{command:?}"), &mut command);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "{} failed {backend} strict verification\nstdout:\n{stdout}\nstderr:\n{stderr}",
            case.name
        );
        assert!(
            stdout.contains("Determinism verified") || stderr.contains("Determinism verified"),
            "{} omitted {backend} verification marker\nstdout:\n{stdout}\nstderr:\n{stderr}",
            case.name
        );
        assert_eq!(stdout, expected, "{} differed on {backend}", case.name);
    }

    // In-guest LiteInst (https://github.com/rrnewton/hermit/issues/3520) runs
    // the same guest and must print the same aliases. It refuses a maximum
    // timeslice until it can deliver Detcore's preemption timer, so this run
    // has none, and it does not pass `--verify` yet.
    #[cfg(feature = "liteinst")]
    {
        let mut command = Command::new("timeout");
        command
            .args(["--kill-after", "10s", "90s"])
            .arg(hermit_test::hermit_binary())
            .args(["--log", "DEBUG", "--backend=liteinst", "run"])
            .args(["--strict", "--max-timeslice=disabled", "--"])
            .arg(&guest);
        hermit_test::configure_guest_execution(&mut command);
        let output = hermit_test::hermit_output(&format!("{command:?}"), &mut command);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "{} failed under in-guest LiteInst\nstdout:\n{stdout}\nstderr:\n{stderr}",
            case.name
        );
        assert!(
            stderr.lines().any(|line| line == IN_GUEST_SELECTED),
            "{} did not run under in-guest LiteInst\nstderr:\n{stderr}",
            case.name
        );
        assert_eq!(
            stdout, expected,
            "{} differed on in-guest LiteInst",
            case.name
        );
    }
}
