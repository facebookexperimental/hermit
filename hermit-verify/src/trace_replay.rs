/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::PathBuf;

use clap::Parser;
use detcore::preemptions::strip_times_from_events_file;

use crate::cli_wrapper::*;
use crate::common::TemporaryEnvironment;
use crate::common::TemporaryEnvironmentBuilder;
use crate::schedule_trace::InspectOpts;
use crate::use_case::split_branches_in_file;
use crate::use_case::UseCase;
use crate::CommonOpts;

/// Verification utility for replaying preemptions under hermit
/// This utility runs a guest program under "hermit run" and records schedules and preemptions. On the second run those recorded schedules are replayed via "hermit run" subcommand
/// The outputs (stdout, stderr, log file, schedules) are captured and placed in a temporary directory allowing further inspection (if --keep-temp-dir is provided)
#[derive(Parser, Debug)]
pub struct TraceReplayOpts {
    /// Whether to keep temp directory created for each container run. This directory contains (stdout, stderr, log file, etc) of the container process
    /// This allows manual inspection of container outputs in cases when a cause of failure is unclear
    #[clap(long, env = "KEEP_TEMP_DIR")]
    keep_temp_dir: bool,

    /// Underlying hermit container will receive "isolated" workdir
    #[clap(long)]
    isolate_workdir: bool,

    /// Active chaos mode in hermit run, and make the log-diff comparison appropriate.
    #[clap(long)]
    chaos: bool,

    /// Strip times from the schedule to make sure we can still replay it without.
    #[clap(long)]
    strip_times: bool,

    /// Additional argument for hermit run subcommand
    #[clap(long)]
    hermit_arg: Vec<String>,

    /// Path to a guest program
    #[clap(value_name = "PROGRAM")]
    guest_program: PathBuf,

    /// Whether to split blocks of multiple branch events in half
    #[clap(long)]
    split_branches: bool,

    /// Arguments for a guest program
    #[clap(value_name = "ARGS")]
    args: Vec<String>,
}

impl UseCase for TraceReplayOpts {
    fn build_temp_env(
        &self,
        common_args: &CommonOpts,
    ) -> crate::common::TemporaryEnvironmentBuilder {
        TemporaryEnvironmentBuilder::new()
            .persist_temp_dir(self.keep_temp_dir)
            .temp_dir_path(common_args.temp_dir_path.as_ref())
            .verbose(common_args.verbose)
            .run_count(2)
    }

    fn build_first_hermit_args(
        &self,
        temp_env: &TemporaryEnvironment,
        current_run: &crate::common::RunEnvironment,
    ) -> Vec<String> {
        Hermit::new()
            .log_level(tracing::Level::TRACE)
            .log_file(current_run.log_file_path.clone())
            .run(self.guest_program.clone(), self.args.clone())
            .hermit_args(self.hermit_arg.clone())
            .chaos(self.chaos)
            .bind(temp_env.path().to_owned())
            .workdir_isolate(current_run.workdir.clone(), self.isolate_workdir)
            .record_preemptions_to(current_run.schedule_file.clone())
            .into_args()
    }

    fn build_next_hermit_args(
        &self,
        _run_no: usize,
        temp_env: &TemporaryEnvironment,
        prev_run: &crate::common::RunEnvironment,
        current_run: &crate::common::RunEnvironment,
    ) -> Vec<String> {
        let replay_sched = if self.strip_times {
            strip_times_from_events_file(&prev_run.schedule_file, None).unwrap()
        } else {
            prev_run.schedule_file.clone()
        };

        Hermit::new()
            .log_level(tracing::Level::TRACE)
            .log_file(current_run.log_file_path.clone())
            .run(self.guest_program.clone(), self.args.clone())
            .hermit_args(self.hermit_arg.clone())
            .chaos(false) // We don't need chaos on replay.
            .bind(temp_env.path().to_owned())
            .workdir_isolate(current_run.workdir.clone(), self.isolate_workdir)
            .replay_schedule_from(replay_sched)
            .record_preemptions_to(current_run.schedule_file.clone())
            .into_args()
    }

    fn options(&self) -> crate::use_case::UseCaseOptions {
        crate::use_case::UseCaseOptions {
            output_stdout: true,
            output_stderr: true,
            verify_stdout: true,
            verify_stderr: true,
            verify_detlog_syscalls: true,
            verify_detlog_syscall_results: true,
            verify_detlog_others: !self.split_branches,
            verify_commits: false,
            verify_exit_statuses: true,
            verify_desync: true,
            verify_schedules: false,
            ignore_lines: if self.chaos {
                vec![
                    "CHAOSRAND".to_string(),
                    "advance global time for scheduler turn".to_string(),
                ]
            } else {
                Vec::new()
            },
        }
    }

    fn exec_first_run(
        &self,
        common_args: &CommonOpts,
        temp_env: &TemporaryEnvironment,
        run: &crate::common::RunEnvironment,
    ) -> anyhow::Result<()> {
        crate::use_case::run_hermit(
            self.build_first_hermit_args(temp_env, run),
            common_args,
            run,
        )?;

        if self.split_branches {
            split_branches_in_file(&run.schedule_file, true)?;
        }

        if common_args.verbose {
            println!("------------------ Recorded Schedule Summary ------------------");
            InspectOpts {
                sched_file: run.schedule_file.clone(),
                verbose: true,
            }
            .run(common_args)?;
            println!("---------------------------------------------------------------\n");
        }

        Ok(())
    }
}
