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
use std::path::Path;
#[cfg(feature = "dbt")]
use std::path::PathBuf;
use std::process::Command as StdCommand;
#[cfg(feature = "dbt")]
use std::process::Output;

use detcore::Config;
use hermit::Error;
use hermit::ExitStatus;
#[cfg(feature = "dbt")]
use reverie_dbt::DbtRunner;
use tracing::metadata::LevelFilter;

use super::run::VerifyAllow;

#[derive(Debug)]
#[cfg(feature = "dbt")]
struct DbtSummary {
    branches: u64,
    syscalls: u64,
    rewritten: u64,
    stdin_reads: u64,
    memory_hash: String,
}

#[cfg(feature = "dbt")]
impl DbtSummary {
    fn same_observable_behavior(&self, other: &Self) -> bool {
        // `branches` is the count at the last intercepted syscall, not an execution digest.
        // Keep it as callback-health telemetry without rejecting otherwise identical runs.
        self.syscalls == other.syscalls
            && self.rewritten == other.rewritten
            && self.stdin_reads == other.stdin_reads
            && self.memory_hash == other.memory_hash
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
fn format_dbt_stats(summary: &DbtSummary) -> String {
    format!(
        "=== DBT backend stats (native DynamoRIO client) ===\n\
         counted branches (deterministic branch clock): {}\n\
         syscalls intercepted:                          {}\n\
         syscall instructions rewritten:                {}\n\
         stdin (fd 0) reads:                            {}\n\
         observed guest-memory hash:                    {}\n",
        summary.branches,
        summary.syscalls,
        summary.rewritten,
        summary.stdin_reads,
        summary.memory_hash,
    )
}

#[derive(Debug, Eq, PartialEq)]
#[cfg(feature = "dbt")]
struct DbtGuestCommand {
    program: PathBuf,
    args: Vec<OsString>,
}

#[cfg(feature = "dbt")]
fn executable_on_path(program: &OsStr, path: &OsStr) -> Option<PathBuf> {
    env::split_paths(path)
        .map(|directory| directory.join(program))
        .find(|candidate| {
            candidate.metadata().is_ok_and(|metadata| {
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
    args: &[String],
    path: Option<&OsStr>,
) -> DbtGuestCommand {
    let unchanged = || DbtGuestCommand {
        program: program.to_path_buf(),
        args: args.iter().map(OsString::from).collect(),
    };
    let Some(shebang) = hermit::Shebang::new(program) else {
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
    let Some(target) = path.and_then(|path| executable_on_path(target, path)) else {
        return unchanged();
    };

    let mut resolved_args = Vec::with_capacity(args.len() + 2);
    resolved_args.push(target.into_os_string());
    resolved_args.push(program.as_os_str().to_owned());
    resolved_args.extend(args.iter().map(OsString::from));
    DbtGuestCommand {
        program: interpreter,
        args: resolved_args,
    }
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

/// Runs `program` through DynamoRIO with the real Detcore Tool.
///
/// `config` is the CLI-derived Detcore configuration (the same value the ptrace
/// backend receives). It is serialized into [`detcore_dbt::DETCONFIG_ENV`] so
/// flags such as `--strict`, `--seed`, and the time/CPUID virtualization
/// switches actually reach the in-guest Detcore Tool instead of being ignored.
///
/// When `verify` is true, the guest is executed twice. Both runs must exit in a
/// way `verify_allow` permits (by default success; `--verify-allow {failure,both}`
/// admits a deliberate non-zero exit), agree on their exit status, produce
/// byte-identical stdout, report `tool=Detcore`, and produce the same observed
/// guest-memory hash from the native DBT runtime.
// This mirrors the option surface of `hermit run`, so its parameters track the
// CLI run flags rather than a cohesive value object; bundling them would not
// clarify the dispatch shim.
#[allow(clippy::too_many_arguments)]
#[cfg(feature = "dbt")]
pub(super) fn run_dbt(
    program: &Path,
    args: &[String],
    verify: bool,
    verify_allow: VerifyAllow,
    summary: bool,
    log: Option<LevelFilter>,
    config: &Config,
    mut environment: BTreeMap<OsString, OsString>,
) -> Result<ExitStatus, Error> {
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

    let stdin_is_terminal = std::io::stdin().is_terminal();

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
        .summary(true)
        .isolated_process_group(panic_on_unsupported_syscalls);
    if panic_on_unsupported_syscalls {
        runner = runner.client_argument("-panic-on-unsupported-syscalls");
    }

    eprintln!(
        "hermit: [dbt backend] Detcore Tool active; running {program:?} under DynamoRIO ({})",
        drrun.display()
    );

    let _unsupported_report = DbtUnsupportedSyscallReport::new()?;
    let prepared = prepare_dbt_guest_command(
        program,
        args,
        environment.get(OsStr::new("PATH")).map(OsString::as_os_str),
    );
    let mut guest = StdCommand::new(&prepared.program);
    if let Some(level) = log {
        environment.insert("HERMIT_LOG".into(), level.to_string().into());
    }
    environment.insert(detcore_dbt::DETCONFIG_ENV.into(), config_json.into());
    apply_exact_environment(&mut guest, &environment);
    guest.args(&prepared.args);

    if !verify {
        if stdin_is_terminal {
            let status = runner
                .status(&guest)
                .map_err(|error| launch_error(&drrun, error))?;
            if summary {
                // stdout/stderr are inherited on the terminal path, so the
                // client's raw `reverie-dbt:` counter line has already been
                // printed above; we cannot re-parse it here.
                eprintln!(
                    ":: DBT summary: see the `reverie-dbt: tool=Detcore ...` line above \
                     (run without a terminal on stdin for the labeled block)"
                );
            }
            return Ok(process_status(status));
        }
        let output = run_once(&runner, &guest, &drrun, std::io::stdin())?;
        write_output(&output)?;
        if summary {
            // Best-effort: surface the native DBT counters the client already
            // emitted. A parse failure here is non-fatal — the run itself
            // succeeded and the raw `reverie-dbt:` line is still on stderr.
            match detcore_summary(&output) {
                Ok(stats) => eprint!("{}", format_dbt_stats(&stats)),
                Err(error) => {
                    eprintln!(":: DBT summary unavailable: {error}");
                }
            }
        }
        return Ok(output_status(&output));
    }

    let mut replay = if stdin_is_terminal {
        None
    } else {
        Some(tempfile::tempfile()?)
    };

    eprintln!(":: DBT Run1...");
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
    if !verify_allow.satisfies(process_status(first.status)) {
        // The first run exited in a way `--verify-allow` does not permit, so a
        // second run cannot establish determinism for the intended contract.
        // This mirrors the ptrace `--verify` path (see `verify` in run.rs).
        // With `--verify-allow {failure,both}` a deliberate non-zero exit *is*
        // permitted, so the double-run comparison below still executes — that is
        // what lets the `exit_status` backend-parity contract reach L2 on DBT.
        write_output(&first)?;
        return Ok(output_status(&first));
    }
    let first_summary = detcore_summary(&first)?;
    if stdin_is_terminal && first_summary.stdin_reads != 0 {
        write_output(&first)?;
        return Err(Error::msg(format!(
            "DBT verification cannot replay terminal stdin: guest attempted {} fd-0 read syscall(s)",
            first_summary.stdin_reads
        )));
    }

    eprintln!(":: DBT Run2...");
    let second = match replay.as_mut() {
        Some(replay) => {
            replay.seek(SeekFrom::Start(0))?;
            run_once(&runner, &guest, &drrun, replay.try_clone()?)?
        }
        None => run_once_with_terminal_input(&runner, &guest, &drrun)?,
    };
    if !verify_allow.satisfies(process_status(second.status)) {
        write_output(&second)?;
        return Ok(output_status(&second));
    }
    let second_summary = detcore_summary(&second)?;

    // Determinism requires both runs to agree on their exit status, not merely
    // that each is permitted by `--verify-allow`. This guards the non-zero-exit
    // contract (e.g. a guest that must exit 23 on both runs); without it, two
    // differing permitted failures would be accepted as "deterministic".
    if first.status != second.status {
        write_output(&first)?;
        return Err(Error::msg(format!(
            "DBT verification failed: guest exit status differed between runs ({:?} != {:?})",
            first.status, second.status
        )));
    }

    if first.stdout != second.stdout {
        return Err(Error::msg(dbt_stdout_mismatch(
            &first.stdout,
            &second.stdout,
        )));
    }
    if !first_summary.same_observable_behavior(&second_summary) {
        return Err(Error::msg(format!(
            "DBT verification failed: native Detcore summaries differed ({first_summary:?} != {second_summary:?})"
        )));
    }
    if first_summary.branches != second_summary.branches {
        eprintln!(
            ":: DBT diagnostic branch counts differed at the last syscall: {} | {}",
            first_summary.branches, second_summary.branches
        );
    }

    write_output(&first)?;
    eprintln!(
        ":: Comparing DBT observed guest-memory hashes... {} | {}",
        first_summary.memory_hash, second_summary.memory_hash
    );
    eprintln!(":: DBT path confirmed: DynamoRIO client reported tool=Detcore");
    eprintln!(":: Success: deterministic. Determinism verified.");
    if summary {
        eprint!("{}", format_dbt_stats(&first_summary));
    }
    // Propagate the guest's own (verified-identical) exit status rather than a
    // hardcoded 0. Before `--verify-allow` was threaded through, this line was
    // only reachable when both runs exited 0, so `Exited(0)` was equivalent; now
    // a deliberately non-zero guest (e.g. the `exit_status` parity contract with
    // `--verify-allow both`) can verify deterministically, and the DBT backend
    // must surface that status to match the ptrace `--verify` path
    // (`compare_two_runs` returns `out2.status`).
    Ok(output_status(&first))
}

#[allow(clippy::too_many_arguments)]
#[cfg(not(feature = "dbt"))]
pub(super) fn run_dbt(
    _program: &Path,
    _args: &[String],
    _verify: bool,
    _verify_allow: VerifyAllow,
    _summary: bool,
    _log: Option<LevelFilter>,
    _config: &Config,
    _environment: BTreeMap<OsString, OsString>,
) -> Result<ExitStatus, Error> {
    Err(Error::msg("DBT support was not included in this build"))
}

#[cfg(feature = "dbt")]
fn dbt_stdout_mismatch(first: &[u8], second: &[u8]) -> String {
    const CONTEXT_BEFORE: usize = 40;
    const CONTEXT_AFTER: usize = 120;

    let offset = first
        .iter()
        .zip(second)
        .position(|(left, right)| left != right)
        .unwrap_or_else(|| first.len().min(second.len()));
    let start = offset.saturating_sub(CONTEXT_BEFORE);
    let first_end = first.len().min(offset.saturating_add(CONTEXT_AFTER));
    let second_end = second.len().min(offset.saturating_add(CONTEXT_AFTER));

    format!(
        concat!(
            "DBT verification failed: guest stdout differed at byte {offset} ",
            "(run1_len={}, run2_len={}); run1[{start}..{first_end}]={:?}; ",
            "run2[{start}..{second_end}]={:?}"
        ),
        first.len(),
        second.len(),
        String::from_utf8_lossy(&first[start..first_end]),
        String::from_utf8_lossy(&second[start..second_end]),
        offset = offset,
        start = start,
        first_end = first_end,
        second_end = second_end,
    )
}

#[cfg(feature = "dbt")]
fn run_once<R: Read + Send + 'static>(
    runner: &DbtRunner,
    guest: &StdCommand,
    drrun: &Path,
    input: R,
) -> Result<Output, Error> {
    runner
        .output_with_detached_reader(guest, input)
        .map_err(|error| launch_error(drrun, error))
}

#[cfg(feature = "dbt")]
fn run_once_with_terminal_input(
    runner: &DbtRunner,
    guest: &StdCommand,
    drrun: &Path,
) -> Result<Output, Error> {
    runner
        .output_with_inherited_stdin(guest)
        .map_err(|error| launch_error(drrun, error))
}

#[cfg(feature = "dbt")]
fn launch_error(drrun: &Path, error: std::io::Error) -> Error {
    Error::msg(format!(
        "failed to launch drrun ({}): {error}",
        drrun.display()
    ))
}

#[cfg(feature = "dbt")]
fn process_status(status: std::process::ExitStatus) -> ExitStatus {
    ExitStatus::Exited(status.code().unwrap_or(1))
}

#[cfg(feature = "dbt")]
fn detcore_summary(output: &Output) -> Result<DbtSummary, Error> {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let summary = stderr
        .lines()
        .rev()
        .find(|line| line.starts_with("reverie-dbt: tool=Detcore "))
        .ok_or_else(|| {
            Error::msg(
                "DBT verification failed: native DynamoRIO summary did not report tool=Detcore",
            )
        })?;

    let field = |name: &str| {
        summary
            .split_ascii_whitespace()
            .find_map(|value| value.strip_prefix(name))
            .ok_or_else(|| Error::msg(format!("DBT verification failed: summary omitted {name}")))
    };
    let branches = field("branches=")?
        .parse::<u64>()
        .map_err(|_| Error::msg("DBT verification failed: invalid branch count"))?;
    let syscalls = field("syscalls=")?
        .parse::<u64>()
        .map_err(|_| Error::msg("DBT verification failed: invalid syscall count"))?;
    let rewritten = field("rewritten=")?
        .parse::<u64>()
        .map_err(|_| Error::msg("DBT verification failed: invalid rewritten count"))?;
    let stdin_reads = field("stdin_reads=")?
        .parse::<u64>()
        .map_err(|_| Error::msg("DBT verification failed: invalid stdin read count"))?;
    // A guest can reach the native callback without asking Detcore to suppress
    // a syscall. For example, a raw program whose only syscall is the native
    // lifecycle `exit` reports `rewritten=0`. The callback is still healthy as
    // long as it observed work and did not report more rewrites than syscalls.
    if branches == 0 || syscalls == 0 || rewritten > syscalls {
        return Err(Error::msg(
            "DBT verification failed: native callback counters are inconsistent",
        ));
    }

    let hash = field("memory_hash=")?;
    if hash.len() != 16 || u64::from_str_radix(hash, 16).is_err() {
        return Err(Error::msg(
            "DBT verification failed: invalid observed-memory hash",
        ));
    }
    Ok(DbtSummary {
        branches,
        syscalls,
        rewritten,
        stdin_reads,
        memory_hash: hash.to_owned(),
    })
}

#[cfg(feature = "dbt")]
fn write_output(output: &Output) -> Result<(), Error> {
    std::io::stdout().write_all(&output.stdout)?;
    std::io::stderr().write_all(&output.stderr)?;
    Ok(())
}

#[cfg(feature = "dbt")]
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

#[cfg(test)]
mod tests {
    #[cfg(feature = "dbt")]
    use std::os::unix::process::ExitStatusExt as _;

    use super::*;

    #[cfg(feature = "dbt")]
    fn write_executable(path: &Path, contents: &[u8]) {
        fs::write(path, contents).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[cfg(feature = "dbt")]
    fn dbt_summary(branches: u64) -> DbtSummary {
        DbtSummary {
            branches,
            syscalls: 169,
            rewritten: 168,
            stdin_reads: 0,
            memory_hash: "4b5e0e70f3050157".to_owned(),
        }
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_summary_treats_last_syscall_branch_count_as_telemetry() {
        assert!(dbt_summary(563_145).same_observable_behavior(&dbt_summary(563_103)));
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_stats_block_labels_counters_honestly() {
        let rendered = format_dbt_stats(&dbt_summary(563_145));
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
    fn dbt_stats_block_round_trips_from_a_client_summary_line() {
        let output = dbt_output(
            "reverie-dbt: tool=Detcore branches=42 syscalls=7 rewritten=6 \
             stdin_reads=0 memory_hash=cbf29ce484222325\n",
        );
        let parsed = detcore_summary(&output).unwrap();
        let rendered = format_dbt_stats(&parsed);
        assert!(rendered.contains("counted branches (deterministic branch clock): 42"));
        assert!(rendered.contains("syscalls intercepted:                          7"));
        assert!(
            rendered.contains("observed guest-memory hash:                    cbf29ce484222325")
        );
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_summary_compares_observable_counters_and_hash() {
        let expected = dbt_summary(100);

        let mut actual = dbt_summary(100);
        actual.syscalls += 1;
        assert!(!expected.same_observable_behavior(&actual));

        let mut actual = dbt_summary(100);
        actual.rewritten -= 1;
        assert!(!expected.same_observable_behavior(&actual));

        let mut actual = dbt_summary(100);
        actual.stdin_reads += 1;
        assert!(!expected.same_observable_behavior(&actual));

        let mut actual = dbt_summary(100);
        actual.memory_hash = "0000000000000000".to_owned();
        assert!(!expected.same_observable_behavior(&actual));
    }

    #[cfg(feature = "dbt")]
    fn dbt_output(summary: &str) -> Output {
        Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: Vec::new(),
            stderr: summary.as_bytes().to_vec(),
        }
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_summary_accepts_a_callback_without_rewritten_syscalls() {
        let output = dbt_output(
            "reverie-dbt: tool=Detcore branches=18 syscalls=1 rewritten=0 \
             stdin_reads=0 memory_hash=cbf29ce484222325\n",
        );

        let summary = detcore_summary(&output).unwrap();

        assert_eq!(summary.syscalls, 1);
        assert_eq!(summary.rewritten, 0);
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_summary_rejects_more_rewrites_than_syscalls() {
        let output = dbt_output(
            "reverie-dbt: tool=Detcore branches=18 syscalls=1 rewritten=2 \
             stdin_reads=0 memory_hash=cbf29ce484222325\n",
        );

        let error = detcore_summary(&output).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("native callback counters are inconsistent"),
            "{error}"
        );
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_stdout_mismatch_reports_offset_lengths_and_bounded_context() {
        let first = [vec![b'a'; 80], b"left-tail".to_vec()].concat();
        let second = [vec![b'a'; 80], b"right-tail-extra".to_vec()].concat();

        let detail = dbt_stdout_mismatch(&first, &second);

        assert!(detail.contains("differed at byte 80"), "{detail}");
        assert!(detail.contains("run1_len=89, run2_len=96"), "{detail}");
        assert!(detail.contains("run1[40..89]"), "{detail}");
        assert!(detail.contains("left-tail"), "{detail}");
        assert!(detail.contains("right-tail-extra"), "{detail}");
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_stdout_mismatch_reports_prefix_length_difference() {
        let detail = dbt_stdout_mismatch(b"same", b"same suffix");

        assert!(detail.contains("differed at byte 4"), "{detail}");
        assert!(detail.contains("run1_len=4, run2_len=11"), "{detail}");
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

        let prepared =
            prepare_dbt_guest_command(&script, &["argument".to_owned()], Some(bin.as_os_str()));

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

        let prepared = prepare_dbt_guest_command(&script, &[], Some(OsStr::new("/usr/bin")));

        assert_eq!(prepared.program, script);
        assert!(prepared.args.is_empty());
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
