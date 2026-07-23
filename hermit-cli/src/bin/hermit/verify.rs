/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::io;
use std::path::PathBuf;

use colored::Colorize;
use detcore::logdiff;
use hermit::Error;
use pretty_assertions::Comparison;
use reverie::process::ExitStatus;
use reverie::process::Output;
use tempfile::NamedTempFile;
use tempfile::TempPath;
use tracing::metadata::LevelFilter;

use super::global_opts::GlobalOpts;

pub(crate) struct ComparedRun<'a> {
    pub output: &'a Output,
    pub log: TempPath,
}

pub(crate) struct ComparisonOptions<'a> {
    pub success_message: &'a str,
    pub failure_message: &'a str,
    pub verbose: bool,
}

pub fn temp_log_files(name1: &str, name2: &str) -> io::Result<(NamedTempFile, NamedTempFile)> {
    let file1 = tempfile::Builder::new()
        .prefix(&format!("{}_log_", name1))
        .rand_bytes(5)
        .tempfile()?;
    let file2 = tempfile::Builder::new()
        .prefix(&format!("{}_log_", name2))
        .rand_bytes(5)
        .tempfile()?;

    Ok((file1, file2))
}

pub fn setup_double_run(
    global: &GlobalOpts,
    name1: &str,
    name2: &str,
) -> ((GlobalOpts, NamedTempFile), (GlobalOpts, NamedTempFile)) {
    let (file1, file2) = temp_log_files(name1, name2).unwrap();

    let path1 = PathBuf::from(file1.path());
    let path2 = PathBuf::from(file2.path());

    // Override global settings.  Unfortunately we lose the log output to the
    // screen.
    let mut global = global.clone();
    global.log_file = Some(path1);
    global.log = Some(LevelFilter::DEBUG);

    let mut global2 = global.clone();
    global2.log_file = Some(path2);
    ((global, file1), (global2, file2))
}

pub fn compare_two_runs(
    first: ComparedRun<'_>,
    second: ComparedRun<'_>,
    options: ComparisonOptions<'_>,
) -> Result<ExitStatus, Error> {
    let ComparedRun {
        output: out1,
        log: log1,
    } = first;
    let ComparedRun {
        output: out2,
        log: log2,
    } = second;
    let mut failed = false;

    if out1.stdout != out2.stdout {
        failed = true;
        eprintln!("Mismatch in stdout between run 1 and run 2:");
        let str1 = String::from_utf8_lossy(&out1.stdout);
        let str2 = String::from_utf8_lossy(&out2.stdout);
        if str1.lines().count() > 1 {
            display_diff(&str1, &str2);
        } else {
            eprintln!("{}", Comparison::new(&str1, &str2));
        }
    }

    if out1.stderr != out2.stderr {
        failed = true;
        eprintln!("Mismatch in stderr between run 1 and run 2:");
        let str1 = String::from_utf8_lossy(&out1.stderr);
        let str2 = String::from_utf8_lossy(&out2.stderr);
        if str1.lines().count() > 1 {
            display_diff(&str1, &str2);
        } else {
            eprintln!("{}", Comparison::new(&str1, &str2));
        }
    }

    eprintln!(
        ":: {} {} and {}",
        "Comparing logs...".yellow().bold(),
        log1.display(),
        log2.display()
    );

    let mut diff_options = logdiff::LogDiffOpts {
        strip_lines: true,
        syscall_history: 5,
        ..Default::default()
    };
    if options.verbose {
        diff_options.comparison = logdiff::LogComparisonMode::FullTrace;
        diff_options.strip_lines = false;
        diff_options.syscall_history = 10;
    }

    if logdiff::log_diff(log1.as_ref(), log2.as_ref(), &diff_options) {
        failed = true;
        eprintln!(":: {}", "Log differences found between runs.".red().bold());
        eprintln!(
            ":: {}: {} {}",
            "Respective Logs retained for further inspection".red(),
            log1.display(),
            log2.display()
        );
    }

    if out1.status != out2.status {
        failed = true;
        eprintln!(
            "Mismatch in exit status between run 1 and run 2: {}",
            Comparison::new(&out1.status, &out2.status)
        );
    }

    if failed {
        eprintln!(":: {}", options.failure_message.red().bold());
        let _ = log1.keep()?;
        let _ = log2.keep()?;
        Err(Error::msg(
            "Mismatch between run 1 and run 2 outputs (logs retained).",
        ))
    } else {
        // Allow the NamedTempFiles to be deleted in this case:
        eprintln!(":: {}", options.success_message.green().bold());
        Ok(out2.status)
    }
}

fn display_diff(left: &str, right: &str) {
    for result in diff::lines(left, right) {
        match result {
            diff::Result::Left(s) => {
                eprintln!("- {}", s.red());
            }
            diff::Result::Right(s) => {
                eprintln!("+ {}", s.green());
            }
            diff::Result::Both(s, _) => {
                eprintln!("  {}", s);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn output(status: i32, stdout: &[u8], stderr: &[u8]) -> Output {
        Output {
            status: ExitStatus::Exited(status),
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        }
    }

    fn empty_logs() -> (TempPath, TempPath) {
        let (left, right) = temp_log_files("verify_left", "verify_right").unwrap();
        (left.into_temp_path(), right.into_temp_path())
    }

    fn compare(
        left: &Output,
        left_log: TempPath,
        right: &Output,
        right_log: TempPath,
    ) -> Result<ExitStatus, Error> {
        compare_two_runs(
            ComparedRun {
                output: left,
                log: left_log,
            },
            ComparedRun {
                output: right,
                log: right_log,
            },
            ComparisonOptions {
                success_message: "verified",
                failure_message: "failed",
                verbose: false,
            },
        )
    }

    #[test]
    fn identical_outputs_verify_successfully() {
        let left = output(0, b"hello\n", b"");
        let right = left.clone();
        let (log1, log2) = empty_logs();

        assert_eq!(
            compare(&left, log1, &right, log2).unwrap(),
            ExitStatus::Exited(0)
        );
    }

    #[test]
    fn stdout_stderr_and_status_mismatches_fail_verification() {
        let baseline = output(0, b"hello\n", b"");
        let mismatches = [
            output(0, b"different\n", b""),
            output(0, b"hello\n", b"different\n"),
            output(1, b"hello\n", b""),
        ];

        for mismatch in mismatches {
            let (log1, log2) = empty_logs();
            let path1 = log1.to_path_buf();
            let path2 = log2.to_path_buf();

            assert!(compare(&baseline, log1, &mismatch, log2).is_err());

            let _ = fs::remove_file(path1);
            let _ = fs::remove_file(path2);
        }
    }
}
