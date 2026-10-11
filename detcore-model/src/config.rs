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
use std::time::SystemTime;

use chrono::DateTime;
use chrono::Utc;
use clap::Parser;
use reverie::BackendCapabilities;
use serde::Deserialize;
use serde::Serialize;

use crate::happens_before::HappensBeforeProgram;
use crate::network_trace::NetworkTraceConfig;
use crate::pid::DetTid;
use crate::schedule::SigWrapper;
use crate::time::NANOS_PER_RCB;
use crate::time::NANOS_PER_SCHED;
use crate::time::RcbTimeMultiplier;

const fn default_backend_capabilities() -> BackendCapabilities {
    BackendCapabilities::PTRACE
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

/// The seccomp state of the thread that starts the guest, which the guest
/// inherits: every filter that thread has applies to the guest's system calls,
/// and to the calls Detcore injects into it. Detcore refuses the guest's own
/// `seccomp(2)` and `prctl(PR_SET_SECCOMP)`, so these and the backend's own
/// filter are the only filters a guest has.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize
)]
pub enum InheritedSeccomp {
    /// Not measured, or not readable, so a filter may apply.
    #[default]
    Unknown,
    /// No seccomp (`Seccomp: 0`).
    Disabled,
    /// Strict mode (`Seccomp: 1`).
    Strict,
    /// Filter mode (`Seccomp: 2`), with the number of filters when
    /// `Seccomp_filters` gives it.
    Filter(Option<u32>),
}

impl InheritedSeccomp {
    /// The state of the calling thread, which a process it forks inherits,
    /// from `/proc/thread-self/status`.
    pub fn measure() -> Self {
        std::fs::read_to_string("/proc/thread-self/status")
            .map_or(Self::Unknown, |status| Self::parse(&status))
    }

    /// The state the `Seccomp` and `Seccomp_filters` lines of the text of a
    /// `/proc/<pid>/status` file give. A missing or unexpected `Seccomp` line
    /// is unknown.
    pub fn parse(status: &str) -> Self {
        let field = |name: &str| {
            status
                .lines()
                .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
                .map(str::trim)
        };
        match field("Seccomp") {
            Some("0") => Self::Disabled,
            Some("1") => Self::Strict,
            Some("2") => Self::Filter(field("Seccomp_filters").and_then(|n| n.parse().ok())),
            _ => Self::Unknown,
        }
    }
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

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-845): Review in-process backend descriptor discovery.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-845): Review host-clock futex deadline detection.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-845): Review backend-owned syscall-clobber determinism.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-845): Review backend-local exit-group RPC cancellation.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1058): Review process-signal identity translation.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1125): Review backend-owned capability-control prctls.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1152): Review deferred vfork child registration.
    /// How the execution backend runs the guest, as the backend reports it
    /// through Reverie (`reverie::Backend::capabilities`).
    ///
    /// Detcore reads facts about the backend only from here, never from the
    /// backend's name, so a new backend is modelled correctly by reporting its
    /// capabilities. The host copies the running backend's answer into this
    /// field before the run starts. The default describes a ptrace-hosted
    /// guest, the backend Detcore's own tests use.
    ///
    /// JSON that the guest can see is written by [`to_legacy_backend_json`],
    /// which carries this field as the separate keys used before it existed;
    /// [`from_legacy_backend_json`] reads them back.
    #[serde(default = "default_backend_capabilities")]
    #[clap(skip = BackendCapabilities::PTRACE)]
    pub backend: BackendCapabilities,

    /// The backend runs the guest as real host threads whose signal state the kernel
    /// owns and reports in `/proc`, and resumes a Tool's restart errno through the
    /// kernel's own signal-delivery and syscall-restart path. Blocking waits then
    /// decide signal interruption from the guest's real mask and dispositions
    /// (<https://github.com/rrnewton/hermit/issues/3146>). Off by default: only the
    /// backends measured to honor that contract opt in.
    ///
    /// The host sets it from the backend's name, because
    /// `reverie::BackendCapabilities` has no field for this fact yet. It has no
    /// legacy key: [`to_legacy_backend_json`] leaves it out and
    /// [`from_legacy_backend_json`] reads it back as false.
    #[serde(default)]
    #[clap(skip)]
    pub backend_supports_blocked_wait_signal_interruption: bool,

    /// The backend delivers the preemption timer `--max-timeslice` arms, so a
    /// thread that never makes a syscall still ends its slice. Under chaos, a
    /// `sched_yield` may then hand its turn to such a thread
    /// (<https://github.com/rrnewton/hermit/issues/4068>); without delivery the
    /// thread would never give it back. Off by default; the host sets it from
    /// the backend's name, because `reverie::BackendCapabilities` has no field
    /// for this fact yet. Like the field above it has no legacy key.
    #[serde(default)]
    #[clap(skip)]
    pub backend_delivers_timer_events: bool,

    /// The guest may start with a terminal: one of the launcher's standard
    /// descriptors is a terminal, the launcher has a controlling terminal that
    /// the guest inherits with its session, or the launcher could not tell.
    /// Such a terminal makes Linux send signals at moments the host sets:
    /// SIGHUP and SIGCONT when it hangs up, SIGINT, SIGQUIT and SIGTSTP for its
    /// interrupt, quit and suspend characters, SIGWINCH when it is resized, and
    /// SIGTTIN and SIGTTOU for a background process group's reads and writes.
    /// The guest makes no traced call that arms them, so where blocked waits
    /// decide interruption from the kernel's signal state
    /// ([`Config::backend_supports_blocked_wait_signal_interruption`]) the
    /// scheduler records all eight as host-timed for every process before the
    /// guest's first instruction, and no gated wait ends for them
    /// (<https://github.com/rrnewton/hermit/issues/3146>).
    ///
    /// The host sets it while it prepares the backend configuration. It has no
    /// legacy key: [`to_legacy_backend_json`] leaves it out and
    /// [`from_legacy_backend_json`] reads it back as false.
    #[serde(default)]
    #[clap(skip)]
    pub guest_may_inherit_a_terminal: bool,

    /// The forwarding policy (`detcore::detlog::ForwardPolicy::encode`) every
    /// in-guest Tool must forward its DETLOG records with, on the socket Hermit
    /// passed it (`hermit run --verify` under SaBRe or in-guest LiteInst);
    /// `None` when no socket is passed.
    ///
    /// The host sets it, not a flag, and every Tool image receives it in the
    /// configuration handshake on its coordinator connection, which guest code
    /// cannot change. A Tool image whose forwarder is missing, or forwards by
    /// another policy (guest code that ran first changed its private
    /// forwarding variables, so some records would be dropped before they are
    /// counted), then reports every forwarded-record count as UNCOUNTED
    /// (`detcore::detlog::require_forwarding`), so verification refuses the run
    /// instead of accepting a log without those records. Like
    /// `guest_may_inherit_a_terminal`, it has no legacy key.
    #[serde(default)]
    #[clap(skip)]
    pub in_guest_detlog_forward_policy: Option<String>,

    /// Whether every in-guest Tool image must run with syscall site patching
    /// off and collect its exit statistics (`hermit --backend=in-guest-trap`).
    ///
    /// The host sets it, not a flag, and every Tool image receives it in the
    /// configuration handshake on its coordinator connection, which guest code
    /// cannot change. The in-guest LiteInst runtime takes both settings from
    /// the guest environment, which guest code running before or during its
    /// installation could change, so once installation returns a Tool image
    /// compares the settings the runtime actually captured with this one and
    /// refuses to run the program when they differ
    /// (`detcore::in_guest_site_patching`). Like
    /// `in_guest_detlog_forward_policy`, it has no legacy key.
    #[serde(default)]
    #[clap(skip)]
    pub in_guest_site_patching_off: bool,

    /// The seccomp state the guest inherits from the thread that starts it
    /// ([`InheritedSeccomp`]). The caller that starts the guest states it, as
    /// only the caller knows the filters its launch installs, such as those of
    /// a `pre_exec` callback; `hermit run` measures it. Not stated, it is
    /// unknown. Nothing reads it. Whether a guest may start under an inherited
    /// filter is decided by the `hermit` command, before it starts the guest,
    /// from its own reading of `/proc/self/status` (`host_seccomp::admit`).
    /// It has no legacy key:
    /// [`to_legacy_backend_json`] leaves it out and
    /// [`from_legacy_backend_json`] reads it back as unknown.
    #[serde(default)]
    #[clap(skip)]
    pub inherited_seccomp: InheritedSeccomp,

    /// Epoch of the logical time.
    ///
    /// This is the datetime from which all time and date modtimes begin and
    /// monotonically increase. It is in RFC3339 format such as `2026-01-01T00:00:00Z`.
    /// The stable default here is for library callers and wire-format fixtures;
    /// the `hermit run` and `hermit oci run` commands replace an omitted CLI
    /// default with one host wall-clock sample taken before backend dispatch.
    #[clap(
        long,
        env = "HERMIT_EPOCH",
        value_name = "YYYY-MM-DDThh:mm:ssZ",
        default_value = DEFAULT_EPOCH_STR,
        hide_default_value = true
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
    /// Detcore validates every `/proc/*/mountinfo` view against this order.
    /// It does not number mount IDs by position in it; see
    /// `mount_id_assignment_order`. It is runtime provenance rather than a
    /// user option; replay retains recording-time raw IDs because its read
    /// events contain recording-time kernel bytes.
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

    /// Raw mount IDs in the order Detcore numbered them for the guest.
    ///
    /// Detcore numbers a raw mount ID when the guest first observes it through
    /// `/proc/*/mountinfo` or `/proc/*/fdinfo/*`; `mountinfo_mount_ids` only
    /// validates mountinfo views. A live run starts empty. Recording persists
    /// the completed order so replay gives every raw ID its recorded number
    /// rather than deriving one from its fresh namespace or launch descriptor
    /// shape.
    #[serde(default)]
    #[clap(skip)]
    pub mount_id_assignment_order: Vec<u64>,

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

    /// Target logical timeslice, in virtual nanoseconds, checked at syscall boundaries and the
    /// other points where the guest returns control to Detcore: signals, timer events, and
    /// trapped `rdtsc` and `cpuid` instructions unless `--target-timeslice-syscalls-only` is given.
    /// This avoids PMU preemption for workloads that enter the kernel frequently. Omit this option
    /// to use only `--max-timeslice`.
    #[serde(default)]
    #[clap(long, value_name = "virtual-nanoseconds")]
    pub target_timeslice: Option<NonZeroU64>,

    /// Do not end an expired `--target-timeslice` at a trapped `rdtsc` or `cpuid` while a PMU
    /// maximum (`--max-timeslice`) is armed; end it at the next syscall, signal or timer event,
    /// or at the PMU maximum. A trapped instruction can sit inside a user-space spinlock: QEMU's
    /// multi-threaded TCG reads the host TSC while holding its `vm_clock_lock`, and a vCPU that
    /// yields there leaves the other vCPUs spinning for whole PMU slices. Has no effect without
    /// `--target-timeslice`, or with `--max-timeslice disabled`, where the trap is the only way
    /// out of a busy-wait on the TSC.
    #[serde(default)]
    #[clap(long, requires = "target_timeslice")]
    pub target_timeslice_syscalls_only: bool,

    /// After a `FUTEX_WAKE` or `FUTEX_WAKE_BITSET` that woke at least one waiter, move the waker to
    /// the back of its priority band and have it yield, so that woken threads of its priority run
    /// before it continues. Without
    /// it, a thread that releases a contended lock keeps running until its slice ends and can
    /// re-take the lock many times before the waiter is scheduled: QEMU's multi-threaded TCG vCPUs
    /// starve its main loop of the big QEMU lock this way. The yield is a scheduler turn of its
    /// own: the waker's timeslice keeps its deadlines. Each extra thread switch is charged
    /// scheduler time that the guest can observe (see `--scheduler-turn-cost`), and costs wall
    /// time: a two-vCPU QEMU boot took 5% to 48% longer with it. The waker keeps its priority, so a
    /// woken thread of a lower priority still runs after it; `--chaos` redraws threads' priorities
    /// from their chaos PRNG, which can put a woken thread in a lower band; and a random
    /// `--sched-heuristic` does not select first in, first out. In those cases a woken thread is not
    /// guaranteed to run first. Applies to the precise futex mode while threads are sequentialized,
    /// on the ptrace backend (including e9patch preprocessing, which runs on it); replay applies it
    /// as the recorded run did.
    #[serde(default)]
    #[clap(long)]
    pub futex_wake_yields: bool,

    /// Virtual nanoseconds that each committed scheduler turn adds to the global clock, before
    /// `--clock-multiplier` (default 500000). The charge keeps time moving while threads take
    /// turns without running. Every thread observes it as a jump in time across a thread switch,
    /// so a guest that times short intervals while another thread runs, such as a QEMU guest
    /// kernel calibrating its TSC against the emulated PIT, can see a gap far larger than any
    /// work done. A smaller charge makes those jumps smaller; a finite poll or futex timeout then
    /// takes more turns to expire. The charge, after the multiplier, must be between 1 and
    /// 1000000000. If a charged turn would pass the end of representable virtual time, the run
    /// stops with HERMIT_VIRTUAL_CLOCK_EXHAUSTED and exit status 125.
    #[serde(default)]
    #[clap(long, value_name = "virtual-nanoseconds")]
    pub scheduler_turn_cost: Option<NonZeroU64>,

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

    /// External network record or replay, set by `hermit run --record-networking`
    /// or `--replay-networking`; `Off` otherwise, in which case Detcore never
    /// consults it. Its perturbation seed has no fallback to `seed` or
    /// `sched_seed`.
    #[serde(default)]
    #[clap(skip)]
    pub network_trace: NetworkTraceConfig,

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

    /// **Internal:** Set only by `hermit replay`, alongside `recordreplay_modes`. Replay serves
    /// socket reads, writes, accepts and connects, and `poll`, `ppoll`, `select` and
    /// `pselect6`, from the recording, so they cannot block. They still run as backgrounded
    /// blocking I/O, as during recording, but Detcore's scheduler commits no other turn until
    /// they finish. Otherwise the descriptor-table changes of different threads could
    /// interleave differently from one replay to the next.
    ///
    /// It never enters the legacy form ([`to_legacy_backend_json`]), so the DBT runtime's
    /// `HERMIT_DBT_DETCONFIG` in the guest's environment is unchanged by its introduction
    /// and reads it as false. Record and replay run only on the ptrace backend.
    #[serde(default)]
    #[clap(skip = false)]
    pub replaying: bool,

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

    /// Whether Detcore records the host identity of each file the guest
    /// opens, sending each record to the global state, which writes the run's
    /// records to [`Self::host_input_log`] when the run ends (see
    /// [`crate::host_input`]). `hermit run --verify` sets it, so that a
    /// divergence caused by a host file replaced during a run can be named.
    /// The records are never part of the compared log. Serialized, so that it
    /// reaches the Detcore tool on every backend, including one that runs
    /// inside the guest and receives this `Config` over RPC; never a CLI flag.
    #[serde(default)]
    #[clap(skip)]
    pub record_host_inputs: bool,

    /// Where the global state writes the run's host-input records, one
    /// [`crate::host_input::HostInputRecord`] JSON line each and a
    /// [`crate::host_input::HostInputLogEnd`] line, when the run ends.
    /// `hermit run --verify` sets a private file. A host path, read only by
    /// the global state, which every backend that hosts it builds from this
    /// `Config` in-process; so set programmatically, like `happens_before`,
    /// and never serialized.
    #[serde(skip)]
    #[clap(skip)]
    pub host_input_log: Option<PathBuf>,

    /// A guest address range, `[start, end)`, holding code whose syscalls
    /// the backend lets through without Detcore seeing them, which the guest
    /// cannot otherwise use without faulting. With
    /// [`Self::record_host_inputs`] set, a guest call that may change the
    /// memory there (a fixed mapping over it, a protection or advice change
    /// or unmapping that covers it, an open of a process-memory file for
    /// writing) is reported as a change at `/`, so no host input change is
    /// named in that run (see [`crate::host_input`]). The backend that hosts
    /// Detcore in-process sets it, so set programmatically, like
    /// `happens_before`, and never serialized.
    #[serde(skip)]
    #[clap(skip)]
    pub untraced_code_range: Option<(u64, u64)>,

    /// Opt-in capture of a core file for a guest thread killed by a signal
    /// whose default action dumps core; `None`, the default, writes nothing.
    /// A host path and host-side limits, read only by the global state at a
    /// fatal exit stop, so set programmatically, like `host_input_log`, and
    /// never serialized.
    #[serde(skip)]
    #[clap(skip)]
    pub fatal_core_capture: Option<FatalCoreCapture>,
}

/// Where and how much [`Config::fatal_core_capture`] may write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FatalCoreCapture {
    /// Directory receiving the compressed cores. Its regular files, whoever
    /// wrote them, count against `max_total_bytes`.
    pub dir: PathBuf,
    /// Prefix of every file name this run writes, so the run can find and
    /// remove its own cores.
    pub file_prefix: String,
    /// Most bytes one compressed core may occupy.
    pub max_core_bytes: u64,
    /// Most bytes all regular files in `dir` may occupy together.
    pub max_total_bytes: u64,
    /// Longest one capture may take before it gives up.
    pub time_limit: std::time::Duration,
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
    /// This configuration with `change` applied to its backend capabilities.
    /// [`BackendCapabilities`] cannot be built field by field outside Reverie,
    /// so a caller that needs to adjust individual facts starts from the value
    /// already here.
    pub fn with_backend(mut self, change: impl FnOnce(&mut BackendCapabilities)) -> Self {
        change(&mut self.backend);
        self
    }

    /// Whether the epoch is the stable library default omitted by `Display`.
    pub fn has_default_epoch(&self) -> bool {
        self.epoch == DEFAULT_EPOCH_STR.parse::<DateTime<Utc>>().unwrap()
    }

    /// Replace the stable library/test default with the host wall clock captured
    /// by the outer `hermit run` invocation. The caller owns the single
    /// host-clock read boundary; all guest clock and metadata observations
    /// consume the resulting concrete epoch.
    pub fn capture_epoch_from_host_time(&mut self, now: SystemTime) {
        self.epoch = epoch_from_host_time(now);
    }

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

    /// Whether a `FUTEX_WAKE` that woke `woken` waiters makes the waker yield
    /// to them (`--futex-wake-yields`).
    ///
    /// A thread that releases a contended lock usually keeps running: the
    /// waiter it woke is queued behind it, and the waker can re-take the lock,
    /// uncontended, many times before that waiter runs.
    /// glibc and Rust mutexes both let a releasing thread barge back in, and on
    /// real hardware the woken thread wins on another CPU within the release
    /// window. Under sequentialized threads it never gets one, so QEMU's
    /// multi-threaded TCG vCPUs starved its main loop of the big QEMU lock for
    /// whole PMU slices at a time
    /// (<https://github.com/rrnewton/hermit/issues/3874>). When this holds, the
    /// scheduler moves the waker to the back of its priority band, and the
    /// waker takes a yield turn at the syscall's post-hook, so woken threads
    /// of its priority run before it continues (it keeps its band; `--chaos`
    /// can redraw priorities, and a random heuristic does not select first
    /// in, first out). The yield leaves the waker's timeslice alone.
    ///
    /// A thread switch is not free in virtual time: every scheduler decision
    /// is charged, so a policy that switches more often moves time that the
    /// guest observes. A QEMU guest kernel that calibrates its TSC against the
    /// emulated PIT failed that calibration with this policy, because the
    /// main loop now ran in the middle of it.
    ///
    /// The decision reads only the configuration and the wake's result, the
    /// number of waiters the scheduler took off the futex in the waker's turn,
    /// which is already deterministic. A wake that woke no one keeps the
    /// slice. Replay applies the policy exactly as the recorded run did: a
    /// preemption record holds slice ends but not this queue order, so the
    /// requeue and the yield must happen again for the replayed run to reach
    /// the same schedule. Because the yield consumes no slice end and draws
    /// nothing from the thread's chaos PRNG, making it again keeps the replay
    /// aligned with its record.
    pub fn yields_after_futex_wake(&self, woken: u64) -> bool {
        self.futex_wake_yields && woken > 0
    }

    /// The virtual nanoseconds each committed scheduler turn adds to the
    /// global clock: `--scheduler-turn-cost` (default [`NANOS_PER_SCHED`])
    /// times `--clock-multiplier`, truncated as it always was. A configured
    /// cost must give a charge of at least one nanosecond, because the charge
    /// is what advances the clock through turns that run no guest code, such
    /// as the retries of a finite poll; a scaled charge of zero would leave
    /// such a timeout unable to expire. A configured cost, or its scaled
    /// charge, above [`MAX_SCHEDULER_TURN_COST`] is refused too, so that a
    /// configured charge never exceeds one second per turn.
    pub fn scheduler_turn_charge(&self) -> Result<u64, String> {
        let multiplier = self.clock_multiplier.unwrap_or(1.0);
        let Some(cost) = self.scheduler_turn_cost else {
            return Ok((NANOS_PER_SCHED * multiplier) as u64);
        };
        let cost = u64::from(cost);
        if cost > MAX_SCHEDULER_TURN_COST {
            return Err(format!(
                "--scheduler-turn-cost must be at most {MAX_SCHEDULER_TURN_COST} virtual \
                 nanoseconds (received {cost})"
            ));
        }
        let charge = cost as f64 * multiplier;
        if !(1.0..=MAX_SCHEDULER_TURN_COST as f64).contains(&charge) {
            return Err(format!(
                "--scheduler-turn-cost {cost} at --clock-multiplier {multiplier} charges {charge} \
                 virtual nanoseconds per scheduler turn; the charge must be at least 1, so that \
                 timed waits keep advancing, and at most {MAX_SCHEDULER_TURN_COST}"
            ));
        }
        Ok(charge as u64)
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
        if !self.has_default_epoch() {
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
        if self.target_timeslice_syscalls_only {
            write!(f, " --target-timeslice-syscalls-only")?;
        }
        if self.futex_wake_yields {
            write!(f, " --futex-wake-yields")?;
        }
        if let Some(cost) = self.scheduler_turn_cost {
            write!(f, " --scheduler-turn-cost={}", cost)?;
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

/// Convert one invocation's captured host instant without losing subsecond
/// precision. CLI and comparison orchestration share this input conversion;
/// guest clock progression never reads the host clock through it.
pub fn epoch_from_host_time(now: SystemTime) -> DateTime<Utc> {
    DateTime::<Utc>::from(now)
}

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

/// The largest `--scheduler-turn-cost` accepted, in virtual nanoseconds, both
/// before and after the clock multiplier scales it: one second per scheduler
/// turn.
pub const MAX_SCHEDULER_TURN_COST: u64 = 1_000_000_000;

/// N.B. we don't want to specify two different notions of "default", so we use the
/// `Clap` instance above.
/// Environment variable carrying the coordinator's [`config_wire_fingerprint`]
/// to an out-of-process plugin.
///
/// Named alongside the other `REVERIE_SABRE_HERMIT_*` launch variables so the
/// two travel together and a reader finds them in one place.
pub const CONFIG_FINGERPRINT_ENV: &str = "REVERIE_SABRE_HERMIT_CONFIG_FINGERPRINT";

/// Environment variable carrying the coordinator's [`config_wire_fingerprint`]
/// to the in-guest LiteInst runtime (`libdetcore_liteinst.so`), which checks it
/// before it decodes the configuration the coordinator sends. Without the
/// check, a runtime built from another tree fails with a bare decoding error
/// that names neither build (<https://github.com/rrnewton/hermit/issues/3986>).
/// The runtime removes it once checked, as it removes its DETLOG variables:
/// the value is zeroed in the environment block and the variable is unset, so
/// the fingerprint never reaches the guest.
pub const LITEINST_CONFIG_FINGERPRINT_ENV: &str = "HERMIT_LITEINST_CONFIG_FINGERPRINT";

const CONFIG_DEFINITION_SOURCES: &[&[u8]] = &[
    include_bytes!("config.rs"),
    // `RawInode`, the host identity the inode RPCs (`DeterminizeInode`,
    // `TouchFile`, `SetFileMtime`) carry. Its layout changed from one `u64`
    // to a device and an inode number, which neither encoding of `Config`
    // shows.
    include_bytes!("fd.rs"),
    include_bytes!("happens_before.rs"),
    include_bytes!("network_trace.rs"),
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
/// - the source files defining `Config`, its local serialized field types,
///   `DetTime`, and the `RawInode` the inode RPCs carry, which catch
///   wire-incompatible changes hidden by a default such as `Option<u64>::None`
///   to `Option<u32>::None`, or an added clock or inode field that leaves both
///   encodings of `Config` unchanged.
///
/// The source and JSON domains are deliberately stricter than the wire format
/// strictly requires. A documentation-only edit in one of these files can
/// require rebuilding the plugin; missing a wire-incompatible hidden variant
/// can make it decode the handshake or a subsequent request at the wrong offsets.
fn config_wire_default() -> Config {
    // `Config::default()` is intentionally environment-aware through Clap.
    // A coordinator may therefore inherit HERMIT_EPOCH/HERMIT_PRNG or
    // HERMIT_SCHED_SEED even though the separately loaded plugin receives a
    // minimal guest environment. Those invocation values are payload data,
    // not wire shape, and must not make two artifacts from the same source
    // reject one another. Explicit arguments take precedence over Clap's
    // environment provider; clear the scheduler override afterward to retain
    // the real environment-free default of `None` in the encoded shape.
    let mut config = Config::parse_from([
        "config-wire-fingerprint",
        &format!("--epoch={DEFAULT_EPOCH_STR}"),
        "--seed=0",
        "--sched-seed=0",
    ]);
    config.sched_seed = None;
    // `BackendCapabilities` is defined in Reverie, outside
    // `CONFIG_DEFINITION_SOURCES`, so the source domain cannot see a change to
    // it; only its encoding can. Every field is a bool, so any constant
    // carries every field's width (see
    // `config_fingerprint_sees_the_width_of_every_backend_capability`). KVM's
    // is kept, as it was when one field was optional and only KVM's set it.
    config.backend = BackendCapabilities::KVM;
    config
}

pub fn config_wire_fingerprint() -> String {
    let config = config_wire_default();
    let wire = bincode::serde::encode_to_vec(&config, bincode::config::legacy())
        .expect("canonical Config wire default must encode with Reverie's bincode configuration");
    let named_shape = serde_json::to_string(&config)
        .expect("canonical Config wire default must encode as JSON for field-name checking");
    fingerprint_of_config_material(&wire, &named_shape, CONFIG_DEFINITION_SOURCES)
}

/// What differs when a coordinator announced `expected` as its
/// [`config_wire_fingerprint`] and this build's is another, or `None` when
/// they are equal. The in-guest LiteInst runtime reports this before it
/// decodes the coordinator's configuration
/// (<https://github.com/rrnewton/hermit/issues/3986>).
pub fn config_fingerprint_mismatch(expected: &std::ffi::OsStr) -> Option<String> {
    let ours = config_wire_fingerprint();
    (expected.to_str() != Some(ours.as_str())).then(|| {
        format!(
            "its configuration and clock RPC fingerprint is {ours}, Hermit's is {}",
            expected.to_string_lossy()
        )
    })
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

/// Serializes `config` as JSON in the form it had before [`Config::backend`]
/// existed.
///
/// [`Config::backend`] is left out. In its place, at the same position in
/// the object, are the fifteen
/// separate backend keys that a serialized configuration carried before,
/// under the names and in the order it carried them, each with a value
/// computed from `config`. [`Config::record_host_inputs`], which the legacy
/// form never had, is left out too, and reads back as false: this form serves
/// only DBT, whose launcher collects no host inputs. So is
/// [`Config::backend_supports_blocked_wait_signal_interruption`] and
/// [`Config::backend_delivers_timer_events`], which are false for DBT, and
/// [`Config::guest_may_inherit_a_terminal`], which only
/// that capability reads, and [`Config::in_guest_detlog_forward_policy`] and
/// [`Config::in_guest_site_patching_off`], which are unset for DBT. So is [`Config::target_timeslice_syscalls_only`],
/// added after this form froze; DBT runs without a PMU maximum, where the
/// option has no effect. So are [`Config::futex_wake_yields`] and
/// [`Config::scheduler_turn_cost`], also added after this form froze, which
/// `hermit run` refuses with the DBT backend, and [`Config::replaying`], set
/// only by `hermit replay`, which runs only on the ptrace backend. So is
/// [`Config::inherited_seccomp`], added after this form froze, which reads
/// back as unknown. Every other field is serialized exactly as
/// `serde_json::to_string(config)` serializes it.
///
/// Use this wherever the JSON is visible to the guest. `hermit run
/// --backend=dbt` passes the configuration to the in-guest DBT runtime in the
/// guest's own environment, so a guest that reads its environment, or whose
/// initial stack layout depends on the environment's size, observes this
/// string; producing the earlier bytes keeps that observation unchanged.
///
/// Read the result with [`from_legacy_backend_json`], which recovers
/// [`Config::backend`] from the fifteen keys. A plain `Config`
/// deserialization ignores the keys and gives the field its serde default.
pub fn to_legacy_backend_json(config: &Config) -> serde_json::Result<String> {
    let mut json = Vec::with_capacity(4096);
    let mut serializer = serde_json::Serializer::new(&mut json);
    config.serialize(legacy_backend_json::ConfigSerializer {
        inner: &mut serializer,
        config,
    })?;
    String::from_utf8(json).map_err(serde::ser::Error::custom)
}

/// The JSON keys, in order, under which a serialized [`Config`] carried the
/// backend facts before they moved into [`Config::backend`], each with the
/// value it has for `config`.
///
/// `backend_is_kvm` named the backend rather than a fact; its value is
/// `provides_process_signal_control`, a capability only KVM reports.
/// `kvm_shared_dequeue_timers` was a startup policy, not a fact: the
/// production launcher, hermit-cli, set it exactly when that capability and
/// [`Config::sequentialize_threads`] held. Its value is that conjunction, and
/// [`from_legacy_backend_json`] refuses any other value.
fn legacy_backend_keys(config: &Config) -> [(&'static str, bool); 15] {
    let backend = &config.backend;
    [
        ("cpuid_virtualized_by_backend", backend.virtualizes_cpuid),
        ("backend_supports_madvise", backend.supports_madvise),
        (
            "discover_live_file_metadata",
            backend.tool_shares_guest_descriptor_table,
        ),
        (
            "detect_host_clock_futex_timeouts",
            backend.guest_clock_reads_bypass_backend,
        ),
        (
            "syscall_clobbers_virtualized_by_backend",
            backend.virtualizes_syscall_clobbers,
        ),
        (
            "cancel_killed_thread_rpcs",
            backend.needs_killed_thread_rpc_cancellation,
        ),
        (
            "backend_reports_physical_process_exits",
            backend.reports_physical_process_exits,
        ),
        (
            "backend_tracks_process_children",
            backend.tracks_process_children,
        ),
        (
            "backend_runs_exit_robust_list",
            backend.runs_exit_robust_list,
        ),
        (
            "backend_requires_thread_directed_process_signals",
            backend.requires_thread_directed_process_signals,
        ),
        ("backend_is_kvm", backend.provides_process_signal_control),
        (
            "kvm_shared_dequeue_timers",
            backend.provides_process_signal_control && config.sequentialize_threads,
        ),
        (
            "backend_supports_parked_write_signal_interruption",
            backend.supports_parked_write_signal_interruption,
        ),
        (
            "backend_virtualizes_capability_prctls",
            backend.virtualizes_capability_prctls,
        ),
        (
            "backend_defers_vfork_child_registration",
            backend.defers_vfork_child_registration,
        ),
    ]
}

/// Parses JSON written by [`to_legacy_backend_json`], or any JSON in the form
/// a serialized [`Config`] had before [`Config::backend`] existed, and recovers
/// [`Config::backend`] from the fifteen legacy backend keys.
///
/// The keys are read exactly as they were read while each was a `Config`
/// field: a key that is absent takes the default that field had, and a key of
/// the wrong type fails the parse. Each key then supplies every capability that
/// replaced a read of it, so a decoded configuration behaves as the same JSON
/// did before:
///
/// - `discover_live_file_metadata` supplies the four descriptor and loopback
///   facts that were each a read of it;
/// - `backend_reports_physical_process_exits` supplies
///   `reports_physical_process_exits` and
///   `signal_interrupts_external_syscalls`;
/// - `backend_is_kvm` supplies the two behaviours it selected that remain
///   capabilities, `provides_process_signal_control` and
///   `emulates_child_waits` (the other three are now answered by the backend
///   itself: refusing a non-leader exec is a typed refusal it reports per
///   exec, the `gettimeofday` repair runs on every backend through its
///   `time(2)` probe and `Guest::storable_memory_ranges`, and the user
///   address limit is each guest's own `Guest::user_address_limit` report);
/// - every other key supplies the one capability of the same meaning.
///
/// This inverts [`to_legacy_backend_json`] for every capability value the keys
/// can express, which includes every backend's own constant except for
/// capabilities added after the legacy form froze: those never enter it, as
/// `record_host_inputs` does not, and read back as their default. There are
/// two, `process_exits_complete_asynchronously` and
/// `virtualizes_guest_sigalrm`, both true only for in-guest LiteInst, which
/// never travels through this DBT-only form.
///
/// `kvm_shared_dequeue_timers` supplies no capability. It was read into a
/// `Config` field that chose the controlled signal path, and that choice is
/// now made from the capabilities, [`Config::sequentialize_threads`] and the
/// control the backend offers. It is read as it was, a boolean that is false
/// when absent, and the parse then fails unless it equals `backend_is_kvm &&
/// sequentialize_threads`, the value the production launcher always wrote. An
/// accepted input whose capabilities are those of the backend that runs it
/// selects the path it selected before. An input that would select another,
/// such as `true` without `backend_is_kvm`, or absent beside `backend_is_kvm`
/// and sequentialized threads, fails the parse rather than being silently
/// reinterpreted. (`backend_is_kvm` given to a backend that offers no control,
/// such as DBT, already misdescribed that backend and stays outside this
/// guarantee; the production launcher never writes it.) A failed parse is
/// handled like any malformed value: the DBT standalone JSON reader falls
/// back to its strict default configuration and emits its
/// `could not parse HERMIT_DBT_DETCONFIG` warning.
///
/// Everything else is read as the legacy form was read, by `Config`'s own
/// derived deserializer with the legacy keys taken out before it sees them:
///
/// - In an object, a key the legacy form did not name is ignored, whatever its
///   value. `backend` is such a key, so it is ignored rather than read as the
///   field it is now; no production writer emits it. A key the legacy form
///   named fails the parse when it appears twice or holds the wrong type.
/// - A JSON array lists the fields by position, as serde reads any derived
///   struct. The positions are those of the legacy form, which are the key
///   order of [`to_legacy_backend_json`]: the fifteen legacy keys stand where
///   [`Config::backend`] stands. Too many elements fail the parse; too few
///   leave the fields
///   after them missing, each taking its default or failing the parse as the
///   derived deserializer decides for that field.
pub fn from_legacy_backend_json(json: &str) -> serde_json::Result<Config> {
    let mut deserializer = serde_json::Deserializer::from_str(json);
    let (mut config, legacy) = serde::Deserializer::deserialize_struct(
        &mut deserializer,
        "Config",
        &[],
        legacy_backend_json::LegacyConfigVisitor,
    )?;
    deserializer.end()?;
    let controlled = legacy.backend_is_kvm && config.sequentialize_threads;
    if legacy.kvm_shared_dequeue_timers != controlled {
        return Err(<serde_json::Error as serde::de::Error>::custom(
            format_args!(
                "kvm_shared_dequeue_timers is {} but backend_is_kvm && \
             sequentialize_threads is {controlled}; the key is no longer an \
             independent setting and must equal that conjunction",
                legacy.kvm_shared_dequeue_timers,
            ),
        ));
    }
    config.backend = legacy.capabilities();
    Ok(config)
}

/// Preserve the exact backend facts alongside an already encoded legacy config.
///
/// The legacy form couples some capabilities and omits others. When it can
/// express `backend` exactly, return `legacy_json` byte for byte: DBT exposes
/// this string in the guest's environment. Otherwise wrap it in a versioned
/// envelope whose typed backend replaces the legacy projection on decoding.
/// Non-backend fields keep the legacy form's semantics in either case.
pub fn encode_backend_config_json(
    legacy_json: String,
    backend: BackendCapabilities,
) -> serde_json::Result<String> {
    if from_legacy_backend_json(&legacy_json)?.backend == backend {
        return Ok(legacy_json);
    }
    serde_json::to_string(&VersionedBackendConfig {
        dbt_config_version: 1,
        config: serde_json::from_str(&legacy_json)?,
        backend,
    })
}

/// Read a legacy configuration or a versioned envelope with exact backend facts.
///
/// Any valid legacy object or positional array retains
/// [`from_legacy_backend_json`]'s behavior, including ignored unknown fields.
/// Otherwise the input must be an envelope: unknown versions, duplicate fields
/// and missing typed capabilities are parse errors.
/// This parser does not choose the caller's error policy. The admitted DBT
/// standalone JSON path retains its strict-default parse fallback; coordinated
/// startup receives its authoritative typed Config through the RPC handshake.
pub fn decode_backend_config_json(json: &str) -> serde_json::Result<Config> {
    // Legacy inputs take precedence: even dbt_config_version was an ignored
    // unknown key in that form, and its presence must not change a valid run.
    if let Ok(config) = from_legacy_backend_json(json) {
        return Ok(config);
    }
    // Deserialize the original bytes, not a Value map that would erase
    // duplicate keys before the typed deserializers can reject them.
    let envelope: VersionedBackendConfig<'_> = serde_json::from_str(json)?;
    if envelope.dbt_config_version != 1 {
        return Err(<serde_json::Error as serde::de::Error>::custom(
            format_args!(
                "unsupported DBT configuration version {}",
                envelope.dbt_config_version
            ),
        ));
    }
    let mut config = from_legacy_backend_json(envelope.config.get())?;
    config.backend = envelope.backend;
    Ok(config)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionedBackendConfig<'a> {
    dbt_config_version: u32,
    #[serde(borrow)]
    config: &'a serde_json::value::RawValue,
    backend: BackendCapabilities,
}

/// The names of the fifteen legacy backend keys, in the order
/// [`legacy_backend_keys`] gives them.
const LEGACY_BACKEND_KEY_NAMES: [&str; 15] = [
    "cpuid_virtualized_by_backend",
    "backend_supports_madvise",
    "discover_live_file_metadata",
    "detect_host_clock_futex_timeouts",
    "syscall_clobbers_virtualized_by_backend",
    "cancel_killed_thread_rpcs",
    "backend_reports_physical_process_exits",
    "backend_tracks_process_children",
    "backend_runs_exit_robust_list",
    "backend_requires_thread_directed_process_signals",
    "backend_is_kvm",
    "kvm_shared_dequeue_timers",
    "backend_supports_parked_write_signal_interruption",
    "backend_virtualizes_capability_prctls",
    "backend_defers_vfork_child_registration",
];

const fn legacy_backend_key_default_true() -> bool {
    true
}

/// The fifteen legacy backend keys with the types and serde defaults they had
/// as `Config` fields. It is deserialized from the keys
/// [`legacy_backend_json::LegacyConfigVisitor`] takes out of the input, so a
/// key given twice fails here as it failed as a `Config` field.
#[derive(Deserialize)]
struct LegacyBackendKeys {
    #[serde(default)]
    cpuid_virtualized_by_backend: bool,
    #[serde(default = "legacy_backend_key_default_true")]
    backend_supports_madvise: bool,
    #[serde(default)]
    discover_live_file_metadata: bool,
    #[serde(default)]
    detect_host_clock_futex_timeouts: bool,
    #[serde(default)]
    syscall_clobbers_virtualized_by_backend: bool,
    #[serde(default)]
    cancel_killed_thread_rpcs: bool,
    #[serde(default)]
    backend_reports_physical_process_exits: bool,
    #[serde(default = "legacy_backend_key_default_true")]
    backend_tracks_process_children: bool,
    #[serde(default = "legacy_backend_key_default_true")]
    backend_runs_exit_robust_list: bool,
    #[serde(default)]
    backend_requires_thread_directed_process_signals: bool,
    #[serde(default)]
    backend_is_kvm: bool,
    /// Read as it was while it was a `Config` field, then checked by
    /// [`from_legacy_backend_json`] against `backend_is_kvm` and
    /// [`Config::sequentialize_threads`]; it supplies no capability.
    #[serde(default)]
    kvm_shared_dequeue_timers: bool,
    #[serde(default = "legacy_backend_key_default_true")]
    backend_supports_parked_write_signal_interruption: bool,
    #[serde(default)]
    backend_virtualizes_capability_prctls: bool,
    #[serde(default)]
    backend_defers_vfork_child_registration: bool,
}

impl LegacyBackendKeys {
    /// The capabilities these keys describe; the inverse of
    /// [`legacy_backend_keys`]. Every field is assigned, so the starting
    /// constant contributes nothing a key decides.
    fn capabilities(&self) -> BackendCapabilities {
        let mut backend = BackendCapabilities::PTRACE;
        backend.tool_shares_guest_descriptor_table = self.discover_live_file_metadata;
        backend.rediscovers_descriptors_after_exec = self.discover_live_file_metadata;
        backend.internal_pipe_turns_are_host_timed = self.discover_live_file_metadata;
        backend.loopback_pollers_yield_to_peers = self.discover_live_file_metadata;
        backend.guest_clock_reads_bypass_backend = self.detect_host_clock_futex_timeouts;
        backend.virtualizes_syscall_clobbers = self.syscall_clobbers_virtualized_by_backend;
        backend.needs_killed_thread_rpc_cancellation = self.cancel_killed_thread_rpcs;
        // A legacy encoding predates this key. Only the ptrace tracer reports
        // child-exit publication, and it is the one backend that needs no
        // killed-thread RPC cancellation.
        backend.reports_child_exit_publication = !self.cancel_killed_thread_rpcs;
        backend.reports_physical_process_exits = self.backend_reports_physical_process_exits;
        backend.signal_interrupts_external_syscalls = self.backend_reports_physical_process_exits;
        backend.tracks_process_children = self.backend_tracks_process_children;
        backend.runs_exit_robust_list = self.backend_runs_exit_robust_list;
        backend.requires_thread_directed_process_signals =
            self.backend_requires_thread_directed_process_signals;
        backend.supports_parked_write_signal_interruption =
            self.backend_supports_parked_write_signal_interruption;
        backend.defers_vfork_child_registration = self.backend_defers_vfork_child_registration;
        backend.virtualizes_capability_prctls = self.backend_virtualizes_capability_prctls;
        backend.virtualizes_cpuid = self.cpuid_virtualized_by_backend;
        backend.supports_madvise = self.backend_supports_madvise;
        // A legacy encoding predates this key: every backend that supports
        // madvise supports MADV_DONTNEED, and KVM implements it alone.
        backend.supports_madv_dontneed = self.backend_supports_madvise || self.backend_is_kvm;
        backend.provides_process_signal_control = self.backend_is_kvm;
        backend.emulates_child_waits = self.backend_is_kvm;
        backend
    }
}

/// The serde adapters behind [`to_legacy_backend_json`] and
/// [`from_legacy_backend_json`].
///
/// The serializer adapter wraps the JSON serializer only at the top level,
/// where `Config`'s derived `Serialize` opens one struct and writes its fields
/// in declaration order. Nested values go straight to the wrapped serializer,
/// so they, and every field other than the two it replaces, are encoded
/// unchanged.
///
/// The deserializer adapters likewise wrap only the top-level object or array,
/// and hand every nested value to the wrapped JSON deserializer.
mod legacy_backend_json {
    use std::fmt;

    use serde::Deserialize;
    use serde::Serialize;
    use serde::de;
    use serde::de::DeserializeSeed;
    use serde::de::IgnoredAny;
    use serde::de::MapAccess;
    use serde::de::SeqAccess;
    use serde::de::Visitor;
    use serde::de::value::BoolDeserializer;
    use serde::de::value::MapAccessDeserializer;
    use serde::de::value::MapDeserializer;
    use serde::de::value::SeqAccessDeserializer;
    use serde::de::value::StringDeserializer;
    use serde::ser::Error as _;
    use serde::ser::Impossible;
    use serde::ser::SerializeStruct;
    use serde::ser::Serializer;

    use super::BackendCapabilities;
    use super::Config;
    use super::LEGACY_BACKEND_KEY_NAMES;
    use super::LegacyBackendKeys;
    use super::legacy_backend_keys;

    /// The keys `Config` has now that its legacy form did not. The legacy
    /// form ignored them, as it ignored every key it did not name.
    /// `record_host_inputs` has no legacy key and no legacy value: the legacy
    /// form serves only DBT, whose launcher collects no host inputs, so it is
    /// never written and always reads as false.
    /// `backend_supports_blocked_wait_signal_interruption` and
    /// `backend_delivers_timer_events` are the same: they are false for DBT,
    /// so they are never written and always read as false.
    /// So is `guest_may_inherit_a_terminal`, which only that capability reads.
    /// `target_timeslice_syscalls_only` came after the legacy form froze; the
    /// legacy form never carried it, so it is never written and reads as false,
    /// the behaviour every legacy reader already has.
    /// `in_guest_detlog_forward_policy` is unset for DBT (its runtime forwards
    /// no records on a socket), so it is never written and reads as unset.
    /// `futex_wake_yields` and `scheduler_turn_cost` came after the legacy
    /// form froze too, and `hermit run` refuses them with the DBT backend.
    /// `replaying` is never written either: only `hermit replay` sets it, and
    /// replay runs only on the ptrace backend.
    /// `in_guest_site_patching_off` is false for DBT (it is set only for
    /// in-guest LiteInst), so it is never written and reads as false.
    /// `inherited_seccomp` came after the legacy form froze too. It is never
    /// written and reads as unknown.
    const FIELDS_WITHOUT_A_LEGACY_KEY: [&str; 12] = [
        "backend",
        "record_host_inputs",
        "backend_supports_blocked_wait_signal_interruption",
        "backend_delivers_timer_events",
        "guest_may_inherit_a_terminal",
        "in_guest_detlog_forward_policy",
        "in_guest_site_patching_off",
        "target_timeslice_syscalls_only",
        "futex_wake_yields",
        "replaying",
        "scheduler_turn_cost",
        "inherited_seccomp",
    ];

    /// Reads the top-level object or array of a legacy configuration. The
    /// derived `Config` deserializer reads every field; this takes the legacy
    /// backend keys out of the input before it sees them and collects them for
    /// [`LegacyBackendKeys`].
    pub(super) struct LegacyConfigVisitor;

    impl<'de> Visitor<'de> for LegacyConfigVisitor {
        type Value = (Config, LegacyBackendKeys);

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("struct Config")
        }

        fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
            let mut legacy = Vec::new();
            let config = Config::deserialize(MapAccessDeserializer::new(LegacyKeys {
                inner: map,
                legacy: &mut legacy,
            }))?;
            Ok((config, legacy_backend_keys_from(legacy)?))
        }

        fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
            let fields = config_field_order().map_err(<A::Error as de::Error>::custom)?;
            let mut legacy = Vec::new();
            let config = Config::deserialize(SeqAccessDeserializer::new(LegacyPositions {
                inner: seq,
                fields: fields.iter(),
                legacy: &mut legacy,
            }))?;
            Ok((config, legacy_backend_keys_from(legacy)?))
        }
    }

    /// Reads the collected legacy keys with their old types and defaults. A
    /// key that appeared twice is collected twice, and fails here as a
    /// duplicate field.
    fn legacy_backend_keys_from<E: de::Error>(
        legacy: Vec<(&'static str, bool)>,
    ) -> Result<LegacyBackendKeys, E> {
        LegacyBackendKeys::deserialize(MapDeserializer::<_, E>::new(legacy.into_iter()))
    }

    /// The top-level object's entries, with each legacy backend key read as the
    /// boolean it was and collected rather than passed on, and each key of
    /// [`FIELDS_WITHOUT_A_LEGACY_KEY`] skipped as the unknown key it was.
    struct LegacyKeys<'l, A> {
        inner: A,
        legacy: &'l mut Vec<(&'static str, bool)>,
    }

    impl<'de, A: MapAccess<'de>> MapAccess<'de> for LegacyKeys<'_, A> {
        type Error = A::Error;

        fn next_key_seed<K: DeserializeSeed<'de>>(
            &mut self,
            seed: K,
        ) -> Result<Option<K::Value>, A::Error> {
            while let Some(key) = self.inner.next_key::<String>()? {
                if let Some(name) = LEGACY_BACKEND_KEY_NAMES
                    .into_iter()
                    .find(|name| *name == key)
                {
                    let value = self.inner.next_value::<bool>()?;
                    self.legacy.push((name, value));
                } else if FIELDS_WITHOUT_A_LEGACY_KEY.contains(&key.as_str()) {
                    self.inner.next_value::<IgnoredAny>()?;
                } else {
                    return seed.deserialize(StringDeserializer::new(key)).map(Some);
                }
            }
            Ok(None)
        }

        fn next_value_seed<V: DeserializeSeed<'de>>(
            &mut self,
            seed: V,
        ) -> Result<V::Value, A::Error> {
            self.inner.next_value_seed(seed)
        }
    }

    /// The top-level array's elements, in the legacy form's positions. The
    /// derived `Config` deserializer asks for one element per field in
    /// declaration order; at [`Config::backend`] this reads up to fifteen
    /// elements as the legacy backend keys, and neither the field it asks for
    /// there nor [`Config::record_host_inputs`],
    /// [`Config::backend_supports_blocked_wait_signal_interruption`],
    /// [`Config::backend_delivers_timer_events`],
    /// [`Config::guest_may_inherit_a_terminal`],
    /// [`Config::in_guest_detlog_forward_policy`],
    /// [`Config::in_guest_site_patching_off`],
    /// [`Config::target_timeslice_syscalls_only`],
    /// [`Config::futex_wake_yields`], [`Config::replaying`],
    /// [`Config::scheduler_turn_cost`] or [`Config::inherited_seccomp`] takes
    /// an element. Each gets a
    /// placeholder; [`super::from_legacy_backend_json`] replaces the first.
    struct LegacyPositions<'f, 'l, A> {
        inner: A,
        fields: std::slice::Iter<'f, String>,
        legacy: &'l mut Vec<(&'static str, bool)>,
    }

    impl<'de, A: SeqAccess<'de>> SeqAccess<'de> for LegacyPositions<'_, '_, A> {
        type Error = A::Error;

        fn next_element_seed<T: DeserializeSeed<'de>>(
            &mut self,
            seed: T,
        ) -> Result<Option<T::Value>, A::Error> {
            match self.fields.next().map(String::as_str) {
                Some("backend") => {
                    for name in LEGACY_BACKEND_KEY_NAMES {
                        match self.inner.next_element::<bool>()? {
                            Some(value) => self.legacy.push((name, value)),
                            // A short array; the rest take their defaults.
                            None => break,
                        }
                    }
                    let placeholder = serde_json::to_value(BackendCapabilities::PTRACE)
                        .map_err(<A::Error as de::Error>::custom)?;
                    seed.deserialize(placeholder)
                        .map(Some)
                        .map_err(<A::Error as de::Error>::custom)
                }
                Some(
                    "record_host_inputs"
                    | "backend_supports_blocked_wait_signal_interruption"
                    | "backend_delivers_timer_events"
                    | "guest_may_inherit_a_terminal"
                    | "in_guest_site_patching_off"
                    | "target_timeslice_syscalls_only"
                    | "futex_wake_yields"
                    | "replaying",
                ) => seed.deserialize(BoolDeserializer::new(false)).map(Some),
                // Unset, as its serde default is.
                Some("in_guest_detlog_forward_policy" | "scheduler_turn_cost") => seed
                    .deserialize(serde_json::Value::Null)
                    .map(Some)
                    .map_err(<A::Error as de::Error>::custom),
                // Unknown, as its serde default is.
                Some("inherited_seccomp") => {
                    let unknown = serde_json::to_value(super::InheritedSeccomp::Unknown)
                        .map_err(<A::Error as de::Error>::custom)?;
                    seed.deserialize(unknown)
                        .map(Some)
                        .map_err(<A::Error as de::Error>::custom)
                }
                _ => self.inner.next_element_seed(seed),
            }
        }
    }

    /// The names of `Config`'s serialized fields in declaration order, which
    /// is the order its derived deserializer reads an array in: no field is
    /// serialized without being deserialized or the reverse.
    ///
    /// The configuration serialized to learn them is built by Clap from no
    /// arguments, with every environment binding removed before parsing.
    /// Building `Config::command()` still reads the bound variables (Clap's
    /// `Arg::env` reads a variable's value when the binding is made), but
    /// removing the bindings discards those values, so no ambient value
    /// reaches the parse. `Config::default()` would instead parse
    /// HERMIT_EPOCH, HERMIT_PRNG and HERMIT_SCHED_SEED, and exit the process
    /// when one of them does not parse; the legacy decoder was not affected
    /// by any of them.
    fn config_field_order() -> serde_json::Result<Vec<String>> {
        let config = environment_free_config().map_err(<serde_json::Error as de::Error>::custom)?;
        let json = serde_json::to_string(&config)?;
        Ok(serde_json::from_str::<KeyOrder>(&json)?.0)
    }

    /// `Config` as Clap builds it from no arguments and no environment.
    fn environment_free_config() -> Result<Config, clap::Error> {
        use clap::CommandFactory;
        use clap::FromArgMatches;

        let matches = Config::command()
            .mut_args(|arg| arg.env(None::<&'static str>))
            .try_get_matches_from(["config-field-order"])?;
        Config::from_arg_matches(&matches)
    }

    /// The keys of a JSON object, in the order they appear.
    pub(super) struct KeyOrder(pub(super) Vec<String>);

    impl<'de> Deserialize<'de> for KeyOrder {
        fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct KeyOrderVisitor;

            impl<'de> Visitor<'de> for KeyOrderVisitor {
                type Value = KeyOrder;

                fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                    formatter.write_str("a JSON object")
                }

                fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<KeyOrder, A::Error> {
                    let mut keys = Vec::new();
                    while let Some(key) = map.next_key::<String>()? {
                        map.next_value::<IgnoredAny>()?;
                        keys.push(key);
                    }
                    Ok(KeyOrder(keys))
                }
            }

            deserializer.deserialize_map(KeyOrderVisitor)
        }
    }

    pub(super) struct ConfigSerializer<'c, S> {
        pub(super) inner: S,
        pub(super) config: &'c Config,
    }

    pub(super) struct ConfigFields<'c, S> {
        inner: S,
        config: &'c Config,
        wrote_backend_keys: bool,
    }

    fn not_a_config<E: serde::ser::Error>() -> E {
        E::custom("the legacy backend JSON encoding applies only to a Detcore Config")
    }

    impl<'c, S: Serializer> Serializer for ConfigSerializer<'c, S> {
        type Ok = S::Ok;
        type Error = S::Error;
        type SerializeSeq = Impossible<S::Ok, S::Error>;
        type SerializeTuple = Impossible<S::Ok, S::Error>;
        type SerializeTupleStruct = Impossible<S::Ok, S::Error>;
        type SerializeTupleVariant = Impossible<S::Ok, S::Error>;
        type SerializeMap = Impossible<S::Ok, S::Error>;
        type SerializeStruct = ConfigFields<'c, S::SerializeStruct>;
        type SerializeStructVariant = Impossible<S::Ok, S::Error>;

        fn is_human_readable(&self) -> bool {
            self.inner.is_human_readable()
        }

        fn serialize_struct(
            self,
            name: &'static str,
            len: usize,
        ) -> Result<Self::SerializeStruct, S::Error> {
            // Two fields are replaced by fifteen keys.
            let len = len + legacy_backend_keys(self.config).len() - 2;
            Ok(ConfigFields {
                inner: self.inner.serialize_struct(name, len)?,
                config: self.config,
                wrote_backend_keys: false,
            })
        }

        fn serialize_bool(self, _: bool) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_i8(self, _: i8) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_i16(self, _: i16) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_i32(self, _: i32) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_i64(self, _: i64) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_u8(self, _: u8) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_u16(self, _: u16) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_u32(self, _: u32) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_u64(self, _: u64) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_f32(self, _: f32) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_f64(self, _: f64) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_char(self, _: char) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_str(self, _: &str) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_bytes(self, _: &[u8]) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_none(self) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_some<T: ?Sized + Serialize>(self, _: &T) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_unit(self) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_unit_struct(self, _: &'static str) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_unit_variant(
            self,
            _: &'static str,
            _: u32,
            _: &'static str,
        ) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_newtype_struct<T: ?Sized + Serialize>(
            self,
            _: &'static str,
            _: &T,
        ) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_newtype_variant<T: ?Sized + Serialize>(
            self,
            _: &'static str,
            _: u32,
            _: &'static str,
            _: &T,
        ) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_seq(self, _: Option<usize>) -> Result<Self::SerializeSeq, S::Error> {
            Err(not_a_config())
        }
        fn serialize_tuple(self, _: usize) -> Result<Self::SerializeTuple, S::Error> {
            Err(not_a_config())
        }
        fn serialize_tuple_struct(
            self,
            _: &'static str,
            _: usize,
        ) -> Result<Self::SerializeTupleStruct, S::Error> {
            Err(not_a_config())
        }
        fn serialize_tuple_variant(
            self,
            _: &'static str,
            _: u32,
            _: &'static str,
            _: usize,
        ) -> Result<Self::SerializeTupleVariant, S::Error> {
            Err(not_a_config())
        }
        fn serialize_map(self, _: Option<usize>) -> Result<Self::SerializeMap, S::Error> {
            Err(not_a_config())
        }
        fn serialize_struct_variant(
            self,
            _: &'static str,
            _: u32,
            _: &'static str,
            _: usize,
        ) -> Result<Self::SerializeStructVariant, S::Error> {
            Err(not_a_config())
        }
    }

    impl<S: SerializeStruct> SerializeStruct for ConfigFields<'_, S> {
        type Ok = S::Ok;
        type Error = S::Error;

        fn serialize_field<T: ?Sized + Serialize>(
            &mut self,
            key: &'static str,
            value: &T,
        ) -> Result<(), S::Error> {
            match key {
                "backend" => {
                    for (legacy_key, legacy_value) in legacy_backend_keys(self.config) {
                        self.inner.serialize_field(legacy_key, &legacy_value)?;
                    }
                    self.wrote_backend_keys = true;
                    Ok(())
                }
                // No legacy key; see FIELDS_WITHOUT_A_LEGACY_KEY.
                "record_host_inputs"
                | "backend_supports_blocked_wait_signal_interruption"
                | "backend_delivers_timer_events"
                | "guest_may_inherit_a_terminal"
                | "in_guest_detlog_forward_policy"
                | "in_guest_site_patching_off"
                | "target_timeslice_syscalls_only"
                | "futex_wake_yields"
                | "replaying"
                | "scheduler_turn_cost"
                | "inherited_seccomp" => Ok(()),
                _ => self.inner.serialize_field(key, value),
            }
        }

        fn skip_field(&mut self, key: &'static str) -> Result<(), S::Error> {
            self.inner.skip_field(key)
        }

        fn end(self) -> Result<S::Ok, S::Error> {
            if !self.wrote_backend_keys {
                return Err(S::Error::custom(
                    "Config serialized no `backend` field to replace with the legacy backend keys",
                ));
            }
            self.inner.end()
        }
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
    fn resolved_epoch_is_serialized_as_an_exact_config_input() {
        let epoch = "2000-12-31T23:59:59.123456789Z"
            .parse::<DateTime<Utc>>()
            .unwrap();
        let config = Config {
            epoch,
            ..Config::default()
        };
        let encoded = serde_json::to_value(&config).unwrap();
        assert_eq!(
            encoded["epoch"],
            serde_json::json!("2000-12-31T23:59:59.123456789Z")
        );
        let decoded: Config = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded.epoch, epoch);
    }

    #[test]
    fn default_backend_capabilities_match_instrumented_backends() {
        let config = Config::default();
        assert_eq!(config.backend, BackendCapabilities::PTRACE);
        assert!(!config.backend.reports_physical_process_exits);
        assert!(config.backend.tracks_process_children);
        assert!(config.backend.runs_exit_robust_list);
        assert!(!config.backend.requires_thread_directed_process_signals);
        assert!(config.backend.supports_parked_write_signal_interruption);
        assert!(!config.backend.virtualizes_capability_prctls);
        assert!(!config.backend.defers_vfork_child_registration);
        assert!(!config.sequentialize_threads);
        assert!(
            legacy_backend_keys(&config).contains(&("kvm_shared_dequeue_timers", false)),
            "the default configuration must not write a controlled-run legacy key"
        );
        assert!(!config.backend_supports_blocked_wait_signal_interruption);
        assert!(!config.backend_delivers_timer_events);
        assert!(!config.guest_may_inherit_a_terminal);
        assert!(config.in_guest_detlog_forward_policy.is_none());
        assert!(!config.in_guest_site_patching_off);
    }

    #[test]
    fn a_config_without_backend_capabilities_deserializes_as_ptrace() {
        let mut encoded = serde_json::to_value(Config::default()).unwrap();
        let fields = encoded.as_object_mut().unwrap();
        assert!(fields.remove("backend").is_some());
        assert!(!fields.contains_key("shared_dequeue_timers"));
        let decoded: Config = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded.backend, BackendCapabilities::PTRACE);
    }

    /// `Config`'s own derived deserializer ignores a key it does not name, and
    /// `shared_dequeue_timers` is no longer one it names, so JSON saved while
    /// it was a field still reads and the key decides nothing, whatever it
    /// holds and however often it appears. This is the whole compatibility
    /// policy for the named form, and it is safe because no production code
    /// reads a `Config` by field name: the guest-visible DBT form is read by
    /// [`from_legacy_backend_json`], which checks the key, and the bincode form
    /// that crosses process boundaries is positional. Removing the field
    /// changed that positional form and [`config_wire_fingerprint`]. SaBRe's
    /// plugin compares the fingerprint and refuses a coordinator from another
    /// build; LiteInst's in-guest runtime does not, so a runtime library built
    /// before the removal and loaded by a newer coordinator can misdecode the
    /// Config and fail its initialization (exit 127). That mixed-build hazard
    /// is not new: every change to the positional form has it.
    #[test]
    fn a_saved_shared_dequeue_timers_field_is_ignored() {
        let baseline = serde_json::to_string(&Config::default()).unwrap();
        for extra in [
            r#""shared_dequeue_timers":false"#,
            r#""shared_dequeue_timers":true"#,
            r#""shared_dequeue_timers":"yes""#,
            r#""shared_dequeue_timers":true,"shared_dequeue_timers":false"#,
        ] {
            let edited = baseline.replacen('{', &format!("{{{extra},"), 1);
            let decoded: Config = serde_json::from_str(&edited).unwrap();
            assert_eq!(
                serde_json::to_string(&decoded).unwrap(),
                baseline,
                "{edited}"
            );
        }
    }

    /// Ptrace's capabilities with the robust-list transition and parked-write
    /// signal interruption both left to Detcore: the value the retired
    /// ptrace-hosted LiteInst runtime reported. No backend reports it now, but
    /// the legacy keys can still express it, so the round trips keep it.
    fn ptrace_without_exit_transitions() -> BackendCapabilities {
        let mut backend = BackendCapabilities::PTRACE;
        backend.runs_exit_robust_list = false;
        backend.supports_parked_write_signal_interruption = false;
        backend
    }

    /// `backend` as the legacy form can carry it: capabilities added after the
    /// form froze never enter it and read back as their default (see
    /// [`process_exits_complete_asynchronously_never_enters_the_legacy_form`]).
    fn legacy_expressible(mut backend: BackendCapabilities) -> BackendCapabilities {
        backend.process_exits_complete_asynchronously = false;
        backend.virtualizes_guest_sigalrm = false;
        // The legacy discover_live_file_metadata bit couples four facts.
        backend.rediscovers_descriptors_after_exec = backend.tool_shares_guest_descriptor_table;
        backend.internal_pipe_turns_are_host_timed = backend.tool_shares_guest_descriptor_table;
        backend.loopback_pollers_yield_to_peers = backend.tool_shares_guest_descriptor_table;
        backend
    }

    #[test]
    fn legacy_backend_json_round_trips_every_backend_capabilities_constant() {
        for (name, backend) in [
            ("PTRACE", BackendCapabilities::PTRACE),
            ("E9PATCH", BackendCapabilities::E9PATCH),
            (
                "ptrace without exit transitions",
                ptrace_without_exit_transitions(),
            ),
            ("LITEINST_IN_GUEST", BackendCapabilities::LITEINST_IN_GUEST),
            ("SABRE", BackendCapabilities::SABRE),
            ("DBT", BackendCapabilities::DBT),
            ("KVM", BackendCapabilities::KVM),
        ] {
            for sequentialize_threads in [false, true] {
                let sent = Config {
                    backend,
                    sequentialize_threads,
                    ..Config::default()
                };
                let json = to_legacy_backend_json(&sent).unwrap();
                let received = from_legacy_backend_json(&json).unwrap();
                assert_eq!(received.backend, legacy_expressible(backend), "{name}");
                assert_eq!(
                    received.sequentialize_threads, sequentialize_threads,
                    "{name}"
                );
                assert_controlled_key(&json, &backend, sequentialize_threads, name);
                let expressible = Config {
                    backend: legacy_expressible(backend),
                    ..sent.clone()
                };
                assert_eq!(
                    serde_json::to_string(&received).unwrap(),
                    serde_json::to_string(&expressible).unwrap(),
                    "{name}"
                );
                assert_eq!(to_legacy_backend_json(&received).unwrap(), json, "{name}");
                assert_controlled_key_is_checked(&json, &sent, name);
            }
        }
    }

    #[test]
    fn backend_config_json_preserves_every_backend_and_representable_bytes() {
        for (name, backend, needs_envelope) in [
            ("PTRACE", BackendCapabilities::PTRACE, false),
            ("E9PATCH", BackendCapabilities::E9PATCH, false),
            (
                "LITEINST_IN_GUEST",
                BackendCapabilities::LITEINST_IN_GUEST,
                true,
            ),
            ("SABRE", BackendCapabilities::SABRE, false),
            ("DBT", BackendCapabilities::DBT, true),
            ("KVM", BackendCapabilities::KVM, false),
        ] {
            for sequentialize_threads in [false, true] {
                let sent = Config {
                    backend,
                    sequentialize_threads,
                    ..Config::default()
                };
                let legacy = to_legacy_backend_json(&sent).unwrap();
                let encoded = encode_backend_config_json(legacy.clone(), backend).unwrap();
                assert_eq!(encoded != legacy, needs_envelope, "{name}");
                assert_eq!(
                    from_legacy_backend_json(&encoded).is_err(),
                    needs_envelope,
                    "{name}: an envelope must not be accepted as a legacy config"
                );
                let received = decode_backend_config_json(&encoded).unwrap();
                assert_eq!(received.backend, backend, "{name}");
                assert_eq!(
                    serde_json::to_string(&received).unwrap(),
                    serde_json::to_string(&sent).unwrap(),
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn backend_config_json_round_trips_each_independent_capability() {
        let baseline = serde_json::to_value(BackendCapabilities::DBT).unwrap();
        for (name, value) in baseline.as_object().unwrap() {
            let mut edited = baseline.clone();
            edited[name] = serde_json::Value::Bool(!value.as_bool().unwrap());
            let backend = serde_json::from_value(edited).unwrap();
            let sent = Config {
                backend,
                ..Config::default()
            };
            let legacy = to_legacy_backend_json(&sent).unwrap();
            let encoded = encode_backend_config_json(legacy, backend).unwrap();
            let received = decode_backend_config_json(&encoded).unwrap();
            assert_eq!(received.backend, backend, "{name}");
        }
    }

    #[test]
    fn backend_config_json_requires_each_typed_capability_once() {
        let sent = Config {
            backend: BackendCapabilities::LITEINST_IN_GUEST,
            ..Config::default()
        };
        let encoded =
            encode_backend_config_json(to_legacy_backend_json(&sent).unwrap(), sent.backend)
                .unwrap();
        let envelope: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        for (name, value) in envelope["backend"].as_object().unwrap() {
            let mut missing = envelope.clone();
            missing["backend"].as_object_mut().unwrap().remove(name);
            assert!(
                decode_backend_config_json(&missing.to_string()).is_err(),
                "missing {name}"
            );
            let backend = serde_json::to_string(&sent.backend).unwrap();
            let duplicate = backend.replacen('{', &format!("{{\"{name}\":{value},"), 1);
            let duplicate = encoded.replace(&backend, &duplicate);
            assert_ne!(duplicate, encoded);
            assert!(
                decode_backend_config_json(&duplicate).is_err(),
                "duplicate {name}"
            );
        }
    }

    #[test]
    fn backend_config_json_keeps_legacy_object_and_array_semantics() {
        let legacy = to_legacy_backend_json(&Config::default()).unwrap();
        let unknown_duplicates = legacy.replacen(
            '{',
            r#"{"unrecognized":1,"unrecognized":null,"backend":null,"backend":true,"dbt_config_version":null,"dbt_config_version":2,"#,
            1,
        );
        let known_duplicate = legacy.replacen('{', r#"{"seed":1,"#, 1);
        let mistyped = legacy.replacen("\"backend_is_kvm\":false", "\"backend_is_kvm\":null", 1);
        assert_ne!(mistyped, legacy);
        for json in [
            legacy.clone(),
            positional(&legacy),
            unknown_duplicates,
            known_duplicate,
            mistyped,
            format!("{legacy} trailing"),
            "[]".to_owned(),
        ] {
            let old = from_legacy_backend_json(&json);
            let new = decode_backend_config_json(&json);
            match (old, new) {
                (Ok(old), Ok(new)) => assert_eq!(
                    serde_json::to_string(&old).unwrap(),
                    serde_json::to_string(&new).unwrap()
                ),
                (Err(_), Err(_)) => {}
                _ => panic!("legacy parse outcome changed for {json}"),
            }
        }
    }

    /// `kvm_shared_dequeue_timers` is read independently of the other keys,
    /// absent, false or true, and the parse succeeds exactly when the value
    /// read (false when absent) equals `backend_is_kvm &&
    /// sequentialize_threads`. Under that condition the accepted value is the
    /// one the decoded capabilities and `sequentialize_threads` imply, so the
    /// decoded configuration is the one sent; otherwise the error names the
    /// key.
    fn assert_controlled_key_is_checked(json: &str, sent: &Config, name: &str) {
        let controlled = sent.backend.provides_process_signal_control && sent.sequentialize_threads;
        let written = format!("\"kvm_shared_dequeue_timers\":{controlled}");
        assert_eq!(json.matches(&written).count(), 1, "{name}");
        for value in [None, Some(false), Some(true)] {
            let edited = match value {
                None => json.replacen(&format!(",{written}"), "", 1),
                Some(value) => json.replacen(
                    &written,
                    &format!("\"kvm_shared_dequeue_timers\":{value}"),
                    1,
                ),
            };
            assert_eq!(
                edited.contains("kvm_shared_dequeue_timers"),
                value.is_some(),
                "{name} {value:?}"
            );
            let case = format!(
                "{name} sequentialize_threads={} key={value:?}",
                sent.sequentialize_threads
            );
            match from_legacy_backend_json(&edited) {
                Ok(received) => {
                    assert_eq!(value.unwrap_or(false), controlled, "{case} was accepted");
                    // Compared with what the legacy form can carry; see
                    // `legacy_expressible`.
                    let expressible = Config {
                        backend: legacy_expressible(sent.backend),
                        ..sent.clone()
                    };
                    assert_eq!(
                        serde_json::to_string(&received).unwrap(),
                        serde_json::to_string(&expressible).unwrap(),
                        "{case}"
                    );
                }
                Err(error) => {
                    assert_ne!(value.unwrap_or(false), controlled, "{case}: {error}");
                    assert!(
                        error.to_string().contains("kvm_shared_dequeue_timers"),
                        "{case}: {error}"
                    );
                }
            }
        }
    }

    /// The four controlled-key outcomes that differ from simply ignoring the
    /// key, spelled out: each would otherwise run a configuration on a path
    /// other than the one the same JSON selected before.
    #[test]
    fn a_controlled_key_that_contradicts_the_capabilities_fails_the_parse() {
        let kvm = Config {
            backend: BackendCapabilities::KVM,
            sequentialize_threads: true,
            ..Config::default()
        };
        let kvm_json = to_legacy_backend_json(&kvm).unwrap();
        assert!(from_legacy_backend_json(&kvm_json).is_ok());
        let dbt = Config {
            backend: BackendCapabilities::DBT,
            sequentialize_threads: true,
            ..Config::default()
        };
        let dbt_json = to_legacy_backend_json(&dbt).unwrap();
        assert!(from_legacy_backend_json(&dbt_json).is_ok());
        for edited in [
            // KVM, sequentialized, key false: the control was left uninstalled.
            kvm_json.replacen(
                r#""kvm_shared_dequeue_timers":true"#,
                r#""kvm_shared_dequeue_timers":false"#,
                1,
            ),
            // KVM, sequentialized, key absent: read as false, as before.
            kvm_json.replacen(r#","kvm_shared_dequeue_timers":true"#, "", 1),
            // No control capability, key true: the controlled loop was chosen.
            dbt_json.replacen(
                r#""kvm_shared_dequeue_timers":false"#,
                r#""kvm_shared_dequeue_timers":true"#,
                1,
            ),
            // KVM without sequentialized threads, key true.
            to_legacy_backend_json(&Config {
                sequentialize_threads: false,
                ..kvm.clone()
            })
            .unwrap()
            .replacen(
                r#""kvm_shared_dequeue_timers":false"#,
                r#""kvm_shared_dequeue_timers":true"#,
                1,
            ),
        ] {
            assert_ne!(edited, kvm_json);
            assert_ne!(edited, dbt_json);
            let error = from_legacy_backend_json(&edited).unwrap_err();
            assert!(
                error.to_string().contains("kvm_shared_dequeue_timers"),
                "{error}"
            );
        }
    }

    /// Each legacy key decodes as the `Config` field it was: absent, it takes
    /// that field's default, and set alone, it gives exactly the capabilities
    /// that replaced Detcore's reads of that field.
    #[test]
    fn legacy_backend_keys_decode_as_the_fields_they_were() {
        let ptrace_json = to_legacy_backend_json(&Config::default()).unwrap();
        let ptrace: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&ptrace_json).unwrap();
        let keys = legacy_backend_keys(&Config::default());

        let mut absent = ptrace.clone();
        for (key, _) in keys {
            assert!(absent.remove(key).is_some(), "{key}");
        }
        let decoded = from_legacy_backend_json(&serde_json::to_string(&absent).unwrap()).unwrap();
        assert_eq!(decoded.backend, BackendCapabilities::PTRACE);
        let unflipped =
            serde_json::to_string(&from_legacy_backend_json(&ptrace_json).unwrap()).unwrap();
        assert_eq!(serde_json::to_string(&decoded).unwrap(), unflipped);

        for (key, ptrace_value) in keys {
            let mut expected = BackendCapabilities::PTRACE;
            match key {
                "cpuid_virtualized_by_backend" => expected.virtualizes_cpuid = true,
                "backend_supports_madvise" => {
                    expected.supports_madvise = false;
                    expected.supports_madv_dontneed = false;
                }
                "discover_live_file_metadata" => {
                    expected.tool_shares_guest_descriptor_table = true;
                    expected.rediscovers_descriptors_after_exec = true;
                    expected.internal_pipe_turns_are_host_timed = true;
                    expected.loopback_pollers_yield_to_peers = true;
                }
                "detect_host_clock_futex_timeouts" => {
                    expected.guest_clock_reads_bypass_backend = true
                }
                "syscall_clobbers_virtualized_by_backend" => {
                    expected.virtualizes_syscall_clobbers = true
                }
                "cancel_killed_thread_rpcs" => {
                    expected.needs_killed_thread_rpc_cancellation = true;
                    expected.reports_child_exit_publication = false;
                }
                "backend_reports_physical_process_exits" => {
                    expected.reports_physical_process_exits = true;
                    expected.signal_interrupts_external_syscalls = true;
                }
                "backend_tracks_process_children" => expected.tracks_process_children = false,
                "backend_runs_exit_robust_list" => expected.runs_exit_robust_list = false,
                "backend_requires_thread_directed_process_signals" => {
                    expected.requires_thread_directed_process_signals = true
                }
                "backend_is_kvm" => {
                    expected.provides_process_signal_control = true;
                    expected.emulates_child_waits = true;
                }
                "kvm_shared_dequeue_timers" => {
                    // Set alone it contradicts `backend_is_kvm`, so the parse
                    // fails; see
                    // `a_controlled_key_that_contradicts_the_capabilities_fails_the_parse`.
                    let mut flipped = ptrace.clone();
                    flipped.insert(key.to_owned(), serde_json::Value::Bool(!ptrace_value));
                    assert!(
                        from_legacy_backend_json(&serde_json::to_string(&flipped).unwrap())
                            .is_err(),
                        "{key}"
                    );
                    continue;
                }
                "backend_supports_parked_write_signal_interruption" => {
                    expected.supports_parked_write_signal_interruption = false
                }
                "backend_virtualizes_capability_prctls" => {
                    expected.virtualizes_capability_prctls = true
                }
                "backend_defers_vfork_child_registration" => {
                    expected.defers_vfork_child_registration = true
                }
                other => panic!("no expectation for legacy key {other}"),
            }
            let mut flipped = ptrace.clone();
            flipped.insert(key.to_owned(), serde_json::Value::Bool(!ptrace_value));
            let decoded =
                from_legacy_backend_json(&serde_json::to_string(&flipped).unwrap()).unwrap();
            assert_eq!(decoded.backend, expected, "{key}");
            assert_ne!(serde_json::to_string(&decoded).unwrap(), unflipped, "{key}");
        }
    }

    /// The legacy `kvm_shared_dequeue_timers` key holds exactly when the
    /// backend provides process signal control and threads are sequentialized,
    /// the condition under which Detcore installs that control.
    fn assert_controlled_key(
        json: &str,
        backend: &BackendCapabilities,
        sequentialize_threads: bool,
        name: &str,
    ) {
        let object: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(json).unwrap();
        assert_eq!(
            object["kvm_shared_dequeue_timers"],
            serde_json::Value::Bool(
                backend.provides_process_signal_control && sequentialize_threads
            ),
            "{name} sequentialize_threads={sequentialize_threads}"
        );
    }

    /// A legacy key failed the parse when it held the wrong type, or appeared
    /// twice, while it was a `Config` field; it still does, so a reader falls
    /// back as it did.
    #[test]
    fn a_legacy_backend_key_of_the_wrong_type_fails_the_parse() {
        let json = to_legacy_backend_json(&Config::default()).unwrap();
        assert!(from_legacy_backend_json(&json).is_ok());
        for (key, value) in legacy_backend_keys(&Config::default()) {
            let field = format!("\"{key}\":{value}");
            assert_eq!(json.matches(&field).count(), 1, "{field}");
            let mistyped = json.replacen(&field, &format!("\"{key}\":0"), 1);
            assert!(from_legacy_backend_json(&mistyped).is_err(), "{mistyped}");
            let repeated = json.replacen(&field, &format!("{field},{field}"), 1);
            assert!(from_legacy_backend_json(&repeated).is_err(), "{repeated}");
        }
    }

    /// The entries of a JSON object, in the order they appear.
    fn ordered_entries(json: &str) -> Vec<(String, serde_json::Value)> {
        struct Entries(Vec<(String, serde_json::Value)>);

        impl<'de> Deserialize<'de> for Entries {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                struct EntriesVisitor;

                impl<'de> serde::de::Visitor<'de> for EntriesVisitor {
                    type Value = Entries;

                    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                        formatter.write_str("a JSON object")
                    }

                    fn visit_map<A: serde::de::MapAccess<'de>>(
                        self,
                        mut map: A,
                    ) -> Result<Entries, A::Error> {
                        let mut entries = Vec::new();
                        while let Some(entry) = map.next_entry()? {
                            entries.push(entry);
                        }
                        Ok(Entries(entries))
                    }
                }

                deserializer.deserialize_map(EntriesVisitor)
            }
        }

        serde_json::from_str::<Entries>(json).unwrap().0
    }

    /// The JSON array of `json`'s values in its key order: the same
    /// configuration, listed by position.
    fn positional(json: &str) -> String {
        let values: Vec<serde_json::Value> = ordered_entries(json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        serde_json::to_string(&values).unwrap()
    }

    /// The legacy form's positions are `Config`'s fields in declaration order
    /// with the fifteen legacy keys where `backend` stands and no
    /// `record_host_inputs`, `backend_supports_blocked_wait_signal_interruption`,
    /// `backend_delivers_timer_events`, `guest_may_inherit_a_terminal`,
    /// `in_guest_detlog_forward_policy`, `in_guest_site_patching_off`,
    /// `target_timeslice_syscalls_only`, `futex_wake_yields`, `replaying`,
    /// `scheduler_turn_cost` or
    /// `inherited_seccomp`, which is the
    /// key order the encoder writes.
    #[test]
    fn legacy_positions_are_the_encoded_key_order() {
        let names = legacy_backend_keys(&Config::default()).map(|(name, _)| name);
        assert_eq!(names, LEGACY_BACKEND_KEY_NAMES);

        let fields = serde_json::from_str::<legacy_backend_json::KeyOrder>(
            &serde_json::to_string(&Config::default()).unwrap(),
        )
        .unwrap()
        .0;
        let mut expected = Vec::new();
        for field in &fields {
            match field.as_str() {
                "backend" => expected.extend(names.map(str::to_owned)),
                "record_host_inputs"
                | "backend_supports_blocked_wait_signal_interruption"
                | "backend_delivers_timer_events"
                | "guest_may_inherit_a_terminal"
                | "in_guest_detlog_forward_policy"
                | "in_guest_site_patching_off"
                | "target_timeslice_syscalls_only"
                | "futex_wake_yields"
                | "replaying"
                | "scheduler_turn_cost"
                | "inherited_seccomp" => {}
                other => expected.push(other.to_owned()),
            }
        }
        let encoded = to_legacy_backend_json(&Config::default()).unwrap();
        let keys: Vec<String> = ordered_entries(&encoded)
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        assert_eq!(keys, expected);
        // `backend` becomes fifteen keys; `record_host_inputs`,
        // `backend_supports_blocked_wait_signal_interruption`,
        // `backend_delivers_timer_events`, `guest_may_inherit_a_terminal`,
        // `in_guest_detlog_forward_policy`, `in_guest_site_patching_off`,
        // `target_timeslice_syscalls_only`, `futex_wake_yields`, `replaying`,
        // `scheduler_turn_cost` and `inherited_seccomp` none.
        assert!(!fields.iter().any(|field| field == "shared_dequeue_timers"));
        assert_eq!(fields.len() + 3, keys.len());
    }

    /// `record_host_inputs` never enters the legacy form, whatever its value:
    /// the guest-visible string stays the legacy bytes, and it reads back as
    /// false from both the object and the array form.
    #[test]
    fn record_host_inputs_never_enters_the_legacy_form() {
        let off = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        let on = Config {
            record_host_inputs: true,
            ..off.clone()
        };
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&off).unwrap());
        assert!(!json.contains("record_host_inputs"), "{json}");
        assert!(!from_legacy_backend_json(&json).unwrap().record_host_inputs);
        let values: Vec<serde_json::Value> = ordered_entries(&json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let array = serde_json::to_string(&values).unwrap();
        assert!(!from_legacy_backend_json(&array).unwrap().record_host_inputs);
    }

    /// `process_exits_complete_asynchronously`, added after the legacy form
    /// froze, never enters it, whatever its value, exactly as
    /// `record_host_inputs` does not: the guest-visible bytes stay the legacy
    /// bytes, and it reads back as false from the object and the array form.
    #[test]
    fn process_exits_complete_asynchronously_never_enters_the_legacy_form() {
        let on = Config {
            backend: BackendCapabilities::LITEINST_IN_GUEST,
            ..Config::default()
        };
        assert!(on.backend.process_exits_complete_asynchronously);
        assert!(on.backend.virtualizes_guest_sigalrm);
        let off = Config {
            backend: legacy_expressible(on.backend),
            ..on.clone()
        };
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&off).unwrap());
        assert!(
            !json.contains("process_exits_complete_asynchronously"),
            "{json}"
        );
        assert!(!json.contains("virtualizes_guest_sigalrm"), "{json}");
        let decoded = from_legacy_backend_json(&json).unwrap();
        assert!(!decoded.backend.process_exits_complete_asynchronously);
        assert!(!decoded.backend.virtualizes_guest_sigalrm);
        let array = positional(&json);
        assert!(
            !from_legacy_backend_json(&array)
                .unwrap()
                .backend
                .process_exits_complete_asynchronously
        );
    }

    /// `backend_supports_blocked_wait_signal_interruption` never enters the
    /// legacy form, whatever its value: the guest-visible string stays the
    /// legacy bytes, and it reads back as false from both the object and the
    /// array form.
    #[test]
    fn blocked_wait_signal_interruption_never_enters_the_legacy_form() {
        let off = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        let on = Config {
            backend_supports_blocked_wait_signal_interruption: true,
            backend_delivers_timer_events: false,
            ..off.clone()
        };
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&off).unwrap());
        assert!(
            !json.contains("backend_supports_blocked_wait_signal_interruption"),
            "{json}"
        );
        assert!(
            !from_legacy_backend_json(&json)
                .unwrap()
                .backend_supports_blocked_wait_signal_interruption
        );
        let values: Vec<serde_json::Value> = ordered_entries(&json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let array = serde_json::to_string(&values).unwrap();
        assert!(
            !from_legacy_backend_json(&array)
                .unwrap()
                .backend_supports_blocked_wait_signal_interruption
        );
    }

    /// `guest_may_inherit_a_terminal` never enters the legacy form, whatever
    /// its value: the guest-visible string stays the legacy bytes, and it reads
    /// back as false from both the object and the array form.
    /// `in_guest_detlog_forward_policy` is unset for DBT, so the legacy form
    /// never carries it, and it reads back as unset.
    #[test]
    fn in_guest_detlog_forward_policy_never_enters_the_legacy_form() {
        let off = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        let on = Config {
            in_guest_detlog_forward_policy: Some("1".to_owned()),
            ..off.clone()
        };
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&off).unwrap());
        assert!(!json.contains("in_guest_detlog_forward_policy"), "{json}");
        assert!(
            from_legacy_backend_json(&json)
                .unwrap()
                .in_guest_detlog_forward_policy
                .is_none()
        );
    }

    /// `in_guest_site_patching_off` is false for DBT, so the legacy form never
    /// carries it, and it reads back as false from both the object and the
    /// array form.
    #[test]
    fn in_guest_site_patching_off_never_enters_the_legacy_form() {
        let off = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        let on = Config {
            in_guest_site_patching_off: true,
            ..off.clone()
        };
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&off).unwrap());
        assert!(!json.contains("in_guest_site_patching_off"), "{json}");
        assert!(
            !from_legacy_backend_json(&json)
                .unwrap()
                .in_guest_site_patching_off
        );
        let values: Vec<serde_json::Value> = ordered_entries(&json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let array = serde_json::to_string(&values).unwrap();
        assert!(
            !from_legacy_backend_json(&array)
                .unwrap()
                .in_guest_site_patching_off
        );
    }

    #[test]
    fn guest_may_inherit_a_terminal_never_enters_the_legacy_form() {
        let off = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        let on = Config {
            guest_may_inherit_a_terminal: true,
            ..off.clone()
        };
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&off).unwrap());
        assert!(!json.contains("guest_may_inherit_a_terminal"), "{json}");
        assert!(
            !from_legacy_backend_json(&json)
                .unwrap()
                .guest_may_inherit_a_terminal
        );
        let values: Vec<serde_json::Value> = ordered_entries(&json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let array = serde_json::to_string(&values).unwrap();
        assert!(
            !from_legacy_backend_json(&array)
                .unwrap()
                .guest_may_inherit_a_terminal
        );
    }

    /// `inherited_seccomp` never enters the legacy form, whatever its value:
    /// the guest-visible string stays the legacy bytes, and it reads back as
    /// unknown from both the object and the array form.
    #[test]
    fn inherited_seccomp_never_enters_the_legacy_form() {
        use crate::config::InheritedSeccomp;
        let unknown = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        assert_eq!(unknown.inherited_seccomp, InheritedSeccomp::Unknown);
        let measured = Config {
            inherited_seccomp: InheritedSeccomp::Filter(Some(2)),
            ..unknown.clone()
        };
        let json = to_legacy_backend_json(&measured).unwrap();
        assert_eq!(json, to_legacy_backend_json(&unknown).unwrap());
        assert!(!json.contains("inherited_seccomp"), "{json}");
        assert_eq!(
            from_legacy_backend_json(&json).unwrap().inherited_seccomp,
            InheritedSeccomp::Unknown
        );
        let values: Vec<serde_json::Value> = ordered_entries(&json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let array = serde_json::to_string(&values).unwrap();
        assert_eq!(
            from_legacy_backend_json(&array).unwrap().inherited_seccomp,
            InheritedSeccomp::Unknown
        );
    }

    /// `Config` crosses Reverie RPC as legacy bincode, which is positional, so
    /// `inherited_seccomp` must be encoded whatever its value, the number of
    /// filters included.
    #[test]
    fn inherited_seccomp_round_trips_through_reverie_bincode() {
        use crate::config::InheritedSeccomp;
        for inherited_seccomp in [
            InheritedSeccomp::Unknown,
            InheritedSeccomp::Disabled,
            InheritedSeccomp::Strict,
            InheritedSeccomp::Filter(None),
            InheritedSeccomp::Filter(Some(3)),
        ] {
            let config = Config {
                inherited_seccomp,
                ..Config::default()
            };
            let wire = bincode::serde::encode_to_vec(&config, bincode::config::legacy()).unwrap();
            let (decoded, read): (Config, usize) =
                bincode::serde::decode_from_slice(&wire, bincode::config::legacy()).unwrap();
            assert_eq!(read, wire.len());
            assert_eq!(decoded.inherited_seccomp, inherited_seccomp);
            assert_eq!(
                bincode::serde::encode_to_vec(&decoded, bincode::config::legacy()).unwrap(),
                wire
            );
        }
    }

    /// `Seccomp` gives the mode, and `Seccomp_filters` the number of filters in
    /// filter mode.
    #[test]
    fn inherited_seccomp_is_read_from_a_status_file() {
        use crate::config::InheritedSeccomp;
        let status = |lines: &str| {
            InheritedSeccomp::parse(&format!(
                "Name:\tguest\nNoNewPrivs:\t1\n{lines}Speculation_Store_Bypass:\tthread vulnerable\n"
            ))
        };
        assert_eq!(
            status("Seccomp:\t0\nSeccomp_filters:\t0\n"),
            InheritedSeccomp::Disabled
        );
        assert_eq!(
            status("Seccomp:\t1\nSeccomp_filters:\t0\n"),
            InheritedSeccomp::Strict
        );
        assert_eq!(
            status("Seccomp:\t2\nSeccomp_filters:\t3\n"),
            InheritedSeccomp::Filter(Some(3))
        );
        assert_eq!(status("Seccomp:\t2\n"), InheritedSeccomp::Filter(None));
        assert_eq!(status(""), InheritedSeccomp::Unknown);
        assert_eq!(status("Seccomp:\t7\n"), InheritedSeccomp::Unknown);
        // This thread's own state is readable.
        assert_ne!(InheritedSeccomp::measure(), InheritedSeccomp::Unknown);
    }

    /// `target_timeslice_syscalls_only` never enters the legacy form, whatever
    /// its value: the guest-visible string stays the legacy bytes, and it reads
    /// back as false from both the object and the array form.
    #[test]
    fn target_timeslice_syscalls_only_never_enters_the_legacy_form() {
        let off = Config {
            backend: BackendCapabilities::DBT,
            target_timeslice: NonZeroU64::new(1_000_000),
            ..Config::default()
        };
        let on = Config {
            target_timeslice_syscalls_only: true,
            ..off.clone()
        };
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&off).unwrap());
        assert!(!json.contains("target_timeslice_syscalls_only"), "{json}");
        assert!(
            !from_legacy_backend_json(&json)
                .unwrap()
                .target_timeslice_syscalls_only
        );
        let values: Vec<serde_json::Value> = ordered_entries(&json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let array = serde_json::to_string(&values).unwrap();
        assert!(
            !from_legacy_backend_json(&array)
                .unwrap()
                .target_timeslice_syscalls_only
        );
    }

    #[test]
    fn a_futex_wake_yields_only_when_it_woke_a_waiter() {
        let config = Config {
            futex_wake_yields: true,
            ..Config::default()
        };
        assert!(config.yields_after_futex_wake(1));
        assert!(config.yields_after_futex_wake(3));
        // A wake that found no waiter hands nothing off and keeps the slice.
        assert!(!config.yields_after_futex_wake(0));
        // Off by default.
        assert!(!Config::default().futex_wake_yields);
        assert!(!Config::default().yields_after_futex_wake(1));
    }

    #[test]
    fn a_futex_wake_yields_under_replay_too() {
        // Neither record holds the queue order the policy creates, so replay
        // must make the same decision as the recorded run.
        let config = Config {
            futex_wake_yields: true,
            ..Config::default()
        };
        let preemption_replay = Config {
            replay_preemptions_from: Some(PathBuf::from("/nonexistent/preemptions.json")),
            ..config.clone()
        };
        assert!(preemption_replay.yields_after_futex_wake(1));
        assert!(!preemption_replay.yields_after_futex_wake(0));
        let schedule_replay = Config {
            replay_schedule_from: Some(PathBuf::from("/nonexistent/schedule.json")),
            ..config
        };
        assert!(schedule_replay.yields_after_futex_wake(1));
    }

    #[test]
    fn scheduler_turn_charge_refuses_a_scaled_charge_below_one_nanosecond() {
        let default = Config::default();
        assert_eq!(default.scheduler_turn_charge(), Ok(500_000));
        let scaled = Config {
            clock_multiplier: Some(0.1),
            ..Config::default()
        };
        assert_eq!(scaled.scheduler_turn_charge(), Ok(50_000));
        let cost = |cost: u64, multiplier: f64| Config {
            scheduler_turn_cost: NonZeroU64::new(cost),
            clock_multiplier: Some(multiplier),
            ..Config::default()
        };
        assert_eq!(cost(10_000, 0.1).scheduler_turn_charge(), Ok(1_000));
        assert_eq!(cost(10, 0.1).scheduler_turn_charge(), Ok(1));
        // Truncation would make these zero: a finite poll would never expire.
        assert!(cost(9, 0.1).scheduler_turn_charge().is_err());
        assert!(cost(1, 0.1).scheduler_turn_charge().is_err());
        assert_eq!(
            cost(MAX_SCHEDULER_TURN_COST, 1.0).scheduler_turn_charge(),
            Ok(MAX_SCHEDULER_TURN_COST)
        );
        assert!(
            cost(MAX_SCHEDULER_TURN_COST + 1, 1.0)
                .scheduler_turn_charge()
                .is_err()
        );
        assert!(cost(u64::MAX, 1.0).scheduler_turn_charge().is_err());
        assert!(
            cost(MAX_SCHEDULER_TURN_COST, 1e12)
                .scheduler_turn_charge()
                .is_err()
        );
        // The scaled charge has the same upper bound as the cost: one charge
        // of 9e18 ns would exhaust the clock in two turns.
        assert!(
            cost(MAX_SCHEDULER_TURN_COST, 9e9)
                .scheduler_turn_charge()
                .is_err()
        );
        assert!(cost(1_000_000, 1_001.0).scheduler_turn_charge().is_err());
        assert_eq!(
            cost(1_000_000, 1_000.0).scheduler_turn_charge(),
            Ok(MAX_SCHEDULER_TURN_COST)
        );
        let message = cost(MAX_SCHEDULER_TURN_COST, 9e9)
            .scheduler_turn_charge()
            .unwrap_err();
        assert!(message.contains("at least 1"), "{message}");
        assert!(message.contains("at most 1000000000"), "{message}");
    }

    /// `futex_wake_yields` never enters the legacy form, whatever its
    /// value: the guest-visible string stays the legacy bytes, and it reads
    /// back as false from both the object and the array form.
    #[test]
    fn futex_wake_yields_never_enters_the_legacy_form() {
        let off = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        let on = Config {
            futex_wake_yields: true,
            ..off.clone()
        };
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&off).unwrap());
        assert!(!json.contains("futex_wake_yields"), "{json}");
        assert!(!from_legacy_backend_json(&json).unwrap().futex_wake_yields);
        let values: Vec<serde_json::Value> = ordered_entries(&json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let array = serde_json::to_string(&values).unwrap();
        assert!(!from_legacy_backend_json(&array).unwrap().futex_wake_yields);
    }

    /// `scheduler_turn_cost` never enters the legacy form, whatever its value:
    /// the guest-visible string stays the legacy bytes, and it reads back as
    /// unset from both the object and the array form.
    #[test]
    fn scheduler_turn_cost_never_enters_the_legacy_form() {
        let off = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        let on = Config {
            scheduler_turn_cost: NonZeroU64::new(10_000),
            ..off.clone()
        };
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&off).unwrap());
        assert!(!json.contains("scheduler_turn_cost"), "{json}");
        assert_eq!(
            from_legacy_backend_json(&json).unwrap().scheduler_turn_cost,
            None
        );
        let values: Vec<serde_json::Value> = ordered_entries(&json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let array = serde_json::to_string(&values).unwrap();
        assert_eq!(
            from_legacy_backend_json(&array)
                .unwrap()
                .scheduler_turn_cost,
            None
        );
    }

    /// `replaying` never enters the legacy form, whatever its value: the
    /// guest-visible string stays the legacy bytes, and it reads back as false
    /// from both the object and the array form.
    #[test]
    fn replaying_never_enters_the_legacy_form() {
        let off = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        let on = Config {
            replaying: true,
            ..off.clone()
        };
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&off).unwrap());
        assert!(!json.contains("replaying"), "{json}");
        assert!(!from_legacy_backend_json(&json).unwrap().replaying);
        let values: Vec<serde_json::Value> = ordered_entries(&json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let array = serde_json::to_string(&values).unwrap();
        assert!(!from_legacy_backend_json(&array).unwrap().replaying);
    }

    /// `Config` crosses Reverie RPC as legacy bincode, which is positional, so
    /// `replaying` must be encoded whatever its value: a field left out when
    /// false would shift every later field for the decoder.
    #[test]
    fn replaying_round_trips_through_reverie_bincode() {
        for replaying in [false, true] {
            let config = Config {
                replaying,
                ..Config::default()
            };
            let wire = bincode::serde::encode_to_vec(&config, bincode::config::legacy()).unwrap();
            let (decoded, read): (Config, usize) =
                bincode::serde::decode_from_slice(&wire, bincode::config::legacy()).unwrap();
            assert_eq!(read, wire.len());
            assert_eq!(decoded.replaying, replaying);
            assert_eq!(
                bincode::serde::encode_to_vec(&decoded, bincode::config::legacy()).unwrap(),
                wire
            );
        }
    }

    /// Serde reads a derived struct from a JSON array by position, so the
    /// legacy form could be given as one. Each array reads as the object
    /// whose values it lists in order, every backend's capabilities included.
    #[test]
    fn a_legacy_positional_array_reads_as_the_object_it_lists() {
        for (name, backend) in [
            ("PTRACE", BackendCapabilities::PTRACE),
            ("E9PATCH", BackendCapabilities::E9PATCH),
            (
                "ptrace without exit transitions",
                ptrace_without_exit_transitions(),
            ),
            ("LITEINST_IN_GUEST", BackendCapabilities::LITEINST_IN_GUEST),
            ("SABRE", BackendCapabilities::SABRE),
            ("DBT", BackendCapabilities::DBT),
            ("KVM", BackendCapabilities::KVM),
        ] {
            for sequentialize_threads in [false, true] {
                let sent = Config {
                    backend,
                    sequentialize_threads,
                    seed: 7,
                    chaos: true,
                    ..Config::default()
                };
                let json = to_legacy_backend_json(&sent).unwrap();
                assert_controlled_key(&json, &backend, sequentialize_threads, name);
                let array = positional(&json);
                assert!(array.starts_with('['), "{array}");
                let received = from_legacy_backend_json(&array).unwrap();
                assert_eq!(received.backend, legacy_expressible(backend), "{name}");
                assert_eq!(
                    received.sequentialize_threads, sequentialize_threads,
                    "{name}"
                );
                assert_eq!(received.seed, 7, "{name}");
                assert!(received.chaos, "{name}");
                assert_eq!(to_legacy_backend_json(&received).unwrap(), json, "{name}");
            }
        }
    }

    /// An array fails the parse where the legacy form's array did: with an
    /// element past the last position, with an element of the wrong type, or
    /// short of a field that has no default. The last position,
    /// `interrupt_at`, has none, so every shorter array fails.
    #[test]
    fn a_malformed_legacy_positional_array_fails_the_parse() {
        let json = to_legacy_backend_json(&Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        })
        .unwrap();
        let entries = ordered_entries(&json);
        let values: Vec<serde_json::Value> =
            entries.iter().map(|(_, value)| value.clone()).collect();
        let parse = |values: &[serde_json::Value]| {
            from_legacy_backend_json(&serde_json::to_string(values).unwrap())
        };
        assert!(parse(&values).is_ok());

        let mut longer = values.clone();
        longer.push(serde_json::Value::Bool(false));
        assert!(parse(&longer).is_err());

        assert_eq!(entries.last().unwrap().0, "interrupt_at");
        for len in 0..values.len() {
            assert!(parse(&values[..len]).is_err(), "{len} elements");
        }

        for (position, (key, _)) in entries.iter().enumerate() {
            if LEGACY_BACKEND_KEY_NAMES.contains(&key.as_str()) || key == "seed" {
                let mut mistyped = values.clone();
                mistyped[position] = serde_json::json!("0");
                assert!(parse(&mistyped).is_err(), "{key}");
            }
        }
    }

    /// Selects the child half of
    /// [`a_legacy_configuration_decodes_whatever_the_ambient_hermit_settings`] and
    /// carries the input it decodes.
    const LEGACY_INPUT_IN_CHILD: &str = "DETCORE_MODEL_TEST_LEGACY_INPUT";
    /// The legacy object the child's decoded configuration must re-encode as.
    const LEGACY_EXPECTED_IN_CHILD: &str = "DETCORE_MODEL_TEST_LEGACY_EXPECTED";
    /// Printed by a child that decoded its input, so that a parent can tell
    /// that run from one that selected no test.
    const LEGACY_DECODED_IN_CHILD: &str = "legacy configuration decoded under the ambient settings";

    /// Clap reads HERMIT_EPOCH, HERMIT_PRNG and HERMIT_SCHED_SEED when it
    /// parses a `Config`, and exits the process when one of them does not
    /// parse. The legacy decoder read none of them, so a valid legacy object
    /// or array decodes to the configuration it lists whatever they hold. Each
    /// decode runs in a child process given those variables unset, set to
    /// text that does not parse, or set to bytes that are not UTF-8. The child
    /// builds no `Config` before decoding, which would read them itself.
    #[test]
    fn a_legacy_configuration_decodes_whatever_the_ambient_hermit_settings() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        if let Some(input) = std::env::var_os(LEGACY_INPUT_IN_CHILD) {
            let expected = std::env::var(LEGACY_EXPECTED_IN_CHILD).unwrap();
            let received = from_legacy_backend_json(input.to_str().unwrap()).unwrap();
            assert_eq!(
                received.backend,
                legacy_expressible(BackendCapabilities::DBT)
            );
            assert_eq!(received.seed, 7);
            assert_eq!(received.sched_seed, Some(11));
            assert_eq!(to_legacy_backend_json(&received).unwrap(), expected);
            println!("{LEGACY_DECODED_IN_CHILD}");
            return;
        }

        const VARIABLES: [&str; 3] = ["HERMIT_EPOCH", "HERMIT_PRNG", "HERMIT_SCHED_SEED"];
        let text = OsStr::new("not-a-value");
        let bytes = OsStr::from_bytes(b"\xff");
        let mut settings: Vec<Vec<(&str, &OsStr)>> = vec![Vec::new()];
        for variable in VARIABLES {
            settings.push(vec![(variable, text)]);
            settings.push(vec![(variable, bytes)]);
        }
        settings.push(VARIABLES.map(|variable| (variable, text)).to_vec());
        settings.push(VARIABLES.map(|variable| (variable, bytes)).to_vec());

        let object = to_legacy_backend_json(&Config {
            backend: BackendCapabilities::DBT,
            seed: 7,
            sched_seed: Some(11),
            ..Config::default()
        })
        .unwrap();
        let array = positional(&object);
        assert!(array.starts_with('['), "{array}");
        let test = format!(
            "{}::a_legacy_configuration_decodes_whatever_the_ambient_hermit_settings",
            module_path!().split_once("::").unwrap().1
        );
        for input in [&object, &array] {
            for setting in &settings {
                let mut child = std::process::Command::new(std::env::current_exe().unwrap());
                child
                    .args(["--exact", &test, "--nocapture"])
                    .env(LEGACY_INPUT_IN_CHILD, input)
                    .env(LEGACY_EXPECTED_IN_CHILD, &object);
                for variable in VARIABLES {
                    child.env_remove(variable);
                }
                for (variable, value) in setting {
                    child.env(variable, value);
                }
                let output = child.output().unwrap();
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                assert!(
                    output.status.success()
                        && stdout.contains(LEGACY_DECODED_IN_CHILD)
                        && stdout.contains("test result: ok. 1 passed; 0 failed;"),
                    "decoding {} under {setting:?} did not succeed exactly once: {}\n{stdout}\n{stderr}",
                    if input.starts_with('[') {
                        "the array"
                    } else {
                        "the object"
                    },
                    output.status,
                );
            }
        }
    }

    /// `backend` and `shared_dequeue_timers` were not keys of the legacy form,
    /// which ignored every key it did not name. They are still ignored,
    /// whatever they hold, however often they appear and wherever they stand,
    /// so the legacy keys alone decide the capabilities. A key the legacy form
    /// named still fails the parse when it appears twice.
    #[test]
    fn keys_the_legacy_form_did_not_name_are_ignored() {
        let sent = Config {
            backend: legacy_expressible(BackendCapabilities::DBT),
            seed: 7,
            ..Config::default()
        };
        let json = to_legacy_backend_json(&sent).unwrap();
        let kvm = serde_json::to_string(&BackendCapabilities::KVM).unwrap();
        for extra in [
            r#""backend":null"#.to_owned(),
            r#""shared_dequeue_timers":null"#.to_owned(),
            r#""backend":null,"shared_dequeue_timers":null"#.to_owned(),
            format!(r#""backend":{kvm},"shared_dequeue_timers":true"#),
            r#""backend":7,"backend":{"supports_madvise":false}"#.to_owned(),
            r#""shared_dequeue_timers":"yes","shared_dequeue_timers":true"#.to_owned(),
            r#""no_such_field":[1,{"x":null}]"#.to_owned(),
        ] {
            for edited in [
                json.replacen('{', &format!("{{{extra},"), 1),
                format!("{},{extra}}}", &json[..json.len() - 1]),
            ] {
                let received = from_legacy_backend_json(&edited).unwrap();
                assert_eq!(received.backend, sent.backend, "{edited}");
                assert_eq!(
                    serde_json::to_string(&received).unwrap(),
                    serde_json::to_string(&sent).unwrap(),
                    "{edited}"
                );
                assert_eq!(to_legacy_backend_json(&received).unwrap(), json, "{edited}");
            }
        }

        let repeated = format!("{},\"seed\":7}}", &json[..json.len() - 1]);
        assert!(from_legacy_backend_json(&repeated).is_err());
    }

    #[test]
    fn network_perturbation_seed_never_falls_back_to_scheduler_seeds() {
        let mut config = Config {
            seed: 41,
            sched_seed: Some(42),
            ..Config::default()
        };
        assert_eq!(config.network_trace.network_perturb_seed, None);

        config.network_trace = NetworkTraceConfig {
            mode: crate::network_trace::NetworkTraceMode::Replay,
            path: Some("network.trace".into()),
            network_perturb_seed: Some(43),
        };
        assert_eq!(config.seed, 41);
        assert_eq!(config.sched_seed(), 42);
        assert_eq!(config.network_trace.network_perturb_seed, Some(43));
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
            .remove("mount_id_assignment_order");
        let restored: Config = serde_json::from_value(value).unwrap();
        assert!(restored.mountinfo_root_rewrites.is_empty());
        assert!(restored.mountinfo_device_rewrites.is_empty());
        assert!(restored.mountinfo_mount_ids.is_empty());
        assert!(!restored.mountinfo_mount_ids_captured);
        assert!(restored.mount_id_assignment_order.is_empty());
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
    fn config_fingerprint_sees_the_width_of_every_backend_capability() {
        // `BackendCapabilities` lives in Reverie, outside the fingerprinted
        // sources, so only its encoding can reveal a change to it. Every
        // field is a bool, one byte in the encoding whatever its value, so the
        // encoded default carries every field, and adding, removing or
        // widening one changes the bytes. A field that is optional would have
        // to be present in the default, or a change to its width would encode
        // as the same bare `None`; this fails first.
        let config = config_wire_default();
        let fields = serde_json::to_value(config.backend).unwrap();
        let fields = fields.as_object().unwrap();
        assert!(
            fields.values().all(serde_json::Value::is_boolean),
            "{fields:?}"
        );
        let encode = |config: &Config| {
            bincode::serde::encode_to_vec(config, bincode::config::legacy()).unwrap()
        };
        assert_eq!(
            bincode::serde::encode_to_vec(config.backend, bincode::config::legacy())
                .unwrap()
                .len(),
            fields.len()
        );
        // The backend's values are part of the encoded default.
        let flipped = config
            .clone()
            .with_backend(|backend| backend.supports_madvise = !backend.supports_madvise);
        assert_eq!(encode(&flipped).len(), encode(&config).len());
        assert_ne!(encode(&flipped), encode(&config));
    }

    #[test]
    fn config_fingerprint_includes_clock_rpc_definitions() {
        let config = config_wire_default();
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

    /// The inode RPCs carry `RawInode`, whose layout neither encoding of
    /// `Config` shows. A plugin built before `RawInode` gained its device
    /// sends one `u64` where the coordinator decodes two, so a change to its
    /// definition must invalidate the fingerprint.
    #[test]
    fn config_fingerprint_includes_the_inode_rpc_identity() {
        let config = config_wire_default();
        let wire = bincode::serde::encode_to_vec(&config, bincode::config::legacy()).unwrap();
        let named_shape = serde_json::to_string(&config).unwrap();
        let current = config_wire_fingerprint();
        let inode_source = include_bytes!("fd.rs").as_slice();
        assert!(CONFIG_DEFINITION_SOURCES.contains(&inode_source));
        let without_device = include_str!("fd.rs").replacen("    pub dev: u64,\n", "", 1);
        assert_ne!(without_device.as_bytes(), inode_source);
        let changed_sources: Vec<_> = CONFIG_DEFINITION_SOURCES
            .iter()
            .map(|source| {
                if *source == inode_source {
                    without_device.as_bytes()
                } else {
                    *source
                }
            })
            .collect();
        assert_ne!(
            fingerprint_of_config_material(&wire, &named_shape, &changed_sources),
            current,
            "an inode-identity layout change must invalidate the fingerprint"
        );
    }

    #[test]
    fn a_config_fingerprint_mismatch_names_both_fingerprints() {
        let ours = config_wire_fingerprint();
        assert_eq!(
            config_fingerprint_mismatch(std::ffi::OsStr::new(&ours)),
            None
        );
        let other = config_fingerprint_mismatch(std::ffi::OsStr::new("0123456789abcdef"))
            .expect("another fingerprint is a mismatch");
        assert!(other.contains(&ours), "{other}");
        assert!(other.contains("0123456789abcdef"), "{other}");
        use std::os::unix::ffi::OsStrExt;
        assert!(config_fingerprint_mismatch(std::ffi::OsStr::from_bytes(b"\xff")).is_some());
    }

    #[test]
    fn config_fingerprint_is_stable_and_shape_sensitive() {
        // STABLE: a build must agree with itself, or the guard would reject a
        // MATCHED pair -- which would be worse than having no guard at all.
        assert_eq!(config_wire_fingerprint(), config_wire_fingerprint());
        assert_eq!(config_wire_fingerprint().len(), 16);

        let config = config_wire_default();
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

    #[test]
    fn config_fingerprint_uses_environment_free_wire_defaults() {
        let environment_derived = Config {
            epoch: "2042-03-04T05:06:07.890123456Z".parse().unwrap(),
            seed: 41,
            sched_seed: Some(42),
            ..Config::default()
        };

        let canonical = config_wire_default();

        assert_ne!(
            serde_json::to_string(&environment_derived).unwrap(),
            serde_json::to_string(&canonical).unwrap()
        );
        assert_eq!(
            canonical.epoch,
            DEFAULT_EPOCH_STR.parse::<DateTime<Utc>>().unwrap()
        );
        assert_eq!(canonical.seed, 0);
        assert_eq!(canonical.sched_seed, None);
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
