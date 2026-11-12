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
//! attempt. Requiring the retry line keeps the test from passing merely
//! because a replacement missed the setup window.

use std::io::Read;
use std::io::Seek;
use std::path::Path;
use std::process::Command;
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

#[test]
fn launches_survive_ld_so_cache_replacement_during_setup() {
    // The fixture needs an outer user+mount namespace; without one there
    // is nothing this test can replace safely.
    let probe = run(Command::new("unshare").args(["-rm", "true"]));
    if !probe.status.success() {
        eprintln!("skip: unshare -rm unavailable: {probe:?}");
        return;
    }
    let Ok(cache_bytes) = std::fs::read("/etc/ld.so.cache") else {
        eprintln!("skip: host has no /etc/ld.so.cache");
        return;
    };
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let etc = directory.path().join("etc");
    std::fs::create_dir(&etc).unwrap();
    std::fs::write(etc.join("ld.so.cache"), &cache_bytes).unwrap();
    std::fs::write(etc.join("ld.so.cache.next"), &cache_bytes).unwrap();
    if let Ok(group) = std::fs::read("/etc/group") {
        std::fs::write(etc.join("group"), group).unwrap();
    }

    let hermit = env!("CARGO_BIN_EXE_hermit");
    let launch = |etc: &Path, stderr_path: &Path| {
        let script = format!(
            "mount --bind {} /etc && exec {} run --strict {} -- /bin/sh -c 'cat /etc/ld.so.cache > /dev/null'",
            etc.display(),
            hermit,
            detcore_model::config::CANONICAL_TEST_EPOCH_ARG,
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
    };

    // Probe: nested hermit must work on this host at all before any
    // replacement races it.
    let probe_stderr = directory.path().join("probe-stderr");
    let probe = launch(&etc, &probe_stderr)
        .wait_with_output()
        .expect("probe launch");
    if !probe.status.success() {
        eprintln!(
            "skip: nested hermit probe failed: {}\n{}",
            probe.status,
            stderr_contents(&probe_stderr)
        );
        return;
    }

    let stop = Arc::new(AtomicBool::new(false));
    let replacer = {
        let stop = Arc::clone(&stop);
        let etc = etc.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let flip = etc.join("ld.so.cache.flip");
                if std::fs::copy(etc.join("ld.so.cache.next"), &flip).is_ok() {
                    let _ = std::fs::rename(&flip, etc.join("ld.so.cache"));
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        })
    };

    let stderr_path = directory.path().join("stderr");
    let mut child = launch(&etc, &stderr_path);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(55);
    let status = loop {
        if stderr_contents(&stderr_path).contains("retrying with a fresh snapshot") {
            stop.store(true, Ordering::Relaxed);
        }
        match child.try_wait().expect("poll launch") {
            Some(status) => break status,
            None => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "launch never finished\n{}",
                    stderr_contents(&stderr_path)
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    };
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
