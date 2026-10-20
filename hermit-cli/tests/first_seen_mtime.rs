/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A file that Hermit sees for the first time reports the epoch as its mtime,
//! unless its host mtime is a canonical, content-independent value: exactly 0
//! or 1 second with no nanoseconds
//! (https://github.com/rrnewton/hermit/issues/3639). Every file in the Nix
//! store has mtime 1, and nixpkgs' stdenv derives `SOURCE_DATE_EPOCH` from the
//! newest source mtime, so reporting the epoch instead moved embedded build
//! timestamps away from those of a native build.

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::fs;
use std::fs::File;
use std::fs::FileTimes;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::time::Duration;
use std::time::SystemTime;

/// The epoch passed to Hermit, and its value in seconds.
const EPOCH: &str = "2027-01-01T00:00:00Z";
const EPOCH_SECS: &str = "1798761600.000000000";

/// Each fixture's host mtime (seconds, nanoseconds) and the mtime the guest
/// must see, as printed by `date -r FILE +%s.%N`.
const FIXTURES: &[(&str, u64, u32, &str)] = &[
    // The Nix store's mtime and the SOURCE_DATE_EPOCH=0 value are kept.
    ("canonical_one", 1, 0, "1.000000000"),
    ("canonical_zero", 0, 0, "0.000000000"),
    // Read before it is ever stat'ed: the read mints the inode, and the first
    // stat must still be the one that decides its mtime.
    ("read_first", 1, 0, "1.000000000"),
    // Ordinary timestamps keep the existing first-seen policy: the epoch.
    ("ordinary", 1_600_000_000, 0, EPOCH_SECS),
    // Matching is exact: a sub-second part or another second is not canonical.
    ("near_canonical", 1, 5, EPOCH_SECS),
    ("two", 2, 0, EPOCH_SECS),
];

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(path: PathBuf) -> Self {
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("failed to create test directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn set_mtime(path: &Path, secs: u64, nanos: u32) {
    let mtime = SystemTime::UNIX_EPOCH + Duration::new(secs, nanos);
    File::open(path)
        .and_then(|file| file.set_times(FileTimes::new().set_accessed(mtime).set_modified(mtime)))
        .unwrap_or_else(|error| panic!("failed to set the mtime of {}: {error}", path.display()));
}

fn host_mtime(path: &Path) -> Duration {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .unwrap_or_else(|error| panic!("failed to read the mtime of {}: {error}", path.display()))
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("mtime is before 1970")
}

fn hermit_run(extra: &[&str], script: &str, args: &[&Path]) -> Output {
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after", "5s", "90s"])
        .arg(hermit_test::hermit_binary())
        .args(["--log=info", "--backend=ptrace", "run", "--strict"])
        .args(extra)
        .arg(format!("--epoch={EPOCH}"))
        // Start the guest in a fresh tmpfs. From the host cwd, /bin/sh stats
        // every ancestor of it, and a busy home directory's st_size changes
        // between the two --verify runs.
        .args([
            "--base-env=minimal",
            "--mount=type=tmpfs,target=/test",
            "--workdir=/test",
            "--",
            "/bin/sh",
            "-c",
            script,
            "sh",
        ])
        .args(args);
    hermit_test::configure_guest_execution(&mut command);
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"));
    assert!(
        output.status.success(),
        "{rendered} failed\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output
}

#[test]
fn canonical_host_mtimes_survive_first_sight_and_others_read_as_epoch() {
    let root = TestDirectory::new(Path::new(env!("CARGO_TARGET_TMPDIR")).join("first-seen-mtime"));
    let inputs = root.path().join("inputs");
    fs::create_dir_all(&inputs).expect("failed to create the input directory");
    for (name, secs, nanos, _) in FIXTURES {
        let path = inputs.join(name);
        fs::write(&path, name).expect("failed to write a fixture");
        set_mtime(&path, *secs, *nanos);
    }
    // A directory with the Nix store's mtime keeps it too.
    let store_dir = inputs.join("store_dir");
    fs::create_dir(&store_dir).expect("failed to create a fixture directory");
    set_mtime(&store_dir, 1, 0);

    // Read one file, list the directory (getdents mints every inode before any
    // stat), and only then print each mtime.
    let script = r#"
        cat "$1/read_first" > /dev/null || exit 3
        ls "$1" > /dev/null || exit 3
        for name in canonical_one canonical_zero read_first ordinary near_canonical two store_dir; do
            printf '%s %s\n' "$name" "$(date -r "$1/$name" +%s.%N)" || exit 3
        done
    "#;
    let report = root.path().join("verify.json");
    let output = hermit_run(
        &[
            "--verify",
            "--verify-strict",
            "--verify-json",
            report.to_str().expect("report path is not UTF-8"),
        ],
        script,
        &[&inputs],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let expected = FIXTURES
        .iter()
        .map(|(name, _, _, mtime)| (*name, *mtime))
        .chain([("store_dir", "1.000000000")]);
    for (name, mtime) in expected {
        let line = format!("{name} {mtime}\n");
        assert!(
            stdout.contains(&line),
            "the guest did not see {name} with mtime {mtime}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }

    let report: serde_json::Value = serde_json::from_slice(
        &fs::read(&report).expect("hermit did not write the verification report"),
    )
    .expect("verification report is not JSON");
    assert_eq!(
        report["bitwise_parity"],
        serde_json::Value::Bool(true),
        "{report}"
    );
    let compared = &report["compared_log_messages"];
    assert!(
        compared["left"].as_u64().is_some_and(|n| n > 0) && compared["left"] == compared["right"],
        "verification compared no INFO messages: {report}"
    );
}

/// `cp -p` copies the mtime that the guest's stat reported onto the host file.
/// Before the fix a Nix store file read as the epoch, so its copy really got
/// the epoch as its host mtime; now it gets the store's 1. The guest must also
/// read 1 back from the copy: stdenv copies its sources with `cp -pr`, and
/// before explicit utimensat mtimes reached the virtual mtime
/// (https://github.com/rrnewton/hermit/issues/3565) the copy read as the time
/// it was written.
#[test]
fn cp_preserve_copies_a_canonical_mtime_to_the_host() {
    let root =
        TestDirectory::new(Path::new(env!("CARGO_TARGET_TMPDIR")).join("first-seen-mtime-cp"));
    let source = root.path().join("source");
    fs::write(&source, "store file").expect("failed to write the source");
    set_mtime(&source, 1, 0);
    let copy = root.path().join("copy");

    let output = hermit_run(
        &[],
        r#"cp -p "$1" "$2" && date -r "$2" +%s.%N"#,
        &[&source, &copy],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout,
        "1.000000000\n",
        "the guest did not read the copied canonical mtime back from the copy\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(
        host_mtime(&copy),
        Duration::new(1, 0),
        "cp -p under hermit gave the copy a host mtime other than the source's canonical 1"
    );
}
