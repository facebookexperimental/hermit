/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::cell::RefCell;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;

use clap::Parser;
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
    /// The recording to replay, by default `last`: the newest one `hermit
    /// record` or `hermit run --record-networking` made, as a bare `rr
    /// replay` replays the newest rr recording. It may be `last`, `last-run`
    /// (the newest of any kind, including the manifest a plain `hermit run`
    /// leaves when it draws an input from the host), a recording ID or a unique prefix
    /// of at least 6 hex digits, or the path of a manifest.json or of its
    /// directory; `hermit list` lists them. A syscall recording (`hermit
    /// record`) replays under gdb, or to the end with `--autopilot`. Any
    /// other recording replays by rerunning its program from its manifest,
    /// as `hermit run --from RECORDING` does, to the end: a network
    /// recording offline from its trace, a run that drew an input from the
    /// host with the values it drew. To serve such a rerun to gdb, run
    /// `hermit run --from RECORDING --gdbserver`.
    #[clap(value_name = "RECORDING")]
    recording: Option<OsString>,

    /// The global options this invocation gave, which a rerun passes on.
    #[clap(skip)]
    global_argv: Vec<OsString>,

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
        use crate::manifest::Kind;
        use crate::manifest::Target;

        if self.dap_listen.is_some() {
            // Before resolving the recording, so a missing adapter is named
            // first, whatever the recording turns out to be.
            super::dap_adapter::hermit_dap_beside_hermit()?;
        }
        let store = HermitData::from(self.data_dir.as_ref());
        let spec = self
            .recording
            .clone()
            .unwrap_or_else(|| hermit::LAST.into());
        // Every kind of recording has one name and one verb
        // (https://github.com/rrnewton/hermit/issues/4271): a syscall
        // recording replays here; any other reruns from its manifest.
        let (hermit, id) = match crate::manifest::resolve(store.data_dir(), &spec)? {
            Target::Recording { data_dir, id } => (HermitData::with_dir(data_dir), id),
            Target::Manifest(path) => {
                let manifest = crate::manifest::read(&path)?;
                if manifest.kind != Kind::FullSyscall {
                    return Err(self.rerun(global, &path, &manifest, store.data_dir()));
                }
                // The directory named, as the replayer finds it: its parent
                // and its name.
                let directory = std::path::absolute(crate::manifest::directory_of(&path))?;
                let (data_dir, id) =
                    crate::manifest::recording_location(&directory, Some(&manifest.id))?;
                (HermitData::with_dir(data_dir), id)
            }
        };
        if let Some(address) = &self.dap_listen {
            return Err(self.exec_dap_adapter(&hermit, id, address));
        }

        let metadata = hermit.recording_metadata(id)?;
        hermit::cpu_profile_admission::check_record_or_replay(metadata.cpu_profile).map_err(
            |refusal| Error::new(super::container::PolicyRefusal).context(refusal.to_string()),
        )?;
        if metadata.cpu_profile == detcore_model::cpu_profile::CpuProfile::X86_64V3 {
            // A v3 recording replays only on a qualifying host; qualify before
            // the container, as `record start` does. The replayer compares the
            // host facts (https://github.com/rrnewton/hermit/issues/4243).
            let policy = |refusal: hermit::cpu_profile_admission::CpuProfileRefusal| {
                Error::new(super::container::PolicyRefusal).context(refusal.to_string())
            };
            hermit::cpu_profile_admission::precheck_replay(
                metadata.cpu_profile,
                metadata.cpu_profile_host.as_ref(),
                metadata.cpu_table_digest.as_deref(),
            )
            .map_err(policy)?;
            let gdb_served = !self.autopilot;
            let request = hermit::cpu_profile_admission::record_or_replay_request(gdb_served);
            hermit::cpu_profile_admission::admit(&request).map_err(|refusal| {
                Error::new(super::container::PolicyRefusal)
                    .context(format!("--cpu-profile x86-64-v3 refused: {refusal}"))
            })?;
        }
        let recorded_local = metadata.local_networking == Some(true);
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

    /// Sets the global options this invocation gave, which a rerun passes on.
    pub(crate) fn set_global_argv(&mut self, global_argv: Vec<OsString>) {
        self.global_argv = global_argv;
    }

    /// Replaces this process with `hermit [GLOBAL] run --from MANIFEST`, the
    /// replay of a recording that is not a syscall recording, run in the
    /// data directory it came from. Returns only the error that kept it
    /// from doing so.
    fn rerun(
        &self,
        global: &GlobalOpts,
        path: &Path,
        manifest: &crate::manifest::Manifest,
        data_dir: &Path,
    ) -> Error {
        use std::os::unix::process::CommandExt;

        let id = &manifest.id;
        let kind = manifest.kind.name();
        if let Some(address) = &self.dap_listen {
            return anyhow::anyhow!(
                "recording {id} is of kind {kind}, which replays by rerunning its program \
                 rather than under gdb, so it has no reverse execution; serve the rerun to a \
                 DAP client with: hermit run --from {} --dap-listen={address}",
                path.display()
            );
        }
        if self.serve_only || !self.gdbex.is_empty() {
            return anyhow::anyhow!(
                "recording {id} is of kind {kind}, which replays by rerunning its program \
                 rather than under gdb; serve the rerun to gdb with: hermit run --from {} \
                 --gdbserver",
                path.display()
            );
        }
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(error) => return Error::from(error).context("Failed to locate hermit"),
        };
        // A WARN: the program runs again from its saved inputs, which is not a
        // replay of its system calls. On stderr even under `--log-file`: the
        // rerun opens that log afresh and would erase it.
        super::tracing::emit_before_tracing(
            global.log,
            super::tracing::EarlyLog::Stderr(global.log_budget()),
            || {
                ::tracing::warn!(
                    target: super::global_opts::CONTROLLER_TARGET,
                    "recording {id} ({kind}) replays by rerunning its program: hermit run --from {}",
                    super::run::home_relative(path)
                )
            },
        );
        let error = std::process::Command::new(&exe)
            .args(&self.global_argv)
            .arg("run")
            .arg("--from")
            .arg(path)
            .env("HERMIT_DATA_DIR", data_dir)
            .exec();
        Error::from(error).context(format!("Failed to execute {}", exe.display()))
    }

    /// Replaces this process with `hermit-dap --listen ADDRESS --replay ID`,
    /// after telling the user what a DAP client's attach request must name.
    /// Returns only the error that kept it from doing so.
    fn exec_dap_adapter(&self, hermit: &HermitData, id: Id, address: &str) -> Error {
        use std::os::unix::process::CommandExt;

        let adapter = match super::dap_adapter::hermit_dap_beside_hermit() {
            Ok(adapter) => adapter,
            Err(error) => return error,
        };
        let exe = hermit.data_dir().join(format!("{}/exe", id));
        let program = Shebang::new(&exe).map_or(exe, |s| s.interpreter().into());
        eprintln!(
            "hermit replay: starting hermit-dap to serve recording {id} to one DAP client; {}",
            super::dap_adapter::attach_hint(&program, self.gdbserver_port)
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
