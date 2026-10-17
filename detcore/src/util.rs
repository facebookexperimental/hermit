/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Widely useful small utilities.

use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::types::NANOS_PER_RCB;

/// Issues syscall `number` with `args` through the syscall instruction itself
/// on x86-64, returning the kernel's result (a negative errno on failure).
/// Under in-guest LiteInst Detcore runs inside the guest, where libc's
/// wrappers (`open`, `fstat`, `syscall`, ...) are dynamic symbols the program
/// or a preloaded library may define, and allocate in.
///
/// # Safety
///
/// As for the syscall itself: pointer arguments must be valid for it.
pub unsafe fn raw_syscall(number: libc::c_long, args: [u64; 6]) -> i64 {
    #[cfg(target_arch = "x86_64")]
    {
        let result: i64;
        // SAFETY: the caller vouches for the arguments.
        unsafe {
            core::arch::asm!(
                "syscall",
                inlateout("rax") number => result,
                in("rdi") args[0],
                in("rsi") args[1],
                in("rdx") args[2],
                in("r10") args[3],
                in("r8") args[4],
                in("r9") args[5],
                lateout("rcx") _,
                lateout("r11") _,
                options(nostack),
            );
        }
        result
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        // SAFETY: the caller vouches for the arguments.
        let result =
            unsafe { libc::syscall(number, args[0], args[1], args[2], args[3], args[4], args[5]) };
        if result < 0 {
            -i64::from(
                std::io::Error::last_os_error()
                    .raw_os_error()
                    .unwrap_or(libc::EIO),
            )
        } else {
            result
        }
    }
}

/// [`raw_syscall`]'s result as an `io::Result`.
pub fn raw_result(result: i64) -> std::io::Result<u64> {
    if (-4095..0).contains(&result) {
        Err(std::io::Error::from_raw_os_error(-result as i32))
    } else {
        Ok(result as u64)
    }
}

/// [`raw_syscall`]'s result for a syscall whose only success value is 0
/// (`fstat`, `pipe2`, `socketpair`, ...): any other value is refused, a
/// negative one as the error it names. [`raw_result`] would take a positive
/// return as success, which for such a call the kernel never gives.
pub fn raw_zero_result(result: i64) -> std::io::Result<()> {
    match raw_result(result)? {
        0 => Ok(()),
        other => Err(std::io::Error::other(format!(
            "a syscall that returns 0 on success returned {other}"
        ))),
    }
}

/// Describes `error` without asking the C library for an errno message.
///
/// `std::io::Error`'s `Display` and `Debug` render an operating-system error
/// through `strerror_r`, which can call `gettext` and allocate through the C
/// library's malloc: under in-guest LiteInst, where Detcore runs inside the
/// guest, the guest's own heap. An operating-system error is described by its
/// kind and number instead; any other error, or an I/O error wrapping one, as
/// before.
pub fn describe_io_error(error: &std::io::Error) -> String {
    if let Some(code) = error.raw_os_error() {
        return format!("{:?} (errno {code})", error.kind());
    }
    match error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<std::io::Error>())
    {
        Some(inner) => describe_io_error(inner),
        None => error.to_string(),
    }
}

#[allow(dead_code)]
/// A simple debugging helper function that makes it easy to printf-debug through
/// layers of stdout/stderr caputure, such as when running under buck test/tpx.
pub fn punch_out_print(msg: &str) {
    use std::io::Write;
    // TODO: if we want this to be more performant, we can have a lazy static
    // global file handle for this. This, however, keeps it simple for occasional usage.œ
    if let Ok(mut tty) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
    {
        writeln!(tty, "{}", msg).unwrap();
    } else {
        // If devtty doesn't exist, we just use stderr.
        eprintln!("{}", msg);
    }
}
/// A helper function to convert a number of Retired Conditional Branches (RCBS) into
/// a `std::time::Duration` via the `NANOS_PER_RCB` defined in ` types.rs`.
pub fn rcbs_to_duration(rcbs: u64) -> Duration {
    Duration::from_nanos((rcbs as f64 * NANOS_PER_RCB) as u64)
}

/// A little better than the builtin string truncation in format strings, because it includes ellipses.
// TODO: There should be some advanced solution for printing potentially huge things that
// doesn't actually render them all...
pub fn truncated(width: usize, mut s: String) -> String {
    if s.len() > width {
        if width >= 3 {
            s.truncate(width - 3);
            s.push_str("...");
            s
        } else {
            s.truncate(width);
            s
        }
    } else {
        s
    }
}

/// A `Write` for the supervisor's own diagnostics on fd 2 that survives a
/// NONBLOCKING description.
///
/// ⚠️ THE GUEST CAN SET `O_NONBLOCK` ON HERMIT'S STDERR AND SILENTLY CUT THE
/// SUPERVISOR'S ERROR CHANNEL. fd 2 is an INHERITED open file description shared
/// with the guest, so `fcntl(2, F_SETFL, O_NONBLOCK)` in the guest changes the
/// behaviour of hermit's OWN later writes. Measured 2026-08-26 on a 4096-byte
/// pipe under back-pressure, `hermit --log info run -- <guest>`:
///
/// ```text
///   control guest (touches nothing)   rc=0    138 lines, ends on the run summary
///   guest sets O_NONBLOCK on fd 2     rc=101  134 lines, ends mid-log
/// ```
///
/// The summary was emitted by `eprint!`, which calls `write_all`; `write_all`
/// does NOT retry `EAGAIN`, so it returns an error and the print macro PANICS.
/// The panic message then went to the same full pipe and was lost as well, so
/// the delivered output contained no panic text, no error, and no marker of any
/// kind -- it simply stopped on a plausible-looking line. Exit 101 was the only
/// surviving evidence, and a caller that reads output rather than status sees a
/// short report, not a truncated one.
///
/// ⚠️ WAIT, DO NOT SPIN, AND DO NOT CLEAR THE FLAG. Clearing `O_NONBLOCK` would
/// change what the guest observes on a descriptor it legitimately shares; this
/// leaves the flag exactly as the guest set it and simply waits for the pipe to
/// drain, which is what a blocking write would have done. A bare retry loop
/// would busy-spin against a full pipe, so it blocks in `poll(POLLOUT)`.
pub struct RetryingStderr;

/// Total wall-clock ALL diagnostic writes in ONE PROCESS may spend waiting for a
/// reader that is not draining. Hermit forks, so read the fork note below before
/// treating this as the whole exit path's budget.
///
/// ⚠️ WITHOUT A CEILING THIS LOOP NEVER ENDS, AND IT RUNS WHILE HERMIT IS ALREADY
/// FAILING. `EAGAIN` -> `poll(POLLOUT, 1s)` -> retry is correct for a reader that
/// is SLOW and unbounded for a reader that is STOPPED. Measured 2026-08-26 on a
/// 4096-byte pipe filled to 3900 with `O_NONBLOCK` set, where nothing ever reads:
///
/// ```text
///   before RetryingStderr (eprintln!)   EXITED rc=101 immediately
///   RetryingStderr, no ceiling          STILL RUNNING after 25s, no exit
/// ```
///
/// The first is wrong loudly; the second does not return at all, on the path that
/// reports why hermit is stopping. A supervisor that hangs while reporting an
/// error is harder to diagnose than one that dies reporting it badly.
///
/// ⚠️ THIS IS A TOTAL FOR THE WHOLE PROCESS, NOT A BUDGET PER `write()` CALL, AND
/// THE DIFFERENCE IS THE ENTIRE GUARANTEE. An earlier version started the clock
/// inside `fn write`, so every call got the full allowance. Nothing writes to
/// stderr exactly once on the exit path:
///
/// ```text
///   hermit-cli/src/bin/hermit/main.rs      the failure class, the head of the
///                                          error chain, then ONE PER CAUSE in
///                                          `for cause in chain` -- 2 + chain length
///   hermit-cli/src/bin/hermit/tracing.rs   .with_writer(|| RetryingStderr) is the
///                                          writer for the WHOLE subscriber: one
///                                          write per log event
///   detcore/src/tool_global.rs             a multi-part `write!`, which `write_fmt`
///                                          splits into several `write` calls
/// ```
///
/// With a per-call budget the process-level cost was N times this number, N is
/// bounded by the error chain rather than by anything here, and every caller's
/// `let _ =` swallowed the overrun silently. A `--log info` run measured at 138
/// lines would have been minutes, not seconds.
///
/// ⚠️ THE VALUE IS DERIVED FROM THE BOUND THAT ENCLOSES IT, NOT CHOSEN. Diagnostics
/// on the exit path run inside `RUN_TIMEOUT_UNWIND_GRACE` (10s,
/// `hermit-cli/src/lib.rs`): the window between a `--timeout` expiring and the
/// SIGALRM fallback calling `_exit`. Overrun there does not merely delay the
/// report, it loses it -- the fallback fires mid-sentence and additionally emits
/// `HERMIT_RUN_TIMEOUT_FALLBACK`, which is supposed to mean the teardown wedged.
/// So the arithmetic is:
///
/// ⚠️ IT USED TO APPLY ONCE PER PROCESS, BECAUSE HERMIT FORKS. `Container::run`
/// forks the container init, which got its own COPY of the process-wide clock and
/// spent its own full deadline; both write diagnostics to the same fd 2 on the way
/// out. Measured 2026-08-26 against a stopped reader on a real guest, which is the
/// case where both processes report:
///
/// ```text
///   deadline 5000ms   ->  10.03s observed   two processes, one deadline each
///   deadline 2500ms   ->   5.02s observed   the same two, halved
///   deadline 2500ms   ->   2.52s observed   missing guest: only ONE process reports
/// ```
///
/// ⚠️ AND THE MULTIPLIER WAS NOT ALWAYS TWO, WHICH IS WHY IT IS NO LONGER A
/// CONSTANT. `hermit run --verify` runs the guest twice and forks once per run, so
/// it is the outer hermit plus TWO container inits -- three writers, and siblings
/// rather than a chain. From the call structure, so it needs no sampling:
///
/// ```text
///   run.rs  fn verify()      Run1 -> self.run_verify(..)   Run2 -> self.run_verify(..)
///           fn run_verify()  -> with_container(..) forks a container init
/// ```
///
/// At three, `3 x 2500ms x 2 = 15s` against a 10s grace: the stated split does not
/// hold. Correcting the constant to three would force the deadline to ~1666ms and
/// leave a hardcoded count for the next differently-forking path to falsify.
///
/// So the ORIGIN is shared across the invocation instead (see
/// `STDERR_ORIGIN_ADDR`): every hermit process measures from the same start, the
/// invocation spends ONE deadline however many processes write, and the count
/// stops being an input. The arithmetic no longer contains it:
///
/// ```text
///   RUN_TIMEOUT_UNWIND_GRACE                    10s     the enclosing bound
///   diagnostics may have half of it              5s     leaving 5s for the unwind
///   this deadline, per INVOCATION             2500ms    comfortably inside the 5s
/// ```
///
/// Half is a split, not a measurement, and is stated as one. What is ALSO measured is
/// that the previous arrangement could not fit: at 5s per CALL, the same real-guest
/// fixture took 15.04s -- three blocked writes -- against a 10s grace, so the inner
/// bound exceeded its outer bound before any unwind work happened at all.
/// `hermit-cli` asserts this relationship in a test, so raising either side without
/// the other fails by name.
///
/// On expiry the write returns `WouldBlock`, `writeln!` gives up, the caller's
/// `let _ =` drops that line, and hermit exits — the pre-existing contract for a
/// diagnostic that cannot be delivered.
pub const STDERR_DIAGNOSTIC_DEADLINE: std::time::Duration = std::time::Duration::from_millis(2500);

/// When this process first found stderr unwritable, shared by every
/// `RetryingStderr`, which is what makes the deadline above a process total.
/// Set on the first `EAGAIN` and never reset: a run that recovers and later
/// blocks again has still spent that earlier time on its exit path.
///
/// Fallback only: used when the invocation-wide origin below is unavailable.
static STDERR_BLOCKED_SINCE: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// The same instant, shared by every hermit process of ONE INVOCATION.
///
/// ⚠️ WITHOUT THIS THE DEADLINE IS MULTIPLIED BY THE NUMBER OF PROCESSES, AND
/// THAT NUMBER IS NOT 2. A `static` lives per process, so each hermit sets its
/// own origin and spends its own full deadline. The comment above states the
/// multiplier as "2 -- hermit + the forked init", which is right for `hermit
/// run` and wrong for `hermit run --verify`. From the call structure, not from
/// sampling a process table:
///
/// ```text
///   hermit-cli/src/bin/hermit/run.rs
///     fn verify()                      -> Run1: self.run_verify(log1_file, global)
///                                      -> Run2: self.run_verify(log2_file, global)
///     fn run_verify()                  -> with_container(..) forks a container init
/// ```
///
/// So `--verify` is the outer hermit plus TWO container inits, which share the
/// outer as their parent. Three writing processes, and siblings rather than a
/// chain. `record --verify` forks twice for the same reason, at
/// `record_verify.record` and `record_verify.replay` in record_start.rs.
///
/// At three the stated derivation does not hold: 3 x 2500ms x 2 = 15s against a
/// 10s grace. Correcting the constant to 3 would force the deadline to ~1666ms
/// and would leave a hardcoded count that the next differently-forking path
/// falsifies again. Sharing the ORIGIN makes the multiplier 1 by construction --
/// every process measures from the same start, so the invocation spends one
/// deadline however many processes write -- and the count stops being an input.
///
/// ⚠️ THE CARRIER IS AN INHERITED SHARED MAPPING, DELIBERATELY NOT AN ENVIRONMENT
/// VARIABLE AND NOT A FILE. A variable set on hermit reaches the GUEST by default
/// (`BaseEnv::Host` does not clear it), and a per-run-varying value visible to the
/// guest, in a determinism tool, on a surface the argv/env hashing covers, would
/// be a worse defect than the one being fixed. A file would put I/O on the path
/// that runs while hermit is already failing. `exec` drops the mapping, so the
/// guest never shares it.
///
/// `CLOCK_MONOTONIC` rather than `Instant`: an `Instant` is not meaningful in
/// another process, while `CLOCK_MONOTONIC` is system-wide on Linux, so the same
/// nanosecond count is directly comparable between parent and child. 0 means
/// unset; the first blocked write in ANY process installs the origin.
static STDERR_ORIGIN_ADDR: OnceLock<usize> = OnceLock::new();

/// Create the origin cell every hermit process of this invocation shares.
///
/// ⚠️ MUST BE CALLED BEFORE THE FIRST FORK, and is called from the top of `main`.
/// A mapping made after a fork is not shared with the child that already exists,
/// so a late call would silently give each process its own origin -- the exact
/// failure this removes. `main` dominates every `with_container` and
/// `run_guarded_at` site in run.rs, replay.rs and record_start.rs.
pub fn init_shared_stderr_deadline_origin() {
    STDERR_ORIGIN_ADDR.get_or_init(|| {
        // SAFETY: an anonymous shared mapping of one page, no fd, no fixed
        // address. Never unmapped: it must outlive every child, and one page per
        // invocation does not justify a teardown path on an exit route.
        let addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                std::mem::size_of::<AtomicU64>(),
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if addr == libc::MAP_FAILED {
            // Not fatal and NOT silent: `stderr_deadline_is_shared` reports false
            // and the per-process fallback still bounds each process.
            return 0;
        }
        // SAFETY: freshly mapped, aligned for u64, sole owner at this point.
        unsafe { (addr as *mut AtomicU64).write(AtomicU64::new(0)) };
        addr as usize
    });
}

fn shared_origin_cell() -> Option<&'static AtomicU64> {
    match STDERR_ORIGIN_ADDR.get() {
        None | Some(0) => None,
        // SAFETY: the address came from `mmap` above, was initialised there, is
        // never unmapped, and `AtomicU64` is safe to share across processes in a
        // `MAP_SHARED` page.
        Some(&addr) => Some(unsafe { &*(addr as *const AtomicU64) }),
    }
}

fn monotonic_nanos() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: one initialised `timespec`, a clock id the kernel always supports.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
}

/// Whether the deadline is bounded per INVOCATION or only per process.
///
/// ⚠️ EXISTS SO THE WEAKER STATE CANNOT BE SILENT. If the mapping failed, or
/// something forks before `init_shared_stderr_deadline_origin`, the bound
/// degrades to per-process and is multiplied by however many processes write.
#[doc(hidden)]
pub fn stderr_deadline_is_shared() -> bool {
    shared_origin_cell().is_some()
}

/// How long this invocation has been blocked on stderr, from the shared origin
/// when there is one and from this process's own first block otherwise.
fn stderr_blocked_for() -> std::time::Duration {
    match shared_origin_cell() {
        Some(cell) => {
            let now = monotonic_nanos();
            // The first blocked write in ANY process installs the origin; every
            // later one, in any process, reads the value that won.
            let origin = match cell.compare_exchange(0, now, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => now,
                Err(existing) => existing,
            };
            std::time::Duration::from_nanos(now.saturating_sub(origin))
        }
        None => STDERR_BLOCKED_SINCE
            .get_or_init(std::time::Instant::now)
            .elapsed(),
    }
}

/// Reset the shared origin. Test-only: brackets that measure the deadline need
/// each case to start from zero, and nothing in a real run wants this.
#[doc(hidden)]
pub fn reset_stderr_deadline_origin_for_test() {
    if let Some(cell) = shared_origin_cell() {
        cell.store(0, Ordering::Release);
    }
}

impl std::io::Write for RetryingStderr {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            // ⚠️ UNDER --max-log-bytes A BLOCKING DESCRIPTOR MUST NOT WAIT INSIDE
            // write(2). The bounded wait below starts only at `EAGAIN`, which a
            // blocking pipe never returns: its write simply waits for a reader,
            // forever when the reader has stopped, and the run never reaches the
            // cap. While `--max-log-bytes` is in force (review of
            // https://github.com/rrnewton/hermit/pull/3686) each attempt is one
            // that cannot wait for a reader, so a full pipe answers `EAGAIN` and
            // takes the same bounded path as a guest-set `O_NONBLOCK`. Without the
            // cap this is the plain `write(2)` it always was.
            let must_not_wait = crate::nonwaiting_write::diagnostics_must_not_wait();
            let attempt = if must_not_wait {
                crate::nonwaiting_write::write_without_waiting_for_a_reader(
                    libc::STDERR_FILENO,
                    buf,
                )
            } else {
                // SAFETY: `write(2)` on fd 2 reads `buf.len()` bytes from `buf`,
                // which is valid for that length, and writes no memory.
                let n = unsafe { libc::write(libc::STDERR_FILENO, buf.as_ptr().cast(), buf.len()) };
                if n >= 0 {
                    Ok(n as usize)
                } else {
                    Err(std::io::Error::last_os_error())
                }
            };
            let err = match attempt {
                Ok(n) => return Ok(n),
                Err(err) => err,
            };
            match err.kind() {
                // ⚠️ UNDER --max-log-bytes AN INTERRUPTED ATTEMPT IS RETRIED ONLY
                // WITHIN THE SAME DEADLINE, AND GIVEN UP AS `TimedOut`. A seccomp
                // policy can answer any call in the attempt with `EINTR` every
                // time: answering `statx` and the file-type probes on fd 2 that
                // way leaves the writer no type, and it returns `statx`'s
                // `EINTR`. An unbounded retry then never returned to
                // `DroppingStderr`, so the log was never charged and the cap
                // never crossed. `Interrupted` itself must not escape either:
                // `write_all`, behind `writeln!`, retries it forever. Without the
                // cap an interrupted write is retried as it always was.
                std::io::ErrorKind::Interrupted if must_not_wait => {
                    if STDERR_DIAGNOSTIC_DEADLINE
                        .checked_sub(stderr_blocked_for())
                        .is_none()
                    {
                        return Err(std::io::ErrorKind::TimedOut.into());
                    }
                    continue;
                }
                std::io::ErrorKind::Interrupted => continue,
                std::io::ErrorKind::WouldBlock => {
                    // ⚠️ BOUNDED, AND THE CLOCK IS PROCESS-WIDE. Waiting for a slow
                    // reader is the point; waiting for a stopped one is a hang on
                    // hermit's exit path. Starting the clock here rather than at the
                    // top of `write` is what makes the deadline a total: the first
                    // blocked write sets it and every later one inherits it, so N
                    // writes cost the deadline once instead of N times.
                    // Measured from the origin shared by every hermit process
                    // of this invocation when there is one, so N processes spend
                    // ONE deadline rather than N. See STDERR_ORIGIN_ADDR.
                    let spent = stderr_blocked_for();
                    let Some(remaining) = STDERR_DIAGNOSTIC_DEADLINE.checked_sub(spent) else {
                        return Err(err);
                    };
                    // Block until the reader makes room. A failed or timed-out
                    // poll falls through to another write attempt rather than
                    // dropping the bytes.
                    let mut pfd = libc::pollfd {
                        fd: libc::STDERR_FILENO,
                        events: libc::POLLOUT,
                        revents: 0,
                    };
                    // ⚠️ CLAMPED TO WHAT IS LEFT, NOT A FLAT SECOND. The check above
                    // happens BEFORE the poll, so a flat 1000ms let a call that
                    // started at 4.9s sleep a further second and overshoot to ~6s --
                    // the deadline documented one number and delivered another.
                    // Clamping makes the elapsed total the stated total.
                    let timeout_ms = i32::try_from(remaining.as_millis())
                        .unwrap_or(i32::MAX)
                        .min(1000);
                    // SAFETY: one initialised `pollfd`, count 1, timeout in ms.
                    unsafe {
                        libc::poll(&mut pfd, 1, timeout_ms);
                    }
                    continue;
                }
                _ => return Err(err),
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// [`RetryingStderr`] for output whose loss must not change how a capped run
/// ends: hermit's own log when it goes to stderr, and the stack traces that
/// `--stacktrace-event` and `--preemption-stacktrace` print there.
///
/// While `--max-log-bytes` is in force
/// ([`crate::nonwaiting_write::diagnostics_must_not_wait`]), bytes that
/// [`RetryingStderr`] gives up on -- its bounded deadline for a reader that is
/// not draining is spent, or the reader is gone -- are reported as written.
/// So a log line that stderr could not take still counts against the cap,
/// as `BoundedWriter` counts the bytes it truncates, and a run whose stderr
/// reader stopped still reaches the cap and exits 123 instead of running on
/// uncounted; and a caller that unwraps the write does not panic.
/// Without the cap this is exactly [`RetryingStderr`], errors included.
pub struct DroppingStderr;

impl std::io::Write for DroppingStderr {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match RetryingStderr.write(buf) {
            Err(_) if crate::nonwaiting_write::diagnostics_must_not_wait() => Ok(buf.len()),
            result => result,
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `eprintln!` for detcore output that a capped run can print before the cap
/// crosses: under `--max-log-bytes` the line goes through [`DroppingStderr`]
/// and never waits beyond [`RetryingStderr`]'s deadline; otherwise this is the
/// `eprintln!` it replaces, with the same arguments, so uncapped output is
/// unchanged.
macro_rules! capped_eprintln {
    ($($arg:tt)*) => {
        if crate::nonwaiting_write::diagnostics_must_not_wait() {
            // One buffer, so the line is not split across several writes.
            let mut line = format!($($arg)*);
            line.push('\n');
            let _ = std::io::Write::write_all(&mut crate::util::DroppingStderr, line.as_bytes());
        } else {
            eprintln!($($arg)*);
        }
    };
}
pub(crate) use capped_eprintln;

#[cfg(test)]
mod shared_stderr_origin_tests {
    use super::*;

    /// ⚠️ THESE TESTS SHARE ONE PROCESS-GLOBAL CELL, SO THEY MUST NOT RUN AT THE
    /// SAME TIME. That is the point of the cell -- it is shared -- but it makes the
    /// tests interfere: one resets the origin while another is timing from it.
    /// Found the honest way: they passed under `--test-threads 1` and failed in the
    /// real suite, so the single-threaded run was not evidence about the suite.
    static SERIALISE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn exclusive() -> std::sync::MutexGuard<'static, ()> {
        SERIALISE.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The origin must actually be SHARED across a fork, or nothing else holds.
    ///
    /// ⚠️ THIS IS THE ONE THING THAT CANNOT BE CHECKED BY READING THE CODE.
    /// `MAP_SHARED` versus `MAP_PRIVATE` is a single token; get it wrong and every
    /// single-process test still passes while each container init silently gets its
    /// own deadline again -- the exact defect this removes. So this forks for real
    /// and checks that a value written by the child is visible in the parent.
    #[test]
    fn the_origin_is_shared_across_a_fork() {
        let _exclusive = exclusive();
        init_shared_stderr_deadline_origin();
        let cell = shared_origin_cell().expect("mapping must exist after init");
        cell.store(0, Ordering::Release);

        // SAFETY: the child does no allocation and no locking -- one atomic store
        // and `_exit` -- so forking from a test harness thread is safe here.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            shared_origin_cell()
                .expect("child inherited no mapping")
                .store(4242, Ordering::Release);
            unsafe { libc::_exit(0) };
        }
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(pid, &mut status, 0) },
            pid,
            "waitpid"
        );
        assert_eq!(status, 0, "child exited nonzero");

        assert_eq!(
            cell.load(Ordering::Acquire),
            4242,
            "the parent cannot see the child's write, so each container init would \
             start its OWN deadline and the invocation would spend one per process -- \
             check MAP_SHARED in init_shared_stderr_deadline_origin"
        );
        cell.store(0, Ordering::Release);
    }

    /// The first blocked write installs the origin; later ones inherit it.
    ///
    /// This is what makes the deadline a TOTAL rather than a fresh allowance per
    /// caller, and it must hold across processes, not just across calls.
    #[test]
    fn the_first_writer_installs_the_origin_and_others_inherit_it() {
        let _exclusive = exclusive();
        init_shared_stderr_deadline_origin();
        reset_stderr_deadline_origin_for_test();
        let cell = shared_origin_cell().expect("mapping must exist after init");

        let first = stderr_blocked_for();
        let installed = cell.load(Ordering::Acquire);
        assert_ne!(installed, 0, "the first call must install a nonzero origin");
        // A fresh origin means almost no time has been spent yet.
        assert!(
            first < Duration::from_millis(50),
            "first call saw {first:?}"
        );

        std::thread::sleep(Duration::from_millis(20));
        let second = stderr_blocked_for();
        assert_eq!(
            cell.load(Ordering::Acquire),
            installed,
            "a later call moved the origin, so each caller would get a fresh \
             allowance and the deadline would stop being a total"
        );
        assert!(
            second >= Duration::from_millis(15),
            "the second call reported {second:?}, so it is not measuring from the \
             origin the first call installed"
        );
        reset_stderr_deadline_origin_for_test();
    }

    /// Which bound is in force must be answerable, not assumed.
    #[test]
    fn whether_the_deadline_is_invocation_wide_is_observable() {
        let _exclusive = exclusive();
        init_shared_stderr_deadline_origin();
        // ⚠️ THE WEAKER STATE MUST NOT BE SILENT. If the mapping failed, or something
        // forks before the init call, the deadline degrades to per-process and is
        // multiplied by however many processes write.
        assert!(
            stderr_deadline_is_shared(),
            "the origin is not shared after init, so the deadline is per-process and \
             is multiplied by the number of writing processes"
        );
    }
}

/// A descriptor opened with a raw syscall, closed with one when dropped.
pub struct RawFd(u64);

impl RawFd {
    /// `path` opened with `flags` (`O_CLOEXEC` added) by a raw `openat`, never
    /// through libc's `open` wrapper (see `raw_syscall`).
    pub fn open(path: &std::path::Path, flags: libc::c_int) -> std::io::Result<Self> {
        let path = raw_path(path)?;
        // SAFETY: a NUL-terminated path.
        let fd = raw_result(unsafe {
            raw_syscall(
                libc::SYS_openat,
                [
                    libc::AT_FDCWD as u64,
                    path.as_ptr() as u64,
                    (flags | libc::O_CLOEXEC) as u64,
                    0,
                    0,
                    0,
                ],
            )
        })?;
        Ok(Self(fd))
    }

    /// The descriptor's number.
    pub fn raw(&self) -> u64 {
        self.0
    }

    /// The open file's status, by a raw `fstat`.
    pub fn stat(&self) -> std::io::Result<libc::stat> {
        // SAFETY: an all-zero `stat` is a valid value, and the kernel writes
        // at most its size.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        raw_zero_result(unsafe {
            raw_syscall(
                libc::SYS_fstat,
                [self.0, &mut stat as *mut libc::stat as u64, 0, 0, 0, 0],
            )
        })?;
        Ok(stat)
    }

    /// Up to `buffer.len()` bytes at `offset`, by a raw `pread64`.
    pub fn read_at(&self, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
        // SAFETY: the buffer is writable for its length.
        raw_result(unsafe {
            raw_syscall(
                libc::SYS_pread64,
                [
                    self.0,
                    buffer.as_mut_ptr() as u64,
                    buffer.len() as u64,
                    offset,
                    0,
                    0,
                ],
            )
        })
        .map(|read| read as usize)
    }
}

impl Drop for RawFd {
    fn drop(&mut self) {
        // SAFETY: a descriptor this value opened and owns.
        unsafe { raw_syscall(libc::SYS_close, [self.0, 0, 0, 0, 0, 0]) };
    }
}

fn raw_path(path: &std::path::Path) -> std::io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))
}

/// The status of the file at `path`, following links, by a raw `newfstatat`.
pub fn raw_stat(path: &std::path::Path) -> std::io::Result<libc::stat> {
    let path = raw_path(path)?;
    // SAFETY: a NUL-terminated path; an all-zero `stat` is a valid value.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    raw_zero_result(unsafe {
        raw_syscall(
            libc::SYS_newfstatat,
            [
                libc::AT_FDCWD as u64,
                path.as_ptr() as u64,
                &mut stat as *mut libc::stat as u64,
                0,
                0,
                0,
            ],
        )
    })?;
    Ok(stat)
}

/// The target of the link at `path`, by a raw `readlinkat`.
pub fn raw_read_link(path: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let path = raw_path(path)?;
    let mut target = vec![0_u8; libc::PATH_MAX as usize];
    // SAFETY: a NUL-terminated path and a buffer writable for its length.
    let read = raw_result(unsafe {
        raw_syscall(
            libc::SYS_readlinkat,
            [
                libc::AT_FDCWD as u64,
                path.as_ptr() as u64,
                target.as_mut_ptr() as u64,
                target.len() as u64,
                0,
                0,
            ],
        )
    })? as usize;
    target.truncate(read);
    Ok(std::path::PathBuf::from(std::ffi::OsStr::from_bytes(
        &target,
    )))
}

/// The whole contents of the file at `path`, read with raw syscalls. Meant
/// for small files such as procfs entries.
pub fn raw_read_file(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    let file = RawFd::open(path, libc::O_RDONLY)?;
    let mut contents = Vec::new();
    let mut chunk = vec![0_u8; 4096];
    loop {
        // SAFETY: the buffer is writable for its length.
        let read = raw_result(unsafe {
            raw_syscall(
                libc::SYS_read,
                [
                    file.raw(),
                    chunk.as_mut_ptr() as u64,
                    chunk.len() as u64,
                    0,
                    0,
                    0,
                ],
            )
        })? as usize;
        if read == 0 {
            return Ok(contents);
        }
        contents.extend_from_slice(&chunk[..read]);
    }
}

/// Calls `visit` with each name in directory `path`, without `.` and `..`,
/// in the kernel's order, until it returns `Some`, and returns that value
/// (`None` when every name was visited).
///
/// Read with raw `getdents64` into one fixed buffer from Rust's allocator,
/// never with `std::fs::read_dir`: that calls glibc's `opendir`, whose
/// directory buffer comes from the C library's `malloc`. Under in-guest
/// LiteInst, Detcore runs inside the guest and that heap is the guest's: the
/// freed buffer would keep the kernel's bytes (raw `/proc` inode numbers, say)
/// where the guest's next allocation finds them, so the guest could observe
/// host-chosen data, for example in the padding of its own `getdents64`
/// records. Rust's allocator is private to the runtime there (inside a Tool
/// callback or an allocation scope). The names are streamed, not collected,
/// so a directory of any size costs the one buffer.
pub fn find_in_directory<T>(
    path: &std::path::Path,
    mut visit: impl FnMut(&std::ffi::OsStr) -> Option<T>,
) -> std::io::Result<Option<T>> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    // Raw syscalls throughout (see raw_syscall). SAFETY: a NUL-terminated
    // path; the descriptor is closed below on every path, a panicking visitor
    // included.
    let fd = raw_result(unsafe {
        raw_syscall(
            libc::SYS_openat,
            [
                libc::AT_FDCWD as u64,
                path.as_ptr() as u64,
                (libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) as u64,
                0,
                0,
                0,
            ],
        )
    })?;
    struct CloseOnDrop(u64);
    impl Drop for CloseOnDrop {
        fn drop(&mut self) {
            // SAFETY: a descriptor find_in_directory opened and owns.
            unsafe { raw_syscall(libc::SYS_close, [self.0, 0, 0, 0, 0, 0]) };
        }
    }
    let directory = CloseOnDrop(fd);
    let mut buffer = vec![0_u8; 32 * 1024];
    let result = 'listing: loop {
        // SAFETY: the buffer is writable for its length.
        let read = unsafe {
            raw_syscall(
                libc::SYS_getdents64,
                [fd, buffer.as_mut_ptr() as u64, buffer.len() as u64, 0, 0, 0],
            )
        };
        if read < 0 {
            break Err(std::io::Error::from_raw_os_error(-read as i32));
        }
        if read == 0 {
            break Ok(None);
        }
        let mut offset = 0_usize;
        let read = read as usize;
        // A record: d_ino (8), d_off (8), d_reclen (2), d_type (1), d_name.
        while offset + 19 <= read {
            let reclen = u16::from_ne_bytes([buffer[offset + 16], buffer[offset + 17]]) as usize;
            if reclen < 19 || offset + reclen > read {
                break;
            }
            let name = &buffer[offset + 19..offset + reclen];
            let name = &name[..name
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(name.len())];
            if name != b"."
                && name != b".."
                && let Some(found) = visit(std::ffi::OsStr::from_bytes(name))
            {
                break 'listing Ok(Some(found));
            }
            offset += reclen;
        }
    };
    drop(directory);
    result
}

#[cfg(test)]
mod find_in_directory_tests {
    use super::*;

    /// Every name `std::fs::read_dir` finds is visited once, the visit stops
    /// at the first `Some`, and a path that is not a directory is an error.
    #[test]
    fn find_in_directory_visits_every_name_and_stops_at_the_first_find() {
        let directory = tempfile::tempdir().unwrap();
        for index in 0..300 {
            std::fs::write(
                directory
                    .path()
                    .join(format!("entry-with-a-long-name-{index}")),
                b"",
            )
            .unwrap();
        }
        let mut visited = Vec::new();
        let none: Option<()> = find_in_directory(directory.path(), |name| {
            visited.push(name.to_owned());
            None
        })
        .unwrap();
        assert!(none.is_none());
        visited.sort();
        let mut expected: Vec<_> = std::fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        expected.sort();
        assert_eq!(visited.len(), 300);
        assert_eq!(visited, expected);
        let mut calls = 0;
        let found = find_in_directory(directory.path(), |name| {
            calls += 1;
            (calls == 7).then(|| name.to_owned())
        })
        .unwrap();
        assert!(found.is_some());
        assert_eq!(calls, 7);
        let file = directory.path().join("entry-with-a-long-name-0");
        assert!(find_in_directory(&file, |_| Some(())).is_err());
    }
}
