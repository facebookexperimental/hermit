/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fmt;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

const GUEST_FIXTURE: &str = "/tmp/integration-matrix";
const DEFAULT_TIMEOUT_SECONDS: u64 = 30;
const EXPECTED_FAIL_TIMEOUT_SECONDS: u64 = 5;

#[derive(Clone, Copy, Debug)]
enum Expectation {
    Pass,
    ExpectedFail,
}

struct Case {
    category: &'static str,
    name: &'static str,
    program: Option<PathBuf>,
    args: Vec<String>,
    marker: Option<&'static str>,
    expectation: Expectation,
    required: bool,
    timeout_seconds: u64,
}

struct Fixture {
    _tempdir: tempfile::TempDir,
    root: PathBuf,
    java_ready: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct Observation {
    exit_code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

struct TimedRun {
    observation: Observation,
    elapsed: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MatrixStatus {
    Pass,
    ExpectedFail,
    Skip,
    Fail,
    UnexpectedPass,
}

impl MatrixStatus {
    fn is_failure(self) -> bool {
        matches!(self, Self::Fail | Self::UnexpectedPass)
    }
}

impl fmt::Display for MatrixStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Pass => "PASS",
            Self::ExpectedFail => "XFAIL",
            Self::Skip => "SKIP",
            Self::Fail => "FAIL",
            Self::UnexpectedPass => "XPASS",
        })
    }
}

struct MatrixRow {
    category: &'static str,
    name: &'static str,
    status: MatrixStatus,
    timing: String,
    detail: String,
    diagnostic: Option<String>,
}

fn executable(candidates: &[&str]) -> Option<PathBuf> {
    candidates.iter().find_map(|candidate| {
        let path = PathBuf::from(candidate);
        let metadata = fs::metadata(&path).ok()?;
        (metadata.is_file() && metadata.permissions().mode() & 0o111 != 0).then_some(path)
    })
}

fn run_host_command(mut command: Command, label: &str) {
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {label}: {rendered}: {error}"));
    assert!(
        output.status.success(),
        "{label} failed: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn build_java_fixture(root: &Path) -> bool {
    let Some(javac) = executable(&["/usr/local/bin/javac", "/usr/bin/javac"]) else {
        return false;
    };
    let Some(jar) = executable(&["/usr/local/bin/jar", "/usr/bin/jar"]) else {
        return false;
    };

    let classes = root.join("java-classes");
    fs::create_dir_all(&classes).expect("failed to create Java classes directory");

    let mut javac_command = Command::new(javac);
    javac_command
        .arg("-d")
        .arg(&classes)
        .arg(root.join("Threaded.java"));
    run_host_command(javac_command, "Java integration fixture compilation");

    let mut jar_command = Command::new(jar);
    jar_command
        .args(["cfe"])
        .arg(root.join("threaded.jar"))
        .arg("Threaded")
        .arg("-C")
        .arg(&classes)
        .arg(".");
    run_host_command(jar_command, "Java integration fixture packaging");
    true
}

fn fixture() -> Fixture {
    let target_tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
    fs::create_dir_all(target_tmp).expect("failed to create Cargo target temp directory");
    let tempdir = tempfile::Builder::new()
        .prefix("integration-matrix-")
        .tempdir_in(target_tmp)
        .expect("failed to create integration matrix fixture directory");
    let root = tempdir.path().to_path_buf();

    fs::write(root.join("input.txt"), b"integration-matrix\n")
        .expect("failed to write basic command fixture");
    fs::write(
        root.join("node_worker.js"),
        include_str!("../../experiments/shared-futex-verify_20260722/node_worker.js"),
    )
    .expect("failed to write Node.js fixture");
    fs::write(
        root.join("Threaded.java"),
        include_str!("../../experiments/shared-futex-verify_20260722/Threaded.java"),
    )
    .expect("failed to write Java fixture");
    fs::write(
        root.join("nginx.conf"),
        r#"worker_processes 1;
daemon off;
master_process off;
error_log stderr notice;
pid /tmp/integration-matrix-nginx.pid;
events { worker_connections 32; }
http {
    access_log off;
    client_body_temp_path /tmp/client_body;
    proxy_temp_path /tmp/proxy;
    fastcgi_temp_path /tmp/fastcgi;
    uwsgi_temp_path /tmp/uwsgi;
    scgi_temp_path /tmp/scgi;
    server {
        listen 127.0.0.42:18081;
        location / { return 200 "nginx-ok\n"; }
    }
}
"#,
    )
    .expect("failed to write Nginx fixture");

    let java_ready = build_java_fixture(&root);
    Fixture {
        _tempdir: tempdir,
        root,
        java_ready,
    }
}

fn case(
    category: &'static str,
    name: &'static str,
    candidates: &[&str],
    args: &[&str],
    marker: Option<&'static str>,
    expectation: Expectation,
    required: bool,
) -> Case {
    Case {
        category,
        name,
        program: executable(candidates),
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        marker,
        expectation,
        required,
        timeout_seconds: match expectation {
            Expectation::Pass => DEFAULT_TIMEOUT_SECONDS,
            Expectation::ExpectedFail => EXPECTED_FAIL_TIMEOUT_SECONDS,
        },
    }
}

fn cases(fixture: &Fixture) -> Vec<Case> {
    let mut cases = vec![
        case(
            "basic",
            "echo",
            &["/usr/bin/echo", "/bin/echo"],
            &["integration-echo"],
            Some("integration-echo"),
            Expectation::Pass,
            true,
        ),
        case(
            "basic",
            "ls",
            &["/usr/bin/ls", "/bin/ls"],
            &["-1", GUEST_FIXTURE],
            Some("input.txt"),
            Expectation::Pass,
            true,
        ),
        case(
            "basic",
            "cat",
            &["/usr/bin/cat", "/bin/cat"],
            &["/tmp/integration-matrix/input.txt"],
            Some("integration-matrix"),
            Expectation::Pass,
            true,
        ),
        case(
            "complex",
            "sqlite3",
            &["/usr/bin/sqlite3", "/usr/local/bin/sqlite3"],
            &[":memory:", "select 'sqlite-ok';"],
            Some("sqlite-ok"),
            Expectation::Pass,
            false,
        ),
        case(
            "complex",
            "python",
            &["/usr/bin/python3", "/bin/python3", "/usr/local/bin/python3"],
            &[
                "-c",
                "import hashlib,json; data=json.dumps({'values':[1,2,3]},sort_keys=True); print('python-ok:'+hashlib.sha256(data.encode()).hexdigest())",
            ],
            Some("python-ok:"),
            Expectation::Pass,
            false,
        ),
        case(
            "threaded",
            "node",
            &["/usr/local/bin/node", "/usr/bin/node"],
            &["/tmp/integration-matrix/node_worker.js"],
            Some("SHARED_FUTEX_NODE_OK workers=4"),
            Expectation::Pass,
            false,
        ),
        // `git --version` (the Meta git wrapper) is futex- and clone3-heavy. It
        // previously timed out because private `FUTEX_WAIT_BITSET` waits with an
        // absolute deadline were misclassified as relative, advancing virtual
        // time by roughly a full epoch. With that classification fixed it now
        // runs deterministically to completion, so it is a passing case.
        case(
            "threaded",
            "git",
            &["/usr/local/bin/git"],
            &["--version"],
            Some("git version"),
            Expectation::Pass,
            false,
        ),
        case(
            "expected-fail",
            "nginx",
            &["/usr/sbin/nginx"],
            &[
                "-t",
                "-p",
                "/tmp/",
                "-c",
                "/tmp/integration-matrix/nginx.conf",
            ],
            None,
            Expectation::ExpectedFail,
            false,
        ),
    ];

    let mut java = case(
        "threaded",
        "java",
        &["/usr/local/bin/java", "/usr/bin/java"],
        &["-jar", "/tmp/integration-matrix/threaded.jar"],
        Some("SHARED_FUTEX_JAVA_OK threads=8 count=80000"),
        Expectation::Pass,
        false,
    );
    if !fixture.java_ready {
        java.program = None;
    }
    cases.insert(6, java);
    cases
}

fn run_case(case: &Case, fixture: &Fixture) -> TimedRun {
    let program = case
        .program
        .as_ref()
        .expect("run_case requires an available program");
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command
        .args([
            "--log=off",
            "run",
            "--base-env=minimal",
            "--no-virtualize-cpuid",
            "--preemption-timeout=disabled",
        ])
        .arg(format!("--bind={}:{GUEST_FIXTURE}", fixture.root.display()))
        .arg("--")
        .arg(program)
        .args(&case.args)
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let rendered = format!("{command:?}");
    let started = Instant::now();
    let mut child = command.spawn().unwrap_or_else(|error| {
        panic!(
            "failed to start integration case {}: {rendered}: {error}",
            case.name
        )
    });
    let deadline = Duration::from_secs(case.timeout_seconds);
    let timed_out = loop {
        match child.try_wait() {
            Ok(Some(_)) => break false,
            Ok(None) if started.elapsed() >= deadline => {
                let process_group = -(child.id() as libc::pid_t);
                unsafe {
                    libc::kill(process_group, libc::SIGKILL);
                }
                break true;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(error) => panic!(
                "failed to poll integration case {}: {rendered}: {error}",
                case.name
            ),
        }
    };
    let output = child.wait_with_output().unwrap_or_else(|error| {
        panic!(
            "failed to collect integration case {}: {rendered}: {error}",
            case.name
        )
    });
    TimedRun {
        observation: Observation {
            exit_code: if timed_out {
                Some(124)
            } else {
                output.status.code()
            },
            stdout: output.stdout,
            stderr: output.stderr,
        },
        elapsed: started.elapsed(),
    }
}

fn contains_marker(observation: &Observation, marker: &str) -> bool {
    String::from_utf8_lossy(&observation.stdout).contains(marker)
        || String::from_utf8_lossy(&observation.stderr).contains(marker)
}

fn diagnostic(case: &Case, first: &TimedRun, second: &TimedRun) -> String {
    format!(
        "{}: expected {:?}\nrun 1 exit={:?}\nstdout:\n{}\nstderr:\n{}\nrun 2 exit={:?}\nstdout:\n{}\nstderr:\n{}",
        case.name,
        case.expectation,
        first.observation.exit_code,
        String::from_utf8_lossy(&first.observation.stdout),
        String::from_utf8_lossy(&first.observation.stderr),
        second.observation.exit_code,
        String::from_utf8_lossy(&second.observation.stdout),
        String::from_utf8_lossy(&second.observation.stderr),
    )
}

fn evaluate(case: &Case, first: TimedRun, second: TimedRun) -> MatrixRow {
    let deterministic = first.observation == second.observation;
    let both_succeeded =
        first.observation.exit_code == Some(0) && second.observation.exit_code == Some(0);
    let both_failed =
        first.observation.exit_code != Some(0) && second.observation.exit_code != Some(0);
    let marker_matched = case.marker.is_none_or(|marker| {
        contains_marker(&first.observation, marker) && contains_marker(&second.observation, marker)
    });

    let status = match case.expectation {
        Expectation::Pass if both_succeeded && marker_matched && deterministic => {
            MatrixStatus::Pass
        }
        Expectation::Pass => MatrixStatus::Fail,
        Expectation::ExpectedFail if both_failed && deterministic => MatrixStatus::ExpectedFail,
        Expectation::ExpectedFail if both_succeeded && marker_matched && deterministic => {
            MatrixStatus::UnexpectedPass
        }
        Expectation::ExpectedFail => MatrixStatus::Fail,
    };
    let detail = format!(
        "exit={:?}/{:?}, output={}",
        first.observation.exit_code,
        second.observation.exit_code,
        if deterministic { "match" } else { "DIFF" },
    );
    MatrixRow {
        category: case.category,
        name: case.name,
        status,
        timing: format!(
            "{}/{} ms",
            first.elapsed.as_millis(),
            second.elapsed.as_millis()
        ),
        detail,
        diagnostic: status
            .is_failure()
            .then(|| diagnostic(case, &first, &second)),
    }
}

fn missing(case: &Case) -> MatrixRow {
    let status = if case.required {
        MatrixStatus::Fail
    } else {
        MatrixStatus::Skip
    };
    MatrixRow {
        category: case.category,
        name: case.name,
        status,
        timing: "-".to_owned(),
        detail: "program or build prerequisite unavailable".to_owned(),
        diagnostic: status
            .is_failure()
            .then(|| format!("required program {} is unavailable", case.name)),
    }
}

fn print_matrix(rows: &[MatrixRow]) {
    println!();
    println!("Hermit run integration compatibility matrix");
    println!(
        "{:<14} {:<10} {:<6} {:>17}  detail",
        "category", "program", "result", "run1/run2"
    );
    println!("{}", "-".repeat(88));
    for row in rows {
        println!(
            "{:<14} {:<10} {:<6} {:>17}  {}",
            row.category, row.name, row.status, row.timing, row.detail
        );
    }
}

#[test]
fn integration_matrix() {
    let fixture = fixture();
    let rows: Vec<_> = cases(&fixture)
        .into_iter()
        .map(|case| {
            println!("running {}/{}...", case.category, case.name);
            match case.program {
                Some(_) => {
                    let first = run_case(&case, &fixture);
                    let second = run_case(&case, &fixture);
                    evaluate(&case, first, second)
                }
                None => missing(&case),
            }
        })
        .collect();

    print_matrix(&rows);
    let failures: Vec<_> = rows
        .iter()
        .filter(|row| row.status.is_failure())
        .filter_map(|row| row.diagnostic.as_deref())
        .collect();
    assert!(
        failures.is_empty(),
        "integration matrix failures:\n\n{}",
        failures.join("\n\n")
    );
}
