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
use detcore_model::config::MountInfoRootRewrite;
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
        self == other
    }
}

/// hermit record/replay version.
// NB: Increase the version number when there are breaking changes, i.e.:
// when new syscalls or event schemas are added.
//
// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#2373)
// 0x10b -> 0x10c: flock(2) stopped being a Detcore no-op and now reaches
// `record_or_replay`, so a post-fix run emits a `Return` event per flock call.
// Replay re-issues calls for materialized descriptors and fails closed when it
// cannot reproduce the lock side effect. A 0x10b recording contains NO flock
// event at all -- the old handler returned Ok(0) before ever reaching the
// recorder, even though flock was already classified Determinized -- so
// replaying one under this build would read the next thread event for every
// flock and desynchronize the stream. The version gate must refuse it.
//
// TODO-HUMAN-REVIEW(#2370)
// 0x10d -> 0x10e: record/replay now carries successful exec image/path state and
// an explicit ppoll timeout-pointer shape. Older readers cannot interpret those
// events without desynchronizing the stream, so the format advances once.
//
// TODO-HUMAN-REVIEW(#2272)
// 0x10c -> 0x10d: `Ppoll` becomes its OWN event rather than sharing `Poll`'s,
// because ppoll additionally copies out a timeout and must preserve it on
// EINTR and on a partial EFAULT copy-out; and `Poll` itself now records the
// partial `revents` copy-out an error return leaves behind. The recorded
// stream therefore carries a different event shape for both syscalls.
//
// ONE increment covers BOTH halves deliberately. This branch bumped twice --
// once per format change -- but a reader either understands the new stream or
// it does not, so what matters is that the version differs from every stream
// a different shape was written under. Landing two increments would imply a
// 0x10d recording exists that this build can read and it cannot: no build
// ever wrote one.
//
// ⚠️ THIS BUMP IS WHY THE REBASE COULD NOT SIMPLY TAKE EITHER SIDE. This
// change was authored against 0x10a and bumped to 0x10b; main has since gone
// to 0x10c for the unrelated flock work. Keeping the branch's 0x10b would
// move the version BACKWARDS and let a 0x10c flock recording be replayed by a
// build whose ppoll events have a different shape. Keeping main's 0x10c
// unchanged would be worse: the format would change with no bump at all, so a
// recording made here would claim 0x10c while containing a `Ppoll` event the
// 0x10c reader does not know -- exactly the desynchronization the paragraph
// above exists to prevent. The version must go FORWARD once more.
//
// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#2407)
// 0x10e -> 0x10f: pidfd_getfd(2) is now explicitly subscribed and recorded.
// Earlier builds let the syscall bypass the recorder, so a successful call
// changed the guest descriptor table without a corresponding stream event.
// The replayer now consumes one exact pidfd_getfd event and reissues the
// syscall to reproduce the kernel side effect; accepting a 0x10e stream would
// therefore consume the next event at every pidfd_getfd call.
//
// 0x10f -> 0x110: metadata now carries the recording namespace's proven
// Hermit-owned mount IDs. Detcore applies that map after both the recorder's
// raw read and the replayer's ReadV2 copyout. An older recording has no map and
// would replay different guest-visible mountinfo bytes under this sanitizer,
// so accepting it would violate replay fidelity even though the event enum did
// not change.
//
// 0x110 -> 0x111: metadata now also carries the recording namespace's full
// raw mount-ID order. That makes fdinfo's mnt_id use the same canonical identity
// as mountinfo during replay instead of consulting the unrelated replay
// namespace or collapsing all descriptors to one mount.
// 0x111 -> 0x113: fdinfo mount identities are now keyed by each observed raw
// mount ID rather than broad descriptor classes. Metadata carries only the
// completed namespace's mountinfo row/parent order; pseudo-filesystem IDs that
// are absent from mountinfo are allocated afterward in deterministic guest
// observation order. Replay must rebuild that mapping from recording-time raw
// bytes, or pidfs/nsfs/anon_inodefs can be collapsed or rejected. The draft
// 0x112 format was never shipped.
//
// 0x113 -> 0x114: recordings persist the completed guest's mountinfo order and
// unlisted fdinfo mount-ID order, and event streams use stable recording-local
// thread IDs rather than host PID allocation. An older reader cannot rebuild
// either mapping reliably for a plain public record API recording.
//
// 0x114 -> 0x115: event stream filenames now encode deterministic process-tree
// pedigrees instead of process-local allocation order. Older readers look for
// integer filenames and cannot replay a recording containing child streams.
//
// 0x115 -> 0x116: event stream filenames are fixed-size SHA-256 names and each
// data/debug stream begins with its complete process-tree identity. Older
// readers cannot skip or validate these headers.
pub(crate) const RECORD_VERSION: RecordVersion = RecordVersion(0x116);

/// The highest RECORD_VERSION this project has ever shipped.
///
/// RECORD_VERSION must never move BACKWARD onto it. This is a COMPILE-TIME
/// assertion rather than a test on purpose: it is decidable from two constants,
/// so it should break the build rather than a test run, and it cannot be
/// skipped, filtered or left unrun.
///
/// WHY IT EXISTS AT ALL. The rejection set in `record_version_requires_an_exact_match`
/// used to be a list of specific versions, and `!compatible_with(0x10a)` was
/// catching a backward move BY ACCIDENT. Replacing that list with a window derived
/// from RECORD_VERSION fixes its silent narrowing but cannot catch a regression,
/// because a derived window re-derives from whatever the constant currently says.
/// Dropping the list without this floor would have traded a stale check for a
/// weaker one.
///
/// The failure it guards is near, not hypothetical: a long-stale branch that bumped
/// 0x109 -> 0x10a while main advanced to 0x10e regresses the constant the moment its
/// conflict is resolved by taking the branch side. The build would then stamp the
/// current schema with a label an older reader already claims, and the exact-match
/// gate would accept a stream whose shape it does not know -- the desynchronization
/// the version exists to prevent.
///
/// RAISE THIS IN THE SAME COMMIT THAT RAISES RECORD_VERSION.
const HIGHEST_SHIPPED_RECORD_VERSION: u32 = 0x116;

const _: () = assert!(
    RECORD_VERSION.0 >= HIGHEST_SHIPPED_RECORD_VERSION,
    "RECORD_VERSION is BELOW HIGHEST_SHIPPED_RECORD_VERSION. A recording made by this \
     build would carry a label an older reader already claims. If this fired during a \
     rebase, the version constant was resolved BACKWARD -- merge it forward."
);

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
    /// Recording-namespace mount roots proven to be Hermit-owned.
    ///
    /// Replay consumes the recorder's raw syscall buffers, so it must apply the
    /// recording-time raw mount IDs rather than IDs from the fresh replay
    /// namespace.  Older recordings default to no private-root rewrites.
    #[serde(default)]
    pub mountinfo_root_rewrites: Vec<MountInfoRootRewrite>,
    /// Recording-namespace raw mount IDs in canonical row order.
    #[serde(default)]
    pub mountinfo_mount_ids: Vec<u64>,
    /// Whether `mountinfo_mount_ids` is a producer-owned snapshot, including a
    /// valid empty snapshot.
    #[serde(default)]
    pub mountinfo_mount_ids_captured: bool,
    /// Recording-time raw fdinfo mount IDs absent from mountinfo, in the order
    /// Detcore first observed them.
    #[serde(default)]
    pub fdinfo_unlisted_mount_ids: Vec<u64>,
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
            mountinfo_root_rewrites: Vec::new(),
            mountinfo_mount_ids: Vec::new(),
            mountinfo_mount_ids_captured: false,
            fdinfo_unlisted_mount_ids: Vec::new(),
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

pub fn record_or_replay_config(data: &Path) -> detcore::Config {
    // NOTE: Record and replay should use the exact same Detcore configuration.
    // Callers add the completed producer namespace's mountinfo order and
    // unlisted fdinfo mount-ID order after this common base is built, so replay
    // uses recording-time raw IDs rather than IDs from its fresh container.
    //
    // WHY THIS IS NOT `hermit run --strict`, WRITTEN HERE ON PURPOSE.
    //
    // `virtualize_time: false` below is DELIBERATE, and the rationale used to live only
    // in the separate dev-hermit workspace -- not in this repository, beside the code it
    // governs. The cost of that was measured: three agents in one night read this line,
    // searched this repo's source, git log, docs/ and issues, found nothing, and could
    // not tell a design decision from a determinism bug; two coordinators then spent
    // hours treating it as a candidate defect. A decision recorded in a different
    // repository from the code it governs is, in practice, undocumented. Hence this
    // comment. See rrnewton/hermit#2295.
    //
    // WHAT RECORD/REPLAY ACTUALLY GUARANTEES. Replay re-executes a recording against the
    // recorded syscall data, so what must be reproducible is THIS recording's replay --
    // not agreement between two independent recordings. Time is therefore left real: the
    // recording captures what the guest actually observed, and replay returns those
    // recorded values. `hermit record start --verify` records once, replays that
    // recording, and compares the two; its success message says exactly that ("replay
    // matched recording") and does not claim more.
    //
    // WHAT IT DOES NOT GUARANTEE, which is the part that misleads readers. Because time
    // is not virtualized, two INDEPENDENT recordings of the same program observe
    // different clock values. Demonstrated: `hermit run -- date` twice returns the
    // identical virtual epoch, while `hermit record start -- date` twice returns real
    // wall-clock times seconds apart. Replay fidelity says nothing about that, and
    // the focused `independent_mountinfo_recordings_are_canonical` test compares
    // two independent mountinfo recordings after replacing exactly field 3, the
    // major:minor device column. With `virtualize_metadata: false`, Linux may
    // legitimately allocate a different anonymous-block-device minor for the
    // private procfs mount in each container. That test still compares mount ID,
    // parent ID, root, mountpoint, options, optional fields, separator,
    // filesystem type, source, and superblock options byte-for-byte. It does not
    // establish general cross-recording determinism, and a green `--verify`
    // remains only a replay-fidelity result.
    //
    // This configuration differs from `hermit run --strict` on four of the five
    // properties that define it (see run.rs: only `sequentialize_threads` matches).
    // Any claim that recording is "strict" in that sense is wrong; the `--strict` flag
    // on `hermit record` is accepted and ignored purely for command-line compatibility.
    let default_config: detcore::Config = Default::default();
    let mut config = detcore::Config {
        // Record and replay are determinism claims, so an unsupported syscall
        // must invalidate the operation instead of being recorded from or
        // replayed against the live host.
        panic_on_unsupported_syscalls: true,
        // Return a typed error through Reverie rather than unwinding across the
        // backend callback. The tracer owns process-tree cleanup on that error.
        exit_on_unsupported_syscall: true,
        shutdown_on_unsupported_syscall: false,
        unsupported_syscall_report_fd: None,
        panic_on_rcb_overshoot: false,
        sequentialize_threads: true,
        runs_post_fork: default_config.runs_post_fork,
        // Record/replay keeps a partial Detcore subscription. Complete coverage
        // of the Determinized classification begins in v0x10a; madvise policy
        // semantics begin in v0x102.
        passthru_opt: true,
        deterministic_io: false,
        virtualize_time: crate::RECORD_REPLAY_VIRTUALIZES_TIME,
        virtualize_metadata: false,
        mountinfo_root_rewrites: Vec::new(),
        mountinfo_device_rewrites: Vec::new(),
        mountinfo_mount_ids: Vec::new(),
        mountinfo_mount_ids_captured: false,
        fdinfo_unlisted_mount_ids: Vec::new(),
        virtualize_cpuid: true,
        cpuid_virtualized_by_backend: false,
        backend_supports_madvise: true,
        discover_live_file_metadata: false,
        use_thread_local_clock_reads: false,
        detect_host_clock_futex_timeouts: false,
        syscall_clobbers_virtualized_by_backend: false,
        cancel_killed_thread_rpcs: false,
        backend_reports_physical_process_exits: false,
        backend_serializes_fork_children: false,
        backend_dispatches_thread_tools: true,
        backend_tracks_process_children: true,
        backend_runs_exit_robust_list: true,
        backend_requires_thread_directed_process_signals: false,
        backend_is_kvm: false,
        kvm_shared_dequeue_timers: false,
        backend_supports_parked_write_signal_interruption: true,
        backend_virtualizes_capability_prctls: false,
        backend_defers_vfork_child_registration: false,
        has_uts_namespace: true,
        // The path to the directory where syscalls will be recorded.
        replay_data: Some(data.to_path_buf()),
        clock_multiplier: None,
        epoch: default_config.epoch,
        gdbserver: false,
        gdbserver_port: default_config.gdbserver_port,
        kill_daemons: default_config.kill_daemons,
        max_timeslice: default_config.max_timeslice,
        target_timeslice: default_config.target_timeslice,
        seed: default_config.seed,
        rng_seed: default_config.rng_seed,
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
        detlog_regs: false,
        detlog_io_buffers: crate::RECORD_REPLAY_HASHES_IO_BUFFERS,
        detlog_regs_cadence: 1,
        sysinfo_uptime_offset: 120,
        memory: default_config.memory,
        interrupt_at: vec![],
        happens_before: None,
        fuzz_futexes: false,
        chaos_target_races: false,
        chaos_per_thread_slowdown: false,
        chaos_slowdown_max_factor: 10.0,
        chaos_epoch_length_ns: 0,
        fuzz_seed: None,
    };
    if config.max_timeslice.is_some() && !reverie_ptrace::is_perf_supported() {
        tracing::warn!(
            "Hardware perf counters are not supported on this machine. Records/Replays may randomly fail!"
        );
        config.max_timeslice = None;
    }
    config
}

#[cfg(test)]
mod tests {
    use reverie::Tool;
    use reverie::syscalls::Sysno;

    use super::*;

    #[test]
    fn record_version_requires_an_exact_match() {
        assert!(RECORD_VERSION.compatible_with(&RECORD_VERSION));

        // DERIVED FROM THE COMPATIBILITY RULE, NOT ENUMERATED.
        //
        // This assertion used to be a list of specific versions -- 0x10a, 0x10c,
        // 0x105, 0x110. A list SILENTLY NARROWS every time RECORD_VERSION
        // advances: the named values drift away from the boundary that matters,
        // and the test keeps passing while checking less and less. It cannot
        // fail for the reason its name gives once the value has moved past the
        // range someone happened to write down.
        //
        // `compatible_with` is exact equality, so the property to assert is
        // "every OTHER version is refused". Re-deriving the cases from
        // RECORD_VERSION itself means the window travels with the constant and
        // cannot go stale.
        let current = RECORD_VERSION.0;
        for delta in 1..=16u32 {
            let older = RecordVersion(current - delta);
            assert!(
                !RECORD_VERSION.compatible_with(&older),
                "recorder at {current:#x} must refuse a recording made at {:#x}",
                older.0
            );
            let newer = RecordVersion(current + delta);
            assert!(
                !RECORD_VERSION.compatible_with(&newer),
                "recorder at {current:#x} must refuse a recording made at {:#x}",
                newer.0
            );
        }
    }

    #[test]
    fn record_and_replay_preserve_partial_subscriptions_and_fail_closed() {
        let config = record_or_replay_config(Path::new("replay-data"));
        assert!(config.passthru_opt);
        assert!(config.panic_on_unsupported_syscalls);
        assert!(config.exit_on_unsupported_syscall);
        assert!(!config.shutdown_on_unsupported_syscall);
    }

    #[test]
    fn record_and_replay_use_run_default_memory() {
        let run_default = detcore::Config::default();
        assert_eq!(run_default.memory, 1_000_000_000);
        assert_eq!(
            record_or_replay_config(Path::new("replay-data")).memory,
            run_default.memory
        );
    }

    /// RECORDING DOES NOT VIRTUALIZE TIME, AND THE VERDICT NOW SAYS SO.
    ///
    /// `virtualize_time: false` here is deliberate (see the rationale block on
    /// `record_or_replay_config`), and it is what makes a green
    /// `record start --verify` mean something weaker than a green
    /// `run --verify`: the replay reproduced *that recording*, not that the guest
    /// is deterministic across invocations. Ported from the residual of
    /// hermit#2269.
    ///
    /// Pinned against the shared constant rather than a literal, because the
    /// value is now read in two places — the config the run uses, and the
    /// `ComparisonOptions` that discloses it in the report. If those drifted, the
    /// report would describe a time policy the run did not use, which is the very
    /// defect the disclosure exists to close.
    #[test]
    fn recording_does_not_virtualize_time_as_documented() {
        let config = record_or_replay_config(Path::new("replay-data"));
        assert!(
            !config.virtualize_time,
            "record/replay must not virtualize time; a green replay verdict would \
             otherwise be mistaken for a determinism result"
        );
        assert_eq!(
            config.virtualize_time,
            crate::RECORD_REPLAY_VIRTUALIZES_TIME,
            "the config the run uses and the constant the report discloses must be \
             the same decision, not two that happen to agree"
        );
    }

    #[test]
    fn record_and_replay_subscribe_every_determinized_syscall() {
        let config = record_or_replay_config(Path::new("replay-data"));
        let record = <detcore::Detcore<crate::recorder::Recorder> as Tool>::subscriptions(&config);
        let replay = <detcore::Detcore<crate::replayer::Replayer> as Tool>::subscriptions(&config);

        for (phase, subscriptions) in [("record", record), ("replay", replay)] {
            let delivered = subscriptions.iter_syscalls().collect::<Vec<_>>();
            let missing = detcore::all_pinned_syscalls()
                .filter(|sysno| detcore::is_determinized_syscall(*sysno))
                .filter(|sysno| !delivered.contains(sysno))
                .collect::<Vec<_>>();
            assert!(
                missing.is_empty(),
                "{phase} lets Determinized syscalls bypass Detcore: {}",
                missing
                    .iter()
                    .map(|sysno| sysno.to_string())
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            assert!(
                delivered.contains(&Sysno::syslog),
                "{phase} must deliver syslog to its deterministic Detcore handler"
            );
            assert!(
                !delivered.contains(&Sysno::chdir),
                "{phase} must leave unlisted PassThrough chdir unsubscribed"
            );
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#2373)
    /// A 0x10b recording predates flock forwarding, so it carries no flock event
    /// while this replayer expects one per call. Replaying it would consume some
    /// other event and desynchronize; the version gate must refuse it instead.
    #[test]
    fn record_version_rejects_pre_flock_streams() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x10b)));
    }

    #[test]
    fn record_version_rejects_previous_memory_configuration() {
        // Metadata does not persist the memory configuration, so a recording made with the
        // previous hardcoded value cannot be replayed compatibly with the corrected default.
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x10a)));
    }

    #[test]
    fn record_version_rejects_pre_complete_determinized_subscription_streams() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x109)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x104)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x102)));
    }
}
