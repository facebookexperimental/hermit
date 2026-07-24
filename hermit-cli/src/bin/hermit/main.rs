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
mod bnz;
mod clean;
mod container;
mod global_opts;
mod instruction_map;
mod list;
mod logdiff;
mod record;
mod record_start;
mod remove;
mod replay;
mod run;
mod schedule_search;
mod strace;
mod tracing;
mod verify;
mod version;
use std::fs::File;
use std::io;
use std::os::fd::FromRawFd;
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

use clap::Parser;
use colored::*;
use hermit::Error;
use hermit::ExitStatus;

use self::analyze::AnalyzeOpts;
use self::bisect::BisectOpts;
use self::global_opts::GlobalOpts;
use self::instruction_map::InstructionMapOpts;
use self::logdiff::LogDiffCLIOpts;
use self::record::RecordOpts;
use self::replay::ReplayOpts;
use self::run::RunOpts;
use self::strace::StraceOpts;
use self::version::Version;

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

#[derive(Debug, Parser)]
enum Subcommand {
    /// Run a program sandboxed and fully deterministically (unless external networking is allowed).
    #[clap(name = "run", trailing_var_arg = true)]
    Run(Box<RunOpts>),

    /// Trace a program's syscalls through the selected backend.
    #[clap(name = "strace")]
    Strace(StraceOpts),

    /// Record the execution of a program (EXPERIMENTAL).
    #[clap(name = "record", trailing_var_arg = true)]
    Record(RecordOpts),

    /// Replay the execution of a program.
    #[clap(name = "replay")]
    Replay(ReplayOpts),

    /// Take the difference of two (run/record) logs written to files.
    LogDiff(LogDiffCLIOpts),

    /// Analyze Pass and failing runs
    Analyze(Box<AnalyzeOpts>),

    /// Bisect passing and failing schedules to localize a race.
    #[clap(name = "bisect", trailing_var_arg = true)]
    Bisect(Box<BisectOpts>),

    /// Generate a JSON map of nondeterministic instructions in an ELF binary.
    #[clap(name = "instruction-map")]
    InstructionMap(InstructionMapOpts),
}

impl Subcommand {
    fn main(&mut self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        if global.backend == Some(hermit::Backend::Sabre)
            && !matches!(self, Subcommand::Strace(_) | Subcommand::Run(_))
        {
            anyhow::bail!(
                "the SaBRe backend is available only through `hermit --backend sabre strace`"
            );
        }
        match self {
            Subcommand::Run(x) => x.main(global),
            Subcommand::Strace(x) => x.main(global),
            Subcommand::Record(x) => x.main(global),
            Subcommand::Replay(x) => x.main(global),
            Subcommand::LogDiff(x) => Ok(x.main(global)),
            Subcommand::Analyze(x) => x.main(global),
            Subcommand::Bisect(x) => x.main(global),
            Subcommand::InstructionMap(x) => x.main(global),
        }
    }
}

#[fbinit::main]
fn main() {
    let Args {
        global,
        mut command,
    } = Args::parse();

    command
        .main(&global)
        .unwrap_or_else(|err| {
            display_error(err);
            ExitStatus::Exited(1)
        })
        .raise_or_exit();
}

fn display_error(error: Error) {
    let mut chain = error.chain();

    if let Some(error) = chain.next() {
        eprintln!("{}: {}", "Error".red().bold(), error);
    }

    for cause in chain {
        eprintln!("     {} {}", ">".dimmed().bold(), cause);
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;
    use clap::Parser;

    use super::Args;
    use super::Subcommand;

    #[test]
    fn clap_configuration_is_valid() {
        Args::command().debug_assert();
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
