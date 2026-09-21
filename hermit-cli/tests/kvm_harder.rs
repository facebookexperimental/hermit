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

#[test]
fn kvm_mountinfo_uses_its_synthetic_namespace_identity() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }
    let _guard = KVM_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let guest = compile_guest(
        "mountinfo_device_identity",
        "tests/backend-parity/fixtures/mountinfo_device_identity.c",
        &[],
    );
    let run = || {
        let mut command = Command::new("timeout");
        command
            .args(["--kill-after", "10s", "90s"])
            .arg(env!("CARGO_BIN_EXE_hermit"))
            .args(["run", "--backend", "kvm", "--strict", "--tmp=/tmp", "--"])
            .arg(&guest);
        let output = command.output().expect("run KVM mountinfo guest");
        assert!(
            output.status.success(),
            "KVM mountinfo failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        output.stdout
    };
    let first = run();
    let second = run();
    assert_eq!(first, second, "KVM mountinfo changed between strict runs");
    let text = std::str::from_utf8(&first).expect("KVM probe output should be UTF-8");
    let mountinfo = text
        .lines()
        .find_map(|line| line.strip_prefix("MOUNTINFO "))
        .expect("probe omitted mountinfo row");
    let fields = mountinfo.split_whitespace().collect::<Vec<_>>();
    assert_eq!(&fields[..2], ["1", "2"]);
    assert_eq!(
        &fields[3..],
        ["/", "/", "rw", "-", "rootfs", "rootfs", "rw"]
    );
    let stat_device = text
        .lines()
        .find_map(|line| line.strip_prefix("STAT "))
        .expect("probe omitted stat device");
    let statx_device = text
        .lines()
        .find_map(|line| line.strip_prefix("STATX "))
        .expect("probe omitted statx device");
    assert_eq!(fields[2], stat_device, "mountinfo disagreed with stat");
    assert_eq!(fields[2], statx_device, "mountinfo disagreed with statx");
    // This is output/status evidence only; KVM does not expose comparable INFO
    // logs for a full L2 claim, and the first-observation policy is not a
    // cross-machine promise.
}
