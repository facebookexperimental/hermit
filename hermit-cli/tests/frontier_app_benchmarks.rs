/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Small application benchmarks whose native clock and entropy observations
//! vary, but whose ptrace executions reach L2 under strict Hermit.
//!
//! Each case first checks that its intended nondeterminism is observable
//! without Hermit, then runs the identical guest command with
//! `--backend=ptrace --strict --verify --log=info`. No determinism relaxations
//! are used.

use std::ffi::OsString;
use std::net::TcpListener;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::Mutex;

const NATIVE_TIMEOUT: &str = "30s";
const HERMIT_TIMEOUT: &str = "180s";
const KILL_AFTER: &str = "10s";
const NATIVE_RETRIES: usize = 4;
const DETERMINISM_MARKER: &str = "Success: deterministic. Determinism verified.";

static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());

const SQLITE_WORKLOAD: &str = "\
PRAGMA journal_mode=memory;
CREATE TABLE t(k TEXT PRIMARY KEY,v INTEGER);
INSERT INTO t VALUES('a',40),('b',2);
UPDATE t SET v=v+1;
SELECT sum(v), random(), hex(randomblob(8)), strftime('%s','now') FROM t;
PRAGMA integrity_check;";

struct Workload {
    name: &'static str,
    nondeterminism: &'static str,
    program: PathBuf,
    args: Vec<OsString>,
    output_marker: &'static str,
}

impl Workload {
    fn assert_native_nondeterminism(&self) {
        let baseline = self.run_native();
        self.assert_native_success(&baseline);

        for _ in 0..NATIVE_RETRIES {
            let candidate = self.run_native();
            self.assert_native_success(&candidate);
            if outputs_differ(&baseline, &candidate) {
                return;
            }
        }

        panic!(
            "{} did not expose native {} nondeterminism across {} executions\n{}",
            self.name,
            self.nondeterminism,
            NATIVE_RETRIES + 1,
            render_output(&baseline),
        );
    }

    fn assert_l2_under_strict_hermit(&self) {
        let _guard = HERMIT_RUN_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut command = Command::new("timeout");
        command
            .args(["--kill-after", KILL_AFTER, HERMIT_TIMEOUT])
            .arg(env!("CARGO_BIN_EXE_hermit"))
            .args([
                "--log=info",
                "run",
                "--backend=ptrace",
                "--strict",
                "--verify",
                "--",
            ])
            .arg(&self.program)
            .args(&self.args);

        let rendered = format!("{command:?}");
        let output = command
            .output()
            .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"));
        let verification_output = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );

        assert!(
            output.status.success() && verification_output.contains(DETERMINISM_MARKER),
            "{} did not reach L2 with the ptrace backend, --log=info, and no relaxations\n\
             command: {rendered}\n{}",
            self.name,
            render_output(&output),
        );
    }

    fn run_native(&self) -> Output {
        let mut command = Command::new("timeout");
        command
            .args(["--kill-after", KILL_AFTER, NATIVE_TIMEOUT])
            .arg(&self.program)
            .args(&self.args);
        let rendered = format!("{command:?}");
        command
            .output()
            .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"))
    }

    fn assert_native_success(&self, output: &Output) {
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains(self.output_marker),
            "{} native control failed or omitted marker {:?}\n{}",
            self.name,
            self.output_marker,
            render_output(output),
        );
    }
}

fn repository() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository")
}

fn required_app(name: &str, candidates: &[&str]) -> PathBuf {
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("required application {name} is missing: {candidates:?}"))
}

fn outputs_differ(left: &Output, right: &Output) -> bool {
    left.status != right.status || left.stdout != right.stdout || left.stderr != right.stderr
}

fn render_output(output: &Output) -> String {
    fn bounded(bytes: &[u8]) -> String {
        const LIMIT: usize = 16 * 1024;
        if bytes.len() <= LIMIT {
            return String::from_utf8_lossy(bytes).into_owned();
        }

        let half = LIMIT / 2;
        format!(
            "{}\n... {} bytes omitted ...\n{}",
            String::from_utf8_lossy(&bytes[..half]),
            bytes.len() - LIMIT,
            String::from_utf8_lossy(&bytes[bytes.len() - half..]),
        )
    }

    format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        bounded(&output.stdout),
        bounded(&output.stderr),
    )
}

fn assert_occasional_application_script(script_name: &str, success_marker: &str) {
    let _guard = HERMIT_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let script = repository()
        .join("tests/e2e/lib/applications")
        .join(script_name);
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after", KILL_AFTER, "240s"])
        .arg(&script)
        .env("HERMIT_BIN", env!("CARGO_BIN_EXE_hermit"))
        .env("HERMIT_APPLICATION_TIMEOUT", HERMIT_TIMEOUT);

    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains(success_marker),
        "occasional application script did not reach L2 under strict Hermit\n\
         command: {rendered}\n{}",
        render_output(&output),
    );
}

fn unused_loopback_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .expect("failed to reserve a loopback port")
        .local_addr()
        .expect("failed to read the reserved loopback port")
        .port()
}

#[test]
#[ignore = "requires PMU, mount namespaces, and sqlite3"]
fn sqlite_transaction_is_nondeterministic_natively_and_l2_under_hermit() {
    let workload = Workload {
        name: "SQLite transaction benchmark",
        nondeterminism: "clock-and-entropy",
        program: required_app("sqlite3", &["/usr/bin/sqlite3", "/usr/local/bin/sqlite3"]),
        args: vec![":memory:".into(), SQLITE_WORKLOAD.into()],
        output_marker: "memory\n44|",
    };

    workload.assert_native_nondeterminism();
    workload.assert_l2_under_strict_hermit();
}

#[test]
#[ignore = "requires PMU, mount namespaces, redis-server, and redis-cli"]
fn redis_commands_are_nondeterministic_natively_and_l2_under_hermit() {
    let script = repository().join("hermit-cli/tests/fixtures/frontier-apps/redis-workload.sh");
    let redis_server = required_app(
        "redis-server",
        &["/usr/bin/redis-server", "/usr/local/bin/redis-server"],
    );
    let redis_cli = required_app(
        "redis-cli",
        &["/usr/bin/redis-cli", "/usr/local/bin/redis-cli"],
    );
    let root = format!("/tmp/hermit-frontier-redis-{}", std::process::id());
    let workload = Workload {
        name: "Redis command benchmark",
        nondeterminism: "server-clock-and-random-key",
        program: required_app("sh", &["/bin/sh", "/usr/bin/sh"]),
        args: vec![
            script.into_os_string(),
            redis_server.into_os_string(),
            redis_cli.into_os_string(),
            root.into(),
        ],
        output_marker: "ping=PONG visits=3 hash-fields=2 time=",
    };

    workload.assert_native_nondeterminism();
    workload.assert_l2_under_strict_hermit();
}

#[test]
#[ignore = "occasional validation: full Redis server/client session is too slow for every commit"]
fn redis_deep_session_is_nondeterministic_natively_and_l2_under_hermit() {
    assert_occasional_application_script("redis_deep.sh", "redis-deep:verified");
}

#[test]
#[ignore = "requires PMU, mount namespaces, and Python 3"]
fn http_server_is_nondeterministic_natively_and_l2_under_hermit() {
    let script = repository().join("hermit-cli/tests/fixtures/frontier-apps/http_server.py");
    let port = unused_loopback_port().to_string();
    let workload = Workload {
        name: "Python HTTP server benchmark",
        nondeterminism: "response-clock-and-entropy",
        program: required_app("python3", &["/usr/bin/python3", "/usr/local/bin/python3"]),
        args: vec![script.into_os_string(), port.into()],
        output_marker: "status=200 path=/frontier time=",
    };

    workload.assert_native_nondeterminism();
    workload.assert_l2_under_strict_hermit();
}

#[test]
#[ignore = "occasional validation: long-lived HTTP server/client session"]
fn http_server_session_is_nondeterministic_natively_and_l2_under_hermit() {
    assert_occasional_application_script("http_server.sh", "http-server:verified");
}
