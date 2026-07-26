/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Regression guest for the determinized signal-sending siblings of the already
//! Determinized `kill`/`tgkill`: `tkill(2)`, `rt_tgsigqueueinfo(2)`, and
//! `rt_sigqueueinfo(2)`. Before these were reclassified Determinized in Detcore,
//! any use aborted under `--strict` with an unsupported-syscall error.
//!
//! The guest installs an `SA_SIGINFO` handler for `SIGUSR1` and then delivers a
//! signal to itself three ways, asserting the handler runs each time:
//!   1. `tkill(tid, SIGUSR1)`            — two-argument thread-directed form.
//!   2. `rt_tgsigqueueinfo(pid, tid, ..)` — thread-directed, carries `siginfo_t`.
//!   3. `rt_sigqueueinfo(pid, ..)`        — process-directed, carries `siginfo_t`.
//!
//! The raw syscalls are issued directly: glibc has no public wrapper for
//! `tkill`/`rt_tgsigqueueinfo`, and `sigqueue()` (the `rt_sigqueueinfo` wrapper)
//! would obscure the exact call under test. Self-delivery keeps the guest
//! single-process so it does not depend on cross-process rendezvous.

use std::sync::atomic::AtomicI32;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

/// si_code used by the sigqueue family (reserved value indicating a
/// user-queued signal); the kernel accepts it for same-thread-group delivery.
const SI_QUEUE: i32 = -1;

static DELIVERED: AtomicUsize = AtomicUsize::new(0);
static LAST_CODE: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_signal(_sig: i32, info: *mut libc::siginfo_t, _ctx: *mut libc::c_void) {
    DELIVERED.fetch_add(1, Ordering::SeqCst);
    if !info.is_null() {
        // SAFETY: the kernel hands us a valid siginfo_t for the duration of the
        // handler; reading si_code is a plain scalar load.
        LAST_CODE.store(unsafe { (*info).si_code }, Ordering::SeqCst);
    }
}

fn raw_gettid() -> i32 {
    // SAFETY: gettid takes no arguments and cannot fail.
    unsafe { libc::syscall(libc::SYS_gettid) as i32 }
}

fn raw_getpid() -> i32 {
    // SAFETY: getpid takes no arguments and cannot fail.
    unsafe { libc::syscall(libc::SYS_getpid) as i32 }
}

/// A minimal, kernel-acceptable `siginfo_t` for a queued signal.
fn queued_siginfo(sig: i32) -> libc::siginfo_t {
    // SAFETY: siginfo_t is a plain-old-data union; a zeroed value is valid.
    let mut si: libc::siginfo_t = unsafe { std::mem::zeroed() };
    si.si_signo = sig;
    si.si_errno = 0;
    si.si_code = SI_QUEUE;
    si
}

fn main() {
    // Install an SA_SIGINFO handler so we observe every delivery.
    // SAFETY: sigaction is a plain struct; a zeroed value is a valid starting
    // point before we fill in the handler and flags.
    let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
    let handler: extern "C" fn(i32, *mut libc::siginfo_t, *mut libc::c_void) = on_signal;
    sa.sa_sigaction = handler as usize;
    sa.sa_flags = libc::SA_SIGINFO;
    // SAFETY: sa_mask is initialized in place before use.
    unsafe { libc::sigemptyset(&mut sa.sa_mask) };
    // SAFETY: sa points at a fully initialized sigaction; oldact is NULL.
    let rc = unsafe { libc::sigaction(libc::SIGUSR1, &sa, std::ptr::null_mut()) };
    assert_eq!(rc, 0, "sigaction(SIGUSR1) failed");

    let tid = raw_gettid();
    let pid = raw_getpid();

    // 1. tkill(tid, SIGUSR1): older two-argument thread-directed signal.
    // SAFETY: tid names this live thread; SIGUSR1 has a handler installed.
    let r = unsafe { libc::syscall(libc::SYS_tkill, tid, libc::SIGUSR1) };
    assert_eq!(r, 0, "tkill returned {r}");
    assert_eq!(
        DELIVERED.load(Ordering::SeqCst),
        1,
        "tkill did not deliver SIGUSR1"
    );

    // 2. rt_tgsigqueueinfo(pid, tid, SIGUSR1, &si): thread-directed with siginfo.
    let mut si = queued_siginfo(libc::SIGUSR1);
    // SAFETY: pid/tid name this process/thread; si is a valid siginfo_t.
    let r = unsafe {
        libc::syscall(
            libc::SYS_rt_tgsigqueueinfo,
            pid,
            tid,
            libc::SIGUSR1,
            &mut si as *mut libc::siginfo_t,
        )
    };
    assert_eq!(r, 0, "rt_tgsigqueueinfo returned {r}");
    assert_eq!(
        DELIVERED.load(Ordering::SeqCst),
        2,
        "rt_tgsigqueueinfo did not deliver SIGUSR1"
    );
    assert_eq!(
        LAST_CODE.load(Ordering::SeqCst),
        SI_QUEUE,
        "rt_tgsigqueueinfo did not preserve the queued si_code"
    );

    // 3. rt_sigqueueinfo(pid, SIGUSR1, &si): process-directed with siginfo. In a
    // single-threaded process this is routed to the sole live thread.
    let mut si = queued_siginfo(libc::SIGUSR1);
    // SAFETY: pid names this process; si is a valid siginfo_t.
    let r = unsafe {
        libc::syscall(
            libc::SYS_rt_sigqueueinfo,
            pid,
            libc::SIGUSR1,
            &mut si as *mut libc::siginfo_t,
        )
    };
    assert_eq!(r, 0, "rt_sigqueueinfo returned {r}");
    assert_eq!(
        DELIVERED.load(Ordering::SeqCst),
        3,
        "rt_sigqueueinfo did not deliver SIGUSR1"
    );

    println!("tkill + rt_tgsigqueueinfo + rt_sigqueueinfo delivery OK. Test complete.");
}
