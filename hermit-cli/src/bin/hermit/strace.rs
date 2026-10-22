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

use super::container::PolicyRefusal;
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
            Some(Backend::Sabre) => {
                // `--max-log-bytes` cannot be enforced here, so it is refused
                // rather than accepted and ignored (round-3 review of
                // https://github.com/rrnewton/hermit/pull/3686, finding 6).
                // This command does not go through `RunOpts::main` and its
                // `refuse_unsupervised_log_cap`: `run_sabre_strace` starts the
                // runner with a plain `Command` (`backends::sabre_command`: no
                // `pre_exec`, no namespace, no ptrace) and only waits for its
                // status. The trace goes to the inherited stderr and this
                // command initializes no tracing, so no byte of it is charged,
                // and nothing ties the runner to this process if the cap were
                // to end it. The check comes before `sabre_artifacts`, so the
                // refusal does not depend on any SaBRe artifact being present.
                if global.max_log_bytes.is_some() {
                    return Err(Error::new(PolicyRefusal).context(
                        "--max-log-bytes cannot be enforced with strace --backend=sabre: the \
                         SaBRe runner's trace output does not pass through hermit's charged \
                         writers, and the runner is a plain child of hermit with no PID \
                         namespace, parent-death signal or ptrace attachment binding it, so \
                         the cap could neither count the trace nor stop the guest; drop \
                         --max-log-bytes",
                    ));
                }
                super::backends::run_sabre_strace(&self.program, &self.args)
            }
            Some(backend) => anyhow::bail!(
                "the M1 strace command requires `--backend sabre`, not `--backend {}`",
                backend.as_str()
            ),
            None => anyhow::bail!("the M1 strace command requires `--backend sabre`"),
        }
    }
}
