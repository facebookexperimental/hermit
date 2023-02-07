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

mod analyze;
mod bnz;
mod clean;
mod container;
mod global_opts;
mod list;
mod logdiff;
mod record;
mod record_start;
mod remove;
mod replay;
mod run;
mod schedule_search;
mod tracing;
mod verify;
mod version;

use clap::AppSettings;
use clap::Parser;
use colored::*;
use hermit::Error;
use hermit::ExitStatus;

use self::analyze::AnalyzeOpts;
use self::global_opts::GlobalOpts;
use self::logdiff::LogDiffCLIOpts;
use self::record::RecordOpts;
use self::replay::ReplayOpts;
use self::run::RunOpts;
use self::version::Version;

#[derive(Debug, Parser)]
#[clap(
    name = "hermit",
    version = Version::get(),
    global_settings(&[AppSettings::ColoredHelp]),
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
    #[clap(name = "run", setting = AppSettings::TrailingVarArg)]
    Run(Box<RunOpts>),

    /// Record the execution of a program (EXPERIMENTAL).
    #[clap(name = "record", setting = AppSettings::TrailingVarArg)]
    Record(RecordOpts),

    /// Replay the execution of a program.
    #[clap(name = "replay", setting = AppSettings::TrailingVarArg)]
    Replay(ReplayOpts),

    /// Take the difference of two (run/record) logs written to files.
    LogDiff(LogDiffCLIOpts),

    /// Analyze Pass and failing runs
    Analyze(Box<AnalyzeOpts>),
}

impl Subcommand {
    fn main(&mut self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        match self {
            Subcommand::Run(x) => x.main(global),
            Subcommand::Record(x) => x.main(global),
            Subcommand::Replay(x) => x.main(global),
            Subcommand::LogDiff(x) => Ok(x.main(global)),
            Subcommand::Analyze(x) => x.main(global),
        }
    }
}

#[fbinit::main]
fn main() {
    let Args {
        global,
        mut command,
    } = Args::from_args();

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
