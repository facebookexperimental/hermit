/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use colored::Colorize;

use super::UseCase;
use crate::common::LogDiffOptions;
use crate::common::RunEnvironment;
use crate::common::TestArtifacts;
use crate::common::Verify;
use crate::use_case::run_hermit::output_stderr;
use crate::use_case::run_hermit::output_stdout;
use crate::CommonOpts;

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

    if options.should_log_diff() {
        result &= verify.verify_logs(
            left,
            right,
            LogDiffOptions {
                ignore_lines: options.ignore_lines,
                skip_commits: !options.verify_commits,
                skip_detlog_others: !options.verify_detlog_others,
                skip_detlog_syscalls: !options.verify_detlog_syscalls,
                skip_detlog_syscall_results: !options.verify_detlog_syscall_results,
                syscall_history: 5,
            },
        )?;
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

pub fn run_use_case<T: UseCase>(use_case: T, common_args: &CommonOpts) -> anyhow::Result<bool> {
    let temp_env = use_case.build_temp_env(common_args).build()?;
    let mut iter = temp_env.runs().iter();
    let mut current = iter.next().expect("use case must contain at least one run");
    let mut counter = 0;

    let options = use_case.options();

    println!("{}", format!(":: Run #{}...", counter + 1).yellow().bold());

    use_case.exec_first_run(common_args, &temp_env, current)?;

    let mut result = true;
    // Compare each run to the one before it (rather than comparing all to the first):
    loop {
        counter += 1;
        if let Some(next) = iter.next() {
            println!("{}", format!(":: Run #{}...", counter + 1).yellow().bold());

            use_case.exec_next_run(common_args, counter + 1, &temp_env, current, next)?;

            if !compare_results(&use_case, common_args, current, next)? {
                result = false;
                break;
            } else {
                // during verification we'll get a diff view so we're skipping printing here
                // it only makes sense to output the stdout/stderr for the first run (if any)
                if counter == 1 {
                    if options.output_stdout {
                        output_stdout(current)?;
                    }
                    if options.output_stderr {
                        output_stderr(current)?;
                    }
                }
            }
            current = next;
        } else {
            break;
        }
    }

    if !common_args.ignore_test_result_artifacts_dir {
        if let Some(path) = &common_args.test_result_artifact_dir {
            println!(
                "{}",
                format!(":: Copy artifacts to {}", path.display())
                    .yellow()
                    .bold()
            );
            let test_artifacts = TestArtifacts::new(path.to_owned());
            test_artifacts.copy_run_results(&temp_env)?;
        }
    }
    Ok(result)
}
