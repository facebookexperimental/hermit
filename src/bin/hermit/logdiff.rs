// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

use std::path::Path;
use std::path::PathBuf;

use clap::Parser;
use detcore::logdiff;
use reverie::process::ExitStatus;

use super::global_opts::GlobalOpts;

/// Command-line options for the "logdiff" subcommand.
#[derive(Debug, Parser)]
pub struct LogDiffCLIOpts {
    /// First log to compare.
    file_a: PathBuf,
    /// Second log to compare.
    file_b: PathBuf,

    #[clap(flatten)]
    more: logdiff::LogDiffOpts,
}

impl LogDiffCLIOpts {
    /// Construct LogDiffOpts to compare two files.
    pub fn new(a: &Path, b: &Path) -> Self {
        Self {
            file_a: PathBuf::from(a),
            file_b: PathBuf::from(b),
            more: Default::default(),
        }
    }

    /// Process log messages from two files.
    pub fn main(&self, _global: &GlobalOpts) -> ExitStatus {
        if logdiff::log_diff(&self.file_a, &self.file_b, &self.more) {
            ExitStatus::Exited(1)
        } else {
            ExitStatus::Exited(0)
        }
    }
}
