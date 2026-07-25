/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::time::Duration;
use std::time::Instant;

static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());
const BINARY_TIMEOUT: Duration = Duration::from_secs(20);
const RUN_TOOLS: &[&str] = &[
    "static_busybox",
    "dynamic_ls",
    "shell",
    "python",
    "curl",
    "git",
    "gcc",
];
const RECORD_REPLAY_TOOLS: &[&str] = &["static_busybox", "shell"];

#[derive(Clone, Debug)]
struct Tool {
    name: &'static str,
    path: PathBuf,
    args: &'static [&'static str],
    marker: &'static str,
}

fn hermit_run_lock() -> MutexGuard<'static, ()> {
    HERMIT_RUN_LOCK
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

fn rustup_tool(name: &str) -> Option<PathBuf> {
    let output = Command::new("rustup").args(["which", name]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let path = PathBuf::from(String::from_utf8(output.stdout).ok()?.trim());
    executable(&[path.to_str()?])
}

fn tool(
    name: &'static str,
    candidates: &[&str],
    args: &'static [&'static str],
    marker: &'static str,
) -> Option<Tool> {
    Some(Tool {
        name,
        path: executable(candidates)?,
        args,
        marker,
    })
}

fn available_tools(allowed: &[&str]) -> Vec<Tool> {
    let mut tools = [
        tool(
            "static_busybox",
            &["/usr/sbin/busybox", "/usr/bin/busybox", "/bin/busybox"],
            &["echo", "busybox-ok"],
            "busybox-ok",
        ),
        tool(
            "dynamic_ls",
            &["/usr/bin/ls", "/bin/ls"],
            &["--version"],
            "ls",
        ),
        tool(
            "shell",
            &["/usr/bin/sh", "/bin/sh"],
            &["-c", "printf 'shell-ok\\n'"],
            "shell-ok",
        ),
        tool(
            "python",
            &["/usr/bin/python3", "/bin/python3"],
            &["-c", "print('python-ok')"],
            "python-ok",
        ),
        tool(
            "node",
            &["/usr/bin/node", "/usr/local/bin/node"],
            &["-e", "console.log('node-ok')"],
            "node-ok",
        ),
        tool(
            "java",
            &["/usr/bin/java", "/usr/local/bin/java"],
            &["-version"],
            "version",
        ),
        tool(
            "go",
            &["/usr/bin/go", "/usr/local/bin/go"],
            &["version"],
            "go version",
        ),
        tool(
            "curl",
            &["/usr/bin/curl", "/usr/local/bin/curl"],
            &["--version"],
            "curl",
        ),
        tool(
            "wget",
            &["/usr/bin/wget", "/usr/local/bin/wget"],
            &["--version"],
            "Wget",
        ),
        tool(
            "git",
            &["/usr/bin/git", "/bin/git"],
            &["--version"],
            "git version",
        ),
        tool("gcc", &["/usr/bin/gcc", "/bin/gcc"], &["--version"], "gcc"),
        tool(
            "make",
            &["/usr/bin/make", "/bin/make"],
            &["--version"],
            "Make",
        ),
        tool(
            "cmake",
            &["/usr/bin/cmake", "/usr/local/bin/cmake"],
            &["--version"],
            "cmake version",
        ),
        tool(
            "sqlite",
            &["/usr/bin/sqlite3", "/usr/local/bin/sqlite3"],
            &[
                ":memory:",
                "CREATE TABLE t(x); INSERT INTO t VALUES (3),(1),(2); SELECT group_concat(x, ',') FROM (SELECT x FROM t ORDER BY x);",
            ],
            "1,2,3",
        ),
    ]
    .into_iter()
    .flatten()
    .filter(|tool| allowed.contains(&tool.name))
    .collect::<Vec<_>>();

    if allowed.contains(&"cargo")
        && let Some(path) = rustup_tool("cargo")
    {
        tools.push(Tool {
            name: "cargo",
            path,
            args: &["--version"],
            marker: "cargo",
        });
    }
    tools
}

fn bounded_command(program: &Path, timeout: Duration) -> Command {
    let mut command = Command::new("timeout");
    command
        .args(["--signal=TERM", "--kill-after=2s"])
        .arg(format!("{:.3}s", timeout.as_secs_f64()))
        .arg(program);
    command
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

fn assert_marker(tool: &Tool, output: &Output, label: &str) {
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains(tool.marker),
        "{label} for {} did not contain {:?}:\n{}",
        tool.name,
        tool.marker,
        combined
    );
}

fn hermit_run(tool: &Tool) -> Output {
    let mut command = bounded_command(Path::new(env!("CARGO_BIN_EXE_hermit")), BINARY_TIMEOUT);
    command
        .args([
            "run",
            "--base-env=minimal",
            "--no-virtualize-cpuid",
            "--preemption-timeout=disabled",
            "--",
        ])
        .arg(&tool.path)
        .args(tool.args);
    command_output(command, &format!("run for {}", tool.name))
}

fn record_replay(tool: &Tool) -> Output {
    let data_dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("arbitrary-binary-recordings")
        .join(tool.name);
    if data_dir.exists() {
        fs::remove_dir_all(&data_dir).expect("failed to remove stale recording directory");
    }
    fs::create_dir_all(&data_dir).expect("failed to create recording directory");

    let mut command = bounded_command(Path::new(env!("CARGO_BIN_EXE_hermit")), BINARY_TIMEOUT);
    command
        .args(["record", "start", "--verify"])
        .arg(format!("--data-dir={}", data_dir.display()))
        .arg("--")
        .arg(&tool.path)
        .args(tool.args);
    command_output(command, &format!("record/replay for {}", tool.name))
}

#[test]
fn run_arbitrary_binary_matrix() {
    let _guard = hermit_run_lock();
    let tools = available_tools(RUN_TOOLS);
    assert!(
        tools.iter().any(|tool| tool.name == "dynamic_ls")
            && tools.iter().any(|tool| tool.name == "shell"),
        "the arbitrary binary matrix requires ls and sh"
    );

    for tool in &tools {
        let output = hermit_run(tool);
        assert_marker(tool, &output, "run output");
    }
}

#[test]
fn record_replay_stable_arbitrary_binaries() {
    let _guard = hermit_run_lock();
    let tools = available_tools(RECORD_REPLAY_TOOLS);

    for tool in &tools {
        let output = record_replay(tool);
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            combined.contains("Success: replay matched recording."),
            "Hermit did not report a matching replay for {}:\n{}",
            tool.name,
            combined
        );
    }
}

#[test]
fn arbitrary_binary_commands_are_bounded() {
    let mut command = bounded_command(Path::new("/bin/sh"), Duration::from_millis(100));
    command.args(["-c", "sleep 10"]);

    let started = Instant::now();
    let output = command
        .output()
        .expect("bounded command should start successfully");
    assert_eq!(output.status.code(), Some(124));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn arbitrary_binary_lists_are_curated_for_ci() {
    for name in [
        "node", "java", "go", "wget", "make", "cmake", "sqlite", "cargo",
    ] {
        assert!(!RUN_TOOLS.contains(&name));
        assert!(!RECORD_REPLAY_TOOLS.contains(&name));
    }
    assert!(
        RECORD_REPLAY_TOOLS
            .iter()
            .all(|name| RUN_TOOLS.contains(name))
    );

    let worst_case_seconds =
        (RUN_TOOLS.len() + RECORD_REPLAY_TOOLS.len()) as u64 * BINARY_TIMEOUT.as_secs();
    assert!(
        worst_case_seconds < 5 * 60,
        "curated arbitrary binary probes can exceed five minutes"
    );
}
