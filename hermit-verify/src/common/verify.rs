/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::ffi::OsStr;
use std::path::Path;

use colored::Colorize;

use super::RunEnvironment;
use crate::cli_wrapper::display_cmd;

pub struct Verify<P: AsRef<OsStr>> {
    hermit_bin: P,
}

impl<P: AsRef<OsStr>> Verify<P> {
    pub fn new(hermit_bin: P) -> Self {
        Self { hermit_bin }
    }

    fn verify_files(left: &Path, right: &Path) -> anyhow::Result<bool> {
        let left = std::fs::read_to_string(left)?;
        let right = std::fs::read_to_string(right)?;
        let result = similar::TextDiff::configure()
            .algorithm(similar::Algorithm::Myers)
            .diff_lines(&left, &right);

        if result.ratio() == 1.0 {
            Ok(true)
        } else {
            for c in result.iter_all_changes() {
                match c.tag() {
                    similar::ChangeTag::Equal => print!("{}", c),
                    similar::ChangeTag::Delete => {
                        print!("{}", format!("-{}", c).red().bold())
                    }
                    similar::ChangeTag::Insert => {
                        print!("{}", format!("+{}", c).green().bold())
                    }
                }
            }
            Ok(false)
        }
    }

    pub fn verify_stdout(
        &self,
        left: &RunEnvironment,
        right: &RunEnvironment,
    ) -> anyhow::Result<bool> {
        println!("{}", "::  Comparing stdout".bold());
        Self::verify_files(
            left.std_out_file_path.as_path(),
            right.std_out_file_path.as_path(),
        )
    }

    pub fn verify_stderr(
        &self,
        left: &RunEnvironment,
        right: &RunEnvironment,
    ) -> anyhow::Result<bool> {
        println!("{}", "::  Comparing stderr".bold());
        Self::verify_files(
            left.std_err_file_path.as_path(),
            right.std_err_file_path.as_path(),
        )
    }

    pub fn verify_logs(
        &self,
        left: &RunEnvironment,
        right: &RunEnvironment,
        skip_detlog: bool,
        skip_commit: bool,
    ) -> anyhow::Result<bool> {
        println!("{}", "::  Comparing log files".bold());
        let mut command = std::process::Command::new(&self.hermit_bin);
        command.arg("log-diff");
        command.arg("--syscall-history=5");
        if skip_detlog {
            command.arg("--skip-detlog");
        }
        if skip_commit {
            command.arg("--skip-commit");
        }
        command.arg(format!("{}", left.log_file_path.display()));
        command.arg(format!("{}", right.log_file_path.display()));

        println!("{}", format!("    {}", display_cmd(&command)).bold());
        Ok(command.status()?.success())
    }

    pub fn verify_exit_statuses(
        &self,
        left: &RunEnvironment,
        right: &RunEnvironment,
    ) -> anyhow::Result<bool> {
        println!("{}", "::  Comparing exit codes".bold());
        Self::verify_files(
            left.exit_status_file_path.as_path(),
            right.exit_status_file_path.as_path(),
        )
    }
}

#[cfg(test)]
mod test {
    use std::fs::File;
    use std::io::Write;
    use std::path::Path;
    use std::path::PathBuf;

    use pretty_assertions::assert_eq;

    use super::*;
    use crate::common::TemporaryEnvironmentBuilder;
    #[test]
    fn test_compare_equal_files() -> anyhow::Result<()> {
        let env = TemporaryEnvironmentBuilder::new().run_count(2).build()?;
        write_to_file(&env.runs()[0].std_out_file_path, "test1")?;
        write_to_file(&env.runs()[1].std_out_file_path, "test1")?;

        let verify = Verify::new(PathBuf::from("hermit"));

        let files_equal = verify.verify_stdout(&env.runs()[0], &env.runs()[1])?;

        assert_eq!(files_equal, true);

        Ok(())
    }

    #[test]
    fn test_compare_not_equal_files() -> anyhow::Result<()> {
        let env = TemporaryEnvironmentBuilder::new().run_count(2).build()?;
        write_to_file(&env.runs()[0].std_out_file_path, "test1")?;
        write_to_file(&env.runs()[1].std_out_file_path, "test2")?;

        let verify = Verify::new(PathBuf::from("hermit"));
        let files_equal = verify.verify_stdout(&env.runs()[0], &env.runs()[1])?;
        assert_eq!(files_equal, false);

        Ok(())
    }

    fn write_to_file(file_path: &Path, content: &str) -> anyhow::Result<()> {
        let mut file = File::create(file_path)?;
        write!(file, "{}", content)?;
        Ok(())
    }
}
