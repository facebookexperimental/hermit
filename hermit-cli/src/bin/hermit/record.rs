/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use clap::Parser;
use hermit::Error;
use reverie::process::ExitStatus;

use super::global_opts::GlobalOpts;
use crate::clean::CleanOpts;
use crate::list::ListOpts;
use crate::record_start::StartOpts;
use crate::remove::RemoveOpts;

/// Command-line options for the "record" subcommand.
#[derive(Debug, Parser)]
pub struct RecordOpts {
    ///Subcommands of record
    #[clap(subcommand)]
    record_commands: RecordCommand,
}

#[derive(Debug, Parser)]
enum RecordCommand {
    /// List the available recordings.
    #[clap(name = "list", alias = "ls")]
    List(ListOpts),

    /// Delete a specific recording.
    #[clap(name = "rm", alias = "remove")]
    Remove(RemoveOpts),

    /// Cleans up all past recordings.
    #[clap(name = "clean")]
    Clean(CleanOpts),

    /// Start recording.
    #[clap(name = "start")]
    Start(StartOpts),
}

impl RecordOpts {
    pub fn main(&self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        match &self.record_commands {
            RecordCommand::List(x) => x.main(global),
            RecordCommand::Remove(x) => x.main(global),
            RecordCommand::Clean(x) => x.main(global),
            RecordCommand::Start(x) => x.main(global),
        }
    }
}
