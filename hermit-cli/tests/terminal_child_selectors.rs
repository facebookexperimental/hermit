/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Runtime coverage for scheduler-owned terminal child selectors.

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;

static GUEST: OnceLock<PathBuf> = OnceLock::new();
static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());

fn hermit_run_guard() -> MutexGuard<'static, ()> {
    HERMIT_RUN_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn selector_guest() -> &'static Path {
    GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("terminal-child-selectors");
        fs::create_dir_all(&build_root).expect("failed to create selector guest build directory");
        let guest = build_root.join("terminal_child_selectors");
        let source = repository.join("tests/c/terminal_child_selectors.c");
        let output = Command::new("cc")
            .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror", "-pthread"])
            .arg(&source)
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile terminal selector guest");
        assert!(
            output.status.success(),
            "selector guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn run_selector_guest(backend: &str, guest_args: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command.args(["run", &format!("--backend={backend}"), "--strict"]);
    if backend == "kvm" {
        command.arg("--max-timeslice=disabled");
    }
    command.arg("--tmp=/tmp");
    command.arg("--").arg(selector_guest()).args(guest_args);
    command
        .output()
        .unwrap_or_else(|error| panic!("failed to run selector guest under {backend}: {error}"))
}

#[test]
fn ptrace_terminal_child_selectors_follow_linux_contract() {
    let _guard = hermit_run_guard();
    let output = run_selector_guest("ptrace", &[]);
    assert!(
        output.status.success(),
        "ptrace selector guest failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "wait4-groups=ok waitid-groups=ok nothread=ok clone-parent=ok\n"
    );
}

#[test]
fn kvm_same_group_terminal_child_selectors_follow_linux_contract() {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("skipping KVM selector test: /dev/kvm is unavailable");
        return;
    }

    let _guard = hermit_run_guard();
    let output = run_selector_guest("kvm", &["--groups-only"]);
    assert!(
        output.status.success(),
        "KVM selector guest failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "wait4-pgid0=ok waitid-pgid0=ok\n"
    );
}
