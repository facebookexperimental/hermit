/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Regression coverage for signals interrupting a blocked futex wait
//! (https://github.com/rrnewton/hermit/issues/3146).
//!
//! Linux interrupts a blocking `FUTEX_WAIT` when a signal with a handler is
//! delivered. An untimed wait restarts under `SA_RESTART`; a timed wait
//! returns `EINTR` even under `SA_RESTART`. Each case below runs the guest in
//! `tests/c/external_signal_interrupt.c` under the ptrace backend and asserts
//! the guest's single deterministic `RESULT` line.
//!
//! A signal that is ignored, blocked, or ignored by default must not end a
//! wait: `poll`, `epoll_wait`, `select`, and a timed futex wait then run to
//! their 300 ms deadline.
//!
//! Nor does a SIGTSTP, SIGTTIN, or SIGTTOU left at its default action end a
//! `poll` with a timeout or a timed `FUTEX_WAIT`, because Detcore restarts those
//! two calls with their relative timeout re-armed. In an orphaned process group
//! Linux discards such a signal and restarts the call with its original
//! deadline, so these calls still return at 300 ms, and the other waits restart
//! with their deadline kept or, for `epoll_wait`, return EINTR.
//!
//! Signal dispositions belong to the whole process, so a sibling can change
//! one while a waiter is parked. The disposition that counts is the one in
//! force when the signal arrives, not the one when the wait began: a signal
//! caught after the wait parked ends it near the 100 ms send, and one ignored
//! after the wait parked leaves it running to its original deadline.
//!
//! A gated wait (a futex wait, `poll` or `epoll_wait`) holds a caught SIGCHLD
//! until the call returns, and the handler runs then, unless the scheduler
//! sent that SIGCHLD to the waiting thread at a commit point: the SIGCHLD of a
//! child that calls `exit_group`, which its child-exit timer sends, ends the
//! wait with EINTR, as on Linux (https://github.com/rrnewton/hermit/issues/4005).
//! The kernel also posts SIGCHLD at host-timed moments, so any other SIGCHLD is
//! held; this gives up that part of https://github.com/rrnewton/hermit/issues/3146
//! until Reverie reports every delivery it holds. The `held` cells assert both:
//! the exit of a child that calls `exit_group` ends the wait near 100 ms, while
//! a child that dies by SIGKILL or by its only thread calling `exit` has no
//! child-exit timer and leaves the wait running to its 300 ms deadline or its
//! wakeup, with the handler run by the `RESULT` line. `select` and the `select`
//! system call are not gated, so a caught SIGCHLD still ends them with EINTR,
//! as on Linux.
//!
//! A child's SIGCHLD is queued on its parent process and taken by one thread
//! that does not block it; Linux offers it first to the thread that forked the
//! child. The role cells give the waiter a sibling that does not block SIGCHLD
//! and check both the waiter's result and which thread's handler ran. A
//! runnable sibling that forks the child takes the signal, and the waiter keeps
//! waiting to its original deadline or its wakeup. A sibling that forks the
//! child and then parks in a wait of its own takes it: for a child that calls
//! `exit_group` its wait ends with EINTR
//! (https://github.com/rrnewton/hermit/issues/4005); for one killed by SIGKILL
//! or leaving through `exit`, the wait holds it until it returns.
//! A waiting main thread that forks the child while a sibling spins does not
//! take it: the sibling does, and the sibling's wake ends the wait, or, for a
//! timed wait the sibling does not wake, the wait runs to its deadline.
//!
//! Every cell with only Hermit-internal senders runs under strict verification
//! (`--verify --verify-strict`) and requires a matched strict report.
//!
//! The watchdog lives in this host process, as in
//! `waitid_signal_interrupt.rs`: a dedicated thread drains Hermit's stderr
//! while this thread polls the process, the guest's stdout file, and an
//! independent wall-clock deadline. A lost interruption is a hang, so each
//! run must print `RESULT` within `RESULT_BOUND` of `READY`.
//!
//! External cells send `SIGUSR1` from this host process, which is outside
//! Hermit's deterministic schedule; the `usr2` cells add `SIGUSR2`, and the
//! external-SIGCHLD cells send a caught `SIGCHLD` instead, to a guest that never
//! had a child, reaped one, or has a live one. Linux ends `select` with EINTR
//! for it in all three. Every such trial runs the guest through a symlink in a
//! fresh directory, so the guest is found by its exact `argv[0]`.
//!
//! Not covered here:
//! - An external signal to a precise-mode futex waiter: with no in-guest waker
//!   Hermit's deadlock detector ends the run before the signal can arrive.
//! - External signals to blocked `wait4`/`waitid`. Those still do not observe
//!   a host-queued signal; that is a separate defect in the same issue.
//! - A caught SIGCHLD sent from outside Hermit to a gated wait. It is held until
//!   the call returns, like every SIGCHLD (above), where Linux returns EINTR at
//!   once; Hermit main did not end `poll` or `epoll_wait` for it either. Only
//!   the select waits, which are not gated, are asserted above.
//! - The `spin` role in polling mode. Between probes a polling waiter blocks
//!   every signal, so the running sibling takes the SIGCHLD that Linux gives the
//!   waiting thread that forked the child.
//! - LiteInst. LiteInst now runs only in-guest, and the in-guest run refuses
//!   `--verify` and a guest signal handler other than `SIG_DFL` or `SIG_IGN`
//!   (https://github.com/rrnewton/reverie/issues/243). Every cell here needs one
//!   or the other: each cell with only Hermit-internal senders runs under
//!   `--verify`, and each external cell catches the signal it is sent. The
//!   LiteInst cells are restoration targets
//!   (https://github.com/rrnewton/hermit/issues/3745).
//! - A default SIGTSTP, SIGTTIN, or SIGTTOU in a process group that is not
//!   orphaned. Linux stops the process when the signal arrives; Detcore lets a
//!   `poll` with a timeout or a timed `FUTEX_WAIT` run to its end first, and the
//!   process stops when the call returns, as on hermit main.
//! - The terminal's signals other than SIGHUP and SIGWINCH. Once the guest may
//!   have a terminal, Hermit holds every signal a terminal sends until a gated
//!   wait returns: SIGINT, SIGQUIT, and SIGTSTP for the interrupt, quit, and
//!   suspend characters, and SIGTTIN and SIGTTOU for a background read or
//!   write, as well as the SIGHUP and SIGWINCH cells assert. Only the Detcore
//!   unit tests assert those five. Typing an interrupt or quit character also
//!   signals Hermit, which is in the same foreground process group.

#[path = "common/hermit_binary.rs"]
mod hermit_binary;

use std::cell::Cell;
use std::fs;
use std::io::BufRead;
use std::io::BufReader;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::OnceLock;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;

/// Wall-clock bound on one whole cell, from starting Hermit to its exit.
const WATCHDOG_BACKSTOP: Duration = Duration::from_secs(90);
/// Wall-clock bound from starting Hermit to the guest's `READY` line: 20 s, the
/// same margin as `RESULT_BOUND`. Every cell measured on 2026-10-05, startup,
/// both strict-verification runs and exit included, took under 3 s; the slowest
/// test, of eight cells, took 8.1 s.
const STARTUP_BOUND: Duration = Duration::from_secs(20);
/// The per-test wall kill in `.config/nextest.toml` (57 s), before the runner
/// scales it by `HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER`.
const NEXTEST_WALL_KILL: Duration = Duration::from_secs(57);
/// Wall time kept free below that kill for this harness to stop Hermit, reap
/// it, and report which cell and phase ran out of time.
const REPORT_MARGIN: Duration = Duration::from_secs(5);
/// Wall-clock bound from the guest's `READY` line to its `RESULT` line: 20 s,
/// unchanged since this file was added. It catches a lost interruption, which
/// hangs; it is not a timing assertion, which the `ELAPSED` windows below make
/// on Hermit's virtual clock. Every passing run measured on 2026-09-28 finished
/// in under 3 s; the rest of the bound is margin for a loaded host.
const RESULT_BOUND: Duration = Duration::from_secs(20);
/// Wall-clock delay between `READY` and the external signal, so the guest has
/// entered the blocking call. An early signal cannot make a broken Hermit pass:
/// the handler would run first and the wait would then block with no waker.
const EXTERNAL_SIGNAL_DELAY: Duration = Duration::from_millis(500);
const EXTERNAL_TRIALS: usize = 10;
const MAX_DIAGNOSTIC_LINES: usize = 2_000;

const EINTR_FUTEX: &str = "RESULT call=futex ret=-1 errno=EINTR handler=1";
/// Guest calls that wait for fd readiness.
const READINESS_CALLS: [&str; 4] = ["poll", "epoll", "select", "rawselect"];
/// The guest's timeout for a wait that a signal must not end.
const QUIET_TIMEOUT_MS: u64 = 300;
/// Virtual-time slack allowed past that deadline before the wait returns.
/// Every must-not-wake cell measured on 2026-09-29 returned at 300-302 ms. A
/// wait that a signal at 100 ms restarts with a fresh 300 ms timeout returns
/// near 400 ms, so the slack must stay well below 100 ms to catch it.
const QUIET_OVERSHOOT_MS: u64 = 50;
/// When the guest's in-Hermit senders send their signal, after the wait began.
const SIGNAL_DELAY_MS: u64 = 100;
/// Slack past `SIGNAL_DELAY_MS` for a wait the signal must end. Natively these
/// waits return at 100-101 ms (measured 2026-09-29). A wait the signal did not
/// end returns at its 300 ms timeout or later, far outside this window.
const WAKE_SLACK_MS: u64 = 100;
/// Lower bound on a wake by a child's exit (`exit`), by the `tstp` cells'
/// sender, and by the `thread` and `process` senders of the restart cells. The
/// guest takes its start stamp before it starts any sender, so each sender's
/// 100 ms sleep begins after the stamp. Until round 7 of
/// https://github.com/rrnewton/hermit/pull/3361 the stamp came after the forks,
/// the thread creation, `READY` and the epoll setup, these wakes read about
/// 1 ms short of 100 ms (99 ms measured on 2026-09-29 and 2026-10-03), and this
/// floor was 90 ms.
const EXIT_WAKE_FLOOR_MS: u64 = 100;
/// Strict-verified repetitions of each child-exit SIGCHLD cell. The kernel also
/// posts its own SIGCHLD for the exit at a host-timed moment, so a single
/// matched pair of runs is weak evidence that the result ignores it.
const SIGCHLD_TRIALS: usize = 3;
/// A role cell's SIGCHLD handler ran once, on the sibling.
const HANDLED_BY_SIBLING: &str = "HANDLER main=0 sibling=1";
/// When the steal role's untimed wait ends: the sibling forks at 100 ms and
/// wakes the futex 100 ms after the handler ran. Measured at 202-205 ms on
/// 2026-09-29, on both backends and in both futex modes.
const STEAL_WAKE_MS: u64 = 2 * SIGNAL_DELAY_MS;
/// Upper bound on the spin role's wake. Natively the wait ends at the child's
/// death, about 100 ms in. Under Hermit the spinning sibling takes the SIGCHLD
/// and then wakes the futex: the wait ended at 456-457 ms for the `spin` and
/// `spinthrexit` deaths and at 254 ms for `spinkill` on 2026-10-05. Before
/// round 9 of https://github.com/rrnewton/hermit/pull/3361, when the SIGCHLD
/// ended the main thread's wait with EINTR, it ended at 450-652 ms on
/// 2026-09-29 and at 454-656 ms on 2026-10-05.
const SPIN_WAKE_BOUND_MS: u64 = 1_000;
/// The guest's timeout for a `timed` futex wait outside the must-not-wake
/// options: 10 s, far beyond every signal and wake in the cell.
const LONG_TIMEOUT_MS: u64 = 10_000;
/// When a wait that the signal interrupted near 100 ms and that Linux then
/// restarted ends: 100 ms after the signal, when the `thread` sender wakes the
/// futex or the `process` sender's child exits. A wait that returned at the
/// signal instead reads near 100 ms, below this window.
const RESTARTED_WAKE_MS: std::ops::Range<u64> =
    SIGNAL_DELAY_MS + EXIT_WAKE_FLOOR_MS..2 * SIGNAL_DELAY_MS + WAKE_SLACK_MS;
/// Upper bound on when the handler of a signal that interrupted a child wait
/// first ran: midway between the child's signal near 100 ms and its exit near
/// 200 ms. A handler that ran only after the wait returned reads near 200 ms.
const HANDLED_BEFORE_EXIT_MS: u64 = SIGNAL_DELAY_MS + SIGNAL_DELAY_MS / 2;
/// Trials in the guest's `sigsuspend creator` mode (`CREATOR_TRIALS` in the
/// guest).
const CREATOR_TRIALS: usize = 6;
/// Trials in the guest's `poll pdeathchld`, `poll pdeathusr1`, `poll exitusr1`,
/// `poll hupopen` and `poll hupctty` modes (`PDEATH_TRIALS` in the guest).
const PDEATH_TRIALS: usize = 3;
/// Unverified strict runs of a parent-death cell; each must match.
const PDEATH_RUNS: usize = 3;
/// Trials in the guest's `racing` mode (`RACING_TRIALS` in the guest).
const RACING_TRIALS: usize = 6;
/// Part of the INFO line Hermit's scheduler logs when a signal is delivered to a
/// thread that sleeps outside its run queue, here the creator in
/// `rt_sigsuspend` (`[dtid N] signal S armed signaled background thread`, in
/// detcore/src/scheduler.rs).
const SIGNALED_BACKGROUND_ARMED: &str = "armed signaled background thread";
/// Part of the INFO line it logs when it later orders that thread's
/// continuation at a fixed point of the schedule (`[step2] Reschedule signaled
/// background dtid`).
const SIGNALED_BACKGROUND_RELEASED: &str = "Reschedule signaled background dtid";

static GUEST: OnceLock<PathBuf> = OnceLock::new();

#[derive(Clone, Copy, Debug)]
enum FutexMode {
    Precise,
    Polling,
}

struct GuestRun {
    status: ExitStatus,
    stdout: String,
    stderr: String,
    /// The strict verification report, for a cell run under `--verify`.
    verify_report: Option<serde_json::Value>,
    /// Hermit's INFO log, for a cell run with `Observe::InfoLog`.
    info_log: Option<String>,
}

/// How a cell is observed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Observe {
    /// Strict verification for a cell with no external signal; for a cell with
    /// an external signal, one plain run.
    Default,
    /// One run, not verified, that writes Hermit's INFO log to a file and keeps
    /// it in `GuestRun::info_log`. A run under `--verify` does not print its INFO
    /// messages, so a test that counts a scheduler INFO line needs this run as
    /// well as the strict-verified one.
    InfoLog,
}

impl GuestRun {
    fn result_line(&self) -> Option<&str> {
        self.stdout.lines().find(|line| line.starts_with("RESULT "))
    }

    fn describe(&self) -> String {
        format!(
            "status={:?}\nverify report: {:?}\nguest stdout:\n{}\nhermit stderr:\n{}",
            self.status, self.verify_report, self.stdout, self.stderr
        )
    }

    /// The `ELAPSED ms=` value a must-not-wake, disposition-change, or
    /// child-exit cell prints.
    fn elapsed_ms(&self) -> Option<u64> {
        self.stdout
            .lines()
            .find_map(|line| line.strip_prefix("ELAPSED ms="))
            .and_then(|value| value.parse().ok())
    }

    /// The first guest stdout line that starts with `prefix`.
    fn line(&self, prefix: &str) -> Option<&str> {
        self.stdout.lines().find(|line| line.starts_with(prefix))
    }

    /// The `ms=` value of the fork role's `SIBLING` line.
    fn sibling_ms(&self) -> Option<u64> {
        self.line("SIBLING ")
            .and_then(|line| line.rsplit_once(" ms="))
            .and_then(|(_, value)| value.parse().ok())
    }

    /// The `HANDLED_AT ms=` value a `restart` child-wait cell prints: when the
    /// handler first ran, after the wait began, or -1 if it never ran.
    fn handled_at_ms(&self) -> Option<i64> {
        self.stdout
            .lines()
            .find_map(|line| line.strip_prefix("HANDLED_AT ms="))
            .and_then(|value| value.parse().ok())
    }
}

fn build_root() -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join("external-signal-interrupt")
}

/// The guest binary, compiled on first use within the test's wall budget, which
/// starts here if no cell has started it (`test_wall_deadline`): a stalled
/// compiler fails with this harness's report instead of reaching the runner's
/// kill. Moving the clock alone cannot bound blocking compilation, so the
/// compiler is a supervised child that is killed and reaped when the budget runs
/// out. Under Cargo's harness, where tests share the process, a test that finds
/// another test compiling waits for that compilation, which its own test's
/// budget bounds.
fn guest() -> &'static Path {
    let deadline = test_wall_deadline();
    GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let root = build_root();
        fs::create_dir_all(&root).expect("failed to create guest build directory");
        // The guest must not live under /tmp: Hermit replaces guest /tmp with
        // an isolated directory, making a host /tmp fixture invisible.
        let guest = root.join("external_signal_interrupt");
        let source = repository.join("tests/c/external_signal_interrupt.c");
        // Nextest runs every test in a process of its own, so each test
        // compiles the guest. Each compiles into a file of its own and renames
        // it into place: rename(2) replaces the name atomically, so no test
        // executes a binary that another test's compiler is still writing, and
        // a guest that is already running keeps the copy it started from.
        let partial = root.join(format!(
            "external_signal_interrupt.{}.partial",
            std::process::id()
        ));
        // The compiler's diagnostics go to a file, not a pipe, so a compiler
        // that writes more than a pipe holds never blocks while it is polled.
        let diagnostics_path = root.join(format!(
            "external_signal_interrupt.{}.stderr",
            std::process::id()
        ));
        let diagnostics =
            fs::File::create(&diagnostics_path).expect("failed to create the compiler's stderr");
        let mut compiler = Command::new("cc")
            .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror", "-pthread"])
            .arg(&source)
            .arg("-o")
            .arg(&partial)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(diagnostics))
            .spawn()
            .unwrap_or_else(|error| panic!("failed to start the guest's compiler: {error}"));
        let status = loop {
            match compiler.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Ok(None) => {
                    let _ = compiler.kill();
                    let reaped = compiler.wait();
                    panic!(
                        "compiling {} did not finish within the test's {:.1}s wall budget; \
                         the compiler was killed and reaped ({reaped:?})",
                        source.display(),
                        test_wall_budget().as_secs_f64(),
                    );
                }
                Err(error) => panic!("failed to wait for the guest's compiler: {error}"),
            }
        };
        assert!(
            status.success(),
            "failed to compile {} ({status})\nstderr:\n{}",
            source.display(),
            fs::read_to_string(&diagnostics_path).unwrap_or_default(),
        );
        let _ = fs::remove_file(&diagnostics_path);
        fs::rename(&partial, &guest).unwrap_or_else(|error| {
            panic!(
                "failed to move the guest from {} to {}: {error}",
                partial.display(),
                guest.display()
            )
        });
        guest
    })
}

fn kill_process_group(child: &mut std::process::Child) {
    // The group contains Hermit and every guest process it started.
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    let _ = child.kill();
}

/// Every host pid whose `argv[0]` is exactly `argv0`.
fn pids_with_argv0(argv0: &Path) -> Vec<libc::pid_t> {
    let want = argv0.as_os_str().as_encoded_bytes();
    let mut pids = Vec::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return pids;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<libc::pid_t>().ok())
        else {
            continue;
        };
        if let Ok(cmdline) = fs::read(entry.path().join("cmdline"))
            && cmdline.split(|byte| *byte == 0).next() == Some(want)
        {
            pids.push(pid);
        }
    }
    pids
}

enum StderrEvent {
    Line(String),
    Error(String),
    Eof,
}

/// The runner's wall-clock timeout multiplier, which also scales its per-test
/// wall kill, read the way `ci/manifest-plan/src/timeouts.rs` reads it: unset is
/// 1, and anything else must be a finite number greater than zero.
fn wall_timeout_multiplier() -> f64 {
    const NAME: &str = "HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER";
    match std::env::var(NAME) {
        Err(std::env::VarError::NotPresent) => 1.0,
        Ok(text) => match text.parse::<f64>() {
            Ok(value) if value.is_finite() && value > 0.0 => value,
            _ => panic!("{NAME} must be a finite number greater than zero, got {text:?}"),
        },
        Err(std::env::VarError::NotUnicode(_)) => panic!("{NAME} must be valid UTF-8"),
    }
}

/// One test's wall budget for all of its cells: the runner's scaled per-test
/// wall kill, less `REPORT_MARGIN`. A test that runs out of it fails with this
/// harness's own report instead of being killed by the runner without one.
fn test_wall_budget() -> Duration {
    NEXTEST_WALL_KILL
        .mul_f64(wall_timeout_multiplier())
        .saturating_sub(REPORT_MARGIN)
}

thread_local! {
    /// When this test began (`test_wall_deadline`), and how many cells it has
    /// started.
    /// Every test runs its cells on its own thread, under Cargo's harness and
    /// under nextest alike.
    static TEST_CELLS: Cell<Option<(Instant, usize)>> = const { Cell::new(None) };
}

/// The running test's wall deadline: `test_wall_budget` after the test began,
/// which is its first guest setup or its first cell, whichever came first.
fn test_wall_deadline() -> Instant {
    let started = TEST_CELLS.with(|cells| {
        let (started, count) = cells.get().unwrap_or((Instant::now(), 0));
        cells.set(Some((started, count)));
        started
    });
    started + test_wall_budget()
}

/// Count a new cell of the running test and print a progress line for it, so
/// that a test the runner stops still names the cell it was in. Returns the
/// cell's 1-based number and the test's wall deadline.
fn begin_cell(describe: &str) -> (usize, Instant) {
    let budget = test_wall_budget();
    let (started, index) = TEST_CELLS.with(|cells| {
        let (started, count) = cells.get().unwrap_or((Instant::now(), 0));
        cells.set(Some((started, count + 1)));
        (started, count + 1)
    });
    eprintln!(
        "[esi] cell {index}: {describe}; {:.1}s of the test's {:.1}s wall budget used",
        started.elapsed().as_secs_f64(),
        budget.as_secs_f64(),
    );
    (index, started + budget)
}

/// The deadline of one phase of a cell that began at `start`: `bound` later,
/// but never past `test_deadline`. Also says which of the two applies.
fn phase_deadline(
    start: Instant,
    bound: Duration,
    test_deadline: Instant,
) -> (Instant, &'static str) {
    let own = start + bound;
    if own <= test_deadline {
        (own, "its phase bound")
    } else {
        (test_deadline, "the test's wall budget")
    }
}

/// Each phase of a cell ends by the test's wall deadline, startup included, and
/// that deadline comes before the runner's scaled per-test wall kill, so a hung
/// cell fails with this harness's report rather than the runner's kill.
#[test]
fn every_cell_phase_ends_by_the_tests_wall_deadline() {
    let start = Instant::now();
    let soon = start + Duration::from_secs(5);
    for bound in [STARTUP_BOUND, RESULT_BOUND, WATCHDOG_BACKSTOP] {
        assert_eq!(
            phase_deadline(start, bound, soon),
            (soon, "the test's wall budget"),
            "a {bound:?} phase must end at a test deadline 5 s away"
        );
        let late = start + bound + Duration::from_secs(1);
        assert_eq!(
            phase_deadline(start, bound, late),
            (start + bound, "its phase bound"),
            "a {bound:?} phase must keep its own bound when the test has time left"
        );
    }
    let budget = test_wall_budget();
    assert!(budget < NEXTEST_WALL_KILL.mul_f64(wall_timeout_multiplier()));
    assert!(
        STARTUP_BOUND < budget && RESULT_BOUND < budget,
        "startup ({STARTUP_BOUND:?}) and the READY-to-RESULT phase ({RESULT_BOUND:?}) must \
         each fit in one test's {budget:?} wall budget"
    );
}

/// Run one cell. With `external`, send `SIGUSR1` to the guest process from
/// this host process `EXTERNAL_SIGNAL_DELAY` after it prints `READY`.
fn run_cell(backend: &str, mode: FutexMode, args: &[&str], external: bool) -> GuestRun {
    let signals: &[libc::c_int] = if external { &[libc::SIGUSR1] } else { &[] };
    run_cell_with_external_signals(backend, mode, args, signals)
}

/// Run one cell, sending each of `external_signals`, in order and back to back,
/// to the guest process from this host process `EXTERNAL_SIGNAL_DELAY` after it
/// prints `READY`. A cell with no external signal runs under strict
/// verification instead.
fn run_cell_with_external_signals(
    backend: &str,
    mode: FutexMode,
    args: &[&str],
    external_signals: &[libc::c_int],
) -> GuestRun {
    run_cell_observed(backend, mode, args, external_signals, Observe::Default)
}

/// Run one cell as `run_cell_with_external_signals` does, observed as `observe`
/// says.
/// The arguments every cell passes to `hermit` after its global options.
/// `--backend` is a global option and goes before the subcommand. The guest
/// gets a minimal environment instead of the test's: under Cargo and Nextest,
/// LD_LIBRARY_PATH names the build's deps directory, which concurrent builds
/// write to while a cell runs, so the guest's dynamic loader stat'ed a
/// directory whose size could change between `--verify`'s two runs, and the
/// cell failed bitwise parity on a host input, not on Hermit.
fn cell_run_args(backend: &str) -> [&str; 5] {
    [
        "--backend",
        backend,
        "run",
        "--strict",
        "--base-env=minimal",
    ]
}

/// The guest of every cell runs without the test's environment, so a
/// directory on the test's LD_LIBRARY_PATH never reaches its dynamic loader
/// (`cell_run_args`).
#[test]
fn cells_give_the_guest_a_minimal_environment() {
    const MARKER: &str = "/hermit-esi-ld-library-path-marker";
    let output = Command::new(hermit_binary::hermit_binary())
        .args(cell_run_args("ptrace"))
        .arg("--")
        .arg("/usr/bin/env")
        .env("LD_LIBRARY_PATH", MARKER)
        .stdin(Stdio::null())
        .output()
        .expect("failed to run hermit");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "hermit env failed: {}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.lines().any(|line| line.starts_with("PATH=")),
        "the minimal environment sets PATH:\n{stdout}"
    );
    assert!(
        !stdout.contains(MARKER),
        "the guest inherited the test's LD_LIBRARY_PATH:\n{stdout}"
    );
}

fn run_cell_observed(
    backend: &str,
    mode: FutexMode,
    args: &[&str],
    external_signals: &[libc::c_int],
    observe: Observe,
) -> GuestRun {
    let external = !external_signals.is_empty();
    // Compiling the guest also creates the directory that holds every trial.
    let guest = guest();
    let trial = tempfile::Builder::new()
        .prefix("trial-")
        .tempdir_in(build_root())
        .expect("failed to create trial directory");
    let program = trial.path().join("esi");
    std::os::unix::fs::symlink(guest, &program).expect("failed to link the guest");
    let stdout_path = trial.path().join("stdout");
    let stdout_writer = fs::File::create(&stdout_path).expect("failed to create guest stdout");

    // Strict verification replays the guest, so it needs a sender inside Hermit.
    let verify_report =
        (!external && observe == Observe::Default).then(|| trial.path().join("verify.json"));
    let info_log = (observe == Observe::InfoLog).then(|| trial.path().join("info.log"));
    let mut command = Command::new(hermit_binary::hermit_binary());
    // Not cargo's `LD_LIBRARY_PATH`, under which the guest's dynamic loader
    // stats build directories that other validation nodes are writing, so the
    // two runs of a `--verify` pair could see different directory sizes
    // (https://github.com/rrnewton/hermit/issues/3846).
    command.env_remove("LD_LIBRARY_PATH");
    if verify_report.is_some() {
        command.arg("--log=info");
    }
    if let Some(log) = &info_log {
        command.arg("--log=info").arg("--log-file").arg(log);
    }
    command.args(cell_run_args(backend));
    if let FutexMode::Polling = mode {
        command.arg("--debug-futex-mode=polling");
    }
    if let Some(report) = &verify_report {
        command
            .args(["--verify", "--verify-strict", "--verify-json"])
            .arg(report);
    }
    command
        .arg("--")
        .arg(&program)
        .args(args)
        .process_group(0)
        .stdout(Stdio::from(stdout_writer))
        .stderr(Stdio::piped());

    let (cell, test_deadline) = begin_cell(&format!(
        "{backend} {mode:?} {args:?} external_signals={external_signals:?} observe={observe:?}"
    ));
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("failed to spawn hermit: {error}"));
    let stderr = child.stderr.take().expect("stderr was piped");
    let (send, receive) = mpsc::channel();
    let reader = thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            match line {
                Ok(line) => {
                    if send.send(StderrEvent::Line(line)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let _ = send.send(StderrEvent::Error(error.to_string()));
                    return;
                }
            }
        }
        let _ = send.send(StderrEvent::Eof);
    });

    let started = Instant::now();
    let (startup_deadline, startup_limit) = phase_deadline(started, STARTUP_BOUND, test_deadline);
    let (cell_deadline, cell_limit) = phase_deadline(started, WATCHDOG_BACKSTOP, test_deadline);
    let mut lines = Vec::new();
    let mut truncated = false;
    let mut stderr_eof = false;
    let mut status = None;
    let mut ready_at = None;
    let mut signalled = !external;
    let mut failure = None;

    while status.is_none() || !stderr_eof {
        if status.is_none() {
            status = child.try_wait().expect("failed to poll hermit");
        }
        let stdout = fs::read_to_string(&stdout_path).unwrap_or_default();
        if ready_at.is_none() && stdout.lines().any(|line| line == "READY") {
            ready_at = Some(Instant::now());
        }
        let has_result = stdout.lines().any(|line| line.starts_with("RESULT "));
        let now = Instant::now();
        if failure.is_none() && ready_at.is_none() && status.is_none() && now >= startup_deadline {
            failure = Some(format!(
                "cell {cell}: no READY line {:.1}s after starting Hermit, at {startup_limit}",
                (now - started).as_secs_f64()
            ));
        }
        if let Some(ready) = ready_at {
            if !signalled && ready.elapsed() >= EXTERNAL_SIGNAL_DELAY {
                let pids = pids_with_argv0(&program);
                if let [pid] = pids[..] {
                    for &signal in external_signals {
                        // SAFETY: plain kill(2) on a pid read from /proc.
                        if unsafe { libc::kill(pid, signal) } != 0 {
                            failure = Some(format!(
                                "kill({pid}, {signal}) failed: {}",
                                std::io::Error::last_os_error()
                            ));
                            break;
                        }
                    }
                } else {
                    failure = Some(format!(
                        "expected exactly one guest process with argv[0] {}, found {pids:?}",
                        program.display()
                    ));
                }
                signalled = true;
            }
            let (result_deadline, result_limit) =
                phase_deadline(ready, RESULT_BOUND, test_deadline);
            if failure.is_none() && !has_result && now >= result_deadline {
                failure = Some(format!(
                    "cell {cell}: no RESULT line {:.1}s after READY, at {result_limit} \
                     (external={external}): the blocked call was not interrupted",
                    (now - ready).as_secs_f64()
                ));
            }
        }
        if failure.is_none() && now >= cell_deadline {
            failure = Some(format!(
                "cell {cell}: watchdog deadline {:.1}s after starting Hermit, at {cell_limit}; \
                 process_exited={}, ready={}",
                (now - started).as_secs_f64(),
                status.is_some(),
                ready_at.is_some(),
            ));
        }
        if failure.is_some() {
            break;
        }
        match receive.recv_timeout(Duration::from_millis(10)) {
            Ok(StderrEvent::Line(line)) => {
                if lines.len() < MAX_DIAGNOSTIC_LINES {
                    lines.push(line);
                } else {
                    truncated = true;
                }
            }
            Ok(StderrEvent::Error(error)) => {
                failure = Some(format!("failed while draining hermit stderr: {error}"));
                break;
            }
            Ok(StderrEvent::Eof) | Err(mpsc::RecvTimeoutError::Disconnected) => stderr_eof = true,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }

    if failure.is_some() {
        kill_process_group(&mut child);
    }
    // Always reap the process here, on timeout, normal exit, and failure.
    let waited_status = child.wait().expect("failed to wait for hermit");
    status.get_or_insert(waited_status);
    reader.join().expect("stderr reader panicked");
    let stdout = fs::read_to_string(&stdout_path).expect("failed to read guest stdout");
    let mut stderr = lines.join("\n");
    if truncated {
        stderr.push_str(&format!(
            "\n[watchdog retained the first {MAX_DIAGNOSTIC_LINES} stderr lines]"
        ));
    }
    if let Some(reason) = failure {
        panic!(
            "{backend} {mode:?} {args:?}: {reason}\nguest stdout:\n{stdout}\nhermit stderr:\n{stderr}"
        );
    }
    let verify_report = verify_report.map(|report| {
        let bytes = fs::read(&report).unwrap_or_else(|error| {
            panic!(
                "{backend} {mode:?} {args:?}: strict verification did not publish its report: \
                 {error}\nguest stdout:\n{stdout}\nhermit stderr:\n{stderr}"
            )
        });
        serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            panic!("{backend} {mode:?} {args:?}: invalid verification report: {error}")
        })
    });
    let info_log = info_log.map(|log| {
        fs::read_to_string(&log).unwrap_or_else(|error| {
            panic!(
                "{backend} {mode:?} {args:?}: Hermit did not write its INFO log: {error}\nguest \
                 stdout:\n{stdout}\nhermit stderr:\n{stderr}"
            )
        })
    });
    GuestRun {
        status: status.expect("hermit status should be collected"),
        stdout,
        stderr,
        verify_report,
        info_log,
    }
}

/// Why a strict verification report is not determinism evidence, or `None` when
/// it is. The two runs must have matched under the canonical comparison of their
/// INFO logs, the report must certify bitwise parity, and each run must have
/// supplied INFO messages to compare. A match alone is not enough: an empty
/// comparison also matches, and Hermit's verifier then reports
/// `bitwise_parity: false` (`verification_report` in
/// hermit-cli/src/bin/hermit/verify.rs).
fn strict_verification_gap(report: &serde_json::Value) -> Option<String> {
    let mut gaps = Vec::new();
    if report["verdict"] != "matched" {
        gaps.push(format!("verdict is {}, not \"matched\"", report["verdict"]));
    }
    if report["verified"] != true {
        gaps.push(format!("verified is {}, not true", report["verified"]));
    }
    let comparison = &report["comparison"];
    if comparison["strictness"] != "canonical" {
        gaps.push(format!(
            "comparison.strictness is {}, not \"canonical\"",
            comparison["strictness"]
        ));
    }
    if comparison["log_scope"] != "info" {
        gaps.push(format!(
            "comparison.log_scope is {}, not \"info\"",
            comparison["log_scope"]
        ));
    }
    if report["bitwise_parity"] != true {
        gaps.push(format!(
            "bitwise_parity is {}, not true",
            report["bitwise_parity"]
        ));
    }
    for side in ["left", "right"] {
        let count = &report["compared_log_messages"][side];
        if !count.as_u64().is_some_and(|count| count > 0) {
            gaps.push(format!(
                "compared_log_messages.{side} is {count}, not a positive count"
            ));
        }
    }
    (!gaps.is_empty()).then(|| gaps.join("; "))
}

/// Require determinism evidence from a cell that ran under `--verify`: a matched
/// canonical comparison of nonzero INFO messages with bitwise parity
/// (`strict_verification_gap`).
fn assert_verified(backend: &str, mode: FutexMode, args: &[&str], run: &GuestRun) {
    let Some(report) = &run.verify_report else {
        return;
    };
    if let Some(gap) = strict_verification_gap(report) {
        panic!(
            "{backend} {mode:?} {args:?}: strict verification is not determinism evidence: \
             {gap}\n{}",
            run.describe()
        );
    }
}

/// A change that `verification_report_with` makes to a matching report.
type ReportEdit = fn(&mut serde_json::Value);

/// A strict verification report with the fields `strict_verification_gap` reads,
/// as Hermit's verifier writes them for two runs that matched on 412 INFO
/// messages each, after `edit`.
fn verification_report_with(edit: ReportEdit) -> serde_json::Value {
    let mut report = serde_json::json!({
        "verified": true,
        "bitwise_parity": true,
        "verdict": "matched",
        "comparison": {"strictness": "canonical", "log_scope": "info"},
        "compared_log_messages": {"left": 412, "right": 412},
    });
    edit(&mut report);
    report
}

/// `assert_verified` accepts only a matched canonical INFO comparison with
/// bitwise parity and nonzero message counts on both sides. Every other report
/// below is refused, including the empty comparison that also matches.
#[test]
fn strict_verification_requires_bitwise_parity_over_compared_info_messages() {
    assert_eq!(
        strict_verification_gap(&verification_report_with(|_| {})),
        None
    );
    let refused: [(&str, ReportEdit); 10] = [
        ("an empty comparison", |report| {
            report["compared_log_messages"] = serde_json::json!({"left": 0, "right": 0});
        }),
        ("an empty right side", |report| {
            report["compared_log_messages"]["right"] = serde_json::json!(0);
        }),
        ("no message counts", |report| {
            report["compared_log_messages"] = serde_json::Value::Null;
        }),
        ("no bitwise parity", |report| {
            report["bitwise_parity"] = serde_json::json!(false);
        }),
        ("an absent bitwise parity", |report| {
            report
                .as_object_mut()
                .expect("the report is an object")
                .remove("bitwise_parity");
        }),
        ("a diverged verdict", |report| {
            report["verdict"] = serde_json::json!("diverged");
        }),
        ("an unverified report", |report| {
            report["verified"] = serde_json::json!(false);
        }),
        ("a stripped comparison", |report| {
            report["comparison"]["strictness"] = serde_json::json!("stripped");
        }),
        ("a deterministic-log comparison", |report| {
            report["comparison"]["log_scope"] = serde_json::json!("deterministic");
        }),
        ("no comparison", |report| {
            report["comparison"] = serde_json::Value::Null;
        }),
    ];
    for (case, edit) in refused {
        assert!(
            strict_verification_gap(&verification_report_with(edit)).is_some(),
            "{case} was accepted as determinism evidence"
        );
    }
}

/// Run a cell and require a clean exit, the expected `RESULT` line, and `DONE`.
fn assert_cell(backend: &str, mode: FutexMode, args: &[&str], external: bool, expected: &str) {
    let run = run_cell(backend, mode, args, external);
    assert!(
        run.status.success() && run.result_line() == Some(expected),
        "{backend} {mode:?} {args:?}: expected `{expected}`\n{}",
        run.describe()
    );
    assert!(
        run.stdout.lines().any(|line| line == "DONE"),
        "{backend} {mode:?} {args:?}: guest did not finish\n{}",
        run.describe()
    );
    assert_verified(backend, mode, args, &run);
}

/// A signal that must not end the wait: the call returns `expected`, its timeout
/// result or the result of a wakeup the guest schedules at 300 ms, and it took
/// those 300 ms. A must-not-wake cell expects `handler=0`; a `held` cell expects
/// `handler=1`, from the handler that ran once the call returned.
fn assert_quiet_cell(backend: &str, mode: FutexMode, args: &[&str], expected: &str) {
    let run = run_cell(backend, mode, args, false);
    assert!(
        run.status.success()
            && run.result_line() == Some(expected)
            && run.stdout.lines().any(|line| line == "DONE"),
        "{backend} {mode:?} {args:?}: expected `{expected}`\n{}",
        run.describe()
    );
    let elapsed = run.elapsed_ms();
    assert!(
        elapsed.is_some_and(|ms| {
            (QUIET_TIMEOUT_MS..QUIET_TIMEOUT_MS + QUIET_OVERSHOOT_MS).contains(&ms)
        }),
        "{backend} {mode:?} {args:?}: the wait took {elapsed:?} ms, not its \
         {QUIET_TIMEOUT_MS} ms timeout\n{}",
        run.describe()
    );
    assert_verified(backend, mode, args, &run);
}

/// A signal the wait must end: the call returns `expected`, and it returned near
/// the signal, well before any timeout.
fn assert_woken_cell(backend: &str, mode: FutexMode, args: &[&str], expected: &str) {
    let run = run_cell(backend, mode, args, false);
    assert!(
        run.status.success()
            && run.result_line() == Some(expected)
            && run.stdout.lines().any(|line| line == "DONE"),
        "{backend} {mode:?} {args:?}: expected `{expected}`\n{}",
        run.describe()
    );
    let floor = if args.contains(&"exit") || args.contains(&"tstp") {
        EXIT_WAKE_FLOOR_MS
    } else {
        SIGNAL_DELAY_MS
    };
    let elapsed = run.elapsed_ms();
    assert!(
        elapsed.is_some_and(|ms| (floor..SIGNAL_DELAY_MS + WAKE_SLACK_MS).contains(&ms)),
        "{backend} {mode:?} {args:?}: the wait took {elapsed:?} ms, not the \
         {SIGNAL_DELAY_MS} ms until the signal\n{}",
        run.describe()
    );
    assert_verified(backend, mode, args, &run);
}

/// A role cell: the main thread's call returns `expected` within `elapsed` ms,
/// the SIGCHLD handler ran on the thread `handler` names, and the run exits
/// cleanly with `DONE` and a matched strict report.
fn assert_role_cell(
    backend: &str,
    mode: FutexMode,
    args: &[&str],
    expected: &str,
    handler: &str,
    elapsed: std::ops::Range<u64>,
) -> GuestRun {
    let run = run_cell(backend, mode, args, false);
    assert!(
        run.status.success()
            && run.result_line() == Some(expected)
            && run.line("HANDLER ") == Some(handler)
            && run.stdout.lines().any(|line| line == "DONE"),
        "{backend} {mode:?} {args:?}: expected `{expected}` and `{handler}`\n{}",
        run.describe()
    );
    let ms = run.elapsed_ms();
    assert!(
        ms.is_some_and(|ms| elapsed.contains(&ms)),
        "{backend} {mode:?} {args:?}: the wait took {ms:?} ms, outside {elapsed:?}\n{}",
        run.describe()
    );
    assert_verified(backend, mode, args, &run);
    run
}

/// Fix A: a polling-mode futex wait must observe a signal that Hermit did not
/// send. Before the fix the retry loop never looked for one and spun until the
/// watchdog fired. The cell runs `EXTERNAL_TRIALS` trials, split into two
/// tests of half as many each (`trials`) so that each test stays inside the
/// per-test wall and CPU bounds with room to spare.
fn assert_polling_futex_wait_observes_external_signal(
    backend: &str,
    trials: std::ops::Range<usize>,
) {
    for trial in trials {
        let run = run_cell(backend, FutexMode::Polling, &["futex", "external"], true);
        assert!(
            run.status.success() && run.result_line() == Some(EINTR_FUTEX),
            "{backend} trial {trial}/{EXTERNAL_TRIALS}: expected `{EINTR_FUTEX}`\n{}",
            run.describe()
        );
    }
}

#[test]
fn ptrace_polling_futex_wait_is_interrupted_by_an_external_signal() {
    assert_polling_futex_wait_observes_external_signal("ptrace", FIRST_HALF_OF_EXTERNAL_TRIALS);
}

#[test]
fn ptrace_polling_futex_wait_is_interrupted_by_an_external_signal_in_later_trials() {
    assert_polling_futex_wait_observes_external_signal("ptrace", SECOND_HALF_OF_EXTERNAL_TRIALS);
}

/// Fix C: a precise-mode futex waiter woken for a signal must return EINTR, not
/// 0 as if a FUTEX_WAKE had arrived. The thread sender is Hermit-internal
/// (tgkill) and wakes the futex 100 ms later.
#[test]
fn precise_futex_wait_is_interrupted_by_a_sibling_thread_signal() {
    assert_cell(
        "ptrace",
        FutexMode::Precise,
        &["futex", "thread"],
        false,
        EINTR_FUTEX,
    );
}

/// A signal from a guest process that stays alive. Before the fix a parked
/// precise waiter was never woken for it and the run hung.
#[test]
fn precise_futex_wait_is_interrupted_by_a_live_sibling_process_signal() {
    assert_cell(
        "ptrace",
        FutexMode::Precise,
        &["futex", "process"],
        false,
        EINTR_FUTEX,
    );
}

/// SIGALRM from ITIMER_REAL, which Hermit's scheduler delivers itself.
#[test]
fn futex_wait_is_interrupted_by_a_timer_signal() {
    for mode in [FutexMode::Precise, FutexMode::Polling] {
        assert_cell("ptrace", mode, &["futex", "timer"], false, EINTR_FUTEX);
    }
}

/// Internal senders in polling mode: a sibling thread (tgkill) and a live
/// sibling process (kill). Before the fix the thread case returned EAGAIN once
/// the word changed and the process case hung.
#[test]
fn polling_futex_wait_is_interrupted_by_internal_signals() {
    for sender in ["thread", "process"] {
        assert_cell(
            "ptrace",
            FutexMode::Polling,
            &["futex", sender],
            false,
            EINTR_FUTEX,
        );
    }
}

/// Linux returns EINTR from a timed FUTEX_WAIT even under SA_RESTART
/// (ERESTART_RESTARTBLOCK), so the timeout does not silently restart.
#[test]
fn timed_futex_wait_returns_eintr_under_sa_restart() {
    for mode in [FutexMode::Precise, FutexMode::Polling] {
        for sender in ["thread", "process", "timer"] {
            assert_cell(
                "ptrace",
                mode,
                &["futex", sender, "restart", "timed"],
                false,
                EINTR_FUTEX,
            );
        }
    }
}

/// Control: an untimed FUTEX_WAIT restarts under SA_RESTART. The handler runs
/// and the call never reports EINTR. The waker thread sets the word and wakes the
/// futex only after the handler has run, so the wait must have been interrupted.
/// Natively the restarted wait is woken by the thread's FUTEX_WAKE and returns
/// 0; precise mode reproduces that. Polling mode's deterministic schedule runs
/// the restarted wait after the thread has set the word, which Linux also
/// permits and reports as EAGAIN. Each mode's result is pinned.
#[test]
fn untimed_futex_wait_restarts_under_sa_restart() {
    assert_cell(
        "ptrace",
        FutexMode::Precise,
        &["futex", "thread", "restart"],
        false,
        "RESULT call=futex ret=0 errno=none handler=1",
    );
    assert_cell(
        "ptrace",
        FutexMode::Polling,
        &["futex", "thread", "restart"],
        false,
        "RESULT call=futex ret=-1 errno=EAGAIN handler=1",
    );
}

/// `poll`, `epoll_wait`, glibc `select` (pselect6 with no mask), and the
/// `select` system call end with EINTR for a caught signal from a sibling
/// thread or a live sibling process, SA_RESTART or not, as Linux does. The 16
/// cells are split by sender, the `thread` cells in one test and the `process`
/// cells in another, so that each test stays inside the per-test CPU bound with
/// room to spare.
fn assert_readiness_waits_are_interrupted(backend: &str, sender: &str) {
    for call in READINESS_CALLS {
        for restart in [None, Some("restart")] {
            let mut args = vec![call, sender];
            args.extend(restart);
            assert_cell(
                backend,
                FutexMode::Precise,
                &args,
                false,
                &format!("RESULT call={call} ret=-1 errno=EINTR handler=1"),
            );
        }
    }
}

#[test]
fn ptrace_readiness_waits_are_interrupted_by_internal_signals() {
    assert_readiness_waits_are_interrupted("ptrace", "thread");
}

#[test]
fn ptrace_readiness_waits_are_interrupted_by_internal_process_signals() {
    assert_readiness_waits_are_interrupted("ptrace", "process");
}

/// An ignored, blocked, or default-ignored signal does not end `poll`,
/// `epoll_wait`, or either `select`: each returns 0 at its 300 ms timeout.
/// Every one of these 24 cells is strict-verified, so they are split by sender
/// and pair of calls, `poll` and `epoll_wait` in one test and the two `select`s
/// in another, to keep each test inside the per-test wall and CPU bounds with
/// room to spare.
fn assert_readiness_waits_are_not_ended(backend: &str, sender: &str, calls: [&str; 2]) {
    for call in calls {
        for quiet in ["ignored", "blocked", "winch"] {
            assert_quiet_cell(
                backend,
                FutexMode::Precise,
                &[call, sender, quiet],
                &format!("RESULT call={call} ret=0 errno=none handler=0"),
            );
        }
    }
}

/// The two halves of `READINESS_CALLS` that `assert_readiness_waits_are_not_ended`
/// runs in separate tests.
const POLL_AND_EPOLL: [&str; 2] = ["poll", "epoll"];
const SELECTS: [&str; 2] = ["select", "rawselect"];

#[test]
fn ptrace_readiness_waits_are_not_ended_by_non_interrupting_thread_signals() {
    assert_readiness_waits_are_not_ended("ptrace", "thread", POLL_AND_EPOLL);
}

#[test]
fn ptrace_selects_are_not_ended_by_non_interrupting_thread_signals() {
    assert_readiness_waits_are_not_ended("ptrace", "thread", SELECTS);
}

#[test]
fn ptrace_readiness_waits_are_not_ended_by_non_interrupting_process_signals() {
    assert_readiness_waits_are_not_ended("ptrace", "process", POLL_AND_EPOLL);
}

#[test]
fn ptrace_selects_are_not_ended_by_non_interrupting_process_signals() {
    assert_readiness_waits_are_not_ended("ptrace", "process", SELECTS);
}

/// `POLL_AND_EPOLL` and `SELECTS` together are exactly `READINESS_CALLS`, so
/// splitting the not-ended cells between them drops none.
#[test]
fn the_not_ended_call_pairs_cover_every_readiness_call() {
    let mut split: Vec<&str> = POLL_AND_EPOLL.iter().chain(&SELECTS).copied().collect();
    let mut all = READINESS_CALLS.to_vec();
    split.sort_unstable();
    all.sort_unstable();
    assert_eq!(split, all);
}

/// A call that does not wait reports its result although a caught signal races
/// it: a ready descriptor for `poll` and `ppoll`, a queued event for
/// `epoll_wait` and `epoll_pwait`, or an argument error, EINVAL for
/// `epoll_wait` with maxevents 0, EBADF for `epoll_wait` on a descriptor that is
/// not open, and EFAULT for `rt_sigtimedwait` with a set it cannot read. Linux
/// looks at pending signals only after those checks: do_poll returns a ready
/// count before it checks signal_pending, ep_poll hands over queued events
/// first, and do_epoll_wait and rt_sigtimedwait check their arguments first.
/// So in each of the guest's six trials, wherever the sibling's SIGUSR1 lands,
/// the call returns its result and the handler runs once; the cell is
/// strict-verified. Before round 7 of
/// https://github.com/rrnewton/hermit/pull/3361, Hermit's first probe of these
/// calls saw the pending signal first and returned EINTR in the trial where
/// the signal was already pending when the call began (`matched=5`).
fn assert_racing_call(backend: &str, call: &str) {
    assert_racing_sender(backend, call, "racing");
}

/// `assert_racing_call` with the guest's `sender`: `racing` sends SIGUSR1 and
/// `racingchld` sends SIGCHLD, caught in both.
fn assert_racing_sender(backend: &str, call: &str, sender: &str) {
    assert_cell(
        backend,
        FutexMode::Precise,
        &[call, sender],
        false,
        &format!("RESULT call={call} role={sender} trials={RACING_TRIALS} matched={RACING_TRIALS}"),
    );
}

#[test]
fn ptrace_poll_reports_a_ready_descriptor_before_a_racing_signal() {
    assert_racing_call("ptrace", "poll");
}

#[test]
fn ptrace_ppoll_reports_a_ready_descriptor_before_a_racing_signal() {
    assert_racing_call("ptrace", "ppoll");
}

#[test]
fn ptrace_epoll_wait_reports_a_queued_event_before_a_racing_signal() {
    assert_racing_call("ptrace", "epoll");
}

#[test]
fn ptrace_epoll_pwait_reports_a_queued_event_before_a_racing_signal() {
    assert_racing_call("ptrace", "epollpwait");
}

#[test]
fn ptrace_epoll_wait_reports_einval_for_zero_maxevents_before_a_racing_signal() {
    assert_racing_call("ptrace", "epollinval");
}

#[test]
fn ptrace_epoll_wait_reports_ebadf_for_a_closed_descriptor_before_a_racing_signal() {
    assert_racing_call("ptrace", "epollbadf");
}

#[test]
fn ptrace_rt_sigtimedwait_reports_efault_for_an_unreadable_set_before_a_racing_signal() {
    assert_racing_call("ptrace", "sigtimedwaitfault");
}

/// `select` and `pselect6` report a ready descriptor although a caught SIGCHLD
/// races them: Linux's core_sys_select returns the count of ready descriptors
/// and looks at pending signals only when none is ready. So in each of the six
/// trials of the guest's `select racingchld` (glibc's select, which issues
/// pselect6 with no mask) and `rawselect racingchld` (the select system call),
/// wherever the sibling's SIGCHLD lands, the call returns 1 with the pipe set
/// and the handler runs once; each cell is strict-verified. A select wait holds
/// no signal until it returns (`KernelSignalWait::for_select`), so a pending
/// caught SIGCHLD does end it when nothing is ready, as on Linux. Before round
/// 12 of https://github.com/rrnewton/hermit/pull/3361, Hermit read the kernel's
/// signal state before the first probe of these calls and returned EINTR in a
/// trial where the signal was already pending when the call began (round-11
/// self-finding "select and pselect6 report a pending signal before a ready
/// descriptor or EBADF").
#[test]
fn ptrace_selects_report_a_ready_descriptor_before_a_racing_sigchld() {
    for call in SELECTS {
        assert_racing_sender("ptrace", call, "racingchld");
    }
}

/// `select` and `pselect6` report EBADF for a descriptor that is not open
/// although a caught SIGUSR1 races them: core_sys_select rejects it
/// (max_select_fd) before it looks at pending signals. The guest's
/// `selectbadf racing` and `rawselectbadf racing` cells, as above.
#[test]
fn ptrace_selects_report_ebadf_for_a_closed_descriptor_before_a_racing_signal() {
    for call in ["selectbadf", "rawselectbadf"] {
        assert_racing_call("ptrace", call);
    }
}

/// Trials in the guest's `parkready` and `parkclose` modes (`PARKED_TRIALS` in
/// the guest).
const PARKED_TRIALS: usize = 4;

/// `assert_cell` for the guest's `parkready` or `parkclose` `sender` with
/// `call`, `select` or `rawselect`.
fn assert_parked_sender(backend: &str, call: &str, sender: &str) {
    assert_cell(
        backend,
        FutexMode::Precise,
        &[call, sender],
        false,
        &format!("RESULT call={call} role={sender} trials={PARKED_TRIALS} matched={PARKED_TRIALS}"),
    );
}

/// `select` and `pselect6` report a descriptor that becomes ready while they
/// wait, although a caught SIGUSR1 follows before they look again: Linux's
/// do_select polls every descriptor on each pass and returns the count of
/// ready descriptors before it looks at pending signals. In each of the four
/// trials of the guest's `select parkready` (glibc's select, which issues
/// pselect6) and `rawselect parkready` (the select system call), the call
/// finds the pipe empty and waits; a sibling then writes to the pipe and sends
/// SIGUSR1, caught with SA_RESTART, after zero to three sched_yield calls. The
/// call returns 1 with the pipe set and the handler runs once, as natively on
/// Linux 7.1.3 in 4 of 4 trials; each cell is strict-verified. Before round 13
/// of https://github.com/rrnewton/hermit/pull/3361, each turn of these waits
/// after the first read the kernel's signal state before its probe and
/// returned EINTR when the signal was already pending at that turn (round-12
/// Medium). For the select system call that was a regression of this pull
/// request: at its base, `rawselect parkready` matched in 4 of 4 trials. Its
/// base already returned EINTR in 2 of the 4 trials of `select parkready`, so
/// for pselect6 the gap predates this pull request; it is fixed here as well.
#[test]
fn ptrace_selects_report_a_descriptor_made_ready_before_a_later_signal() {
    for call in SELECTS {
        assert_parked_sender("ptrace", call, "parkready");
    }
}

/// `select` and `pselect6` report EBADF for the descriptor they wait on when a
/// sibling closes it while they wait and then sends a caught SIGUSR1, in the
/// guest's `select parkclose` and `rawselect parkclose` cells, as above. This
/// is Hermit's result and not Linux's: each probe Hermit makes is a new select
/// call, which rejects the closed descriptor (max_select_fd), while Linux
/// 7.1.3 polls it on the next pass and returns 1 with it set (measured
/// natively). The cell pins that the call reports the descriptor and not the
/// signal: before round 13 of https://github.com/rrnewton/hermit/pull/3361,
/// each turn after the first returned EINTR when the signal was already
/// pending at that turn (round-12 Medium). As above, that was a regression of
/// this pull request for the select system call (EBADF in 4 of 4 trials of
/// `rawselect parkclose` at its base), while its base already returned EINTR
/// in 2 of the 4 trials of `select parkclose`.
#[test]
fn ptrace_selects_report_ebadf_for_a_descriptor_closed_before_a_later_signal() {
    for call in SELECTS {
        assert_parked_sender("ptrace", call, "parkclose");
    }
}

/// An ignored, blocked, or default-ignored signal does not end a timed futex wait
/// in either mode: it returns ETIMEDOUT at its original 300 ms deadline.
fn assert_timed_futex_wait_is_not_ended(backend: &str) {
    for mode in [FutexMode::Precise, FutexMode::Polling] {
        for sender in ["thread", "process"] {
            for quiet in ["ignored", "blocked", "winch"] {
                assert_quiet_cell(
                    backend,
                    mode,
                    &["futex", sender, quiet],
                    "RESULT call=futex ret=-1 errno=ETIMEDOUT handler=0",
                );
            }
        }
    }
}

#[test]
fn ptrace_timed_futex_wait_is_not_ended_by_non_interrupting_signals() {
    assert_timed_futex_wait_is_not_ended("ptrace");
}

/// The waits whose Linux restart keeps the absolute deadline (`poll` with a
/// positive timeout, a timed `FUTEX_WAIT`, and a timed `FUTEX_WAIT_BITSET`, here
/// glibc's `sem_timedwait`) keep it when a SIGTSTP left at SIG_DFL arrives in an
/// orphaned process group (the guest's `tstp` option). Linux discards that
/// signal: the process does not stop, no handler runs, and the kernel restarts
/// the interrupted call with the end time it saved, so these calls return their
/// timeout result at 300 ms. Detcore leaves them waiting through a default
/// SIGTSTP, SIGTTIN or SIGTTOU, as hermit main does for the first two, instead
/// of ending them: the backend holds such a signal, and a wait ended for a held
/// signal other than SIGSTOP restarts as ERESTARTNOHAND, which re-arms a
/// relative timeout and reads an absolute deadline again. Ending them returned
/// near 400 ms (review of https://github.com/rrnewton/hermit/pull/3361 at
/// `cbb36408`, finding 4). `sem_timedwait` joined this group when its restart
/// began to keep the deadline it copied (round-10 finding Medium 4); its cell
/// was in the control test below before, with the same expected result.
fn assert_discarded_default_stop_leaves_rearming_waits(backend: &str) {
    for sender in ["thread", "process"] {
        for mode in [FutexMode::Precise, FutexMode::Polling] {
            assert_quiet_cell(
                backend,
                mode,
                &["futex", sender, "tstp"],
                "RESULT call=futex ret=-1 errno=ETIMEDOUT handler=0",
            );
        }
        assert_quiet_cell(
            backend,
            FutexMode::Precise,
            &["poll", sender, "tstp"],
            "RESULT call=poll ret=0 errno=none handler=0",
        );
    }
    assert_quiet_cell(
        backend,
        FutexMode::Precise,
        &["sem", "thread", "tstp"],
        "RESULT call=sem ret=-1 errno=ETIMEDOUT handler=0",
    );
}

#[test]
fn ptrace_timed_futex_wait_and_poll_keep_their_deadline_through_a_discarded_default_stop() {
    assert_discarded_default_stop_leaves_rearming_waits("ptrace");
}

/// How many SIGCHLDs the `chldflood` cells send before their SIGUSR1.
const SIGCHLD_FLOOD: usize = 20_000;

/// `poll` and a timed polling `FUTEX_WAIT` keep their deadline after a flood of
/// default-ignored SIGCHLD from outside Hermit (the guest's `chldflood`
/// option). Linux neither ends nor restarts either call for that signal, so
/// both return their timeout result at 300 ms. The harness sends the whole
/// flood first and SIGUSR1 last, and the guest starts its wait only once its
/// SIGUSR1 handler has run. `kill` returns once the signal is queued, so every
/// SIGCHLD has been sent before the wait starts and none arrives while it runs.
/// Standard signals merge, so at most one may still be pending in the kernel
/// then; if one is, the first `/proc` read shows it, and the stop it makes is
/// identified and held to the call's return (`HeldKind::Precious`). Nothing
/// here requires that a SIGCHLD is still pending, so this test covers the
/// deadline after a flood, not arrivals during the wait.
///
/// The flood used to follow the SIGUSR1, so it raced the wait's first probe and
/// its mask change, the two injections that run under the guest's own mask. A
/// SIGCHLD that arrives after Detcore's `/proc` read and before such an
/// injection stops it in a way `/proc` cannot identify, because the backend
/// does not say which signal it holds. A second SIGCHLD pending before the
/// next injection would replace that unidentified held signal, which may be
/// one whose loss matters, so since round 10 of
/// https://github.com/rrnewton/hermit/pull/3361 the wait is refused there,
/// fail-closed, before the injection (`KernelSignalWait::inject_absorbing`,
/// `HeldSignalLoss::Replaced`), and the run ends with HERMIT_POLICY_REFUSAL.
/// Whether two flood signals land in that window is host timing, so the cell
/// was refused intermittently, most often on a loaded host, and a refused run
/// also made the harness's remaining `kill` calls fail with ESRCH. The Detcore
/// unit tests pin each decision of that path:
/// `a_stop_that_cannot_be_identified_keeps_a_timed_wait_and_its_deadline`, and
/// `a_sigchld_after_an_unidentified_stop_is_refused_before_it_replaces_it` for
/// the refusal. A racing flood can pass every run only once Detcore no longer
/// has to guess the held signal: for example if the backend names it, keeps
/// every held signal, or lets Detcore change the mask without an injection.
fn assert_sigchld_flood_leaves_timed_waits_their_deadline(backend: &str) {
    let mut signals = vec![libc::SIGCHLD; SIGCHLD_FLOOD];
    signals.push(libc::SIGUSR1);
    let cells: [(&[&str], &str); 2] = [
        (
            &["poll", "external", "chldflood"],
            "RESULT call=poll ret=0 errno=none handler=1",
        ),
        (
            &["futex", "external", "timed", "chldflood"],
            "RESULT call=futex ret=-1 errno=ETIMEDOUT handler=1",
        ),
    ];
    for (args, expected) in cells {
        let run = run_cell_with_external_signals(backend, FutexMode::Polling, args, &signals);
        assert!(
            run.status.success()
                && run.result_line() == Some(expected)
                && run.stdout.lines().any(|line| line == "DONE"),
            "{backend} {args:?}: expected `{expected}`\n{}",
            run.describe()
        );
        let elapsed = run.elapsed_ms();
        assert!(
            elapsed.is_some_and(|ms| {
                (QUIET_TIMEOUT_MS..QUIET_TIMEOUT_MS + QUIET_OVERSHOOT_MS).contains(&ms)
            }),
            "{backend} {args:?}: the wait took {elapsed:?} ms, not its \
             {QUIET_TIMEOUT_MS} ms timeout\n{}",
            run.describe()
        );
    }
}

#[test]
fn ptrace_timed_futex_wait_and_poll_keep_their_deadline_after_a_sigchld_flood() {
    assert_sigchld_flood_leaves_timed_waits_their_deadline("ptrace");
}

/// Control for the test above: the same discarded SIGTSTP still ends, or
/// restarts, every other wait as Linux does. glibc `select` and the `select`
/// system call restart with their deadline kept (Detcore writes the time left
/// back for both), so they return their timeout result at 300 ms. `epoll_wait`
/// is not restarted: Linux returns EINTR when the signal arrives, although no
/// handler runs. (`sem_timedwait` moved to the test above in round 11.)
fn assert_discarded_default_stop_is_handled_as_on_linux(backend: &str) {
    for (call, expected) in [
        ("select", "RESULT call=select ret=0 errno=none handler=0"),
        (
            "rawselect",
            "RESULT call=rawselect ret=0 errno=none handler=0",
        ),
    ] {
        assert_quiet_cell(
            backend,
            FutexMode::Precise,
            &[call, "thread", "tstp"],
            expected,
        );
    }
    for sender in ["thread", "process"] {
        assert_woken_cell(
            backend,
            FutexMode::Precise,
            &["epoll", sender, "tstp"],
            "RESULT call=epoll ret=-1 errno=EINTR handler=0",
        );
    }
}

#[test]
fn ptrace_other_waits_take_a_discarded_default_stop_as_on_linux() {
    assert_discarded_default_stop_is_handled_as_on_linux("ptrace");
}

/// SIGSTOP and then SIGCONT, neither with a handler, interrupt `poll` and a
/// relative timed `FUTEX_WAIT` 100 ms into their 300 ms timeout (the guest's
/// `stopcont` mode). Linux restarts both through the restart block it saved
/// (`do_restart_poll`, `futex_wait_restart`), which keeps the original absolute
/// deadline, so they return their timeout result 300 ms after they began.
/// Detcore ends these waits with Linux's code, `ERESTART_RESTARTBLOCK`, and
/// resumes the `restart_syscall` the kernel then runs with the deadline it
/// kept. It used to end them with `ERESTARTNOHAND`, so the kernel ran the
/// original call again and its relative timeout started again: they returned
/// near 400 ms (https://github.com/rrnewton/hermit/issues/3358, item 1).
fn assert_stop_and_continue_keep_the_deadline(backend: &str) {
    for mode in [FutexMode::Precise, FutexMode::Polling] {
        assert_quiet_cell(
            backend,
            mode,
            &["futex", "stopcont"],
            "RESULT call=futex ret=-1 errno=ETIMEDOUT handler=0",
        );
    }
    assert_quiet_cell(
        backend,
        FutexMode::Precise,
        &["poll", "stopcont"],
        "RESULT call=poll ret=0 errno=none handler=0",
    );
}

#[test]
fn ptrace_timed_futex_wait_and_poll_keep_their_deadline_through_sigstop_and_sigcont() {
    assert_stop_and_continue_keep_the_deadline("ptrace");
}

/// A timed `FUTEX_WAIT_BITSET` keeps the absolute deadline it copied when the
/// wait began (the guest's `bitset` call in its `stopcont` mode). The guest's
/// child moves that deadline 200 ms later in shared memory and then sends
/// SIGSTOP and SIGCONT. Linux copied the deadline into the restart block, which
/// `futex_wait_restart` uses, so the move has no effect and the call returns
/// ETIMEDOUT 300 ms after it began. Detcore used to end this wait with
/// `ERESTARTNOHAND`, so the kernel ran the call again with its original
/// arguments, which read the moved deadline (round-10 finding Medium 4 on
/// https://github.com/rrnewton/hermit/pull/3361).
fn assert_a_timed_bitset_wait_keeps_its_copied_deadline(backend: &str) {
    for mode in [FutexMode::Precise, FutexMode::Polling] {
        assert_quiet_cell(
            backend,
            mode,
            &["bitset", "stopcont"],
            "RESULT call=bitset ret=-1 errno=ETIMEDOUT handler=0",
        );
    }
}

#[test]
fn ptrace_a_timed_futex_wait_bitset_keeps_its_copied_deadline_through_sigstop_and_sigcont() {
    assert_a_timed_bitset_wait_keeps_its_copied_deadline("ptrace");
}

/// The wait that resumes after SIGCONT is still a wait a caught signal ends:
/// SIGUSR1, caught with flags 0, arrives 50 ms after SIGCONT (the guest's
/// `stopcontusr1` mode), and the call returns -1 with EINTR before its deadline,
/// as on Linux, where a handler turns the restart code into EINTR. The wait
/// cannot end before the SIGSTOP 100 ms in, and must end before its 300 ms
/// deadline.
fn assert_a_caught_signal_ends_the_wait_resumed_after_sigcont(backend: &str) {
    for (call, mode) in [
        ("futex", FutexMode::Precise),
        ("futex", FutexMode::Polling),
        ("poll", FutexMode::Precise),
    ] {
        let args = [call, "stopcontusr1"];
        let expected = format!("RESULT call={call} ret=-1 errno=EINTR handler=1");
        let run = run_cell(backend, mode, &args, false);
        assert!(
            run.status.success()
                && run.result_line() == Some(expected.as_str())
                && run.stdout.lines().any(|line| line == "DONE"),
            "{backend} {mode:?} {args:?}: expected `{expected}`\n{}",
            run.describe()
        );
        let elapsed = run.elapsed_ms();
        assert!(
            elapsed.is_some_and(|ms| (SIGNAL_DELAY_MS..QUIET_TIMEOUT_MS).contains(&ms)),
            "{backend} {mode:?} {args:?}: the wait took {elapsed:?} ms, not between the \
             SIGSTOP at {SIGNAL_DELAY_MS} ms and its {QUIET_TIMEOUT_MS} ms deadline\n{}",
            run.describe()
        );
        assert_verified(backend, mode, &args, &run);
    }
}

#[test]
fn ptrace_a_caught_signal_ends_a_timed_wait_resumed_after_sigcont() {
    assert_a_caught_signal_ends_the_wait_resumed_after_sigcont("ptrace");
}

/// A caught handler whose first syscall is `restart_syscall` resumes the wait
/// the signal interrupted, as on Linux (the guest's `restartfirst` mode). A
/// sibling thread interrupts a timed `FUTEX_WAIT` or `FUTEX_WAIT_BITSET`, made
/// through libc's `syscall()`, with SIGUSR1 100 ms after it began, and the
/// handler calls `syscall(SYS_restart_syscall)` through the same wrapper, so the
/// call stops at the address of the interrupted one. Linux resets the restart
/// block only at sigreturn (`restore_sigcontext`), so the handler's call runs
/// `futex_wait_restart` with the copied arguments and absolute deadline: it
/// returns ETIMEDOUT at the original deadline, and the interrupted call returns
/// EINTR after the handler, 300 ms after it began. Round-10 finding Medium 3 on
/// https://github.com/rrnewton/hermit/pull/3361 asked Detcore to discard its
/// record when a handler takes control; a native run of this guest shows the
/// Linux behavior asserted here instead.
fn assert_a_handlers_first_restart_syscall_resumes_the_interrupted_wait(backend: &str) {
    for call in ["futex", "bitset"] {
        let args = [call, "restartfirst"];
        let expected = format!("RESULT call={call} ret=-1 errno=EINTR handler=1");
        let run = run_cell(backend, FutexMode::Precise, &args, false);
        assert!(
            run.status.success()
                && run.result_line() == Some(expected.as_str())
                && run
                    .stdout
                    .lines()
                    .any(|line| line == "RESTART ret=-1 errno=ETIMEDOUT")
                && run.stdout.lines().any(|line| line == "DONE"),
            "{backend} {args:?}: expected `{expected}` and `RESTART ret=-1 errno=ETIMEDOUT`\n{}",
            run.describe()
        );
        let elapsed = run.elapsed_ms();
        assert!(
            elapsed.is_some_and(|ms| {
                (QUIET_TIMEOUT_MS..QUIET_TIMEOUT_MS + QUIET_OVERSHOOT_MS).contains(&ms)
            }),
            "{backend} {args:?}: the wait took {elapsed:?} ms, not its original \
             {QUIET_TIMEOUT_MS} ms deadline\n{}",
            run.describe()
        );
        assert_verified(backend, FutexMode::Precise, &args, &run);
    }
}

#[test]
fn ptrace_a_handlers_first_restart_syscall_resumes_the_interrupted_timed_futex_wait() {
    assert_a_handlers_first_restart_syscall_resumes_the_interrupted_wait("ptrace");
}

/// Positive control: `select` already observed an external signal before the
/// fix, on both backends.
#[test]
fn select_is_interrupted_by_an_external_signal() {
    assert_cell(
        "ptrace",
        FutexMode::Precise,
        &["select", "external"],
        true,
        "RESULT call=select ret=-1 errno=EINTR handler=1",
    );
}

/// The guest options that catch SIGCHLD in a process that never had a child,
/// that reaped one, and that has a live one.
const EXTERNAL_SIGCHLD_OPTIONS: [&str; 3] = ["extchld", "extchldreaped", "extchldlive"];

/// A caught SIGCHLD sent with kill(2) from outside Hermit ends glibc's `select`,
/// which is `pselect6` with no mask, and the `select` system call with EINTR,
/// as on Linux, whether or not the process has had a child. Before select and
/// pselect6 stopped asking the scheduler whether a pending SIGCHLD may end the
/// wait, the scheduler, which only makes the SIGCHLD it can order eligible,
/// held this one in a process that had had a child, and the wait never ended
/// (https://github.com/rrnewton/hermit/issues/3146).
fn assert_external_sigchld_ends_selects(backend: &str) {
    for option in EXTERNAL_SIGCHLD_OPTIONS {
        for call in ["select", "rawselect"] {
            let args = [call, "external", option];
            let run = run_cell_with_external_signals(
                backend,
                FutexMode::Precise,
                &args,
                &[libc::SIGCHLD],
            );
            let expected = format!("RESULT call={call} ret=-1 errno=EINTR handler=1");
            assert!(
                run.status.success() && run.result_line() == Some(expected.as_str()),
                "{backend} {args:?}: expected `{expected}`\n{}",
                run.describe()
            );
            assert!(
                run.stdout.lines().any(|line| line == "DONE"),
                "{backend} {args:?}: guest did not finish\n{}",
                run.describe()
            );
        }
    }
}

#[test]
fn ptrace_selects_are_ended_by_an_external_sigchld_with_or_without_a_child() {
    assert_external_sigchld_ends_selects("ptrace");
}

/// Positive control: `wait4` and `waitid` on a live child already returned
/// EINTR for a signal from that child before the fix.
#[test]
fn child_waits_are_interrupted_by_a_live_sibling_process_signal() {
    for call in ["wait4", "waitid"] {
        assert_cell(
            "ptrace",
            FutexMode::Precise,
            &[call, "process"],
            false,
            &format!("RESULT call={call} ret=-1 errno=EINTR handler=1"),
        );
    }
}

/// A signal whose disposition a sibling changes to caught while the waiter is
/// parked ends the wait with EINTR near the send, timed or not, as Linux does:
/// SIGUSR1 turned from SIG_IGN to a handler. Before
/// https://github.com/rrnewton/hermit/pull/3361 stopped filtering on the
/// disposition read when the wait began, the precise wait ran to its timeout
/// and the untimed one never ended. The SIGCHLD half of this cell (`chldlate`)
/// moved to `assert_caught_sigchld_is_held_by_futex_wait`: a SIGCHLD no longer
/// ends a gated wait.
fn assert_caught_after_parking_ends_futex_wait(backend: &str) {
    for mode in [FutexMode::Precise, FutexMode::Polling] {
        for timed in [None, Some("timed")] {
            let mut args = vec!["futex", "thread", "ign2caught"];
            args.extend(timed);
            assert_woken_cell(backend, mode, &args, EINTR_FUTEX);
        }
    }
}

#[test]
fn ptrace_futex_wait_is_ended_by_a_signal_caught_after_it_parked() {
    assert_caught_after_parking_ends_futex_wait("ptrace");
}

/// A SIGCHLD whose disposition a sibling changes to caught while the waiter is
/// parked, from a child that then dies by `exit_group` (`chldlate`), by SIGKILL
/// (`chldkill`), or by its only thread calling `exit` (`chldthrexit`).
///
/// In precise mode, for a child that calls `exit_group`, the scheduler commits
/// its SIGCHLD to the parked waiter through its child-exit timer, and the wake
/// ends the wait with EINTR near the child's death, timed or not, as on Linux
/// (https://github.com/rrnewton/hermit/issues/4005). Round 9 of
/// https://github.com/rrnewton/hermit/pull/3361 had made it hold the SIGCHLD.
///
/// Hermit still holds the SIGCHLD until the call returns (the guest's `held`
/// option) where the scheduler commits none to the waiter: for a child that
/// dies by SIGKILL or by its only thread's `exit`, which has no child-exit
/// timer, so the only SIGCHLD is the kernel's, posted at a host-timed moment;
/// and in polling mode, where the waiter is not the thread the scheduler
/// commits the signal to. A timed wait then returns ETIMEDOUT at its 300 ms
/// deadline, and an untimed one is ended by the sibling's FUTEX_WAKE, which
/// precise mode reports as 0 and polling mode as EAGAIN. Linux ends these
/// waits with EINTR too. Each futex mode is its own test, to stay inside the
/// per-test wall and CPU bounds.
fn assert_caught_sigchld_after_parking(backend: &str, mode: FutexMode) {
    for flip in ["chldlate", "chldkill", "chldthrexit"] {
        match mode {
            FutexMode::Precise if flip == "chldlate" => {
                for timed in [None, Some("timed")] {
                    let mut args = vec!["futex", "thread", flip, "held"];
                    args.extend(timed);
                    assert_woken_cell(backend, mode, &args, EINTR_FUTEX);
                }
            }
            _ => {
                assert_quiet_cell(
                    backend,
                    mode,
                    &["futex", "thread", flip, "held", "timed"],
                    "RESULT call=futex ret=-1 errno=ETIMEDOUT handler=1",
                );
                let untimed = match mode {
                    FutexMode::Precise => "RESULT call=futex ret=0 errno=none handler=1",
                    FutexMode::Polling => "RESULT call=futex ret=-1 errno=EAGAIN handler=1",
                };
                assert_quiet_cell(backend, mode, &["futex", "thread", flip, "held"], untimed);
            }
        }
    }
}

#[test]
fn ptrace_precise_futex_wait_is_ended_by_a_sigchld_caught_after_it_parked() {
    assert_caught_sigchld_after_parking("ptrace", FutexMode::Precise);
}

#[test]
fn ptrace_polling_futex_wait_holds_a_sigchld_caught_after_it_parked_until_it_returns() {
    assert_caught_sigchld_after_parking("ptrace", FutexMode::Polling);
}

/// A signal whose disposition a sibling changes to ignored while the waiter is
/// parked does not end the wait: a timed wait returns ETIMEDOUT at its original
/// 300 ms deadline, not after a restart with a fresh timeout, and an untimed
/// wait is ended only by the sibling's FUTEX_WAKE 200 ms after the signal.
/// Precise mode reports that wake as 0; polling mode runs its next probe after
/// the sibling set the word and reports EAGAIN, as in
/// `untimed_futex_wait_restarts_under_sa_restart`.
fn assert_ignored_after_parking_leaves_futex_wait(backend: &str) {
    for mode in [FutexMode::Precise, FutexMode::Polling] {
        for flip in ["caught2ign", "chldign"] {
            assert_quiet_cell(
                backend,
                mode,
                &["futex", "thread", flip, "timed"],
                "RESULT call=futex ret=-1 errno=ETIMEDOUT handler=0",
            );
            let untimed = match mode {
                FutexMode::Precise => "RESULT call=futex ret=0 errno=none handler=0",
                FutexMode::Polling => "RESULT call=futex ret=-1 errno=EAGAIN handler=0",
            };
            assert_quiet_cell(backend, mode, &["futex", "thread", flip], untimed);
        }
    }
}

#[test]
fn ptrace_futex_wait_is_not_ended_by_a_signal_ignored_after_it_parked() {
    assert_ignored_after_parking_leaves_futex_wait("ptrace");
}

/// A caught SIGCHLD from a child that exits during the wait ends a futex wait
/// with EINTR near the exit, as on Linux: the scheduler commits it through the
/// child-exit timer, and only that committed wake decides
/// (https://github.com/rrnewton/hermit/issues/4005). The kernel posts its own
/// SIGCHLD for the exit at a host-timed moment, which must not decide the
/// result, so each cell is strict-verified `SIGCHLD_TRIALS` times. Round 9 of
/// https://github.com/rrnewton/hermit/pull/3361 had made it hold the SIGCHLD to
/// the 300 ms deadline (the guest's `held` option). Each futex mode is its own
/// test, to stay inside the per-test wall and CPU bounds.
fn assert_child_exit_sigchld_ends_futex_wait(backend: &str, mode: FutexMode) {
    for _ in 0..SIGCHLD_TRIALS {
        assert_woken_cell(backend, mode, &["futex", "exit", "held"], EINTR_FUTEX);
    }
}

#[test]
fn ptrace_precise_futex_wait_is_ended_by_the_sigchld_of_an_exiting_child() {
    assert_child_exit_sigchld_ends_futex_wait("ptrace", FutexMode::Precise);
}

#[test]
fn ptrace_polling_futex_wait_is_ended_by_the_sigchld_of_an_exiting_child() {
    assert_child_exit_sigchld_ends_futex_wait("ptrace", FutexMode::Polling);
}

/// The same child-exit SIGCHLD ends `poll` and `epoll_wait` with EINTR near
/// the exit, as on Linux, each strict-verified `SIGCHLD_TRIALS` times
/// (https://github.com/rrnewton/hermit/issues/4005). Round 9 of
/// https://github.com/rrnewton/hermit/pull/3361 had made each run to its
/// 300 ms timeout.
fn assert_child_exit_sigchld_ends_readiness_waits(backend: &str) {
    for _ in 0..SIGCHLD_TRIALS {
        for call in ["poll", "epoll"] {
            assert_woken_cell(
                backend,
                FutexMode::Precise,
                &[call, "exit", "held"],
                &format!("RESULT call={call} ret=-1 errno=EINTR handler=1"),
            );
        }
    }
}

/// `rt_sigtimedwait` for a blocked SIGUSR1 with a 1 s timeout, while a child
/// exits after 100 ms, under `run --strict --verify --verify-strict`
/// (tests/c/record_replay_sigtimedwait_child_exit.c). Linux ends the wait with
/// EINTR when the guest catches SIGCHLD, and lets it time out with EAGAIN when
/// SIGCHLD is blocked or left at its default. Hermit returned EAGAIN in the
/// caught case too (https://github.com/rrnewton/hermit/issues/4005).
#[test]
fn ptrace_rt_sigtimedwait_is_ended_by_the_sigchld_of_an_exiting_child() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let root = build_root();
    fs::create_dir_all(&root).expect("failed to create guest build directory");
    let witness = root.join(format!("sigtimedwait_child_exit.{}", std::process::id()));
    let compiled = Command::new("cc")
        .args(["-O2", "-Wall", "-Wextra", "-Werror", "-pthread"])
        .arg(repository.join("tests/c/record_replay_sigtimedwait_child_exit.c"))
        .arg("-o")
        .arg(&witness)
        .stdin(Stdio::null())
        .output()
        .expect("failed to start the witness's compiler");
    assert!(
        compiled.status.success(),
        "failed to compile the witness: {}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    for (mode, errno) in [
        ("caught", libc::EINTR),
        ("blocked", libc::EAGAIN),
        ("default", libc::EAGAIN),
    ] {
        let trial = tempfile::Builder::new()
            .prefix("trial-")
            .tempdir_in(&root)
            .expect("failed to create trial directory");
        let report = trial.path().join("verify.json");
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "120s"])
            .arg(hermit_binary::hermit_binary())
            .env_remove("LD_LIBRARY_PATH")
            .arg("--log=info")
            .args(cell_run_args("ptrace"))
            .args(["--verify", "--verify-strict", "--verify-json"])
            .arg(&report)
            .arg("--")
            .arg(&witness)
            .arg(mode)
            .stdin(Stdio::null())
            .output()
            .expect("failed to run hermit");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let expected = format!("mode={mode} result=-1 errno={errno} ");
        assert!(
            output.status.success() && stdout.contains(&expected),
            "{mode}: expected `{expected}`, as Linux returns, with exit 0; got {}\nstdout:\n{stdout}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        let verdict: serde_json::Value = serde_json::from_slice(
            &fs::read(&report).expect("strict verification wrote no verdict"),
        )
        .expect("strict verification verdict is valid JSON");
        if let Some(gap) = strict_verification_gap(&verdict) {
            panic!("{mode}: strict verification is not determinism evidence: {gap}");
        }
    }
    let _ = fs::remove_file(&witness);
}

/// The parent catches SIGCHLD and waits in `rt_sigtimedwait` for a blocked
/// SIGUSR1, which its child sends before it calls `exit_group`
/// (tests/c/sigtimedwait_queued_signal_before_child_exit.c). Linux returns the
/// queued SIGUSR1, with the child's siginfo, before it considers EINTR for the
/// child's SIGCHLD. Hermit returned EINTR once the committed SIGCHLD ended the
/// wait before the turn's probe, leaving SIGUSR1 queued (Codex review of
/// https://github.com/rrnewton/hermit/pull/4039).
#[test]
fn ptrace_rt_sigtimedwait_takes_a_queued_signal_before_the_sigchld_of_an_exiting_child() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let root = build_root();
    fs::create_dir_all(&root).expect("failed to create guest build directory");
    let witness = root.join(format!("sigtimedwait_queued.{}", std::process::id()));
    let compiled = Command::new("cc")
        .args(["-O2", "-Wall", "-Wextra", "-Werror"])
        .arg(repository.join("tests/c/sigtimedwait_queued_signal_before_child_exit.c"))
        .arg("-o")
        .arg(&witness)
        .stdin(Stdio::null())
        .output()
        .expect("failed to start the witness's compiler");
    assert!(
        compiled.status.success(),
        "failed to compile the witness: {}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let trial = tempfile::Builder::new()
        .prefix("trial-")
        .tempdir_in(&root)
        .expect("failed to create trial directory");
    let report = trial.path().join("verify.json");
    let output = Command::new("timeout")
        .args(["--kill-after=5s", "120s"])
        .arg(hermit_binary::hermit_binary())
        .env_remove("LD_LIBRARY_PATH")
        .arg("--log=info")
        .args(cell_run_args("ptrace"))
        .args(["--verify", "--verify-strict", "--verify-json"])
        .arg(&report)
        .arg("--")
        .arg(&witness)
        .stdin(Stdio::null())
        .output()
        .expect("failed to run hermit");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let expected = format!("wait_result={} wait_errno=0 ", libc::SIGUSR1);
    assert!(
        output.status.success() && stdout.contains(&expected),
        "expected `{expected}`, as Linux returns, with exit 0; got {}\nstdout:\n{stdout}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let verdict: serde_json::Value =
        serde_json::from_slice(&fs::read(&report).expect("strict verification wrote no verdict"))
            .expect("strict verification verdict is valid JSON");
    if let Some(gap) = strict_verification_gap(&verdict) {
        panic!("strict verification is not determinism evidence: {gap}");
    }
    let _ = fs::remove_file(&witness);
}

/// A pselect6 with nfds=65 sleeps outside the scheduler, and a sibling
/// rewrites its mask buffer before the call reaches the kernel
/// (tests/c/pselect_large_mutating_mask.c). The call must sleep under the mask
/// it was entered with and return 1 when the sibling writes the pipe after its
/// 20 ms sleep, as natively. The scheduler waited for a mask the kernel never
/// installed, withheld the sibling's timer, and the call returned 0 at its 2 s
/// timeout (Codex review of https://github.com/rrnewton/hermit/pull/4053).
#[test]
fn ptrace_large_pselect6_sleeps_under_its_entry_mask_while_a_sibling_rewrites_it() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let root = build_root();
    fs::create_dir_all(&root).expect("failed to create guest build directory");
    let witness = root.join(format!(
        "pselect_large_mutating_mask.{}",
        std::process::id()
    ));
    let compiled = Command::new("cc")
        .args(["-O2", "-Wall", "-Wextra", "-Werror", "-pthread"])
        .arg(repository.join("tests/c/pselect_large_mutating_mask.c"))
        .arg("-o")
        .arg(&witness)
        .stdin(Stdio::null())
        .output()
        .expect("failed to start the witness's compiler");
    assert!(
        compiled.status.success(),
        "failed to compile the witness: {}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    for mode in ["stable", "mutable"] {
        let trial = tempfile::Builder::new()
            .prefix("trial-")
            .tempdir_in(&root)
            .expect("failed to create trial directory");
        let report = trial.path().join("verify.json");
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "120s"])
            .arg(hermit_binary::hermit_binary())
            .env_remove("LD_LIBRARY_PATH")
            .arg("--log=info")
            .args(cell_run_args("ptrace"))
            .args(["--verify", "--verify-strict", "--verify-json"])
            .arg(&report)
            .arg("--")
            .arg(&witness)
            .arg(mode)
            .stdin(Stdio::null())
            .output()
            .expect("failed to run hermit");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let expected = format!("mode={mode} result=1 errno=0 ready=1 writer_ok=1 oracle=1");
        assert!(
            output.status.success() && stdout.contains(&expected),
            "{mode}: expected `{expected}`, as natively, with exit 0; got {}\nstdout:\n{stdout}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        let verdict: serde_json::Value = serde_json::from_slice(
            &fs::read(&report).expect("strict verification wrote no verdict"),
        )
        .expect("strict verification verdict is valid JSON");
        if let Some(gap) = strict_verification_gap(&verdict) {
            panic!("{mode}: strict verification is not determinism evidence: {gap}");
        }
    }
    let _ = fs::remove_file(&witness);
}

/// A pselect6 with more than 64 descriptors and a temporary mask is valid on a
/// stack with no writable memory below its red zone, and Hermit stages its
/// private copy of the mask (`stage_private_cells`) somewhere else there.
/// tests/c/pselect_tight_stack.c makes a ready call, a call ended by a SIGALRM
/// that only the temporary mask unblocks, a call the kernel restarts for an
/// ignored SIGCONT (re-reading its mask), a ready call again, and a ready call
/// whose fd set and timeout sit in the red zone, and (at the adjacent and ample
/// depths) one whose fd set fills it, on a stack
/// 144, 160 and 1024 bytes into its page, then two batches of four threads
/// make the ready call on stacks of their own, and the bytes mapped must not
/// change; a pattern in the red zone below each call's stack must survive it.
/// It must print what Linux prints. Staging the copy on the stack
/// returned EFAULT in the tight mode; unmapping a transient page after the
/// call restarted the signal-ended call forever; a per-thread page left a
/// mapping behind for every thread that used it; staging at the top of the
/// red zone corrupted an fd set the guest kept there (Codex re-checks of
/// https://github.com/rrnewton/hermit/pull/4053). A call whose fd set fills
/// the red zone leaves no room for the copy and is refused by name.
#[test]
fn ptrace_large_pselect6_on_a_stack_with_no_scratch_behaves_as_natively() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let root = build_root();
    fs::create_dir_all(&root).expect("failed to create guest build directory");
    let witness = root.join(format!("pselect_tight_stack.{}", std::process::id()));
    let compiled = Command::new("cc")
        .args(["-O2", "-Wall", "-Wextra", "-Werror", "-pthread"])
        .arg(repository.join("tests/c/pselect_tight_stack.c"))
        .arg("-o")
        .arg(&witness)
        .stdin(Stdio::null())
        .output()
        .expect("failed to start the witness's compiler");
    assert!(
        compiled.status.success(),
        "failed to compile the witness: {}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    for mode in ["tight", "adjacent", "ample"] {
        let trial = tempfile::Builder::new()
            .prefix("trial-")
            .tempdir_in(&root)
            .expect("failed to create trial directory");
        let report = trial.path().join("verify.json");
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "120s"])
            .arg(hermit_binary::hermit_binary())
            .env_remove("LD_LIBRARY_PATH")
            .arg("--log=info")
            .args(cell_run_args("ptrace"))
            .args(["--verify", "--verify-strict", "--verify-json"])
            .arg(&report)
            .arg("--")
            .arg(&witness)
            .arg(mode)
            .stdin(Stdio::null())
            .output()
            .expect("failed to run hermit");
        let stdout = String::from_utf8_lossy(&output.stdout);
        // The crowded call leaves no red-zone window on the tight stack, so
        // only adjacent and ample make it here; mode crowded covers tight.
        let crowded = if mode == "tight" { "none" } else { "1" };
        let expected = format!(
            "mode={mode} ready=1 signal=-4 alarms=1 restart=0 again=1 aliased=1 crowded={crowded} maps=1 threads=1 red_zone=1 oracle=1"
        );
        assert!(
            output.status.success() && stdout.contains(&expected),
            "{mode}: expected `{expected}`, as natively, with exit 0; got {}\nstdout:\n{stdout}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        let verdict: serde_json::Value = serde_json::from_slice(
            &fs::read(&report).expect("strict verification wrote no verdict"),
        )
        .expect("strict verification verdict is valid JSON");
        if let Some(gap) = strict_verification_gap(&verdict) {
            panic!("{mode}: strict verification is not determinism evidence: {gap}");
        }
    }
    // Mode crowded: nfds 1024 and an fd set filling the red zone leave no room
    // for the private copy. A fail-closed run refuses the call by name, never
    // forwarding a mask the scheduler did not record (Codex re-check #4); a
    // run that allows unsupported operations gets Linux's answer.
    let refused = Command::new("timeout")
        .args(["--kill-after=5s", "120s"])
        .arg(hermit_binary::hermit_binary())
        .env_remove("LD_LIBRARY_PATH")
        .args(cell_run_args("ptrace"))
        .arg("--")
        .arg(&witness)
        .arg("crowded")
        .stdin(Stdio::null())
        .output()
        .expect("failed to run hermit");
    let refused_stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        refused.status.code() == Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT)
            && refused_stderr
                .contains("unsupported: pselect6 with more than 64 descriptors and a temporary")
            && !String::from_utf8_lossy(&refused.stdout).contains("mode=crowded"),
        "crowded: expected the named policy refusal; got {}\nstdout:\n{}\nstderr:\n{refused_stderr}",
        refused.status,
        String::from_utf8_lossy(&refused.stdout)
    );
    let allowed = Command::new("timeout")
        .args(["--kill-after=5s", "120s"])
        .arg(hermit_binary::hermit_binary())
        .env_remove("LD_LIBRARY_PATH")
        .args(["--backend", "ptrace", "run", "--allow-unsupported-syscalls"])
        .args(["--base-env=minimal", "--"])
        .arg(&witness)
        .arg("crowded")
        .stdin(Stdio::null())
        .output()
        .expect("failed to run hermit");
    let allowed_stdout = String::from_utf8_lossy(&allowed.stdout);
    assert!(
        allowed.status.success() && allowed_stdout.contains("mode=crowded crowded=1 oracle=1"),
        "crowded with --allow-unsupported-syscalls: expected Linux's answer; got {}\nstdout:\n{allowed_stdout}\nstderr:\n{}",
        allowed.status,
        String::from_utf8_lossy(&allowed.stderr)
    );
    let _ = fs::remove_file(&witness);
}

/// A ppoll on a stack with no writable memory below its red zone is valid,
/// and Hermit stages its private zero timeout and mask copy in the red zone,
/// clear of the pollfd array and, for a call the kernel waits in, the timeout
/// (`stage_private_cells`). Each mode of tests/c/record_replay_select.c below
/// must print what Linux prints, under strict verification, with the pattern
/// it keeps in the red zone intact. These calls returned EFAULT before
/// (https://github.com/rrnewton/hermit/issues/4113). At 8 bytes no 24-byte
/// window is writable, and a fail-closed run refuses the call by name.
#[test]
fn ptrace_ppoll_on_a_stack_with_no_scratch_behaves_as_natively() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let root = build_root();
    fs::create_dir_all(&root).expect("failed to create guest build directory");
    let witness = root.join(format!("ppoll_tight_stack.{}", std::process::id()));
    let compiled = Command::new("cc")
        .args(["-O2", "-Wall", "-Wextra", "-Werror", "-pthread"])
        .arg(repository.join("tests/c/record_replay_select.c"))
        .arg("-o")
        .arg(&witness)
        .stdin(Stdio::null())
        .output()
        .expect("failed to start the witness's compiler");
    assert!(
        compiled.status.success(),
        "failed to compile the witness: {}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    for (mode, expected) in [
        (
            "ppoll-mask-zero-raise-tight24",
            "result=-0000000004 handled=1 revents=00",
        ),
        (
            "ppoll-mask-raise-tight24",
            "result=-0000000004 handled=1 revents=00",
        ),
        (
            "ppoll-mask-raise-tight128",
            "result=-0000000004 handled=1 revents=00",
        ),
        (
            "ppoll-zero-tight16",
            "result=00000000000 handled=0 revents=00",
        ),
        (
            "ppoll-mask-redzone-pollfd",
            "result=00000000001 handled=0 revents=01",
        ),
    ] {
        let trial = tempfile::Builder::new()
            .prefix("trial-")
            .tempdir_in(&root)
            .expect("failed to create trial directory");
        let report = trial.path().join("verify.json");
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "120s"])
            .arg(hermit_binary::hermit_binary())
            .env_remove("LD_LIBRARY_PATH")
            .arg("--log=info")
            .args(cell_run_args("ptrace"))
            .args(["--verify", "--verify-strict", "--verify-json"])
            .arg(&report)
            .arg("--")
            .arg(&witness)
            .arg(mode)
            .stdin(Stdio::null())
            .output()
            .expect("failed to run hermit");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let expected = format!("{expected} red_zone_changed=000");
        assert!(
            output.status.success() && stdout.contains(&expected),
            "{mode}: expected `{expected}`, as natively, with exit 0; got {}\nstdout:\n{stdout}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        let verdict: serde_json::Value = serde_json::from_slice(
            &fs::read(&report).expect("strict verification wrote no verdict"),
        )
        .expect("strict verification verdict is valid JSON");
        if let Some(gap) = strict_verification_gap(&verdict) {
            panic!("{mode}: strict verification is not determinism evidence: {gap}");
        }
    }
    let refused = Command::new("timeout")
        .args(["--kill-after=5s", "120s"])
        .arg(hermit_binary::hermit_binary())
        .env_remove("LD_LIBRARY_PATH")
        .args(cell_run_args("ptrace"))
        .arg("--")
        .arg(&witness)
        .arg("ppoll-mask-raise-tight8")
        .stdin(Stdio::null())
        .output()
        .expect("failed to run hermit");
    let refused_stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        refused.status.code() == Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT)
            && refused_stderr
                .contains("unsupported: ppoll on a stack with no room below its red zone")
            && !String::from_utf8_lossy(&refused.stdout).contains("ppoll-mask-raise-tight8"),
        "ppoll-mask-raise-tight8: expected the named policy refusal; got {}\nstdout:\n{}\nstderr:\n{refused_stderr}",
        refused.status,
        String::from_utf8_lossy(&refused.stdout)
    );
    let _ = fs::remove_file(&witness);
}

#[test]
fn ptrace_poll_and_epoll_are_ended_by_the_sigchld_of_an_exiting_child() {
    assert_child_exit_sigchld_ends_readiness_waits("ptrace");
}

/// The same child-exit SIGCHLD ends both `select`s with EINTR near the exit,
/// as on Linux, each strict-verified `SIGCHLD_TRIALS` times: select and
/// pselect6 are not gated.
fn assert_child_exit_ends_selects(backend: &str) {
    for _ in 0..SIGCHLD_TRIALS {
        for call in ["select", "rawselect"] {
            assert_woken_cell(
                backend,
                FutexMode::Precise,
                &[call, "exit"],
                &format!("RESULT call={call} ret=-1 errno=EINTR handler=1"),
            );
        }
    }
}

#[test]
fn ptrace_selects_are_ended_by_a_caught_sigchld_from_an_exiting_child() {
    assert_child_exit_ends_selects("ptrace");
}

/// A child's SIGCHLD goes to the thread that forked it when that thread does
/// not block it and is running (Linux's `complete_signal`). Here a sibling forks
/// a child that dies at once and stays runnable until the handler has run, so
/// the main thread's futex wait is not interrupted. A timed wait returns
/// ETIMEDOUT at its original 300 ms deadline; an untimed one is ended by the
/// sibling's FUTEX_WAKE 100 ms after the handler ran, which precise mode reports
/// as 0 and polling mode as EAGAIN, as in
/// `untimed_futex_wait_restarts_under_sa_restart`. The child dies by
/// `exit_group`, by SIGKILL, and by its only thread calling `exit`. Each wait
/// runs from a cold and from a warm call site (the guest's `warm` option, which
/// matters to a backend that patches a call site at its first execution).
/// Timed and untimed waits are separate tests so that each test stays within
/// two thirds of the per-test CPU budget.
///
/// Before https://github.com/rrnewton/hermit/pull/3361 was fixed Hermit also
/// woke the main thread for the sibling's SIGCHLD in precise mode. The kernel
/// then restarted a timed wait with a fresh timeout, ending it near 400 ms, and
/// at LiteInst's patched site the kernel-internal errno 512 or 514 reached the
/// guest. Polling mode already left the wait alone; its cells guard that. The
/// ptrace precise untimed cell also passes without the fix, because there the
/// kernel restarts an untimed `FUTEX_WAIT` transparently.
fn assert_runnable_sibling_takes_the_sigchld(backend: &str, mode: FutexMode, timed: bool) {
    let (expected, elapsed) = match (timed, mode) {
        (true, _) => (
            "RESULT call=futex ret=-1 errno=ETIMEDOUT handler=1",
            QUIET_TIMEOUT_MS..QUIET_TIMEOUT_MS + QUIET_OVERSHOOT_MS,
        ),
        (false, FutexMode::Precise) => (
            "RESULT call=futex ret=0 errno=none handler=1",
            STEAL_WAKE_MS..STEAL_WAKE_MS + QUIET_OVERSHOOT_MS,
        ),
        (false, FutexMode::Polling) => (
            "RESULT call=futex ret=-1 errno=EAGAIN handler=1",
            STEAL_WAKE_MS..STEAL_WAKE_MS + QUIET_OVERSHOOT_MS,
        ),
    };
    for death in ["stealgrp", "stealkill", "stealthrexit"] {
        for warm in [false, true] {
            let mut args = vec!["futex", "thread"];
            if timed {
                args.push("timed");
            }
            if warm {
                args.push("warm");
            }
            args.push(death);
            assert_role_cell(
                backend,
                mode,
                &args,
                expected,
                HANDLED_BY_SIBLING,
                elapsed.clone(),
            );
        }
    }
}

#[test]
fn ptrace_precise_timed_futex_wait_keeps_its_deadline_when_a_runnable_sibling_takes_the_sigchld() {
    assert_runnable_sibling_takes_the_sigchld("ptrace", FutexMode::Precise, true);
}

#[test]
fn ptrace_precise_untimed_futex_wait_keeps_waiting_when_a_runnable_sibling_takes_the_sigchld() {
    assert_runnable_sibling_takes_the_sigchld("ptrace", FutexMode::Precise, false);
}

#[test]
fn ptrace_polling_timed_futex_wait_keeps_its_deadline_when_a_runnable_sibling_takes_the_sigchld() {
    assert_runnable_sibling_takes_the_sigchld("ptrace", FutexMode::Polling, true);
}

#[test]
fn ptrace_polling_untimed_futex_wait_keeps_waiting_when_a_runnable_sibling_takes_the_sigchld() {
    assert_runnable_sibling_takes_the_sigchld("ptrace", FutexMode::Polling, false);
}

/// As above, but the sibling that forks the child, which dies 100 ms later,
/// then parks in a 300 ms FUTEX_WAIT of its own. For a child that calls
/// `exit_group` (`forkgrp`), the scheduler commits the SIGCHLD to the sibling
/// through its child-exit timer, which ends the sibling's wait with EINTR near
/// 100 ms, as on Linux (https://github.com/rrnewton/hermit/issues/4005). For a
/// child that dies by SIGKILL (`forkkill`) or by its only thread's `exit`
/// (`forkthrexit`) there is no child-exit timer and the only SIGCHLD is the
/// kernel's, posted at a host-timed moment, so Hermit holds it until the
/// sibling's call returns at its 300 ms deadline; Linux ends that wait with
/// EINTR too. The main thread's timed wait runs to ETIMEDOUT at its 300 ms
/// deadline in every case. Round 9 of
/// https://github.com/rrnewton/hermit/pull/3361 had made every sibling wait
/// hold the SIGCHLD.
fn assert_parked_forker_takes_the_sigchld(backend: &str) {
    for mode in [FutexMode::Precise, FutexMode::Polling] {
        for death in ["forkgrp", "forkkill", "forkthrexit"] {
            let args = ["futex", "thread", "timed", death];
            let run = assert_role_cell(
                backend,
                mode,
                &args,
                "RESULT call=futex ret=-1 errno=ETIMEDOUT handler=1",
                HANDLED_BY_SIBLING,
                QUIET_TIMEOUT_MS..QUIET_TIMEOUT_MS + QUIET_OVERSHOOT_MS,
            );
            let ms = run.sibling_ms();
            let (prefix, window) = if death == "forkgrp" {
                (
                    "SIBLING ret=-1 errno=EINTR ms=",
                    SIGNAL_DELAY_MS..SIGNAL_DELAY_MS + WAKE_SLACK_MS,
                )
            } else {
                (
                    "SIBLING ret=-1 errno=ETIMEDOUT ms=",
                    QUIET_TIMEOUT_MS..QUIET_TIMEOUT_MS + QUIET_OVERSHOOT_MS,
                )
            };
            assert!(
                run.line("SIBLING ")
                    .is_some_and(|line| line.starts_with(prefix))
                    && ms.is_some_and(|ms| window.contains(&ms)),
                "{backend} {mode:?} {args:?}: expected the sibling's wait to end as \
                 `{prefix}` within {window:?} ms, not after {ms:?} ms\n{}",
                run.describe()
            );
        }
    }
}

#[test]
fn ptrace_futex_wait_of_the_thread_that_forked_the_child_takes_its_sigchld() {
    assert_parked_forker_takes_the_sigchld("ptrace");
}

/// The thread that forked the child does not take its SIGCHLD while it waits,
/// although Linux offers the signal to that thread first. Here the main thread
/// forks a child that dies after 100 ms and waits, while a sibling that does
/// not block SIGCHLD spins in user code, with no system calls, until a handler
/// has run. The handler runs once, on the sibling, before the main thread's
/// wait returns. Linux instead ends the main thread's wait with EINTR near the
/// child's death and runs the handler there. Hermit's main branch behaved the
/// same way before https://github.com/rrnewton/hermit/pull/3361: the spinning
/// sibling took the SIGCHLD there too. Precise mode only; see the module
/// documentation for polling mode.
///
/// - Untimed (`timed` is false): the sibling then stores 1 to the futex word
///   and wakes it, within `SPIN_WAKE_BOUND_MS` for every death: at 456-457 ms
///   when the child calls `exit_group` or its only thread calls `exit`, and at
///   254 ms when it sends itself SIGKILL (measured on 2026-10-05).
///   - After the `exit` and SIGKILL deaths, the main thread's wait, woken for
///     the SIGCHLD, has parked again by then, and the wake ends it: 0.
///   - After the `exit_group` death it has not run again yet, so it is off the
///     waiter list and the wake finds no waiter. It then re-reads its changed
///     word, as Linux's retry does, and returns EAGAIN
///     (https://github.com/rrnewton/hermit/issues/4033). Before that fix it
///     returned 0, which a waiter the wake never reached cannot answer.
/// - Timed (`timed`, a 10 s timeout): the sibling does not wake the futex, so
///   the held SIGCHLD neither ends nor restarts the wait, and the wait returns
///   ETIMEDOUT at its original deadline, within `QUIET_OVERSHOOT_MS` of
///   `LONG_TIMEOUT_MS`, for every death (at 10003 ms for all three, measured
///   on 2026-10-06). The child dies at least `EXIT_WAKE_FLOOR_MS` into the
///   wait, so a wait its SIGCHLD restarted with a fresh timeout would return
///   at least 100 ms late, and a wait the SIGCHLD ended would return EINTR.
///   Only this cell checks that the wait ends at its deadline: the untimed
///   cell's wake would hide a wait that never did.
///
/// Until round 9 of https://github.com/rrnewton/hermit/pull/3361 both cells
/// asserted that EINTR and the main thread's handler. The untimed and the timed
/// cells are separate tests, so that each stays inside the per-test CPU and
/// wall bounds with room to spare.
fn assert_waiting_forker_leaves_the_sigchld_to_the_sibling(backend: &str, timed: bool) {
    for death in ["spin", "spinkill", "spinthrexit"] {
        let timed_args = ["futex", "exit", "timed", death];
        let untimed_args = ["futex", "exit", death];
        let (args, expected, elapsed) = if timed {
            (
                timed_args.as_slice(),
                "RESULT call=futex ret=-1 errno=ETIMEDOUT handler=1",
                LONG_TIMEOUT_MS..LONG_TIMEOUT_MS + QUIET_OVERSHOOT_MS,
            )
        } else {
            (
                untimed_args.as_slice(),
                // Only after an `exit_group` death is the woken wait still off
                // the waiter list when the sibling's wake comes (see above).
                if death == "spin" {
                    "RESULT call=futex ret=-1 errno=EAGAIN handler=1"
                } else {
                    "RESULT call=futex ret=0 errno=none handler=1"
                },
                EXIT_WAKE_FLOOR_MS..SPIN_WAKE_BOUND_MS,
            )
        };
        assert_role_cell(
            backend,
            FutexMode::Precise,
            args,
            expected,
            HANDLED_BY_SIBLING,
            elapsed,
        );
    }
}

#[test]
fn ptrace_precise_futex_wait_leaves_its_childs_sigchld_to_a_running_sibling() {
    assert_waiting_forker_leaves_the_sigchld_to_the_sibling("ptrace", false);
}

#[test]
fn ptrace_precise_timed_futex_wait_leaves_its_childs_sigchld_to_a_running_sibling() {
    assert_waiting_forker_leaves_the_sigchld_to_the_sibling("ptrace", true);
}

/// A thread other than the thread-group leader forks a child and waits for it in
/// `rt_sigsuspend` with SIGCHLD unblocked, while the leader blocks SIGCHLD and
/// waits in `pthread_join`. Linux sends the child's SIGCHLD to the thread that
/// forked it, so each of the guest's six trials ends with the creator's handler
/// running once and `sigsuspend` returning EINTR after the child's exit, and the
/// cell is strict-verified. Hermit's scheduler does not keep a thread in
/// `rt_sigsuspend` on its run queue; it delivers the signal by arming the
/// sleeper and ordering its continuation at a fixed point of the schedule. A
/// second run, with Hermit's INFO log written to a file, counts the arm and
/// release lines of that path: one each per trial. Before
/// https://github.com/rrnewton/hermit/pull/3361 round 7, Hermit panicked when the
/// child's exit sent its SIGCHLD to the sleeping creator ("should be parked",
/// exit status 125).
fn assert_nonleader_creator_takes_its_childs_sigchld_in_sigsuspend(backend: &str) {
    let args = ["sigsuspend", "creator"];
    let expected = format!(
        "RESULT call=sigsuspend role=creator trials={CREATOR_TRIALS} matched={CREATOR_TRIALS}"
    );
    assert_cell(backend, FutexMode::Precise, &args, false, &expected);
    let run = run_cell_observed(backend, FutexMode::Precise, &args, &[], Observe::InfoLog);
    assert!(
        run.status.success()
            && run.result_line() == Some(expected.as_str())
            && run.stdout.lines().any(|line| line == "DONE"),
        "{backend} {args:?} with an INFO log: expected `{expected}`\n{}",
        run.describe()
    );
    let log = run
        .info_log
        .as_deref()
        .expect("an InfoLog run keeps its log");
    let armed = log.matches(SIGNALED_BACKGROUND_ARMED).count();
    let released = log.matches(SIGNALED_BACKGROUND_RELEASED).count();
    assert!(
        armed == CREATOR_TRIALS && released == CREATOR_TRIALS,
        "{backend} {args:?}: expected {CREATOR_TRIALS} `{SIGNALED_BACKGROUND_ARMED}` and \
         {CREATOR_TRIALS} `{SIGNALED_BACKGROUND_RELEASED}` INFO lines, one per trial; found \
         {armed} and {released}\n{}",
        run.describe()
    );
}

#[test]
fn ptrace_nonleader_creator_takes_its_childs_sigchld_in_sigsuspend() {
    assert_nonleader_creator_takes_its_childs_sigchld_in_sigsuspend("ptrace");
}

/// A child that armed SIGCHLD as its parent-death signal (`PR_SET_PDEATHSIG`)
/// polls no descriptors for 300 ms while another guest process kills its parent,
/// which sleeps outside Hermit's run queue (the guest's `poll pdeathchld` mode).
/// The child has never had a child. Linux returns EINTR when the parent dies.
/// Under Hermit the parent exits physically before the scheduler deregisters it,
/// at an instant set by the host, so counting that SIGCHLD would end the poll at
/// a host-dependent turn. Hermit keeps the behaviour from before the external
/// signal work instead: in each trial the poll returns 0 after its full timeout
/// (the guest's 300 ms and 50 ms overshoot match `QUIET_TIMEOUT_MS` and
/// `QUIET_OVERSHOOT_MS`) and the handler runs once afterwards. Round 7 of
/// https://github.com/rrnewton/hermit/pull/3361 counted that SIGCHLD under its
/// childless-process exception (round-8 High 3) and returned EINTR. Round 9
/// removed that exception: no SIGCHLD ends a gated wait.
///
/// The cell runs `PDEATH_RUNS` times under `--strict` without `--verify`, and
/// every trial of every run must keep its deadline. It is not strict-verified:
/// killing a parked process from another guest is itself host-timed in Hermit.
/// The tracer retires the killed parent at a host-set point relative to the
/// killer's own `kill` completion and to the killer's SIGCHLD, so two runs'
/// INFO logs differ on the killer's side whatever the child does. That race is
/// the physical-death-before-logical-deregistration race the finding names; this
/// pull request does not close it, so it claims only the child's result here.
fn assert_parent_death_sigchld_keeps_a_childless_poll_to_its_deadline(backend: &str) {
    assert_parent_death_signal_keeps_a_poll_to_its_deadline(backend, "pdeathchld");
}

/// Runs the guest's `poll <role>` parent-death mode `PDEATH_RUNS` times under
/// `--strict` without `--verify`; every trial of every run must keep its deadline.
fn assert_parent_death_signal_keeps_a_poll_to_its_deadline(backend: &str, role: &str) {
    let args = ["poll", role];
    let expected =
        format!("RESULT call=poll role={role} trials={PDEATH_TRIALS} matched={PDEATH_TRIALS}");
    for attempt in 0..PDEATH_RUNS {
        let run = run_cell_observed(backend, FutexMode::Precise, &args, &[], Observe::InfoLog);
        assert!(
            run.status.success()
                && run.result_line() == Some(expected.as_str())
                && run.stdout.lines().any(|line| line == "DONE"),
            "{backend} {args:?} run {attempt} of {PDEATH_RUNS}: expected `{expected}`\n{}",
            run.describe()
        );
    }
}

#[test]
fn ptrace_parent_death_sigchld_keeps_a_childless_poll_to_its_deadline() {
    assert_parent_death_sigchld_keeps_a_childless_poll_to_its_deadline("ptrace");
}

/// The same cell with SIGUSR1 as the parent-death signal (the guest's
/// `poll pdeathusr1` mode). The child catches SIGUSR1 and arms it with
/// `PR_SET_PDEATHSIG`; Linux returns EINTR when the parent dies. Hermit cannot
/// tell the kernel's copy, posted at an instant set by the host, from a SIGUSR1
/// a guest sends at a fixed point, so the arming call records SIGUSR1 as
/// host-timed for the child's process, in the child's turn, and no gated wait of
/// that process ends for it afterwards: in each trial the poll returns 0 after
/// its full timeout and the handler runs once afterwards. Before that record, the
/// poll returned EINTR at a turn the host chose. A guest `kill` of SIGUSR1 to
/// that process is held the same way, which is what every gated wait did before
/// the external signal work (https://github.com/rrnewton/hermit/issues/3146).
///
/// Unverified for the reason given for the SIGCHLD cell: the killer's side of
/// two runs' INFO logs differs whatever the child does.
#[test]
fn ptrace_parent_death_usr1_keeps_a_poll_to_its_deadline() {
    assert_parent_death_signal_keeps_a_poll_to_its_deadline("ptrace", "pdeathusr1");
}

/// A child's exit signal other than SIGCHLD (the guest's `poll exitusr1`
/// mode). A process catches SIGUSR1, creates a child with the raw clone system
/// call and SIGUSR1 as its exit signal, and polls; the child exits 50 ms later.
/// Linux returns EINTR when the child exits. Under Hermit the kernel posts that
/// SIGUSR1 at the child's physical exit, an instant set by the host, so the
/// clone call records SIGUSR1 as host-timed for the creating process, in its
/// turn and before the child exists, and no gated wait of that process ends for
/// it afterwards: in each trial the poll returns 0 after its full timeout and
/// the handler runs once afterwards. Before that record, the poll returned EINTR
/// at 53 ms in every measured trial, at the first turn after the host posted the
/// signal (https://github.com/rrnewton/hermit/issues/3146).
/// Unlike the parent-death cells, no guest kills a parked process here, so the
/// cell runs once under strict verification and must show bitwise parity.
#[test]
fn ptrace_child_exit_usr1_keeps_a_poll_to_its_deadline() {
    let backend = "ptrace";
    let args = ["poll", "exitusr1"];
    let expected =
        format!("RESULT call=poll role=exitusr1 trials={PDEATH_TRIALS} matched={PDEATH_TRIALS}");
    let run = run_cell(backend, FutexMode::Precise, &args, false);
    assert!(
        run.status.success()
            && run.result_line() == Some(expected.as_str())
            && run.stdout.lines().any(|line| line == "DONE"),
        "{backend} {args:?}: expected `{expected}`\n{}",
        run.describe()
    );
    assert_verified(backend, FutexMode::Precise, &args, &run);
}

/// A terminal hangup (the guest's `poll hupopen` and `poll hupctty` modes). A
/// session leader makes a pseudoterminal's slave its controlling terminal and
/// sleeps outside Hermit's run queue; its child, in the terminal's foreground
/// group, catches SIGHUP and polls no descriptors for 300 ms while another
/// guest process kills the leader. When a session leader whose controlling
/// terminal is a pseudoterminal exits, Linux sends SIGHUP to the terminal's
/// foreground group, so natively the poll returns EINTR when the leader dies,
/// 50 ms in. Under Hermit the leader exits physically at an instant set by the
/// host, so the call that gives the session its controlling terminal records
/// SIGHUP and SIGCONT as host-timed for every process of the container, in the
/// caller's turn, and no gated wait ends for them afterwards: in each trial the
/// poll returns 0 after its full timeout and the handler runs once afterwards.
/// `hupopen` gains the terminal by opening the slave without O_NOCTTY,
/// `hupctty` by TIOCSCTTY. Round-9 High 2 of
/// https://github.com/rrnewton/hermit/pull/3361: neither call recorded those
/// signals, and in every trial the poll returned EINTR at a turn the host
/// chose, 51 to 54 ms in by the guest's clock
/// (https://github.com/rrnewton/hermit/issues/3146).
///
/// Unverified for the reason given for the parent-death cells: killing a
/// parked process from another guest is itself host-timed in Hermit, so the
/// killer's side of two runs' INFO logs differs whatever the child does.
#[test]
fn ptrace_terminal_hangup_after_open_keeps_a_poll_to_its_deadline() {
    assert_parent_death_signal_keeps_a_poll_to_its_deadline("ptrace", "hupopen");
}

/// The same cell, with the controlling terminal gained by TIOCSCTTY.
#[test]
fn ptrace_terminal_hangup_after_tiocsctty_keeps_a_poll_to_its_deadline() {
    assert_parent_death_signal_keeps_a_poll_to_its_deadline("ptrace", "hupctty");
}

/// The end of the guest's `poll hupinherit` or `poll winchinherit` RESULT line
/// when the poll keeps its deadline and the handler runs once afterwards: the
/// line is `TerminalEvent::kept_prefix`, the guest's `elapsed` milliseconds,
/// then this suffix.
const INHERITED_KEPT_SUFFIX: &str = " handled=1 ok=1";

/// What this process does to the guest of an inherited-terminal cell
/// `EXTERNAL_SIGNAL_DELAY` after the guest's READY line, from outside the
/// container.
#[derive(Clone, Copy, Debug)]
enum TerminalEvent {
    /// Send the guest SIGHUP, standing for the terminal's hangup (the guest's
    /// `poll hupinherit` mode).
    Hangup,
    /// Set a new window size on the terminal's master (TIOCSWINSZ), as a
    /// user's resize would, so that Linux sends SIGWINCH to the terminal's
    /// foreground process group, which holds the guest (the guest's
    /// `poll winchinherit` mode).
    Resize,
}

impl TerminalEvent {
    /// The guest's mode for this event, which is also its RESULT line's role.
    fn role(self) -> &'static str {
        match self {
            TerminalEvent::Hangup => "hupinherit",
            TerminalEvent::Resize => "winchinherit",
        }
    }

    /// The start of the guest's RESULT line when the poll keeps its deadline,
    /// up to the guest's `elapsed` milliseconds (see `INHERITED_KEPT_SUFFIX`).
    fn kept_prefix(self) -> String {
        format!(
            "RESULT call=poll role={} ret=0 errno=none elapsed=",
            self.role()
        )
    }
}

/// One run of the guest's `poll hupinherit` or `poll winchinherit` mode under
/// a session leader that owns a pseudoterminal.
struct InheritedTerminalRun {
    event: TerminalEvent,
    /// How the session leader that started Hermit ended, which is how Hermit
    /// ended: the leader exits with Hermit's status.
    status: ExitStatus,
    stdout: String,
    stderr: String,
    /// Host wall time from the guest's READY line to its RESULT line.
    ready_to_result: Option<Duration>,
}

impl InheritedTerminalRun {
    fn line(&self, prefix: &str) -> Option<&str> {
        self.stdout.lines().find(|line| line.starts_with(prefix))
    }

    fn has_line(&self, want: &str) -> bool {
        self.stdout.lines().any(|line| line == want)
    }

    fn kept_its_deadline(&self) -> bool {
        self.line("RESULT ").is_some_and(|line| {
            line.starts_with(&self.event.kept_prefix()) && line.ends_with(INHERITED_KEPT_SUFFIX)
        })
    }

    fn describe(&self) -> String {
        format!(
            "status={:?} ready_to_result={:?}\nguest stdout:\n{}\nhermit stderr:\n{}",
            self.status, self.ready_to_result, self.stdout, self.stderr
        )
    }
}

/// Run the guest's mode for `event` under Hermit, started by a shell that
/// leads a new session whose controlling terminal is a pseudoterminal's slave,
/// so that Hermit and the guest inherit that terminal and none of the three
/// standard descriptors is a terminal. `EXTERNAL_SIGNAL_DELAY` after the
/// guest's READY line this process causes `event`, from outside the
/// container: it sends the guest SIGHUP, or it checks that the guest's
/// process group is the terminal's foreground group and sets a new window
/// size on the master. This process keeps the master open until the run ends,
/// so the terminal never hangs up.
fn run_inherited_terminal_cell(namespace: bool, event: TerminalEvent) -> InheritedTerminalRun {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;

    let describe = format!("ptrace poll {} namespace={namespace}", event.role());
    // Compiling the guest also creates the directory that holds every trial.
    let guest = guest();
    let trial = tempfile::Builder::new()
        .prefix("trial-")
        .tempdir_in(build_root())
        .expect("failed to create trial directory");
    let program = trial.path().join("esi");
    std::os::unix::fs::symlink(guest, &program).expect("failed to link the guest");
    let stdout_path = trial.path().join("stdout");
    let stdout_writer = fs::File::create(&stdout_path).expect("failed to create guest stdout");

    let master = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open("/dev/ptmx")
        .expect("failed to open /dev/ptmx");
    let mut name = [0 as libc::c_char; 128];
    // SAFETY: plain calls on a descriptor this process owns; `name` has room
    // for any pseudoterminal path, and ptsname_r terminates it.
    let slave_path = unsafe {
        assert_eq!(libc::grantpt(master.as_raw_fd()), 0, "grantpt failed");
        assert_eq!(libc::unlockpt(master.as_raw_fd()), 0, "unlockpt failed");
        assert_eq!(
            libc::ptsname_r(master.as_raw_fd(), name.as_mut_ptr(), name.len()),
            0,
            "ptsname_r failed"
        );
        std::ffi::CStr::from_ptr(name.as_ptr())
            .to_str()
            .expect("the pseudoterminal path should be UTF-8")
            .to_owned()
    };
    let slave = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(&slave_path)
        .unwrap_or_else(|error| panic!("failed to open {slave_path}: {error}"));
    let slave_fd = slave.as_raw_fd();

    // The shell stays the session leader, as a login shell would, and exits
    // with Hermit's status.
    let mut command = Command::new("/bin/sh");
    command
        // As above: keep the guest's loader out of the build directories.
        .env_remove("LD_LIBRARY_PATH")
        .arg("-c")
        .arg(r#""$@"; exit "$?""#)
        .arg("sh")
        .arg(hermit_binary::hermit_binary())
        .args(["--backend", "ptrace", "run"]);
    // `--strict` refuses `--no-namespace`, which shares the host's network.
    command.arg(if namespace {
        "--strict"
    } else {
        "--no-namespace"
    });
    command
        .arg("--")
        .arg(&program)
        .args(["poll", event.role()])
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_writer))
        .stderr(Stdio::piped());
    // SAFETY: only async-signal-safe calls run between fork and exec, on a
    // descriptor that stays open until exec.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() < 0 || libc::ioctl(slave_fd, libc::TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let (cell, test_deadline) = begin_cell(&describe);
    let mut leader = command
        .spawn()
        .unwrap_or_else(|error| panic!("failed to spawn the session leader: {error}"));
    drop(slave);
    let leader_pid = leader.id() as libc::pid_t;
    let stderr = leader.stderr.take().expect("stderr was piped");
    let (send, receive) = mpsc::channel();
    let reader = thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            match line {
                Ok(line) => {
                    if send.send(StderrEvent::Line(line)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let _ = send.send(StderrEvent::Error(error.to_string()));
                    return;
                }
            }
        }
        let _ = send.send(StderrEvent::Eof);
    });

    let started = Instant::now();
    let (startup_deadline, startup_limit) = phase_deadline(started, STARTUP_BOUND, test_deadline);
    let (cell_deadline, cell_limit) = phase_deadline(started, WATCHDOG_BACKSTOP, test_deadline);
    let mut status: Option<ExitStatus> = None;
    let mut ready_at: Option<Instant> = None;
    let mut result_at: Option<Instant> = None;
    let mut signalled = false;
    let mut lines = Vec::new();
    let mut truncated = false;
    let mut stderr_eof = false;
    let mut failure = None;

    while status.is_none() || !stderr_eof {
        if status.is_none() {
            status = leader
                .try_wait()
                .expect("failed to poll the session leader");
        }
        let stdout = fs::read_to_string(&stdout_path).unwrap_or_default();
        let now = Instant::now();
        if ready_at.is_none() && stdout.lines().any(|line| line == "READY") {
            ready_at = Some(now);
        }
        if result_at.is_none() && stdout.lines().any(|line| line.starts_with("RESULT ")) {
            result_at = Some(now);
        }
        if failure.is_none() && ready_at.is_none() && status.is_none() && now >= startup_deadline {
            failure = Some(format!(
                "cell {cell}: no READY line {:.1}s after starting Hermit, at {startup_limit}",
                (now - started).as_secs_f64()
            ));
        }
        if let Some(ready) = ready_at {
            if !signalled && status.is_none() && ready.elapsed() >= EXTERNAL_SIGNAL_DELAY {
                let pids = pids_with_argv0(&program);
                if let [pid] = pids[..] {
                    failure = match event {
                        TerminalEvent::Hangup => send_terminal_hangup(pid),
                        TerminalEvent::Resize => resize_foreground_terminal(&master, pid),
                    };
                } else {
                    failure = Some(format!(
                        "expected exactly one guest process with argv[0] {}, found {pids:?}",
                        program.display()
                    ));
                }
                signalled = true;
            }
            let (result_deadline, result_limit) =
                phase_deadline(ready, RESULT_BOUND, test_deadline);
            if failure.is_none() && result_at.is_none() && now >= result_deadline {
                failure = Some(format!(
                    "cell {cell}: no RESULT line {:.1}s after READY, at {result_limit}",
                    (now - ready).as_secs_f64()
                ));
            }
        }
        if failure.is_none() && now >= cell_deadline {
            failure = Some(format!(
                "cell {cell}: watchdog deadline {:.1}s after starting Hermit, at {cell_limit}; \
                 status={status:?}, ready={}",
                (now - started).as_secs_f64(),
                ready_at.is_some(),
            ));
        }
        if failure.is_some() {
            break;
        }
        match receive.recv_timeout(Duration::from_millis(10)) {
            Ok(StderrEvent::Line(line)) => {
                if lines.len() < MAX_DIAGNOSTIC_LINES {
                    lines.push(line);
                } else {
                    truncated = true;
                }
            }
            Ok(StderrEvent::Error(error)) => {
                failure = Some(format!("failed while draining hermit stderr: {error}"));
                break;
            }
            Ok(StderrEvent::Eof) | Err(mpsc::RecvTimeoutError::Disconnected) => stderr_eof = true,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }

    if failure.is_some() {
        // The leader's process group holds Hermit and every guest process.
        // SAFETY: plain kill(2).
        unsafe {
            libc::kill(-leader_pid, libc::SIGKILL);
        }
        let _ = leader.kill();
    }
    // Always reap the leader here, on normal exit and failure alike.
    let status = match status {
        Some(status) => status,
        None => leader
            .wait()
            .expect("failed to wait for the session leader"),
    };
    drop(master);
    reader.join().expect("stderr reader panicked");
    let stdout = fs::read_to_string(&stdout_path).expect("failed to read guest stdout");
    let mut stderr = lines.join("\n");
    if truncated {
        stderr.push_str(&format!(
            "\n[watchdog retained the first {MAX_DIAGNOSTIC_LINES} stderr lines]"
        ));
    }
    if let Some(reason) = failure {
        panic!("{describe}: {reason}\nguest stdout:\n{stdout}\nhermit stderr:\n{stderr}");
    }
    let ready_to_result = ready_at
        .zip(result_at)
        .map(|(ready, result)| result - ready);
    eprintln!(
        "[esi] cell {cell}: {}; READY to RESULT {ready_to_result:?} of host wall time",
        stdout
            .lines()
            .find(|line| line.starts_with("RESULT "))
            .unwrap_or("no RESULT line")
    );
    InheritedTerminalRun {
        event,
        status,
        stdout,
        stderr,
        ready_to_result,
    }
}

/// Send guest process `pid` SIGHUP, as the hangup of its terminal would. The
/// failure to report, if any.
fn send_terminal_hangup(pid: libc::pid_t) -> Option<String> {
    // SAFETY: plain kill(2) on a pid read from /proc.
    if unsafe { libc::kill(pid, libc::SIGHUP) } != 0 {
        return Some(format!(
            "kill({pid}, SIGHUP) failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    None
}

/// Check that guest process `pid` is in the foreground process group of the
/// pseudoterminal whose `master` this process holds, then give the terminal a
/// window size it does not have, as a user's resize would: Linux sends
/// SIGWINCH to that group (`pty_resize`). The failure to report, if any.
fn resize_foreground_terminal(master: &fs::File, pid: libc::pid_t) -> Option<String> {
    use std::os::fd::AsRawFd;

    let master = master.as_raw_fd();
    // SAFETY: plain calls on a descriptor this process owns and a pid read
    // from /proc. TIOCGPGRP on a master reads its slave's foreground group.
    let (foreground, group) = unsafe { (libc::tcgetpgrp(master), libc::getpgid(pid)) };
    if foreground <= 0 || foreground != group {
        return Some(format!(
            "guest {pid} is in process group {group}, not in the terminal's foreground \
             process group {foreground}: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCGWINSZ writes one winsize to `size`.
    if unsafe { libc::ioctl(master, libc::TIOCGWINSZ, &mut size) } != 0 {
        return Some(format!(
            "TIOCGWINSZ failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    // Linux signals nothing for a size the terminal already has.
    size.ws_row = if size.ws_row == 40 { 41 } else { 40 };
    size.ws_col = 100;
    // SAFETY: TIOCSWINSZ reads one winsize from `size`.
    if unsafe { libc::ioctl(master, libc::TIOCSWINSZ, &size) } != 0 {
        return Some(format!(
            "TIOCSWINSZ failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    None
}

/// A SIGHUP from outside the container while Hermit holds an inherited
/// controlling terminal (the guest's `poll hupinherit` mode). A shell leads a
/// new session whose controlling terminal is a pseudoterminal and starts
/// Hermit, which the guest inherits the session and the terminal from; the
/// guest neither opens the terminal nor issues TIOCSCTTY, and none of its
/// standard descriptors is a terminal. The guest catches SIGHUP and polls no
/// descriptors for 10 s; 500 ms after its READY line this process sends it
/// SIGHUP. Natively the poll returns EINTR then. Under Hermit that instant is
/// set by the host, and no traced call gives Detcore a turn in which to record
/// it, so Hermit records SIGHUP and SIGCONT as host-timed for the whole
/// container before the guest's first instruction whenever the launch may pass
/// the guest a terminal, and no gated wait ends for them: in each run the poll
/// returns 0 after its full timeout and the handler runs once afterwards.
/// Round-10 High 2 of https://github.com/rrnewton/hermit/pull/3361: nothing
/// recorded those signals for an inherited terminal, and the poll returned
/// EINTR at a turn the host chose
/// (https://github.com/rrnewton/hermit/issues/3146).
///
/// The signal stands for the terminal's own hangup, which cannot be used
/// here: closing the master hangs the terminal up, Linux sends SIGHUP and
/// SIGCONT to the session leader, and the leader's exit sends both to the
/// terminal's foreground process group. That group holds Hermit's container
/// process too, which answers SIGHUP by ending the run with exit code 129
/// (`on_container_init_stop_signal` in hermit-cli's container.rs), in both
/// namespace modes, before the guest's poll returns.
///
/// Each run is a separate launch, `PDEATH_RUNS` of them, and none runs under
/// `--verify`: the signal arrives once per launch, from outside, so the second
/// run of a verified pair would have no signal and would differ in its INFO
/// log whatever Hermit did with the first. `--strict` refuses `--no-namespace`,
/// so this cell runs without it.
#[test]
fn ptrace_an_inherited_terminal_keeps_a_poll_to_its_deadline_through_sighup_without_a_namespace() {
    assert_an_inherited_terminal_keeps_a_poll_to_its_deadline(false, TerminalEvent::Hangup);
}

/// The same cell in the default namespace mode, under `--strict`. A guest
/// started there inherits the launcher's controlling terminal too, since
/// neither Hermit nor Reverie starts a new session or process group for it:
/// the guest prints `CTTY 1`.
#[test]
fn ptrace_an_inherited_terminal_keeps_a_poll_to_its_deadline_through_sighup_in_a_namespace() {
    assert_an_inherited_terminal_keeps_a_poll_to_its_deadline(true, TerminalEvent::Hangup);
}

/// A resize of an inherited controlling terminal while the guest polls (the
/// guest's `poll winchinherit` mode), with the terminal set up as in the
/// SIGHUP cells above. The guest catches SIGWINCH and polls no descriptors for
/// 10 s; 500 ms after its READY line this process checks that the guest's
/// process group is the terminal's foreground process group and sets a new
/// window size on the terminal's master, and Linux sends SIGWINCH to that
/// group. Natively the poll returns EINTR then. Under Hermit that instant is
/// set by the host, so Hermit records SIGWINCH as host-timed for the whole
/// container before the guest's first instruction whenever the launch may
/// pass the guest a terminal, with every other signal a terminal sends, and no
/// gated wait ends for it: in each run the poll returns 0 after its full
/// timeout and the handler runs once afterwards. Round-11 finding "Terminal
/// resize remains an admitted host-timed interruption" of
/// https://github.com/rrnewton/hermit/pull/3361: only SIGHUP and SIGCONT were
/// recorded, and the poll returned EINTR at a turn the host chose
/// (https://github.com/rrnewton/hermit/issues/3146). Runs as the SIGHUP cells
/// do: `PDEATH_RUNS` separate launches, none under `--verify`.
#[test]
fn ptrace_an_inherited_terminal_keeps_a_poll_to_its_deadline_through_a_resize_without_a_namespace()
{
    assert_an_inherited_terminal_keeps_a_poll_to_its_deadline(false, TerminalEvent::Resize);
}

/// The same resize cell in the default namespace mode, under `--strict`.
#[test]
fn ptrace_an_inherited_terminal_keeps_a_poll_to_its_deadline_through_a_resize_in_a_namespace() {
    assert_an_inherited_terminal_keeps_a_poll_to_its_deadline(true, TerminalEvent::Resize);
}

fn assert_an_inherited_terminal_keeps_a_poll_to_its_deadline(
    namespace: bool,
    event: TerminalEvent,
) {
    for run_index in 1..=PDEATH_RUNS {
        let run = run_inherited_terminal_cell(namespace, event);
        assert!(
            run.status.success()
                && run.has_line("CTTY 1")
                && run.kept_its_deadline()
                && run.has_line("DONE"),
            "run {run_index} of {PDEATH_RUNS}, namespace={namespace}, poll {}: expected CTTY 1, \
             `{}<ms>{INHERITED_KEPT_SUFFIX}`, DONE, and Hermit exiting 0\n{}",
            event.role(),
            event.kept_prefix(),
            run.describe()
        );
    }
}

/// `wait4` and `waitid` restart under SA_RESTART, as Linux restarts them: the
/// waited child signals the parent near 100 ms and exits 100 ms later. The
/// handler runs near 100 ms, before `HANDLED_BEFORE_EXIT_MS`, the call does not
/// report EINTR, and the restarted wait returns the child near 200 ms.
/// Strict-verified.
fn assert_child_waits_restart_under_sa_restart(backend: &str) {
    for (call, expected) in [
        ("wait4", "RESULT call=wait4 ret=child errno=none handler=1"),
        ("waitid", "RESULT call=waitid ret=0 errno=none handler=1"),
    ] {
        let args = [call, "process", "restart"];
        let run = run_cell(backend, FutexMode::Precise, &args, false);
        assert!(
            run.status.success()
                && run.result_line() == Some(expected)
                && run.stdout.lines().any(|line| line == "DONE"),
            "{backend} {args:?}: expected `{expected}`\n{}",
            run.describe()
        );
        let elapsed = run.elapsed_ms();
        assert!(
            elapsed.is_some_and(|ms| RESTARTED_WAKE_MS.contains(&ms)),
            "{backend} {args:?}: the wait took {elapsed:?} ms, outside \
             {RESTARTED_WAKE_MS:?}\n{}",
            run.describe()
        );
        let handled_at = run.handled_at_ms();
        assert!(
            handled_at.is_some_and(|ms| {
                (EXIT_WAKE_FLOOR_MS as i64..HANDLED_BEFORE_EXIT_MS as i64).contains(&ms)
            }),
            "{backend} {args:?}: the handler first ran at {handled_at:?} ms, not near the \
             child's signal at {SIGNAL_DELAY_MS} ms: the signal did not interrupt the wait\n{}",
            run.describe()
        );
        assert_verified(backend, FutexMode::Precise, &args, &run);
    }
}

#[test]
fn ptrace_child_waits_restart_under_sa_restart() {
    assert_child_waits_restart_under_sa_restart("ptrace");
}

/// A caught signal and a default-ignored SIGCHLD pending together end a futex
/// wait with EINTR near the signal, timed or not, and the handler runs, as on
/// Linux. The sibling reaps a child, whose SIGCHLD stays pending where only the
/// waiter can take it, and then sends SIGUSR1. Under ptrace the kernel queues
/// even a default-ignored signal and stops the guest for it, so both signals
/// reach the backend, and the backend holds one signal per thread.
/// Strict-verified.
fn assert_caught_signal_with_a_pending_sigchld_ends_futex_wait(backend: &str) {
    for mode in [FutexMode::Precise, FutexMode::Polling] {
        for timed in [false, true] {
            let mut args = vec!["futex", "thread", "chldpend"];
            if timed {
                args.push("timed");
            }
            assert_woken_cell(backend, mode, &args, EINTR_FUTEX);
        }
    }
}

#[test]
fn ptrace_futex_wait_is_ended_by_a_caught_signal_pending_with_a_default_ignored_sigchld() {
    assert_caught_signal_with_a_pending_sigchld_ends_futex_wait("ptrace");
}

/// Two caught signals sent together end a polling futex wait with EINTR, and
/// both handlers run, as on Linux: the harness sends SIGUSR1 and SIGUSR2 back to
/// back from outside Hermit, and the guest reports both handlers on its
/// `HANDLED` line. The backend holds one signal per thread, so a second signal
/// that stops the guest must not replace the first. External signals arrive at
/// host-timed moments, so the cell runs `EXTERNAL_TRIALS` trials, split into
/// two tests of half as many each (`trials`) so that each test stays inside the
/// per-test CPU bound with room to spare.
fn assert_two_external_signals_are_both_delivered(backend: &str, trials: std::ops::Range<usize>) {
    let args = ["futex", "external", "usr2"];
    for trial in trials {
        let run = run_cell_with_external_signals(
            backend,
            FutexMode::Polling,
            &args,
            &[libc::SIGUSR1, libc::SIGUSR2],
        );
        assert!(
            run.status.success()
                && run.result_line() == Some(EINTR_FUTEX)
                && run.line("HANDLED ") == Some("HANDLED usr1=1 usr2=1")
                && run.stdout.lines().any(|line| line == "DONE"),
            "{backend} {args:?} trial {trial}/{EXTERNAL_TRIALS}: expected `{EINTR_FUTEX}` and \
             `HANDLED usr1=1 usr2=1`\n{}",
            run.describe()
        );
    }
}

/// The first half of `EXTERNAL_TRIALS`; the second half is
/// `SECOND_HALF_OF_EXTERNAL_TRIALS`.
const FIRST_HALF_OF_EXTERNAL_TRIALS: std::ops::Range<usize> = 0..EXTERNAL_TRIALS / 2;
const SECOND_HALF_OF_EXTERNAL_TRIALS: std::ops::Range<usize> = EXTERNAL_TRIALS / 2..EXTERNAL_TRIALS;

#[test]
fn ptrace_polling_futex_wait_delivers_both_of_two_external_signals() {
    assert_two_external_signals_are_both_delivered("ptrace", FIRST_HALF_OF_EXTERNAL_TRIALS);
}

#[test]
fn ptrace_polling_futex_wait_delivers_both_of_two_external_signals_in_later_trials() {
    assert_two_external_signals_are_both_delivered("ptrace", SECOND_HALF_OF_EXTERNAL_TRIALS);
}
