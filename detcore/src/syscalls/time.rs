/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! System calls for dealing with threads and concurrency.
use std::time::Duration;

use nix::sys::signal::Signal;
use reverie::Error;
use reverie::Guest;
use reverie::Stack;
use reverie::syscalls;
use reverie::syscalls::AddrMut;
use reverie::syscalls::ClockId;
use reverie::syscalls::Errno;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Syscall;
use reverie::syscalls::Timespec;
use reverie::syscalls::Timeval;
use reverie::syscalls::family::NanosleepFamily;
use tracing::error;
use tracing::info;
use tracing::trace;

use crate::detlog;
use crate::procmaps;
use crate::record_or_replay::RecordOrReplay;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::scheduler::Priority;
use crate::scheduler::entropy_to_priority;
use crate::tool_global::ResumeStatus;
use crate::tool_global::register_posix_timer;
use crate::tool_global::thread_observe_time;
use crate::tool_local::Detcore;
use crate::types::LogicalTime;

fn time_from_resources(rsrcs: &Resources) -> Option<LogicalTime> {
    if rsrcs.resources.len() > 1 {
        panic!(
            "time_from_resources: multiple resource ids in resource request: {:?}",
            rsrcs
        );
    }
    for rs in rsrcs.resources.iter() {
        if let (ResourceID::SleepUntil(tm), _) = rs {
            return Some(*tm);
        }
    }
    None
}

/// Flatten a `timespec` to nanoseconds. Negative fields are not valid for the
/// timer syscalls we handle; treat them as zero rather than panicking.
fn timespec_to_ns(ts: libc::timespec) -> u64 {
    let secs = ts.tv_sec.max(0) as u64;
    let nsec = ts.tv_nsec.max(0) as u64;
    secs.saturating_mul(1_000_000_000).saturating_add(nsec)
}

/// Inverse of [`timespec_to_ns`].
fn ns_to_timespec(ns: u64) -> libc::timespec {
    libc::timespec {
        tv_sec: (ns / 1_000_000_000) as libc::time_t,
        tv_nsec: (ns % 1_000_000_000) as libc::c_long,
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-857): Query-only timex mode boundary.
fn timex_mode_is_query(modes: libc::c_uint) -> bool {
    modes == 0 || modes == libc::ADJ_OFFSET_SS_READ
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-857): Host-independent NTP discipline snapshot.
fn deterministic_timex(now: Timespec) -> libc::timex {
    // SAFETY: `libc::timex` contains only integer fields and padding; zero is a
    // valid baseline for the fields not modeled by Hermit.
    let mut tx: libc::timex = unsafe { std::mem::zeroed() };
    tx.status = libc::STA_UNSYNC;
    tx.tick = 10_000;
    tx.time = libc::timeval {
        tv_sec: now.tv_sec,
        tv_usec: now.tv_nsec / 1_000,
    };
    tx
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-845): Review SaBRe thread-local guest clock reads.
pub(crate) async fn guest_clock_time<G, T>(guest: &mut G) -> LogicalTime
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let raw = thread_observe_time(guest).await;
    guest.thread_state().observe_guest_clock(raw)
}

/// Replacing host time in the `tv` of a `gettimeofday` that failed with EFAULT
/// could not finish. `tv` may still hold host wall-clock time, so this is a
/// failed run, not a guest errno.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TvRepairFailure {
    /// The word being stored: `tv_sec` or `tv_usec`.
    field: &'static str,
    kind: TvRepairFailureKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TvRepairFailureKind {
    /// The backend could not execute the `time(2)` probe. EFAULT is not a
    /// failure: it means Linux stopped before storing this word.
    ProbeFailed(Errno),
    /// After a store probe returned EFAULT, `time(NULL)`, which Linux cannot
    /// fail, failed too. Something other than the store, such as a seccomp
    /// filter, produced the EFAULT, so it says nothing about `tv`.
    ControlProbeFailed(Errno),
    /// A word at or after the one whose store probe returned EFAULT no longer
    /// reads as it did before the call, so the original call stored it and the
    /// EFAULT did not come from the store.
    StoppedWordChanged,
    /// A word at or after the one whose store probe returned EFAULT could not
    /// be read before or after the call, although every byte of it is mapped.
    /// The read failure may not be a fault: a seccomp filter can deny the
    /// read while the store went through, so nothing shows the word unstored.
    StoppedWordUnreadable,
    /// The guest's memory map, needed to tell an unmapped word from an
    /// unreadable one, could not be read or was empty.
    MapsUnavailable,
    /// The exact overwrite after a successful probe failed. The probe just
    /// stored host seconds, so the run cannot safely continue.
    OverwriteFailed(Errno),
    /// The one-shot overwrite reported a count other than the whole word.
    OverwriteCount { expected: usize, reported: usize },
    /// Adding the field offset to the guest's `timeval` address overflowed.
    AddressOverflow,
    /// A recorded EFAULT does not prove that this replay execution performed
    /// the original stores, so live probing would mix replayed and host state.
    ReplayMode,
}

impl std::fmt::Display for TvRepairFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "replacing host time in the {} of a failed gettimeofday: ",
            self.field
        )?;
        match self.kind {
            TvRepairFailureKind::ProbeFailed(errno) => {
                write!(f, "time(2) store probe failed: {errno}")
            }
            TvRepairFailureKind::ControlProbeFailed(errno) => write!(
                f,
                "time(NULL) control probe failed: {errno}, so the store probe's EFAULT was not a store fault"
            ),
            TvRepairFailureKind::StoppedWordChanged => f.write_str(
                "the word changed during the call although its store probe returned EFAULT",
            ),
            TvRepairFailureKind::StoppedWordUnreadable => f.write_str(
                "the word is mapped but could not be read, so its store probe's EFAULT is unconfirmed",
            ),
            TvRepairFailureKind::MapsUnavailable => {
                f.write_str("the guest's memory map could not be read")
            }
            TvRepairFailureKind::OverwriteFailed(errno) => {
                write!(f, "virtual-time overwrite failed: {errno}")
            }
            TvRepairFailureKind::OverwriteCount { expected, reported } => write!(
                f,
                "virtual-time overwrite reported {reported} of {expected} bytes"
            ),
            TvRepairFailureKind::AddressOverflow => f.write_str("field address overflowed"),
            TvRepairFailureKind::ReplayMode => {
                f.write_str("cannot probe a recorded or replayed EFAULT")
            }
        }
    }
}

impl std::error::Error for TvRepairFailure {}

fn tv_repair_error(field: &'static str, kind: TvRepairFailureKind) -> Error {
    Error::Tool(anyhow::Error::new(TvRepairFailure { field, kind }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeStoreProbe {
    Stored,
    Stopped,
}

fn classify_time_store_probe(
    field: &'static str,
    result: Result<i64, Errno>,
) -> Result<TimeStoreProbe, Error> {
    match result {
        Ok(_) => Ok(TimeStoreProbe::Stored),
        Err(Errno::EFAULT) => Ok(TimeStoreProbe::Stopped),
        Err(errno) => Err(tv_repair_error(
            field,
            TvRepairFailureKind::ProbeFailed(errno),
        )),
    }
}

/// `time(NULL)` stores nothing and cannot fail on Linux, so any error means
/// the injected `time(2)` never reached the native call.
fn require_native_time_control_probe(
    field: &'static str,
    result: Result<i64, Errno>,
) -> Result<(), Error> {
    match result {
        Ok(_) => Ok(()),
        Err(errno) => Err(tv_repair_error(
            field,
            TvRepairFailureKind::ControlProbeFailed(errno),
        )),
    }
}

/// How a word that Linux did not store reads after the call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StoppedWord {
    /// Readable, with the value it held before the call.
    Unchanged,
    /// Unreadable both before and after the call. This proves nothing by
    /// itself; see [`require_unmapped_unreadable_word`].
    Unreadable,
}

/// A word Linux did not store keeps both its contents and its readability.
fn require_unchanged_stopped_word(
    field: &'static str,
    before: Result<libc::time_t, Errno>,
    after: Result<libc::time_t, Errno>,
) -> Result<StoppedWord, Error> {
    match (before, after) {
        (Ok(before), Ok(after)) if before == after => Ok(StoppedWord::Unchanged),
        (Err(_), Err(_)) => Ok(StoppedWord::Unreadable),
        _ => Err(tv_repair_error(
            field,
            TvRepairFailureKind::StoppedWordChanged,
        )),
    }
}

/// Whether every byte of the `time_t` at `word` lies in one of `ranges`, the
/// half-open address ranges of the guest's mappings.
fn time_word_is_fully_mapped(mut ranges: Vec<(u64, u64)>, word: u64) -> bool {
    let Some(end) = word.checked_add(std::mem::size_of::<libc::time_t>() as u64) else {
        return false;
    };
    ranges.sort_unstable();
    let mut covered = word;
    for (start, stop) in ranges {
        if start <= covered && covered < stop {
            covered = stop;
            if covered >= end {
                return true;
            }
        }
    }
    false
}

/// An unreadable word is accepted as unstored only when part of it is
/// unmapped: Linux's eight-byte `put_user` faults there before storing any
/// byte. A failed read of a fully mapped word is not evidence of a fault: a
/// seccomp filter can deny the read while the store succeeds. Ptrace reads use
/// `FOLL_FORCE`, which bypasses page permissions, but some listed mappings
/// still refuse the read: file pages beyond end of file, hardware-poisoned
/// pages, `MADV_GUARD_INSTALL` guard regions, `VM_PFNMAP` mappings without an
/// `access` operation, missing pages of a userfaultfd region in SIGBUS mode,
/// and `[vsyscall]` in xonly mode. Backends
/// that read with `process_vm_readv` also cannot read `PROT_NONE` pages. A
/// `gettimeofday` whose store faults on any of these ends the run rather than
/// returning EFAULT. An empty map is refused because a filter that fakes a
/// successful zero-byte read would otherwise make every word look unmapped.
fn require_unmapped_unreadable_word(
    field: &'static str,
    maps: Result<Vec<(u64, u64)>, Error>,
    word: u64,
) -> Result<(), Error> {
    let ranges = match maps {
        Ok(ranges) if !ranges.is_empty() => ranges,
        Ok(_) => {
            error!("gettimeofday tv repair: the guest's memory map is empty");
            return Err(tv_repair_error(field, TvRepairFailureKind::MapsUnavailable));
        }
        Err(err) => {
            error!("gettimeofday tv repair: reading the guest's memory map: {err}");
            return Err(tv_repair_error(field, TvRepairFailureKind::MapsUnavailable));
        }
    };
    if time_word_is_fully_mapped(ranges, word) {
        Err(tv_repair_error(
            field,
            TvRepairFailureKind::StoppedWordUnreadable,
        ))
    } else {
        Ok(())
    }
}

/// What Detcore's memory reader returned for `tv_sec` and `tv_usec` before a
/// `gettimeofday`, with the read error where it failed. Under ptrace the read
/// ignores page protection and protection keys; backends that read with
/// `process_vm_readv` respect `VM_READ`.
type TimevalWordSnapshot = [Result<libc::time_t, Errno>; 2];

const TIMEVAL_WORDS: [(&str, usize); 2] = [
    ("tv_sec", std::mem::offset_of!(Timeval, tv_sec)),
    ("tv_usec", std::mem::offset_of!(Timeval, tv_usec)),
];

fn snapshot_timeval_words<'a, G, T>(
    guest: &mut G,
    tv_addr: AddrMut<'a, Timeval>,
) -> TimevalWordSnapshot
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    TIMEVAL_WORDS.map(|(field, offset)| {
        let addr = timeval_word_addr(field, tv_addr, offset).map_err(|_| Errno::EFAULT)?;
        guest.memory().read_value(addr)
    })
}

fn timeval_word_addr<'a>(
    field: &'static str,
    tv_addr: AddrMut<'a, Timeval>,
    offset: usize,
) -> Result<AddrMut<'a, libc::time_t>, Error> {
    tv_addr
        .as_raw()
        .checked_add(offset)
        .and_then(AddrMut::<libc::time_t>::from_raw)
        .ok_or_else(|| tv_repair_error(field, TvRepairFailureKind::AddressOverflow))
}

fn require_complete_time_word_overwrite(
    field: &'static str,
    expected: usize,
    result: Result<usize, Errno>,
) -> Result<(), Error> {
    match result {
        Ok(reported) if reported == expected => Ok(()),
        Ok(reported) => Err(tv_repair_error(
            field,
            TvRepairFailureKind::OverwriteCount { expected, reported },
        )),
        Err(errno) => Err(tv_repair_error(
            field,
            TvRepairFailureKind::OverwriteFailed(errno),
        )),
    }
}

fn require_live_time_store_probe(replay_data_is_some: bool) -> Result<(), Error> {
    if replay_data_is_some {
        Err(tv_repair_error("tv", TvRepairFailureKind::ReplayMode))
    } else {
        Ok(())
    }
}

fn should_repair_failed_gettimeofday_tv(backend_is_kvm: bool) -> bool {
    !backend_is_kvm
}

/// Replaces the host wall-clock time that a `gettimeofday` failing with EFAULT
/// may have stored in `tv` with virtual time, in exactly the words Linux
/// stored.
///
/// Linux stores `tv_sec`, then `tv_usec`, each with one eight-byte `put_user`,
/// and only then copies `tz`; the first fault ends the call with EFAULT. A
/// store that faults on either page it touches commits nothing, so each word
/// is stored whole or not at all, and nothing after an unstored word is
/// attempted. `time(2)` uses the same eight-byte `put_user` store for its
/// `tloc`, so the repair injects it at each word in kernel order. EFAULT means
/// the original call stopped at that word too. Success means the probe itself
/// just stored host seconds in those exact bytes; only then does Hermit replace
/// the word with its virtual value. Other probe errors and overwrite failures
/// fail closed because host time may remain.
///
/// An EFAULT from the probe is only evidence about `tv` if it came from the
/// store. A seccomp filter can return EFAULT for `time(2)` without running it,
/// and would otherwise end the repair while host time remains in a writable
/// `tv`. Two checks therefore confirm every EFAULT before it is trusted.
/// First, `time(NULL)` is injected: it stores nothing and cannot fail on
/// Linux, so any error means the probes are not reaching the native call.
/// Second, every word from the stopped one onward must still read exactly as
/// it did before the original call: readable with the same value, or
/// unreadable both times and not fully mapped, so that the store could only
/// have faulted. A changed word was stored by that call, and an unreadable
/// word that is fully mapped may have been, because a failed read is not
/// itself a fault. Each of these failures ends the run, including for the
/// mapped but unreadable pages listed at `require_unmapped_unreadable_word`.
///
/// These checks are observations Detcore makes with its own syscalls, so they
/// trust those syscalls' results, as every Detcore handler that reads back a
/// result does. They catch a filter that makes a syscall fail, whether it is
/// the guest's probe or Detcore's own read. A seccomp filter installed on
/// Hermit itself can also make one of Detcore's own syscalls report success
/// without running it, and that is outside this guarantee. A guest cannot
/// install a filter: Detcore refuses `seccomp(2)` and `PR_SET_SECCOMP`.
///
/// The repair never writes a word the kernel could not store, and it does not
/// test writability by rewriting a word, because a remote write ignores
/// protection keys that deny the guest's own stores. The pre-call snapshot is
/// a read only. The host seconds written by a successful
/// probe exist transiently until the overwrite. Default thread
/// sequentialization prevents another guest thread from observing that
/// interval, but another process sharing the page can observe it; such
/// external shared state is outside Hermit's determinism guarantee.
/// Standard record/replay disables time virtualization, so this handler is not
/// used there. A custom configuration that combines replay data with virtual
/// time fails closed before probing: a recorded EFAULT does not establish that
/// this execution performed any stores for a live `time(2)` to reproduce.
/// KVM does not call this repair: its executor implements `gettimeofday`
/// directly with an all-zero timeval, so it never stores host time, and it
/// does not implement the injected `time(2)` probe. Its original EFAULT is
/// therefore already safe and must be returned unchanged.
async fn overwrite_failed_gettimeofday_tv<'a, G, T>(
    guest: &mut G,
    tv_addr: AddrMut<'a, Timeval>,
    tv: &Timeval,
    before: &TimevalWordSnapshot,
) -> Result<(), Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let values = [tv.tv_sec as libc::time_t, tv.tv_usec as libc::time_t];
    for (index, (field, offset)) in TIMEVAL_WORDS.into_iter().enumerate() {
        let addr = timeval_word_addr(field, tv_addr, offset)?;
        let probe = syscalls::Time::new().with_tloc(Some(addr));
        match classify_time_store_probe(field, guest.inject(probe).await)? {
            TimeStoreProbe::Stored => {
                let bytes = values[index].to_ne_bytes();
                let overwrite = guest
                    .memory()
                    .write_with_user_access(addr.cast::<u8>(), &bytes);
                require_complete_time_word_overwrite(field, bytes.len(), overwrite)?;
            }
            TimeStoreProbe::Stopped => {
                let control = syscalls::Time::new().with_tloc(None);
                require_native_time_control_probe(field, guest.inject(control).await)?;
                for (stopped, (field, offset)) in TIMEVAL_WORDS.into_iter().enumerate().skip(index)
                {
                    let addr = timeval_word_addr(field, tv_addr, offset)?;
                    let after = guest.memory().read_value(addr);
                    match require_unchanged_stopped_word(field, before[stopped], after)? {
                        StoppedWord::Unchanged => {}
                        StoppedWord::Unreadable => {
                            let maps = procmaps::from_pid(guest.pid(), |_| true)
                                .map(|maps| maps.into_iter().map(|map| map.address).collect());
                            require_unmapped_unreadable_word(field, maps, addr.as_raw() as u64)?;
                        }
                    }
                }
                break;
            }
        }
    }
    Ok(())
}

fn remaining_sleep_duration(target: LogicalTime, now: LogicalTime) -> Duration {
    if target > now {
        target.duration_since(now)
    } else {
        Duration::ZERO
    }
}

impl<T: RecordOrReplay> Detcore<T> {
    /// Convenience function for constructing a sleep request with a nanosecond offset from "now".
    pub async fn sleep_request<G: Guest<Self>>(guest: &mut G, ns_delta: Duration) -> Resources {
        let base_time = thread_observe_time(guest).await;
        let target_time = base_time + ns_delta;
        let resource = ResourceID::SleepUntil(target_time);
        guest.thread_state().mk_request(resource, Permission::W)
    }

    /// Convenience function for constructing a sleep request with a absolute nanosecond value from the realtime clock.
    pub async fn sleep_request_abs<G: Guest<Self>>(guest: &mut G, time: LogicalTime) -> Resources {
        // TODO T124594597 Record-replay case requires better handling of time
        let resource = ResourceID::SleepUntil(time);
        guest.thread_state().mk_request(resource, Permission::W)
    }

    /// Convenience function for constructing a thread yield request.
    /// Implemented as a sleep ending at the epoch (in the past).
    pub fn yield_request<G: Guest<Self>>(guest: &mut G) -> Resources {
        let resource = ResourceID::SleepUntil(LogicalTime::from_nanos(0));
        guest.thread_state().mk_request(resource, Permission::W)
    }

    /// Construct a request for a strong, one-turn scheduler yield.
    pub fn sched_yield_request<G: Guest<Self>>(guest: &mut G) -> Resources {
        guest
            .thread_state()
            .mk_request(ResourceID::SchedYield, Permission::W)
    }

    /// Construct a random PriorityChangePoint request using the local PRNG.
    pub fn random_priority_changepoint_request<G: Guest<Self>>(
        guest: &mut G,
        change_time: LogicalTime,
    ) -> Resources {
        let entropy = guest.thread_state_mut().chaos_prng_next_u64("priority");
        let new_priority = entropy_to_priority(entropy);
        Self::priority_changepoint_request(guest, change_time, new_priority)
    }

    /// Construct a PriorityChangePoint request using the supplied time and priority.
    pub fn priority_changepoint_request<G: Guest<Self>>(
        guest: &mut G,
        change_time: LogicalTime,
        new_priority: Priority,
    ) -> Resources {
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-1151)
        let epochs = guest.thread_state_mut().take_pending_chaos_epochs();
        let rcbs = guest.thread_state().committed_clock_value;
        let resource = ResourceID::PriorityChangePoint(new_priority, change_time, rcbs, epochs);
        guest.thread_state().mk_request(resource, Permission::W)
    }

    /// gettimeofday
    pub async fn handle_gettimeofday<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Gettimeofday,
    ) -> Result<i64, Error> {
        let time_ns = guest_clock_time(guest).await;

        let repair_on_efault = should_repair_failed_gettimeofday_tv(guest.config().backend_is_kvm);
        // What Detcore could read in `tv` before the call; the repair uses it
        // to confirm which words a failing call left alone.
        let before = match call.tv() {
            Some(tp) if repair_on_efault => Some(snapshot_timeval_words(guest, tp.into())),
            _ => None,
        };

        // A call failing with EFAULT may still have stored host wall-clock time
        // in `tv`, so keep its result until `tv` holds virtual time.
        let result = self
            .record_or_replay_preserving_tool_errors(guest, call)
            .await;

        let tv: Timeval = time_ns.into();

        if let Some(tp) = call.tv() {
            match (&result, &before) {
                (Ok(_), _) => guest.memory().write_value(tp, &tv)?,
                // Linux's gettimeofday fails only with EFAULT, which is taken
                // to be its own even when a seccomp filter returned it without
                // running the call. Any other error came from the backend, the
                // tool, a seccomp filter or a replayed log, and says nothing
                // about what reached `tv`, so memory is left alone.
                (Err(Error::Errno(Errno::EFAULT)), Some(before)) => {
                    require_live_time_store_probe(self.cfg.replay_data.is_some())?;
                    overwrite_failed_gettimeofday_tv(guest, tp.into(), &tv, before).await?
                }
                (Err(_), _) => {}
            }
        }

        result
    }

    /// time
    pub async fn handle_time<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Time,
    ) -> Result<i64, Error> {
        let time_ns = guest_clock_time(guest).await;
        let secs = time_ns.as_secs() as i64;

        if let Some(tloc) = call.tloc() {
            let mut memory = guest.memory();
            memory.write_value(tloc, &secs)?;
        }

        Ok(secs)
    }

    /// clock_gettime
    pub async fn handle_clock_gettime<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::ClockGettime,
    ) -> Result<i64, Error> {
        let time_ns = guest_clock_time(guest).await;
        trace!("Converting nanoseconds into clock_gettime: {}", time_ns);

        let tp = call.tp().ok_or(Errno::EFAULT)?;

        let t: Timespec = time_ns.into();

        guest.memory().write_value(tp, &t)?;

        Ok(0)
    }

    /// clock_gettime
    pub async fn handle_clock_getres<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::ClockGetres,
    ) -> Result<i64, Error> {
        // A NULL `res` pointer is valid for clock_getres: the kernel validates
        // the clockid and returns 0 without storing the resolution. GHC's RTS
        // probes the per-thread CPU clock exactly this way
        // (clock_getres(clockid, NULL)) in getCurrentThreadCPUTime, so
        // returning EFAULT here spuriously aborts the guest. Only write the
        // resolution when the caller supplied a destination.
        if let Some(res) = call.res() {
            // For now we report a constant clock res of 10ms:
            let clock_res = 10;

            let t = Timespec {
                tv_sec: 0,
                tv_nsec: 1000 * clock_res as i64,
            };

            guest.memory().write_value(res, &t)?;
        }

        Ok(0)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-857): Deterministic adjtimex query/refusal policy.
    /// Report Hermit's virtual clock with a fixed unsynchronized discipline.
    /// Adjustment modes are capability-gated host mutations and receive EPERM.
    pub async fn handle_adjtimex<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Adjtimex,
    ) -> Result<i64, Error> {
        self.write_deterministic_timex(guest, call.buf()).await
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-857): Deterministic clock_adjtime query/refusal policy.
    /// Apply the adjtimex policy to CLOCK_REALTIME. Linux does not permit NTP
    /// adjustment of the other fixed clock IDs, so reject them with EOPNOTSUPP.
    pub async fn handle_clock_adjtime<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::ClockAdjtime,
    ) -> Result<i64, Error> {
        if call.clockid() != ClockId::CLOCK_REALTIME {
            return Err(Errno::EOPNOTSUPP.into());
        }
        self.write_deterministic_timex(guest, call.buf()).await
    }

    async fn write_deterministic_timex<G: Guest<Self>>(
        &self,
        guest: &mut G,
        buf: Option<reverie::syscalls::AddrMut<'_, libc::timex>>,
    ) -> Result<i64, Error> {
        let buf = buf.ok_or(Errno::EFAULT)?;
        let request: libc::timex = guest.memory().read_value(buf)?;
        if !timex_mode_is_query(request.modes) {
            return Err(Errno::EPERM.into());
        }

        let now: Timespec = thread_observe_time(guest).await.into();
        guest.memory().write_value(buf, &deterministic_timex(now))?;
        Ok(libc::TIME_ERROR as i64)
    }

    /// Helper function to wait a given period, which may either succeed or be interrupted by a signal.
    /// Return 0 or EINTR respectively.
    async fn wait_and_return<R: Guest<Self>>(
        guest: &mut R,
        request: Resources,
        call: NanosleepFamily,
    ) -> Result<i64, Error> {
        let target_time = time_from_resources(&request).expect("a sleepuntil resource request");
        match crate::tool_global::parked_wait_request(
            guest,
            request,
            crate::scheduler::parked::ParkedWaitPolicy::NanosleepNoHandlerRestart {
                absolute_deadline: target_time,
            },
        )
        .await
        {
            ResumeStatus::Normal => Ok(0),
            ResumeStatus::Signaled(_) => {
                let now = thread_observe_time(guest).await;
                let delta = remaining_sleep_duration(target_time, now);
                // Linux never touches remain for TIMER_ABSTIME, even when
                // a caught signal interrupts the absolute sleep.
                let addr2 = if call.flags() & libc::TIMER_ABSTIME == 0 {
                    call.rem()
                } else {
                    None
                };
                if let Some(addr2) = addr2 {
                    info!(
                        "[interrupted] sleep till (until {}), woke up {:?} early, writing into nanosleep rem argument.",
                        target_time, delta
                    );
                    let t = Timespec {
                        tv_sec: delta.as_secs() as i64,
                        tv_nsec: delta.subsec_nanos() as i64,
                    };
                    guest.memory().write_value(addr2, &t)?;
                } else {
                    info!("[interrupted] nanosleep rem argument is null, not writing it.")
                }
                Err(reverie::Error::Errno(Errno::EINTR))
            }
        }
    }

    /// clock_nanosleep and nanosleep
    pub async fn handle_nanosleep_family<R: Guest<Self>>(
        &self,
        guest: &mut R,
        call: NanosleepFamily,
    ) -> Result<i64, Error> {
        if call.flags() > libc::TIMER_ABSTIME {
            trace!("Unhandled clock_nanosleep flags, letting syscall through...");
            return Ok(guest.inject(Syscall::from(call)).await?);
        }

        let addr = call.req().ok_or(Errno::EFAULT)?;
        let t: Timespec = guest.memory().read_value(addr)?;

        // Linux validates the requested interval BEFORE sleeping: nanosleep(2)
        // and clock_nanosleep(2) both fail EINVAL when tv_nsec is outside
        // [0, 999999999] or tv_sec is negative.
        //
        // Detcore skipped that check and fed the raw fields through `as u64`.
        // `Timespec` stores both as i64, so `tv_sec = -1` wrapped to
        // u64::MAX (~1.8e19 seconds) and became `SleepUntil(INDEFINITE)`: the
        // only guest thread parked with no deadline, the run queue emptied, and
        // `step2d_handle_empty_queue` deliberately never jumps the clock for an
        // indefinite waiter (doing so would also wake a `pause(2)`). The
        // container then died -- exit 1, "Sandbox container exited
        // unexpectedly" -- where Linux returns an errno and keeps running.
        //
        // A past *absolute* deadline is NOT an error and must still return 0,
        // so this rejects only malformed fields, never an early deadline.
        if t.tv_sec < 0 || t.tv_nsec < 0 || t.tv_nsec > 999_999_999 {
            return Err(Errno::EINVAL.into());
        }

        match call.flags() {
            0 => {
                if self.cfg.sequentialize_threads {
                    let time = Duration::from_secs(t.tv_sec as u64)
                        + Duration::from_nanos(t.tv_nsec as u64);
                    let request = Self::sleep_request(guest, time).await;
                    trace!(
                        "nanosleep adding delta {:?} to yield request {:?}",
                        time, &request
                    );
                    Self::wait_and_return(guest, request, call).await
                } else {
                    trace!("Not sequentializing threads, letting nanosleep through...");
                    Ok(guest.inject(Syscall::from(call)).await?)
                }
            }
            libc::TIMER_ABSTIME => {
                let target_time = LogicalTime::from_secs(t.tv_sec as u64)
                    + LogicalTime::from_nanos(t.tv_nsec as u64);
                if self.cfg.sequentialize_threads {
                    if self.cfg.virtualize_time {
                        let request = Self::sleep_request_abs(guest, target_time).await;
                        trace!(
                            "nanosleep setting absolute time {:?} to yield request {:?}",
                            target_time, &request
                        );
                        Self::wait_and_return(guest, request, call).await
                    } else {
                        // TODO T124594597: Record-replay case here, need better ideas to enable proper handling of this case.
                        error!(
                            "Sequentializing but not virtualizing, so can't rely on passed abs time, especially when replaying a recording, just yelding"
                        );
                        let request = Self::yield_request(guest);
                        Self::wait_and_return(guest, request, call).await
                    }
                } else if self.cfg.virtualize_time {
                    trace!(
                        "Not sequentializing, but virtualizing so calculating relative time and invoking nanosleep..."
                    );
                    let relative_ts = Self::relative_time_from_abs_target(guest, target_time).await;
                    let mut stack = guest.stack().await;
                    let req = stack.push(relative_ts);
                    stack.commit()?;
                    let modified_call = syscalls::Nanosleep::new().with_req(Some(req));
                    Ok(guest.inject(modified_call).await?)
                } else {
                    trace!(
                        "Not sequentializing threads not virtualizing, letting nanosleep through..."
                    );
                    Ok(guest.inject(Syscall::from(call)).await?)
                }
            }
            _ => unreachable!("Unexpected, unhandled flag value"),
        }
    }

    async fn relative_time_from_abs_target<G: Guest<Self>>(
        guest: &mut G,
        target_time: LogicalTime,
    ) -> Timespec {
        let base_time = thread_observe_time(guest).await;

        // An absolute deadline already in the past is NOT an error on Linux --
        // clock_nanosleep(TIMER_ABSTIME) simply returns 0 without sleeping.
        // `LogicalTime`'s `Sub` is a plain subtraction (unlike its `Add` impls,
        // which saturate deliberately), so `target_time - base_time` underflows
        // for a past deadline: a debug build panics with "attempt to subtract
        // with overflow", and a release build wraps to an enormous interval --
        // the same effectively-indefinite sleep this handler exists to avoid.
        //
        // Clamped here rather than by making the shared operator saturate,
        // because this is the only subtraction of two `LogicalTime`s in the
        // tree and a silently-saturating operator could hide a real underflow
        // in some future caller.
        let relative_logical = if target_time <= base_time {
            LogicalTime::from_nanos(0)
        } else {
            target_time - base_time
        };

        Timespec {
            tv_sec: relative_logical.as_secs() as i64,
            tv_nsec: relative_logical.subsec_nanos() as i64,
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    /// timer_create: allocate a per-process POSIX timer and hand back a
    /// deterministic id, retaining any scheduler-deliverable signal.
    pub async fn handle_timer_create<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::TimerCreate,
    ) -> Result<i64, Error> {
        // The kernel writes the new timer id here; a null pointer is EFAULT.
        let timerid_ptr = call.timerid().ok_or(Errno::EFAULT)?;
        let clockid = call.clockid();
        let signal = if let Some(event_ptr) = call.sevp() {
            let event: libc::sigevent = guest.memory().read_value(event_ptr)?;
            match event.sigev_notify {
                libc::SIGEV_NONE => None,
                // Linux uses 4 for SIGEV_THREAD_ID. Treat it as process-directed
                // until Detcore tracks per-timer thread targeting.
                libc::SIGEV_SIGNAL | 4 => {
                    if !(1..=64).contains(&event.sigev_signo) {
                        return Err(Errno::EINVAL.into());
                    }
                    Signal::try_from(event.sigev_signo).ok()
                }
                _ => return Err(Errno::ENOSYS.into()),
            }
        } else {
            Some(Signal::SIGALRM)
        };
        let id = {
            let mut timers = guest.thread_state().posix_timers.lock().unwrap();
            timers.create(signal.map(|sig| sig as i32))
        };
        guest
            .memory()
            .write_value(timerid_ptr, &(id as libc::c_int))?;
        detlog!(
            "[dtid {}] timer_create(clockid={:?}) => deterministic timer id {}, signal {:?}",
            guest.thread_state().dettid,
            clockid,
            id,
            signal,
        );
        Ok(0)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    /// timer_settime: arm or disarm a timer against the deterministic virtual
    /// clock. The old arming is reported through `old_value` when requested.
    pub async fn handle_timer_settime<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::TimerSettime,
    ) -> Result<i64, Error> {
        let id = call.timerid();
        let new_ptr = call.new_value().ok_or(Errno::EINVAL)?;
        let new: libc::itimerspec = guest.memory().read_value(new_ptr)?;
        let interval_ns = timespec_to_ns(new.it_interval);
        let value_ns = timespec_to_ns(new.it_value);

        let now = thread_observe_time(guest).await;
        let deadline = if value_ns == 0 {
            None
        } else if call.flags() & libc::TIMER_ABSTIME != 0 {
            // Absolute expiration is interpreted against the same virtual clock.
            Some(LogicalTime::from_nanos(value_ns))
        } else {
            Some(now + Duration::from_nanos(value_ns))
        };

        let (old, signal_number) = {
            let mut timers = guest.thread_state().posix_timers.lock().unwrap();
            let old = timers.settime(id, interval_ns, deadline, now);
            let signal = timers.signal(id);
            (old, signal)
        };
        let (old_remaining_ns, old_interval_ns) = old.ok_or(Errno::EINVAL)?;
        let signal_number = signal_number.ok_or(Errno::EINVAL)?;

        if let Some(old_ptr) = call.old_value() {
            let old_spec = libc::itimerspec {
                it_interval: ns_to_timespec(old_interval_ns),
                it_value: ns_to_timespec(old_remaining_ns),
            };
            guest.memory().write_value(old_ptr, &old_spec)?;
        }

        if let Some(signal) = signal_number.and_then(|signum| Signal::try_from(signum).ok()) {
            register_posix_timer(
                guest,
                id,
                deadline,
                LogicalTime::from_nanos(interval_ns),
                signal,
            )
            .await;
        }

        detlog!(
            "[dtid {}] timer_settime(id={}, interval_ns={}, value_ns={}) armed against virtual clock",
            guest.thread_state().dettid,
            id,
            interval_ns,
            value_ns,
        );
        Ok(0)
    }

    /// timer_gettime: report the time remaining until the next expiration and
    /// the reload interval, both computed from the virtual clock.
    pub async fn handle_timer_gettime<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::TimerGettime,
    ) -> Result<i64, Error> {
        let id = call.timerid();
        let value_ptr = call.value().ok_or(Errno::EFAULT)?;
        let now = thread_observe_time(guest).await;
        let cur = {
            let timers = guest.thread_state().posix_timers.lock().unwrap();
            timers.gettime(id, now)
        };
        let (remaining_ns, interval_ns) = cur.ok_or(Errno::EINVAL)?;
        let spec = libc::itimerspec {
            it_interval: ns_to_timespec(interval_ns),
            it_value: ns_to_timespec(remaining_ns),
        };
        guest.memory().write_value(value_ptr, &spec)?;
        Ok(0)
    }

    /// timer_getoverrun: coalesced expiration accounting is not modeled, so the
    /// overrun count is always 0 for a live timer.
    pub async fn handle_timer_getoverrun<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::TimerGetoverrun,
    ) -> Result<i64, Error> {
        let id = call.timerid();
        let exists = guest
            .thread_state()
            .posix_timers
            .lock()
            .unwrap()
            .contains(id);
        if exists {
            Ok(0)
        } else {
            Err(Errno::EINVAL.into())
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    /// timer_delete: destroy a timer created by `timer_create`.
    pub async fn handle_timer_delete<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::TimerDelete,
    ) -> Result<i64, Error> {
        let id = call.timerid();
        let signal_number = guest
            .thread_state()
            .posix_timers
            .lock()
            .unwrap()
            .signal(id)
            .ok_or(Errno::EINVAL)?;
        let existed = {
            let mut timers = guest.thread_state().posix_timers.lock().unwrap();
            timers.remove(id)
        };
        if existed {
            if let Some(signal) = signal_number.and_then(|signum| Signal::try_from(signum).ok()) {
                register_posix_timer(guest, id, None, LogicalTime::ZERO, signal).await;
            }
            detlog!(
                "[dtid {}] timer_delete(id={})",
                guest.thread_state().dettid,
                id,
            );
            Ok(0)
        } else {
            Err(Errno::EINVAL.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timex_policy_distinguishes_queries_from_mutations() {
        assert!(timex_mode_is_query(0));
        assert!(timex_mode_is_query(libc::ADJ_OFFSET_SS_READ));
        assert!(!timex_mode_is_query(libc::ADJ_OFFSET));
        assert!(!timex_mode_is_query(libc::ADJ_FREQUENCY));
    }

    #[test]
    fn timex_snapshot_is_unsynchronized_and_uses_virtual_time() {
        let tx = deterministic_timex(Timespec {
            tv_sec: 123,
            tv_nsec: 456_789_000,
        });
        assert_eq!(tx.status, libc::STA_UNSYNC);
        assert_eq!(tx.tick, 10_000);
        assert_eq!(tx.time.tv_sec, 123);
        assert_eq!(tx.time.tv_usec, 456_789);
    }

    #[test]
    fn interrupted_sleep_remaining_time_floors_at_zero() {
        let target = LogicalTime::from_nanos(1_000);

        assert_eq!(
            remaining_sleep_duration(target, LogicalTime::from_nanos(750)),
            Duration::from_nanos(250)
        );
        assert_eq!(remaining_sleep_duration(target, target), Duration::ZERO);
        assert_eq!(
            remaining_sleep_duration(target, LogicalTime::from_nanos(1_250)),
            Duration::ZERO
        );
    }

    mod failed_gettimeofday_tv {
        use super::*;

        fn failure(error: Error) -> TvRepairFailure {
            match error {
                Error::Tool(error) => *error
                    .downcast_ref::<TvRepairFailure>()
                    .expect("a typed repair failure"),
                other => panic!("expected a Tool error, got {other:?}"),
            }
        }

        #[test]
        fn successful_time_store_probe_requires_an_overwrite() {
            assert_eq!(
                classify_time_store_probe("tv_sec", Ok(1)).unwrap(),
                TimeStoreProbe::Stored
            );
        }

        #[test]
        fn efault_time_store_probe_stops_without_an_overwrite() {
            assert_eq!(
                classify_time_store_probe("tv_usec", Err(Errno::EFAULT)).unwrap(),
                TimeStoreProbe::Stopped
            );
        }

        #[test]
        fn any_control_probe_error_means_the_efault_was_not_a_store_fault() {
            require_native_time_control_probe("tv_sec", Ok(1_767_225_600)).unwrap();
            for errno in [Errno::EFAULT, Errno::EPERM, Errno::ENOSYS] {
                let error = require_native_time_control_probe("tv_usec", Err(errno))
                    .expect_err("time(NULL) cannot fail natively");
                assert_eq!(
                    failure(error),
                    TvRepairFailure {
                        field: "tv_usec",
                        kind: TvRepairFailureKind::ControlProbeFailed(errno),
                    }
                );
            }
        }

        #[test]
        fn a_stopped_word_must_keep_its_contents_and_readability() {
            assert_eq!(
                require_unchanged_stopped_word("tv_sec", Ok(7), Ok(7)).unwrap(),
                StoppedWord::Unchanged
            );
            for (before, after) in [(Errno::EFAULT, Errno::EFAULT), (Errno::EIO, Errno::EPERM)] {
                assert_eq!(
                    require_unchanged_stopped_word("tv_sec", Err(before), Err(after)).unwrap(),
                    StoppedWord::Unreadable
                );
            }
            for (before, after) in [
                (Ok(7), Ok(1_791_041_091)),
                (Ok(7), Err(Errno::EFAULT)),
                (Err(Errno::EFAULT), Ok(7)),
            ] {
                let error = require_unchanged_stopped_word("tv_usec", before, after)
                    .expect_err("a changed stopped word means the call stored it");
                assert_eq!(
                    failure(error),
                    TvRepairFailure {
                        field: "tv_usec",
                        kind: TvRepairFailureKind::StoppedWordChanged,
                    }
                );
            }
        }

        #[test]
        fn an_unreadable_word_is_unstored_only_if_part_of_it_is_unmapped() {
            const PAGE: u64 = 0x1000;
            let two_pages = || Ok(vec![(3 * PAGE, 4 * PAGE), (2 * PAGE, 3 * PAGE)]);
            // Outside every mapping, and straddling either end of the mapped pages.
            for word in [1, 2 * PAGE - 4, 4 * PAGE - 4, 5 * PAGE, u64::MAX - 3] {
                require_unmapped_unreadable_word("tv_sec", two_pages(), word).unwrap();
            }
            // Inside one page, and across the boundary of two adjacent mappings.
            for word in [2 * PAGE, 3 * PAGE - 4, 4 * PAGE - 8] {
                let error = require_unmapped_unreadable_word("tv_usec", two_pages(), word)
                    .expect_err("a failed read of a mapped word is not a fault");
                assert_eq!(
                    failure(error),
                    TvRepairFailure {
                        field: "tv_usec",
                        kind: TvRepairFailureKind::StoppedWordUnreadable,
                    }
                );
            }
        }

        #[test]
        fn an_unreadable_word_needs_a_readable_nonempty_map() {
            let unreadable = Err(Error::Errno(Errno::EPERM));
            for maps in [Ok(Vec::new()), unreadable] {
                let error = require_unmapped_unreadable_word("tv_sec", maps, 1)
                    .expect_err("without a map an unreadable word proves nothing");
                assert_eq!(
                    failure(error),
                    TvRepairFailure {
                        field: "tv_sec",
                        kind: TvRepairFailureKind::MapsUnavailable,
                    }
                );
            }
        }

        #[test]
        fn failed_gettimeofday_repair_is_skipped_only_for_kvm() {
            assert!(should_repair_failed_gettimeofday_tv(false));
            assert!(!should_repair_failed_gettimeofday_tv(true));
        }

        #[test]
        fn other_time_store_probe_errors_fail_closed() {
            for errno in [Errno::ENOSYS, Errno::EPERM, Errno::EIO] {
                let error = classify_time_store_probe("tv_sec", Err(errno))
                    .expect_err("a non-EFAULT probe error must fail the repair");
                assert_eq!(
                    failure(error),
                    TvRepairFailure {
                        field: "tv_sec",
                        kind: TvRepairFailureKind::ProbeFailed(errno),
                    }
                );
            }
        }

        #[test]
        fn overwrite_errors_and_nonexact_counts_fail_closed() {
            assert_eq!(
                failure(
                    require_complete_time_word_overwrite("tv_usec", 8, Err(Errno::EFAULT),)
                        .unwrap_err()
                ),
                TvRepairFailure {
                    field: "tv_usec",
                    kind: TvRepairFailureKind::OverwriteFailed(Errno::EFAULT),
                }
            );
            for reported in [0, 3, 7, 9] {
                assert_eq!(
                    failure(
                        require_complete_time_word_overwrite("tv_sec", 8, Ok(reported))
                            .unwrap_err()
                    ),
                    TvRepairFailure {
                        field: "tv_sec",
                        kind: TvRepairFailureKind::OverwriteCount {
                            expected: 8,
                            reported,
                        },
                    }
                );
            }
            require_complete_time_word_overwrite("tv_sec", 8, Ok(8)).unwrap();

            require_live_time_store_probe(false).unwrap();
            assert_eq!(
                failure(require_live_time_store_probe(true).unwrap_err()),
                TvRepairFailure {
                    field: "tv",
                    kind: TvRepairFailureKind::ReplayMode,
                }
            );
        }

        #[test]
        fn timeval_word_addresses_follow_kernel_order_and_overflow_fails_closed() {
            assert_eq!(TIMEVAL_WORDS, [("tv_sec", 0), ("tv_usec", 8)]);
            let tv_addr = AddrMut::<Timeval>::from_raw(0x10_0000).unwrap();
            assert_eq!(
                timeval_word_addr("tv_sec", tv_addr, std::mem::offset_of!(Timeval, tv_sec))
                    .unwrap()
                    .as_raw(),
                tv_addr.as_raw()
            );
            assert_eq!(
                timeval_word_addr("tv_usec", tv_addr, std::mem::offset_of!(Timeval, tv_usec))
                    .unwrap()
                    .as_raw(),
                tv_addr.as_raw() + 8
            );

            let overflowing = AddrMut::<Timeval>::from_raw(usize::MAX - 3).unwrap();
            let error = timeval_word_addr("tv_usec", overflowing, 8)
                .expect_err("overflowing field address must fail the repair");
            assert_eq!(
                failure(error),
                TvRepairFailure {
                    field: "tv_usec",
                    kind: TvRepairFailureKind::AddressOverflow,
                }
            );
        }
    }
}
