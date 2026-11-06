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
//! | [`contended_blocking_upgrade_waits_for_the_release`] | a contended blocking flock was refused, where Linux waits (Detcore's flock model, the kvm lane's slice SP2) |
//! | [`contended_blocking_upgrade_waits_at_l2_under_strict`] | the same wait, deterministic at L2 |
//! | [`kernel_path_contended_blocking_upgrade_restores_the_shared_lock_and_is_refused`] | on a file the kernel decides, the `LOCK_NB` probe of a refused blocking conversion destroyed the shared lock the caller held |
//! | [`kernel_path_contended_blocking_upgrade_is_fail_closed_under_strict`] | the same refusal handed back an errno, or a timeout, under a fail-closed run |
//! | [`kernel_path_unknown_state_blocking_upgrade_is_refused_before_any_probe`] | a blocking conversion through a description of unknown mode was probed, which could delete a lock Detcore cannot restore |
//! | [`an_outside_lock_refuses_the_mirror_and_the_model_lets_the_grant_go`] | after a refused mirror the model kept the grant the guest was told it did not get |
//! | [`a_received_description_holds_the_senders_lock`] | a description received through `SCM_RIGHTS` lost, or kept, its lock other than as Linux does |
//! | [`a_private_table_closing_every_descriptor_frees_the_lock_on_ptrace`] | a description a private-table thread had referenced kept its lock in the model after every descriptor closed |
//! | [`a_job_control_stop_of_a_waiter_is_refused_by_name_on_ptrace`] | a stopped waiter stayed queued, and was granted while Linux kept it stopped |
//! | [`an_exec_ends_the_flock_wait_of_a_thread_it_kills_on_ptrace`] | a waiter an exec killed stayed queued, took the lock, and kept it for the rest of the run, or left a request blocked under its own forever |
//! | [`a_waiter_kept_by_a_mapping_alone_is_refused_by_name_on_ptrace`] | a waiting call whose descriptor a sibling closed while a mapping kept its description returned 0 with no lock on the host, where the kernel then granted a conflicting lock |
//! | [`dbt_copied_vfork_child_refuses_every_flock_by_name`] | a copied DBT vfork child ran `LOCK_NB` and `LOCK_UN` natively, changing locks behind the flock model |
//! | [`dbt_forked_child_preserves_safe_flock_operations`] | copied-child refusal overmatched malformed, nonblocking, and unlock operations; a DBT fork child's calls now reach the flock model |
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
//! | [`recording_and_replay_wait_for_a_contended_blocking_flock`] | record mode refused a contended blocking flock that Linux waits for |
//! | [`pre_flock_recordings_are_refused_by_the_version_gate`] | a 0x10b recording has no flock event and desynchronized |
//! | [`flock_model_matches_linux_at_l2_on_ptrace`] and [`flock_model_matches_linux_at_l2_on_kvm`] | flock between processes was refused on KVM, and a blocking waiter was refused everywhere |
//! | [`a_mapping_keeps_the_lock_after_the_last_close_on_ptrace`] | the model released a lock at the last close while a mapping kept it |
//! | [`kvm_refuses_the_last_close_of_a_mapped_locked_description`] | the same release on KVM, where no host lock can decide the file |
//! | [`a_description_shared_before_its_first_flock_keeps_the_kernel_path_on_ptrace`] | the model released a lock at a last reference it could not see |
//! | [`kvm_refuses_a_flock_through_a_description_shared_before_its_first_flock`] | the same on KVM |
//! | [`record_and_replay_reproduce_the_flock_model`] | the flock model's mirrors were not recorded, or not re-issued in replay |
//! | [`the_flock_utility_shell_pattern_keeps_the_kernel_path_on_ptrace`] | the shell's `exec 9>lockfile; flock -n 9` was decided from references the model never saw |
//! | [`kvm_refuses_the_flock_utility_shell_pattern_by_name`] | the same on KVM, where no host lock can decide the file |

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::fs;
use std::os::fd::AsRawFd;
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

/// Compile `tests/c/<name>.c` into this test binary's build directory.
fn compile(name: &str) -> PathBuf {
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("flock-exclusion-{}", std::process::id()));
    fs::create_dir_all(&build_root).expect("failed to create the flock guest build directory");
    let binary = build_root.join(name);
    let source = repository().join(format!("tests/c/{name}.c"));
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
}

/// Compile `tests/c/flock_exclusion.c` once per test binary.
fn guest() -> &'static Path {
    static GUEST: OnceLock<PathBuf> = OnceLock::new();
    GUEST.get_or_init(|| compile("flock_exclusion"))
}

/// Compile `tests/c/flock_shared_exclusive.c`, the flock model's
/// cross-process guest, once per test binary.
fn model_guest() -> &'static Path {
    static GUEST: OnceLock<PathBuf> = OnceLock::new();
    GUEST.get_or_init(|| compile("flock_shared_exclusive"))
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

/// The directory in which every run of the flock model's guest, native or
/// under Hermit, keeps its lock files: each run's `TMPDIR` is a fresh
/// directory in it, and under Hermit the guest's private `/tmp` is a
/// directory Hermit makes in that `TMPDIR`. The native output is the
/// reference every backend must print, so both sides need a filesystem on
/// which a file mapping keeps the open file description the guest locked, as
/// Linux's file-description semantics say: `mmap` takes a reference to the
/// mapped `struct file` (`__mmap_new_file_vma`, mm/vma.c in v6.17), so the
/// last `fput` (`__fput`, fs/file_table.c), whose `locks_remove_file`
/// removes the description's flock (fs/locks.c), waits for the last
/// `munmap` (`remove_vma`, mm/vma.c). Overlayfs breaks that: `ovl_mmap`
/// (fs/overlayfs/file.c) maps the backing file through `backing_file_mmap`
/// (fs/backing-file.c), whose `vma_set_file` (mm/util.c) swaps the overlay
/// file for the backing one and drops the mapping's reference to the overlay
/// file, so the overlay description's last close removes its lock while the
/// mapping lives. The validate pinned root's `/tmp` is the container's
/// overlayfs root, where the guest's `mmap-close` and `waiter-mapped` checks
/// fail natively (`child LOCK_EX|LOCK_NB with the mapping left = ok`). So the
/// directory is in the build's own `CARGO_TARGET_TMPDIR`, which the pinned
/// root binds from the host, and a directory on overlayfs is refused rather
/// than compared.
fn flock_scratch_root() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("flock-scratch-{}", std::process::id()));
        fs::create_dir_all(&root).expect("failed to create the flock scratch root");
        let path = std::ffi::CString::new(root.as_os_str().as_encoded_bytes())
            .expect("the flock scratch root has no NUL byte");
        // SAFETY: `path` is a NUL-terminated string and `stat` a writable
        // statfs buffer, both live for the call.
        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::statfs(path.as_ptr(), &mut stat) },
            0,
            "statfs {}: {}",
            root.display(),
            std::io::Error::last_os_error()
        );
        assert_ne!(
            stat.f_type,
            libc::OVERLAYFS_SUPER_MAGIC,
            "{} is on overlayfs, where a mapping does not keep the open file description \
             the guest locked (ovl_mmap maps the backing file), so the native reference \
             would not show Linux's flock semantics",
            root.display()
        );
        root
    })
}

/// Run the flock model's guest (`tests/c/flock_shared_exclusive.c`)
/// `scenario` under `hermit run` on `backend`, with `/proc/locks` visible
/// (`--proc=standard`) and `extra` inserted before `--`, keeping its files
/// where the native reference keeps its own ([`flock_scratch_root`]).
fn hermit_run_model(backend: &str, log: &str, extra: &[&str], scenario: &str) -> Run {
    let scratch = tempfile::tempdir_in(flock_scratch_root())
        .expect("failed to create the Hermit flock scratch directory");
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after", "10s", "300s"])
        .arg(hermit_test::hermit_binary())
        .arg(format!("--log={log}"))
        .args([
            &format!("--backend={backend}"),
            "run",
            "--base-env=minimal",
            "--proc=standard",
        ])
        .args(extra)
        .arg("--")
        .arg(model_guest())
        .arg(scenario)
        .env("TMPDIR", scratch.path());
    hermit_test::configure_guest_execution(&mut command);
    finish(hermit_test::hermit_output(
        &format!("{command:?}"),
        &mut command,
    ))
}

/// The flock model's guest `scenario`, run natively: the output every
/// backend must print, from a lock file on a filesystem with Linux's
/// file-description semantics ([`flock_scratch_root`]).
fn native_model_run(scenario: &str) -> Run {
    let scratch = tempfile::tempdir_in(flock_scratch_root())
        .expect("failed to create the native flock scratch directory");
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after", "10s", "120s"])
        .arg(model_guest())
        .arg(scenario)
        .env("TMPDIR", scratch.path());
    let run = finish(
        command
            .output()
            .unwrap_or_else(|error| panic!("failed to start {command:?}: {error}")),
    );
    assert!(
        run.status.success(),
        "the native flock model guest failed; the comparison would prove nothing\n{}",
        run.combined()
    );
    run
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

const UPGRADE_WAITS_MARKERS: [&str; 5] = [
    "flock-upgrade-parent-holds-shared",
    "flock-upgrade-contender-holds-shared",
    "flock-upgrade-acquired-after-the-release",
    "flock-upgrade-holds-exclusive",
    "flock-upgrade-ok",
];

/// A contended BLOCKING `LOCK_SH` -> `LOCK_EX` conversion waits, as on Linux,
/// and is granted at the contender's release (Detcore's flock model, the kvm
/// lane's slice SP2). Linux deletes the caller's shared lock before it finds
/// the conflict (`flock_lock_inode`), so the caller waits holding nothing;
/// the exclusive lock it ends with then excludes a fresh open.
///
/// Before the model Detcore probed the kernel with `LOCK_NB` and refused the
/// contended call (ENOLCK under `--allow-unsupported-syscalls`), so this
/// guest printed `FAIL: the contended blocking LOCK_EX upgrade failed
/// (errno=37)`.
#[test]
fn contended_blocking_upgrade_waits_for_the_release() {
    let run = hermit_run("off", &["--allow-unsupported-syscalls"], "upgrade-waits");
    assert!(
        run.status.success(),
        "non-strict upgrade run failed\n{}",
        run.combined()
    );
    for marker in UPGRADE_WAITS_MARKERS {
        assert!(
            run.stdout.contains(marker),
            "missing {marker}\n{}",
            run.combined()
        );
    }
}

/// The same wait under a fail-closed strict run, at L2: the waiter and the
/// contender's release are ordered by Detcore's schedule, so the two runs of
/// `--verify --verify-strict` match exactly.
#[test]
fn contended_blocking_upgrade_waits_at_l2_under_strict() {
    let run = hermit_run(
        "info",
        &["--strict", "--verify", "--verify-strict"],
        "upgrade-waits",
    );
    assert!(
        run.status.success(),
        "strict --verify upgrade run failed\n{}",
        run.combined()
    );
    for marker in UPGRADE_WAITS_MARKERS {
        assert!(
            run.stdout.contains(marker),
            "missing {marker}\n{}",
            run.combined()
        );
    }
    assert!(
        run.combined().contains("bitwise parity established"),
        "the upgrade run did not verify at L2\n{}",
        run.combined()
    );
}

const KERNEL_UPGRADE_REFUSAL: &str = "Refusing rather than granting a lock another guest holds. \
     LOCK_NB is the supported form where the backend supports flock at all; ENOLCK is returned \
     instead only under --allow-unsupported-syscalls.";

/// On a file the kernel decides (a description of it was shared with a child
/// before its first flock, which Detcore's flock model cannot follow), a
/// contended BLOCKING `LOCK_SH` -> `LOCK_EX` conversion is still probed with
/// `LOCK_NB`, as before the model: Linux's conversion deletes the caller's
/// shared lock before it finds the conflict (`flock_lock_inode`), so Detcore
/// restores that lock and then refuses the call by name. The guest proves
/// the restored lock through a fresh open after the contender's release.
/// Pre-fix control: with the restore suppressed, the probe acquires and the
/// guest prints `FAIL: the refused upgrade destroyed this description's
/// shared lock`. (The Claude review of the kvm lane's slice SP2: the model's
/// tests had moved off this path.)
#[test]
fn kernel_path_contended_blocking_upgrade_restores_the_shared_lock_and_is_refused() {
    let run = hermit_run("error", &["--allow-unsupported-syscalls"], "kernel-upgrade");
    assert!(
        run.status.success(),
        "kernel-path upgrade run failed\n{}",
        run.combined()
    );
    for marker in [
        "flock-kernel-upgrade-contender-holds-shared",
        "flock-kernel-upgrade-holds-shared",
        // ENOLCK is 37 on Linux/x86_64.
        "flock-kernel-upgrade-refused errno=37",
        "flock-kernel-upgrade-preserved-shared-lock",
        "flock-kernel-upgrade-ok",
    ] {
        assert!(
            run.stdout.contains(marker),
            "missing {marker}\n{}",
            run.combined()
        );
    }
    assert!(
        run.stderr.contains("blocking flock(fd=") && run.stderr.contains(KERNEL_UPGRADE_REFUSAL),
        "the run did not take the kernel path's contended refusal\n{}",
        run.combined()
    );
}

/// The same contention under a fail-closed run stops the run as a policy
/// refusal, with its reason, rather than hand back an errno.
#[test]
fn kernel_path_contended_blocking_upgrade_is_fail_closed_under_strict() {
    let run = hermit_run("error", &["--strict"], "kernel-upgrade");
    assert_eq!(
        run.status.code(),
        Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
        "a contended blocking flock upgrade on the kernel path must fail closed as a policy \
         refusal\n{}",
        run.combined()
    );
    assert!(
        run.combined()
            .contains("HERMIT_POLICY_REFUSAL class=policy-refusal"),
        "the refusal must be machine-readable as a refusal\n{}",
        run.combined()
    );
    assert!(
        run.stdout.contains("flock-kernel-upgrade-holds-shared")
            && !run.stdout.contains("flock-kernel-upgrade-ok"),
        "the strict run did not stop at the contended upgrade\n{}",
        run.combined()
    );
    assert!(
        run.stderr.contains(KERNEL_UPGRADE_REFUSAL),
        "the strict run did not report the kernel path's contended refusal\n{}",
        run.combined()
    );
}

/// A blocking conversion through a description shared before its first
/// flock, whose shared lock a child took through that same description: the
/// caller's Detcore cannot know the description's mode, so a `LOCK_NB` probe
/// could delete a lock it cannot restore, and the kernel path refuses the
/// call by name before any probe. The child's shared lock still excludes a
/// fresh open after the contender's release.
#[test]
fn kernel_path_unknown_state_blocking_upgrade_is_refused_before_any_probe() {
    let run = hermit_run("error", &["--allow-unsupported-syscalls"], "kernel-unknown");
    assert!(
        run.status.success(),
        "kernel-path unknown-state run failed\n{}",
        run.combined()
    );
    for marker in [
        "flock-kernel-unknown-child-holds-shared",
        "flock-kernel-unknown-contender-holds-shared",
        "flock-kernel-unknown-refused errno=37",
        "flock-kernel-unknown-preserved-shared-lock",
        "flock-kernel-unknown-ok",
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
        "the run did not take the unknown-state refusal\n{}",
        run.combined()
    );
}

/// A process outside the run (this test) holds `LOCK_EX` on the file. The
/// flock model grants the guest's `LOCK_EX|LOCK_NB`, its host mirror finds the
/// outside lock, and the call is refused by name, `ENOLCK` under
/// `--allow-unsupported-syscalls`. The model then lets its grant go, as the
/// guest holds no lock, so a second description's `LOCK_SH|LOCK_NB` is granted
/// by the model and refused the same way, never answered EWOULDBLOCK from a
/// lock the first description does not hold (the Claude review of the kvm
/// lane's slice SP2: the model kept the grant). A fail-closed run ends at the
/// first refusal.
#[test]
fn an_outside_lock_refuses_the_mirror_and_the_model_lets_the_grant_go() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the outside-holder directory");
    let path = directory.path().join("outside-holder");
    let holder = fs::File::create(&path).expect("failed to create the outside-holder file");
    // SAFETY: `holder` owns a valid descriptor.
    assert_eq!(
        unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0,
        "the outside holder's LOCK_EX: {}",
        std::io::Error::last_os_error()
    );
    let run_with = |policy: &str| {
        let mut command = Command::new("timeout");
        command
            .args(["--kill-after", "10s", "120s"])
            .arg(hermit_test::hermit_binary())
            .arg("--log=error")
            .args(["--backend=ptrace", "run", "--base-env=minimal", policy])
            .arg("--")
            .arg(guest())
            .arg("outside-holder")
            .arg(&path);
        hermit_test::configure_guest_execution(&mut command);
        finish(hermit_test::hermit_output(
            &format!("{command:?}"),
            &mut command,
        ))
    };
    let run = run_with("--allow-unsupported-syscalls");
    assert!(
        run.status.success(),
        "outside-holder run failed\n{}",
        run.combined()
    );
    for marker in [
        "flock-outside-first errno=37",
        "flock-outside-second errno=37",
        "flock-outside-holder-ok",
    ] {
        assert!(
            run.stdout.contains(marker),
            "missing {marker}\n{}",
            run.combined()
        );
    }
    let run = run_with("--strict");
    assert_eq!(
        run.status.code(),
        Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
        "the refused mirror must end a fail-closed run\n{}",
        run.combined()
    );
    assert!(
        run.stderr
            .contains("a lock its model does not know holds the file on the host")
            && !run.stdout.contains("flock-outside-first"),
        "the fail-closed run did not stop at the refused mirror\n{}",
        run.combined()
    );
    drop(holder);
}

/// A description received through `SCM_RIGHTS` is the sender's: it holds
/// the sender's shared lock, which excludes a fresh open's exclusive lock, and
/// a contended nonblocking conversion through it deletes that lock before it
/// fails, as Linux's conversion does (`flock_lock_inode`). The receipt joins
/// the description its sender's table modeled, and the lock ledger counted
/// the copy in flight, so Detcore's flock model follows the description
/// throughout (the kvm lane's slice SP2). Before the model, Detcore refused a
/// blocking conversion through a received description whose lock history it
/// had not observed.
#[test]
fn a_received_description_holds_the_senders_lock() {
    let run = hermit_run(
        "info",
        &["--strict", "--verify", "--verify-strict"],
        "received",
    );
    assert!(
        run.status.success(),
        "received-description run failed\n{}",
        run.combined()
    );
    for marker in [
        "flock-received-holds-the-shared-lock",
        "flock-received-upgrade-would-block errno=11",
        "flock-received-upgrade-dropped-the-shared-lock",
        "flock-received-upgrade-ok",
    ] {
        assert!(
            run.stdout.contains(marker),
            "missing {marker}\n{}",
            run.combined()
        );
    }
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
        "flock-pidfd-source-upgrade-would-block errno=11",
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
/// the duplicate releases its lock, the source holds none: a nonblocking
/// conversion through it finds only the contender's lock, and no stale state
/// restores the released one. Failed calls preserve source authority, and
/// duplicating an unrelated descriptor does not poison an independently held
/// lock.
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
            "flock-sender-upgrade-would-block errno=11",
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
/// same open file description. Once the receiver unlocks it, the sender holds
/// no stale lock: its nonblocking conversion finds only a contender's, and
/// after that contender's unlock a fresh open takes the exclusive lock.
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

/// A positive sendmmsg result says at least one message was consumed. The
/// description the sent message carried was joined by its receiver, which
/// unlocked it, and the unsent one, which the second message's unreadable
/// control data never carried, stayed with the sender: Detcore's flock model
/// follows both, and grants each one's uncontended blocking conversion, as
/// Linux does. Before the model the conservative rule refused both.
#[test]
fn partial_sendmmsg_keeps_both_descriptions_in_the_flock_model() {
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
        "flock-partial-sendmmsg-both-granted",
    ] {
        assert!(
            run.stdout.contains(marker),
            "missing {marker}\n{}",
            run.combined()
        );
    }
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

    // `--log=debug` here too: the recording's own injections are the flock
    // calls Detcore's model made on the host (its mirrors), which replay must
    // re-issue one for one.
    let mut record = Command::new("timeout");
    record
        .args(["--kill-after", "10s", "180s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=debug", "record", "start", "--record-timeout=120"])
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
    let recorded_mirrors = recorded
        .stderr
        .matches("beginning inject of syscall: flock")
        .count();

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
        requested > 0 && recorded_mirrors > 0,
        "the guest issued no flock calls, or the model mirrored none; the probe is measuring \
         nothing\n{}",
        replayed.combined()
    );
    // Detcore's flock model answers a call that changes no lock (a `LOCK_NB`
    // conflict) without the kernel, and mirrors every other on the host; each
    // mirror is a recorded event.
    assert_eq!(
        injected,
        recorded_mirrors,
        "replay re-issued {injected} of the recording's {recorded_mirrors} flock mirrors \
         ({requested} guest flock calls); a replayed mirror that is not re-issued establishes \
         no lock, so the replayed run only claims to hold one\n{}",
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

/// A contended blocking flock waits under record as in a run (Detcore's flock
/// model), and replay reproduces the wait from the recording: the guest's
/// output is the same in both. Before the model, record mode refused the
/// call, as record and replay have no compatibility opt-out.
#[test]
fn recording_and_replay_wait_for_a_contended_blocking_flock() {
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
        .arg("upgrade-waits");
    let recorded = finish(hermit_test::hermit_output(
        &format!("{record:?}"),
        &mut record,
    ));
    assert!(
        recorded.status.success(),
        "recording the contended flock upgrade failed\n{}",
        recorded.combined()
    );
    for marker in UPGRADE_WAITS_MARKERS {
        assert!(
            recorded.stdout.contains(marker),
            "the recorded run missed {marker}\n{}",
            recorded.combined()
        );
    }

    let mut replay = Command::new("timeout");
    replay
        .args(["--kill-after", "10s", "180s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=error", "replay", "--autopilot"])
        .arg(format!("--data-dir={}", data_dir.path().display()));
    let replayed = finish(hermit_test::hermit_output(
        &format!("{replay:?}"),
        &mut replay,
    ));
    assert!(
        replayed.status.success(),
        "replaying the contended flock upgrade failed\n{}",
        replayed.combined()
    );
    assert_eq!(
        replayed.stdout,
        recorded.stdout,
        "replay printed other output than the recording\n{}",
        replayed.combined()
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
        current, 0x19c,
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

/// Every cross-process flock case of the flock model's guest at L2, with the
/// output native Linux prints: shared and exclusive locks between processes,
/// `LOCK_NB` conflicts, a conversion that drops the held lock first, blocking
/// waiters woken at a release, a last close and an exit, a fork's copy of a
/// locked description, the wake order of two exclusive waiters and of mixed
/// ones, the rows `/proc/locks` shows (a blocked request's `->` row
/// included), a lock taken before children are forked and exec'd (rustup's
/// pattern), a signal that ends a waiting call with `EINTR` or restarts it
/// under `SA_RESTART`, a waiting thread woken by a release in another
/// thread of its process, a waiting call whose descriptor a sibling closes or
/// replaces, and the Claude review's wake-order cases (a blocking upgrade that
/// blocks ahead of the waiter its conversion woke, a cancelled waiter that
/// wakes the requests blocked under it, a retry that converts its
/// description's lock, and a release through the waiting call's own
/// description before its retry), with an unrelated file's OFD lock whose
/// `/proc/locks` row stays while those waiting calls end. Then the guest's
/// `ebadf`, which `all` leaves out because replay refuses its call: a flock
/// through a number that names nothing fails with EBADF, and the unrelated
/// row stays (slice SP2's third round; the row went, on main too). Before
/// Detcore's flock model a blocking waiter was refused on every backend, and
/// KVM refused every flock; before the review, a release handed the lock to
/// waiters in its own turn, in the order they first blocked, which no Linux
/// schedule does.
fn assert_flock_model_matches_linux_at_l2(backend: &str) {
    for scenario in ["all", "ebadf"] {
        let native = native_model_run(scenario);
        let run = hermit_run_model(
            backend,
            "info",
            &["--strict", "--verify", "--verify-strict"],
            scenario,
        );
        assert!(
            run.status.success(),
            "{backend} strict --verify flock model run of {scenario} failed\n{}",
            run.combined()
        );
        assert!(
            run.combined().contains("bitwise parity established"),
            "{backend} flock model run of {scenario} did not verify at L2\n{}",
            run.combined()
        );
        assert_eq!(
            run.stdout,
            native.stdout,
            "{backend} printed other answers than native Linux for {scenario}\n{}",
            run.combined()
        );
    }
    // A waiter a signal interrupts asks whether a job-control stop is pending
    // (`flock_wait_stopped`): on KVM through the backend's signal state, never
    // a host `/proc` read of an unrelated host task, which Detcore refuses and
    // logs at ERROR (SIGSHARE S2). A run under --verify keeps its logs to
    // itself, so this one runs once, logging to its own stderr.
    let native = native_model_run("signal-interrupts");
    let run = hermit_run_model(backend, "info", &["--strict"], "signal-interrupts");
    assert!(
        run.status.success(),
        "{backend} strict flock model run of signal-interrupts failed\n{}",
        run.combined()
    );
    assert_eq!(
        run.stdout,
        native.stdout,
        "{backend} printed other answers than native Linux for signal-interrupts\n{}",
        run.combined()
    );
    assert!(
        run.combined().contains("INFO"),
        "{backend}: the signal-interrupts run logged nothing to its stderr\n{}",
        run.combined()
    );
    assert!(
        !run.combined()
            .contains("refused a host /proc signal state read"),
        "{backend} flock model run of signal-interrupts reached a refused host /proc read\n{}",
        run.combined()
    );
}

#[test]
fn flock_model_matches_linux_at_l2_on_ptrace() {
    assert_flock_model_matches_linux_at_l2("ptrace");
}

#[test]
fn flock_model_matches_linux_at_l2_on_kvm() {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("skipping: /dev/kvm is absent");
        return;
    }
    assert_flock_model_matches_linux_at_l2("kvm");
}

/// A thread with a descriptor table of its own (`CLONE_THREAD` without
/// `CLONE_FILES`), its process and a forked child close every descriptor of a
/// locked description, and a fresh open then takes the lock, natively. The
/// lock ledger cannot tell such a process's tables apart, so the file leaves
/// the model, and the kernel, which knows the lock went, decides it, where a
/// model answer resting on a reference the ledger never saw go said
/// EWOULDBLOCK (Codex review of the kvm lane's slice SP2). KVM refuses the
/// thread by name.
#[test]
fn a_private_table_closing_every_descriptor_frees_the_lock_on_ptrace() {
    let native = native_model_run("private-table-close");
    let run = hermit_run_model(
        "ptrace",
        "info",
        &["--strict", "--verify", "--verify-strict"],
        "private-table-close",
    );
    assert!(
        run.status.success(),
        "ptrace private-table-close run failed\n{}",
        run.combined()
    );
    assert_eq!(
        run.stdout,
        native.stdout,
        "ptrace printed other answers than native Linux\n{}",
        run.combined()
    );
}

/// A job-control stop takes a waiter out of the queue, and Linux queues the
/// call again only once SIGCONT continues it (the guest's `stopped-waiter`,
/// natively). Detcore does not represent job-control stops
/// (https://github.com/rrnewton/hermit/issues/4944): on ptrace the stopped
/// thread is not held, and would restart the call and retry for the lock at
/// once, so the interrupted wait is refused by name instead (Codex review of
/// the kvm lane's slice SP2). KVM refuses the stop by name.
#[test]
fn a_job_control_stop_of_a_waiter_is_refused_by_name_on_ptrace() {
    let native = native_model_run("stopped-waiter");
    assert!(
        native
            .stdout
            .contains("stopped-waiter: rows while the second waiter is stopped = WRITE ->READ"),
        "natively the stopped waiter left the queue\n{}",
        native.combined()
    );
    let run = hermit_run_model("ptrace", "error", &["--strict"], "stopped-waiter");
    assert_eq!(
        run.status.code(),
        Some(122),
        "ptrace must refuse the stopped waiter's call by name\n{}",
        run.combined()
    );
    assert!(
        run.stderr
            .contains("a job-control stop interrupted this waiting flock"),
        "ptrace refused the stopped waiter's call without its reason\n{}",
        run.combined()
    );
}

/// The refusal of a waiting call whose descriptor was closed or replaced
/// while another reference kept the description it took.
const WAITER_KEPT_BY_ANOTHER_REFERENCE: &str = "the waiting call's descriptor was closed or \
     replaced while it waited, and another reference (another process's table, a mapping, a \
     copy in flight) keeps the open file description the call took";

/// A sibling closes the descriptor a waiting call took while a mapping of its
/// open file description remains (the guest's `waiter-mapped`, natively):
/// Linux's call takes the lock at the release, and the mapping keeps it after
/// the call returns, so another description's `LOCK_EX|LOCK_NB` fails until
/// the munmap. No descriptor names the description any longer, through which
/// Detcore could place the lock on the host, so the call is refused by name;
/// under `--allow-unsupported-syscalls` it fails with ENOLCK, and the model,
/// like the host, then holds no lock for the description, so the child's
/// `LOCK_EX|LOCK_NB` succeeds. Before the Claude review of the kvm lane's
/// slice SP2's second round, the call returned 0 with no lock on the host,
/// and the kernel, which decided the file from then on, granted the child's
/// conflicting lock. KVM refuses the thread of a process with a shared file
/// mapping, so it cannot run this.
fn assert_a_waiter_kept_by_a_mapping_alone_is_refused_by_name(backend: &str) {
    let native = native_model_run("waiter-mapped");
    assert!(
        native.stdout.contains(
            "waiter-mapped: child LOCK_EX|LOCK_NB while the mapping remains = EWOULDBLOCK"
        ),
        "natively the mapping kept the waiting call's lock\n{}",
        native.combined()
    );
    let run = hermit_run_model(backend, "error", &["--strict"], "waiter-mapped");
    // The unsupported-operation policy's exit: DBT's runtime ends the run
    // with 101, as its copied-child refusals do.
    let refused = if backend == "dbt" { 101 } else { 122 };
    assert_eq!(
        run.status.code(),
        Some(refused),
        "{backend} must refuse the waiting call by name\n{}",
        run.combined()
    );
    assert!(
        run.stderr.contains(WAITER_KEPT_BY_ANOTHER_REFERENCE),
        "{backend} refused the waiting call without its reason\n{}",
        run.combined()
    );
    assert!(
        !run.stdout.contains("while the mapping remains"),
        "{backend} went on after refusing the waiting call\n{}",
        run.combined()
    );
    let run = hermit_run_model(
        backend,
        "error",
        &["--allow-unsupported-syscalls"],
        "waiter-mapped",
    );
    for marker in [
        "FAIL waiter-mapped: the waiting call after the release = errno=37, expected ok",
        "FAIL waiter-mapped: child LOCK_EX|LOCK_NB while the mapping remains = ok, expected \
         EWOULDBLOCK",
        "waiter-mapped: child LOCK_EX|LOCK_NB after the munmap = ok",
    ] {
        assert!(
            run.stdout.contains(marker),
            "{backend} --allow-unsupported-syscalls: missing {marker}\n{}",
            run.combined()
        );
    }
    assert!(
        run.stderr.contains(WAITER_KEPT_BY_ANOTHER_REFERENCE),
        "{backend} --allow-unsupported-syscalls: the ENOLCK came without its reason\n{}",
        run.combined()
    );
}

#[test]
fn a_waiter_kept_by_a_mapping_alone_is_refused_by_name_on_ptrace() {
    assert_a_waiter_kept_by_a_mapping_alone_is_refused_by_name("ptrace");
}

/// An exec kills the caller's other threads before it closes the
/// close-on-exec descriptors (`de_thread`): a thread waiting for `LOCK_EX`
/// behind its process's lock leaves the queue with it, which wakes another
/// process's request blocked under the thread's, and that process takes the
/// lock once the exec closes the holder's descriptor (the guest's
/// `exec-waiter`, natively). Before the Claude review of the kvm lane's slice
/// SP2, the killed thread's request stayed queued and took the lock at the
/// exec's close, for the rest of the run; with waiters that retry in their
/// own turns and without the exec taking the request out, the other process
/// would wait forever under it. KVM hangs at an exec while another thread of
/// the process is blocked, in a flock or a pipe read alike
/// (https://github.com/rrnewton/hermit/issues/4904), so this runs on ptrace.
#[test]
fn an_exec_ends_the_flock_wait_of_a_thread_it_kills_on_ptrace() {
    let native = native_model_run("exec-waiter");
    let run = hermit_run_model(
        "ptrace",
        "info",
        &["--strict", "--verify", "--verify-strict"],
        "exec-waiter",
    );
    assert!(
        run.status.success(),
        "ptrace exec-waiter run failed\n{}",
        run.combined()
    );
    assert!(
        run.combined().contains("bitwise parity established"),
        "ptrace exec-waiter run did not verify at L2\n{}",
        run.combined()
    );
    assert_eq!(
        run.stdout,
        native.stdout,
        "ptrace printed other answers than native Linux\n{}",
        run.combined()
    );
}

/// A mapping keeps a description, and its flock lock, after the description's
/// last descriptor closes, until the mapping goes. Where the backend's
/// descriptions are the host's, the model leaves the file to the kernel at
/// that close, which keeps the lock until `munmap`, as native Linux does.
#[test]
fn a_mapping_keeps_the_lock_after_the_last_close_on_ptrace() {
    let native = native_model_run("mmap-close");
    let run = hermit_run_model(
        "ptrace",
        "info",
        &["--strict", "--verify", "--verify-strict"],
        "mmap-close",
    );
    assert!(
        run.status.success(),
        "ptrace mmap-close run failed\n{}",
        run.combined()
    );
    assert_eq!(
        run.stdout,
        native.stdout,
        "ptrace printed other answers than native Linux\n{}",
        run.combined()
    );
}

/// On KVM no host lock can decide the file, and Detcore cannot place the
/// release a mapping defers: the last close of a locked, mapped description is
/// refused by name, whose policy ends the run.
#[test]
fn kvm_refuses_the_last_close_of_a_mapped_locked_description() {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("skipping: /dev/kvm is absent");
        return;
    }
    let run = hermit_run_model("kvm", "error", &["--strict"], "mmap-close");
    assert_eq!(
        run.status.code(),
        Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
        "KVM did not refuse the close by name\n{}",
        run.combined()
    );
    assert!(
        run.stderr.contains("close refused")
            && run
                .stderr
                .contains("Linux keeps the lock until the last mapping goes"),
        "KVM's refusal did not name the mapping\n{}",
        run.combined()
    );
    assert!(
        run.stdout.contains("mmap-close: parent LOCK_EX = ok")
            && !run.stdout.contains("with the mapping left"),
        "the KVM run did not stop at the close\n{}",
        run.combined()
    );
}

/// A descriptor opened before a fork and locked in the child (the shell's
/// `exec 9>lockfile; flock -n 9`): Detcore never saw every reference to that
/// description, so its flock model leaves the file to the kernel, as before
/// the model, where the backend's descriptions are the host's.
#[test]
fn a_description_shared_before_its_first_flock_keeps_the_kernel_path_on_ptrace() {
    let native = native_model_run("shell");
    let run = hermit_run_model(
        "ptrace",
        "info",
        &["--strict", "--verify", "--verify-strict"],
        "shell",
    );
    assert!(
        run.status.success(),
        "ptrace shell-pattern run failed\n{}",
        run.combined()
    );
    assert_eq!(
        run.stdout,
        native.stdout,
        "ptrace printed other answers than native Linux\n{}",
        run.combined()
    );
}

/// The same pattern on KVM, where no host lock can decide the file: refused by
/// name, with the reason.
#[test]
fn kvm_refuses_a_flock_through_a_description_shared_before_its_first_flock() {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("skipping: /dev/kvm is absent");
        return;
    }
    let run = hermit_run_model("kvm", "error", &["--strict"], "shell");
    assert_eq!(
        run.status.code(),
        Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
        "KVM did not refuse the flock by name\n{}",
        run.combined()
    );
    assert!(
        run.stderr.contains("flock(fd=9")
            && run
                .stderr
                .contains("was shared with another process before its first flock"),
        "KVM's refusal did not give its reason\n{}",
        run.combined()
    );
}

/// The flock model under record and replay: the recording holds the identity
/// probes and the model's mirrors, replay re-issues each mirror, and both
/// print what native Linux prints.
#[test]
fn record_and_replay_reproduce_the_flock_model() {
    let _guard = record_lock();
    let native = native_model_run("all");
    let data_dir = tempfile::tempdir().expect("failed to create the flock recording directory");
    let mut record = Command::new("timeout");
    record
        .args(["--kill-after", "10s", "300s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=error",
            "record",
            "start",
            "--record-timeout=240",
            "--proc=standard",
        ])
        .arg(format!("--data-dir={}", data_dir.path().display()))
        .arg("--")
        .arg(model_guest())
        .arg("all");
    let recorded = finish(hermit_test::hermit_output(
        &format!("{record:?}"),
        &mut record,
    ));
    assert!(
        recorded.status.success(),
        "recording the flock model guest failed\n{}",
        recorded.combined()
    );
    assert_eq!(
        recorded.stdout,
        native.stdout,
        "the recorded run printed other answers than native Linux\n{}",
        recorded.combined()
    );
    let mut replay = Command::new("timeout");
    replay
        .args(["--kill-after", "10s", "300s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=error", "replay", "--autopilot"])
        .arg(format!("--data-dir={}", data_dir.path().display()));
    let replayed = finish(hermit_test::hermit_output(
        &format!("{replay:?}"),
        &mut replay,
    ));
    assert!(
        replayed.status.success(),
        "replaying the flock model guest failed\n{}",
        replayed.combined()
    );
    assert_eq!(
        replayed.stdout,
        recorded.stdout,
        "replay printed other answers than the recording\n{}",
        replayed.combined()
    );
}

/// The shell's own pattern with util-linux's flock(1): `exec 9>lockfile;
/// flock -n 9` locks the description the shell opened, through a child that
/// inherited it, and the shell's descriptor keeps the lock after that child
/// exits; another open of the file is then refused with `LOCK_NB` until the
/// shell closes 9.
const FLOCK_UTILITY_SCRIPT: &str = r#"set -e
lock="$1/lockfile"
exec 9>"$lock"
flock -n 9 && echo "flock -n 9: locked"
flock -n "$lock" -c true || echo "another open: would block"
exec 9>&-
flock -n "$lock" -c 'echo "another open after the close: locked"'
"#;

const FLOCK_UTILITY_OUTPUT: &str = "flock -n 9: locked\nanother open: would block\n\
                                    another open after the close: locked\n";

/// Run [`FLOCK_UTILITY_SCRIPT`] under `hermit run` on `backend`, with its lock
/// file in a directory of its own.
fn hermit_run_flock_utility(backend: &str, log: &str, extra: &[&str]) -> Run {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the flock(1) directory");
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after", "10s", "120s"])
        .arg(hermit_test::hermit_binary())
        .arg(format!("--log={log}"))
        .args([&format!("--backend={backend}"), "run", "--base-env=minimal"])
        .args(extra)
        .args(["--", "/bin/sh", "-c", FLOCK_UTILITY_SCRIPT, "sh"])
        .arg(directory.path());
    hermit_test::configure_guest_execution(&mut command);
    finish(hermit_test::hermit_output(
        &format!("{command:?}"),
        &mut command,
    ))
}

/// Natively, and on a backend whose descriptions are the host's, the shell's
/// pattern behaves as Linux's: the description the shell opened before the
/// fork is not one Detcore saw every reference to, so the kernel decides the
/// file, as before Detcore's flock model.
#[test]
fn the_flock_utility_shell_pattern_keeps_the_kernel_path_on_ptrace() {
    let run = hermit_run_flock_utility(
        "ptrace",
        "info",
        &["--strict", "--verify", "--verify-strict"],
    );
    assert!(
        run.status.success(),
        "ptrace flock(1) run failed\n{}",
        run.combined()
    );
    assert_eq!(
        run.stdout,
        FLOCK_UTILITY_OUTPUT,
        "ptrace flock(1) output\n{}",
        run.combined()
    );
    assert!(
        run.combined().contains("bitwise parity established"),
        "the flock(1) run did not verify at L2\n{}",
        run.combined()
    );
}

/// On KVM no host lock can decide the file, so the same pattern is refused by
/// name at its first flock, with the reason.
#[test]
fn kvm_refuses_the_flock_utility_shell_pattern_by_name() {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("skipping: /dev/kvm is absent");
        return;
    }
    let run = hermit_run_flock_utility("kvm", "error", &["--strict"]);
    assert_eq!(
        run.status.code(),
        Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
        "KVM did not refuse the shell pattern by name\n{}",
        run.combined()
    );
    assert!(
        run.stderr.contains("flock(fd=9")
            && run
                .stderr
                .contains("was shared with another process before its first flock"),
        "KVM's refusal did not give its reason\n{}",
        run.combined()
    );
    assert!(
        !run.stdout.contains("locked"),
        "the KVM guest went on past the refused lock\n{}",
        run.combined()
    );
}
