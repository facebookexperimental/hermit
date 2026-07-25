/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::PathBuf;

use clap::Parser;
use hermit::Backend;
use hermit::Error;
use hermit::ExitStatus;

use super::global_opts::GlobalOpts;

/// Arguments for the narrow SaBRe M1 syscall tracing command.
#[derive(Debug, Parser)]
pub struct StraceOpts {
    /// Program to trace.
    #[clap(value_name = "PROGRAM")]
    program: PathBuf,

    /// Arguments passed to the traced program.
    #[clap(
        value_name = "ARGS",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    args: Vec<String>,
}

impl StraceOpts {
    pub fn main(&self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        if global.log.is_some() || global.log_file.is_some() {
            anyhow::bail!("the SaBRe strace backend does not support --log or --log-file");
        }
        match global.backend {
            Some(Backend::Sabre) => super::backends::run_sabre_strace(&self.program, &self.args),
            Some(backend) => anyhow::bail!(
                "the M1 strace command requires `--backend sabre`, not `--backend {}`",
                backend.as_str()
            ),
            None => anyhow::bail!("the M1 strace command requires `--backend sabre`"),
        }
    }
}
