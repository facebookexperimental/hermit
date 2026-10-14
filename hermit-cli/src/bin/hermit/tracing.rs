/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs::File;
use std::io;
use std::io::IsTerminal;
use std::io::Write;
use std::io::stderr;
use std::mem;
use std::os::fd::AsRawFd;
use std::os::fd::RawFd;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use hermit::Backend;
use hermit::liteinst_bootstrap::EffectiveFilter;
use hermit::nonwaiting_write::write_without_waiting;
use tracing::Subscriber;
use tracing::metadata::LevelFilter;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::util::SubscriberInitExt;

const DEFAULT_TRACE_LEVEL: LevelFilter = LevelFilter::WARN;

/// Environment override for [`DEFAULT_LOG_MAX_BYTES`]; `0` disables the bound.
pub const LOG_MAX_BYTES_ENV: &str = "HERMIT_LOG_MAX_BYTES";

/// Ceiling on one run's log file.
///
/// A run that makes no progress still logs. Measured 2026-08-11 over the 4111
/// retained `--verify` comparison logs on one host: p50 0.4 MiB, p90 43.5 MiB,
/// p99 145.5 MiB -- and then a tail to 928.8 GiB, 4357.4 GiB in total. The
/// largest single file was a KVM guest livelocked on `sched_yield`, logging the
/// same fifteen line shapes for 11.7 hours; its registers at the 50 GiB and
/// 900 GiB offsets were identical.
///
/// 1 GiB is about 7x p99, so it never truncates a log anyone reads, and it
/// removes 98.6% of those bytes while touching 0.56% of the files. The
/// distribution is bimodal enough that any bound from 0.25 to 16 GiB reclaims
/// over 94%, so this is a safety limit rather than a tuning parameter.
///
/// Compression is NOT the control. `/tmp` happens to be zstd-compressed here,
/// which is why a terabyte of repetitive DETLOG has been survivable, but a
/// compression ratio scales with how repetitive the runaway happens to be and
/// silently converts a hard failure into an invisible one.
pub const DEFAULT_LOG_MAX_BYTES: u64 = 1 << 30;

/// Written once, in-band, when the bound is reached.
///
/// A truncated log MUST say so. Output that simply stops reads as a run that
/// ENDED, which would be a worse evidence defect than the disk hazard this
/// bound removes: a reader would draw conclusions from an absence we created.
///
/// The surrounding newlines are load-bearing, not cosmetic. The marker must
/// occupy a LINE OF ITS OWN at the very END of the file, because that is what
/// [`detcore::logdiff::log_was_truncated`] anchors on to tell a truncated log
/// apart from a log that merely quotes the text. This literal is deliberately
/// NOT `detcore::logdiff::TRUNCATION_MARKER` itself: keeping the two texts
/// independent is what leaves
/// `the_written_marker_is_the_text_the_comparator_matches` able to fail.
const TRUNCATION_MARKER: &[u8] =
    b"\n=== HERMIT LOG TRUNCATED: reached the configured size bound (HERMIT_LOG_MAX_BYTES). \
Output beyond this point was DISCARDED. The run itself continued and was NOT affected. ===\n";

/// Resolve the configured ceiling; `0` means unlimited.
///
/// A malformed value is an ERROR rather than a fallback. Silently substituting
/// the 1 GiB default would mean `HERMIT_LOG_MAX_BYTES=unlimited`, or any typo
/// in the value used to disable the bound, quietly re-enabled it -- i.e. the
/// documented way to turn the bound off is one keystroke away from turning it
/// back on without saying so.
pub fn log_max_bytes() -> Result<u64, String> {
    match std::env::var(LOG_MAX_BYTES_ENV) {
        Ok(raw) => raw.trim().parse().map_err(|_| {
            format!(
                "{LOG_MAX_BYTES_ENV}={raw:?} is not a byte count. Set a non-negative integer \
                 number of bytes, or 0 to disable the bound."
            )
        }),
        Err(_) => Ok(DEFAULT_LOG_MAX_BYTES),
    }
}

/// A writer that stops after `limit` bytes and says so in-band.
///
/// Beyond the bound this reports the bytes as consumed rather than returning a
/// short count: `tracing_appender` treats a short write as an I/O error, so a
/// truthful-but-short return would turn a size limit into a logging failure.
pub struct BoundedWriter<W: Write> {
    inner: W,
    remaining: u64,
    bounded: bool,
    announced: bool,
}

impl<W: Write> BoundedWriter<W> {
    /// `limit == 0` disables the bound entirely.
    pub fn new(inner: W, limit: u64) -> Self {
        Self {
            inner,
            remaining: limit,
            bounded: limit != 0,
            announced: false,
        }
    }

    /// Continues a log that already holds `written` bytes under the same
    /// `limit` (`0`: unbounded): the bound counts those bytes too, so appending
    /// to a log cannot take it past the ceiling, and appending to a log that
    /// has already reached it discards the bytes and announces the truncation.
    pub fn resume(inner: W, limit: u64, written: u64) -> Self {
        Self {
            inner,
            remaining: limit.saturating_sub(written),
            bounded: limit != 0,
            announced: false,
        }
    }
}

impl<W: Write> BoundedWriter<W> {
    /// Whether bytes have been discarded and the truncation marker written.
    /// From then on the marker is this log's final line.
    fn has_truncated(&self) -> bool {
        self.announced
    }

    /// Announce truncation the FIRST time bytes are actually discarded.
    ///
    /// Deferring this to the next `write` leaves a log that was truncated on
    /// its final write silent, which is the exact failure the marker exists to
    /// prevent -- caught end-to-end: a 100-byte bound produced exactly 100
    /// bytes and no marker, because no further write ever arrived.
    fn announce_truncation(&mut self) -> io::Result<()> {
        if !self.announced {
            self.announced = true;
            self.inner.write_all(TRUNCATION_MARKER)?;
            self.inner.flush()?;
        }
        Ok(())
    }
}

impl<W: Write> Write for BoundedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if !self.bounded {
            return self.inner.write(buf);
        }
        if self.remaining == 0 {
            self.announce_truncation()?;
            return Ok(buf.len());
        }
        let take = buf.len().min(self.remaining as usize);
        let written = self.inner.write(&buf[..take])?;
        self.remaining -= written as u64;
        if written < take {
            // A genuine short write by the inner writer: report it truthfully
            // and let the caller retry the remainder.
            return Ok(written);
        }
        if take < buf.len() {
            // This write is the one that crossed the bound; its tail is being
            // dropped, so say so now rather than hoping for another write.
            self.announce_truncation()?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Parse a `--max-log-bytes` value: a byte count with an optional binary
/// suffix. `8G` is 8 GiB; `K`, `M`, `G`, `T` are powers of 1024, may be lower
/// case, and may be spelled `Ki`/`KiB`/`KB` alike -- there is deliberately no
/// SI reading, so `8G` and `8GiB` cannot mean two different caps.
///
/// Zero is refused rather than read as "unlimited": a cap that aborts on the
/// first log line is never what anyone meant, and the way to run uncapped is to
/// omit the flag.
pub fn parse_max_log_bytes(raw: &str) -> Result<u64, String> {
    let usage = "Give a positive byte count with an optional K/M/G/T suffix \
                 (powers of 1024), e.g. --max-log-bytes=8G.";
    let text = raw.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (digits, suffix) = text.split_at(split);
    if digits.is_empty() {
        return Err(format!("{raw:?} is not a byte count. {usage}"));
    }
    let shift = match suffix.to_ascii_lowercase().as_str() {
        "" | "b" => 0,
        "k" | "ki" | "kb" | "kib" => 10,
        "m" | "mi" | "mb" | "mib" => 20,
        "g" | "gi" | "gb" | "gib" => 30,
        "t" | "ti" | "tb" | "tib" => 40,
        _ => return Err(format!("{raw:?} has an unknown size suffix. {usage}")),
    };
    let count: u64 = digits
        .parse()
        .map_err(|_| format!("{raw:?} is too large for a byte count. {usage}"))?;
    let bytes = count
        .checked_mul(1u64 << shift)
        .ok_or_else(|| format!("{raw:?} is too large for a byte count. {usage}"))?;
    if bytes == 0 {
        return Err(format!(
            "{raw:?} would abort the run on its first log line. Omit --max-log-bytes to \
             leave hermit's log output uncapped, or give a positive size such as 8G."
        ));
    }
    Ok(bytes)
}

/// Render a byte count the way `--max-log-bytes` accepts it, choosing the
/// largest suffix that represents it exactly (`8589934592` -> `8G`).
pub fn format_byte_size(bytes: u64) -> String {
    for (shift, suffix) in [(40, "T"), (30, "G"), (20, "M"), (10, "K")] {
        let unit = 1u64 << shift;
        if bytes != 0 && bytes.is_multiple_of(unit) {
            return format!("{}{suffix}", bytes / unit);
        }
    }
    bytes.to_string()
}

/// The process-shared state of a [`LogBudget`].
///
/// The crossing is recorded in `admission`, separate from `spent`, because
/// `spent` goes DOWN: a write the sink accepted only in part, or refused, is
/// refunded. Deciding the crossing from the total alone made it reversible --
/// with a limit of 10, A charges 6, B charges 5 and crosses, A's failed write
/// refunds 6, and C's 6 crosses a second time. [`CROSSED`] only ever goes from
/// clear to set.
#[repr(C)]
struct BudgetCells {
    spent: AtomicU64,
    /// The admission word. Bit 63 is [`CROSSED`]; the low 63 bits count the
    /// charged writes that were admitted and have not finished yet, in every
    /// process sharing the budget. See [`admit_write`].
    admission: AtomicU64,
}

/// Bit 63 of [`BudgetCells::admission`]: a write has crossed the cap. Once it
/// is set no write is admitted, so no new log bytes reach any sink.
const CROSSED: u64 = 1 << 63;

/// How long the crossing writer waits for log writes that were admitted
/// before the crossing to finish. If they have not finished by then, it omits
/// the final message: written while another write was still in progress, the
/// message could be followed by that write's line.
const LOG_CAP_DRAIN_BOUND: Duration = Duration::from_millis(50);

/// The longest drain bound any caller may set. The crossing process must exit
/// within one second of the crossing, whatever its sinks do (review of
/// https://github.com/rrnewton/hermit/pull/3686, round 2, finding 1): this
/// wait plus the final message's attempts, which never wait, stay far below
/// that.
const LOG_CAP_DRAIN_BOUND_MAX: Duration = Duration::from_millis(250);
const _: () = assert!(LOG_CAP_DRAIN_BOUND.as_millis() <= LOG_CAP_DRAIN_BOUND_MAX.as_millis());

/// How often the crossing writer looks at the admission word while it waits.
const LOG_CAP_DRAIN_POLL: Duration = Duration::from_millis(1);

/// Admit one charged write: count it as in progress, unless a write has
/// already crossed the cap. One compare-and-swap, retried only when another
/// writer changed the word first. The admitted writer must end its admission
/// exactly once, with [`finish_write`] or [`claim_crossing`].
///
/// WHY ADMISSION AND THE CROSSING SHARE ONE WORD. Every change to it is a
/// read-modify-write of the same location, so they happen in one order. A
/// write admitted before the crossing's `fetch_or` is counted in the value that
/// the crossing writer reads afterwards; a write that comes after it sees
/// [`CROSSED`] and is refused. So once the count reaches zero no admitted
/// write is left in progress and no new one can start, and the final message
/// written then is the last line of every sink that charges this budget
/// (review of https://github.com/rrnewton/hermit/pull/3686, round 3,
/// finding 5).
fn admit_write(admission: &AtomicU64) -> bool {
    let mut current = admission.load(Ordering::Relaxed);
    loop {
        if current & CROSSED != 0 {
            return false;
        }
        match admission.compare_exchange_weak(
            current,
            current + 1,
            Ordering::AcqRel,
            Ordering::Relaxed,
        ) {
            Ok(_) => return true,
            Err(actual) => current = actual,
        }
    }
}

/// End an admitted write once its bytes have reached the sink, or failed to.
fn finish_write(admission: &AtomicU64) {
    admission.fetch_sub(1, Ordering::Release);
}

/// For an admitted write whose charge went past the limit: set [`CROSSED`],
/// then end this write's own admission. True only for the first such write,
/// which is the crossing; every later one finds the bit already set.
fn claim_crossing(admission: &AtomicU64) -> bool {
    let before = admission.fetch_or(CROSSED, Ordering::AcqRel);
    finish_write(admission);
    before & CROSSED == 0
}

/// Admitted writes that have not finished.
fn writes_in_progress(admission: &AtomicU64) -> u64 {
    admission.load(Ordering::Acquire) & !CROSSED
}

/// Whether some process has already claimed the crossing, read the way
/// [`exit_if_log_cap_already_crossed`] needs: with a read-modify-write that
/// changes nothing (`fetch_or(0)`) and has acquire AND release ordering, not
/// with a load. See that function for why the release half matters.
fn crossing_already_claimed(admission: &AtomicU64) -> bool {
    admission.fetch_or(0, Ordering::AcqRel) & CROSSED != 0
}

/// The invocation's budget, for code that runs in a freshly forked process
/// and has no `GlobalOpts`: the container init's arming point and the
/// `run --namespace-only` `pre_exec` callback. `main` registers it once, right
/// after `GlobalOpts::prepare_log_budget` and before any fork. Nothing else
/// registers one, so a unit test that prepares a budget leaves none behind.
static INVOCATION_LOG_BUDGET: OnceLock<LogBudget> = OnceLock::new();

/// Make the invocation's budget reachable from [`exit_if_log_cap_already_crossed`].
/// The first registration wins; `main` makes the only one.
pub(crate) fn register_invocation_log_budget(budget: Option<LogBudget>) {
    if let Some(budget) = budget {
        let _ = INVOCATION_LOG_BUDGET.set(budget);
    }
}

/// Exit 123 at once, writing nothing, if `--max-log-bytes` was already crossed.
///
/// Called by a process that is about to start a guest, right after it armed
/// `PR_SET_PDEATHSIG`: the container init in `arm_container_init_guards`, and
/// the `run --namespace-only` child in its `pre_exec`. It closes the window
/// between the fork and that `prctl`: if the parent crossed the cap and exited
/// 123 inside it, the death signal was armed after the parent was gone, so it
/// will never arrive, and without this check the guest would start with
/// nothing left to stop it.
///
/// WHY A READ-MODIFY-WRITE, AND WHY IT IS ENOUGH. Every cap-caused exit sets
/// [`CROSSED`] first, with `fetch_or` in [`claim_crossing`], and every write to
/// the admission word after it is created is a read-modify-write (the CAS in
/// [`admit_write`], `fetch_sub` in [`finish_write`], `fetch_or` in
/// [`claim_crossing`]). So this `fetch_or(0, AcqRel)` heads a release sequence
/// containing every later change to the word. If it reads CROSSED clear, the
/// crosser's `fetch_or(CROSSED, AcqRel)` comes later in the word's modification
/// order, reads a value from that release sequence, and so synchronizes with
/// this read-modify-write. The caller's `prctl` is sequenced before it, so the
/// kernel's store of this process's death signal happens before everything
/// the crosser does next, including its `_exit`. The exit of the parent thread
/// (the crosser itself, or a sibling the group exit kills under `siglock`) runs
/// `exit_notify`, which reads each child's death signal under `tasklist_lock`
/// and sends it, so this process is killed. If it reads CROSSED set, it exits
/// here, before any guest exists. A plain acquire load would not do: a store
/// (the death signal) followed by a load here, against the crosser's
/// read-modify-write followed by its read of the death signal, is the
/// store-buffering shape, in which both sides can miss each other.
///
/// It closes only the part of the fork-to-`prctl` window that the cap opens. A
/// parent killed for another reason inside that window still leaves the child
/// unsupervised; see `arm_parent_death_signal`.
pub(crate) fn exit_if_log_cap_already_crossed() {
    if let Some(budget) = INVOCATION_LOG_BUDGET.get()
        && crossing_already_claimed(&budget.cells().admission)
    {
        // SAFETY: _exit has no preconditions and is async-signal-safe, so it
        // is also sound in a `pre_exec` callback. Nothing is written: the
        // crossing process owns the final message.
        unsafe { libc::_exit(hermit::HERMIT_LOG_CAP_EXIT) }
    }
}

/// Wait until no admitted write is in progress, polling with `nanosleep`, for
/// at most `bound`. Returns whether none was left; a zero bound looks once.
///
/// A WRITER KILLED IN THE MIDDLE OF A WRITE NEVER FINISHES IT. Its count stays
/// in the word for the rest of the invocation, so every later wait times out
/// and the final message is omitted. That costs only the message: the exit
/// status is still 123, and no line can follow a message that was never
/// written.
fn wait_for_writes_in_progress(admission: &AtomicU64, bound: Duration) -> bool {
    let start = Instant::now();
    loop {
        if writes_in_progress(admission) == 0 {
            return true;
        }
        let left = bound.saturating_sub(start.elapsed());
        if left.is_zero() {
            return false;
        }
        let step = left.min(LOG_CAP_DRAIN_POLL);
        let pause = libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::c_long::from(step.subsec_nanos()),
        };
        // SAFETY: nanosleep reads one initialized timespec; the remainder
        // pointer may be null. An interrupted sleep just polls again.
        unsafe { libc::nanosleep(&pause, std::ptr::null_mut()) };
    }
}

#[derive(Debug)]
struct SharedBudgetCells {
    address: NonNull<BudgetCells>,
}

// SAFETY: initialized atomics in a MAP_SHARED mapping that stays live while
// any clone of the owning LogBudget exists; the atomics supply synchronization.
unsafe impl Send for SharedBudgetCells {}
unsafe impl Sync for SharedBudgetCells {}

impl Drop for SharedBudgetCells {
    fn drop(&mut self) {
        // SAFETY: this process owns the mapping created in LogBudget::new.
        unsafe {
            libc::munmap(self.address.as_ptr().cast(), mem::size_of::<BudgetCells>());
        }
    }
}

/// The `--max-log-bytes` budget: a limit and a running total of the bytes
/// hermit has offered to its public log sink.
///
/// THE TOTAL IS PROCESS-SHARED. Tracing is initialized after the container fork, and `run --verify` forks
/// one container per run. A heap counter would be copied at each fork, so every
/// run would get the full budget again; this MAP_SHARED cell, created in `main`
/// before any fork, makes the cap a total for the invocation.
#[derive(Clone, Debug)]
pub struct LogBudget {
    limit: u64,
    cells: Arc<SharedBudgetCells>,
    /// Rendered up front so the abort path allocates nothing: it can run on
    /// any thread, with arbitrary locks held.
    message: Arc<str>,
    /// [`LOG_CAP_DRAIN_BOUND`], except in tests.
    drain_bound: Duration,
}

/// Outcome of charging one write against a [`LogBudget`].
#[derive(Debug, PartialEq, Eq)]
enum Charge {
    /// The write fits and is admitted; perform it, then call
    /// [`LogBudget::finish`] once.
    Within,
    /// This write is the one that crossed the cap; abort the run. Its
    /// admission has already ended.
    Crossed,
    /// A write already crossed the cap and is aborting; drop this one. Nothing
    /// is left to finish.
    AlreadyOver,
}

impl LogBudget {
    pub fn new(limit: u64) -> io::Result<Self> {
        // SAFETY: anonymous shared mapping, no fixed address and no backing fd.
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                mem::size_of::<BudgetCells>(),
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if address == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let address = NonNull::new(address.cast::<BudgetCells>())
            .expect("mmap returned a non-null non-failure address");
        // SAFETY: the fresh mapping is writable and suitably page-aligned.
        unsafe {
            address.as_ptr().write(BudgetCells {
                spent: AtomicU64::new(0),
                admission: AtomicU64::new(0),
            })
        };
        Ok(Self {
            limit,
            cells: Arc::new(SharedBudgetCells { address }),
            message: exceeded_message(limit).into(),
            drain_bound: LOG_CAP_DRAIN_BOUND,
        })
    }

    /// This budget with another drain bound, never above
    /// [`LOG_CAP_DRAIN_BOUND_MAX`].
    #[cfg(test)]
    fn with_drain_bound(mut self, bound: Duration) -> Self {
        self.drain_bound = bound.min(LOG_CAP_DRAIN_BOUND_MAX);
        self
    }

    /// The final message printed when the cap fires.
    pub fn exceeded_message(&self) -> &str {
        &self.message
    }

    fn cells(&self) -> &BudgetCells {
        // SAFETY: the Arc keeps the initialized mapping live in this process.
        unsafe { self.cells.address.as_ref() }
    }

    /// Bytes charged so far, across every process sharing this budget.
    #[cfg(test)]
    fn spent(&self) -> u64 {
        self.cells().spent.load(Ordering::Relaxed)
    }

    /// Whether `other` charges the same process-shared cells as `self`.
    #[cfg(test)]
    pub(crate) fn shares_counter_with(&self, other: &LogBudget) -> bool {
        self.cells.address == other.cells.address
    }

    /// Charge `len` bytes: admit the write ([`admit_write`]), then account for
    /// it. Two compare-exchange loops per write, each retried only when another
    /// writer raced it: this sits on the hot path of every log line. A write
    /// that would take the total past the limit claims the one-way [`CROSSED`]
    /// bit; the single claimant is the crossing, every other writer is already
    /// over. Refunds lower `spent` but never clear [`CROSSED`], so a crossing
    /// is final.
    fn charge(&self, len: u64) -> Charge {
        if !admit_write(&self.cells().admission) {
            return Charge::AlreadyOver;
        }
        self.account(len)
    }

    /// The second step of [`LogBudget::charge`], for a write already admitted.
    ///
    /// A TOTAL THAT WOULD OVERFLOW IS A CROSSING. `fetch_add` wrapped the stored
    /// total, and with the largest accepted limit (`u64::MAX`) no sum could
    /// ever compare above it, so that cap could never fire. The stored total
    /// saturates instead of wrapping, and an addition with no `u64` result is
    /// treated as past every limit.
    fn account(&self, len: u64) -> Charge {
        let cells = self.cells();
        let mut before = cells.spent.load(Ordering::Relaxed);
        while let Err(current) = cells.spent.compare_exchange_weak(
            before,
            before.saturating_add(len),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            before = current;
        }
        match before.checked_add(len) {
            Some(total) if total <= self.limit => Charge::Within,
            _ if claim_crossing(&cells.admission) => Charge::Crossed,
            _ => Charge::AlreadyOver,
        }
    }

    /// Return bytes charged for a write that the sink accepted only in part, so
    /// the caller's retry of the remainder is not counted twice.
    fn refund(&self, len: u64) {
        self.cells().spent.fetch_sub(len, Ordering::Relaxed);
    }

    /// End a write that [`LogBudget::charge`] admitted as [`Charge::Within`].
    fn finish(&self) {
        finish_write(&self.cells().admission);
    }

    /// For the crossing writer: wait, up to the drain bound, until no admitted
    /// write is in progress in any process sharing this budget. True when the
    /// final message may be written as the last line.
    fn writes_drained(&self) -> bool {
        wait_for_writes_in_progress(&self.cells().admission, self.drain_bound)
    }

    /// Admitted writes in progress, for tests.
    #[cfg(test)]
    fn in_progress(&self) -> u64 {
        writes_in_progress(&self.cells().admission)
    }
}

/// Ends a [`Charge::Within`] write's admission when the write returns, by any
/// path: success, an error, a partial write or a panic in the sink.
struct AdmittedWrite<'a>(&'a LogBudget);

impl Drop for AdmittedWrite<'_> {
    fn drop(&mut self) {
        self.0.finish();
    }
}

/// The final message. Names the bound, what happens next, and what to change,
/// then ends with [`LOG_CAP_CLASS_LINE`].
///
/// THE CLASS LINE IS PART OF THE MESSAGE because a crossing can end hermit with
/// no outer process left to print a report: under `run --namespace-only`
/// tracing runs in the outer hermit itself, so its crossing `_exit`s without
/// reaching `display_error`. Where an outer hermit does survive (the container
/// paths), it prints the class line again in its own report, so stderr may
/// carry it twice; both are best effort. The whole message stays far below
/// `PIPE_BUF`, so [`write_without_waiting`]'s truncation never cuts the marker.
fn exceeded_message(limit: u64) -> String {
    format!(
        "hermit: log output exceeded --max-log-bytes={} ({} bytes); aborting the run and killing \
         the guest process tree (exit {}). Lower --log / RUST_LOG verbosity, or raise \
         --max-log-bytes, to let the run finish.\n{}",
        format_byte_size(limit),
        limit,
        hermit::HERMIT_LOG_CAP_EXIT,
        LOG_CAP_CLASS_LINE,
    )
}

/// Counts the bytes offered to hermit's public log sink against a
/// [`LogBudget`] and ends the process when the budget is exhausted.
///
/// This sits OUTSIDE [`BoundedWriter`]: the file bound silently discards
/// beyond 1 GiB while the run continues, so counting after it would never see a
/// runaway once the file stopped growing. The incident this exists for
/// (2026-08-17) was hermit's own stderr at `--log=info` -- ~4.5 TB per process
/// from the scheduler's per-poll banner -- which had no bound at all.
///
/// WHY `_exit` FROM INSIDE A WRITER. The writer runs wherever tracing does: the
/// container init (PID 1 of the run's PID namespace, whose exit makes the
/// kernel SIGKILL every guest in it), the outer hermit process (whose exit
/// ends that init through its parent-death signal), or the non-blocking
/// appender's worker thread in one of those processes. In each case ending the
/// process is exactly how hermit's other deliberate stops tear the guest tree
/// down (`record --record-timeout`, the container-init stop-signal handler).
/// Where no PID namespace contains the guest -- `--no-namespace`, whose ptrace
/// tracer binds the guest with PTRACE_O_EXITKILL only after it exists, and
/// DBT -- the cap is refused before any guest starts
/// (`RunOpts::log_cap_refusal`).
/// `_exit` rather than `exit` because this can run on any thread, possibly
/// while the subscriber's writer lock is held, and there is nothing left worth
/// flushing: every sink here is unbuffered. The parent maps the status through
/// `classify_container_result`.
///
/// THE FINAL MESSAGE NEVER WAITS FOR A SINK. It is written with
/// [`write_without_waiting`], not through the inner writer or
/// `RetryingStderr`: a blocking `write(2)` to a full pipe whose reader has
/// stopped reading, or to a FIFO log sink, would hold the `_exit` -- and with
/// it the guest teardown -- for as long as nobody drains the sink.
///
/// THE ONE WAIT IS BOUNDED, AND IT IS FOR OTHER WRITERS. Before the message the
/// crossing writer waits at most [`LOG_CAP_DRAIN_BOUND`] for log writes that
/// were admitted before the crossing to finish (see [`admit_write`]), so that
/// none of them can put a line after the message. If one is still in progress
/// then -- blocked on a full pipe, say -- the message is omitted, and the exit
/// follows at once either way.
pub struct CappedWriter<W: Write> {
    inner: W,
    budget: Option<LogBudget>,
    /// Where the final message goes besides stderr: the descriptor under the
    /// inner writer. `None` when the inner writer is stderr itself (so the
    /// message is written once) or has no descriptor (an in-memory test sink).
    final_message_fd: Option<RawFd>,
    /// Whether the inner writer has already ended its output with its own
    /// last line. [`BoundedWriter`] writes the truncation marker as the final
    /// bytes of a truncated log, and the comparator recognizes a truncated log
    /// only by that marker at end of file, so the final message must not be
    /// appended after it.
    inner_has_final_line: fn(&W) -> bool,
}

impl<W: Write> CappedWriter<W> {
    /// Wrap a sink with no descriptor of its own; the final message goes to
    /// stderr only. `None` disables the cap (no counting at all).
    pub fn new(inner: W, budget: Option<LogBudget>) -> Self {
        Self {
            inner,
            budget,
            final_message_fd: None,
            inner_has_final_line: |_| false,
        }
    }

    fn exceeded(&mut self, budget: &LogBudget) -> ! {
        // No write is admitted any more. Wait, for at most the drain bound,
        // for the writes admitted before the crossing; a line one of them
        // wrote after the final message would leave the message not last. If
        // they do not finish in time, omit the message and exit 123 anyway.
        if budget.writes_drained() {
            self.write_final_message(budget);
        }
        // SAFETY: _exit has no preconditions; see the type-level note for why
        // the immediate exit is the teardown.
        unsafe { libc::_exit(hermit::HERMIT_LOG_CAP_EXIT) }
    }

    fn write_final_message(&mut self, budget: &LogBudget) {
        let message = budget.exceeded_message().as_bytes();
        if let Some(fd) = self.final_message_fd {
            // Best effort: the log should say why it ends. Written to the
            // descriptor directly, so it lands even when BoundedWriter is
            // close to its bound (a few hundred bytes past it, once) -- but
            // NOT once BoundedWriter has truncated: its marker must stay the
            // final line of a truncated log, because that is the only thing
            // `detcore::logdiff::log_was_truncated` accepts as truncation.
            // Stderr still gets an attempt below, also without waiting, so
            // delivery there is best effort too.
            if !(self.inner_has_final_line)(&self.inner) {
                write_without_waiting(fd, message);
            }
        }
        write_without_waiting(libc::STDERR_FILENO, message);
    }
}

impl CappedWriter<BoundedWriter<File>> {
    /// Wrap a log file under its `HERMIT_LOG_MAX_BYTES` bound (`file_limit`,
    /// `0` for none). `None` disables the cap (no counting at all).
    pub fn file(file: File, file_limit: u64, budget: Option<LogBudget>) -> Self {
        let fd = file.as_raw_fd();
        Self {
            inner: BoundedWriter::new(file, file_limit),
            budget,
            final_message_fd: Some(fd),
            inner_has_final_line: BoundedWriter::has_truncated,
        }
    }
}

impl CappedWriter<detcore::util::DroppingStderr> {
    /// Wrap hermit's stderr sink.
    ///
    /// Under `--max-log-bytes` the sink never waits inside `write(2)`: a full
    /// pipe gets `detcore::util::RetryingStderr`'s bounded wait, shared by the
    /// whole invocation, and a line it then cannot deliver is dropped but
    /// still charged to the budget, so a run whose stderr reader stopped
    /// reaches the cap and exits 123 (see `detcore::util::DroppingStderr`).
    /// Without the cap it is `RetryingStderr`, as before.
    pub fn stderr(budget: Option<LogBudget>) -> Self {
        Self {
            inner: detcore::util::DroppingStderr,
            budget,
            final_message_fd: None,
            inner_has_final_line: |_| false,
        }
    }
}

/// Set by `main` when `--max-log-bytes` is in force; see [`bound_log_cap_exit`].
static LOG_CAP_EXIT_BOUND_ENABLED: AtomicBool = AtomicBool::new(false);
/// Whether the outer diagnostic -- `main`'s final report or the exit path's
/// class line -- has been attempted. Both paths claim it with a swap, so it is
/// attempted at most once.
static LOG_CAP_REPORTED: AtomicBool = AtomicBool::new(false);
static LOG_CAP_EXIT_SCHEDULED: AtomicBool = AtomicBool::new(false);

/// How long the outer process may keep reporting after it learns that a run
/// crossed the cap, before it exits 123 regardless.
const LOG_CAP_OUTER_GRACE: std::time::Duration = std::time::Duration::from_millis(750);

/// Let [`bound_log_cap_exit`] act in this process. Only `main` calls this, so
/// unit tests that classify a 123 status never start the exit timer.
///
/// It also forbids waiting diagnostics
/// ([`hermit::nonwaiting_write::forbid_waiting_diagnostics`]) in this process
/// and in every process it forks afterwards, which is why `main` calls it
/// before any fork: the library's `/proc` warning reads that flag in the
/// namespace-only `pre_exec` callback and in the container init.
pub(crate) fn enable_log_cap_exit_bound() {
    LOG_CAP_EXIT_BOUND_ENABLED.store(true, Ordering::Relaxed);
    hermit::nonwaiting_write::forbid_waiting_diagnostics();
}

/// Write a diagnostic `message` to stderr from the outer process.
///
/// While `--max-log-bytes` is in force, termination of the run must not
/// depend on any diagnostic write (review of
/// https://github.com/rrnewton/hermit/pull/3686), including the ones printed
/// before the cap can cross at all. So the message is written once without
/// waiting, and omitted when stderr cannot take it at once (see
/// [`write_without_waiting`]). Without the cap it goes through
/// `detcore::util::RetryingStderr`, as before.
pub(crate) fn write_stderr_diagnostic(message: &str) {
    if hermit::nonwaiting_write::diagnostics_must_not_wait() {
        write_without_waiting(libc::STDERR_FILENO, message.as_bytes());
    } else {
        let _ = detcore::util::RetryingStderr.write_all(message.as_bytes());
    }
}

/// `eprintln!` for a diagnostic that can run in a capped run before the cap
/// or the 123 classification: the warnings and banners the outer process
/// prints while it validates its arguments and prepares the guest.
///
/// While `--max-log-bytes` is in force
/// ([`hermit::nonwaiting_write::diagnostics_must_not_wait`]) the formatted
/// line, newline included, is written once with [`write_without_waiting`]
/// and omitted when stderr cannot take it at once; a line longer than
/// `PIPE_BUF` is cut there. Otherwise this is exactly the `eprintln!` it
/// replaces, with the same arguments, so uncapped output is unchanged.
macro_rules! diagnostic_eprintln {
    ($($arg:tt)*) => {
        if hermit::nonwaiting_write::diagnostics_must_not_wait() {
            let mut line = format!($($arg)*);
            line.push('\n');
            hermit::nonwaiting_write::write_without_waiting(
                libc::STDERR_FILENO,
                line.as_bytes(),
            );
        } else {
            eprintln!($($arg)*);
        }
    };
}
pub(crate) use diagnostic_eprintln;

/// The process that classifies a container's 123 status must itself exit 123
/// within a fixed bound, whatever its remaining diagnostics do: between the
/// classification and `main`'s final report it can still print through
/// blocking `eprintln!` calls (cleanup notices, verification notes) to a
/// stderr nobody reads. This starts a thread that, after
/// [`LOG_CAP_OUTER_GRACE`], attempts the class line once without waiting
/// (unless the final report already did) and calls `_exit(123)`. A normal exit
/// before then ends the thread with the process.
///
/// Two ways around that timer are closed here. If the thread cannot be
/// created, nothing would bound the diagnostics that follow, so the process
/// exits 123 at once. And from here on a panic exits 123 instead of
/// unwinding to the panic status 101, because the run's class is already
/// decided: `eprintln!`, for one, panics when stderr's reader has gone. A
/// failed `analyze` or `bisect` trial does not panic; it returns its error to
/// `main`, which reports a cap crossing by its class.
pub(crate) fn bound_log_cap_exit() {
    bound_log_cap_exit_with(|timer| {
        std::thread::Builder::new()
            .name("log-cap-exit".to_string())
            .spawn(timer)
            .map(drop)
    });
}

/// [`bound_log_cap_exit`] with the timer thread's creation supplied, so a test
/// can make it fail.
fn bound_log_cap_exit_with(start_timer: impl FnOnce(Box<dyn FnOnce() + Send>) -> io::Result<()>) {
    if !LOG_CAP_EXIT_BOUND_ENABLED.load(Ordering::Relaxed)
        || LOG_CAP_EXIT_SCHEDULED.swap(true, Ordering::Relaxed)
    {
        return;
    }
    std::panic::set_hook(Box::new(|info| {
        write_without_waiting(
            libc::STDERR_FILENO,
            format!("hermit: panic after the log cap ended the run: {info}\n").as_bytes(),
        );
        exit_with_log_cap_status()
    }));
    let timer: Box<dyn FnOnce() + Send> = Box::new(|| {
        std::thread::sleep(LOG_CAP_OUTER_GRACE);
        exit_with_log_cap_status()
    });
    if start_timer(timer).is_err() {
        exit_with_log_cap_status()
    }
}

/// Attempt the class line once without waiting, unless a report already did,
/// and exit 123.
fn exit_with_log_cap_status() -> ! {
    if !LOG_CAP_REPORTED.swap(true, Ordering::Relaxed) {
        write_without_waiting(libc::STDERR_FILENO, LOG_CAP_CLASS_LINE.as_bytes());
    }
    // SAFETY: _exit has no preconditions.
    unsafe { libc::_exit(hermit::HERMIT_LOG_CAP_EXIT) }
}

/// The stderr class line for a run the log cap ended.
pub(crate) const LOG_CAP_CLASS_LINE: &str = "HERMIT_LOG_CAP class=log-cap\n";

/// Write the outer report for a run the log cap ended: once, without waiting,
/// omitted when it cannot be delivered at once (see [`write_without_waiting`]).
/// It is elected with the same swap as [`exit_with_log_cap_status`], so if the
/// exit timer's class line, or an earlier report, was already attempted, this
/// attempts nothing.
pub(crate) fn report_log_cap_without_waiting(report: &str) {
    if !LOG_CAP_REPORTED.swap(true, Ordering::Relaxed) {
        write_without_waiting(libc::STDERR_FILENO, report.as_bytes());
    }
}

impl<W: Write> Write for CappedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let Some(budget) = &self.budget else {
            return self.inner.write(buf);
        };
        match budget.charge(buf.len() as u64) {
            Charge::Within => {}
            Charge::Crossed => {
                let budget = budget.clone();
                self.exceeded(&budget)
            }
            // The crossing writer is already ending the process. Report the
            // bytes consumed (as BoundedWriter does) rather than erroring.
            Charge::AlreadyOver => return Ok(buf.len()),
        }
        // Ends the admission after the write, on every path out of it.
        let _admitted = AdmittedWrite(budget);
        let written = self.inner.write(buf).inspect_err(|_| {
            budget.refund(buf.len() as u64);
        })?;
        if written < buf.len() {
            budget.refund((buf.len() - written) as u64);
        }
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn env_filter(level: LevelFilter) -> EnvFilter {
    EffectiveFilter::from_default_env(level).into_filter()
}

/// Keeps a nonblocking public writer alive when one is installed. A private
/// evidence layer is synchronous and therefore needs no drain worker or guard.
pub struct TracingGuard {
    _worker: Option<tracing_appender::non_blocking::WorkerGuard>,
}

impl TracingGuard {
    fn synchronous() -> Self {
        Self { _worker: None }
    }
}

/// Returns a non-blocking subscriber for logging to a file.
///
/// NOTE: Writes to `f` are unbuffered, so this may be slow.
fn file_subscriber<W: Write + Send + 'static>(
    level: LevelFilter,
    f: W,
) -> (impl Subscriber, tracing_appender::non_blocking::WorkerGuard) {
    let filter = env_filter(level);
    let (writer, guard) = tracing_appender::non_blocking(f);

    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(false)
        .finish();

    (subscriber, guard)
}

/// A file subscriber whose writes CANNOT be lost when the process dies.
///
/// `file_subscriber` uses `tracing_appender::non_blocking`, which queues to a
/// background thread and only drains when its guard drops. A run that dies
/// without unwinding -- which is exactly what a fail-closed guest does -- never
/// drops the guard, so the QUEUED TAIL IS LOST. The tail is where a fatal
/// diagnostic lives, so the one line that explains the failure is the one line
/// reliably discarded.
///
/// MEASURED 2026-08-25 on c-programs/dbt-unsupported-syscall under `--verify`:
///   non-blocking  18 runs, diagnostic present 0 times, logs truncated at a
///                 RANDOM syscall each run (#5,#6,#10,#11,#16,#23,#27,#31...)
///                 and random sizes (5,482 / 7,632 / 10,123 / 15,546 bytes)
///   synchronous    4 runs, diagnostic present 4 times, every run reaching
///                 syscall #32 and producing an IDENTICAL 16,171-byte log
/// The randomness was the tell: a deterministic guest under a deterministic
/// engine cannot genuinely die at a different syscall each time.
///
/// COST, measured rather than assumed, on a 477 KB verify log:
/// synchronous 1.77s versus non-blocking 1.73s -- inside noise, identical
/// output size. The queue was buying nothing and losing the diagnostic.
fn sync_file_subscriber<W: Write + Send + 'static>(level: LevelFilter, f: W) -> impl Subscriber {
    let filter = env_filter(level);
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::sync::Mutex::new(f))
        .with_ansi(false)
        .finish()
}

/// Initializes SYNCHRONOUS tracing to `f`, for logs whose tail must survive a
/// crash. See [`sync_file_subscriber`].
pub fn init_sync_file_tracing<W: Write + Send + 'static>(
    level: Option<LevelFilter>,
    f: W,
    backend: Backend,
) -> TracingGuard {
    // Match the PID/TID slot consumed by file tracing's worker while keeping
    // verification evidence synchronous and the namespace single-threaded.
    align_tracing_pid_baseline(backend);
    let level = level.unwrap_or(DEFAULT_TRACE_LEVEL);
    let subscriber = sync_file_subscriber(level, f);
    subscriber
        .try_init()
        .expect("global tracing subscriber to install");
    TracingGuard::synchronous()
}

/// Initializes tracing to the given file `f`.
///
/// NOTE: Writes to `f` are unbuffered, so this may be slow.
#[must_use = "This function returns a guard that should not be immediately dropped"]
pub fn init_file_tracing<W: Write + Send + 'static>(
    level: Option<LevelFilter>,
    f: W,
) -> TracingGuard {
    let level = level.unwrap_or(DEFAULT_TRACE_LEVEL);

    let (subscriber, guard) = file_subscriber(level, f);

    subscriber
        .try_init()
        .expect("global tracing subscriber to install");

    TracingGuard {
        _worker: Some(guard),
    }
}

/// Make the next guest task use the same identity as the asynchronous logger.
///
/// `init_file_tracing` starts a nonblocking writer thread inside the fresh PID
/// namespace. The namespace init is PID 1, that worker consumes PID 2, and the
/// root guest consequently starts at [`detcore::ROOT_DETPID`] (PID 3). The
/// synchronous and stderr subscribers have no worker, but their guests still
/// need the same identity baseline.
///
/// Creating and joining a dummy host thread to consume PID 2 is not equivalent:
/// the thread participates in the parent's signal/timer state and made KVM's
/// `setitimer` determinism case die from SIGALRM. A forked process has separate
/// signal and interval-timer state, and it exits before any backend starts.
/// Outside a PID-namespace init (notably `--no-namespace`) this is a deliberate
/// no-op; Hermit must not create a process merely to alter the host namespace.
fn align_tracing_pid_baseline(backend: Backend) {
    // KVM supplies the deterministic root identity explicitly rather than
    // deriving it from a Linux task in this namespace. Allocating PID 2 for
    // that backend is both unnecessary and harmful: it makes the setitimer
    // sandbox die from SIGALRM before a comparison can run.
    if backend == Backend::Kvm {
        return;
    }

    // SAFETY: getpid has no preconditions and cannot fail.
    if unsafe { libc::getpid() } != 1 {
        return;
    }

    // SAFETY: the child returns through `_exit` immediately and touches no Rust
    // allocation, lock, or destructor inherited across fork. POSIX specifies
    // that interval timers are not inherited by a fork child.
    let child = unsafe { libc::fork() };
    assert!(
        child >= 0,
        "failed to reserve the tracing PID slot: {}",
        io::Error::last_os_error()
    );
    if child == 0 {
        unsafe { libc::_exit(0) };
    }

    let mut status = 0;
    loop {
        let waited = unsafe { libc::waitpid(child, &mut status, 0) };
        if waited == child {
            break;
        }
        let error = io::Error::last_os_error();
        if waited < 0 && error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        panic!("failed to reap tracing PID-slot process {child}: {error}");
    }
    assert!(
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
        "tracing PID-slot process {child} exited unexpectedly: {status:#x}"
    );
    assert_eq!(
        child,
        detcore::ROOT_DETPID.as_raw() - 1,
        "fresh PID namespace did not reserve the expected tracing slot"
    );
}

/// Returns a tracing subscriber that logs to `stderr`.
///
/// NOTE: Writes to stderr are unbuffered, so this may be slow.
pub fn stderr_subscriber(level: Option<LevelFilter>, budget: Option<LogBudget>) -> impl Subscriber {
    let level = level.unwrap_or(DEFAULT_TRACE_LEVEL);

    let filter = env_filter(level);
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        // NOT `io::stderr`: a guest can set O_NONBLOCK on the inherited fd 2,
        // after which a full pipe makes these writes fail with EAGAIN. The fmt
        // layer discards the write error, so log lines would vanish with no
        // marker at all. `RetryingStderr` waits for the reader instead of
        // dropping, and does not alter the flag the guest set. `CappedWriter`
        // charges each event against `--max-log-bytes`, when given.
        .with_writer(move || CappedWriter::stderr(budget.clone()))
        .with_ansi(stderr().is_terminal())
        .finish()
}

/// Initializes tracing to `stderr`.
///
/// NOTE: Writes to stderr are unbuffered, so this may be slow.
pub fn init_stderr_tracing(
    level: Option<LevelFilter>,
    backend: Backend,
    budget: Option<LogBudget>,
) {
    align_tracing_pid_baseline(backend);

    stderr_subscriber(level, budget)
        .try_init()
        .expect("global tracing subscriber to install")
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use hermit::nonwaiting_write::last_errno;
    use hermit::nonwaiting_write::proc_self_fd_path;
    use hermit::nonwaiting_write::suppressing_diagnostic_signals;
    use reverie::process::Container;
    use reverie::process::Mount;
    use reverie::process::Namespace;

    use super::*;

    /// `log_max_bytes` reads the process environment, which libtest's threads
    /// share.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn pid_namespace() -> Container {
        let mut container = Container::new();
        container
            .unshare(Namespace::PID)
            .map_root()
            .mount(Mount::proc().allow_readonly_fallback());
        container
    }

    fn fork_and_observe_child_pid() -> i32 {
        // SAFETY: the child makes only getpid and _exit calls. The parent waits
        // synchronously, so no child is left behind in the test namespace.
        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork failed: {}", io::Error::last_os_error());
        if child == 0 {
            let observed = unsafe { libc::getpid() };
            unsafe {
                libc::_exit(if observed == detcore::ROOT_DETPID.as_raw() {
                    0
                } else {
                    1
                })
            };
        }
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 0, "child observed the wrong PID");
        child
    }

    fn deny_host_thread_creation() -> io::Result<()> {
        let denied = 0x0005_0000 | libc::EPERM as u32; // SECCOMP_RET_ERRNO
        let mut filter = [
            libc::sock_filter {
                code: 0x20, // BPF_LD | BPF_W | BPF_ABS
                jt: 0,
                jf: 0,
                k: 0, // offsetof(seccomp_data, nr)
            },
            libc::sock_filter {
                code: 0x15, // BPF_JMP | BPF_JEQ | BPF_K
                jt: 4,
                jf: 0,
                k: libc::SYS_clone3 as u32,
            },
            libc::sock_filter {
                code: 0x15,
                jt: 0,
                jf: 2,
                k: libc::SYS_clone as u32,
            },
            libc::sock_filter {
                code: 0x20, // BPF_LD | BPF_W | BPF_ABS
                jt: 0,
                jf: 0,
                k: 16, // offsetof(seccomp_data, args[0])
            },
            libc::sock_filter {
                code: 0x45, // BPF_JMP | BPF_JSET | BPF_K
                jt: 1,
                jf: 0,
                k: libc::CLONE_THREAD as u32,
            },
            libc::sock_filter {
                code: 0x06, // BPF_RET | BPF_K
                jt: 0,
                jf: 0,
                k: 0x7fff_0000, // SECCOMP_RET_ALLOW
            },
            libc::sock_filter {
                code: 0x06, // BPF_RET | BPF_K
                jt: 0,
                jf: 0,
                k: denied,
            },
        ];
        let program = libc::sock_fprog {
            len: filter.len() as u16,
            filter: filter.as_mut_ptr(),
        };
        // SAFETY: `program` describes the live filter array above. The filter
        // allows process creation but rejects clone3 and clone(CLONE_THREAD),
        // the two paths std::thread uses. It is installed only in the
        // disposable namespace child used by the test.
        unsafe {
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::prctl(
                libc::PR_SET_SECCOMP,
                libc::SECCOMP_MODE_FILTER,
                &program as *const libc::sock_fprog,
            ) == -1
            {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    #[test]
    fn synchronous_pid_alignment_matches_the_file_appender_baseline() {
        let synchronous_pid = pid_namespace()
            .run(|| {
                assert_eq!(unsafe { libc::getpid() }, 1);
                align_tracing_pid_baseline(Backend::Ptrace);
                fork_and_observe_child_pid()
            })
            .expect("synchronous baseline namespace failed");

        let appender_pid = pid_namespace()
            .run(|| {
                assert_eq!(unsafe { libc::getpid() }, 1);
                let (_subscriber, guard) = file_subscriber(DEFAULT_TRACE_LEVEL, io::sink());
                let child = fork_and_observe_child_pid();
                drop(guard);
                child
            })
            .expect("file-appender baseline namespace failed");

        assert_eq!(synchronous_pid, detcore::ROOT_DETPID.as_raw());
        assert_eq!(synchronous_pid, appender_pid);
    }

    #[test]
    fn pid_alignment_succeeds_when_host_thread_creation_is_denied() {
        let guest_pid = pid_namespace()
            .run(|| {
                assert_eq!(unsafe { libc::getpid() }, 1);
                deny_host_thread_creation().expect("cannot install host-thread deny filter");
                align_tracing_pid_baseline(Backend::Ptrace);
                fork_and_observe_child_pid()
            })
            .expect("thread-free alignment namespace failed");

        assert_eq!(guest_pid, detcore::ROOT_DETPID.as_raw());
    }

    #[test]
    fn kvm_pid_alignment_allocates_no_host_task() {
        let next_pid = pid_namespace()
            .run(|| {
                assert_eq!(unsafe { libc::getpid() }, 1);
                align_tracing_pid_baseline(Backend::Kvm);
                // No task was allocated by alignment, so this first actual
                // child must receive PID 2 rather than PID 3.
                let child = unsafe { libc::fork() };
                assert!(child >= 0, "fork failed: {}", io::Error::last_os_error());
                if child == 0 {
                    unsafe { libc::_exit(0) };
                }
                let mut status = 0;
                assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
                assert!(libc::WIFEXITED(status));
                assert_eq!(libc::WEXITSTATUS(status), 0);
                child
            })
            .expect("KVM no-allocation namespace failed");

        assert_eq!(next_pid, 2);
    }

    /// Bracketed both ways on purpose: a bound that always fires would silently
    /// truncate ordinary diagnostic logs, which is the failure mode opposite to
    /// the one being fixed and just as damaging to an investigation.
    #[test]
    fn bound_truncates_and_announces_exactly_once() {
        let mut sink: Vec<u8> = Vec::new();
        {
            let mut writer = BoundedWriter::new(&mut sink, 100);
            for _ in 0..50 {
                writer.write_all(&[b'x'; 10]).unwrap();
            }
            writer.flush().unwrap();
        }
        let body = sink.iter().filter(|byte| **byte == b'x').count();
        let text = String::from_utf8_lossy(&sink);
        assert_eq!(body, 100, "wrote past the bound");
        assert_eq!(
            text.matches("HERMIT LOG TRUNCATED").count(),
            1,
            "truncation must be announced exactly once, not per write"
        );
    }

    #[test]
    fn under_the_bound_nothing_is_truncated_or_announced() {
        let mut sink: Vec<u8> = Vec::new();
        {
            let mut writer = BoundedWriter::new(&mut sink, 1000);
            for _ in 0..50 {
                writer.write_all(&[b'x'; 10]).unwrap();
            }
        }
        assert_eq!(sink.len(), 500);
        assert!(!String::from_utf8_lossy(&sink).contains("HERMIT LOG TRUNCATED"));
    }

    #[test]
    fn a_zero_limit_disables_the_bound() {
        let mut sink: Vec<u8> = Vec::new();
        {
            let mut writer = BoundedWriter::new(&mut sink, 0);
            writer.write_all(&[b'z'; 4096]).unwrap();
        }
        assert_eq!(sink.len(), 4096);
    }

    /// The regression the unit tests originally missed and an end-to-end run
    /// caught: a log whose FINAL write crosses the bound must still announce
    /// itself. Deferring the marker to the next write left a 100-byte-bounded
    /// log at exactly 100 bytes with no marker at all.
    #[test]
    fn a_single_straddling_write_announces_without_a_following_write() {
        let mut sink: Vec<u8> = Vec::new();
        {
            let mut writer = BoundedWriter::new(&mut sink, 10);
            writer.write_all(&[b'q'; 40]).unwrap();
            // deliberately no further write
        }
        let text = String::from_utf8_lossy(&sink);
        assert_eq!(
            text.matches("HERMIT LOG TRUNCATED").count(),
            1,
            "a truncated final write must announce itself: {text}"
        );
        assert_eq!(sink.iter().filter(|b| **b == b'q').count(), 10);
    }

    /// What this writer produces must be what the comparator recognizes.
    ///
    /// `detcore::logdiff` refuses to return a comparison verdict for a
    /// truncated log. If the bytes written here ever drift from what
    /// [`detcore::logdiff::log_was_truncated`] accepts, that refusal stops
    /// firing and a truncated pair silently returns to comparing only its
    /// prefix -- with nothing failing to say so. This test is the binding
    /// between them.
    ///
    /// It runs the REAL writer's output through the REAL predicate rather than
    /// comparing two constants, so it binds the anchoring as well as the text:
    /// the marker must survive as a whole line at end of file, which is what
    /// the comparator actually keys on. Drift in this file's literal or in
    /// detcore's constant or predicate all fail it.
    #[test]
    fn the_written_marker_is_the_text_the_comparator_matches() {
        let mut sink: Vec<u8> = Vec::new();
        {
            let mut writer = BoundedWriter::new(&mut sink, 64);
            // A realistic tail: whole "lines", the last of which straddles the
            // bound, so the marker has to terminate a partial line.
            for _ in 0..20 {
                writer
                    .write_all(b"2022-09-06T14:15:47.000000Z INFO detcore: DETLOG x\n")
                    .unwrap();
            }
            writer.flush().unwrap();
        }
        let written = String::from_utf8(sink).unwrap();
        assert!(
            detcore::logdiff::log_was_truncated(&written),
            "the bounded writer produced {written:?}, which detcore::logdiff does not classify \
             as truncated; a truncated pair would silently be compared on its prefix"
        );

        // The other direction, so this cannot pass by the predicate accepting
        // everything: the same content under a bound it never reaches is NOT
        // truncated.
        let mut unbounded: Vec<u8> = Vec::new();
        {
            let mut writer = BoundedWriter::new(&mut unbounded, 0);
            writer
                .write_all(b"2022-09-06T14:15:47.000000Z INFO detcore: DETLOG x\n")
                .unwrap();
        }
        assert!(
            !detcore::logdiff::log_was_truncated(&String::from_utf8(unbounded).unwrap()),
            "an untruncated log must not be classified as truncated"
        );
    }

    /// A malformed bound must be an error, not a silent 1 GiB.
    ///
    /// `HERMIT_LOG_MAX_BYTES=0` is the documented way to disable the bound, so
    /// a value that fails to parse must not quietly become the default: that
    /// would re-enable the bound for anyone who typed `unlimited`, `none`, or
    /// `1GiB` and reasonably believed it was off.
    ///
    /// Serialized against the other env-var cases in this file because the
    /// process environment is global; `log_max_bytes` reads it directly.
    #[test]
    fn a_malformed_bound_is_an_error_not_a_silent_default() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let restore = std::env::var(LOG_MAX_BYTES_ENV).ok();

        // SAFETY: guarded by ENV_LOCK; no other thread in this test binary
        // reads the environment concurrently.
        unsafe {
            for bad in ["unlimited", "none", "1GiB", "-1", "", "1 000"] {
                std::env::set_var(LOG_MAX_BYTES_ENV, bad);
                assert!(
                    log_max_bytes().is_err(),
                    "{bad:?} must be rejected, not silently read as {DEFAULT_LOG_MAX_BYTES}"
                );
            }
            // Bracket the accepting direction, including the documented
            // "disable" value, so this is not a check that rejects everything.
            for (good, expected) in [("0", 0u64), ("4096", 4096), (" 4096 ", 4096)] {
                std::env::set_var(LOG_MAX_BYTES_ENV, good);
                assert_eq!(
                    log_max_bytes().unwrap(),
                    expected,
                    "{good:?} must be accepted"
                );
            }
            std::env::remove_var(LOG_MAX_BYTES_ENV);
            assert_eq!(
                log_max_bytes().unwrap(),
                DEFAULT_LOG_MAX_BYTES,
                "an unset variable is the only thing that means the default"
            );
            match restore {
                Some(v) => std::env::set_var(LOG_MAX_BYTES_ENV, v),
                None => std::env::remove_var(LOG_MAX_BYTES_ENV),
            }
        }
    }

    /// The non-obvious correctness point: `tracing_appender` treats a short
    /// write as an I/O error, so a discarding writer must still report the
    /// caller's whole buffer as consumed.
    #[test]
    fn a_write_straddling_the_bound_reports_full_consumption() {
        let mut sink: Vec<u8> = Vec::new();
        let mut writer = BoundedWriter::new(&mut sink, 5);
        assert_eq!(writer.write(&[b'y'; 40]).unwrap(), 40);
    }

    #[test]
    fn max_log_bytes_accepts_binary_suffixes() {
        for (raw, expected) in [
            ("1", 1),
            ("4096", 4096),
            ("1k", 1 << 10),
            ("64K", 64 << 10),
            ("512m", 512 << 20),
            ("8G", 8 << 30),
            ("8g", 8 << 30),
            ("8Gi", 8 << 30),
            ("8GiB", 8 << 30),
            ("8GB", 8 << 30),
            ("2T", 2 << 40),
            ("100b", 100),
            (" 8G ", 8 << 30),
        ] {
            assert_eq!(parse_max_log_bytes(raw), Ok(expected), "{raw:?}");
        }
    }

    #[test]
    fn max_log_bytes_refusals_redirect_to_a_working_value() {
        for raw in [
            "",
            "G",
            "-1",
            "8X",
            "8.5G",
            "8 G",
            "99999999999999999999",
            "20000000T",
        ] {
            let error = parse_max_log_bytes(raw).expect_err(raw);
            assert!(
                error.contains("e.g. --max-log-bytes=8G"),
                "{raw:?}: {error}"
            );
        }
        for raw in ["0", "0G"] {
            let error = parse_max_log_bytes(raw).expect_err(raw);
            assert!(
                error.contains("Omit --max-log-bytes") && error.contains("8G"),
                "{raw:?}: {error}"
            );
        }
    }

    #[test]
    fn byte_sizes_render_in_the_accepted_spelling() {
        for (bytes, expected) in [
            (8u64 << 30, "8G"),
            (1536 << 20, "1536M"),
            (64 << 10, "64K"),
            (3 << 40, "3T"),
            (1000, "1000"),
        ] {
            assert_eq!(format_byte_size(bytes), expected);
            assert_eq!(parse_max_log_bytes(expected), Ok(bytes));
        }
    }

    #[test]
    fn the_budget_reports_the_crossing_exactly_once() {
        let budget = LogBudget::new(10).unwrap();
        assert_eq!(budget.charge(4), Charge::Within);
        assert_eq!(budget.charge(6), Charge::Within, "exactly the limit fits");
        assert_eq!(budget.charge(1), Charge::Crossed);
        assert_eq!(budget.charge(1), Charge::AlreadyOver);
        assert_eq!(budget.charge(100), Charge::AlreadyOver);

        let budget = LogBudget::new(10).unwrap();
        assert_eq!(budget.charge(8), Charge::Within);
        budget.refund(3);
        assert_eq!(budget.spent(), 5);
        assert_eq!(budget.charge(u64::MAX), Charge::Crossed, "no wraparound");
        assert_eq!(
            budget.spent(),
            u64::MAX,
            "the stored total saturates instead of wrapping to 4"
        );
    }

    /// The largest accepted limit still fires: a total that would overflow
    /// `u64` is a crossing, and the stored total never wraps back under the
    /// limit (review of https://github.com/rrnewton/hermit/pull/3686,
    /// round 2, finding 7).
    #[test]
    fn a_total_past_u64_max_crosses_even_the_largest_limit() {
        let budget = LogBudget::new(u64::MAX).unwrap();
        assert_eq!(budget.charge(u64::MAX - 1), Charge::Within);
        assert_eq!(budget.charge(1), Charge::Within, "exactly u64::MAX fits");
        assert_eq!(budget.spent(), u64::MAX);
        assert_eq!(budget.charge(1), Charge::Crossed, "one byte past u64::MAX");
        assert_eq!(budget.spent(), u64::MAX, "saturated, not wrapped to 0");
        assert_eq!(budget.charge(1), Charge::AlreadyOver);
        assert_eq!(budget.charge(u64::MAX), Charge::AlreadyOver);
        assert_eq!(budget.spent(), u64::MAX);
    }

    /// A crossing is final. Independent stderr writers refund failed or
    /// partial writes without a shared lock, so the total can fall back under
    /// the limit after a crossing; that must not produce a second crossing
    /// (review of https://github.com/rrnewton/hermit/pull/3686, round 1).
    #[test]
    fn a_refund_after_the_crossing_does_not_reopen_the_budget() {
        let budget = LogBudget::new(10).unwrap();
        assert_eq!(budget.charge(6), Charge::Within, "writer A");
        assert_eq!(budget.charge(5), Charge::Crossed, "writer B crosses at 11");
        budget.refund(6);
        assert_eq!(budget.spent(), 5, "writer A's failed write was refunded");
        assert_eq!(
            budget.charge(6),
            Charge::AlreadyOver,
            "writer C must not cross a second time"
        );
        assert_eq!(budget.charge(1), Charge::AlreadyOver);
    }

    /// The interleaving from review of
    /// https://github.com/rrnewton/hermit/pull/3686, round 3, finding 5, one
    /// step at a time: writer A is admitted within the limit and has not
    /// written yet when writer B crosses. B must not write the final message
    /// while A's write is in progress, or A's line could follow it.
    #[test]
    fn a_write_in_progress_at_the_crossing_withholds_the_final_message() {
        let budget = LogBudget::new(10).unwrap().with_drain_bound(Duration::ZERO);
        assert_eq!(budget.charge(6), Charge::Within, "A is admitted");
        assert_eq!(budget.in_progress(), 1);
        assert_eq!(budget.charge(5), Charge::Crossed, "B crosses at 11");
        assert_eq!(budget.in_progress(), 1, "B's own admission has ended");
        assert!(
            !budget.writes_drained(),
            "A's write is in progress, so B must omit the final message"
        );
        budget.finish();
        assert_eq!(budget.in_progress(), 0, "A has written");
        assert!(
            budget.writes_drained(),
            "nothing is in progress, so the message may be written last"
        );
        assert_eq!(budget.charge(1), Charge::AlreadyOver, "C is refused");
        assert_eq!(budget.in_progress(), 0, "and C was never admitted");
        assert!(budget.writes_drained());
    }

    /// The second interleaving from the same finding: writer C read the
    /// budget before the crossing, and its accounting retried after a refund
    /// had lowered the total, so it came out within the limit after the
    /// crossing. With admission, C was counted before the crossing, so the
    /// crossing writer waits for it; and a writer that comes after the
    /// crossing is refused before it charges anything, refund or not.
    #[test]
    fn a_write_admitted_before_the_crossing_is_waited_for_even_after_a_refund() {
        let budget = LogBudget::new(10).unwrap().with_drain_bound(Duration::ZERO);
        assert_eq!(budget.charge(6), Charge::Within, "A is admitted");
        assert!(admit_write(&budget.cells().admission), "C is admitted");
        assert_eq!(budget.charge(5), Charge::Crossed, "B crosses at 11");
        budget.refund(6);
        budget.finish();
        assert_eq!(budget.spent(), 5, "A's failed write was refunded");
        assert_eq!(
            budget.account(1),
            Charge::Within,
            "C's accounting fits after the refund"
        );
        assert_eq!(budget.in_progress(), 1, "C is still in progress");
        assert!(!budget.writes_drained(), "so the message is withheld");
        assert_eq!(budget.charge(1), Charge::AlreadyOver, "D is refused");
        assert_eq!(budget.spent(), 6, "D charged nothing");
        budget.finish();
        assert!(budget.writes_drained(), "C has written");
    }

    /// Two admitted writes both go past the limit: the first to set CROSSED is
    /// the crossing, the other is already over, and both admissions end.
    #[test]
    fn only_the_first_write_past_the_limit_claims_the_crossing() {
        let budget = LogBudget::new(10).unwrap();
        let admission = &budget.cells().admission;
        assert!(admit_write(admission), "A is admitted");
        assert!(admit_write(admission), "B is admitted");
        assert_eq!(budget.account(11), Charge::Crossed, "B crosses");
        assert_eq!(budget.account(1), Charge::AlreadyOver, "A is over");
        assert_eq!(budget.in_progress(), 0);
        assert_eq!(admission.load(Ordering::Relaxed), CROSSED);
        assert!(!admit_write(admission), "nothing is admitted after it");
        assert_eq!(admission.load(Ordering::Relaxed), CROSSED);
    }

    /// A write that never finishes -- its process was killed in the middle of
    /// it -- makes the crossing writer give up at the drain bound, and the
    /// production bound keeps the crossing process inside the one-second exit
    /// bound. A caller cannot set a bound above the maximum.
    #[test]
    fn the_drain_wait_gives_up_at_its_bound() {
        let budget = LogBudget::new(10).unwrap();
        assert_eq!(budget.charge(1), Charge::Within, "a write that never ends");
        assert_eq!(budget.charge(10), Charge::Crossed);
        for bound in [Duration::from_millis(30), LOG_CAP_DRAIN_BOUND] {
            let started = Instant::now();
            assert!(!wait_for_writes_in_progress(
                &budget.cells().admission,
                bound
            ));
            let waited = started.elapsed();
            assert!(
                waited >= bound,
                "gave up after {waited:?}, before {bound:?}"
            );
            // A generous ceiling for a loaded host: the wait itself is the
            // bound plus one poll of 1 ms; the rest is scheduling delay.
            assert!(
                waited < Duration::from_secs(1),
                "waited {waited:?} for a bound of {bound:?}"
            );
        }
        assert_eq!(
            LogBudget::new(1)
                .unwrap()
                .with_drain_bound(Duration::from_secs(10))
                .drain_bound,
            LOG_CAP_DRAIN_BOUND_MAX
        );
    }

    /// Every admission a CappedWriter starts ends when its write returns:
    /// after a whole write, a partial one, an error and a panic in the sink.
    /// A missed end would make every later crossing omit its final message.
    #[test]
    fn capped_writer_ends_every_admission_it_starts() {
        struct Sink(fn(&[u8]) -> io::Result<usize>);
        impl Write for Sink {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                (self.0)(buf)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let budget = LogBudget::new(1 << 20).unwrap();
        let mut whole = CappedWriter::new(Sink(|buf| Ok(buf.len())), Some(budget.clone()));
        assert_eq!(whole.write(b"abcd").unwrap(), 4);
        assert_eq!((budget.in_progress(), budget.spent()), (0, 4));
        let mut partial = CappedWriter::new(Sink(|buf| Ok(buf.len() / 2)), Some(budget.clone()));
        assert_eq!(partial.write(b"abcd").unwrap(), 2);
        assert_eq!((budget.in_progress(), budget.spent()), (0, 6));
        let mut refusing = CappedWriter::new(
            Sink(|_| Err(io::Error::from_raw_os_error(libc::EIO))),
            Some(budget.clone()),
        );
        assert!(refusing.write(b"abcd").is_err());
        assert_eq!((budget.in_progress(), budget.spent()), (0, 6));
        let mut panicking =
            CappedWriter::new(Sink(|_| panic!("the sink panicked")), Some(budget.clone()));
        let unwound =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| panicking.write(b"abcd")));
        assert!(unwound.is_err());
        assert_eq!(budget.in_progress(), 0);
    }

    /// Set in the re-executed child of the stress test below: the drain bound
    /// in milliseconds.
    const LOG_CAP_STRESS_CHILD: &str = "HERMIT_TEST_LOG_CAP_STRESS_DRAIN_MS";
    const LOG_CAP_STRESS_LIMIT: u64 = 64 << 10;
    const LOG_CAP_STRESS_WRITERS: usize = 8;
    const LOG_CAP_STRESS_RUNS: usize = 20;

    /// Real threads racing the crossing on one stderr pipe: whenever the final
    /// message is delivered, it is the last line (review of
    /// https://github.com/rrnewton/hermit/pull/3686, round 3, finding 5). The
    /// child must be a fresh process with its own writer threads, and the cap
    /// tests above fork, so this re-executes the test binary rather than
    /// forking it. 20 runs at a zero drain bound and 20 at the production
    /// bound. Run with --no-capture to see the delivery rates.
    #[test]
    fn the_final_message_is_the_last_line_when_writers_race_the_crossing() {
        if let Ok(bound) = std::env::var(LOG_CAP_STRESS_CHILD) {
            race_writers_to_the_crossing(Duration::from_millis(bound.parse().unwrap()));
        }
        let message = exceeded_message(LOG_CAP_STRESS_LIMIT);
        let message = message.as_bytes();
        for bound in [Duration::ZERO, LOG_CAP_DRAIN_BOUND] {
            let mut delivered = 0;
            for run in 0..LOG_CAP_STRESS_RUNS {
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "tracing::tests::the_final_message_is_the_last_line_when_writers_race_the_crossing",
                        "--exact",
                        "--nocapture",
                        "--test-threads=1",
                    ])
                    .env(LOG_CAP_STRESS_CHILD, bound.as_millis().to_string())
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::piped())
                    .output()
                    .unwrap();
                let stderr = &output.stderr;
                let tail = String::from_utf8_lossy(&stderr[stderr.len().saturating_sub(600)..]);
                assert_eq!(
                    output.status.code(),
                    Some(hermit::HERMIT_LOG_CAP_EXIT),
                    "run {run} at drain bound {bound:?}; stderr tail:\n{tail}"
                );
                let copies = stderr
                    .windows(message.len())
                    .filter(|window| *window == message)
                    .count();
                assert!(copies <= 1, "run {run}: {copies} final messages");
                if copies == 1 {
                    delivered += 1;
                    assert!(
                        stderr.ends_with(message),
                        "run {run} at drain bound {bound:?}: a line followed the final \
                         message; stderr tail:\n{tail}"
                    );
                }
            }
            eprintln!(
                "drain bound {bound:?}: the final message was delivered, last, in {delivered} \
                 of {LOG_CAP_STRESS_RUNS} runs; every run exited {}",
                hermit::HERMIT_LOG_CAP_EXIT
            );
        }
    }

    /// The child: writers on stderr, each through its own CappedWriter on one
    /// budget, until one of them crosses and ends the process.
    fn race_writers_to_the_crossing(drain_bound: Duration) -> ! {
        let budget = LogBudget::new(LOG_CAP_STRESS_LIMIT)
            .unwrap()
            .with_drain_bound(drain_bound);
        let start = Arc::new(std::sync::Barrier::new(LOG_CAP_STRESS_WRITERS));
        let writers: Vec<_> = (0..LOG_CAP_STRESS_WRITERS)
            .map(|writer| {
                let budget = budget.clone();
                let start = Arc::clone(&start);
                std::thread::spawn(move || {
                    let mut sink = CappedWriter::stderr(Some(budget));
                    start.wait();
                    for line in 0u64..=u64::MAX {
                        // One write per line, so each line reaches the pipe whole.
                        let _ = sink.write_all(format!("writer {writer} line {line}\n").as_bytes());
                    }
                })
            })
            .collect();
        for writer in writers {
            let _ = writer.join();
        }
        // Unreachable: the crossing write ends the process with exit 123.
        // SAFETY: _exit has no preconditions.
        unsafe { libc::_exit(1) }
    }

    #[test]
    fn capped_writer_counts_what_it_passes_through() {
        let mut sink = Vec::new();
        let budget = LogBudget::new(1 << 20).unwrap();
        let mut writer = CappedWriter::new(&mut sink, Some(budget.clone()));
        writer.write_all(b"hello ").unwrap();
        writer.write_all(b"world\n").unwrap();
        drop(writer);
        assert_eq!(sink, b"hello world\n");
        assert_eq!(budget.spent(), 12);

        // Uncapped: identical output, nothing counted, nothing to abort.
        let mut sink = Vec::new();
        let mut writer = CappedWriter::new(&mut sink, None);
        writer.write_all(&[b'x'; 4096]).unwrap();
        drop(writer);
        assert_eq!(sink.len(), 4096);
    }

    #[test]
    fn capped_writer_counts_bytes_the_file_bound_discards() {
        // The cap must still see a runaway after HERMIT_LOG_MAX_BYTES has
        // stopped the file growing; that is why it wraps BoundedWriter.
        let mut sink = Vec::new();
        let budget = LogBudget::new(1 << 20).unwrap();
        let mut writer = CappedWriter::new(BoundedWriter::new(&mut sink, 8), Some(budget.clone()));
        for _ in 0..10 {
            writer.write_all(&[b'z'; 100]).unwrap();
        }
        assert_eq!(budget.spent(), 1000);
    }

    /// End to end in a real process: the write that crosses the cap ends the
    /// process with HERMIT_LOG_CAP_EXIT, the bytes before it are delivered, the
    /// bytes of the crossing write are not, and the sink's last words name the
    /// bound. Also proves the count is shared with a forked child.
    #[test]
    fn crossing_the_cap_exits_with_the_log_cap_status_and_says_why() {
        use std::io::Read;
        use std::os::fd::FromRawFd;

        let budget = LogBudget::new(100).unwrap();
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        let (read_fd, write_fd) = (fds[0], fds[1]);
        // SAFETY: the child uses only preformatted data, atomics, write(2) and
        // _exit, so it is safe after fork in a multithreaded test process.
        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork failed: {}", io::Error::last_os_error());
        if child == 0 {
            unsafe {
                libc::close(read_fd);
                // Keep the test runner's stderr clean.
                let null = libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY);
                libc::dup2(null, 2);
                let mut writer = CappedWriter::file(
                    std::fs::File::from_raw_fd(write_fd),
                    0,
                    Some(budget.clone()),
                );
                let _ = writer.write_all(&[b'a'; 60]);
                let _ = writer.write_all(&[b'b'; 60]);
                libc::_exit(0);
            }
        }
        unsafe { libc::close(write_fd) };
        let mut output = Vec::new();
        unsafe { std::fs::File::from_raw_fd(read_fd) }
            .read_to_end(&mut output)
            .unwrap();
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        assert!(libc::WIFEXITED(status), "status {status:#x}");
        assert_eq!(libc::WEXITSTATUS(status), hermit::HERMIT_LOG_CAP_EXIT);

        let text = String::from_utf8(output).unwrap();
        let (delivered, message) = text.split_at(60);
        assert_eq!(delivered, "a".repeat(60));
        assert!(
            !message.contains("bbbb"),
            "the crossing write was not delivered"
        );
        assert!(
            message.starts_with("hermit: log output exceeded --max-log-bytes=100 (100 bytes)"),
            "{message}"
        );
        // The crossing process's own last words carry the class marker: under
        // `run --namespace-only` no outer hermit survives to print it.
        assert!(
            message.ends_with("to let the run finish.\nHERMIT_LOG_CAP class=log-cap\n"),
            "{message}"
        );
        assert_eq!(
            budget.spent(),
            120,
            "the child's charges are visible to the parent"
        );
    }

    /// Every prepared crossing message names its bound, ends with exactly one
    /// class line, and fits in one `PIPE_BUF` write, so the truncation in
    /// [`write_without_waiting`] can never cut the marker off.
    #[test]
    fn the_crossing_message_ends_with_the_class_line_at_every_limit() {
        for limit in [1, 100, 64 << 10, 8 << 30, u64::MAX] {
            let message = exceeded_message(limit);
            let named = format!(
                "hermit: log output exceeded --max-log-bytes={} ({limit} bytes);",
                format_byte_size(limit)
            );
            assert!(message.starts_with(&named), "{message}");
            assert!(message.ends_with(LOG_CAP_CLASS_LINE), "{message}");
            assert_eq!(message.matches("HERMIT_LOG_CAP").count(), 1, "{message}");
            assert!(message.len() < libc::PIPE_BUF, "{} bytes", message.len());
        }
    }

    /// The comparator recognizes a log stopped by the cap from its last line,
    /// so the real stop message must end a log in a form
    /// `detcore::logdiff::log_was_truncated` accepts -- at every limit, after
    /// an ordinary record (round-2 review finding 5 on
    /// <https://github.com/rrnewton/hermit/pull/3686>).
    #[test]
    fn a_log_ending_with_the_stop_message_reads_as_truncated() {
        let record = "2026-10-05T12:00:00.000000Z INFO detcore: DETLOG [syscall] finish syscall #1: \
                      write(1, 0x2000, 1) = Ok(1)\n";
        for limit in [1, 100, 64 << 10, 8 << 30, u64::MAX] {
            let log = format!("{record}{}", exceeded_message(limit));
            assert!(detcore::logdiff::log_was_truncated(&log), "{log}");
        }
        assert!(!detcore::logdiff::log_was_truncated(record));
    }

    /// Once the file bound has truncated the log, the cap must not append its
    /// stop message after the truncation marker: the marker is the final line
    /// of a truncated log, and the comparator accepts nothing else after it.
    /// Stderr still gets a best-effort attempt at the stop message; this test
    /// sends stderr to /dev/null and does not check it. (Round-2 review
    /// finding 5 on <https://github.com/rrnewton/hermit/pull/3686>.)
    #[test]
    fn the_stop_message_is_not_appended_after_the_truncation_marker() {
        use std::io::Read;
        use std::os::fd::FromRawFd;

        let budget = LogBudget::new(100).unwrap();
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        let (read_fd, write_fd) = (fds[0], fds[1]);
        // SAFETY: the child uses only preformatted data, atomics, write(2) and
        // _exit, so it is safe after fork in a multithreaded test process.
        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork failed: {}", io::Error::last_os_error());
        if child == 0 {
            unsafe {
                libc::close(read_fd);
                let null = libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY);
                libc::dup2(null, 2);
                // A 50-byte file bound under a 100-byte cap: the first write
                // truncates the file, the second crosses the cap.
                let mut writer = CappedWriter::file(
                    std::fs::File::from_raw_fd(write_fd),
                    50,
                    Some(budget.clone()),
                );
                let _ = writer.write_all(&[b'a'; 60]);
                let _ = writer.write_all(&[b'b'; 60]);
                libc::_exit(0);
            }
        }
        unsafe { libc::close(write_fd) };
        let mut output = Vec::new();
        unsafe { std::fs::File::from_raw_fd(read_fd) }
            .read_to_end(&mut output)
            .unwrap();
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        assert!(libc::WIFEXITED(status), "status {status:#x}");
        assert_eq!(libc::WEXITSTATUS(status), hermit::HERMIT_LOG_CAP_EXIT);

        let mut expected = vec![b'a'; 50];
        expected.extend_from_slice(TRUNCATION_MARKER);
        let text = String::from_utf8(output).unwrap();
        assert_eq!(text.as_bytes(), expected.as_slice(), "{text}");
        assert!(detcore::logdiff::log_was_truncated(&text), "{text}");
    }

    /// A pipe whose buffer is full and whose reader never reads, switched back
    /// to blocking mode: a plain `write(2)` to it waits forever.
    fn full_blocking_pipe() -> (libc::c_int, libc::c_int) {
        let mut fds = [0; 2];
        assert_eq!(
            unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) },
            0
        );
        for chunk in [&[b'x'; 4096][..], &[b'x'; 1][..]] {
            while unsafe { libc::write(fds[1], chunk.as_ptr().cast(), chunk.len()) } > 0 {}
            assert_eq!(
                io::Error::last_os_error().raw_os_error(),
                Some(libc::EAGAIN)
            );
        }
        assert_eq!(unsafe { libc::fcntl(fds[1], libc::F_SETFL, 0) }, 0);
        (fds[0], fds[1])
    }

    /// The crossing must end the process even when no sink can take the
    /// final message: here both the log sink (a FIFO-like pipe) and stderr
    /// are full blocking pipes whose readers stay open and never read. A
    /// blocking diagnostic write would hold `_exit`, and with it the guest
    /// teardown, indefinitely (review of
    /// https://github.com/rrnewton/hermit/pull/3686, round 1).
    #[test]
    fn crossing_the_cap_exits_even_when_no_sink_can_take_the_message() {
        use std::os::fd::FromRawFd;
        use std::time::Duration;
        use std::time::Instant;

        let budget = LogBudget::new(100).unwrap();
        let (sink_read, sink_write) = full_blocking_pipe();
        let (stderr_read, stderr_write) = full_blocking_pipe();
        // SAFETY: as in the test above -- the child uses preformatted data,
        // atomics, raw syscalls and _exit.
        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork failed: {}", io::Error::last_os_error());
        if child == 0 {
            unsafe {
                libc::dup2(stderr_write, 2);
                let mut writer = CappedWriter::file(
                    std::fs::File::from_raw_fd(sink_write),
                    0,
                    Some(budget.clone()),
                );
                // One write that crosses at once: nothing before it needs
                // room in the full sink.
                let _ = writer.write(&[b'c'; 120]);
                libc::_exit(0);
            }
        }
        unsafe {
            libc::close(sink_write);
            libc::close(stderr_write);
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut status = 0;
        let exited = loop {
            let reaped = unsafe { libc::waitpid(child, &mut status, libc::WNOHANG) };
            assert!(reaped >= 0, "waitpid: {}", io::Error::last_os_error());
            if reaped == child {
                break true;
            }
            if Instant::now() >= deadline {
                unsafe {
                    libc::kill(child, libc::SIGKILL);
                    libc::waitpid(child, &mut status, 0);
                }
                break false;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        unsafe {
            libc::close(sink_read);
            libc::close(stderr_read);
        }
        assert!(
            exited,
            "the crossing writer was still alive after 20 s: it waited on a full sink \
             instead of exiting"
        );
        assert!(libc::WIFEXITED(status), "status {status:#x}");
        assert_eq!(libc::WEXITSTATUS(status), hermit::HERMIT_LOG_CAP_EXIT);
        assert_eq!(budget.spent(), 120);
    }

    /// Run `body` in a forked child whose SIGPIPE has its default disposition
    /// and whose signal mask is empty -- the state Reverie leaves the tracer
    /// in -- and return the child's raw wait status.
    fn in_child_with_default_sigpipe(body: impl FnOnce()) -> libc::c_int {
        // SAFETY: the child runs raw syscalls and a closure that does the
        // same, then _exits.
        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork failed: {}", io::Error::last_os_error());
        if child == 0 {
            unsafe {
                libc::signal(libc::SIGPIPE, libc::SIG_DFL);
                let mut empty: libc::sigset_t = mem::zeroed();
                libc::sigemptyset(&mut empty);
                libc::pthread_sigmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
                body();
                libc::_exit(0);
            }
        }
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        status
    }

    fn assert_exited_cleanly(status: libc::c_int, what: &str) {
        assert!(
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
            "{what}: status {status:#x} (signal {})",
            if libc::WIFSIGNALED(status) {
                libc::WTERMSIG(status)
            } else {
                0
            }
        );
    }

    /// Round-3 review of https://github.com/rrnewton/hermit/pull/3686, finding
    /// 1: when the exit timer's thread cannot be created, nothing would bound
    /// the diagnostics that follow the classification, so the process must
    /// exit 123 at once rather than carry on unbounded.
    #[test]
    fn a_failed_exit_timer_start_exits_with_the_log_cap_status_at_once() {
        let status = in_child_with_default_sigpipe(|| {
            LOG_CAP_EXIT_SCHEDULED.store(false, Ordering::Relaxed);
            enable_log_cap_exit_bound();
            bound_log_cap_exit_with(|_timer| Err(io::Error::from_raw_os_error(libc::EAGAIN)));
        });
        assert!(libc::WIFEXITED(status), "status {status:#x}");
        assert_eq!(libc::WEXITSTATUS(status), hermit::HERMIT_LOG_CAP_EXIT);
    }

    /// Round-3 review of https://github.com/rrnewton/hermit/pull/3686,
    /// findings 3 and 4: a panic after the cap ended the run -- `eprintln!` to
    /// a stderr whose reader has gone, or an `analyze` trial's `expect` --
    /// must still exit 123, not the panic status 101.
    #[test]
    fn a_panic_after_the_cap_ended_the_run_still_exits_with_the_log_cap_status() {
        let status = in_child_with_default_sigpipe(|| {
            LOG_CAP_EXIT_SCHEDULED.store(false, Ordering::Relaxed);
            enable_log_cap_exit_bound();
            // A timer that never runs, so only the panic can end the child.
            bound_log_cap_exit_with(|_timer| Ok(()));
            let _ = std::panic::catch_unwind(|| panic!("a diagnostic after the cap"));
        });
        assert!(libc::WIFEXITED(status), "status {status:#x}");
        assert_eq!(libc::WEXITSTATUS(status), hermit::HERMIT_LOG_CAP_EXIT);
    }

    /// Round-3 review of https://github.com/rrnewton/hermit/pull/3686, finding
    /// 7: the outer report and the exit path elect the one diagnostic with the
    /// same swap, so it is attempted at most once. Here the report is called
    /// twice and then the exit path runs, as the exit timer's thread would; only
    /// the first report may reach stderr.
    #[test]
    fn the_outer_log_cap_diagnostic_is_attempted_at_most_once() {
        use std::io::Read;
        use std::os::fd::FromRawFd;

        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        let status = in_child_with_default_sigpipe(|| {
            unsafe { libc::dup2(fds[1], libc::STDERR_FILENO) };
            LOG_CAP_REPORTED.store(false, Ordering::Relaxed);
            report_log_cap_without_waiting("first report\n");
            report_log_cap_without_waiting("second report\n");
            exit_with_log_cap_status()
        });
        unsafe { libc::close(fds[1]) };
        let mut delivered = String::new();
        // SAFETY: fds[0] is this test's own read end, owned by the File below.
        unsafe { std::fs::File::from_raw_fd(fds[0]) }
            .read_to_string(&mut delivered)
            .unwrap();
        assert!(libc::WIFEXITED(status), "status {status:#x}");
        assert_eq!(libc::WEXITSTATUS(status), hermit::HERMIT_LOG_CAP_EXIT);
        assert_eq!(delivered, "first report\n");
    }

    /// Round-2 review of https://github.com/rrnewton/hermit/pull/3686, finding
    /// 4: a cap diagnostic to a pipe whose reader is gone, or to a socket whose
    /// peer is gone, must not raise SIGPIPE in a process that has SIGPIPE at
    /// its default disposition. Such a death turned exit 123 into 125.
    #[test]
    fn a_cap_diagnostic_to_a_departed_reader_raises_no_signal() {
        let mut pipe = [0; 2];
        assert_eq!(
            unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        unsafe { libc::close(pipe[0]) };
        let status = in_child_with_default_sigpipe(|| {
            write_without_waiting(pipe[1], b"hermit: diagnostic\n");
        });
        unsafe { libc::close(pipe[1]) };
        assert_exited_cleanly(status, "pipe without a reader");

        let mut pair = [0; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
                    0,
                    pair.as_mut_ptr(),
                )
            },
            0
        );
        unsafe { libc::close(pair[1]) };
        let status = in_child_with_default_sigpipe(|| {
            write_without_waiting(pair[0], b"hermit: diagnostic\n");
        });
        unsafe { libc::close(pair[0]) };
        assert_exited_cleanly(status, "socket without a peer");
    }

    /// The guard itself: a raw write that does raise SIGPIPE is survived, the
    /// signal it generated is consumed rather than left pending, and the
    /// signal mask is restored.
    #[test]
    fn the_diagnostic_signal_guard_consumes_only_the_signal_its_write_raised() {
        let mut pipe = [0; 2];
        assert_eq!(
            unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        unsafe { libc::close(pipe[0]) };
        let status = in_child_with_default_sigpipe(|| unsafe {
            suppressing_diagnostic_signals(|| {
                if libc::write(pipe[1], b"x".as_ptr().cast(), 1) < 0 {
                    last_errno()
                } else {
                    0
                }
            });
            let mut pending: libc::sigset_t = mem::zeroed();
            libc::sigpending(&mut pending);
            let mut mask: libc::sigset_t = mem::zeroed();
            libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), &mut mask);
            if libc::sigismember(&pending, libc::SIGPIPE) == 1
                || libc::sigismember(&mask, libc::SIGPIPE) == 1
                || libc::sigismember(&mask, libc::SIGTTOU) == 1
            {
                libc::_exit(3);
            }
        });
        unsafe { libc::close(pipe[1]) };
        assert_exited_cleanly(status, "raw write to a pipe without a reader");
    }

    #[test]
    fn the_proc_fd_path_names_the_descriptor() {
        for (fd, expected) in [
            (0, "/proc/self/fd/0"),
            (2, "/proc/self/fd/2"),
            (1234, "/proc/self/fd/1234"),
            (i32::MAX, "/proc/self/fd/2147483647"),
        ] {
            let path = proc_self_fd_path(fd);
            let end = path.iter().position(|&byte| byte == 0).unwrap();
            assert_eq!(std::str::from_utf8(&path[..end]).unwrap(), expected);
        }
    }
}
