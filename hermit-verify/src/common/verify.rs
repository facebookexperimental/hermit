/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::ffi::OsStr;
use std::io::BufRead;
use std::io::BufReader;
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

    fn verify_lines<S1: AsRef<str>, S2: AsRef<str>>(left: S1, right: S2) -> anyhow::Result<bool> {
        let result = similar::TextDiff::configure()
            .algorithm(similar::Algorithm::Myers)
            .diff_lines(left.as_ref(), right.as_ref());

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

    fn verify_files(left: &Path, right: &Path) -> anyhow::Result<bool> {
        let left = std::fs::read_to_string(left)?;
        let right = std::fs::read_to_string(right)?;
        Self::verify_lines(left, right)
    }

    fn format_json<TPath: AsRef<Path>>(path: TPath) -> anyhow::Result<String> {
        let value: serde_json::Value =
            serde_json::from_reader(std::fs::File::open(path.as_ref())?)?;
        let schedules = match &value {
            serde_json::Value::Object(sched_file) => sched_file.get("global").ok_or_else(|| {
                anyhow::Error::msg("expecting \"global\" key in the target json file")
            }),
            _ => Err(anyhow::Error::msg(format!(
                "{} has unexpected format",
                path.as_ref().display()
            ))),
        };
        Ok(serde_json::to_string_pretty(schedules?)?)
    }

    pub fn verify_schedules(
        &self,
        left: &RunEnvironment,
        right: &RunEnvironment,
    ) -> anyhow::Result<bool> {
        println!(
            "{}",
            "::  Checking that event schedules match (fixed point)".bold()
        );
        Self::verify_lines(
            Self::format_json(&left.schedule_file)?,
            Self::format_json(&right.schedule_file)?,
        )
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

    pub fn verify_desync(&self, right: &RunEnvironment) -> anyhow::Result<bool> {
        //FIXME: extract log-diff out of hermit and handle DESYNCs there (T135657122 + some extra work)
        println!("{}", "::  Looking for desync events:".bold());
        let buffer = BufReader::new(std::fs::File::open(&right.log_file_path)?);
        for line in buffer.lines() {
            if line?.contains("DESYNC") {
                println!("{}", "WARNING: DESYNCs found".red());
                return Ok(false);
            }
        }
        println!("DESYNC events not found");
        Ok(true)
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

    #[test]
    fn test_compare_desync() -> anyhow::Result<()> {
        let env = TemporaryEnvironmentBuilder::new().run_count(3).build()?;

        write_to_file(&env.runs()[0].log_file_path, "DESYNC Hello")?;
        write_to_file(&env.runs()[1].log_file_path, "Test 1")?;
        write_to_file(&env.runs()[2].log_file_path, "Test 1: TIME-DESYNC")?;

        let verify = Verify::new(PathBuf::from("hermit"));
        let logs_equal = [
            verify.verify_desync(&env.runs()[0])?,
            verify.verify_desync(&env.runs()[1])?,
            verify.verify_desync(&env.runs()[2])?,
        ];
        assert_eq!(logs_equal, [false, true, false]);

        Ok(())
    }

    #[test]
    fn test_compare_schedules() -> anyhow::Result<()> {
        let env = TemporaryEnvironmentBuilder::new().run_count(4).build()?;
        let verify = Verify::new("hermit");
        write_to_file(
            &env.runs()[0].schedule_file,
            r#"{ "global" : { "name" : "name1"} }"#,
        )?;
        write_to_file(
            &env.runs()[1].schedule_file,
            r#"{ "global" : { "name" : "name2"} }"#,
        )?;
        write_to_file(
            &env.runs()[2].schedule_file,
            r#"{ "global" : { "name" : "name123"} }"#,
        )?;
        write_to_file(
            &env.runs()[3].schedule_file,
            r#"{ "some": "value", "global" : { "name" : "name123"} }"#,
        )?;

        let result = [
            verify.verify_schedules(&env.runs()[0], &env.runs()[1])?,
            verify.verify_schedules(&env.runs()[2], &env.runs()[3])?,
        ];
        assert_eq!(result, [false, true]);
        Ok(())
    }

    fn write_to_file<P: AsRef<Path>>(file_path: &P, content: &str) -> anyhow::Result<()> {
        let mut file = File::create(file_path.as_ref())?;
        write!(file, "{}", content)?;
        Ok(())
    }
}
