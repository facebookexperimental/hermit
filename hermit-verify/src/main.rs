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

mod cli_wrapper;
mod common;
mod run;
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
}

#[cli::main("hermit-verify", error_logging)]
fn main(_fb: fbinit::FacebookInit, args: Args) -> Result<()> {
    let result = match args.command {
        Commands::Run(cmd) => run_use_case(cmd, &args.common_opts)?,
        Commands::TraceReplay(cmd) => run_use_case(cmd, &args.common_opts)?,
    };

    if !result {
        println!("{}", "Verification use case failed!".red().bold());
        anyhow::bail!("Verification check failed")
    } else {
        println!("{}", "Success!".green().bold());
    }

    Ok(())
}
