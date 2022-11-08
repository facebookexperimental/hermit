// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

use std::path::PathBuf;

use clap::Parser;
use colored::Colorize;
use hermit::Error;
use hermit::HermitData;
use reverie::ExitStatus;

use super::global_opts::GlobalOpts;

/// Command-line options for the "list" subcommand.
#[derive(Debug, Parser)]
pub struct ListOpts {
    /// Directory where recorded syscall data is stored.
    #[clap(long, value_name = "DIR", env = "HERMIT_DATA_DIR")]
    data_dir: Option<PathBuf>,
}

impl ListOpts {
    pub fn main(&self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        let _guard = global.init_tracing();

        let hermit = HermitData::from(self.data_dir.as_ref());

        for id in hermit.recordings() {
            let metadata = hermit.recording_metadata(id)?;

            println!(
                "{id}  {program} {args}",
                id = id.to_string().bold(),
                program = metadata.program.cyan().bold(),
                args = metadata.args.join(" ").bold().dimmed(),
            );
        }

        Ok(ExitStatus::SUCCESS)
    }
}
