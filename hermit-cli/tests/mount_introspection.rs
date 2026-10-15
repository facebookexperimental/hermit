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
    let rendered = format!("{command:?}");
    let output = hermit_test::hermit_output(&rendered, &mut command);
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
fn mount_introspection_syscalls_fall_back_deterministically() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("mount-introspection");
    fs::create_dir_all(&build_root).expect("failed to create guest build directory");

    let cases = [
        ("sysfs", repository.join("tests/c/sysfs_enosys.c")),
        ("statmount", repository.join("tests/c/statmount_enosys.c")),
        ("listmount", repository.join("tests/c/listmount_enosys.c")),
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
            .arg(env!("CARGO_BIN_EXE_hermit"))
            .args([
                "--log=info",
                "--backend=ptrace",
                "run",
                "--strict",
                "--verify",
                "--panic-on-unsupported-syscalls",
                "--base-env=minimal",
                "--",
            ])
            .arg(&guest);
        let verify_output = command_output(verify, &format!("{syscall} strict verification"));
        let stdout = String::from_utf8_lossy(&verify_output.stdout);
        let stderr = String::from_utf8_lossy(&verify_output.stderr);
        assert!(
            stdout.contains("Determinism verified") || stderr.contains("Determinism verified"),
            "{syscall} omitted Hermit's determinism marker\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }
}

/// Compile tests/c/mountinfo_reread.c into this test's build directory.
fn mountinfo_reread_guest(build_root: &Path) -> std::path::PathBuf {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    fs::create_dir_all(build_root).expect("failed to create guest build directory");
    let guest = build_root.join("mountinfo_reread");
    let mut compile = Command::new("cc");
    compile
        .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(repository.join("tests/c/mountinfo_reread.c"))
        .arg("-o")
        .arg(&guest);
    command_output(compile, "mountinfo_reread guest compilation");
    guest
}

/// The guest's mounts carry no propagation tag (`master:N`, `shared:N`): the
/// container remounts its tree private before any other mount, so host mount
/// events cannot reach it. A mount namespace made inside a user namespace
/// starts as a slave of the host's; before, 156 of 158 rows carried
/// `master:N`, and a host mount during a run changed the guest's mount table
/// (https://github.com/rrnewton/hermit/issues/4167).
#[test]
fn guest_mounts_receive_no_host_propagation() {
    let mut run = Command::new("timeout");
    run.args(["--kill-after", "5s", "90s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "run",
            "--strict",
            "--base-env=minimal",
            "--",
            "/bin/cat",
            "/proc/self/mountinfo",
        ]);
    let output = command_output(run, "guest mountinfo read");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.lines().count() > 1, "no mountinfo rows:\n{stdout}");
    let tagged: Vec<&str> = stdout
        .lines()
        .filter(|line| {
            line.split(" - ")
                .next()
                .unwrap_or("")
                .split_whitespace()
                .any(|field| field.starts_with("master:") || field.starts_with("shared:"))
        })
        .collect();
    assert!(
        tagged.is_empty(),
        "guest mounts carry propagation tags:\n{}",
        tagged.join("\n")
    );
}

/// Host mounts made and removed during a run do not reach the guest. Hermit
/// runs inside a user and mount namespace whose mounts are shared, while a
/// loop mounts and unmounts a tmpfs there; the guest rereads its mountinfo
/// 500 times (within the default per-test CPU budget of 22 s). Before the container's private remount, the first read after a
/// mount event aborted the run ("mountinfo snapshot failed strict identity
/// validation"), which killed `go build` within minutes
/// (https://github.com/rrnewton/hermit/issues/4167).
#[test]
fn host_mount_events_during_a_run_leave_the_guest_mountinfo_unchanged() {
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("mountinfo-propagation");
    let guest = mountinfo_reread_guest(&build_root);
    let mount_point = build_root.join("churn");
    fs::create_dir_all(&mount_point).expect("failed to create the churn mount point");
    let script = r#"
        churn_point=$1; shift
        ( while :; do
            mount -t tmpfs none "$churn_point" || exit 1
            sleep 0.02
            umount "$churn_point" || exit 1
            sleep 0.02
          done ) &
        churn=$!
        sleep 0.2
        kill -0 "$churn" || { echo "the mount churn did not start" >&2; exit 3; }
        "$@"
        status=$?
        kill "$churn"
        exit "$status"
    "#;
    let mut run = Command::new("unshare");
    run.args([
        "--user",
        "--map-root-user",
        "--mount",
        "--propagation",
        "shared",
        "sh",
        "-c",
        script,
        "sh",
    ])
    .arg(&mount_point)
    .args(["timeout", "--kill-after", "5s", "120s"])
    .arg(env!("CARGO_BIN_EXE_hermit"))
    .args(["run", "--strict", "--base-env=minimal", "--"])
    .arg(&guest)
    .arg("500");
    let output = command_output(run, "guest mountinfo rereads during host mount churn");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout, "500 reads; lines constant\n");
}
