//! The existing closed-copy policy used by the real provenance-checked native entry.
//! No Tool, registry, lifecycle admission or scheduler is constructed here.

use reverie::syscalls::Sysno;

fn copied_child_flock_action(operation: Option<i32>) -> i32 {
    let Some(operation) = operation else {
        return -libc::ENOLCK;
    };
    let known_flags = libc::LOCK_SH | libc::LOCK_EX | libc::LOCK_UN | libc::LOCK_NB;
    let base = operation & !libc::LOCK_NB;
    let valid = operation & !known_flags == 0
        && matches!(base, libc::LOCK_SH | libc::LOCK_EX | libc::LOCK_UN);
    if !valid || base == libc::LOCK_UN || operation & libc::LOCK_NB != 0 {
        0
    } else {
        -libc::ENOLCK
    }
}

/// Whether a copied child's `fcntl` with these arguments sets or clears an OFD
/// lock (`F_OFD_SETLK`, `F_OFD_SETLKW`, whatever the lock type). Missing
/// arguments count as a change, so the strict refusal fails closed.
fn copied_child_fcntl_changes_ofd_lock(args: *const u64) -> bool {
    if args.is_null() {
        return true;
    }
    // SAFETY: Reverie's DYNAMORIO client passes its live six-element syscall
    // argument array for the duration of this callback.
    let command = unsafe { args.add(1).read() } as i32;
    matches!(command, libc::F_OFD_SETLK | libc::F_OFD_SETLKW)
}

/// Whether `sysno` reads the clock or a resource limit, which the DYNAMORIO client no
/// longer answers from a table of its own (Reverie A8 step 2) and which a copied
/// child cannot take to Detcore. A relative `clock_nanosleep` on a wall or
/// monotonic clock only waits and reads nothing; the client's table passed it
/// to the kernel too, so it is not counted here.
fn copied_child_reads_clock_or_limit(sysno: Sysno, args: *const u64) -> bool {
    match sysno {
        Sysno::clock_gettime
        | Sysno::clock_getres
        | Sysno::clock_settime
        | Sysno::gettimeofday
        | Sysno::time
        | Sysno::getrlimit
        | Sysno::setrlimit
        | Sysno::prlimit64 => true,
        Sysno::clock_nanosleep => {
            if args.is_null() {
                return true;
            }
            // SAFETY: Reverie's DYNAMORIO client passes its live six-element syscall
            // argument array for the duration of this callback.
            let (clock, flags) = unsafe { (args.read() as i32, args.add(1).read() as i32) };
            let cpu_clock = clock < 0
                || clock == libc::CLOCK_PROCESS_CPUTIME_ID
                || clock == libc::CLOCK_THREAD_CPUTIME_ID;
            flags != 0 || cpu_clock
        }
        _ => false,
    }
}

pub(super) unsafe fn copied_action(
    sysnum: i64,
    args: *const u64,
    strict: bool,
    report_fd: i32,
    emit: fn(&[u8]),
) -> i32 {
    let sysno = Sysno::from(sysnum as i32);
    // Bash probes the foreground process group in a copied child before
    // running a background shell function. Hermit's captured stderr is not a
    // terminal, so the instrumented root observes ENOTTY for the same request.
    // Emulate that result instead of either exposing a host terminal's process
    // group or aborting an otherwise deterministic child.
    //
    // Every other ioctl remains fail-closed: copied children still cannot enter
    // the Rust Detcore Tool, and the ABI has no guest-memory channel for safely
    // handling socket timestamps or arbitrary device operations.
    if sysno == Sysno::ioctl && strict {
        let request = if args.is_null() {
            None
        } else {
            // SAFETY: Reverie's DYNAMORIO client passes its live six-element syscall
            // argument array for the duration of this callback.
            Some(unsafe { args.add(1).read() })
        };
        if request == Some(libc::TIOCGPGRP) {
            return -libc::ENOTTY;
        }
        return 1;
    }
    // TODO-HUMAN-REVIEW(PR-981): Copied DYNAMORIO children cannot enter the Rust
    // Detcore Tool. Strict mode therefore fails closed for receive syscalls that
    // can expose native socket timestamps. Non-strict mode retained native
    // behavior until the descriptor-import arm below, which now answers
    // recvmsg and recvmmsg with EPERM there.
    // TODO-HUMAN-REVIEW(PR-972): readlink identity canonicalization also requires
    // Detcore mediation. This ABI has neither syscall arguments nor a memory
    // writer, so a copied child must fail closed rather than expose native
    // pipe/socket inode identities.
    // A copied pre-exec child has no Detcore Tool. Preserve kernel handling
    // for malformed operations, nonblocking locks, and unlock; only blocking
    // acquisitions can strand the child behind a parent that is waiting for it.
    if sysno == Sysno::flock {
        let operation = (!args.is_null()).then(|| unsafe { args.add(1).read() as i32 });
        return copied_child_flock_action(operation);
    }
    // pidfd_getfd NEVER RUNS NATIVELY IN A COPIED CHILD because it is the one native
    // route by which a copied child can reach an open file description that
    // ANOTHER guest process holds a Detcore claim about. A sibling that took
    // LOCK_SH through Detcore holds a known mode for its description; a
    // copied child that duplicates that description with pidfd_getfd and
    // runs LOCK_UN natively (the flock arm above lets unlock through) makes
    // the sibling's claim stale, and its next refused blocking upgrade would
    // "restore" a lock it no longer holds. The instrumented root mediates
    // pidfd_getfd (self only); a copied child cannot enter that handler.
    // Evidence capture refuses this call too, but only in verify runs. The
    // protection does not depend on the unsupported-syscall policy: a stale
    // claim corrupts flock semantics (a "restored" lock the guest released),
    // not only determinism, and before copied children could be admitted
    // after a fork the up-front vfork guard, which is not policy-gated,
    // refused this shape under --allow-unsupported-syscalls too. Only the
    // form of the refusal follows the policy: the default fail-closed policy
    // ends the run by name; the --allow-unsupported-syscalls opt-out
    // answers EPERM without executing the call (the errno Linux gives when
    // ptrace access to the target is denied), as the root answers an
    // unserviceable call with an errno under that opt-out. Ending the run
    // there is not an option: a copied child's fail-closed exit under the
    // opt-out left the run hung (measured: the vfork parent stayed in
    // kernel_clone and the tree never ended). SCM_RIGHTS reception, the
    // other import route, is refused by the recvmsg arm below the same way.
    if sysno == Sysno::pidfd_getfd {
        emit(b"detcore-dynamorio: refusing pidfd_getfd in copied child: a native duplicate of another process's open file description could change a lock that process's Detcore state holds a claim about\n");
        return if strict { 1 } else { -libc::EPERM };
    }
    // An OFD lock change NEVER RUNS NATIVELY IN A COPIED CHILD because the
    // `/proc/locks` ledger in Detcore's global state records which OFD locks
    // each description holds, and a copied child's native F_OFD_SETLK or
    // F_OFD_SETLKW (an unlock included) changes the kernel's lock without
    // updating that record. An OFD row prints owner -1 in every PID
    // namespace, and the ledger keeps such a row by field match, so a stale
    // entry admits a host process's identical lock into the guest's
    // /proc/locks. The parent's up-front OFD vfork guard cannot see a lock a
    // sibling took through its own split copy of the description's model, so
    // the refusal belongs at the call. F_OFD_GETLK changes nothing and stays
    // native, as in the root, where it also sees host locks. POSIX
    // F_SETLK/F_SETLKW stay native: POSIX rows are not in the ledger (it
    // keeps OFD and flock rows only) and are kept by their owner pid, and
    // Detcore caches no POSIX lock state. The protection does not depend on
    // the unsupported-syscall policy, for the reason the pidfd_getfd arm
    // gives: the up-front OFD and flock vfork guards it stands in for are not
    // policy-gated. Under the --allow-unsupported-syscalls opt-out the call
    // is answered ENOLCK without executing, as a copied child's blocking
    // flock is (`copied_child_flock_action`), so the kernel's lock and the
    // ledger's record stay in agreement.
    if sysno == Sysno::fcntl && copied_child_fcntl_changes_ofd_lock(args) {
        emit(b"detcore-dynamorio: refusing fcntl F_OFD_SETLK/F_OFD_SETLKW in copied child: a native OFD lock change would leave the /proc/locks ledger's record of that description stale\n");
        return if strict { 1 } else { -libc::ENOLCK };
    }
    // recvmsg and recvmmsg NEVER RUN NATIVELY IN A COPIED CHILD, under any
    // policy, because an SCM_RIGHTS message is the other native route by
    // which a copied child can import an open file description another guest
    // process holds a Detcore claim about. A sibling's promoted description
    // (OpenFileState::Shared) keeps its canonical known mode across the
    // sender's sendmsg; a copied child that receives it and runs LOCK_UN
    // natively makes that claim stale, as with pidfd_getfd above. Whether a
    // call carries a control buffer cannot be told here: the msghdr is guest
    // memory and this ABI has no reader (a fault in the runtime would take
    // the place of the guest's EFAULT), so every call is refused, including
    // one without a control buffer that would import nothing. Strict
    // execution already refused both for native socket timestamps. Under the
    // default fail-closed policy the run ends by name; under the
    // --allow-unsupported-syscalls opt-out the call answers EPERM without
    // executing (a security module can refuse a receive with it), for the
    // reason the pidfd_getfd arm gives.
    if matches!(sysno, Sysno::recvmsg | Sysno::recvmmsg) {
        emit(b"detcore-dynamorio: refusing recvmsg/recvmmsg in copied child: an SCM_RIGHTS message could import an open file description another process's Detcore state holds a claim about\n");
        return if strict { 1 } else { -libc::EPERM };
    }
    if matches!(sysno, Sysno::readlink | Sysno::readlinkat) && strict {
        return 1;
    }
    // setpgid FAILS CLOSED IN STRICT MODE BECAUSE THE COPIED CHILD CAN PERFORM
    // IT BUT CANNOT RECORD IT.
    //
    // `handle_setpgid` in the instrumented root does two things: it injects the
    // call, so the KERNEL really moves the process group, and then it mirrors
    // that move into Detcore's model with `set_process_group`, "so
    // group-selecting waits do not consult host /proc state". A copied child has
    // no Detcore Tool, so it performs the first half and silently skips the
    // second: the kernel's process groups and Detcore's model diverge, and the
    // next `wait` with a negative pgid selects against a model that is now
    // wrong. That is the machinery hermit#bc9d178 reclassified setpgid to
    // determinize in the first place.
    //
    // ⚠️ AND A FIXED ERRNO IS THE WRONG SHAPE HERE, WHICH IS WHY THIS IS NOT
    // `-EPERM`. The ioctl arm above emulates ENOTTY because the instrumented
    // root observes ENOTTY too -- that is faithful. The root observes setpgid
    // SUCCEEDING, so no errno we return is faithful, and the choice is between
    // two divergences. EPERM is the quiet one: setpgid's return is widely
    // ignored, so a child would carry on believing it leads a process group it
    // does not lead. Failing closed is the loud one, and a wrong answer about
    // process-group identity is worse than no answer.
    //
    // Strict-only, matching the receive/readlink arms directly above: non-strict
    // copied children keep native behaviour, and only strict execution -- which
    // is where determinism is actually claimed -- refuses.
    if sysno == Sysno::setpgid && strict {
        return 1;
    }
    // getpgrp, getpgid and getsid FAIL CLOSED IN STRICT MODE BECAUSE THE
    // KERNEL'S ANSWER IS NOT DETCORE'S.
    //
    // The instrumented root is answered from Detcore's model (slice B3a-4c of
    // https://github.com/rrnewton/hermit/issues/4266): a group or session
    // inherited from outside the container's PID namespace reads 0, as Linux
    // shows it. In a copied child the call would run natively, and the kernel
    // answers for the process group the DYNAMORIO launcher made for the run, whose
    // id is the root's pid. A child would read a different group from its
    // parent's. As with setpgid above, no fixed errno is faithful (the root's
    // calls succeed), and a wrong answer about process-group identity is
    // worse than no answer.
    if matches!(sysno, Sysno::getpgrp | Sysno::getpgid | Sysno::getsid) && strict {
        return 1;
    }
    // Detcore answers ordinary scheduling-policy queries with SCHED_OTHER.
    // A copied child cannot enter the Tool and may inherit a different native
    // policy. Refuse that strict escape without inventing a second answer.
    if sysno == Sysno::sched_getscheduler && strict {
        // The emitter is installed before READY and inherited by copied
        // children. Use its raw write path, without an allocating/locking
        // stderr fallback on the copied runtime stack.
        emit(b"detcore-dynamorio: unsupported syscall sched_getscheduler (145) in copied child\n");
        return 1;
    }
    // Ordinary scheduler parameters are written by Detcore's shared Tool.
    // A copied child cannot enter that Tool or write its modeled output through
    // this ABI. Refuse the strict escape without a second parameter policy.
    if sysno == Sysno::sched_getparam && strict {
        emit(b"detcore-dynamorio: unsupported syscall sched_getparam (143) in copied child\n");
        return 1;
    }
    // Detcore's shared Tool owns affinity masks. A copied child cannot
    // enter that Tool to write its modeled CPU set; refuse the strict native
    // escape without installing another affinity policy here.
    if sysno == Sysno::sched_getaffinity && strict {
        emit(b"detcore-dynamorio: unsupported syscall sched_getaffinity (204) in copied child\n");
        return 1;
    }
    // Ordinary priority queries belong to Detcore's shared policy. A copied
    // child cannot enter that Tool, so strict execution must refuse the native
    // answer rather than install a second priority model here.
    if sysno == Sysno::getpriority && strict {
        emit(b"detcore-dynamorio: unsupported syscall getpriority (140) in copied child\n");
        return 1;
    }
    // A copied child's clock and resource-limit reads have no deterministic
    // answer here: the DYNAMORIO client keeps no table of its own any more, and
    // Detcore, which answers them in the root and in forked children, cannot
    // be entered from a copied child. Strict mode refuses them by name rather
    // than let a vfork child read host time or host limits; non-strict keeps
    // native behaviour, as the arms above do.
    if strict && copied_child_reads_clock_or_limit(sysno, args) {
        return 1;
    }
    if detcore::is_deterministically_refused_syscall(sysno)
        && (strict || !detcore::is_strict_only_deterministic_refusal_syscall(sysno))
    {
        return 1;
    }
    if !detcore::is_unsupported_syscall(sysno) {
        return 0;
    }
    if strict {
        1
    } else {
        append_copied_syscall_record(sysnum, report_fd);
        0
    }
}

fn append_copied_syscall_record(sysnum: i64, report_fd: i32) {
    if report_fd == -1 {
        return;
    }
    let mut buffer = [0_u8; 24];
    let mut index = buffer.len() - 1;
    buffer[index] = b'\n';
    let mut value = sysnum as u64;
    loop {
        index -= 1;
        buffer[index] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    index -= 1;
    buffer[index] = b'@';
    let _ = unsafe {
        libc::write(
            report_fd,
            buffer[index..].as_ptr().cast(),
            buffer.len() - index,
        )
    };
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    use super::*;

    // Test inputs for the same private gate; production obtains these only
    // from the admitted bootstrap policy. No callback/ready/Tool is fabricated.
    static COPIED_PANIC_ON_UNSUPPORTED: AtomicBool = AtomicBool::new(true);
    static COPIED_CHILD_POLICY_TEST_LOCK: Mutex<()> = Mutex::new(());
    static DIAGNOSTICS: Mutex<Vec<u8>> = Mutex::new(Vec::new());

    fn capture_diagnostic(bytes: &[u8]) {
        DIAGNOSTICS.lock().unwrap().extend_from_slice(bytes);
    }

    unsafe fn copied_gate_for_test(sysnum: i64, args: *const u64) -> i32 {
        unsafe {
            copied_action(
                sysnum,
                args,
                COPIED_PANIC_ON_UNSUPPORTED.load(Ordering::Acquire),
                -1,
                capture_diagnostic,
            )
        }
    }

    fn copied_child_action(sysnum: i64) -> i32 {
        copied_child_action_with_args(sysnum, [0; 6])
    }

    fn copied_child_action_with_args(sysnum: i64, args: [u64; 6]) -> i32 {
        // SAFETY: The real gate reads this live six-word argument array only
        // for this synchronous invocation, exactly as the native owner does.
        unsafe { copied_gate_for_test(sysnum, args.as_ptr()) }
    }

    const ACKNOWLEDGED_STRICT_COPIED_CHILD_ESCAPES: &[&str] = &[
        "accept",
        "accept4",
        "adjtimex",
        "alarm",
        "arch_prctl",
        "bind",
        "chown",
        "clock_adjtime",
        "clock_nanosleep",
        "clone",
        "clone3",
        "close",
        "close_range",
        "connect",
        "creat",
        "dup",
        "dup2",
        "dup3",
        "epoll_create",
        "epoll_create1",
        "epoll_ctl",
        "epoll_pwait",
        "epoll_pwait2",
        "epoll_wait",
        "epoll_wait_old",
        "eventfd",
        "eventfd2",
        "execve",
        "execveat",
        "exit",
        "exit_group",
        "fadvise64",
        "fchown",
        "fchownat",
        "fcntl",
        "flock",
        "fork",
        "fstat",
        "fstatfs",
        "futex",
        "get_mempolicy",
        "getcpu",
        "getdents",
        "getdents64",
        "getegid",
        "geteuid",
        "getgid",
        "getitimer",
        "getpeername",
        "getpid",
        "getrandom",
        "getresgid",
        "getresuid",
        "getrusage",
        "getsockname",
        "getsockopt",
        "gettid",
        "getuid",
        "inotify_add_watch",
        "inotify_init",
        "inotify_init1",
        "inotify_rm_watch",
        "ioprio_get",
        "ioprio_set",
        "kill",
        "lchown",
        "listen",
        "lseek",
        "lstat",
        "madvise",
        "mbind",
        "membarrier",
        "memfd_create",
        "migrate_pages",
        "mincore",
        "mmap",
        "move_pages",
        "mremap",
        "munmap",
        "nanosleep",
        "newfstatat",
        "open",
        "openat",
        "pause",
        "pidfd_open",
        "pidfd_send_signal",
        "pipe",
        "pipe2",
        "poll",
        "ppoll",
        "prctl",
        "pread64",
        "preadv",
        "preadv2",
        "process_madvise",
        "pselect6",
        "pwrite64",
        "pwritev",
        "pwritev2",
        "read",
        "readv",
        "recvfrom",
        "rt_sigaction",
        "rt_sigpending",
        "rt_sigprocmask",
        "rt_sigqueueinfo",
        "rt_sigsuspend",
        "rt_sigtimedwait",
        "rt_tgsigqueueinfo",
        "sched_getattr",
        "sched_rr_get_interval",
        "sched_setaffinity",
        "sched_setattr",
        "sched_setparam",
        "sched_setscheduler",
        "sched_yield",
        "seccomp",
        "select",
        "sendfile",
        "sendmmsg",
        "sendmsg",
        "sendto",
        "set_mempolicy",
        "set_mempolicy_home_node",
        "setfsgid",
        "setfsuid",
        "setgid",
        "setgroups",
        "setitimer",
        "setpriority",
        "setregid",
        "setresgid",
        "setresuid",
        "setreuid",
        "setsid",
        "setsockopt",
        "setuid",
        "shutdown",
        "signalfd",
        "signalfd4",
        "socket",
        "socketpair",
        "stat",
        "statfs",
        "statx",
        "sysinfo",
        "syslog",
        "tgkill",
        "timer_create",
        "timer_delete",
        "timer_getoverrun",
        "timer_gettime",
        "timer_settime",
        "timerfd_create",
        "timerfd_gettime",
        "timerfd_settime",
        "times",
        "tkill",
        "uname",
        "userfaultfd",
        "utime",
        "utimensat",
        "utimes",
        "vfork",
        "wait4",
        "waitid",
        "write",
        "writev",
    ];

    #[test]
    fn copied_child_disposition_of_the_covered_reviewer_findings() {
        let _guard = COPIED_CHILD_POLICY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let saved = COPIED_PANIC_ON_UNSUPPORTED.load(Ordering::Acquire);
        COPIED_PANIC_ON_UNSUPPORTED.store(true, Ordering::Release);

        // Refused before reaching the host.
        for sysno in [Sysno::perf_event_open, Sysno::remap_file_pages] {
            assert!(detcore::is_determinized_syscall(sysno));
            assert_eq!(
                copied_child_action(sysno.id() as i64),
                1,
                "{sysno} must not reach the host from a strict copied child"
            );
        }

        // Still diverging: emulated on the ptrace path, native here.
        for sysno in [Sysno::ioprio_get, Sysno::ioprio_set] {
            assert!(detcore::is_determinized_syscall(sysno));
            assert!(!detcore::is_deterministically_refused_syscall(sysno));
            assert_eq!(
                copied_child_action(sysno.id() as i64),
                0,
                "{sysno} disposition changed; update this test and issue #1793"
            );
        }

        COPIED_PANIC_ON_UNSUPPORTED.store(saved, Ordering::Release);
    }

    #[test]
    fn copied_child_refuses_group_and_session_getters_in_strict_mode() {
        let _guard = COPIED_CHILD_POLICY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let saved = COPIED_PANIC_ON_UNSUPPORTED.load(Ordering::Acquire);
        for (strict, expected) in [(true, 1), (false, 0)] {
            COPIED_PANIC_ON_UNSUPPORTED.store(strict, Ordering::Release);
            for sysno in [Sysno::getpgrp, Sysno::getpgid, Sysno::getsid] {
                assert!(detcore::is_determinized_syscall(sysno));
                assert_eq!(
                    copied_child_action(sysno.id() as i64),
                    expected,
                    "{sysno} in a copied child, strict={strict}"
                );
            }
        }
        COPIED_PANIC_ON_UNSUPPORTED.store(saved, Ordering::Release);
    }

    #[test]
    fn copied_child_refuses_scheduler_policy_queries_only_in_strict_mode() {
        let _guard = COPIED_CHILD_POLICY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let saved = COPIED_PANIC_ON_UNSUPPORTED.load(Ordering::Acquire);
        assert!(detcore::is_determinized_syscall(Sysno::sched_getscheduler));
        for (strict, expected) in [(true, 1), (false, 0)] {
            COPIED_PANIC_ON_UNSUPPORTED.store(strict, Ordering::Release);
            assert_eq!(
                copied_child_action(Sysno::sched_getscheduler.id() as i64),
                expected,
                "sched_getscheduler in a copied child, strict={strict}"
            );
        }
        COPIED_PANIC_ON_UNSUPPORTED.store(saved, Ordering::Release);
    }

    #[test]
    fn copied_child_refuses_scheduler_parameter_queries_only_in_strict_mode() {
        let _guard = COPIED_CHILD_POLICY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let saved = COPIED_PANIC_ON_UNSUPPORTED.load(Ordering::Acquire);
        assert!(detcore::is_determinized_syscall(Sysno::sched_getparam));
        for (strict, expected) in [(true, 1), (false, 0)] {
            COPIED_PANIC_ON_UNSUPPORTED.store(strict, Ordering::Release);
            assert_eq!(
                copied_child_action(Sysno::sched_getparam.id() as i64),
                expected,
                "sched_getparam in a copied child, strict={strict}"
            );
        }
        COPIED_PANIC_ON_UNSUPPORTED.store(saved, Ordering::Release);
    }

    #[test]
    fn copied_child_refuses_affinity_queries_only_in_strict_mode() {
        let _guard = COPIED_CHILD_POLICY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let saved = COPIED_PANIC_ON_UNSUPPORTED.load(Ordering::Acquire);
        assert!(detcore::is_determinized_syscall(Sysno::sched_getaffinity));
        for (strict, expected) in [(true, 1), (false, 0)] {
            COPIED_PANIC_ON_UNSUPPORTED.store(strict, Ordering::Release);
            assert_eq!(
                copied_child_action(Sysno::sched_getaffinity.id() as i64),
                expected,
                "sched_getaffinity in a copied child, strict={strict}"
            );
        }
        COPIED_PANIC_ON_UNSUPPORTED.store(saved, Ordering::Release);
    }

    #[test]
    fn copied_child_refuses_priority_queries_only_in_strict_mode() {
        let _guard = COPIED_CHILD_POLICY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let saved = COPIED_PANIC_ON_UNSUPPORTED.load(Ordering::Acquire);
        assert!(detcore::is_determinized_syscall(Sysno::getpriority));
        for (strict, expected) in [(true, 1), (false, 0)] {
            COPIED_PANIC_ON_UNSUPPORTED.store(strict, Ordering::Release);
            assert_eq!(
                copied_child_action(Sysno::getpriority.id() as i64),
                expected,
                "getpriority in a copied child, strict={strict}"
            );
        }
        COPIED_PANIC_ON_UNSUPPORTED.store(saved, Ordering::Release);
    }

    #[test]
    fn no_new_determinized_syscall_silently_escapes_the_copied_child() {
        let _guard = COPIED_CHILD_POLICY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let saved = COPIED_PANIC_ON_UNSUPPORTED.load(Ordering::Acquire);
        COPIED_PANIC_ON_UNSUPPORTED.store(true, Ordering::Release);

        let observed: Vec<String> = detcore::all_pinned_syscalls()
            .filter(|sysno| detcore::is_determinized_syscall(*sysno))
            .filter(|sysno| copied_child_action(sysno.id() as i64) == 0)
            .map(|sysno| sysno.to_string())
            .collect();

        COPIED_PANIC_ON_UNSUPPORTED.store(saved, Ordering::Release);

        let expected: Vec<String> = ACKNOWLEDGED_STRICT_COPIED_CHILD_ESCAPES
            .iter()
            .map(|name| (*name).to_string())
            .collect();
        let added: Vec<&String> = observed.iter().filter(|s| !expected.contains(s)).collect();
        let removed: Vec<&String> = expected.iter().filter(|s| !observed.contains(s)).collect();

        assert!(
            added.is_empty(),
            "these Determinized syscalls newly run NATIVELY in a strict copied child, \
             bypassing Detcore: {added:?}. Give each one a copied-child policy \
             (fixed errno or fail-closed), or add it to \
             ACKNOWLEDGED_STRICT_COPIED_CHILD_ESCAPES with a stated reason."
        );
        assert!(
            removed.is_empty(),
            "these syscalls no longer escape — remove them from \
             ACKNOWLEDGED_STRICT_COPIED_CHILD_ESCAPES so the register stays exact: {removed:?}"
        );
    }

    #[test]
    fn census_determinized_syscalls_reaching_the_copied_child_gate() {
        let _guard = COPIED_CHILD_POLICY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let saved = COPIED_PANIC_ON_UNSUPPORTED.load(Ordering::Acquire);

        for strict in [true, false] {
            COPIED_PANIC_ON_UNSUPPORTED.store(strict, Ordering::Release);
            let mut determinized = 0usize;
            let mut stopped = 0usize;
            let mut escaping: Vec<String> = Vec::new();
            for sysno in detcore::all_pinned_syscalls() {
                if !detcore::is_determinized_syscall(sysno) {
                    continue;
                }
                determinized += 1;
                if copied_child_action(sysno.id() as i64) == 0 {
                    escaping.push(sysno.to_string());
                } else {
                    stopped += 1;
                }
            }
            println!(
                "copied-child strict={strict}: {stopped}/{determinized} Determinized syscalls \
                 stopped before the host; {} run natively",
                escaping.len()
            );
            println!("  escaping: {}", escaping.join(" "));
        }

        COPIED_PANIC_ON_UNSUPPORTED.store(saved, Ordering::Release);
    }

    #[test]
    fn copied_child_refuses_keyring_syscalls_under_strict() {
        let _guard = COPIED_CHILD_POLICY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let saved = COPIED_PANIC_ON_UNSUPPORTED.load(Ordering::Acquire);

        // Strict (panic-on-unsupported): keyring syscalls are refused so the
        // copied child cannot mutate host keyrings or trigger request-key
        // upcalls. `1` tells the native client to exit the isolated runtime
        // tree (fail closed), matching the pre-848 Unsupported behavior.
        COPIED_PANIC_ON_UNSUPPORTED.store(true, Ordering::Release);
        assert_eq!(copied_child_action(libc::SYS_keyctl), 1);
        assert_eq!(copied_child_action(libc::SYS_add_key), 1);
        assert_eq!(copied_child_action(libc::SYS_request_key), 1);
        // A supported syscall still runs natively even under strict mode.
        assert_eq!(copied_child_action(libc::SYS_getpid), 0);

        // Non-strict: keyring syscalls fall through to native pass-through,
        // matching the root process's non-strict keyring behavior.
        COPIED_PANIC_ON_UNSUPPORTED.store(false, Ordering::Release);
        assert_eq!(copied_child_action(libc::SYS_keyctl), 0);
        assert_eq!(copied_child_action(libc::SYS_add_key), 0);
        assert_eq!(copied_child_action(libc::SYS_request_key), 0);

        COPIED_PANIC_ON_UNSUPPORTED.store(saved, Ordering::Release);
    }

    #[test]
    fn copied_child_gate_refuses_deterministic_refusal_families_in_strict() {
        let _guard = COPIED_CHILD_POLICY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // The copied pre-exec child runs natively with no Detcore tool. Under
        // strict mode the gate must fail-close (return 1) not only for the
        // classic Unsupported set but for the full deterministic-refusal
        // boundary (splice/tee/vmsplice, perf_event_open, the keyring family),
        // otherwise strict guests execute those syscalls natively against the
        // host. Report fd is left at its -1 default so `append_copied_syscall_record`
        // is a no-op and the non-strict branch has no observable side effect.
        let previous = COPIED_PANIC_ON_UNSUPPORTED.load(Ordering::Acquire);

        // Every family member must be recognized by the shared predicate.
        for sysno in [
            Sysno::splice,
            Sysno::tee,
            Sysno::vmsplice,
            Sysno::perf_event_open,
            Sysno::keyctl,
            Sysno::add_key,
            Sysno::request_key,
        ] {
            assert!(
                detcore::is_deterministically_refused_syscall(sysno),
                "{sysno:?} should be in Detcore's deterministic-refusal boundary"
            );
        }

        // Strict: refused families and Unsupported syscalls fail-close (1);
        // ordinary passthrough syscalls continue natively (0).
        COPIED_PANIC_ON_UNSUPPORTED.store(true, Ordering::Release);
        for sysnum in [
            libc::SYS_splice,
            libc::SYS_tee,
            libc::SYS_vmsplice,
            libc::SYS_perf_event_open,
            libc::SYS_keyctl,
            libc::SYS_add_key,
            libc::SYS_request_key,
            libc::SYS_ioctl,
            libc::SYS_recvmsg,
            libc::SYS_recvmmsg,
            libc::SYS_readlink,
            libc::SYS_readlinkat,
        ] {
            assert_eq!(
                copied_child_action(sysnum),
                1,
                "strict copied child must refuse syscall {sysnum}"
            );
        }
        let mut ioctl_args = [0; 6];
        ioctl_args[1] = libc::TIOCGPGRP;
        assert_eq!(
            copied_child_action_with_args(libc::SYS_ioctl, ioctl_args),
            -libc::ENOTTY,
            "TIOCGPGRP must receive the deterministic non-terminal result"
        );
        ioctl_args[1] = 0x8906; // SIOCGSTAMP_OLD
        assert_eq!(
            copied_child_action_with_args(libc::SYS_ioctl, ioctl_args),
            1,
            "socket timestamp ioctls must remain fail-closed"
        );
        // SAFETY: A null argument vector is an explicit fail-closed ABI test;
        // the callback checks it before dereferencing.
        assert_eq!(
            unsafe { copied_gate_for_test(libc::SYS_ioctl, std::ptr::null()) },
            1,
            "missing ioctl arguments must fail closed"
        );
        for sysnum in [libc::SYS_read, libc::SYS_write, libc::SYS_getpid] {
            assert_eq!(
                copied_child_action(sysnum),
                0,
                "strict copied child must allow ordinary syscall {sysnum}"
            );
        }

        // Non-strict: strict-only compatibility families continue natively,
        // while unconditional fixed-error families still fail closed because
        // the copied-child ABI cannot inject their deterministic errno.
        COPIED_PANIC_ON_UNSUPPORTED.store(false, Ordering::Release);
        for sysnum in [
            libc::SYS_splice,
            libc::SYS_keyctl,
            libc::SYS_ioctl,
            libc::SYS_read,
            libc::SYS_readlink,
            libc::SYS_readlinkat,
        ] {
            assert_eq!(
                copied_child_action(sysnum),
                0,
                "non-strict copied child must allow syscall {sysnum}"
            );
        }
        // recvmsg and recvmmsg never run natively in a copied child: an
        // SCM_RIGHTS message could import another process's description. Under
        // the non-strict policy the call does not run and answers EPERM.
        for sysnum in [libc::SYS_recvmsg, libc::SYS_recvmmsg] {
            assert_eq!(
                copied_child_action(sysnum),
                -libc::EPERM,
                "non-strict copied child must answer EPERM for syscall {sysnum}"
            );
        }
        for sysnum in [
            libc::SYS_perf_event_open,
            libc::SYS_openat2,
            libc::SYS_io_uring_setup,
        ] {
            assert_eq!(
                copied_child_action(sysnum),
                1,
                "unconditional refusal must fail closed for syscall {sysnum}"
            );
        }

        for operation in [libc::LOCK_SH, libc::LOCK_EX] {
            let mut args = [0; 6];
            args[1] = operation as u64;
            assert_eq!(
                copied_child_action_with_args(libc::SYS_flock, args),
                -libc::ENOLCK,
                "a copied child must not enter a blocking flock operation",
            );
        }
        for operation in [
            0,
            libc::LOCK_SH | libc::LOCK_EX,
            libc::LOCK_SH | libc::LOCK_NB,
            libc::LOCK_EX | libc::LOCK_NB,
            libc::LOCK_UN,
            libc::LOCK_UN | libc::LOCK_NB,
        ] {
            let mut args = [0; 6];
            args[1] = operation as u64;
            assert_eq!(
                copied_child_action_with_args(libc::SYS_flock, args),
                0,
                "operation {operation} must retain kernel validation or safe nonblocking behavior",
            );
        }
        assert_eq!(
            unsafe { copied_gate_for_test(libc::SYS_flock, std::ptr::null()) },
            -libc::ENOLCK,
            "missing copied-child flock arguments must fail closed",
        );

        COPIED_PANIC_ON_UNSUPPORTED.store(previous, Ordering::Release);
    }
}
