/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Regression tests for https://github.com/rrnewton/hermit/issues/4121: under
//! `--strict` at the default 200 ms maximum slice, a thread spinning on a cheap
//! syscall kept a sleeping thread waiting for its whole slice. A deadline is
//! acted on only at a scheduler turn, and the spinner took none.
//!
//! Every reply that resumes a thread from a scheduler grant now carries the
//! time left before the earliest pending deadline, and the thread ends its
//! slice there. On main ab5a57a371 each wait below took about 201 ms of
//! virtual time; with the cap it takes about 5.5 ms.
//!
//! The cap ends a slice at the thread's next check-in (a syscall, signal or
//! timer event). It does not lower the PMU maximum: a thread that makes no
//! syscalls still runs to it, as before. The last test pins that scope.

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::OnceLock;

/// A healthy run finishes in a few seconds of host time. Without the cap,
/// the getppid cases take about 40 s.
const TIMEOUT_SECONDS: u64 = 120;

/// The longest wait, in virtual microseconds, accepted for a 5 ms timed wait
/// whose deadline falls inside another thread's slice. Main took about
/// 201,000; the cap gives about 5,500.
const MAX_WAIT_US: i64 = 20_000;

/// The shortest wait that shows a slice ran to its maximum (200 ms).
const UNCAPPED_WAIT_US: i64 = 150_000;

/// The least virtual time the storm fixture's 20M-iteration loop may take
/// under a slow timer; it takes about 200 ms
/// (https://github.com/rrnewton/hermit/issues/4139).
const MIN_STORM_WORK_US: i64 = 50_000;

static SPIN_WAIT: OnceLock<PathBuf> = OnceLock::new();
static FUTEX_RESUME: OnceLock<PathBuf> = OnceLock::new();
static SELF_ALARM: OnceLock<PathBuf> = OnceLock::new();
static START_CAP: OnceLock<PathBuf> = OnceLock::new();
static PARENT_CONTINUE: OnceLock<PathBuf> = OnceLock::new();
static SLEEPY_SIBLING: OnceLock<PathBuf> = OnceLock::new();
static ITIMER_LOOP: OnceLock<PathBuf> = OnceLock::new();
static ITIMER_STORM: OnceLock<PathBuf> = OnceLock::new();

/// Compile `hermit-cli/tests/fixtures/<name>.c` once.
fn fixture(cell: &'static OnceLock<PathBuf>, name: &str) -> &'static Path {
    cell.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("deadline-slice-cap");
        fs::create_dir_all(&build_root).expect("failed to create build directory");
        let source = repository.join(format!("hermit-cli/tests/fixtures/{name}.c"));
        let output = build_root.join(name);
        let mut command = Command::new("cc");
        command
            .args(["-std=c11", "-O2", "-g", "-pthread", "-D_GNU_SOURCE"])
            .args(["-Wall", "-Wextra", "-Werror"])
            .arg(&source)
            .arg("-o")
            .arg(&output);
        let status = command
            .status()
            .unwrap_or_else(|error| panic!("failed to run cc for {}: {error}", source.display()));
        assert!(status.success(), "guest compilation failed: {command:?}");
        output
    })
}

/// Run `guest` under `hermit run --strict` with `hermit_args`, require a clean
/// exit within the timeout, and return its stdout and stderr.
fn run_strict(guest: &Path, guest_args: &[&str], hermit_args: &[&str]) -> (String, String) {
    run_strict_with(guest, guest_args, hermit_args, &[])
}

/// [`run_strict`] with `run_args` after `run` as well.
fn run_strict_with(
    guest: &Path,
    guest_args: &[&str],
    hermit_args: &[&str],
    run_args: &[&str],
) -> (String, String) {
    let (output, rendered) = strict_output(guest, guest_args, hermit_args, run_args);
    checked(output, &rendered)
}

/// [`run_strict_with`] for a 20 ms maximum slice, which takes about ten
/// precise PMU stops per run. Every run here retries a typed skid-overshoot
/// refusal ([`hermit_test::hermit_output`]).
fn run_strict_retrying_skid(guest: &Path, guest_args: &[&str], run_args: &[&str]) -> String {
    run_strict_with(guest, guest_args, &[], run_args).0
}

/// Require a clean exit and return stdout and stderr.
fn checked(output: Output, rendered: &str) -> (String, String) {
    let stdout = String::from_utf8(output.stdout).expect("guest stdout should be UTF-8");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "guest failed: {rendered}\nstatus: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status,
    );
    (stdout, stderr)
}

/// Run `guest` once under `hermit run --strict` and return its output.
fn strict_output(
    guest: &Path,
    guest_args: &[&str],
    hermit_args: &[&str],
    run_args: &[&str],
) -> (Output, String) {
    let mut command = Command::new("timeout");
    command
        .arg("--kill-after=2s")
        .arg(format!("{TIMEOUT_SECONDS}s"))
        .arg(hermit_test::hermit_binary())
        .args(hermit_args)
        .args([
            "run",
            "--base-env=minimal",
            "--strict",
            "--epoch=2026-01-01T00:00:00Z",
        ])
        .args(run_args)
        .arg("--")
        .arg(guest)
        .args(guest_args);
    hermit_test::configure_guest_execution(&mut command);
    let rendered = format!("{command:?}");
    let output = hermit_test::hermit_output(&rendered, &mut command);
    (output, rendered)
}

/// The integer after `key=` in `stdout`.
fn field(stdout: &str, key: &str) -> i64 {
    let start = stdout
        .find(&format!("{key}="))
        .unwrap_or_else(|| panic!("no {key}= in stdout:\n{stdout}"))
        + key.len()
        + 1;
    let digits: String = stdout[start..]
        .chars()
        .take_while(|c| *c == '-' || c.is_ascii_digit())
        .collect();
    digits
        .parse()
        .unwrap_or_else(|error| panic!("bad {key} in stdout ({error}):\n{stdout}"))
}

/// A 5 ms timed wait whose deadline falls inside a getppid spinner's slice
/// ends near its deadline, for each kind of timed wait.
#[test]
fn a_timed_wait_ends_a_syscall_spinners_slice_at_its_deadline() {
    let guest = fixture(&SPIN_WAIT, "deadline_spin_wait");
    for wait in ["nanosleep", "condwait", "poll", "pollpipe"] {
        let (stdout, _) = run_strict(guest, &[wait, "getppid", "late"], &[]);
        assert!(
            stdout.contains("spun=1"),
            "{wait}: sibling never spun:\n{stdout}"
        );
        let waited = field(&stdout, "waited_us");
        assert!(
            (5_000..MAX_WAIT_US).contains(&waited),
            "{wait}: a 5 ms wait took {waited} us:\n{stdout}"
        );
    }
}

/// A thread resumed by a futex wake carries the cap of the grant that woke
/// it: A waits on a futex with no timeout, B sleeps 5 ms, C wakes A, and A
/// spins until B is back (Codex design review of the 4121 prototype, P2-1).
#[test]
fn a_futex_wake_carries_the_deadline_cap() {
    let guest = fixture(&FUTEX_RESUME, "deadline_futex_resume");
    let (stdout, _) = run_strict(guest, &["getppid", "sleep"], &[]);
    let waited = field(&stdout, "b_waited_us");
    assert!(
        (5_000..MAX_WAIT_US).contains(&waited),
        "B's 5 ms sleep took {waited} us:\n{stdout}"
    );
    // The control: no deadline anywhere, nothing to cap.
    let (stdout, _) = run_strict(guest, &["getppid", "nosleep"], &[]);
    assert_eq!(field(&stdout, "b_waited_us"), -1, "{stdout}");
}

/// A thread that arms its own timer ends its slice when the timer is due,
/// at its next syscall.
#[test]
fn a_self_armed_timer_ends_a_syscall_spinners_slice() {
    let guest = fixture(&SELF_ALARM, "deadline_self_alarm");
    let (stdout, _) = run_strict(guest, &["getppid"], &[]);
    let waited = field(&stdout, "waited_us");
    assert!(
        (5_000..MAX_WAIT_US).contains(&waited),
        "a 5 ms alarm took {waited} us:\n{stdout}"
    );
}

/// A new thread's first slice is capped: its start grant's hint arrives
/// before the slice exists, and thread start applies it once the slice does.
#[test]
fn a_new_threads_first_slice_is_capped() {
    let guest = fixture(&START_CAP, "deadline_start_cap");
    let (stdout, _) = run_strict(guest, &[], &[]);
    let waited = field(&stdout, "w_waited_us");
    assert!(
        (5_000..MAX_WAIT_US).contains(&waited),
        "W's 5 ms sleep took {waited} us:\n{stdout}"
    );
}

/// The parent's continuation after clone is a grant: W runs first and starts
/// a 5 ms sleep, and the parent, resumed from clone while that deadline is
/// pending, spins until W is back.
#[test]
fn a_parents_continuation_after_clone_is_capped() {
    let guest = fixture(&PARENT_CONTINUE, "deadline_parent_continue");
    let (stdout, _) = run_strict(guest, &[], &[]);
    let waited = field(&stdout, "w_waited_us");
    assert!(
        (5_000..MAX_WAIT_US).contains(&waited),
        "W's 5 ms sleep took {waited} us:\n{stdout}"
    );
}

/// A capped slice runs at least one turn charge (500 us), so the turns the
/// cap adds cost no more virtual time than the work: A makes 100,000 getppid
/// calls (27 virtual ms alone) while B sleeps 501 us in a loop. With an
/// 11 ns floor, A took about 366 times as long; with the turn-charge floor,
/// about 3 times, B's own turns included, and B makes progress.
#[test]
fn the_caps_turns_cost_no_more_than_the_work() {
    let guest = fixture(&SLEEPY_SIBLING, "deadline_sleepy_sibling");
    for sleep_ns in ["501000", "600000"] {
        let (stdout, _) = run_strict(guest, &[sleep_ns, "100"], &[]);
        let a_ms = field(&stdout, "a_virtual_ms");
        let b_iters = field(&stdout, "b_iters");
        assert!(
            a_ms <= 100,
            "{sleep_ns} ns: A's 27 ms of work took {a_ms} virtual ms:\n{stdout}"
        );
        assert!(
            b_iters >= 20,
            "{sleep_ns} ns: B made {b_iters} iterations:\n{stdout}"
        );
    }
}

/// A periodic timer whose period is at most two turn charges (1 ms) ends no
/// slice (`fast_timer_period`): its expiries could cost more turns than the
/// period (https://github.com/rrnewton/hermit/issues/4139). The thread's 50,000
/// getppid calls (13.5 virtual ms) then run as before the cap. A slower timer
/// does end slices, at a bounded cost.
#[test]
fn a_fast_periodic_timer_ends_no_slice() {
    let guest = fixture(&ITIMER_LOOP, "deadline_itimer_loop");
    for period_us in ["400", "1000"] {
        let (stdout, _) = run_strict(guest, &[period_us, "50"], &[]);
        // These pin the cap's exclusion of a periodic timer of at most two
        // turn charges (`fast_timer_period`), which keeps such a timer from
        // ending a syscall spinner's slice. Lifting that exclusion must
        // change them; the no-charge delivery of
        // https://github.com/rrnewton/hermit/issues/4139 does not.
        assert_eq!(
            field(&stdout, "signals"),
            0,
            "{period_us} us (lifting the cap's fast_timer_period exclusion must update this):\n{stdout}"
        );
        let virtual_ms = field(&stdout, "virtual_ms");
        assert!(
            virtual_ms < 20,
            "{period_us} us: {virtual_ms} ms (lifting the cap's fast_timer_period exclusion must update this):\n{stdout}"
        );
    }
    let (stdout, _) = run_strict(guest, &["2000", "50"], &[]);
    let signals = field(&stdout, "signals");
    let virtual_ms = field(&stdout, "virtual_ms");
    assert!(
        signals >= 5,
        "2 ms timer delivered {signals} signals:\n{stdout}"
    );
    assert!(virtual_ms < 50, "2 ms timer: {virtual_ms} ms:\n{stdout}");
}

/// A periodic `ITIMER_REAL` of at most one scheduler turn charge (500 us) no
/// longer stalls a CPU-bound thread that makes no syscalls
/// (https://github.com/rrnewton/hermit/issues/4139). Each delivery to the
/// running thread used to be charged a turn, so the next expiry was due
/// before the guest ran again: on main 0923c6a175 the 100 us and 400 us
/// periods never finished, and 520 us took 5,173 virtual ms for 200 ms of
/// work. Now each period takes about as long as the same loop under a 50 ms
/// timer.
///
/// This test no longer checks recurrence. Missed periods now coalesce as on
/// Linux, so a loop that makes no syscalls receives one delivery per slice end
/// it crosses, and at the default 200 ms maximum slice the 200 ms loop crosses
/// about one. That premise is shown by the base revision: its count check
/// passed only through the burst of queued expiries at a slice end inside the
/// pure-work loop, at every one of these periods. So `signals >= 1` holds for a
/// correct implementation, and it is also what a timer that never re-arms
/// gives. `itimer_expiries_recur_once_per_slice_end` is the recurrence check.
/// Delivering about once per period inside syscall-free code, as Linux does,
/// needs a precise stop per period and stays open as
/// https://github.com/rrnewton/hermit/issues/4261.
#[test]
fn a_fast_periodic_timer_does_not_stall_a_cpu_bound_thread() {
    let guest = fixture(&ITIMER_STORM, "deadline_itimer_storm");
    let (stdout, _) = run_strict(guest, &["50000", "20"], &[]);
    let work_us = field(&stdout, "virtual_us");
    // The loop's 20M iterations take about 200 virtual ms; a far smaller
    // reference would make the bounds below meaningless.
    assert!(
        work_us >= MIN_STORM_WORK_US,
        "the loop under a 50 ms timer took {work_us} virtual us:\n{stdout}"
    );
    for period_us in [100, 400, 520, 1000] {
        let (stdout, _) = run_strict(guest, &[&period_us.to_string(), "20"], &[]);
        let virtual_us = field(&stdout, "virtual_us");
        let signals = field(&stdout, "signals");
        // Virtual time keeps counting the loop's work through the deliveries,
        // and the deliveries add little to it.
        assert!(
            virtual_us >= work_us * 9 / 10 && virtual_us <= work_us * 5 / 4,
            "{period_us} us: {virtual_us} virtual us against {work_us} under a 50 ms timer:\n{stdout}"
        );
        let periods = virtual_us / period_us;
        assert!(
            periods >= 100 && signals >= 1 && signals <= periods + 1,
            "{period_us} us: {signals} signals over {periods} periods:\n{stdout}"
        );
    }
}

/// The capped schedules are deterministic: canonical INFO verification
/// (`--verify --verify-strict`) passes with no relaxation.
#[test]
fn capped_schedules_verify_bitwise() {
    let cases: [(&Path, &[&str]); 2] = [
        (
            fixture(&SPIN_WAIT, "deadline_spin_wait"),
            &["nanosleep", "getppid", "late"],
        ),
        (
            fixture(&FUTEX_RESUME, "deadline_futex_resume"),
            &["getppid", "sleep"],
        ),
    ];
    for (guest, args) in cases {
        let mut command = Command::new("timeout");
        command
            .arg("--kill-after=2s")
            .arg(format!("{TIMEOUT_SECONDS}s"))
            .arg(hermit_test::hermit_binary())
            .args([
                "--log=info",
                "--backend=ptrace",
                "run",
                "--base-env=minimal",
                "--strict",
                "--verify",
                "--verify-strict",
                "--epoch=2026-01-01T00:00:00Z",
                "--seed=1",
            ])
            .arg("--")
            .arg(guest)
            .args(args);
        hermit_test::configure_guest_execution(&mut command);
        let rendered = format!("{command:?}");
        let output = hermit_test::hermit_output(&rendered, &mut command);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "verify failed: {rendered}\nstatus: {}\nstderr:\n{stderr}",
            output.status
        );
        assert!(
            stderr.contains("comparison=BitwiseInfoV1 relaxations=none"),
            "verify did not use the canonical comparator: {rendered}\nstderr:\n{stderr}"
        );
    }
}

/// Scope: the cap ends a slice only at a check-in. A spinner that makes no
/// syscalls runs to its PMU maximum, as it did before the cap, because a
/// precise PMU stop at every deadline costs about 70 ms of host time each.
/// If that changes, this test must change with it.
#[test]
fn a_spinner_without_syscalls_keeps_its_maximum_slice() {
    let guest = fixture(&SPIN_WAIT, "deadline_spin_wait");
    let (stdout, _) = run_strict(guest, &["nanosleep", "pure", "late"], &[]);
    let waited = field(&stdout, "waited_us");
    assert!(
        waited >= UNCAPPED_WAIT_US,
        "a no-syscall spinner's slice ended early: the wait took {waited} us:\n{stdout}"
    );
}

static ITIMER_COALESCE: OnceLock<PathBuf> = OnceLock::new();

/// A 20 ms maximum slice: the 200 ms loop crosses about ten slice ends.
const TWENTY_MS_SLICE: &str = "--max-timeslice=20000000";

/// The integer after `key=`, or the word after it as a string.
fn word(stdout: &str, key: &str) -> String {
    let start = stdout
        .find(&format!("{key}="))
        .unwrap_or_else(|| panic!("no {key}= in stdout:\n{stdout}"))
        + key.len()
        + 1;
    stdout[start..]
        .chars()
        .take_while(|c| !c.is_whitespace())
        .collect()
}

/// Run the fixture natively, to check that an oracle holds on Linux. Every
/// wait inside it is bounded; a bound that ran out under host load prints
/// INCONCLUSIVE, which fails the test by name and never counts as a pass.
fn run_native(args: &[&str]) -> String {
    let guest = fixture(&ITIMER_COALESCE, "itimer_coalesce");
    let output = Command::new("timeout")
        .arg("--kill-after=2s")
        .arg(format!("{TIMEOUT_SECONDS}s"))
        .arg(guest)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("failed to run {args:?} natively: {error}"));
    let stdout = String::from_utf8(output.stdout).expect("native stdout should be UTF-8");
    assert!(
        output.status.success(),
        "native {args:?} failed: {}\n{stdout}",
        output.status
    );
    assert!(
        !stdout.contains("INCONCLUSIVE"),
        "INCONCLUSIVE: native {args:?} ran out of a bounded wait under host load; no oracle was checked:\n{stdout}"
    );
    stdout
}

/// Recurrence across slice ends (https://github.com/rrnewton/hermit/issues/4139):
/// a syscall-free loop under a fast periodic timer and a 20 ms maximum slice
/// gets exactly one delivery per slice end, spread through the loop. Before,
/// every missed period fired at each slice end, about 200 deliveries at one
/// loop index; a timer that never re-arms gives one.
#[test]
fn itimer_expiries_recur_once_per_slice_end() {
    let guest = fixture(&ITIMER_COALESCE, "itimer_coalesce");
    for period in ["100", "400", "520", "1000"] {
        let stdout =
            run_strict_retrying_skid(guest, &["storm", period, "20", "1"], &[TWENTY_MS_SLICE]);
        // The bounds follow from the loop's own virtual time W, which depends
        // on the compiler's code for the loop: W / 20 ms slice ends, each one
        // slice of iterations apart.
        let work_us = field(&stdout, "work_us");
        assert!(
            work_us >= MIN_STORM_WORK_US,
            "{period} us: work {work_us} us:\n{stdout}"
        );
        let slices = work_us / 20_000;
        let per_slice = 20_000_000 * 20_000 / work_us;
        let signals = field(&stdout, "t0_signals");
        assert!(
            (slices - 1..=slices + 1).contains(&signals),
            "{period} us: {signals} deliveries for {slices} slice ends:\n{stdout}"
        );
        assert!(
            field(&stdout, "t0_first") <= per_slice * 5 / 4,
            "{period} us: late first:\n{stdout}"
        );
        assert!(
            field(&stdout, "t0_last") >= 20_000_000 - per_slice * 5 / 4,
            "{period} us: early last:\n{stdout}"
        );
        let (min, max) = (field(&stdout, "t0_min_gap"), field(&stdout, "t0_max_gap"));
        let gap = per_slice * 3 / 4..=per_slice * 5 / 4;
        assert!(
            gap.contains(&min) && gap.contains(&max),
            "{period} us: gaps {min}..{max} iterations, not one slice ({per_slice}) apart:\n{stdout}"
        );
        assert_eq!(field(&stdout, "overflow"), 0, "{stdout}");
        // Each delivery's clock is at or past the first grid point after the
        // previous delivery's clock: the timer restarted at the dequeue, on
        // the grid that began at arming.
        assert_eq!(
            field(&stdout, "grid_violations"),
            0,
            "{period} us:\n{stdout}"
        );
    }
}

/// Where check-ins exist, a coalescing timer still delivers about once per
/// period: each expiry ends a slice, its copy is dequeued, and the timer
/// re-arms. A timer that never re-arms delivers once.
fn syscall_heavy_timer_delivers_each_period(period_us: i64, calls: &str) {
    let guest = fixture(&ITIMER_LOOP, "deadline_itimer_loop");
    let (stdout, _) = run_strict(guest, &[&period_us.to_string(), calls], &[]);
    let signals = field(&stdout, "signals");
    let periods = field(&stdout, "virtual_ms") * 1_000 / period_us;
    assert!(
        periods >= 10 && signals >= periods * 9 / 10 && signals <= periods + 1,
        "{period_us} us: {signals} signals over {periods} periods:\n{stdout}"
    );
}

/// [`syscall_heavy_timer_delivers_each_period`] at 2 ms, 150k calls.
#[test]
fn itimer_syscall_heavy_timer_delivers_each_2_ms_period() {
    syscall_heavy_timer_delivers_each_period(2_000, "150");
}

/// [`syscall_heavy_timer_delivers_each_period`] at 5 ms, 250k calls: about 15
/// periods. Each case is its own test to stay inside the per-test CPU bound.
#[test]
fn itimer_syscall_heavy_timer_delivers_each_5_ms_period() {
    syscall_heavy_timer_delivers_each_period(5_000, "250");
}

/// Two CPU-bound threads under a fast timer finish in about the time of their
/// work. Before, at 100 and 400 us the expiries missed during one thread's
/// slice fired one per pass, each a charged thread switch, and the run never
/// finished; at 1000 us it took 1.5 times the work.
#[test]
fn itimer_two_cpu_bound_threads_finish_in_their_work_time() {
    let guest = fixture(&ITIMER_COALESCE, "itimer_coalesce");
    // Every run here retries a typed PMU skid refusal: these two-thread runs
    // take precise PMU stops too.
    let stdout = run_strict_retrying_skid(guest, &["storm", "50000", "20", "2"], &[]);
    let work_us = field(&stdout, "work_us");
    assert!(
        work_us >= 2 * MIN_STORM_WORK_US,
        "reference took {work_us} us:\n{stdout}"
    );
    for period in ["100", "400", "1000"] {
        let stdout = run_strict_retrying_skid(guest, &["storm", period, "20", "2"], &[]);
        let virtual_us = field(&stdout, "work_us");
        assert!(
            virtual_us >= work_us * 9 / 10 && virtual_us <= work_us * 5 / 4,
            "{period} us: {virtual_us} virtual us against {work_us}:\n{stdout}"
        );
    }
    // Under 20 ms slices the work W crosses W / 20 ms slice ends, plus the
    // thread start, join and exit boundaries: one delivery each.
    let stdout = run_strict_retrying_skid(guest, &["storm", "1000", "20", "2"], &[TWENTY_MS_SLICE]);
    let slices = field(&stdout, "work_us") / 20_000;
    let total = field(&stdout, "t0_signals") + field(&stdout, "t1_signals");
    assert!(
        (slices - 1..=slices + 5).contains(&total),
        "{total} deliveries for {slices} slice ends:\n{stdout}"
    );
}

/// Expiries during 3 blocked periods coalesce into one copy: `getitimer`
/// reads 0 while it is pending (Linux's inactive timer), and with the timer
/// disarmed, unblocking delivers exactly one and nothing follows.
#[test]
fn itimer_a_blocked_expiry_is_delivered_once() {
    let guest = fixture(&ITIMER_COALESCE, "itimer_coalesce");
    for stdout in [
        run_strict(guest, &["blocked", "5000"], &[]).0,
        run_native(&["blocked", "50000"]),
    ] {
        assert_eq!(field(&stdout, "blocked_remaining_us"), 0, "{stdout}");
        assert_eq!(field(&stdout, "in_unblock"), 1, "{stdout}");
        assert_eq!(field(&stdout, "after"), 0, "{stdout}");
    }
}

/// After the blocked copy is delivered, the timer recurs: through 10 periods
/// of `getppid` calls each delivery's clock is at or past the first grid point
/// after the previous one, with at least 3 after the first. Natively, host
/// descheduling can merge expiries, so at least one follows.
#[test]
fn itimer_a_timer_recurs_after_unblocking() {
    let guest = fixture(&ITIMER_COALESCE, "itimer_coalesce");
    let (stdout, _) = run_strict(guest, &["live", "5000"], &[]);
    assert_eq!(field(&stdout, "blocked_remaining_us"), 0, "{stdout}");
    assert!(field(&stdout, "deliveries") >= 4, "{stdout}");
    assert_eq!(field(&stdout, "grid_violations"), 0, "{stdout}");
    let native = run_native(&["live", "50000"]);
    assert_eq!(
        field(&native, "blocked_remaining_us"),
        0,
        "native: {native}"
    );
    assert!(field(&native, "deliveries") >= 2, "native: {native}");
}

/// A non-blocking signalfd read that dequeues the timer's copy restarts the
/// timer at the read, even when the copy-out then faults (EFAULT after the
/// dequeue), for `read` and `readv`, or the read is a `preadv2` at offset -1.
/// Every path creates the signalfd after the expiry (late registration).
/// Each consumption is acknowledged: the call's result, errno and record, and
/// the copy gone from the pending set at once, before the re-arm proof
/// accepts a fresh copy. After two periods of
/// syscall-free work the restarted timer has expired again: a new copy is
/// pending and `getitimer` reads at most 1 us. Without a re-arm at the read,
/// the later query would restart it from its own clock and read more than
/// half a period. A `rt_sigtimedwait` that faults on its set dequeues nothing:
/// the original copy stays and the timer stays inactive.
#[test]
fn itimer_a_signalfd_dequeue_re_arms_the_timer() {
    let guest = fixture(&ITIMER_COALESCE, "itimer_coalesce");
    for (path, result, errno, signo) in [
        ("read", 128, 0, 14),
        ("fault", -1, 14, 0),
        ("readv_fault", -1, 14, 0),
        ("preadv2", 128, 0, 14),
    ] {
        let (stdout, _) = run_strict(guest, &["sfd", "5000", path, "2"], &[]);
        // Linux: the same consumption and causal proof; `getitimer` is
        // informational there.
        let native = run_native(&["sfd", "50000", path, "2"]);
        for out in [&stdout, &native] {
            assert_eq!(field(out, "result"), result, "{path}: {out}");
            assert_eq!(field(out, "errno"), errno, "{path}: {out}");
            assert_eq!(field(out, "signo"), signo, "{path}: {out}");
            assert_eq!(field(out, "cleared"), 1, "{path}: {out}");
            assert_eq!(word(out, "rearm"), "PASS", "{path}: {out}");
        }
        assert!(field(&stdout, "remaining_us") <= 1, "{path}: {stdout}");
        assert_eq!(field(&stdout, "pending_after"), 1, "{path}: {stdout}");
    }
    let (stdout, _) = run_strict(guest, &["sfd", "5000", "read", "0"], &[]);
    let remaining = field(&stdout, "remaining_us");
    assert!(
        (1..=5_000).contains(&remaining),
        "immediate query: {stdout}"
    );
    // Controls that fail before any dequeue: the original copy stays, the
    // timer stays inactive, and the proof never accepts that copy.
    for (path, errno) in [("badset", 14), ("ebadf", 9)] {
        let (stdout, _) = run_strict(guest, &["sfd", "5000", path, "0"], &[]);
        let native = run_native(&["sfd", "50000", path, "0"]);
        for out in [&stdout, &native] {
            assert_eq!(field(out, "errno"), errno, "{path}: {out}");
            assert_eq!(field(out, "cleared"), 0, "{path}: {out}");
            assert_eq!(word(out, "rearm"), "UNCONSUMED", "{path}: {out}");
        }
        assert_eq!(field(&stdout, "pending_after"), 1, "{path}: {stdout}");
        assert_eq!(field(&stdout, "remaining_us"), 0, "{path}: {stdout}");
    }
}

/// An expiry the leader ignores (SIG_IGN, unblocked) is discarded at
/// generation and the timer stops; installing a handler does not restart it,
/// but the dequeue of a guest's own `kill` does.
#[test]
fn itimer_an_ignored_expiry_stops_the_timer_until_a_dequeue() {
    let guest = fixture(&ITIMER_COALESCE, "itimer_coalesce");
    for stdout in [
        run_strict(guest, &["ignored", "5000"], &[]).0,
        run_native(&["ignored", "50000"]),
    ] {
        assert_eq!(field(&stdout, "ignored_remaining_us"), 0, "{stdout}");
        assert_eq!(field(&stdout, "handled_remaining_us"), 0, "{stdout}");
        assert_eq!(field(&stdout, "before_kill"), 0, "{stdout}");
        assert_eq!(field(&stdout, "kill_deliveries"), 1, "{stdout}");
        assert!(
            field(&stdout, "after") >= 1,
            "the kill's dequeue re-armed nothing: {stdout}"
        );
    }
}

/// Setting SIG_IGN flushes the pending copy without a dequeue, so the timer
/// stays inactive.
#[test]
fn itimer_sig_ign_flushes_the_pending_copy() {
    let guest = fixture(&ITIMER_COALESCE, "itimer_coalesce");
    for stdout in [
        run_strict(guest, &["flush", "5000"], &[]).0,
        run_native(&["flush", "50000"]),
    ] {
        assert_eq!(field(&stdout, "flushed"), 1, "{stdout}");
        assert_eq!(field(&stdout, "remaining_us"), 0, "{stdout}");
        assert_eq!(field(&stdout, "deliveries"), 0, "{stdout}");
    }
}

/// A timer re-armed in the middle of a turn, by a non-blocking signalfd read
/// between stretches of syscall-free work, records the slice-end hint, so the
/// reader keeps up with a 5 ms period at the default slice. Without the hint,
/// about one record per 200 ms slice gets through.
#[test]
fn itimer_a_mid_turn_re_arm_keeps_a_signalfd_reader_timely() {
    let guest = fixture(&ITIMER_COALESCE, "itimer_coalesce");
    let (stdout, _) = run_strict(guest, &["midturn", "5000", "200"], &[]);
    let records = field(&stdout, "records");
    let periods = field(&stdout, "elapsed_us") / 5_000;
    assert!(
        periods >= 30 && records >= periods * 9 / 10 && records <= periods + 1,
        "{records} records over {periods} periods:\n{stdout}"
    );
}

/// A `rt_sigtimedwait` that dequeues the timer's copy restarts the timer at
/// the call, as the signalfd read does, including when the siginfo copy-out
/// faults after the dequeue. This is the O2 witness for `rt_sigtimedwait`:
/// after two periods of syscall-free work the restarted timer has expired
/// again, so `getitimer` reads at most 1 us and a new copy is pending; without
/// a re-arm at the call, the later query would restart it from its own clock.
#[test]
fn itimer_an_rt_sigtimedwait_dequeue_re_arms_the_timer() {
    let guest = fixture(&ITIMER_COALESCE, "itimer_coalesce");
    for (path, errno) in [("ok", 0), ("fault", 14)] {
        let (stdout, _) = run_strict(guest, &["stw", "5000", path, "2"], &[]);
        let native = run_native(&["stw", "50000", path, "2"]);
        for out in [&stdout, &native] {
            assert_eq!(field(out, "errno"), errno, "{path}: {out}");
            assert_eq!(field(out, "cleared"), 1, "{path}: {out}");
            assert_eq!(word(out, "rearm"), "PASS", "{path}: {out}");
        }
        if errno == 0 {
            assert_eq!(field(&stdout, "result"), 14, "{stdout}");
        }
        assert!(field(&stdout, "remaining_us") <= 1, "{path}: {stdout}");
        assert_eq!(field(&stdout, "pending_after"), 1, "{path}: {stdout}");
    }
}

/// A copy consumed late in a turn, after 10 periods of syscall-free work,
/// restarts the timer on the grid after the read, not at the turn's start: a
/// restart from the turn's start would already be due, which `getitimer`
/// reports as the 1 us floor of an overdue timer. The next expiry is not
/// observed before the deadline that `getitimer` reported.
#[test]
fn itimer_a_late_consumption_re_arms_after_the_read() {
    let guest = fixture(&ITIMER_COALESCE, "itimer_coalesce");
    let (stdout, _) = run_strict(guest, &["late", "5000"], &[]);
    assert_eq!(field(&stdout, "result"), 128, "{stdout}");
    let remaining = field(&stdout, "remaining_us");
    assert!((2..=5_000).contains(&remaining), "{stdout}");
    let next = field(&stdout, "next_after_us");
    assert!(
        next >= remaining && next <= remaining + 5_000,
        "the next expiry came {next} us after the read, {remaining} us left:\n{stdout}"
    );
    let native = run_native(&["late", "50000"]);
    assert_eq!(field(&native, "result"), 128, "native: {native}");
    assert!(field(&native, "next_after_us") > 0, "native: {native}");
}

/// A handler that runs for 3 periods, without `SA_NODEFER`, takes at most one
/// copy per return: the expiries during it coalesce into one pending copy,
/// so entries are at least 3 periods apart and at most 11 fit in the window
/// of 30. The handler stops the timer itself at the window's end.
#[test]
fn itimer_a_long_handler_takes_one_copy_per_return() {
    let guest = fixture(&ITIMER_COALESCE, "itimer_coalesce");
    let (stdout, _) = run_strict(guest, &["longhandler", "5000"], &[]);
    let entries = field(&stdout, "entries");
    assert!((6..=11).contains(&entries), "{entries} entries:\n{stdout}");
    assert!(field(&stdout, "min_spacing_us") >= 15_000, "{stdout}");
    assert_eq!(field(&stdout, "stopped"), 1, "{stdout}");
    let native = run_native(&["longhandler", "5000"]);
    assert!(field(&native, "entries") >= 2, "native: {native}");
}

/// SIG_IGN with the leader blocking SIGALRM and a worker not blocking it:
/// the leader's blocked mask makes the expiry a real send, the worker's
/// dequeue discards it and restarts the timer, so the timer keeps running.
/// Discarding at generation because some thread ignores it would leave the
/// timer stopped, reading 0 at every sample after the first expiry.
///
/// Under Hermit (`mixedsample`, every half period for 10 periods) at least 15
/// of the 20 samples read positive. A sample can legally read 0 in the window
/// between an expiry and the worker's dequeue, as on Linux, whenever the
/// leader is resumed first after the pop; the timer's own grid puts at most a
/// few samples there. A stopped timer reads positive at most at the first
/// two samples, before its first expiry. The acknowledged protocol of
/// `mixed` runs natively with a 50 ms period, where at least one
/// acknowledged generation must read positive.
#[test]
fn itimer_mixed_masks_keep_the_timer_running() {
    let guest = fixture(&ITIMER_COALESCE, "itimer_coalesce");
    let (stdout, _) = run_strict(guest, &["mixedsample", "5000"], &[]);
    let samples = field(&stdout, "samples");
    assert_eq!(samples, 20, "{stdout}");
    assert!(field(&stdout, "positive") >= 15, "{stdout}");
    let native = run_native(&["mixed", "50000"]);
    assert!(field(&native, "positive") >= 1, "native: {native}");
}

/// SIG_IGN set while SIGALRM is blocked: the expiry queues, and unblocking
/// dequeues and discards the ignored copy, which restarts the timer.
#[test]
fn itimer_an_ignored_blocked_copy_re_arms_when_discarded() {
    let guest = fixture(&ITIMER_COALESCE, "itimer_coalesce");
    let (stdout, _) = run_strict(guest, &["ignblocked", "5000"], &[]);
    let remaining = field(&stdout, "remaining_us");
    assert!((1..=5_000).contains(&remaining), "{stdout}");
    assert_eq!(word(&stdout, "rearm"), "PASS", "{stdout}");
    let native = run_native(&["ignblocked", "50000"]);
    assert_eq!(word(&native, "rearm"), "PASS", "native: {native}");
}

/// A private `tgkill` copy is taken before the timer's shared copy and does
/// not restart the timer: the first handler reads 0. The shared copy's
/// dequeue does: the second reads a positive remainder.
#[test]
fn itimer_a_private_copy_does_not_re_arm() {
    let guest = fixture(&ITIMER_COALESCE, "itimer_coalesce");
    let (stdout, _) = run_strict(guest, &["private", "5000"], &[]);
    assert_eq!(field(&stdout, "handled"), 2, "{stdout}");
    assert_eq!(field(&stdout, "first_remaining_us"), 0, "{stdout}");
    let second = field(&stdout, "second_remaining_us");
    assert!((1..=5_000).contains(&second), "{stdout}");
    let native = run_native(&["private", "50000"]);
    assert_eq!(field(&native, "handled"), 2, "native: {native}");
    assert_eq!(field(&native, "first_remaining_us"), 0, "native: {native}");
}

/// A `sigsuspend` timer loop, where each wait ends at the pop, gets every
/// expiry: 20 waits, 20 deliveries.
#[test]
fn itimer_a_sigsuspend_loop_takes_every_expiry() {
    let guest = fixture(&ITIMER_COALESCE, "itimer_coalesce");
    let (stdout, _) = run_strict(guest, &["sigsuspend", "5000", "20"], &[]);
    assert_eq!(field(&stdout, "deliveries"), 20, "{stdout}");
    let native = run_native(&["sigsuspend", "5000", "20"]);
    assert_eq!(field(&native, "deliveries"), 20, "native: {native}");
}

/// A consumer whose descriptor table is private (a thread created without
/// CLONE_FILES, whose creator then closes its own copy of the signalfd)
/// dequeues the copy, and the timer restarts at its read: after two periods
/// of syscall-free work it has expired again. O2 keys on Detcore's
/// classification of the consuming thread's own descriptor.
#[test]
fn itimer_a_private_descriptor_table_consumer_re_arms() {
    let guest = fixture(&ITIMER_COALESCE, "itimer_coalesce");
    let (stdout, _) = run_strict(guest, &["privfd", "5000"], &[]);
    let native = run_native(&["privfd", "50000"]);
    for out in [&stdout, &native] {
        assert_eq!(field(out, "result"), 128, "{out}");
        assert_eq!(field(out, "cleared"), 1, "{out}");
        assert_eq!(field(out, "pending_after"), 1, "{out}");
    }
    assert!(field(&stdout, "remaining_us") <= 1, "{stdout}");
}

/// A stopped timer (SIG_IGN, discarded at generation) restarts on the dequeue
/// of a SIGALRM that a POSIX timer produced, as Linux restarts it on any
/// shared-queue SIGALRM dequeue: `getitimer` reads a positive remainder at
/// once and a fresh copy follows.
#[test]
fn itimer_a_posix_timer_copy_restarts_a_stopped_timer() {
    let guest = fixture(&ITIMER_COALESCE, "itimer_coalesce");
    let (stdout, _) = run_strict(guest, &["posix", "20000"], &[]);
    let native = run_native(&["posix", "20000"]);
    for out in [&stdout, &native] {
        assert_eq!(field(out, "stopped_us"), 0, "{out}");
        assert_eq!(field(out, "result"), 128, "{out}");
        assert_eq!(field(out, "signo"), 14, "{out}");
        assert_eq!(field(out, "cleared"), 1, "{out}");
        assert_eq!(word(out, "rearm"), "PASS", "{out}");
    }
    let immediate = field(&stdout, "immediate_us");
    assert!((1..=20_000).contains(&immediate), "{stdout}");
}

/// Generation discard follows the mask Linux's `blocked` holds during a wait.
/// With SIG_IGN and SIGALRM unblocked, an expiry during a plain `poll`,
/// `select` or `pselect6` without a mask, where Detcore blocks every signal for
/// the wait, or during a `ppoll` whose temporary mask leaves it unblocked, is
/// discarded and the timer stays stopped. A `ppoll` or `pselect6` whose
/// temporary mask blocks it queues the copy, whose discard as the call returns
/// restarts the timer.
#[test]
fn itimer_generation_discard_follows_the_wait_mask() {
    let guest = fixture(&ITIMER_COALESCE, "itimer_coalesce");
    for kind in ["poll", "select", "pselect6_null", "ppoll_unblock"] {
        let (stdout, _) = run_strict(guest, &["pollmask", "20000", kind], &[]);
        let native = run_native(&["pollmask", "20000", kind]);
        for out in [&stdout, &native] {
            assert_eq!(field(out, "result"), 0, "{kind}: {out}");
            assert_eq!(field(out, "first_us"), 0, "{kind}: {out}");
            assert_eq!(field(out, "positive"), 0, "{kind}: {out}");
        }
    }
    for kind in ["ppoll_block", "pselect6_block"] {
        let (stdout, _) = run_strict(guest, &["pollmask", "20000", kind], &[]);
        let first = field(&stdout, "first_us");
        assert!((1..=20_000).contains(&first), "{kind}: {stdout}");
        let native = run_native(&["pollmask", "20000", kind]);
        assert!(field(&native, "positive") >= 1, "native {kind}: {native}");
    }
}

/// A wait's published mask is withdrawn when the wait returns. SIGALRM is
/// SIG_IGN and unblocked during a 30 ms wait of each publishing kind; then the
/// thread blocks SIGALRM and arms a 10 ms timer. Linux queues that expiry
/// (blocked means not ignored), so a copy is pending 45 ms later. A mask
/// left published from the wait (SIGALRM unblocked) would discard it.
#[test]
fn itimer_a_wait_mask_is_withdrawn_when_the_wait_returns() {
    let guest = fixture(&ITIMER_COALESCE, "itimer_coalesce");
    for kind in [
        "poll",
        "select",
        "pselect6_null",
        "pselect6_mask",
        "ppoll_mask",
        "epoll_pwait",
        "sigtimedwait",
    ] {
        let (stdout, _) = run_strict(guest, &["stale", kind], &[]);
        let native = run_native(&["stale", kind]);
        for out in [&stdout, &native] {
            assert_eq!(field(out, "pending"), 1, "{kind}: {out}");
        }
    }
}

/// Run `args` under `--verify --verify-strict` with `run_args`, retrying a
/// typed skid refusal, and require canonical INFO verification with no
/// relaxation.
fn verify_bitwise(args: &[&str], run_args: &[&str]) {
    let guest = fixture(&ITIMER_COALESCE, "itimer_coalesce");
    let mut command = Command::new("timeout");
    command
        .arg("--kill-after=2s")
        .arg(format!("{TIMEOUT_SECONDS}s"))
        .arg(hermit_test::hermit_binary())
        .args([
            "--log=info",
            "--backend=ptrace",
            "run",
            "--base-env=minimal",
            "--strict",
            "--verify",
            "--verify-strict",
            "--epoch=2026-01-01T00:00:00Z",
            "--seed=1",
            TWENTY_MS_SLICE,
        ])
        .args(run_args)
        .arg("--")
        .arg(guest)
        .args(args);
    hermit_test::configure_guest_execution(&mut command);
    let rendered = format!("{command:?}");
    let output = hermit_test::hermit_output(&rendered, &mut command);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "verify failed: {rendered}\nstatus: {}\nstderr:\n{stderr}",
        output.status
    );
    assert!(
        stderr.contains("comparison=BitwiseInfoV1 relaxations=none"),
        "verify did not use the canonical comparator: {rendered}\nstderr:\n{stderr}"
    );
}

/// The programs whose schedules the heuristic verifications cover: one and
/// two CPU-bound threads, a `sigsuspend` loop, and a leader in `pause` beside
/// a computing worker, at half the default-heuristic loop sizes so that each
/// test stays inside the per-test CPU bound.
const VERIFIED: [&[&str]; 4] = [
    &["storm", "400", "10", "1"],
    &["storm", "400", "10", "2"],
    &["sigsuspend", "5000", "20"],
    &["pause", "1000", "10"],
];

/// The coalescing schedules of one and two CPU-bound threads are
/// deterministic: canonical INFO verification (`--verify --verify-strict`)
/// passes with no relaxation.
#[test]
fn itimer_coalescing_schedules_verify_bitwise() {
    verify_bitwise(&["storm", "400", "20", "1"], &[]);
    verify_bitwise(&["storm", "400", "20", "2"], &[]);
}

/// The waiting schedules verify the same way: a `sigsuspend` loop, a leader
/// in `pause` beside a computing worker, a signalfd consumer, and a loop
/// beside a sibling whose 1 ms `poll` retries are host timed.
#[test]
fn itimer_waiting_schedules_verify_bitwise() {
    verify_bitwise(&["sigsuspend", "5000", "20"], &[]);
    verify_bitwise(&["pause", "1000", "20"], &[]);
    verify_bitwise(&["sfd", "5000", "read", "2"], &[]);
    verify_bitwise(&["polling", "1000", "20"], &[]);
}

/// The programs verify under the random scheduling heuristic.
#[test]
fn itimer_coalescing_verifies_under_the_random_heuristic() {
    for args in VERIFIED {
        verify_bitwise(args, &["--sched-heuristic=random"]);
    }
}

/// The programs verify under chaos, whose preemption points move every slice
/// end.
#[test]
fn itimer_coalescing_verifies_under_chaos() {
    for args in VERIFIED {
        verify_bitwise(args, &["--chaos"]);
    }
}

/// Replaying a recorded chaos schedule of two CPU-bound threads reproduces the
/// run: the loop index of each delivery, and the scheduler turns, syscalls
/// and virtual time of the summary.
#[test]
fn itimer_a_recorded_schedule_replays() {
    let guest = fixture(&ITIMER_COALESCE, "itimer_coalesce");
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("itimer-coalesce-replay");
    std::fs::create_dir_all(&dir).expect("failed to create the replay directory");
    let preemptions = dir.join("preemptions.json");
    let _ = std::fs::remove_file(&preemptions);
    let mut results = Vec::new();
    for (phase, option) in [
        ("record", "--record-preemptions-to"),
        ("replay", "--replay-preemptions-from"),
    ] {
        let summary = dir.join(format!("{phase}.summary.json"));
        let _ = std::fs::remove_file(&summary);
        let run_args = [
            "--chaos".to_owned(),
            "--seed=1".to_owned(),
            TWENTY_MS_SLICE.to_owned(),
            format!("{option}={}", preemptions.display()),
            format!("--summary-json={}", summary.display()),
        ];
        let run_args: Vec<&str> = run_args.iter().map(String::as_str).collect();
        let stdout = run_strict_retrying_skid(guest, &["storm", "400", "20", "2"], &run_args);
        let text = std::fs::read_to_string(&summary)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", summary.display()));
        let json: serde_json::Value = serde_json::from_str(&text)
            .unwrap_or_else(|error| panic!("{} is not JSON: {error}", summary.display()));
        let counts: Vec<u64> = ["sched_turns", "syscalls", "virttime_elapsed"]
            .iter()
            .map(|name| {
                json[*name]
                    .as_u64()
                    .unwrap_or_else(|| panic!("{phase} summary has no integer {name}: {text}"))
            })
            .collect();
        results.push((stdout, counts));
    }
    assert_eq!(
        results[0], results[1],
        "the replay did not reproduce the record"
    );
}
