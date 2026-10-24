/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A child subreaper (`prctl(PR_SET_CHILD_SUBREAPER)`) inherits the orphans of
//! its descendants, and Detcore's own record of children follows, so the
//! subreaper's waits see them (https://github.com/rrnewton/hermit/issues/3997).

fn set_child_subreaper(on: bool) {
    assert_eq!(
        unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, libc::c_ulong::from(on)) },
        0
    );
}

fn child_subreaper() -> libc::c_int {
    let mut flag: libc::c_int = -1;
    assert_eq!(
        unsafe { libc::prctl(libc::PR_GET_CHILD_SUBREAPER, &mut flag as *mut libc::c_int) },
        0
    );
    flag
}

/// Fork a child that forks the orphan-to-be and exits with status 3. The
/// orphan exits with status 7: before its parent when `orphan_first`, held as
/// a zombie with `WNOWAIT`; otherwise after it, as a live orphan.
fn fork_parent_of_orphan(orphan_first: bool) -> libc::pid_t {
    spawn_orphan_family(orphan_first, false, None)
}

/// `fork_parent_of_orphan`, with two options. With `default_sigchld` the child
/// restores SIGCHLD's default action first, so that the orphan stays a zombie
/// even when the caller ignores SIGCHLD (an ignored disposition is inherited
/// across fork, and would make the child reap it at once). With `report`, the
/// child writes one byte there once the zombie precondition holds.
///
/// The zombie precondition is asserted: the child exits with 99 instead of 3
/// unless `waitid(WNOWAIT)` reports exactly that orphan, exited with status 7.
fn spawn_orphan_family(
    orphan_first: bool,
    default_sigchld: bool,
    report: Option<libc::c_int>,
) -> libc::pid_t {
    let child = unsafe { libc::fork() };
    assert!(child >= 0, "fork failed");
    if child == 0 {
        if default_sigchld {
            let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
            action.sa_sigaction = libc::SIG_DFL;
            if unsafe { libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()) } != 0 {
                unsafe { libc::_exit(98) };
            }
        }
        let orphan = unsafe { libc::fork() };
        if orphan == 0 {
            if !orphan_first {
                for _ in 0..50 {
                    unsafe { libc::sched_yield() };
                }
            }
            unsafe { libc::_exit(7) };
        }
        if orphan_first {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let rc = unsafe {
                libc::waitid(
                    libc::P_PID,
                    orphan as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOWAIT,
                )
            };
            let zombie = rc == 0
                && unsafe { info.si_pid() } == orphan
                && info.si_code == libc::CLD_EXITED
                && unsafe { info.si_status() } == 7;
            if !zombie {
                unsafe { libc::_exit(99) };
            }
        }
        if let Some(fd) = report
            && unsafe { libc::write(fd, (&b'Z' as *const u8).cast(), 1) } != 1
        {
            unsafe { libc::_exit(97) };
        }
        unsafe { libc::_exit(3) };
    }
    child
}

/// Reap every child with blocking `wait`, returning each exit status in
/// order, and check that the last `wait` fails with ECHILD.
fn reap_all() -> Vec<i32> {
    let mut statuses = Vec::new();
    loop {
        let mut status = 0;
        let pid = unsafe { libc::wait(&mut status) };
        if pid < 0 {
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
            return statuses;
        }
        assert!(libc::WIFEXITED(status));
        statuses.push(libc::WEXITSTATUS(status));
    }
}

#[test]
fn a_subreaper_reaps_an_orphan_that_outlives_its_parent() {
    super::det_test_fn_sequential_without_pmu(|| {
        set_child_subreaper(true);
        assert_eq!(child_subreaper(), 1);
        fork_parent_of_orphan(false);
        assert_eq!(reap_all(), vec![3, 7]);
    });
}

#[test]
fn a_subreaper_reaps_an_orphan_that_died_before_its_parent() {
    super::det_test_fn_sequential_without_pmu(|| {
        set_child_subreaper(true);
        fork_parent_of_orphan(true);
        assert_eq!(reap_all(), vec![3, 7]);
    });
}

/// Without the flag an orphan goes to the container's init, and the guest
/// reaps only its own child. A forked child does not inherit the flag.
#[test]
fn a_cleared_subreaper_flag_sends_orphans_to_init_and_is_not_inherited() {
    super::det_test_fn_sequential_without_pmu(|| {
        set_child_subreaper(true);
        let child = unsafe { libc::fork() };
        if child == 0 {
            unsafe { libc::_exit(child_subreaper()) };
        }
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        assert_eq!(libc::WEXITSTATUS(status), 0, "the flag was inherited");

        set_child_subreaper(false);
        assert_eq!(child_subreaper(), 0);
        fork_parent_of_orphan(false);
        assert_eq!(reap_all(), vec![3]);
    });
}

/// The nearest live subreaper inherits an orphan; when that one exits without
/// reaping it, the orphan moves on to the next subreaper up the chain.
#[test]
fn nested_subreapers_hand_an_orphan_on_when_the_nearest_one_exits() {
    super::det_test_fn_sequential_without_pmu(|| {
        set_child_subreaper(true);
        let inner = unsafe { libc::fork() };
        if inner == 0 {
            set_child_subreaper(true);
            let child = fork_parent_of_orphan(false);
            let mut status = 0;
            let reaped = unsafe { libc::waitpid(child, &mut status, 0) } == child
                && libc::WEXITSTATUS(status) == 3;
            unsafe { libc::_exit(if reaped { 5 } else { 1 }) };
        }
        assert_eq!(reap_all(), vec![5, 7]);
    });
}

/// A subreaper that ignores SIGCHLD has its children reaped by the kernel as
/// they exit, a re-parented zombie included (`reparent_leader` drops it when
/// `do_notify_parent` reports the parent ignores the signal). Its `wait` then
/// fails with ECHILD and reaps nothing. Checked for both ways of ignoring:
/// `SIG_IGN` with a zombie orphan (asserted to be one), and `SA_NOCLDWAIT`
/// with a live one.
#[test]
fn a_subreaper_that_ignores_sigchld_reaps_no_orphan() {
    super::det_test_fn_sequential_without_pmu(|| {
        set_child_subreaper(true);
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = libc::SIG_IGN;
        assert_eq!(
            unsafe { libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()) },
            0
        );
        // The child restores the default action, so the orphan really is a
        // zombie when the child exits, and the child reports that it checked.
        let mut report = [0; 2];
        assert_eq!(unsafe { libc::pipe(report.as_mut_ptr()) }, 0);
        spawn_orphan_family(true, true, Some(report[1]));
        unsafe { libc::close(report[1]) };
        let mut byte = 0_u8;
        assert_eq!(
            unsafe { libc::read(report[0], (&mut byte as *mut u8).cast(), 1) },
            1,
            "the child never saw the orphan as a zombie"
        );
        assert_eq!(byte, b'Z');
        unsafe { libc::close(report[0]) };
        assert_eq!(reap_all(), Vec::<i32>::new());

        action.sa_sigaction = libc::SIG_DFL;
        action.sa_flags = libc::SA_NOCLDWAIT;
        assert_eq!(
            unsafe { libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()) },
            0
        );
        fork_parent_of_orphan(false);
        assert_eq!(reap_all(), Vec::<i32>::new());
    });
}

static SIGCHLD_COUNT: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static SIGCHLD_PID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static SIGCHLD_STATUS: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

extern "C" fn record_sigchld(_: libc::c_int, info: *mut libc::siginfo_t, _: *mut libc::c_void) {
    use std::sync::atomic::Ordering;
    if SIGCHLD_COUNT.fetch_add(1, Ordering::SeqCst) == 0 {
        let info = unsafe { &*info };
        SIGCHLD_PID.store(unsafe { info.si_pid() }, Ordering::SeqCst);
        SIGCHLD_STATUS.store(unsafe { info.si_status() }, Ordering::SeqCst);
    }
}

/// When a zombie orphan is re-parented, Linux notifies the subreaper from the
/// exiting parent's `exit_notify`, before the parent's own notification, and
/// the two standard signals merge into one pending SIGCHLD. So the subreaper's
/// handler runs once, with the orphan's pid and exit status.
#[test]
fn the_subreaper_gets_one_sigchld_with_the_reparented_zombies_siginfo() {
    super::det_test_fn_sequential_without_pmu(|| {
        use std::sync::atomic::Ordering;
        set_child_subreaper(true);
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = record_sigchld as *const () as usize;
        action.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
        assert_eq!(
            unsafe { libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()) },
            0
        );
        let child = fork_parent_of_orphan(true);
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        let orphan = unsafe { libc::wait(&mut status) };
        assert!(orphan > 0 && orphan != child);
        assert_eq!(libc::WEXITSTATUS(status), 7);
        assert_eq!(SIGCHLD_COUNT.load(Ordering::SeqCst), 1);
        assert_eq!(SIGCHLD_PID.load(Ordering::SeqCst), orphan);
        assert_eq!(SIGCHLD_STATUS.load(Ordering::SeqCst), 7);
    });
}

/// Linux treats any nonzero argument as "on".
#[test]
fn a_nonzero_argument_other_than_one_sets_the_flag() {
    super::det_test_fn_sequential_without_pmu(|| {
        assert_eq!(
            unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 2 as libc::c_ulong) },
            0
        );
        assert_eq!(child_subreaper(), 1);
        fork_parent_of_orphan(true);
        assert_eq!(reap_all(), vec![3, 7]);
    });
}

/// The subreaper R is two levels above the process M that dies: R → A → M →
/// G. G is already a zombie when M exits, and A, M's parent, stays alive. So
/// nothing but the re-parent itself can wake R's wait for G: M's own exit
/// notifies A, not R. R reaps G while A still lives, then lets A exit and
/// reaps it.
#[test]
fn a_zombie_orphan_wakes_a_subreaper_that_was_not_its_parents_parent() {
    super::det_test_fn_sequential_without_pmu(|| {
        set_child_subreaper(true);
        let mut release = [0; 2];
        assert_eq!(unsafe { libc::pipe(release.as_mut_ptr()) }, 0);
        let a = unsafe { libc::fork() };
        if a == 0 {
            unsafe { libc::close(release[1]) };
            let m = fork_parent_of_orphan(true);
            let mut status = 0;
            let reaped_m =
                unsafe { libc::waitpid(m, &mut status, 0) } == m && libc::WEXITSTATUS(status) == 3;
            let mut byte = 0_u8;
            let released = unsafe { libc::read(release[0], (&mut byte as *mut u8).cast(), 1) } == 1;
            unsafe { libc::_exit(if reaped_m && released { 5 } else { 1 }) };
        }
        unsafe { libc::close(release[0]) };
        let mut status = 0;
        let first = unsafe { libc::wait(&mut status) };
        assert!(first > 0 && first != a, "the orphan, not A, comes first");
        assert_eq!(libc::WEXITSTATUS(status), 7);
        assert_eq!(
            unsafe { libc::write(release[1], (&0_u8 as *const u8).cast(), 1) },
            1
        );
        assert_eq!(unsafe { libc::waitpid(a, &mut status, 0) }, a);
        assert_eq!(libc::WEXITSTATUS(status), 5);
        assert_eq!(reap_all(), Vec::<i32>::new());
    });
}

/// Fork B, which forks G and exits with status 3; G waits on a pipe and then
/// exits with status 7, so it outlives B as an orphan. Returns B, G and the
/// pipe's write end, which releases G.
fn fork_held_orphan() -> (libc::pid_t, libc::pid_t, libc::c_int) {
    let mut names = [0; 2];
    let mut release = [0; 2];
    assert_eq!(unsafe { libc::pipe(names.as_mut_ptr()) }, 0);
    assert_eq!(unsafe { libc::pipe(release.as_mut_ptr()) }, 0);
    let b = unsafe { libc::fork() };
    assert!(b >= 0);
    if b == 0 {
        unsafe { libc::close(names[0]) };
        let g = unsafe { libc::fork() };
        if g == 0 {
            unsafe { libc::close(release[1]) };
            let mut byte = 0_u8;
            let released = unsafe { libc::read(release[0], (&mut byte as *mut u8).cast(), 1) } == 1;
            unsafe { libc::_exit(if released { 7 } else { 14 }) };
        }
        let size = std::mem::size_of::<libc::pid_t>();
        let sent = unsafe { libc::write(names[1], (&g as *const libc::pid_t).cast(), size) };
        unsafe { libc::_exit(if sent == size as isize { 3 } else { 15 }) };
    }
    unsafe {
        libc::close(names[1]);
        libc::close(release[0]);
    }
    let mut g: libc::pid_t = 0;
    let size = std::mem::size_of::<libc::pid_t>();
    assert_eq!(
        unsafe { libc::read(names[0], (&mut g as *mut libc::pid_t).cast(), size) },
        size as isize
    );
    unsafe { libc::close(names[0]) };
    (b, g, release[1])
}

/// Run `prctl(PR_SET_CHILD_SUBREAPER, on)` in a raw clone child with exit
/// signal SIGUSR1, and reap it with `__WCLONE`. The ptrace backend reports
/// such a child with its creator's pid, but Detcore registers it as its own
/// process.
fn set_flag_in_raw_clone_child(on: bool) {
    let child =
        unsafe { libc::syscall(libc::SYS_clone, libc::SIGUSR1 as libc::c_ulong, 0, 0, 0, 0) };
    assert!(child >= 0, "raw clone failed");
    if child == 0 {
        let rc = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, libc::c_ulong::from(on)) };
        unsafe { libc::_exit(if rc == 0 { 0 } else { 12 }) };
    }
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(child as libc::pid_t, &mut status, libc::__WCLONE) },
        child as libc::pid_t
    );
    assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
}

/// A raw clone child's setter changes only its own flag, in the kernel and in
/// Detcore's record alike. Setting it in the child does not make the creator a
/// subreaper: the creator adopts no orphan. Clearing it in a child does not
/// clear the creator's own flag: the creator still adopts.
#[test]
fn a_raw_clone_childs_setter_does_not_change_its_creators_flag() {
    super::det_test_fn_sequential_without_pmu(|| {
        let mut ignore: libc::sigaction = unsafe { std::mem::zeroed() };
        ignore.sa_sigaction = libc::SIG_IGN;
        assert_eq!(
            unsafe { libc::sigaction(libc::SIGUSR1, &ignore, std::ptr::null_mut()) },
            0
        );

        set_flag_in_raw_clone_child(true);
        assert_eq!(child_subreaper(), 0);
        let (b, g, release) = fork_held_orphan();
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(b, &mut status, 0) }, b);
        assert_eq!(libc::WEXITSTATUS(status), 3);
        assert_eq!(
            unsafe { libc::waitpid(g, std::ptr::null_mut(), libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
        assert_eq!(
            unsafe { libc::write(release, (&b'x' as *const u8).cast(), 1) },
            1
        );
        unsafe { libc::close(release) };

        set_child_subreaper(true);
        set_flag_in_raw_clone_child(false);
        assert_eq!(child_subreaper(), 1);
        let (b, g, release) = fork_held_orphan();
        assert_eq!(unsafe { libc::waitpid(b, &mut status, 0) }, b);
        assert_eq!(libc::WEXITSTATUS(status), 3);
        assert_eq!(
            unsafe { libc::waitpid(g, std::ptr::null_mut(), libc::WNOHANG) },
            0
        );
        assert_eq!(
            unsafe { libc::write(release, (&b'x' as *const u8).cast(), 1) },
            1
        );
        unsafe { libc::close(release) };
        assert_eq!(unsafe { libc::waitpid(g, &mut status, 0) }, g);
        assert_eq!(libc::WEXITSTATUS(status), 7);
    });
}

static NS_INIT: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static NS_SIGCHLDS: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static NS_UNEXPECTED: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

extern "C" fn count_namespace_sigchld(
    _: libc::c_int,
    info: *mut libc::siginfo_t,
    _: *mut libc::c_void,
) {
    use std::sync::atomic::Ordering;
    NS_SIGCHLDS.fetch_add(1, Ordering::SeqCst);
    if unsafe { (*info).si_pid() } != NS_INIT.load(Ordering::SeqCst) {
        NS_UNEXPECTED.fetch_add(1, Ordering::SeqCst);
    }
}

/// Linux's `find_new_reaper` stops at a PID namespace boundary: an orphan
/// inside a namespace whose init N still lives goes to N, never to a
/// subreaper outside. Subreaper R creates N with `CLONE_NEWPID`; inside, P
/// creates G and exits, and G sees its parent become N (pid 1) and exits. R
/// must get no SIGCHLD before it releases N, and exactly one, from N, after.
#[test]
fn an_outer_subreaper_adopts_nothing_from_a_pid_namespace_with_a_live_init() {
    super::det_test_fn_sequential_without_pmu(|| {
        use std::sync::atomic::Ordering;
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = count_namespace_sigchld as *const () as usize;
        action.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
        assert_eq!(
            unsafe { libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()) },
            0
        );
        set_child_subreaper(true);
        let mut control = [0; 2];
        let mut ready = [0; 2];
        assert_eq!(unsafe { libc::pipe(control.as_mut_ptr()) }, 0);
        assert_eq!(unsafe { libc::pipe(ready.as_mut_ptr()) }, 0);
        let flags = (libc::CLONE_NEWUSER | libc::CLONE_NEWPID | libc::SIGCHLD) as libc::c_ulong;
        let n = unsafe { libc::syscall(libc::SYS_clone, flags, 0, 0, 0, 0) };
        assert!(n >= 0, "clone(CLONE_NEWUSER | CLONE_NEWPID) failed");
        let read_byte = |fd: libc::c_int| {
            let mut byte = 0_u8;
            (unsafe { libc::read(fd, (&mut byte as *mut u8).cast(), 1) } == 1).then_some(byte)
        };
        let write_byte = |fd: libc::c_int, byte: u8| unsafe {
            libc::write(fd, (&byte as *const u8).cast(), 1) == 1
        };
        if n == 0 {
            unsafe {
                libc::close(control[1]);
                libc::close(ready[0]);
            }
            if read_byte(control[0]) != Some(b's') {
                unsafe { libc::_exit(92) };
            }
            let p = unsafe { libc::fork() };
            if p == 0 {
                let g = unsafe { libc::fork() };
                if g == 0 {
                    let mut attempts = 0;
                    while unsafe { libc::getppid() } != 1 && attempts < 100_000 {
                        unsafe { libc::sched_yield() };
                        attempts += 1;
                    }
                    let parent_is_init = unsafe { libc::getppid() } == 1;
                    write_byte(ready[1], if parent_is_init { b'G' } else { b'E' });
                    unsafe { libc::_exit(7) };
                }
                unsafe { libc::_exit(3) };
            }
            if read_byte(control[0]) != Some(b'r') {
                unsafe { libc::_exit(97) };
            }
            unsafe { libc::_exit(9) };
        }
        NS_INIT.store(n as i32, Ordering::SeqCst);
        unsafe {
            libc::close(control[0]);
            libc::close(ready[1]);
        }
        assert!(write_byte(control[1], b's'));
        assert_eq!(
            read_byte(ready[0]),
            Some(b'G'),
            "G did not see N as its parent"
        );
        let mut delay = libc::timespec {
            tv_sec: 0,
            tv_nsec: 200_000_000,
        };
        while unsafe { libc::nanosleep(&delay, &mut delay) } != 0 {}
        assert_eq!(
            NS_SIGCHLDS.load(Ordering::SeqCst),
            0,
            "R was notified while N lived"
        );
        assert!(write_byte(control[1], b'r'));
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(n as libc::pid_t, &mut status, 0) },
            n as libc::pid_t
        );
        assert_eq!(libc::WEXITSTATUS(status), 9);
        assert_eq!(NS_SIGCHLDS.load(Ordering::SeqCst), 1);
        assert_eq!(NS_UNEXPECTED.load(Ordering::SeqCst), 0);
    });
}

/// A process created by a raw clone with exit signal SIGUSR1 forks G and
/// exits. The ptrace backend reports that exit with the creator's pid, but
/// the process is its own in Detcore's record, so its exit re-parents G to the
/// subreaper, as Linux does: the subreaper waits for G and reaps status 7.
#[test]
fn a_raw_clone_processs_orphan_goes_to_the_subreaper() {
    super::det_test_fn_sequential_without_pmu(|| {
        let mut ignore: libc::sigaction = unsafe { std::mem::zeroed() };
        ignore.sa_sigaction = libc::SIG_IGN;
        assert_eq!(
            unsafe { libc::sigaction(libc::SIGUSR1, &ignore, std::ptr::null_mut()) },
            0
        );
        set_child_subreaper(true);
        let mut names = [0; 2];
        let mut release = [0; 2];
        assert_eq!(unsafe { libc::pipe(names.as_mut_ptr()) }, 0);
        assert_eq!(unsafe { libc::pipe(release.as_mut_ptr()) }, 0);
        let c =
            unsafe { libc::syscall(libc::SYS_clone, libc::SIGUSR1 as libc::c_ulong, 0, 0, 0, 0) };
        assert!(c >= 0, "raw clone failed");
        if c == 0 {
            unsafe { libc::close(names[0]) };
            let g = unsafe { libc::fork() };
            if g == 0 {
                unsafe { libc::close(release[1]) };
                let mut byte = 0_u8;
                let released =
                    unsafe { libc::read(release[0], (&mut byte as *mut u8).cast(), 1) } == 1;
                unsafe { libc::_exit(if released { 7 } else { 14 }) };
            }
            let size = std::mem::size_of::<libc::pid_t>();
            let sent = unsafe { libc::write(names[1], (&g as *const libc::pid_t).cast(), size) };
            unsafe { libc::_exit(if sent == size as isize { 0 } else { 15 }) };
        }
        unsafe {
            libc::close(names[1]);
            libc::close(release[0]);
        }
        let mut g: libc::pid_t = 0;
        let size = std::mem::size_of::<libc::pid_t>();
        assert_eq!(
            unsafe { libc::read(names[0], (&mut g as *mut libc::pid_t).cast(), size) },
            size as isize
        );
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(c as libc::pid_t, &mut status, libc::__WCLONE) },
            c as libc::pid_t
        );
        assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
        assert_eq!(
            unsafe { libc::waitpid(g, std::ptr::null_mut(), libc::WNOHANG) },
            0,
            "G is the subreaper's live child"
        );
        assert_eq!(
            unsafe { libc::write(release[1], (&b'x' as *const u8).cast(), 1) },
            1
        );
        unsafe { libc::close(release[1]) };
        assert_eq!(unsafe { libc::waitpid(g, &mut status, 0) }, g);
        assert_eq!(libc::WEXITSTATUS(status), 7);
    });
}
