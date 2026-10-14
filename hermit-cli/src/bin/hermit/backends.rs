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

#[cfg(feature = "dbt")]
use std::collections::BTreeMap;
#[cfg(feature = "dbt")]
use std::collections::BTreeSet;
#[cfg(feature = "dbt")]
use std::env;
#[cfg(feature = "dbt")]
use std::ffi::OsStr;
#[cfg(any(feature = "sabre", feature = "dbt"))]
use std::ffi::OsString;
#[cfg(any(feature = "sabre", feature = "dbt"))]
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
#[cfg(any(feature = "sabre", feature = "dbt"))]
use std::os::unix::fs::PermissionsExt;
#[cfg(any(feature = "dbt", test))]
use std::os::unix::process::ExitStatusExt as _;
use std::path::Path;
#[cfg(feature = "dbt")]
use std::path::PathBuf;
#[cfg(any(feature = "sabre", feature = "dbt"))]
use std::process::Command as StdCommand;
#[cfg(any(feature = "dbt", test))]
use std::process::Output;

#[cfg(feature = "dbt")]
use detcore::Config;
#[cfg(feature = "dbt")]
use detcore_model::backend_engagement::BackendEngagement;
#[cfg(feature = "dbt")]
use detcore_model::backend_engagement::BackendEngagementReport;
use hermit::Error;
use hermit::ExitStatus;
#[cfg(feature = "dbt")]
use reverie_dbt::DbtEvidence;
#[cfg(feature = "dbt")]
use reverie_dbt::DbtEvidenceLogLevel;
#[cfg(feature = "dbt")]
use reverie_dbt::DbtRunner;
#[cfg(feature = "dbt")]
use reverie_dbt::backend_stats::DbtBackendStatsAggregator;
#[cfg(feature = "dbt")]
use reverie_dbt::backend_stats::DbtBackendStatsSnapshot;
#[cfg(any(feature = "sabre", feature = "dbt"))]
use tracing::metadata::LevelFilter;

#[cfg(feature = "dbt")]
use super::record_envelope::RecordEnvelope;
#[cfg(feature = "dbt")]
use super::verify::ComparedRun;
#[cfg(feature = "dbt")]
use super::verify::ComparisonOptions;
#[cfg(feature = "dbt")]
use super::verify::DbtCountedBranchComparison;
#[cfg(feature = "dbt")]
use super::verify::LogCompareStrictness;
#[cfg(feature = "dbt")]
use super::verify::SecondRun;
#[cfg(feature = "dbt")]
use super::verify::Verdict;
#[cfg(feature = "dbt")]
use super::verify::VerificationOutcome;
#[cfg(feature = "dbt")]
use super::verify::VerificationRun;
#[cfg(feature = "dbt")]
use super::verify::announce_verification_outcome;
#[cfg(feature = "dbt")]
use super::verify::compare_two_runs;
#[cfg(feature = "dbt")]
use super::verify::record_terminal_deadlock;
#[cfg(feature = "dbt")]
use super::verify::verification_log_level;
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

/// Compare the authenticated number of DBT process images as a typed dimension.
///
/// Each process-image initialization record is also compared in the log, at its
/// stream position, so a differing count diverges there too. This check names
/// that cause directly instead of leaving it to be inferred from a record diff.
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

/// Own the typed statistics stream for one complete DBT process tree.
///
/// Reverie's client writes one fixed-size record per process image at exit.
/// The protected-evidence runner does not expose its convenience stats method,
/// so this adapter uses the same public typed wire decoder and waits until the
/// runner has reaped the whole isolated process group before reading it.
#[cfg(feature = "dbt")]
struct DbtStatsCapture {
    directory: tempfile::TempDir,
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
        Ok(Self { directory, path })
    }

    fn configure(&self, runner: DbtRunner) -> DbtRunner {
        runner
            .client_argument("-stats_path")
            .client_argument(self.path.clone().into_os_string())
    }

    fn finish(self) -> Result<DbtBackendStatsSnapshot, Error> {
        read_dbt_stats(&self.path)
    }

    /// The typed whole-process statistics stream this capture configures.
    fn records(&self) -> &Path {
        &self.path
    }
}

/// Decode a typed whole-process statistics stream that a DBT run wrote to
/// `path` (`DbtStatsCapture`).
#[cfg(feature = "dbt")]
pub(super) fn read_dbt_stats(path: &Path) -> Result<DbtBackendStatsSnapshot, Error> {
    {
        let bytes = match fs::read(path) {
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

/// The exit code with which the DBT native client ends a process on a backend
/// or Tool failure (`exit_runtime_tree(101)` in reverie-dbt's client).
#[cfg(feature = "dbt")]
const DBT_BACKEND_FAILURE_EXIT_CODE: i32 = 101;

/// Prints and records a single run's statistics as requested, and returns them
/// when they could be read. A run always captures them: they are also how a
/// backend failure in a guest child is found (see [`fail_on_dbt_backend_failure`]).
#[cfg(feature = "dbt")]
fn finish_single_run_dbt_stats(
    capture: DbtStatsCapture,
    summary: bool,
    engagement_json: Option<&Path>,
) -> Result<Option<DbtBackendStatsSnapshot>, Error> {
    match capture.finish() {
        Ok(stats) => {
            if summary {
                eprint!("{}", format_dbt_stats(&stats));
            }
            if let Some(path) = engagement_json {
                write_dbt_engagement(path, &stats)?;
            }
            Ok(Some(stats))
        }
        Err(error) if engagement_json.is_some() => Err(error),
        Err(error) => {
            if summary {
                eprintln!(":: DBT summary unavailable: {error}");
            }
            eprintln!(
                ":: WARNING: the DBT process records are unavailable, so a backend failure in a \
                 guest child cannot be ruled out: {error}"
            );
            Ok(None)
        }
    }
}

/// `status`, or a failed status when any process image of the run ended on a
/// backend or Tool failure.
///
/// The native client ends such a process with exit code 101. When the guest
/// tree has no isolated process group (`--allow-unsupported-syscalls` without
/// `--verify`), that ends only the failing process, and a parent that ignores
/// its child's status can still finish the run with any status, 0 included.
/// The failing image flags its stats record, so the run fails here instead.
#[cfg(feature = "dbt")]
fn fail_on_dbt_backend_failure(
    status: ExitStatus,
    stats: Option<&DbtBackendStatsSnapshot>,
) -> ExitStatus {
    let failures = stats.map_or(0, DbtBackendStatsSnapshot::backend_failures);
    if failures == 0 || status == ExitStatus::Exited(DBT_BACKEND_FAILURE_EXIT_CODE) {
        return status;
    }
    eprintln!(
        "hermit: [dbt backend] {failures} guest process(es) ended on a DBT backend or Tool \
         failure (exit {DBT_BACKEND_FAILURE_EXIT_CODE}), but the guest tree finished with \
         {status:?}; failing the run with exit {DBT_BACKEND_FAILURE_EXIT_CODE}. The failing \
         process's diagnostic is above."
    );
    ExitStatus::Exited(DBT_BACKEND_FAILURE_EXIT_CODE)
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
fn decode_dbt_evidence(file: &mut std::fs::File) -> Result<DbtEvidence, Error> {
    file.seek(SeekFrom::Start(0))?;
    let mut encoded = Vec::new();
    file.read_to_end(&mut encoded)?;
    if encoded.is_empty() {
        return Err(Error::msg("DBT canonical evidence was empty"));
    }
    reverie_dbt::decode_evidence(&encoded).map_err(|error| {
        Error::msg(format!(
            "DBT canonical evidence was malformed or truncated: {error}"
        ))
    })
}

#[cfg(feature = "dbt")]
/// The latches of one DBT run. Detcore runs in the guest's DynamoRIO client,
/// so its determinism-loss latch is read from the run's evidence: the client
/// writes a record with target [`detcore::detlog::DETERMINISM_LOSS_RECORD_TARGET`]
/// at process exit when its latch is set. The reason is the record's text after
/// the target, escaped as every evidence record is.
fn dbt_run_latches(evidence: &DbtEvidence) -> super::run::RunLatches {
    let marker = format!(
        " ERROR {}: ",
        detcore::detlog::DETERMINISM_LOSS_RECORD_TARGET
    );
    let determinism_loss = evidence.all_records().iter().find_map(|record| {
        let text = String::from_utf8_lossy(record);
        text.split_once(&marker)
            .map(|(_, reason)| reason.trim_end_matches('\n').to_owned())
    });
    super::run::RunLatches {
        determinism_loss,
        log_sink_failure: None,
    }
}

/// Write the comparison log for one DBT run from its decoded evidence.
///
/// The log holds `DbtEvidence::all_records()`: every authenticated record in
/// stream order, with each process-image initialization record where the
/// Reverie collector placed it, immediately before that image's first comparable
/// record (or at the end of the stream if the image wrote none). That is exactly
/// the stream the `all_records_v1` envelope
/// compares and counts. The refusal guards count `DbtEvidence::records()`
/// instead, as described on `materialize_dbt_comparison_log`.
#[cfg(feature = "dbt")]
fn materialize_dbt_evidence_log(
    evidence: &DbtEvidence,
    log: std::fs::File,
    path: &Path,
) -> Result<usize, Error> {
    let published = evidence.all_records();
    materialize_dbt_comparison_log(evidence.records(), &published, log, path)
}

/// Refuse evidence without guest records, then write `published` to the log.
///
/// `guest_records` are the decoded records other than initialization records,
/// and `published` is the complete stream that is compared. Every stream holds
/// at least one initialization record, which is an INFO record, so a guard over
/// `published` could never fire: a run whose guest produced no evidence would
/// reach the verdict with nonzero compared INFO counts. That is the gap
/// https://github.com/rrnewton/hermit/pull/2356 closed, so both guards here
/// count `guest_records`. Returns the number of INFO records in `published`.
#[cfg(feature = "dbt")]
fn materialize_dbt_comparison_log(
    guest_records: &[Vec<u8>],
    published: &[&[u8]],
    mut log: std::fs::File,
    path: &Path,
) -> Result<usize, Error> {
    if guest_records.is_empty() {
        return Err(Error::msg("DBT canonical evidence contained no records"));
    }
    // Detcore counts INFO records by parsing a log file, so the guest records
    // are written and counted on their own first. The published stream then
    // replaces them and is counted by the same parser.
    let mut write_and_count = |records: &[&[u8]]| -> Result<usize, Error> {
        write_dbt_comparison_records(&mut log, path, records)?;
        detcore::logdiff::write_canonical_info(path, &mut std::io::sink()).map_err(|error| {
            Error::msg(format!(
                "DBT canonical evidence did not contain a valid log stream: {error}"
            ))
        })
    };
    let guest_records: Vec<&[u8]> = guest_records.iter().map(Vec::as_slice).collect();
    if write_and_count(guest_records.as_slice())? == 0 {
        return Err(Error::msg(
            "DBT canonical evidence contained no INFO records",
        ));
    }
    write_and_count(published)
}

/// Replace the contents of the DBT comparison log with `records`, one record
/// per line, and check that none was lost or split on the way.
#[cfg(feature = "dbt")]
fn write_dbt_comparison_records(
    log: &mut std::fs::File,
    path: &Path,
    records: &[&[u8]],
) -> Result<(), Error> {
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

    // The verdict publishes `all_records_v1`, so nothing between Reverie's
    // authenticated decoder and this log may select records: every record,
    // initialization records included, must reach the log in stream order.
    // Filtering here would be an undisclosed second selection. Each record was
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
             comparison publishes the all_records_v1 envelope and must not drop any",
            records.len()
        )));
    }
    Ok(())
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
pub(super) fn dbt_log_file_is_refused(log_file_requested: bool, verify: bool) -> bool {
    log_file_requested && !verify
}

/// The prepared physical DBT invocation, shared by ordinary runs and the
/// common verifier. Each execution still owns its container and coordinator.
#[cfg(feature = "dbt")]
pub(super) struct DbtInvocation {
    execution: DbtExecution,
    runner: DbtRunner,
    guest: StdCommand,
    drrun: PathBuf,
    config: Config,
    _unsupported_report: DbtUnsupportedSyscallReport,
}

#[cfg(feature = "dbt")]
impl DbtInvocation {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        program: &Path,
        args: &[String],
        config: &Config,
        mut environment: BTreeMap<OsString, OsString>,
        workdir: Option<&Path>,
        binds: Vec<(PathBuf, PathBuf)>,
        options: &super::run::RunOpts,
    ) -> Result<Self, Error> {
        // The DBT backend drives a single Detcore external scheduler, so it cannot
        // honor a request to relax thread sequentialization. Fail loudly rather
        // than silently ignoring the flag.
        if !config.sequentialize_threads {
            return Err(Error::msg(
                "the dbt backend requires sequentialized threads; \
             remove --no-sequentialize-threads (or --strace-only) to run under --backend dbt",
            ));
        }
        // The guest sees this string in its environment, so it keeps the encoding
        // it had before the backend facts moved into `Config::backend` and before
        // three constant switches were deleted; see `hermit::dbt_detconfig_json`.
        // The DBT runtime decodes the backend facts back out of it (here DBT's
        // own); when the string is absent or does not parse, it uses the strict
        // default's facts, which are ptrace's.
        let config_json = hermit::dbt_detconfig_json(config).map_err(|error| {
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
        let binds = binds
            .into_iter()
            .map(|(source, target)| hermit_test_workdir::BindMount { source, target })
            .collect::<Vec<_>>();
        // A bound workdir is as fresh as the bind source the caller made for this
        // run, which is how the other backends get theirs.
        let workdir_is_bound = workdir
            .is_some_and(|workdir| binds.iter().any(|bind| workdir.starts_with(&bind.target)));
        if isolated_workdir.is_some() && workdir != isolated_workdir && !workdir_is_bound {
            return Err(Error::msg(
                "HERMIT_E2E_EMPTY_WORKDIR=/test requires --workdir=/test, or a --workdir inside a \
             --bind target, for DBT",
            ));
        }
        // The container applies the binds; keep the existing target admission.
        for bind in &binds {
            hermit_test_workdir::check_bind_target(&bind.target)?;
        }
        let execution = DbtExecution::new(
            options,
            isolated_workdir.is_some() && workdir == isolated_workdir,
        );

        let (drrun, client) = detcore_dbt::prepare_native_client().map_err(|error| {
            Error::msg(format!(
                "failed to prepare the Detcore DynamoRIO client: {error}"
            ))
        })?;
        let mut runner = DbtRunner::new(&drrun, &client)
            .map_err(|error| {
                Error::msg(format!(
                    "failed to configure the DynamoRIO DBT runner (drrun={}, client={}): {error}",
                    drrun.display(),
                    client.display()
                ))
            })?
            .summary(options.summary)
            .isolated_process_group(panic_on_unsupported_syscalls);
        if panic_on_unsupported_syscalls {
            runner = runner.client_argument("-panic-on-unsupported-syscalls");
        }

        eprintln!(
            "hermit: [dbt backend] Detcore Tool active; running {program:?} under DynamoRIO ({})",
            drrun.display()
        );

        let unsupported_report = DbtUnsupportedSyscallReport::new()?;
        let mut guest = StdCommand::new(program);
        environment.remove(OsStr::new("HERMIT_LOG_FILE"));
        environment.insert(detcore_dbt::DETCONFIG_ENV.into(), config_json.into());
        apply_exact_environment(&mut guest, &environment);
        guest.args(args);
        apply_dbt_workdir(&mut guest, workdir);

        Ok(Self {
            execution,
            runner,
            guest,
            drrun,
            config: config.clone(),
            _unsupported_report: unsupported_report,
        })
    }

    pub(super) fn run(
        mut self,
        summary: bool,
        engagement_json: Option<&Path>,
        log: Option<LevelFilter>,
        log_file: Option<&Path>,
    ) -> Result<ExitStatus, Error> {
        if dbt_log_file_is_refused(log_file.is_some(), false) {
            return Err(Error::msg(DBT_LOG_FILE_REFUSAL));
        }
        if let Some(level) = log {
            self.guest.env("HERMIT_LOG", level.to_string());
        }
        let stats = match self.execution.stats_capture() {
            Ok(capture) => Some(capture),
            Err(error) if summary || engagement_json.is_some() => return Err(error),
            Err(error) => {
                eprintln!(
                    ":: WARNING: the DBT process records cannot be set up, so a backend failure in a guest child cannot be ruled out: {error}"
                );
                None
            }
        };
        if let Some(capture) = &stats {
            self.runner = capture.configure(self.runner);
        }
        let status = if std::io::stdin().is_terminal() {
            process_status(run_status(
                &self.execution,
                &self.runner,
                &self.guest,
                &self.drrun,
                &self.config,
            )?)
        } else {
            let output = run_once(
                &self.execution,
                &self.runner,
                &self.guest,
                &self.drrun,
                &self.config,
                None,
                std::io::stdin(),
            )?;
            write_output(&output)?;
            output_status(&output)
        };
        let stats = stats
            .map(|capture| finish_single_run_dbt_stats(capture, summary, engagement_json))
            .transpose()?
            .flatten();
        Ok(fail_on_dbt_backend_failure(status, stats.as_ref()))
    }

    pub(super) fn verify_run(
        self,
        directory: &Path,
        log: Option<LevelFilter>,
        verbose: bool,
        stdin: Option<std::fs::File>,
        options: &super::run::RunOpts,
    ) -> Result<ExitStatus, Error> {
        dbt_verify_run(
            directory,
            &self.execution,
            self.runner,
            &self.guest,
            &self.drrun,
            &self.config,
            dbt_evidence_log_level(log, verbose),
            verbose,
            stdin,
            options,
        )
    }

    pub(super) fn verification(
        self,
        stdin: Option<std::fs::File>,
    ) -> Result<DbtVerification, Error> {
        let terminal = stdin
            .as_ref()
            .is_some_and(|input| unsafe { libc::isatty(input.as_raw_fd()) } == 1);
        Ok(DbtVerification {
            invocation: self,
            stdin,
            terminal,
            replay: tempfile::tempfile()?,
        })
    }
}

/// Input replay belongs to one common verify invocation. Reading begins inside
/// run 1; an open, idle producer never has to reach EOF before the guest starts.
#[cfg(feature = "dbt")]
pub(super) struct DbtVerification {
    invocation: DbtInvocation,
    stdin: Option<std::fs::File>,
    terminal: bool,
    replay: std::fs::File,
}

#[cfg(feature = "dbt")]
impl DbtVerification {
    pub(super) fn run(
        &mut self,
        log: std::fs::File,
        log_path: &Path,
        run: VerificationRun,
        requested_log: Option<LevelFilter>,
        verbose: bool,
        verify_json: Option<&Path>,
    ) -> Result<super::run::VerificationRunOutput, Error> {
        let invocation = &self.invocation;
        let stats = invocation.execution.stats_capture()?;
        let runner = stats.configure(invocation.runner.clone());
        let mut evidence_file = tempfile::tempfile()?;
        let evidence = Some(DbtRunEvidence {
            file: &evidence_file,
            log_level: dbt_evidence_log_level(requested_log, verbose),
            label: match run {
                VerificationRun::Run1 => "run-1",
                VerificationRun::Run2 => "run-2",
            },
        });
        let deadlock_recording = record_terminal_deadlock(verify_json, run);
        let output = if self.terminal {
            run_once_with_terminal_input(
                &invocation.execution,
                &runner,
                &invocation.guest,
                &invocation.drrun,
                &invocation.config,
                evidence,
            )
        } else {
            let input: Box<dyn Read + Send> = match run {
                VerificationRun::Run1 => match self.stdin.take() {
                    Some(input) => Box::new(TeeReader {
                        input,
                        replay: self.replay.try_clone()?,
                    }),
                    None => Box::new(std::io::empty()),
                },
                VerificationRun::Run2 => {
                    self.replay.seek(SeekFrom::Start(0))?;
                    Box::new(self.replay.try_clone()?)
                }
            };
            run_once(
                &invocation.execution,
                &runner,
                &invocation.guest,
                &invocation.drrun,
                &invocation.config,
                evidence,
                input,
            )
        };
        drop(deadlock_recording);
        let output = output?;
        // These failures occurred after a physical run, unlike preparation
        // errors. Keep the cause and let the common owner publish its run label.
        let evidence = decode_dbt_evidence(&mut evidence_file)
            .map_err(|error| error.context(hermit::BackendRunFailure))?;
        materialize_dbt_evidence_log(&evidence, log, log_path)
            .map_err(|error| error.context(hermit::BackendRunFailure))?;
        Ok(super::run::VerificationRunOutput {
            output: dbt_verification_output(output),
            skid_overshoots: 0,
            latches: dbt_run_latches(&evidence),
            dbt: Some(DbtRunCompletion {
                stats,
                initialization_records: evidence.initialization_records(),
                terminal_stdin: self.terminal,
            }),
        })
    }
}

/// Stats are consumed only after latches and first-run exit policy, preserving
/// their diagnostic precedence even when the stats artifact is missing.
#[cfg(feature = "dbt")]
pub(super) struct DbtRunCompletion {
    stats: DbtStatsCapture,
    initialization_records: usize,
    terminal_stdin: bool,
}

#[cfg(feature = "dbt")]
impl DbtRunCompletion {
    pub(super) fn finish(
        self,
        output: &reverie::process::Output,
        run: VerificationRun,
    ) -> Result<DbtRunFacts, Error> {
        let stats = self
            .stats
            .finish()
            .map_err(|error| error.context(hermit::BackendRunFailure))?;
        if run == VerificationRun::Run1 && self.terminal_stdin && stats.stdin_reads() != 0 {
            std::io::stdout().write_all(&output.stdout)?;
            std::io::stderr().write_all(&output.stderr)?;
            return Err(Error::msg(format!(
                "DBT verification cannot replay terminal stdin: guest attempted {} fd-0 read syscall(s)",
                stats.stdin_reads()
            )));
        }
        Ok(DbtRunFacts {
            stats,
            initialization_records: self.initialization_records,
        })
    }
}

#[cfg(feature = "dbt")]
pub(super) struct DbtRunFacts {
    stats: DbtBackendStatsSnapshot,
    initialization_records: usize,
}

/// Backend facts supplement the common comparison, never replace it. Decide
/// retention before the comparator consumes logs; apply facts before reporting.
#[cfg(feature = "dbt")]
pub(super) struct DbtVerificationComparison {
    branches: DbtCountedBranchComparison,
    summary: DbtSummaryComparison,
}

#[cfg(feature = "dbt")]
impl DbtVerificationComparison {
    pub(super) fn new(first: &DbtRunFacts, second: &DbtRunFacts) -> Self {
        Self {
            branches: DbtCountedBranchComparison {
                left: first.stats.counted_branches(),
                right: second.stats.counted_branches(),
            },
            summary: DbtSummaryComparison::compare(
                &first.stats,
                &second.stats,
                first.initialization_records,
                second.initialization_records,
            ),
        }
    }

    pub(super) fn configure(&self, options: &mut ComparisonOptions) {
        // DBT already compared canonically even for plain --verify. Joining
        // the common owner must not lower that existing observation contract.
        options.strictness = LogCompareStrictness::Canonical;
        let mismatch = !self.branches.matched() || self.summary.requires_log_retention();
        options.keep_logs |= mismatch;
        options.match_overridden |= mismatch;
    }

    pub(super) fn apply(self, outcome: &mut VerificationOutcome) -> Vec<String> {
        let summary = self.summary.apply(outcome);
        let branches = record_dbt_branch_clock_comparison(outcome, self.branches);
        branches.into_iter().chain(summary).collect()
    }
}

/// `run --verify-run DIR` on DBT: one physical verification run,
/// run through the same adapter as live verification, with what its comparison reads written to DIR
/// (`verify_run`): the log materialized from the run's evidence, the guest's
/// output and status, the typed whole-process statistics, and the count of
/// initialization records. `hermit log-diff --verify-json` compares two such
/// directories with [`conclude_dbt_verification`].
#[allow(clippy::too_many_arguments)]
#[cfg(feature = "dbt")]
fn dbt_verify_run(
    directory: &Path,
    execution: &DbtExecution,
    runner: DbtRunner,
    guest: &StdCommand,
    drrun: &Path,
    config: &Config,
    evidence_level: DbtEvidenceLogLevel,
    verify_verbose: bool,
    verification_stdin: Option<std::fs::File>,
    options: &super::run::RunOpts,
) -> Result<ExitStatus, Error> {
    use super::verify_run;

    let writer = verify_run::VerifyRunWriter::create(directory)?;
    let log_path = writer.path(verify_run::LOG);
    let log_file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&log_path)
        .map_err(|error| {
            Error::msg(format!(
                "creating the verification log {}: {error}",
                log_path.display()
            ))
        })?;
    let comparison = verify_run::RecordedComparison {
        strictness: LogCompareStrictness::Canonical,
        diagnostic_full_trace: verify_verbose,
        compare_io_buffers: config.detlog_io_buffers,
        virtualize_time: config.virtualize_time,
    };
    let mut evidence = tempfile::tempfile()?;
    let stats = execution.stats_capture()?;
    let runner = stats.configure(runner);
    let run_evidence = Some(DbtRunEvidence {
        file: &evidence,
        log_level: evidence_level,
        label: "run",
    });
    let terminal_stdin = verification_stdin.as_ref().is_some_and(|file| {
        // SAFETY: `as_raw_fd` borrows a live descriptor for this check.
        (unsafe { libc::isatty(file.as_raw_fd()) }) == 1
    });

    eprintln!(":: DBT run...");
    let raw = if terminal_stdin {
        run_once_with_terminal_input(execution, &runner, guest, drrun, config, run_evidence)
    } else {
        match verification_stdin {
            Some(input) => run_once(
                execution,
                &runner,
                guest,
                drrun,
                config,
                run_evidence,
                input,
            ),
            None => run_once(
                execution,
                &runner,
                guest,
                drrun,
                config,
                run_evidence,
                std::io::empty(),
            ),
        }
    };
    // Each step in the order live verification takes for each of its runs, so the
    // comparison meets a failure where `run --verify` meets it.
    let completed = raw.and_then(|raw| {
        let evidence = decode_dbt_evidence(&mut evidence)
            .map_err(|error| error.context(hermit::BackendRunFailure))?;
        materialize_dbt_evidence_log(&evidence, log_file, &log_path)
            .map_err(|error| error.context(hermit::BackendRunFailure))?;
        dbt_run_latches(&evidence).refuse_comparison("run")?;
        let snapshot = read_dbt_stats(stats.records())
            .map_err(|error| error.context(hermit::BackendRunFailure))?;
        if terminal_stdin && snapshot.stdin_reads() != 0 {
            return Err(Error::msg(format!(
                "DBT verification cannot replay terminal stdin: guest attempted {} fd-0 read \
                 syscall(s)",
                snapshot.stdin_reads()
            )));
        }
        let records = fs::read(stats.records()).map_err(|error| {
            Error::msg(format!(
                "reading the DBT whole-process statistics stream: {error}"
            ))
            .context(hermit::BackendRunFailure)
        })?;
        Ok((dbt_verification_output(raw), evidence, records))
    });
    let (outcome, dbt, result) = match completed {
        Ok((output, evidence, records)) => {
            writer.write(verify_run::STDOUT, &output.stdout)?;
            writer.write(verify_run::STDERR, &output.stderr)?;
            writer.write(verify_run::DBT_STATS, &records)?;
            (
                verify_run::VerifyRunOutcome::Completed {
                    status: verify_run::RecordedStatus::of(output.status),
                    skid_overshoots: 0,
                    forwarded_syscalls: None,
                },
                Some(verify_run::DbtRunRecord {
                    initialization_records: evidence.initialization_records() as u64,
                }),
                Ok(output.status),
            )
        }
        Err(error) => (super::run::verify_run_outcome_of(&error), None, Err(error)),
    };
    writer.commit(&options.verify_run_record(comparison, None, outcome, dbt))?;
    result
}

/// One completed DBT verification run: what [`conclude_dbt_verification`]
/// compares.
#[cfg(feature = "dbt")]
pub(super) struct DbtCompletedRun<'a> {
    pub(super) output: &'a reverie::process::Output,
    /// The run's comparison log (`materialize_dbt_evidence_log`).
    pub(super) log: tempfile::TempPath,
    pub(super) stats: DbtBackendStatsSnapshot,
    pub(super) initialization_records: usize,
}

/// How a DBT verification is concluded once both runs completed.
#[cfg(feature = "dbt")]
pub(super) struct DbtConclusion<'a> {
    pub(super) verify_json: Option<&'a Path>,
    pub(super) verify_verbose: bool,
    pub(super) compare_io_buffers: bool,
    pub(super) virtualize_time: bool,
    pub(super) keep_logs: bool,
    /// The logs are links to the caller's (`ComparisonOptions::caller_owns_logs`).
    pub(super) caller_owns_logs: bool,
}

/// Compare two completed DBT verification runs: the common comparison, then
/// the counted-branch clocks and the typed native statistics, publish the
/// report, and announce it. `hermit log-diff --verify-json` reaches this
/// offline conclusion after reading two `--verify-run` directories. Live
/// verification uses the same typed facts in the common two-run owner.
#[cfg(feature = "dbt")]
pub(super) fn conclude_dbt_verification(
    first: DbtCompletedRun<'_>,
    second: DbtCompletedRun<'_>,
    conclusion: DbtConclusion<'_>,
) -> Result<VerificationOutcome, Error> {
    let DbtConclusion {
        verify_json,
        verify_verbose,
        compare_io_buffers,
        virtualize_time,
        keep_logs,
        caller_owns_logs,
    } = conclusion;
    let comparison = DbtVerificationComparison::new(
        &DbtRunFacts {
            stats: first.stats,
            initialization_records: first.initialization_records,
        },
        &DbtRunFacts {
            stats: second.stats,
            initialization_records: second.initialization_records,
        },
    );
    let mut options = ComparisonOptions {
        verbose: verify_verbose,
        strictness: LogCompareStrictness::Canonical,
        compare_logs: true,
        diagnostic_full_trace: verify_verbose,
        compare_io_buffers,
        virtualize_time,
        keep_logs,
        match_overridden: false,
        failed_log_retention: (!keep_logs).then(super::verify::default_failed_verify_log_retention),
        // Every process-image initialization remains at its observed stream
        // position, and typed initialization counts also enter the facts below.
        record_envelope: RecordEnvelope::all_records_v1(),
        caller_owns_logs,
    };
    comparison.configure(&mut options);
    let mut outcome = compare_two_runs(
        ComparedRun {
            output: first.output,
            log: first.log,
            label: "run 1",
        },
        ComparedRun {
            output: second.output,
            log: second.log,
            label: "run 2",
        },
        options,
    )?;
    let messages = comparison.apply(&mut outcome);
    if let Some(path) = verify_json {
        write_verification_json(path, &outcome)?;
    }
    // Publish before diagnostics, just as the common live owner does.
    for message in messages {
        eprintln!(":: {message}");
    }
    eprintln!(":: DBT path confirmed: DynamoRIO client reported tool=Detcore");
    let success_message = if compare_io_buffers {
        "Success: deterministic. Determinism verified."
    } else {
        "Success: deterministic. Determinism verified. NOTE: syscall \
         output-buffer CONTENT was not compared because --no-detlog-io-buffers \
         was given, so a divergence confined to a buffer whose length is stable \
         would not have been seen; drop that flag to include it."
    };
    announce_verification_outcome(
        &outcome,
        SecondRun::Rerun,
        success_message,
        "Failure: nondeterministic.",
    );
    Ok(outcome)
}

/// Where DynamoRIO keeps its configuration files when set; otherwise `HOME`.
#[cfg(feature = "dbt")]
const DYNAMORIO_CONFIG_DIR_ENV: &str = "DYNAMORIO_CONFIGDIR";

/// Each physical DBT run is the init of its own container, the one every
/// backend uses (`RunOpts::launch_in_container`): PID, UTS, mount and
/// network namespaces, a private /tmp and the identity mounts. Under
/// `--no-namespace` it is a plain child. The coordinator runtime is created
/// inside, so its workers and the guest share those namespaces.
#[cfg(feature = "dbt")]
struct DbtExecution {
    options: super::run::RunOpts,
    /// Statistics sinks, made below the original /tmp before the run, that
    /// the client must still reach at the same path inside the private /tmp.
    statistics: std::cell::RefCell<Vec<PathBuf>>,
}

#[cfg(feature = "dbt")]
impl DbtExecution {
    fn new(options: &super::run::RunOpts, test_workdir: bool) -> Self {
        // A marked workdir gets the fresh tmpfs that `hermit_test_workdir`
        // mounts on it.
        let mounts = if test_workdir {
            vec![
                reverie::process::Mount::tmpfs(hermit_test_workdir::WORKDIR)
                    .flags(
                        reverie::process::MountFlags::MS_NOSUID
                            | reverie::process::MountFlags::MS_NODEV,
                    )
                    .data("mode=1777"),
            ]
        } else {
            Vec::new()
        };
        Self {
            options: options.with_dbt_mounts(&[], mounts),
            statistics: Default::default(),
        }
    }

    fn stats_capture(&self) -> Result<DbtStatsCapture, Error> {
        let capture = DbtStatsCapture::new()?;
        self.statistics
            .borrow_mut()
            .push(capture.directory.path().to_path_buf());
        Ok(capture)
    }

    /// `guest`, copied, with DynamoRIO's configuration directory moved into
    /// the container's private /tmp.
    ///
    /// drrun hands its client options to the launched process in a one-time
    /// file, `<dir>/.dynamorio/<program>.<pid>.1config64`, where `<dir>` is
    /// `DYNAMORIO_CONFIGDIR` or else `HOME`. In the container the PID is the
    /// namespace's own, so concurrent runs that share `HOME` agree on the
    /// file name: one guest then reads and deletes the other's file, runs with
    /// that run's evidence channel, and neither run receives its evidence.
    /// DynamoRIO removes every `DYNAMORIO_` variable from the guest's
    /// environment, and the value is the same on every run, so the guest's
    /// initial stack does not change between runs.
    fn contained_command(&self, guest: &StdCommand) -> StdCommand {
        let mut command = copy_dbt_command(guest);
        if self.options.uses_container() {
            command.env(DYNAMORIO_CONFIG_DIR_ENV, "/tmp");
        }
        command
    }

    fn new_runtime() -> Result<tokio::runtime::Runtime, Error> {
        // RPCs can wait on the scheduler, so both need independent workers.
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|error| Error::msg(format!("failed to start the DBT coordinator: {error}")))
    }

    /// Run `run` with a fresh coordinator runtime and `config` completed for
    /// the container. The guest's environment keeps the configuration
    /// encoded before the container exists: the completed one carries this
    /// run's host mount IDs, and its length would move the guest's stack.
    fn run<F, T>(&self, config: &Config, mut run: F) -> Result<T, Error>
    where
        F: FnMut(&tokio::runtime::Runtime, &Config) -> Result<T, Error> + 'static,
        T: serde::Serialize + serde::de::DeserializeOwned + 'static,
    {
        // A verification's run-1 statistics sink is finished and removed before
        // run 2; only the sinks that still exist are this run's to preserve.
        let preserved = self
            .statistics
            .borrow()
            .iter()
            .filter(|path| path.exists())
            .cloned()
            .collect::<Vec<_>>();
        let options = self.options.with_dbt_mounts(&preserved, Vec::new());
        let config = config.clone();
        options
            .launch_in_container(
                (),
                "the DBT coordinator",
                options.run_timeout(),
                move |_, identity| {
                    // The detached stdin reader may stay blocked after the
                    // guest exits.
                    super::container::exit_init_after_publication();
                    let config = super::run::complete_container_config(config.clone(), identity)?;
                    let runtime = Self::new_runtime()?;
                    // The operation consumes GlobalState cleanup; runtime Drop
                    // joins its workers before the result is published.
                    run(&runtime, &config)
                },
            )
            .map(|(value, ())| value)
    }
}

/// A run's wait status and output, in the form the container init publishes.
#[cfg(feature = "dbt")]
#[derive(serde::Serialize, serde::Deserialize)]
struct DbtPhysicalOutput {
    status: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

#[cfg(feature = "dbt")]
impl From<Output> for DbtPhysicalOutput {
    fn from(output: Output) -> Self {
        Self {
            status: output.status.into_raw(),
            stdout: output.stdout,
            stderr: output.stderr,
        }
    }
}

#[cfg(feature = "dbt")]
impl From<DbtPhysicalOutput> for Output {
    fn from(output: DbtPhysicalOutput) -> Self {
        Self {
            status: std::process::ExitStatus::from_raw(output.status),
            stdout: output.stdout,
            stderr: output.stderr,
        }
    }
}

/// The container init's copy-on-write view of the caller's input. The init
/// exits without unwinding, which closes its descriptors; dropping the reader
/// would free heap the parent still owns.
#[cfg(feature = "dbt")]
struct ChildInput<R>(std::mem::ManuallyDrop<R>);

#[cfg(feature = "dbt")]
impl<R: Read> Read for ChildInput<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buffer)
    }
}

/// [`DbtRunEvidence`] with its own descriptor, for the container init.
#[cfg(feature = "dbt")]
struct OwnedDbtRunEvidence {
    file: std::fs::File,
    log_level: DbtEvidenceLogLevel,
    label: &'static str,
}

#[cfg(feature = "dbt")]
impl OwnedDbtRunEvidence {
    fn from_borrowed(evidence: Option<DbtRunEvidence<'_>>) -> Result<Option<Self>, Error> {
        evidence
            .map(|evidence| {
                Ok(Self {
                    file: evidence.file.try_clone()?,
                    log_level: evidence.log_level,
                    label: evidence.label,
                })
            })
            .transpose()
    }

    fn borrowed(&self) -> DbtRunEvidence<'_> {
        DbtRunEvidence {
            file: &self.file,
            log_level: self.log_level,
            label: self.label,
        }
    }
}

/// A copy of `guest`, which `std::process::Command` cannot clone.
#[cfg(feature = "dbt")]
fn copy_dbt_command(guest: &StdCommand) -> StdCommand {
    let mut command = StdCommand::new(guest.get_program());
    command.args(guest.get_args());
    for (name, value) in guest.get_envs() {
        match value {
            Some(value) => {
                command.env(name, value);
            }
            None => {
                command.env_remove(name);
            }
        }
    }
    if let Some(directory) = guest.get_current_dir() {
        command.current_dir(directory);
    }
    command
}

/// Create the abstract Unix evidence listener only after entering this physical
/// run's network namespace. A listener created by the caller would be invisible
/// to the guest. The completed artifact is an ordinary file read by the caller.
#[cfg(feature = "dbt")]
struct DbtRunEvidence<'a> {
    file: &'a std::fs::File,
    log_level: DbtEvidenceLogLevel,
    label: &'static str,
}

#[cfg(feature = "dbt")]
fn configure_dbt_run_evidence(
    runner: &DbtRunner,
    evidence: Option<DbtRunEvidence<'_>>,
) -> Result<DbtRunner, Error> {
    let runner = runner.clone();
    match evidence {
        Some(evidence) => runner
            .evidence_file(evidence.file)
            .map(|runner| runner.evidence_log_level(evidence.log_level))
            .map_err(|error| {
                Error::msg(format!(
                    "failed to configure protected DBT {} evidence: {error}",
                    evidence.label
                ))
            }),
        None => Ok(runner),
    }
}

#[cfg(feature = "dbt")]
fn run_once<R: Read + Send + 'static>(
    execution: &DbtExecution,
    runner: &DbtRunner,
    guest: &StdCommand,
    drrun: &Path,
    config: &Config,
    evidence: Option<DbtRunEvidence<'_>>,
    input: R,
) -> Result<Output, Error> {
    let runner = runner.clone();
    let guest = execution.contained_command(guest);
    let drrun = drrun.to_path_buf();
    let evidence = OwnedDbtRunEvidence::from_borrowed(evidence)?;
    let mut input = Some(input);
    execution
        .run(config, move |runtime, config| {
            let runner = configure_dbt_run_evidence(
                &runner,
                evidence.as_ref().map(OwnedDbtRunEvidence::borrowed),
            )?;
            let guest = prepare_dbt_physical_command(&guest)?;
            let input =
                ChildInput(std::mem::ManuallyDrop::new(input.take().ok_or_else(
                    || Error::msg("DBT physical input already consumed"),
                )?));
            let (output, global) = runtime
                .block_on(
                    runner.output_with_detached_reader_and_global::<detcore::GlobalState, _>(
                        &guest,
                        input,
                        config.clone(),
                    ),
                )
                .map_err(|error| dbt_run_error(&drrun, error))?;
            runtime.block_on(clean_up_dbt_global(&output.status, global));
            Ok(DbtPhysicalOutput::from(output))
        })
        .map(Output::from)
}

#[cfg(feature = "dbt")]
fn run_once_with_terminal_input(
    execution: &DbtExecution,
    runner: &DbtRunner,
    guest: &StdCommand,
    drrun: &Path,
    config: &Config,
    evidence: Option<DbtRunEvidence<'_>>,
) -> Result<Output, Error> {
    let runner = runner.clone();
    let guest = execution.contained_command(guest);
    let drrun = drrun.to_path_buf();
    let evidence = OwnedDbtRunEvidence::from_borrowed(evidence)?;
    execution
        .run(config, move |runtime, config| {
            let runner = configure_dbt_run_evidence(
                &runner,
                evidence.as_ref().map(OwnedDbtRunEvidence::borrowed),
            )?;
            let guest = prepare_dbt_physical_command(&guest)?;
            let (output, global) = runtime
                .block_on(
                    runner.output_with_inherited_stdin_and_global::<detcore::GlobalState>(
                        &guest,
                        config.clone(),
                    ),
                )
                .map_err(|error| dbt_run_error(&drrun, error))?;
            runtime.block_on(clean_up_dbt_global(&output.status, global));
            Ok(DbtPhysicalOutput::from(output))
        })
        .map(Output::from)
}

#[cfg(feature = "dbt")]
fn run_status(
    execution: &DbtExecution,
    runner: &DbtRunner,
    guest: &StdCommand,
    drrun: &Path,
    config: &Config,
) -> Result<std::process::ExitStatus, Error> {
    let runner = runner.clone();
    let guest = execution.contained_command(guest);
    let drrun = drrun.to_path_buf();
    execution
        .run(config, move |runtime, config| {
            let guest = prepare_dbt_physical_command(&guest)?;
            let (status, global) = runtime
                .block_on(runner.status_with_global::<detcore::GlobalState>(&guest, config.clone()))
                .map_err(|error| dbt_run_error(&drrun, error))?;
            runtime.block_on(clean_up_dbt_global(&status, global));
            Ok(status.into_raw())
        })
        .map(std::process::ExitStatus::from_raw)
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
    // A run that failed after its tree was reaped (protected evidence reporting
    // a backend failure, for one) still carries what it captured. Its stderr
    // usually holds the line that says why, so show it before the error.
    if let Some(output) = reverie_dbt::failed_run_output(&error)
        && !output.stderr.is_empty()
    {
        eprintln!(":: stderr captured from the failed DBT run:");
        let _ = std::io::stderr().write_all(&output.stderr);
    }
    match error.kind() {
        ErrorKind::NotFound | ErrorKind::PermissionDenied | ErrorKind::ExecutableFileBusy => {
            Error::msg(format!(
                "failed to launch drrun ({}): {error}",
                drrun.display()
            ))
        }
        // Typed under the message: `run --verify` records it as a backend
        // failure of the run, not as a run that never started.
        _ => Error::new(hermit::BackendRunFailure).context(format!(
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

#[cfg(feature = "sabre")]
fn sabre_artifact(variable: &str, description: &str, executable: bool) -> Result<OsString, Error> {
    let value = std::env::var_os(variable).ok_or_else(|| {
        Error::msg(format!(
            "the sabre backend needs {variable}=<path-to-{description}>"
        ))
    })?;
    validate_sabre_artifact(Path::new(&value), variable, executable)
}

#[cfg(feature = "sabre")]
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

#[cfg(feature = "sabre")]
const SABRE_QUIET_ENV: &str = "REVERIE_SABRE_STRACE_QUIET";

#[cfg(feature = "sabre")]
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

#[cfg(feature = "sabre")]
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
#[cfg(feature = "sabre")]
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

#[cfg(not(feature = "sabre"))]
pub fn run_sabre_strace(_program: &Path, _args: &[String]) -> Result<ExitStatus, Error> {
    hermit::Backend::Sabre.ensure_available()?;
    unreachable!("SaBRe availability must fail when the feature is disabled");
}

#[cfg(test)]
mod tests {
    #[cfg(not(feature = "sabre"))]
    #[test]
    fn disabled_sabre_strace_refuses_before_resolving_artifacts() {
        let error = super::run_sabre_strace(
            std::path::Path::new("/hermit-missing-disabled-sabre-program"),
            &[],
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("the `sabre` feature is not enabled")
        );
    }

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
    /// A behavioural pin, not a text match: require every record of the
    /// published stream, initialization records included, to reach the
    /// comparison log unchanged and in order. The guest records are passed
    /// separately because only they are counted by the refusal guards.
    #[test]
    fn materialization_keeps_every_decoded_comparable_record() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("evidence.log");
        let file = std::fs::File::create(&path).expect("create log");
        let records: Vec<Vec<u8>> = vec![
            b"1970-01-01T00:00:00.000000Z  INFO detcore: DETLOG first\n".to_vec(),
            b"1970-01-01T00:00:00.000000Z  INFO detcore: DETLOG second\n".to_vec(),
        ];
        let published = [
            DBT_INITIALIZATION_RECORD,
            records[0].as_slice(),
            records[1].as_slice(),
        ];

        super::materialize_dbt_comparison_log(&records, &published, file, &path)
            .expect("materialization must accept comparable records");

        assert_eq!(std::fs::read(&path).expect("read back"), published.concat());
    }

    /// Initialization records decide the terminal outcome in two ways: their
    /// typed count is compared as a separate dimension, and under
    /// `all_records_v1` their positions are compared with the guest records.
    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_initialization_record_counts_control_the_terminal_outcome() {
        use super::super::record_envelope::RecordEnvelopePolicy;
        use super::super::verify::ComparedLogCounts;
        use super::super::verify::verification_report;

        fn dbt_verdict(directory: &Path, left: &[&[u8]], right: &[&[u8]]) -> VerificationOutcome {
            let output = reverie::process::Output {
                status: ExitStatus::Exited(0),
                stdout: Vec::new(),
                stderr: Vec::new(),
            };
            let log = |records: &[&[u8]]| {
                let (file, path) = tempfile::NamedTempFile::new_in(directory)
                    .unwrap()
                    .into_parts();
                materialize_dbt_evidence_log(&decoded_dbt_evidence(records), file, &path).unwrap();
                path
            };
            compare_two_runs(
                ComparedRun {
                    output: &output,
                    log: log(left),
                    label: "run 1",
                },
                ComparedRun {
                    output: &output,
                    log: log(right),
                    label: "run 2",
                },
                ComparisonOptions {
                    verbose: false,
                    strictness: LogCompareStrictness::Canonical,
                    compare_logs: true,
                    diagnostic_full_trace: false,
                    compare_io_buffers: true,
                    keep_logs: false,
                    match_overridden: false,
                    failed_log_retention: None,
                    record_envelope: RecordEnvelope::all_records_v1(),
                    virtualize_time: true,
                    caller_owns_logs: false,
                },
            )
            .unwrap()
        }

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

        // End to end through the verdict: logs materialized by the production
        // path are compared with the options the DBT adapter uses. The
        // comparison counts five INFO records per side, where `records()` alone
        // holds three, and the report names `all_records_v1`. Moving the child's
        // initialization record away from the position the collector gives it,
        // immediately before the child's first record, diverges the verdict
        // although the guest records and the initialization counts are
        // unchanged.
        let parent_first: &[u8] = b"1970-01-01T00:00:00.000000Z INFO detcore: parent record 1\n";
        let parent_second: &[u8] = b"1970-01-01T00:00:00.000000Z INFO detcore: parent record 2\n";
        let child: &[u8] = b"1970-01-01T00:00:00.000000Z INFO detcore: child record 1\n";
        let canonical = [
            DBT_INITIALIZATION_RECORD,
            parent_first,
            parent_second,
            DBT_INITIALIZATION_RECORD,
            child,
        ];
        let moved = [
            DBT_INITIALIZATION_RECORD,
            parent_first,
            DBT_INITIALIZATION_RECORD,
            parent_second,
            child,
        ];
        // A diverged comparison keeps its logs, so they must live in a
        // directory this test removes.
        let directory = tempfile::tempdir().unwrap();

        let matched = dbt_verdict(directory.path(), &canonical, &canonical);
        assert_eq!(matched.verdict, Verdict::Matched);
        assert_eq!(
            matched.compared_log_messages,
            Some(ComparedLogCounts { left: 5, right: 5 }),
            "the initialization records must be compared with the guest records"
        );
        assert_eq!(
            matched.comparison.record_envelope,
            RecordEnvelopePolicy::AllRecordsV1
        );
        let report = serde_json::to_value(verification_report(&matched)).unwrap();
        assert_eq!(report["comparison"]["record_envelope"], "all_records_v1");
        assert_eq!(report["bitwise_parity"], true);

        let diverged = dbt_verdict(directory.path(), &canonical, &moved);
        assert_eq!(diverged.verdict, Verdict::Diverged);
        assert_eq!(
            diverged.compared_log_messages,
            Some(ComparedLogCounts { left: 5, right: 5 })
        );
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
            refused_log_comparison: (verdict == Verdict::NoResult)
                .then(|| "fixture comparator refusal".into()),
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
            first_divergent_record_positions: None,
            first_divergent_virtual_nanoseconds: None,
            first_divergent_record: None,
            first_divergent_syscall: None,
            first_divergent_left_message: None,
            first_divergent_right_message: None,
        }
    }

    /// The common owner must consume DBT facts before it publishes a verdict,
    /// while a rejected run must never reach run 2 or a stats-error override.
    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_verification_run_path_binds_terminal_verdict_to_branch_stats() {
        let common = include_str!("run.rs")
            .split_once("    fn verify(")
            .unwrap()
            .1
            .split_once("    /// The `--bind`s")
            .unwrap()
            .0;
        let physical = include_str!("backends.rs")
            .split_once("impl DbtVerification {")
            .unwrap()
            .1
            .split_once("/// Stats are consumed")
            .unwrap()
            .0;
        assert!(!physical.contains("if verify_json.is_none()"));
        assert!(
            physical.find("decode_dbt_evidence").unwrap()
                < physical.find("materialize_dbt_evidence_log").unwrap()
        );
        assert!(
            physical.find("materialize_dbt_evidence_log").unwrap()
                < physical
                    .find("Ok(super::run::VerificationRunOutput")
                    .unwrap()
        );
        let at = |needle| {
            common
                .find(needle)
                .unwrap_or_else(|| panic!("missing common owner boundary: {needle}"))
        };
        let run1 = at("&run1_options,");
        let signal1 = at("signal_reports.prepare(\"run 1\"");
        let latch1 = at("latches1.refuse_comparison");
        let rejected = at("if let Some(error) = reject_first_run(");
        let stats1 = at("facts.finish(&out1, VerificationRun::Run1)");
        let run2 = at("&run2_options,");
        let signal2 = at("signal_reports.prepare(\"run 2\"");
        let latch2 = at("latches2.refuse_comparison");
        let stats2 = at("facts.finish(&out2, VerificationRun::Run2)");
        let comparison = at("DbtVerificationComparison::new(first, second)");
        let conclude = at("let outcome = conclude_verification(");
        let exit = at("return outcome.into_exit_status()");
        assert!(
            run1 < signal1
                && signal1 < latch1
                && latch1 < rejected
                && rejected < stats1
                && stats1 < run2
        );
        assert!(
            run2 < signal2
                && signal2 < latch2
                && latch2 < stats2
                && stats2 < comparison
                && comparison < conclude
                && conclude < exit
        );
        let conclusion = include_str!("run.rs")
            .split_once("pub(super) fn conclude_verification(")
            .unwrap()
            .1;
        let boundary = |needle| conclusion.find(needle).unwrap();
        let retention = boundary("comparison.configure(&mut comparison_options)");
        let compare = boundary("let mut outcome = compare_two_runs(");
        let apply = boundary("comparison.apply(&mut outcome)");
        let publish = boundary("write_verification_json(path, &outcome)");
        let announce = boundary("if let Some(messages) = dbt_messages");
        assert!(retention < compare && compare < apply && apply < publish && publish < announce);
        let offline = include_str!("backends.rs")
            .split_once("pub(super) fn conclude_dbt_verification(")
            .unwrap()
            .1
            .split_once("/// Where DynamoRIO")
            .unwrap()
            .0;
        assert!(
            offline.find("comparison.configure(&mut options)").unwrap()
                < offline.find("compare_two_runs(").unwrap()
        );
        assert!(
            offline.find("comparison.apply(&mut outcome)").unwrap()
                < offline
                    .find("write_verification_json(path, &outcome)")
                    .unwrap()
        );
        assert!(offline.contains("caller_owns_logs,"));
        assert!(exit < at("signal_reports.verified()"));
        let rejection = &common[rejected..stats1];
        assert!(rejection.contains("drop(signal_reports)"));
        let reject = include_str!("run.rs")
            .split_once("pub(super) fn reject_first_run(")
            .unwrap()
            .1
            .split_once("pub(super) fn verify_run_outcome_of(")
            .unwrap()
            .0;
        assert!(reject.contains("first_run_rejected_report("));
        assert!(reject.contains("write_report_json(path, &report)"));
        for (begin, end) in [(stats1, run2), (stats2, comparison)] {
            let refusal = &common[begin..end];
            assert!(refusal.contains("publish_run_failure("));
            assert!(refusal.contains("retain_logs_after_verification_error("));
            assert!(refusal.contains("return Err(error)"));
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

        let mut outcome = outcome;
        let failure = record_dbt_branch_clock_comparison(&mut outcome, comparison);
        write_verification_json(verdict_file.path(), &outcome).unwrap();
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
            let mut outcome = outcome;
            let message = record_dbt_branch_clock_comparison(&mut outcome, comparison);
            write_verification_json(verdict_file.path(), &outcome).unwrap();
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

        let mut outcome = outcome;
        let failure = record_dbt_branch_clock_comparison(&mut outcome, comparison);
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
    fn dbt_facts_preserve_canonical_comparison_and_force_mismatch_retention() {
        let first = DbtRunFacts {
            stats: dbt_stats(100, 7, 7, 0, 42),
            initialization_records: 2,
        };
        for (branches, images, mismatch, caller_owns_logs, initial_override) in [
            (100, 2, false, false, false),
            (101, 2, true, false, false),
            (100, 3, true, false, false),
            (100, 2, false, true, false),
            (101, 2, true, true, false),
            (100, 3, true, true, false),
            (100, 2, false, false, true),
            (101, 2, true, false, true),
            (100, 3, true, false, true),
            (100, 2, false, true, true),
            (101, 2, true, true, true),
            (100, 3, true, true, true),
        ] {
            let second = DbtRunFacts {
                stats: dbt_stats(branches, 7, 7, 0, 42),
                initialization_records: images,
            };
            let comparison = DbtVerificationComparison::new(&first, &second);
            let mut options = ComparisonOptions {
                verbose: false,
                strictness: LogCompareStrictness::Stripped,
                compare_logs: true,
                diagnostic_full_trace: false,
                compare_io_buffers: true,
                keep_logs: false,
                match_overridden: initial_override,
                failed_log_retention: None,
                record_envelope: RecordEnvelope::all_records_v1(),
                virtualize_time: true,
                caller_owns_logs,
            };
            comparison.configure(&mut options);
            assert_eq!(options.caller_owns_logs, caller_owns_logs);
            assert_eq!(options.strictness, LogCompareStrictness::Canonical);
            assert!(options.compare_logs && options.compare_io_buffers && options.virtualize_time);
            assert_eq!(options.keep_logs, mismatch);
            assert_eq!(options.match_overridden, initial_override || mismatch);
            let mut outcome = typed_outcome(Verdict::Matched, 23);
            let failures = comparison.apply(&mut outcome);
            assert_eq!(!failures.is_empty(), mismatch);
            assert_eq!(outcome.verified(), !mismatch);
            assert_eq!(outcome.guest_status, ExitStatus::Exited(23));
        }
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn missing_dbt_completion_stats_are_attributed_to_the_completed_run() {
        let output = reverie::process::Output {
            status: ExitStatus::Exited(0),
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        for (run, expected) in [
            (VerificationRun::Run1, "run1"),
            (VerificationRun::Run2, "run2"),
        ] {
            let completion = DbtRunCompletion {
                stats: DbtStatsCapture::new().unwrap(),
                initialization_records: 1,
                terminal_stdin: false,
            };
            let error = completion
                .finish(&output, run)
                .err()
                .expect("missing stats must refuse");
            assert!(error.downcast_ref::<hermit::BackendRunFailure>().is_some());
            let saved =
                serde_json::to_value(super::super::run::verify_run_outcome_of(&error)).unwrap();
            assert_eq!(saved["kind"], "run_failed");
            assert_eq!(saved["failure"], "backend_failure");
            assert!(
                saved["detail"]
                    .as_str()
                    .unwrap()
                    .contains("typed whole-process statistics were missing")
            );
            let report = tempfile::NamedTempFile::new().unwrap();
            let error = super::super::verify::publish_run_failure(Some(report.path()), run, error);
            assert!(format!("{error:#}").contains("typed whole-process statistics were missing"));
            let report: serde_json::Value =
                serde_json::from_slice(&fs::read(report.path()).unwrap()).unwrap();
            assert_eq!(report["verdict"], "no_result");
            assert_eq!(report["no_result_reason"]["kind"], "run_failed");
            assert_eq!(report["no_result_reason"]["run"], expected);
            assert_eq!(report["no_result_reason"]["failure"], "backend_failure");
        }
    }

    #[cfg(feature = "dbt")]
    fn common_dbt_verify_options(
        directory: &Path,
        allow: &str,
    ) -> (super::super::run::RunOpts, PathBuf) {
        use clap::Parser;

        let report = directory.join("verdict.json");
        let logs = directory.join("logs");
        let allow = format!("--verify-allow={allow}");
        let mut options = super::super::run::RunOpts::parse_from([
            "run",
            "--verify",
            &allow,
            "--keep-logs",
            "--verify-log-dir",
            logs.to_str().unwrap(),
            "--verify-json",
            report.to_str().unwrap(),
            "--",
            "/bin/true",
        ]);
        options.set_backend(Some(hermit::Backend::Dbt));
        super::super::verify::write_pending_verification_json(&report).unwrap();
        (options, report)
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn common_verifier_checks_first_run_disposition_before_dbt_stats() {
        for (case, status, loss) in [
            ("latch", 0, Some("injected run-1 loss")),
            ("rejected", 23, None),
            ("stats", 0, None),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let (options, report_path) = common_dbt_verify_options(directory.path(), "success");
            let mut calls = Vec::new();
            let mut log_path = None;
            let error = options
                .verify_with_execution(|_, mut log, path, forwarded, run| {
                    calls.push(run);
                    assert_eq!(run, VerificationRun::Run1, "{case}: run 2 must not start");
                    assert!(forwarded.is_none());
                    log.write_all(b"retained first-run evidence\n")?;
                    log_path = Some(path.to_path_buf());
                    Ok(super::super::run::VerificationRunOutput {
                        output: reverie::process::Output {
                            status: ExitStatus::Exited(status),
                            stdout: Vec::new(),
                            stderr: Vec::new(),
                        },
                        skid_overshoots: 0,
                        latches: super::super::run::RunLatches {
                            determinism_loss: loss.map(str::to_owned),
                            log_sink_failure: None,
                        },
                        dbt: Some(DbtRunCompletion {
                            stats: DbtStatsCapture::new()?,
                            initialization_records: 1,
                            terminal_stdin: false,
                        }),
                    })
                })
                .unwrap_err();
            assert_eq!(calls, [VerificationRun::Run1]);
            assert_eq!(
                fs::read(log_path.unwrap()).unwrap(),
                b"retained first-run evidence\n"
            );
            let report: serde_json::Value =
                serde_json::from_slice(&fs::read(report_path).unwrap()).unwrap();
            assert_eq!(report["verdict"], "no_result");
            assert_eq!(report["verified"], false);
            let reason = &report["no_result_reason"];
            let detail = format!("{error:#}");
            match case {
                "latch" => {
                    assert_eq!(reason["failure"], "determinism_loss");
                    assert_eq!(reason["run"], "run1");
                    assert!(detail.contains("injected run-1 loss"), "{detail}");
                    assert!(!detail.contains("statistics were missing"), "{detail}");
                }
                "rejected" => {
                    assert_eq!(reason["kind"], "first_run_rejected");
                    assert_eq!(report["guest_exit_code"], 23);
                    assert!(detail.contains("First run during --verify"), "{detail}");
                    assert!(!detail.contains("statistics were missing"), "{detail}");
                }
                "stats" => {
                    assert_eq!(reason["kind"], "run_failed");
                    assert_eq!(reason["failure"], "backend_failure");
                    assert_eq!(reason["run"], "run1");
                    assert!(detail.contains("statistics were missing"), "{detail}");
                }
                _ => unreachable!(),
            }
        }
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn common_verifier_retains_run1_when_second_dbt_completion_fails() {
        let directory = tempfile::tempdir().unwrap();
        let (options, report_path) = common_dbt_verify_options(directory.path(), "both");
        let mut runs = Vec::new();
        let error = options
            .verify_with_execution(|_, mut log, path, forwarded, run| {
                assert!(forwarded.is_none());
                let bytes = format!("completed {run:?}: guest exit 23\n");
                log.write_all(bytes.as_bytes())?;
                runs.push((run, path.to_path_buf(), bytes));
                let stats = DbtStatsCapture::new()?;
                if run == VerificationRun::Run1 {
                    let record = reverie_dbt::backend_stats::DbtProcessRecord {
                        branches: 1,
                        syscalls: 1,
                        rewritten: 1,
                        ..Default::default()
                    };
                    fs::write(
                        &stats.path,
                        reverie_dbt::backend_stats::encode_process_record(&record),
                    )?;
                }
                Ok(super::super::run::VerificationRunOutput {
                    output: reverie::process::Output {
                        status: ExitStatus::Exited(23),
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                    },
                    skid_overshoots: 0,
                    latches: super::super::run::RunLatches::default(),
                    dbt: Some(DbtRunCompletion {
                        stats,
                        initialization_records: 1,
                        terminal_stdin: false,
                    }),
                })
            })
            .unwrap_err();
        assert_eq!(
            runs.iter().map(|(run, _, _)| *run).collect::<Vec<_>>(),
            [VerificationRun::Run1, VerificationRun::Run2]
        );
        for (_, path, bytes) in runs {
            assert_eq!(fs::read_to_string(path).unwrap(), bytes);
        }
        assert!(format!("{error:#}").contains("statistics were missing"));
        let report: serde_json::Value =
            serde_json::from_slice(&fs::read(report_path).unwrap()).unwrap();
        assert_eq!(report["verdict"], "no_result");
        assert_eq!(report["no_result_reason"]["kind"], "run_failed");
        assert_eq!(report["no_result_reason"]["failure"], "backend_failure");
        assert_eq!(report["no_result_reason"]["run"], "run2");
        // The existing failure schema does not invent a guest status or
        // comparison from incomplete facts. Both real per-run logs survive.
        assert!(report["guest_exit_code"].is_null());
        assert!(report["comparison"].is_null());
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
        super::super::verify::write_pending_verification_json(verdict_file.path()).unwrap();

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

    /// The published stream reaches the comparison log unchanged and in
    /// stream order, initialization records included.
    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_canonical_evidence_materializes_records_unchanged() {
        let log = tempfile::NamedTempFile::new().unwrap();
        let (file, path) = log.into_parts();
        let records = vec![
            b"1970-01-01T00:00:00.000000Z INFO detcore: DETLOG first\n".to_vec(),
            b"1970-01-01T00:00:00.000000Z INFO detcore::scheduler: second\n".to_vec(),
        ];
        let published = [
            DBT_INITIALIZATION_RECORD,
            records[0].as_slice(),
            records[1].as_slice(),
        ];

        let compared = materialize_dbt_comparison_log(&records, &published, file, &path).unwrap();

        // The initialization record is an INFO record of the published stream,
        // so it is counted with the guest records.
        assert_eq!(compared, 3);
        assert_eq!(fs::read(&path).unwrap(), published.concat());

        // Materialization publishes an initialization record where the decoded
        // stream holds it, neither dropped nor moved. This stream is built by
        // hand: the Reverie collector would place the second initialization
        // record immediately before "child record 1", but the artifact carries
        // no image identity, so the decoder cannot enforce that placement and
        // accepts this layout too. Two streams with the same guest records and
        // the same initialization count, differing only in where an
        // initialization record sits, publish different logs; the typed count
        // alone could not tell them apart.
        let parent_first: &[u8] = b"1970-01-01T00:00:00.000000Z INFO detcore: parent record 1\n";
        let parent_second: &[u8] = b"1970-01-01T00:00:00.000000Z INFO detcore: parent record 2\n";
        let child: &[u8] = b"1970-01-01T00:00:00.000000Z INFO detcore: child record 1\n";
        let stream = [
            DBT_INITIALIZATION_RECORD,
            parent_first,
            DBT_INITIALIZATION_RECORD,
            parent_second,
            child,
        ];
        let evidence = decoded_dbt_evidence(&stream);
        assert_eq!(evidence.all_records(), stream);
        let (file, path) = tempfile::NamedTempFile::new().unwrap().into_parts();
        let compared = materialize_dbt_evidence_log(&evidence, file, &path).unwrap();
        assert_eq!(compared, 5, "both initialization records are INFO records");
        assert_eq!(fs::read(&path).unwrap(), stream.concat());

        let front_loaded = decoded_dbt_evidence(&[
            DBT_INITIALIZATION_RECORD,
            DBT_INITIALIZATION_RECORD,
            parent_first,
            parent_second,
            child,
        ]);
        assert_eq!(front_loaded.records(), evidence.records());
        assert_eq!(
            front_loaded.initialization_records(),
            evidence.initialization_records()
        );
        let (file, path) = tempfile::NamedTempFile::new().unwrap().into_parts();
        materialize_dbt_evidence_log(&front_loaded, file, &path).unwrap();
        assert_ne!(fs::read(&path).unwrap(), stream.concat());
    }

    /// Evidence without guest records, or with an unframed record, is refused
    /// before anything is compared.
    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_canonical_evidence_fails_closed_on_empty_or_unframed_records() {
        let empty = tempfile::NamedTempFile::new().unwrap();
        let (file, path) = empty.into_parts();
        assert!(materialize_dbt_comparison_log(&[], &[], file, &path).is_err());

        let unframed_record = b"1970-01-01T00:00:00.000000Z INFO detcore: missing newline";
        let unframed = tempfile::NamedTempFile::new().unwrap();
        let (file, path) = unframed.into_parts();
        assert!(
            materialize_dbt_comparison_log(
                &[unframed_record.to_vec()],
                &[DBT_INITIALIZATION_RECORD, unframed_record],
                file,
                &path,
            )
            .is_err()
        );

        // Valid guest records do not excuse an unframed record in the stream
        // that is actually published.
        let framed_record = b"1970-01-01T00:00:00.000000Z INFO detcore: framed\n";
        let unframed_published = tempfile::NamedTempFile::new().unwrap();
        let (file, path) = unframed_published.into_parts();
        assert!(
            materialize_dbt_comparison_log(
                &[framed_record.to_vec()],
                &[framed_record, unframed_record],
                file,
                &path,
            )
            .is_err()
        );

        // The refusal https://github.com/rrnewton/hermit/pull/2356 added must
        // survive the publication of initialization records. Every decoded
        // stream holds at least one initialization record, and it is an INFO
        // record, so a guard that counted `all_records()` would pass both
        // streams below and the verdict would report nonzero compared INFO
        // counts for a guest that left no evidence. Each refusal is asserted by
        // its exact message: counting the published stream in either guard
        // makes this test fail, either with a different message or because
        // materialization succeeds.
        let startup_only = decoded_dbt_evidence(&[DBT_INITIALIZATION_RECORD]);
        assert!(startup_only.records().is_empty());
        assert_eq!(startup_only.all_records(), [DBT_INITIALIZATION_RECORD]);
        let (file, path) = tempfile::NamedTempFile::new().unwrap().into_parts();
        let error = materialize_dbt_evidence_log(&startup_only, file, &path).unwrap_err();
        assert_eq!(
            error.to_string(),
            "DBT canonical evidence contained no records"
        );

        // A guest record outside the INFO comparison cannot stand in for guest
        // evidence, even beside two INFO initialization records.
        let no_guest_info = decoded_dbt_evidence(&[
            DBT_INITIALIZATION_RECORD,
            b"1970-01-01T00:00:00.000000Z DEBUG detcore: guest diagnostic\n",
            DBT_INITIALIZATION_RECORD,
        ]);
        assert_eq!(no_guest_info.records().len(), 1);
        assert_eq!(no_guest_info.initialization_records(), 2);
        let (file, path) = tempfile::NamedTempFile::new().unwrap().into_parts();
        let error = materialize_dbt_evidence_log(&no_guest_info, file, &path).unwrap_err();
        assert_eq!(
            error.to_string(),
            "DBT canonical evidence contained no INFO records"
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

    /// The process-image initialization record, byte for byte the constant
    /// Reverie's decoder admits. Every decoded stream holds at least one.
    #[cfg(feature = "dbt")]
    const DBT_INITIALIZATION_RECORD: &[u8] =
        b"1970-01-01T00:00:00.000000Z INFO reverie_dbt::evidence: protected evidence initialized\n";

    /// Encode `records` in the version 1 evidence file format that
    /// `reverie_dbt::decode_evidence` authenticates. Reverie exports no
    /// encoder, so the layout is written out here: a 40-byte header holding the
    /// magic, version, header length, a zero reserved word, the record count,
    /// the payload byte count and an FNV-1a digest of the body, followed by the
    /// body, in which each record is preceded by its little-endian `u32` length.
    #[cfg(feature = "dbt")]
    fn encode_dbt_evidence(records: &[&[u8]]) -> Vec<u8> {
        let mut body = Vec::new();
        for record in records {
            body.extend_from_slice(&u32::try_from(record.len()).unwrap().to_le_bytes());
            body.extend_from_slice(record);
        }
        let payload_bytes: usize = records.iter().map(|record| record.len()).sum();
        let digest = body.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
        let mut encoded = Vec::with_capacity(40 + body.len());
        encoded.extend_from_slice(b"RVDBTEF1");
        encoded.extend_from_slice(&1_u16.to_le_bytes());
        encoded.extend_from_slice(&40_u16.to_le_bytes());
        encoded.extend_from_slice(&0_u32.to_le_bytes());
        encoded.extend_from_slice(&u64::try_from(records.len()).unwrap().to_le_bytes());
        encoded.extend_from_slice(&u64::try_from(payload_bytes).unwrap().to_le_bytes());
        encoded.extend_from_slice(&digest.to_le_bytes());
        encoded.extend_from_slice(&body);
        encoded
    }

    /// Decode `records` through the production decoder, as a DBT run's
    /// evidence file is decoded.
    #[cfg(feature = "dbt")]
    fn decoded_dbt_evidence(records: &[&[u8]]) -> DbtEvidence {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(&encode_dbt_evidence(records)).unwrap();
        decode_dbt_evidence(&mut file).expect("fixture evidence must decode")
    }

    // Under DBT the determinism-loss latch is in the guest; the client
    // reports it as an evidence record, and the run is refused by the same
    // check the generic verifier uses, never compared.
    #[test]
    #[cfg(feature = "dbt")]
    fn a_dbt_run_whose_evidence_records_a_determinism_loss_is_not_compared() {
        let record: &[u8] =
            b"1970-01-01T00:00:00.000000Z INFO detcore: [dtid 3] DETLOG [syscall] getpid() = 3\n";
        let clean = decoded_dbt_evidence(&[DBT_INITIALIZATION_RECORD, record]);
        assert_eq!(dbt_run_latches(&clean).determinism_loss, None);
        assert!(dbt_run_latches(&clean).refuse_comparison("run 1").is_ok());

        let loss = format!(
            "1970-01-01T00:00:00.000000Z ERROR {}: process 3 exited without deregistering\n",
            detcore::detlog::DETERMINISM_LOSS_RECORD_TARGET
        );
        let lost = decoded_dbt_evidence(&[DBT_INITIALIZATION_RECORD, record, loss.as_bytes()]);
        let refusal = dbt_run_latches(&lost)
            .refuse_comparison("run 2")
            .unwrap_err()
            .to_string();
        assert_eq!(
            refusal,
            "run 2: determinism loss recorded: process 3 exited without deregistering"
        );
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
    #[cfg(feature = "sabre")]
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
