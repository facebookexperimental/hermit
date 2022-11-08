// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

use std::path::PathBuf;

use clap::Parser;
use hermit::Context;
use hermit::Error;
use hermit::HermitData;
use hermit::Id;
use reverie::ExitStatus;

use super::global_opts::GlobalOpts;

/// Subcommand for removing a previous recording.
#[derive(Debug, Parser)]
pub struct RemoveOpts {
    /// The ID of the recording to delete.
    #[clap(value_name = "ID")]
    id: Id,

    /// Directory where recorded syscall data is stored.
    #[clap(long, value_name = "DIR", env = "HERMIT_DATA_DIR")]
    data_dir: Option<PathBuf>,
}

impl RemoveOpts {
    pub fn main(&self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        let _guard = global.init_tracing();

        let hermit = HermitData::from(self.data_dir.as_ref());

        hermit
            .remove(self.id)
            .with_context(|| format!("Failed to delete recording {}", self.id))?;

        Ok(ExitStatus::SUCCESS)
    }
}
