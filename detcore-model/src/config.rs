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
use nix::errno;
use nix::sys::signal::Signal;
use serde::de;
use serde::Deserialize;
use serde::Serialize;
use serde::Serializer;

use crate::pid::DetTid;

/// Configuration options for detcore.
#[derive(Debug, Serialize, Deserialize, Clone, Parser)]
pub struct Config {
    /// Disable virtual/logical time. Note that virtual time is required for virtual metadata.
    #[clap(long = "no-virtualize-time", parse(from_flag = std::ops::Not::not))]
    pub virtualize_time: bool,

    /// Disable virtual cpuid
    #[clap(long = "no-virtualize-cpuid", parse(from_flag = std::ops::Not::not))]
    pub virtualize_cpuid: bool,

    /// Epoch of the logical time.
    ///
    /// This is the datetime from which all time and date modtimes begin and
    /// monotonically increase. It is in RFC3339 format such as: 1999-12-31T23:59:59Z"
    #[clap(
        long,
        env = "HERMIT_EPOCH",
        value_name = "YYYY-MM-DDThh:mm:ssZ",
        default_value = DEFAULT_EPOCH_STR
    )]
    pub epoch: DateTime<Utc>,

    /// Use this number to seed the PRNG that supplies randomness to the guest.
    /// This supplies guest system calls that expose randomness, as well as
    /// the `/dev/[u]random` files. It does not affect the `rdrand` instruction,
    /// which is disabled in the guest.
    #[clap(long, env = "HERMIT_PRNG", default_value = "0", value_name = "uint64")]
    pub seed: u64,

    /// Logical clock multiplier. Values above one make time appear to go faster within the sandbox.
    #[clap(long, value_name = "float")]
    pub clock_multiplier: Option<f64>,

    /// Disable substitution of virtual (deterministic) file metadata, in lieu
    /// of the real metadata returned by `fstat`, implies `virtualize_time`.
    #[clap(long = "no-virtualize-metadata", parse(from_flag = std::ops::Not::not))]
    pub virtualize_metadata: bool,

    /// Sequentialize thread execution deterministically.
    #[clap(long)]
    pub sequentialize_threads: bool,

    /// In chaos mode, uses much cheaper approximate preemption timers.  Only makes sense
    /// when recording preemptions for later (precise) replay.
    #[clap(long)]
    pub imprecise_timers: bool,

    /// Schedule threads chaotically.
    ///
    /// The behavior of this flag is subject to change. Current behavior is to randomize thread
    /// priorities at every timeout caused by the `--preemption-timeout`.  Other randomization
    /// strategies are possible with `--sched-heuristic`.
    ///
    /// Thread scheduling remains deterministic, determined by the random seed.
    #[clap(long)]
    pub chaos: bool,

    /// Record the timing of preemption events for future replay or experimentation.
    /// This is only useful in chaos modes.
    #[clap(long)]
    pub record_preemptions: bool,

    /// File to write the record of preemptions (in JSON).  Implies `--record-preemptions`.
    #[clap(long, value_name = "filepath")]
    pub record_preemptions_to: Option<PathBuf>,

    /// JSON file to read recorded preemptions from.  When `--chaos` mode is activated, these
    /// recorded preemption points take the place of randomized scheduling decisions.
    #[clap(long, value_name = "filepath", conflicts_with = "replay-schedule-from")]
    pub replay_preemptions_from: Option<PathBuf>,

    /// File to read recorded schedule trace from. This execution will replay the schedule verbatim
    /// from the file.
    #[clap(
        long,
        value_name = "filepath",
        conflicts_with = "replay-preemptions-from"
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
           parse(try_from_str = parse_index_with_path))]
    pub stacktrace_event: Vec<(u64, Option<PathBuf>)>,

    /// Internal feature used to signal the guest with SIGINT at every `--stacktrace-event`, this is
    /// in-lieu of using hermit's internal stacktrace printing facility, to instead have an external
    /// debugger handle it.  Accepts either signal names or numbers.
    #[clap(long, value_name = "signame", parse(try_from_str))]
    pub stacktrace_signal: Option<SigWrapper>,

    /// [DEPRECATED] Print a stacktrace each time the program is preempted.  Only makes sense in `--chaos` mode
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

    /// DANGEROUS: Panic on unsupported syscalls, this is useful for
    /// debugging detcore itself, not recommended otherwise.
    #[clap(long)]
    pub panic_on_unsupported_syscalls: bool,

    /// [Internal] Set to `true` if we're inside a UTS namespace.
    // FIXME: This can be removed once spawn_fn-based tests support namespaces.
    #[clap(skip)]
    pub has_uts_namespace: bool,

    /// [Internal] Path to the replay data folder.
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

    /// Configure the longest time slice for which a guest thread should be allowed to run
    /// uninterrupted. This uses the unit of "virtual nanoseconds", and is implemented using
    /// retired conditional branche (RCB) counting.
    ///
    /// To disable preemption based on RCB count, set`preemption_timeout` to "disabled" or "0".
    /// Note: Set `preemption_timeout` to a non-zero value requires hardware performance counters.
    #[clap(long,
                value_name = "uint64|'disabled'",
                default_value = "200000000",
                parse(try_from_str = parse_preemption_timeout))]
    pub preemption_timeout: MaybePreemptionTimeout,

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

    /// [Internal] An internal flag for indicating to Detcore whether we are in `hermit record` or
    /// `hermit replay` mode.  This is necessary because there are DIFFERENT global
    /// invariants in record mode (e.g. files dont exist).  If we move to a chroot model
    /// and reproduce more, recording less, then this flag should become obsolete.
    #[clap(skip = false)]
    pub recordreplay_modes: bool,

    /// [Internal] debugging option to stop execution after a specific scheduler commit, aka turn number
    /// (non-negative integer). This only makes sense if `--sequentialize-threads` is specified, as the scheduler is otherwise not engaged.
    #[clap(long, value_name = "turn_N")]
    pub stop_after_turn: Option<u64>,

    /// [Internal] debugging option to stop execution after a scheduler loop iteration (non-negative integer).
    /// This only makes sense if `--sequentialize-threads` is specified, as the scheduler is otherwise not engaged.
    #[clap(long, value_name = "iter_N")]
    pub stop_after_iter: Option<u64>,

    /// [Internal] Debugging option to treat all sockets as mysterious external, nondeterministic
    /// entities, rather than container-internal and determinstically scheduled.
    #[clap(long)]
    pub debug_externalize_sockets: bool,

    /// [Internal] Debugging option to change how futexes are implemented, either precisely modeled
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
    /// preemption-timeout is specified.
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
    #[clap(long, default_value = "1GB", parse(try_from_str = try_parse_memory), value_name = "bytesize")]
    pub memory: u64,

    /// Configure extra interrupt points based on thread id and rcb counter. Detcore will raise a precise
    /// timer for this RCB whenever it detects that current current thread timeslice intercects any of the
    /// interrupt points specified
    #[clap(long, value_name = "tid:rcbs", parse(try_from_str = try_parse_numbers_with_colon))]
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
    /// Sanity check the flags, and update any wherever flag B is implied by A.
    pub fn validate(&mut self) {
        assert!(self.sched_sticky_random_param >= 0.0);
        assert!(self.sched_sticky_random_param <= 1.0);

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
            tracing::warn!(
                "Setting --imprecise timers with --replay-preemptions-from is probably not what you want. They won't replay precisely."
            );
        }

        if self.stop_after_turn.is_some() && !self.sequentialize_threads {
            tracing::warn!(
                "--stop-after-turn will have no effect if --no-sequentialize-threads is enabled"
            );
            self.stop_after_turn = None;
        }
        if self.stop_after_iter.is_some() && !self.sequentialize_threads {
            tracing::warn!(
                "--stop-after-iter will have no effect if --no-sequentialize-threads is enabled"
            );
            self.stop_after_iter = None;
        }

        if self.debug_externalize_sockets && !self.sequentialize_threads {
            tracing::warn!(
                "--debug-externalize-sockets will have no effect if --no-sequentialize-threads is enabled"
            );
            self.debug_externalize_sockets = false;
        }

        if self.preemption_stacktrace_log_file.is_some() {
            self.preemption_stacktrace = true;
        }
    }

    /// Should we use RCB in computing logical time?
    ///
    /// The answer is NO either if `--no-rcb-time` is specified or if HW counters are disabled by
    /// setting `--preemption-timeout=disabled`.
    pub fn use_rcb_time(&self) -> bool {
        self.preemption_timeout.is_some() && !self.no_rcb_time
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
    /// Precisely model the [un]blocking behavior inside hermit.
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Parser, PartialEq, Eq)]
/// Apply a specialized scheduling heuristic which may help exercise certain bugs.
pub enum SchedHeuristic {
    /// Don't modify the scheduling algorithm.
    // TODO: Is the default a round robin?
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

impl Default for SchedHeuristic {
    fn default() -> Self {
        SchedHeuristic::None
    }
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

/// If this is set to None, the RCB (retired conditional branch) hardware counter feature is disabled.
///
/// Limitations with clap require a type alias here.
pub type MaybePreemptionTimeout = Option<NonZeroU64>;

fn parse_preemption_timeout(
    src: &str,
) -> Result<MaybePreemptionTimeout, ParsePreemptionTimeoutError> {
    if let Ok(n) = src.parse::<u64>() {
        Ok(NonZeroU64::new(n))
    } else {
        match src {
            "disabled" => Ok(None),
            _ => Err(ParsePreemptionTimeoutError::new(
                "Unable to parse string as preemption-timeout, expected 'disabled' or an non-negative integer",
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
struct ParsePreemptionTimeoutError {
    details: String,
}

impl fmt::Display for ParsePreemptionTimeoutError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.details)
    }
}

impl ParsePreemptionTimeoutError {
    fn new(msg: &str) -> ParsePreemptionTimeoutError {
        ParsePreemptionTimeoutError {
            details: msg.to_string(),
        }
    }
}

impl std::error::Error for ParsePreemptionTimeoutError {
    fn description(&self) -> &str {
        &self.details
    }
}

/// The default epoch used by DetCore for things like initial file modtimes.
///
/// N.B. Default to a reasonable date. Some programs (like zip) have trouble with the
/// original unix epoch (time zero).
pub static DEFAULT_EPOCH_STR: &str = "1999-12-31T23:59:59Z";

impl Config {
    /// Construct the config using environment variables only, not CLI args.
    pub fn from_env() -> Self {
        let args: [OsString; 2] = [
            OsString::from("CMD"), // Silly/unused.
            OsString::from(format!("--epoch={}", DEFAULT_EPOCH_STR)),
        ];
        Config::from_iter(args.iter())
    }
}

/// N.B. we don't want to specify two different notions of "default", so we use the
/// `Clap` instance above.
impl Default for Config {
    fn default() -> Self {
        let v: Vec<String> = vec![];
        Config::from_iter(v.iter())
    }
}

/// Simply a type to hang Serialize/Deserialize instances off of.
#[derive(PartialEq, Debug, Eq, Clone, Hash)]
pub struct SigWrapper(pub Signal);

impl Serialize for SigWrapper {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.0.as_str())
    }
}

impl<'de> de::Deserialize<'de> for SigWrapper {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        struct SignalVisitor;
        impl<'de> de::Visitor<'de> for SignalVisitor {
            type Value = Signal;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                write!(f, "string representing a Signal")
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                let sig: Result<Signal, String> =
                    FromStr::from_str(v).map_err(|e: errno::Errno| e.to_string());
                sig.map_err(de::Error::custom)
            }
        }

        let sig = deserializer.deserialize_str(SignalVisitor)?;
        Ok(SigWrapper(sig))
    }
}

impl FromStr for SigWrapper {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        Ok(SigWrapper(Signal::from_str(s)?))
    }
}
