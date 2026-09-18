// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
// Licensed under the BSD-style license in the LICENSE file.

//! Real Detcore RPC cancellation, including cancellation inside an RDTSC posthook.
//! The official runner owns aggregate CPU containment. Agent qualification uses
//! the same 22 CPU-second observer budget; this helper does not replace it with
//! a per-process rlimit. Its wall and output backstops also apply to direct tests.

use std::fs;
use std::io::Read;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Child;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use hermit::canonical_verdict::VerificationReport;
use regex::Regex;

const MIB: u64 = 1024 * 1024;
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

struct ProcessGroup {
    child: Child,
    reaped: bool,
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        if self.reaped {
            // Reaping releases ownership of the numeric PID/process-group ID.
            // Never send a signal to that number after it may be reused.
            return;
        }
        // Only this newly created child process group is targeted. The outer
        // official runner retains ownership of its complete descendant cgroup.
        unsafe { libc::kill(-(self.child.id() as i32), libc::SIGKILL) };
        let _ = self.child.wait();
    }
}

struct CapturedStream<R> {
    pipe: R,
    output: fs::File,
    name: &'static str,
    limit: u64,
    written: u64,
    eof: bool,
}

impl<R: Read + AsRawFd> CapturedStream<R> {
    fn new(pipe: R, directory: &Path, name: &'static str, limit: u64) -> Self {
        // SAFETY: this is the parent's owned pipe read end. Its nonblocking
        // flag does not change the child's separate write end.
        let flags = unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL) };
        assert!(
            flags >= 0,
            "read capture flags: {}",
            std::io::Error::last_os_error()
        );
        let changed =
            unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) };
        assert_eq!(
            changed,
            0,
            "set capture flags: {}",
            std::io::Error::last_os_error()
        );
        Self {
            pipe,
            output: fs::File::create(directory.join(name)).expect("capture file"),
            name,
            limit,
            written: 0,
            eof: false,
        }
    }

    fn drain(&mut self) {
        if self.eof {
            return;
        }
        let mut buffer = [0; 8192];
        // Bound each poll's work so a continuously writing child cannot keep
        // the other stream, log checks or wall deadline from being serviced.
        for _ in 0..8 {
            let count = match self.pipe.read(&mut buffer) {
                Ok(0) => {
                    self.eof = true;
                    break;
                }
                Ok(count) => count,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => panic!("read {}: {error}", self.name),
            };
            let keep = count.min((self.limit - self.written) as usize);
            self.output
                .write_all(&buffer[..keep])
                .expect("retain stream");
            self.written += keep as u64;
            // Retained files never exceed their bound. An observed extra byte
            // fails the test and the still-owned ProcessGroup kills/reaps it;
            // truncated output can never be accepted as a successful command.
            assert_eq!(keep, count, "{} exceeded its output bound", self.name);
        }
    }
}

fn bounded_read(path: &Path, limit: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .expect("read retained artifact");
    assert!(bytes.len() as u64 <= limit, "oversized {}", path.display());
    bytes
}

fn check_outputs(directory: &Path) {
    for (name, limit) in [("stdout", 64 * MIB), ("stderr", 16 * MIB)] {
        assert!(
            fs::metadata(directory.join(name))
                .expect("stream metadata")
                .len()
                <= limit,
            "{name} exceeded its bound; evidence retained at {}",
            directory.display()
        );
    }
    let logs = directory.join("verify-logs");
    if logs.exists() {
        for entry in fs::read_dir(logs).expect("verify log directory") {
            let entry = entry.expect("verify log entry");
            assert!(entry.file_type().expect("log type").is_file());
            assert!(entry.metadata().expect("log size").len() <= 64 * MIB);
        }
    }
}

fn bounded_command(command: &mut Command, directory: &Path) -> ExitStatus {
    fs::create_dir_all(directory).expect("create retained command directory");
    fs::write(directory.join("command.txt"), format!("{command:?}\n"))
        .expect("retain actual command");
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    // Limit the captured streams, not all file descriptors: RLIMIT_FSIZE also
    // rejects the KVM backend's required 1 GiB anonymous memory backing file.
    let start = Instant::now();
    let mut child = ProcessGroup {
        child: command.spawn().expect("start bounded command"),
        reaped: false,
    };
    let mut stdout = CapturedStream::new(
        child.child.stdout.take().expect("stdout pipe"),
        directory,
        "stdout",
        64 * MIB,
    );
    let mut stderr = CapturedStream::new(
        child.child.stderr.take().expect("stderr pipe"),
        directory,
        "stderr",
        16 * MIB,
    );
    let status = loop {
        stdout.drain();
        stderr.drain();
        check_outputs(directory);
        // Do not reap the direct child until both pipes reach EOF. If a
        // descendant retains a write end after the child exits, the unchanged
        // deadline fails while we still own the unreaped process-group ID.
        // There are no reader threads whose join could block after reaping.
        if stdout.eof
            && stderr.eof
            && let Some(status) = child.child.try_wait().expect("poll bounded command")
        {
            child.reaped = true;
            break status;
        }
        assert!(
            start.elapsed() < Duration::from_secs(57),
            "57-second wall limit exceeded; evidence at {}",
            directory.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    // Fast exit must still pass size and wall checks.
    check_outputs(directory);
    assert!(start.elapsed() < Duration::from_secs(57));
    fs::write(directory.join("status.txt"), format!("{status}\n")).expect("retain actual status");
    status
}

fn line_after(lines: &[&str], start: usize, needle: &str) -> usize {
    start
        + lines[start..]
            .iter()
            .position(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("missing {needle:?} after line {}", start + 1))
}

fn number(text: &str, marker: &str) -> u64 {
    let tail = text
        .split_once(marker)
        .unwrap_or_else(|| panic!("missing {marker:?}: {text}"))
        .1;
    let digits: String = tail
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '_')
        .filter(|c| *c != '_')
        .collect();
    digits.parse().expect("numeric trace field")
}

fn seconds_ns(text: &str) -> u64 {
    let value = text
        .split('s')
        .next()
        .expect("seconds field")
        .replace('_', "");
    let (seconds, nanos) = value.split_once('.').expect("nanosecond precision");
    assert_eq!(nanos.len(), 9);
    seconds.parse::<u64>().expect("seconds") * 1_000_000_000
        + nanos.parse::<u64>().expect("nanoseconds")
}

fn assert_mechanism(log: &str, loops: usize, timestamp_rip: u64) {
    let lines: Vec<_> = log.lines().collect();
    // These fixture tasks are identified by the real syscall/clock records,
    // rather than an assumed host PID or a successful final status.
    let leader_re =
        Regex::new(r"\[detcore, dtid (\d+)\] inbound syscall: exit_group\(17\)").unwrap();
    let leader = leader_re.captures(log).expect("leader exit request")[1].to_owned();
    let worker_re = Regex::new(r"\[dtid (\d+)\] inbound rdtsc,").unwrap();
    let worker = worker_re.captures(log).expect("actual worker timestamp")[1].to_owned();
    assert_ne!(leader, worker);
    let (winner, canceled) = if loops == 64 {
        (&worker, &leader)
    } else {
        (&leader, &worker)
    };
    let commit = lines
        .iter()
        .position(|line| {
            line.contains(&format!(
                "dettid {winner} using resources {{Exit {{ group: true"
            ))
        })
        .expect("real winning Exit commit");
    // The response Ivar's Display is a pre-await snapshot, not the wake event.
    // Bind the canceled operation to the scheduler's actual pending request.
    let pending = lines[..commit]
        .iter()
        .rposition(|line| line.contains(&format!(" ==> dtid {canceled}, req <ivar Ok(Resources")))
        .expect("canceled request must be pending before the winning COMMIT");
    let resources = lines[pending]
        .split_once("req <ivar Ok(")
        .unwrap()
        .1
        .split_once(")>, resp <ivar ")
        .expect("pending request and response Ivar")
        .0;
    let publication = lines[..pending]
        .iter()
        .rposition(|line| {
            line.contains(&format!(
                "[detcore, dtid {canceled}] ResourceRequest, filling request into "
            ))
        })
        .expect("canceled request publication");
    let killed = line_after(
        &lines,
        commit + 1,
        &format!("logically_kill: Scheduler removing all knowledge of [det]tid {canceled} "),
    );
    let removed = line_after(
        &lines,
        killed + 1,
        &format!("[detcore, dtid {canceled}] terminating pending request after logical removal"),
    );
    let waits: Vec<_> = lines[publication + 1..removed]
        .iter()
        .filter(|line| {
            line.contains(&format!("[detcore, dtid {canceled}] waiting on <ivar "))
                && line.ends_with(&format!(" for resources: {resources}"))
        })
        .collect();
    assert_eq!(
        waits.len(),
        1,
        "exact canceled request must await its response"
    );
    assert!(
        !lines[publication + 1..removed].iter().any(|line| {
            line.contains(&format!(
                "[detcore, dtid {canceled}] ResourceRequest, filling request into "
            )) || (line.contains(" COMMIT turn ")
                && line.contains(&format!("dettid {canceled} using resources")))
        }),
        "canceled request must not be replaced or granted"
    );
    let terminal = line_after(
        &lines,
        removed + 1,
        &format!("[detcore, dtid {canceled}] exiting after terminal scheduler cancellation"),
    );

    let mut last_deregistration = terminal;
    for tid in [&leader, &worker] {
        let hook = format!("[detcore, dtid {tid}] thread exit hook, deregistering from scheduler.");
        let hooks: Vec<_> = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.contains(&hook))
            .collect();
        assert_eq!(hooks.len(), 1, "each task consumes exactly one exit hook");
        if tid == canceled {
            assert!(hooks[0].0 > terminal);
        }
        let deregistered_message =
            format!("[detcore, dtid {tid}] thread deregistered, removed from sched structures.");
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.contains(&deregistered_message))
                .count(),
            1,
            "each task completes deregistration exactly once"
        );
        let deregistered = line_after(&lines, hooks[0].0 + 1, &deregistered_message);
        last_deregistration = last_deregistration.max(deregistered);
    }
    for message in [
        "Global state cleanup, continuing...",
        "detcore shut down, destroying global state",
    ] {
        assert_eq!(
            lines.iter().filter(|line| line.contains(message)).count(),
            1
        );
    }
    let cleanup = line_after(&lines, terminal + 1, "Global state cleanup, continuing...");
    assert!(last_deregistration < cleanup);
    let destroy = line_after(
        &lines,
        cleanup + 1,
        "detcore shut down, destroying global state",
    );
    line_after(&lines, destroy + 1, "reverie-kvm lifecycle phase timings");
    assert!(
        !lines[pending + 1..]
            .iter()
            .any(|line| line.contains(&format!(
                "[detcore, dtid {canceled}] UNBLOCKED, acquired resources:"
            )))
    );

    let callbacks = lines
        .iter()
        .filter(|line| line.contains(&format!("[dtid {worker}] inbound rdtsc,")))
        .count();
    if loops != 512 {
        assert_eq!(callbacks, loops, "all fixed-loop instructions executed");
        for tid in [&leader, &worker] {
            assert!(
                lines[..commit].iter().any(|line| line
                    .contains(&format!("==> dtid {tid}, req <ivar Ok(Resources"))
                    && line.contains("resources: {Exit { group: true")),
                "both Exit requests must be queued before the grant"
            );
        }
        assert!(lines[pending].contains("resources: {Exit { group: true"));
        assert!(lines[..commit].iter().any(|line| line.contains(&format!(
            "[detcore, dtid {worker}] inbound syscall: exit_group(95)"
        ))));
        return;
    }

    assert!(
        callbacks > 0 && callbacks < loops,
        "worker canceled before completing its loop"
    );
    assert!(!log.contains("inbound syscall: exit_group(95)"));
    assert!(lines[pending].contains("resources: {SleepUntil(LogicalTime(0)): W}"));
    let inbound = lines[..commit]
        .iter()
        .rposition(|line| line.contains(&format!("[dtid {worker}] inbound rdtsc,")))
        .unwrap();
    let before = lines[..inbound]
        .iter()
        .rposition(|line| {
            line.contains(&format!(
                "[dtid {worker}] updated rcb clock, new logical time:"
            ))
        })
        .unwrap();
    assert_eq!(
        number(lines[inbound], "nondet_instrs: "),
        number(lines[before], "nondet_instrs: ") + 1
    );
    let pre_time = seconds_ns(lines[before].split_once("i.e. ").unwrap().1);
    let deadline = seconds_ns(lines[before].split_once("timeslice end: ").unwrap().1);
    assert!(
        pre_time < deadline,
        "terminal yield must follow the charge, not precede it"
    );
    let pre_regs = line_after(
        &lines,
        before + 1,
        &format!("DETLOG (pre) registers [dtid {worker}]"),
    );
    assert!(pre_regs < inbound);
    assert!(lines[pre_regs].contains(&format!(" rip {timestamp_rip:#x} ")));
    let tick = line_after(
        &lines,
        inbound + 1,
        &format!("[tid {worker}] ticked its global time component to "),
    );
    let post_time = number(lines[inbound], "starting_micros: ") * 1000
        + number(lines[tick], "global time component to ");
    assert!(post_time > pre_time && post_time >= deadline);
    let reached = line_after(&lines, tick + 1, &format!("[dtid {worker}] logical time "));
    assert!(lines[reached].contains("reached timeslice target"));
    let block = line_after(
        &lines,
        reached + 1,
        &format!("[detcore, dtid {worker}] BLOCKING on resource_request rpc..."),
    );
    assert!(lines[block].contains("resources: {SleepUntil(LogicalTime(0)): W}"));
    assert!(block < commit);
    assert!(!lines[inbound + 1..].iter().any(|line| {
        line.contains(&format!("DETLOG (post) registers [dtid {worker}]"))
            || line.contains(&format!("[dtid {worker}] inbound rdtsc,"))
    }));
}

pub(super) fn run(loops: usize, expected: i32) {
    let _lock = super::hermit_run_guard();
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .expect("this selected KVM regression requires usable /dev/kvm");
    let repository = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    fs::create_dir_all(env!("CARGO_TARGET_TMPDIR")).expect("fixture parent directory");
    let root = tempfile::Builder::new()
        .prefix(&format!("kvm-cancel-{loops}-"))
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("retained fixture directory")
        .keep();
    eprintln!("KVM cancellation artifacts retained at {}", root.display());
    let guest = root.join("program");
    let fixture = repository.join(format!("tests/c/kvm_cancellation_{loops}.S"));
    fs::copy(&fixture, root.join("guest.S")).expect("retain exact assembly");
    let status = bounded_command(
        Command::new("cc")
            .args([
                "-nostdinc",
                "-nostdlib",
                "-nostartfiles",
                "-static",
                "-no-pie",
                "-fno-pie",
                "-Wl,--build-id=none",
                "-Wall",
                "-Wextra",
                "-Werror",
                "-o",
            ])
            .arg(&guest)
            .arg(&fixture),
        &root.join("compile"),
    );
    assert!(
        status.success(),
        "guest compiler failed; see {}",
        root.display()
    );
    let elf_bytes = bounded_read(&guest, 16 * MIB);
    let elf = goblin::elf::Elf::parse(&elf_bytes).expect("actual fixture ELF");
    assert_eq!(elf.header.e_type, goblin::elf::header::ET_EXEC);
    assert_eq!(elf.header.e_machine, goblin::elf::header::EM_X86_64);
    assert!(
        elf.interpreter.is_none(),
        "fixture must not execute a dynamic loader"
    );
    let timestamp = elf
        .syms
        .iter()
        .find(|sym| elf.strtab.get_at(sym.st_name) == Some("worker_timestamp"))
        .expect("actual timestamp symbol")
        .st_value;
    let next = elf
        .syms
        .iter()
        .find(|sym| elf.strtab.get_at(sym.st_name) == Some("worker_timestamp_next"))
        .expect("actual next instruction symbol")
        .st_value;
    assert_eq!(next, timestamp + 2);
    let segment = elf
        .program_headers
        .iter()
        .find(|segment| {
            segment.p_type == goblin::elf::program_header::PT_LOAD
                && timestamp >= segment.p_vaddr
                && next <= segment.p_vaddr + segment.p_filesz
        })
        .expect("file-backed timestamp instruction");
    let offset = usize::try_from(segment.p_offset + timestamp - segment.p_vaddr).unwrap();
    assert_eq!(&elf_bytes[offset..offset + 2], &[0x0f, 0x31]);
    let run_dir = root.join("run");
    let logs = run_dir.join("verify-logs");
    fs::create_dir_all(&logs).expect("retained verification logs");
    let report_path = run_dir.join("verification.json");
    let home = root.join("guest-home");
    let config = root.join("guest-config");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&config).unwrap();
    let args = [
        "--log=trace",
        "run",
        "--base-env=minimal",
        "--backend=kvm",
        "--strict",
        "--target-timeslice=6000",
        "--verify-strict",
        "--verify",
        "--verify-allow=failure",
        "--verify-json",
        report_path.to_str().unwrap(),
        "--keep-logs",
        "--verify-log-dir",
        logs.to_str().unwrap(),
        "--mount=type=tmpfs,target=/test",
        "--workdir=/test",
        "--env=LC_ALL=C",
        "--env=TZ=UTC",
        "--env",
        &format!("HOME={}", home.display()),
        "--env",
        &format!("XDG_CONFIG_HOME={}", config.display()),
        "--",
        guest.to_str().unwrap(),
    ];
    let mut command = super::hermit_command(&args);
    command.env("HERMIT_LOG_MAX_BYTES", (64 * MIB).to_string());
    let status = bounded_command(&mut command, &run_dir);
    assert_eq!(
        status.code(),
        Some(expected),
        "actual guest status, not Hermit zero-status success"
    );
    let report = VerificationReport::from_json_slice(&bounded_read(&report_path, 16 * MIB))
        .expect("complete typed verification result");
    report
        .require_canonical_match()
        .expect("full nonempty canonical INFO match");
    report
        .require_exact_output_match()
        .expect("exact status/stdout/stderr repeat match");
    assert_eq!(report.guest_exit_code, Some(expected));
    assert!(report.guest_signal.is_none());
    let policy = report.comparison.as_ref().unwrap();
    assert_eq!(policy.display_name.as_deref(), Some("BitwiseInfoV1"));
    assert_eq!(policy.compare_io_buffers, Some(true));
    assert_eq!(policy.virtualize_time, Some(true));
    assert_eq!(policy.strip_lines, Some(false));
    assert_eq!(policy.canonicalize_addresses, Some(true));
    assert_eq!(policy.full_trace, Some(true));
    assert_eq!(policy.exact_remainder, Some(true));
    assert_eq!(policy.ignore_lines, Some(false));
    assert_eq!(policy.skip_commit, Some(false));
    assert_eq!(policy.skip_detlog, Some(false));
    assert_eq!(
        policy.log_scope,
        Some(hermit::canonical_verdict::ComparedLogScope::Info)
    );
    assert_eq!(
        policy.stripped_prefixes.as_deref(),
        Some(["real-wall-clock-prefix/v1".to_owned()].as_slice())
    );
    assert_eq!(
        policy.canonicalizations.as_deref(),
        Some(["host-address-to-first-appearance-ordinal/v1".to_owned()].as_slice())
    );
    for operand in [
        &report.compared_outputs.as_ref().unwrap().left,
        &report.compared_outputs.as_ref().unwrap().right,
    ] {
        assert_eq!((operand.stdout_bytes, operand.stderr_bytes), (0, 0));
        assert_eq!(operand.stdout_sha256, EMPTY_SHA256);
        assert_eq!(operand.stderr_sha256, EMPTY_SHA256);
    }
    for prefix in ["run1_log_", "run2_log_"] {
        let matches: Vec<_> = fs::read_dir(&logs)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(prefix)
            })
            .collect();
        assert_eq!(
            matches.len(),
            1,
            "exactly one retained log for each real run"
        );
        let log =
            String::from_utf8(bounded_read(&matches[0], 16 * MIB)).expect("complete UTF-8 trace");
        assert_mechanism(&log, loops, timestamp);
    }
}
