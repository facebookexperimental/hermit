// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

use std::collections::HashMap;
use std::fmt;
use std::ops::Add;
use std::ops::Sub;
use std::time::Duration;

use chrono::DateTime;
use chrono::Utc;
use reverie_syscalls::Timespec;
use reverie_syscalls::Timeval;
use serde::Deserialize;
use serde::Serialize;
use tracing::trace;

use crate::config::Config;
use crate::pid::DetTid;

// Time conversion constants from https://doc.rust-lang.org/stable/src/core/time.rs.html#26-30
const NANOS_PER_SEC: u64 = 1_000_000_000;
const NANOS_PER_MILLI: u64 = 1_000_000;
const NANOS_PER_MICRO: u64 = 1_000;
const MILLIS_PER_SEC: u64 = 1_000;
const MICROS_PER_SEC: u64 = 1_000_000;

// TODO: make all of these integral types to rule out fractional values.

/// Virtual nanoseconds elapsed per system call (uniform for now).
pub const NANOS_PER_SYSCALL: f64 = 10000.0;

/// Virtual nanoseconds elapsed per Retired Conditional Branch.
pub const NANOS_PER_RCB: f64 = 10.0;

/// Virtual nanoseconds elapsed per nondeterministic instruction other than system calls.
pub const NANOS_PER_NONDET_INSTR: f64 = 25.0;

/// Virtual nanoseconds elapsed per step of the scheduler.
pub const NANOS_PER_SCHED: f64 = 500_000.0;

// TODO: should map addresses to physical addresses.

// Deterministic Time:
//--------------------------------------------------------------------------------

/// Represents an absolute point in time in nanoseconds.
/// Parts of this API are largely inspired by `std::time::Duration`.
/// This could go to 128 bits if we need more than ~585 years of nanosecond precision.
#[derive(
    Default,
    Debug,
    Clone,
    Copy,
    Serialize,
    Deserialize,
    Ord,
    PartialOrd,
    Eq,
    PartialEq,
    Hash
)]
pub struct LogicalTime(u64);

impl LogicalTime {
    /// 0 integer nanoseconds.
    pub const ZERO: LogicalTime = LogicalTime(0);
    /// The maximum representable integer nanoseconds.
    pub const MAX: LogicalTime = LogicalTime(u64::MAX);

    /// Returns the total number of whole microseconds contained by this `LogicalTime`.
    pub fn as_micros(&self) -> u64 {
        self.0 / (NANOS_PER_MICRO as u64)
    }

    /// Returns the total number of whole milliseconds contained by this `LogicalTime`.
    pub fn as_millis(&self) -> u64 {
        self.0 / (NANOS_PER_MILLI as u64)
    }

    /// Returns the total number of nanoseconds contained by this `LogicalTime`.
    pub fn as_nanos(&self) -> u64 {
        self.0
    }

    /// Returns the total number of *whole* seconds contained by this `LogicalTime`.
    /// The returned value does not include fractional (nanosecond) part of the duration,
    /// which can be obtained using `subsec_nanos`.
    pub fn as_secs(&self) -> u64 {
        self.0 / (NANOS_PER_SEC as u64)
    }

    /// Creates a new `LogicalTime` from the specified number of microseconds.
    pub fn from_micros(micros: u64) -> Self {
        LogicalTime(micros * (NANOS_PER_MICRO as u64))
    }

    /// Creates a new `LogicalTime` from the specified number of milliseconds.
    pub fn from_millis(millis: u64) -> Self {
        LogicalTime(millis * (NANOS_PER_MILLI as u64))
    }

    /// Creates a new `LogicalTime` from the specified number of nanoseconds.
    pub fn from_nanos(nanos: u64) -> Self {
        LogicalTime(nanos)
    }

    /// Creates a new `LogicalTime` from the specified number of nanoseconds.
    pub fn from_big_nanos(nanos: u128) -> Self {
        // No good solution for this until we change the internal rep to 128 bit:
        LogicalTime(nanos as u64)
    }

    /// Creates a new `LogicalTime` from the specified number of seconds.
    pub fn from_secs(secs: u64) -> Self {
        LogicalTime(secs * (NANOS_PER_SEC as u64))
    }

    /// Returns the fractional part of this `LogicalTime`, in microseconds.
    /// This method does not return the length of the duration when represented by microseconds. The returned number always represents
    /// a fractional portion of a second (i.e., it is less than one million).
    pub fn subsec_micros(&self) -> u32 {
        (self.0 % (MICROS_PER_SEC as u64)) as u32
    }

    /// Returns the fractional part of this `LogicalTime`, in milliseconds.
    /// This method does not return the length of the duration when represented by milliseconds. The returned number always represents
    /// a fractional portion of a second (i.e., it is less than one thousand).
    pub fn subsec_millis(&self) -> u32 {
        (self.0 % (MILLIS_PER_SEC as u64)) as u32
    }

    /// Returns the fractional part of this `LogicalTime`, in nanoseconds.
    /// This method does not return the length of the duration when represented by nanoseconds. The returned number always represents
    /// a fractional portion of a second (i.e., it is less than one billion).
    pub fn subsec_nanos(&self) -> u32 {
        (self.0 % (NANOS_PER_SEC as u64)) as u32
    }

    /// Convert a number of Retired Conditional Branches (RCBs) to Nanoseconds
    pub fn from_rcbs(n: u64) -> Self {
        LogicalTime((n as f64 * NANOS_PER_RCB) as u64)
    }

    /// Inverse of from_rcbs.  Non-injective, as it loses information, truncating to a
    /// coarser grained unit of time.
    pub fn into_rcbs(self) -> u64 {
        (self.0 as f64 / NANOS_PER_RCB) as u64
    }

    /// Test if the quantity is zero nanoseconds.
    pub fn is_zero(&self) -> bool {
        self.0 == 0
    }

    /// Measure the duration of the time interval since a previous time.
    pub fn duration_since(&self, from: LogicalTime) -> Duration {
        if from.0 > self.0 {
            panic!(
                "LogicalTime::duration_since cannot take duration since a time in the *future* ({}), relative to {}",
                from, self
            );
        }
        Duration::from_nanos(self.0 - from.0)
    }
}

impl std::fmt::Display for LogicalTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Start with the raw characters for printed u64:
        let chars = format!("{}", self.0);
        let mut remain = chars.len();
        let mut first_char = true;
        for ch in chars.chars() {
            if !first_char && remain % 3 == 0 {
                if remain == 9 {
                    write!(f, ".")?;
                } else {
                    // Could also consider \u{2009} thin space here, but it prints fixed
                    // width in most terminals anyway:
                    write!(f, "_")?;
                }
            }
            first_char = false;
            remain -= 1;
            write!(f, "{}", ch)?;
        }
        if chars.len() <= 9 {
            write!(f, "ns")
        } else {
            write!(f, "s")
        }
    }
}

impl Add for LogicalTime {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        LogicalTime(self.0 + rhs.0)
    }
}

impl Add<Duration> for LogicalTime {
    type Output = Self;
    fn add(self, rhs: Duration) -> Self {
        // Cast will be unsafe if rhs ever exceeds u64::MAX nanosecs
        LogicalTime(self.0 + rhs.as_nanos() as u64)
    }
}

impl Add<u128> for LogicalTime {
    type Output = Self;
    fn add(self, rhs: u128) -> Self {
        // Maybe this will be total in the future, but for now it can fail:
        LogicalTime(self.0 + rhs as u64)
    }
}

impl Sub for LogicalTime {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        LogicalTime(self.0 - rhs.0)
    }
}

impl From<LogicalTime> for Timespec {
    fn from(logical_time: LogicalTime) -> Timespec {
        Timespec {
            tv_sec: logical_time.as_secs() as i64,
            tv_nsec: logical_time.subsec_nanos() as i64,
        }
    }
}

impl From<LogicalTime> for Timeval {
    fn from(logical_time: LogicalTime) -> Timeval {
        Timeval {
            tv_sec: logical_time.as_secs() as i64,
            tv_usec: logical_time.subsec_micros() as i64,
        }
    }
}

#[test]
fn print_nanoseconds() {
    let ns1 = LogicalTime(946_684_799_000_000_000);
    let ns2 = LogicalTime(729_860_000);
    assert_eq!(format!("{}", ns1), "946_684_799.000_000_000s");
    assert_eq!(format!("{}", ns1 + ns2), "946_684_799.729_860_000s");
    assert_eq!(format!("{}", ns2), "729_860_000ns");
}

/// The same basic type alias as nanoseconds. Just for clarity/readability.
pub type Microseconds = u64;

/// A determinstic notion of time, measuring progress of one thread.
///
/// It is based on counting syscalls and conditionals.  Ideally it would count
/// instructions, but that is not possible deterministically.
///
/// `DetTime` is a measure of the LOCAL progress of a thread. The notion of
/// global time is defined by aggretion of multiple local times (vector
/// clocks, as in the Kendo algorithm).
///
/// Here are a few relevant definitions for deterministic time:
///
/// **Granularity**
///
/// How rapidly and consistently does the clock tick with thread progress?
/// A finer notion of deterministic time counts *more* events (ideally instructions).
/// A coarser notion of deterministic time counts fewer events (like syscalls).
///
/// **Productivity**
///
/// In corecursion, or coinductive datatypes like streams, productivity means you can get
/// the next result with finite work. Deterministic time can be viewed a stream of "tick
/// events". We don't want a guest thread to be able to do an unbounded amount of work,
/// without a tick occurring. For example, retired branches are a safe bet (any
/// non-trivial amount of work will execute a branch), but system calls are not: a
/// spinning thread can burn cycles forever without executing a syscall.
///
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetTime {
    /// The syscalls issued by this thread.
    syscalls: u64,

    /// Retired conditional branches, as given "opaquely" by the reverie clock.
    /// Technically, that these are RCBs is an implementation detail of reverie.
    rcbs: u64,

    /// Number of nondeterministic instructions (rdtsc, cpuid)
    nondet_instrs: u64,

    /// Baseline amount of time to add.
    starting_micros: Microseconds,

    /// Multiplier for all time advances.
    multiplier: f64,
}

// Don't derive Default because it would give us a 0.0 multiplier:
impl Default for DetTime {
    fn default() -> Self {
        DetTime {
            syscalls: 0,
            rcbs: 0,
            nondet_instrs: 0,
            starting_micros: 0,
            multiplier: 1.0,
        }
    }
}

impl Eq for DetTime {}

/// `DetTime` behaves as a totally ordered scalar.
impl Ord for DetTime {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let nanos = other.as_nanos();
        self.as_nanos().cmp(&nanos)
    }
}

impl PartialOrd for DetTime {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        let nanos = other.as_nanos();
        self.as_nanos().partial_cmp(&nanos)
    }
}

impl PartialEq for DetTime {
    fn eq(&self, other: &Self) -> bool {
        self.as_nanos() == other.as_nanos()
    }
}

impl From<&DateTime<Utc>> for DetTime {
    fn from(dt: &DateTime<Utc>) -> Self {
        DetTime {
            syscalls: 0,
            rcbs: 0,
            nondet_instrs: 0,
            starting_micros: micros_from_utc(dt),
            multiplier: 1.0,
        }
    }
}

fn micros_from_utc(dt: &DateTime<Utc>) -> Microseconds {
    dt.timestamp() as Microseconds * 1_000_000 + dt.timestamp_subsec_micros() as Microseconds
}

// implementing From<DetTime> for Timespec is not possible due to dependency graph
#[allow(clippy::from_over_into)]
impl Into<Timespec> for DetTime {
    fn into(self) -> Timespec {
        self.as_nanos().into()
    }
}

impl DetTime {
    /// Create an initial `DetTime` respecting a `Config`
    pub fn new(cfg: &Config) -> Self {
        // We inflate the amount of time everything consumes to compensate for the fact that we
        // are ONLY counting certain events sparsely. In theory this should be based on some kind
        // of expected value for compute-between-syscalls on average applications. (But is that
        // even a normal distribution?)
        let additional_multiplier = if cfg.sequentialize_threads && !cfg.use_rcb_time() {
            500.0
        } else {
            // Otherwise, virtual time isn't really used for scheduling, just for
            // metadata, so it doesn't really matter what the rate of ticking is.
            1.0
        };
        match cfg.clock_multiplier {
            Some(m) => DetTime::from(&cfg.epoch).with_multiplier(m * additional_multiplier),
            None => DetTime::from(&cfg.epoch).with_multiplier(additional_multiplier),
        }
    }

    /// Create a new `Dettime` which is the earliest possible.
    pub fn zero() -> Self {
        DetTime {
            syscalls: 0,
            rcbs: 0,
            nondet_instrs: 0,
            starting_micros: 0,
            multiplier: 1.0,
        }
    }

    /// Register that another syscall has executed.
    pub fn add_syscall(&mut self) {
        self.syscalls += 1;
        trace!(
            "[detcore] added syscall to logical time, yielding: {:?}",
            self
        );
    }

    /// Register that an `rdtsc` intsruction has executed.
    pub fn add_rdtsc(&mut self) {
        self.nondet_instrs += 1;
    }

    /// Register that an `cpuid` intsruction has executed.
    pub fn add_cpuid(&mut self) {
        self.nondet_instrs += 1;
    }

    /// Update internal counts using the reverie clock value.
    pub fn add_rcbs(&mut self, count: u64) {
        self.rcbs += count;
    }

    /// Return current rcbs
    pub fn rcbs(&self) -> u64 {
        self.rcbs
    }

    /// Project deterministic logical time into a rough number of nanoseconds.
    pub fn as_nanos(&self) -> LogicalTime {
        // Note: these counts could be pre-collapsed into scalar within the DetTime
        // representation.  But currently we leave them separate for debuggability.
        LogicalTime(
            (self.starting_micros * 1000) as u64
                + ((self.syscalls as f64 * NANOS_PER_SYSCALL * self.multiplier) as u64)
                + ((self.rcbs as f64 * NANOS_PER_RCB * self.multiplier) as u64)
                + ((self.nondet_instrs as f64 * NANOS_PER_NONDET_INSTR * self.multiplier) as u64),
        )
    }

    /// Project deterministic logical time into a rough number of microseconds.
    pub fn as_micros(&self) -> Microseconds {
        self.as_nanos().0 / 1000
    }

    /// Set the clock multiplier
    pub fn with_multiplier(mut self, m: f64) -> Self {
        self.multiplier = m;
        self
    }

    /// Project deterministic time duration from imaginary starting point of deterministic time creation
    pub fn as_duration(&self) -> std::time::Duration {
        std::time::Duration::from_nanos(self.as_nanos().0 - self.starting_micros * 1000)
    }
}

/// Deterministic global time, combining local times.
#[derive(Default, Debug, Clone, Serialize, Deserialize)]
pub struct GlobalTime {
    /// The time when the container began execution.
    starting_nanos: LogicalTime,

    /// The local work performed by each threads, quantified as virtual
    /// nanoseconds, NOT including `starting_micros` and not including time
    /// waiting.
    time_vector: HashMap<DetTid, LogicalTime>,

    /// A source of central, logically external, time passage generated by the scheduler.
    extra_time: LogicalTime,

    /// This won't always be possible, but for now, since local clocks don't tick
    /// asynchronously, it is quite straightforward to keep a count of cumulative progress.
    total: LogicalTime,

    /// Immutable. Simply copied from the Config.
    multiplier: f64,
}

impl GlobalTime {
    /// Create a fresh global time, respecting the `Config`.
    pub fn new(cfg: &Config) -> Self {
        let base = DetTime::new(cfg);
        GlobalTime {
            starting_nanos: LogicalTime::from_micros(micros_from_utc(&cfg.epoch)),
            time_vector: HashMap::new(),
            extra_time: LogicalTime::from_nanos(0),
            total: base.as_nanos(),
            multiplier: cfg.clock_multiplier.unwrap_or(1.0),
        }
    }

    /// Tick the time of a particular thread.
    pub fn update_global_time(&mut self, tid: DetTid, newtime: LogicalTime) {
        trace!(
            "[tid {}] ticked its global time component to {}",
            tid,
            newtime,
        );

        if newtime < self.starting_nanos {
            panic!(
                "update_global_time: Cannot set thread {} time to {}, which is before start of container execution {}",
                tid, newtime, self.starting_nanos
            );
        }

        // TODO(T136359599): change to duration_since, and store durations in time_vector:
        let newtime = newtime - self.starting_nanos;
        if let Some(old) = self.time_vector.insert(tid, newtime) {
            if old > newtime {
                panic!(
                    "Attempted to update tid {} time to {}, but was already {}",
                    tid, newtime, old
                );
            }
            // Update the cached total for efficiency:
            let LogicalTime(diff) = newtime - old;
            self.bump_total(Duration::from_nanos(diff));
        } else {
            // Don't add starting_nanos in because it's already accounted for
            // and we don't want to count it multiple times anyway:
            self.bump_total(Duration::from_nanos(newtime.0));
        }
    }

    fn sanity(&self) {
        debug_assert_eq!(self.sum_up(), self.total);
    }

    fn bump_total(&mut self, delta: Duration) {
        self.total = self.total + delta;
        self.sanity();
    }

    // The expensive way to get the total (internal)
    fn sum_up(&self) -> LogicalTime {
        let mut sum = self.starting_nanos;
        for (_tid, tm) in self.time_vector.iter() {
            sum = sum + *tm;
        }
        sum + self.extra_time
    }

    /// Add time that passage is not driven by the internal events within guest threads.
    /// This is effectively used to account for "time" consumed by the scheduler, and to
    /// ensure monotonic increase of global time while scheduling.
    pub fn add_scheduler_time(&mut self) -> LogicalTime {
        let delta = Duration::from_nanos((NANOS_PER_SCHED * self.multiplier) as u64);
        self.add_extra_time(delta)
    }

    /// Update the global clock to account for time not driven by internal
    /// within guest threads.  This is a central or external expenditure of
    /// time, rather than a thread-internal one.
    ///
    /// The argument is in nanosecods and should have had any clock multiplier
    /// applied alreday.
    pub fn add_extra_time(&mut self, delta: Duration) -> LogicalTime {
        self.extra_time = self.extra_time + delta;
        // Update the cached total for efficiency:
        self.bump_total(delta);
        self.as_nanos()
    }

    /// Project out the time of a particular thread, which includes its own work only.
    pub fn threads_time(&self, dtid: DetTid) -> LogicalTime {
        self.starting_nanos
            + *self.time_vector.get(&dtid).unwrap_or_else(|| {
                panic!(
                    "Trying to extract time for thread {}, but no entry found!",
                    dtid
                )
            })
    }

    #[allow(unused)]
    /// Deterministic lower bound on the amount of work that has happened across all
    /// threads, starting with the same epoch time as individual thread clocks.
    ///
    /// This roughly models something like real time if all threads were running on one core.
    pub fn as_nanos(&self) -> LogicalTime {
        self.total
    }
}
