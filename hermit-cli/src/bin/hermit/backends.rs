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
//! The DBT path launches the real guest through DynamoRIO and links the native
//! client against Hermit's `detcore-dbt` runtime. That runtime instantiates the
//! production [`detcore::Detcore`] Tool over [`reverie_dbt::DbtGuest`].
//!
//! Generic SaBRe runs are coordinated by `libhermit` with the real Detcore
//! plugin. This module retains the separate
//! `hermit --backend sabre strace` diagnostic path.

use std::collections::BTreeMap;
#[cfg(feature = "dbt")]
use std::collections::BTreeSet;
#[cfg(feature = "dbt")]
use std::env;
#[cfg(feature = "dbt")]
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
#[cfg(feature = "dbt")]
use std::io::IsTerminal as _;
#[cfg(feature = "dbt")]
use std::io::Read;
#[cfg(feature = "dbt")]
use std::io::Seek as _;
#[cfg(feature = "dbt")]
use std::io::SeekFrom;
#[cfg(feature = "dbt")]
use std::io::Write;
#[cfg(feature = "dbt")]
use std::os::fd::AsRawFd;
#[cfg(feature = "dbt")]
use std::os::fd::FromRawFd;
use std::os::unix::fs::PermissionsExt;
#[cfg(any(feature = "dbt", test))]
use std::os::unix::process::ExitStatusExt as _;
use std::path::Path;
#[cfg(feature = "dbt")]
use std::path::PathBuf;
use std::process::Command as StdCommand;
#[cfg(any(feature = "dbt", test))]
use std::process::Output;

use detcore::Config;
#[cfg(feature = "dbt")]
use detcore_model::backend_engagement::BackendEngagement;
#[cfg(feature = "dbt")]
use detcore_model::backend_engagement::BackendEngagementReport;
use hermit::Error;
use hermit::ExitStatus;
#[cfg(feature = "dbt")]
use reverie_dbt::DbtEvidenceLogLevel;
#[cfg(feature = "dbt")]
use reverie_dbt::DbtRunner;
#[cfg(feature = "dbt")]
use reverie_dbt::backend_stats::DbtBackendStatsAggregator;
#[cfg(feature = "dbt")]
use reverie_dbt::backend_stats::DbtBackendStatsSnapshot;
use tracing::metadata::LevelFilter;

#[cfg(feature = "dbt")]
use super::record_envelope::RecordEnvelope;
use super::run::VerifyAllow;
#[cfg(feature = "dbt")]
use super::verify::ComparedRun;
#[cfg(feature = "dbt")]
use super::verify::ComparisonOptions;
#[cfg(feature = "dbt")]
use super::verify::DbtCountedBranchComparison;
#[cfg(feature = "dbt")]
use super::verify::LogCompareStrictness;
#[cfg(feature = "dbt")]
use super::verify::Verdict;
#[cfg(feature = "dbt")]
use super::verify::VerificationOutcome;
#[cfg(feature = "dbt")]
use super::verify::announce_verification_outcome;
#[cfg(feature = "dbt")]
use super::verify::compare_two_runs;
#[cfg(feature = "dbt")]
use super::verify::retain_verification_logs;
#[cfg(feature = "dbt")]
use super::verify::temp_log_files_in;
#[cfg(feature = "dbt")]
use super::verify::verification_log_level;
#[cfg(feature = "dbt")]
use super::verify::write_pending_verification_json;
#[cfg(feature = "dbt")]
use super::verify::write_verification_json;

#[cfg(feature = "dbt")]
fn same_dbt_observable_behavior(
    first: &DbtBackendStatsSnapshot,
    second: &DbtBackendStatsSnapshot,
) -> bool {
    // `counted_branches` is deliberately absent here because the branch clock
    // is checked separately, with its own typed evidence. It is NOT excluded
    // because a difference is tolerable.
    first.intercepted_syscalls() == second.intercepted_syscalls()
        && first.rewritten_syscalls() == second.rewritten_syscalls()
        && first.stdin_reads() == second.stdin_reads()
        && first.memory_hash_fold() == second.memory_hash_fold()
}

/// Describe a counted-branch-clock divergence without conflating it with the
/// other native summary fields.
#[cfg(feature = "dbt")]
fn dbt_branch_clock_mismatch(first: u64, second: u64) -> Option<String> {
    (first != second).then(|| {
        format!(
            "DBT verification failed: counted-branch clocks differed between runs ({first} != {second}); \
             the clock is a deterministic function of the executed instruction stream"
        )
    })
}

/// Compare the authenticated number of DBT process images separately from the
/// log records whose arrival order is deterministic.
#[cfg(feature = "dbt")]
fn dbt_initialization_record_mismatch(first: usize, second: usize) -> Option<String> {
    (first != second).then(|| {
        format!(
            "DBT verification failed: authenticated process-image initialization counts differed between runs ({first} != {second})"
        )
    })
}

#[cfg(feature = "dbt")]
#[derive(Debug)]
struct DbtSummaryComparison {
    failure: Option<String>,
}

#[cfg(feature = "dbt")]
impl DbtSummaryComparison {
    fn compare(
        first_stats: &DbtBackendStatsSnapshot,
        second_stats: &DbtBackendStatsSnapshot,
        first_initialization_records: usize,
        second_initialization_records: usize,
    ) -> Self {
        let mut failures = Vec::new();
        if !same_dbt_observable_behavior(first_stats, second_stats) {
            failures.push(format!(
                "DBT verification failed: typed native Detcore statistics differed \
                 ({first_stats:?} != {second_stats:?})"
            ));
        }
        if let Some(failure) = dbt_initialization_record_mismatch(
            first_initialization_records,
            second_initialization_records,
        ) {
            failures.push(failure);
        }
        Self {
            failure: (!failures.is_empty()).then(|| failures.join("; ")),
        }
    }

    fn requires_log_retention(&self) -> bool {
        self.failure.is_some()
    }

    fn apply(self, outcome: &mut VerificationOutcome) -> Option<String> {
        if self.failure.is_some() {
            outcome.verdict = Verdict::Diverged;
            outcome.no_result_reason = None;
        }
        self.failure
    }
}

/// Add a backend-observed divergence to the typed verification verdict.
///
/// The canonical comparator may have matched its stdout, stderr, status, and
/// INFO records, or it may have refused a truncated log. Neither can erase a
/// separately observed difference in the deterministic counted-branch clock.
/// Mutating the typed outcome before it is serialized ensures `--verify-json`
/// says `diverged` with `verified=false` and `bitwise_parity=false`, rather than
/// leaving a false match or the invocation's pending `no_result` record.
#[cfg(feature = "dbt")]
fn record_dbt_branch_clock_comparison(
    outcome: &mut VerificationOutcome,
    comparison: DbtCountedBranchComparison,
) -> Option<String> {
    match dbt_branch_clock_mismatch(comparison.left, comparison.right) {
        Some(message) => {
            outcome.dbt_counted_branches = Some(comparison);
            outcome.verdict = Verdict::Diverged;
            outcome.no_result_reason = None;
            Some(message)
        }
        None if outcome.verdict == Verdict::NoResult => {
            // Equal clocks do not turn a refused common comparison into a
            // verdict. Keep the backend field absent so `no_result` does not
            // claim that this one successful dimension authorized anything.
            outcome.dbt_counted_branches = None;
            None
        }
        None => {
            outcome.dbt_counted_branches = Some(comparison);
            None
        }
    }
}

/// Attach the DBT-specific comparison and publish the terminal typed verdict.
///
/// Returning the human-readable failure only after the JSON write preserves the
/// report-first ordering: once a terminal branch-clock failure is announced,
/// the artifact already names the same divergence and both compared values.
#[cfg(feature = "dbt")]
fn finalize_dbt_verification(
    mut outcome: VerificationOutcome,
    comparison: DbtCountedBranchComparison,
    verify_json: Option<&Path>,
) -> Result<(VerificationOutcome, Option<String>), Error> {
    let failure = record_dbt_branch_clock_comparison(&mut outcome, comparison);
    if let Some(path) = verify_json {
        write_verification_json(path, &outcome)?;
    }
    Ok((outcome, failure))
}

/// Own the typed statistics stream for one complete DBT process tree.
///
/// Reverie's client writes one fixed-size record per process image at exit.
/// The protected-evidence runner does not expose its convenience stats method,
/// so this adapter uses the same public typed wire decoder and waits until the
/// runner has reaped the whole isolated process group before reading it.
#[cfg(feature = "dbt")]
struct DbtStatsCapture {
    _directory: tempfile::TempDir,
    path: PathBuf,
}

#[cfg(feature = "dbt")]
impl DbtStatsCapture {
    fn new() -> Result<Self, Error> {
        let directory = tempfile::Builder::new()
            .prefix("hermit-dbt-verify-stats-")
            .tempdir()
            .map_err(|error| {
                Error::msg(format!(
                    "failed to create DBT whole-process statistics sink: {error}"
                ))
            })?;
        let path = directory.path().join("records.bin");
        Ok(Self {
            _directory: directory,
            path,
        })
    }

    fn configure(&self, runner: DbtRunner) -> DbtRunner {
        runner
            .client_argument("-stats_path")
            .client_argument(self.path.clone().into_os_string())
    }

    fn finish(self) -> Result<DbtBackendStatsSnapshot, Error> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) if !bytes.is_empty() => bytes,
            Ok(_) => {
                return Err(Error::msg(
                    "DBT verification did not reach a verdict: typed whole-process statistics were empty",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::msg(
                    "DBT verification did not reach a verdict: typed whole-process statistics were missing",
                ));
            }
            Err(error) => {
                return Err(Error::msg(format!(
                    "DBT verification did not reach a verdict: failed to read typed whole-process statistics: {error}"
                )));
            }
        };
        let mut aggregator = DbtBackendStatsAggregator::new();
        let records = aggregator.absorb_wire_stream(&bytes).map_err(|error| {
            Error::msg(format!(
                "DBT verification did not reach a verdict: typed whole-process statistics were unreadable: {error}"
            ))
        })?;
        if records == 0 {
            return Err(Error::msg(
                "DBT verification did not reach a verdict: typed whole-process statistics contained no process records",
            ));
        }
        let snapshot = aggregator.snapshot();
        if snapshot.counted_branches() == 0
            || snapshot.intercepted_syscalls() == 0
            || snapshot.rewritten_syscalls() > snapshot.intercepted_syscalls()
        {
            return Err(Error::msg(
                "DBT typed native callback counters are inconsistent",
            ));
        }
        Ok(snapshot)
    }
}

/// Render the native DBT counters as a labeled `--summary` block.
///
/// These are the counters the DynamoRIO client already emits at exit; hermit
/// simply surfaces them on the normal run path. Labels are deliberately honest
/// about what each number is: `branches` is Detcore's deterministic
/// counted-branch clock (cbr/ubr/call/return retired), **not** a count of
/// translated basic blocks, and `memory hash` is the client's observed
/// guest-memory digest, not a Detcore RunSummary field.
#[cfg(feature = "dbt")]
fn format_dbt_stats(summary: &DbtBackendStatsSnapshot) -> String {
    format!(
        "=== DBT backend stats (native DynamoRIO client) ===\n\
         counted branches (deterministic branch clock): {}\n\
         syscalls intercepted:                          {}\n\
         syscall instructions rewritten:                {}\n\
         stdin (fd 0) reads:                            {}\n\
         observed guest-memory hash:                    {}\n",
        summary.counted_branches(),
        summary.intercepted_syscalls(),
        summary.rewritten_syscalls(),
        summary.stdin_reads(),
        format_args!("{:016x}", summary.memory_hash_fold()),
    )
}

#[cfg(feature = "dbt")]
fn write_dbt_engagement(path: &Path, snapshot: &DbtBackendStatsSnapshot) -> Result<(), Error> {
    let report = BackendEngagementReport::new(BackendEngagement::Dbt {
        counted_branches: snapshot.counted_branches(),
    });
    report.validate().map_err(Error::msg)?;
    let mut bytes = serde_json::to_vec(&report)?;
    bytes.push(b'\n');
    fs::write(path, bytes).map_err(|error| {
        Error::msg(format!(
            "writing DBT engagement {}: {error}",
            path.display()
        ))
    })
}

#[cfg(feature = "dbt")]
fn finish_single_run_dbt_stats(
    capture: DbtStatsCapture,
    summary: bool,
    engagement_json: Option<&Path>,
) -> Result<(), Error> {
    match capture.finish() {
        Ok(stats) => {
            if summary {
                eprint!("{}", format_dbt_stats(&stats));
            }
            if let Some(path) = engagement_json {
                write_dbt_engagement(path, &stats)?;
            }
            Ok(())
        }
        Err(error) if engagement_json.is_some() => Err(error),
        Err(error) => {
            eprintln!(":: DBT summary unavailable: {error}");
            Ok(())
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
#[cfg(feature = "dbt")]
struct DbtGuestCommand {
    program: PathBuf,
    args: Vec<OsString>,
}

#[cfg(feature = "dbt")]
fn executable_on_path(program: &OsStr, path: &OsStr, cwd: &Path) -> Option<PathBuf> {
    executable_path_candidate(program, path, cwd).map(|candidate| cwd.join(candidate))
}

#[cfg(feature = "dbt")]
fn executable_path_candidate(program: &OsStr, path: &OsStr, cwd: &Path) -> Option<PathBuf> {
    env::split_paths(path)
        .map(|directory| directory.join(program))
        .find(|candidate| {
            cwd.join(candidate).metadata().is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
}

/// Resolve the simple `#!/usr/bin/env PROGRAM` form before DynamoRIO starts.
///
/// DynamoRIO follows an absolute exec target correctly, but its copied exec path
/// can wait indefinitely when `env` later resolves a bare target through PATH.
/// Keep `env` in the process chain and replace only its single plain program
/// token with the equivalent absolute PATH match. More complex `env` forms are
/// left unchanged for the normal launcher rather than partially interpreting
/// options or assignments here.
#[cfg(feature = "dbt")]
fn prepare_dbt_guest_command(
    program: &Path,
    args: &[OsString],
    path: Option<&OsStr>,
    cwd: &Path,
) -> DbtGuestCommand {
    let unchanged = || DbtGuestCommand {
        program: program.to_path_buf(),
        args: args.to_vec(),
    };
    // Command resolves relative paths and PATH entries after changing cwd.
    // Inspect that same file in the physical run's already-established mount
    // namespace; the caller's /test may have been hidden by its fresh tmpfs.
    let script_path =
        if program.is_absolute() || program.as_os_str().as_encoded_bytes().contains(&b'/') {
            program.to_path_buf()
        } else if let Some(found) =
            path.and_then(|path| executable_path_candidate(program.as_os_str(), path, cwd))
        {
            found
        } else {
            return unchanged();
        };
    let Some(shebang) = hermit::Shebang::new(cwd.join(&script_path)) else {
        return unchanged();
    };
    let (interpreter, interpreter_args) = shebang.into_parts();
    if interpreter.file_name() != Some(OsStr::new("env")) || interpreter_args.len() != 1 {
        return unchanged();
    }

    let target = &interpreter_args[0];
    let target_bytes = std::os::unix::ffi::OsStrExt::as_bytes(target.as_os_str());
    if target_bytes.starts_with(b"-")
        || target_bytes.contains(&b'=')
        || target_bytes.contains(&b'/')
    {
        return unchanged();
    }
    let Some(target) = path.and_then(|path| executable_on_path(target, path, cwd)) else {
        return unchanged();
    };

    let mut resolved_args = Vec::with_capacity(args.len() + 2);
    resolved_args.push(target.into_os_string());
    // execvp supplies the selected PATH candidate to a script interpreter.
    // Explicit paths retain their spelling; a bare name must not discard the
    // directory that actually selected its script.
    resolved_args.push(script_path.into_os_string());
    resolved_args.extend_from_slice(args);
    DbtGuestCommand {
        program: interpreter,
        args: resolved_args,
    }
}

// run_dbt supplies program/args, an exact environment represented by values
// and explicit removals, and cwd.
// Reconstruct those fields inside the physical run, before DbtRunner performs
// its own command reconstruction. Stdin and evidence descriptors remain owned
// by the existing runner adapters.
#[cfg(feature = "dbt")]
fn prepare_dbt_physical_command(guest: &StdCommand) -> Result<StdCommand, Error> {
    let environment = guest
        .get_envs()
        .filter_map(|(key, value)| value.map(|value| (key.to_owned(), value.to_owned())))
        .collect::<BTreeMap<_, _>>();
    let args = guest.get_args().map(OsStr::to_owned).collect::<Vec<_>>();
    let cwd = env::current_dir()?;
    let cwd = guest
        .get_current_dir()
        .map_or(cwd.clone(), |workdir| cwd.join(workdir));
    let prepared = prepare_dbt_guest_command(
        Path::new(guest.get_program()),
        &args,
        environment.get(OsStr::new("PATH")).map(OsString::as_os_str),
        &cwd,
    );
    let mut command = StdCommand::new(prepared.program);
    command.args(prepared.args);
    // Do not call env_clear here: std::Command then erases removal entries,
    // so DbtRunner cannot see them when it reconstructs from get_envs().
    // DbtRunner copies explicit removals as well as values. Preserve the raw
    // command's removal entries instead of re-enumerating ambient variables.
    for (name, value) in guest.get_envs() {
        if let Some(value) = value {
            command.env(name, value);
        } else {
            command.env_remove(name);
        }
    }
    if let Some(workdir) = guest.get_current_dir() {
        command.current_dir(workdir);
    }
    Ok(command)
}

#[cfg(feature = "dbt")]
fn apply_exact_environment(command: &mut StdCommand, environment: &BTreeMap<OsString, OsString>) {
    // DbtRunner reconstructs its launcher command from Command::get_envs(),
    // which cannot expose env_clear(). Make removals explicit so --base-env
    // does not accidentally inherit the Hermit launcher's environment.
    for (key, _) in env::vars_os() {
        if !environment.contains_key(&key) {
            command.env_remove(key);
        }
    }
    command.envs(environment);
}

#[cfg(feature = "dbt")]
fn apply_dbt_workdir(command: &mut StdCommand, workdir: Option<&Path>) {
    if let Some(workdir) = workdir {
        command.current_dir(workdir);
    }
}
// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-644): Review inherited DBT policy descriptors and bounded reports.
#[cfg(feature = "dbt")]
struct InstalledFd {
    target: i32,
    backup: Option<i32>,
    original_flags: Option<i32>,
}

#[cfg(feature = "dbt")]
impl InstalledFd {
    fn install(source: i32, target: i32) -> std::io::Result<Self> {
        // Keep the backup above the reserved transport descriptor so installing the target
        // cannot overwrite its backup.
        let backup = unsafe {
            libc::fcntl(
                target,
                libc::F_DUPFD_CLOEXEC,
                detcore_dbt::UNSUPPORTED_SYSCALL_REPORT_FD + 1,
            )
        };
        let backup = if backup == -1 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EBADF) {
                None
            } else {
                return Err(error);
            }
        } else {
            Some(backup)
        };
        let original_flags = if let Some(backup_fd) = backup {
            let flags = unsafe { libc::fcntl(target, libc::F_GETFD) };
            if flags == -1 {
                let error = std::io::Error::last_os_error();
                let _ = unsafe { libc::close(backup_fd) };
                return Err(error);
            }
            Some(flags)
        } else {
            None
        };
        let installed = Self {
            target,
            backup,
            original_flags,
        };
        if unsafe { libc::dup2(source, target) } == -1 {
            return Err(std::io::Error::last_os_error());
        }
        if unsafe { libc::fcntl(target, libc::F_SETFD, 0) } == -1 {
            let error = std::io::Error::last_os_error();
            drop(installed);
            return Err(error);
        }
        Ok(installed)
    }
}

#[cfg(feature = "dbt")]
impl Drop for InstalledFd {
    fn drop(&mut self) {
        if let Some(backup) = self.backup {
            let _ = unsafe { libc::dup2(backup, self.target) };
            if let Some(flags) = self.original_flags {
                let _ = unsafe { libc::fcntl(self.target, libc::F_SETFD, flags) };
            }
            let _ = unsafe { libc::close(backup) };
        } else {
            let _ = unsafe { libc::close(self.target) };
        }
    }
}

#[cfg(feature = "dbt")]
struct DbtUnsupportedSyscallReport {
    reader: std::fs::File,
    _writer: std::fs::File,
    _report_fd: InstalledFd,
}

#[cfg(feature = "dbt")]
impl DbtUnsupportedSyscallReport {
    fn new() -> std::io::Result<Self> {
        let mut descriptors = [-1; 2];
        let result =
            unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
        if result == -1 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: pipe2 initialized both descriptors, transferring their ownership here.
        let reader = unsafe { std::fs::File::from_raw_fd(descriptors[0]) };
        let writer = unsafe { std::fs::File::from_raw_fd(descriptors[1]) };
        let report_fd = InstalledFd::install(
            writer.as_raw_fd(),
            detcore_dbt::UNSUPPORTED_SYSCALL_REPORT_FD,
        )?;
        Ok(Self {
            reader,
            _writer: writer,
            _report_fd: report_fd,
        })
    }

    fn emit(&mut self) -> std::io::Result<()> {
        const MAX_REPORT_BYTES: usize = 1024 * 1024;
        let mut contents = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            match self.reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    if contents.len() + read > MAX_REPORT_BYTES {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "DBT unsupported-syscall report exceeded 1 MiB",
                        ));
                    }
                    contents.extend_from_slice(&buffer[..read]);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }
        let contents = String::from_utf8_lossy(&contents);
        let syscalls = contents
            .lines()
            .filter_map(|line| {
                if let Some(raw) = line.strip_prefix("@") {
                    let sysno = raw
                        .parse::<i32>()
                        .ok()
                        .map(reverie::syscalls::Sysno::from)?;
                    detcore::is_unsupported_syscall(sysno).then(|| sysno.to_string())
                } else if !line.is_empty()
                    && line.len() <= 64
                    && line
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                {
                    Some(line.to_owned())
                } else {
                    None
                }
            })
            .take(512)
            .collect::<BTreeSet<_>>();
        if let Some(message) = detcore::format_unsupported_syscall_warning(&syscalls) {
            eprintln!("WARNING: {message}");
        }
        Ok(())
    }
}

#[cfg(feature = "dbt")]
impl Drop for DbtUnsupportedSyscallReport {
    fn drop(&mut self) {
        if let Err(error) = self.emit() {
            eprintln!("WARNING: failed to read DBT unsupported-syscall report: {error}");
        }
    }
}

#[cfg(feature = "dbt")]
struct TeeReader<R, W> {
    input: R,
    replay: W,
}

#[cfg(feature = "dbt")]
impl<R: Read, W: Write> Read for TeeReader<R, W> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.input.read(buffer)?;
        self.replay.write_all(&buffer[..read])?;
        Ok(read)
    }
}

#[cfg(feature = "dbt")]
fn dbt_evidence_log_level(
    requested: Option<LevelFilter>,
    diagnostic_full_trace: bool,
) -> DbtEvidenceLogLevel {
    let level = verification_log_level(
        requested,
        LogCompareStrictness::Canonical,
        diagnostic_full_trace,
    );
    if level >= LevelFilter::TRACE {
        DbtEvidenceLogLevel::Trace
    } else if level >= LevelFilter::DEBUG {
        DbtEvidenceLogLevel::Debug
    } else {
        DbtEvidenceLogLevel::Info
    }
}

#[cfg(feature = "dbt")]
#[derive(Debug)]
struct DecodedDbtEvidence {
    records: Vec<Vec<u8>>,
    initialization_records: usize,
}

#[cfg(feature = "dbt")]
fn decode_dbt_evidence(file: &mut std::fs::File) -> Result<DecodedDbtEvidence, Error> {
    file.seek(SeekFrom::Start(0))?;
    let mut encoded = Vec::new();
    file.read_to_end(&mut encoded)?;
    if encoded.is_empty() {
        return Err(Error::msg("DBT canonical evidence was empty"));
    }
    let evidence = reverie_dbt::decode_evidence(&encoded).map_err(|error| {
        Error::msg(format!(
            "DBT canonical evidence was malformed or truncated: {error}"
        ))
    })?;
    let initialization_records = evidence.initialization_records();
    Ok(DecodedDbtEvidence {
        records: evidence.into_records(),
        initialization_records,
    })
}

#[cfg(feature = "dbt")]
fn materialize_dbt_comparison_log(
    records: &[Vec<u8>],
    mut log: std::fs::File,
    path: &Path,
) -> Result<usize, Error> {
    if records.is_empty() {
        return Err(Error::msg("DBT canonical evidence contained no records"));
    }
    log.set_len(0)?;
    log.seek(SeekFrom::Start(0))?;
    for record in records {
        let Some(payload) = record.strip_suffix(b"\n") else {
            return Err(Error::msg(
                "DBT canonical evidence record was missing its terminal newline",
            ));
        };
        if payload.contains(&b'\n') || payload.contains(&b'\r') {
            return Err(Error::msg(
                "DBT canonical evidence record contained an embedded line boundary",
            ));
        }
        log.write_all(record)?;
    }
    log.flush()?;

    // The verdict publishes `dbt_evidence_transport_v1`: Reverie's authenticated
    // decoder has already represented exact initialization records by their
    // count, and the comparison envelope excludes any other transport-target
    // record. Every comparable decoded record must still reach this log;
    // filtering here would be an undisclosed second selection. Each record was
    // checked above to hold no embedded line boundary, so one record is one line
    // and this count is exact.
    let materialized = std::fs::read(path)
        .map_err(|error| Error::msg(format!("DBT canonical evidence log unreadable: {error}")))?
        .iter()
        .filter(|byte| **byte == b'\n')
        .count();
    if materialized != records.len() {
        return Err(Error::msg(format!(
            "DBT canonical evidence log holds {materialized} records but {} were decoded; the \
             comparison publishes the dbt_evidence_transport_v1 envelope and must not drop any",
            records.len()
        )));
    }

    let compared =
        detcore::logdiff::write_canonical_info(path, &mut std::io::sink()).map_err(|error| {
            Error::msg(format!(
                "DBT canonical evidence did not contain a valid log stream: {error}"
            ))
        })?;
    if compared == 0 {
        return Err(Error::msg(
            "DBT canonical evidence contained no INFO records",
        ));
    }
    Ok(compared)
}

#[cfg(feature = "dbt")]
fn dbt_verification_output(output: Output) -> reverie::process::Output {
    reverie::process::Output {
        status: process_status(output.status),
        stdout: output.stdout,
        stderr: output.stderr,
    }
}

/// The DBT `--log-file` refusal message, named so a test can assert the exact text
/// rather than a paraphrase of it.
#[cfg(any(feature = "dbt", test))]
pub(super) const DBT_LOG_FILE_REFUSAL: &str =
    "DBT --log-file is unavailable on the ordinary single-run adapter";

/// Is `--log-file` refused for this DBT invocation?
///
/// ⚠️ EXTRACTED SO THE POLICY IS DEFENDED. Before this, `git grep 'log-file is
/// unavailable'` returned exactly ONE hit tree-wide -- the error string itself --
/// with no test asserting when it fires. An undefended refusal can be reversed by
/// accident, and there is a live proposal to reverse this one: hermit#1689's first
/// claim implements DBT `--log-file`, and main adopted the opposite policy in
/// `f0584c1aac` about four minutes after that branch forked, inside a commit about
/// verification verdicts rather than as the subject of a decision.
///
/// This does NOT take a side on which policy is right -- that is an owner ruling.
/// It pins what main does TODAY, so reversing it means deleting a test, which is
/// visible in a diff, instead of editing one condition, which is not.
///
/// The rule: a requested log file is refused on the ordinary single-run adapter,
/// and permitted under `--verify`, where the verification adapter owns the sink.
#[cfg(any(feature = "dbt", test))]
fn dbt_log_file_is_refused(log_file_requested: bool, verify: bool) -> bool {
    log_file_requested && !verify
}

/// Runs `program` through DynamoRIO with the real Detcore Tool.
///
/// Verification obtains structured tracing records through Reverie's
/// authenticated per-run evidence channel, decodes the finalized framed
/// artifact after the complete process tree is reaped, and hands the resulting
/// log plus exact stdout/stderr/status to Hermit's ordinary typed comparator.
/// A JSON report is an optional output of that comparison, never the switch
/// that decides whether the comparison runs.
// This mirrors the option surface of `hermit run`, so its parameters track the
// CLI run flags rather than a cohesive value object; bundling them would not
// clarify the dispatch shim.
#[allow(clippy::too_many_arguments)]
#[cfg(feature = "dbt")]
pub(super) fn run_dbt(
    program: &Path,
    args: &[String],
    verify: bool,
    verify_verbose: bool,
    verify_allow: VerifyAllow,
    print_verify_logs: bool,
    keep_logs: bool,
    verify_log_dir: Option<&Path>,
    verify_json: Option<&Path>,
    summary: bool,
    backend_engagement_json: Option<&Path>,
    log: Option<LevelFilter>,
    log_file: Option<&Path>,
    config: &Config,
    mut environment: BTreeMap<OsString, OsString>,
    workdir: Option<&Path>,
    verification_stdin: Option<std::fs::File>,
) -> Result<ExitStatus, Error> {
    if let Some(path) = verify_json.filter(|_| verify) {
        write_pending_verification_json(path)?;
    }
    if dbt_log_file_is_refused(log_file.is_some(), verify) {
        return Err(Error::msg(DBT_LOG_FILE_REFUSAL));
    }
    // The DBT backend drives a single Detcore external scheduler, so it cannot
    // honor a request to relax thread sequentialization. Fail loudly rather
    // than silently ignoring the flag.
    if !config.sequentialize_threads {
        return Err(Error::msg(
            "the dbt backend requires sequentialized threads; \
             remove --no-sequentialize-threads (or --strace-only) to run under --backend dbt",
        ));
    }
    let config_json = serde_json::to_string(config).map_err(|error| {
        Error::msg(format!(
            "failed to serialize the Detcore config for the DBT backend: {error}"
        ))
    })?;
    // The full DetConfig now reaches the DBT runtime via the serialized env
    // above; the fail-closed policy (PR #644) still drives process-group
    // isolation and the client flag here.
    let panic_on_unsupported_syscalls = config.panic_on_unsupported_syscalls;

    let marker = std::env::var_os(hermit_test_workdir::REQUEST_ENV);
    let isolated_workdir = hermit_test_workdir::requested_workdir(marker.as_deref())?;
    if isolated_workdir.is_some() && workdir != isolated_workdir {
        return Err(Error::msg(
            "HERMIT_E2E_EMPTY_WORKDIR=/test requires --workdir=/test for DBT",
        ));
    }
    let stdin_is_terminal = std::io::stdin().is_terminal();

    let (drrun, client) = detcore_dbt::prepare_native_client().map_err(|error| {
        Error::msg(format!(
            "failed to prepare the Detcore DynamoRIO client: {error}"
        ))
    })?;
    let single_run_stats = (!verify && (summary || backend_engagement_json.is_some()))
        .then(DbtStatsCapture::new)
        .transpose()?;
    let mut runner = DbtRunner::new(&drrun, &client)
        .map_err(|error| {
            Error::msg(format!(
                "failed to configure the DynamoRIO DBT runner (drrun={}, client={}): {error}",
                drrun.display(),
                client.display()
            ))
        })?
        .summary(summary)
        .isolated_process_group(panic_on_unsupported_syscalls);
    if let Some(capture) = &single_run_stats {
        runner = capture.configure(runner);
    }
    if panic_on_unsupported_syscalls {
        runner = runner.client_argument("-panic-on-unsupported-syscalls");
    }

    eprintln!(
        "hermit: [dbt backend] Detcore Tool active; running {program:?} under DynamoRIO ({})",
        drrun.display()
    );

    let _unsupported_report = DbtUnsupportedSyscallReport::new()?;
    let mut guest = StdCommand::new(program);
    if !verify && let Some(level) = log {
        environment.insert("HERMIT_LOG".into(), level.to_string().into());
    }
    environment.remove(OsStr::new("HERMIT_LOG_FILE"));
    environment.insert(detcore_dbt::DETCONFIG_ENV.into(), config_json.into());
    apply_exact_environment(&mut guest, &environment);
    guest.args(args);
    apply_dbt_workdir(&mut guest, workdir);

    let execution = DbtExecution::new(isolated_workdir)?;

    if !verify {
        if stdin_is_terminal {
            let status = run_status(&execution, &runner, &guest, &drrun, config)?;
            if let Some(capture) = single_run_stats {
                finish_single_run_dbt_stats(capture, summary, backend_engagement_json)?;
            }
            return Ok(process_status(status));
        }
        let output = run_once(
            &execution,
            &runner,
            &guest,
            &drrun,
            config,
            std::io::stdin(),
        )?;
        write_output(&output)?;
        if let Some(capture) = single_run_stats {
            finish_single_run_dbt_stats(capture, summary, backend_engagement_json)?;
        }
        return Ok(output_status(&output));
    }

    // The capture names are READ BY THE HARNESS, so they are not a local choice.
    //
    // `ci/compat-envelope/pressure-test.rs` and `ci/manifest-plan/src/runner.rs`
    // both scan a retained verify-log directory with
    // `name.starts_with("run1_log_")` / `("run2_log_")`. This call used to pass
    // "dbt-run1"/"dbt-run2", which `temp_log_files_in` turns into the prefixes
    // `dbt-run1_log_` / `dbt-run2_log_` -- and those do not start with
    // `run1_log_`. `run.rs` passes "run1"/"run2" for every other backend.
    //
    // Measured 2026-08-27 on one guest with one binary, backend the only
    // variable: DBT wrote both captures, 47,507 bytes each, and the harness
    // predicate matched 0 of the 2 it requires; the same command without
    // `--backend dbt` wrote `run1_log_`/`run2_log_` and matched 2 of 2. So every
    // dbt verify cell was recorded `infrastructure-error` for
    // "terminal verify result must retain exactly one nonempty run1 log and one
    // nonempty run2 log" while its logs sat in the directory under a name
    // nothing looked for. 174 of 174 records under that condition were dbt.
    //
    // Fixed HERE rather than by teaching the scanners a second prefix: the
    // scanners already refuse a missing, duplicated, or empty capture, and
    // widening what they accept would weaken the check that caught this.
    let (log1, log2) = temp_log_files_in("run1", "run2", verify_log_dir)
        .map_err(|error| Error::msg(format!("failed to create DBT verification logs: {error}")))?;
    let (log1_file, log1_path) = log1.into_parts();
    let (log2_file, log2_path) = log2.into_parts();
    let evidence_level = dbt_evidence_log_level(log, verify_verbose);
    let mut evidence1 = tempfile::tempfile()?;
    let stats1 = DbtStatsCapture::new()?;
    let runner1 = stats1
        .configure(runner.clone())
        .evidence_file(&evidence1)
        .map_err(|error| {
            Error::msg(format!(
                "failed to configure protected DBT run-1 evidence: {error}"
            ))
        })?
        .evidence_log_level(evidence_level);
    let mut evidence2 = tempfile::tempfile()?;
    let stats2 = DbtStatsCapture::new()?;
    let runner2 = stats2
        .configure(runner)
        .evidence_file(&evidence2)
        .map_err(|error| {
            Error::msg(format!(
                "failed to configure protected DBT run-2 evidence: {error}"
            ))
        })?
        .evidence_log_level(evidence_level);

    let mut replay = tempfile::tempfile()?;
    let terminal_stdin = verification_stdin.as_ref().is_some_and(|file| {
        // SAFETY: `as_raw_fd` borrows a live descriptor for this check.
        (unsafe { libc::isatty(file.as_raw_fd()) }) == 1
    });
    let replayable_stdin = verification_stdin.filter(|file| {
        // SAFETY: `as_raw_fd` borrows a live descriptor for this check.
        (unsafe { libc::isatty(file.as_raw_fd()) }) != 1
    });

    eprintln!(":: DBT Run1...");
    let first_raw = if terminal_stdin {
        run_once_with_terminal_input(&execution, &runner1, &guest, &drrun, config)
    } else {
        match replayable_stdin {
            Some(input) => run_once(
                &execution,
                &runner1,
                &guest,
                &drrun,
                config,
                TeeReader {
                    input,
                    replay: replay.try_clone()?,
                },
            ),
            None => run_once(
                &execution,
                &runner1,
                &guest,
                &drrun,
                config,
                std::io::empty(),
            ),
        }
    };
    let first_raw = match first_raw {
        Ok(output) => output,
        Err(error) => {
            if keep_logs {
                retain_verification_logs([("run 1", log1_path)])?;
            }
            return Err(error);
        }
    };
    let first_evidence = match decode_dbt_evidence(&mut evidence1) {
        Ok(evidence) => evidence,
        Err(error) => {
            if keep_logs {
                retain_verification_logs([("run 1", log1_path)])?;
            }
            return Err(error);
        }
    };
    if let Err(error) =
        materialize_dbt_comparison_log(&first_evidence.records, log1_file, &log1_path)
    {
        if keep_logs {
            retain_verification_logs([("run 1", log1_path)])?;
        }
        return Err(error);
    }
    if print_verify_logs {
        std::io::stderr().write_all(&fs::read(&log1_path)?)?;
    }
    if !verify_allow.satisfies(process_status(first_raw.status)) {
        let first = dbt_verification_output(first_raw);
        eprintln!(
            "First run errored during --verify, not continuing to a second. Stdout:\n{}\nStderr:\n{}",
            String::from_utf8_lossy(&first.stdout),
            String::from_utf8_lossy(&first.stderr),
        );
        if keep_logs {
            retain_verification_logs([("run 1", log1_path)])?;
        }
        return Err(Error::msg("First run during --verify exited in error"));
    }
    let first_stats = match stats1.finish() {
        Ok(stats) => stats,
        Err(error) => {
            if keep_logs {
                retain_verification_logs([("run 1", log1_path)])?;
            }
            return Err(error);
        }
    };
    if terminal_stdin && first_stats.stdin_reads() != 0 {
        let first = dbt_verification_output(first_raw);
        std::io::stdout().write_all(&first.stdout)?;
        std::io::stderr().write_all(&first.stderr)?;
        if keep_logs {
            retain_verification_logs([("run 1", log1_path)])?;
        }
        return Err(Error::msg(format!(
            "DBT verification cannot replay terminal stdin: guest attempted {} fd-0 read syscall(s)",
            first_stats.stdin_reads()
        )));
    }
    let first = dbt_verification_output(first_raw);

    replay.seek(SeekFrom::Start(0))?;
    eprintln!(":: DBT Run2...");
    let second_raw = match if terminal_stdin {
        run_once_with_terminal_input(&execution, &runner2, &guest, &drrun, config)
    } else {
        run_once(
            &execution,
            &runner2,
            &guest,
            &drrun,
            config,
            replay.try_clone()?,
        )
    } {
        Ok(output) => output,
        Err(error) => {
            if keep_logs {
                retain_verification_logs([("run 1", log1_path), ("run 2", log2_path)])?;
            }
            return Err(error);
        }
    };
    let second_evidence = match decode_dbt_evidence(&mut evidence2) {
        Ok(evidence) => evidence,
        Err(error) => {
            if keep_logs {
                retain_verification_logs([("run 1", log1_path), ("run 2", log2_path)])?;
            }
            return Err(error);
        }
    };
    if let Err(error) =
        materialize_dbt_comparison_log(&second_evidence.records, log2_file, &log2_path)
    {
        if keep_logs {
            retain_verification_logs([("run 1", log1_path), ("run 2", log2_path)])?;
        }
        return Err(error);
    }
    let second_stats = match stats2.finish() {
        Ok(stats) => stats,
        Err(error) => {
            if keep_logs {
                retain_verification_logs([("run 1", log1_path), ("run 2", log2_path)])?;
            }
            return Err(error);
        }
    };
    let second = dbt_verification_output(second_raw);

    let branch_clock_comparison = DbtCountedBranchComparison {
        left: first_stats.counted_branches(),
        right: second_stats.counted_branches(),
    };
    let branch_clock_diverged = !branch_clock_comparison.matched();
    let summary_comparison = DbtSummaryComparison::compare(
        &first_stats,
        &second_stats,
        first_evidence.initialization_records,
        second_evidence.initialization_records,
    );
    let mut outcome = compare_two_runs(
        ComparedRun {
            output: &first,
            log: log1_path,
            label: "run 1",
        },
        ComparedRun {
            output: &second,
            log: log2_path,
            label: "run 2",
        },
        ComparisonOptions {
            verbose: verify_verbose,
            strictness: LogCompareStrictness::Canonical,
            compare_logs: true,
            diagnostic_full_trace: verify_verbose,
            compare_io_buffers: config.detlog_io_buffers,
            // Read from the LIVE config, for the same reason the run path does:
            // this is a genuine runtime setting, so a hard-coded value would
            // publish a time policy the run did not use.
            virtualize_time: config.virtualize_time,
            // A backend-observed divergence needs the same retained evidence
            // as a divergence found by the ordinary comparator.
            keep_logs: keep_logs
                || branch_clock_diverged
                || summary_comparison.requires_log_retention(),
            failed_log_retention: (!keep_logs)
                .then(super::verify::default_failed_verify_log_retention),
            // Reverie's authenticated decoder represents each process-image
            // initialization record by a count instead of preserving its
            // host-arrival position in the comparable stream. The counts are
            // checked above; this named envelope states that transport records
            // are excluded while every remaining record is compared.
            record_envelope: RecordEnvelope::dbt_evidence_transport_v1(),
        },
    )?;
    let summary_failure = summary_comparison.apply(&mut outcome);
    let (outcome, branch_clock_failure) =
        finalize_dbt_verification(outcome, branch_clock_comparison, verify_json)?;
    // Publish the typed record before announcing any terminal verdict. If the
    // process is killed after this line, a reader still sees this invocation's
    // divergence rather than the pending `no_result` stamp.
    if let Some(message) = branch_clock_failure {
        eprintln!(":: {message}");
    }
    if let Some(message) = summary_failure {
        eprintln!(":: {message}");
    }
    eprintln!(":: DBT path confirmed: DynamoRIO client reported tool=Detcore");
    let success_message = if config.detlog_io_buffers {
        "Success: deterministic. Determinism verified."
    } else {
        "Success: deterministic. Determinism verified. NOTE: syscall \
         output-buffer CONTENT was not compared because --no-detlog-io-buffers \
         was given, so a divergence confined to a buffer whose length is stable \
         would not have been seen; drop that flag to include it."
    };
    announce_verification_outcome(&outcome, success_message, "Failure: nondeterministic.");
    if !outcome.verified() {
        return outcome.into_exit_status();
    }

    std::io::stdout().write_all(&first.stdout)?;
    std::io::stderr().write_all(&first.stderr)?;
    Ok(outcome.guest_status)
}

#[allow(clippy::too_many_arguments)]
#[cfg(not(feature = "dbt"))]
pub(super) fn run_dbt(
    _program: &Path,
    _args: &[String],
    _verify: bool,
    _verify_verbose: bool,
    _verify_allow: VerifyAllow,
    _print_verify_logs: bool,
    _keep_logs: bool,
    _verify_log_dir: Option<&Path>,
    _verify_json: Option<&Path>,
    _summary: bool,
    _backend_engagement_json: Option<&Path>,
    _log: Option<LevelFilter>,
    _log_file: Option<&Path>,
    _config: &Config,
    _environment: BTreeMap<OsString, OsString>,
    _workdir: Option<&Path>,
    _verification_stdin: Option<std::fs::File>,
) -> Result<ExitStatus, Error> {
    Err(Error::msg("DBT support was not included in this build"))
}

/// Ordinary runs share the existing coordinator runtime. Marked runs create
/// all coordinator workers inside a new mount namespace for each physical run.
#[cfg(feature = "dbt")]
struct DbtExecution {
    runtime: Option<tokio::runtime::Runtime>,
}

#[cfg(feature = "dbt")]
impl DbtExecution {
    fn new(workdir: Option<&Path>) -> Result<Self, Error> {
        Ok(Self {
            runtime: if workdir.is_some() {
                None
            } else {
                Some(Self::new_runtime()?)
            },
        })
    }

    fn new_runtime() -> Result<tokio::runtime::Runtime, Error> {
        // RPCs can wait on the scheduler, so both need independent workers.
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|error| Error::msg(format!("failed to start the DBT coordinator: {error}")))
    }

    fn run<F, T>(&self, run: F) -> Result<T, Error>
    where
        F: FnOnce(&tokio::runtime::Runtime) -> Result<T, Error> + Send,
        T: Send,
    {
        if let Some(runtime) = &self.runtime {
            return run(runtime);
        }
        hermit_test_workdir::with_isolated_workdir(move || {
            let runtime = Self::new_runtime()?;
            // The operation consumes GlobalState cleanup before returning;
            // runtime Drop joins its workers before this scoped thread exits.
            run(&runtime)
        })?
    }
}

#[cfg(feature = "dbt")]
fn run_once<R: Read + Send + 'static>(
    execution: &DbtExecution,
    runner: &DbtRunner,
    guest: &StdCommand,
    drrun: &Path,
    config: &Config,
    input: R,
) -> Result<Output, Error> {
    execution.run(move |runtime| {
        let guest = prepare_dbt_physical_command(guest)?;
        let (output, global) = runtime
            .block_on(
                runner.output_with_detached_reader_and_global::<detcore::GlobalState, _>(
                    &guest,
                    input,
                    config.clone(),
                ),
            )
            .map_err(|error| dbt_run_error(drrun, error))?;
        runtime.block_on(clean_up_dbt_global(&output.status, global));
        Ok(output)
    })
}

#[cfg(feature = "dbt")]
fn run_once_with_terminal_input(
    execution: &DbtExecution,
    runner: &DbtRunner,
    guest: &StdCommand,
    drrun: &Path,
    config: &Config,
) -> Result<Output, Error> {
    execution.run(move |runtime| {
        let guest = prepare_dbt_physical_command(guest)?;
        let (output, global) = runtime
            .block_on(
                runner.output_with_inherited_stdin_and_global::<detcore::GlobalState>(
                    &guest,
                    config.clone(),
                ),
            )
            .map_err(|error| dbt_run_error(drrun, error))?;
        runtime.block_on(clean_up_dbt_global(&output.status, global));
        Ok(output)
    })
}

#[cfg(feature = "dbt")]
fn run_status(
    execution: &DbtExecution,
    runner: &DbtRunner,
    guest: &StdCommand,
    drrun: &Path,
    config: &Config,
) -> Result<std::process::ExitStatus, Error> {
    execution.run(move |runtime| {
        let guest = prepare_dbt_physical_command(guest)?;
        let (status, global) = runtime
            .block_on(runner.status_with_global::<detcore::GlobalState>(&guest, config.clone()))
            .map_err(|error| dbt_run_error(drrun, error))?;
        runtime.block_on(clean_up_dbt_global(&status, global));
        Ok(status)
    })
}

#[cfg(feature = "dbt")]
async fn clean_up_dbt_global(status: &std::process::ExitStatus, mut global: detcore::GlobalState) {
    if !status.success() {
        global.force_shutdown_with_error();
        // The physical supervisor and RPC owner drain have finished. A client
        // that failed before registration cannot start the owned scheduler.
        global.cancel_internal_scheduler().await;
    }
    global.clean_up(false, &None).await;
}

/// Name the stage that actually failed.
///
/// This used to be `launch_error`, and it announced EVERY `io::Error` from a
/// DBT run as "failed to launch drrun ({path})". But the calls it wraps --
/// `output_with_detached_reader` and `output_with_inherited_stdin` -- run the
/// whole lifecycle: spawn, wait, collect output, and finalize the protected
/// evidence session. A failure in any later stage was reported as a failure of
/// the first one.
///
/// Measured cost of that: a campaign agent hit
/// `"failed to launch drrun (target/install_pkg/rsrcs/dynamorio/bin64/drrun):
/// DBT guest exited with status S..."`, correctly observed that drrun was a
/// real 737 KB ELF matching the build cache, and could not proceed. The inner
/// text is the giveaway -- it comes from `reverie-dbt`'s
/// `(Ok(status), Err(error))` arm, so the guest HAD RUN AND EXITED and the
/// failure was in evidence finalization. A binary that failed to launch cannot
/// produce an exit status. The wrapper had renamed a post-run failure into a
/// missing-binary hunt.
///
/// The discriminator is `ErrorKind`. A real spawn failure surfaces the OS error
/// (`NotFound` when drrun is absent, `PermissionDenied` when it is not
/// executable, `ExecutableFileBusy`). Where the kind does not prove a spawn
/// failure this deliberately does NOT claim one.
///
/// The pinned Reverie revision preserves this distinction: once it has an exit
/// status, an evidence-finalization failure is returned as `Other`, even when
/// the underlying error was `NotFound` or `PermissionDenied`. Spawn failures
/// bypass that combination and retain their original kind. Do not replace this
/// typed boundary with matching on display text.
#[cfg(feature = "dbt")]
fn dbt_run_error(drrun: &Path, error: std::io::Error) -> Error {
    use std::io::ErrorKind;
    match error.kind() {
        ErrorKind::NotFound | ErrorKind::PermissionDenied | ErrorKind::ExecutableFileBusy => {
            Error::msg(format!(
                "failed to launch drrun ({}): {error}",
                drrun.display()
            ))
        }
        _ => Error::msg(format!(
            "drrun started ({}) and the DBT run then failed: {error} \
             -- this is NOT a launch failure; the drrun binary is not implicated. \
             Read the text after the last ':' for the stage that actually failed.",
            drrun.display()
        )),
    }
}

// ⚠️ `test` AS WELL AS `dbt`, AND THE REASON IS THAT THE BRACKETS BELOW MUST RUN.
// `dbt` is not in `default` (hermit-cli/Cargo.toml: `default = []`), so under
// `#[cfg(feature = "dbt")]` alone this function and any test of it compile only in
// a build validation does not perform. Measured on main before this change:
//
//     $ cargo test -p hermit --bin hermit dbt_status
//     running 0 tests
//
// Zero -- so the only check that a signalled death is not reported as a normal exit
// contributed nothing to any receipt. Adding `test` keeps the symbol out of a
// feature-off PRODUCTION build, where `clippy -D warnings` would call the import
// dead, while letting the brackets execute wherever tests do.
#[cfg(any(feature = "dbt", test))]
fn process_status(status: std::process::ExitStatus) -> ExitStatus {
    ExitStatus::from_raw(status.into_raw())
}

#[cfg(feature = "dbt")]
fn write_output(output: &Output) -> Result<(), Error> {
    std::io::stdout().write_all(&output.stdout)?;
    std::io::stderr().write_all(&output.stderr)?;
    Ok(())
}

// `test` as well as `dbt`, matching process_status above. This is the site where
// the signal-losing conversion was MISSED -- `code().unwrap_or(1)` reported every
// signalled death as Exited(1) -- so it is the one that most needs a bracket that
// actually runs, and the bracket for it ran ZERO times on the default build.
#[cfg(any(feature = "dbt", test))]
fn output_status(output: &Output) -> ExitStatus {
    // ⚠️ SAME SIGNAL-LOSING CONVERSION `process_status` ALREADY FIXED, MISSED HERE.
    // `std::process::ExitStatus::code()` is `None` for a process killed by a
    // signal, so `code().unwrap_or(1)` reported every signalled death as a
    // normal `Exited(1)`: a guest killed by SIGSEGV came back as "exited
    // normally with status 1", and WIFSIGNALED/WTERMSIG/WCOREDUMP were all lost.
    // `ExitStatus::from_raw` decodes exited-versus-signalled and the core-dump
    // flag the same way the ptrace backend does.
    ExitStatus::from_raw(output.status.into_raw())
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

#[cfg(test)]
mod tests {
    #[cfg(feature = "dbt")]
    #[tokio::test]
    async fn dbt_abnormal_cleanup_ends_an_unregistered_scheduler() {
        use std::os::unix::process::ExitStatusExt;

        use reverie::GlobalTool;

        let config = detcore::Config {
            sequentialize_threads: true,
            ..detcore::Config::default()
        };
        let mut outcomes = Vec::new();
        for (name, raw_status) in [("exit255", 255 << 8), ("SIGKILL", libc::SIGKILL)] {
            let global = detcore::GlobalState::init_global_state(&config).await;
            let status = std::process::ExitStatus::from_raw(raw_status);
            let completed = tokio::time::timeout(
                std::time::Duration::from_millis(100),
                super::clean_up_dbt_global(&status, global),
            )
            .await
            .is_ok();
            assert_eq!(status.into_raw(), raw_status);
            outcomes.push((name, completed));
        }
        assert_eq!(
            outcomes,
            [("exit255", true), ("SIGKILL", true)],
            "DBT cleanup waited for a scheduler whose physical guest had already failed"
        );
    }

    #[cfg(feature = "dbt")]
    #[tokio::test]
    async fn dbt_successful_cleanup_preserves_normal_scheduler_completion() {
        use std::os::unix::process::ExitStatusExt;

        use reverie::GlobalTool;

        let status = std::process::ExitStatus::from_raw(0);
        let config = detcore::Config {
            sequentialize_threads: false,
            ..detcore::Config::default()
        };
        let global = detcore::GlobalState::init_global_state(&config).await;
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                super::clean_up_dbt_global(&status, global),
            )
            .await
            .is_ok(),
            "cleanup without an owned scheduler did not complete"
        );
        let config = detcore::Config {
            sequentialize_threads: true,
            ..detcore::Config::default()
        };
        let global = detcore::GlobalState::init_global_state(&config).await;
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                super::clean_up_dbt_global(&status, global),
            )
            .await
            .is_err(),
            "successful DBT cleanup silently cancelled a scheduler that had not completed"
        );
        assert!(status.success());
    }
    #[cfg(feature = "dbt")]
    #[test]
    fn dbt_workdir_reaches_the_dynamorio_guest_command() {
        let mut command = std::process::Command::new("/bin/true");
        super::apply_dbt_workdir(&mut command, Some(std::path::Path::new("/test")));
        assert_eq!(
            command.get_current_dir(),
            Some(std::path::Path::new("/test"))
        );
    }

    #[cfg(feature = "dbt")]
    /// A behavioural pin, not a text match: require every record returned by
    /// Reverie's authenticated decoder to reach the comparison log. The decoder
    /// represents transport initialization records separately by count, so this
    /// input is exactly the comparable-record side of that boundary.
    #[test]
    fn materialization_keeps_every_decoded_comparable_record() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("evidence.log");
        let file = std::fs::File::create(&path).expect("create log");
        let records: Vec<Vec<u8>> = vec![
            b"1970-01-01T00:00:00.000000Z  INFO detcore: DETLOG first\n".to_vec(),
            b"1970-01-01T00:00:00.000000Z  INFO detcore: DETLOG second\n".to_vec(),
        ];

        super::materialize_dbt_comparison_log(&records, file, &path)
            .expect("materialization must accept comparable records");

        assert_eq!(std::fs::read(&path).expect("read back"), records.concat());
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_initialization_record_counts_control_the_terminal_outcome() {
        let stats = dbt_stats(10, 20, 5, 2, 99);
        let equal = DbtSummaryComparison::compare(&stats, &stats, 2, 2);
        assert!(!equal.requires_log_retention());
        let mut matched = typed_outcome(Verdict::Matched, 0);
        assert!(equal.apply(&mut matched).is_none());
        assert_eq!(matched.verdict, Verdict::Matched);

        let unequal = DbtSummaryComparison::compare(&stats, &stats, 2, 3);
        assert!(
            unequal.requires_log_retention(),
            "an initialization mismatch must retain both evidence logs"
        );
        let mut outcome = typed_outcome(Verdict::Matched, 0);
        let failure = unequal.apply(&mut outcome).unwrap();
        assert_eq!(outcome.verdict, Verdict::Diverged);
        assert!(!outcome.verified());
        assert!(
            !outcome
                .into_exit_status()
                .expect("divergence has a terminal status")
                .success()
        );
        assert!(
            failure.contains("initialization counts differed"),
            "{failure}"
        );
        assert!(failure.contains("2 != 3"), "{failure}");
    }
    use super::*;

    #[cfg(feature = "dbt")]
    fn write_executable(path: &Path, contents: &[u8]) {
        fs::write(path, contents).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[cfg(feature = "dbt")]
    fn dbt_stats(
        branches: u64,
        syscalls: u64,
        rewritten: u64,
        stdin_reads: u64,
        memory_hash: u64,
    ) -> DbtBackendStatsSnapshot {
        use reverie_dbt::backend_stats::DbtProcessRecord;

        let record = DbtProcessRecord {
            branches,
            syscalls,
            rewritten,
            stdin_reads,
            memory_hash,
            ..Default::default()
        };
        let mut aggregator = DbtBackendStatsAggregator::new();
        aggregator.record(&record);
        aggregator.snapshot()
    }

    #[cfg(feature = "dbt")]
    fn typed_outcome(verdict: Verdict, guest_exit: i32) -> VerificationOutcome {
        use super::super::verify::ComparedLogCounts;
        use super::super::verify::ComparisonSpec;

        VerificationOutcome {
            verdict,
            no_result_reason: (verdict == Verdict::NoResult).then(|| {
                super::super::verify::NoResultReason::ComparisonRefused {
                    detail: "fixture comparator refusal".into(),
                }
            }),
            guest_status: ExitStatus::Exited(guest_exit),
            comparison: ComparisonSpec::new(
                LogCompareStrictness::Canonical,
                true,
                false,
                true,
                RecordEnvelope::all_records_v1().policy(),
                true,
            ),
            compared_log_messages: Some(ComparedLogCounts { left: 4, right: 4 }),
            compared_outputs: {
                let output = hermit::canonical_verdict::ComparedOutput {
                    exit_code: Some(guest_exit),
                    signal: None,
                    stdout_sha256: "a".repeat(64),
                    stdout_bytes: 0,
                    stderr_sha256: "b".repeat(64),
                    stderr_bytes: 0,
                };
                hermit::canonical_verdict::ComparedOutputs {
                    left: output.clone(),
                    right: output,
                }
            },
            dbt_counted_branches: None,
            runtime: None,
            first_divergent_scheduler_turn: None,
            first_divergent_virtual_nanoseconds: None,
            first_divergent_record: None,
            first_divergent_syscall: None,
            first_divergent_left_message: None,
            first_divergent_right_message: None,
        }
    }

    /// Pin the production control flow without launching DynamoRIO. This uses
    /// the same source-order contract style as the CLI dispatch and report-first
    /// tests: every slice ends before the test module, so its own needles cannot
    /// satisfy the assertions.
    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_verification_run_path_binds_terminal_verdict_to_branch_stats() {
        let source = include_str!("backends.rs");
        let finalizer = source
            .split_once("fn finalize_dbt_verification(")
            .expect("typed DBT finalizer")
            .1
            .split_once("/// Own the typed statistics stream")
            .expect("end of typed DBT finalizer")
            .0;
        let attach = finalizer
            .find("record_dbt_branch_clock_comparison(")
            .expect("attach branch-clock comparison");
        let publish = finalizer
            .find("write_verification_json(path, &outcome)")
            .expect("publish terminal typed verdict");
        let return_outcome = finalizer
            .find("Ok((outcome, failure))")
            .expect("return finalized outcome");
        assert!(
            attach < publish && publish < return_outcome,
            "the finalizer must attach the branch comparison and publish JSON before returning it"
        );

        let canonical_marker = concat!("#[cfg(feature = \"dbt\")]\n", "pub(super) fn ", "run_dbt(");
        let canonical = source
            .split_once(canonical_marker)
            .expect("canonical DBT run path")
            .1
            .split_once(r#"#[cfg(not(feature = "dbt"))]"#)
            .expect("end of canonical DBT run path")
            .0;
        assert!(
            !canonical.contains("if verify_json.is_none()"),
            "DBT verification must not bypass protected INFO evidence when no JSON output path \
             was requested"
        );
        let first_stats = canonical
            .find("let first_stats = match stats1.finish()")
            .expect("run-1 typed stats");
        let second_stats = canonical
            .find("let second_stats = match stats2.finish()")
            .expect("run-2 typed stats");
        let comparison = canonical
            .find("let branch_clock_comparison = DbtCountedBranchComparison")
            .expect("typed branch-clock comparison");
        let summary_comparison = canonical
            .find("let summary_comparison = DbtSummaryComparison::compare(")
            .expect("typed summary and initialization comparison");
        let force_branch_logs = canonical
            .find("|| branch_clock_diverged")
            .expect("branch divergence forces log retention");
        let force_summary_logs = canonical
            .find("|| summary_comparison.requires_log_retention()")
            .expect("summary divergence forces log retention");
        let apply_summary = canonical
            .find("let summary_failure = summary_comparison.apply(&mut outcome)")
            .expect("summary divergence updates the terminal outcome");
        let finalize = canonical
            .find("finalize_dbt_verification(outcome, branch_clock_comparison, verify_json)")
            .expect("terminal typed finalization");
        let announce = canonical
            .find("if let Some(message) = branch_clock_failure")
            .expect("branch-specific terminal announcement");
        let exit = canonical
            .find("return outcome.into_exit_status()")
            .expect("terminal nonzero conversion");
        assert!(
            first_stats < second_stats
                && second_stats < comparison
                && comparison < summary_comparison
                && summary_comparison < force_branch_logs
                && force_branch_logs < force_summary_logs
                && force_summary_logs < apply_summary
                && apply_summary < finalize
                && finalize < announce
                && announce < exit,
            "canonical verification must collect both stats and authenticated initialization \
             counts, compute both comparisons, force logs, apply summary divergence, publish \
             through the finalizer, and only then announce or convert the terminal status"
        );

        for (stats, next) in [
            (
                "let first_stats = match stats1.finish()",
                "let first = dbt_verification_output",
            ),
            (
                "let second_stats = match stats2.finish()",
                "let second = dbt_verification_output",
            ),
        ] {
            let failure_arm = canonical
                .split_once(stats)
                .expect("typed stats collection")
                .1
                .split_once(next)
                .expect("end of typed stats failure arm")
                .0;
            assert!(failure_arm.contains("Err(error) =>"));
            assert!(
                failure_arm.contains("return Err(error);"),
                "unreadable typed stats must return while the pre-stamped no_result is still current"
            );
        }
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn equal_branch_clocks_do_not_authorize_a_no_result() {
        let outcome = typed_outcome(Verdict::NoResult, 23);
        let comparison = DbtCountedBranchComparison {
            left: 563_145,
            right: 563_145,
        };
        let verdict_file = tempfile::NamedTempFile::new().unwrap();

        let (outcome, failure) =
            finalize_dbt_verification(outcome, comparison, Some(verdict_file.path())).unwrap();
        let json: serde_json::Value =
            serde_json::from_slice(&fs::read(verdict_file.path()).unwrap()).unwrap();

        assert!(failure.is_none());
        assert_eq!(outcome.verdict, Verdict::NoResult);
        assert!(outcome.dbt_counted_branches.is_none());
        assert_eq!(json["verdict"], "no_result");
        assert_eq!(json["verified"], false);
        assert!(json.get("dbt_counted_branches").is_none());
        let error = outcome.into_exit_status().unwrap_err().to_string();
        assert!(error.contains("did not reach a verdict"), "{error}");
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn branch_clock_divergence_is_retained_in_the_serialized_typed_verdict() {
        use super::super::verify::ComparedLogCounts;

        for initial_verdict in [Verdict::Matched, Verdict::NoResult] {
            let outcome = typed_outcome(initial_verdict, 0);
            let comparison = DbtCountedBranchComparison {
                left: 563_145,
                right: 563_103,
            };
            let verdict_file = tempfile::NamedTempFile::new().unwrap();
            let (outcome, message) =
                finalize_dbt_verification(outcome, comparison, Some(verdict_file.path())).unwrap();
            let message = message.unwrap();
            let json: serde_json::Value =
                serde_json::from_slice(&fs::read(verdict_file.path()).unwrap()).unwrap();

            assert!(message.contains("563145 != 563103"), "{message}");
            assert_eq!(outcome.verdict, Verdict::Diverged);
            assert_eq!(outcome.dbt_counted_branches, Some(comparison));
            assert_eq!(json["verdict"], "diverged");
            assert_eq!(json["verified"], false);
            assert_eq!(json["bitwise_parity"], false);
            assert_eq!(json["dbt_counted_branches"]["left"], 563_145);
            assert_eq!(json["dbt_counted_branches"]["right"], 563_103);
            assert_eq!(
                outcome.compared_log_messages,
                Some(ComparedLogCounts { left: 4, right: 4 })
            );
            assert_eq!(
                outcome.into_exit_status().unwrap(),
                ExitStatus::Exited(hermit::HERMIT_VERIFICATION_DIVERGENCE_EXIT)
            );
        }
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn equal_branch_clocks_preserve_match_and_nonzero_guest_status() {
        let outcome = typed_outcome(Verdict::Matched, 23);
        let comparison = DbtCountedBranchComparison {
            left: 563_145,
            right: 563_145,
        };

        let (outcome, failure) = finalize_dbt_verification(outcome, comparison, None).unwrap();
        assert!(failure.is_none());
        let report = super::super::verify::verification_report(&outcome);

        assert_eq!(outcome.verdict, Verdict::Matched);
        assert_eq!(report.dbt_counted_branches, Some(comparison));
        assert!(report.verified);
        assert!(report.bitwise_parity);
        assert_eq!(outcome.into_exit_status().unwrap(), ExitStatus::Exited(23));
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn typed_stats_capture_aggregates_the_whole_process_tree() {
        use reverie_dbt::backend_stats::DbtProcessRecord;
        use reverie_dbt::backend_stats::encode_process_record;

        let capture = DbtStatsCapture::new().unwrap();
        let root = DbtProcessRecord {
            branches: 400,
            syscalls: 7,
            ..Default::default()
        };
        let child = DbtProcessRecord {
            branches: 23,
            syscalls: 2,
            ..Default::default()
        };
        let bytes = [encode_process_record(&root), encode_process_record(&child)].concat();
        fs::write(&capture.path, bytes).unwrap();

        let snapshot = capture.finish().unwrap();

        assert_eq!(snapshot.process_images(), 2);
        assert_eq!(snapshot.counted_branches(), 423);
        assert_eq!(snapshot.intercepted_syscalls(), 9);
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_engagement_record_follows_the_typed_branch_count() {
        fn record(branches: u64) -> serde_json::Value {
            let capture = DbtStatsCapture::new().unwrap();
            let row = reverie_dbt::backend_stats::DbtProcessRecord {
                branches,
                syscalls: 1,
                rewritten: 1,
                ..Default::default()
            };
            fs::write(
                &capture.path,
                reverie_dbt::backend_stats::encode_process_record(&row),
            )
            .unwrap();
            let output = tempfile::NamedTempFile::new().unwrap();
            finish_single_run_dbt_stats(capture, false, Some(output.path())).unwrap();
            serde_json::from_slice(&fs::read(output.path()).unwrap()).unwrap()
        }

        let first = record(18);
        let second = record(19);
        assert_eq!(first["engagement"]["backend"], "dbt");
        assert_eq!(first["engagement"]["counted_branches"], 18);
        assert_eq!(second["engagement"]["counted_branches"], 19);
        assert_ne!(
            first, second,
            "mutating the producer count must move the record"
        );
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn typed_stats_capture_refuses_missing_empty_or_truncated_evidence() {
        let missing = DbtStatsCapture::new().unwrap();
        let error = missing.finish().unwrap_err();
        assert!(
            error.to_string().contains("statistics were missing"),
            "{error}"
        );

        let empty = DbtStatsCapture::new().unwrap();
        fs::write(&empty.path, b"").unwrap();
        let error = empty.finish().unwrap_err();
        assert!(
            error.to_string().contains("statistics were empty"),
            "{error}"
        );

        let truncated = DbtStatsCapture::new().unwrap();
        fs::write(&truncated.path, b"truncated").unwrap();
        let error = truncated.finish().unwrap_err();
        assert!(
            error.to_string().contains("statistics were unreadable"),
            "{error}"
        );
        assert!(error.to_string().contains("truncated"), "{error}");

        let inconsistent = DbtStatsCapture::new().unwrap();
        let record = reverie_dbt::backend_stats::DbtProcessRecord {
            branches: 18,
            syscalls: 1,
            rewritten: 2,
            ..Default::default()
        };
        fs::write(
            &inconsistent.path,
            reverie_dbt::backend_stats::encode_process_record(&record),
        )
        .unwrap();
        let error = inconsistent.finish().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("typed native callback counters are inconsistent"),
            "{error}"
        );
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn typed_stats_failure_leaves_the_pre_stamped_no_result() {
        let verdict_file = tempfile::NamedTempFile::new().unwrap();
        write_pending_verification_json(verdict_file.path()).unwrap();

        let empty = DbtStatsCapture::new().unwrap();
        fs::write(&empty.path, b"").unwrap();
        assert!(empty.finish().is_err());

        let json: serde_json::Value =
            serde_json::from_slice(&fs::read(verdict_file.path()).unwrap()).unwrap();
        assert_eq!(json["verdict"], "no_result");
        assert_eq!(json["verified"], false);
        assert_eq!(json["bitwise_parity"], false);
        assert!(json.get("dbt_counted_branches").is_none());
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_canonical_evidence_materializes_records_unchanged() {
        let log = tempfile::NamedTempFile::new().unwrap();
        let (file, path) = log.into_parts();
        let records = vec![
            b"1970-01-01T00:00:00.000000Z INFO detcore: DETLOG first\n".to_vec(),
            b"1970-01-01T00:00:00.000000Z INFO detcore::scheduler: second\n".to_vec(),
        ];

        let compared = materialize_dbt_comparison_log(&records, file, &path).unwrap();

        assert_eq!(compared, 2);
        assert_eq!(fs::read(&path).unwrap(), records.concat());
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_canonical_evidence_fails_closed_on_empty_or_unframed_records() {
        let empty = tempfile::NamedTempFile::new().unwrap();
        let (file, path) = empty.into_parts();
        assert!(materialize_dbt_comparison_log(&[], file, &path).is_err());

        let unframed = tempfile::NamedTempFile::new().unwrap();
        let (file, path) = unframed.into_parts();
        assert!(
            materialize_dbt_comparison_log(
                &[b"1970-01-01T00:00:00.000000Z INFO detcore: missing newline".to_vec()],
                file,
                &path,
            )
            .is_err()
        );
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_canonical_evidence_fails_closed_on_empty_or_malformed_artifact() {
        let mut empty = tempfile::tempfile().unwrap();
        assert!(decode_dbt_evidence(&mut empty).is_err());

        let mut malformed = tempfile::tempfile().unwrap();
        malformed.write_all(b"not framed evidence").unwrap();
        assert!(decode_dbt_evidence(&mut malformed).is_err());
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_stats_block_labels_counters_honestly() {
        let rendered = format_dbt_stats(&dbt_stats(563_145, 169, 168, 0, 0x4b5e0e70f3050157));
        // The branch counter must be labeled as a branch clock, never as
        // "basic blocks translated" — the client counts retired branches.
        assert!(rendered.contains("counted branches (deterministic branch clock): 563145"));
        assert!(rendered.contains("syscalls intercepted:                          169"));
        assert!(rendered.contains("syscall instructions rewritten:                168"));
        assert!(rendered.contains("stdin (fd 0) reads:                            0"));
        assert!(
            rendered.contains("observed guest-memory hash:                    4b5e0e70f3050157")
        );
        assert!(!rendered.to_lowercase().contains("basic block"));
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_stats_block_renders_the_typed_wire_snapshot() {
        let rendered = format_dbt_stats(&dbt_stats(42, 7, 6, 0, 0xcbf29ce484222325));
        assert!(rendered.contains("counted branches (deterministic branch clock): 42"));
        assert!(rendered.contains("syscalls intercepted:                          7"));
        assert!(
            rendered.contains("observed guest-memory hash:                    cbf29ce484222325")
        );
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_summary_compares_every_non_branch_observation() {
        let expected = dbt_stats(100, 169, 168, 0, 0x4b5e0e70f3050157);

        assert!(!same_dbt_observable_behavior(
            &expected,
            &dbt_stats(100, 170, 168, 0, 0x4b5e0e70f3050157)
        ));
        assert!(!same_dbt_observable_behavior(
            &expected,
            &dbt_stats(100, 169, 167, 0, 0x4b5e0e70f3050157)
        ));
        assert!(!same_dbt_observable_behavior(
            &expected,
            &dbt_stats(100, 169, 168, 1, 0x4b5e0e70f3050157)
        ));
        assert!(!same_dbt_observable_behavior(
            &expected,
            &dbt_stats(100, 169, 168, 0, 0)
        ));

        let different_branch_clock = dbt_stats(101, 169, 168, 0, 0x4b5e0e70f3050157);
        assert!(
            same_dbt_observable_behavior(&expected, &different_branch_clock),
            "the counted-branch clock has its own typed comparison and must not be folded into \
             the opaque summary mismatch"
        );
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_physical_command_preserves_explicit_environment_removals() {
        let inherited = env::vars_os()
            .map(|(key, _)| key)
            .find(|key| key != "DBT_PHYSICAL_KEPT")
            .expect("native fixture requires an inherited environment entry");
        let environment = BTreeMap::from([(
            OsString::from("DBT_PHYSICAL_KEPT"),
            OsString::from("literal guest value"),
        )]);
        let mut command = StdCommand::new("/usr/bin/env");
        apply_exact_environment(&mut command, &environment);
        command.env_remove("DBT_PHYSICAL_EXPLICIT_REMOVAL");
        let original = command
            .get_envs()
            .map(|(key, value)| (key.to_owned(), value.map(OsStr::to_owned)))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(original.get(&inherited), Some(&None));
        assert_eq!(
            original.get(OsStr::new("DBT_PHYSICAL_EXPLICIT_REMOVAL")),
            Some(&None)
        );
        let prepared = prepare_dbt_physical_command(&command).unwrap();
        let transported = prepared
            .get_envs()
            .map(|(key, value)| (key.to_owned(), value.map(OsStr::to_owned)))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            transported, original,
            "physical command preparation must preserve each removal for DbtRunner"
        );
        assert_eq!(prepared.get_program(), command.get_program());
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_resolves_simple_env_shebang_target_to_absolute_path() {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let python = bin.join("python3");
        write_executable(&python, b"\x7fELFplaceholder");
        let script = root.path().join("guest.py");
        write_executable(&script, b"#!/usr/bin/env python3\n");

        let prepared = prepare_dbt_guest_command(
            &script,
            &[OsString::from("argument")],
            Some(bin.as_os_str()),
            root.path(),
        );

        assert_eq!(prepared.program, Path::new("/usr/bin/env"));
        assert_eq!(
            prepared.args,
            [
                python.into_os_string(),
                script.into_os_string(),
                OsString::from("argument"),
            ]
        );
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_leaves_complex_env_shebang_for_launcher() {
        let root = tempfile::tempdir().unwrap();
        let script = root.path().join("guest.py");
        write_executable(&script, b"#!/usr/bin/env -S python3 -u\n");

        let prepared =
            prepare_dbt_guest_command(&script, &[], Some(OsStr::new("/usr/bin")), root.path());

        assert_eq!(prepared.program, script);
        assert!(prepared.args.is_empty());
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_relative_shebang_uses_guest_cwd_and_preserves_script_arguments() {
        let root = tempfile::Builder::new()
            .prefix("dbt-workdir-")
            .tempdir_in(".")
            .unwrap();
        let relative = PathBuf::from(root.path().file_name().unwrap()).join("guest.py");
        let guest_cwd = fs::canonicalize(root.path()).unwrap().join("guest-cwd");
        let guest_script = guest_cwd.join(&relative);
        fs::create_dir_all(guest_script.parent().unwrap()).unwrap();
        write_executable(&relative, b"#!/usr/bin/env caller-only\n");
        write_executable(&guest_script, b"#!/usr/bin/env guest-only\n");
        let relative_bin = PathBuf::from(root.path().file_name().unwrap()).join("bin");
        let guest_bin = guest_cwd.join(&relative_bin);
        fs::create_dir_all(&relative_bin).unwrap();
        fs::create_dir_all(&guest_bin).unwrap();
        write_executable(&relative_bin.join("caller-only"), b"\x7fELFcaller");
        write_executable(&guest_bin.join("guest-only"), b"\x7fELFguest");
        let mut command = StdCommand::new(&relative);
        command
            .current_dir(&guest_cwd)
            .env_clear()
            .env("PATH", &relative_bin)
            .env("KEPT", "literal value")
            .env_remove("EXPLICIT_REMOVAL")
            .args(["literal argument", ""]);
        let prepared = prepare_dbt_physical_command(&command).unwrap();
        assert_eq!(prepared.get_program(), OsStr::new("/usr/bin/env"));
        assert_eq!(
            prepared.get_args().map(OsStr::to_owned).collect::<Vec<_>>(),
            [
                guest_bin.join("guest-only").into_os_string(),
                relative.into_os_string(),
                OsString::from("literal argument"),
                OsString::new()
            ]
        );
        assert_eq!(prepared.get_current_dir(), Some(guest_cwd.as_path()));
        assert_eq!(
            prepared.get_envs().collect::<Vec<_>>(),
            command.get_envs().collect::<Vec<_>>()
        );
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_path_selected_script_keeps_the_selected_script_path() {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        write_executable(
            &bin.join("guest.py"),
            b"#!/usr/bin/env chosen-interpreter\n",
        );
        let interpreter = bin.join("chosen-interpreter");
        write_executable(&interpreter, b"\x7fELFguest");
        for (cwd, path, script, target) in [
            (
                root.path(),
                OsStr::new("bin"),
                PathBuf::from("bin/guest.py"),
                interpreter.clone(),
            ),
            (
                root.path(),
                bin.as_os_str(),
                bin.join("guest.py"),
                interpreter.clone(),
            ),
            (
                root.path(),
                OsStr::new(":bin"),
                PathBuf::from("bin/guest.py"),
                interpreter.clone(),
            ),
            (
                bin.as_path(),
                OsStr::new(""),
                PathBuf::from("guest.py"),
                interpreter.clone(),
            ),
            (
                bin.as_path(),
                OsStr::new("."),
                PathBuf::from("./guest.py"),
                bin.join("./chosen-interpreter"),
            ),
        ] {
            let mut command = StdCommand::new("guest.py");
            command
                .current_dir(cwd)
                .env_clear()
                .env("PATH", path)
                .args(["", "two words"]);
            let prepared = prepare_dbt_physical_command(&command).unwrap();
            assert_eq!(prepared.get_program(), OsStr::new("/usr/bin/env"));
            assert_eq!(
                prepared.get_args().map(OsStr::to_owned).collect::<Vec<_>>(),
                [
                    target.into_os_string(),
                    script.into_os_string(),
                    OsString::new(),
                    OsString::from("two words"),
                ],
                "PATH {path:?} in {cwd:?} must identify the selected script"
            );
            assert_eq!(prepared.get_current_dir(), command.get_current_dir());
            assert_eq!(
                prepared.get_envs().collect::<Vec<_>>(),
                command.get_envs().collect::<Vec<_>>()
            );
        }
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_empty_path_component_and_hidden_script_use_physical_cwd() {
        let root = tempfile::Builder::new()
            .prefix("dbt-physical-cwd-")
            .tempdir_in(".")
            .unwrap();
        let relative = PathBuf::from(root.path().file_name().unwrap()).join("guest.py");
        let guest_cwd = fs::canonicalize(root.path()).unwrap().join("empty-workdir");
        fs::create_dir(&guest_cwd).unwrap();
        write_executable(&relative, b"#!/usr/bin/env chosen-interpreter\n");
        let caller_bin = root.path().join("bin");
        fs::create_dir(&caller_bin).unwrap();
        write_executable(&caller_bin.join("chosen-interpreter"), b"\x7fELFcaller");
        let mut command = StdCommand::new(&relative);
        command
            .current_dir(&guest_cwd)
            .env_clear()
            .env("PATH", &caller_bin)
            .arg("argument");
        // The caller can see this relative script; the physical guest cannot.
        let absent = prepare_dbt_physical_command(&command).unwrap();
        assert_eq!(absent.get_program(), relative.as_os_str());
        assert_eq!(
            absent.get_args().collect::<Vec<_>>(),
            [OsStr::new("argument")]
        );
        // An empty PATH component denotes the physical guest's cwd.
        let script = guest_cwd.join("script");
        write_executable(&script, b"#!/usr/bin/env chosen-interpreter\n");
        let interpreter = guest_cwd.join("chosen-interpreter");
        write_executable(&interpreter, b"\x7fELFguest");
        let mut command = StdCommand::new(&script);
        command.current_dir(&guest_cwd).env_clear().env("PATH", "");
        let prepared = prepare_dbt_physical_command(&command).unwrap();
        assert_eq!(prepared.get_program(), OsStr::new("/usr/bin/env"));
        assert_eq!(
            prepared.get_args().collect::<Vec<_>>(),
            [interpreter.as_os_str(), script.as_os_str()]
        );
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

    /// A guest killed by a signal must report as SIGNALLED, not as a normal exit.
    ///
    /// ⚠️ THIS IS THE TEST THAT WAS MISSING, WHICH IS WHY ONE SITE GOT FIXED AND
    /// ITS SIBLING DID NOT. `process_status` was converted to `from_raw`;
    /// `output_status` kept `code().unwrap_or(1)` and nothing noticed, because no
    /// test asserted the signalled case for either. Asserting it here binds BOTH.
    #[test]
    fn a_signalled_guest_is_not_reported_as_a_normal_exit() {
        use std::os::unix::process::ExitStatusExt as _;

        // raw wait status for "killed by SIGSEGV" (11), no core dump.
        let raw = 11i32;
        let native = std::process::ExitStatus::from_raw(raw);
        assert!(native.code().is_none(), "SIGSEGV death has no exit code");

        let converted = process_status(native);
        assert!(
            !matches!(converted, ExitStatus::Exited(_)),
            "a signalled guest must not read as a normal exit, got {converted:?}"
        );

        // ⚠️ CALL `output_status` ITSELF, not an expression that looks like it.
        // My first version compared `process_status(..)` against an inline
        // `ExitStatus::from_raw(..)` and never invoked `output_status` at all --
        // so reverting `output_status` to the buggy form left this test GREEN.
        // Caught by mutating BOTH sites instead of one.
        let output = Output {
            status: native,
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        let from_output = output_status(&output);
        assert!(
            !matches!(from_output, ExitStatus::Exited(_)),
            "output_status must not report a signalled guest as a normal exit, got {from_output:?}"
        );
        assert_eq!(
            converted, from_output,
            "output_status and process_status must agree; they diverged once and \
             only one of them was fixed"
        );
    }

    /// A real spawn failure -- drrun absent or not executable -- must still say
    /// so, because that is when the operator SHOULD go and look at the binary.
    #[cfg(feature = "dbt")]
    #[test]
    fn a_spawn_failure_still_names_the_launch() {
        let drrun = Path::new("/nonexistent/dynamorio/bin64/drrun");
        for kind in [
            std::io::ErrorKind::NotFound,
            std::io::ErrorKind::PermissionDenied,
        ] {
            let rendered = dbt_run_error(drrun, std::io::Error::new(kind, "boom")).to_string();
            assert!(
                rendered.contains("failed to launch drrun"),
                "a {kind:?} spawn failure must be reported as a launch failure: {rendered}"
            );
        }
    }

    /// The regression this function exists for. A post-launch failure -- the
    /// evidence-finalization error is the real-world one -- must NOT be
    /// reported as a launch failure. It sent a campaign agent hunting a 737 KB
    /// drrun that was correct and working, and the inner text proves the guest
    /// had already exited.
    #[cfg(feature = "dbt")]
    #[test]
    fn a_post_launch_failure_does_not_blame_the_binary() {
        let drrun = Path::new("/real/dynamorio/bin64/drrun");
        let inner = "DBT guest exited with status Some(1) while protected evidence failed: \
                     DBT evidence collector thread panicked";
        let rendered = dbt_run_error(drrun, std::io::Error::other(inner)).to_string();
        assert!(
            !rendered.contains("failed to launch drrun"),
            "a post-launch failure must not be reported as a launch failure: {rendered}"
        );
        assert!(
            rendered.contains("drrun started"),
            "the message must say the launch succeeded: {rendered}"
        );
        assert!(
            rendered.contains("NOT a launch failure"),
            "the message must rule the binary out explicitly: {rendered}"
        );
        // The cause must survive verbatim. Truncating it here is what cost the
        // original investigation its answer.
        assert!(
            rendered.contains("protected evidence failed"),
            "the underlying error must be preserved in full: {rendered}"
        );
    }

    /// A signalled death must not be reported as a normal exit.
    ///
    /// Ported from hermit#1689's fifth commit as the one free-standing piece of
    /// that head: the behaviour it brackets (`ExitStatus::from_raw` preserving
    /// exited-versus-signalled) is ALREADY on main -- that head's claim 2 landed
    /// via `b441950f72` -- while its bracket never did. Landed code with no
    /// coverage, which is the pairing worth closing first.
    #[test]
    fn dbt_status_preserves_normal_exit_codes() {
        for code in [0, 1, 42, 255] {
            let raw = std::process::ExitStatus::from_raw(code << 8);
            assert_eq!(
                process_status(raw),
                ExitStatus::Exited(code),
                "a normal exit with code {code} must round-trip"
            );
        }
    }

    #[test]
    fn dbt_status_preserves_death_by_signal() {
        for (signum, signal) in [
            (libc::SIGABRT, reverie::Signal::SIGABRT),
            (libc::SIGSEGV, reverie::Signal::SIGSEGV),
            (libc::SIGFPE, reverie::Signal::SIGFPE),
            (libc::SIGILL, reverie::Signal::SIGILL),
            (libc::SIGTERM, reverie::Signal::SIGTERM),
            (libc::SIGKILL, reverie::Signal::SIGKILL),
            (libc::SIGTRAP, reverie::Signal::SIGTRAP),
        ] {
            // without a core dump
            let raw = std::process::ExitStatus::from_raw(signum);
            assert_eq!(
                process_status(raw),
                ExitStatus::Signaled(signal, false),
                "death by signal {signum} must be reported as Signaled, not Exited"
            );
            // with the core-dump flag set (bit 0x80 of the wait status)
            let raw = std::process::ExitStatus::from_raw(signum | 0x80);
            assert_eq!(
                process_status(raw),
                ExitStatus::Signaled(signal, true),
                "the core-dump flag for signal {signum} must survive the conversion"
            );
        }
    }

    /// Pins main's CURRENT DBT `--log-file` policy, which was undefended.
    ///
    /// hermit#1689 claim 1 would reverse this. That reversal may well be correct --
    /// it is an owner ruling, and this test takes no side on it. What it removes is
    /// the possibility of reversing it SILENTLY: before this, the refusal existed
    /// only as an error string with no assertion, so flipping the condition changed
    /// behaviour without changing any test.
    #[test]
    fn dbt_log_file_is_refused_on_the_single_run_adapter_and_permitted_under_verify() {
        // the refusal: a log file was asked for, and this is not a verification run
        assert!(
            dbt_log_file_is_refused(true, false),
            "a requested --log-file must be refused on the ordinary single-run adapter"
        );
        // --verify owns its own sink, so the request is honoured there
        assert!(
            !dbt_log_file_is_refused(true, true),
            "--verify must keep accepting --log-file; the verification adapter owns the sink"
        );
        // no request, nothing to refuse, either way
        assert!(!dbt_log_file_is_refused(false, false));
        assert!(!dbt_log_file_is_refused(false, true));
    }

    /// The refusal text is what a caller greps for, so pin it exactly.
    #[test]
    fn dbt_log_file_refusal_names_the_adapter_it_is_about() {
        assert_eq!(
            DBT_LOG_FILE_REFUSAL,
            "DBT --log-file is unavailable on the ordinary single-run adapter"
        );
    }
}
