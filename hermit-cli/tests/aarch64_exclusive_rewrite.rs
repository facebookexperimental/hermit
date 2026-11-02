/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! aarch64: `--rewrite-exclusives` under `--no-namespace` rewrites a guest's
//! load/store-exclusive retry loops in its memory, end to end through the
//! CLI. The guest, its loader and its libc run from tmpfs: a file opened by
//! name is proven to be the mapped one only outside btrfs and overlayfs, and
//! an unprivileged Hermit cannot open `/proc/<pid>/map_files`.

#![cfg(target_arch = "aarch64")]

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

/// What the guest prints: 4 threads of 10000 additions each.
const EXPECTED: &str = "counter 40000\n";

/// The paths of this test process's own dynamic loader, libc and, before
/// glibc 2.34 merged it into libc, libpthread, which the guest runs with.
fn host_libraries() -> Vec<PathBuf> {
    let maps = fs::read_to_string("/proc/self/maps").unwrap();
    let mapped = |name: &str| {
        maps.lines()
            .filter_map(|line| line.split_whitespace().nth(5))
            .find(|path| Path::new(path).file_name().is_some_and(|file| file == name))
            .map(PathBuf::from)
    };
    let mut libraries: Vec<PathBuf> = ["ld-linux-aarch64.so.1", "libc.so.6"]
        .into_iter()
        .map(|name| {
            mapped(name).unwrap_or_else(|| panic!("this test process maps no {name}:\n{maps}"))
        })
        .collect();
    libraries.extend(mapped("libpthread.so.0"));
    libraries
}

/// Why `/dev/shm` cannot hold the guest here, if it cannot: a `noexec`
/// mount runs nothing from it, and on btrfs or overlayfs the in-memory
/// rewrite cannot prove a mapped file by its inode, so without
/// `/proc/<pid>/map_files` (`CAP_CHECKPOINT_RESTORE` or `CAP_SYS_ADMIN`) it
/// would write nothing there.
fn shm_cannot_run_code() -> Option<String> {
    // SAFETY: an all-zero `statvfs` is a valid buffer.
    let mut fs: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: a NUL-terminated path and a `statvfs` buffer.
    if unsafe { libc::statvfs(c"/dev/shm".as_ptr(), &mut fs) } != 0 {
        return Some(format!("/dev/shm: {}", std::io::Error::last_os_error()));
    }
    if fs.f_flag & libc::ST_NOEXEC != 0 {
        return Some("/dev/shm is mounted noexec".to_owned());
    }
    const BTRFS_SUPER_MAGIC: i64 = 0x9123_683e;
    const OVERLAYFS_SUPER_MAGIC: i64 = 0x794c_7630;
    // SAFETY: an all-zero `statfs` is a valid buffer.
    let mut kind: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: a NUL-terminated path and a `statfs` buffer.
    if unsafe { libc::statfs(c"/dev/shm".as_ptr(), &mut kind) } != 0 {
        return Some(format!("/dev/shm: {}", std::io::Error::last_os_error()));
    }
    let unproven = [BTRFS_SUPER_MAGIC, OVERLAYFS_SUPER_MAGIC].contains(&(kind.f_type as i64));
    (unproven && !map_files_open()).then(|| {
        "/dev/shm is on btrfs or overlayfs, where the in-memory rewrite proves a mapped file \
         only through /proc/<pid>/map_files, which this process cannot open"
            .to_owned()
    })
}

/// Whether this process can open its own `/proc/self/map_files` entries,
/// which needs `CAP_CHECKPOINT_RESTORE` or `CAP_SYS_ADMIN`.
fn map_files_open() -> bool {
    fs::read_to_string("/proc/self/maps")
        .ok()
        .and_then(|maps| {
            let range = maps.lines().next()?.split_whitespace().next()?.to_owned();
            Some(fs::File::open(format!("/proc/self/map_files/{range}")).is_ok())
        })
        .unwrap_or(false)
}

/// One file's counts in the summary's `exclusive_rewrites`.
fn rewrite_of(summary: &serde_json::Value, path: &Path) -> (u64, u64, u64) {
    let entry = &summary["exclusive_rewrites"][path.to_str().unwrap()];
    let count = |field: &str| {
        entry[field]
            .as_u64()
            .unwrap_or_else(|| panic!("{} has no {field} in {summary:#}", path.display()))
    };
    (count("rewritten"), count("left"), count("failed"))
}

/// The guest's one retry loop is rewritten in its memory and still counts
/// exactly. Every file the guest maps is planned and written where the
/// rewriter accepts its loops: none is counted as not written. The host's
/// loader and libc are only logged: a loop the rewriter refuses, or one
/// across a page, is left by design, and an LSE-only libc has none. Where no
/// loop is left anywhere, a run that counts store-exclusives (one that
/// records preemptions) sees none fail.
///
/// It needs code it can run from `/dev/shm`, a C compiler (`cc`) and ptrace,
/// so it is `#[ignore]`d: only a run that asks for it (`--run-ignored`)
/// selects it, and one on a host whose `/dev/shm` is `noexec`, or is btrfs or
/// overlayfs without `/proc/<pid>/map_files` access, fails here, naming that,
/// instead of passing without checking anything or failing on its counts.
#[test]
#[ignore = "host-limited: runs the guest from /dev/shm and compiles it with cc; run with --run-ignored on an aarch64 host where both work (docs/TESTING_ENVIRONMENTS.md)"]
fn the_in_memory_rewrite_replaces_a_guests_loop_and_it_still_counts() {
    if let Some(reason) = shm_cannot_run_code() {
        panic!("this host cannot run this test: {reason}");
    }
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let dir = tempfile::tempdir_in("/dev/shm").expect("a directory on /dev/shm (tmpfs)");
    let mut libraries = Vec::new();
    for host in host_libraries() {
        let copy = dir.path().join(host.file_name().unwrap());
        fs::copy(&host, &copy).unwrap();
        libraries.push(copy);
    }
    let loader = libraries[0].clone();
    let guest = dir.path().join("exclusive_loop_counter");
    let compiled = Command::new("cc")
        .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror", "-pthread"])
        .arg(repository.join("tests/c/exclusive_loop_counter.c"))
        .arg("-o")
        .arg(&guest)
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );

    let summary_path = dir.path().join("summary.json");
    let record = dir.path().join("record.json");
    let mut run = Command::new("timeout");
    run.args(["--kill-after", "5s", "120s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=info",
            "run",
            "--no-namespace",
            "--rewrite-exclusives",
            "--base-env=minimal",
            "--preemption-timeout=disabled",
        ])
        .arg(format!("--summary-json={}", summary_path.display()))
        .arg(format!("--record-preemptions-to={}", record.display()))
        .arg("--")
        .arg(&loader)
        .arg("--library-path")
        .arg(dir.path())
        .arg(&guest);
    let rendered = format!("{run:?}");
    let output = hermit_test::hermit_output(&rendered, &mut run);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{rendered}\n{stderr}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        EXPECTED,
        "{stderr}"
    );

    let summary: serde_json::Value =
        serde_json::from_slice(&fs::read(&summary_path).unwrap()).unwrap();
    assert_eq!(rewrite_of(&summary, &guest), (1, 0, 0), "{summary:#}");
    // Every file with loops is one this test put on tmpfs: anything else
    // (a library the guest found on the host) cannot be proven and would be
    // counted as not written.
    let pairs = summary["exclusive_pairs"]
        .as_object()
        .unwrap_or_else(|| panic!("no exclusive_pairs in {summary:#}"));
    let mut left_anywhere = false;
    for (path, count) in pairs {
        assert!(
            Path::new(path).starts_with(dir.path()),
            "the guest mapped {path} from outside {}: {summary:#}",
            dir.path().display()
        );
        let (rewritten, left, failed) = rewrite_of(&summary, Path::new(path));
        assert_eq!(failed, 0, "{path} not written: {summary:#}");
        assert_eq!(Some(rewritten + left), count.as_u64(), "{summary:#}");
        left_anywhere |= left > 0;
        eprintln!("{path}: {rewritten} rewritten, {left} left");
    }
    // Counted where the host's PMU counts it: with no loop left, none can
    // fail, and where the counters ran for the whole run, no store-exclusive
    // ran at all.
    if let Some(counts) = summary.get("store_exclusives").filter(|_| !left_anywhere) {
        assert_eq!(counts["failed"].as_u64(), Some(0), "{summary:#}");
        if counts["fully_counted"].as_bool() == Some(true)
            && let Some(executed) = counts.get("executed")
        {
            assert_eq!(executed.as_u64(), Some(0), "{summary:#}");
        }
    }
}
