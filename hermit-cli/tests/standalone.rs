// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
// Licensed under the BSD-style license in the LICENSE file.

//! The internal Buck matrix's standalone targets: the `tests/standalone`
//! scripts, the no-networking bind checks, `standalone_stacktrace_events` and
//! the `verify_replay` script, each run as its `buck_sh_test` runs it, with
//! Hermit as its first argument and in `HERMIT_BIN`.
//!
//! Each runs in a fresh directory that holds the two layouts the scripts name
//! relative to it: `hermetic_infra/hermit` (fbsource's path to this
//! repository, which `replay_trace.sh` searches) and `examples`. A fixed
//! `HERMIT_EPOCH` keeps the runs a script compares on one virtual clock, and
//! keeps Hermit from saving each run's inputs under `~/.cache/hermit`.
//!
//! Not ported: `curl_client_only.sh` needs the network. `cat_proc_uptime.sh`
//! no longer tests what it says. It expects `run --verify` of three
//! `/proc/uptime` reads to find differences, but Detcore renders that file
//! deterministically now, and the relaxed `--verify` it uses is refused
//! before any comparison, so its nonzero exit would pass without measuring
//! anything. The `strict_setitimer` and `strict_timer_create` scripts are not
//! standalone targets internally either.

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

/// The epoch every script's Hermit runs share.
const EPOCH: &str = "2026-01-01T00:00:00Z";

fn repository() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository")
}

/// The tests package's binary NAME, which a workspace build puts beside this
/// test's `hermit`.
fn workspace_binary(name: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_BIN_EXE_hermit")).with_file_name(name);
    assert!(
        path.is_file(),
        "{} is missing; build it with cargo build -p hermetic_infra_hermit_tests --bins",
        path.display()
    );
    path
}

/// `tests/c/simple/<name>.c`, compiled into DIRECTORY with EXTRA flags.
fn compile_c(name: &str, directory: &Path, extra: &[&str]) -> PathBuf {
    let source = repository()
        .join("tests/c/simple")
        .join(format!("{name}.c"));
    let binary = directory.join(name);
    let output = Command::new("cc")
        .args(["-O2", "-g"])
        .args(extra)
        .arg("-o")
        .arg(&binary)
        .arg(&source)
        .output()
        .unwrap_or_else(|error| panic!("cannot run cc for {}: {error}", source.display()));
    assert!(
        output.status.success(),
        "cc failed for {}:\n{}",
        source.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    binary
}

/// Run PROGRAM with ARGS as the internal target does, in a fresh directory
/// with the layouts above, and require it to exit 0.
fn run_standalone(label: &str, program: &Path, args: &[&Path]) {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the standalone test's directory");
    let work = directory.path().join("work");
    std::fs::create_dir_all(work.join("hermetic_infra")).unwrap();
    std::os::unix::fs::symlink(repository(), work.join("hermetic_infra/hermit")).unwrap();
    std::os::unix::fs::symlink(repository().join("examples"), work.join("examples")).unwrap();
    let minimal_hello = compile_c("hello_nostdlib", directory.path(), &["-nostdlib"]);
    let (output, ()) = hermit_test::hermit_output_with(label, || {
        let output = Command::new(program)
            .args(args)
            .current_dir(&work)
            .env("HERMIT_BIN", hermit_test::hermit_binary())
            .env("MINIMAL_HELLO", &minimal_hello)
            .env("HERMIT_EPOCH", EPOCH)
            .env("TMPDIR", directory.path())
            .output()
            .unwrap_or_else(|error| panic!("failed to run {label}: {error}"));
        (output, ())
    });
    assert!(
        output.status.success(),
        "{label} exited with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// `tests/standalone/<name>.sh`, run with bash and Hermit as its argument.
fn run_script(name: &str) {
    let script = repository()
        .join("tests/standalone")
        .join(format!("{name}.sh"));
    run_standalone(
        name,
        Path::new("bash"),
        &[script.as_path(), hermit_test::hermit_binary()],
    );
}

macro_rules! standalone_scripts {
    ($($(#[ignore = $reason:literal])? $name:ident,)+) => {
        $(
            #[test]
            $(#[ignore = $reason])?
            fn $name() {
                run_script(stringify!($name));
            }
        )+
    };
}

standalone_scripts! {
    env_var_passthru,
    minimal_hello_backtraces,
    replay_and_print_stacktraces,
    replay_chaos_trace,
    replay_trace,
    stacktrace_signal,
    #[ignore = "over budget: 26.0 CPU-s (killed at the cap; its runs go two at a time) against the 22 CPU-s per-test cap in the pinned root, 2026-10-15"]
    validate_race_example_properties,
}

/// `test_no_networking_network_bind` and `_full`: a guest that binds a port
/// runs under Hermit's default networking without error.
fn run_no_networking(binary: &str) {
    let script = repository().join("tests/standalone/test_no_networking_network_bind.sh");
    let guest = workspace_binary(binary);
    run_standalone(
        binary,
        Path::new("bash"),
        &[
            script.as_path(),
            hermit_test::hermit_binary(),
            guest.as_path(),
        ],
    );
}

#[test]
fn no_networking_network_bind() {
    run_no_networking("network_bind_rs");
}

/// The guest binds every port from 32768 to 65534, 32,767 listeners, each a
/// few syscalls through ptrace.
#[test]
#[ignore = "over budget: 14.5 CPU-s unloaded, killed at the 22 CPU-s per-test cap (exit 124 at 24.3 s) in the 838f2582 validation, 2026-10-15"]
fn no_networking_network_bind_full() {
    run_no_networking("network_bind_full_rs");
}

/// `test_standalone_stacktrace_events__<target>`: the
/// `standalone_stacktrace_events` program checks the stack traces Hermit
/// prints for scheduled events of TARGET.
fn run_stacktrace_events(label: &str, target: &Path) {
    let program = workspace_binary("standalone_stacktrace_events");
    run_standalone(label, &program, &[hermit_test::hermit_binary(), target]);
}

#[test]
fn stacktrace_events_nanosleep_threads_nocrash() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let target = compile_c("nanosleep-threads-nocrash", directory.path(), &["-pthread"]);
    run_stacktrace_events("stacktrace_events nanosleep-threads-nocrash", &target);
}

#[test]
fn stacktrace_events_nanosleep_threads_simple() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let target = compile_c("nanosleep-threads-simple", directory.path(), &["-pthread"]);
    run_stacktrace_events("stacktrace_events nanosleep-threads-simple", &target);
}

#[test]
fn stacktrace_events_cas_sequence_easy() {
    let target = workspace_binary("chaos_cas_sequence_bin");
    run_stacktrace_events("stacktrace_events cas_sequence_easy", &target);
}

/// `verify_replay__nanosleep-threads-simple`: tests/util's script records a
/// chaotic run's preemptions, replays them, and requires identical output.
#[test]
fn verify_replay_nanosleep_threads_simple() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let target = compile_c("nanosleep-threads-simple", directory.path(), &["-pthread"]);
    let script = repository().join("tests/util/hermit_verify_trace_replay.sh");
    run_standalone(
        "verify_replay nanosleep-threads-simple",
        Path::new("bash"),
        &[script.as_path(), target.as_path()],
    );
}
