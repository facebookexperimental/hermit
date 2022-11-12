/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::PathBuf;

use clap::Parser;
use hermit::Context;
use hermit::Error;
use hermit::HermitData;
use hermit::Id;
use hermit::Shebang;
use reverie::process::ExitStatus;

use super::container::default_container;
use super::container::with_container;
use super::global_opts::GlobalOpts;

/// Command-line options for the "replay" subcommand.
#[derive(Debug, Parser)]
pub struct ReplayOpts {
    /// The ID of the recording to replay. This is obtained by first running
    /// `hermit record`. This is optional. Defaults to the last successful
    /// recording.
    #[clap(value_name = "ID")]
    id: Option<Id>,

    /// Directory where recorded syscall data is stored.
    #[clap(long, value_name = "DIR", env = "HERMIT_DATA_DIR")]
    data_dir: Option<PathBuf>,

    /// The port to use for the gdb server.
    #[clap(long, default_value = "1234")]
    // FIXME: This shouldn't exist. It'd be better to use a Unix domain socket.
    gdbserver_port: u16,

    /// Replay to the end without an attached debugger.
    #[clap(long, short)]
    autopilot: bool,

    /// Additional gdb command passed by `-ex`
    #[clap(long, value_delimiter = ';', use_delimiter = true)]
    gdbex: Vec<String>,
}

impl ReplayOpts {
    pub fn main(&self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        let hermit = HermitData::from(self.data_dir.as_ref());

        let id = match self.id {
            Some(id) => id,
            None => hermit
                .last_id()
                .context("Failed to find last recording ID")?,
        };

        if self.autopilot {
            let mut container = default_container(true);
            with_container(&mut container, || {
                self.container_main(global, self.autopilot, &hermit, id)
            })
        } else {
            // Find the path to the executable so that GDB can use it to resolve
            // symbols.
            let exe = hermit.data_dir().join(format!("{}/exe", id));
            let real_exe = Shebang::new(&exe).map_or(exe, |s| s.interpreter().into());

            // Run the gdb client outside of the PID namespace. This cannot be done
            // inside of the PID namespace because it would perturb the
            // deterministic PID allocation that is needed for the replay.
            let mut gdb_command = std::process::Command::new("gdb");
            gdb_command
                .arg(real_exe)
                .arg("-quiet")
                .arg("-iex")
                // don't prompt (dialog) when breakpoint symbol doesn't exist.
                .arg("set breakpoint pending on")
                .arg("-ex")
                .arg(format!("target remote :{}", self.gdbserver_port));
            for ex in &self.gdbex {
                gdb_command.arg("-ex").arg(ex);
            }
            let mut gdb_client = gdb_command
                .spawn()
                .context("Failed to run gdb command. Please make sure it is in your $PATH.")?;

            // TODO: For replay, we ought to construct the container from
            // `metadata.json`. That logic belongs in `hermit::replay`, but we have
            // to initialize logging inside the container because it may spawn a
            // thread. If we can guarantee that tracing won't spawn a thread, then
            // that restriction be lifted.
            let mut container = default_container(true);
            let result = with_container(&mut container, || {
                self.container_main(global, self.autopilot, &hermit, id)
            });
            let _ = gdb_client.wait();
            result
        }
    }

    fn container_main(
        &self,
        global: &GlobalOpts,
        autopilot: bool,
        hermit: &HermitData,
        id: Id,
    ) -> Result<ExitStatus, Error> {
        let _guard = global.init_tracing();

        if autopilot {
            hermit.replay(id)
        } else {
            hermit.replay_with_gdbserver(id, self.gdbserver_port)
        }
    }
}
