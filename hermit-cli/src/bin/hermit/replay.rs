/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use clap::Parser;
use hermit::Context;
use hermit::Error;
use hermit::HermitData;
use hermit::Id;
use hermit::Shebang;
use reverie::process::ExitStatus;

use super::container::deterministic_container;
use super::gdb_client::CLIENT_EXITED_BEFORE_CONNECTING;
use super::gdb_client::GdbClientWatch;
use super::global_opts::GlobalOpts;
use super::run::NetworkingMode;

/// Command-line options for the "replay" subcommand.
#[derive(Debug, Parser, Clone)]
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

    /// Serve the replay through the gdb remote protocol without launching gdb.
    #[clap(long, conflicts_with = "autopilot")]
    serve_only: bool,

    /// Additional gdb command passed by `-ex`
    #[clap(long, value_delimiter = ';', conflicts_with = "serve_only")]
    gdbex: Vec<String>,

    /// Serve the replay to one Debug Adapter Protocol client, such as an
    /// editor, on the TCP address [HOST:]PORT (HOST defaults to 127.0.0.1).
    /// This runs `hermit-dap --replay` from beside this executable, which
    /// supports stepBack and reverseContinue. See DEBUGGING.md.
    #[clap(
        long,
        value_name = "[HOST:]PORT",
        conflicts_with_all = ["autopilot", "serve_only", "gdbex"]
    )]
    dap_listen: Option<String>,
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

        // A run that drew an input from the host leaves only a manifest,
        // which `hermit run --from` reruns; this command replays syscall
        // recordings (https://github.com/rrnewton/hermit/issues/4271).
        let directory = hermit.data_dir().join(id.to_string());
        if !directory.join(hermit::RECORDING_METADATA_NAME).exists()
            && directory.join(crate::manifest::MANIFEST_NAME).exists()
        {
            anyhow::bail!(
                "recording {id} is a run manifest, not a syscall recording; rerun it with: \
                 hermit run --from {}",
                directory.display()
            );
        }
        if let Some(address) = &self.dap_listen {
            return Err(self.exec_dap_adapter(&hermit, id, address));
        }

        let recorded_local = hermit.recording_metadata(id)?.local_networking == Some(true);
        let network = replay_network(self.autopilot, recorded_local);
        if recorded_local && network == NetworkingMode::Host {
            // Same override, and same warning, as `hermit run --gdbserver`.
            eprintln!(
                "hermit replay: WARNING: recording {id} used --network local, but a \
                 gdb-served replay runs on the host network; syscalls replayed by \
                 re-execution may see different interfaces and ports"
            );
        }
        if self.autopilot || self.serve_only {
            let (mut container, identity) = deterministic_container()?;
            network.configure(&mut container, false);
            let options = self.clone();
            let global = global.clone();
            let resources = format!("replay {} and identity mounts", hermit.data_dir().display());
            super::owned_container::run(
                &mut container,
                (identity, hermit),
                resources,
                true,
                "with_container",
                None,
                move |(_, hermit)| options.container_main(&global, options.autopilot, hermit, id),
            )
            .map(|(value, _guards)| value)
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

            // TODO: For replay, we ought to construct the container from
            // `metadata.json`. That logic belongs in `hermit::replay`, but we have
            // to initialize logging inside the container because it may spawn a
            // thread. If we can guarantee that tracing won't spawn a thread, then
            // that restriction be lifted.
            // Same unbounded accept as the record path: the client is spawned
            // before the container that binds the port, so a client that dies
            // early leaves the gdbserver waiting for a peer that cannot arrive.
            // ⚠️ CORRECTION TO WHAT LANDED HERE. This comment used to claim this
            // file's `wait()` "WAS reached on every path ... so it never leaked a
            // zombie", and that only the record path leaked. That is wrong:
            // `deterministic_container()?` on the next line precedes the wait, so
            // an error building the container skipped it here too. BOTH files
            // leaked, and the merged description of hermit#2654 says otherwise.
            // Recorded rather than quietly deleted, because the claim is in a
            // landed commit message where it cannot be edited.
            let gdb_watch = GdbClientWatch::spawn(gdb_command, self.gdbserver_port)?;
            let (mut container, identity) = deterministic_container()?;
            let guards = Rc::new(RefCell::new((identity, hermit, gdb_watch)));
            let resources = format!(
                "replay {}, identity mounts and GDB watcher",
                guards.borrow().1.data_dir().display()
            );
            let options = self.clone();
            let global = global.clone();
            let result = super::owned_container::run(
                &mut container,
                Rc::clone(&guards),
                resources,
                true,
                "with_container",
                None,
                move |guards| {
                    options.container_main(&global, options.autopilot, &guards.borrow().1, id)
                },
            );
            // On unresolved cleanup the factory retains the SAME guard scope.
            // Do not signal watcher completion while its container still owns it.
            if result.as_ref().is_err_and(|e| {
                e.downcast_ref::<super::owned_container::ParentCleanupUnconfirmed>()
                    .is_some()
            }) {
                return result.map(|(status, _)| status);
            }
            let finished = guards.borrow_mut().2.finish();
            match (result, finished) {
                (Ok((status, _)), Ok(_)) => Ok(status),
                (Ok(_), Err(watcher)) => Err(watcher),
                (Err(primary), Err(watcher)) => {
                    Err(primary.context(format!("GDB watcher also failed: {watcher:#}")))
                }
                (Err(primary), Ok(true)) => Err(primary.context(CLIENT_EXITED_BEFORE_CONNECTING)),
                (Err(primary), Ok(false)) => Err(primary),
            }
        }
    }

    /// Replaces this process with `hermit-dap --listen ADDRESS --replay ID`,
    /// after telling the user what a DAP client's attach request must name.
    /// Returns only the error that kept it from doing so.
    fn exec_dap_adapter(&self, hermit: &HermitData, id: Id, address: &str) -> Error {
        use std::os::unix::process::CommandExt;

        let adapter = match std::env::current_exe() {
            Ok(exe) => exe.with_file_name("hermit-dap"),
            Err(error) => return Error::from(error).context("Failed to locate hermit"),
        };
        if !adapter.is_file() {
            return Error::msg(format!(
                "--dap-listen needs hermit-dap beside hermit, and {} does not exist; \
                 build it with `cargo build -p hermit --bin hermit --bin hermit-dap`",
                adapter.display()
            ));
        }
        let exe = hermit.data_dir().join(format!("{}/exe", id));
        let program = Shebang::new(&exe).map_or(exe, |s| s.interpreter().into());
        eprintln!(
            "hermit replay: starting hermit-dap to serve recording {id} to one DAP client; \
             its attach request needs \"program\": {:?} and \"target\": \"127.0.0.1:{}\"",
            program.display().to_string(),
            self.gdbserver_port
        );
        let error = std::process::Command::new(&adapter)
            .arg("--listen")
            .arg(address)
            .arg("--replay")
            .arg(id.to_string())
            .arg("--data-dir")
            .arg(hermit.data_dir())
            .arg("--gdbserver-port")
            .arg(self.gdbserver_port.to_string())
            .exec();
        Error::from(error).context(format!("Failed to execute {}", adapter.display()))
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

/// The network a replay runs in. Syscalls the replayer does not serve from
/// the recording are executed for real, so an autopilot replay uses the
/// network the recording used; recordings that predate the stored choice ran
/// on the host network. A replay served to gdb uses the host network so the
/// gdb client can reach its gdbserver port, as `hermit run --gdbserver` does.
fn replay_network(autopilot: bool, recorded_local: bool) -> NetworkingMode {
    if autopilot && recorded_local {
        NetworkingMode::Local
    } else {
        NetworkingMode::Host
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_autopilot_replay_of_a_local_recording_is_local() {
        assert_eq!(replay_network(true, true), NetworkingMode::Local);
        assert_eq!(replay_network(true, false), NetworkingMode::Host);
        assert_eq!(replay_network(false, true), NetworkingMode::Host);
        assert_eq!(replay_network(false, false), NetworkingMode::Host);
    }
}
