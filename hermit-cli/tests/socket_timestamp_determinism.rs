/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::path::Path;
use std::process::Command;

/// The line Hermit prints when it runs the guest under in-guest LiteInst.
#[cfg(feature = "liteinst")]
const IN_GUEST_SELECTED: &str =
    "hermit: [liteinst in-guest] selected: the guest preload is to host the Detcore Tool";

/// Runs `guest` under in-guest LiteInst (https://github.com/rrnewton/hermit/issues/3520)
/// twice and returns its stdout. In-guest LiteInst refuses a maximum timeslice
/// until it can deliver Detcore's preemption timer, so these runs have none,
/// and they do not pass `--verify` yet. The guest checks its own timestamps
/// against logical time and fails if one escapes; the second run must print the
/// same output. Both runs pin
/// the same `--epoch`, as `--verify` pins one epoch for its two runs; without
/// it each run starts its virtual clock at the host's current time.
#[cfg(feature = "liteinst")]
fn run_in_guest_liteinst_twice(guest: &Path, args: &[&str], label: &str) -> String {
    let run = || {
        let mut command = Command::new("timeout");
        command
            .args(["--kill-after", "5s", "90s"])
            .arg(hermit_test::hermit_binary())
            .args([
                "--log=info",
                "--backend=liteinst",
                "run",
                "--epoch=2026-01-01T00:00:00Z",
            ])
            .args([
                "--strict",
                "--max-timeslice=disabled",
                "--base-env=minimal",
                "--",
            ])
            .arg(guest)
            .args(args);
        hermit_test::configure_guest_execution(&mut command);
        let output = hermit_test::hermit_output(&format!("{command:?}"), &mut command);
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "liteinst/{label} failed: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            output.status
        );
        assert!(
            stderr.lines().any(|line| line == IN_GUEST_SELECTED),
            "liteinst/{label} did not run under in-guest LiteInst\nstderr:\n{stderr}"
        );
        stdout
    };
    let first = run();
    assert!(!first.is_empty(), "liteinst/{label} printed nothing");
    assert_eq!(
        run(),
        first,
        "liteinst/{label} printed different output on its second run"
    );
    first
}

/// tests/c/socket_timestamp_edge_cases.c under strict ptrace verification at
/// epochs whose fraction puts a second boundary between its first receive and
/// its batched receive. The guest used to require the batch to share the
/// first timestamp's whole second, so it failed there: with the preemption
/// timer near .995 (a window of about 3 ms), and without it, where virtual time
/// runs 500 times faster, at the epoch .564328890 a GitHub-hosted run used
/// (https://github.com/rrnewton/hermit/actions/runs/37707343949).
#[test]
fn socket_timestamp_edge_cases_hold_at_every_epoch_fraction() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("socket-timestamp-epochs");
    std::fs::create_dir_all(&build_root).expect("failed to create guest build directory");
    let guest = build_root.join("socket_timestamp_edge_cases");
    let compile = Command::new("cc")
        .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(repository.join("tests/c/socket_timestamp_edge_cases.c"))
        .arg("-o")
        .arg(&guest)
        .output()
        .expect("failed to compile socket_timestamp_edge_cases");
    assert!(
        compile.status.success(),
        "socket_timestamp_edge_cases compilation failed: {}",
        String::from_utf8_lossy(&compile.stderr)
    );
    for (epoch, timeslice) in [
        ("2026-10-08T01:05:18.995000000+00:00", None),
        (
            "2026-10-08T01:05:18.564328890+00:00",
            Some("--max-timeslice=disabled"),
        ),
    ] {
        let mut command = Command::new("timeout");
        command
            .args(["--kill-after", "5s", "90s"])
            .arg(hermit_test::hermit_binary())
            .args(["--log=info", "--backend=ptrace", "run"])
            .arg(format!("--epoch={epoch}"))
            .args(timeslice)
            .args(["--strict", "--verify", "--base-env=minimal", "--"])
            .arg(&guest);
        hermit_test::configure_guest_execution(&mut command);
        let verify = hermit_test::hermit_output(&format!("{command:?}"), &mut command);
        let stdout = String::from_utf8_lossy(&verify.stdout);
        let stderr = String::from_utf8_lossy(&verify.stderr);
        assert!(
            verify.status.success() && stdout.contains("batch=ok"),
            "epoch {epoch} {timeslice:?}: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            verify.status
        );
    }
}

#[test]
fn socket_receive_timestamps_use_logical_time() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("socket-timestamp-determinism");
    std::fs::create_dir_all(&build_root).expect("failed to create guest build directory");

    for name in [
        "socket_timestamp_timeval",
        "socket_timestamp_timespec",
        "socket_timestamp_edge_cases",
    ] {
        let source = repository.join(format!("tests/c/{name}.c"));
        let guest = build_root.join(name);
        let compile = Command::new("cc")
            .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
            .arg(source)
            .arg("-o")
            .arg(&guest)
            .output()
            .unwrap_or_else(|error| panic!("failed to compile {name}: {error}"));
        assert!(
            compile.status.success(),
            "{name} compilation failed: {}",
            String::from_utf8_lossy(&compile.stderr)
        );

        for backend in ["ptrace", "dbt"] {
            let mut command = Command::new("timeout");
            command
                .args(["--kill-after", "5s", "90s"])
                .arg(hermit_test::hermit_binary())
                .arg("--log=info")
                .arg(format!("--backend={backend}"))
                .arg("run")
                .args(["--strict", "--verify", "--base-env=minimal", "--"])
                .arg(&guest);
            hermit_test::configure_guest_execution(&mut command);
            let verify = hermit_test::hermit_output(&format!("{command:?}"), &mut command);
            let stdout = String::from_utf8_lossy(&verify.stdout);
            let stderr = String::from_utf8_lossy(&verify.stderr);
            assert!(
                verify.status.success(),
                "{backend}/{name} strict verification failed: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
                verify.status
            );
            assert!(
                stdout.contains("Determinism verified") || stderr.contains("Determinism verified"),
                "{backend}/{name} omitted Hermit's determinism marker\nstdout:\n{stdout}\nstderr:\n{stderr}"
            );
        }

        if name == "socket_timestamp_timeval" {
            let mut command = Command::new("timeout");
            command
                .args(["--kill-after", "5s", "90s"])
                .arg(hermit_test::hermit_binary())
                .args([
                    "--log=off",
                    "--backend=ptrace",
                    "run",
                    "--no-virtualize-time",
                    "--no-virtualize-metadata",
                    "--base-env=minimal",
                    "--",
                ])
                .arg(&guest);
            hermit_test::configure_guest_execution(&mut command);
            let realtime = hermit_test::hermit_output(&format!("{command:?}"), &mut command);
            assert!(
                realtime.status.success(),
                "host-clock socket timestamp case failed: {}\nstdout:\n{}\nstderr:\n{}",
                realtime.status,
                String::from_utf8_lossy(&realtime.stdout),
                String::from_utf8_lossy(&realtime.stderr)
            );
            // The guest checks the timestamp against its own clock_gettime,
            // which reads the host clock under --no-virtualize-time, and
            // prints "timestamp=ok" only if it is within a second of it (it
            // printed the raw timeval before 6f2969722b29).
            assert_eq!(
                String::from_utf8_lossy(&realtime.stdout).trim(),
                "timestamp=ok",
                "host-clock socket timestamp case: stderr:\n{}",
                String::from_utf8_lossy(&realtime.stderr)
            );
        }
    }
}

/// The three receive-timestamp guests under in-guest LiteInst. A separate test
/// from the ptrace and DBT one above so that its host-clock leg, which runs
/// only for the first guest, cannot hide the in-guest result for the others.
#[test]
#[cfg(feature = "liteinst")]
fn socket_receive_timestamps_use_logical_time_in_guest_liteinst() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("socket-timestamp-in-guest");
    std::fs::create_dir_all(&build_root).expect("failed to create guest build directory");
    for (name, expected) in [
        ("socket_timestamp_timeval", "timestamp=ok\n"),
        ("socket_timestamp_timespec", "timestampns=ok\n"),
        (
            "socket_timestamp_edge_cases",
            "truncated=ok alias=ok batch=ok\n",
        ),
    ] {
        let guest = build_root.join(name);
        let compile = Command::new("cc")
            .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join(format!("tests/c/{name}.c")))
            .arg("-o")
            .arg(&guest)
            .output()
            .unwrap_or_else(|error| panic!("failed to compile {name}: {error}"));
        assert!(
            compile.status.success(),
            "{name} compilation failed: {}",
            String::from_utf8_lossy(&compile.stderr)
        );
        assert_eq!(run_in_guest_liteinst_twice(&guest, &[], name), expected);
    }
}
