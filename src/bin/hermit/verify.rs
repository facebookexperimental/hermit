// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

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
    out1: &Output,
    log1: TempPath,
    out2: &Output,
    log2: TempPath,
    success_msg: &str,
    failure_msg: &str,
) -> Result<ExitStatus, Error> {
    let mut failed = false;

    if out1.stdout != out2.stdout {
        failed = true;
        eprintln!("Mismatch in stdout between runs:",);
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
        eprintln!("Mismatch in stderr between runs:",);
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

    // TODO(T103558443) stripping logs until this task is completely closed:
    if logdiff::log_diff(
        log1.as_ref(),
        log2.as_ref(),
        &logdiff::LogDiffOpts {
            strip_lines: true,
            syscall_history: 5,
            ..Default::default()
        },
    ) {
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
            "Mismatch in exit status between runs: {}",
            Comparison::new(&out1.status, &out2.status)
        );
    }

    if failed {
        eprintln!(":: {}", failure_msg.red().bold());
        let _ = log1.keep()?;
        let _ = log2.keep()?;
        Err(Error::msg(
            "Mismatch between run1 and run2 outputs (logs retained).",
        ))
    } else {
        // Allow the NamedTempFiles to be deleted in this case:
        eprintln!(":: {}", success_msg.green().bold());
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
