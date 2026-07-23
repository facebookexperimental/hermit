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
use hermit::Error;
use hermit::HermitData;
use reverie::ExitStatus;
use serde::Serialize;

use super::global_opts::GlobalOpts;

/// Command-line options for the "list" subcommand.
#[derive(Debug, Parser)]
pub struct ListOpts {
    /// Directory where recorded syscall data is stored.
    #[clap(long, value_name = "DIR", env = "HERMIT_DATA_DIR")]
    data_dir: Option<PathBuf>,

    /// Print the recording inventory as JSON.
    #[clap(long)]
    json: bool,
}

#[derive(Debug, Serialize)]
struct RecordingInfo {
    id: String,
    program: String,
    args: Vec<String>,
}

impl ListOpts {
    pub fn main(&self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        let _guard = global.init_tracing();

        let hermit = HermitData::from(self.data_dir.as_ref());

        let mut recordings = hermit
            .recordings()
            .map(|id| {
                let metadata = hermit.recording_metadata(id)?;
                Ok(RecordingInfo {
                    id: id.to_string(),
                    program: metadata.program,
                    args: metadata.args,
                })
            })
            .collect::<Result<Vec<_>, Error>>()?;
        recordings.sort_unstable_by(|left, right| left.id.cmp(&right.id));

        if self.json {
            println!("{}", serde_json::to_string(&recordings)?);
            return Ok(ExitStatus::SUCCESS);
        }

        for recording in recordings {
            println!(
                "{id}  {program} {args}",
                id = recording.id.bold(),
                program = recording.program.cyan().bold(),
                args = recording.args.join(" ").bold().dimmed(),
            );
        }

        Ok(ExitStatus::SUCCESS)
    }
}

#[cfg(test)]
mod tests {
    use super::RecordingInfo;

    #[test]
    fn recording_info_has_a_stable_json_shape() {
        let recording = RecordingInfo {
            id: "0123456789abcdef0123456789abcdef".to_string(),
            program: "/bin/echo".to_string(),
            args: vec!["hello".to_string()],
        };

        assert_eq!(
            serde_json::to_value(recording).unwrap(),
            serde_json::json!({
                "id": "0123456789abcdef0123456789abcdef",
                "program": "/bin/echo",
                "args": ["hello"],
            })
        );
    }
}
