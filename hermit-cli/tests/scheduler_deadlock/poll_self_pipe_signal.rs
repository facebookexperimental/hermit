// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
// Licensed under the BSD-style license in the LICENSE file.

//! External signals are not deterministic inputs. These regressions preserve
//! the ptrace path's existing ability to interrupt a polled read, alongside
//! controls whose signal cannot run a handler. The native and Hermit runs must
//! agree on the byte count, handler count and SA_RESTART result.

use std::fs;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Child;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use super::hermit_test as hermit_binary;

const RUN_BOUND: Duration = Duration::from_secs(15);
const SIGNAL_DELAY: Duration = Duration::from_millis(500);

struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            // The test creates this private process group while its owner is
            // alive. On failure kill the tracer and its guest, then reap it.
            unsafe { libc::kill(-(self.0.id() as i32), libc::SIGKILL) };
            let _ = self.0.wait();
        }
    }
}

/// Keep the process-group leader unreaped until the stderr pipe has closed.
/// This reserves its identity even if a descendant outlives it, so cleanup
/// can safely kill our private group on the same deadline as the run.
struct ObservedGroup {
    child: Child,
    reaped: bool,
}

impl ObservedGroup {
    fn exited(&self) -> bool {
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.child.id(),
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        assert_eq!(
            result,
            0,
            "observe guest exit: {}",
            std::io::Error::last_os_error()
        );
        unsafe { info.si_pid() != 0 }
    }

    fn reap(&mut self) -> std::process::ExitStatus {
        // The leader's unreaped exit keeps this group identity reserved.
        // Clean up any remaining descendants before releasing that identity.
        unsafe { libc::kill(-(self.child.id() as i32), libc::SIGKILL) };
        let status = self.child.wait().expect("reap observed guest");
        self.reaped = true;
        status
    }
}

impl Drop for ObservedGroup {
    fn drop(&mut self) {
        if !self.reaped {
            unsafe { libc::kill(-(self.child.id() as i32), libc::SIGKILL) };
            let _ = self.child.wait();
        }
    }
}

fn guest_matches(program: &Path) -> Vec<i32> {
    fs::read_dir("/proc")
        .expect("read /proc")
        .flatten()
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse::<i32>().ok()?;
            let args = fs::read(entry.path().join("cmdline")).ok()?;
            (args.split(|byte| *byte == 0).next()? == program.as_os_str().as_bytes()).then_some(pid)
        })
        .collect()
}

fn guest_pid(program: &Path) -> i32 {
    let matches = guest_matches(program);
    assert_eq!(matches.len(), 1, "guest {}: {matches:?}", program.display());
    matches[0]
}

fn send_to_guest(program: &Path) -> i32 {
    let pid = guest_pid(program);
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    assert!(raw >= 0, "pidfd_open: {}", std::io::Error::last_os_error());
    let pidfd = unsafe { OwnedFd::from_raw_fd(raw as i32) };
    let args = fs::read(format!("/proc/{pid}/cmdline")).expect("guest still exists");
    assert_eq!(
        args.split(|byte| *byte == 0).next(),
        Some(program.as_os_str().as_bytes()),
        "guest identity changed before signaling"
    );
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            libc::SIGUSR1,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    assert_eq!(
        result,
        0,
        "send SIGUSR1: {}",
        std::io::Error::last_os_error()
    );
    pid
}

fn run(program: &Path, mode: &str, hermit: bool, directory: &Path) -> Output {
    let output_path = directory.join(if hermit {
        "hermit.stdout"
    } else {
        "native.stdout"
    });
    let mut command = if hermit {
        let mut command = Command::new(hermit_binary::hermit_binary());
        command.args([
            "--backend=ptrace",
            "run",
            "--strict",
            detcore_model::config::CANONICAL_TEST_EPOCH_ARG,
            "--base-env=minimal",
            "--",
        ]);
        command.arg(program).arg(mode);
        hermit_binary::configure_guest_execution(&mut command);
        command
    } else {
        let mut command = Command::new(program);
        command.arg(mode);
        command
    };
    command
        .env_remove("LD_LIBRARY_PATH")
        .stdin(Stdio::null())
        .stdout(fs::File::create(&output_path).expect("create stdout"))
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = Running(command.spawn().expect("start guest run"));
    let mut stderr = child.0.stderr.take().unwrap();
    let reader = thread::spawn(move || {
        let mut kept = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            let count = stderr.read(&mut chunk).expect("drain stderr");
            if count == 0 {
                return kept;
            }
            let keep = count.min(65_536_usize.saturating_sub(kept.len()));
            kept.extend_from_slice(&chunk[..keep]);
        }
    });
    let deadline = Instant::now() + RUN_BOUND;
    let mut ready = None;
    let mut sent = false;
    let status = loop {
        if let Some(status) = child.0.try_wait().expect("poll guest run") {
            break status;
        }
        let stdout = fs::read_to_string(&output_path).expect("read stdout");
        if ready.is_none() && stdout.lines().any(|line| line == "READY") {
            ready = Some(Instant::now());
        }
        if mode.starts_with("caught")
            && !sent
            && ready.is_some_and(|at| at.elapsed() >= SIGNAL_DELAY)
        {
            send_to_guest(program);
            sent = true;
        }
        assert!(
            Instant::now() < deadline,
            "{mode}, hermit={hermit}: run timed out\n{stdout}"
        );
        thread::sleep(Duration::from_millis(10));
    };
    let stderr = reader.join().expect("stderr reader");
    let stdout = fs::read(&output_path).expect("read stdout after exit");
    Output {
        status,
        stdout,
        stderr,
    }
}

const READER_DEADLOCK: &str = "Deadlock detected: thread(s) retrying a pipe or socket-pair call no remaining thread can satisfy";

#[derive(Debug, Default)]
struct WaitTrace {
    empty_probe: bool,
    usr1_deliveries: usize,
    usr1_sigreturn: bool,
    read_after_sigreturn: bool,
    deadlock: bool,
    deadlock_after_return: bool,
}

impl WaitTrace {
    fn observe(&mut self, line: &str, wait_name: &str) {
        // Both the generic loop and the kernel-signal-state loop emit this
        // only after a real nonblocking probe found nothing to report.
        if line.contains("Retry #1 for syscall due to result ")
            && line.contains(&format!(": {wait_name}("))
            && (wait_name == "read" || line.contains("due to result Ok(0):"))
        {
            self.empty_probe = true;
        }
        if line.contains("finish delivering signal (") && line.trim_end().ends_with(" SIGUSR1") {
            self.usr1_deliveries += 1;
        }
        // The ptrace backend reports rt_sigreturn directly, rather than as
        // an ordinary syscall callback. Bind the report to our sole SIGUSR1
        // delivery, then require the restarted guest read too: the report
        // itself precedes execution of the return instruction.
        if self.usr1_deliveries == 1
            && line.contains("signal-state copy: rt_sigreturn reported, installing ")
        {
            self.usr1_sigreturn = true;
        }
        if self.usr1_sigreturn && line.contains("inbound syscall: read(") {
            self.read_after_sigreturn = true;
        }
        if line.contains(READER_DEADLOCK) {
            self.deadlock = true;
            self.deadlock_after_return = self.read_after_sigreturn;
        }
    }
}

#[derive(Debug)]
struct ObservedRun {
    output: Output,
    trace: WaitTrace,
    sent: bool,
    signal_error: Option<String>,
    native_returned_to_read: bool,
    read_fd: i32,
}

fn signal_observed_guest(program: &Path, pid: i32) -> std::io::Result<()> {
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if raw < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let pidfd = unsafe { OwnedFd::from_raw_fd(raw as i32) };
    let args = fs::read(format!("/proc/{pid}/cmdline"))?;
    assert_eq!(
        args.split(|byte| *byte == 0).next(),
        Some(program.as_os_str().as_bytes()),
        "observed guest identity changed before signaling"
    );
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            libc::SIGUSR1,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn native_waiting(pid: i32, wait_name: &str, read_fd: i32) -> bool {
    let syscall = match fs::read_to_string(format!("/proc/{pid}/syscall")) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return false,
        Err(error) => panic!("read native syscall: {error}"),
    };
    let mut fields = syscall.split_whitespace();
    let expected = match wait_name {
        "poll" => libc::SYS_poll,
        "ppoll" => libc::SYS_ppoll,
        "read" => libc::SYS_read,
        _ => unreachable!(),
    };
    if fields.next().and_then(|value| value.parse::<i64>().ok()) != Some(expected) {
        return false;
    }
    if wait_name == "read"
        && fields
            .next()
            .and_then(|value| value.strip_prefix("0x"))
            .and_then(|value| i32::from_str_radix(value, 16).ok())
            != Some(read_fd)
    {
        return false;
    }
    let status = match fs::read_to_string(format!("/proc/{pid}/status")) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return false,
        Err(error) => panic!("read native status: {error}"),
    };
    status.lines().any(|line| line.starts_with("State:\tS"))
}

/// New lifecycle-sensitive cases use a witnessed empty attempt, not elapsed
/// time after READY. Original read cases above retain their existing driver.
fn run_observed(program: &Path, mode: &str, hermit: bool, directory: &Path) -> ObservedRun {
    let no_write = mode == "caught-once-no-write";
    let wait_name = if mode == "caught-poll" {
        "poll"
    } else if mode == "caught-ppoll" {
        "ppoll"
    } else {
        "read"
    };
    let output_path = directory.join(if hermit {
        "observed-hermit.stdout"
    } else {
        "observed-native.stdout"
    });
    let mut command = if hermit {
        let mut command = Command::new(hermit_binary::hermit_binary());
        command.args([
            "--backend=ptrace",
            "--log=trace",
            "--max-log-bytes=16777216",
            "run",
            "--strict",
            detcore_model::config::CANONICAL_TEST_EPOCH_ARG,
            "--base-env=minimal",
            "--",
        ]);
        command.arg(program).arg(mode);
        hermit_binary::configure_guest_execution(&mut command);
        command
    } else if no_write {
        // SIGTERM is explicitly default and unblocked in this fixture.
        // Native must stay in its restarted read until this exact 1s bound.
        let mut command = Command::new("timeout");
        command.args(["--signal=TERM", "--kill-after=1", "1"]);
        command.arg(program).arg(mode);
        command
    } else {
        let mut command = Command::new(program);
        command.arg(mode);
        command
    };
    command
        .env_remove("LD_LIBRARY_PATH")
        .stdin(Stdio::null())
        .stdout(fs::File::create(&output_path).expect("create stdout"))
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = ObservedGroup {
        child: command.spawn().expect("start observed guest run"),
        reaped: false,
    };
    let stderr = child.child.stderr.take().unwrap();
    let trace = Arc::new(Mutex::new(WaitTrace::default()));
    let reader_trace = trace.clone();
    let reader = thread::spawn(move || {
        let mut stderr = BufReader::new(stderr);
        let mut kept = Vec::new();
        let mut line = Vec::new();
        loop {
            line.clear();
            let count = stderr.read_until(b'\n', &mut line).expect("drain stderr");
            if count == 0 {
                return kept;
            }
            // Keep bounded diagnostics; continue parsing AND draining after
            // the retained prefix fills, so a late return cannot be lost.
            let keep = count.min(65_536_usize.saturating_sub(kept.len()));
            kept.extend_from_slice(&line[..keep]);
            reader_trace
                .lock()
                .unwrap()
                .observe(&String::from_utf8_lossy(&line), wait_name);
        }
    });
    let deadline = Instant::now()
        + if no_write && hermit {
            Duration::from_secs(10)
        } else {
            RUN_BOUND
        };
    let mut pid = None;
    let mut read_fd = None;
    let mut sent = false;
    let mut signal_error = None;
    let mut native_returned_to_read = false;
    loop {
        if child.exited() {
            break;
        }
        let stdout = fs::read_to_string(&output_path).expect("read stdout");
        if read_fd.is_none() {
            read_fd = stdout
                .lines()
                .find_map(|line| line.strip_prefix("WAIT fd="))
                .map(|value| value.parse::<i32>().expect("fixture fd"));
        }
        if let Some(fd) = read_fd {
            if pid.is_none() {
                let matches = guest_matches(program);
                assert!(matches.len() <= 1, "ambiguous observed guest: {matches:?}");
                pid = matches.first().copied();
            }
            if let Some(pid) = pid {
                if !sent && signal_error.is_none() {
                    let waiting = if hermit {
                        let trace = trace.lock().unwrap();
                        trace.empty_probe && !trace.deadlock
                    } else {
                        native_waiting(pid, wait_name, fd)
                    };
                    if waiting {
                        match signal_observed_guest(program, pid) {
                            Ok(()) => sent = true,
                            // Keep a premature product exit and its exact verdict
                            // inspectable. Every successful case requires sent=true;
                            // a failed send can never satisfy the regression.
                            Err(error) => signal_error = Some(error.to_string()),
                        }
                    }
                }
                if !hermit
                    && no_write
                    && sent
                    && !native_returned_to_read
                    && stdout.lines().any(|line| line == "HANDLED count=1")
                    && native_waiting(pid, "read", fd)
                {
                    // The handler calls write only. Its observed delivery marker
                    // followed by this sleeping read proves it returned; the
                    // SA_RESETHAND reset on handler entry alone proves no such thing.
                    native_returned_to_read = true;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "{mode}, hermit={hermit}: observed run timed out\n{stdout}\n{:?}",
            trace.lock().unwrap()
        );
        thread::sleep(Duration::from_millis(1));
    }
    // A descendant holding stderr must not turn leader exit into an
    // unbounded join. On failure ObservedGroup kills the still-owned group.
    while !reader.is_finished() {
        assert!(
            Instant::now() < deadline,
            "{mode}, hermit={hermit}: stderr did not close before the run deadline"
        );
        thread::sleep(Duration::from_millis(1));
    }
    let stderr = reader.join().expect("stderr reader");
    let status = child.reap();
    let stdout = fs::read(&output_path).expect("read stdout after exit");
    if read_fd.is_none() {
        read_fd = String::from_utf8_lossy(&stdout)
            .lines()
            .find_map(|line| line.strip_prefix("WAIT fd="))
            .map(|value| value.parse::<i32>().expect("fixture fd"));
    }
    let trace = Arc::try_unwrap(trace).unwrap().into_inner().unwrap();
    ObservedRun {
        output: Output {
            status,
            stdout,
            stderr,
        },
        trace,
        sent,
        signal_error,
        native_returned_to_read,
        read_fd: read_fd.expect("fixture announced its read descriptor"),
    }
}

fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"));
    fs::create_dir_all(root).expect("create target tmp directory");
    let directory = tempfile::Builder::new()
        .prefix("poll-self-pipe-")
        .tempdir_in(root)
        .expect("create fixture directory");
    let program = directory.path().join("self-pipe");
    let built = Command::new("timeout")
        .args([
            "--signal=KILL",
            "10",
            "cc",
            "-std=c11",
            "-O2",
            "-Wall",
            "-Wextra",
            "-Werror",
        ])
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/poll_self_pipe_signal.c"))
        .arg("-o")
        .arg(&program)
        .output()
        .expect("compile self-pipe fixture");
    assert!(built.status.success(), "compiler failed: {built:?}");
    (directory, program)
}

fn caught_signal_case(mode: &str, expected: &str) {
    let (directory, program) = fixture();
    let native = run(&program, mode, false, directory.path());
    assert!(native.status.success(), "native {mode}: {native:?}");
    assert_eq!(String::from_utf8_lossy(&native.stdout), expected);
    let hermit = run(&program, mode, true, directory.path());
    assert!(hermit.status.success(), "ptrace {mode}: {hermit:?}");
    assert_eq!(hermit.stdout, native.stdout, "ptrace {mode}: {hermit:?}");
}

#[test]
fn external_signal_self_pipe_read_restarts_and_finishes() {
    caught_signal_case(
        "caught",
        "READY\nRESULT first=1 errno=0 count=1 value=x handled=1\n",
    );
}

#[test]
fn external_signal_self_pipe_read_returns_eintr_then_the_byte() {
    caught_signal_case(
        "caught-no-restart",
        "READY\nRESULT first=-1 errno=4 count=1 value=x handled=1\n",
    );
}

#[test]
fn blocked_and_ignored_handlers_still_report_the_idle_reader_deadlock() {
    let (directory, program) = fixture();
    for mode in ["blocked", "ignored"] {
        let output = run(&program, mode, true, directory.path());
        assert_eq!(
            output.status.code(),
            Some(detcore_model::HERMIT_GUEST_DEADLOCK_EXIT),
            "{mode}: {output:?}"
        );
        assert_eq!(output.stdout, b"READY\n", "{mode}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("Deadlock detected: thread(s) retrying a pipe or socket-pair call no remaining thread can satisfy"),
            "{mode}: {output:?}"
        );
    }
}

fn caught_poll_case(mode: &str) {
    let (directory, program) = fixture();
    let native = run_observed(&program, mode, false, directory.path());
    assert!(
        native.sent && native.signal_error.is_none(),
        "signal must follow the native wait: {native:?}"
    );
    assert!(native.output.status.success(), "native {mode}: {native:?}");
    assert_eq!(
        String::from_utf8_lossy(&native.output.stdout),
        format!(
            "READY\nWAIT fd={}\nRESULT first=-1 errno=4 count=1 value=x handled=1\n",
            native.read_fd
        ),
        "native {mode}: {native:?}"
    );
    let hermit = run_observed(&program, mode, true, directory.path());
    assert!(hermit.output.status.success(), "ptrace {mode}: {hermit:?}");
    assert!(
        hermit.trace.empty_probe && hermit.sent && hermit.signal_error.is_none(),
        "signal must follow a real empty ptrace probe: {hermit:?}"
    );
    assert_eq!(
        hermit.output.stdout, native.output.stdout,
        "ptrace {mode}: {hermit:?}"
    );
}

#[test]
fn external_signal_self_pipe_poll_returns_eintr_then_the_byte() {
    // Linux poll(2) does not restart for SA_RESTART. The driver witnesses the
    // empty wait before sending; the handler then writes its only ready byte.
    caught_poll_case("caught-poll");
}

#[test]
fn external_signal_self_pipe_ppoll_returns_eintr_then_the_byte() {
    // The null-mask ppoll uses the ordinary, unmasked request producer.
    caught_poll_case("caught-ppoll");
}

#[test]
fn blocked_and_ignored_handlers_still_report_idle_poll_deadlocks() {
    let (directory, program) = fixture();
    for mode in [
        "blocked-poll",
        "ignored-poll",
        "blocked-ppoll",
        "ignored-ppoll",
    ] {
        let output = run(&program, mode, true, directory.path());
        assert_eq!(
            output.status.code(),
            Some(detcore_model::HERMIT_GUEST_DEADLOCK_EXIT),
            "{mode}: {output:?}"
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        let fd = stdout
            .lines()
            .find_map(|line| line.strip_prefix("WAIT fd="))
            .expect("fixture's monitored descriptor")
            .parse::<i32>()
            .expect("fixture fd");
        assert_eq!(
            stdout,
            format!("READY\nWAIT fd={fd}\n"),
            "{mode}: {output:?}"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(READER_DEADLOCK),
            "{mode}: {output:?}"
        );
    }
}

#[test]
fn one_shot_handler_byte_is_progress_and_its_return_without_a_byte_is_deadlock() {
    let (directory, program) = fixture();
    let mode = "caught-once-write";
    let native = run_observed(&program, mode, false, directory.path());
    assert!(native.sent && native.output.status.success(), "{native:?}");
    assert_eq!(
        String::from_utf8_lossy(&native.output.stdout),
        format!(
            "READY\nWAIT fd={}\nHANDLED count=1\nRESULT first=1 errno=0 count=1 value=x handled=1\n",
            native.read_fd
        )
    );
    let hermit = run_observed(&program, mode, true, directory.path());
    assert!(hermit.sent && hermit.output.status.success(), "{hermit:?}");
    assert_eq!(hermit.output.stdout, native.output.stdout, "{hermit:?}");

    // Same signal, mask, one-shot disposition, admission and handler marker;
    // the sole handler-side difference is omitting the monitored pipe write.
    let mode = "caught-once-no-write";
    let native = run_observed(&program, mode, false, directory.path());
    assert_eq!(native.output.status.code(), Some(124), "{native:?}");
    assert!(native.sent && native.native_returned_to_read, "{native:?}");
    assert_eq!(
        String::from_utf8_lossy(&native.output.stdout),
        format!("READY\nWAIT fd={}\nHANDLED count=1\n", native.read_fd)
    );
    let hermit = run_observed(&program, mode, true, directory.path());
    assert_eq!(
        hermit.output.status.code(),
        Some(detcore_model::HERMIT_GUEST_DEADLOCK_EXIT),
        "{hermit:?}"
    );
    assert_eq!(hermit.output.stdout, native.output.stdout, "{hermit:?}");
    assert!(hermit.sent && hermit.trace.empty_probe, "{hermit:?}");
    assert_eq!(hermit.trace.usr1_deliveries, 1, "{hermit:?}");
    assert!(
        hermit.trace.usr1_sigreturn
            && hermit.trace.read_after_sigreturn
            && hermit.trace.deadlock_after_return,
        "deadlock must follow this handler's return into the read: {hermit:?}"
    );
}
