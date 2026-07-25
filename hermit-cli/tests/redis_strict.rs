/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::thread;
use std::time::Duration;

static HERMIT_REDIS_LOCK: Mutex<()> = Mutex::new(());

struct RedisServerGuard {
    redis_cli: PathBuf,
    port: u16,
}

impl Drop for RedisServerGuard {
    fn drop(&mut self) {
        let _ = Command::new(&self.redis_cli)
            .args([
                "-h",
                "127.0.0.1",
                "-p",
                &self.port.to_string(),
                "SHUTDOWN",
                "NOSAVE",
            ])
            .output();
    }
}

fn hermit_redis_lock() -> MutexGuard<'static, ()> {
    HERMIT_REDIS_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn executable(candidates: &[&str]) -> Option<PathBuf> {
    candidates.iter().find_map(|candidate| {
        let path = PathBuf::from(candidate);
        let metadata = fs::metadata(&path).ok()?;
        (metadata.is_file() && metadata.permissions().mode() & 0o111 != 0).then_some(path)
    })
}

fn command_output(mut command: Command, label: &str) -> Output {
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
    output
}

fn repository() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository")
}

fn unused_loopback_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .expect("failed to reserve a unique loopback port for Redis");
    listener
        .local_addr()
        .expect("failed to read reserved Redis address")
        .port()
}

fn system_redis() -> (PathBuf, PathBuf) {
    let redis_server = executable(&["/usr/bin/redis-server", "/usr/local/bin/redis-server"])
        .expect("redis-server is required; the self-hosted CI job installs it");
    let redis_cli = executable(&["/usr/bin/redis-cli", "/usr/local/bin/redis-cli"])
        .expect("redis-cli is required; the self-hosted CI job installs it");
    (redis_server, redis_cli)
}

fn redis_cli_output(redis_cli: &Path, port: u16, args: &[&str]) -> Output {
    Command::new(redis_cli)
        .args(["--raw", "-h", "127.0.0.1", "-p", &port.to_string()])
        .args(args)
        .output()
        .expect("failed to start redis-cli")
}

fn run_strict_workload(
    redis_server: &Path,
    redis_cli: &Path,
    mode: &str,
    iteration: usize,
) -> Output {
    let workload = repository().join("experiments/redis-strict/workload.sh");
    let instance = format!("cargo-{}-{iteration}", std::process::id());
    let port = unused_loopback_port();
    let mut command = Command::new("timeout");
    command
        .arg("90")
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log", "off", "run", "--strict", "--", "/bin/sh"])
        .arg(workload)
        .arg(redis_server)
        .arg(redis_cli)
        .arg(mode)
        .arg(instance)
        .arg(port.to_string());
    command_output(
        command,
        &format!("strict Redis {mode} workload, iteration {iteration}"),
    )
}

#[test]
fn redis_small_subset_is_deterministic_under_strict_hermit() {
    let _guard = hermit_redis_lock();
    let (redis_server, redis_cli) = system_redis();

    let first = run_strict_workload(&redis_server, &redis_cli, "small", 1);
    let second = run_strict_workload(&redis_server, &redis_cli, "small", 2);
    assert_eq!(
        first.stdout, second.stdout,
        "Redis stdout changed between runs"
    );
    assert_eq!(
        first.stderr, second.stderr,
        "Redis stderr changed between runs"
    );

    let stdout = String::from_utf8_lossy(&first.stdout);
    for marker in [
        "ping=PONG\n",
        "string=hermit\n",
        "counter=2\n",
        "list=alpha,beta,gamma\n",
        "redis-strict-small-ok\n",
    ] {
        assert!(
            stdout.contains(marker),
            "Redis output omitted {marker:?}:\n{stdout}"
        );
    }
}

#[test]
fn redis_persistence_restart_is_deterministic_under_strict_hermit() {
    let _guard = hermit_redis_lock();
    let (redis_server, redis_cli) = system_redis();

    let first = run_strict_workload(&redis_server, &redis_cli, "extended", 1);
    let second = run_strict_workload(&redis_server, &redis_cli, "extended", 2);
    assert_eq!(
        first.stdout, second.stdout,
        "Redis persistence stdout changed between runs"
    );
    assert_eq!(
        first.stderr, second.stderr,
        "Redis persistence stderr changed between runs"
    );

    let stdout = String::from_utf8_lossy(&first.stdout);
    for marker in [
        "pid-turnover=ok\n",
        "persistence=ok\n",
        "redis-strict-extended-ok\n",
    ] {
        assert!(
            stdout.contains(marker),
            "Redis persistence output omitted {marker:?}:\n{stdout}"
        );
    }
}

#[test]
fn redis_workload_refuses_to_control_a_preexisting_server() {
    let _guard = hermit_redis_lock();
    let (redis_server, redis_cli) = system_redis();
    let port = unused_loopback_port();
    let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create unrelated Redis directory");
    let pidfile = root.path().join("redis.pid");
    let logfile = root.path().join("redis.log");

    let mut start = Command::new(&redis_server);
    start
        .args([
            "--daemonize",
            "yes",
            "--bind",
            "127.0.0.1",
            "--protected-mode",
            "no",
            "--port",
            &port.to_string(),
            "--save",
            "",
            "--appendonly",
            "no",
            "--pidfile",
        ])
        .arg(&pidfile)
        .arg("--logfile")
        .arg(&logfile)
        .arg("--dir")
        .arg(root.path());
    command_output(start, "unrelated Redis server");
    let _server_guard = RedisServerGuard {
        redis_cli: redis_cli.clone(),
        port,
    };

    let mut ready = false;
    for _ in 0..100 {
        let output = redis_cli_output(&redis_cli, port, &["PING"]);
        if output.status.success() && output.stdout == b"PONG\n" {
            ready = true;
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(ready, "unrelated Redis server did not become ready");
    let expected_pid = fs::read_to_string(&pidfile)
        .expect("unrelated Redis server omitted its pidfile")
        .trim()
        .to_owned();

    let workload = repository().join("experiments/redis-strict/workload.sh");
    let output = Command::new("/bin/sh")
        .arg(workload)
        .arg(&redis_server)
        .arg(&redis_cli)
        .arg("small")
        .arg(format!("collision-{}", std::process::id()))
        .arg(port.to_string())
        .output()
        .expect("failed to run Redis collision probe");
    assert!(
        !output.status.success(),
        "workload unexpectedly attached to a preexisting Redis server"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("already serving before launch"),
        "workload did not report its occupied endpoint:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let info = redis_cli_output(&redis_cli, port, &["INFO", "server"]);
    assert!(
        info.status.success(),
        "preexisting Redis server was stopped"
    );
    let info_stdout = String::from_utf8_lossy(&info.stdout);
    let observed_pid = info_stdout
        .lines()
        .find_map(|line| line.strip_prefix("process_id:"))
        .map(str::trim)
        .expect("Redis INFO omitted process_id");
    assert_eq!(
        observed_pid, expected_pid,
        "workload replaced or stopped the preexisting Redis server"
    );
}

#[test]
#[ignore = "downloads and builds pinned Redis, then runs the extended strict suite"]
fn redis_source_build_and_extended_suite_under_strict_hermit() {
    let _guard = hermit_redis_lock();
    let runner = repository().join("experiments/redis-strict/run.sh");
    let mut command = Command::new("timeout");
    command
        .arg("900")
        .arg(runner)
        .env("HERMIT_BIN", env!("CARGO_BIN_EXE_hermit"));
    let output = command_output(command, "pinned Redis source-build strict suite");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("redis-source-build-strict-ok\n"),
        "source-build runner omitted its success marker:\n{stdout}"
    );
}
