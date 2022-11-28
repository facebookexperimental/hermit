/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::PathBuf;

use clap::Parser;
use colored::Colorize;
use rand::Rng;

use crate::cli_wrapper::display_cmd;
use crate::cli_wrapper::*;
use crate::CommonOpts;

/// Verification utility for replaying preemptions under hermit
/// This utility runs a guest program under "hermit run" and records schedules and preemptions. On the second run those recorded schedules are replayed via "hermit run" subcommand
/// The outputs (stdout, stderr, log file, schedules) are captured and placed in a temporary directory allowing further inspection (if --keep-temp-dir is provided)
#[derive(Parser, Debug)]
pub struct ChaosStressOpts {
    // Maximum number of iterations performed during the test
    #[clap(long, default_value = "100")]
    max_iterations_count: i32,
    /// Path to a guest program
    #[clap(value_name = "PROGRAM")]
    guest_program: PathBuf,
    /// Additional argument for hermit run subcommand
    #[clap(long)]
    hermit_arg: Vec<String>,
    /// Arguments for a guest program
    #[clap(value_name = "ARGS")]
    args: Vec<String>,
}

impl ChaosStressOpts {
    fn run_hermit(&self, common_args: &CommonOpts) -> anyhow::Result<bool> {
        let mut rng = rand::thread_rng();
        let random_seed: u16 = rng.gen();
        let hermit_cli_args = Hermit::new()
            .run(self.guest_program.clone(), self.args.clone())
            .hermit_args(self.hermit_arg.clone())
            .seed(i32::from(random_seed))
            .log_level(tracing::Level::WARN)
            .into_args();

        let mut command = std::process::Command::new("time");
        command.arg(&common_args.hermit_bin);
        command.args(hermit_cli_args);
        println!("   {}", display_cmd(&command).white().bold());
        let status = command.status()?;
        let status_code = status.code().unwrap_or(0);
        println!("Executed command with status: {}", status_code);

        Ok(status_code == 0)
    }
    pub fn run(&self, common_args: &CommonOpts) -> anyhow::Result<bool> {
        println!("Run at max {} iterations", self.max_iterations_count);
        let mut success_count = 0;
        let mut fail_count = 0;

        for i in 1..=self.max_iterations_count {
            println!("{}", format!(":: Run #{}...", i).yellow().bold());
            if self.run_hermit(common_args)? {
                success_count += 1;
            } else {
                fail_count += 1;
            }

            if success_count > 0 && fail_count > 0 {
                return Ok(true);
            }
        }

        Ok(false)
    }
}
