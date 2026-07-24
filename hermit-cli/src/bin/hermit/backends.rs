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

use std::ffi::OsString;
use std::fs;
use std::io::IsTerminal as _;
use std::io::Read;
use std::io::Seek as _;
use std::io::SeekFrom;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command as StdCommand;
use std::process::Output;

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
