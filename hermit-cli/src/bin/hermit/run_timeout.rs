/* Copyright (c) Meta Platforms, Inc. and affiliates. */

//! Process alarms belong only to the isolated CLI init, never an in-process
//! Record/Replay or run library operation that can return a retained owner.
use std::sync::atomic::AtomicPtr;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use hermit::Error;
use hermit::HERMIT_DEADLINE_EXIT;
use nix::sys::signal::SaFlags;
use nix::sys::signal::SigAction;
use nix::sys::signal::SigHandler;
use nix::sys::signal::SigSet;
use nix::sys::signal::Signal;
use nix::sys::signal::sigaction;

pub(super) const RUN_TIMEOUT_UNWIND_GRACE: Duration = Duration::from_secs(10);

/// Test-only: hold the post-expiry path open past the grace so the SIGALRM
/// fallback is the thing that ends the run.
///
/// ⚠️ THIS EXISTS BECAUSE THE FALLBACK COULD NOT BE MADE TO FIRE ANY OTHER WAY,
/// AND AN UNEXERCISED SAFETY PATH IS THE FAILURE MODE THIS PROJECT KEEPS
/// FINDING. Measured 2026-08-26 at this commit: the primary path fired at
/// exactly the bound for a userspace spinner, a guest blocked reading a pipe
/// with no writer, a guest that `SIGSTOP`s itself, an eight-thread guest
/// ignoring `SIGTERM`, and a multi-process guest ignoring `SIGTERM` -- five
/// shapes, five clean unwinds, no wedge. That is a good result for the primary
/// path and it leaves the fallback with zero executions, which is exactly the
/// mechanism-that-has-never-run shape.
///
/// ⚠️ WHAT THIS DOES AND DOES NOT REPRODUCE, stated precisely rather than
/// implied. It reproduces the CONDITION the fallback is specified against --
/// the post-expiry path not completing within `RUN_TIMEOUT_UNWIND_GRACE` -- and
/// it exercises the real alarm, the real inherited-mask handling, the real
/// handler, the real message and the real `_exit`. It does NOT reproduce any
/// particular upstream CAUSE of a slow unwind, because none is known; the delay
/// is here, after the drop, rather than inside a wedged destructor. A future
/// reader must not read a passing fallback test as evidence that some specific
/// teardown hang is handled.
///
/// Deliberately keyed off an internal environment variable rather than a
/// `cfg(test)` gate: the fallback lives in the shipped binary and must be
/// exercised there, not in a differently-compiled one.
pub(super) fn stall_the_unwind_if_asked() {
    const STALL_ENV: &str = "HERMIT_INTERNAL_RUN_TIMEOUT_STALL_UNWIND";
    if std::env::var_os(STALL_ENV).as_deref() != Some(std::ffi::OsStr::new("1")) {
        return;
    }
    // Comfortably past the grace, so the alarm -- not this sleep -- ends the
    // process. If the fallback is broken this returns and the caller sees an
    // ordinary timeout, which is what makes the test able to fail.
    std::thread::sleep(RUN_TIMEOUT_UNWIND_GRACE + Duration::from_secs(5));
}

static RUN_TIMEOUT_MESSAGE: AtomicPtr<u8> = AtomicPtr::new(std::ptr::null_mut());
static RUN_TIMEOUT_MESSAGE_LEN: AtomicUsize = AtomicUsize::new(0);

/// The hard fallback: fires only if the unwind above did not finish in time.
///
/// Identical in construction to `record_start.rs`'s `recording_timeout_handler`
/// -- non-blocking stderr so a full pipe cannot wedge the handler, then
/// `_exit` -- and identical in exit code, because "a deadline fired" is one
/// meaning and 124 already carries it for GNU `timeout`, for `safehermit`'s wall
/// bound, and for `hermit record`'s own deadline. Reusing it here adds no new
/// collision; inventing a fourth number for the same event would.
extern "C" fn run_timeout_fallback_handler(_signal: libc::c_int) {
    let len = RUN_TIMEOUT_MESSAGE_LEN.load(Ordering::Acquire);
    let message = RUN_TIMEOUT_MESSAGE.load(Ordering::Acquire);
    if !message.is_null() && len != 0 {
        // SAFETY: the message is leaked before the timer is armed, and
        // fcntl(2), write(2) and _exit(2) are async-signal-safe.
        unsafe {
            let flags = libc::fcntl(libc::STDERR_FILENO, libc::F_GETFL);
            if flags != -1 {
                libc::fcntl(libc::STDERR_FILENO, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
            libc::write(libc::STDERR_FILENO, message.cast(), len);
        }
    }
    // Exiting the namespace init tears down the container and its tracees.
    // SAFETY: _exit(2) is async-signal-safe and runs no Rust destructors --
    // which is precisely why this is the fallback and not the primary path.
    unsafe { libc::_exit(HERMIT_DEADLINE_EXIT) }
}

pub(super) struct RunTimeoutFallback {
    previous_handler: SigAction,
    reblock_sigalrm: bool,
}

impl RunTimeoutFallback {
    pub(super) fn arm(after: Duration) -> Result<Self, Error> {
        let seconds: libc::c_uint = after
            .as_secs()
            .try_into()
            .map_err(|_| Error::msg("--timeout exceeds the platform alarm limit"))?;
        let message = Box::leak(
            format!(
                "HERMIT_RUN_TIMEOUT_FALLBACK: the --timeout unwind did not complete within {} seconds; \
                 the container was terminated without a clean teardown\n",
                RUN_TIMEOUT_UNWIND_GRACE.as_secs()
            )
            .into_boxed_str(),
        );
        RUN_TIMEOUT_MESSAGE.store(message.as_mut_ptr(), Ordering::Release);
        RUN_TIMEOUT_MESSAGE_LEN.store(message.len(), Ordering::Release);

        let action = SigAction::new(
            SigHandler::Handler(run_timeout_fallback_handler),
            SaFlags::SA_RESETHAND,
            SigSet::empty(),
        );
        // SAFETY: the handler uses only async-signal-safe operations and stays
        // installed until this guard disarms it.
        let previous_handler = unsafe { sigaction(Signal::SIGALRM, &action) }?;

        // A blocked SIGALRM stays pending forever and the handler never runs,
        // silently disabling the fallback. `record_start.rs` learned this too.
        let mut alarm = SigSet::empty();
        alarm.add(Signal::SIGALRM);
        let reblock_sigalrm = SigSet::thread_get_mask()
            .map(|mask| mask.contains(Signal::SIGALRM))
            .unwrap_or(false);
        if reblock_sigalrm {
            let _ = alarm.thread_unblock();
        }

        // SAFETY: `seconds` fits c_uint.
        unsafe { libc::alarm(seconds) };
        Ok(Self {
            previous_handler,
            reblock_sigalrm,
        })
    }
}

impl Drop for RunTimeoutFallback {
    fn drop(&mut self) {
        // SAFETY: disarm the alarm before restoring the inherited handler.
        unsafe {
            libc::alarm(0);
            let _ = sigaction(Signal::SIGALRM, &self.previous_handler);
        }
        if self.reblock_sigalrm {
            let mut alarm = SigSet::empty();
            alarm.add(Signal::SIGALRM);
            let _ = alarm.thread_block();
        }
        RUN_TIMEOUT_MESSAGE_LEN.store(0, Ordering::Release);
        RUN_TIMEOUT_MESSAGE.store(std::ptr::null_mut(), Ordering::Release);
    }
}
