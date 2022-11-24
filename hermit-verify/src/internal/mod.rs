/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

mod schedule_trace;

use clap::Parser;
pub use schedule_trace::SchedTraceOpts;

use crate::CommonOpts;

#[derive(Debug, Parser)]
pub struct InternalOpts {
    #[clap(subcommand)]
    pub internal_command: InternalSubcommand,
}

#[derive(Debug, Parser)]
pub enum InternalSubcommand {
    /// Schedule Trace
    #[clap(subcommand)]
    SchedTrace(SchedTraceOpts),
}

impl InternalSubcommand {
    pub fn main(&self, common_args: &CommonOpts) -> anyhow::Result<bool> {
        match self {
            InternalSubcommand::SchedTrace(x) => x.main(common_args),
        }
    }
}
