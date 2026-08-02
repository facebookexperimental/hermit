/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

#[path = "common/liteinst.rs"]
mod liteinst_runtime;

const STDOUT_LINK: &str = "/proc/self/fd/1";

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
        .args(["--log", "DEBUG", "run", "--backend=ptrace", "--strict"]);
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
    liteinst_runtime::ensure_liteinst_runtime();
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
    let expected = concat!(
        "canonical=pipe:[1001]\n",
        "truncated=pipe:[1001\n",
        "numeric=pipe:[1001]\n",
        "dev-fd=pipe:[1001]\n",
        "lexical=pipe:[1001]\n",
        "readlinkat=pipe:[1001]\n",
    );
    for backend in ["ptrace", "dbi", "liteinst"] {
        let output = Command::new("timeout")
            .args(["--kill-after", "10s", "90s"])
            .arg(env!("CARGO_BIN_EXE_hermit"))
            .args(["--log", "DEBUG", "run"])
            .arg(format!("--backend={backend}"))
            .args(["--strict", "--verify", "--"])
            .arg(&guest)
            .output()
            .expect("failed to start Hermit");
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
}
