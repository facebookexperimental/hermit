/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Bracket Hermit's `sched_setattr` argument handling against the host kernel's.
//!
//! `tests/c/sched_setattr_abi.c` prints one line per probed case and asserts
//! nothing itself; this driver runs it natively and under Hermit and compares
//! the two transcripts byte for byte. The expectation is therefore "Hermit
//! agrees with the kernel it is running on" rather than a table that could be
//! wrong in both places at once, and it fails in either direction -- a case
//! Hermit wrongly refuses is caught as readily as one it wrongly accepts.
//!
//! This is an ordinary `cargo test` target on purpose. An earlier version of
//! this regression test lived in `detcore/tests/lit/`, which nothing in this
//! repository executes: there is no lit runner, no `lit.cfg`, and the handful
//! of `detcore/tests/lit/` fixtures that do run are the ones enumerated by path
//! in `hermit-cli/tests/hermit_modes.rs`. A test that compiles but never runs
//! is worse than no test, so the fixture moved here.

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

/// Compile the probe fixture and return its path. The build root is
/// `CARGO_TARGET_TMPDIR`, which lives under `target/` rather than `/tmp`:
/// Hermit isolates `/tmp`, so a guest binary placed there would be invisible
/// inside the sandbox and the run would silently do nothing.
fn build_fixture() -> PathBuf {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("sched-setattr-abi");
    fs::create_dir_all(&build_root).expect("failed to create guest build directory");
    let guest = build_root.join("sched_setattr_abi");

    let source = repository.join("tests/c/sched_setattr_abi.c");
    let mut compile = Command::new("cc");
    compile
        .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(&source)
        .arg("-o")
        .arg(&guest);
    let rendered = format!("{compile:?}");
    let output = compile
        .output()
        .unwrap_or_else(|error| panic!("failed to start the compiler: {rendered}: {error}"));
    assert!(
        output.status.success(),
        "compiling the sched_setattr probe failed: {rendered}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
    guest
}

/// Run `command`, require a clean exit, and return its stdout.
fn transcript(mut command: Command, label: &str) -> String {
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {label}: {rendered}: {error}"));
    assert!(
        output.status.success(),
        "{label} exited nonzero: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout).expect("the probe prints ASCII")
}

#[test]
fn sched_setattr_argument_handling_matches_the_host_kernel() {
    let guest = build_fixture();

    let mut native_command = Command::new(&guest);
    native_command.env_clear();
    let native = transcript(native_command, "the native sched_setattr probe");

    let sandboxed = run_under_hermit(&guest, "ptrace");
    compare(&native, &sandboxed, "ptrace");

    // SaBRe is the second backend this handler runs under, and the ABI it
    // presents must not depend on which one is loaded. Skipped rather than
    // failed when the loader and plugin are not built, the same convention
    // `sabre_examples.rs` uses -- a missing third-party artifact is an
    // environment fact, not a product verdict.
    match sabre_artifacts() {
        Some((loader, plugin)) => {
            let sabre = run_under_hermit_sabre(&guest, &loader, &plugin);
            compare(&native, &sabre, "sabre");
        }
        None => eprintln!(
            "skipping the SaBRe arm of the sched_setattr ABI bracket: loader or plugin missing"
        ),
    }
}

fn run_under_hermit(guest: &Path, backend: &str) -> String {
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after", "5s", "120s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "run",
            &format!("--backend={backend}"),
            "--strict",
            "--base-env=minimal",
            "--",
        ])
        .arg(guest);
    transcript(command, &format!("the {backend} sched_setattr probe"))
}

fn sabre_artifacts() -> Option<(PathBuf, PathBuf)> {
    let hermit = PathBuf::from(env!("CARGO_BIN_EXE_hermit"));
    let executable_dir = hermit.parent()?;
    let target_dir = executable_dir.parent()?;
    let loader = std::env::var_os("HERMIT_SABRE_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| target_dir.join("sabre/sabre"));
    let plugin = executable_dir.join("libdetcore_sabre.so");
    (loader.is_file() && plugin.is_file()).then_some((loader, plugin))
}

fn run_under_hermit_sabre(guest: &Path, loader: &Path, plugin: &Path) -> String {
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after", "5s", "120s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["run", "--backend=sabre", "--strict", "--base-env=minimal"])
        .arg("--sabre")
        .arg(loader)
        .arg("--sabre-plugin")
        .arg(plugin)
        .arg("--")
        .arg(guest);
    transcript(command, "the sabre sched_setattr probe")
}

fn compare(native: &str, sandboxed: &str, backend: &str) {
    assert!(
        !native.trim().is_empty(),
        "the probe printed nothing natively, so the comparison would be vacuous"
    );

    if native != sandboxed {
        eprintln!("backend under comparison: {backend}");
        let mut differing = String::new();
        for (native_line, sandboxed_line) in native.lines().zip(sandboxed.lines()) {
            if native_line != sandboxed_line {
                differing.push_str(&format!(
                    "  native : {native_line}\n  hermit : {sandboxed_line}\n\n"
                ));
            }
        }
        if native.lines().count() != sandboxed.lines().count() {
            differing.push_str(&format!(
                "  line counts differ: native {} vs hermit {}\n",
                native.lines().count(),
                sandboxed.lines().count()
            ));
        }
        panic!(
            "Hermit's sched_setattr answers differ from the host kernel's:\n\n{differing}\
             full native transcript:\n{native}\nfull hermit transcript:\n{sandboxed}"
        );
    }
}
