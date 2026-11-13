/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A launch whose container setup straddles a host replacement of
//! `/etc/ld.so.cache` must succeed by retrying setup, never refuse with
//! exit 125: a package manager's `ldconfig` rename detaches Hermit's
//! frozen bind in the guest namespace
//! (https://github.com/rrnewton/hermit/issues/4799, dh-8k6lh4).
//!
//! The fixture runs hermit inside an outer user+mount namespace whose
//! `/etc` is a test-owned directory, so the test can rename-replace the
//! cache Hermit freezes without touching the real `/etc`. A replacer
//! thread replaces the cache continuously from before the launch until
//! hermit's own stderr proves a setup attempt was detached and is being
//! retried; the launch must then complete successfully on a later
//! attempt. Requiring the retry line keeps the tests from passing
//! merely because a replacement missed the setup window. The
//! exhaustion test never stops the replacer: every attempt is detached
//! and the bound is reached, which only a replacement landing in every
//! attempt's bind-to-record window can defeat — at a sub-millisecond
//! replacement period against a multi-millisecond window that race is
//! not realistically winnable.

use std::io::Read;
use std::io::Seek;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

fn run(command: &mut Command) -> std::process::Output {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn")
}

fn stderr_contents(path: &Path) -> String {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(_) => return String::new(),
    };
    let mut text = String::new();
    let _ = file.seek(std::io::SeekFrom::Start(0));
    let _ = file.read_to_string(&mut text);
    text
}

/// The test-owned `/etc` and its tempdir, or `None` after printing the
/// skip reason (no outer namespace, no host cache, or nested hermit
/// does not run on this host).
struct Fixture {
    _directory: tempfile::TempDir,
    etc: PathBuf,
}

fn launch(fixture: &Fixture, stderr_path: &Path, extra_args: &str) -> std::process::Child {
    let hermit = env!("CARGO_BIN_EXE_hermit");
    let script = format!(
        "mount --bind {} /etc && exec {} run --strict {} {} -- /bin/sh -c 'cat /etc/ld.so.cache > /dev/null'",
        fixture.etc.display(),
        hermit,
        detcore_model::config::CANONICAL_TEST_EPOCH_ARG,
        extra_args,
    );
    let stderr = std::fs::File::create(stderr_path).expect("create stderr file");
    Command::new("timeout")
        .args([
            "--kill-after",
            "5s",
            "60s",
            "unshare",
            "-rm",
            "sh",
            "-c",
            &script,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .expect("spawn launch")
}

fn fixture() -> Option<Fixture> {
    let probe = run(Command::new("unshare").args(["-rm", "true"]));
    if !probe.status.success() {
        eprintln!("skip: unshare -rm unavailable: {probe:?}");
        return None;
    }
    let cache_bytes = std::fs::read("/etc/ld.so.cache").ok()?;
    if cache_bytes.is_empty() {
        eprintln!("skip: host has no /etc/ld.so.cache");
        return None;
    }
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let etc = directory.path().join("etc");
    std::fs::create_dir(&etc).unwrap();
    std::fs::write(etc.join("ld.so.cache"), &cache_bytes).unwrap();
    std::fs::write(etc.join("ld.so.cache.next"), &cache_bytes).unwrap();
    std::fs::write(etc.join("group.next"), []).unwrap();
    if let Ok(group) = std::fs::read("/etc/group") {
        std::fs::write(etc.join("group"), &group).unwrap();
        std::fs::write(etc.join("group.next"), &group).unwrap();
    }
    let fixture = Fixture {
        _directory: directory,
        etc,
    };
    // Probe: nested hermit must work on this host at all before any
    // replacement races it.
    let probe_stderr = fixture.etc.parent().unwrap().join("probe-stderr");
    let probe = launch(&fixture, &probe_stderr, "")
        .wait_with_output()
        .expect("probe launch");
    if !probe.status.success() {
        eprintln!(
            "skip: nested hermit probe failed: {}\n{}",
            probe.status,
            stderr_contents(&probe_stderr)
        );
        return None;
    }
    Some(fixture)
}

fn start_replacer(etc: &Path, stop: &Arc<AtomicBool>, pause: bool) -> std::thread::JoinHandle<()> {
    let stop = Arc::clone(stop);
    let etc = etc.to_path_buf();
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            for name in ["ld.so.cache", "group"] {
                let flip = etc.join(format!("{name}.flip"));
                if std::fs::copy(etc.join(format!("{name}.next")), &flip).is_ok() {
                    let _ = std::fs::rename(&flip, etc.join(name));
                }
            }
            if pause {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
    })
}

fn wait_launch(
    child: &mut std::process::Child,
    stderr_path: &Path,
    stop_on_retry: Option<&Arc<AtomicBool>>,
) -> ExitStatus {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(55);
    loop {
        if let Some(stop) = stop_on_retry
            && stderr_contents(stderr_path).contains("retrying with a fresh snapshot")
        {
            stop.store(true, Ordering::Relaxed);
        }
        match child.try_wait().expect("poll launch") {
            Some(status) => return status,
            None => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "launch never finished\n{}",
                    stderr_contents(stderr_path)
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }
}

#[test]
fn launches_survive_ld_so_cache_replacement_during_setup() {
    let Some(fixture) = fixture() else { return };
    let stop = Arc::new(AtomicBool::new(false));
    let replacer = start_replacer(&fixture.etc, &stop, true);
    let stderr_path = fixture.etc.parent().unwrap().join("stderr");
    let mut child = launch(&fixture, &stderr_path, "");
    let status = wait_launch(&mut child, &stderr_path, Some(&stop));
    stop.store(true, Ordering::Relaxed);
    let _ = replacer.join();

    let stderr = stderr_contents(&stderr_path);
    assert!(
        stderr.contains("retrying with a fresh snapshot"),
        "continuous replacement never detached a setup attempt\n{stderr}"
    );
    assert!(
        status.success(),
        "launch refused across a setup replacement: {status}\n{stderr}"
    );
}

/// The same race through `--verify`: each leg's setup retries the same
/// way (the pair rerun cannot cover a leg that never started), and the
/// final report verifies with no discarded pair.
#[test]
fn verify_legs_survive_ld_so_cache_replacement_during_setup() {
    let Some(fixture) = fixture() else { return };
    let stop = Arc::new(AtomicBool::new(false));
    let replacer = start_replacer(&fixture.etc, &stop, true);
    let root = fixture.etc.parent().unwrap();
    let stderr_path = root.join("verify-stderr");
    let report_path = root.join("verify.json");
    let extra = format!(
        "--verify --verify-strict --verify-json {}",
        report_path.display()
    );
    let mut child = launch(&fixture, &stderr_path, &extra);
    let status = wait_launch(&mut child, &stderr_path, Some(&stop));
    stop.store(true, Ordering::Relaxed);
    let _ = replacer.join();

    let stderr = stderr_contents(&stderr_path);
    assert!(
        stderr.contains("retrying with a fresh snapshot"),
        "continuous replacement never detached a leg setup attempt\n{stderr}"
    );
    assert!(
        status.success(),
        "verify launch refused across a setup replacement: {status}\n{stderr}"
    );
    let report = hermit::canonical_verdict::VerificationReport::from_current_json_slice(
        &std::fs::read(&report_path).expect("read verify json"),
    )
    .expect("parse verify json");
    assert!(report.verified, "{stderr}");
    assert!(report.bitwise_parity, "{stderr}");
    assert!(report.discarded_attempts.is_empty(), "{stderr}");
}

/// The bound itself: with the replacer never stopping, every setup
/// attempt is detached and the launch refuses, naming the attempts,
/// instead of retrying forever.
#[test]
fn setup_retry_exhaustion_refuses_naming_the_attempts() {
    let Some(fixture) = fixture() else { return };
    let stop = Arc::new(AtomicBool::new(false));
    let replacer = start_replacer(&fixture.etc, &stop, false);
    let stderr_path = fixture.etc.parent().unwrap().join("exhaustion-stderr");
    let mut child = launch(&fixture, &stderr_path, "");
    let status = wait_launch(&mut child, &stderr_path, None);
    stop.store(true, Ordering::Relaxed);
    let _ = replacer.join();

    let stderr = stderr_contents(&stderr_path);
    assert_eq!(
        status.code(),
        Some(hermit::HERMIT_INTERNAL_FAILURE_EXIT),
        "{stderr}"
    );
    assert!(
        stderr.contains("still detached after 5 attempts"),
        "exhaustion must name the attempts\n{stderr}"
    );
    assert_eq!(
        stderr.matches("retrying with a fresh snapshot").count(),
        4,
        "four retries precede the fifth, refusing attempt\n{stderr}"
    );
}
