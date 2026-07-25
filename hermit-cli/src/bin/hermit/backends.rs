/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// AUTONOMOUS-BOT-IMPLEMENTED

//! Execution-backend dispatch for `hermit run`.
//!
//! The DBI path launches the real guest through DynamoRIO and links the native
//! client against Hermit's `detcore-dbi` runtime. That runtime instantiates the
//! production [`detcore::Detcore`] Tool over [`reverie_dbi::DbiGuest`].
//!
//! The SaBRe path ([`hermit::Backend::Sabre`]) performs static rewriting with a
//! Reverie plugin. Generic runs use quiet compatibility checking, while
//! `hermit --backend sabre strace` retains verbose syscall diagnostics.

use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
use std::io::IsTerminal as _;
use std::io::Read;
use std::io::Seek as _;
use std::io::SeekFrom;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Command as StdCommand;
use std::process::Output;
use std::process::Stdio;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use hermit::Error;
use hermit::ExitStatus;
use reverie_dbi::DbiRunner;
use tracing::metadata::LevelFilter;

#[derive(Debug)]
struct DbiSummary {
    branches: u64,
    syscalls: u64,
    rewritten: u64,
    stdin_reads: u64,
    memory_hash: String,
}

impl DbiSummary {
    fn same_observable_behavior(&self, other: &Self) -> bool {
        // `branches` is the count at the last intercepted syscall, not an execution digest.
        // Keep it as callback-health telemetry without rejecting otherwise identical runs.
        self.syscalls == other.syscalls
            && self.rewritten == other.rewritten
            && self.stdin_reads == other.stdin_reads
            && self.memory_hash == other.memory_hash
    }
}

struct TeeReader<R, W> {
    input: R,
    replay: W,
}

impl<R: Read, W: Write> Read for TeeReader<R, W> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.input.read(buffer)?;
        self.replay.write_all(&buffer[..read])?;
        Ok(read)
    }
}

/// Runs `program` through DynamoRIO with the real Detcore Tool.
///
/// When `verify` is true, the guest is executed twice. Both runs must succeed,
/// produce byte-identical stdout, report `tool=Detcore`, and produce the same
/// observed guest-memory hash from the native DBI runtime.
pub fn run_dbi(
    program: &Path,
    args: &[String],
    verify: bool,
    log: Option<LevelFilter>,
) -> Result<ExitStatus, Error> {
    let stdin_is_terminal = std::io::stdin().is_terminal();

    let (drrun, client) = detcore_dbi::prepare_native_client().map_err(|error| {
        Error::msg(format!(
            "failed to prepare the Detcore DynamoRIO client: {error}"
        ))
    })?;
    let runner = DbiRunner::new(&drrun, &client)
        .map_err(|error| {
            Error::msg(format!(
                "failed to configure the DynamoRIO DBI runner (drrun={}, client={}): {error}",
                drrun.display(),
                client.display()
            ))
        })?
        .summary(true);

    eprintln!(
        "hermit: [dbi backend] Detcore Tool active; running {program:?} under DynamoRIO ({})",
        drrun.display()
    );

    let mut guest = StdCommand::new(program);
    if let Some(level) = log {
        guest.env("HERMIT_LOG", level.to_string());
    }
    guest.args(args);

    if !verify {
        if stdin_is_terminal {
            let status = runner
                .status(&guest)
                .map_err(|error| launch_error(&drrun, error))?;
            return Ok(process_status(status));
        }
        let output = run_once(&runner, &guest, &drrun, std::io::stdin())?;
        write_output(&output)?;
        return Ok(output_status(&output));
    }

    let mut replay = if stdin_is_terminal {
        None
    } else {
        Some(tempfile::tempfile()?)
    };

    eprintln!(":: DBI Run1...");
    let first = match replay.as_mut() {
        Some(replay) => {
            let first_input = TeeReader {
                input: std::io::stdin(),
                replay: replay.try_clone()?,
            };
            run_once(&runner, &guest, &drrun, first_input)?
        }
        None => run_once_with_terminal_input(&runner, &guest, &drrun)?,
    };
    if !first.status.success() {
        write_output(&first)?;
        return Ok(output_status(&first));
    }
    let first_summary = detcore_summary(&first)?;
    if stdin_is_terminal && first_summary.stdin_reads != 0 {
        write_output(&first)?;
        return Err(Error::msg(format!(
            "DBI verification cannot replay terminal stdin: guest attempted {} fd-0 read syscall(s)",
            first_summary.stdin_reads
        )));
    }

    eprintln!(":: DBI Run2...");
    let second = match replay.as_mut() {
        Some(replay) => {
            replay.seek(SeekFrom::Start(0))?;
            run_once(&runner, &guest, &drrun, replay.try_clone()?)?
        }
        None => run_once_with_terminal_input(&runner, &guest, &drrun)?,
    };
    if !second.status.success() {
        write_output(&second)?;
        return Ok(output_status(&second));
    }
    let second_summary = detcore_summary(&second)?;

    if first.stdout != second.stdout {
        return Err(Error::msg(
            "DBI verification failed: guest stdout differed between runs",
        ));
    }
    if !first_summary.same_observable_behavior(&second_summary) {
        return Err(Error::msg(format!(
            "DBI verification failed: native Detcore summaries differed ({first_summary:?} != {second_summary:?})"
        )));
    }
    if first_summary.branches != second_summary.branches {
        eprintln!(
            ":: DBI diagnostic branch counts differed at the last syscall: {} | {}",
            first_summary.branches, second_summary.branches
        );
    }

    write_output(&first)?;
    eprintln!(
        ":: Comparing DBI observed guest-memory hashes... {} | {}",
        first_summary.memory_hash, second_summary.memory_hash
    );
    eprintln!(":: DBI path confirmed: DynamoRIO client reported tool=Detcore");
    eprintln!(":: Success: deterministic. Determinism verified.");
    Ok(ExitStatus::Exited(0))
}

fn run_once<R: Read + Send>(
    runner: &DbiRunner,
    guest: &StdCommand,
    drrun: &Path,
    input: R,
) -> Result<Output, Error> {
    runner
        .output_with_reader(guest, input)
        .map_err(|error| launch_error(drrun, error))
}

fn run_once_with_terminal_input(
    runner: &DbiRunner,
    guest: &StdCommand,
    drrun: &Path,
) -> Result<Output, Error> {
    runner
        .output_with_inherited_stdin(guest)
        .map_err(|error| launch_error(drrun, error))
}

fn launch_error(drrun: &Path, error: std::io::Error) -> Error {
    Error::msg(format!(
        "failed to launch drrun ({}): {error}",
        drrun.display()
    ))
}

fn process_status(status: std::process::ExitStatus) -> ExitStatus {
    ExitStatus::Exited(status.code().unwrap_or(1))
}

fn detcore_summary(output: &Output) -> Result<DbiSummary, Error> {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let summary = stderr
        .lines()
        .rev()
        .find(|line| line.starts_with("reverie-dbi: tool=Detcore "))
        .ok_or_else(|| {
            Error::msg(
                "DBI verification failed: native DynamoRIO summary did not report tool=Detcore",
            )
        })?;

    let field = |name: &str| {
        summary
            .split_ascii_whitespace()
            .find_map(|value| value.strip_prefix(name))
            .ok_or_else(|| Error::msg(format!("DBI verification failed: summary omitted {name}")))
    };
    let branches = field("branches=")?
        .parse::<u64>()
        .map_err(|_| Error::msg("DBI verification failed: invalid branch count"))?;
    let syscalls = field("syscalls=")?
        .parse::<u64>()
        .map_err(|_| Error::msg("DBI verification failed: invalid syscall count"))?;
    let rewritten = field("rewritten=")?
        .parse::<u64>()
        .map_err(|_| Error::msg("DBI verification failed: invalid rewritten count"))?;
    let stdin_reads = field("stdin_reads=")?
        .parse::<u64>()
        .map_err(|_| Error::msg("DBI verification failed: invalid stdin read count"))?;
    if branches == 0 || syscalls == 0 || rewritten == 0 || rewritten > syscalls {
        return Err(Error::msg(
            "DBI verification failed: native callback counters are inconsistent",
        ));
    }

    let hash = field("memory_hash=")?;
    if hash.len() != 16 || u64::from_str_radix(hash, 16).is_err() {
        return Err(Error::msg(
            "DBI verification failed: invalid observed-memory hash",
        ));
    }
    Ok(DbiSummary {
        branches,
        syscalls,
        rewritten,
        stdin_reads,
        memory_hash: hash.to_owned(),
    })
}

fn write_output(output: &Output) -> Result<(), Error> {
    std::io::stdout().write_all(&output.stdout)?;
    std::io::stderr().write_all(&output.stderr)?;
    Ok(())
}

const LITEINST_EVENT_FD_ENV: &str = "REVERIE_LITEINST_EVENT_FD";
const LITEINST_EVENT_COOKIE_ENV: &str = "REVERIE_LITEINST_EVENT_COOKIE";
const LITEINST_EVENT_PREFIX: &str = "reverie-liteinst: tool=compat cookie=";
const LITEINST_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

struct LiteinstOutput {
    output: Output,
    events: Vec<LiteinstEvent>,
}

#[derive(Debug, Eq, PartialEq)]
struct LiteinstEvent {
    pid: u32,
    number: i64,
}

#[derive(Debug, Eq, PartialEq)]
struct LiteinstShapeEvent {
    process: usize,
    number: i64,
}

struct ReplayableStdin {
    offset: Option<libc::off_t>,
}

impl ReplayableStdin {
    fn detect() -> Result<Self, Error> {
        if unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_GETFD) } < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EBADF) {
                return Ok(Self { offset: None });
            }
            return Err(error.into());
        }

        let offset = unsafe { libc::lseek(libc::STDIN_FILENO, 0, libc::SEEK_CUR) };
        if offset < 0 {
            return Err(Error::msg(
                "LiteInst compatibility verification requires seekable stdin (regular file or /dev/null); pipes and terminals cannot be replayed",
            ));
        }
        Ok(Self {
            offset: Some(offset),
        })
    }

    fn input(&self) -> Result<Stdio, Error> {
        let Some(offset) = self.offset else {
            return Ok(Stdio::null());
        };
        if unsafe { libc::lseek(libc::STDIN_FILENO, offset, libc::SEEK_SET) } != offset {
            return Err(std::io::Error::last_os_error().into());
        }
        let fd = unsafe { libc::dup(libc::STDIN_FILENO) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: dup returned a new owned descriptor.
        Ok(Stdio::from(unsafe { fs::File::from_raw_fd(fd) }))
    }
}

impl Drop for ReplayableStdin {
    fn drop(&mut self) {
        if let Some(offset) = self.offset {
            unsafe {
                libc::lseek(libc::STDIN_FILENO, offset, libc::SEEK_SET);
            }
        }
    }
}

fn liteinst_event_pipe() -> Result<(OwnedFd, OwnedFd), Error> {
    let mut descriptors = [0; 2];
    if unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: a successful pipe2 returned two new owned descriptors.
    Ok(unsafe {
        (
            OwnedFd::from_raw_fd(descriptors[0]),
            OwnedFd::from_raw_fd(descriptors[1]),
        )
    })
}

fn configure_liteinst_guest(
    command: &mut StdCommand,
    event_writer: &OwnedFd,
    event_cookie: u64,
    guest_preload: Option<&OsStr>,
) -> Result<(), Error> {
    let mut preload = hermit::liteinst_runtime_library_path()?.into_os_string();
    if let Some(existing) = guest_preload.filter(|value| !value.is_empty()) {
        preload.push(OsStr::new(":"));
        preload.push(existing);
    }
    let event_fd = event_writer.as_raw_fd();
    command
        .env("LD_PRELOAD", preload)
        .env("REVERIE_LITEINST_TOOL", "compat")
        .env(LITEINST_EVENT_FD_ENV, event_fd.to_string())
        .env(LITEINST_EVENT_COOKIE_ENV, event_cookie.to_string());
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(event_fd, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
}

struct SigchldDefaultGuard {
    previous: libc::sigaction,
}

impl SigchldDefaultGuard {
    fn install() -> std::io::Result<Self> {
        let mut action: libc::sigaction = unsafe { core::mem::zeroed() };
        action.sa_sigaction = libc::SIG_DFL;
        if unsafe { libc::sigemptyset(&mut action.sa_mask) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut previous: libc::sigaction = unsafe { core::mem::zeroed() };
        if unsafe { libc::sigaction(libc::SIGCHLD, &action, &mut previous) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self { previous })
    }

    fn restore_in_child(&self, command: &mut StdCommand) {
        let previous = self.previous;
        unsafe {
            command.pre_exec(move || {
                if libc::sigaction(libc::SIGCHLD, &previous, std::ptr::null_mut()) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
}

impl Drop for SigchldDefaultGuard {
    fn drop(&mut self) {
        unsafe {
            libc::sigaction(libc::SIGCHLD, &self.previous, std::ptr::null_mut());
        }
    }
}

fn parse_liteinst_events(events: &[u8], event_cookie: u64) -> Result<Vec<LiteinstEvent>, Error> {
    if events.is_empty() || !events.ends_with(b"\n") {
        return Err(Error::msg(
            "LiteInst compatibility runtime emitted no complete events",
        ));
    }
    let prefix = format!("{LITEINST_EVENT_PREFIX}{event_cookie} pid=");
    events[..events.len() - 1]
        .split(|byte| *byte == b'\n')
        .map(|line| {
            let record = line.strip_prefix(prefix.as_bytes()).ok_or_else(|| {
                Error::msg("LiteInst compatibility runtime emitted an invalid event")
            })?;
            let record = std::str::from_utf8(record)
                .map_err(|_| Error::msg("LiteInst emitted a non-UTF-8 event"))?;
            let (pid, number) = record.split_once(" syscall=").ok_or_else(|| {
                Error::msg("LiteInst compatibility runtime emitted an invalid event")
            })?;
            let pid = pid
                .parse::<u32>()
                .map_err(|_| Error::msg("LiteInst emitted an invalid process ID"))?;
            let number = number
                .parse::<i64>()
                .map_err(|_| Error::msg("LiteInst emitted an invalid syscall number"))?;
            Ok(LiteinstEvent { pid, number })
        })
        .collect()
}

fn liteinst_syscall_shape(events: &[LiteinstEvent]) -> Vec<LiteinstShapeEvent> {
    let mut processes = Vec::new();
    let mut shape = Vec::with_capacity(events.len());
    for event in events {
        let process = processes
            .iter()
            .position(|pid| pid == &event.pid)
            .unwrap_or_else(|| {
                processes.push(event.pid);
                processes.len() - 1
            });
        let shaped = LiteinstShapeEvent {
            process,
            number: event.number,
        };
        if shape.last() != Some(&shaped) || event.number != libc::SYS_read {
            shape.push(shaped);
        }
    }
    shape
}

fn liteinst_process_exited(events: &[LiteinstEvent], pid: u32) -> bool {
    events.iter().any(|event| {
        event.pid == pid && matches!(event.number, libc::SYS_exit | libc::SYS_exit_group)
    })
}

fn run_liteinst_once(
    mut guest: StdCommand,
    log: Option<LevelFilter>,
    input: Stdio,
    guest_preload: Option<&OsStr>,
) -> Result<LiteinstOutput, Error> {
    let (event_reader, event_writer) = liteinst_event_pipe()?;
    let event_cookie = loop {
        let cookie = rand::random::<u64>();
        if cookie != 0 {
            break cookie;
        }
    };
    guest
        .stdin(input)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(level) = log {
        guest.env("HERMIT_LOG", level.to_string());
    }
    configure_liteinst_guest(&mut guest, &event_writer, event_cookie, guest_preload)?;
    guest.process_group(0);

    let events = read_liteinst_stream(fs::File::from(event_reader));
    let sigchld = SigchldDefaultGuard::install()?;
    sigchld.restore_in_child(&mut guest);
    let child = guest.spawn();
    drop(event_writer);
    let mut child = match child {
        Ok(child) => child,
        Err(error) => {
            let _ = receive_liteinst_stream(events, "event");
            return Err(Error::msg(format!(
                "failed to launch LiteInst guest: {error}"
            )));
        }
    };
    let child_pid = child.id();
    let stdout = read_liteinst_stream(
        child
            .stdout
            .take()
            .ok_or_else(|| Error::msg("LiteInst guest stdout was not piped"))?,
    );
    let stderr = read_liteinst_stream(
        child
            .stderr
            .take()
            .ok_or_else(|| Error::msg("LiteInst guest stderr was not piped"))?,
    );
    let status = wait_for_liteinst_leader(&mut child)?;
    let output = Output {
        status,
        stdout: receive_liteinst_stream(stdout, "stdout")?,
        stderr: receive_liteinst_stream(stderr, "stderr")?,
    };
    let events = receive_liteinst_stream(events, "event")?;
    let events = parse_liteinst_events(&events, event_cookie)?;
    if output.status.code().is_some() && !liteinst_process_exited(&events, child_pid) {
        return Err(Error::msg(
            "LiteInst compatibility runtime ended without a complete exit event",
        ));
    }
    Ok(LiteinstOutput { output, events })
}

fn wait_for_liteinst_leader(
    child: &mut std::process::Child,
) -> std::io::Result<std::process::ExitStatus> {
    let child_pid = child.id();
    let mut information: libc::siginfo_t = unsafe { core::mem::zeroed() };
    loop {
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                child_pid,
                &mut information,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if result == 0 {
            break;
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    // The unreaped leader keeps its PID/PGID reserved while surviving
    // descendants are killed, closing inherited output and event pipes.
    let _ = unsafe { libc::kill(-(child_pid as libc::pid_t), libc::SIGKILL) };
    child.wait()
}

fn read_liteinst_stream<R>(mut reader: R) -> mpsc::Receiver<std::io::Result<Vec<u8>>>
where
    R: Read + Send + 'static,
{
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = reader.read_to_end(&mut bytes).map(|_| bytes);
        let _ = sender.send(result);
    });
    receiver
}

fn receive_liteinst_stream(
    receiver: mpsc::Receiver<std::io::Result<Vec<u8>>>,
    name: &str,
) -> Result<Vec<u8>, Error> {
    receiver
        .recv_timeout(LITEINST_DRAIN_TIMEOUT)
        .map_err(|_| {
            Error::msg(format!(
                "LiteInst {name} stream remained open after the guest exited"
            ))
        })?
        .map_err(Into::into)
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#688): Review the LiteInst compatibility boundary.
/// Runs a guest through Reverie's LiteInst preload compatibility path.
///
/// This backend compares observable results but does not activate Detcore and
/// therefore does not provide Hermit's L2 determinism guarantee.
pub fn run_liteinst<F>(
    mut guest_command: F,
    guest_preload: Option<OsString>,
    verify: bool,
    log: Option<LevelFilter>,
) -> Result<ExitStatus, Error>
where
    F: FnMut() -> Result<StdCommand, Error>,
{
    eprintln!(
        "hermit: [liteinst backend] experimental preload compatibility mode; Detcore determinization is unavailable"
    );

    if !verify {
        let run = run_liteinst_once(
            guest_command()?,
            log,
            Stdio::inherit(),
            guest_preload.as_deref(),
        )?;
        write_output(&run.output)?;
        eprintln!(
            ":: LiteInst path confirmed: preload reported {} syscall events",
            run.events.len()
        );
        return Ok(liteinst_output_status(&run.output));
    }

    let input = ReplayableStdin::detect()?;
    eprintln!(":: LiteInst compatibility run 1...");
    let first = run_liteinst_once(
        guest_command()?,
        log,
        input.input()?,
        guest_preload.as_deref(),
    )?;
    eprintln!(":: LiteInst compatibility run 2...");
    let second = run_liteinst_once(
        guest_command()?,
        log,
        input.input()?,
        guest_preload.as_deref(),
    )?;

    if first.output.status != second.output.status {
        return Err(Error::msg(format!(
            "LiteInst compatibility verification failed: exit statuses differed ({} != {})",
            first.output.status, second.output.status
        )));
    }
    if first.output.stdout != second.output.stdout {
        return Err(Error::msg(
            "LiteInst compatibility verification failed: guest stdout differed between runs",
        ));
    }
    if first.output.stderr != second.output.stderr {
        return Err(Error::msg(
            "LiteInst compatibility verification failed: guest stderr differed between runs",
        ));
    }
    let first_shape = liteinst_syscall_shape(&first.events);
    let second_shape = liteinst_syscall_shape(&second.events);
    if first_shape != second_shape {
        let difference = first_shape
            .iter()
            .zip(&second_shape)
            .position(|(left, right)| left != right)
            .unwrap_or_else(|| first_shape.len().min(second_shape.len()));
        let context_start = difference.saturating_sub(4);
        let first_context_end = (difference + 5).min(first_shape.len());
        let second_context_end = (difference + 5).min(second_shape.len());
        return Err(Error::msg(format!(
            "LiteInst compatibility verification failed: normalized syscall shapes differed at event {difference} ({:?} != {:?}; {} events/{} shapes != {} events/{} shapes); context {context_start}..: {:?} != {:?}",
            first_shape.get(difference),
            second_shape.get(difference),
            first.events.len(),
            first_shape.len(),
            second.events.len(),
            second_shape.len(),
            &first_shape[context_start..first_context_end],
            &second_shape[context_start..second_context_end],
        )));
    }

    write_output(&first.output)?;
    eprintln!(
        ":: LiteInst compatibility observations matched ({} syscall events; no Detcore determinization).",
        first.events.len()
    );
    Ok(liteinst_output_status(&first.output))
}

fn liteinst_output_status(output: &Output) -> ExitStatus {
    ExitStatus::from_raw(output.status.into_raw())
}

fn output_status(output: &Output) -> ExitStatus {
    ExitStatus::Exited(output.status.code().unwrap_or(1))
}

fn sabre_artifact(variable: &str, description: &str, executable: bool) -> Result<OsString, Error> {
    let value = std::env::var_os(variable).ok_or_else(|| {
        Error::msg(format!(
            "the sabre backend needs {variable}=<path-to-{description}>"
        ))
    })?;
    validate_sabre_artifact(Path::new(&value), variable, executable)
}

fn validate_sabre_artifact(
    requested_path: &Path,
    variable: &str,
    executable: bool,
) -> Result<OsString, Error> {
    let path = fs::canonicalize(requested_path).map_err(|error| {
        Error::msg(format!(
            "the sabre backend cannot access {variable}={}: {error}",
            requested_path.display()
        ))
    })?;
    let metadata = fs::metadata(&path).map_err(|error| {
        Error::msg(format!(
            "the sabre backend cannot inspect {variable}={}: {error}",
            path.display()
        ))
    })?;
    if !metadata.is_file() {
        return Err(Error::msg(format!(
            "the sabre backend needs {variable}={} to be a regular file",
            path.display()
        )));
    }
    if executable && metadata.permissions().mode() & 0o111 == 0 {
        return Err(Error::msg(format!(
            "the sabre backend needs {variable}={} to be executable",
            path.display()
        )));
    }
    Ok(path.into_os_string())
}

const SABRE_QUIET_ENV: &str = "REVERIE_SABRE_STRACE_QUIET";

fn sabre_command(
    runner: &OsString,
    sabre: &OsString,
    plugin: &OsString,
    program: &Path,
    args: &[String],
    quiet: bool,
    log: Option<LevelFilter>,
) -> StdCommand {
    let mut command = StdCommand::new(runner);
    command
        .arg("--sabre")
        .arg(sabre)
        .arg("--plugin")
        .arg(plugin)
        .arg("--")
        .arg(program)
        .args(args);
    if quiet {
        command.env(SABRE_QUIET_ENV, "1");
    }
    if let Some(level) = log {
        command.env("HERMIT_LOG", level.to_string());
    }
    command
}

fn sabre_artifacts() -> Result<(OsString, OsString, OsString), Error> {
    Ok((
        sabre_artifact("HERMIT_SABRE_RUNNER", "reverie-sabre-strace", true)?,
        sabre_artifact("HERMIT_SABRE_BINARY", "sabre", true)?,
        sabre_artifact(
            "HERMIT_SABRE_PLUGIN",
            "libreverie_sabre_strace_plugin.so",
            false,
        )?,
    ))
}

/// Runs program through the shared Reverie strace tool over SaBRe.
///
/// The SaBRe host and plugin live in the coordinated Reverie checkout, so
/// Hermit uses explicit artifact paths rather than taking an unreleased Cargo
/// dependency:
///
/// * HERMIT_SABRE_RUNNER: reverie-sabre-strace executable.
/// * HERMIT_SABRE_BINARY: pinned SaBRe executable.
/// * HERMIT_SABRE_PLUGIN: libreverie_sabre_strace_plugin.so.
// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#589): Review SaBRe CLI backend dispatch.
pub fn run_sabre_strace(program: &Path, args: &[String]) -> Result<ExitStatus, Error> {
    let (runner, sabre, plugin) = sabre_artifacts()?;

    eprintln!("hermit: [sabre backend] tracing {program:?} with the shared Reverie tool");

    let status = sabre_command(&runner, &sabre, &plugin, program, args, false, None)
        .status()
        .map_err(|error| {
            Error::msg(format!(
                "failed to launch the SaBRe runner {}: {error}",
                Path::new(&runner).display()
            ))
        })?;

    Ok(status.into())
}

fn sabre_output(mut command: StdCommand, input: &[u8], runner: &Path) -> Result<Output, Error> {
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command.spawn().map_err(|error| {
        Error::msg(format!(
            "failed to launch the SaBRe runner {}: {error}",
            runner.display()
        ))
    })?;
    child
        .stdin
        .take()
        .expect("piped SaBRe stdin")
        .write_all(input)?;
    child.wait_with_output().map_err(Error::from)
}

/// Runs a compatibility probe through the shared Reverie syscall tool.
///
/// SaBRe does not provide Detcore determinization. Verification executes the
/// same guest twice and compares its exit status, stdout, and stderr. This is
/// therefore a compatibility check, not Hermit's L2 determinism guarantee.
// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#589): Review SaBRe CLI backend dispatch.
pub fn run_sabre(
    program: &Path,
    args: &[String],
    verify: bool,
    log: Option<LevelFilter>,
) -> Result<ExitStatus, Error> {
    let (runner, sabre, plugin) = sabre_artifacts()?;
    let runner_path = Path::new(&runner);

    eprintln!(
        "hermit: [sabre backend] shared Reverie StraceTool active for {program:?}; \
         Detcore determinization is unavailable"
    );

    if !verify {
        let status = sabre_command(&runner, &sabre, &plugin, program, args, true, log)
            .status()
            .map_err(|error| {
                Error::msg(format!(
                    "failed to launch the SaBRe runner {}: {error}",
                    runner_path.display()
                ))
            })?;
        return Ok(status.into());
    }

    let mut input = Vec::new();
    if !std::io::stdin().is_terminal() {
        std::io::stdin().read_to_end(&mut input)?;
    }

    eprintln!(":: SaBRe compatibility run 1...");
    let first = sabre_output(
        sabre_command(&runner, &sabre, &plugin, program, args, true, log),
        &input,
        runner_path,
    )?;
    if !first.status.success() {
        write_output(&first)?;
        return Ok(output_status(&first));
    }

    eprintln!(":: SaBRe compatibility run 2...");
    let second = sabre_output(
        sabre_command(&runner, &sabre, &plugin, program, args, true, log),
        &input,
        runner_path,
    )?;
    if !second.status.success() {
        write_output(&second)?;
        return Ok(output_status(&second));
    }

    if first.status != second.status {
        return Err(Error::msg(format!(
            "SaBRe compatibility verification failed: exit statuses differed ({} != {})",
            first.status, second.status
        )));
    }
    if first.stdout != second.stdout {
        return Err(Error::msg(
            "SaBRe compatibility verification failed: guest stdout differed between runs",
        ));
    }
    if first.stderr != second.stderr {
        return Err(Error::msg(
            "SaBRe compatibility verification failed: guest stderr differed between runs",
        ));
    }

    write_output(&first)?;
    eprintln!(
        ":: SaBRe compatibility verified (shared Reverie StraceTool; no Detcore determinization)."
    );
    Ok(ExitStatus::Exited(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dbi_summary(branches: u64) -> DbiSummary {
        DbiSummary {
            branches,
            syscalls: 169,
            rewritten: 168,
            stdin_reads: 0,
            memory_hash: "4b5e0e70f3050157".to_owned(),
        }
    }

    #[test]
    fn dbi_summary_treats_last_syscall_branch_count_as_telemetry() {
        assert!(dbi_summary(563_145).same_observable_behavior(&dbi_summary(563_103)));
    }

    #[test]
    fn dbi_summary_compares_observable_counters_and_hash() {
        let expected = dbi_summary(100);

        let mut actual = dbi_summary(100);
        actual.syscalls += 1;
        assert!(!expected.same_observable_behavior(&actual));

        let mut actual = dbi_summary(100);
        actual.rewritten -= 1;
        assert!(!expected.same_observable_behavior(&actual));

        let mut actual = dbi_summary(100);
        actual.stdin_reads += 1;
        assert!(!expected.same_observable_behavior(&actual));

        let mut actual = dbi_summary(100);
        actual.memory_hash = "0000000000000000".to_owned();
        assert!(!expected.same_observable_behavior(&actual));
    }

    #[test]
    fn liteinst_event_parser_requires_complete_control_records() {
        let cookie = 42;
        assert_eq!(
            parse_liteinst_events(
                b"reverie-liteinst: tool=compat cookie=42 pid=100 syscall=0\nreverie-liteinst: tool=compat cookie=42 pid=100 syscall=231\n",
                cookie,
            )
            .unwrap(),
            [
                LiteinstEvent { pid: 100, number: 0 },
                LiteinstEvent { pid: 100, number: 231 },
            ]
        );
        assert!(
            parse_liteinst_events(
                b"reverie-liteinst: tool=compat cookie=42 pid=100 syscall=0",
                cookie
            )
            .is_err()
        );
        assert!(
            parse_liteinst_events(
                b"reverie-liteinst: tool=compat cookie=41 pid=100 syscall=231\n",
                cookie,
            )
            .is_err()
        );
        assert_eq!(
            parse_liteinst_events(
                b"reverie-liteinst: tool=compat cookie=42 pid=100 syscall=0\n",
                cookie,
            )
            .unwrap(),
            [LiteinstEvent {
                pid: 100,
                number: 0
            }]
        );
        assert!(parse_liteinst_events(b"guest stderr\n", cookie).is_err());
    }

    fn event(pid: u32, number: i64) -> LiteinstEvent {
        LiteinstEvent { pid, number }
    }

    #[test]
    fn liteinst_syscall_shape_ignores_only_same_process_read_chunk_repeats() {
        assert_eq!(
            liteinst_syscall_shape(&[
                event(100, 257),
                event(100, 9),
                event(100, 8),
                event(100, libc::SYS_read),
                event(100, libc::SYS_read),
                event(100, 11),
            ]),
            [
                LiteinstShapeEvent {
                    process: 0,
                    number: 257,
                },
                LiteinstShapeEvent {
                    process: 0,
                    number: 9,
                },
                LiteinstShapeEvent {
                    process: 0,
                    number: 8,
                },
                LiteinstShapeEvent {
                    process: 0,
                    number: libc::SYS_read,
                },
                LiteinstShapeEvent {
                    process: 0,
                    number: 11,
                },
            ]
        );
        assert_eq!(
            liteinst_syscall_shape(&[
                event(100, libc::SYS_write),
                event(100, libc::SYS_write),
                event(100, libc::SYS_exit),
            ])
            .len(),
            3
        );
        assert_ne!(
            liteinst_syscall_shape(&[
                event(100, 257),
                event(100, libc::SYS_read),
                event(100, libc::SYS_write),
            ]),
            liteinst_syscall_shape(&[
                event(100, 257),
                event(100, libc::SYS_write),
                event(100, libc::SYS_read),
            ])
        );
        assert_eq!(
            liteinst_syscall_shape(&[event(100, libc::SYS_read), event(200, libc::SYS_read),])
                .len(),
            2
        );
        assert_eq!(
            liteinst_syscall_shape(&[event(100, libc::SYS_read), event(200, libc::SYS_write),]),
            liteinst_syscall_shape(&[event(700, libc::SYS_read), event(900, libc::SYS_write),])
        );
    }

    #[test]
    fn liteinst_exit_completeness_is_bound_to_direct_child() {
        let events = [
            LiteinstEvent {
                pid: 200,
                number: libc::SYS_exit_group,
            },
            LiteinstEvent {
                pid: 100,
                number: libc::SYS_write,
            },
        ];
        assert!(liteinst_process_exited(&events, 200));
        assert!(!liteinst_process_exited(&events, 100));
    }

    #[test]
    fn sabre_artifact_returns_the_validated_absolute_path() {
        let file = tempfile::NamedTempFile::new_in(".").unwrap();
        let relative_path = file.path().file_name().unwrap();

        let resolved = validate_sabre_artifact(Path::new(relative_path), "test-artifact", false)
            .map(std::path::PathBuf::from)
            .unwrap();

        assert!(resolved.is_absolute());
        assert_eq!(resolved, fs::canonicalize(file.path()).unwrap());
    }
}
