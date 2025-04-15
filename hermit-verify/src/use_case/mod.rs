/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

mod run_hermit;
mod run_usecase;
mod split_branches;

pub use run_hermit::run_hermit;
pub use run_usecase::run_use_case;
pub use split_branches::split_branches_in_file;

use crate::CommonOpts;
use crate::common::RunEnvironment;
use crate::common::TemporaryEnvironment;
use crate::common::TemporaryEnvironmentBuilder;

pub struct UseCaseOptions {
    pub output_stdout: bool,
    pub output_stderr: bool,
    pub verify_stdout: bool,
    pub verify_stderr: bool,
    pub verify_detlog_syscalls: bool,
    pub verify_detlog_syscall_results: bool,
    pub verify_detlog_others: bool,
    pub verify_desync: bool,
    pub verify_commits: bool,
    pub verify_exit_statuses: bool,
    pub verify_schedules: bool,
    pub ignore_lines: Vec<String>,
}

impl UseCaseOptions {
    fn should_log_diff(&self) -> bool {
        self.verify_commits
            || self.verify_detlog_others
            || self.verify_detlog_syscalls
            || self.verify_detlog_syscall_results
    }
}

pub trait UseCase {
    fn options(&self) -> UseCaseOptions {
        UseCaseOptions {
            output_stdout: true,
            output_stderr: true,
            verify_stdout: true,
            verify_stderr: true,
            verify_detlog_syscalls: true,
            verify_detlog_syscall_results: true,
            verify_detlog_others: true,
            verify_commits: true,
            verify_exit_statuses: true,
            verify_desync: true,
            verify_schedules: false,
            ignore_lines: Vec::new(),
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

    fn exec_first_run(
        &self,
        common_args: &CommonOpts,
        temp_env: &TemporaryEnvironment,
        run: &RunEnvironment,
    ) -> anyhow::Result<()> {
        run_hermit(
            self.build_first_hermit_args(temp_env, run),
            common_args,
            run,
        )
    }

    fn exec_next_run(
        &self,
        common_args: &CommonOpts,
        counter: usize,
        temp_env: &TemporaryEnvironment,
        prev_run: &RunEnvironment,
        current_run: &RunEnvironment,
    ) -> anyhow::Result<()> {
        run_hermit(
            self.build_next_hermit_args(counter + 1, temp_env, prev_run, current_run),
            common_args,
            current_run,
        )
    }
}
