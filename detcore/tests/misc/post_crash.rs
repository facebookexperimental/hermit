/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The post-crash timeslice switch (`Config::post_crash_timeslice`): a crash
//! whose handler starts another program, as Android's crash handler starts
//! `crash_dump`, switches the crashed process's later slices to the
//! post-crash bound; a fault the handler resolves and resumes from, and a
//! fault signal sent with `tgkill`, do not; a handler waiting in
//! `sigsuspend` while a sibling execs switches the same way in every run.
//! The preemption record says where the switch was taken.
//!
//! Each scenario runs after an exec, as every guest Hermit runs does (Detcore
//! takes no step before the guest's first exec): the guest function execs
//! this test binary again, which runs the scenario named in
//! `SCENARIO_ENV`.

use std::num::NonZeroU64;
use std::path::Path;
use std::path::PathBuf;

use detcore::Detcore;

/// Iterations of the CPU-bound loop the guest runs after its crash: tens of
/// thousands of branches, several slices at the fine bound and part of one
/// at the post-crash bound. Kept small because a host without precise
/// branch-count interrupts steps through bounded code slowly.
const SPINS: u64 = 10_000;

/// The run's own slice bound in these tests, in virtual nanoseconds (2000
/// branches).
const FINE: u64 = 20_000;

/// Names the scenario a re-executed test binary runs.
const SCENARIO_ENV: &str = "HERMIT_POST_CRASH_SCENARIO";

/// In the re-executed test binary, run the scenario `test` names; outside
/// it, return.
fn run_scenario_if_inner() {
    match std::env::var(SCENARIO_ENV).as_deref() {
        Ok("abort_handler_execs") => abort_handler_execs(),
        Ok("resolved_fault_then_exec") => resolved_fault_then_exec(),
        Ok("sent_segv_handler_execs") => sent_segv_handler_execs(),
        Ok("handler_waits_while_sibling_execs") => handler_waits_while_sibling_execs(),
        Ok("handler_unblocks_then_sibling_execs") => handler_unblocks_then_sibling_execs(),
        Ok("pending_realtime_handler_execs") => pending_realtime_handler_execs(),
        #[cfg(target_arch = "x86_64")]
        Ok("recovered_then_rdtsc_until_sibling_execs") => {
            recovered_then_rdtsc_until_sibling_execs()
        }
        Ok("one_of_two_crashed_processes_recovers") => one_of_two_crashed_processes_recovers(),
        Ok("two_trials_crash_in_turn") => two_trials_crash_in_turn(),
        Ok("handler_aborts") => handler_aborts(),
        Ok(other) => panic!("unknown scenario {other}"),
        Err(_) => return,
    }
    std::process::exit(0);
}

fn spin() -> u64 {
    let mut acc = 0_u64;
    for i in 0..SPINS {
        acc = std::hint::black_box(acc.wrapping_mul(31).wrapping_add(i));
    }
    acc
}

/// What a run's preemption record says about the switch.
struct Recorded {
    /// The post-crash bound in effect.
    bound: Option<u64>,
    /// Where the first switch was taken.
    switch: Option<detcore::post_crash::PostCrashSwitch>,
    /// Every switch taken, in order.
    switches: Vec<detcore::post_crash::PostCrashSwitch>,
    /// How many slice ends the record holds, across all threads.
    slice_ends: usize,
}

/// Run `scenario` (run by test `test` of this binary) under a fine bound with
/// the switch at `post_crash`, recording preemptions, and read back the
/// record.
fn recorded(test: &'static str, scenario: &'static str, post_crash: Option<u64>) -> Recorded {
    recorded_at(test, scenario, post_crash, FINE)
}

/// [`recorded`] under the bound `fine`.
fn recorded_at(
    test: &'static str,
    scenario: &'static str,
    post_crash: Option<u64>,
    fine: u64,
) -> Recorded {
    let record: PathBuf = tempfile::NamedTempFile::new()
        .unwrap()
        .into_temp_path()
        .keep()
        .unwrap();
    let config = detcore::Config {
        // The running backend's own capabilities, as `hermit run` copies them
        // over a configuration before a run: a caught crash arms the switch
        // only where the kernel's signal masks are the guest's.
        backend: <reverie_ptrace::PtraceBackend as reverie::Backend>::capabilities(),
        max_timeslice: NonZeroU64::new(fine),
        // Chaos records every slice end in the preemption record.
        chaos: true,
        seed: 1,
        sequentialize_threads: true,
        record_preemptions: true,
        record_preemptions_to: Some(record.clone()),
        post_crash_timeslice: post_crash.and_then(NonZeroU64::new),
        ..Default::default()
    };
    // On a thread of its own with a large stack: Detcore's futures outgrow
    // the test harness thread's.
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(move || {
            detcore_testutils::check_fn_with_config::<Detcore, _>(
                move || {
                    let status = std::process::Command::new(std::env::current_exe().unwrap())
                        .arg(format!("post_crash::{test}"))
                        .args(["--exact", "--nocapture", "--test-threads=1"])
                        .env(SCENARIO_ENV, scenario)
                        .status()
                        .unwrap();
                    assert!(status.success(), "{status:?}");
                },
                detcore_testutils::prepare_ptrace_timer_config(config),
                true,
            );
        })
        .unwrap()
        .join()
        .unwrap();
    read(&record)
}

fn read(path: &Path) -> Recorded {
    let reader = detcore::preemptions::PreemptionReader::new(path);
    let record = reader.load_all();
    std::fs::remove_file(path).unwrap();
    Recorded {
        bound: record.post_crash_timeslice(),
        switch: record.post_crash().first().copied(),
        switches: record.post_crash().to_vec(),
        slice_ends: reader.size(),
    }
}

/// SIGABRT's handler forks a child that execs `true`, as debuggerd's handler
/// starts `crash_dump`, waits for it, computes, and exits without
/// returning.
fn abort_handler_execs() {
    extern "C" fn on_abort(_: libc::c_int) {
        // SAFETY: async-signal-safe calls only, in a single-threaded guest.
        unsafe {
            let child = libc::fork();
            if child == 0 {
                let path = c"/bin/true";
                let argv = [path.as_ptr(), std::ptr::null()];
                let envp = [std::ptr::null()];
                libc::execve(path.as_ptr(), argv.as_ptr(), envp.as_ptr());
                libc::_exit(127);
            }
            let mut status = 0;
            libc::waitpid(child, &mut status, 0);
        }
        std::hint::black_box(spin());
        // SAFETY: ends the process from the handler, as a crash handler does.
        unsafe { libc::_exit(0) };
    }
    // SAFETY: installs a plain handler for SIGABRT.
    unsafe {
        libc::signal(libc::SIGABRT, on_abort as *const () as libc::sighandler_t);
        libc::raise(libc::SIGABRT);
    }
    unreachable!("the handler exits");
}

/// The switch is taken at the child's `execve`, names SIGABRT and the
/// crashed thread, and every slice after it is coarser: the run records
/// fewer slice ends than with the switch off.
#[test]
fn a_crash_handler_that_execs_switches_to_the_post_crash_bound() {
    run_scenario_if_inner();
    const TEST: &str = "a_crash_handler_that_execs_switches_to_the_post_crash_bound";
    let switched = recorded(TEST, "abort_handler_execs", Some(200_000_000));
    let switch = switched.switch.expect("the switch is taken");
    assert_eq!(switch.signal, libc::SIGABRT, "{switch:?}");
    let exec_by = switch.exec_by.expect("taken at the child's execve");
    assert_ne!(exec_by, switch.dtid, "{switch:?}");
    assert_eq!(switch.bound, 200_000_000, "{switch:?}");
    assert_eq!(switched.bound, Some(200_000_000));

    let off = recorded(TEST, "abort_handler_execs", None);
    assert_eq!((off.bound, off.switch), (None, None));
    // Up to the crash both runs slice the same; after it, the loop takes
    // several fine slices without the switch and part of one with it.
    let (with, without) = (switched.slice_ends, off.slice_ends);
    assert!(
        with + 5 <= without,
        "slice ends with the switch {with}, without {without}"
    );
}

/// SIGSEGV from a real fault, which the handler resolves (it makes the page
/// readable) and returns from, then an exec: a runtime's handled fault never
/// switches.
fn resolved_fault_then_exec() {
    static PAGE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    extern "C" fn on_segv(_: libc::c_int) {
        let page = PAGE.load(std::sync::atomic::Ordering::Relaxed);
        // SAFETY: makes the guest's own page readable.
        unsafe { libc::mprotect(page as *mut libc::c_void, 4096, libc::PROT_READ) };
    }
    // SAFETY: maps one inaccessible page and installs a handler for SIGSEGV.
    unsafe {
        let page = libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        );
        assert_ne!(page, libc::MAP_FAILED);
        PAGE.store(page as usize, std::sync::atomic::Ordering::Relaxed);
        libc::signal(libc::SIGSEGV, on_segv as *const () as libc::sighandler_t);
        assert_eq!(std::ptr::read_volatile(page as *const u8), 0);
    }
    let status = std::process::Command::new("/bin/true").status().unwrap();
    assert!(status.success());
}

#[test]
fn a_fault_the_handler_resolves_does_not_switch() {
    run_scenario_if_inner();
    let record = recorded(
        "a_fault_the_handler_resolves_does_not_switch",
        "resolved_fault_then_exec",
        Some(200_000_000),
    );
    assert_eq!(record.switch, None);
    assert_eq!(record.bound, Some(200_000_000));
}

/// A real fault whose handler waits in `sigsuspend` with nothing blocked
/// while a sibling thread spawns a program, then wakes it with SIGUSR1; the
/// handler resolves the fault and returns. The kernel swaps the waiting
/// thread's mask only inside the wait, so the switch must not depend on
/// when the sibling's exec reads it.
fn handler_waits_while_sibling_execs() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicI32;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    static PAGE: AtomicUsize = AtomicUsize::new(0);
    static IN_HANDLER: AtomicBool = AtomicBool::new(false);
    static FAULTING: AtomicI32 = AtomicI32::new(0);
    extern "C" fn on_usr1(_: libc::c_int) {}
    extern "C" fn on_segv(_: libc::c_int) {
        IN_HANDLER.store(true, Ordering::SeqCst);
        // SAFETY: waits with an empty mask until SIGUSR1's handler has run,
        // then makes the guest's own page readable.
        unsafe {
            let mut nothing: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut nothing);
            libc::sigsuspend(&nothing);
            let page = PAGE.load(Ordering::SeqCst);
            libc::mprotect(page as *mut libc::c_void, 4096, libc::PROT_READ);
        }
    }
    // SAFETY: maps one inaccessible page and installs the two handlers.
    let page = unsafe {
        let page = libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        );
        assert_ne!(page, libc::MAP_FAILED);
        libc::signal(libc::SIGUSR1, on_usr1 as *const () as libc::sighandler_t);
        libc::signal(libc::SIGSEGV, on_segv as *const () as libc::sighandler_t);
        page
    };
    PAGE.store(page as usize, Ordering::SeqCst);
    FAULTING.store(nix::unistd::gettid().as_raw(), Ordering::SeqCst);
    let sibling = std::thread::spawn(|| {
        while !IN_HANDLER.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        let status = std::process::Command::new("/bin/true").status().unwrap();
        assert!(status.success());
        // SAFETY: wakes the faulting thread's handler.
        unsafe {
            libc::syscall(
                libc::SYS_tgkill,
                libc::getpid(),
                FAULTING.load(Ordering::SeqCst),
                libc::SIGUSR1,
            );
        }
    });
    // SAFETY: the read faults; the handler makes the page readable.
    assert_eq!(unsafe { std::ptr::read_volatile(page as *const u8) }, 0);
    sibling.join().unwrap();
}

#[test]
fn a_handler_waiting_in_sigsuspend_switches_the_same_way_every_run() {
    run_scenario_if_inner();
    let test = "a_handler_waiting_in_sigsuspend_switches_the_same_way_every_run";
    let scenario = "handler_waits_while_sibling_execs";
    let first = recorded(test, scenario, Some(200_000_000)).switch;
    let second = recorded(test, scenario, Some(200_000_000)).switch;
    // The handler still runs (its signal blocked as of its own last stop,
    // the wait's entry) when the sibling's child execs.
    let first = first.expect("the switch is taken");
    assert_eq!(first.signal, libc::SIGSEGV);
    let second = second.expect("the switch is taken again");
    assert_eq!(
        (second.signal, second.turn, second.exec_by.is_some()),
        (first.signal, first.turn, first.exec_by.is_some())
    );
}

/// A real fault whose handler resolves it, then unblocks SIGSEGV with
/// `pthread_sigmask` and returns; a sibling thread waits for that and then
/// spawns a program. Under a bound smaller than a syscall's cost the slice
/// ends at the unblocking call's exit, which hands the turn to the sibling
/// there: the handler has returned in all but its last instructions, so the
/// switch must not be taken.
fn handler_unblocks_then_sibling_execs() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    static PAGE: AtomicUsize = AtomicUsize::new(0);
    static UNBLOCKING: AtomicBool = AtomicBool::new(false);
    extern "C" fn on_segv(_: libc::c_int) {
        // SAFETY: makes the guest's own page readable, then unblocks SIGSEGV.
        unsafe {
            let page = PAGE.load(Ordering::SeqCst);
            libc::mprotect(page as *mut libc::c_void, 4096, libc::PROT_READ);
            UNBLOCKING.store(true, Ordering::SeqCst);
            let mut segv: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut segv);
            libc::sigaddset(&mut segv, libc::SIGSEGV);
            libc::pthread_sigmask(libc::SIG_UNBLOCK, &segv, std::ptr::null_mut());
        }
    }
    // SAFETY: maps one inaccessible page and installs the handler.
    let page = unsafe {
        let page = libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        );
        assert_ne!(page, libc::MAP_FAILED);
        libc::signal(libc::SIGSEGV, on_segv as *const () as libc::sighandler_t);
        page
    };
    PAGE.store(page as usize, Ordering::SeqCst);
    let sibling = std::thread::spawn(|| {
        while !UNBLOCKING.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        let status = std::process::Command::new("/bin/true").status().unwrap();
        assert!(status.success());
    });
    // SAFETY: the read faults; the handler makes the page readable.
    assert_eq!(unsafe { std::ptr::read_volatile(page as *const u8) }, 0);
    sibling.join().unwrap();
}

#[test]
fn a_handler_that_unblocks_before_a_sibling_execs_does_not_switch() {
    run_scenario_if_inner();
    // Below the 10 microseconds a syscall costs, so every syscall's exit
    // ends the slice and can hand the turn over.
    let record = recorded_at(
        "a_handler_that_unblocks_before_a_sibling_execs_does_not_switch",
        "handler_unblocks_then_sibling_execs",
        Some(200_000_000),
        5_000,
    );
    assert_eq!(record.switch, None);
}

/// Maps one inaccessible page and installs `handler` for SIGSEGV with
/// `sa_mask` adding `also_block`; returns the page.
fn fault_page_with(handler: extern "C" fn(libc::c_int), also_block: Option<libc::c_int>) -> usize {
    // SAFETY: maps one page and installs a handler, with valid arguments.
    unsafe {
        let page = libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        );
        assert_ne!(page, libc::MAP_FAILED);
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handler as *const () as libc::sighandler_t;
        libc::sigemptyset(&mut action.sa_mask);
        if let Some(signal) = also_block {
            libc::sigaddset(&mut action.sa_mask, signal);
        }
        assert_eq!(
            libc::sigaction(libc::SIGSEGV, &action, std::ptr::null_mut()),
            0
        );
        page as usize
    }
}

/// A real fault whose handler resolves it and queues a realtime signal its
/// mask blocks, then returns straight into that signal's handler, whose mask
/// blocks SIGSEGV and which spawns a program: the crash handler has
/// returned, so the switch must not be taken.
fn pending_realtime_handler_execs() {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    static PAGE: AtomicUsize = AtomicUsize::new(0);
    extern "C" fn on_rt(_: libc::c_int) {
        let status = std::process::Command::new("/bin/true").status().unwrap();
        assert!(status.success());
    }
    extern "C" fn on_segv(_: libc::c_int) {
        // SAFETY: makes the guest's own page readable and queues SIGRTMIN to
        // this thread, which this handler's mask blocks until it returns.
        unsafe {
            libc::mprotect(
                PAGE.load(Ordering::SeqCst) as *mut libc::c_void,
                4096,
                libc::PROT_READ,
            );
            libc::syscall(
                libc::SYS_tgkill,
                libc::getpid(),
                libc::gettid(),
                libc::SIGRTMIN(),
            );
        }
    }
    // SAFETY: installs the realtime handler, whose mask blocks SIGSEGV.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_rt as *const () as libc::sighandler_t;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaddset(&mut action.sa_mask, libc::SIGSEGV);
        assert_eq!(
            libc::sigaction(libc::SIGRTMIN(), &action, std::ptr::null_mut()),
            0
        );
    }
    let page = fault_page_with(on_segv, Some(libc::SIGRTMIN()));
    PAGE.store(page, Ordering::SeqCst);
    // SAFETY: the read faults; the handler makes the page readable.
    assert_eq!(unsafe { std::ptr::read_volatile(page as *const u8) }, 0);
}

#[test]
fn a_pending_realtime_handler_that_execs_does_not_switch() {
    run_scenario_if_inner();
    let record = recorded(
        "a_pending_realtime_handler_that_execs_does_not_switch",
        "pending_realtime_handler_execs",
        Some(200_000_000),
    );
    assert_eq!(record.switches, Vec::new());
}

/// A real fault whose handler resolves it and returns; the thread then runs
/// RDTSC, which Detcore traps, in a loop with no syscall until its slice ends
/// at a trap and a waiting sibling spawns a program: the switch must not be
/// taken.
#[cfg(target_arch = "x86_64")]
fn recovered_then_rdtsc_until_sibling_execs() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    static PAGE: AtomicUsize = AtomicUsize::new(0);
    static RETURNED: AtomicBool = AtomicBool::new(false);
    static SPAWNED: AtomicBool = AtomicBool::new(false);
    extern "C" fn on_segv(_: libc::c_int) {
        // SAFETY: makes the guest's own page readable.
        unsafe {
            libc::mprotect(
                PAGE.load(Ordering::SeqCst) as *mut libc::c_void,
                4096,
                libc::PROT_READ,
            );
        }
    }
    let page = fault_page_with(on_segv, None);
    PAGE.store(page, Ordering::SeqCst);
    let sibling = std::thread::spawn(|| {
        while !RETURNED.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        let status = std::process::Command::new("/bin/true").status().unwrap();
        assert!(status.success());
        SPAWNED.store(true, Ordering::SeqCst);
    });
    // SAFETY: the read faults; the handler makes the page readable.
    assert_eq!(unsafe { std::ptr::read_volatile(page as *const u8) }, 0);
    RETURNED.store(true, Ordering::SeqCst);
    let mut acc = 0_u64;
    while !SPAWNED.load(Ordering::SeqCst) {
        // SAFETY: RDTSC has no preconditions.
        acc = acc.wrapping_add(unsafe { core::arch::x86_64::_rdtsc() });
    }
    std::hint::black_box(acc);
    sibling.join().unwrap();
}

#[cfg(target_arch = "x86_64")]
#[test]
fn a_recovered_handler_followed_by_rdtsc_traps_does_not_switch() {
    run_scenario_if_inner();
    let record = recorded(
        "a_recovered_handler_followed_by_rdtsc_traps_does_not_switch",
        "recovered_then_rdtsc_until_sibling_execs",
        Some(200_000_000),
    );
    assert_eq!(record.switches, Vec::new());
}

/// Two unrelated processes crash into handlers: a forked child, whose
/// handler never returns, and this process, whose handler resolves the
/// fault and returns before this process spawns a program. That exec is in
/// the recovered crash's former lineage, not the child's, so the switch must
/// not be taken.
fn one_of_two_crashed_processes_recovers() {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    static PAGE: AtomicUsize = AtomicUsize::new(0);
    static READY: AtomicUsize = AtomicUsize::new(0);
    extern "C" fn stuck(_: libc::c_int) {
        // SAFETY: tells the parent this handler runs, then waits to be killed.
        unsafe {
            let fd = READY.load(Ordering::SeqCst) as libc::c_int;
            libc::write(fd, b"x".as_ptr().cast(), 1);
            loop {
                libc::pause();
            }
        }
    }
    extern "C" fn resolves(_: libc::c_int) {
        // SAFETY: makes the guest's own page readable.
        unsafe {
            libc::mprotect(
                PAGE.load(Ordering::SeqCst) as *mut libc::c_void,
                4096,
                libc::PROT_READ,
            );
        }
    }
    let mut fds = [0; 2];
    // SAFETY: a pipe, then a fork whose child runs only async-signal-safe
    // code before it faults.
    let child = unsafe {
        assert_eq!(libc::pipe(fds.as_mut_ptr()), 0);
        let child = libc::fork();
        assert!(child >= 0);
        if child == 0 {
            READY.store(fds[1] as usize, Ordering::SeqCst);
            let page = fault_page_with(stuck, None);
            std::ptr::read_volatile(page as *const u8);
            libc::_exit(1);
        }
        let mut byte = 0_u8;
        assert_eq!(libc::read(fds[0], (&mut byte as *mut u8).cast(), 1), 1);
        child
    };
    let page = fault_page_with(resolves, None);
    PAGE.store(page, Ordering::SeqCst);
    // SAFETY: the read faults; the handler makes the page readable.
    assert_eq!(unsafe { std::ptr::read_volatile(page as *const u8) }, 0);
    let status = std::process::Command::new("/bin/true").status().unwrap();
    assert!(status.success());
    // SAFETY: ends and reaps the child.
    unsafe {
        libc::kill(child, libc::SIGKILL);
        libc::waitpid(child, std::ptr::null_mut(), 0);
    }
}

#[test]
fn a_recovered_lineage_does_not_take_another_crashs_switch() {
    run_scenario_if_inner();
    let record = recorded(
        "a_recovered_lineage_does_not_take_another_crashs_switch",
        "one_of_two_crashed_processes_recovers",
        Some(200_000_000),
    );
    assert_eq!(record.switches, Vec::new());
}

/// Two child trials, one after another, abort with the default action: each
/// takes a switch of its own.
fn two_trials_crash_in_turn() {
    for _ in 0..2 {
        // SAFETY: a fork whose child resets SIGABRT and aborts.
        unsafe {
            let child = libc::fork();
            assert!(child >= 0);
            if child == 0 {
                libc::signal(libc::SIGABRT, libc::SIG_DFL);
                libc::abort();
            }
            let mut status = 0;
            assert_eq!(libc::waitpid(child, &mut status, 0), child);
            assert!(libc::WIFSIGNALED(status));
        }
    }
}

/// A child whose SIGSEGV handler aborts with SIGABRT's default action: the
/// abort switches at once, though the thread is armed for the fault.
fn handler_aborts() {
    extern "C" fn on_segv(_: libc::c_int) {
        // SAFETY: restores SIGABRT's default action and aborts.
        unsafe {
            libc::signal(libc::SIGABRT, libc::SIG_DFL);
            libc::abort();
        }
    }
    // SAFETY: a fork whose child faults into a handler that aborts.
    unsafe {
        let child = libc::fork();
        assert!(child >= 0);
        if child == 0 {
            let page = fault_page_with(on_segv, None);
            std::ptr::read_volatile(page as *const u8);
            libc::_exit(1);
        }
        let mut status = 0;
        assert_eq!(libc::waitpid(child, &mut status, 0), child);
        assert!(libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGABRT);
    }
}

#[test]
fn an_abort_inside_a_fault_handler_switches_at_once() {
    run_scenario_if_inner();
    let record = recorded(
        "an_abort_inside_a_fault_handler_switches_at_once",
        "handler_aborts",
        Some(200_000_000),
    );
    assert_eq!(record.switches.len(), 1, "{:?}", record.switches);
    assert_eq!(record.switches[0].signal, libc::SIGABRT);
    assert_eq!(record.switches[0].exec_by, None);
}

#[test]
fn each_crashing_trial_takes_its_own_switch() {
    run_scenario_if_inner();
    let record = recorded(
        "each_crashing_trial_takes_its_own_switch",
        "two_trials_crash_in_turn",
        Some(200_000_000),
    );
    assert_eq!(record.switches.len(), 2, "{:?}", record.switches);
    assert_ne!(record.switches[0].dtid, record.switches[1].dtid);
    assert!(
        record
            .switches
            .iter()
            .all(|switch| switch.exec_by.is_none())
    );
}

/// SIGSEGV sent with `raise` (`tgkill`), caught by a handler that execs: not
/// a fault the kernel raised, so not a crash.
fn sent_segv_handler_execs() {
    extern "C" fn on_segv(_: libc::c_int) {
        // SAFETY: async-signal-safe calls only, in a single-threaded guest.
        unsafe {
            let child = libc::fork();
            if child == 0 {
                let path = c"/bin/true";
                let argv = [path.as_ptr(), std::ptr::null()];
                let envp = [std::ptr::null()];
                libc::execve(path.as_ptr(), argv.as_ptr(), envp.as_ptr());
                libc::_exit(127);
            }
            let mut status = 0;
            libc::waitpid(child, &mut status, 0);
            libc::_exit(0);
        }
    }
    // SAFETY: installs a handler and sends the signal to this thread.
    unsafe {
        libc::signal(libc::SIGSEGV, on_segv as *const () as libc::sighandler_t);
        libc::raise(libc::SIGSEGV);
    }
    unreachable!("the handler exits");
}

#[test]
fn a_sent_fault_signal_does_not_switch() {
    run_scenario_if_inner();
    let record = recorded(
        "a_sent_fault_signal_does_not_switch",
        "sent_segv_handler_execs",
        Some(200_000_000),
    );
    assert_eq!(record.switch, None);
    assert_eq!(record.bound, Some(200_000_000));
}
