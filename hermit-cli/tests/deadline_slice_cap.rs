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
        .arg("--")
        .arg(guest)
        .args(guest_args);
    hermit_test::configure_guest_execution(&mut command);
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start guest: {rendered}: {error}"));
    let stdout = String::from_utf8(output.stdout).expect("guest stdout should be UTF-8");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "guest failed: {rendered}\nstatus: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status,
    );
    (stdout, stderr)
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
/// timer, and delivers about one signal per period, as Linux does.
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
            periods >= 100 && signals >= periods * 9 / 10 && signals <= periods + 1,
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
        let output = command
            .output()
            .unwrap_or_else(|error| panic!("failed to start guest: {rendered}: {error}"));
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
