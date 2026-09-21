/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Regression coverage for blocking `waitid` progress and Linux's
//! child-ready-before-interrupt precedence.
//!
//! The liveness defect is an unbounded polling loop. The watchdog therefore
//! lives in this host process, outside both the guest and every blocking pipe
//! read. A dedicated reader drains Hermit's stderr while this thread polls the
//! process and an independent wall-clock deadline.

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

const MAX_WAITID_RETRIES: usize = 20_000;
const WATCHDOG_BACKSTOP: Duration = Duration::from_secs(60);
const MAX_STDERR_EVENTS_PER_WATCHDOG_TICK: usize = 256;
const MAX_DIAGNOSTIC_LINES: usize = 2_000;

#[derive(Clone, Copy)]
struct WatchdogLimits {
    max_waitid_retries: usize,
    backstop: Duration,
    max_stderr_events_per_tick: usize,
}

const WATCHDOG_LIMITS: WatchdogLimits = WatchdogLimits {
    max_waitid_retries: MAX_WAITID_RETRIES,
    backstop: WATCHDOG_BACKSTOP,
    max_stderr_events_per_tick: MAX_STDERR_EVENTS_PER_WATCHDOG_TICK,
};

static WAITID_GUEST: OnceLock<PathBuf> = OnceLock::new();

struct GuestRun {
    status: ExitStatus,
    stdout: String,
    stderr: String,
    retries: usize,
}

enum StderrEvent {
    Line(String),
    Error(String),
    Eof,
}

#[derive(Debug, Eq, PartialEq)]
enum DrainOutcome {
    QueueEmpty,
    BudgetExhausted,
    Eof,
}

fn waitid_guest() -> &'static Path {
    WAITID_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("waitid-signal-interrupt");
        fs::create_dir_all(&build_root).expect("failed to create waitid guest build directory");

        // The guest must not live under /tmp: Hermit replaces guest /tmp with
        // an isolated directory, making a host /tmp fixture invisible.
        let guest = build_root.join("waitid_signal_interrupt");
        let source = repository.join("tests/c/waitid_signal_interrupt.c");
        let compile = Command::new("cc")
            // `-pthread` is required by the thread-directed sender mode, which
            // uses `pthread_kill` to reach `tgkill`.
            .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror", "-pthread"])
            .arg(&source)
            .arg("-o")
            .arg(&guest)
            .output()
            .unwrap_or_else(|error| panic!("failed to compile the waitid guest: {error}"));
        assert!(
            compile.status.success(),
            "failed to compile {}\nstderr:\n{}",
            source.display(),
            String::from_utf8_lossy(&compile.stderr),
        );
        guest
    })
}

fn kill_process_group(child: &mut std::process::Child) {
    // The group contains Hermit and every guest process it started. Killing
    // only the CLI can strand the deliberately nonterminating fixture child.
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    let _ = child.kill();
}

#[derive(Default)]
struct StderrState {
    retries: usize,
    lines: Vec<String>,
    eof: bool,
    truncated: bool,
}

fn observe_stderr_event(event: StderrEvent, state: &mut StderrState) -> Result<(), String> {
    match event {
        StderrEvent::Line(line) => {
            if line.contains("Retry #") && line.contains("waitid") {
                state.retries += 1;
            }
            if state.lines.len() < MAX_DIAGNOSTIC_LINES {
                state.lines.push(line);
            } else {
                state.truncated = true;
            }
            Ok(())
        }
        StderrEvent::Error(error) => {
            state.eof = true;
            Err(format!("failed while draining hermit stderr: {error}"))
        }
        StderrEvent::Eof => {
            state.eof = true;
            Ok(())
        }
    }
}

fn drain_stderr_batch(
    receive: &mpsc::Receiver<StderrEvent>,
    state: &mut StderrState,
    max_events: usize,
) -> Result<DrainOutcome, String> {
    assert!(max_events > 0, "stderr drain budget must be nonzero");
    for _ in 0..max_events {
        match receive.try_recv() {
            Ok(event) => {
                observe_stderr_event(event, state)?;
                if state.eof {
                    return Ok(DrainOutcome::Eof);
                }
            }
            Err(mpsc::TryRecvError::Empty) => return Ok(DrainOutcome::QueueEmpty),
            Err(mpsc::TryRecvError::Disconnected) => {
                state.eof = true;
                return Ok(DrainOutcome::Eof);
            }
        }
    }
    Ok(DrainOutcome::BudgetExhausted)
}

fn watchdog_limit_failure(
    state: &StderrState,
    elapsed: Duration,
    process_exited: bool,
    limits: WatchdogLimits,
) -> Option<String> {
    if state.retries > limits.max_waitid_retries {
        return Some(format!(
            "waitid watchdog retry limit exceeded: observed {} retries (limit {}); \
             process_exited={process_exited}, stderr_eof={}",
            state.retries, limits.max_waitid_retries, state.eof,
        ));
    }
    if elapsed >= limits.backstop {
        return Some(format!(
            "waitid watchdog wall-clock deadline exceeded: elapsed {}ms reached limit {}ms; \
             process_exited={process_exited}, stderr_eof={}, retries={}; stderr draining is \
             capped at {} events per watchdog tick",
            elapsed.as_millis(),
            limits.backstop.as_millis(),
            state.eof,
            state.retries,
            limits.max_stderr_events_per_tick,
        ));
    }
    None
}

fn run_bounded(args: &[&str], trace_retries: bool, verify_report: Option<&Path>) -> GuestRun {
    run_bounded_for_backend_with_limits(
        "ptrace",
        args,
        trace_retries,
        verify_report,
        WATCHDOG_LIMITS,
    )
}

fn run_bounded_with_limits(
    args: &[&str],
    trace_retries: bool,
    verify_report: Option<&Path>,
    limits: WatchdogLimits,
) -> GuestRun {
    run_bounded_for_backend_with_limits("ptrace", args, trace_retries, verify_report, limits)
}

fn run_bounded_for_backend(backend: &str, args: &[&str], trace_retries: bool) -> GuestRun {
    run_bounded_for_backend_with_limits(backend, args, trace_retries, None, WATCHDOG_LIMITS)
}

fn run_bounded_for_backend_with_limits(
    backend: &str,
    args: &[&str],
    trace_retries: bool,
    verify_report: Option<&Path>,
    limits: WatchdogLimits,
) -> GuestRun {
    let build_root = waitid_guest()
        .parent()
        .expect("waitid guest should have a parent");
    let stdout_file = tempfile::Builder::new()
        .prefix("waitid-guest-")
        .tempfile_in(build_root)
        .expect("failed to create guest stdout file");
    let stdout_path = stdout_file.path().to_path_buf();
    let stdout_writer = stdout_file
        .reopen()
        .expect("failed to reopen guest stdout file");

    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    if args.contains(&"--wait4-live-sibling-signal-blocked") {
        // The regression is an internal ERESTARTSYS followed by a transparent
        // kernel restart, so guest exit alone cannot distinguish old from new.
        // INFO exposes each guest-visible wait4 entry while the narrower TRACE
        // filter retains the bounded retry diagnostics.
        command.env("RUST_LOG", "detcore=info,detcore::syscalls::threads=trace");
    } else if trace_retries {
        command.env("RUST_LOG", "detcore::syscalls::threads=trace");
    } else {
        command.arg("--log=info");
    }
    command.args(["run", "--backend", backend, "--strict"]);
    if let Some(report) = verify_report {
        let report_parent = report
            .parent()
            .expect("verification report should have a parent directory");
        let report_name = report
            .file_name()
            .expect("verification report should have a file name");
        command
            .current_dir(report_parent)
            .args(["--verify", "--verify-strict"])
            .arg("--verify-json")
            .arg(report_name);
    }
    command
        .arg("--")
        .arg(waitid_guest())
        .args(args)
        .process_group(0)
        .stdout(Stdio::from(stdout_writer))
        .stderr(Stdio::piped());

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
    let mut stderr_state = StderrState::default();
    let mut status = None;
    let mut failure = None;

    while status.is_none() || !stderr_state.eof {
        if status.is_none() {
            status = child.try_wait().expect("failed to poll hermit");
        }
        if status.is_some() && stderr_state.eof {
            break;
        }
        if failure.is_none() {
            failure =
                watchdog_limit_failure(&stderr_state, started.elapsed(), status.is_some(), limits);
        }
        if failure.is_some() {
            break;
        }

        let drain_outcome = match drain_stderr_batch(
            &receive,
            &mut stderr_state,
            limits.max_stderr_events_per_tick,
        ) {
            Ok(outcome) => outcome,
            Err(reason) => {
                failure = Some(reason);
                break;
            }
        };

        if failure.is_none() {
            failure =
                watchdog_limit_failure(&stderr_state, started.elapsed(), status.is_some(), limits);
        }
        if failure.is_some() {
            break;
        }
        if status.is_some() && stderr_state.eof {
            break;
        }
        if drain_outcome == DrainOutcome::BudgetExhausted {
            // A live producer may keep stderr permanently nonempty. Start the
            // next watchdog tick immediately so its limits and child-status
            // poll cannot be starved by diagnostic traffic.
            continue;
        }

        match receive.recv_timeout(Duration::from_millis(10)) {
            Ok(event) => {
                if let Err(reason) = observe_stderr_event(event, &mut stderr_state) {
                    failure = Some(reason);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => stderr_state.eof = true,
        }
    }

    if failure.is_some() {
        kill_process_group(&mut child);
    }

    // Always reap the process here. Keeping this unconditional makes the
    // lifecycle obvious on timeout, normal exit, and every diagnostic path.
    let waited_status = child.wait().expect("failed to wait for hermit");
    status.get_or_insert(waited_status);
    reader.join().expect("stderr reader panicked");
    let stdout = fs::read_to_string(&stdout_path).expect("failed to read guest stdout");
    let mut stderr = stderr_state.lines.join("\n");
    if stderr_state.truncated {
        stderr.push_str(&format!(
            "\n[waitid watchdog retained the first {MAX_DIAGNOSTIC_LINES} stderr lines; \
             later lines were drained but omitted]"
        ));
    }
    if let Some(reason) = failure {
        panic!("{reason}\nguest stdout:\n{stdout}\nhermit stderr:\n{stderr}");
    }

    GuestRun {
        status: status.expect("hermit status should be collected"),
        stdout,
        stderr,
        retries: stderr_state.retries,
    }
}

fn dbt_unavailable(test: &str) -> bool {
    if cfg!(feature = "dbt") {
        return false;
    }
    assert!(
        std::env::var_os("HERMIT_REQUIRE_DBT").is_none(),
        "HERMIT_REQUIRE_DBT is set, but this test binary was built without the dbt feature, so \
         {test} cannot exercise DBT"
    );
    eprintln!("skipping {test}: built without the dbt feature");
    true
}

#[test]
fn retry_watchdog_fires_and_reports_before_stderr_backlog_drains() {
    let (send, receive) = mpsc::channel();
    send.send(StderrEvent::Line("Retry #1 for waitid".into()))
        .unwrap();
    send.send(StderrEvent::Line("Retry #2 for waitid".into()))
        .unwrap();
    let mut state = StderrState::default();
    let limits = WatchdogLimits {
        max_waitid_retries: 0,
        backstop: Duration::from_secs(60),
        max_stderr_events_per_tick: 1,
    };

    assert_eq!(
        drain_stderr_batch(&receive, &mut state, limits.max_stderr_events_per_tick).unwrap(),
        DrainOutcome::BudgetExhausted
    );
    let failure = watchdog_limit_failure(&state, Duration::ZERO, false, limits)
        .expect("the deliberately tiny retry limit must fire");
    assert!(
        failure.contains("waitid watchdog retry limit exceeded")
            && failure.contains("observed 1 retries (limit 0)"),
        "the failure must name the bounded condition and measured values: {failure}"
    );
    assert!(
        receive.try_recv().is_ok(),
        "the watchdog must evaluate its limit while stderr is still backlogged"
    );
    drop(send);
}

#[test]
fn deadline_watchdog_fires_and_reports_before_stderr_backlog_drains() {
    let (send, receive) = mpsc::channel();
    send.send(StderrEvent::Line("diagnostic one".into()))
        .unwrap();
    send.send(StderrEvent::Line("diagnostic two".into()))
        .unwrap();
    let mut state = StderrState::default();
    let limits = WatchdogLimits {
        max_waitid_retries: usize::MAX,
        backstop: Duration::ZERO,
        max_stderr_events_per_tick: 1,
    };

    assert_eq!(
        drain_stderr_batch(&receive, &mut state, limits.max_stderr_events_per_tick).unwrap(),
        DrainOutcome::BudgetExhausted
    );
    let failure = watchdog_limit_failure(&state, Duration::ZERO, false, limits)
        .expect("the deliberately zero wall-clock deadline must fire");
    assert!(
        failure.contains("waitid watchdog wall-clock deadline exceeded")
            && failure.contains("limit 0ms")
            && failure.contains("capped at 1 events per watchdog tick"),
        "the failure must name the deadline and the fairness bound: {failure}"
    );
    assert!(
        receive.try_recv().is_ok(),
        "the deadline must be evaluated while stderr is still backlogged"
    );
    drop(send);
}

#[test]
fn waitid_interrupted_by_a_signal_returns_instead_of_spinning() {
    let run = run_bounded(&[], true, None);
    assert!(
        run.status.success(),
        "hermit exited with {}\nguest stdout:\n{}\nhermit stderr:\n{}",
        run.status,
        run.stdout,
        run.stderr,
    );
    assert!(
        run.stdout
            .contains("waitid-signal-interrupt rc=-1 errno=4 handler=1 target-match=1"),
        "waitid did not report EINTR with its handler seeing the guest child ID\n\
         guest stdout:\n{}",
        run.stdout,
    );
    assert!(
        run.stdout.contains("waitid-signal-interrupt-done"),
        "the guest did not reach its child cleanup\nguest stdout:\n{}",
        run.stdout,
    );
    assert!(
        run.retries <= MAX_WAITID_RETRIES,
        "waitid returned correctly but performed {} retries",
        run.retries,
    );
}

#[test]
fn waitid_live_sibling_signal_interrupts_without_spinning() {
    let limits = WatchdogLimits {
        max_waitid_retries: 2_000,
        backstop: Duration::from_secs(5),
        max_stderr_events_per_tick: 64,
    };
    let run = run_bounded_with_limits(&["--live-sibling-signal"], true, None, limits);
    assert!(
        run.status.success(),
        "hermit exited with {}\nguest stdout:\n{}\nhermit stderr:\n{}",
        run.status,
        run.stdout,
        run.stderr,
    );
    assert!(
        run.stdout
            .contains("waitid-live-sibling rc=-1 errno=4 handler=1 mask-preserved=1 sender-live=1"),
        "a live sibling signal did not interrupt waitid with its handler run\n\
         guest stdout:\n{}",
        run.stdout,
    );
    assert!(
        run.stdout.contains("waitid-live-sibling-done"),
        "the guest did not clean up both live children\nguest stdout:\n{}",
        run.stdout,
    );
    assert!(
        run.retries <= limits.max_waitid_retries,
        "waitid returned correctly but performed {} retries",
        run.retries,
    );
}

#[test]
fn waitid_live_sibling_signal_honors_sa_restart_and_preserves_mask() {
    let limits = WatchdogLimits {
        max_waitid_retries: 4_000,
        backstop: Duration::from_secs(8),
        max_stderr_events_per_tick: 64,
    };
    let run = run_bounded_with_limits(&["--live-sibling-signal-restart"], true, None, limits);
    assert!(
        run.status.success(),
        "hermit exited with {}\nguest stdout:\n{}\nhermit stderr:\n{}",
        run.status,
        run.stdout,
        run.stderr,
    );
    assert!(
        run.stdout.contains(
            "waitid-live-sibling-restart rc=0 errno=0 handler=1 pid-match=1 code=1 status=29 mask-preserved=1 sender-live=1"
        ),
        "a live sibling signal did not preserve SA_RESTART, child identity, and the guest mask\n\
         guest stdout:\n{}",
        run.stdout,
    );
    assert!(
        run.stdout.contains("waitid-live-sibling-done"),
        "the guest did not clean up the live signaler\nguest stdout:\n{}",
        run.stdout,
    );
    assert!(
        run.retries <= limits.max_waitid_retries,
        "restarted waitid performed {} retries",
        run.retries,
    );
}

#[test]
fn waitid_keeps_a_guest_blocked_sibling_signal_pending() {
    let limits = WatchdogLimits {
        max_waitid_retries: 4_000,
        backstop: Duration::from_secs(8),
        max_stderr_events_per_tick: 64,
    };
    let run = run_bounded_with_limits(&["--live-sibling-signal-blocked"], true, None, limits);
    assert!(
        run.status.success(),
        "hermit exited with {}\nguest stdout:\n{}\nhermit stderr:\n{}",
        run.status,
        run.stdout,
        run.stderr,
    );
    assert!(
        run.stdout.contains(
            "waitid-live-sibling-blocked rc=0 errno=0 handler=0 pid-match=1 code=1 status=29 mask-preserved=1 sender-live=1"
        ),
        "a guest-blocked sibling signal incorrectly interrupted waitid or changed the mask\n\
         guest stdout:\n{}",
        run.stdout,
    );
    assert!(
        run.retries <= limits.max_waitid_retries,
        "blocked-signal waitid performed {} retries",
        run.retries,
    );
}

#[test]
fn wait4_keeps_a_guest_blocked_sibling_signal_pending() {
    let limits = WatchdogLimits {
        max_waitid_retries: 4_000,
        backstop: Duration::from_secs(8),
        max_stderr_events_per_tick: 64,
    };
    let run = run_bounded_with_limits(&["--wait4-live-sibling-signal-blocked"], true, None, limits);
    assert!(
        run.status.success(),
        "hermit exited with {}\nguest stdout:\n{}\nhermit stderr:\n{}",
        run.status,
        run.stdout,
        run.stderr,
    );
    assert!(
        run.stdout.contains(
            "wait4-live-sibling-blocked rc-ok=1 errno=0 handler=0 pid-match=1 exited=1 status=29 mask-preserved=1 signals-pending=1 sender-live=1"
        ),
        "a guest-blocked sibling signal incorrectly interrupted wait4 or changed the mask\n\
         guest stdout:\n{}",
        run.stdout,
    );
    assert!(
        run.stdout.contains("wait4-live-sibling-done"),
        "the guest did not clean up both live children\nguest stdout:\n{}",
        run.stdout,
    );
    let wait4_entries = run.stderr.matches("inbound syscall: wait4(").count();
    assert_eq!(
        wait4_entries, 3,
        "the blocked signal restarted wait4 internally instead of remaining pending; \
         expected target wait plus two cleanup waits, saw {wait4_entries}\nhermit stderr:\n{}",
        run.stderr,
    );
}

#[test]
fn waitid_honors_sa_restart_after_running_the_handler() {
    let run = run_bounded(&["--signal-restart"], true, None);
    assert!(
        run.status.success(),
        "hermit exited with {}\nguest stdout:\n{}\nhermit stderr:\n{}",
        run.status,
        run.stdout,
        run.stderr,
    );
    assert!(
        run.stdout
            .contains("waitid-signal-restart rc=0 errno=0 handler=1 pid-match=1 code=1 status=17"),
        "waitid did not restart after the SA_RESTART handler\nguest stdout:\n{}",
        run.stdout,
    );
    assert!(
        run.retries <= MAX_WAITID_RETRIES,
        "restarted waitid performed {} retries",
        run.retries,
    );
}

#[test]
fn waitid_wcontinued_signal_interrupts_without_panicking() {
    let run = run_bounded(&["--waitid-wcontinued-signal-interrupt"], true, None);
    assert!(
        run.status.success(),
        "hermit exited with {}\nguest stdout:\n{}\nhermit stderr:\n{}",
        run.status,
        run.stdout,
        run.stderr,
    );
    assert!(
        run.stdout
            .contains("waitid-wcontinued-signal-interrupt rc=-1 errno=4 handler=1"),
        "WCONTINUED waitid did not return EINTR after its handler\nguest stdout:\n{}",
        run.stdout,
    );
}

#[test]
fn waitid_wstopped_signal_interrupts_without_panicking() {
    let run = run_bounded(&["--waitid-wstopped-signal-interrupt"], true, None);
    assert!(
        run.status.success(),
        "hermit exited with {}\nguest stdout:\n{}\nhermit stderr:\n{}",
        run.status,
        run.stdout,
        run.stderr,
    );
    assert!(
        run.stdout
            .contains("waitid-wstopped-signal-interrupt rc=-1 errno=4 handler=1"),
        "WSTOPPED waitid did not return EINTR after its handler\nguest stdout:\n{}",
        run.stdout,
    );
}

#[test]
fn waitid_ready_child_path_is_ptrace_l2() {
    let build_root = waitid_guest()
        .parent()
        .expect("waitid guest should have a parent");
    let report = tempfile::Builder::new()
        .prefix("waitid-verify-")
        .tempfile_in(build_root)
        .expect("failed to create verify report beside the guest");
    let run = run_bounded(&["--child-ready-wins"], false, Some(report.path()));
    assert!(
        run.status.success(),
        "hermit exited with {}\nguest stdout:\n{}\nhermit stderr:\n{}",
        run.status,
        run.stdout,
        run.stderr,
    );
    assert!(
        run.stdout
            .contains("waitid-ready-wins rc=0 errno=0 handler=1 pid-match=1 code=1 status=23"),
        "waitid did not report the ready child before delivering simultaneous SIGCHLD\n\
         guest stdout:\n{}",
        run.stdout,
    );

    let report: serde_json::Value = serde_json::from_slice(
        &fs::read(report.path()).expect("strict verification did not publish its report"),
    )
    .expect("strict verification report was not valid JSON");
    assert_eq!(report["verdict"], "matched", "verify report: {report}");
    assert_eq!(report["verified"], true, "verify report: {report}");
    assert_eq!(report["bitwise_parity"], true, "verify report: {report}");
    assert_eq!(
        report["comparison"]["strictness"], "canonical",
        "verify report: {report}"
    );
    assert_eq!(
        report["comparison"]["log_scope"], "info",
        "verify report: {report}"
    );
    assert_eq!(
        report["comparison"]["compare_logs"], true,
        "verify report: {report}"
    );
    assert_eq!(
        report["comparison"]["compare_io_buffers"], true,
        "verify report: {report}"
    );
    assert_eq!(
        report["comparison"]["record_envelope"], "all_records_v1",
        "verify report: {report}"
    );
    for side in ["left", "right"] {
        assert!(
            report["compared_log_messages"][side]
                .as_u64()
                .is_some_and(|count| count > 0),
            "verify report compared no INFO evidence on {side}: {report}"
        );
    }
}

fn assert_dbt_wait_case(args: &[&str], expected: &str, done: Option<&str>) {
    let run = run_bounded_for_backend("dbt", args, true);
    assert!(
        run.status.success(),
        "DBT Hermit exited with {}\nguest stdout:\n{}\nhermit stderr:\n{}",
        run.status,
        run.stdout,
        run.stderr,
    );
    assert!(
        run.stdout.contains(expected),
        "DBT child wait did not preserve the expected Linux result\nexpected: {expected}\n\
         guest stdout:\n{}\nhermit stderr:\n{}",
        run.stdout,
        run.stderr,
    );
    if let Some(done) = done {
        assert!(
            run.stdout.contains(done),
            "DBT guest did not finish child cleanup\nguest stdout:\n{}\nhermit stderr:\n{}",
            run.stdout,
            run.stderr,
        );
    }
}

#[test]
fn dbt_exact_child_waits_return_eintr_without_sa_restart() {
    if dbt_unavailable("dbt_exact_child_waits_return_eintr_without_sa_restart") {
        return;
    }
    assert_dbt_wait_case(
        &[],
        "waitid-signal-interrupt rc=-1 errno=4 handler=1 target-match=1",
        Some("waitid-signal-interrupt-done"),
    );
    assert_dbt_wait_case(
        &["--wait4-signal-interrupt"],
        "wait4-signal-interrupt rc=-1 errno=4 handler=1 target-match=1",
        Some("wait4-signal-interrupt-done"),
    );
}

#[test]
fn dbt_legacy_waitid_signals_return_eintr_without_panicking() {
    if dbt_unavailable("dbt_legacy_waitid_signals_return_eintr_without_panicking") {
        return;
    }
    assert_dbt_wait_case(
        &["--waitid-wstopped-signal-interrupt"],
        "waitid-wstopped-signal-interrupt rc=-1 errno=4 handler=1",
        None,
    );
    assert_dbt_wait_case(
        &["--waitid-wcontinued-signal-interrupt"],
        "waitid-wcontinued-signal-interrupt rc=-1 errno=4 handler=1",
        None,
    );
}

#[test]
fn dbt_legacy_waitid_signal_contexts_use_the_guest_pid() {
    if dbt_unavailable("dbt_legacy_waitid_signal_contexts_use_the_guest_pid") {
        return;
    }
    assert_dbt_wait_case(
        &["--waitid-wstopped-signal-context"],
        "waitid-wstopped-signal-context rc=-1 errno=4 handler=1 target-match=1",
        None,
    );
    assert_dbt_wait_case(
        &["--waitid-wcontinued-signal-context"],
        "waitid-wcontinued-signal-context rc=-1 errno=4 handler=1 target-match=1",
        None,
    );
}

#[test]
fn dbt_exact_child_waits_honor_sa_restart() {
    if dbt_unavailable("dbt_exact_child_waits_honor_sa_restart") {
        return;
    }
    assert_dbt_wait_case(
        &["--signal-restart"],
        "waitid-signal-restart rc=0 errno=0 handler=1 pid-match=1 code=1 status=17",
        None,
    );
    assert_dbt_wait_case(
        &["--wait4-signal-restart"],
        "wait4-signal-restart rc-ok=1 errno=0 handler=1 pid-match=1 exited=1 status=17",
        None,
    );
    assert_dbt_wait_case(
        &["--signal-restart-handler"],
        "waitid-restart-handler rc=0 errno=0 handler=1 pid-match=1 code=2 status=9",
        None,
    );
    assert_dbt_wait_case(
        &["--wait4-signal-restart-handler"],
        "wait4-restart-handler rc-ok=1 errno=0 handler=1 pid-match=1 signaled=1 signal=9",
        None,
    );
}

#[test]
fn dbt_exact_child_waits_apply_each_signal_restart_disposition() {
    if dbt_unavailable("dbt_exact_child_waits_apply_each_signal_restart_disposition") {
        return;
    }
    assert_dbt_wait_case(
        &["--signal-restart-then-interrupt"],
        "waitid-restart-then-interrupt rc=-1 errno=4 restart-handler=1 interrupt-handler=1 target-live=1 sender-live=1",
        None,
    );
    assert_dbt_wait_case(
        &["--wait4-signal-restart-then-interrupt"],
        "wait4-restart-then-interrupt rc=-1 errno=4 restart-handler=1 interrupt-handler=1 target-live=1 sender-live=1",
        None,
    );
}

#[test]
fn dbt_exact_child_waits_honor_a_changed_signal_context() {
    if dbt_unavailable("dbt_exact_child_waits_honor_a_changed_signal_context") {
        return;
    }
    assert_dbt_wait_case(
        &["--signal-restart-context"],
        "waitid-restart-context rc=-1 errno=4 handler=1 target-match=1",
        None,
    );
    assert_dbt_wait_case(
        &["--wait4-signal-restart-context"],
        "wait4-restart-context rc=-1 errno=4 handler=1 target-match=1",
        None,
    );
}

#[test]
fn dbt_exact_child_waits_preserve_a_handler_changed_target() {
    if dbt_unavailable("dbt_exact_child_waits_preserve_a_handler_changed_target") {
        return;
    }
    assert_dbt_wait_case(
        &["--signal-restart-target"],
        "waitid-restart-target rc=0 errno=0 handler=1 target-match=1 replacement-match=1 code=2 status=9 original-live=1",
        None,
    );
    assert_dbt_wait_case(
        &["--wait4-signal-restart-target"],
        "wait4-restart-target rc-ok=1 errno=0 handler=1 target-match=1 replacement-match=1 signaled=1 signal=9 original-live=1",
        None,
    );
}

#[test]
fn dbt_exact_child_waits_respect_noninterrupting_default_dispositions() {
    if dbt_unavailable("dbt_exact_child_waits_respect_noninterrupting_default_dispositions") {
        return;
    }
    for signal in [libc::SIGCHLD, libc::SIGCONT, libc::SIGURG, libc::SIGWINCH] {
        let signal = signal.to_string();
        assert_dbt_wait_case(
            &["--waitid-default-disposition", &signal],
            &format!(
                "waitid-default-disposition signal={signal} rc=0 errno=0 pid-match=1 code=2 signal-status=9 sender-live=1"
            ),
            None,
        );
        assert_dbt_wait_case(
            &["--wait4-default-disposition", &signal],
            &format!(
                "wait4-default-disposition signal={signal} rc-ok=1 errno=0 pid-match=1 signaled=1 signal-status=9 sender-live=1"
            ),
            None,
        );
    }
}

#[test]
fn dbt_exact_child_waits_preserve_mask_and_result_for_blocked_signals() {
    if dbt_unavailable("dbt_exact_child_waits_preserve_mask_and_result_for_blocked_signals") {
        return;
    }
    assert_dbt_wait_case(
        &["--live-sibling-signal-blocked"],
        "waitid-live-sibling-blocked rc=0 errno=0 handler=0 pid-match=1 code=1 status=29 mask-preserved=1 sender-live=1",
        Some("waitid-live-sibling-done"),
    );
    assert_dbt_wait_case(
        &["--wait4-live-sibling-signal-blocked"],
        "wait4-live-sibling-blocked rc-ok=1 errno=0 handler=0 pid-match=1 exited=1 status=29 mask-preserved=1 signals-pending=1 sender-live=1",
        Some("wait4-live-sibling-done"),
    );
}

#[test]
fn dbt_exact_child_waits_return_a_ready_child_before_the_signal() {
    if dbt_unavailable("dbt_exact_child_waits_return_a_ready_child_before_the_signal") {
        return;
    }
    assert_dbt_wait_case(
        &["--child-ready-wins"],
        "waitid-ready-wins rc=0 errno=0 handler=1 pid-match=1 code=1 status=23",
        None,
    );
    assert_dbt_wait_case(
        &["--wait4-child-ready-wins"],
        "wait4-ready-wins rc-ok=1 errno=0 handler=1 pid-match=1 exited=1 status=23",
        None,
    );
}

/// End-to-end proof that the watchdog is EFFECTIVE, not merely present.
///
/// The two tests above exercise `watchdog_limit_failure` and
/// `drain_stderr_batch` in isolation: they prove the predicate *reports*. They
/// do not prove that the surrounding loop ever escapes, that a live Hermit is
/// actually killed, or that the diagnostic reaches the caller. A bound that
/// fires into a loop that keeps waiting is the failure mode this guards.
///
/// The bound is shrunk until it MUST trigger against a real Hermit process, and
/// the run is then required to come back — bounded — carrying a message that
/// names the condition and the measured values.
#[test]
fn watchdog_escapes_and_reports_against_a_live_hermit() {
    let limits = WatchdogLimits {
        max_waitid_retries: usize::MAX,
        // Deliberately far below any real run: this must fire on the first tick.
        backstop: Duration::from_millis(1),
        max_stderr_events_per_tick: 1,
    };

    let started = Instant::now();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_bounded_with_limits(&["--live-sibling-signal"], true, None, limits)
    }));
    let elapsed = started.elapsed();

    // EFFECTIVE: the call returned instead of waiting on the guest.
    let payload = match outcome {
        Err(payload) => payload,
        Ok(run) => panic!(
            "a 1ms deadline against a live Hermit must abort the run, not complete it \
             (hermit exited {}, {} retries observed)",
            run.status, run.retries,
        ),
    };
    assert!(
        elapsed < Duration::from_secs(30),
        "the watchdog took {elapsed:?} to escape, so the bound is not enforced"
    );

    // REPORTED: the diagnostic names the bounded condition and its measurements.
    let message = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .expect("the watchdog failure must carry a string payload");
    assert!(
        message.contains("waitid watchdog wall-clock deadline exceeded"),
        "the watchdog fired but did not name the condition: {message}"
    );
    assert!(
        message.contains("limit 1ms") && message.contains("elapsed"),
        "the watchdog must report the limit and the measured elapsed time: {message}"
    );
    assert!(
        message.contains("process_exited=") && message.contains("stderr_eof="),
        "the watchdog must report the lifecycle state it observed: {message}"
    );
    assert!(
        message.contains("guest stdout:") && message.contains("hermit stderr:"),
        "the watchdog must attach the captured diagnostics: {message}"
    );
}

/// A sibling THREAD, not a sibling process.
///
/// Every other fixture here signals from a forked process, which reaches
/// Detcore through `handle_kill`. `pthread_kill` lowers to `tgkill`, a
/// different handler. An earlier revision of the waitid wakeup notified the
/// scheduler only from `handle_kill`, so this case hung forever while all the
/// process-sender tests passed -- both adversarial reviewers found it and it
/// reproduced as a 30s timeout against a real Hermit.
///
/// The bound is deliberately tight: if the wakeup regresses, this reports a
/// named watchdog failure in seconds instead of hanging the suite.
#[test]
fn waitid_live_sibling_thread_signal_interrupts_without_spinning() {
    let limits = WatchdogLimits {
        max_waitid_retries: 2_000,
        backstop: Duration::from_secs(8),
        max_stderr_events_per_tick: 64,
    };
    let run = run_bounded_with_limits(&["--live-sibling-thread-signal"], true, None, limits);
    assert!(
        run.status.success(),
        "hermit exited with {}\nguest stdout:\n{}\nhermit stderr:\n{}",
        run.status,
        run.stdout,
        run.stderr,
    );
    // errno 4 is EINTR, and handler=1 proves the signal was actually delivered
    // rather than the wait merely being abandoned.
    assert!(
        run.stdout
            .contains("waitid-thread-sibling rc=-1 errno=4 handler=1"),
        "a thread-directed sibling signal did not interrupt waitid with its handler run\n\
         guest stdout:\n{}",
        run.stdout,
    );
    assert!(
        run.stdout.contains("waitid-thread-sibling-done"),
        "the guest did not clean up its child\nguest stdout:\n{}",
        run.stdout,
    );
}
