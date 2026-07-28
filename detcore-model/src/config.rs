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

use crate::pid::DetTid;
use crate::schedule::SigWrapper;
use crate::time::NANOS_PER_RCB;

const fn default_true() -> bool {
    true
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
    /// Guest clock reads use per-thread logical time instead of the global RPC arrival order.
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

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1058): Review process-signal identity translation.
    /// The backend cannot execute process-directed signal syscalls using Detcore's guest PID and
    /// therefore requires Detcore to translate an unambiguous process target to a specific thread.
    #[serde(default)]
    #[clap(skip)]
    pub backend_requires_thread_directed_process_signals: bool,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1125): Review backend-owned capability-control prctls.
    /// The execution backend virtualizes capability bounding-set and ambient-capability state.
    #[serde(default)]
    #[clap(skip)]
    pub backend_virtualizes_capability_prctls: bool,

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

    /// Disable substitution of virtual (deterministic) file metadata, in lieu
    /// of the real metadata returned by `fstat`, implies `virtualize_time`.
    #[clap(long = "no-virtualize-metadata", action = clap::ArgAction::SetFalse)]
    pub virtualize_metadata: bool,

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
    // TODO-HUMAN-REVIEW(PR-1069)
    /// RR-style stable per-thread slowdown factors. When set (with `--chaos`),
    /// each thread is assigned a slowdown factor that is CONSTANT for the whole
    /// run — some threads are consistently slower, some faster — rather than
    /// redrawing a fresh scheduling priority every timeslice (which averages out
    /// over a long run by the law of large numbers). The factor scales the mean
    /// chaos timeslice length only (an out-of-band scheduling budget); it does
    /// NOT scale guest-visible virtual time, so determinism is preserved and the
    /// mode is `--strict`-safe. The factor is a pure, replayable function of the
    /// scheduler seed and the thread's deterministic id, so a fixed seed
    /// reproduces the same per-thread factors. Mirrors rr's `--nested` /
    /// chaos-mode per-task priority idea.
    #[clap(long)]
    pub chaos_per_thread_slowdown: bool,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1069)
    /// Maximum ratio between the slowest and fastest per-thread slowdown factor
    /// for `--chaos-per-thread-slowdown`. Each thread's factor is drawn
    /// log-uniformly from `[1/R, R]` where `R` is this value, so a thread's mean
    /// timeslice can be up to `R`x longer or `1/R`x shorter than nominal. Must be
    /// `>= 1.0`; `1.0` disables the spread (all threads nominal). Default 10.0.
    #[clap(long, default_value = "10.0", value_name = "double")]
    pub chaos_slowdown_max_factor: f64,

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
    /// Explicit strict mode enables this policy.
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
    #[clap(long)]
    pub detlog_stack: bool,

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
        let multiplier = self.clock_multiplier.unwrap_or(1.0);
        ((NANOS_PER_RCB * multiplier).ceil() as u64).max(NANOS_PER_RCB as u64)
    }

    /// Check invariants that must hold at every execution boundary without mutating the config.
    pub fn validate_invariants(&self) {
        assert!(self.sched_sticky_random_param >= 0.0);
        assert!(self.sched_sticky_random_param <= 1.0);
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-1069)
        assert!(
            self.chaos_slowdown_max_factor.is_finite() && self.chaos_slowdown_max_factor >= 1.0,
            "chaos_slowdown_max_factor must be finite and >= 1.0, got {}",
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
        // TODO-HUMAN-REVIEW(PR-1069)
        if self.chaos_per_thread_slowdown {
            write!(f, " --chaos-per-thread-slowdown")?;
            write!(
                f,
                " --chaos-slowdown-max-factor={}",
                self.chaos_slowdown_max_factor
            )?;
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
        assert!(!config.backend_serializes_fork_children);
        assert!(config.backend_dispatches_thread_tools);
        assert!(!config.backend_requires_thread_directed_process_signals);
        assert!(!config.backend_virtualizes_capability_prctls);
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
    // TODO-HUMAN-REVIEW(PR-1069)
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
    // TODO-HUMAN-REVIEW(PR-1069)
    #[test]
    #[should_panic(expected = "chaos_slowdown_max_factor must be finite and >= 1.0")]
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
}
