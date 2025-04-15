/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::PathBuf;

use clap::Parser;

use crate::CommonOpts;
use crate::cli_wrapper::*;
use crate::common::TemporaryEnvironment;
use crate::common::TemporaryEnvironmentBuilder;
use crate::use_case::UseCase;

/// Verification utility for replaying preemptions under hermit
/// This utility runs a guest program under "hermit run" and records schedules and preemptions. On the second run those recorded schedules are replayed via "hermit run" subcommand
/// The outputs (stdout, stderr, log file, schedules) are captured and placed in a temporary directory allowing further inspection (if --keep-temp-dir is provided)
#[derive(Parser, Debug)]
pub struct ChaosReplayOpts {
    /// Whether to keep temp directory created for each container run. This directory contains (stdout, stderr, log file, etc) of the container process
    /// This allows manual inspection of container outputs in cases when a cause of failure is unclear
    #[clap(long)]
    keep_temp_dir: bool,

    /// Underlying hermit container will receive "isolated" workdir
    #[clap(long)]
    isolate_workdir: bool,

    /// Additional arguments for hermit run subcommand
    #[clap(long)]
    hermit_arg: Vec<String>,

    /// Path to a guest program
    #[clap(value_name = "PROGRAM")]
    guest_program: PathBuf,
    /// Arguments for a guest program
    #[clap(value_name = "ARGS")]
    args: Vec<String>,
}

impl UseCase for ChaosReplayOpts {
    fn build_temp_env(
        &self,
        _common_args: &CommonOpts,
    ) -> crate::common::TemporaryEnvironmentBuilder {
        TemporaryEnvironmentBuilder::new()
            .persist_temp_dir(self.keep_temp_dir)
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
        Hermit::new()
            .log_level(tracing::Level::TRACE)
            .log_file(current_run.log_file_path.clone())
            .run(self.guest_program.clone(), self.args.clone())
            .hermit_args(self.hermit_arg.clone())
            .bind(temp_env.path().to_owned())
            .workdir_isolate(current_run.workdir.clone(), self.isolate_workdir)
            .replay_preemptions_from(prev_run.schedule_file.clone())
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
            verify_detlog_others: true,
            verify_commits: false,
            verify_exit_statuses: true,
            verify_desync: true,
            verify_schedules: true,
            ignore_lines: vec![String::from("CHAOSRAND")],
        }
    }
}
