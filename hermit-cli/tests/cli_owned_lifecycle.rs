/* Copyright (c) Meta Platforms, Inc. and affiliates. */
//! Re-exec the real CLI before threads; no raw clone runs in this libtest process.
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

fn monotonic_ns() -> u64 {
    let mut now = std::mem::MaybeUninit::<libc::timespec>::uninit();
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, now.as_mut_ptr()) },
        0
    );
    let now = unsafe { now.assume_init() };
    u64::try_from(now.tv_sec).unwrap() * 1_000_000_000 + u64::try_from(now.tv_nsec).unwrap()
}
fn lifecycle(mode: &str) {
    // Includes executable startup: child receives this absolute deadline rather
    // than resetting three seconds after exec. Separate rescue is at most2s.
    let end = monotonic_ns() + 3_000_000_000;
    let mut child = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .env("HERMIT_INTERNAL_CLI_LIFECYCLE", "1")
        .args(["__hermit-cli-lifecycle", mode, &end.to_string()])
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let outer = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(
                status.success(),
                "{mode}: lifecycle predicate failed: {status}"
            );
            return;
        }
        if Instant::now() >= outer {
            child.kill().unwrap();
            let _ = child.wait();
            panic!("{mode}: original predicate plus separate rescue exceeded outer bound");
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}
#[test]
fn abnormal_init_retains_live_descendant_backing_resources() {
    lifecycle("abnormal");
}
#[test]
fn reported_error_retains_live_descendant_backing_resources() {
    lifecycle("reported");
}
#[test]
fn malformed_success_frame_retains_live_descendant_backing_resources() {
    lifecycle("malformed");
}
#[test]
fn private_namespace_exit_retires_actual_descendant_before_guard_drop() {
    lifecycle("private-abnormal");
}
#[test]
fn successful_completion_returns_original_guard() {
    lifecycle("success");
}

#[test]
fn copied_gdb_watch_cannot_finish_or_drop_the_original_owner() {
    lifecycle("gdb-foreign-copy");
}
#[test]
fn original_gdb_parent_death_stops_probes_despite_inherited_socket_alias() {
    lifecycle("gdb-parent-death-alias");
}
#[test]
fn gdb_creator_thread_exit_does_not_end_process_supervision() {
    lifecycle("gdb-creator-thread-exit");
}

// Each control enters an isolated pre-thread CLI natural reaper. The shared
// libtest process never becomes a subreaper or assumes ownership of descendants.
#[test]
fn fixture_worker_exits_when_lifecycle_cli_asserts_before_stop() {
    lifecycle("containment-early-assertion");
}

#[test]
fn fixture_worker_exits_when_lifecycle_cli_is_killed() {
    lifecycle("containment-cli-owner-death");
}
