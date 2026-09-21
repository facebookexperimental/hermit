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
use std::process::Output;

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(path: PathBuf) -> Self {
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("failed to create isolated guest tmp directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
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

#[test]
fn deterministic_file_io_syscalls_verify() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("syscall-file-io");
    fs::create_dir_all(&build_root).expect("failed to create syscall guest build directory");
    let guest = build_root.join("syscall_file_io");
    let guest_tmp = TestDirectory::new(build_root.join("guest-tmp"));

    let mut compile = Command::new("cc");
    compile
        .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(repository.join("tests/c/syscall_file_io.c"))
        .arg("-o")
        .arg(&guest);
    command_output(compile, "file-IO syscall guest compilation");

    let mut trace = Command::new("timeout");
    trace
        .args(["--kill-after", "5s", "60s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=trace",
            "run",
            "--backend=ptrace",
            "--strict",
            "--panic-on-unsupported-syscalls",
            "--base-env=minimal",
            "--tmp",
        ])
        .arg(guest_tmp.path())
        .arg("--")
        .arg(&guest);
    let trace_output = command_output(trace, "strict file-IO syscall trace");
    let trace_stdout = String::from_utf8_lossy(&trace_output.stdout);
    let trace_stderr = String::from_utf8_lossy(&trace_output.stderr);
    assert!(
        trace_stdout.contains("syscall-file-io-ok count=5"),
        "guest omitted its success marker\nstdout:\n{trace_stdout}\nstderr:\n{trace_stderr}",
    );
    for syscall in ["fallocate", "readlinkat", "rename", "renameat", "truncate"] {
        assert!(
            trace_stderr.contains(&format!("inbound syscall: {syscall}(")),
            "trace omitted {syscall}\nstdout:\n{trace_stdout}\nstderr:\n{trace_stderr}",
        );
    }

    let mut verify = Command::new("timeout");
    verify
        .args(["--kill-after", "5s", "90s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=info",
            "run",
            "--backend=ptrace",
            "--strict",
            "--verify",
            "--panic-on-unsupported-syscalls",
            "--base-env=minimal",
            "--tmp",
        ])
        .arg(guest_tmp.path())
        .arg("--")
        .arg(&guest);
    let verify_output = command_output(verify, "strict file-IO syscall verification");
    let verify_stdout = String::from_utf8_lossy(&verify_output.stdout);
    let verify_stderr = String::from_utf8_lossy(&verify_output.stderr);
    assert!(
        verify_stdout.contains("Determinism verified")
            || verify_stderr.contains("Determinism verified"),
        "Hermit omitted its determinism marker\nstdout:\n{verify_stdout}\nstderr:\n{verify_stderr}",
    );
}
