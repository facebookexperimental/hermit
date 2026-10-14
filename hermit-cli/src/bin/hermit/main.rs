/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Treat all Clippy warnings as errors.
#![deny(clippy::all)]
#![allow(clippy::uninlined_format_args)]
#![allow(
    unexpected_cfgs,
    reason = "`fbcode_build` is supplied by the internal Buck build"
)]

mod analyze;
mod backends;
mod bisect;
mod clean;
mod cli_owned_lifecycle;
mod container;
mod gdb_client;
mod gdb_watch_helper;
mod global_opts;
mod guest_capture;
mod host_capabilities;
mod host_seccomp;
mod image;
mod instruction_map;
mod list;
mod logdiff;
mod manifest;
mod oci;
mod owned_container;
mod podman_store;
mod record;
mod record_envelope;
mod record_start;
mod remove;
mod replay;
mod run;
mod run_config;
mod run_evidence;
mod run_timeout;
mod schedule_search;
mod staged_summary;
mod strace;
mod tracing;
mod verify;
mod version;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::FromRawFd;
use std::path::Path;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::Ordering;

const STDIN_UNCAPTURED: i32 = i32::MIN;
const STDIN_TAKEN: i32 = i32::MIN + 1;
static STARTUP_STDIN: AtomicI32 = AtomicI32::new(STDIN_UNCAPTURED);

unsafe extern "C" fn capture_startup_stdin() {
    // SAFETY: this runs single-threaded before Rust can sanitize a closed fd 0.
    let fd = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_DUPFD_CLOEXEC, 3) };
    let value = if fd >= 0 {
        fd
    } else {
        // SAFETY: fcntl failed in this thread, so errno contains its error.
        let errno = unsafe { *libc::__errno_location() };
        -errno - 1
    };
    STARTUP_STDIN.store(value, Ordering::Relaxed);
}

#[used]
#[unsafe(link_section = ".preinit_array")]
static CAPTURE_STARTUP_STDIN: unsafe extern "C" fn() = capture_startup_stdin;

fn startup_stdin() -> io::Result<Option<File>> {
    let value = STARTUP_STDIN.swap(STDIN_TAKEN, Ordering::AcqRel);
    if value >= 0 {
        // SAFETY: the startup hook created this owned descriptor and transfers it here once.
        return Ok(Some(unsafe { File::from_raw_fd(value) }));
    }
    if value == STDIN_UNCAPTURED || value == STDIN_TAKEN {
        return Err(io::Error::other(
            "startup stdin was not captured exactly once",
        ));
    }
    let errno = -value - 1;
    if errno == libc::EBADF {
        Ok(None)
    } else {
        Err(io::Error::from_raw_os_error(errno))
    }
}

/// Retire unsolicited caller descriptors before this exec image starts workers
/// or creates its own resources. Private exec roles supply only borrowed handles
/// from their already adopted owners; ordinary CLI entry supplies none.
fn retire_inherited_descriptors(owned: &[BorrowedFd<'_>]) -> io::Result<()> {
    let saved_stdin = STARTUP_STDIN.load(Ordering::Acquire);
    if saved_stdin == STDIN_UNCAPTURED || saved_stdin == STDIN_TAKEN {
        return Err(io::Error::other(
            "startup stdin reservation is unavailable at CLI entry",
        ));
    }
    // Negative capture results own no descriptor. Keep their encoding untouched:
    // startup_stdin still interprets EBADF as closed input and propagates every
    // other original error at the existing point that consumes the reservation.
    let stdin_owner = (saved_stdin >= 3).then_some(saved_stdin as u32);
    let mut first = 3;
    loop {
        let next_owner = stdin_owner
            .into_iter()
            .chain(owned.iter().map(|fd| fd.as_raw_fd() as u32))
            .filter(|fd| *fd >= first)
            .min();
        let last = next_owner.map_or(u32::MAX, |fd| fd - 1);
        if first <= last {
            // SAFETY: close_range accesses this process's descriptor table, not
            // memory. Entry is before workers; the retained owners remain alive
            // throughout the call. Flags0 actually removes the other entries.
            if unsafe { libc::syscall(libc::SYS_close_range, first, last, 0u32) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        match next_owner {
            Some(fd) => first = fd + 1,
            None => return Ok(()),
        }
    }
}

use clap::ArgMatches;
use clap::CommandFactory;
use clap::FromArgMatches;
use clap::Parser;
use clap::parser::ValueSource;
use colored::*;
use hermit::BackendUnavailable;
use hermit::Error;
use hermit::ExitStatus;
use hermit::HERMIT_INTERNAL_FAILURE_EXIT;

use self::analyze::AnalyzeOpts;
use self::bisect::BisectOpts;
use self::container::ContainerChildExit;
use self::container::ContainerChildPanic;
use self::container::LogCapExceeded;
use self::container::PolicyRefusal;
use self::container::RunTimeoutMarker;
use self::container::SignalDeath;
use self::global_opts::GlobalOpts;
use self::host_capabilities::HostCapabilitiesOpts;
use self::instruction_map::InstructionMapOpts;
use self::logdiff::LogDiffCLIOpts;
use self::oci::OciOpts;
use self::record::RecordOpts;
use self::replay::ReplayOpts;
use self::run::GuestProgramFault;
use self::run::RunOpts;
use self::run_evidence::RunEvidenceSession;
use self::strace::StraceOpts;
use self::verify::write_pending_verification_json;
use self::version::Version;
use self::version::VersionOpts;

#[derive(Debug, Parser)]
#[clap(
    name = "hermit",
    version = Version::get(),
)]
struct Args {
    #[clap(flatten)]
    global: GlobalOpts,

    #[clap(subcommand)]
    command: Subcommand,
}

/// Build parsed arguments while resolving an omitted `run --epoch` exactly
/// once at the process boundary. Keeping this above backend dispatch means a
/// `--verify` pair and every backend adapter share one concrete input.
fn args_from_matches_with_clock(
    matches: &ArgMatches,
    capture_now: impl FnOnce() -> std::time::SystemTime,
) -> Result<Args, clap::Error> {
    let run_matches = matches.subcommand_matches("run").or_else(|| {
        matches
            .subcommand_matches("oci")
            .and_then(|oci| oci.subcommand_matches("run"))
    });
    let run_epoch_source = run_matches.and_then(|run| run.value_source("epoch"));
    let mut args = Args::from_arg_matches(matches)?;
    // `--namespace-only` bypasses instrumentation, so an explicit backend would
    // be silently ignored. The backend is a global option, which clap's
    // per-subcommand `conflicts_with` cannot see, so refuse the pair here as the
    // same parse-time usage error.
    if let (Some(backend), Some(run)) = (args.global.backend, run_matches)
        && run.get_flag("namespace_only")
    {
        return Err(Args::command().error(
            clap::error::ErrorKind::ArgumentConflict,
            format!(
                "the argument '--backend={}' cannot be used with '--namespace-only': \
                 namespace-only mode bypasses instrumentation, so the backend would be \
                 ignored. Drop `--backend` to run namespace-only, or drop \
                 `--namespace-only` to run under the {} backend.",
                backend.as_str(),
                backend.as_str()
            ),
        ));
    }
    if run_epoch_source == Some(ValueSource::DefaultValue) {
        match &mut args.command {
            Subcommand::Run(run) => run.capture_default_epoch(capture_now),
            Subcommand::Oci(oci) => oci.capture_default_run_epoch(capture_now),
            _ => unreachable!("only run subcommands carry an epoch"),
        }
    }
    // `--save-config` records the options as given, which only the matches
    // still hold apart from defaults. `hermit oci run` refuses the option.
    // A run manifest records them too, so every top-level `hermit run`
    // captures them; only `--save-config` makes a failed capture (an option
    // value that is not UTF-8) an error, and otherwise the run writes no
    // manifest.
    if let (Subcommand::Run(run), Some(run_matches)) =
        (&mut args.command, matches.subcommand_matches("run"))
    {
        match run_config::capture(&Args::command(), matches, run_matches) {
            Ok(config) => run.set_config_capture(config),
            Err(error) if run.saves_config() => {
                return Err(Args::command().error(
                    clap::error::ErrorKind::ValueValidation,
                    format!("{error:#}"),
                ));
            }
            Err(_) => {}
        }
    }
    Ok(args)
}

/// Point a subcommand-level `--backend` at the global position.
///
/// `--backend` is a global option (`hermit --backend <BACKEND> <SUBCOMMAND>`) and
/// no subcommand declares its own, so clap already refuses it after a subcommand
/// as an unexpected argument. This keeps that refusal and its usage-error exit
/// status, and only replaces clap's generic hint and subcommand usage -- which
/// name an unrelated flag, `--backend-engagement-json` -- with the working
/// spelling: the caller's own command with `--backend` moved in front of the
/// subcommand, and the top-level usage that shows where global options go.
fn redirect_misplaced_backend(error: clap::Error, argv: &[std::ffi::OsString]) -> clap::Error {
    use clap::builder::StyledStr;
    use clap::error::ContextKind;
    use clap::error::ContextValue;
    use clap::error::ErrorKind;

    let Some(invalid) = misplaced_backend_argument(&error) else {
        return error;
    };
    let mut command = Args::command();
    let mut redirected = clap::Error::new(ErrorKind::UnknownArgument).with_cmd(&command);
    redirected.insert(ContextKind::InvalidArg, ContextValue::String(invalid));
    redirected.insert(
        ContextKind::Usage,
        ContextValue::StyledStr(command.render_usage()),
    );
    let corrected = globally_placed_backend(argv)
        .unwrap_or_else(|| "hermit --backend=<BACKEND> <SUBCOMMAND> [OPTIONS]".to_owned());
    redirected.insert(
        ContextKind::Suggested,
        ContextValue::StyledStrs(vec![StyledStr::from(format!(
            "`--backend` is a global option and must come before the subcommand: `{corrected}`"
        ))]),
    );
    redirected
}

/// The offending argument when clap refused a `--backend` it found after a
/// subcommand (`--backend` or `--backend=VALUE`), and `None` for every other
/// parse error, including similarly named flags such as
/// `--backend-engagement-json`.
pub(crate) fn misplaced_backend_argument(error: &clap::Error) -> Option<String> {
    use clap::error::ContextKind;
    use clap::error::ContextValue;
    use clap::error::ErrorKind;

    if error.kind() != ErrorKind::UnknownArgument {
        return None;
    }
    match error.get(ContextKind::InvalidArg) {
        Some(ContextValue::String(invalid))
            if invalid == "--backend" || invalid.starts_with("--backend=") =>
        {
            Some(invalid.clone())
        }
        _ => None,
    }
}

/// `argv` with its single `--backend` option moved directly after the program
/// name, rendered as a shell command. `None` when the option cannot be moved
/// unambiguously: it appears more than once before `--`, or its value is
/// missing, empty, or itself looks like an option (`run --backend --strict`
/// must not suggest `--backend=--strict`).
fn globally_placed_backend(argv: &[std::ffi::OsString]) -> Option<String> {
    let args = argv
        .iter()
        .map(|arg| arg.to_str())
        .collect::<Option<Vec<_>>>()?;
    let (program, rest) = args.split_first()?;
    let options_end = rest
        .iter()
        .position(|arg| *arg == "--")
        .unwrap_or(rest.len());
    let mut positions = rest[..options_end]
        .iter()
        .enumerate()
        .filter(|(_, arg)| **arg == "--backend" || arg.starts_with("--backend="))
        .map(|(index, _)| index);
    let index = positions.next()?;
    if positions.next().is_some() {
        return None;
    }
    let mut remaining = rest.to_vec();
    let backend = if remaining[index] == "--backend" {
        if index + 1 >= options_end {
            return None;
        }
        let value = remaining.remove(index + 1);
        remaining.remove(index);
        value
    } else {
        remaining.remove(index).strip_prefix("--backend=")?
    };
    if backend.is_empty() || backend.starts_with('-') {
        return None;
    }
    let flag = format!("--backend={backend}");
    let words = std::iter::once(*program)
        .chain(std::iter::once(flag.as_str()))
        .chain(remaining.iter().copied())
        .map(|word| {
            // Quote only what the shell would otherwise interpret, so the common
            // case reads as typed: `hermit --backend=ptrace run -- /bin/true`.
            let plain = !word.is_empty()
                && word
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_./:=,@%+".contains(&byte));
            if plain {
                word.to_owned()
            } else {
                shell_words::quote(word).into_owned()
            }
        })
        .collect::<Vec<_>>();
    Some(words.join(" "))
}

#[derive(Debug, Parser)]
enum Subcommand {
    /// Report the machine facilities that control validation selection.
    #[clap(name = "host-capabilities")]
    HostCapabilities(HostCapabilitiesOpts),

    /// Print version and build information.
    #[clap(name = "version")]
    Version(VersionOpts),

    /// Run a program sandboxed and fully deterministically (unless external networking is allowed).
    #[clap(name = "run", trailing_var_arg = true)]
    Run(Box<RunOpts>),

    /// Trace a program's syscalls through the selected backend.
    #[clap(name = "strace")]
    Strace(StraceOpts),

    /// Record the execution of a program (EXPERIMENTAL).
    #[clap(name = "record", trailing_var_arg = true)]
    Record(Box<RecordOpts>),

    /// Replay the execution of a program.
    #[clap(name = "replay")]
    Replay(ReplayOpts),

    /// Print one log canonically, or compare two run/record logs.
    ///
    /// COMPARING TWO SEPARATELY-PRODUCED RUNS: MAKE THE TWO COMMAND LINES
    /// BYTE-IDENTICAL, or you will measure your own inputs.
    ///
    /// The kernel places argv and the environment at the top of the initial
    /// process stack, so a command-line difference perturbs the guest before it
    /// executes a single instruction. That one fact shows up as two different
    /// failure modes, with two different triggers, and the obvious fix for the
    /// first does not fix the second:
    ///
    ///   * A LENGTH difference moves the entry stack pointer, so every stack
    ///     ADDRESS shifts. Measured: one byte of argv moved addresses by 32
    ///     bytes -- not one-for-one, because the stack is aligned -- and two
    ///     otherwise identical runs diverged 20 records in.
    ///
    ///   * A CONTENT difference at equal length leaves the addresses alone but
    ///     changes the stack BYTES, which --detlog-stack hashes. Measured: with
    ///     argv padded to equal length the stack range was identical in both
    ///     runs, yet the hash diverged 14 records in; with byte-identical argv
    ///     that same hash held for 5023 records.
    ///
    /// So padding to equal length fixes the first and leaves the second. Only
    /// byte-identical argv and environment fixes both. If the two runs must
    /// differ, put the difference somewhere other than the command line -- for
    /// example in a file the guest reads.
    ///
    /// `hermit run --verify` is NOT affected: it produces both runs from one
    /// invocation, so their command lines are identical by construction. This
    /// warning is for comparisons you assemble yourself.
    ///
    /// The general point, which outlives this particular trap: before reusing a
    /// control convention from earlier work, ask what the NEW measurement is
    /// sensitive to. Holding a run-directory name to a fixed width controls
    /// length but not content, and those are different properties.
    #[clap(verbatim_doc_comment)]
    LogDiff(LogDiffCLIOpts),

    /// Analyze Pass and failing runs
    Analyze(Box<AnalyzeOpts>),

    /// Bisect passing and failing schedules to localize a race.
    #[clap(name = "bisect", trailing_var_arg = true)]
    Bisect(Box<BisectOpts>),

    /// Generate a JSON map of nondeterministic instructions in an ELF binary.
    #[clap(name = "instruction-map")]
    InstructionMap(InstructionMapOpts),

    /// Discover and run OCI images from the local image store.
    #[clap(name = "oci")]
    Oci(Box<OciOpts>),
}

impl Subcommand {
    fn validate_backend_scope(&self, backend: Option<hermit::Backend>) -> Result<(), Error> {
        // `analyze` and `bisect` launch their trials through `RunOpts::run`,
        // not `RunOpts::main`, so a trial gets none of `main`'s per-backend
        // preparation: no e9patch rewrite, no availability preflight. Only a
        // backend whose trials are measured to run on it through that path is
        // in scope for them: KVM (measured 2026-09-30: `hermit --backend=kvm
        // analyze` ran its target trial on KVM). Before `run` lost its own `--backend`, this was spelled
        // `analyze --run-arg=--backend=kvm`.
        let runs_trials = matches!(self, Subcommand::Analyze(_) | Subcommand::Bisect(_));
        if backend == Some(hermit::Backend::Sabre)
            && !matches!(self, Subcommand::Strace(_) | Subcommand::Run(_))
        {
            // The predicate admits Strace AND Run, and `run` genuinely works --
            // measured 2026-08-06 on a 4-thread guest: `hermit --backend sabre run`
            // exited 0 with the correct deterministic result. The message used to
            // name only `strace`, so it told users a working path was unsupported
            // and hid real backend maturity. Message and predicate are now derived
            // from the same list; if the predicate changes, this text must too.
            anyhow::bail!(
                "the SaBRe backend is available only through `hermit --backend sabre run` \
                 and `hermit --backend sabre strace`"
            );
        }
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-696): Review the expanded e9patch CLI scope.
        let starts_e9patch_guest = matches!(self, Subcommand::Run(_))
            || matches!(self, Subcommand::Record(record) if record.starts_recording());
        if backend == Some(hermit::Backend::E9patch) && !starts_e9patch_guest {
            anyhow::bail!(
                "the e9patch preprocessor is available only through `hermit --backend e9patch \
                 run` and `hermit --backend e9patch record`; other subcommands, including \
                 `analyze` and `bisect`, do not preprocess their guest"
            );
        }
        if let Some(backend) = backend.filter(|backend| backend.is_in_guest_liteinst())
            && !matches!(self, Subcommand::Run(_))
        {
            anyhow::bail!(
                "the LiteInst preload backend is available only through `hermit --backend \
                 {backend} run`; other subcommands, including `analyze` and `bisect`, do not \
                 use the preload runtime",
                backend = backend.as_str()
            );
        }
        if backend == Some(hermit::Backend::Kvm)
            && !(matches!(self, Subcommand::Run(_)) || runs_trials)
        {
            anyhow::bail!(
                "the KVM backend is available only through `hermit --backend kvm run`, \
                 `hermit --backend kvm analyze` and `hermit --backend kvm bisect`; record and \
                 replay require the ptrace runtime's sequentialized scheduler"
            );
        }
        if backend == Some(hermit::Backend::Dbt) && !matches!(self, Subcommand::Run(_)) {
            anyhow::bail!(
                "the DBT backend is available only through `hermit --backend dbt run`; record \
                 and replay use the ptrace runtime, and `analyze` and `bisect` bind a \
                 workspace into every trial, which DBT cannot apply"
            );
        }
        Ok(())
    }

    /// Whether this invocation starts a guest, and so must first check for an
    /// inherited seccomp filter (`host_seccomp::admit`).
    fn starts_guest(&self) -> bool {
        match self {
            Subcommand::Run(_)
            | Subcommand::Strace(_)
            | Subcommand::Replay(_)
            | Subcommand::Analyze(_)
            | Subcommand::Bisect(_) => true,
            Subcommand::Record(record) => record.starts_recording(),
            Subcommand::Oci(oci) => oci.starts_guest(),
            Subcommand::HostCapabilities(_)
            | Subcommand::Version(_)
            | Subcommand::LogDiff(_)
            | Subcommand::InstructionMap(_) => false,
        }
    }

    /// The `--verify-json` path this invocation will publish a verdict to, if
    /// any. Only the two subcommands that can produce a verification verdict
    /// have one.
    fn verification_json_path(&self) -> Option<&Path> {
        match self {
            Subcommand::Run(run) => run.verify_json_path(),
            Subcommand::Record(record) => record.verify_json_path(),
            _ => None,
        }
    }

    fn run_evidence_request(
        &self,
        global_backend: Option<hermit::Backend>,
    ) -> Option<(&Path, hermit::Backend)> {
        match self {
            Subcommand::Run(run) => run.run_evidence_request(global_backend),
            _ => None,
        }
    }

    fn main(&mut self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        // Stamp the invocation-bound NO-RESULT record BEFORE the first fallible
        // statement of the whole program. This is the outermost point at which
        // `--verify-json` is known, and it is the only placement that dominates
        // every path that can exit without a verdict:
        //
        //   * `validate_backend_scope` immediately below;
        //   * `RunOpts::main`'s preflight -- log-level validation, stdin
        //     reservation, `validate_args`, backend availability, PMU config,
        //     mount-source and program validation, happens-before resolution,
        //     e9patch preparation;
        //   * the DBT arm, which returns `run_dbt(..)` and therefore must
        //     publish its verdict through that dedicated path;
        //   * `--namespace-only`, which likewise bypasses `verify()`;
        //   * `StartOpts::main`'s own pre-validation before `record_verify`.
        //
        // Stamping as the first statement of `verify()`/`record_verify()` did
        // NOT cover any of those: they all exit above it, leaving a previous
        // invocation's `{verified:true}` at the path to be read as this run's
        // result. If the stamp itself cannot be written we fail here rather than
        // run, so the operator learns the artifact is unreliable instead of
        // silently inheriting a stale one.
        if let Some(path) = self.verification_json_path() {
            write_pending_verification_json(path)?;
        }
        self.validate_backend_scope(global.backend)?;
        // After the verdict stamp above, so a refused run leaves a no-result
        // verdict rather than a previous run's, and before anything starts a
        // guest or reads its stdin.
        if self.starts_guest() {
            host_seccomp::admit(global.unsafe_ignore_host_seccomp)?;
        }
        match self {
            Subcommand::HostCapabilities(x) => x.main(),
            Subcommand::Version(x) => x.main(),
            Subcommand::Run(x) => x.main(global),
            Subcommand::Strace(x) => x.main(global),
            Subcommand::Record(x) => x.main(global),
            Subcommand::Replay(x) => x.main(global),
            Subcommand::LogDiff(x) => Ok(x.main(global)),
            Subcommand::Analyze(x) => x.main(global),
            Subcommand::Bisect(x) => x.main(global),
            Subcommand::InstructionMap(x) => x.main(global),
            Subcommand::Oci(x) => x.main(global),
        }
    }
}

#[fbinit::main]
fn main() {
    if let Some(status) = cli_owned_lifecycle::maybe_run() {
        std::process::exit(status);
    }
    if let Some(result) = gdb_watch_helper::maybe_run() {
        match result {
            Ok(()) => return,
            Err(error) => {
                display_entry_error("GDB helper failed", &error);
                std::process::exit(125);
            }
        }
    }
    if let Err(error) = retire_inherited_descriptors(&[]) {
        let argv = std::env::args_os().collect::<Vec<_>>();
        std::process::exit(refuse_entry_descriptor_failure(error, &argv));
    }
    // ⚠️ BEFORE ANYTHING THAT CAN FORK. The stderr diagnostic deadline is a total
    // for the INVOCATION, and the origin every hermit process measures from is a
    // shared mapping that children inherit across fork. A mapping made after the
    // first fork is not shared with the child that already exists, so each would
    // get its own origin and the deadline would be multiplied by the number of
    // container inits -- three on `--verify`, which forks once per run. This
    // placement dominates every with_container and run_guarded_at site.
    detcore::util::init_shared_stderr_deadline_origin();
    // Parse through `ArgMatches` so the run command can distinguish an
    // omitted epoch from an explicit CLI/environment value. `Config` keeps a
    // stable library default for wire fingerprints and unit fixtures; only an
    // actual `hermit run` invocation captures host wall time.
    let argv = std::env::args_os().collect::<Vec<_>>();
    // `hermit run --config FILE` means the command line the file names, so
    // it is rewritten into that command line before anything is parsed.
    let (argv, config_applied) = match run_config::expand_argv(&Args::command(), &argv) {
        Ok(Some(expanded)) => (expanded, true),
        Ok(None) => (argv, false),
        Err(error) => Args::command()
            .error(
                clap::error::ErrorKind::ValueValidation,
                format!("{error:#}"),
            )
            .exit(),
    };
    let matches = Args::command()
        .try_get_matches_from(&argv)
        .unwrap_or_else(|error| {
            let error = redirect_misplaced_backend(error, &argv);
            if config_applied && error.use_stderr() {
                let _ = error.print();
                eprintln!(
                    "note: the options of the --config file count as given on the command \
                     line, and an option given on the command line replaces the file's value"
                );
                std::process::exit(error.exit_code());
            }
            error.exit()
        });
    let Args {
        mut global,
        mut command,
    } = args_from_matches_with_clock(&matches, std::time::SystemTime::now)
        .unwrap_or_else(|error| error.exit());

    // Claim the evidence destination before any fallible run preflight. The
    // directory itself is the invocation boundary: it must not exist, so no
    // stale success can survive and no concurrent invocation can share it.
    let evidence = match command.run_evidence_request(global.backend) {
        Some((directory, backend)) => match RunEvidenceSession::create(directory, backend) {
            Ok(session) => Some(session),
            Err(error) => {
                display_error(error);
                ExitStatus::Exited(HERMIT_INTERNAL_FAILURE_EXIT).raise_or_exit();
            }
        },
        None => None,
    };
    if let Some(session) = &evidence {
        global.set_run_evidence_log_handle(session.log_handle(), session.write_error_latch());
    }

    // Open --log-file HERE, in the host's filename namespace, before any container
    // exists. This is the moment a shell would perform `> file`, and doing it later
    // -- inside the container, where tracing must be initialized -- resolves the path
    // against the guest's fresh /tmp and silently discards the log.
    // The --max-log-bytes counter is mapped here for the same reason: it must
    // exist before the first container fork to be shared by every run.
    if global.max_log_bytes.is_some() {
        self::tracing::enable_log_cap_exit_bound();
    }
    // Registered before any fork, so a container init can check the crossing
    // bit right after it arms its death signal.
    let result = global
        .prepare_log_budget()
        .map(|()| self::tracing::register_invocation_log_budget(global.log_budget()))
        .and_then(|()| global.open_log_file())
        .and_then(|()| command.main(&global));

    // The manifest is the commit marker and is published only after the run
    // result and private log are final. Evidence failure is reported but cannot
    // replace the guest's status; a consumer observes the absent/no-result
    // manifest and fails closed independently of the public CLI channel.
    let evidence_error = evidence.and_then(|session| session.finish(result.as_ref()).err());
    let status = result.unwrap_or_else(|error| {
        let status = ExitStatus::Exited(failure_exit_code(&error));
        display_error(error);
        status
    });
    if let Some(error) = evidence_error {
        display_error(error);
    }
    // From here on a write may fail but must not replace `status`.
    block_write_signals_until_exit();
    // Guest output agreed by both `--verify` runs can still be buffered (no
    // final newline), and `exit` would flush it after the report, behind
    // whatever room the report took. As at exit, a failed flush is ignored.
    let _ = std::io::Write::flush(&mut std::io::stdout());
    // Last, so that nothing hermit must say waits behind it.
    verify::write_queued_signal_reports();
    status.raise_or_exit();
}

fn stamp_entry_descriptor_failure(argv: &[std::ffi::OsString]) -> Result<(), Error> {
    let expanded = run_config::expand_argv(&Args::command(), argv)?;
    let argv = expanded.as_deref().unwrap_or(argv);
    let matches = Args::command().try_get_matches_from(argv)?;
    let args = args_from_matches_with_clock(&matches, std::time::SystemTime::now)?;
    if let Some(path) = args.command.verification_json_path() {
        write_pending_verification_json(path)?;
    }
    Ok(())
}

fn refuse_entry_descriptor_failure(error: io::Error, argv: &[std::ffi::OsString]) -> i32 {
    block_write_signals_until_exit();
    // Stamp before diagnostic output. Parsing serves only this pending record:
    // neither clap's help/error exit nor a secondary failure replaces the saved
    // cleanup errno and125. No normal CLI setup or guest execution follows.
    let stamp = stamp_entry_descriptor_failure(argv);
    display_entry_error(
        "Inherited descriptor cleanup failed; use a Linux environment that allows close_range",
        &error.into(),
    );
    if let Err(secondary) = stamp {
        display_entry_error("Entry-failure verification stamp failed", &secondary);
    }
    HERMIT_INTERNAL_FAILURE_EXIT
}

fn display_entry_error(context: &str, error: &Error) {
    block_write_signals_until_exit();
    // This entry failure must terminate even if inherited stderr is full,
    // blocking or broken. The existing one-attempt writer neither panics nor
    // waits for a reader, changes shared stream flags or renews a deadline.
    let report = format!("{context}: {error:#}\n");
    hermit::nonwaiting_write::write_without_waiting(libc::STDERR_FILENO, report.as_bytes());
}

/// Block `SIGXFSZ` and `SIGPIPE` for the rest of the process. A write of
/// required output to a stdout file already at its `RLIMIT_FSIZE`, or to a
/// closed reader, would otherwise kill hermit with that signal in place of the
/// status it is about to propagate. Blocked, the write fails with `EFBIG` or
/// `EPIPE` instead. That includes the flush `exit` retries when a buffered
/// write failed, so the block lasts until exit rather than one write.
/// Dispositions are unchanged and nothing is consumed, so a signal already
/// pending stays pending. `raise_or_exit` unblocks the guest's own signal after
/// raising it, so a guest that died of either still has that signal propagated.
/// `SIGTTOU` stays unblocked, so a background hermit still stops on a `TOSTOP`
/// terminal.
fn block_write_signals_until_exit() {
    // SAFETY: sigset operations on a local, initialized set; pthread_sigmask
    // affects only the calling thread, which is the one that writes and exits.
    unsafe {
        let mut blocked: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut blocked);
        libc::sigaddset(&mut blocked, libc::SIGXFSZ);
        libc::sigaddset(&mut blocked, libc::SIGPIPE);
        libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, std::ptr::null_mut());
    }
}

/// Machine-readable classification of a hermit-internal failure, on stderr.
///
/// ⚠️ WHY A STDERR MARKER AND NOT A SECOND EXIT CODE. Every value in `0..=255`
/// is a legal guest exit status and both channels a process has — code and
/// terminating signal — are already spoken for by the guest, so exit codes can
/// only ever REDUCE collisions, never remove them. Spending a second reserved
/// number to separate two *internal* failures buys the least and costs the most.
///
/// ⚠️ AND WHY NOT THE VERIFICATION JSON, which is the other candidate and looks
/// structurally stronger. Measured on `main` at `b92c2227fc`, it cannot see
/// these cases at all:
///   * `--verify-json` is clap-`requires`-gated on `--verify`, so a plain
///     `hermit run` has no channel — measured: `error: the following required
///     arguments were not provided: --verify`;
///   * with both flags, an `--log-file` failure still writes NO record — the
///     stamp lives inside `Subcommand::main`, and `open_log_file` fails above it;
///   * a bad flag never reaches hermit's code at all (clap exits 2 itself).
///
/// A channel that is absent for the common invocation cannot be the mechanism
/// that distinguishes failure classes. It remains the right home for a RICH
/// verdict when a path was requested; it is not an alternative to this.
///
/// stderr has neither limitation: it always exists, it has no 256-value ceiling,
/// and it needs no new plumbing — only that the typed status stop being
/// discarded, which is what [`ContainerChildExit`] now prevents.
/// The exit code for a failure, DERIVED FROM ITS CLASS rather than hardcoded.
///
/// ⚠️ THIS IS THE PIECE THAT WAS MISSING, AND ITS ABSENCE IS WHY ONE DEFECT KEPT
/// REAPPEARING. The failure's class is reconstructed at three boundaries and was
/// then thrown away again here, because the exit-code decision was a constant
/// that consulted nothing. Every new distinction therefore needed its own patch.
/// Expressed as a mapping FROM the class, the next distinction is a line in this
/// match instead of another fix to the same shape.
///
/// Most classes deliberately still map to 125: separating two INTERNAL failures
/// does not justify spending a reserved number, since every value in `0..=255`
/// is a legal guest status — that is the argument at
/// [`HERMIT_INTERNAL_FAILURE_EXIT`] and it is unchanged. A GUEST-SIDE fault is
/// the case that does justify one, and 127/126 are the established GNU
/// `env`/`chroot`/`timeout` conventions that 125 itself came from. The two
/// positions are consistent: the code channel exists to tell hermit's failures
/// apart from the guest's, not to subdivide hermit's own.
fn failure_exit_code(error: &Error) -> i32 {
    // A completed comparison with a recorded PMU skid overshoot is an
    // understood infrastructure failure. Hermit deliberately declines to
    // vouch for the result, so it uses the same coarse exit class as other
    // policy refusals; the verification receipt carries the finer cause and
    // count.
    if error.downcast_ref::<hermit::SkidOvershootError>().is_some() {
        return detcore_model::HERMIT_POLICY_REFUSAL_EXIT;
    }
    // ⚠️ A REFUSAL IS NOT A FAILURE, AND IT IS CHECKED FIRST FOR THAT REASON.
    // A fail-closed policy stopping the run is hermit working; reporting it as
    // HERMIT_INTERNAL_FAILURE_EXIT tells the operator to file a bug against a
    // shutdown path that behaved exactly as designed.
    if error.downcast_ref::<PolicyRefusal>().is_some() {
        return detcore_model::HERMIT_POLICY_REFUSAL_EXIT;
    }
    // A signal death reports 128 + signo, the status the shell would have shown
    // had the container been an ordinary process. Also not a failure.
    if let Some(SignalDeath(signal)) = error.downcast_ref::<SignalDeath>() {
        return detcore_model::signal_exit_status(*signal);
    }
    // ⚠️ A DEADLINE IS NOT A HERMIT FAILURE EITHER. The bound the caller asked
    // for was reached and hermit tore the container down on purpose, so
    // reporting HERMIT_INTERNAL_FAILURE_EXIT would send the operator to file a
    // bug against a mechanism that did its job. 124 is the established code for
    // "a deadline fired" -- GNU `timeout`, `safehermit`'s wall bound, and
    // `hermit record --record-timeout` all already use it, so this adds no new
    // meaning to the number.
    if error.downcast_ref::<RunTimeoutMarker>().is_some() {
        return 124;
    }
    // The --max-log-bytes cap is a bound working as designed, like the deadline.
    if error.downcast_ref::<LogCapExceeded>().is_some() {
        return hermit::HERMIT_LOG_CAP_EXIT;
    }
    match error.downcast_ref::<GuestProgramFault>() {
        Some(fault) => fault.exit_code(),
        None => HERMIT_INTERNAL_FAILURE_EXIT,
    }
}

fn classify_failure(error: &Error) -> String {
    if let Some(overshoot) = error.downcast_ref::<hermit::SkidOvershootError>() {
        return format!(
            "HERMIT_POLICY_REFUSAL class=policy-refusal cause=skid-overshoot count={}",
            overshoot.count()
        );
    }
    // ⚠️ REFUSAL BEFORE FAILURE. This arm and the ContainerChildExit arm below
    // describe the SAME observation -- a container child that exited -- and
    // only the status separates them. Ordering matters: a refusal that fell
    // through to the arm below would be reported as an accident, which is the
    // defect this exists to remove.
    if error.downcast_ref::<PolicyRefusal>().is_some() {
        // NOT HERMIT_INTERNAL_FAILURE: hermit did not fail. It refused, on
        // purpose, and printed why immediately above this line.
        return "HERMIT_POLICY_REFUSAL class=policy-refusal".to_string();
    }
    // ⚠️ THE MARKER IS WHAT MAKES 130 UNAMBIGUOUS, exactly as it is for 122. A
    // guest may exit 130 of its own accord and hermit cannot stop it; the status
    // alone therefore cannot say who produced it. `signal=` is included because
    // the band has several members and the number is the report.
    if let Some(SignalDeath(signal)) = error.downcast_ref::<SignalDeath>() {
        return format!("HERMIT_SIGNAL_DEATH class=signal-death signal={}", signal);
    }
    // NOT HERMIT_INTERNAL_FAILURE: the bound fired and hermit unwound the
    // container deliberately. The seconds are in the line because "it timed
    // out" without the bound it exceeded is the anonymous kill this replaces.
    if error.downcast_ref::<RunTimeoutMarker>().is_some() {
        return "HERMIT_RUN_TIMEOUT class=run-timeout".to_string();
    }
    // NOT HERMIT_INTERNAL_FAILURE either: the requested log cap fired and the
    // writer that crossed it tore the run down. Its message with the bound is
    // best effort and may be missing.
    if error.downcast_ref::<LogCapExceeded>().is_some() {
        return "HERMIT_LOG_CAP class=log-cap".to_string();
    }
    // ONE discriminant, read once, covering all three flattenings.
    if let Some(ContainerChildExit(status)) = error.downcast_ref::<ContainerChildExit>() {
        // The child died with a status IT DID NOT CHOOSE: a kill, a fault, a
        // panic no handler caught. Nothing reported it; reverie observed it.
        return format!("HERMIT_INTERNAL_FAILURE class=container-child-exit status={status:?}");
    }
    if let Some(fault) = error.downcast_ref::<GuestProgramFault>() {
        // ⚠️ A GUEST-SIDE FACT, NOT A HERMIT FAILURE. Hermit worked; the path it
        // was handed is not executable. Bucketing this with `cli-error` told a
        // caller "hermit is broken" for what is almost always a typo.
        return format!("HERMIT_INTERNAL_FAILURE class={}", fault.class());
    }
    if let Some(unavailable) = error.downcast_ref::<BackendUnavailable>() {
        return format!(
            "HERMIT_INTERNAL_FAILURE class=backend-unavailable backend={}",
            unavailable.backend().as_str()
        );
    }
    if error.downcast_ref::<ContainerChildPanic>().is_some() {
        // The child PANICKED and the panic was caught and reported. Still the
        // tracer breaking, not the CLI refusing -- which is the distinction the
        // `kind` discriminant on SerializableError exists to carry.
        return "HERMIT_INTERNAL_FAILURE class=container-child-panic".to_string();
    }
    // Everything else: bad flag, unwritable log path, unreadable program.
    "HERMIT_INTERNAL_FAILURE class=cli-error".to_string()
}

fn display_error(error: Error) {
    // NOT `eprintln!` on any of these three.
    //
    // ⚠️ THIS IS THE LAST THING HERMIT SAYS, AND A GUEST CAN CUT IT OFF. fd 2 is
    // an open file description shared with the guest, so `fcntl(2, F_SETFL,
    // O_NONBLOCK)` in the guest makes these writes fail with EAGAIN once the
    // reader is slow. `eprintln!` calls `write_all`, `write_all` does not retry
    // EAGAIN, and the print macro panics -- taking the rest of this report with
    // it. Measured 2026-08-26 against a 4096-byte pipe with 196 bytes free: 170
    // of 2240 bytes arrived, the process exited 101 instead of 127, and the
    // delivered text was not even a prefix of the truth -- the first 92 bytes of
    // this report were followed by the tail of the panic message, so it read as
    // a complete short error rather than a severed one.
    //
    // The ordering below (class first) was already written for a truncated
    // capture. Ordering limits the damage; it does not prevent it.
    use std::io::Write;
    if error.downcast_ref::<LogCapExceeded>().is_some() {
        // A run the log cap ended must exit 123 promptly even when stderr is
        // a full pipe nobody reads, so this one report is attempted once and
        // never waits (RetryingStderr below retries until it is delivered).
        let mut report = classify_failure(&error);
        report.push('\n');
        for (index, cause) in error.chain().enumerate() {
            report.push_str(&if index == 0 {
                format!("{}: {}\n", "Error".red().bold(), cause)
            } else {
                format!("     {} {}\n", ">".dimmed().bold(), cause)
            });
        }
        self::tracing::report_log_cap_without_waiting(&report);
        return;
    }
    let mut out = detcore::util::RetryingStderr;
    let _ = writeln!(out, "{}", classify_failure(&error));

    let mut chain = error.chain();

    if let Some(error) = chain.next() {
        let _ = writeln!(out, "{}: {}", "Error".red().bold(), error);
    }

    for cause in chain {
        let _ = writeln!(out, "     {} {}", ">".dimmed().bold(), cause);
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;
    use clap::Parser;
    use hermit::Backend;
    use hermit::BackendUnavailable;
    use hermit::Error;

    use super::Args;
    use super::Subcommand;
    use super::args_from_matches_with_clock;
    use super::classify_failure;
    use super::failure_exit_code;

    #[derive(Debug, PartialEq, Eq)]
    struct EntryFdSnapshot {
        device: libc::dev_t,
        inode: libc::ino_t,
        mode: libc::mode_t,
        descriptor_flags: i32,
        file_flags: i32,
        offset: libc::off_t,
    }

    // Only stack values and async-signal-safe syscalls are used in the forked
    // entry checks. No allocation, panic/unwind or libtest work runs there.
    fn entry_fd_snapshot(fd: i32) -> Option<EntryFdSnapshot> {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: fstat writes exactly this valid stat buffer; fcntl/lseek access
        // only the supplied descriptor. SEEK_CUR with offset0 changes no offset.
        unsafe {
            if libc::fstat(fd, stat.as_mut_ptr()) < 0 {
                return None;
            }
            let stat = stat.assume_init();
            let descriptor_flags = libc::fcntl(fd, libc::F_GETFD);
            let file_flags = libc::fcntl(fd, libc::F_GETFL);
            if descriptor_flags < 0 || file_flags < 0 {
                return None;
            }
            Some(EntryFdSnapshot {
                device: stat.st_dev,
                inode: stat.st_ino,
                mode: stat.st_mode,
                descriptor_flags,
                file_flags,
                offset: libc::lseek(fd, 0, libc::SEEK_CUR),
            })
        }
    }

    fn entry_fd_is_closed(fd: i32) -> bool {
        // SAFETY: fcntl touches no memory; its errno is read immediately.
        (unsafe { libc::fcntl(fd, libc::F_GETFD) }) < 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::EBADF)
    }

    fn check_single_threaded_entry_child(check: impl FnOnce() -> i32) {
        // SAFETY: the child has its own fdtable and only one surviving thread.
        // Its callback uses stack operations/syscalls only, then _exit; it never
        // unwinds or enters the multithreaded parent test runner.
        let pid = unsafe { libc::fork() };
        assert!(
            pid >= 0,
            "fork entry check: {}",
            std::io::Error::last_os_error()
        );
        if pid == 0 {
            let status = check();
            // SAFETY: terminate only this owned child without Rust destructors.
            unsafe { libc::_exit(status) };
        }
        let mut status = 0;
        loop {
            // SAFETY: wait only for this exact owned child, with valid output.
            let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
            if waited < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            assert_eq!(waited, pid);
            break;
        }
        assert_eq!(status, 0, "entry check child returned raw status {status}");
    }

    #[test]
    fn entry_retires_unsolicited_fds_and_preserves_owned_streams() {
        use std::io::Seek;
        use std::os::fd::AsFd;
        use std::os::fd::AsRawFd;
        use std::os::fd::FromRawFd;
        use std::os::unix::net::UnixStream;

        let mut streams = [
            tempfile::tempfile().unwrap(),
            tempfile::tempfile().unwrap(),
            tempfile::tempfile().unwrap(),
        ];
        for (index, file) in streams.iter_mut().enumerate() {
            file.seek(std::io::SeekFrom::Start(7 + index as u64))
                .unwrap();
        }
        let (control, peer) = UnixStream::pair().unwrap();
        // SAFETY: pidfd_open names this live process and returns an owned fd.
        let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, libc::getpid(), 0u32) } as i32;
        assert!(
            pidfd >= 0,
            "pidfd_open: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: this successful syscall transfers ownership exactly once.
        let parent = unsafe { std::os::fd::OwnedFd::from_raw_fd(pidfd) };
        let unsolicited = [tempfile::tempfile().unwrap(), tempfile::tempfile().unwrap()];
        let parent_fds = [
            streams[0].as_raw_fd(),
            streams[1].as_raw_fd(),
            streams[2].as_raw_fd(),
            control.as_raw_fd(),
            peer.as_raw_fd(),
            parent.as_raw_fd(),
            unsolicited[0].as_raw_fd(),
            unsolicited[1].as_raw_fd(),
        ];
        let parent_before = parent_fds.map(entry_fd_snapshot);
        assert!(parent_before.iter().all(Option::is_some));
        check_single_threaded_entry_child(|| {
            for (fd, file) in streams.iter().enumerate() {
                // SAFETY: dup2 changes only the child's table. The three distinct
                // owned files give positions unaffected by parent harness output.
                if unsafe { libc::dup2(file.as_raw_fd(), fd as i32) } < 0 {
                    return 1;
                }
                // Include different descriptor flags in the preserved stdio.
                if unsafe {
                    libc::fcntl(
                        fd as i32,
                        libc::F_SETFD,
                        if fd == 1 { libc::FD_CLOEXEC } else { 0 },
                    )
                } < 0
                {
                    return 2;
                }
            }
            // SAFETY: this child is single-threaded and fd0 now names its own
            // controlled input; execute the actual preinit capture operation.
            unsafe { super::capture_startup_stdin() };
            let saved = super::STARTUP_STDIN.load(std::sync::atomic::Ordering::Acquire);
            if saved < 3 {
                return 3;
            }
            let stdio_before = [0, 1, 2].map(entry_fd_snapshot);
            let kept = [saved, control.as_raw_fd(), parent.as_raw_fd()];
            let kept_before = kept.map(entry_fd_snapshot);
            if kept_before.iter().any(Option::is_none) {
                return 4;
            }
            // Deliberately reverse the two typed helper owners; range closure
            // must not assume their numeric order or exempt a broad fd range.
            if super::retire_inherited_descriptors(&[parent.as_fd(), control.as_fd()]).is_err() {
                return 5;
            }
            if super::STARTUP_STDIN.load(std::sync::atomic::Ordering::Acquire) != saved
                || [0, 1, 2].map(entry_fd_snapshot) != stdio_before
                || kept.map(entry_fd_snapshot) != kept_before
            {
                return 6;
            }
            let removed = [
                streams[0].as_raw_fd(),
                streams[1].as_raw_fd(),
                streams[2].as_raw_fd(),
                peer.as_raw_fd(),
                unsolicited[0].as_raw_fd(),
                unsolicited[1].as_raw_fd(),
            ];
            if removed.into_iter().any(|fd| !entry_fd_is_closed(fd)) {
                return 7;
            }
            match super::startup_stdin() {
                Ok(Some(file))
                    if file.as_raw_fd() == saved && entry_fd_snapshot(saved) == kept_before[0] =>
                {
                    0
                }
                _ => 8,
            }
        });
        // The child neither closed the calling process's entries nor changed
        // the retained file descriptions' flags or controlled positions.
        assert_eq!(parent_fds.map(entry_fd_snapshot), parent_before);
    }

    #[test]
    fn entry_leaves_captured_stdin_errors_for_the_original_consumer() {
        use std::os::fd::AsRawFd;

        let unsolicited = tempfile::tempfile().unwrap();
        for closed in [true, false] {
            check_single_threaded_entry_child(|| {
                // SAFETY: this changes only the child's stdin or fd limit. A
                // limit of3 makes the actual F_DUPFD_CLOEXEC(min3) return EINVAL.
                let changed = unsafe {
                    if libc::dup2(unsolicited.as_raw_fd(), 0) < 0 {
                        return 1;
                    }
                    if closed {
                        libc::close(0)
                    } else {
                        let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
                        if libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) < 0 {
                            return 1;
                        }
                        let mut limit = limit.assume_init();
                        limit.rlim_cur = 3;
                        libc::setrlimit(libc::RLIMIT_NOFILE, &limit)
                    }
                };
                if changed < 0 {
                    return 2;
                }
                // SAFETY: same single-threaded actual capture as at CLI entry.
                unsafe { super::capture_startup_stdin() };
                let saved = super::STARTUP_STDIN.load(std::sync::atomic::Ordering::Acquire);
                let expected_errno = if closed { libc::EBADF } else { libc::EINVAL };
                if saved != -expected_errno - 1 {
                    return 3;
                }
                if super::retire_inherited_descriptors(&[]).is_err()
                    || super::STARTUP_STDIN.load(std::sync::atomic::Ordering::Acquire) != saved
                    || !entry_fd_is_closed(unsolicited.as_raw_fd())
                    || (closed && !entry_fd_is_closed(0))
                {
                    return 4;
                }
                match super::startup_stdin() {
                    Ok(None) if closed => 0,
                    Err(error) if !closed && error.raw_os_error() == Some(expected_errno) => 0,
                    _ => 5,
                }
            });
        }
        assert!(entry_fd_snapshot(unsolicited.as_raw_fd()).is_some());
    }

    #[test]
    fn entry_descriptor_failure_can_stamp_no_result_without_running_a_guest() {
        use std::os::fd::AsRawFd;
        use std::os::fd::FromRawFd;
        use std::os::fd::OwnedFd;
        use std::os::unix::process::CommandExt;
        use std::process::Command;
        use std::process::Stdio;
        use std::time::Duration;
        use std::time::Instant;

        const REPORT: &str = "HERMIT_TEST_ENTRY_DESCRIPTOR_REPORT";
        const MODE: &str = "HERMIT_TEST_ENTRY_DESCRIPTOR_MODE";
        if let Some(report) = std::env::var_os(REPORT) {
            // A fresh exec image permits allocation/parsing without entering a
            // forked copy of the multithreaded parent runner. The actual syscall
            // is denied by its test-owned pre-exec filter below.
            let error = super::retire_inherited_descriptors(&[]).unwrap_err();
            assert_eq!(error.raw_os_error(), Some(libc::EPERM));
            let argv = if std::env::var(MODE).unwrap() == "help" {
                vec!["hermit".into(), "--help".into()]
            } else {
                vec![
                    "hermit".into(),
                    "run".into(),
                    "--verify".into(),
                    "--verify-json".into(),
                    report,
                    "--".into(),
                    "/bin/true".into(),
                ]
            };
            // Exercise the shared production refusal handler, not production
            // main's private-role dispatch or a guest. Never return to libtest.
            std::process::exit(super::refuse_entry_descriptor_failure(error, &argv));
        }

        let directory = tempfile::tempdir().unwrap();
        let report = directory.path().join("verification.json");
        for mode in ["verify", "help"] {
            std::fs::write(&report, b"{\"verified\":true}").unwrap();
            let mut pipe = [-1; 2];
            // SAFETY: a valid two-fd buffer; both successful outputs are owned.
            assert_eq!(
                unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) },
                0
            );
            let reader = unsafe { OwnedFd::from_raw_fd(pipe[0]) };
            let writer = unsafe { OwnedFd::from_raw_fd(pipe[1]) };
            let flags = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_GETFL) };
            assert!(flags >= 0);
            if mode == "verify" {
                // Fill stderr, then restore its original blocking state before
                // exec. Keep the reader alive without draining it. A plain
                // diagnostic write here would hang; mutating O_NONBLOCK in the
                // child would change this parent's retained description.
                assert_eq!(
                    unsafe {
                        libc::fcntl(writer.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK)
                    },
                    0
                );
                let bytes = [0u8; 4096];
                loop {
                    let result = unsafe {
                        libc::write(writer.as_raw_fd(), bytes.as_ptr().cast(), bytes.len())
                    };
                    if result < 0 {
                        assert_eq!(
                            std::io::Error::last_os_error().raw_os_error(),
                            Some(libc::EAGAIN)
                        );
                        break;
                    }
                }
                assert_eq!(
                    unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_SETFL, flags) },
                    0
                );
            } else {
                // Broken stderr and clap help still cannot replace cleanup's
                //125 with a write signal, panic or clap's successful exit.
                drop(reader);
            }
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "tests::entry_descriptor_failure_can_stamp_no_result_without_running_a_guest",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(REPORT, &report)
                .env(MODE, mode)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::from(writer.try_clone().unwrap()));
            // SAFETY: only stack BPF records and prctl run in the pre-exec
            // child. No parent policy changes; the kernel copies the filter.
            unsafe {
                command.pre_exec(|| {
                    let mut filter = [
                        libc::sock_filter {
                            code: (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
                            jt: 0,
                            jf: 0,
                            k: 0,
                        },
                        libc::sock_filter {
                            code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
                            jt: 0,
                            jf: 1,
                            k: libc::SYS_close_range as u32,
                        },
                        libc::sock_filter {
                            code: (libc::BPF_RET | libc::BPF_K) as u16,
                            jt: 0,
                            jf: 0,
                            k: libc::SECCOMP_RET_ERRNO | libc::EPERM as u32,
                        },
                        libc::sock_filter {
                            code: (libc::BPF_RET | libc::BPF_K) as u16,
                            jt: 0,
                            jf: 0,
                            k: libc::SECCOMP_RET_ALLOW,
                        },
                    ];
                    let program = libc::sock_fprog {
                        len: filter.len() as u16,
                        filter: filter.as_mut_ptr(),
                    };
                    if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) < 0
                        || libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &program)
                            < 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let mut child = command.spawn().unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            let outcome = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break Ok(status),
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(10))
                    }
                    Ok(None) => {
                        break Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "entry refusal did not terminate",
                        ));
                    }
                    Err(error) => break Err(error),
                }
            };
            let status = outcome.unwrap_or_else(|error| {
                let _ = child.kill();
                let _ = child.wait();
                panic!("entry refusal: {error}");
            });
            assert_eq!(status.code(), Some(hermit::HERMIT_INTERNAL_FAILURE_EXIT));
            assert_eq!(
                unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_GETFL) },
                flags
            );
            let actual: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
            if mode == "verify" {
                assert_eq!(actual["verdict"], "no_result");
                assert_eq!(actual["verified"], false);
            } else {
                // Help requests no verification destination; its parse result
                // is secondary to125, not authority for this unrelated file.
                assert_eq!(actual, serde_json::json!({"verified": true}));
            }
        }
    }

    #[test]
    fn clap_configuration_is_valid() {
        Args::command().debug_assert();
    }

    // Default-input tests must not inherit HERMIT_EPOCH from the test runner.
    // Change only this test Command; production still honors the environment.
    fn command_without_epoch_env() -> clap::Command {
        Args::command()
            .mut_subcommand("run", |run| {
                run.mut_arg("epoch", |arg| arg.env(None::<&str>))
            })
            .mut_subcommand("oci", |oci| {
                oci.mut_subcommand("run", |run| {
                    run.mut_arg("epoch", |arg| arg.env(None::<&str>))
                })
            })
    }

    fn parse_at<const N: usize>(argv: [&str; N], now: std::time::SystemTime) -> Args {
        let matches = command_without_epoch_env()
            .try_get_matches_from(argv)
            .unwrap();
        args_from_matches_with_clock(&matches, || now).unwrap()
    }

    #[test]
    fn omitted_run_epoch_is_captured_once_at_parse_boundary() {
        let now = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_933_135_628);
        let args = parse_at(["hermit", "run", "/bin/true"], now);
        let Subcommand::Run(run) = args.command else {
            panic!("expected run")
        };
        assert_eq!(
            run.epoch_capture_for_test(),
            ("2031-04-05T06:07:08+00:00".to_owned(), true)
        );
    }

    #[test]
    fn explicit_run_epoch_is_preserved_instead_of_reading_host_time() {
        let matches = command_without_epoch_env()
            .try_get_matches_from([
                "hermit",
                "run",
                "--epoch=2000-12-31T23:59:59.123456789Z",
                "/bin/true",
            ])
            .unwrap();
        let args = args_from_matches_with_clock(&matches, || {
            panic!("an explicit epoch must not read the host clock")
        })
        .unwrap();
        let Subcommand::Run(run) = args.command else {
            panic!("expected run")
        };
        let (epoch, captured) = run.epoch_capture_for_test();
        assert_eq!(epoch, "2000-12-31T23:59:59.123456789+00:00");
        assert!(!captured);
    }

    #[test]
    fn nonvirtualized_run_epoch_does_not_read_the_host_clock() {
        let matches = command_without_epoch_env()
            .try_get_matches_from([
                "hermit",
                "run",
                "--no-virtualize-time",
                "--no-virtualize-metadata",
                "/bin/true",
            ])
            .unwrap();
        let args = args_from_matches_with_clock(&matches, || {
            panic!("a nonvirtualized run must not capture a virtual epoch")
        })
        .unwrap();
        let Subcommand::Run(run) = args.command else {
            panic!("expected run")
        };
        assert!(!run.epoch_capture_for_test().1);
    }

    #[test]
    fn namespace_only_run_epoch_is_neither_captured_nor_reported() {
        let matches = command_without_epoch_env()
            .try_get_matches_from(["hermit", "run", "--namespace-only", "/bin/true"])
            .unwrap();
        let args = args_from_matches_with_clock(&matches, || {
            panic!("namespace-only bypasses Detcore and must not capture host time")
        })
        .unwrap();
        let Subcommand::Run(run) = args.command else {
            panic!("expected run")
        };
        assert!(!run.epoch_capture_for_test().1);
        assert!(!run.reports_virtual_epoch_for_test());
    }

    #[test]
    fn strace_only_run_epoch_is_neither_captured_nor_reported() {
        let matches = command_without_epoch_env()
            .try_get_matches_from(["hermit", "run", "--strace-only", "/bin/true"])
            .unwrap();
        let args = args_from_matches_with_clock(&matches, || {
            panic!("strace-only disables virtual time and must not sample the host clock")
        })
        .unwrap();
        let Subcommand::Run(run) = args.command else {
            panic!("expected run")
        };
        assert!(!run.epoch_capture_for_test().1);
        assert!(!run.reports_virtual_epoch_for_test());
    }

    #[test]
    fn omitted_oci_run_epoch_is_captured_once_with_host_provenance() {
        let now = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_933_135_628);
        let args = parse_at(
            [
                "hermit",
                "oci",
                "run",
                "example.invalid/image:tag",
                "--",
                "/bin/true",
            ],
            now,
        );
        let Subcommand::Oci(oci) = args.command else {
            panic!("expected oci")
        };
        assert_eq!(
            oci.run_epoch_capture_for_test(),
            ("2031-04-05T06:07:08+00:00".to_owned(), true, true)
        );
    }

    #[test]
    fn explicit_oci_run_epoch_preserves_explicit_provenance() {
        let matches = command_without_epoch_env()
            .try_get_matches_from([
                "hermit",
                "oci",
                "run",
                "example.invalid/image:tag",
                "--epoch=2000-12-31T23:59:59Z",
                "--",
                "/bin/true",
            ])
            .unwrap();
        let args = args_from_matches_with_clock(&matches, || {
            panic!("an explicit OCI epoch must not read the host clock")
        })
        .unwrap();
        let Subcommand::Oci(oci) = args.command else {
            panic!("expected oci")
        };
        assert_eq!(
            oci.run_epoch_capture_for_test(),
            ("2000-12-31T23:59:59+00:00".to_owned(), false, true)
        );
    }

    #[test]
    fn invalid_run_epoch_is_rejected_by_the_cli() {
        assert!(
            command_without_epoch_env()
                .try_get_matches_from(["hermit", "run", "--epoch=not-a-time", "/bin/true"])
                .is_err()
        );
    }

    #[test]
    fn run_epoch_help_describes_capture_instead_of_the_fixture_default() {
        let mut command = command_without_epoch_env();
        let run = command.find_subcommand_mut("run").unwrap();
        let help = run.render_long_help().to_string();
        assert!(help.contains("host wall-clock sample"), "{help}");
        assert!(!help.contains("[default: 2026-01-01T00:00:00Z]"), "{help}");
    }

    #[test]
    fn top_level_help_states_what_the_defaults_actually_are() {
        let mut command = command_without_epoch_env();
        // Compare whitespace-normalized text so terminal wrapping cannot matter.
        let help = command
            .render_long_help()
            .to_string()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        // The old text promised identical output with no epoch caveat, said
        // networking was disallowed, and named a ~/.hermit store nothing uses.
        for stale in [
            "networking is disallowed",
            "bitwise identical",
            "~/.hermit",
            "content addressible",
            "Record mode has no `--network` flag",
        ] {
            assert!(!help.contains(stale), "stale claim {stale:?} in:\n{help}");
        }
        for claim in [
            "functions of their controlled inputs",
            "`hermit run` starts it at the host's current time",
            "--epoch=2026-01-01T00:00:00Z",
            "In run and record mode, by default (`--network=local`)",
            "only a loopback interface",
            "A recording stores its choice",
            "`hermit record start --verify-with-gdbex` defaults to, and requires, `--network=host`",
            "The DBT backend does not apply `--network` yet",
            "`$XDG_CACHE_HOME/hermit`, which is `~/.cache/hermit`",
            "`--data-dir` or `HERMIT_DATA_DIR`",
        ] {
            assert!(help.contains(claim), "missing {claim:?} in:\n{help}");
        }

        // The help names `local` as the default; keep that tied to the parser.
        let run = command.find_subcommand_mut("run").unwrap();
        let network = run
            .get_arguments()
            .find(|arg| arg.get_id() == "network")
            .expect("run --network");
        assert_eq!(network.get_default_values(), ["local"]);

        // The help describes `--network` for run and record mode only; if
        // another subcommand gains one, the help must say what it does. The
        // record default is tied to the parser by
        // record_start::tests::record_networking_defaults_to_local_like_run.
        fn network_args(command: &clap::Command) -> Vec<String> {
            let mut found: Vec<String> = command
                .get_arguments()
                .filter(|arg| arg.get_long() == Some("network"))
                .map(|_| command.get_name().to_string())
                .collect();
            for sub in command.get_subcommands() {
                found.extend(network_args(sub));
            }
            found
        }
        let record = command.find_subcommand("record").unwrap();
        // `hermit record PROGRAM` is shorthand for `hermit record start PROGRAM`.
        assert_eq!(network_args(record), ["record", "start"]);
        assert_eq!(
            network_args(command.find_subcommand("run").unwrap()),
            ["run"]
        );
    }

    #[test]
    fn run_epoch_environment_and_cli_precedence() {
        const CHILD: &str = "HERMIT_EPOCH_PRECEDENCE_TEST_CHILD";
        const ENV_EPOCH: &str = "2000-12-31T23:59:59.123456789Z";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tests::run_epoch_environment_and_cli_precedence",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("HERMIT_EPOCH", ENV_EPOCH)
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            return;
        }
        assert_eq!(std::env::var("HERMIT_EPOCH").unwrap(), ENV_EPOCH);
        for oci in [false, true] {
            for explicit in [None, Some("2026-01-01T00:00:00Z")] {
                let mut argv = vec!["hermit"];
                if oci {
                    argv.extend(["oci", "run", "example.invalid/image:tag"]);
                } else {
                    argv.push("run");
                }
                if let Some(epoch) = explicit {
                    argv.extend(["--epoch", epoch]);
                }
                argv.extend(["--", "/bin/true"]);
                let matches = Args::command().try_get_matches_from(argv).unwrap();
                let args = args_from_matches_with_clock(&matches, || {
                    panic!("CLI/environment epochs must not sample the host clock")
                })
                .unwrap();
                let (epoch, captured) = match args.command {
                    Subcommand::Run(run) => run.epoch_capture_for_test(),
                    Subcommand::Oci(oci) => {
                        let (epoch, captured, reported) = oci.run_epoch_capture_for_test();
                        assert!(reported);
                        (epoch, captured)
                    }
                    _ => panic!("expected run"),
                };
                assert_eq!(
                    epoch,
                    if explicit.is_some() {
                        "2026-01-01T00:00:00+00:00"
                    } else {
                        "2000-12-31T23:59:59.123456789+00:00"
                    }
                );
                assert!(!captured);
            }
        }
    }

    #[test]
    fn run_epoch_reproducer_retains_default_and_fractional_inputs_once() {
        for epoch in ["2026-01-01T00:00:00Z", "2000-12-31T23:59:59.123456789Z"] {
            let mut original = super::run::RunOpts::parse_from([
                "run",
                "--epoch",
                epoch,
                "--seed=71",
                "/bin/true",
            ]);
            original.validate_args().unwrap();
            let rendered = original.to_string();
            assert_eq!(rendered.matches("--epoch=").count(), 1, "{rendered}");
            let mut argv = vec!["hermit".to_owned(), "run".to_owned()];
            argv.extend(shell_words::split(&rendered).unwrap());
            let matches = command_without_epoch_env()
                .try_get_matches_from(argv)
                .unwrap();
            assert_eq!(
                matches
                    .subcommand_matches("run")
                    .unwrap()
                    .value_source("epoch"),
                Some(clap::parser::ValueSource::CommandLine)
            );
            let args = args_from_matches_with_clock(&matches, || {
                panic!("a concrete reproducer must not capture a replacement epoch")
            })
            .unwrap();
            let Subcommand::Run(reparsed) = args.command else {
                panic!("expected run")
            };
            assert_eq!(
                reparsed.epoch_capture_for_test().0,
                original.epoch_capture_for_test().0
            );
            assert!(!reparsed.epoch_capture_for_test().1);
            assert_eq!(reparsed.det_opts.det_config.seed, 71);
        }
    }

    #[test]
    fn backend_unavailability_is_classified_by_type_not_message() {
        let typed = Error::new(BackendUnavailable::new(Backend::Dbt, "SDK absent"));
        assert_eq!(
            classify_failure(&typed),
            "HERMIT_INTERNAL_FAILURE class=backend-unavailable backend=dbt"
        );

        let prose_only = Error::msg("backend `dbt` is unavailable: SDK absent");
        assert_eq!(
            classify_failure(&prose_only),
            "HERMIT_INTERNAL_FAILURE class=cli-error",
            "changing only prose must not manufacture the typed class"
        );
    }

    #[test]
    fn skid_overshoot_uses_the_policy_refusal_exit_and_marker() {
        let error = Error::new(hermit::SkidOvershootError::new(2));
        assert_eq!(
            failure_exit_code(&error),
            detcore_model::HERMIT_POLICY_REFUSAL_EXIT
        );
        assert_eq!(
            classify_failure(&error),
            "HERMIT_POLICY_REFUSAL class=policy-refusal cause=skid-overshoot count=2"
        );
    }

    /// Plant a previous invocation's GREEN verdict at `path`, the way a caller
    /// that reuses one `--verify-json` file across runs would have.
    fn plant_previous_green(path: &std::path::Path) {
        std::fs::write(
            path,
            "{\"verified\":true,\"bitwise_parity\":true,\"verdict\":\"matched\"}\n",
        )
        .unwrap();
        let planted: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(planted["verified"], serde_json::json!(true));
    }

    fn read_verdict(path: &std::path::Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    /// The record the stamp must leave behind on every non-verdict exit.
    fn assert_no_result(path: &std::path::Path, context: &str) {
        let now = read_verdict(path);
        assert_eq!(now["verdict"], serde_json::json!("no_result"), "{context}");
        assert_eq!(now["verified"], serde_json::json!(false), "{context}");
        assert_eq!(now["bitwise_parity"], serde_json::json!(false), "{context}");
    }

    /// Drive `Subcommand::main` for `argv` and assert that (a) it exits Err
    /// without reaching a verdict, and (b) the planted green has been replaced
    /// by an invocation-bound no-result.
    ///
    /// Each case is a DIFFERENT top-level exit that occurs ABOVE
    /// `verify()`/`record_verify()`. Stamping as the first statement of those
    /// inner functions did not cover any of them.
    fn assert_top_level_exit_leaves_no_result(argv: &[&str], context: &str) {
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().to_path_buf();
        plant_previous_green(&path);

        let json = format!("--verify-json={}", path.display());
        let mut full: Vec<&str> = argv.to_vec();
        full.push(&json);
        full.push("--");
        // A guest that cannot pass program validation, so no case here can
        // accidentally start a real run and reach a genuine verdict.
        full.push("/nonexistent/hermit-test-guest");

        let mut args = Args::try_parse_from(full).expect("argv should parse");
        let result = args.command.main(&args.global);
        assert!(result.is_err(), "{context}: expected a non-verdict exit");
        assert_no_result(&path, context);
    }

    /// TOP-LEVEL EXIT 1 -- the main preflight (`validate_backend_scope`), which
    /// runs before `RunOpts::main` is even entered.
    #[test]
    fn main_preflight_exit_leaves_an_invocation_bound_no_result() {
        assert_top_level_exit_leaves_no_result(
            &["hermit", "--backend", "kvm", "record", "--verify"],
            "backend-scope preflight",
        );
    }

    // The series jumps 1 -> 3 on purpose. A DBT-flavoured case sat at 2 and was
    // removed with #2359; it should NOT be recreated, because it could not fail
    // on its own. `assert_top_level_exit_leaves_no_result` always appends a
    // nonexistent guest, and `RunOpts::main` calls `validate_program()`
    // (run.rs:2149) BEFORE the `match backend` that returns through the DBT
    // adapter (run.rs:2166), so such a case exits at exactly the statement
    // EXIT 3 already covers and never consults the backend at all. It was
    // `run_preflight_exit_leaves_an_invocation_bound_no_result` with one flag
    // changed. Executing is not the same as discriminating.
    //
    // What DID need restoring from that deletion is the ordering premise
    // immediately below.

    /// `RunOpts::main` returns through the dedicated DBT adapter BEFORE the
    /// generic `verify()` dispatch. That ordering is the whole reason `run_dbt`
    /// has to carry the verdict-artifact path itself: if the generic dispatch
    /// ran first, it would not.
    ///
    /// The next test's doc comment has asserted this in prose since #2359 while
    /// nothing checked it, which is the worse failure mode — a reader trusts a
    /// documented guarantee. Restored rather than deleted because reading
    /// `run.rs` confirms the property still holds. Only the OTHER half of the
    /// original assertion, that a DBT run therefore keeps `no_result`, was made
    /// false by `f0584c1aac` (which gave `run_dbt` a `verify_json` path, a
    /// `ComparisonOptions`, and a real `compare_two_runs` outcome), and that
    /// half is deliberately not restored.
    ///
    /// Both needles must appear EXACTLY once. A bare `find` is satisfied by a
    /// comment quoting the string, and #2359's own round-2 review caught
    /// precisely that: a text assertion a comment can satisfy is not a pin.
    /// Requiring uniqueness turns such a quote into a loud failure.
    #[test]
    fn dbt_arm_returns_before_the_generic_verify_dispatch() {
        let source = include_str!("run.rs");
        let sole_offset = |needle: &str| -> usize {
            let hits: Vec<usize> = source.match_indices(needle).map(|(i, _)| i).collect();
            assert_eq!(
                hits.len(),
                1,
                "expected exactly one occurrence of {needle:?} in run.rs, found {}. Zero means \
                 the shape this check reads has moved or gone, so it can no longer see the \
                 ordering; two or more (including one inside a comment) would make the check \
                 meaningless rather than merely wrong",
                hits.len()
            );
            hits[0]
        };
        let dbt_return = sole_offset("return super::backends::run_dbt(");
        let generic_verify = sole_offset("self.verify(global)");
        assert!(
            dbt_return < generic_verify,
            "RunOpts::main must return through the dedicated DBT adapter before the generic \
             verify() dispatch; if that order ever reverses, run_dbt no longer needs to carry \
             the verdict-artifact path and dbt_arm_has_a_channel_to_publish_a_verdict is \
             asserting something that no longer follows"
        );
    }

    /// The DBT arm of `RunOpts::main` returns `run_dbt(..)` and never reaches
    /// the common `verify()` function, so both cfg arms of `run_dbt` must carry
    /// the verdict-artifact path explicitly. The ordering that makes that true
    /// is pinned by `dbt_arm_returns_before_the_generic_verify_dispatch`.
    ///
    /// Asserted structurally rather than by executing the arm so this focused
    /// test cannot block on the harness stdin or require DynamoRIO.
    #[test]
    fn dbt_arm_has_a_channel_to_publish_a_verdict() {
        let source = include_str!("backends.rs");
        let production = source
            .split_once("#[cfg(test)]")
            .expect("backends test module")
            .0;
        let signatures: Vec<&str> = production
            .match_indices("fn run_dbt(")
            .map(|(i, _)| {
                let rest = &production[i..];
                &rest[..rest
                    .find(") -> Result<ExitStatus, Error>")
                    .expect("run_dbt signature")]
            })
            .collect();
        assert_eq!(signatures.len(), 2, "expected both cfg arms of run_dbt");
        for signature in signatures {
            assert!(
                signature.contains("verify"),
                "run_dbt should still receive the verify flag"
            );
            assert!(
                signature.contains("verify_json"),
                "run_dbt must receive the verdict-artifact path because the DBT arm bypasses \
                 the common verify() function:\n{signature}"
            );
        }
    }

    /// The DBT arm builds its own [`verify::ComparisonOptions`], so the record
    /// envelope it names is not covered by the generic verifier's tests. Pin it
    /// to `all_records_v1`, the envelope the schema-10 ledger accepts: the DBT
    /// log holds every authenticated record, initialization records included at
    /// the positions Reverie's collector gives them. An opaque `caller_defined` predicate here would
    /// silently disqualify every DBT verdict from bitwise parity, while
    /// `dbt_evidence_transport_v1` would leave the initialization positions
    /// uncompared. The DBT evidence tests in `backends.rs` check behaviourally
    /// that the log and the verdict carry that stream.
    #[test]
    fn dbt_verdict_names_a_canonical_record_envelope() {
        let source = include_str!("backends.rs");
        let start = source
            .find("= compare_two_runs(")
            .expect("DBT arm must compare two runs");
        let options = source[start..]
            .find("ComparisonOptions {")
            .map(|offset| &source[start + offset..])
            .expect("DBT comparison must build ComparisonOptions");
        let block = &options[..options.find("\n        },").expect("options block end")];

        let record_envelope_literal = || {
            block
                .lines()
                .find_map(|line| line.trim().strip_prefix("record_envelope:"))
                .map(|value| value.trim().trim_end_matches(',').to_string())
                .expect(
                    "the DBT comparison must state its record envelope; an unnamed selection \
                     is exactly the undisclosed filtering the envelope exists to prevent",
                )
        };
        assert_eq!(
            record_envelope_literal(),
            "RecordEnvelope::all_records_v1()",
            "the DBT adapter compares every authenticated evidence record, initialization \
             records included at their stream positions, and also compares the initialization \
             count as a typed dimension. Changing this envelope changes which records are \
             compared, so update the adapter and its evidence together, not just this literal"
        );
        // Naming the envelope is worth nothing if the adapter applies another
        // unreported filter while materializing the already decoded records.
        // `write_canonical_info_with_filter` invites that at a backend boundary,
        // so pin the unfiltered call.
        let materialize = source
            .find("fn materialize_dbt_comparison_log")
            .expect("DBT comparison log materialization");
        let body = &source[materialize..];
        let body = &body[..body.find("\n}\n").expect("materialization body end")];
        assert!(
            body.contains("detcore::logdiff::write_canonical_info(path, &mut std::io::sink())"),
            "the DBT comparison log must be materialized unfiltered; filtering here would \
             contradict the envelope the verdict publishes"
        );
        assert!(
            !body.contains("write_canonical_info_with_filter"),
            "filtering at the DBT boundary would apply an undisclosed second policy"
        );
        // Bind the literal above to the real policy, so renaming the
        // constructor without preserving its meaning fails here too.
        assert!(
            crate::record_envelope::RecordEnvelope::all_records_v1()
                .policy()
                .is_canonical(),
            "all_records_v1 must remain canonical or DBT verdicts silently lose \
             bitwise-parity eligibility"
        );
        assert_eq!(
            crate::record_envelope::RecordEnvelope::all_records_v1()
                .policy()
                .as_str(),
            "all_records_v1"
        );
    }

    /// The DBT backend observes a counted-branch or statistics divergence
    /// before the log comparison, so its logs can match while the published
    /// verdict is a divergence. `--keep-logs` keeps a single golden log after a
    /// match, so the DBT arm must mark such a match overridden with exactly the
    /// predicates that later turn the verdict into a divergence
    /// (`finalize_dbt_verification` for the clock, `DbtSummaryComparison::apply`
    /// for the statistics). Otherwise run 2's log of a divergent run would be
    /// deleted. DBT cannot execute in unit tests, so the wiring is pinned here
    /// and `verify::tests::an_overridden_match_keeps_both_logs_in_the_failure_directory`
    /// proves what an overridden match retains.
    #[test]
    fn dbt_backend_divergence_overrides_a_log_match_for_retention() {
        let source = include_str!("backends.rs");
        let start = source
            .find("= compare_two_runs(")
            .expect("DBT arm must compare two runs");
        let options = source[start..]
            .find("ComparisonOptions {")
            .map(|offset| &source[start + offset..])
            .expect("DBT comparison must build ComparisonOptions");
        let block = &options[..options.find("\n        },").expect("options block end")];
        let normalized = block.split_whitespace().collect::<Vec<_>>().join(" ");
        let backend_divergence =
            "branch_clock_diverged || summary_comparison.requires_log_retention(),";
        for field in [
            format!("keep_logs: keep_logs || {backend_divergence}"),
            format!("match_overridden: {backend_divergence}"),
        ] {
            assert_eq!(
                normalized.matches(&field).count(),
                1,
                "the DBT comparison must state `{field}` exactly once"
            );
        }
        assert!(
            source.contains("let branch_clock_diverged = !branch_clock_comparison.matched();"),
            "the retention predicate must be the same clock comparison the verdict uses"
        );
    }

    /// `--namespace-only` appears on the list of paths that bypass `verify()`,
    /// but it is NOT reachable with a verdict artifact: clap rejects
    /// `--verify` together with `--namespace-only`, and `--verify-json`
    /// requires `--verify`. Asserted rather than guarded, so the day that
    /// conflict is relaxed this test fails and the stamp coverage is revisited
    /// instead of silently developing a hole.
    #[test]
    fn namespace_only_cannot_carry_a_verdict_artifact() {
        let parsed = Args::try_parse_from([
            "hermit",
            "run",
            "--verify",
            "--namespace-only",
            "--",
            "/bin/true",
        ]);
        assert!(
            parsed.is_err(),
            "--verify with --namespace-only must remain a parse-time conflict; if this now \
             parses, --namespace-only bypasses verify() and needs the pending stamp too"
        );
    }

    /// TOP-LEVEL EXIT 3 -- `RunOpts::main`'s own preflight, entered after the
    /// dispatcher and still far above `verify()`.
    ///
    /// The case chosen is `validate_log_level`, which is the FIRST fallible
    /// statement of `RunOpts::main`. That choice is deliberate: everything after
    /// it reaches `reserve_output_stdin_snapshot(startup_stdin()?)`, which
    /// BLOCKS reading the harness's stdin, so a test driving any later preflight
    /// step through `main()` hangs instead of failing. The later steps
    /// (`validate_args`, `ensure_available`, `install_pmu_config`,
    /// `validate_mount_sources`, `validate_program`, happens-before resolution,
    /// e9patch preparation) are therefore NOT exercised here; they are covered
    /// by construction, because the stamp is the first statement of
    /// `Subcommand::main` and so dominates every one of them.
    #[test]
    fn run_preflight_exit_leaves_an_invocation_bound_no_result() {
        assert_top_level_exit_leaves_no_result(
            &[
                "hermit",
                "--log",
                "warn",
                "--backend=ptrace",
                "run",
                "--verify",
            ],
            "RunOpts::main log-level preflight",
        );
    }

    /// TOP-LEVEL EXIT 4 -- `StartOpts::main` pre-validation: the record path
    /// validates the log level before calling `record_verify`.
    #[test]
    fn record_start_prevalidation_exit_leaves_an_invocation_bound_no_result() {
        assert_top_level_exit_leaves_no_result(
            &["hermit", "--log", "warn", "record", "start", "--verify"],
            "record start log-level pre-validation",
        );
    }

    /// POSITIVE control: the stamp is not a dead end. A subcommand that carries
    /// no `--verify-json` must not have a path at all, so nothing is written and
    /// no unrelated file is disturbed.
    #[test]
    fn subcommands_without_verify_json_have_no_verdict_path() {
        for argv in [
            vec!["hermit", "run", "--", "/bin/true"],
            vec!["hermit", "record", "start", "--", "/bin/true"],
            vec!["hermit", "run", "--verify", "--", "/bin/true"],
        ] {
            let args = Args::try_parse_from(argv.clone()).expect("argv should parse");
            assert!(
                args.command.verification_json_path().is_none(),
                "{argv:?} should carry no verification-json path"
            );
        }
    }

    /// POSITIVE control for the accessor that feeds the stamp: when
    /// `--verify-json` IS present, every spelling that can produce a verdict
    /// reports it -- including `record`'s flattened direct form, which is a
    /// different code path from `record start`.
    #[test]
    fn every_verdict_producing_spelling_reports_its_verdict_path() {
        for argv in [
            vec![
                "hermit",
                "run",
                "--verify",
                "--verify-json=/tmp/v.json",
                "--",
                "/bin/true",
            ],
            vec![
                "hermit",
                "record",
                "--verify",
                "--verify-json=/tmp/v.json",
                "--",
                "/bin/true",
            ],
            vec![
                "hermit",
                "record",
                "start",
                "--verify",
                "--verify-json=/tmp/v.json",
                "--",
                "/bin/true",
            ],
        ] {
            let args = Args::try_parse_from(argv.clone()).expect("argv should parse");
            assert_eq!(
                args.command.verification_json_path(),
                Some(std::path::Path::new("/tmp/v.json")),
                "{argv:?} must report its verdict path to the stamp"
            );
        }
    }

    #[test]
    fn replay_accepts_an_optional_id_and_options() {
        let args = Args::try_parse_from([
            "hermit",
            "replay",
            "--autopilot",
            "--data-dir",
            "/tmp/recordings",
            "0123456789abcdef0123456789abcdef",
        ])
        .unwrap();

        assert!(matches!(args.command, Subcommand::Replay(_)));
    }

    #[test]
    fn replay_accepts_serve_only_for_an_external_debugger() {
        let args =
            Args::try_parse_from(["hermit", "replay", "--serve-only", "--gdbserver-port=2345"])
                .unwrap();

        assert!(matches!(args.command, Subcommand::Replay(_)));
    }

    #[test]
    fn replay_serve_only_rejects_gdb_commands() {
        let error =
            Args::try_parse_from(["hermit", "replay", "--serve-only", "--gdbex=break main"])
                .unwrap_err();

        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn bisect_accepts_schedule_endpoints_and_run_args() {
        let args = Args::try_parse_from([
            "hermit",
            "bisect",
            "--good",
            "good.json",
            "--bad",
            "bad.json",
            "--",
            "--max-timeslice=disabled",
            "/bin/true",
        ])
        .unwrap();

        assert!(matches!(args.command, Subcommand::Bisect(_)));
    }

    #[test]
    fn backend_parses_in_global_position() {
        use hermit::Backend;

        let args = Args::try_parse_from(["hermit", "--backend", "kvm", "run", "prog"])
            .expect("global-position --backend should parse");
        assert_eq!(args.global.backend, Some(Backend::Kvm));
        assert!(matches!(args.command, Subcommand::Run(_)));
    }

    /// `analyze` and `bisect` launch trials through `RunOpts::run`, which skips
    /// `main`'s per-backend preparation, so only backends whose trials really run
    /// on them through that path are in scope: ptrace and KVM. The rest must be
    /// refused rather than silently run as ptrace (e9patch without its rewrite)
    /// or fail on analyze's workspace bind (DBT). A management subcommand stays
    /// out of scope for every non-default backend.
    #[test]
    fn analyze_and_bisect_scope_is_limited_to_backends_whose_trials_run() {
        for (backend, in_scope) in [
            ("ptrace", true),
            ("kvm", true),
            ("dbt", false),
            ("liteinst", false),
            ("sabre", false),
            ("e9patch", false),
        ] {
            for argv in [
                vec!["hermit", "--backend", backend, "analyze", "--", "/bin/true"],
                vec![
                    "hermit",
                    "--backend",
                    backend,
                    "bisect",
                    "--good=g.json",
                    "--bad=b.json",
                    "--",
                    "/bin/true",
                ],
            ] {
                let args = Args::try_parse_from(&argv)
                    .unwrap_or_else(|error| panic!("{argv:?} should parse: {error}"));
                let scope = args.command.validate_backend_scope(args.global.backend);
                if in_scope {
                    scope.unwrap_or_else(|error| panic!("{argv:?} must be in scope: {error:#}"));
                } else {
                    let error =
                        scope.expect_err("a backend whose trials cannot run must be refused");
                    assert!(
                        error
                            .to_string()
                            .contains(&format!("hermit --backend {backend} run")),
                        "{argv:?}: refusal does not name the working path: {error:#}"
                    );
                }
            }
            if backend != "ptrace" {
                let args = Args::try_parse_from(["hermit", "--backend", backend, "record", "list"])
                    .unwrap();
                assert!(
                    args.command
                        .validate_backend_scope(args.global.backend)
                        .is_err(),
                    "--backend {backend} record list must stay out of scope"
                );
            }
        }
    }

    fn os_args(args: &[&str]) -> Vec<std::ffi::OsString> {
        args.iter().map(std::ffi::OsString::from).collect()
    }

    #[test]
    fn subcommand_level_backend_is_a_usage_error_naming_the_global_form() {
        use clap::error::ErrorKind;

        for (argv, corrected) in [
            (
                &["hermit", "run", "--backend=ptrace", "--", "/bin/true"][..],
                "hermit --backend=ptrace run -- /bin/true",
            ),
            (
                &[
                    "hermit",
                    "record",
                    "start",
                    "--backend",
                    "e9patch",
                    "--",
                    "a b",
                ][..],
                "hermit --backend=e9patch record start -- 'a b'",
            ),
        ] {
            let argv = os_args(argv);
            let error = Args::command()
                .try_get_matches_from(&argv)
                .expect_err("a subcommand-level --backend must not parse");
            let error = super::redirect_misplaced_backend(error, &argv);
            assert_eq!(error.kind(), ErrorKind::UnknownArgument);
            assert_eq!(error.exit_code(), 2);
            let rendered = error.render().to_string();
            assert!(
                rendered.contains(&format!("must come before the subcommand: `{corrected}`")),
                "{rendered}"
            );
            assert!(
                rendered.contains("Usage: hermit [OPTIONS] <COMMAND>"),
                "{rendered}"
            );
        }

        // Other unknown arguments keep clap's own error, suggestions included.
        let argv = os_args(&["hermit", "run", "--backend-engagement-jsn=x", "prog"]);
        let error = Args::command().try_get_matches_from(&argv).unwrap_err();
        let untouched = error.render().to_string();
        let error = super::redirect_misplaced_backend(error, &argv);
        assert_eq!(error.render().to_string(), untouched);
        assert!(
            untouched.contains("--backend-engagement-json"),
            "{untouched}"
        );
    }

    #[test]
    fn misplaced_backend_is_only_moved_when_unambiguous() {
        use super::globally_placed_backend;

        assert_eq!(
            globally_placed_backend(&os_args(&["h", "--log", "info", "run", "--backend", "kvm"]))
                .as_deref(),
            Some("h --backend=kvm --log info run")
        );
        // A guest argument named `--backend` is not a Hermit option.
        assert_eq!(
            globally_placed_backend(&os_args(&[
                "h",
                "run",
                "--backend=dbt",
                "--",
                "prog",
                "--backend",
            ]))
            .as_deref(),
            Some("h --backend=dbt run -- prog --backend")
        );
        // Two backend options, or one whose value is missing, empty, or looks
        // like an option, cannot be moved safely, so the generic global
        // spelling is shown instead.
        for argv in [
            &["h", "--backend=kvm", "run", "--backend=dbt", "--", "p"][..],
            &["h", "run", "--backend"][..],
            &["h", "run", "--backend", "--", "p"][..],
            &["h", "run", "--backend", "--strict", "--", "p"][..],
            &["h", "run", "--backend=", "--", "p"][..],
            &["h", "run", "--backend=-x", "--", "p"][..],
        ] {
            assert_eq!(globally_placed_backend(&os_args(argv)), None, "{argv:?}");
        }
    }

    #[test]
    fn global_backend_with_namespace_only_is_a_usage_error() {
        use clap::error::ErrorKind;

        for argv in [
            &[
                "hermit",
                "--backend=ptrace",
                "run",
                "--namespace-only",
                "prog",
            ][..],
            &[
                "hermit",
                "--backend=kvm",
                "oci",
                "run",
                "--namespace-only",
                "img",
                "p",
            ][..],
        ] {
            let matches = Args::command().try_get_matches_from(argv).unwrap();
            let error = args_from_matches_with_clock(&matches, || {
                panic!("a refused command must not capture host time")
            })
            .expect_err("--backend with --namespace-only must be refused");
            assert_eq!(error.kind(), ErrorKind::ArgumentConflict, "{argv:?}");
            assert_eq!(error.exit_code(), 2);
            let rendered = error.render().to_string();
            assert!(rendered.contains("--namespace-only"), "{rendered}");
            assert!(rendered.contains("Drop `--backend`"), "{rendered}");
        }
        let matches = Args::command()
            .try_get_matches_from(["hermit", "run", "--namespace-only", "prog"])
            .unwrap();
        assert!(args_from_matches_with_clock(&matches, || panic!("no clock")).is_ok());
    }

    #[test]
    fn e9patch_is_allowed_for_recording_but_rejected_for_management_and_replay() {
        use hermit::Backend;

        for command in [
            vec![
                "hermit",
                "--backend",
                "e9patch",
                "record",
                "start",
                "--",
                "/bin/true",
            ],
            vec![
                "hermit",
                "--backend",
                "e9patch",
                "record",
                "--",
                "/bin/true",
            ],
        ] {
            let args = Args::try_parse_from(command).unwrap();
            args.command
                .validate_backend_scope(Some(Backend::E9patch))
                .unwrap();
        }

        for command in [
            vec!["hermit", "--backend", "e9patch", "record", "list"],
            vec![
                "hermit",
                "--backend",
                "e9patch",
                "replay",
                "0123456789abcdef0123456789abcdef",
            ],
        ] {
            let args = Args::try_parse_from(command).unwrap();
            let error = args
                .command
                .validate_backend_scope(Some(Backend::E9patch))
                .unwrap_err();
            assert!(error.to_string().contains("only through"));
        }
    }

    #[test]
    fn liteinst_is_rejected_outside_run() {
        use hermit::Backend;

        let args = Args::try_parse_from([
            "hermit",
            "--backend",
            "liteinst",
            "record",
            "list",
            "--json",
        ])
        .unwrap();
        let error = args
            .command
            .validate_backend_scope(Some(Backend::Liteinst))
            .unwrap_err();
        assert!(error.to_string().contains("only through"));
    }

    #[test]
    fn in_guest_trap_is_rejected_outside_run() {
        use hermit::Backend;

        let args = Args::try_parse_from([
            "hermit",
            "--backend",
            "in-guest-trap",
            "record",
            "list",
            "--json",
        ])
        .unwrap();
        let error = args
            .command
            .validate_backend_scope(Some(Backend::InGuestTrap))
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("only through `hermit --backend in-guest-trap run`"),
            "{error}"
        );
    }

    #[test]
    fn kvm_is_rejected_outside_run_instead_of_silently_recording_with_ptrace() {
        use hermit::Backend;

        let args = Args::try_parse_from([
            "hermit",
            "--backend",
            "kvm",
            "record",
            "start",
            "--",
            "/bin/true",
        ])
        .unwrap();
        let error = args
            .command
            .validate_backend_scope(Some(Backend::Kvm))
            .unwrap_err();
        assert!(error.to_string().contains("require the ptrace runtime"));
    }

    #[test]
    fn dbt_is_rejected_outside_run_instead_of_silently_recording_with_ptrace() {
        use hermit::Backend;

        let run =
            Args::try_parse_from(["hermit", "--backend", "dbt", "run", "--", "/bin/true"]).unwrap();
        run.command
            .validate_backend_scope(Some(Backend::Dbt))
            .expect("DBT run is the supported DBT execution path");

        let record = Args::try_parse_from([
            "hermit",
            "--backend",
            "dbt",
            "record",
            "start",
            "--",
            "/bin/true",
        ])
        .unwrap();
        let error = record
            .command
            .validate_backend_scope(Some(Backend::Dbt))
            .expect_err("DBT record must not silently execute through ptrace");
        assert!(error.to_string().contains("use the ptrace runtime"));
    }

    #[test]
    fn record_accepts_strict_direct_and_start_forms() {
        for args in [
            vec!["hermit", "record", "--strict", "--", "/bin/echo", "hello"],
            vec![
                "hermit",
                "record",
                "start",
                "--strict",
                "--",
                "/bin/echo",
                "hello",
            ],
        ] {
            let parsed = Args::try_parse_from(args).expect("record --strict should parse");
            assert!(matches!(parsed.command, Subcommand::Record(_)));
        }
    }

    /// The scope PREDICATE and the scope MESSAGE must name the same set.
    ///
    /// They drifted apart: the predicate admitted `Strace | Run` while the text
    /// said "only ... strace", so a working path was documented as unsupported.
    /// Both directions are asserted, because either half alone is satisfiable by
    /// a wrong fix -- narrowing the predicate to strace would pass a
    /// message-only test, and leaving the message alone would pass a
    /// predicate-only test.
    #[test]
    fn sabre_scope_admits_exactly_run_and_strace() {
        use hermit::Backend;

        // POSITIVE: both admitted subcommands must pass the scope guard.
        for sub in ["run", "strace"] {
            let args = Args::try_parse_from([
                "hermit",
                "--backend",
                "sabre",
                sub,
                "--",
                "/bin/echo",
                "hi",
            ])
            .unwrap_or_else(|e| panic!("`--backend sabre {sub}` should parse: {e}"));
            assert_eq!(args.global.backend, Some(Backend::Sabre));
            args.command
                .validate_backend_scope(args.global.backend)
                .unwrap_or_else(|e| panic!("`--backend sabre {sub}` must be in scope: {e}"));
        }

        // NEGATIVE: something outside the set is still refused. Without this the
        // guard could admit everything and still pass the positives above.
        let args = Args::try_parse_from([
            "hermit",
            "--backend",
            "sabre",
            "log-diff",
            "/dev/null",
            "/dev/null",
        ])
        .expect("log-diff should parse");
        let err = args
            .command
            .validate_backend_scope(args.global.backend)
            .expect_err("`--backend sabre log-diff` must be rejected");

        // And the refusal must NAME both supported forms. A message that omits
        // `run` is the original defect: it sends a user away from a path that
        // works.
        let text = err.to_string();
        assert!(text.contains("sabre run"), "message must name run: {text}");
        assert!(
            text.contains("sabre strace"),
            "message must name strace: {text}"
        );
    }

    #[test]
    fn sabre_strace_command_parses_in_requested_form() {
        use hermit::Backend;

        let args = Args::try_parse_from([
            "hermit",
            "--backend",
            "sabre",
            "strace",
            "--",
            "/bin/echo",
            "hello",
        ])
        .expect("requested SaBRe strace form should parse");
        assert_eq!(args.global.backend, Some(Backend::Sabre));
        assert!(matches!(args.command, Subcommand::Strace(_)));
    }

    #[test]
    fn sabre_strace_rejects_run_options_it_does_not_honor() {
        for option in [
            "--namespace-only",
            "--verify",
            "--strict",
            "--env=SHOULD_NOT_BE_IGNORED=1",
            "--workdir=/tmp",
        ] {
            let result = Args::try_parse_from([
                "hermit",
                "--backend",
                "sabre",
                "strace",
                option,
                "--",
                "/bin/true",
            ]);
            assert!(
                result.is_err(),
                "SaBRe strace unexpectedly accepted unsupported option {option}"
            );
        }
    }

    #[test]
    fn record_accepts_a_positive_timeout() {
        Args::try_parse_from([
            "hermit",
            "record",
            "start",
            "--record-timeout=1",
            "--",
            "/bin/true",
        ])
        .unwrap();
    }

    #[test]
    fn record_rejects_a_zero_timeout() {
        assert!(
            Args::try_parse_from([
                "hermit",
                "record",
                "start",
                "--record-timeout=0",
                "--",
                "/bin/true",
            ])
            .is_err()
        );
    }

    #[test]
    fn instruction_map_accepts_binary_and_cache_directory() {
        let args = Args::try_parse_from([
            "hermit",
            "instruction-map",
            "--cache-dir",
            "/tmp/instruction-maps",
            "/bin/ls",
        ])
        .unwrap();

        assert!(matches!(args.command, Subcommand::InstructionMap(_)));
    }
}
