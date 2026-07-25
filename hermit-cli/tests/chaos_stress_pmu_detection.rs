/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Regression tests for GH #21: the chaos stress wrapper falsely skipped
//! PMU-capable hosts.
//!
//! `tests/util/chaos_stress_wrapper.sh` used to probe PMU support with
//! `perf list hardware | grep -i "Hardware event"`. On current x86_64 hosts the
//! retired-branch counter works, yet `perf list hardware` labels the section
//! "legacy hardware" and never prints the phrase "Hardware event", so the probe
//! reported a false negative and skipped chaos coverage while CI stayed green.
//!
//! The probe now lives in `tests/util/perf_supported.sh` and opens the counter
//! with `perf stat -e branches:u`. These tests drive that script with a fake
//! `perf` (via the `PERF` env override) to pin down its behavior across the
//! capability outcomes, without depending on the host's real PMU.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository")
        .to_path_buf()
}

fn perf_supported_script() -> PathBuf {
    repo_root().join("tests/util/perf_supported.sh")
}

/// Write an executable fake `perf` whose body is `body`, and return its path.
fn write_fake_perf(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("perf");
    fs::write(&path, body).expect("write fake perf");
    let mut perms = fs::metadata(&path).expect("stat fake perf").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).expect("chmod fake perf");
    path
}

/// Run `perf_supported.sh` with `PERF` pointed at `fake_perf`; return whether it
/// reported support (exit 0).
fn probe_reports_supported(fake_perf: &Path) -> bool {
    let output = Command::new("bash")
        .arg(perf_supported_script())
        .env("PERF", fake_perf)
        .output()
        .expect("failed to run perf_supported.sh");
    output.status.success()
}

fn unique_dir(tag: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("pmu-detect-{tag}"));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// A capable host: `perf stat -e branches:u` opens the counter, prints a numeric
/// count, and exits 0.
#[test]
fn detects_capable_host() {
    let dir = unique_dir("capable");
    let perf = write_fake_perf(
        &dir,
        r#"#!/bin/bash
# Emulate a working retired-branch counter on stderr (where perf reports).
cat >&2 <<'OUT'

 Performance counter stats for '/bin/true':

            52,230      branches:u

       0.001238131 seconds time elapsed
OUT
exit 0
"#,
    );
    assert!(
        probe_reports_supported(&perf),
        "capable host should be detected as PMU-supported"
    );
}

/// The exact GH #21 regression: `perf list hardware` uses the "legacy hardware"
/// heading with no "Hardware event" phrase (which defeated the old probe), but
/// `perf stat -e branches:u` works. The new probe must report support.
#[test]
fn detects_legacy_hardware_labelled_host() {
    let dir = unique_dir("legacy");
    let perf = write_fake_perf(
        &dir,
        r#"#!/bin/bash
case "$1" in
  list)
    # Old probe grepped this for "Hardware event" and found nothing here.
    cat <<'OUT'

legacy hardware:
  branches
       [Retired branch instructions. Unit: cpu]
OUT
    exit 0
    ;;
  stat)
    cat >&2 <<'OUT'

 Performance counter stats for '/bin/true':

            48,915      branches:u

       0.001100000 seconds time elapsed
OUT
    exit 0
    ;;
esac
"#,
    );
    assert!(
        probe_reports_supported(&perf),
        "host with 'legacy hardware' perf-list labelling must still be detected (GH #21)"
    );
}

/// A restricted host where the counter cannot be opened: `perf stat` exits 0 but
/// prints "<not supported>" for the event. Must be reported as unsupported.
#[test]
fn rejects_not_supported_counter() {
    let dir = unique_dir("not-supported");
    let perf = write_fake_perf(
        &dir,
        r#"#!/bin/bash
cat >&2 <<'OUT'

 Performance counter stats for '/bin/true':

     <not supported>      branches:u

       0.000900000 seconds time elapsed
OUT
exit 0
"#,
    );
    assert!(
        !probe_reports_supported(&perf),
        "'<not supported>' counter must be reported as unsupported"
    );
}

/// Similar restricted host where perf prints "<not counted>". Also unsupported.
#[test]
fn rejects_not_counted_counter() {
    let dir = unique_dir("not-counted");
    let perf = write_fake_perf(
        &dir,
        r#"#!/bin/bash
cat >&2 <<'OUT'

 Performance counter stats for '/bin/true':

       <not counted>      branches:u

       0.000900000 seconds time elapsed
OUT
exit 0
"#,
    );
    assert!(
        !probe_reports_supported(&perf),
        "'<not counted>' counter must be reported as unsupported"
    );
}

/// A host where the event is unknown: `perf stat` exits non-zero. Unsupported.
#[test]
fn rejects_perf_stat_failure() {
    let dir = unique_dir("stat-failure");
    let perf = write_fake_perf(
        &dir,
        r#"#!/bin/bash
>&2 echo "event syntax error: 'branches:u'"
exit 1
"#,
    );
    assert!(
        !probe_reports_supported(&perf),
        "a non-zero 'perf stat' must be reported as unsupported"
    );
}

/// No `perf` binary at all: the PERF override points at a nonexistent path.
#[test]
fn rejects_missing_perf_binary() {
    let dir = unique_dir("missing");
    let missing = dir.join("definitely-missing-perf");
    assert!(
        !probe_reports_supported(&missing),
        "a missing perf binary must be reported as unsupported"
    );
}
