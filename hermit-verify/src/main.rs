/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use anyhow::Result;
use clap::Parser;
use clap::Subcommand;

mod chaos_replay;
mod chaos_stress;
mod cli_wrapper;
mod common;
mod run;
mod schedule_trace;
mod trace_replay;
mod use_case;

use colored::*;
pub use common::CommonOpts;
use use_case::run_use_case;

#[derive(Parser, Debug)]
#[clap(author = "oncall+hermit@xmail.facebook.com")]
struct Args {
    #[clap(subcommand)]
    command: Commands,

    #[clap(flatten)]
    common_opts: CommonOpts,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    Run(run::RunOpts),
    TraceReplay(trace_replay::TraceReplayOpts),
    ChaosReplay(chaos_replay::ChaosReplayOpts),
    /// Schedule Trace
    #[clap(subcommand)]
    SchedTrace(schedule_trace::SchedTraceOpts),
    ChaosStress(chaos_stress::ChaosStressOpts),
}

#[fbinit::main]
fn main() -> Result<()> {
    let Args {
        command,
        common_opts,
    } = Args::from_args();

    let mut print_success = true;
    let result = match command {
        Commands::Run(cmd) => run_use_case(cmd, &common_opts)?,
        Commands::TraceReplay(cmd) => run_use_case(cmd, &common_opts)?,
        Commands::ChaosReplay(cmd) => run_use_case(cmd, &common_opts)?,
        Commands::ChaosStress(cmd) => cmd.run(&common_opts)?,
        Commands::SchedTrace(cmd) => {
            print_success = false;
            cmd.main(&common_opts)?
        }
    };

    if !result {
        println!("{}", "Verification use case failed!".red().bold());
        anyhow::bail!("Verification check failed")
    } else if print_success {
        println!("{}", "Success!".green().bold());
    };

    Ok(())
}
