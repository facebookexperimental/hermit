/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! aarch64: `--rewrite-exclusives` under `--no-namespace` rewrites a guest's
//! load/store-exclusive retry loops in its memory, end to end through the
//! CLI. The guest, its loader and its libc run from an owned disk directory.
//! Btrfs and overlayfs need actual map_files access to prove their mappings;
//! disk-backed storage does not relax the rewriter's identity checks.

#![cfg(target_arch = "aarch64")]

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::fs;
use std::os::fd::AsRawFd;
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

/// Create fixtures only below an explicitly selected disk directory.
fn fixture_dir() -> tempfile::TempDir {
    let root = std::env::var_os("HERMIT_AARCH64_TEST_ROOT")
        .filter(|root| !root.is_empty())
        .map(PathBuf::from)
        .expect("this host cannot run this test: set HERMIT_AARCH64_TEST_ROOT to a disk directory");
    assert!(
        root.is_absolute(),
        "HERMIT_AARCH64_TEST_ROOT must be absolute"
    );
    let root = root.canonicalize().unwrap_or_else(|error| {
        panic!(
            "this host cannot run this test: {}: {error}",
            root.display()
        )
    });
    assert!(
        !["/tmp", "/dev/shm", "/run/user"]
            .iter()
            .any(|prefix| root.starts_with(prefix)),
        "this host cannot run this test: {} is a prohibited temporary location",
        root.display()
    );
    let directory = std::fs::File::open(&root).unwrap_or_else(|error| {
        panic!(
            "this host cannot run this test: {}: {error}",
            root.display()
        )
    });
    assert!(directory.metadata().unwrap().is_dir());
    // SAFETY: zeroed output buffers and a live descriptor for the actual root.
    let (mut mount, mut kind): (libc::statvfs, libc::statfs) = unsafe { std::mem::zeroed() };
    // SAFETY: both output buffers are writable and the descriptor stays open.
    let inspected = unsafe {
        libc::fstatvfs(directory.as_raw_fd(), &mut mount) == 0
            && libc::fstatfs(directory.as_raw_fd(), &mut kind) == 0
    };
    assert!(
        inspected,
        "this host cannot run this test: inspect {}: {}",
        root.display(),
        std::io::Error::last_os_error()
    );
    assert!(
        mount.f_flag & libc::ST_NOEXEC == 0,
        "this host cannot run this test: {} is mounted noexec",
        root.display()
    );
    // TMPFS_MAGIC and RAMFS_MAGIC: a pathname alone does not prove disk backing.
    assert!(
        ![0x0102_1994, 0x8584_58f6].contains(&(kind.f_type as u64)),
        "this host cannot run this test: {} is not disk-backed",
        root.display()
    );
    tempfile::tempdir_in(&root).unwrap_or_else(|error| {
        panic!(
            "this host cannot run this test: create in {}: {error}",
            root.display()
        )
    })
}

/// An unreliable filesystem needs access to this owned file's actual mapping.
/// This is a prerequisite probe; the guest's real rewrite results still decide
/// whether all its mappings were proved and written successfully.
fn require_mapping_proof(path: &Path) {
    let file = fs::File::open(path).unwrap();
    // SAFETY: zeroed output buffer and a live descriptor for the chosen file.
    let mut kind: libc::statfs = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::fstatfs(file.as_raw_fd(), &mut kind) },
        0,
        "this host cannot run this test: statfs {}: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
    if ![0x9123_683e, 0x794c_7630].contains(&(kind.f_type as u64)) {
        return;
    }
    // SAFETY: querying page geometry has no pointer arguments.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    assert!(
        page_size > 0,
        "this host cannot run this test: unknown page size"
    );
    let len = page_size as usize;
    // SAFETY: a new readable private mapping of the owned, nonempty guest file.
    let address = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            file.as_raw_fd(),
            0,
        )
    };
    assert_ne!(
        address,
        libc::MAP_FAILED,
        "this host cannot run this test: mmap: {}",
        std::io::Error::last_os_error()
    );
    let start = address as usize;
    let exact = fs::File::open(format!("/proc/self/map_files/{start:x}-{:x}", start + len));
    // SAFETY: unmap exactly the mapping above, before asserting the probe result.
    assert_eq!(unsafe { libc::munmap(address, len) }, 0);
    assert!(
        exact.is_ok(),
        "this host cannot run this test: {} is on btrfs/overlayfs and its map_files entry cannot be opened: {:?}",
        path.display(),
        exact.err()
    );
}

/// The explicit launcher is a path to safehermit, or `official` only when the
/// maintained validation/manifest runner already owns this run's bounds.
/// The latter is a caller declaration, not a technical proof of those bounds.
fn hermit_command() -> Command {
    let selected = hermit_test::hermit_binary();
    let launcher = std::env::var_os("HERMIT_AARCH64_TEST_LAUNCHER")
        .filter(|value| !value.is_empty())
        .expect("this host cannot run this test: set HERMIT_AARCH64_TEST_LAUNCHER to the absolute safehermit path (or official inside a maintained runner)");
    if launcher == "official" {
        return Command::new(selected);
    }
    let launcher = PathBuf::from(launcher);
    assert!(
        launcher.is_absolute(),
        "HERMIT_AARCH64_TEST_LAUNCHER must be an absolute safehermit path"
    );
    let mut command = Command::new(launcher);
    command
        .args(["--sh-deadline", "120", "--sh-bin"])
        .arg(selected);
    command
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
/// It needs an executable disk fixture directory, a C compiler and ptrace.
/// Explicit selection on an unsuitable host fails by name, never passes early.
#[test]
#[ignore = "host-limited: needs HERMIT_AARCH64_TEST_ROOT, HERMIT_AARCH64_TEST_LAUNCHER, cc and aarch64 ptrace (docs/TESTING_ENVIRONMENTS.md)"]
fn the_in_memory_rewrite_replaces_a_guests_loop_and_it_still_counts() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let dir = fixture_dir();
    let mut libraries = Vec::new();
    for host in host_libraries() {
        let copy = dir.path().join(host.file_name().unwrap());
        fs::copy(&host, &copy).unwrap();
        libraries.push(copy);
    }
    let loader = libraries[0].clone();
    let guest = dir.path().join("exclusive_loop_counter");
    let compiled = Command::new("cc")
        .env("TMPDIR", dir.path())
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

    require_mapping_proof(&guest);
    let summary_path = dir.path().join("summary.json");
    let record = dir.path().join("record.json");
    let mut run = hermit_command();
    run.env("TMPDIR", dir.path());
    run.args([
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
    // Every file with loops is one this test put in its disk directory: anything else
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
