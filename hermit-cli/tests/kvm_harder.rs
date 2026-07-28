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
use std::sync::Mutex;

static KVM_RUN_LOCK: Mutex<()> = Mutex::new(());

fn compile_guest(name: &str, source: &str, extra_args: &[&str]) -> PathBuf {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("kvm-harder");
    fs::create_dir_all(&build_root).expect("failed to create KVM guest build directory");
    let binary = build_root.join(name);
    let output = Command::new("cc")
        .args(["-std=c11", "-O2", "-g", "-Wall", "-Wextra", "-Werror"])
        .args(extra_args)
        .arg(repository.join(source))
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap_or_else(|error| panic!("failed to compile {source}: {error}"));
    assert!(
        output.status.success(),
        "failed to compile {source}:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
    binary
}

fn run_guest(backend: &str, binary: &Path, verify: bool) -> Output {
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after", "10s", "90s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["run", "--backend", backend, "--strict"]);
    if verify {
        command.arg("--verify");
    }
    command.arg("--").arg(binary);
    command
        .output()
        .unwrap_or_else(|error| panic!("failed to run {binary:?} on {backend}: {error}"))
}

fn assert_ptrace_kvm_parity(name: &str, source: &str, extra_args: &[&str], expected_stdout: &str) {
    if !Path::new("/dev/kvm").exists() {
        return;
    }
    let _guard = KVM_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let binary = compile_guest(name, source, extra_args);

    for backend in ["ptrace", "kvm"] {
        let output = run_guest(backend, &binary, false);
        assert!(
            output.status.success(),
            "{backend} strict run failed for {name}:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout), expected_stdout);

        let verified = run_guest(backend, &binary, true);
        assert!(
            verified.status.success(),
            "{backend} strict verify failed for {name}:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&verified.stdout),
            String::from_utf8_lossy(&verified.stderr),
        );
        assert!(
            String::from_utf8_lossy(&verified.stderr).contains("Success:"),
            "{backend} omitted the verification success marker for {name}:\n{}",
            String::from_utf8_lossy(&verified.stderr),
        );
    }
}

#[test]
fn kvm_matches_ptrace_for_pthread_lifecycle() {
    assert_ptrace_kvm_parity(
        "pthread_lifecycle",
        "tests/backend-parity/fixtures/pthread_lifecycle.c",
        &["-pthread"],
        "threads=4 total=10\n",
    );
}

#[test]
fn kvm_matches_ptrace_for_fork_tree() {
    assert_ptrace_kvm_parity(
        "fork_tree",
        "tests/e2e/determinism-stress/fork_tree.c",
        &[],
        "fork-tree processes=13 syscalls-per-process=100 child-exits=20,21,22,23 \
         grandchild-exits=40,41,42,43,44,45,46,47\n",
    );
}

#[test]
fn kvm_matches_ptrace_for_prefilled_pipe_across_fork() {
    assert_ptrace_kvm_parity(
        "pipe_prefill",
        "tests/e2e/determinism-stress/pipe_prefill.c",
        &[],
        "fork-pipe bytes=21 child-exit=37 payload=KVM PIPE INHERITANCE\n",
    );
}
