/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Regression coverage for `flock(2)` under Detcore (PR #2373).
//!
//! Before #2373 `handle_flock` was an unconditional no-op success, so two
//! guests held the same `LOCK_EX` at once. These tests pin down the properties
//! that fix depends on, each with a stated pre-fix control so none
//! of them can quietly go inert:
//!
//! | test | pre-fix behavior it stops |
//! | --- | --- |
//! | [`flock_excludes_a_second_open_and_a_second_process`] | the no-op: every `flock` returned 0 |
//! | [`contended_blocking_upgrade_is_refused_without_losing_the_shared_lock`] | the `LOCK_NB` probe destroyed the caller's `LOCK_SH` |
//! | [`contended_blocking_upgrade_is_fail_closed_under_strict`] | refusal policy ignored two of three config knobs |
//! | [`blocking_upgrade_on_received_fd_preserves_unknown_lock_state`] | the probe destroyed a lock received through `SCM_RIGHTS` |
//! | [`dbt_forked_child_blocking_flock_fails_closed_without_deadlock`] | a copied DBT child ran blocking flock natively and deadlocked |
//! | [`dbt_forked_child_preserves_safe_flock_operations`] | copied-child refusal overmatched malformed, nonblocking, and unlock operations |
//! | [`dbt_nested_vfork_child_blocking_flock_reaches_the_copied_policy`] | a copied fork child's vfork path was assumed to bypass the copied-syscall flock guard |
//! | [`failed_process_clone_preserves_known_flock_state`] | a failed clone made an unlocked descriptor permanently unknown |
//! | [`pidfd_getfd_alias_mutation_invalidates_source_flock_authority`] | a pidfd_getfd duplicate unlocked its source OFD while the source cache stayed stale and restored the released lock |
//! | [`transferred_lock_state_is_unknown_to_the_sender`] | the sender restored stale state after the receiver unlocked the OFD |
//! | [`dbt_vfork_child_flock_fails_closed_without_deadlock`] | a copied vfork child blocked in the kernel while its parent was suspended |
//! | [`pipe_fork_then_vfork_runs_at_l2`] | every fork invalidated every description, so DBT refused any later vfork, `make -jN` included |
//! | [`dbt_pipe_fork_then_vfork_runs_at_l2`] | the same, on DBT, where the guard runs |
//! | [`dbt_vfork_after_a_pipe_lock_and_a_fork_fails_closed`] | narrowing the guard to granted locks would lose a grant made before or after the fork |
//! | [`dbt_copied_child_pidfd_getfd_is_refused_under_every_policy`] | a copied vfork child duplicated a sibling's locked description with native pidfd_getfd and unlocked it under the sibling's held claim |
//! | [`dbt_copied_child_recvmsg_import_is_refused_under_every_policy`] | a copied vfork child received a sibling's locked shared description over SCM_RIGHTS under the allow policy and unlocked it under the sibling's held claim |
//! | [`dbt_copied_child_ofd_getlk_and_posix_locks_stay_native`] | the copied-child OFD refusal overmatched a read-only F_OFD_GETLK or a POSIX F_SETLK |
//! | [`dbt_clone_vfork_forms_fail_closed_before_copy`] | clone and clone3 with `CLONE_VFORK` bypassed the vfork pre-copy guard |
//! | [`dbt_process_clone_files_is_refused_before_copied_child_mutation`] | a copied process shared the kernel fd table while Detcore copied its metadata |
//! | [`replay_reissues_every_flock_for_a_materialized_file`] | replay consumed the recorded return and took no lock |
//! | [`replay_refuses_flock_for_a_non_materialized_file`] | replay reported success while locking only a placeholder |
//! | [`replay_reissues_pidfd_getfd_success_and_failure`] | replay consumed pidfd_getfd results without reproducing descriptor side effects |
//! | [`recording_refuses_a_contended_blocking_flock`] | record mode silently accepted the ordinary-run compatibility fallback |
//! | [`pre_flock_recordings_are_refused_by_the_version_gate`] | a 0x10b recording has no flock event and desynchronized |

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;

/// `hermit record` writes to shared per-run state; serialize the record/replay
/// cases the same way `record_replay.rs` does.
static RECORD_LOCK: Mutex<()> = Mutex::new(());
#[cfg(feature = "dbt")]
const DBT_VFORK_FLOCK_REFUSAL: &str =
    "detcore-dbt: refusing vfork/CLONE_VFORK while an open file description may hold a flock";
#[cfg(feature = "dbt")]
const DBT_COPIED_PIDFD_GETFD_REFUSAL: &str = "detcore-dbt: refusing pidfd_getfd in copied child";
#[cfg(feature = "dbt")]
const DBT_COPIED_RECVMSG_REFUSAL: &str = "detcore-dbt: refusing recvmsg/recvmmsg in copied child";
#[cfg(feature = "dbt")]
const DBT_PROCESS_CLONE_FILES_REFUSAL: &str =
    "detcore-dbt: refusing process clone with CLONE_FILES without CLONE_THREAD";

fn record_lock() -> MutexGuard<'static, ()> {
    RECORD_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn repository() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository")
}

/// Compile `tests/c/flock_exclusion.c` once per test binary.
fn guest() -> &'static Path {
    static GUEST: OnceLock<PathBuf> = OnceLock::new();
    GUEST.get_or_init(|| {
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("flock-exclusion-{}", std::process::id()));
        fs::create_dir_all(&build_root).expect("failed to create the flock guest build directory");
        let binary = build_root.join("flock_exclusion");
        let source = repository().join("tests/c/flock_exclusion.c");
        let compile = Command::new("cc")
            .args(["-O1", "-std=c11", "-Wall", "-Wextra", "-Werror"])
            .arg(&source)
            .arg("-o")
            .arg(&binary)
            .output()
            .unwrap_or_else(|error| panic!("failed to start cc for {}: {error}", source.display()));
        assert!(
            compile.status.success(),
            "failed to compile {}:\n{}",
            source.display(),
            String::from_utf8_lossy(&compile.stderr)
        );
        binary
    })
}

struct Run {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

impl Run {
    fn combined(&self) -> String {
        format!("stdout:\n{}\nstderr:\n{}", self.stdout, self.stderr)
    }
}

fn finish(output: Output) -> Run {
    Run {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// Run the guest under `hermit run`, with `extra` inserted before `--`.
///
/// `log` is the global `--log` level; `--verify` requires at least `info`, so
/// it cannot simply be pinned to `off`.
fn hermit_run(log: &str, extra: &[&str], scenario: &str) -> Run {
    hermit_run_backend("ptrace", log, extra, scenario)
}

fn hermit_run_backend(backend: &str, log: &str, extra: &[&str], scenario: &str) -> Run {
    hermit_run_backend_timeout(backend, log, extra, scenario, "120s")
}

fn hermit_run_backend_timeout(
    backend: &str,
    log: &str,
    extra: &[&str],
    scenario: &str,
    timeout: &str,
) -> Run {
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after", "10s", timeout])
        .arg(hermit_test::hermit_binary())
        .arg(format!("--log={log}"))
        .args([&format!("--backend={backend}"), "run", "--base-env=minimal"])
        .args(extra)
        .arg("--")
        .arg(guest())
        .arg(scenario);
    // In the pinned validate root this adds the hermetic `--workdir=/test`,
    // without which the DBT backend refuses the run.
    hermit_test::configure_guest_execution(&mut command);
    finish(hermit_test::hermit_output(
        &format!("{command:?}"),
        &mut command,
    ))
}

/// Mutual exclusion, the property the pre-#2373 no-op removed outright: under
/// the no-op every `flock` returned 0, so both the second open file description
/// and the second process "acquired" a lock the first process was holding, and
/// the guest below printed `FAIL` on its very first contention probe. The
/// `--verify` pass additionally pins that forwarding to the kernel did not cost
/// determinism -- the exclusion outcome is fixed by Detcore's schedule, not by
/// which process happens to reach the kernel first.
#[test]
fn flock_excludes_a_second_open_and_a_second_process() {
    let run = hermit_run(
        "info",
        &["--strict", "--verify", "--panic-on-unsupported-syscalls"],
        "exclusion",
    );
    assert!(
        run.status.success(),
        "strict --verify exclusion run failed\n{}",
        run.combined()
    );
    for marker in [
        "flock-first-holder-acquired",
        "flock-second-open-excluded",
        "flock-second-process-excluded",
        "flock-released-and-reacquired",
        "flock-exclusion-ok",
    ] {
        assert!(
            run.stdout.contains(marker),
            "missing {marker}\n{}",
            run.combined()
        );
    }
    assert!(
        run.stdout.contains("Determinism verified") || run.stderr.contains("Determinism verified"),
        "exclusion run did not verify as deterministic\n{}",
        run.combined()
    );
}

/// A contended BLOCKING `LOCK_SH` -> `LOCK_EX` conversion must be refused
/// without costing the caller the lock it already holds.
///
/// Detcore rewrites a blocking request to `LOCK_NB` so a guest thread cannot
/// park in the kernel where the deterministic scheduler cannot see it. Linux
/// converts a lock non-atomically -- `flock_lock_inode` deletes the caller's
/// existing lock before it scans for a conflict -- so that rewrite silently
/// destroys a `LOCK_SH` the guest is relying on and then reports `EWOULDBLOCK`.
/// Natively the guest never sees that state, because a blocking request sleeps
/// and eventually acquires.
///
/// The guest measures survival through a fresh open file description after
/// releasing the separate shared contender, so the probe can only be answered
/// by the original lock. Keeping both descriptions in one process ensures this
/// test exercises the known-mode restore path; the separate SCM_RIGHTS test
/// below covers state whose history Detcore did not observe. Pre-fix control:
/// with the
/// restore suppressed, the probe acquires and the guest prints
/// `FAIL: the refused upgrade destroyed this process's shared lock`.
#[test]
fn contended_blocking_upgrade_is_refused_without_losing_the_shared_lock() {
    let run = hermit_run("off", &["--allow-unsupported-syscalls"], "upgrade");
    assert!(
        run.status.success(),
        "non-strict upgrade run failed\n{}",
        run.combined()
    );
    for marker in [
        "flock-upgrade-parent-holds-shared",
        "flock-upgrade-contender-holds-shared",
        // Non-strict policy: the guest gets a normal errno rather than a
        // fail-closed shutdown. ENOLCK is 37 on Linux/x86_64.
        "flock-upgrade-refused errno=37",
        "flock-upgrade-preserved-shared-lock",
        "flock-upgrade-ok",
    ] {
        assert!(
            run.stdout.contains(marker),
            "missing {marker}\n{}",
            run.combined()
        );
    }
}

/// The same contention under a fail-closed run must stop the run rather than
/// hand back an errno.
///
/// `--strict` sets `panic_on_unsupported_syscalls`, which the CLI couples to
/// `shutdown_on_unsupported_syscall`, so the refusal goes through
/// `Detcore::refuse_unserviceable_operation` and terminates. Asserting the
/// diagnostic as well as the exit status keeps this from passing on an
/// unrelated failure: a timeout or a broken guest would exit non-zero too.
#[test]
fn contended_blocking_upgrade_is_fail_closed_under_strict() {
    let run = hermit_run("error", &["--strict"], "upgrade");
    // ⚠️ 122 IS "HERMIT REFUSED", NOT "HERMIT BROKE", AND NOT THE GUEST'S 1.
    // This assertion read `Some(1)` because `unrecoverable_shutdown` used to
    // `exit(1)`. That made the parent's classifier -- whose arm is documented
    // "the child died with a status it did not pick" -- report
    // `class=container-child-exit` and 125 for a shutdown that worked exactly as
    // designed, sending a reader to hunt a defect in the refusal path. `1` was
    // no better as a target: it is the commonest guest exit status, so it cannot
    // distinguish "hermit refused" from "your program returned 1".
    //
    // Imported rather than written out: a copied exit code is what left eight
    // cli tests asserting a stale value for a day after the product moved.
    assert_eq!(
        run.status.code(),
        Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
        "a contended blocking flock upgrade must fail closed as a POLICY REFUSAL, not time out or die for an unrelated reason\n{}",
        run.combined()
    );
    // The class is asserted too, because the code alone cannot carry it: every
    // value in 0..=255 is a legal guest status, so only the marker is exclusive.
    assert!(
        run.combined()
            .contains("HERMIT_POLICY_REFUSAL class=policy-refusal"),
        "the refusal must be machine-readable as a refusal, not as a failure\n{}",
        run.combined()
    );
    assert!(
        run.stdout.contains("flock-upgrade-contender-holds-shared"),
        "the strict run did not reach the contended upgrade\n{}",
        run.combined()
    );
    assert!(
        !run.stdout.contains("flock-upgrade-ok"),
        "the strict run completed the upgrade scenario instead of failing closed\n{}",
        run.combined()
    );
    assert!(
        run.stderr.contains("blocking flock(fd=")
            && run.stderr.contains(
                "Refusing rather than granting a lock another guest holds. LOCK_NB is the \
                 supported form where the backend supports flock at all; ENOLCK is returned \
                 instead only under --allow-unsupported-syscalls."
            ),
        "the strict run did not report the specific flock refusal\n{}",
        run.combined()
    );
}

/// A descriptor received through `SCM_RIGHTS` can already hold a lock, but its
/// acquisition history is outside Detcore's descriptor model. A blocking
/// conversion must therefore be refused before issuing the destructive
/// nonblocking probe. The guest proves the received shared lock still excludes
/// a fresh open file description after the refusal.
#[test]
fn blocking_upgrade_on_received_fd_preserves_unknown_lock_state() {
    let run = hermit_run("error", &["--allow-unsupported-syscalls"], "received");
    assert!(
        run.status.success(),
        "received-fd upgrade run failed\n{}",
        run.combined()
    );
    for marker in [
        "flock-received-upgrade-refused errno=37",
        "flock-received-upgrade-preserved-shared-lock",
        "flock-received-upgrade-ok",
    ] {
        assert!(
            run.stdout.contains(marker),
            "missing {marker}\n{}",
            run.combined()
        );
    }
    assert!(
        run.stderr.contains(
            "existed before Detcore observed its lock state, so a nonblocking probe could \
             destroy a lock that cannot be restored. LOCK_NB is the supported form where the \
             backend supports flock at all; ENOLCK is returned instead only under \
             --allow-unsupported-syscalls."
        ),
        "the run did not take the unknown-state refusal path\n{}",
        run.combined()
    );
}

/// A copied DBT fork child cannot enter the Rust Detcore syscall handler. Its
/// blocking flock must therefore fail closed before reaching the kernel; otherwise
/// it sleeps behind the parent lock while the parent waits for the child.
#[cfg(feature = "dbt")]
#[test]
#[ignore = "https://github.com/rrnewton/hermit/issues/4173: DBT kills the run (exit 137) on the refused fork child's flock instead of a clean refusal"]
fn dbt_forked_child_blocking_flock_fails_closed_without_deadlock() {
    let run = hermit_run_backend_timeout("dbt", "error", &[], "fork-blocking-refusal", "5s");
    assert_eq!(
        run.status.code(),
        Some(0),
        "copied DBT child flock must return ENOLCK, not time out or run natively\n{}",
        run.combined()
    );
    for marker in [
        "flock-fork-child-blocking-refused errno=37",
        "flock-fork-child-refusal-ok",
    ] {
        assert!(
            run.stdout.contains(marker),
            "missing {marker}\n{}",
            run.combined()
        );
    }
}

/// Copied DBT children still use the kernel for operations that cannot block:
/// malformed operations retain EINVAL, nonblocking locks retain their real
/// result, and unlock changes the shared open file description.
#[cfg(feature = "dbt")]
#[test]
fn dbt_forked_child_preserves_safe_flock_operations() {
    let run = hermit_run_backend_timeout("dbt", "error", &[], "fork-safe-operations", "5s");
    assert_eq!(
        run.status.code(),
        Some(0),
        "copied DBT child safe flock operations changed or timed out\n{}",
        run.combined()
    );
    for marker in [
        "flock-fork-child-malformed-einval",
        "flock-fork-child-nonblocking-contended errno=11",
        "flock-fork-child-nonblocking-ok",
        "flock-fork-child-unlock-ok",
        "flock-fork-child-safe-operations-ok",
    ] {
        assert!(
            run.stdout.contains(marker),
            "missing {marker}\n{}",
            run.combined()
        );
    }
}

/// A copied DBT fork child is already outside the ordinary Detcore syscall
/// path. Its nested vfork child nevertheless reaches the copied-syscall policy
/// on the pinned Reverie runtime, where blocking flock returns ENOLCK before
/// reaching the kernel. Native Linux times out on the identical operation.
///
/// The exact event shape distinguishes this from both ordinary Detcore flock
/// handling and the root-process vfork guard: only the root's two setup and two
/// cleanup flocks appear as Detcore inbound events, while the nested child
/// reports ENOLCK and exits.
/// Removing only `copied_child_flock_action`'s blocking refusal makes the DBT
/// half time out with status 124, just like the native control.
#[cfg(feature = "dbt")]
#[test]
fn dbt_nested_vfork_child_blocking_flock_reaches_the_copied_policy() {
    let mut native = Command::new("timeout");
    native
        .args(["--kill-after", "1s", "2s"])
        .arg(guest())
        .arg("fork-vfork-blocking");
    let native = finish(hermit_test::hermit_output(
        &format!("{native:?}"),
        &mut native,
    ));
    assert_eq!(
        native.status.code(),
        Some(124),
        "the native control must block in the kernel or the DBT comparison proves nothing\n{}",
        native.combined()
    );
    assert!(
        native.stdout.contains("flock-fork-vfork-child-entered"),
        "the native control never reached the copied-fork-child analogue\n{}",
        native.combined()
    );

    let run = hermit_run_backend_timeout("dbt", "info", &[], "fork-vfork-blocking", "5s");
    assert_eq!(
        run.status.code(),
        Some(0),
        "the copied-syscall flock guard did not stop the nested vfork child before the kernel block\n{}",
        run.combined()
    );
    for marker in [
        "flock-fork-vfork-child-entered",
        "flock-nested-vfork-blocking-refused errno=37",
        "flock-fork-vfork-copied-policy-ok",
    ] {
        assert!(
            run.stdout.contains(marker),
            "missing {marker}\n{}",
            run.combined()
        );
    }
    assert_eq!(
        run.stderr.matches("inbound syscall: flock").count(),
        4,
        "the nested flock unexpectedly entered the ordinary Detcore syscall handler\n{}",
        run.combined()
    );
    assert!(
        !run.stderr.contains(
            "detcore-dbt: refusing vfork while an open file description may hold a flock"
        ),
        "the root-process vfork guard fired instead of the copied-child flock policy\n{}",
        run.combined()
    );
    assert!(
        !run.stderr.contains(&format!(
            "unsupported syscall {} in copied child",
            libc::SYS_vfork
        )),
        "the nested vfork itself was refused instead of preserving its safe copied-child path\n{}",
        run.combined()
    );
    assert!(
        !run.stderr.contains("blocking flock(fd="),
        "the nested flock unexpectedly reached the ordinary Detcore refusal path\n{}",
        run.combined()
    );
}

fn assert_failed_clone_preserves_known_flock_state(backend: &str) {
    let run = hermit_run_backend(backend, "error", &[], "failed-clone");
    assert!(
        run.status.success(),
        "{backend} failed-clone run failed\n{}",
        run.combined()
    );
    for marker in [
        "flock-failed-clone-rejected errno=22",
        "flock-after-failed-clone-acquired",
        "flock-failed-clone-ok",
    ] {
        assert!(
            run.stdout.contains(marker),
            "{backend} missed {marker}\n{}",
            run.combined()
        );
    }
}

/// A failed process-clone syscall creates no child capable of changing an
/// inherited open file description. It must therefore preserve known flock
/// state. The invalid CLONE_SIGHAND-without-CLONE_VM call returns EINVAL; an
/// uncontended blocking LOCK_EX immediately afterwards must still succeed.
#[test]
fn failed_process_clone_preserves_known_flock_state() {
    assert_failed_clone_preserves_known_flock_state("ptrace");
}

#[cfg(feature = "dbt")]
#[test]
fn dbt_failed_process_clone_preserves_known_flock_state() {
    assert_failed_clone_preserves_known_flock_state("dbt");
}

fn assert_pidfd_getfd_alias_mutation_invalidates_source_flock_authority(backend: &str) {
    let run = hermit_run_backend(
        backend,
        "error",
        &["--allow-unsupported-syscalls"],
        "pidfd-getfd",
    );
    assert!(
        run.status.success(),
        "{backend} pidfd_getfd flock-alias run failed\n{}",
        run.combined()
    );
    for marker in [
        "flock-pidfd-duplicate-unlocked",
        "flock-pidfd-source-upgrade-refused errno=37",
        "flock-pidfd-stale-restore-absent",
        "flock-pidfd-failed-getfd-preserved errno=9",
        "flock-pidfd-valid-pidfd-valid-targetfd-flags-precedence errno=22",
        "flock-pidfd-valid-pidfd-invalid-targetfd-flags-precedence errno=22",
        "flock-pidfd-invalid-pidfd-valid-targetfd-flags-precedence errno=22",
        "flock-pidfd-invalid-pidfd-invalid-targetfd-flags-precedence errno=22",
        "flock-pidfd-recorded-failure-preserved errno=22",
        "flock-pidfd-unrelated-authority-preserved",
        "flock-pidfd-foreign-source-refused errno=95",
        "flock-pidfd-getfd-ok",
    ] {
        assert!(
            run.stdout.contains(marker),
            "{backend} missed {marker}\n{}",
            run.combined()
        );
    }
}

/// A self `pidfd_getfd` result aliases the source open file description. After
/// the duplicate releases its lock, Detcore must not let the source's stale
/// cache restore that lock during a refused blocking conversion. Failed calls
/// preserve source authority, and duplicating an unrelated descriptor does not
/// poison an independently held lock.
#[test]
fn pidfd_getfd_alias_mutation_invalidates_source_flock_authority() {
    assert_pidfd_getfd_alias_mutation_invalidates_source_flock_authority("ptrace");
}

#[test]
fn pidfd_getfd_relaxed_mode_refuses_before_kernel_injection() {
    let run = hermit_run(
        "debug",
        &["--no-sequentialize-threads", "--allow-unsupported-syscalls"],
        "pidfd-getfd-relaxed-refusal",
    );
    assert!(
        run.status.success() && run.stdout.contains("flock-pidfd-relaxed-refused errno=95"),
        "relaxed-mode pidfd_getfd did not return EOPNOTSUPP and continue\n{}",
        run.combined()
    );
    assert!(
        !run.stderr
            .contains("beginning inject of syscall: pidfd_getfd"),
        "relaxed-mode pidfd_getfd reached the kernel\n{}",
        run.combined()
    );
}

fn assert_transferred_lock_state_is_unknown_to_the_sender(backend: &str) {
    for scenario in ["sent-after-fork", "sent-after-fork-mmsg"] {
        let run = hermit_run_backend(
            backend,
            "error",
            &["--allow-unsupported-syscalls"],
            scenario,
        );
        assert!(
            run.status.success(),
            "{backend} transferred-lock run failed\n{}",
            run.combined()
        );
        for marker in [
            "flock-sender-locked-after-fork",
            "flock-receiver-unlocked-transferred-lock",
            "flock-sender-upgrade-refused errno=37",
            "flock-transfer-release-remained-unlocked",
            "flock-sent-after-fork-ok",
        ] {
            assert!(
                run.stdout.contains(marker),
                "{backend} missed {marker}\n{}",
                run.combined()
            );
        }
    }
}

/// A successful SCM_RIGHTS transfer creates another process that can mutate the
/// same open file description. Once the receiver unlocks it, the sender must not
/// restore its stale pre-transfer shared mode during a later failed conversion.
#[test]
fn transferred_lock_state_is_unknown_to_the_sender() {
    assert_transferred_lock_state_is_unknown_to_the_sender("ptrace");
}

/// A failed send transfers no descriptor, so the sender retains authoritative
/// flock state and an uncontended blocking conversion must still succeed.
#[test]
fn failed_send_preserves_known_flock_state() {
    let run = hermit_run("error", &[], "failed-send");
    assert!(
        run.status.success(),
        "failed-send run failed\n{}",
        run.combined()
    );
    for marker in [
        "flock-failed-send-rejected errno=9",
        "flock-after-failed-send-acquired",
    ] {
        assert!(
            run.stdout.contains(marker),
            "missing {marker}\n{}",
            run.combined()
        );
    }
}

/// A positive sendmmsg result says at least one message was consumed, but mutable
/// guest metadata cannot safely identify a narrower descriptor set across a
/// deschedule. The conservative rule makes all cached flock modes unknown.
#[test]
fn partial_sendmmsg_invalidates_all_flock_state() {
    let run = hermit_run(
        "error",
        &["--allow-unsupported-syscalls"],
        "partial-sendmmsg",
    );
    assert!(
        run.status.success(),
        "partial-sendmmsg run failed\n{}",
        run.combined()
    );
    for marker in [
        "flock-partial-sendmmsg-sent-one",
        "flock-partial-sendmmsg-invalidated-all",
    ] {
        assert!(
            run.stdout.contains(marker),
            "missing {marker}\n{}",
            run.combined()
        );
    }
}

/// A DBT vfork child is not observable by the external runtime before it execs
/// or exits. If an inherited open file description already holds a flock, a
/// blocking conversion in that child can deadlock the complete process tree.
/// Refuse that unsafe vfork before copying, while still allowing vfork when no
/// known flock is held. The exact exit and diagnostic distinguish refusal from
/// a timeout.
#[cfg(feature = "dbt")]
#[test]
fn dbt_vfork_child_flock_fails_closed_without_deadlock() {
    let run = hermit_run_backend_timeout("dbt", "info", &[], "vfork-upgrade", "5s");
    assert_eq!(
        run.status.code(),
        Some(101),
        "copied DBT vfork flock must fail closed with exit 101, not time out or return\n{}",
        run.combined()
    );
    assert!(
        run.stderr.contains(DBT_VFORK_FLOCK_REFUSAL),
        "copied DBT vfork did not report the flock refusal\n{}",
        run.combined()
    );
}

/// A successful process fork makes inherited flock state unknown. Unknown
/// alone no longer refuses; this scenario took `LOCK_SH|LOCK_NB` before the
/// fork, so a lock was granted through the description and may still be held,
/// and the same vfork guard must refuse rather than let the unobservable child
/// block in the kernel or change that lock natively.
#[cfg(feature = "dbt")]
#[test]
fn dbt_vfork_with_unknown_flock_state_fails_closed_without_deadlock() {
    // Eight seconds provides policy margin over the censored five-second
    // failures under load; it does not establish that the stall is fixed.
    // The per-test CPU and wall limits still contain a persistent stall.
    let run = hermit_run_backend_timeout("dbt", "info", &[], "vfork-unknown-upgrade", "8s");
    assert_eq!(
        run.status.code(),
        Some(101),
        "DBT vfork with unknown flock state must fail closed, not time out\n{}",
        run.combined()
    );
    assert!(
        run.stderr.contains(DBT_VFORK_FLOCK_REFUSAL),
        "DBT vfork with unknown flock state missed its refusal diagnostic\n{}",
        run.combined()
    );
}

/// When no descriptor can possibly carry flock state, ordinary DBT vfork remains
/// available. This brackets the conservative pre-copy refusal above.
#[cfg(feature = "dbt")]
#[test]
fn dbt_vfork_without_flock_state_still_runs() {
    let run = hermit_run_backend_timeout("dbt", "error", &[], "vfork-no-flock-state", "5s");
    assert_eq!(
        run.status.code(),
        Some(0),
        "DBT vfork without possible flock state must remain available\n{}",
        run.combined()
    );
}

/// Run `pipe-fork-vfork` at L2 on `backend`: `--strict --verify
/// --verify-strict`, requiring bitwise parity over a nonzero, equal number of
/// compared INFO messages.
fn assert_pipe_fork_then_vfork_runs_at_l2(backend: &str) {
    let report_dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("create the verification report directory");
    let report = report_dir.path().join("verify.json");
    let report_arg = report.to_str().expect("UTF-8 report path");
    let run = hermit_run_backend(
        backend,
        "info",
        &[
            "--strict",
            "--verify",
            "--verify-strict",
            "--verify-json",
            report_arg,
        ],
        "pipe-fork-vfork",
    );
    assert!(
        run.status.success(),
        "{backend}: a pipe, a fork, then vfork must run under strict verification\n{}",
        run.combined()
    );
    assert!(
        run.stdout.contains("flock-pipe-fork-vfork-ok lock=0"),
        "{backend}: the guest did not reach its marker\n{}",
        run.combined()
    );
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(&report).unwrap_or_else(|error| {
            panic!(
                "{backend}: no verification report: {error}\n{}",
                run.combined()
            )
        }))
        .unwrap_or_else(|error| panic!("{backend}: the verification report is not JSON: {error}"));
    assert_eq!(
        report["bitwise_parity"],
        serde_json::Value::Bool(true),
        "{backend}: {report}"
    );
    let compared = &report["compared_log_messages"];
    assert!(
        compared["left"].as_u64().is_some_and(|count| count > 0)
            && compared["left"] == compared["right"],
        "{backend}: verification compared no INFO messages: {report}"
    );
}

/// A pipe, an ordinary fork whose child exits, then vfork and exec: the shape
/// every `make -jN` reaches, and one that never calls `flock`. The ptrace
/// backend has no vfork flock guard; this is the reference the DBT case below
/// must match.
#[test]
fn pipe_fork_then_vfork_runs_at_l2() {
    assert_pipe_fork_then_vfork_runs_at_l2("ptrace");
}

/// The same guest on DBT. Before the guard asked whether a lock was ever
/// granted, the fork invalidated the pipe's flock cache, and an invalidated
/// description refused the vfork with exit 101 and
/// [`DBT_VFORK_FLOCK_REFUSAL`]: every DBT `make -j4` and `make -j8` cell
/// stopped there in its first run.
#[cfg(feature = "dbt")]
#[test]
fn dbt_pipe_fork_then_vfork_runs_at_l2() {
    assert_pipe_fork_then_vfork_runs_at_l2("dbt");
}

/// The negative controls for the two cases above: the same guest with a
/// `LOCK_EX` taken on the pipe, before the fork (a mode the fork then makes
/// unknown) or after it (a grant served while the mode was already unknown).
/// Either way the description may hold a lock a copied vfork child could
/// change behind Detcore's back, so the vfork must still be refused.
#[cfg(feature = "dbt")]
#[test]
fn dbt_vfork_after_a_pipe_lock_and_a_fork_fails_closed() {
    for scenario in ["locked-pipe-fork-vfork", "pipe-fork-locked-vfork"] {
        let run = hermit_run_backend_timeout("dbt", "info", &[], scenario, "8s");
        assert_eq!(
            run.status.code(),
            Some(101),
            "DBT {scenario} must fail closed with exit 101, not time out or run\n{}",
            run.combined()
        );
        assert!(
            run.stderr.contains(DBT_VFORK_FLOCK_REFUSAL),
            "DBT {scenario} missed the vfork flock refusal\n{}",
            run.combined()
        );
        assert!(
            !run.stdout.contains("flock-pipe-fork-vfork-ok"),
            "DBT {scenario} completed the vfork it must refuse\n{}",
            run.combined()
        );
    }
}

/// A forked holder takes `LOCK_SH` on a fresh description, so its Detcore
/// holds a known mode for it. The parent's vfork passes the up-front guard
/// (nothing the parent holds was ever locked), and its copied child calls
/// `pidfd_getfd` on the holder's descriptor, then `LOCK_UN`. Natively that
/// would release the holder's lock under its held claim. DBT never runs the
/// copied child's `pidfd_getfd`, under any policy (base refused this shape at
/// the vfork under every policy): `run --strict` ends the run with exit 101
/// and the named refusal; the `--allow-unsupported-syscalls` opt-out names
/// the refusal and answers EPERM, and the guest goes on. Not `--verify`:
/// evidence capture refuses the call on its own in verify runs, which would
/// hide a missing refusal.
#[cfg(feature = "dbt")]
#[test]
fn dbt_copied_child_pidfd_getfd_is_refused_under_every_policy() {
    let run = hermit_run_backend_timeout("dbt", "info", &["--strict"], "copied-pidfd-getfd", "30s");
    assert_eq!(
        run.status.code(),
        Some(101),
        "--strict: a copied child's pidfd_getfd must fail closed with exit 101\n{}",
        run.combined()
    );
    assert!(
        run.stderr.contains(DBT_COPIED_PIDFD_GETFD_REFUSAL),
        "--strict: the copied child's pidfd_getfd missed its named refusal\n{}",
        run.combined()
    );
    assert!(
        !run.stdout.contains("reached pidfd_getfd") && !run.stdout.contains("refused errno"),
        "--strict: the copied child's pidfd_getfd ran or returned\n{}",
        run.combined()
    );
    let run = hermit_run_backend_timeout(
        "dbt",
        "info",
        &["--allow-unsupported-syscalls"],
        "copied-pidfd-getfd",
        "30s",
    );
    assert_eq!(
        run.status.code(),
        Some(0),
        "--allow-unsupported-syscalls: the copied child's pidfd_getfd must answer EPERM\n{}",
        run.combined()
    );
    assert!(
        run.stderr.contains(DBT_COPIED_PIDFD_GETFD_REFUSAL)
            && run
                .stdout
                .contains("flock-copied-pidfd-getfd-refused errno=EPERM"),
        "--allow-unsupported-syscalls: the copied child's pidfd_getfd was not refused with EPERM\n{}",
        run.combined()
    );
}

/// Codex's SCM_RIGHTS sequence: the parent opens `/dev/urandom` (promoted to
/// DBT's shared model at the fork), a forked holder takes `LOCK_SH` on it, so
/// the shared model holds a known mode, and sends it over a socketpair; the
/// parent closes its own copy and vforks, which the up-front guard admits
/// (nothing the parent still holds was locked through it). The copied child
/// would receive the description natively and unlock it under the holder's
/// claim. DBT never runs the copied child's `recvmsg`: `--strict` ends the
/// run with exit 101 and the named refusal; `--allow-unsupported-syscalls`
/// names the refusal and answers EPERM.
#[cfg(feature = "dbt")]
#[test]
fn dbt_copied_child_recvmsg_import_is_refused_under_every_policy() {
    let run =
        hermit_run_backend_timeout("dbt", "info", &["--strict"], "copied-recvmsg-import", "30s");
    assert_eq!(
        run.status.code(),
        Some(101),
        "--strict: a copied child's recvmsg must fail closed with exit 101\n{}",
        run.combined()
    );
    assert!(
        run.stderr.contains(DBT_COPIED_RECVMSG_REFUSAL)
            && !run.stdout.contains("reached recvmsg")
            && !run.stdout.contains("refused errno"),
        "--strict: the copied child's recvmsg was not refused by name\n{}",
        run.combined()
    );
    let run = hermit_run_backend_timeout(
        "dbt",
        "info",
        &["--allow-unsupported-syscalls"],
        "copied-recvmsg-import",
        "30s",
    );
    assert_eq!(
        run.status.code(),
        Some(0),
        "--allow-unsupported-syscalls: the copied child's recvmsg must answer EPERM\n{}",
        run.combined()
    );
    assert!(
        run.stderr.contains(DBT_COPIED_RECVMSG_REFUSAL)
            && run
                .stdout
                .contains("flock-copied-recvmsg-import-refused errno=EPERM"),
        "--allow-unsupported-syscalls: the copied child's recvmsg was not refused with EPERM\n{}",
        run.combined()
    );
}

/// The copied-child OFD refusal covers only lock changes. A copied vfork
/// child's `F_OFD_GETLK` (a query) and POSIX `F_SETLK` (a process-owned lock
/// the ledger does not record) still run natively under `--strict`.
#[cfg(feature = "dbt")]
#[test]
fn dbt_copied_child_ofd_getlk_and_posix_locks_stay_native() {
    let run =
        hermit_run_backend_timeout("dbt", "info", &["--strict"], "copied-native-locks", "30s");
    assert_eq!(
        run.status.code(),
        Some(0),
        "a copied child's F_OFD_GETLK and POSIX F_SETLK must run\n{}",
        run.combined()
    );
    assert!(
        run.stdout.contains("flock-copied-native-locks-ok"),
        "the copied child's native lock calls did not complete\n{}",
        run.combined()
    );
}

#[cfg(feature = "dbt")]
#[test]
fn dbt_clone_vfork_forms_fail_closed_before_copy() {
    for scenario in ["clone-vfork-upgrade", "clone3-vfork-upgrade"] {
        let run = hermit_run_backend_timeout("dbt", "info", &[], scenario, "5s");
        assert_eq!(
            run.status.code(),
            Some(101),
            "DBT {scenario} must fail closed before copying, not time out or return\n{}",
            run.combined()
        );
        assert!(
            run.stderr.contains(DBT_VFORK_FLOCK_REFUSAL),
            "DBT {scenario} missed the vfork-family refusal diagnostic\n{}",
            run.combined()
        );
    }
}

#[cfg(feature = "dbt")]
#[test]
fn dbt_process_clone_files_is_refused_before_copied_child_mutation() {
    for scenario in ["clone-files-process", "clone3-files-process"] {
        let native = finish(hermit_test::hermit_output(
            "hermit run",
            Command::new(guest()).arg(scenario),
        ));
        assert!(
            native.status.success()
                && native
                    .stdout
                    .contains(&format!("flock-{scenario}-shared-mutation-observed")),
            "native {scenario} did not prove CLONE_FILES table sharing\n{}",
            native.combined()
        );

        let run = hermit_run_backend_timeout("dbt", "info", &[], scenario, "5s");
        assert_eq!(
            run.status.code(),
            Some(101),
            "DBT {scenario} must fail closed before copying\n{}",
            run.combined()
        );
        assert!(
            run.stderr.contains(DBT_PROCESS_CLONE_FILES_REFUSAL),
            "DBT {scenario} missed the shared-files pre-copy diagnostic\n{}",
            run.combined()
        );
        assert!(
            !run.stdout
                .contains(&format!("flock-{scenario}-shared-mutation-observed")),
            "DBT {scenario} executed the copied child's descriptor mutation\n{}",
            run.combined()
        );
    }
}

/// Replay must take the kernel lock again for a materialized file, not merely
/// repeat the recorded return value.
///
/// `Replayer::handle_simple` consumes the recorded `Return` and injects
/// nothing, which is valid only when nothing outside the return value depends
/// on the call. `flock`'s entire product is kernel state, so under
/// `handle_simple` a replayed run *printed exactly the output asserted below
/// while holding no locks at all* -- which is why this test counts injections
/// instead of trusting stdout.
///
/// Bracket, measured on this guest at this commit: 5 guest `flock` calls, 5
/// re-issued to the kernel during replay. With the arm reverted to
/// `handle_simple` the count is 0 and the stdout assertions still pass -- the
/// silent failure this test exists to catch.
#[test]
fn replay_reissues_every_flock_for_a_materialized_file() {
    let _guard = record_lock();
    let data_dir = tempfile::tempdir().expect("failed to create the flock recording directory");

    let mut record = Command::new("timeout");
    record
        .args(["--kill-after", "10s", "180s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=off", "record", "start", "--record-timeout=120"])
        .arg(format!("--data-dir={}", data_dir.path().display()))
        .args(["--"])
        .arg(guest())
        .arg("exclusion");
    let recorded = finish(hermit_test::hermit_output(
        &format!("{record:?}"),
        &mut record,
    ));
    assert!(
        recorded.status.success(),
        "recording the flock exclusion guest failed\n{}",
        recorded.combined()
    );
    assert!(
        recorded.stdout.contains("flock-exclusion-ok"),
        "the recorded run did not complete the exclusion scenario\n{}",
        recorded.combined()
    );

    // `--log=debug` surfaces reverie's injection of each syscall the replayer
    // re-issues. That line is the witness that the kernel really performed the
    // lock operation during replay.
    let mut replay = Command::new("timeout");
    replay
        .args(["--kill-after", "10s", "180s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=debug", "replay", "--autopilot"])
        .arg(format!("--data-dir={}", data_dir.path().display()));
    let replayed = finish(hermit_test::hermit_output(
        &format!("{replay:?}"),
        &mut replay,
    ));
    assert!(
        replayed.status.success(),
        "replaying the flock exclusion guest failed\n{}",
        replayed.combined()
    );

    let requested = replayed.stderr.matches("inbound syscall: flock").count();
    let injected = replayed
        .stderr
        .matches("beginning inject of syscall: flock")
        .count();
    assert!(
        requested > 0,
        "the replayed guest issued no flock calls at all; the probe is measuring nothing\n{}",
        replayed.combined()
    );
    assert_eq!(
        injected,
        requested,
        "replay re-issued {injected} of {requested} flock calls to the kernel; a replayed \
         flock that is not re-issued establishes no lock, so the replayed run only claims to \
         hold one\n{}",
        replayed.combined()
    );

    for marker in [
        "flock-second-open-excluded",
        "flock-second-process-excluded",
        "flock-exclusion-ok",
    ] {
        assert!(
            replayed.stdout.contains(marker),
            "missing {marker} in the replayed run\n{}",
            replayed.combined()
        );
    }
}

/// pidfd_getfd creates a real descriptor alias, so replay must both consume an
/// exact recorded result and execute the syscall again. The guest proves that
/// the successful duplicate shares one file offset with its source, then
/// brackets four nonzero-flags failures and one pre-injection validation error.
#[test]
fn replay_reissues_pidfd_getfd_success_and_failure() {
    let _guard = record_lock();
    let data_dir =
        tempfile::tempdir().expect("failed to create the pidfd_getfd recording directory");

    let mut record = Command::new("timeout");
    record
        .args(["--kill-after", "10s", "180s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=off", "record", "start", "--record-timeout=120"])
        .arg(format!("--data-dir={}", data_dir.path().display()))
        .args(["--"])
        .arg(guest())
        .arg("pidfd-getfd-record");
    let recorded = finish(hermit_test::hermit_output(
        &format!("{record:?}"),
        &mut record,
    ));
    assert!(
        recorded.status.success(),
        "recording pidfd_getfd failed\n{}",
        recorded.combined()
    );
    assert!(recorded.stdout.contains("flock-pidfd-record-ok"));

    let mut replay = Command::new("timeout");
    replay
        .args(["--kill-after", "10s", "180s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=debug", "replay", "--autopilot"])
        .arg(format!("--data-dir={}", data_dir.path().display()));
    let replayed = finish(hermit_test::hermit_output(
        &format!("{replay:?}"),
        &mut replay,
    ));
    assert!(
        replayed.status.success(),
        "replaying pidfd_getfd failed\n{}",
        replayed.combined()
    );
    for marker in [
        "flock-pidfd-record-success-shared-ofd",
        "flock-pidfd-record-validation-failure errno=9",
        "flock-pidfd-record-flags-failures errno=22 count=4",
        "flock-pidfd-record-ok",
    ] {
        assert!(
            replayed.stdout.contains(marker),
            "replayed pidfd_getfd scenario missed {marker}\n{}",
            replayed.combined()
        );
    }
    assert_eq!(
        replayed
            .stderr
            .matches("beginning inject of syscall: pidfd_getfd")
            .count(),
        5,
        "replay must re-execute the successful pidfd_getfd call and all four flags-first EINVAL calls\n{}",
        replayed.combined()
    );
}

/// Replay cannot reproduce the lock side effect for an external file that was
/// not materialized in the replay root. It must fail closed rather than replay
/// the recorded success while holding no lock.
#[test]
fn replay_refuses_flock_for_a_non_materialized_file() {
    let _guard = record_lock();
    let data_dir = tempfile::tempdir().expect("failed to create the flock recording directory");
    let mut record = Command::new("timeout");
    record
        .args(["--kill-after", "10s", "180s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=off", "record", "start", "--record-timeout=120"])
        .arg(format!("--data-dir={}", data_dir.path().display()))
        .args(["--"])
        .arg(guest())
        .arg("holder")
        .arg("/etc/hosts");
    let recorded = finish(hermit_test::hermit_output(
        &format!("{record:?}"),
        &mut record,
    ));
    assert!(
        recorded.status.success(),
        "recording flock on the external file failed\\n{}",
        recorded.combined()
    );
    assert!(recorded.stdout.contains("flock-holder-ok"));

    let mut replay = Command::new("timeout");
    replay
        .args(["--kill-after", "10s", "180s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=off", "replay", "--autopilot"])
        .arg(format!("--data-dir={}", data_dir.path().display()));
    let replayed = finish(hermit_test::hermit_output(
        &format!("{replay:?}"),
        &mut replay,
    ));
    assert_ne!(
        replayed.status.code(),
        Some(124),
        "external-file replay timed out instead of failing closed\\n{}",
        replayed.combined()
    );
    assert!(
        !replayed.status.success(),
        "external-file replay reported success without reproducing the flock side effect\\n{}",
        replayed.combined()
    );
    assert!(
        replayed.stderr.contains("cannot replay flock side effects")
            && replayed.stderr.contains("outside the replay root"),
        "external-file replay did not report the unsupported flock side effect\\n{}",
        replayed.combined()
    );
}

/// Record/replay is fail-closed and has no compatibility opt-out. A contended
/// blocking flock therefore invalidates the recording after Detcore restores
/// the shared lock that its nonblocking probe temporarily displaced. It must
/// not silently record the compatibility-mode `ENOLCK` fallback.
#[test]
fn recording_refuses_a_contended_blocking_flock() {
    let _guard = record_lock();
    let data_dir = tempfile::tempdir().expect("failed to create the flock recording directory");

    let mut record = Command::new("timeout");
    record
        .args(["--kill-after", "10s", "180s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=error", "record", "start", "--record-timeout=120"])
        .arg(format!("--data-dir={}", data_dir.path().display()))
        .args(["--"])
        .arg(guest())
        .arg("upgrade");
    let recorded = finish(hermit_test::hermit_output(
        &format!("{record:?}"),
        &mut record,
    ));
    assert_ne!(
        recorded.status.code(),
        Some(124),
        "recording the contended flock upgrade wedged\n{}",
        recorded.combined()
    );
    assert!(
        !recorded.status.success(),
        "recording accepted a deterministic fallback that record/replay's fail-closed policy forbids\n{}",
        recorded.combined()
    );
    for marker in [
        "flock-upgrade-parent-holds-shared",
        "flock-upgrade-contender-holds-shared",
    ] {
        assert!(
            recorded.stdout.contains(marker),
            "recording never reached the contended conversion; refusal would prove nothing\n{}",
            recorded.combined()
        );
    }
    assert!(
        !recorded.stdout.contains("flock-upgrade-refused errno=37")
            && recorded.stderr.contains("unsupported syscall: flock"),
        "recording did not fail closed at the unsupported blocking flock\n{}",
        recorded.combined()
    );
}

/// A recording made before flock forwarding must be refused, not replayed.
///
/// Under the old handler `flock` returned `Ok(0)` before ever reaching
/// `record_or_replay`, so a 0x10b recording contains no flock event. This
/// replayer expects one per call, so replaying such a stream would consume the
/// *next* event for every flock and desynchronize the run. `RECORD_VERSION` was
/// bumped to 0x10c precisely so the gate in `hermit-cli/src/replay.rs` refuses
/// it up front.
///
/// The fixture is a real current recording with only its metadata version
/// rewritten, so this exercises the live gate rather than a hand-built file,
/// and the unmodified recording is replayed first to prove the refusal comes
/// from the version and not from a broken fixture.
#[test]
fn pre_flock_recordings_are_refused_by_the_version_gate() {
    let _guard = record_lock();
    let data_dir = tempfile::tempdir().expect("failed to create the flock recording directory");

    let mut record = Command::new("timeout");
    record
        .args(["--kill-after", "10s", "180s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=off", "record", "start", "--record-timeout=120"])
        .arg(format!("--data-dir={}", data_dir.path().display()))
        .args(["--"])
        .arg(guest())
        .arg("holder");
    let recorded = finish(hermit_test::hermit_output(
        &format!("{record:?}"),
        &mut record,
    ));
    assert!(
        recorded.status.success(),
        "recording the flock holder guest failed\n{}",
        recorded.combined()
    );

    let replay_command = |dir: &Path| {
        let mut replay = Command::new("timeout");
        replay
            .args(["--kill-after", "10s", "180s"])
            .arg(env!("CARGO_BIN_EXE_hermit"))
            .args(["--log=off", "replay", "--autopilot"])
            .arg(format!("--data-dir={}", dir.display()));
        finish(hermit_test::hermit_output(
            &format!("{replay:?}"),
            &mut replay,
        ))
    };

    // Positive half of the bracket: the untouched recording replays.
    let accepted = replay_command(data_dir.path());
    assert!(
        accepted.status.success(),
        "the current-version recording did not replay; the fixture is broken, so the refusal \
         below would prove nothing\n{}",
        accepted.combined()
    );

    let metadata_path = find_metadata(data_dir.path());
    let text = fs::read_to_string(&metadata_path).expect("failed to read the recording metadata");
    let mut metadata: serde_json::Value =
        serde_json::from_str(&text).expect("recording metadata is not JSON");
    let current = metadata["version"]
        .as_u64()
        .expect("recording metadata has no numeric version");
    assert_eq!(
        current, 0x198,
        "RECORD_VERSION moved; point this test at the current and pre-flock epochs"
    );
    metadata["version"] = serde_json::json!(0x10b);
    fs::write(
        &metadata_path,
        serde_json::to_string(&metadata).expect("failed to serialize the rewritten metadata"),
    )
    .expect("failed to rewrite the recording metadata");

    // Negative half: the same recording, labelled as pre-flock, is refused.
    let refused = replay_command(data_dir.path());
    assert!(
        !refused.status.success(),
        "a 0x10b recording was replayed instead of refused; it has no flock event, so the \
         replay would read another thread's event for every flock\n{}",
        refused.combined()
    );
    assert!(
        refused.stderr.contains("Version mismatch"),
        "the 0x10b recording failed for some reason other than the version gate\n{}",
        refused.combined()
    );
}

fn find_metadata(data_dir: &Path) -> PathBuf {
    for entry in fs::read_dir(data_dir).expect("failed to read the recording directory") {
        let path = entry
            .expect("failed to read a recording directory entry")
            .path();
        let candidate = path.join("metadata.json");
        if candidate.is_file() {
            return candidate;
        }
    }
    panic!("no recording metadata.json below {}", data_dir.display());
}
