/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Network record/replay acceptance: `hermit run --record-networking` against
//! a loopback controller, then offline `--replay-networking` under several
//! schedules, each verified at L2; and a real curl fetch from a loopback HTTP
//! server, recorded, then replayed offline with the record and replay logs
//! compared by `hermit log-diff`. Ad-hoc runs set `HERMIT_SAFEHERMIT` to the
//! dev-hermit `bin/safehermit` wrapper; without it Hermit runs under the same
//! `timeout` bound as the rest of this file.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;
use std::thread;
use std::time::Duration;
use std::time::Instant;

const CONTROLLER_WALL_SECONDS: u64 = 32;
const NATIVE_CLIENT_WALL_SECONDS: u64 = 10;
const HERMIT_WALL_SECONDS: u64 = 30;
const OUTER_WALL_SECONDS: u64 = 45;

// A small, predeclared population: one scheduler seed at a time, crossing a
// timeslice boundary, without turning this into a broad schedule campaign.
const REPLAY_CELLS: &[(u64, u64)] = &[
    (0, 1_000_000),
    (1, 1_000_000),
    (2, 4_000_000),
    (3, 4_000_000),
];

const OUTBOUND_HEX: &str = "726571756573740a6e6578740a646f6e650a";

fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(14_695_981_039_346_656_037, |digest, byte| {
            (digest ^ u64::from(*byte)).wrapping_mul(1_099_511_628_211)
        })
}

fn expected_invariant() -> String {
    format!(
        "aggregate=abcdef readiness=pollin,pollin eof=1 outbound_hex={OUTBOUND_HEX} outbound_fnv1a64={:016x}",
        fnv1a64(b"request\nnext\ndone\n")
    )
}

/// Checks the schedule-independent output and returns the reader marker,
/// which names the thread that won the race for the first input.
/// The select client's whole stdout: every check it makes passed.
fn expected_select_output() -> String {
    format!(
        "select=ebadf,poll-limit,timeout,pipe-writable,first,three-sets,second,eof aggregate=abcdef \
         outbound_hex={OUTBOUND_HEX} outbound_fnv1a64={:016x}\n",
        fnv1a64(b"request\nnext\ndone\n")
    )
}

fn assert_guest_invariants(output: &[u8], label: &str) -> String {
    let text = std::str::from_utf8(output)
        .unwrap_or_else(|error| panic!("{label} stdout was not UTF-8: {error}"));
    let invariants = text
        .lines()
        .filter(|line| line.starts_with("aggregate="))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        invariants,
        BTreeSet::from([expected_invariant().as_str()]),
        "{label} lost the fixed input, EOF, or outbound-stream invariant:\n{text}"
    );
    let markers = text
        .lines()
        .filter_map(|line| line.strip_prefix("reader-marker="))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        markers.len(),
        1,
        "{label} must publish one competing-reader marker:\n{text}"
    );
    let marker = markers.into_iter().next().unwrap();
    assert!(
        matches!(marker, "0:abc,1:def" | "1:abc,0:def"),
        "{label} published an impossible reader marker {marker:?}"
    );
    marker.to_owned()
}

fn bounded_command(program: &Path, arguments: &[&OsStr], seconds: u64) -> Output {
    Command::new("timeout")
        .args(["--kill-after=1s", &format!("{seconds}s")])
        .arg(program)
        .args(arguments)
        .output()
        .unwrap_or_else(|error| panic!("failed to launch bounded command {program:?}: {error}"))
}

/// The fixture's loopback TCP server, which checks the exact outbound stream.
struct Controller {
    child: Option<Child>,
    contact_path: PathBuf,
    report_path: PathBuf,
}

impl Controller {
    fn start(program: &Path, directory: &Path) -> (Self, String) {
        Self::start_mode(program, directory, "controller")
    }

    /// Starts the fixture's controller `mode`.
    fn start_mode(program: &Path, directory: &Path, mode: &str) -> (Self, String) {
        let port_path = directory.join("controller.port");
        let contact_path = directory.join("controller.contact");
        let report_path = directory.join("controller.report");
        let child = Command::new("timeout")
            .args(["--kill-after=1s", &format!("{CONTROLLER_WALL_SECONDS}s")])
            .arg(program)
            .arg(mode)
            .arg(&port_path)
            .arg(&report_path)
            .arg(&contact_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to start bounded TCP controller");
        let mut controller = Self {
            child: Some(child),
            contact_path,
            report_path,
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(port) = fs::read_to_string(&port_path) {
                let port = port.trim().to_owned();
                assert!(
                    port.parse::<u16>().is_ok_and(|value| value != 0),
                    "controller published an invalid port {port:?}"
                );
                return (controller, port);
            }
            if let Some(status) = controller
                .child
                .as_mut()
                .expect("controller child exists")
                .try_wait()
                .expect("failed to inspect TCP controller")
            {
                let output = controller.finish_child();
                panic!(
                    "TCP controller exited before publishing its port: {status}\nstdout:\n{}\nstderr:\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            assert!(
                Instant::now() < deadline,
                "TCP controller did not publish its port within two seconds"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Waits for the protocol to complete and returns the controller's report.
    fn finish(mut self) -> String {
        let output = self.finish_child();
        assert!(
            output.status.success(),
            "TCP controller failed with {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        fs::read_to_string(&self.report_path).expect("controller omitted its exact-stream report")
    }

    /// Stops a controller that no client may have reached.
    fn stop_without_connection(mut self) {
        let child = self.child.as_mut().expect("controller child exists");
        Self::request_stop(child).expect("failed to stop unused TCP controller");
        let output = self.finish_child();
        assert!(
            !output.status.success(),
            "an unused controller must be stopped rather than complete successfully"
        );
        assert!(
            !self.contact_path.exists(),
            "the guest reached the controller's accept boundary"
        );
    }

    fn request_stop(child: &mut Child) -> std::io::Result<()> {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        // TERM the unreaped timeout supervisor, which forwards it and then
        // kills the controller after --kill-after. Never signal a PID after
        // Child has reaped it.
        let pid = i32::try_from(child.id()).map_err(std::io::Error::other)?;
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGTERM,
        )
        .map_err(std::io::Error::from)
    }

    fn finish_child(&mut self) -> Output {
        self.child
            .take()
            .expect("controller child exists")
            .wait_with_output()
            .expect("failed to collect bounded TCP controller")
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = Self::request_stop(child);
            let _ = child.wait();
        }
    }
}

fn assert_controller_report(report: &str) {
    let expected = format!(
        "controller=complete\noutbound_hex={OUTBOUND_HEX}\noutbound_fnv1a64={:016x}\n",
        fnv1a64(b"request\nnext\ndone\n")
    );
    assert_eq!(
        report, expected,
        "controller observed a changed outbound stream"
    );
}

/// Runs Hermit with `arguments`, through `HERMIT_SAFEHERMIT` when it is set,
/// and keeps its stdout and stderr in `evidence`.
fn hermit_command(
    evidence: &Path,
    label: &str,
    arguments: &[String],
    guest: &Path,
    guest_arguments: &[&str],
) -> Output {
    let mut command = Command::new("timeout");
    command.args(["--kill-after=5s", &format!("{OUTER_WALL_SECONDS}s")]);
    if let Some(safehermit) = std::env::var_os("HERMIT_SAFEHERMIT") {
        command
            .arg(safehermit)
            .args(["--sh-deadline", &HERMIT_WALL_SECONDS.to_string()]);
    }
    // Cargo's `LD_LIBRARY_PATH` names this build's own directories. A guest
    // that inherited it would have its dynamic loader stat them while other
    // validation nodes write there, so one run of a `--verify` pair could see
    // a directory size the other did not
    // (https://github.com/rrnewton/hermit/issues/3846). Hermit does not use it.
    command
        .env_remove("LD_LIBRARY_PATH")
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(arguments)
        .arg("--")
        .arg(guest)
        .args(guest_arguments);
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {label}: {error}"));
    fs::write(evidence.join(format!("{label}.stdout")), &output.stdout)
        .unwrap_or_else(|error| panic!("failed to retain {label} stdout: {error}"));
    fs::write(evidence.join(format!("{label}.stderr")), &output.stderr)
        .unwrap_or_else(|error| panic!("failed to retain {label} stderr: {error}"));
    output
}

fn run_arguments(seed: u64, max_timeslice: u64) -> Vec<String> {
    vec![
        "--backend=ptrace".into(),
        "--log=info".into(),
        "run".into(),
        "--strict".into(),
        "--chaos".into(),
        "--seed=0".into(),
        "--rng-seed=0".into(),
        "--fuzz-seed=0".into(),
        format!("--sched-seed={seed}"),
        "--sched-heuristic=random".into(),
        format!("--max-timeslice={max_timeslice}"),
    ]
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_success(output: &Output, label: &str) {
    assert!(
        output.status.success(),
        "{label} failed with {}\n{}",
        output.status,
        output_text(output)
    );
}

fn assert_l2_report(path: &Path, label: &str) {
    let report: serde_json::Value = serde_json::from_slice(
        &fs::read(path).unwrap_or_else(|error| panic!("{label} omitted verify JSON: {error}")),
    )
    .unwrap_or_else(|error| panic!("{label} verify JSON was malformed: {error}"));
    assert_eq!(report["verdict"], "matched", "{label} report: {report}");
    assert_eq!(report["verified"], true, "{label} report: {report}");
    assert_eq!(report["bitwise_parity"], true, "{label} report: {report}");
    assert_eq!(
        report["comparison"]["strictness"], "canonical",
        "{label} report: {report}"
    );
    assert_eq!(
        report["comparison"]["compare_io_buffers"], true,
        "{label} report: {report}"
    );
    for side in ["left", "right"] {
        assert!(
            report["compared_log_messages"][side]
                .as_u64()
                .is_some_and(|count| count > 0),
            "{label} compared no INFO records on {side}: {report}"
        );
    }
}

/// Requires a prompt failure whose output contains `required`. Exit 124 or
/// 137 means a `timeout` bound fired instead.
fn assert_fails_naming(output: &Output, label: &str, required: &str) {
    let text = output_text(output);
    assert!(
        !output.status.success(),
        "{label} unexpectedly succeeded\n{text}"
    );
    assert!(
        !matches!(output.status.code(), Some(124 | 137)),
        "{label} reached a wall-clock bound instead of failing promptly\n{text}"
    );
    assert!(
        text.contains(required),
        "{label} did not report {required:?}:\n{text}"
    );
}

/// Requires the policy-refusal exit status and output naming `reason` and
/// `remedy`.
fn assert_refused(output: &Output, label: &str, reason: &str, remedy: &str) {
    assert_fails_naming(output, label, reason);
    assert_fails_naming(output, label, remedy);
    assert_eq!(
        output.status.code(),
        Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
        "{label} did not exit with the policy-refusal status\n{}",
        output_text(output)
    );
}

#[test]
fn network_replay_tcp_fixture_has_the_exact_native_contract() {
    let fixture = &super::workload("c_network_replay_tcp_bracket").path;
    let evidence = tempfile::tempdir().expect("create native network bracket directory");
    let (controller, port) = Controller::start(fixture, evidence.path());
    let output = bounded_command(
        fixture,
        &[OsStr::new("client"), OsStr::new(&port), OsStr::new("match")],
        NATIVE_CLIENT_WALL_SECONDS,
    );
    assert_success(&output, "native TCP bracket client");
    assert_guest_invariants(&output.stdout, "native TCP bracket client");
    assert_controller_report(&controller.finish());

    let directory = evidence.path().join("select");
    fs::create_dir(&directory).expect("create native select directory");
    let (controller, port) = Controller::start(fixture, &directory);
    let output = bounded_command(
        fixture,
        &[
            OsStr::new("client"),
            OsStr::new(&port),
            OsStr::new("select"),
        ],
        NATIVE_CLIENT_WALL_SECONDS,
    );
    assert_success(&output, "native select client");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        expected_select_output(),
        "native select client"
    );
    assert_controller_report(&controller.finish());

    for mode in BACKPRESSURE_MODES.into_iter().chain([INTERLEAVED_MODE]) {
        let directory = evidence.path().join(mode);
        fs::create_dir(&directory).expect("create native backpressure directory");
        let (controller, port) =
            Controller::start_mode(fixture, &directory, "backpressure-controller");
        let output = bounded_command(
            fixture,
            &[OsStr::new("client"), OsStr::new(&port), OsStr::new(mode)],
            NATIVE_CLIENT_WALL_SECONDS,
        );
        let label = format!("native {mode} client");
        assert_success(&output, &label);
        assert_backpressure_output(&output.stdout, mode, &label);
        assert_backpressure_report(&controller.finish());
    }
}

/// The client modes in which one thread fills a connection whose peer
/// drains it only after a second thread sends on another connection.
const BACKPRESSURE_MODES: [&str; 2] = ["backpressure-nonblocking", "backpressure-blocking"];
const BACKPRESSURE_BYTES: usize = 256 * 1024;
/// A blocking send waits before any byte of it is accepted while another
/// thread's nonblocking send on the same connection is refused.
const INTERLEAVED_MODE: &str = "backpressure-interleaved";
/// A blocking send that also prints whether the go thread had run when it
/// completed, and the monotonic clock around it.
const OBSERVE_MODE: &str = "backpressure-observe";
/// A blocking send whose descriptor number another thread replaces while it
/// waits.
const REPLACE_MODE: &str = "backpressure-replace";
/// A blocking send from a shared file mapping whose unsent half a child
/// process rewrites while it waits.
const SHARED_MODE: &str = "backpressure-shared";

/// The client's stdout less the observe mode's own lines, which are checked
/// against the recording. Every other byte, terminators included, is kept.
fn backpressure_protocol(stdout: &[u8], mode: &str) -> Vec<u8> {
    if mode != OBSERVE_MODE {
        return stdout.to_vec();
    }
    stdout
        .split_inclusive(|&byte| byte == b'\n')
        .filter(|line| !line.starts_with(b"observe "))
        .flatten()
        .copied()
        .collect()
}

fn assert_backpressure_output(stdout: &[u8], mode: &str, label: &str) {
    let kind = mode.strip_prefix("backpressure-").unwrap();
    assert_eq!(
        String::from_utf8_lossy(&backpressure_protocol(stdout, mode)),
        format!("backpressure={kind} bytes={BACKPRESSURE_BYTES} reply=ok\n"),
        "{label} did not complete the backpressure protocol:\n{}",
        String::from_utf8_lossy(stdout)
    );
}

#[test]
fn backpressure_output_check_is_exact_except_for_the_observe_lines() {
    let accepts = |stdout: &str, mode: &str| {
        std::panic::catch_unwind(|| assert_backpressure_output(stdout.as_bytes(), mode, "probe"))
            .is_ok()
    };
    let blocking = format!("backpressure=blocking bytes={BACKPRESSURE_BYTES} reply=ok");
    let observe = format!("backpressure=observe bytes={BACKPRESSURE_BYTES} reply=ok");
    let observed = "observe released=1\nobserve clock_before=1.0 clock_after=2.0\n";

    assert!(accepts(&format!("{blocking}\n"), "backpressure-blocking"));
    assert!(!accepts(&blocking, "backpressure-blocking"));
    assert!(!accepts(
        &format!("{blocking}\r\n"),
        "backpressure-blocking"
    ));
    assert!(!accepts(
        &format!("{blocking}\n\n"),
        "backpressure-blocking"
    ));
    // Outside the observe mode an "observe " line is ordinary output.
    assert!(!accepts(
        &format!("observe released=1\n{blocking}\n"),
        "backpressure-blocking"
    ));

    assert!(accepts(&format!("{observed}{observe}\n"), OBSERVE_MODE));
    assert!(!accepts(&format!("{observed}{observe}"), OBSERVE_MODE));
    assert!(!accepts(&format!("{observed}{observe}\r\n"), OBSERVE_MODE));
}

fn assert_backpressure_report(report: &str) {
    let digest = fnv1a64(
        &(0..BACKPRESSURE_BYTES)
            .map(|index| (index * 131 + (index >> 9)) as u8)
            .collect::<Vec<_>>(),
    );
    assert_eq!(
        report,
        format!("controller=backpressure\nbytes={BACKPRESSURE_BYTES}\nfnv1a64={digest:016x}\n"),
        "backpressure controller observed a changed data stream"
    );
}

fn trace_version(trace: &[u8]) -> u32 {
    let start = detcore_model::network_trace::NETWORK_TRACE_MAGIC.len();
    u32::from_le_bytes(trace[start..start + 4].try_into().unwrap())
}

/// Records `mode` against a live backpressure controller, which checks the
/// exact outbound stream, and checks the client's output and the trace
/// version. Returns the trace and the controller port.
fn record_backpressure(fixture: &Path, evidence: &Path, mode: &str) -> (PathBuf, String) {
    use detcore_model::network_trace::NETWORK_TRACE_VERSION_V3;
    use detcore_model::network_trace::NETWORK_TRACE_VERSION_V4;

    let trace = evidence.join(format!("{mode}.trace"));
    let controller_directory = evidence.join(format!("{mode}-controller"));
    fs::create_dir(&controller_directory).expect("create backpressure controller directory");
    let (controller, port) =
        Controller::start_mode(fixture, &controller_directory, "backpressure-controller");
    let mut arguments = run_arguments(0, 1_000_000);
    arguments.push(format!("--record-networking={}", trace.display()));
    let label = format!("record-{mode}");
    let recorded = hermit_command(
        evidence,
        &label,
        &arguments,
        fixture,
        &["client", &port, mode],
    );
    assert_success(&recorded, &label);
    assert_backpressure_output(&recorded.stdout, mode, &label);
    assert_backpressure_report(&controller.finish());
    let bytes = fs::read(&trace).expect("recording omitted its network trace");
    // Only a nonblocking send can be refused; a blocking one waits, and the
    // chunks it sends after waiting carry the waits.
    let expected_version = if mode == "backpressure-nonblocking" {
        NETWORK_TRACE_VERSION_V3
    } else {
        NETWORK_TRACE_VERSION_V4
    };
    assert_eq!(
        trace_version(&bytes),
        expected_version,
        "{label} recorded the wrong trace version"
    );
    (trace, port)
}

/// Replays a `mode` recording offline under each of `cells` at L2.
fn replay_backpressure_at_l2(
    fixture: &Path,
    evidence: &Path,
    mode: &str,
    trace: &Path,
    port: &str,
    cells: &[(u64, u64)],
) {
    for (seed, max_timeslice) in cells {
        let label = format!("replay-{mode}-seed-{seed}-timeslice-{max_timeslice}");
        let report = evidence.join(format!("{label}.verify.json"));
        let mut arguments = run_arguments(*seed, *max_timeslice);
        arguments.extend([
            "--verify".into(),
            "--verify-strict".into(),
            format!("--verify-json={}", report.display()),
            format!("--replay-networking={}", trace.display()),
        ]);
        let replayed = hermit_command(
            evidence,
            &label,
            &arguments,
            fixture,
            &["client", port, mode],
        );
        assert_success(&replayed, &label);
        assert_backpressure_output(&replayed.stdout, mode, &label);
        assert_l2_report(&report, &label);
    }
}

/// A thread stuck on a full send buffer must not hold back the thread that
/// the peer waits for. A nonblocking send reports EAGAIN promptly, which
/// the recording holds so that replay reports it at the same point.
#[test]
fn tcp_backpressure_nonblocking_records_and_replays_without_stalling_other_threads() {
    let _guard = super::hermit_record_lock();
    let fixture = &super::workload("c_network_replay_tcp_bracket").path;
    let directory = tempfile::tempdir().expect("create backpressure evidence directory");
    let evidence = directory.path();
    let mode = "backpressure-nonblocking";
    let (trace, port) = record_backpressure(fixture, evidence, mode);
    replay_backpressure_at_l2(fixture, evidence, mode, &trace, &port, &REPLAY_CELLS[1..3]);
}

/// A thread stuck on a full send buffer must not hold back the thread that
/// the peer waits for. A blocking send yields while it waits.
#[test]
fn tcp_backpressure_blocking_records_and_replays_without_stalling_other_threads() {
    let _guard = super::hermit_record_lock();
    let fixture = &super::workload("c_network_replay_tcp_bracket").path;
    let directory = tempfile::tempdir().expect("create backpressure evidence directory");
    let evidence = directory.path();
    let mode = "backpressure-blocking";
    let (trace, port) = record_backpressure(fixture, evidence, mode);
    replay_backpressure_at_l2(fixture, evidence, mode, &trace, &port, &REPLAY_CELLS[1..3]);
}

/// Replay refuses a blocking send where the recording holds a refused
/// nonblocking one, and record refuses an output event that another thread
/// makes while a blocking send waits.
#[test]
fn tcp_backpressure_refuses_a_diverged_replay_and_an_interleaved_recording() {
    let _guard = super::hermit_record_lock();
    let fixture = &super::workload("c_network_replay_tcp_bracket").path;
    let directory = tempfile::tempdir().expect("create backpressure evidence directory");
    let evidence = directory.path();

    // A blocking send where the recording holds a refused nonblocking send
    // diverged from the recording.
    let (trace, port) = record_backpressure(fixture, evidence, "backpressure-nonblocking");
    let mut arguments = run_arguments(0, 1_000_000);
    arguments.push(format!("--replay-networking={}", trace.display()));
    let diverged = hermit_command(
        evidence,
        "replay-blocking-at-refusal",
        &arguments,
        fixture,
        &["client", &port, "backpressure-blocking"],
    );
    assert_refused(
        &diverged,
        "replay-blocking-at-refusal",
        "where the recording holds a nonblocking send refused with EAGAIN",
        "diverged from the recording",
    );

    // Another thread's output event while a blocking send waits, here a
    // refusal before the waiting send's first accepted byte, would be
    // replayed in the other order, so record refuses it.
    let controller_directory = evidence.join("interleaved-controller");
    fs::create_dir(&controller_directory).expect("create interleaved controller directory");
    let (_controller, port) =
        Controller::start_mode(fixture, &controller_directory, "backpressure-controller");
    let mut arguments = run_arguments(0, 1_000_000);
    arguments.push(format!(
        "--record-networking={}",
        evidence.join("interleaved.trace").display()
    ));
    let interleaved = hermit_command(
        evidence,
        "record-interleaved",
        &arguments,
        fixture,
        &["client", &port, INTERLEAVED_MODE],
    );
    assert_refused(
        &interleaved,
        "record-interleaved",
        "accepted or refused, reached stream offset",
        "one thread at a time",
    );
}

/// The scheduler's report of each network-send retry in `stderr`, in order,
/// from the waiting thread onwards; the turn number is left out.
fn network_send_retries(stderr: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(stderr)
        .lines()
        .filter(|line| line.contains("polling resource") && line.contains("\"network send\""))
        .filter_map(|line| {
            line.find("SKIP dettid")
                .map(|start| line[start..].to_owned())
        })
        .collect()
}

/// Replay completes a blocking send that waited for buffer space when the
/// recording did: after as many waits, at the same virtual time, and so
/// after the other thread whose message let the peer drain it. Each
/// difference would change what the program observes; see
/// https://github.com/rrnewton/hermit/issues/3792.
#[test]
fn tcp_backpressure_replay_completes_a_waited_send_when_the_recording_did() {
    let _guard = super::hermit_record_lock();
    let fixture = &super::workload("c_network_replay_tcp_bracket").path;
    let directory = tempfile::tempdir().expect("create backpressure evidence directory");
    let evidence = directory.path();
    let mode = OBSERVE_MODE;
    let (trace, port) = record_backpressure(fixture, evidence, mode);
    let recorded_stdout = fs::read(evidence.join(format!("record-{mode}.stdout")))
        .expect("read the recording's stdout");
    let recorded_stderr = fs::read(evidence.join(format!("record-{mode}.stderr")))
        .expect("read the recording's stderr");
    let recorded = String::from_utf8_lossy(&recorded_stdout);
    // The peer drains nothing before the go thread's message.
    assert!(
        recorded.contains("observe released=1\n"),
        "the recording completed the send before the go thread ran:\n{recorded}"
    );
    let recorded_retries = network_send_retries(&recorded_stderr);
    assert!(
        !recorded_retries.is_empty(),
        "the recording's send never waited for buffer space"
    );

    // The same configuration as the recording, offline.
    let label = format!("replay-{mode}");
    let mut arguments = run_arguments(0, 1_000_000);
    arguments.push(format!("--replay-networking={}", trace.display()));
    let replayed = hermit_command(
        evidence,
        &label,
        &arguments,
        fixture,
        &["client", &port, mode],
    );
    assert_success(&replayed, &label);
    assert_backpressure_output(&replayed.stdout, mode, &label);
    let replayed_text = String::from_utf8_lossy(&replayed.stdout);
    let mut mismatches = Vec::new();
    for prefix in ["observe released=", "observe clock_before="] {
        let line = |text: &str| {
            text.lines()
                .find(|line| line.starts_with(prefix))
                .map(str::to_owned)
        };
        let (recorded_line, replayed_line) = (line(&recorded), line(&replayed_text));
        if recorded_line != replayed_line {
            mismatches.push(format!(
                "record printed {recorded_line:?}, replay printed {replayed_line:?}"
            ));
        }
    }
    if replayed.stdout != recorded_stdout {
        mismatches.push(format!(
            "stdout differs:\nrecord:\n{recorded}replay:\n{replayed_text}"
        ));
    }
    let replayed_retries = network_send_retries(&replayed.stderr);
    if replayed_retries != recorded_retries {
        mismatches.push(format!(
            "record retried the send {} times, replay {} times; the first difference is at retry {}",
            recorded_retries.len(),
            replayed_retries.len(),
            recorded_retries
                .iter()
                .zip(&replayed_retries)
                .take_while(|(left, right)| left == right)
                .count()
        ));
    }
    assert!(
        mismatches.is_empty(),
        "replay did not complete the waited send when the recording did:\n{}",
        mismatches.join("\n")
    );
}

/// Record refuses to resume a waiting send once another thread replaced its
/// descriptor number, before any byte reaches the replacement connection.
/// Linux would finish the send on the original connection, which a dup
/// keeps open; Hermit's injected sends name the number, not the connection.
#[test]
fn tcp_backpressure_record_refuses_a_waiting_send_whose_descriptor_was_replaced() {
    let _guard = super::hermit_record_lock();
    let fixture = &super::workload("c_network_replay_tcp_bracket").path;
    let directory = tempfile::tempdir().expect("create backpressure evidence directory");
    let evidence = directory.path();
    let controller_directory = evidence.join("replace-controller");
    fs::create_dir(&controller_directory).expect("create replace controller directory");
    let (controller, port) =
        Controller::start_mode(fixture, &controller_directory, "replace-controller");
    let mut arguments = run_arguments(0, 1_000_000);
    arguments.push(format!(
        "--record-networking={}",
        evidence.join("replace.trace").display()
    ));
    let label = "record-replace";
    let recorded = hermit_command(
        evidence,
        label,
        &arguments,
        fixture,
        &["client", &port, REPLACE_MODE],
    );
    assert_refused(
        &recorded,
        label,
        "network trace does not model closing or replacing a descriptor while a send on it waits for buffer space",
        "supports only outbound TCP clients",
    );
    let report = controller.finish();
    assert!(
        report.starts_with("controller=replace\n") && report.ends_with("\nreplacement_bytes=0\n"),
        "the replacement connection received part of the waiting send:\n{report}"
    );
}

/// The digest of the shared mode's stream after its child rewrote the
/// second half.
fn shared_stream_digest() -> u64 {
    fnv1a64(
        &(0..BACKPRESSURE_BYTES)
            .map(|index| {
                let byte = (index * 131 + (index >> 9)) as u8;
                if index < BACKPRESSURE_BYTES / 2 {
                    byte
                } else {
                    !byte
                }
            })
            .collect::<Vec<_>>(),
    )
}

/// The trace holds the bytes each resumed send injected, read from the
/// guest's buffer just before it: here the half that a child process
/// rewrote while the send waited, as Linux sent it. Replay reads the buffer
/// at the same points and matches.
#[test]
fn tcp_backpressure_records_the_bytes_each_resumed_send_injected() {
    use detcore_model::network_trace::NETWORK_TRACE_VERSION_V4;
    use detcore_model::network_trace::NetworkTraceV2;

    let _guard = super::hermit_record_lock();
    let fixture = &super::workload("c_network_replay_tcp_bracket").path;
    let directory = tempfile::tempdir().expect("create backpressure evidence directory");
    let evidence = directory.path();
    let mode = SHARED_MODE;
    let trace = evidence.join(format!("{mode}.trace"));
    let controller_directory = evidence.join("shared-controller");
    fs::create_dir(&controller_directory).expect("create shared controller directory");
    let (controller, port) =
        Controller::start_mode(fixture, &controller_directory, "shared-controller");
    let mut arguments = run_arguments(0, 1_000_000);
    arguments.push(format!("--record-networking={}", trace.display()));
    let label = format!("record-{mode}");
    let recorded = hermit_command(
        evidence,
        &label,
        &arguments,
        fixture,
        &["client", &port, mode],
    );
    assert_success(&recorded, &label);
    assert_backpressure_output(&recorded.stdout, mode, &label);
    let digest = shared_stream_digest();
    assert_eq!(
        controller.finish(),
        format!("controller=shared\nbytes={BACKPRESSURE_BYTES}\nfnv1a64={digest:016x}\n"),
        "the peer did not receive the rewritten stream"
    );

    let bytes = fs::read(&trace).expect("recording omitted its network trace");
    assert_eq!(
        trace_version(&bytes),
        NETWORK_TRACE_VERSION_V4,
        "{label} recorded the wrong trace version"
    );
    let decoded = NetworkTraceV2::read_framed(&bytes[..]).expect("decode the recorded trace");
    let streams = decoded
        .channels
        .iter()
        .map(|channel| {
            decoded
                .outputs
                .iter()
                .filter(|output| output.channel == channel.id)
                .flat_map(|output| output.bytes.iter().copied())
                .collect::<Vec<_>>()
        })
        .filter(|stream| stream.len() == BACKPRESSURE_BYTES)
        .collect::<Vec<_>>();
    assert_eq!(
        streams.len(),
        1,
        "the trace must hold exactly one data stream"
    );
    assert_eq!(
        format!("{:016x}", fnv1a64(&streams[0])),
        format!("{digest:016x}"),
        "the trace holds bytes the peer did not receive"
    );

    replay_backpressure_at_l2(fixture, evidence, mode, &trace, &port, &REPLAY_CELLS[..1]);
}

/// Records the fixture's `match` client against a live controller, which
/// checks the exact outbound stream. Returns the trace, its bytes, and the
/// controller port the recording connected to, which replay must name.
/// A network recording writes `TRACE.manifest.json` beside its trace
/// (https://github.com/rrnewton/hermit/issues/4271): the run's options with
/// the epoch it drew from the host clock written out, and the trace's
/// version, size and digest. `hermit run --from` that manifest replays the
/// recording offline, with no server and no retyped options, and gives the
/// recording's output; a trace that changed since is refused.
fn assert_network_manifest_reruns_offline(
    fixture: &Path,
    evidence: &Path,
    trace: &Path,
    original_trace: &[u8],
    port: &str,
) {
    use detcore_model::network_trace::NETWORK_TRACE_MAGIC;

    let manifest_path = trace.with_file_name("network.trace.manifest.json");
    let manifest: serde_json::Value = serde_json::from_slice(
        &fs::read(&manifest_path).expect("a network recording writes its manifest"),
    )
    .unwrap();
    assert_eq!(manifest["kind"], "network", "{manifest}");
    let recorded = &manifest["artifacts"]["network_trace"];
    assert_eq!(recorded["path"], "network.trace", "{manifest}");
    assert_eq!(recorded["format"], "hermit-net-trace", "{manifest}");
    assert_eq!(recorded["bytes"], original_trace.len() as u64, "{manifest}");
    assert_eq!(
        recorded["version"],
        u32::from_le_bytes(
            original_trace[NETWORK_TRACE_MAGIC.len()..NETWORK_TRACE_MAGIC.len() + 4]
                .try_into()
                .unwrap()
        ),
        "{manifest}"
    );
    assert_eq!(
        recorded["sha256"],
        detcore::Digest::new(original_trace).to_string(),
        "{manifest}"
    );
    assert_eq!(
        manifest["inputs"]["host_drawn"]["epoch"], "host-now",
        "{manifest}"
    );
    assert!(
        manifest["inputs"]["run"].get("record-networking").is_none(),
        "the trace path is an output, not an input: {manifest}"
    );

    // A bare `--from`: the guest command and every option come from the
    // manifest. The recording and its manifest also move together: the
    // manifest names its trace relative to its own directory.
    let moved = evidence.join("moved");
    fs::create_dir(&moved).unwrap();
    fs::copy(trace, moved.join("network.trace")).unwrap();
    fs::copy(&manifest_path, moved.join("network.trace.manifest.json")).unwrap();
    for (label, from) in [
        ("rerun-from-network-manifest", manifest_path.clone()),
        (
            "rerun-from-a-moved-network-manifest",
            moved.join("network.trace.manifest.json"),
        ),
    ] {
        let mut command = Command::new("timeout");
        command
            .args(["--kill-after=5s", &format!("{OUTER_WALL_SECONDS}s")])
            .env_remove("LD_LIBRARY_PATH")
            .arg(env!("CARGO_BIN_EXE_hermit"))
            .arg("run")
            .arg("--from")
            .arg(&from)
            .current_dir(evidence)
            .stdin(std::process::Stdio::null());
        let rerun = command.output().expect("hermit should start");
        fs::write(evidence.join(format!("{label}.stdout")), &rerun.stdout).unwrap();
        fs::write(evidence.join(format!("{label}.stderr")), &rerun.stderr).unwrap();
        assert_success(&rerun, label);
        assert_guest_invariants(&rerun.stdout, label);
    }
    assert_eq!(
        fs::read(trace).unwrap(),
        original_trace,
        "a rerun replays the trace and leaves it alone"
    );

    // A trace that is not the one the manifest recorded is refused before
    // the guest starts.
    let copy = evidence.join("changed");
    fs::create_dir(&copy).unwrap();
    let mut changed = original_trace.to_vec();
    *changed.last_mut().unwrap() ^= 1;
    fs::write(copy.join("network.trace"), &changed).unwrap();
    fs::copy(&manifest_path, copy.join("network.trace.manifest.json")).unwrap();
    let refused = hermit_command(
        evidence,
        "rerun-from-a-changed-trace",
        &[
            "run".into(),
            "--from".into(),
            copy.join("network.trace.manifest.json")
                .display()
                .to_string(),
        ],
        fixture,
        &["client", port, "match"],
    );
    assert_fails_naming(
        &refused,
        "rerun from a changed trace",
        "is not the one its manifest recorded",
    );
}

fn record_match_trace(fixture: &Path, evidence: &Path) -> (PathBuf, Vec<u8>, String) {
    let trace = evidence.join("network.trace");
    let record_directory = evidence.join("record-controller");
    fs::create_dir(&record_directory).expect("create record controller directory");
    let (controller, port) = Controller::start(fixture, &record_directory);
    let mut record_arguments = run_arguments(0, 1_000_000);
    record_arguments.push(format!("--record-networking={}", trace.display()));
    let recorded = hermit_command(
        evidence,
        "record-networking",
        &record_arguments,
        fixture,
        &["client", &port, "match"],
    );
    assert_success(&recorded, "network recording");
    assert_guest_invariants(&recorded.stdout, "network recording");
    assert_controller_report(&controller.finish());
    let original_trace = fs::read(&trace).expect("recording omitted its network trace");
    assert!(
        !original_trace.is_empty(),
        "recording wrote an empty network trace"
    );
    (trace, original_trace, port)
}

/// Fixture modes that each try a way into the host's network namespace
/// outside an outbound TCP channel, with the refusal reason and remedy. The
/// refusals do not depend on the mode, so record and replay both run them.
const OUTSIDE_CHANNEL_REFUSALS: &[(&str, &str, &str)] = &[
    (
        "udp",
        "does not model sendto on an IPv4 or IPv6 socket that is not a connected TCP client",
        "--network=host",
    ),
    (
        "listen",
        "does not model bind on an IPv4 or IPv6 socket",
        "--network=host",
    ),
    (
        "netlink",
        "does not model netlink sockets",
        "--network=host",
    ),
    (
        "abstract",
        "does not model connect to an abstract AF_UNIX address",
        "--network=host",
    ),
    (
        "rcvtimeo",
        "does not model socket send or receive timeouts",
        "--network=host",
    ),
    (
        "rcvtimeo-negative",
        "does not model socket send or receive timeouts",
        "--network=host",
    ),
    (
        "scm-rights",
        "does not model a socket of address family 2 received through SCM_RIGHTS",
        "--network=host",
    ),
    (
        "epoll",
        "does not model epoll registration of an IPv4 or IPv6 socket",
        "--network=host",
    ),
    (
        "async",
        "does not model signal-driven I/O",
        "--network=host",
    ),
    (
        "ifindex",
        "does not model the interface and route ioctl 0x8933",
        "--network=host",
    ),
    (
        "procnet",
        "does not model reading /proc/net/dev",
        "--network=host",
    ),
];

#[test]
fn tcp_recording_replays_offline_across_schedules_and_refuses_mismatch() {
    let _guard = super::hermit_record_lock();
    let fixture = &super::workload("c_network_replay_tcp_bracket").path;
    let directory = tempfile::tempdir().expect("create network replay evidence directory");
    let evidence = directory.path();

    // With no networking flag, run behaves as it always has: the guest's
    // private network namespace cannot reach the host's loopback listener.
    let unused = evidence.join("no-flag-controller");
    fs::create_dir(&unused).expect("create no-flag controller directory");
    let (unused_controller, unused_port) = Controller::start(fixture, &unused);
    let no_flag = hermit_command(
        evidence,
        "no-networking-flag",
        &run_arguments(0, 1_000_000),
        fixture,
        &["client", &unused_port, "match"],
    );
    assert_fails_naming(&no_flag, "run without a networking flag", "connect client");
    unused_controller.stop_without_connection();

    let (trace, original_trace, port) = record_match_trace(fixture, evidence);
    assert_network_manifest_reruns_offline(fixture, evidence, &trace, &original_trace, &port);

    // Replay with no server. Each cell verifies twice under one fixed seed;
    // across cells the race winner must change while the input does not.
    let mut reader_markers = BTreeSet::new();
    for (seed, max_timeslice) in REPLAY_CELLS {
        let label = format!("replay-seed-{seed}-timeslice-{max_timeslice}");
        let report = evidence.join(format!("{label}.verify.json"));
        let mut arguments = run_arguments(*seed, *max_timeslice);
        arguments.extend([
            "--verify".into(),
            "--verify-strict".into(),
            format!("--verify-json={}", report.display()),
            format!("--replay-networking={}", trace.display()),
        ]);
        let replayed = hermit_command(
            evidence,
            &label,
            &arguments,
            fixture,
            &["client", &port, "match"],
        );
        assert_success(&replayed, &label);
        reader_markers.insert(assert_guest_invariants(&replayed.stdout, &label));
        assert_l2_report(&report, &label);
        assert_eq!(
            fs::read(&trace).expect("replay removed its network trace"),
            original_trace,
            "{label} changed the network trace"
        );
    }
    assert!(
        reader_markers.len() >= 2,
        "the replay seeds and timeslices exposed no schedule variation: {reader_markers:?}"
    );

    // A changed outbound byte ends replay rather than accepting a different
    // request stream.
    let mut mismatch_arguments = run_arguments(0, 1_000_000);
    mismatch_arguments.push(format!("--replay-networking={}", trace.display()));
    let mismatch = hermit_command(
        evidence,
        "outbound-mismatch",
        &mismatch_arguments,
        fixture,
        &["client", &port, "mismatch"],
    );
    assert_fails_naming(
        &mismatch,
        "outbound stream mismatch",
        "network outbound mismatch",
    );
}

/// The select client waits on a channel and a pipe with select, pselect6 and
/// glibc's select and pselect, and checks each result against Linux's. Record
/// must complete it against a live controller, and replay offline must print
/// the same output and verify at L2.
#[test]
fn tcp_select_family_records_and_replays_offline() {
    let _guard = super::hermit_record_lock();
    let fixture = &super::workload("c_network_replay_tcp_bracket").path;
    let directory = tempfile::tempdir().expect("create select evidence directory");
    let evidence = directory.path();
    let trace = evidence.join("select.trace");
    let record_directory = evidence.join("record-controller");
    fs::create_dir(&record_directory).expect("create select record controller directory");
    let (controller, port) = Controller::start(fixture, &record_directory);
    let mut record_arguments = run_arguments(0, 1_000_000);
    record_arguments.push(format!("--record-networking={}", trace.display()));
    let recorded = hermit_command(
        evidence,
        "select-record",
        &record_arguments,
        fixture,
        &["client", &port, "select"],
    );
    assert_success(&recorded, "select recording");
    assert_eq!(
        String::from_utf8_lossy(&recorded.stdout),
        expected_select_output(),
        "select recording"
    );
    assert_controller_report(&controller.finish());
    let original_trace = fs::read(&trace).expect("select recording omitted its network trace");

    for (seed, max_timeslice) in &REPLAY_CELLS[..2] {
        let label = format!("select-replay-seed-{seed}-timeslice-{max_timeslice}");
        let report = evidence.join(format!("{label}.verify.json"));
        let mut arguments = run_arguments(*seed, *max_timeslice);
        arguments.extend([
            "--verify".into(),
            "--verify-strict".into(),
            format!("--verify-json={}", report.display()),
            format!("--replay-networking={}", trace.display()),
        ]);
        let replayed = hermit_command(
            evidence,
            &label,
            &arguments,
            fixture,
            &["client", &port, "select"],
        );
        assert_success(&replayed, &label);
        assert_eq!(
            String::from_utf8_lossy(&replayed.stdout),
            expected_select_output(),
            "{label}"
        );
        assert_l2_report(&report, &label);
        assert_eq!(
            fs::read(&trace).expect("select replay removed its network trace"),
            original_trace,
            "{label} changed the network trace"
        );
    }
}

#[test]
fn tcp_replay_refuses_divergence_and_operations_outside_a_channel() {
    let _guard = super::hermit_record_lock();
    let fixture = &super::workload("c_network_replay_tcp_bracket").path;
    let directory = tempfile::tempdir().expect("create network replay evidence directory");
    let evidence = directory.path();
    let (trace, _, port) = record_match_trace(fixture, evidence);

    // A replay that ends with part of the recording unsent, sends through a
    // call the engine does not model, connects where the recording did not,
    // or reaches the network outside a channel is refused with a remedy.
    let divergences = [
        (
            "truncated",
            "network replay ended after",
            "diverged from the recording",
        ),
        (
            "sendmsg",
            "does not model sendmsg on a recorded socket",
            "--network=host",
        ),
        (
            "sendfile",
            "does not model sendfile on a recorded socket",
            "--network=host",
        ),
        // Linux select reads nfds bits, so an alias above FD_SETSIZE names
        // the channel too.
        (
            "select-high",
            "does not model select with nfds above 64 on a recorded socket",
            "--network=host",
        ),
        (
            "pselect-high",
            "does not model pselect6 with nfds above 64 on a recorded socket",
            "--network=host",
        ),
        (
            "pselect-mask",
            "does not model pselect6 with a signal mask pointer on a recorded socket",
            "--network=host",
        ),
        (
            "unspecified",
            "network trace mismatch",
            "diverged from the recording",
        ),
        (
            "ipv6-24",
            "network trace mismatch",
            "diverged from the recording",
        ),
    ];
    for (mode, reason, remedy) in divergences.iter().chain(OUTSIDE_CHANNEL_REFUSALS) {
        let label = format!("replay-refuses-{mode}");
        let mut arguments = run_arguments(0, 1_000_000);
        arguments.push(format!("--replay-networking={}", trace.display()));
        let refused = hermit_command(
            evidence,
            &label,
            &arguments,
            fixture,
            &["client", &port, mode],
        );
        assert_refused(&refused, &label, reason, remedy);
    }

    // A missing trace is refused before the guest starts.
    let mut missing_arguments = run_arguments(0, 1_000_000);
    missing_arguments.push(format!(
        "--replay-networking={}",
        evidence.join("missing.trace").display()
    ));
    let missing = hermit_command(
        evidence,
        "missing-trace",
        &missing_arguments,
        fixture,
        &["client", &port, "match"],
    );
    assert_fails_naming(
        &missing,
        "missing network trace",
        "cannot open network trace",
    );
    assert!(
        !String::from_utf8_lossy(&missing.stdout).contains("aggregate="),
        "the guest ran without its network trace"
    );
}

#[test]
fn tcp_record_refuses_operations_outside_a_channel() {
    let _guard = super::hermit_record_lock();
    let fixture = &super::workload("c_network_replay_tcp_bracket").path;
    let directory = tempfile::tempdir().expect("create network record evidence directory");
    let evidence = directory.path();

    // Record refuses, before touching the host, anything that would reach
    // the network outside an outbound TCP channel, or the host's network
    // namespace at all, or wait in wall-clock time. A controller listens, and
    // no probe may reach its accept boundary.
    let (controller, port) = Controller::start(fixture, evidence);
    let untraceable = [
        ("unspecified", "names no single host", "nonzero port"),
        // A 24-byte IPv6 address, without sin6_scope_id, is still traced.
        ("ipv6-24", "names no single host", "nonzero port"),
    ];
    for (mode, reason, remedy) in untraceable.iter().chain(OUTSIDE_CHANNEL_REFUSALS) {
        let label = format!("record-refuses-{mode}");
        let mut arguments = run_arguments(0, 1_000_000);
        arguments.push(format!(
            "--record-networking={}",
            evidence.join(format!("{label}.trace")).display()
        ));
        let refused = hermit_command(
            evidence,
            &label,
            &arguments,
            fixture,
            &["client", &port, mode],
        );
        assert_refused(&refused, &label, reason, remedy);
    }
    controller.stop_without_connection();
}

/// Linux reads the `nfds` of `poll` and `ppoll` as an `unsigned int`. Record
/// reads the pollfd array before it knows whether the call names a channel,
/// so it must size that read the same way: the fixture's poll and ppoll on a
/// readable pipe pass `nfds` with its high word set and its low word 1, and
/// each must return 1 under record as natively, not fail the run.
#[test]
fn record_reads_poll_family_nfds_as_linux_does() {
    let _guard = super::hermit_record_lock();
    let fixture = &super::workload("c_network_replay_tcp_bracket").path;
    let directory = tempfile::tempdir().expect("create network record evidence directory");
    let evidence = directory.path();
    let expected = "poll-high-word poll=1/pollin ppoll=1/pollin\n";

    let native = bounded_command(
        fixture,
        &[OsStr::new("poll-high-word")],
        NATIVE_CLIENT_WALL_SECONDS,
    );
    assert_success(&native, "native poll-high-word");
    assert_eq!(String::from_utf8_lossy(&native.stdout), expected);

    let label = "record-poll-high-word";
    let mut arguments = run_arguments(0, 1_000_000);
    arguments.push(format!(
        "--record-networking={}",
        evidence.join(format!("{label}.trace")).display()
    ));
    let recorded = hermit_command(evidence, label, &arguments, fixture, &["poll-high-word"]);
    assert_success(&recorded, label);
    assert_eq!(String::from_utf8_lossy(&recorded.stdout), expected);
}

const HTTP_RESPONSE: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\nConnection: close\r\n\r\nhello world\n";

/// The host's curl, which this test runs unmodified as the guest.
fn curl() -> PathBuf {
    std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .map(|directory| directory.join("curl"))
        .find(|candidate| candidate.is_file())
        .expect("curl is not on PATH; install curl to run the curl network record/replay test")
}

/// Starts the one-shot loopback HTTP server and returns it with its port.
fn start_http_server(directory: &Path) -> (Child, String) {
    let server = &super::workload("c_localhost_http_server").path;
    let port_path = directory.join("http.port");
    let response_path = directory.join("http.response");
    fs::write(&response_path, HTTP_RESPONSE).expect("write HTTP response");
    let mut child = Command::new("timeout")
        .args(["--kill-after=1s", &format!("{CONTROLLER_WALL_SECONDS}s")])
        .arg(server)
        .arg(&port_path)
        .arg(&response_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to start bounded HTTP server");
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Ok(port) = fs::read_to_string(&port_path)
            && let Ok(number) = port.trim().parse::<u16>()
            && number != 0
        {
            return (child, number.to_string());
        }
        if child
            .try_wait()
            .expect("failed to inspect HTTP server")
            .is_some()
        {
            let output = child.wait_with_output().expect("collect HTTP server");
            panic!(
                "HTTP server exited before publishing its port: {}\nstderr:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert!(
            Instant::now() < deadline,
            "HTTP server did not publish its port within two seconds"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn curl_arguments(log: &Path, network_flag: String) -> Vec<String> {
    vec![
        "--backend=ptrace".into(),
        "--log=info".into(),
        format!("--log-file={}", log.display()),
        "run".into(),
        "--strict".into(),
        network_flag,
    ]
}

#[test]
fn curl_recording_replays_offline_with_identical_logs() {
    let _guard = super::hermit_record_lock();
    let curl = curl();
    let directory = tempfile::tempdir().expect("create curl network replay directory");
    let evidence = directory.path();
    let trace = evidence.join("curl.trace");
    let record_log = evidence.join("record.log");
    let replay_log = evidence.join("replay.log");

    let (server, port) = start_http_server(evidence);
    let url = format!("http://127.0.0.1:{port}/");
    let curl_arguments_for =
        |log: &Path, flag: &str| curl_arguments(log, format!("{flag}={}", trace.display()));
    let recorded = hermit_command(
        evidence,
        "curl-record",
        &curl_arguments_for(&record_log, "--record-networking"),
        &curl,
        &["-sS", &url],
    );
    let server = server
        .wait_with_output()
        .expect("collect bounded HTTP server");
    assert_success(&recorded, "curl recording");
    assert!(
        server.status.success(),
        "HTTP server failed with {}\nstderr:\n{}",
        server.status,
        String::from_utf8_lossy(&server.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&recorded.stdout),
        "hello world\n",
        "curl recording did not print the served body"
    );

    // The server has served its one connection and exited, so replay can
    // only succeed from the trace.
    let replayed = hermit_command(
        evidence,
        "curl-replay",
        &curl_arguments_for(&replay_log, "--replay-networking"),
        &curl,
        &["-sS", &url],
    );
    assert_success(&replayed, "curl replay");
    assert_eq!(
        replayed.stdout, recorded.stdout,
        "curl replay printed different output from the recording"
    );

    let diff_report = evidence.join("record-vs-replay.json");
    let diff = Command::new("timeout")
        .args(["--kill-after=1s", &format!("{NATIVE_CLIENT_WALL_SECONDS}s")])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .arg("log-diff")
        .arg(format!("--json={}", diff_report.display()))
        .arg(&record_log)
        .arg(&replay_log)
        .output()
        .expect("failed to start hermit log-diff");
    assert_success(&diff, "log-diff of the curl record and replay logs");
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(&diff_report).expect("log-diff omitted its JSON report"))
            .expect("log-diff JSON was malformed");
    assert_eq!(report["verdict"], "matched", "log-diff report: {report}");
    for side in ["left", "right"] {
        assert!(
            report["selected_messages"][side]
                .as_u64()
                .is_some_and(|count| count > 0),
            "log-diff compared no INFO messages on {side}: {report}"
        );
    }

    let verify_report = evidence.join("curl-replay.verify.json");
    let mut verify_arguments =
        curl_arguments_for(&evidence.join("verify.log"), "--replay-networking");
    verify_arguments.extend([
        "--verify".into(),
        "--verify-strict".into(),
        format!("--verify-json={}", verify_report.display()),
    ]);
    let verified = hermit_command(
        evidence,
        "curl-replay-verify",
        &verify_arguments,
        &curl,
        &["-sS", &url],
    );
    assert_success(&verified, "curl replay under --verify-strict");
    assert_eq!(verified.stdout, recorded.stdout);
    assert_l2_report(&verify_report, "curl replay under --verify-strict");
}
