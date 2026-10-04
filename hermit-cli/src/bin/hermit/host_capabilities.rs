/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::io::Write;

use clap::Args;
use detcore_model::host_capability::HostCapabilitiesReport;
use hermit::Error;
use hermit::ExitStatus;

#[derive(Debug, Args)]
pub struct HostCapabilitiesOpts {
    /// Emit the complete producer-owned capability record as one JSON object.
    #[clap(long)]
    json: bool,
}

impl HostCapabilitiesOpts {
    pub fn main(&self) -> Result<ExitStatus, Error> {
        let report = HostCapabilitiesReport::probe();
        report.validate().map_err(Error::msg)?;
        let mut stdout = std::io::stdout().lock();
        if self.json {
            serde_json::to_writer(&mut stdout, &report)?;
            writeln!(stdout)?;
        } else {
            for (capability, verdict) in report.host_capabilities {
                writeln!(
                    stdout,
                    "{}: {} — {}",
                    capability.value(),
                    if verdict.present { "PRESENT" } else { "ABSENT" },
                    verdict.evidence
                )?;
            }
        }
        Ok(ExitStatus::Exited(0))
    }
}
