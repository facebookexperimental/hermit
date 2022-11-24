/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::os::unix::prelude::ExitStatusExt;

use colored::Colorize;

use crate::cli_wrapper::build_hermit_cmd;
use crate::cli_wrapper::display_cmd;
use crate::common::RunEnvironment;
use crate::CommonOpts;

pub fn output_stdout(run: &RunEnvironment) -> anyhow::Result<()> {
    println!(":: ------------------Stdout----------------------");
    println!(
        "{}",
        std::fs::read_to_string(run.std_out_file_path.as_path())?
    );
    println!(":: ----------------End Stdout--------------------");
    println!();
    Ok(())
}

pub fn output_stderr(run: &RunEnvironment) -> anyhow::Result<()> {
    println!(":: ------------------Stderr----------------------");
    println!(
        "{}",
        std::fs::read_to_string(run.std_err_file_path.as_path())?
    );
    println!(":: ----------------End Stderr--------------------");
    println!();
    Ok(())
}

pub fn run_hermit(
    hermit_cli_args: Vec<String>,
    common_args: &CommonOpts,
    run: &RunEnvironment,
) -> anyhow::Result<()> {
    let mut command = build_hermit_cmd(&common_args.hermit_bin, hermit_cli_args, run)?;
    println!("   {}", display_cmd(&command).white().bold());
    let status = command.status()?;
    println!(
        "Executed command with status: {}",
        status.code().unwrap_or(0)
    );
    println!();

    std::fs::write(&run.exit_status_file_path, format!("{}", status.into_raw()))?;
    if status.success() || common_args.allow_nonzero_exit {
        Ok(())
    } else {
        println!(
            "{}",
            "Failing run because of non-zero exit code.".red().bold()
        );
        println!(
            "{}",
            "Try --allow-nonzero-exit if this is desired."
                .yellow()
                .bold()
        );
        let _ = output_stdout(run);
        let _ = output_stderr(run);
        anyhow::bail!("Nonzero exit not allowed.")
    }
}
