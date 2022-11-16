/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap;
use std::env;
use std::path::Path;
use std::path::PathBuf;

use detcore::BlockingMode;
use reverie::process::Command;
use serde::Deserialize;
use serde::Serialize;

use crate::error::Error;

/// Hermit record version. Recorded as part of hermit-record, hermit-replay
/// will check this version and will fail if hermit-record version is newer.
#[derive(
    Debug,
    Clone,
    Copy,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Deserialize,
    Serialize
)]
#[repr(transparent)]
pub struct RecordVersion(u32);
impl RecordVersion {
    /// Check if the recorder/replayer version is compatible with a given
    /// recording (trace).
    pub fn compatible_with(&self, other: &RecordVersion) -> bool {
        self.0 >= other.0
    }
}

/// hermit record/replay version.
// NB: Increase the version number when there's any breaking changes, i.e.:
// when new syscalls are added.
pub(crate) const RECORD_VERSION: RecordVersion = RecordVersion(0x100);

/// Metadata associated with the recording. This is serialized as a JSON file.
#[derive(Debug, Serialize, Deserialize)]
pub struct Metadata {
    /// The real path to the program.
    pub exe: PathBuf,
    /// The name of the program.
    pub program: String,
    /// The first argument passed to the program.
    pub arg0: String,
    /// Program arguments (not including arg0).
    pub args: Vec<String>,
    /// The working directory of the program.
    pub current_dir: PathBuf,
    /// The hostname in the UTS namespace used by the program.
    pub hostname: Option<String>,
    /// The domainname in the UTS namespace used by the program.
    pub domainname: Option<String>,
    /// The environment variables used by the program.
    pub envs: BTreeMap<String, String>,
    /// Hermit record/replay version.
    pub version: RecordVersion,
}

impl Metadata {
    /// Creates a new metadata object, populating it with information about a
    /// command.
    pub fn new(command: &Command) -> Result<Self, Error> {
        let exe = command.find_program()?;

        let program = command.get_program().to_string_lossy().into_owned();
        let arg0 = command.get_arg0().to_string_lossy().into_owned();

        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();

        let envs = command
            .get_captured_envs()
            .into_iter()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect();

        let current_dir = command
            .get_current_dir()
            .map_or_else(|| env::current_dir().unwrap(), ToOwned::to_owned);

        let hostname = command
            .get_hostname()
            .map(|s| s.to_string_lossy().into_owned());
        let domainname = command
            .get_domainname()
            .map(|s| s.to_string_lossy().into_owned());

        Ok(Self {
            exe,
            program,
            arg0,
            args,
            current_dir,
            hostname,
            domainname,
            envs,
            version: RECORD_VERSION,
        })
    }

    /// Constructs a command from the metadata.
    pub fn command(&self) -> Command {
        // NOTE: We bypass the normal $PATH search here by passing in the
        // absolute path to the program directly.
        let mut command = Command::new(&self.exe);
        command.arg0(&self.arg0);
        command.args(&self.args);
        command.env_clear();
        command.envs(&self.envs);
        command.current_dir(&self.current_dir);

        if let Some(hostname) = &self.hostname {
            command.hostname(hostname);
        }

        if let Some(domainname) = &self.domainname {
            command.domainname(domainname);
        }

        command
    }
}

// TODO: Record this in the metadata instead of hardcoding this.
pub fn record_or_replay_config(data: &Path) -> detcore::Config {
    // NOTE: Record and replay should use the exact same detcore
    // configuration. Otherwise, the behavior of the program could diverge
    // during replay.
    let default_config: detcore::Config = Default::default();
    let mut config = detcore::Config {
        panic_on_unsupported_syscalls: false,
        sequentialize_threads: true,
        deterministic_io: false,
        virtualize_time: false,
        virtualize_metadata: false,
        virtualize_cpuid: true,
        has_uts_namespace: true,
        // The path to the directory where syscalls will be recorded.
        replay_data: Some(data.to_path_buf()),
        clock_multiplier: None,
        epoch: default_config.epoch,
        gdbserver: false,
        gdbserver_port: default_config.gdbserver_port,
        kill_daemons: default_config.kill_daemons,
        preemption_timeout: default_config.preemption_timeout,
        seed: default_config.seed,
        imprecise_timers: false,
        chaos: false,
        sigint_instakill: false,
        warn_non_zero_binds: false,
        sched_heuristic: Default::default(),
        sched_seed: default_config.sched_seed,
        recordreplay_modes: true,
        record_preemptions: false,
        record_preemptions_to: None,
        replay_preemptions_from: None,
        replay_schedule_from: None,
        replay_exhausted_panic: false,
        die_on_desync: true,
        stacktrace_event: Vec::new(),
        stacktrace_signal: None,
        preemption_stacktrace: false,
        preemption_stacktrace_log_file: None,
        stop_after_turn: None,
        stop_after_iter: None,
        debug_externalize_sockets: false,
        debug_futex_mode: BlockingMode::Precise,
        sched_sticky_random_param: 0.0,
        no_rcb_time: false,
        detlog_heap: false,
        detlog_stack: false,
        sysinfo_uptime_offset: 120,
        memory: 1024 * 1024 * 1024,
        interrupt_at: vec![],
    };
    if config.preemption_timeout.is_some() && !reverie_ptrace::is_perf_supported() {
        tracing::warn!(
            "Hardware perf counters are not supported on this machine. Records/Replays may randomly fail!"
        );
        config.preemption_timeout = None;
    }
    config
}
