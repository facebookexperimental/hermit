/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! End-to-end L2 coverage for standard command-line tools that are expected on
//! the self-hosted CI runner.

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::MutexGuard;

static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());

const HERMIT_VERIFY_TIMEOUT: &str = "60s";
const HERMIT_VERIFY_KILL_AFTER: &str = "10s";

struct StrictCommandCase {
    name: &'static str,
    candidates: &'static [&'static str],
    args: &'static [&'static str],
    stdin: Option<&'static [u8]>,
}

fn hermit_run_lock() -> MutexGuard<'static, ()> {
    HERMIT_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn required_command(case: &StrictCommandCase) -> PathBuf {
    case.candidates
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .unwrap_or_else(|| {
            panic!(
                "ERROR: required command {} is missing; expected one of {:?}",
                case.name, case.candidates
            )
        })
}

fn assert_l2_under_strict_verify(case: &StrictCommandCase) {
    let program = required_command(case);
    let mut command = Command::new("timeout");
    command
        .args([
            "--kill-after",
            HERMIT_VERIFY_KILL_AFTER,
            HERMIT_VERIFY_TIMEOUT,
        ])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=off", "run", "--strict", "--verify", "--"])
        .arg(&program)
        .args(case.args)
        .stdin(if case.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let rendered = format!("{command:?}");
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"));
    if let Some(input) = case.stdin {
        child
            .stdin
            .take()
            .expect("piped stdin should be available")
            .write_all(input)
            .unwrap_or_else(|error| panic!("failed to write stdin for {rendered}: {error}"));
    }
    let output = child
        .wait_with_output()
        .unwrap_or_else(|error| panic!("failed to collect {rendered}: {error}"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "{} did not reach L2 under strict verification ({rendered})\n\
         status: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        case.name,
        output.status,
    );
    assert!(
        stderr.contains("Determinism verified") || stdout.contains("Determinism verified"),
        "{} exited 0 without Hermit's determinism marker ({rendered})\n\
         stdout:\n{stdout}\nstderr:\n{stderr}",
        case.name,
    );
}

#[test]
#[ignore = "e2e: requires hermit + PMU/mount namespaces + standard Unix command tools"]
fn common_commands_are_deterministic_under_strict_verify() {
    let _guard = hermit_run_lock();
    let cases = [
        StrictCommandCase {
            name: "cat",
            candidates: &["/usr/bin/cat", "/bin/cat"],
            args: &["/etc/hostname"],
            stdin: None,
        },
        StrictCommandCase {
            name: "wc",
            candidates: &["/usr/bin/wc", "/bin/wc"],
            args: &["-l", "/etc/passwd"],
            stdin: None,
        },
        StrictCommandCase {
            name: "head",
            candidates: &["/usr/bin/head", "/bin/head"],
            args: &["-n", "3", "/etc/passwd"],
            stdin: None,
        },
        StrictCommandCase {
            name: "sort",
            candidates: &["/usr/bin/sort", "/bin/sort"],
            args: &[],
            stdin: Some(b"gamma\nalpha\nbeta\n"),
        },
        StrictCommandCase {
            name: "env",
            candidates: &["/usr/bin/env", "/bin/env"],
            args: &["-i", "HERMIT_COMMAND_COMPAT=1"],
            stdin: None,
        },
        StrictCommandCase {
            name: "date",
            candidates: &["/usr/bin/date", "/bin/date"],
            args: &["-u", "+%s"],
            stdin: None,
        },
        StrictCommandCase {
            name: "id",
            candidates: &["/usr/bin/id", "/bin/id"],
            args: &["-u"],
            stdin: None,
        },
        StrictCommandCase {
            name: "hostname",
            candidates: &["/usr/bin/hostname", "/bin/hostname"],
            args: &[],
            stdin: None,
        },
        StrictCommandCase {
            name: "uname",
            candidates: &["/usr/bin/uname", "/bin/uname"],
            args: &["-a"],
            stdin: None,
        },
        StrictCommandCase {
            name: "tr",
            candidates: &["/usr/bin/tr", "/bin/tr"],
            args: &["a-z", "A-Z"],
            stdin: Some(b"hello hermit\n"),
        },
        StrictCommandCase {
            name: "cut",
            candidates: &["/usr/bin/cut", "/bin/cut"],
            args: &["-d:", "-f1", "/etc/passwd"],
            stdin: None,
        },
        StrictCommandCase {
            name: "tee",
            candidates: &["/usr/bin/tee", "/bin/tee"],
            args: &["/dev/null"],
            stdin: Some(b"tee-through-hermit\n"),
        },
        StrictCommandCase {
            name: "diff",
            candidates: &["/usr/bin/diff", "/bin/diff"],
            args: &["/etc/hostname", "/etc/hostname"],
            stdin: None,
        },
        StrictCommandCase {
            name: "grep",
            candidates: &["/usr/bin/grep", "/bin/grep"],
            args: &["-m", "1", "root", "/etc/passwd"],
            stdin: None,
        },
        StrictCommandCase {
            name: "sed",
            candidates: &["/usr/bin/sed", "/bin/sed"],
            args: &["-n", "1,3p", "/etc/passwd"],
            stdin: None,
        },
        StrictCommandCase {
            name: "find",
            candidates: &["/usr/bin/find", "/bin/find"],
            args: &[
                "/etc",
                "-maxdepth",
                "1",
                "-type",
                "f",
                "-name",
                "hostname",
                "-print",
            ],
            stdin: None,
        },
        StrictCommandCase {
            name: "xargs",
            candidates: &["/usr/bin/xargs", "/bin/xargs"],
            args: &["echo"],
            stdin: Some(b"one two three\n"),
        },
        StrictCommandCase {
            name: "basename",
            candidates: &["/usr/bin/basename", "/bin/basename"],
            args: &["/tmp/hermit-example.txt", ".txt"],
            stdin: None,
        },
        StrictCommandCase {
            name: "dirname",
            candidates: &["/usr/bin/dirname", "/bin/dirname"],
            args: &["/tmp/hermit-example.txt"],
            stdin: None,
        },
        StrictCommandCase {
            name: "realpath",
            candidates: &["/usr/bin/realpath", "/bin/realpath"],
            args: &["/etc/../etc/passwd"],
            stdin: None,
        },
        StrictCommandCase {
            name: "md5sum",
            candidates: &["/usr/bin/md5sum", "/bin/md5sum"],
            args: &["/etc/hostname"],
            stdin: None,
        },
        StrictCommandCase {
            name: "sha256sum",
            candidates: &["/usr/bin/sha256sum", "/bin/sha256sum"],
            args: &["/etc/hostname"],
            stdin: None,
        },
        StrictCommandCase {
            name: "du",
            candidates: &["/usr/bin/du", "/bin/du"],
            args: &["-b", "/etc/hostname"],
            stdin: None,
        },
        StrictCommandCase {
            name: "sqlite3",
            candidates: &["/usr/bin/sqlite3", "/usr/local/bin/sqlite3"],
            args: &[
                ":memory:",
                "CREATE TABLE t(v); INSERT INTO t VALUES(3),(1),(2); \
                 SELECT group_concat(v, ',') FROM (SELECT v FROM t ORDER BY v);",
            ],
            stdin: None,
        },
        StrictCommandCase {
            name: "awk",
            candidates: &["/usr/bin/awk", "/bin/awk"],
            args: &["BEGIN { for (i = 1; i <= 10; ++i) sum += i; print sum }"],
            stdin: None,
        },
        StrictCommandCase {
            name: "perl",
            candidates: &["/usr/bin/perl", "/bin/perl"],
            args: &["-e", "print join(',', map { $_ * $_ } 1..5), qq(\n)"],
            stdin: None,
        },
    ];

    for case in &cases {
        assert_l2_under_strict_verify(case);
    }
}

#[test]
#[ignore = "e2e: requires hermit + PMU/mount namespaces + /usr/bin/python3"]
fn python_prlimit64_query_is_deterministic_under_strict_verify() {
    let _guard = hermit_run_lock();
    let case = StrictCommandCase {
        name: "python3 prlimit64 query",
        candidates: &["/usr/bin/python3"],
        args: &[],
        stdin: None,
    };
    let python = required_command(&case);
    let query = "import resource; print(resource.getrlimit(resource.RLIMIT_NOFILE))";

    let strict_output = Command::new("timeout")
        .args([
            "--kill-after",
            HERMIT_VERIFY_KILL_AFTER,
            HERMIT_VERIFY_TIMEOUT,
        ])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=off", "run", "--strict", "--"])
        .arg(&python)
        .args(["-c", query])
        .output()
        .expect("failed to start Python prlimit64 strict-mode value regression");
    let strict_stdout = String::from_utf8_lossy(&strict_output.stdout);
    let strict_stderr = String::from_utf8_lossy(&strict_output.stderr);
    assert!(
        strict_output.status.success(),
        "Python prlimit64 strict-mode query failed: {}\n\
         stdout:\n{strict_stdout}\nstderr:\n{strict_stderr}",
        strict_output.status
    );
    assert_eq!(
        strict_stdout.trim(),
        "(1048576, 1048576)",
        "Python observed a non-deterministic RLIMIT_NOFILE value"
    );

    let output = Command::new("timeout")
        .args([
            "--kill-after",
            HERMIT_VERIFY_KILL_AFTER,
            HERMIT_VERIFY_TIMEOUT,
        ])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=off", "run", "--strict", "--verify", "--"])
        .arg(&python)
        .args(["-c", query])
        .output()
        .expect("failed to start Python prlimit64 strict/verify regression");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "Python prlimit64 query did not reach L2 under strict verification: {}\n\
         stdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
    assert!(
        stderr.contains("Determinism verified") || stdout.contains("Determinism verified"),
        "Hermit exited 0 without its determinism marker\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[test]
#[ignore = "e2e: requires hermit + PMU/mount namespaces + /usr/bin/python3"]
fn python_getrandom_is_deterministic_under_strict_verify() {
    let _guard = hermit_run_lock();
    let case = StrictCommandCase {
        name: "python3 getrandom",
        candidates: &["/usr/bin/python3"],
        args: &["-c", "import os; print(os.urandom(16).hex())"],
        stdin: None,
    };
    assert_l2_under_strict_verify(&case);
}
