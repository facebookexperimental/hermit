/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::os::unix::process::ExitStatusExt;

use colored::Colorize;

use crate::cli_wrapper::build_hermit_cmd;
use crate::cli_wrapper::display_cmd;
use crate::common::RunEnvironment;
use crate::common::TemporaryEnvironment;
use crate::common::TemporaryEnvironmentBuilder;
use crate::common::Verify;
use crate::CommonOpts;

pub struct UseCaseOptions {
    pub output_stdout: bool,
    pub output_stderr: bool,
    pub verify_stdout: bool,
    pub verify_stderr: bool,
    pub verify_detlog: bool,
    pub verify_desync: bool,
    pub verify_commits: bool,
    pub verify_exit_statuses: bool,
    pub verify_schedules: bool,
}

pub trait UseCase {
    fn options(&self) -> UseCaseOptions {
        UseCaseOptions {
            output_stdout: true,
            output_stderr: true,
            verify_stdout: true,
            verify_stderr: true,
            verify_detlog: true,
            verify_commits: true,
            verify_exit_statuses: true,
            verify_desync: true,
            verify_schedules: false,
        }
    }

    fn build_temp_env(&self, common_args: &CommonOpts) -> TemporaryEnvironmentBuilder;

    fn build_first_hermit_args(
        &self,
        temp_env: &TemporaryEnvironment,
        current_run: &RunEnvironment,
    ) -> Vec<String>;

    fn build_next_hermit_args(
        &self,
        run_no: usize,
        temp_env: &TemporaryEnvironment,
        prev_run: &RunEnvironment,
        current_run: &RunEnvironment,
    ) -> Vec<String>;
}

fn compare_results<T: UseCase>(
    use_case: &T,
    common_args: &CommonOpts,
    left: &RunEnvironment,
    right: &RunEnvironment,
) -> anyhow::Result<bool> {
    let options = use_case.options();
    let verify = Verify::new(common_args.hermit_bin.as_path());
    let mut result = true;

    if options.verify_stdout {
        result &= verify.verify_stdout(left, right)?;
    }

    if options.verify_stderr {
        result &= verify.verify_stderr(left, right)?;
    }

    if options.verify_commits || options.verify_detlog {
        result &=
            verify.verify_logs(left, right, !options.verify_detlog, !options.verify_commits)?;
    }

    if options.verify_exit_statuses {
        result &= verify.verify_exit_statuses(left, right)?;
    }

    if options.verify_desync {
        result &= verify.verify_desync(right)?;
    }

    if options.verify_schedules {
        result &= verify.verify_schedules(left, right)?;
    }

    Ok(result)
}

fn output_stdout(run: &RunEnvironment, options: &UseCaseOptions) -> anyhow::Result<()> {
    if options.output_stdout {
        println!(":: ------------------Stdout----------------------");
        println!(
            "{}",
            std::fs::read_to_string(run.std_out_file_path.as_path())?
        );
        println!(":: ----------------End Stdout--------------------");
        println!();
    }
    Ok(())
}

fn output_stderr(run: &RunEnvironment, options: &UseCaseOptions) -> anyhow::Result<()> {
    if options.output_stderr {
        println!(":: ------------------Stderr----------------------");
        println!(
            "{}",
            std::fs::read_to_string(run.std_err_file_path.as_path())?
        );
        println!(":: ----------------End Stderr--------------------");
        println!();
    }
    Ok(())
}

fn run_hermit(
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
        anyhow::bail!("Nonzero exit not allowed.")
    }
}

pub fn run_use_case<T: UseCase>(use_case: T, common_args: &CommonOpts) -> anyhow::Result<bool> {
    let temp_env = use_case.build_temp_env(common_args).build()?;
    let mut iter = temp_env.runs().iter();
    let mut current = iter.next().expect("use case must contain at least one run");
    let mut counter = 0;

    let options = use_case.options();

    println!("{}", format!(":: Run #{}...", counter + 1).yellow().bold());

    run_hermit(
        use_case.build_first_hermit_args(&temp_env, current),
        common_args,
        current,
    )?;

    // Compare each run to the one before it (rather than comparing all to the first):
    loop {
        counter += 1;
        if let Some(next) = iter.next() {
            println!("{}", format!(":: Run #{}...", counter + 1).yellow().bold());
            run_hermit(
                use_case.build_next_hermit_args(counter + 1, &temp_env, current, next),
                common_args,
                next,
            )?;

            if !compare_results(&use_case, common_args, current, next)? {
                return Ok(false);
            } else {
                // during verification we'll get a diff view so we're skipping printing here
                // it only makes sense to output the stdout/stderr for the first run (if any)
                if counter == 1 {
                    output_stdout(current, &options)?;
                    output_stderr(current, &options)?;
                }
            }
            current = next;
        } else {
            break;
        }
    }
    Ok(true)
}
