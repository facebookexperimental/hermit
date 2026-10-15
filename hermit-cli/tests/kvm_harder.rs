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
        .args(["--backend", backend, "run", "--strict"]);
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
        "tests/c/pthread_lifecycle.c",
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

/// What `tests/c/mountinfo_device_identity.c` printed: the fields of the first
/// mountinfo row, and the device of `/` from stat and from statx.
#[derive(Debug, PartialEq)]
struct MountinfoProbe {
    row: Vec<String>,
    stat: String,
    statx: String,
}

impl MountinfoProbe {
    fn parse(stdout: &[u8]) -> Self {
        let text = std::str::from_utf8(stdout).expect("mountinfo probe output should be UTF-8");
        let field = |prefix: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(prefix))
                .unwrap_or_else(|| panic!("probe omitted {prefix:?}:\n{text}"))
                .to_owned()
        };
        Self {
            row: field("MOUNTINFO ")
                .split_whitespace()
                .map(str::to_owned)
                .collect(),
            stat: field("STAT "),
            statx: field("STATX "),
        }
    }

    /// The row's device, the third field.
    fn device(&self) -> &str {
        &self.row[2]
    }
}

fn run_mountinfo_probe(backend: &str, run_args: &[&str], guest: &Path) -> Vec<u8> {
    let output = Command::new("timeout")
        .args(["--kill-after", "10s", "90s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--backend", backend, "run"])
        .args(run_args)
        .args(["--tmp=/tmp", "--"])
        .arg(guest)
        .output()
        .unwrap_or_else(|error| panic!("failed to run the mountinfo probe on {backend}: {error}"));
    assert!(
        output.status.success(),
        "{backend} {run_args:?} mountinfo probe failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output.stdout
}

#[test]
fn kvm_mountinfo_matches_ptrace_and_keeps_the_linux_device_relation() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }
    let _guard = KVM_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let guest = compile_guest(
        "mountinfo_device_identity",
        "tests/c/mountinfo_device_identity.c",
        &[],
    );
    // Linux's own answer in the same container, with host device numbers.
    // Whether the root row's device equals stat's is a property of the host
    // filesystem: on btrfs, stat reports the subvolume's anonymous device and
    // mountinfo the superblock's, so they differ natively.
    let native = MountinfoProbe::parse(&run_mountinfo_probe(
        "ptrace",
        &["--no-virtualize-metadata"],
        &guest,
    ));
    let strict = |backend: &str| {
        let first = run_mountinfo_probe(backend, &["--strict"], &guest);
        let second = run_mountinfo_probe(backend, &["--strict"], &guest);
        assert_eq!(
            first, second,
            "{backend} mountinfo changed between strict runs"
        );
        MountinfoProbe::parse(&first)
    };
    let ptrace = strict("ptrace");
    let kvm = strict("kvm");
    for (backend, view) in [("ptrace", &ptrace), ("kvm", &kvm)] {
        // Detcore numbers mount IDs by first observation, so the first row
        // the guest reads is mount 1; everything after the IDs is Linux's.
        assert_eq!(view.row[0], "1", "{backend}: {view:?}");
        assert_eq!(view.row[3..], native.row[3..], "{backend}: {view:?}");
        assert_eq!(view.stat, view.statx, "{backend}: {view:?}");
        assert_eq!(
            view.device() == view.stat,
            native.device() == native.stat,
            "{backend} changed whether mountinfo and stat name one device: \
             {view:?} against native {native:?}"
        );
    }
    // KVM gives the same view as ptrace, except the row's device number. The
    // probe's stdio fstats the open mountinfo file before reading it. KVM's
    // own /proc gives that file a device of its own, and the fstat takes one
    // more run-global device number on KVM than on ptrace, so the row's
    // device is numbered one later (reading before the fstat numbers it alike).
    // https://github.com/rrnewton/hermit/issues/4282 removes KVM's own /proc;
    // compare the whole view then.
    assert_eq!(kvm.row[..2], ptrace.row[..2]);
    assert_eq!(kvm.row[3..], ptrace.row[3..]);
    assert_eq!((&kvm.stat, &kvm.statx), (&ptrace.stat, &ptrace.statx));
    // This is output/status evidence only; KVM does not expose comparable INFO
    // logs for a full L2 claim, and the first-observation policy is not a
    // cross-machine promise.
}
