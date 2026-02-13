/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs::File;
use std::path::PathBuf;

use clap::Parser;
use tracing::metadata::LevelFilter;

use super::tracing::init_file_tracing;
use super::tracing::init_stderr_tracing;

/// Hermit provides a sandbox for deterministic and reproducible execution.
/// Arbitrary programs run inside (guests) become deterministic
/// functions of their inputs. Configuration flags control the initial
/// environment.
///
/// See the "run" and "record" subcommands to run programs within hermit.
/// In both modes, the host file system is visible
/// to the command run inside hermit, and the results will depend on the contents
/// (but not timestamps or inode numbers) of those inputs.
///
/// In run mode, networking is disallowed.  Run mode guarantees that if you
/// run twice with the same input files, you will receive bitwise identical
/// outputs from the computation.
///
/// In record mode, inputs (both files and network traffic) are captured
/// in a content addressible store (CAS).  In this preview version of
/// hermit, the CAS is stored locally in your home directory (~/.hermit).
///
/// Below are options common to all subcommands.
#[derive(Debug, Parser, Clone)]
pub struct GlobalOpts {
    /// The verbosity level of log output.
    #[clap(short, long, value_name = "LEVEL", env = "HERMIT_LOG")]
    pub log: Option<LevelFilter>,

    /// Log to a file instead of the terminal.
    #[clap(long, value_name = "FILE", env = "HERMIT_LOG_FILE")]
    pub log_file: Option<PathBuf>,
}

impl GlobalOpts {
    /// Initalizes tracing. If using a container, this must be done *inside* of
    /// the container because the tracer may create a new thread.
    #[must_use = "This function returns a guard that should not be immediately dropped"]
    pub fn init_tracing(&self) -> Option<impl Drop + use<>> {
        if let Some(path) = &self.log_file {
            let file_writer = File::create(path).expect("Failed to open log file");
            Some(init_file_tracing(self.log, file_writer))
        } else {
            init_stderr_tracing(self.log);
            None
        }
    }
}
