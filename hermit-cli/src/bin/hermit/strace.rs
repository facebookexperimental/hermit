/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::PathBuf;

use clap::Parser;
use hermit::Error;
use hermit::ExitStatus;

use super::container::PolicyRefusal;
use super::global_opts::GlobalOpts;

/// Arguments for the executable-selected syscall tracing command.
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
    pub fn main_with_backend<A: super::backend_hook::BackendAdapter>(
        &self,
        global: &GlobalOpts,
        adapter: &A,
    ) -> Result<ExitStatus, Error> {
        if global.log.is_some() || global.log_file.is_some() {
            anyhow::bail!(
                "the {} strace backend does not support --log or --log-file",
                A::NAME
            );
        }
        // An executable without a tracer refuses the command itself, so
        // `--max-log-bytes` neither changes that refusal nor is refused for a
        // runner that does not exist. Both checks are static facts.
        if !A::SUPPORTS_STRACE {
            return Err(Error::new(PolicyRefusal).context(format!(
                "executable-selected backend `{}` does not support syscall tracing",
                A::NAME
            )));
        }
        // This physical tracer does not use the core's charged writers or
        // container owner. Refuse the cap before any adapter/artifact work.
        if global.max_log_bytes.is_some() {
            return Err(Error::new(PolicyRefusal).context(format!(
                "--max-log-bytes cannot be enforced with {} strace: the runner's trace \
                 output does not pass through hermit's charged writers, and the runner \
                 is a plain child with no PID namespace, parent-death signal or ptrace \
                 attachment binding it, so the cap could neither count the trace nor \
                 stop the guest; drop --max-log-bytes",
                A::NAME
            )));
        }
        adapter.strace(&self.program, &self.args)
    }
}
