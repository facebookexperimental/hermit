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
use std::sync::OnceLock;

static LITEINST_ADVANCED_GUEST: OnceLock<PathBuf> = OnceLock::new();

fn advanced_guest() -> &'static Path {
    LITEINST_ADVANCED_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("liteinst-advanced");
        fs::create_dir_all(&build_root).expect("failed to create LiteInst guest directory");
        let guest = build_root.join("liteinst_advanced");
        let output = Command::new("cc")
            .args(["-O2", "-g", "-Wall", "-Wextra", "-Werror", "-pthread"])
            .arg(repository.join("tests/c/liteinst_advanced.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile LiteInst advanced guest");
        assert!(
            output.status.success(),
            "LiteInst advanced guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn run_liteinst(program: &Path, args: &[&str], verify: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command.args(["run", "--backend", "liteinst", "--strict"]);
    if verify {
        command.arg("--verify");
    }
    command.arg("--").arg(program).args(args);
    command.output().expect("failed to run Hermit LiteInst")
}

fn assert_l2(program: &Path, args: &[&str], expected_stdout: &[u8]) {
    let output = run_liteinst(program, args, true);
    assert!(
        output.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(output.stdout, expected_stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("liteinst backend] Detcore Tool active"),
        "{stderr}"
    );
    assert!(
        stderr.contains("Success: deterministic. Determinism verified."),
        "{stderr}"
    );
    assert!(
        stderr.contains("LiteInst (reverie-liteinst LiteinstGuest<Detcore>)"),
        "{stderr}"
    );
}

#[test]
fn liteinst_detcore_strict_verify_micro_suite() {
    assert_l2(Path::new("/bin/true"), &[], b"");
    assert_l2(Path::new("/bin/echo"), &["hello"], b"hello\n");

    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let readme = repository.join("README.md");
    let expected = fs::read(&readme).expect("read README fixture");
    assert_l2(
        Path::new("/bin/cat"),
        &[readme.to_str().unwrap()],
        &expected,
    );
}

fn assert_clone_boundary(mode: &str, operation: &str) {
    let output = run_liteinst(advanced_guest(), &[mode], false);
    assert_eq!(
        output.status.code(),
        Some(1),
        "status={:?}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(operation), "{stderr}");
    assert!(!stderr.contains("Bad system call"), "{stderr}");
}

#[test]
fn liteinst_thread_clone_fails_closed_without_sigsys() {
    assert_clone_boundary("threads", "pthread_create: Operation not supported");
}

#[test]
fn liteinst_fork_fails_closed_without_hanging() {
    assert_clone_boundary("fork", "fork: Operation not supported");
}
