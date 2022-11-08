/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::PathBuf;

use clap::Parser;
use colored::Colorize;
use hermit::Context;
use hermit::Error;
use hermit::HermitData;
use reverie::ExitStatus;

use super::global_opts::GlobalOpts;

/// Command-line options for the "clean" subcommand.
#[derive(Debug, Parser)]
pub struct CleanOpts {
    /// Directory where recorded syscall data is stored.
    #[clap(long, value_name = "DIR", env = "HERMIT_DATA_DIR")]
    data_dir: Option<PathBuf>,
}

impl CleanOpts {
    pub fn main(&self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        let _guard = global.init_tracing();

        let hermit = HermitData::from(self.data_dir.as_ref());

        for id in hermit.recordings() {
            println!("{} {} ...", "Deleting".red().bold(), id.to_string().bold());
            hermit
                .remove(id)
                .with_context(|| format!("Failed to delete recording {}", id))?;
        }

        Ok(ExitStatus::SUCCESS)
    }
}
