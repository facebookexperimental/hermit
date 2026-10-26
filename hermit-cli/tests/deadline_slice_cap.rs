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
//!
//! The suite runs on ptrace unless the harness names a backend in
//! `HERMIT_TEST_BACKEND` (see [`backend_option`]). The validation node
//! privileged-test.deadline_slice_cap_kvm runs it on KVM, where every test must
//! pass except those in `deadline_slice_cap.kvm-gaps.tsv`, the KVM gap ledger
//! beside this file: each row names a known KVM gap and the issue tracking it.
//! The node runs a row's test, which must still fail on KVM the way the row
//! says and never counts as a pass, or, when the row says skipped, leaves it
//! out of the KVM run. [`capped_schedules_verify_bitwise`] also proves to that
//! node that its runs were on KVM ([`BACKEND_PROOF_ENV`]).

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::fs;
use std::io::Write;
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

/// The `--backend` option every Hermit run of the suite gets, from the
/// harness's `HERMIT_TEST_BACKEND` (`ptrace` or `kvm`). Unset, there is none:
/// the runs use Hermit's default backend, ptrace, exactly as before this
/// switch, and the verification runs keep the `--backend=ptrace` they have
/// always named ([`verification_backend_option`]). With `kvm`, every run names
/// KVM, the verification runs included, so no test can pass on KVM's behalf by
/// running on ptrace. Any other value, an empty one included, refuses: a
/// misspelled backend must not fall back to ptrace and report a KVM pass.
fn backend_option() -> Option<&'static str> {
    static OPTION: OnceLock<Option<&'static str>> = OnceLock::new();
    *OPTION.get_or_init(|| {
        let value = std::env::var_os("HERMIT_TEST_BACKEND")?;
        match value.to_str() {
            Some("ptrace") => Some("--backend=ptrace"),
            Some("kvm") => Some("--backend=kvm"),
            _ => panic!("HERMIT_TEST_BACKEND must be ptrace or kvm, got {value:?}"),
        }
    })
}

/// The `--backend` option of a `--verify` run: the harness's backend
/// ([`backend_option`]), or ptrace, which these runs have always named.
fn verification_backend_option() -> &'static str {
    backend_option().unwrap_or("--backend=ptrace")
}

/// What a verified `--backend=kvm` run prints after its comparison, and no
/// other backend prints (hermit-cli/src/bin/hermit/run.rs and verify_run.rs).
const KVM_VERIFY_BANNER: &str = ":: Backend: KVM (reverie-kvm KvmGuest<Detcore>)";

/// The file a node's runner names for the backend proof
/// (`known_gaps::BACKEND_PROOF_ENV` in ci/manifest-plan): when it is set,
/// [`capped_schedules_verify_bitwise`] appends the backend each verified run
/// showed, one line per run. ci/kvm-gap-ledger.rs requires every line to name
/// the node's backend, so a runner that stopped passing `HERMIT_TEST_BACKEND`,
/// or a suite that stopped honouring it, fails the node instead of running the
/// suite on ptrace.
const BACKEND_PROOF_ENV: &str = "HERMIT_TEST_BACKEND_PROOF";

static SPIN_WAIT: OnceLock<PathBuf> = OnceLock::new();
static FUTEX_RESUME: OnceLock<PathBuf> = OnceLock::new();
static SELF_ALARM: OnceLock<PathBuf> = OnceLock::new();
static START_CAP: OnceLock<PathBuf> = OnceLock::new();
static PARENT_CONTINUE: OnceLock<PathBuf> = OnceLock::new();
static SLEEPY_SIBLING: OnceLock<PathBuf> = OnceLock::new();
static ITIMER_LOOP: OnceLock<PathBuf> = OnceLock::new();
static ITIMER_STORM: OnceLock<PathBuf> = OnceLock::new();
static LONE_SPIN: OnceLock<PathBuf> = OnceLock::new();

/// Compile `hermit-cli/tests/fixtures/<name>.c` once.
fn fixture(cell: &'static OnceLock<PathBuf>, name: &str) -> &'static Path {
    cell.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("deadline-slice-cap-{}", std::process::id()));
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
    strict_output_env(guest, guest_args, hermit_args, run_args, &[])
}

/// [`strict_output`] with `env` set for Hermit.
fn strict_output_env(
    guest: &Path,
    guest_args: &[&str],
    hermit_args: &[&str],
    run_args: &[&str],
    env: &[(&str, &str)],
) -> (Output, String) {
    let mut command = Command::new("timeout");
    command
        .envs(env.iter().copied())
        .arg("--kill-after=2s")
        .arg(format!("{TIMEOUT_SECONDS}s"))
        .arg(hermit_test::hermit_binary())
        .args(backend_option())
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
                verification_backend_option(),
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
        // The positive control of the backend switch: the verified run ran on
        // the backend it was asked for, which only KVM's banner shows.
        let on_kvm = stderr.contains(KVM_VERIFY_BANNER);
        assert_eq!(
            on_kvm,
            verification_backend_option() == "--backend=kvm",
            "the verified run did not show the requested {}: {rendered}\nstderr:\n{stderr}",
            verification_backend_option()
        );
        if let Some(path) = std::env::var_os(BACKEND_PROOF_ENV) {
            let mut proof = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap_or_else(|error| {
                    panic!("cannot open {BACKEND_PROOF_ENV}={path:?}: {error}")
                });
            writeln!(proof, "{}", if on_kvm { "kvm" } else { "ptrace" }).unwrap_or_else(|error| {
                panic!("cannot write {BACKEND_PROOF_ENV}={path:?}: {error}")
            });
        }
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

/// [`syscall_heavy_timer_delivers_each_period`] at 2 ms, 150k calls. A 5 ms,
/// 250k-call case asserted the same bound in the same regime (above the 1 ms
/// `fast_timer_period` exclusion, far below the maximum slice) and was deleted
/// by the 2026-10-17 test audit.
#[test]
fn itimer_syscall_heavy_timer_delivers_each_2_ms_period() {
    syscall_heavy_timer_delivers_each_period(2_000, "150");
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
///
/// `ppoll_mask` runs last: on KVM it is a known gap (F' of
/// https://github.com/rrnewton/hermit/issues/4929: a masked ppoll that would
/// wait is refused by name), so the ledger row that records its refusal also
/// shows that every other kind passed there first.
#[test]
fn itimer_a_wait_mask_is_withdrawn_when_the_wait_returns() {
    let guest = fixture(&ITIMER_COALESCE, "itimer_coalesce");
    for kind in [
        "poll",
        "select",
        "pselect6_null",
        "pselect6_mask",
        "epoll_pwait",
        "sigtimedwait",
        "ppoll_mask",
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
            verification_backend_option(),
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
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("itimer-coalesce-replay-{}", std::process::id()));
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

/// The INFO log lines in `stderr`, without their wall-clock prefixes.
fn info_lines(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .filter(|line| line.contains(" INFO "))
        .map(|line| line.split_once(' ').map_or(line, |(_, rest)| rest))
        .collect()
}

/// `lines` with each host address (`<hostaddr 0x...>`, which address-space
/// randomization moves from run to run) replaced by its ordinal of first
/// appearance, as `--verify-strict` compares them.
fn canonical_host_addresses(lines: Vec<&str>) -> Vec<String> {
    let mut ordinals: Vec<String> = Vec::new();
    lines
        .into_iter()
        .map(|line| {
            let mut out = String::new();
            let mut rest = line;
            while let Some(start) = rest.find("<hostaddr 0x") {
                let Some(len) = rest[start..].find('>') else {
                    break;
                };
                let address = &rest[start..=start + len];
                let ordinal = ordinals
                    .iter()
                    .position(|seen| seen == address)
                    .unwrap_or_else(|| {
                        ordinals.push(address.to_owned());
                        ordinals.len() - 1
                    });
                out.push_str(&rest[..start]);
                out.push_str(&format!("<hostaddr #{ordinal}>"));
                rest = &rest[start + len + 1..];
            }
            out.push_str(rest);
            out
        })
        .collect()
}

/// What a run of the `lone_spin_then_race` fixture printed and logged.
struct LoneRun {
    stdout: String,
    info: Vec<String>,
    /// `PreemptionSummary::stopless_slice_ends` from `--summary-json`.
    stopless: Option<u64>,
    /// `schedevent_desynced` from `--summary-json`.
    desynced: u64,
}

/// The directory the lone-slice-end tests write summaries and records to.
fn lone_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("lone-slice-ends-{}", std::process::id()));
    fs::create_dir_all(&dir).expect("failed to create the summary directory");
    dir
}

/// Run the fixture in `mode` under `--strict --chaos --seed=3` at
/// `max_timeslice`, skipping a lone thread's slice-end stops (the default)
/// or taking them (`eager`).
fn lone_run(mode: &str, max_timeslice: &str, eager: bool) -> LoneRun {
    lone_run_with(mode, max_timeslice, eager, "run", &[])
}

/// [`lone_run`] with more `hermit run` arguments, for a run named `tag`.
fn lone_run_with(
    mode: &str,
    max_timeslice: &str,
    eager: bool,
    tag: &str,
    extra: &[String],
) -> LoneRun {
    let guest = fixture(&LONE_SPIN, "lone_spin_then_race");
    let summary = lone_dir().join(format!("{mode}-{max_timeslice}-{eager}-{tag}.json"));
    let mut run_args = vec![
        "--chaos".to_owned(),
        "--seed=3".to_owned(),
        format!("--max-timeslice={max_timeslice}"),
        format!("--summary-json={}", summary.display()),
    ];
    if eager {
        run_args.push("--eager-lone-preemptions".to_owned());
    }
    run_args.extend_from_slice(extra);
    let run_args: Vec<&str> = run_args.iter().map(String::as_str).collect();
    let (stdout, stderr) = run_strict_with(guest, &[mode], &["--log=info"], &run_args);
    let text = fs::read_to_string(&summary)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", summary.display()));
    let json: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("{} is not JSON: {error}", summary.display()));
    LoneRun {
        stdout,
        info: canonical_host_addresses(info_lines(&stderr)),
        stopless: json["preemption"]["stopless_slice_ends"].as_u64(),
        desynced: json["schedevent_desynced"]
            .as_u64()
            .unwrap_or_else(|| panic!("{tag}: no schedevent_desynced in {text}")),
    }
}

/// Run `mode` both ways and require the same output and the same INFO log,
/// line for line; return how many ends the skipping run took without a stop.
/// `--eager-lone-preemptions` takes every stop and reports no count.
fn lone_and_eager_agree(mode: &str, max_timeslice: &str) -> u64 {
    let lazy = lone_run(mode, max_timeslice, false);
    let eager = lone_run(mode, max_timeslice, true);
    assert!(lazy.stdout.contains("sum="), "{mode}: {}", lazy.stdout);
    assert_eq!(lazy.stdout, eager.stdout, "{mode}: outputs differ");
    assert!(!lazy.info.is_empty(), "{mode}: no INFO lines");
    if let Some((index, (lazy, eager))) = lazy
        .info
        .iter()
        .zip(&eager.info)
        .enumerate()
        .find(|(_, (lazy, eager))| lazy != eager)
    {
        panic!("{mode}: INFO line {index} differs:\n  skipping: {lazy}\n  eager:    {eager}");
    }
    assert_eq!(
        lazy.info.len(),
        eager.info.len(),
        "{mode}: INFO line counts"
    );
    assert_eq!(
        eager.stopless, None,
        "{mode}: --eager-lone-preemptions skips no stop"
    );
    lazy.stopless
        .unwrap_or_else(|| panic!("{mode}: the skipping run reports no count"))
}

/// A slice end where no other thread could run is taken without a stop and
/// processed when the thread next enters Detcore. Under chaos at 1000 ns, a
/// main thread computing 25000 iterations alone while its worker waits on a
/// condition variable, then racing it on a counter, prints the same output
/// and the same INFO log as with a stop at every slice end, and takes some
/// ends without a stop.
#[test]
fn slice_ends_taken_without_a_stop_leave_the_run_unchanged() {
    let skipped = lone_and_eager_agree("wait", "1000");
    assert!(skipped > 0, "no slice end was taken without a stop");
}

/// A worker in a timed wait could run at any slice end, so main's loop keeps
/// its stops; with the worker in an untimed wait it skips them. At 1000 ns
/// the 25000-iteration loop alone is about 500 slice ends.
#[test]
fn a_timed_waiter_keeps_the_stops_an_untimed_waiter_does_not() {
    let napping = lone_and_eager_agree("sleep", "1000");
    let waiting = lone_run("wait", "1000", false)
        .stopless
        .expect("the skipping run reports a count");
    assert!(
        napping + 300 < waiting,
        "ends taken without a stop: {napping} beside a napping worker, {waiting} beside a waiting one"
    );
}

/// Ends skipped before a trapped instruction (rdtsc) are processed before
/// Detcore handles the instruction.
#[test]
fn skipped_slice_ends_come_before_a_trapped_instruction() {
    let skipped = lone_and_eager_agree("rdtsc", "1000");
    assert!(skipped > 0, "no slice end was taken without a stop");
}

/// Ends skipped before a trapped cpuid are processed before Detcore handles
/// the instruction (`handle_cpuid_event`, a path apart from rdtsc's).
#[test]
fn skipped_slice_ends_come_before_a_cpuid() {
    let skipped = lone_and_eager_agree("cpuid", "1000");
    assert!(skipped > 0, "no slice end was taken without a stop");
}

/// Ends skipped before a signal (a SIGSEGV the guest handles) are processed
/// before Detcore handles the signal.
#[test]
fn skipped_slice_ends_come_before_a_signal() {
    let skipped = lone_and_eager_agree("segv", "1000");
    assert!(skipped > 0, "no slice end was taken without a stop");
}

/// A lone stretch longer than the backstop timer of a thread running alone
/// (2M branches; at 1000000 ns the stretch is about 40 slice ends, 37 of them
/// taken without a stop, and the backstop fires once) leaves the run
/// unchanged. Its eager run costs about 3.4 us per branch (8 CPU seconds,
/// measured 2026-10-27 on an AMD EPYC host) and can take a skid-overshoot
/// retry (https://github.com/rrnewton/hermit/issues/1845), so the stretch is
/// kept short, and apart from the exec case, for one retry to fit the
/// per-test budgets.
#[test]
fn skipped_slice_ends_survive_the_backstop() {
    let skipped = lone_and_eager_agree("long", "1000000");
    assert!(skipped > 20, "long: {skipped} ends taken without a stop");
    // The stretch must cross the backstop, or the case tests nothing: a
    // compiler that hoisted the loop's trap check would halve its branches
    // (measured: 19136 to 2519268 with GCC -O2, the backstop at 2019216).
    // Detcore's own debug line for a backstop timer event counts them on any
    // backend that delivers timer events, where a backend's log line would
    // count them on that backend only.
    let guest = fixture(&LONE_SPIN, "lone_spin_then_race");
    let (output, rendered) = strict_output_env(
        guest,
        &["long"],
        &["--log=info"],
        &["--chaos", "--seed=3", "--max-timeslice=1000000"],
        &[("RUST_LOG", "detcore::lone_backstop=debug")],
    );
    let (_, stderr) = checked(output, &rendered);
    let backstops = stderr
        .lines()
        .filter(|line| line.contains("lone backstop timer event"))
        .count();
    assert_eq!(backstops, 1, "long: backstop events");
}

/// An exec right after a lone stretch leaves the run unchanged.
#[test]
fn skipped_slice_ends_survive_exec() {
    let skipped = lone_and_eager_agree("exec", "1000");
    assert!(skipped > 0, "exec: no slice end was taken without a stop");
}

/// Record `mode` with `--record-preemptions-to`, skipping lone slice-end
/// stops or not (`eager`); return the run and the parsed record.
fn lone_record(mode: &str, eager: bool) -> (LoneRun, PathBuf, serde_json::Value) {
    let record = lone_dir().join(format!("{mode}-{eager}.preemptions.json"));
    let _ = fs::remove_file(&record);
    let run = lone_run_with(
        mode,
        "1000",
        eager,
        "record",
        &[format!("--record-preemptions-to={}", record.display())],
    );
    let text = fs::read_to_string(&record)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", record.display()));
    let json = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("{} is not JSON: {error}", record.display()));
    (run, record, json)
}

/// The global events of a parsed record.
fn global_events(record: &serde_json::Value) -> Vec<detcore_model::schedule::SchedEvent> {
    serde_json::from_value(record["global"].clone()).expect("the record's global events parse")
}

/// A recording keeps a lone thread's slice ends without a stop. Each such end
/// is the same event a stop records, with no RIP and marked `stopless`; the
/// per-thread preemption rows and callback checkpoints are identical; and
/// the run prints and logs what a recording with a stop at every end does.
#[test]
fn a_recording_keeps_lone_slice_ends_and_marks_them() {
    let (lazy, _, lazy_record) = lone_record("wait", false);
    let (eager, _, eager_record) = lone_record("wait", true);
    assert_eq!(lazy.stdout, eager.stdout, "outputs differ");
    assert_eq!(lazy.info, eager.info, "INFO logs differ");
    let skipped = lazy
        .stopless
        .expect("the skipping recording reports a count");
    assert!(skipped > 300, "{skipped} ends taken without a stop");
    for field in ["per_thread", "callback_checkpoints"] {
        assert_eq!(lazy_record[field], eager_record[field], "{field} differs");
    }
    let lazy_events = global_events(&lazy_record);
    let eager_events = global_events(&eager_record);
    assert_eq!(lazy_events.len(), eager_events.len(), "event counts differ");
    let marked: Vec<_> = lazy_events.iter().filter(|event| event.stopless).collect();
    assert_eq!(
        marked.len() as u64,
        skipped,
        "one marked event per end taken without a stop"
    );
    assert!(marked.iter().all(|event| event.end_rip.is_none()));
    assert!(!eager_events.iter().any(|event| event.stopless));
    for (index, (lazy, eager)) in lazy_events.iter().zip(&eager_events).enumerate() {
        assert!(
            lazy.same_operations(eager),
            "event {index} differs: {lazy} against {eager}"
        );
        if lazy.stopless {
            assert!(
                eager.end_rip.is_some(),
                "event {index}: the stop read no RIP"
            );
        } else {
            assert_eq!(lazy, eager, "event {index}");
        }
    }
}

/// Both replays of a recording that kept lone slice ends take those ends
/// without a stop where the thread runs alone again, and leave the replayed
/// run as a stop at every end does: the same output, with its virtual
/// CLOCK_MONOTONIC reading, the same INFO log and the same desynchronized
/// events, so a schedule replay accepts a stop at an end its recording took
/// without one. A schedule replay of a recording that stopped at every end
/// takes none without a stop, since it checks every RIP that recording read;
/// a preemption replay of it takes them, since it checks none.
#[test]
fn replays_take_recorded_lone_slice_ends_the_same_way() {
    let (_, marked, marked_record) = lone_record("wait", false);
    let (_, unmarked, _) = lone_record("wait", true);
    let marked_ends = global_events(&marked_record)
        .iter()
        .filter(|event| event.stopless)
        .count();
    assert!(marked_ends > 300, "{marked_ends} marked ends");
    for option in ["--replay-schedule-from", "--replay-preemptions-from"] {
        let replay = |record: &Path, eager: bool, tag: &str| {
            let tag = format!("{}-{tag}", option.trim_start_matches("--"));
            lone_run_with(
                "wait",
                "1000",
                eager,
                &tag,
                &[format!("{option}={}", record.display())],
            )
        };
        let lazy = replay(marked.as_path(), false, "marked");
        let eager = replay(marked.as_path(), true, "marked-eager");
        assert_eq!(lazy.stdout, eager.stdout, "{option}: outputs differ");
        assert_eq!(lazy.info, eager.info, "{option}: INFO logs differ");
        assert_eq!(lazy.desynced, eager.desynced, "{option}: desyncs differ");
        // How many depends on how much of the recording the replay follows,
        // which the replay fixes outside this change decide.
        let skipped = lazy.stopless.expect("the replay reports a count");
        assert!(skipped > 0, "{option}: no marked end taken without a stop");
        assert_eq!(eager.stopless, None, "{option}");
        // Its output is not compared with the marked record's replay: how
        // closely a schedule replay follows a record without marks depends
        // on replay fixes outside this change.
        let all_stops = replay(unmarked.as_path(), false, "unmarked");
        let skipped = all_stops.stopless.expect("the replay reports a count");
        if option == "--replay-schedule-from" {
            assert_eq!(skipped, 0, "{option}: a recorded stop was skipped");
        } else {
            assert!(skipped > 0, "{option}: no end taken without a stop");
        }
    }
}
