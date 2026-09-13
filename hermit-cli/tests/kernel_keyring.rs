/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::fs;
use std::path::Path;
use std::process::Command;
use std::process::Output;

fn command_output(mut command: Command, label: &str) -> Output {
    hermit_test::configure_guest_execution(&mut command);
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
fn kernel_keyring_is_deterministically_unavailable() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("kernel-keyring");
    fs::create_dir_all(&build_root).expect("failed to create guest build directory");

    let cases = [
        ("add_key", repository.join("tests/c/add_key_enosys.c")),
        (
            "request_key",
            repository.join("tests/c/request_key_enosys.c"),
        ),
        ("keyctl", repository.join("tests/c/keyctl_enosys.c")),
    ];

    for (syscall, source) in cases {
        let guest = build_root.join(format!("{syscall}_enosys"));
        let mut compile = Command::new("cc");
        compile
            .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
            .arg(source)
            .arg("-o")
            .arg(&guest);
        command_output(compile, &format!("{syscall} guest compilation"));

        let mut verify = Command::new("timeout");
        verify
            .args(["--kill-after", "5s", "90s"])
            .arg(hermit_test::hermit_binary())
            .args([
                "--log=info",
                "run",
                "--backend=ptrace",
                "--strict",
                "--verify",
                "--panic-on-unsupported-syscalls",
                "--base-env=minimal",
                "--",
            ])
            .arg(&guest);
        let output = command_output(verify, &format!("{syscall} strict verification"));
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stdout.contains("Determinism verified") || stderr.contains("Determinism verified"),
            "{syscall} omitted Hermit's determinism marker\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }
}

/// Extracts the single `keyctl_enosys=<0|1>` line printed by the
/// `keyctl_passthrough` guest, panicking with full context if it is absent.
fn keyctl_enosys_flag(output: &Output, label: &str) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .find(|line| line.starts_with("keyctl_enosys="))
        .unwrap_or_else(|| {
            panic!(
                "{label} did not report a keyctl_enosys flag\nstdout:\n{stdout}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stderr)
            )
        })
        .to_string()
}

/// The explicit unsupported-syscall compatibility opt-out must pass
/// kernel-keyring syscalls through to the host rather than forcing the default
/// fail-closed `ENOSYS` boundary. This is a regression guard for the enabled rr
/// `keyctl` compatibility test. The check is host-independent: it compares the
/// guest's ENOSYS-or-not verdict natively against the same guest under the
/// opt-out, so it holds whether or not the host kernel has `CONFIG_KEYS`.
#[test]
fn kernel_keyring_passes_through_with_compatibility_opt_out() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("kernel-keyring-passthrough");
    fs::create_dir_all(&build_root).expect("failed to create guest build directory");

    let source = repository.join("tests/c/keyctl_passthrough.c");
    let guest = build_root.join("keyctl_passthrough");
    let mut compile = Command::new("cc");
    compile
        .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(&source)
        .arg("-o")
        .arg(&guest);
    command_output(compile, "keyctl passthrough guest compilation");

    // Native baseline: whatever the host kernel reports for keyctl.
    let native = Command::new(&guest);
    let native_output = command_output(native, "native keyctl passthrough");
    let native_flag = keyctl_enosys_flag(&native_output, "native keyctl passthrough");

    // The explicit compatibility opt-out must let the keyring syscall reach
    // the host and observe the same verdict.
    let mut hermit = Command::new("timeout");
    hermit
        .args(["--kill-after", "5s", "90s"])
        .arg(hermit_test::hermit_binary())
        .args([
            "--log=off",
            "run",
            "--backend=ptrace",
            "--base-env=minimal",
            "--allow-unsupported-syscalls",
            "--",
        ])
        .arg(&guest);
    let hermit_output = command_output(hermit, "explicit Hermit keyctl compatibility opt-out");
    let hermit_flag = keyctl_enosys_flag(
        &hermit_output,
        "explicit Hermit keyctl compatibility opt-out",
    );

    assert_eq!(
        hermit_flag, native_flag,
        "Hermit's explicit compatibility opt-out produced a different keyctl result than the host: \
         native reported `{native_flag}`, Hermit reported `{hermit_flag}` \
         (a Hermit-forced ENOSYS here would break the rr keyctl compatibility test)"
    );
}
