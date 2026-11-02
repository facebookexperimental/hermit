/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The forced-gate harness of A5 step 3c.1 (design D60,
//! <https://github.com/rrnewton/hermit/issues/4485>): end-to-end rows that run
//! a guest on in-guest-trap with the in-guest delivery gate on, before the
//! flip (3c.1f) turns it on for real.
//!
//! Each row re-executes this test binary as PID 1 of its own user and PID
//! namespaces (`unshare --user --map-root-user --pid --fork --mount-proc`), so
//! that the launcher really owns the guests' PID namespace
//! (`scheduler_owns_guest_pid_namespace`) and nothing is forged. Only that
//! child sets the force flag ([`super::FORCE_IN_GUEST_DELIVERY`]), after it
//! checks the marker and that it is PID 1, and it prints `FORCED-GATE-TAKEN`
//! before it launches, which the parent requires. The flag alone does not make
//! a run gated: every other conjunct of
//! [`super::in_guest_signal_delivery_gate`] must hold too. So each row also
//! proves that its own run was gated: the set point that computed the gate
//! on records it ([`super::GATED_LAUNCHES`], and the line
//! [`super::GATED_LAUNCH_WITNESS`] on stdout), a pass row requires exactly one
//! more gated launch after its run, and the parent requires the line from
//! every row, the refusal row included. A host where the namespaces cannot be
//! created, the in-guest runtime is missing, or no C compiler is found fails
//! the row: a skip is a failure. A row that must end in a stopping refusal is
//! asserted as one: the policy-refusal exit status, the named reason on
//! stderr, the gate witnesses on stdout, and never the line the guest prints
//! after the point that must not be reached.
//!
//! A pass row compiles `guest.c`, runs its mode natively and under Hermit in
//! the child, and requires the same output, plus the lines that make the row
//! discriminate. Each row names the mutation it catches; the mutation "the
//! gate computed off" (one more conjunct of the gate that is false) fails every
//! forced_gate_* row, those whose output Linux also prints included.

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use super::Backend;
use super::DetConfig;

const CHILD_MARKER: &str = "HERMIT_TEST_IN_GUEST_DELIVERY_CHILD";
const GATE_WITNESS: &str = "FORCED-GATE-TAKEN";
const GUEST_SOURCE: &str = include_str!("in_guest_delivery_harness/guest.c");

/// What the parent requires of a row's isolated child.
enum Expect {
    /// The row passed in the child.
    Pass,
    /// The child ended in the stopping refusal named by `reason`.
    Refusal { reason: &'static str },
}

/// Runs the calling row in an isolated child and checks it (`expect`), or, in
/// that child, forces the gate and returns true: the caller then runs the row.
fn isolated(name: &str, expect: Expect) -> bool {
    let test = format!("{}::{name}", module_path!().split_once("::").unwrap().1);
    if std::env::var(CHILD_MARKER).as_deref() == Ok(test.as_str()) {
        // SAFETY: getpid has no preconditions.
        assert_eq!(
            unsafe { libc::getpid() },
            1,
            "{test}: the child is not PID 1"
        );
        // unshare(1) may leave SIGINT and SIGTERM ignored in its child, and an
        // ignored action survives exec into the guests: start them as a shell
        // would, with every action the default.
        for signal in 1..=64 {
            if signal == libc::SIGKILL || signal == libc::SIGSTOP {
                continue;
            }
            // SAFETY: queries and, for an ignored signal, resets this
            // process's own action; no handler is installed.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(signal, std::ptr::null(), &mut action) == 0
                    && action.sa_sigaction == libc::SIG_IGN
                {
                    action.sa_sigaction = libc::SIG_DFL;
                    assert_eq!(libc::sigaction(signal, &action, std::ptr::null_mut()), 0);
                }
            }
        }
        super::FORCE_IN_GUEST_DELIVERY.store(true, std::sync::atomic::Ordering::Release);
        assert!(super::runtime_delivers_signals());
        println!("{GATE_WITNESS} {name}");
        return true;
    }
    let output = Command::new("unshare")
        .args([
            "--user",
            "--map-root-user",
            "--pid",
            "--fork",
            "--mount-proc",
        ])
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", &test, "--nocapture", "--test-threads=1"])
        .env(CHILD_MARKER, &test)
        .output()
        .unwrap_or_else(|error| panic!("{test}: unshare could not run: {error}"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let context = format!(
        "{test}: {:?}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        output.status
    );
    assert!(
        stdout.contains(&format!("{GATE_WITNESS} {name}")),
        "the forced gate was not taken (a skip is a failure): {context}"
    );
    assert!(
        stdout
            .lines()
            .any(|line| line == super::GATED_LAUNCH_WITNESS),
        "the row's run did not compute the in-guest delivery gate on: {context}"
    );
    match expect {
        Expect::Pass => {
            assert!(output.status.success(), "{context}");
            assert!(stdout.contains("1 passed"), "{context}");
        }
        Expect::Refusal { reason } => {
            assert_eq!(
                output.status.code(),
                Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
                "{context}"
            );
            assert!(stderr.contains(reason), "missing {reason}: {context}");
            assert!(stdout.contains("ROW-STARTED"), "{context}");
            assert!(!stdout.contains("ROW-REACHED-AFTER-REFUSAL"), "{context}");
        }
    }
    false
}

/// `guest.c`, compiled into `directory`.
fn compile_guest(directory: &Path) -> PathBuf {
    let source = directory.join("guest.c");
    std::fs::write(&source, GUEST_SOURCE).unwrap();
    let binary = directory.join("guest");
    let output = Command::new("cc")
        .args(["-O1", "-o"])
        .arg(&binary)
        .arg(&source)
        .output()
        .unwrap_or_else(|error| panic!("cc could not run (a skip is a failure): {error}"));
    assert!(output.status.success(), "cc failed: {output:?}");
    binary
}

/// The configuration of a gated in-guest run: sequentialized threads (a D35
/// conjunct) and no maximum timeslice (in-guest has no preemption timer; a
/// timeslice would turn the gate off, D33). The launcher fills in the rest of
/// D35's conjuncts (the in-guest capability table, site patching off), and
/// the wait4 row's SigBlk witnesses that the gate was on.
fn config() -> DetConfig {
    DetConfig {
        max_timeslice: None,
        sequentialize_threads: true,
        ..DetConfig::default()
    }
}

fn require_runtime() {
    if let Err(error) = Backend::InGuestTrap.ensure_available() {
        panic!("the in-guest runtime is unavailable (a skip is a failure): {error}");
    }
}

/// Runs `mode` natively and under the forced gate, requires the same stdout
/// and a zero exit, and that the output holds every line of `required`.
fn same_as_native(mode: &str, required: &[&str]) {
    require_runtime();
    let directory = tempfile::tempdir().unwrap();
    let guest = compile_guest(directory.path());
    let native = Command::new(&guest).arg(mode).output().unwrap();
    assert!(native.status.success(), "native {mode}: {native:?}");
    let mut command = super::Command::new(&guest);
    command.arg(mode);
    let launches = super::GATED_LAUNCHES.load(std::sync::atomic::Ordering::Acquire);
    let gated =
        super::run_with_output_backend(command, config(), false, &None, Backend::InGuestTrap)
            .unwrap_or_else(|error| panic!("{mode} under the forced gate: {error:?}"));
    assert_eq!(
        super::GATED_LAUNCHES.load(std::sync::atomic::Ordering::Acquire),
        launches + 1,
        "{mode}: the run did not compute the in-guest delivery gate on"
    );
    let stdout = String::from_utf8_lossy(&gated.stdout).into_owned();
    println!("{mode} under the forced gate:\n{stdout}");
    assert_eq!(
        gated.status,
        super::ExitStatus::Exited(0),
        "{mode}: stderr {}",
        String::from_utf8_lossy(&gated.stderr)
    );
    assert_eq!(
        stdout,
        String::from_utf8_lossy(&native.stdout),
        "{mode}: native vs gated"
    );
    for line in required {
        assert!(
            stdout.lines().any(|seen| seen == *line),
            "{mode}: missing {line:?}"
        );
    }
}

/// The parent never forces the gate: its own runs keep today's delivery.
#[test]
fn the_parent_runner_never_forces_the_gate() {
    assert!(std::env::var(CHILD_MARKER).is_err());
    assert!(!super::FORCE_IN_GUEST_DELIVERY.load(std::sync::atomic::Ordering::Acquire));
    assert!(!super::runtime_delivers_signals());
}

/// A gated managed wait4 reads the guest's mask in place: a child reading its
/// waiting parent's SigBlk sees the parent's own mask (0x200), not every
/// signal blocked. Mutation: "Detcore's block left in place under the gate"
/// (the child reads 0xfffffffe3ffb7aff).
#[test]
fn forced_gate_a_managed_wait4_publishes_the_guests_own_mask() {
    if isolated(
        "forced_gate_a_managed_wait4_publishes_the_guests_own_mask",
        Expect::Pass,
    ) {
        same_as_native(
            "wait4",
            &[
                "child-reads-parent-sigblk=0x200",
                "wait4=child status=7 mask=0x200",
            ],
        );
    }
}

/// 3c.1d's D37 removal still runs under the gate, through the Tool's injected
/// zero-timeout rt_sigtimedwait: an ignored SIGURG queued while blocked is
/// gone after a masked zero-timeout ppoll whose mask lets it in, and a masked
/// epoll_pwait with a timeout returns EINTR for it (IR-3). Mutations: "the
/// runtime refuses every rt_sigtimedwait" (the run is refused); "the gate
/// computed off" (Linux prints the same lines, but no gated launch is
/// recorded).
#[test]
fn forced_gate_d37_removal_runs_through_the_tools_injection() {
    if isolated(
        "forced_gate_d37_removal_runs_through_the_tools_injection",
        Expect::Pass,
    ) {
        same_as_native(
            "d37",
            &[
                "urg-before=0x400000",
                "ppoll-masked-zero=0 errno=0 urg-after=0",
                "epoll_pwait-masked=-1 errno=4 urg-after=0",
            ],
        );
    }
}

/// A masked ppoll parks while another process sends an ignored SIGURG that
/// its temporary mask lets in and the guest's own mask blocks; Linux discarded
/// it at send, so nothing is pending after. In the first wait, which times
/// out, A's removal at the wait's entry and park takes it; in the second,
/// which the other process's write ends right after its send, the removal of
/// the window's arrivals as the wait ends (D37's R2) does. Mutations: "the
/// entry and park removal skipped" (the first wait's SIGURG stays pending);
/// "R2 skipped at the wait's end" (the second's does); "the runtime refuses
/// every rt_sigtimedwait" (the run is refused); "the gate computed off" (no
/// gated launch is recorded).
#[test]
fn forced_gate_a_masked_waits_arrivals_are_removed() {
    if isolated(
        "forced_gate_a_masked_waits_arrivals_are_removed",
        Expect::Pass,
    ) {
        same_as_native(
            "r2",
            &[
                "ppoll-parked=0 errno=0 child=0 pending-after=0",
                "ppoll-ready=1 errno=0 readable=1 child=0 pending-after=0",
            ],
        );
    }
}

/// The guest's own sigtimedwait with a timeout, which Detcore polls with
/// zero-timeout injections, still runs: woken by another process's send, and
/// with its signal already pending. Mutations: "the runtime refuses every
/// rt_sigtimedwait" (the run is refused); "the gate computed off" (no gated
/// launch is recorded).
#[test]
fn forced_gate_the_polled_guest_sigtimedwait_runs() {
    if isolated(
        "forced_gate_the_polled_guest_sigtimedwait_runs",
        Expect::Pass,
    ) {
        same_as_native(
            "sigtimedwait",
            &["sigtimedwait-woken=10 sigtimedwait-pending=12 pending-after=0"],
        );
    }
}

/// rt_sigprocmask and rt_sigpending act on the guest's saved mask in Linux's
/// order (EINVAL for an invalid how with a set or a bad size, EFAULT for an
/// unreadable set or an unwritable old set with the change kept, 0 for an
/// invalid how with a null set), and the calls the narrowed refusal must
/// leave alone still run: a zero-timeout rt_sigtimedwait, a signalfd read and
/// a masked ppoll whose timeout Linux rejects. Mutations: "the calls act on
/// the kernel mask" (the mask lines show the runtime mask); "the runtime
/// refuses every rt_sigtimedwait" (the run is refused); "the gate computed
/// off" (no gated launch is recorded).
#[test]
fn forced_gate_mask_calls_act_on_the_saved_mask_in_linuxs_order() {
    if isolated(
        "forced_gate_mask_calls_act_on_the_saved_mask_in_linuxs_order",
        Expect::Pass,
    ) {
        same_as_native(
            "maskcalls",
            &[
                "invalid-how-with-set result=-22 mask=0x4200",
                "invalid-how-null-set result=0 mask=0x4200",
                "invalid-how-unreadable-set result=-14 mask=0x4200",
                "size-4 result=-22 mask=0x4200",
                "unwritable-old result=-14 mask=0x4201",
                "pending-8 result=0 set=0x200",
                "pending-0-unwritable result=0",
                "pending-9 result=-22",
                "pending-unwritable result=-14",
                "sigtimedwait-zero taken=10 again=-11",
                "signalfd read=128 signo=12",
                "ppoll-invalid-timeout=-1 errno=22",
            ],
        );
    }
}

/// A refusal row: a vfork child sends its parent SIGTERM while the parent
/// waits in vfork, a kernel-run call through which the runtime mask would
/// hold the signal; the run ends in the named stopping refusal (D44, D49)
/// before the parent returns. Mutation: "the D44 refusal skipped" (the guest
/// prints ROW-REACHED-AFTER-REFUSAL and the run exits 0).
#[test]
fn forced_gate_a_send_to_a_vfork_parent_is_refused_by_name() {
    if isolated(
        "forced_gate_a_send_to_a_vfork_parent_is_refused_by_name",
        Expect::Refusal {
            reason: "HERMIT_KERNEL_RUN_SIGNAL_REFUSED",
        },
    ) {
        require_runtime();
        let directory = tempfile::tempdir().unwrap();
        let guest = compile_guest(directory.path());
        let mut command = super::Command::new(&guest);
        command.arg("vfork-refusal");
        // The guest's stdout is this child's, so the parent sees how far the
        // guest got; the refusal ends this process.
        let status = super::run_with_backend(command, config(), false, &None, Backend::InGuestTrap);
        panic!("the run was not refused: {status:?}");
    }
}
