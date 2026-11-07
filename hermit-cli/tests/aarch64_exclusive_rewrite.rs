/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! aarch64: `--rewrite-exclusives` under `--no-namespace` rewrites a guest's
//! load/store-exclusive retry loops in its memory, end to end through the
//! CLI, where it can prove that nothing reads the loop's bytes, and leaves
//! them, naming why, where it cannot. The guests (and a C-library guest's
//! loader and libc) run from an owned disk directory. Btrfs and overlayfs
//! need actual map_files access to prove their mappings; disk-backed storage
//! does not relax the rewriter's identity checks.

#![cfg(target_arch = "aarch64")]

#[path = "common/exclusive_rewrite_summary.rs"]
mod exclusive_rewrite_summary;
#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::fs;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

/// What each guest prints: 40000 additions (the C guest's 4 threads of 10000
/// each).
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

/// Compiles `source` (under `tests/c`) with `cc` and `flags` into `dir`,
/// with the compiler's temporary files there too.
fn compile(dir: &Path, source: &str, flags: &[&str]) -> PathBuf {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let output = dir.join(Path::new(source).file_stem().unwrap());
    let compiled = Command::new("cc")
        .env("TMPDIR", dir)
        .args(flags)
        .arg(repository.join("tests/c").join(source))
        .arg("-o")
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    output
}

/// Runs `command` under `hermit run --no-namespace --rewrite-exclusives`
/// through the selected launcher, recording preemptions so that the run
/// counts store-exclusives, requires it to succeed printing `expected`, and
/// returns its summary.
fn run_rewriting(dir: &Path, command: &[&Path], expected: &str) -> serde_json::Value {
    let summary_path = dir.join("summary.json");
    let record = dir.join("record.json");
    let mut run = hermit_command();
    run.env("TMPDIR", dir);
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
    .args(command);
    let rendered = format!("{run:?}");
    let output = hermit_test::hermit_output(&rendered, &mut run);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{rendered}\n{stderr}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        expected,
        "{stderr}"
    );
    serde_json::from_slice(&fs::read(&summary_path).unwrap()).unwrap()
}

/// No rewrite row failed, every file with loops is one the test put in `dir`
/// (anything else, a library the guest found on the host, cannot be proven
/// and would be counted as not written), every such file was written where
/// planned, and its loops are all counted as rewritten or left. Returns
/// whether any loop was left.
fn every_file_with_loops_is_accounted_for(summary: &serde_json::Value, dir: &Path) -> bool {
    exclusive_rewrite_summary::check_no_failed_rewrites(summary)
        .unwrap_or_else(|error| panic!("{error}: {summary:#}"));
    let pairs = summary["exclusive_pairs"]
        .as_object()
        .unwrap_or_else(|| panic!("no exclusive_pairs in {summary:#}"));
    let mut left_anywhere = false;
    for (path, count) in pairs {
        assert!(
            Path::new(path).starts_with(dir),
            "the guest mapped {path} from outside {}: {summary:#}",
            dir.display()
        );
        let (rewritten, left, failed) = rewrite_of(summary, Path::new(path));
        assert_eq!(failed, 0, "{path} not written: {summary:#}");
        assert_eq!(Some(rewritten + left), count.as_u64(), "{summary:#}");
        left_anywhere |= left > 0;
        eprintln!("{path}: {rewritten} rewritten, {left} left");
    }
    left_anywhere
}

/// A guest whose image the rewrite can prove, one with no ADRP or ADR
/// anywhere (tests/c/exclusive_loop_bare.S, built without a C library), has
/// its one retry loop rewritten in its memory, and it still counts exactly.
/// With no loop left, a run that counts store-exclusives (one that records
/// preemptions) sees none fail, and where the counters ran for the whole
/// run, none ran at all.
///
/// It needs an executable disk fixture directory, a C compiler and ptrace.
/// Explicit selection on an unsuitable host fails by name, never passes early.
#[test]
#[ignore = "host-limited: needs HERMIT_AARCH64_TEST_ROOT, HERMIT_AARCH64_TEST_LAUNCHER, cc and aarch64 ptrace (docs/TESTING_ENVIRONMENTS.md)"]
fn the_in_memory_rewrite_replaces_a_provable_guests_loop_and_it_still_counts() {
    let dir = fixture_dir();
    let guest = compile(
        dir.path(),
        "exclusive_loop_bare.S",
        &["-nostdlib", "-static"],
    );
    require_mapping_proof(&guest);
    let summary = run_rewriting(dir.path(), &[&guest], EXPECTED);
    assert_eq!(rewrite_of(&summary, &guest), (1, 0, 0), "{summary:#}");
    assert!(
        !every_file_with_loops_is_accounted_for(&summary, dir.path()),
        "{summary:#}"
    );
    if let Some(counts) = summary.get("store_exclusives") {
        assert_eq!(counts["failed"].as_u64(), Some(0), "{summary:#}");
        if counts["fully_counted"].as_bool() == Some(true)
            && let Some(executed) = counts.get("executed")
        {
            assert_eq!(executed.as_u64(), Some(0), "{summary:#}");
        }
    }
}

/// An ordinary C-library guest (tests/c/exclusive_loop_counter.c, four
/// threads adding through the same loop) keeps its loop: its start files
/// hold address computations (ADRP and ADR) whose reads the rewrite's
/// image-wide scan cannot follow, so it cannot prove that nothing reads the
/// loop's bytes and leaves the loop, naming why
/// (`literal-inside-the-sequence`, as `hermit rewrite-exclusives
/// --list-refusals` lists it). The guest still counts exactly. The host's
/// loader and libc are only logged: a loop the rewriter refuses, or one
/// across a page, is left by design, and an LSE-only libc has none.
///
/// It has the same host needs as the test above.
#[test]
#[ignore = "host-limited: needs HERMIT_AARCH64_TEST_ROOT, HERMIT_AARCH64_TEST_LAUNCHER, cc and aarch64 ptrace (docs/TESTING_ENVIRONMENTS.md)"]
fn a_c_library_guests_loop_is_left_with_its_reason_and_it_still_counts() {
    let dir = fixture_dir();
    let mut libraries = Vec::new();
    for host in host_libraries() {
        let copy = dir.path().join(host.file_name().unwrap());
        fs::copy(&host, &copy).unwrap();
        libraries.push(copy);
    }
    let loader = libraries[0].clone();
    let guest = compile(
        dir.path(),
        "exclusive_loop_counter.c",
        &["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror", "-pthread"],
    );
    require_mapping_proof(&guest);
    let summary = run_rewriting(
        dir.path(),
        &[&loader, Path::new("--library-path"), dir.path(), &guest],
        EXPECTED,
    );
    assert_eq!(rewrite_of(&summary, &guest), (0, 1, 0), "{summary:#}");
    every_file_with_loops_is_accounted_for(&summary, dir.path());

    let mut list = hermit_command();
    list.env("TMPDIR", dir.path())
        .args(["rewrite-exclusives", "--list-refusals"])
        .arg(&guest);
    let rendered = format!("{list:?}");
    let listed = hermit_test::hermit_output(&rendered, &mut list);
    // `rewrite-exclusives` exits 1 when a file still holds a loop it refused.
    assert_eq!(
        listed.status.code(),
        Some(1),
        "{rendered}\n{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let listed = String::from_utf8_lossy(&listed.stdout);
    let reasons: Vec<&str> = listed
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("at "))
        .filter_map(|site| site.split_once(": ").map(|(_, why)| why))
        .collect();
    assert_eq!(reasons, ["literal-inside-the-sequence"], "{listed}");
}
