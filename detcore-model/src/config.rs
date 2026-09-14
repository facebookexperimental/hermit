/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Detcore configuration and widely used types.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fmt;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::str::FromStr;

use chrono::DateTime;
use chrono::Utc;
use clap::Parser;
use serde::Deserialize;
use serde::Serialize;

use crate::happens_before::HappensBeforeProgram;
use crate::pid::DetTid;
use crate::schedule::SigWrapper;
use crate::time::NANOS_PER_RCB;
use crate::time::RcbTimeMultiplier;

const fn default_true() -> bool {
    true
}

/// One mount row whose kernel-private root must be replaced before it becomes
/// guest-visible.
///
/// The CLI populates this only after proving, with held file descriptors in the
/// completed mount namespace, that the mount is one Hermit created.  The raw
/// mount ID is namespace-local, so these entries are valid only for the one
/// container run whose configuration carries them.
#[derive(Debug, Serialize, Deserialize, Clone, Eq, PartialEq)]
pub struct MountInfoRootRewrite {
    /// Mount ID read from the held target descriptor's `/proc/self/fdinfo`.
    pub raw_mount_id: u64,
    /// Stable guest-visible replacement for the row's root field.
    pub deterministic_root: Vec<u8>,
    /// Exact encoded kernel root prefix used for descendant mount rows.
    ///
    /// This is present only for a proven private `/tmp`. Mounts installed below
    /// that directory before it is bound over guest `/tmp` otherwise expose the
    /// randomly named backing directory in mountinfo field 5.
    #[serde(default)]
    pub raw_root_prefix: Option<Vec<u8>>,
    /// Guest-visible prefix replacing `raw_root_prefix`.
    #[serde(default)]
    pub deterministic_root_prefix: Option<Vec<u8>>,
    /// Exact encoded host path prefix used for descendant mountpoints.
    #[serde(default)]
    pub raw_mountpoint_prefix: Option<Vec<u8>>,
    /// Guest-visible prefix replacing `raw_mountpoint_prefix`.
    #[serde(default)]
    pub deterministic_mountpoint_prefix: Option<Vec<u8>>,
}

/// Configuration options for detcore.
#[derive(Debug, Serialize, Deserialize, Clone, Parser)]
pub struct Config {
    /// Disable virtual/logical time. Note that virtual time is required for virtual metadata.
    #[clap(long = "no-virtualize-time", action = clap::ArgAction::SetFalse)]
    pub virtualize_time: bool,

    /// Disable virtual cpuid
    #[clap(long = "no-virtualize-cpuid", action = clap::ArgAction::SetFalse)]
    pub virtualize_cpuid: bool,

    /// The execution backend installs a deterministic CPUID policy without instruction faults.
    #[serde(default)]
    #[clap(skip)]
    pub cpuid_virtualized_by_backend: bool,

    /// The execution backend implements guest-visible madvise semantics.
    #[serde(default = "default_true")]
    #[clap(skip = true)]
    pub backend_supports_madvise: bool,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-845): Review in-process backend descriptor discovery.
    /// The execution backend runs Detcore inside the guest and can inspect its live descriptors.
    #[serde(default)]
    #[clap(skip)]
    pub discover_live_file_metadata: bool,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-845): Review backend-local guest clock observations.
    /// Legacy serialized setting retained for record compatibility. Guest-visible wall and
    /// monotonic clocks always use the coordinator's virtual-time domain.
    #[serde(default)]
    #[clap(skip)]
    pub use_thread_local_clock_reads: bool,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-845): Review host-clock futex deadline detection.
    /// Direct guest clock reads may bypass backend virtualization, so absolute futex deadlines
    /// must be classified against both the host and logical clocks.
    #[serde(default)]
    #[clap(skip)]
    pub detect_host_clock_futex_timeouts: bool,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-845): Review backend-owned syscall-clobber determinism.
    /// The execution backend already returns deterministic values for registers clobbered by a
    /// syscall instruction, so Detcore must not write the complete register set back afterward.
    #[serde(default)]
    #[clap(skip)]
    pub syscall_clobbers_virtualized_by_backend: bool,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-845): Review backend-local exit-group RPC cancellation.
    /// Logically killed guest threads need an explicit scheduler response because the backend
    /// does not rely on ptrace's kernel-driven exit-group teardown.
    #[serde(default)]
    #[clap(skip)]
    pub cancel_killed_thread_rpcs: bool,

    /// The execution backend reports final physical process exits after logical tool cleanup, so
    /// Detcore can prevent virtual timers from overtaking kernel child-exit publication.
    #[serde(default)]
    #[clap(skip)]
    pub backend_reports_physical_process_exits: bool,

    // TODO-HUMAN-REVIEW(PR-1013): Review backend child process execution ordering.
    /// The execution backend completes forked process children before returning to the parent.
    #[serde(default)]
    #[clap(skip)]
    pub backend_serializes_fork_children: bool,

    // TODO-HUMAN-REVIEW(PR-1013): Review backend thread callback coverage.
    /// The execution backend dispatches cloned thread syscalls through this tool.
    #[serde(default = "default_true")]
    #[clap(skip = true)]
    pub backend_dispatches_thread_tools: bool,

    /// The backend reports every process child through Detcore's child-registration protocol.
    /// When true, an empty scheduler selection is authoritative ECHILD rather than a reason to
    /// fall back to backend-specific wait filtering.
    #[serde(default = "default_true")]
    #[clap(skip = true)]
    pub backend_tracks_process_children: bool,

    /// The execution backend completes Linux's robust-list cleanup before its task-exit callback
    /// lets another modeled thread run. Detcore still wakes waiters parked in its precise futex
    /// model, but it leaves the owner-word transition to Linux so it remains atomic.
    #[serde(default = "default_true")]
    #[clap(skip = true)]
    pub backend_runs_exit_robust_list: bool,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1058): Review process-signal identity translation.
    /// The backend cannot execute process-directed signal syscalls using Detcore's guest PID and
    /// therefore requires Detcore to translate an unambiguous process target to a specific thread.
    #[serde(default)]
    #[clap(skip)]
    pub backend_requires_thread_directed_process_signals: bool,

    /// The backend can wake a scheduler-managed pipe write for a cross-task signal while
    /// preserving Linux signal-mask, disposition, and syscall-restart behavior.
    #[serde(default = "default_true")]
    #[clap(skip = true)]
    pub backend_supports_parked_write_signal_interruption: bool,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1125): Review backend-owned capability-control prctls.
    /// The execution backend virtualizes capability bounding-set and ambient-capability state.
    #[serde(default)]
    #[clap(skip)]
    pub backend_virtualizes_capability_prctls: bool,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1152): Review deferred vfork child registration.
    /// The execution backend does not keep a `CLONE_VFORK` parent blocked inside the injected
    /// `clone(2)` until the child registers. The ptrace backend relies on the kernel to suspend a
    /// vfork parent until the child execs or exits, so the child always registers its vfork barrier
    /// before the parent asks to continue. Out-of-process backends such as KVM service the clone by
    /// deferring the child spawn, so the child registers only *after* the parent posts its
    /// continuation. When this is set the scheduler keeps an unfulfilled vfork barrier in place at
    /// parent continuation (waiting for the late child) instead of treating it as a failed clone.
    #[serde(default)]
    #[clap(skip)]
    pub backend_defers_vfork_child_registration: bool,

    /// Epoch of the logical time.
    ///
    /// This is the datetime from which all time and date modtimes begin and
    /// monotonically increase. It is in RFC3339 format such as `2026-01-01T00:00:00Z`.
    #[clap(
        long,
        env = "HERMIT_EPOCH",
        value_name = "YYYY-MM-DDThh:mm:ssZ",
        default_value = DEFAULT_EPOCH_STR
    )]
    pub epoch: DateTime<Utc>,

    /// Use this number to seed the PRNG randomness for both RNG and scheduler.
    /// This acts as a global fallback in case either `sched_seed` or `rng-seed`
    /// are not explicitly specified
    #[clap(
        long = "seed",
        env = "HERMIT_PRNG",
        default_value = "0",
        value_name = "uint64"
    )]
    pub seed: u64,

    /// Use this number to seed the PRNG that supplies randomness to the guest.
    /// This supplies guest system calls that expose randomness, as well as
    /// the `/dev/[u]random` files. It does not affect the `rdrand` instruction,
    /// which is disabled in the guest.
    #[clap(long, value_name = "uint64")]
    pub rng_seed: Option<u64>,

    /// Seeds the PRNG which drives syscall response fuzzing (i.e. chaotically exercising syscall
    /// nondeterminism).  Like other seeds, this is initialized from the `--seed` if not
    /// specifically provided.
    #[clap(long, value_name = "uint64")]
    pub fuzz_seed: Option<u64>,

    /// Logical clock multiplier. Values above one make time appear to go faster within the sandbox.
    #[clap(long, value_name = "float")]
    pub clock_multiplier: Option<f64>,

    /// Disable substitution of virtual (deterministic) file metadata in lieu
    /// of the real metadata returned by `stat`/`statx`. This also preserves raw
    /// mountinfo device numbers so those interfaces continue to agree. Raw
    /// device values are host/filesystem observations and are not promised to
    /// reproduce across machines. Virtual metadata implies `virtualize_time`.
    #[clap(long = "no-virtualize-metadata", action = clap::ArgAction::SetFalse)]
    pub virtualize_metadata: bool,

    /// Proven Hermit-owned mount roots to hide from `/proc/*/mountinfo`.
    ///
    /// This is runtime provenance, not a user option.  `serde(default)` keeps
    /// older serialized configurations compatible and makes backends which do
    /// not use the common container setup explicitly receive no rewrite claim.
    #[serde(default)]
    #[clap(skip)]
    pub mountinfo_root_rewrites: Vec<MountInfoRootRewrite>,

    /// Backend-proven pairs of (`mountinfo` raw device, `stat`/`statx` raw device).
    ///
    /// A backend may synthesize mountinfo independently from its pathname
    /// metadata implementation.  These pairs state that the two raw numbers
    /// describe the same filesystem, so Detcore can feed both surfaces through
    /// one device identity.  The pairs are runtime provenance, not a user
    /// option; an absent pair must never be inferred from numeric coincidence.
    #[serde(default)]
    #[clap(skip)]
    pub mountinfo_device_rewrites: Vec<(u64, u64)>,

    /// Recording/container namespace mount IDs in canonical row order.
    ///
    /// Detcore uses this same mapping for `/proc/*/mountinfo` and
    /// `/proc/*/fdinfo/*`. It is runtime provenance rather than a user option;
    /// replay retains recording-time raw IDs because its read events contain
    /// recording-time kernel bytes.
    #[serde(default)]
    #[clap(skip)]
    pub mountinfo_mount_ids: Vec<u64>,

    /// Whether `mountinfo_mount_ids` is an exact producer-owned snapshot.
    ///
    /// The distinction matters for an empty mountinfo file: an absent snapshot
    /// asks Detcore to observe the completed guest namespace, while a captured
    /// empty snapshot must remain empty during replay.
    #[serde(default)]
    #[clap(skip)]
    pub mountinfo_mount_ids_captured: bool,

    /// Raw fdinfo mount IDs absent from mountinfo, in first-observation order.
    ///
    /// Recording persists this producer-observed order so replay does not
    /// derive identities from its fresh namespace or launch descriptor shape.
    #[serde(default)]
    #[clap(skip)]
    pub fdinfo_unlisted_mount_ids: Vec<u64>,

    /// Sequentialize thread execution deterministically.
    #[clap(long)]
    pub sequentialize_threads: bool,

    /// Choose which side of an ordinary fork/clone runs first after the child is registered.
    /// Random choices are deterministic under `--sched-seed`.
    #[serde(default)]
    #[clap(long, default_value = "child", value_name = "child|parent|random")]
    pub runs_post_fork: RunsPostFork,

    /// Use the optimized partial syscall subscription set instead of intercepting every syscall.
    /// This permits unlisted syscalls to bypass Detcore and therefore weakens deterministic
    /// accounting; leave it disabled for fail-closed execution.
    #[serde(default)]
    #[clap(long)]
    pub passthru_opt: bool,

    /// In chaos mode, uses much cheaper approximate preemption timers.  Only makes sense
    /// when recording preemptions for later (precise) replay.
    #[clap(long)]
    pub imprecise_timers: bool,

    /// Schedule threads chaotically.
    ///
    /// The behavior of this flag is subject to change. Current behavior is to randomize thread
    /// priorities at every logical timeslice. Other randomization strategies are possible with
    /// `--sched-heuristic`.
    ///
    /// Thread scheduling remains deterministic, determined by the random seed.
    #[clap(long)]
    pub chaos: bool,

    /// Uses the `--fuzz-seed` to generate randomness and fuzz nondeterminism in the futex semantics.
    #[clap(long)]
    pub fuzz_futexes: bool,

    /// Targeted chaos: bias scheduling toward known concurrency race patterns
    /// instead of exploring interleavings uniformly. At the scheduler's existing
    /// nondeterminism points it uses `--fuzz-seed` to (a) deliver a
    /// process-directed signal to a randomly chosen thread in the group (signal
    /// timing races) and (b) randomize the requeue position of a force-unblocked
    /// thread (lock-ordering / wakeup races). Only takes effect with `--chaos`;
    /// like the rest of chaos mode it remains reproducible under a fixed seed.
    #[clap(long)]
    pub chaos_target_races: bool,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1149)
    // TODO-HUMAN-REVIEW(PR-1151)
    /// Reproducible per-thread slowdown factors for chaos mode. A factor greater
    /// than one makes each RCB consume proportionally more virtual time, while a
    /// factor below one makes it consume less. Thus scheduling deadlines and the
    /// guest-visible virtual clock describe the same slowed execution rather than
    /// applying an out-of-band scheduling bias. The factor is a pure function of
    /// scheduler seed, stable deterministic thread id, and chaos epoch. A fixed
    /// seed therefore reproduces both timing and interleavings.
    #[clap(long)]
    pub chaos_per_thread_slowdown: bool,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1149)
    // TODO-HUMAN-REVIEW(PR-1151)
    /// Maximum ratio between the slowest and fastest per-thread slowdown factor
    /// for `--chaos-per-thread-slowdown`. Each thread's factor is drawn
    /// log-uniformly from `[1/R, R]` where `R` is this value. Must fit the Q32
    /// virtual-time representation and be `>= 1.0`; `1.0` disables the spread.
    #[clap(long, default_value = "10.0", value_name = "double")]
    pub chaos_slowdown_max_factor: f64,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    /// Length of a deterministic slowdown epoch in elapsed per-thread logical
    /// nanoseconds. At the first scheduler commit at or after each boundary the
    /// factor is redrawn as `factor(seed, stable_dettid, epoch)`. This is never
    /// wall time. `0` means one epoch for the entire run, making constant slowdown
    /// the single-epoch special case. Recorded preemption artifacts carry exact
    /// epoch transitions and factors for replay. Inert without chaos slowdown.
    #[clap(long, default_value = "0", value_name = "nanos")]
    pub chaos_epoch_length_ns: u64,

    /// Record the timing of preemption events for future replay or experimentation.
    /// This is only useful in chaos modes.
    #[clap(long)]
    pub record_preemptions: bool,

    /// File to write the record of preemptions (in JSON).  Implies `--record-preemptions`.
    #[clap(long, value_name = "filepath")]
    pub record_preemptions_to: Option<PathBuf>,

    /// JSON file to read recorded preemptions from.  When `--chaos` mode is activated, these
    /// recorded preemption points take the place of randomized scheduling decisions.
    #[clap(long, value_name = "filepath", conflicts_with = "replay_schedule_from")]
    pub replay_preemptions_from: Option<PathBuf>,

    /// File to read recorded schedule trace from. This execution will replay the schedule verbatim
    /// from the file.
    #[clap(
        long,
        value_name = "filepath",
        conflicts_with = "replay_preemptions_from"
    )]
    pub replay_schedule_from: Option<PathBuf>,

    /// If we run out of events while replaying a schedule, treat that as a fatal event and panic,
    /// rather than continuing execution.
    #[clap(long)]
    pub replay_exhausted_panic: bool,

    /// When playing a schedule trace from disk, bail out on the first time we desynchronize from
    /// the event sequence specified in the trace.
    #[clap(long)]
    pub die_on_desync: bool,

    /// Given schedule events traced on recording or replaying, print the stack trace at the moment
    /// after the Nth event in the trace. Optionally, provide an output file into which the stack
    /// trace will be printed, otherwise it goes to stderr.
    #[clap(long,
           short = 's',
           value_name = "index[,path]",
           value_parser = parse_index_with_path)]
    pub stacktrace_event: Vec<(u64, Option<PathBuf>)>,

    /// Internal feature used to signal the guest with SIGINT at every `--stacktrace-event`, this is
    /// in-lieu of using hermit's internal stacktrace printing facility, to instead have an external
    /// debugger handle it.  Accepts either signal names or numbers.
    #[clap(long, value_name = "signame")]
    pub stacktrace_signal: Option<SigWrapper>,

    /// **Deprecated:** Print a stacktrace each time the program is preempted.  Only makes sense in `--chaos` mode
    /// and typically goes with preemption recording/replaying.
    #[clap(long)]
    pub preemption_stacktrace: bool,

    /// File to write preemption stacktraces to. Implies `--preemption-stacktrace`. If a
    /// log file is not specified, preemption stacktraces are printed to stderr by default.
    #[clap(long, value_name = "filepath")]
    pub preemption_stacktrace_log_file: Option<PathBuf>,

    /// Enable deterministic IO by reassuring we always read/write the maximum possible bytes
    /// from IO syscalls. There might be cases that read/write syscalls return less bytes than
    /// requests. Detcore, makes an effort to request additional bytes until we reach the ones
    /// requested or EOF.
    #[clap(long)]
    pub deterministic_io: bool,

    /// Fail immediately on unsupported syscalls instead of forwarding them.
    /// Ordinary `hermit run` enables this policy; compatibility requires the
    /// explicit `--allow-unsupported-syscalls` opt-out.
    #[clap(long)]
    pub panic_on_unsupported_syscalls: bool,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-644): Review backend-safe fail-closed termination.
    /// Return a typed Tool error instead of unwinding through a backend callback.
    #[serde(default)]
    #[clap(skip)]
    pub exit_on_unsupported_syscall: bool,
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-644): Review process-tree shutdown for ptrace fail-closed mode.
    /// Terminate the whole tracer when an unsupported syscall is observed.
    #[serde(default)]
    #[clap(skip)]
    pub shutdown_on_unsupported_syscall: bool,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-644): Review the internal cross-process warning report channel.
    /// Internal inherited file descriptor used to aggregate unsupported syscalls.
    #[serde(default)]
    #[clap(skip)]
    pub unsupported_syscall_report_fd: Option<i32>,

    /// Panic when a precise PMU timer overshoots its expected RCB target instead of logging an
    /// error and continuing through normal timer handling. Intended for Detcore debugging.
    #[serde(default)]
    #[clap(
        long = "panic-on-rbc-overshoot",
        visible_alias = "panic-on-rcb-overshoot"
    )]
    pub panic_on_rcb_overshoot: bool,

    /// **Internal:** Set to `true` if we're inside a UTS namespace.
    // FIXME: This can be removed once spawn_fn-based tests support namespaces.
    #[clap(skip)]
    pub has_uts_namespace: bool,

    /// **Internal:** Path to the replay data folder.
    #[clap(skip)]
    pub replay_data: Option<PathBuf>,

    /// Kill all remaining tasks iff daemons are the only ones left.
    /// Disabled by default.
    #[clap(long)]
    pub kill_daemons: bool,

    /// Start gdbserver on `gdbserver_port` for remote debugging
    /// Disabled by default.
    #[clap(long)]
    pub gdbserver: bool,
    /// port gdbserver listening on
    #[clap(
        long,
        value_name = "uint16",
        help = "Port gdbserver listening on",
        default_value = "1234"
    )]
    pub gdbserver_port: u16,

    /// Configure the maximum time a guest thread may run without returning to Detcore. This is
    /// measured in virtual nanoseconds and enforced with retired conditional branch (RCB)
    /// counting. `--preemption-timeout` is retained as a deprecated alias.
    ///
    /// Set this to `disabled` or `0` to disable PMU-backed preemption. Positive values must be at
    /// least one RCB (10 virtual nanoseconds at the default clock multiplier) and require
    /// user-space hardware performance counters.
    #[serde(alias = "preemption_timeout")]
    #[clap(
                long,
                visible_alias = "preemption-timeout",
                value_name = "uint64|'disabled'",
                default_value = "200000000",
                value_parser = parse_timeslice)]
    pub max_timeslice: MaybeTimeslice,

    /// Target logical timeslice checked at syscall boundaries, in virtual nanoseconds. This avoids
    /// PMU preemption for workloads that enter the kernel frequently. Omit this option to use only
    /// `--max-timeslice`.
    #[serde(default)]
    #[clap(long, value_name = "virtual-nanoseconds")]
    pub target_timeslice: Option<NonZeroU64>,

    /// Shut down immediately upon SIGINT, rather than letting the guest handle it.
    #[clap(long)]
    pub sigint_instakill: bool,

    /// Warn if binds are non-zero.
    #[clap(long)]
    pub warn_non_zero_binds: bool,

    /// Apply a specialized scheduling heuristic which may help exercise certain bugs.
    #[clap(long, default_value = "none", value_name = "str")]
    // TODO: Rename this to scheduler_strategy?
    pub sched_heuristic: SchedHeuristic,

    /// Use this number to seed the PRNG that supplies randomness to the scheduler.
    #[clap(long, env = "HERMIT_SCHED_SEED", value_name = "uint64")]
    pub sched_seed: Option<u64>,

    /// Configure the probability for the Sticky Random scheduler to stay in a thread.
    /// For value 0.0, we are behaving like Random.
    /// For value 1.0, we are behaving like a DFS, where the same thread is
    /// always picked as long as it is available in the Run queue. After
    /// this thread is exhausted, the next thread will be chosen randomly.
    /// For value 0.5, we have a 50/50 chance to pick the same thread.
    #[clap(long, default_value = "0.0", value_name = "double")]
    pub sched_sticky_random_param: f64,

    /// **Internal:** An internal flag for indicating to Detcore whether we are in `hermit record` or
    /// `hermit replay` mode.  This is necessary because there are DIFFERENT global
    /// invariants in record mode (e.g. files dont exist).  If we move to a chroot model
    /// and reproduce more, recording less, then this flag should become obsolete.
    #[clap(skip = false)]
    pub recordreplay_modes: bool,

    /// **Internal:** debugging option to stop execution after a specific scheduler commit, aka turn number
    /// (non-negative integer). This only makes sense if `--sequentialize-threads` is specified, as the scheduler is otherwise not engaged.
    #[clap(long, value_name = "turn_N")]
    pub stop_after_turn: Option<u64>,

    /// **Internal:** debugging option to stop execution after a scheduler loop iteration (non-negative integer).
    /// This only makes sense if `--sequentialize-threads` is specified, as the scheduler is otherwise not engaged.
    #[clap(long, value_name = "iter_N")]
    pub stop_after_iter: Option<u64>,

    /// **Internal:** Debugging option to treat all sockets as mysterious external, nondeterministic
    /// entities, rather than container-internal and determinstically scheduled.
    #[clap(long)]
    pub debug_externalize_sockets: bool,

    /// **Internal:** Debugging option to change how futexes are implemented, either precisely modeled
    /// by hermit, by polling the kernel with non-blocking futex operations, or treated as external
    /// (nondeterministic) operations which unblock at imprecise times.
    #[clap(
        long,
        value_name = "precise|polling|external",
        default_value = "precise"
    )]
    pub debug_futex_mode: BlockingMode,

    /// Do not count the retired conditional branches (RCBs) of each thread towards its logical
    /// time.  Instead, count each checkin with the scheduler as a fixed increment to logical time.
    /// Even when this option is set, HW RCB performance counters may still be enabled if a
    /// max-timeslice is specified.
    #[clap(long)]
    pub no_rcb_time: bool,

    /// An option to enable logging the hash of heap memory maps for the purpose of determinism checking
    #[clap(long)]
    pub detlog_heap: bool,

    /// An option to enable logging the hash of stack memory maps for the purpose of determinism checking
    ///
    /// THIS HASH COVERS argv AND THE ENVIRONMENT, which the kernel places at the
    /// top of the initial process stack. Two runs whose command lines differ by a
    /// single character therefore produce different stack hashes from the first
    /// sample, even when the command lines are the same LENGTH and every stack
    /// address matches. Measured: equal-length-but-different argv diverged the
    /// hash 14 records in, while byte-identical argv held it for 5023 records.
    ///
    /// Holding a run-directory name to a fixed WIDTH is a sufficient control when
    /// only addresses matter, and is NOT sufficient here. Comparing two runs
    /// under this flag requires byte-identical argv and environment; otherwise
    /// the first divergence you find is your own input.
    #[clap(long)]
    pub detlog_stack: bool,

    /// Log a hash of the guest REGISTER FILE at guest-logical-control points, for determinism
    /// checking. stdout, the INFO log, the stack and the heap are all hashed today; the register
    /// file is not, so two backends can differ in register state and every existing check still
    /// reports parity.
    ///
    /// SAMPLED ONLY AT GUEST-LOGICAL-CONTROL POINTS -- see `Detcore::detlog_registers`. Registers
    /// are NOT sampled inside a tool handler: a backend running its handler in-guest executes code
    /// the ptrace reference never executes, so a difference there is correct behaviour, not a
    /// determinism bug.
    #[clap(long)]
    pub detlog_regs: bool,

    /// Log a hash of each syscall's OUTPUT BUFFER, taken at the syscall boundary from the
    /// address and length in the syscall's own arguments.
    ///
    /// WHAT IT SEES THAT THE MAPPING HASHES DO NOT. `--detlog-heap` and `--detlog-stack` hash a
    /// whole named mapping, so their coverage is decided by where the guest happened to ALLOCATE
    /// a buffer. Measured, three runs per cell, same netlink exchange with only the receive
    /// buffer's home changed: a `[stack]` buffer is missed by `--detlog-heap`, a `[heap]` buffer
    /// is missed by `--detlog-stack`, and a BSS/static or anonymous-`mmap` buffer is missed by
    /// BOTH even with both enabled. Anonymous `mmap` is where glibc puts any `malloc` above the
    /// 128 KiB `M_MMAP_THRESHOLD`. Reading the extent out of the syscall arguments makes the
    /// buffer's home irrelevant.
    ///
    /// WHY IT IS NOT REDUNDANT WITH `--verify`. A syscall whose buffer is a bare pointer in
    /// Reverie prints the ADDRESS, not the contents, so a `recvmsg` returning a stable
    /// `Ok(1468)` whose payload varies produces a character-identical record and `--verify`
    /// reports `bitwise_parity: true`. 44.1% of the syscalls in a QEMU/Linux boot move bytes
    /// through such a buffer.
    ///
    /// COST is proportional to bytes actually moved, NOT to syscall count or mapping size:
    /// ~0.75 s per GB of guest I/O. A QEMU/Linux boot moves 139.1 MB through these buffers,
    /// against the 10.9 TB `--detlog-heap` hashes over the same run.
    ///
    /// NAME IS PROVISIONAL: `io-buffers` is the owner's candidate and is not settled.
    ///
    /// ON BY DEFAULT. It was opt-in until 2026-08-24, and opt-in made the
    /// determinism gate weaker than its name: with the hash absent, the netlink
    /// `recvmsg` above compares equal and `--verify` reports success. A check
    /// that must be requested is not a standard. The opt-out exists for the
    /// deliberate case (bulk I/O where the cost matters and content parity is
    /// not the question), not as the ordinary setting.
    ///
    /// COST OF THE DEFAULT, measured 2026-08-24 on a 316-core x86_64 Linux
    /// build host: a typical small test guest pays about ONE MILLISECOND
    /// (`/bin/true` 0.029s -> 0.030s, `/bin/ls` 0.041s -> 0.041s, 8 runs each).
    /// 64 MiB through `cat` costs +0.07-0.10s in a RELEASE build, which is the
    /// ~1.1-1.6 s/GB matching the figure quoted above. The same workload in a
    /// DEBUG build costs +3.4s, roughly 50x more, because the hash loop is
    /// unoptimized -- so a debug-built node moving tens of megabytes is the one
    /// place the default is felt.
    #[clap(long = "no-detlog-io-buffers", action = clap::ArgAction::SetFalse)]
    pub detlog_io_buffers: bool,

    /// Sampling cadence for `--detlog-regs`: hash every Nth guest-logical-control point.
    ///
    /// COST TIER. 1 (the default) is the FULL tier -- every control point hashed -- and is what a
    /// short test should use. Measured cost at this scale is within run-to-run noise: /bin/true
    /// (49 control points), `wc -l /etc/passwd` (135) and a 5-iteration shell loop (195) were
    /// 0.04-0.07s with the flag on and the same with it off. A larger N is the SPOT-CHECK tier for
    /// runs where full hashing is too expensive; it trades detection latency for cost, since a
    /// divergence is only seen at the next sampled point. Every emitted line records the tier it
    /// was produced under, so a cell can state which tier it met instead of leaving it implicit.
    #[clap(long, default_value = "1", value_name = "uint64")]
    pub detlog_regs_cadence: u64,

    /// Configure a time offset (in seconds) between a container OS considered booted and a guest is executed
    /// This primarily affects 'sysinfo' syscall's 'uptime' field reporting
    #[clap(long, default_value = "120", value_name = "uint64")]
    pub sysinfo_uptime_offset: u64,

    /// Configure memory available for the container.  Takes a number of bytes, or shorthand (e.g.
    /// "1GB"). Right now this doesn't enforce an upper bound, but does affect the amount of memory
    /// reported to the guest.
    #[clap(long, default_value = "1GB", value_parser = try_parse_memory, value_name = "bytesize")]
    pub memory: u64,

    /// Configure extra interrupt points based on thread id and rcb counter. Detcore will raise a precise
    /// timer for this RCB whenever it detects that current current thread timeslice intercects any of the
    /// interrupt points specified
    #[clap(long, value_name = "tid:rcbs", value_parser = try_parse_numbers_with_colon)]
    pub interrupt_at: Vec<(DetTid, u64)>,

    /// Resolved happens-before program: deterministic ordering edges between
    /// anchored events (see `detcore_model::happens_before`). This is populated
    /// programmatically by hermit-cli after loading and resolving a
    /// `--happens-before` spec against the guest binary; it is not a direct CLI
    /// flag and is not serialized (it is reconstructed from the spec file each
    /// run, so `#[serde(skip)]` avoids requiring serde on `Sysno`-bearing
    /// positions and keeps save-config output stable). The scheduler enforces
    /// these edges only when `sequentialize_threads` is set.
    #[serde(skip)]
    #[clap(skip)]
    pub happens_before: Option<HappensBeforeProgram>,
}

fn try_parse_numbers_with_colon(from_str: &str) -> anyhow::Result<(DetTid, u64)> {
    if let Some((thread_id_str, time_str)) = from_str.split_once(':') {
        Ok((
            thread_id_str
                .parse::<DetTid>()
                .map_err(anyhow::Error::msg)?,
            time_str.parse::<u64>().map_err(anyhow::Error::msg)?,
        ))
    } else {
        anyhow::bail!(
            "unable to parse <thread_id>:<logical_time> from '{}'",
            from_str
        )
    }
}

fn try_parse_memory(from_str: &str) -> anyhow::Result<u64> {
    <bytesize::ByteSize as FromStr>::from_str(from_str)
        .map(|res| res.as_u64())
        .map_err(anyhow::Error::msg)
}

impl Config {
    /// Smallest PMU-backed maximum representable by one RCB at this clock multiplier.
    pub fn minimum_max_timeslice_nanos(&self) -> u64 {
        let slowdown = if self.chaos && self.chaos_per_thread_slowdown {
            self.chaos_slowdown_max_factor
        } else {
            1.0
        };
        let multiplier = self.clock_multiplier.unwrap_or(1.0) * slowdown;
        ((NANOS_PER_RCB * multiplier).ceil() as u64).max(NANOS_PER_RCB as u64)
    }

    /// Check invariants that must hold at every execution boundary without mutating the config.
    pub fn validate_invariants(&self) {
        assert!(self.sched_sticky_random_param >= 0.0);
        assert!(self.sched_sticky_random_param <= 1.0);
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-1149)
        assert!(
            self.chaos_slowdown_max_factor.is_finite()
                && self.chaos_slowdown_max_factor >= 1.0
                && self.chaos_slowdown_max_factor <= RcbTimeMultiplier::MAX,
            "chaos_slowdown_max_factor must be finite and in [1.0, {}], got {}",
            RcbTimeMultiplier::MAX,
            self.chaos_slowdown_max_factor
        );
        if let Some(multiplier) = self.clock_multiplier {
            assert!(
                multiplier.is_finite() && multiplier > 0.0,
                "clock_multiplier must be finite and positive"
            );
        }
        let minimum_max_timeslice = self.minimum_max_timeslice_nanos();
        assert!(
            self.max_timeslice
                .is_none_or(|timeslice| u64::from(timeslice) >= minimum_max_timeslice),
            "max_timeslice must be at least one RCB ({} virtual nanoseconds)",
            minimum_max_timeslice
        );
    }

    /// Sanity check the flags, and update any wherever flag B is implied by A.
    pub fn validate(&mut self) {
        self.validate_invariants();

        // TODO(T124429978) Restore the eprintln! calls below to tracing::warn! when the tracing
        // subscriber is set up early enough for these warnings to print.

        if self.record_preemptions_to.is_some() {
            self.record_preemptions = true;
        }
        // TODO: separate out recording flags: --record-preemptions vs --record-schedule-trace
        // if self.record_preemptions && !self.chaos {
        //     tracing::warn!(
        //         "Setting --record-preemptions when not in chaos mode doesn't do anything."
        //     );
        // }

        if self.replay_schedule_from.is_some() && self.replay_preemptions_from.is_some() {
            panic!("Cannot set both --replay-preemptions-from and --replay-schedule-from!!");
        }

        if self.chaos {
            self.sequentialize_threads = true;
        }

        if self.replay_preemptions_from.is_some() && self.imprecise_timers {
            eprintln!(
                "WARNING: Setting --imprecise timers with --replay-preemptions-from is probably not what you want. They won't replay precisely."
            );
        }

        if self.stop_after_turn.is_some() && !self.sequentialize_threads {
            eprintln!(
                "WARNING: --stop-after-turn will have no effect if --no-sequentialize-threads is enabled"
            );
            self.stop_after_turn = None;
        }
        if self.stop_after_iter.is_some() && !self.sequentialize_threads {
            eprintln!(
                "WARNING: --stop-after-iter will have no effect if --no-sequentialize-threads is enabled"
            );
            self.stop_after_iter = None;
        }

        if self.debug_externalize_sockets && !self.sequentialize_threads {
            eprintln!(
                "WARNING: --debug-externalize-sockets will have no effect if --no-sequentialize-threads is enabled"
            );
            self.debug_externalize_sockets = false;
        }

        if !self.stacktrace_event.is_empty()
            && !self.record_preemptions
            && self.replay_schedule_from.is_none()
        {
            eprintln!(
                "WARNING: -s/--stacktrace-event has no effect if not recording/replaying events!"
            );
        }

        if self.preemption_stacktrace_log_file.is_some() {
            self.preemption_stacktrace = true;
        }
    }

    /// Should we use RCB in computing logical time?
    ///
    /// The answer is NO either if `--no-rcb-time` is specified or if HW counters are disabled by
    /// setting `--max-timeslice=disabled`.
    pub fn use_rcb_time(&self) -> bool {
        self.max_timeslice.is_some() && !self.no_rcb_time
    }

    /// Should we convert sockets to SOCK_NONBLOCK?
    pub fn use_nonblocking_sockets(&self) -> bool {
        self.sequentialize_threads && !self.debug_externalize_sockets
    }

    /// Should we call trace_schedevent to trace each SchedEvent?
    /// This applies to both record and replay for scheduled events.
    pub fn should_trace_schedevent(&self) -> bool {
        self.record_preemptions || self.replay_schedule_from.is_some()
    }

    /// Returns manual interuption points for a given thread
    pub fn interrupts_for_thread(&self, thread_id: DetTid) -> BTreeSet<u64> {
        self.interrupt_at
            .iter()
            .filter_map(|(tid, time)| {
                if tid.eq(&thread_id) {
                    Some(*time)
                } else {
                    None
                }
            })
            .collect::<BTreeSet<u64>>()
    }
}

impl fmt::Display for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.virtualize_time {
            write!(f, " --no-virtualize-time")?;
        }
        if !self.virtualize_cpuid {
            write!(f, " --no-virtualize-cpuid")?;
        }
        if !self.virtualize_metadata {
            write!(f, " --no-virtualize-metadata")?;
        }
        if self.passthru_opt {
            write!(f, " --passthru-opt")?;
        }
        match self.runs_post_fork {
            RunsPostFork::Child => {}
            RunsPostFork::Parent => write!(f, " --runs-post-fork=parent")?,
            RunsPostFork::Random => write!(f, " --runs-post-fork=random")?,
        }
        let default_epoch: DateTime<Utc> = DEFAULT_EPOCH_STR.parse::<DateTime<Utc>>().unwrap();
        if self.epoch != default_epoch {
            write!(f, " --epoch={}", self.epoch.to_rfc3339())?;
        }
        if self.seed != 0 {
            write!(f, " --seed={}", self.seed)?;
        }

        if let Some(rng_seed) = self.rng_seed {
            write!(f, " --rng-seed={}", rng_seed)?;
        }
        if let Some(fuzz_seed) = self.fuzz_seed {
            write!(f, " --fuzz-seed={}", fuzz_seed)?;
        }

        if self.fuzz_futexes {
            write!(f, " --fuzz-futexes")?;
        }
        if self.chaos_target_races {
            write!(f, " --chaos-target-races")?;
        }
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-1149)
        if self.chaos_per_thread_slowdown {
            write!(f, " --chaos-per-thread-slowdown")?;
            write!(
                f,
                " --chaos-slowdown-max-factor={}",
                self.chaos_slowdown_max_factor
            )?;
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-1151)
            if self.chaos_epoch_length_ns > 0 {
                write!(f, " --chaos-epoch-length-ns={}", self.chaos_epoch_length_ns)?;
            }
        }
        if let Some(m) = self.clock_multiplier {
            write!(f, " --clock-multiplier={}", m)?;
        }
        if self.imprecise_timers {
            write!(f, " --imprecise-timers")?;
        }
        if self.chaos {
            write!(f, " --chaos")?;
        }
        if self.record_preemptions {
            write!(f, " --record-preemptions")?;
        }

        if let Some(p) = &self.record_preemptions_to {
            let s = p.to_str().expect("valid unicode path");
            write!(f, " --record-preemptions-to={}", shell_words::quote(s))?;
        }
        if let Some(p) = &self.replay_preemptions_from {
            let s = p.to_str().expect("valid unicode path");
            write!(f, " --replay-preemptions-from={}", shell_words::quote(s))?;
        }
        if let Some(p) = &self.replay_schedule_from {
            let s = p.to_str().expect("valid unicode path");
            write!(f, " --replay-schedule-from={}", shell_words::quote(s))?;
        }
        if self.replay_exhausted_panic {
            write!(f, " --replay-exhausted-panic")?;
        }
        if self.die_on_desync {
            write!(f, " --die-on-desync")?;
        }
        for (index, path) in &self.stacktrace_event {
            write!(f, " --stacktrace-event={}", index)?;
            if let Some(p) = path {
                let s = p.to_str().expect("valid unicode path");
                write!(f, ",{}", shell_words::quote(s))?;
            }
        }
        if self.preemption_stacktrace {
            write!(f, " --preemption-stacktrace")?;
        }
        if self.panic_on_unsupported_syscalls {
            write!(f, " --panic-on-unsupported-syscalls")?;
        }
        if self.panic_on_rcb_overshoot {
            write!(f, " --panic-on-rbc-overshoot")?;
        }
        if self.kill_daemons {
            write!(f, " --kill-daemons")?;
        }
        if self.gdbserver {
            write!(f, " --gdbserver")?;
        }
        if self.gdbserver_port != /* default */ 1234u16 {
            write!(f, " --gdbserver-port={}", self.gdbserver_port)?;
        }
        match &self.max_timeslice {
            Some(x) => {
                if *x != NonZeroU64::new(200_000_000).unwrap() {
                    write!(f, " --max-timeslice={}", x)?;
                }
            }
            None => {
                write!(f, " --max-timeslice=disabled")?;
            }
        }
        if let Some(target_timeslice) = self.target_timeslice {
            write!(f, " --target-timeslice={}", target_timeslice)?;
        }
        if self.sigint_instakill {
            write!(f, " --sigint-instakill")?;
        }
        if self.warn_non_zero_binds {
            write!(f, " --warn-non-zero-binds")?;
        }
        match &self.sched_heuristic {
            SchedHeuristic::None => {}
            SchedHeuristic::ConnectBind => {
                write!(f, " --sched-heuristic=connectbind")?;
            }
            SchedHeuristic::Random => {
                write!(f, " --sched-heuristic=random")?;
            }
            SchedHeuristic::StickyRandom => {
                write!(f, " --sched-heuristic=stickyrandom")?;
            }
        }
        if let Some(s) = self.sched_seed {
            write!(f, " --sched-seed={}", s)?;
        }
        if self.sched_sticky_random_param != 0.0 {
            write!(
                f,
                " --sched-sticky-random-param={}",
                self.sched_sticky_random_param
            )?;
        }
        if let Some(t) = self.stop_after_turn {
            write!(f, " --stop-after-turn={}", t)?;
        }
        if let Some(i) = self.stop_after_iter {
            write!(f, " --stop-after-iter={}", i)?;
        }
        if self.debug_externalize_sockets {
            write!(f, " --debug-externalize-sockets")?;
        }
        match &self.debug_futex_mode {
            BlockingMode::External => {
                write!(f, " --debug-futex-mode=external")?;
            }
            BlockingMode::Polling => {
                write!(f, " --debug-futex-mode=polling")?;
            }
            BlockingMode::Precise => { /* default */ }
        }
        if self.no_rcb_time {
            write!(f, " --no-rcb-time")?;
        }
        if self.detlog_heap {
            write!(f, " --detlog-heap")?;
        }
        if self.detlog_stack {
            write!(f, " --detlog-stack")?;
        }
        if self.detlog_regs {
            write!(f, " --detlog-regs")?;
        }
        if self.detlog_regs_cadence != /* default */ 1 {
            write!(f, " --detlog-regs-cadence={}", self.detlog_regs_cadence)?;
        }
        if !self.detlog_io_buffers {
            write!(f, " --no-detlog-io-buffers")?;
        }
        if self.sysinfo_uptime_offset != /* default */ 120 {
            write!(f, " --sysinfo-uptime-offset={}", self.sysinfo_uptime_offset)?;
        }
        if self.memory != 1_000_000_000 {
            write!(f, " --memory={}", self.memory)?;
        }
        for (tid, rcb) in &self.interrupt_at {
            write!(f, " --interrupt-at={}:{}", tid, rcb)?;
        }
        Ok(())
    }
}

/// Which side of an ordinary fork/clone receives the first post-registration turn.
#[derive(
    Debug,
    Default,
    Clone,
    Copy,
    Serialize,
    Deserialize,
    Parser,
    PartialEq,
    Eq
)]
pub enum RunsPostFork {
    /// Run the newly registered child before its parent resumes.
    #[default]
    Child,
    /// Allow the parent to resume before the newly registered child starts.
    Parent,
    /// Deterministically choose child-first or parent-first from the scheduler seed.
    Random,
}

impl FromStr for RunsPostFork {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "child" => Ok(Self::Child),
            "parent" => Ok(Self::Parent),
            "random" => Ok(Self::Random),
            _ => Err(format!(
                "Expected Child|Parent|Random, could not parse: {:?}",
                s
            )),
        }
    }
}

/// How should we handle syscalls which may block, but are internal to the hermit container?
/// These syscalls are determinizable, but there are multiple methods of doing so.
/// These choices *do not* apply to blocking syscalls that wait for external conditions outside the
/// container, such as network responses.
///
/// Mostly it helps to switch this as: (1) a debugging aid to figure out what is going wrong with a
/// given guest program, or (2) in order to find the more performant mode for a given guest program.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Parser, PartialEq, Eq)]
pub enum BlockingMode {
    /// Handle the internal blocking syscall as though it was external, and unblocks at an
    /// unpredictable nondeterministic time.  These blocked threads will be parked in the
    /// scheduler's BlockedPool.
    ///
    /// (TODO: In the future these scheduling decisions will be recorded, and this comment needs to
    /// be updated accordingly.)
    External,
    /// Transform each blocking syscall into non-blocking, and then the scheduler will use that
    /// non-blocking form to repeatedly poll for completion of the operation.  When polling occurs
    /// (and the backoff policy there on) is decided by the scheduler.
    /// See NOTE [Blocking Syscalls via Internal Polling] in this folder.
    Polling,
    /// Precisely model the blocking and unblocking behavior inside hermit.
    /// TODO: This work is not completed yet for all forms of blocking syscalls.
    Precise,
}

impl FromStr for BlockingMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "polling" => Ok(BlockingMode::Polling),
            "precise" => Ok(BlockingMode::Precise),
            "external" => Ok(BlockingMode::External),
            _ => Err(format!(
                "Expected Polling|Precise|External, could not parse: {:?}",
                s
            )),
        }
    }
}

#[derive(
    Debug,
    Default,
    Clone,
    Copy,
    Serialize,
    Deserialize,
    Parser,
    PartialEq,
    Eq
)]
/// Apply a specialized scheduling heuristic which may help exercise certain bugs.
pub enum SchedHeuristic {
    /// Don't modify the scheduling algorithm.
    // TODO: Is the default a round robin?
    #[default]
    None,
    /// Prioritize connect and deprioritize bind to exercise races
    ConnectBind,
    /// Random: Randomly pick any available thread to make progress.
    Random,
    /// Sticky Random: Randomly pick any available thread. On the next round,
    /// and after the thread is parked, randomly choose if we will continue
    /// executing on the same thread, or picking another one.
    StickyRandom,
    // TODO: make all sleeps "instant".
}

// Lame to not derive this, but even `derive_more` won't do enums.
impl FromStr for SchedHeuristic {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "none" | "roundrobin" => Ok(SchedHeuristic::None),
            "connectbind" => Ok(SchedHeuristic::ConnectBind),
            "random" => Ok(SchedHeuristic::Random),
            "stickyrandom" => Ok(SchedHeuristic::StickyRandom),
            _ => Err(format!(
                "Expected None|ConnectBind|Random|StickyRandom, could not parse: {:?}",
                s
            )),
        }
    }
}

/// An optional virtual-timeslice duration. `None` disables that preemption mechanism.
pub type MaybeTimeslice = Option<NonZeroU64>;

/// Deprecated name for an optional PMU-backed virtual-timeslice duration.
#[deprecated(note = "use MaybeTimeslice")]
pub type MaybePreemptionTimeout = MaybeTimeslice;

fn parse_timeslice(src: &str) -> Result<MaybeTimeslice, ParseTimesliceError> {
    if let Ok(n) = src.parse::<u64>() {
        if n != 0 && n < NANOS_PER_RCB as u64 {
            Err(ParseTimesliceError::new(
                "PMU-backed timeslices must be at least one RCB (10 virtual nanoseconds)",
            ))
        } else {
            Ok(NonZeroU64::new(n))
        }
    } else {
        match src {
            "disabled" => Ok(None),
            _ => Err(ParseTimesliceError::new(
                "Unable to parse timeslice, expected disabled or a non-negative integer",
            )),
        }
    }
}

fn parse_index_with_path(src: &str) -> Result<(u64, Option<PathBuf>), String> {
    let convert = |e| format!("Failed to parse int index before comma: {e}");
    if let Some((index_str, path)) = src.split_once(',') {
        let ix = index_str.parse::<u64>().map_err(convert)?;
        let pathbuf = PathBuf::from_str(path).map_err(|_| "the impossible happened")?;
        Ok((ix, Some(pathbuf)))
    } else {
        let ix = src.parse::<u64>().map_err(convert)?;
        Ok((ix, None))
    }
}

#[derive(Debug)]
struct ParseTimesliceError {
    details: String,
}

impl fmt::Display for ParseTimesliceError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.details)
    }
}

impl ParseTimesliceError {
    fn new(msg: &str) -> ParseTimesliceError {
        ParseTimesliceError {
            details: msg.to_string(),
        }
    }
}

impl std::error::Error for ParseTimesliceError {
    fn description(&self) -> &str {
        &self.details
    }
}

/// The default epoch used by DetCore for things like initial file modtimes.
///
/// N.B. Default to a reasonable date. Some programs (like zip) have trouble with the
/// original unix epoch (time zero).
pub static DEFAULT_EPOCH_STR: &str = "2026-01-01T00:00:00Z";

impl Config {
    /// Construct the config using environment variables only, not CLI args.
    pub fn from_env() -> Self {
        let args: [OsString; 2] = [
            OsString::from("CMD"), // Silly/unused.
            OsString::from(format!("--epoch={}", DEFAULT_EPOCH_STR)),
        ];
        Config::parse_from(args.iter())
    }

    /// Returns effective "rng-seed" parameter taking in account "seed"
    /// parameter if former isn't specified
    pub fn rng_seed(&self) -> u64 {
        self.rng_seed.unwrap_or(self.seed)
    }

    /// Returns the fuzz_seed, as specified by the user or defaulting to the primary seed if
    /// unspecified.
    pub fn fuzz_seed(&self) -> u64 {
        self.fuzz_seed.unwrap_or(self.seed)
    }

    /// Returns effective "sched-seed" parameter taking in account "seed"
    /// parameter if former isn't specified
    pub fn sched_seed(&self) -> u64 {
        self.sched_seed.unwrap_or(self.seed)
    }
}

/// N.B. we don't want to specify two different notions of "default", so we use the
/// `Clap` instance above.
/// Environment variable carrying the coordinator's [`config_wire_fingerprint`]
/// to an out-of-process plugin.
///
/// Named alongside the other `REVERIE_SABRE_HERMIT_*` launch variables so the
/// two travel together and a reader finds them in one place.
pub const CONFIG_FINGERPRINT_ENV: &str = "REVERIE_SABRE_HERMIT_CONFIG_FINGERPRINT";

const CONFIG_DEFINITION_SOURCES: &[&[u8]] = &[
    include_bytes!("config.rs"),
    include_bytes!("happens_before.rs"),
    include_bytes!("pid.rs"),
    include_bytes!("schedule.rs"),
    include_bytes!("time.rs"),
];

/// A fingerprint of this build's [`Config`] payload and the configuration and
/// clock RPC definitions shared by a plugin and its coordinator.
///
/// # Why this exists
///
/// An out-of-process plugin such as `libdetcore_sabre.so` is a separate Cargo
/// artifact that lands in the same target directory as `hermit`. Changing
/// `Config` or `DetTime` -- or merely switching branches -- leaves the plugin
/// stale while everything still *looks* built. `Config` is transferred during
/// the RPC handshake, and `DetTime` is the first field in every Detcore request.
/// A stale plugin decodes either against the wrong layout and the failure
/// surfaces as an opaque codec error: measured, one added `bool` field
/// produced `Decode(InvalidBooleanValue(20))` at connect, which points nowhere
/// near "your plugin is from a different build" and cost a long diagnosis while
/// blocking every SaBRe measurement.
///
/// # What it measures
///
/// Two encodings of `Config::default()` and the source definitions for the
/// configuration and clock RPC fields are fingerprinted with separate domains:
///
/// - the exact legacy-bincode bytes used by Reverie RPC, which detect changes
///   such as `u32` to `u64` even when both default to JSON number zero; and
/// - the JSON encoding, which carries every field name and makes a pure rename
///   visible even though bincode is positional; and
/// - the source files defining `Config`, its local serialized field types, and
///   `DetTime`, which catch wire-incompatible changes hidden by a default such
///   as `Option<u64>::None` to `Option<u32>::None`, or an added clock field that
///   leaves both encodings of `Config` unchanged.
///
/// The source and JSON domains are deliberately stricter than the wire format
/// strictly requires. A documentation-only edit in one of these files can
/// require rebuilding the plugin; missing a wire-incompatible hidden variant
/// can make it decode the handshake or a subsequent request at the wrong offsets.
pub fn config_wire_fingerprint() -> String {
    let config = Config::default();
    let wire = bincode::serde::encode_to_vec(&config, bincode::config::legacy())
        .expect("Config::default() must encode with the Reverie RPC bincode configuration");
    let named_shape = serde_json::to_string(&config)
        .expect("Config::default() must encode as JSON for field-name checking");
    fingerprint_of_config_material(&wire, &named_shape, CONFIG_DEFINITION_SOURCES)
}

/// Domain-separated FNV-1a over wire bytes, named JSON, and defining source.
/// This is a mismatch detector, not a security boundary. Length-prefixing each
/// domain prevents two different source-file boundaries from hashing the same
/// concatenation.
fn fingerprint_of_config_material(
    wire: &[u8],
    named_shape: &str,
    definition_sources: &[&[u8]],
) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut update = |domain: u8, bytes: &[u8]| {
        for byte in std::iter::once(&domain)
            .chain((bytes.len() as u64).to_le_bytes().iter())
            .chain(bytes.iter())
        {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x1000_0000_01b3);
        }
    };
    update(0, wire);
    update(1, named_shape.as_bytes());
    for source in definition_sources {
        update(2, source);
    }
    format!("{hash:016x}")
}

impl Default for Config {
    fn default() -> Self {
        let v: Vec<String> = vec![];
        Config::parse_from(v.iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_epoch_is_2026() {
        assert_eq!(DEFAULT_EPOCH_STR, "2026-01-01T00:00:00Z");
        let epoch = DEFAULT_EPOCH_STR.parse::<DateTime<Utc>>().unwrap();
        assert_eq!(epoch.timestamp(), 1_767_225_600);
    }

    #[test]
    fn default_backend_capabilities_match_instrumented_backends() {
        let config = Config::default();
        assert!(!config.backend_reports_physical_process_exits);
        assert!(!config.backend_serializes_fork_children);
        assert!(config.backend_dispatches_thread_tools);
        assert!(config.backend_tracks_process_children);
        assert!(config.backend_runs_exit_robust_list);
        assert!(!config.backend_requires_thread_directed_process_signals);
        assert!(config.backend_supports_parked_write_signal_interruption);
        assert!(!config.backend_virtualizes_capability_prctls);
        assert!(!config.backend_defers_vfork_child_registration);
    }

    #[test]
    fn missing_mountinfo_provenance_deserializes_as_empty() {
        let mut value = serde_json::to_value(Config::default()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .remove("mountinfo_root_rewrites");
        value
            .as_object_mut()
            .unwrap()
            .remove("mountinfo_device_rewrites");
        value.as_object_mut().unwrap().remove("mountinfo_mount_ids");
        value
            .as_object_mut()
            .unwrap()
            .remove("mountinfo_mount_ids_captured");
        value
            .as_object_mut()
            .unwrap()
            .remove("fdinfo_unlisted_mount_ids");
        let restored: Config = serde_json::from_value(value).unwrap();
        assert!(restored.mountinfo_root_rewrites.is_empty());
        assert!(restored.mountinfo_device_rewrites.is_empty());
        assert!(restored.mountinfo_mount_ids.is_empty());
        assert!(!restored.mountinfo_mount_ids_captured);
        assert!(restored.fdinfo_unlisted_mount_ids.is_empty());
    }

    #[test]
    fn runs_post_fork_parses_all_modes_and_defaults_to_child() {
        assert_eq!(Config::default().runs_post_fork, RunsPostFork::Child);
        assert_eq!(
            Config::parse_from(["detcore", "--runs-post-fork=parent"]).runs_post_fork,
            RunsPostFork::Parent
        );
        assert_eq!(
            Config::parse_from(["detcore", "--runs-post-fork=random"]).runs_post_fork,
            RunsPostFork::Random
        );
        assert!(Config::try_parse_from(["detcore", "--runs-post-fork=invalid"]).is_err());
    }

    #[test]
    fn panic_on_rcb_overshoot_is_opt_in_and_round_trips() {
        assert!(!Config::default().panic_on_rcb_overshoot);

        let config = Config::parse_from(["detcore", "--panic-on-rbc-overshoot"]);
        assert!(config.panic_on_rcb_overshoot);
        assert!(config.to_string().contains(" --panic-on-rbc-overshoot"));

        let alias = Config::parse_from(["detcore", "--panic-on-rcb-overshoot"]);
        assert!(alias.panic_on_rcb_overshoot);
    }

    #[test]
    fn config_display_preserves_nondefault_post_fork_modes() {
        let mut config = Config {
            runs_post_fork: RunsPostFork::Parent,
            ..Config::default()
        };
        assert!(config.to_string().contains(" --runs-post-fork=parent"));

        config.runs_post_fork = RunsPostFork::Random;
        assert!(config.to_string().contains(" --runs-post-fork=random"));
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1149)
    #[test]
    fn chaos_per_thread_slowdown_is_opt_in_and_round_trips() {
        // Off by default; the factor default is present but inert.
        let dflt = Config::default();
        assert!(!dflt.chaos_per_thread_slowdown);
        assert_eq!(dflt.chaos_slowdown_max_factor, 10.0);
        // Default (disabled) config does not emit the flags.
        assert!(!dflt.to_string().contains("--chaos-per-thread-slowdown"));

        let config = Config::parse_from([
            "detcore",
            "--chaos",
            "--chaos-per-thread-slowdown",
            "--chaos-slowdown-max-factor=4.5",
        ]);
        assert!(config.chaos_per_thread_slowdown);
        assert_eq!(config.chaos_slowdown_max_factor, 4.5);

        // The Display round-trips both flags into the recorded schedule artifact.
        let rendered = config.to_string();
        assert!(rendered.contains(" --chaos-per-thread-slowdown"));
        assert!(rendered.contains(" --chaos-slowdown-max-factor=4.5"));
        let reparsed = Config::parse_from(
            std::iter::once("detcore".to_string())
                .chain(rendered.split_whitespace().map(String::from)),
        );
        assert!(reparsed.chaos_per_thread_slowdown);
        assert_eq!(reparsed.chaos_slowdown_max_factor, 4.5);
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    #[test]
    fn chaos_epoch_length_is_opt_in_and_round_trips() {
        // Off by default (single stable factor == plain per-thread-slowdown).
        let dflt = Config::default();
        assert_eq!(dflt.chaos_epoch_length_ns, 0);
        assert!(!dflt.to_string().contains("--chaos-epoch-length-ns"));

        // Epochs are only emitted alongside per-thread-slowdown.
        let config = Config::parse_from([
            "detcore",
            "--chaos",
            "--chaos-per-thread-slowdown",
            "--chaos-epoch-length-ns=100000",
        ]);
        assert_eq!(config.chaos_epoch_length_ns, 100000);

        let rendered = config.to_string();
        assert!(rendered.contains(" --chaos-epoch-length-ns=100000"));
        let reparsed = Config::parse_from(
            std::iter::once("detcore".to_string())
                .chain(rendered.split_whitespace().map(String::from)),
        );
        assert_eq!(reparsed.chaos_epoch_length_ns, 100000);

        // Without per-thread-slowdown the epoch flag is inert and not rendered.
        let no_slowdown = Config::parse_from(["detcore", "--chaos", "--chaos-epoch-length-ns=100"]);
        assert_eq!(no_slowdown.chaos_epoch_length_ns, 100);
        assert!(!no_slowdown.to_string().contains("--chaos-epoch-length-ns"));
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1149)
    #[test]
    #[should_panic(expected = "chaos_slowdown_max_factor must be finite and in")]
    fn validate_rejects_chaos_slowdown_max_factor_below_one() {
        let mut config = Config {
            chaos_slowdown_max_factor: 0.5,
            ..Default::default()
        };
        config.validate();
    }

    #[test]
    #[should_panic(expected = "max_timeslice must be at least one RCB")]
    fn validate_rejects_max_timeslice_below_one_rcb() {
        let mut config = Config {
            max_timeslice: NonZeroU64::new(NANOS_PER_RCB as u64 - 1),
            ..Default::default()
        };

        config.validate();
    }

    #[test]
    fn validate_accepts_one_rcb_max_timeslice() {
        let mut config = Config {
            max_timeslice: NonZeroU64::new(NANOS_PER_RCB as u64),
            ..Default::default()
        };

        config.validate();
    }

    #[test]
    #[should_panic(expected = "clock_multiplier must be finite and positive")]
    fn validate_rejects_invalid_clock_multiplier() {
        let mut config = Config {
            clock_multiplier: Some(0.0),
            ..Default::default()
        };
        config.validate();
    }

    #[test]
    #[should_panic(expected = "max_timeslice must be at least one RCB")]
    fn validate_scales_one_rcb_minimum_with_clock_multiplier() {
        let mut config = Config {
            max_timeslice: NonZeroU64::new(10),
            clock_multiplier: Some(2.0),
            ..Default::default()
        };
        config.validate();
    }

    #[test]
    fn config_fingerprint_includes_clock_rpc_definitions() {
        let config = Config::default();
        let wire = bincode::serde::encode_to_vec(&config, bincode::config::legacy()).unwrap();
        let named_shape = serde_json::to_string(&config).unwrap();
        let current = config_wire_fingerprint();
        let clock_source = include_bytes!("time.rs").as_slice();

        // The previous guard covered these same Config bytes and definitions,
        // but omitted DetTime. Adding a positional clock field could therefore
        // pass the handshake guard and corrupt the following request on decode.
        let without_clock: Vec<_> = CONFIG_DEFINITION_SOURCES
            .iter()
            .copied()
            .filter(|source| *source != clock_source)
            .collect();
        assert_ne!(
            fingerprint_of_config_material(&wire, &named_shape, &without_clock),
            current,
            "the published fingerprint must reject source inputs that omit the RPC clock"
        );

        // Hold all Config material fixed and remove only the added serialized
        // clock field from its definition. A future clock-only change must also
        // invalidate the existing artifact guard, independently of config.rs.
        let changed_clock =
            include_str!("time.rs").replacen("    inherited_nanos: LogicalDuration,", "", 1);
        assert_ne!(changed_clock.as_bytes(), clock_source);
        let changed_sources: Vec<_> = CONFIG_DEFINITION_SOURCES
            .iter()
            .map(|source| {
                if *source == clock_source {
                    changed_clock.as_bytes()
                } else {
                    *source
                }
            })
            .collect();
        assert_ne!(
            fingerprint_of_config_material(&wire, &named_shape, &changed_sources),
            current,
            "a clock-only serialized field change must invalidate the fingerprint"
        );
    }

    #[test]
    fn config_fingerprint_is_stable_and_shape_sensitive() {
        // STABLE: a build must agree with itself, or the guard would reject a
        // MATCHED pair -- which would be worse than having no guard at all.
        assert_eq!(config_wire_fingerprint(), config_wire_fingerprint());
        assert_eq!(config_wire_fingerprint().len(), 16);

        let config = Config::default();
        let base = serde_json::to_string(&config).unwrap();
        let wire = bincode::serde::encode_to_vec(&config, bincode::config::legacy()).unwrap();
        assert_eq!(
            fingerprint_of_config_material(&wire, &base, CONFIG_DEFINITION_SOURCES),
            config_wire_fingerprint()
        );

        // SHAPE-SENSITIVE, checked on the same mechanism the real function uses.
        // One added field is exactly the change that caused the outage.
        let with_extra_field = format!("{},\"a_new_flag\":false}}", &base[..base.len() - 1]);
        assert_ne!(
            fingerprint_of_config_material(&wire, &with_extra_field, CONFIG_DEFINITION_SOURCES),
            config_wire_fingerprint()
        );
        // A removed field.
        let removed = base.replacen("\"virtualize_time\":true,", "", 1);
        assert_ne!(
            fingerprint_of_config_material(&wire, &removed, CONFIG_DEFINITION_SOURCES),
            config_wire_fingerprint()
        );
        // A pure rename, which bincode would tolerate but which we still refuse.
        let renamed = base.replacen("\"virtualize_time\"", "\"virtualise_time\"", 1);
        assert_ne!(
            fingerprint_of_config_material(&wire, &renamed, CONFIG_DEFINITION_SOURCES),
            config_wire_fingerprint()
        );

        // The counterexample the JSON-only fingerprint missed: serde_json emits
        // the same text for integer zero regardless of width, but legacy bincode
        // changes the payload width. A stale peer would decode every following
        // field at the wrong offset.
        #[derive(Serialize)]
        struct U32Field {
            field: u32,
        }
        #[derive(Serialize)]
        struct U64Field {
            field: u64,
        }
        let u32_value = U32Field { field: 0 };
        let u64_value = U64Field { field: 0 };
        let u32_json = serde_json::to_string(&u32_value).unwrap();
        let u64_json = serde_json::to_string(&u64_value).unwrap();
        assert_eq!(
            u32_json, u64_json,
            "the planted JSON collision must be real"
        );
        let u32_wire =
            bincode::serde::encode_to_vec(&u32_value, bincode::config::legacy()).unwrap();
        let u64_wire =
            bincode::serde::encode_to_vec(&u64_value, bincode::config::legacy()).unwrap();
        assert_ne!(u32_wire, u64_wire, "the planted wire retype must be real");
        assert_ne!(
            fingerprint_of_config_material(&u32_wire, &u32_json, &[b"struct S { field: u32 }"]),
            fingerprint_of_config_material(&u64_wire, &u64_json, &[b"struct S { field: u64 }"]),
            "a wire-incompatible integer retype must change the fingerprint"
        );

        // Defaults can hide an incompatible inner type in BOTH value encodings.
        // The definition source is therefore load-bearing, not decorative.
        #[derive(Serialize)]
        struct OptionalU32 {
            field: Option<u32>,
        }
        #[derive(Serialize)]
        struct OptionalU64 {
            field: Option<u64>,
        }
        let optional_u32 = OptionalU32 { field: None };
        let optional_u64 = OptionalU64 { field: None };
        let optional_u32_json = serde_json::to_string(&optional_u32).unwrap();
        let optional_u64_json = serde_json::to_string(&optional_u64).unwrap();
        assert_eq!(optional_u32_json, optional_u64_json);
        let optional_u32_wire =
            bincode::serde::encode_to_vec(&optional_u32, bincode::config::legacy()).unwrap();
        let optional_u64_wire =
            bincode::serde::encode_to_vec(&optional_u64, bincode::config::legacy()).unwrap();
        assert_eq!(
            optional_u32_wire, optional_u64_wire,
            "the planted default must be invisible in both value encodings"
        );
        assert_ne!(
            fingerprint_of_config_material(
                &optional_u32_wire,
                &optional_u32_json,
                &[b"struct S { field: Option<u32> }"]
            ),
            fingerprint_of_config_material(
                &optional_u64_wire,
                &optional_u64_json,
                &[b"struct S { field: Option<u64> }"]
            ),
            "a hidden wire-incompatible inner-type change must alter the fingerprint"
        );
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    #[test]
    #[should_panic(expected = "max_timeslice must be at least one RCB")]
    fn validate_scales_one_rcb_minimum_with_chaos_slowdown() {
        let mut config = Config {
            chaos: true,
            chaos_per_thread_slowdown: true,
            chaos_slowdown_max_factor: 4.0,
            max_timeslice: NonZeroU64::new(39),
            ..Default::default()
        };
        config.validate();
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    #[test]
    #[should_panic(expected = "chaos_slowdown_max_factor must be finite and in")]
    fn validate_rejects_unrepresentable_chaos_slowdown_factor() {
        let mut config = Config {
            chaos_slowdown_max_factor: RcbTimeMultiplier::MAX * 2.0,
            ..Default::default()
        };
        config.validate();
    }
}
