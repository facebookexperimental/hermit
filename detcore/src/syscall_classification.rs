/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use reverie::syscalls::Sysno;

const EXPECTED_X86_64_SYSNO_COUNT: usize = 373;

// `Sysno` is externally `#[non_exhaustive]`. These assertions make additions,
// removals, or a changed table endpoint fail at compile time instead of silently
// reaching the required final arm.
const _: () = {
    assert!(Sysno::count() == EXPECTED_X86_64_SYSNO_COUNT);
    assert!(Sysno::last().id() == 461);
};

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
/// Detcore's execution policy for a named Linux syscall.
pub(crate) enum SyscallClassification {
    /// Detcore models the syscall or applies an explicit deterministic refusal policy.
    Determinized,
    /// The syscall is intentionally forwarded under documented container assumptions.
    PassThrough,
    /// The syscall lacks a deterministic implementation and uses the configured fallback policy.
    // TODO-HUMAN-REVIEW(PR-643): Review the issue-backed unsupported classification policy.
    Unsupported,
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#275): Review syscall policy categories and fail-closed boundaries.
/// Classifies every syscall in the pinned x86_64 `Sysno` table.
pub(crate) const fn classify_syscall(sysno: Sysno) -> SyscallClassification {
    match sysno {
        // ===== DETERMINIZED SYSCALLS =====
        // These have a Detcore handler, deterministic replacement, or explicit refusal policy.
        Sysno::accept
        | Sysno::accept4
        | Sysno::alarm
        | Sysno::arch_prctl
        | Sysno::bind
        | Sysno::clock_getres
        | Sysno::clock_gettime
        | Sysno::clock_nanosleep
        | Sysno::clone
        | Sysno::clone3
        | Sysno::close
        | Sysno::connect
        | Sysno::creat
        | Sysno::dup
        | Sysno::dup2
        | Sysno::dup3
        | Sysno::epoll_create
        | Sysno::epoll_create1
        | Sysno::epoll_ctl
        | Sysno::epoll_ctl_old
        | Sysno::epoll_pwait
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#773): epoll_pwait2 is epoll_pwait with a
        // `struct timespec *` timeout instead of int-milliseconds. Recent glibc
        // implements epoll_wait/epoll_pwait via epoll_pwait2 when the kernel
        // supports it, so programs fail-close here without it. Untyped
        // (Syscall::Other) in the pinned Reverie; dispatched by Sysno in lib.rs
        // and handled identically to epoll_pwait.
        | Sysno::epoll_pwait2
        | Sysno::epoll_wait
        | Sysno::epoll_wait_old
        | Sysno::eventfd
        | Sysno::eventfd2
        | Sysno::execve
        | Sysno::execveat
        | Sysno::exit
        | Sysno::exit_group
        | Sysno::fadvise64
        | Sysno::fcntl
        | Sysno::fork
        | Sysno::fstat
        | Sysno::fstatfs
        | Sysno::futex
        | Sysno::futimesat
        | Sysno::getcpu
        | Sysno::getdents
        | Sysno::getdents64
        | Sysno::getrandom
        | Sysno::getrusage
        | Sysno::gettimeofday
        | Sysno::inotify_add_watch
        | Sysno::inotify_init
        | Sysno::inotify_init1
        | Sysno::inotify_rm_watch
        | Sysno::io_uring_enter
        | Sysno::io_uring_register
        | Sysno::io_uring_setup
        | Sysno::ioctl
        | Sysno::lstat
        | Sysno::madvise
        | Sysno::membarrier
        | Sysno::memfd_create
        | Sysno::mmap
        | Sysno::mremap
        | Sysno::munmap
        | Sysno::nanosleep
        | Sysno::newfstatat
        | Sysno::open
        | Sysno::openat
        | Sysno::pause
        | Sysno::pipe
        | Sysno::pipe2
        | Sysno::poll
        | Sysno::ppoll
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#686): Review scratch fd sets and scheduler polling.
        | Sysno::pselect6
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#800): select is the timeval sibling of pselect6.
        | Sysno::select
        | Sysno::prlimit64
        | Sysno::pread64
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#683): Confirm positional-write ordering and replay semantics.
        | Sysno::pwrite64
        | Sysno::read
        | Sysno::recvfrom
        | Sysno::recvmsg
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#788): Vectored datagram-receive sibling of recvmsg.
        // recvmsg/recvfrom are already Determinized and recvmmsg is just their
        // multi-message form; it shares the same NonblockableSyscall impl
        // (network_comm_syscall) and executes atomically on a temporarily
        // nonblocking fd, so the kernel fills the mmsghdr array itself. Its
        // timeout argument is ignored (the fd is nonblocking and the Detcore
        // scheduler owns blocking), matching recvmsg's determinism model.
        | Sysno::recvmmsg
        | Sysno::rseq
        | Sysno::rt_sigaction
        | Sysno::rt_sigprocmask
        | Sysno::rt_sigtimedwait
        | Sysno::rt_sigsuspend
        | Sysno::sched_getaffinity
        | Sysno::sched_setaffinity
        | Sysno::sched_yield
        | Sysno::sendmmsg
        | Sysno::sendmsg
        | Sysno::sendto
        | Sysno::setsid
        | Sysno::signalfd
        | Sysno::signalfd4
        | Sysno::socket
        | Sysno::socketpair
        | Sysno::stat
        | Sysno::statfs
        | Sysno::statx
        | Sysno::sysinfo
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::times
        | Sysno::time
        | Sysno::timer_create
        | Sysno::timer_delete
        | Sysno::timer_getoverrun
        | Sysno::timer_gettime
        | Sysno::timer_settime
        | Sysno::timerfd_create
        | Sysno::timerfd_gettime
        | Sysno::timerfd_settime
        | Sysno::uname
        | Sysno::userfaultfd
        | Sysno::utime
        | Sysno::utimensat
        | Sysno::utimes
        | Sysno::vfork
        | Sysno::wait4
        | Sysno::waitid
        | Sysno::write
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#663)
        | Sysno::clock_settime
        | Sysno::getpeername
        | Sysno::getsockname
        | Sysno::getsockopt
        | Sysno::getpriority
        | Sysno::getrlimit
        | Sysno::kill
        | Sysno::listen
        | Sysno::prctl
        | Sysno::rt_sigpending
        | Sysno::setitimer
        | Sysno::setpriority
        | Sysno::process_madvise
        | Sysno::setrlimit
        | Sysno::setsockopt
        | Sysno::tgkill
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#812): signal-sending siblings of the already
        // Determinized kill/tgkill. tkill is the two-argument thread-directed
        // predecessor of tgkill; rt_sigqueueinfo/rt_tgsigqueueinfo are the
        // data-carrying sigqueue forms. Signal generation/delivery is
        // scheduler-serialized and TGID/TID are stable in the fresh PID
        // namespace, so these are deterministic (KVM ratchet round 11).
        | Sysno::tkill
        | Sysno::rt_sigqueueinfo
        | Sysno::rt_tgsigqueueinfo
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#715): Deterministic ENOSYS for syscalls the pinned
        // x86_64 kernel leaves unimplemented (sys_ni_syscall). A fixed -ENOSYS is
        // deterministic by construction and matches the modern kernel's own return,
        // so guest-visible behavior is unchanged while dropping the host dependency.
        | Sysno::_sysctl
        | Sysno::afs_syscall
        | Sysno::create_module
        | Sysno::get_kernel_syms
        | Sysno::getpmsg
        | Sysno::lookup_dcookie
        | Sysno::nfsservctl
        | Sysno::putpmsg
        | Sysno::query_module
        | Sysno::security
        | Sysno::tuxcall
        | Sysno::uselib
        | Sysno::vserver
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#722): Deterministic EPERM for privileged
        // system-administration syscalls that mutate global kernel/host state
        // (module load/unload, kexec, reboot, swap, raw I/O ports, root-mount
        // pivot, host/domain name, tty hangup, disk quotas). The deterministic
        // guest does not hold the required capabilities against the host kernel,
        // so a fixed -EPERM is the same errno an unprivileged process receives.
        // Refusing them in Detcore (rather than the legacy pass-through, which
        // forwarded them to the real kernel) removes a host dependency and a
        // global-state isolation hole, and is bitwise-identical across --verify
        // and record/replay. Dispatched by Sysno in lib.rs before the typed
        // match below.
        | Sysno::init_module
        | Sysno::finit_module
        | Sysno::delete_module
        | Sysno::kexec_load
        | Sysno::kexec_file_load
        | Sysno::reboot
        | Sysno::swapon
        | Sysno::swapoff
        | Sysno::ioperm
        | Sysno::iopl
        | Sysno::pivot_root
        | Sysno::sethostname
        | Sysno::setdomainname
        | Sysno::vhangup
        | Sysno::quotactl
        | Sysno::quotactl_fd
        // TODO-HUMAN-REVIEW(#547)
        | Sysno::writev
        // ===== BATCH 3: NUMA memory-placement and Linux CPU-scheduling policy =====
        // Hermit presents a single deterministic virtual CPU and a single virtual
        // NUMA node, and Detcore replaces the Linux scheduler with its own. NUMA
        // placement policy and Linux scheduling policy/priority are therefore
        // inoperative: they cannot change guest-visible computation. Left as
        // passthrough their results depend on host NUMA topology, host scheduler
        // state, and privilege (all nondeterministic). They are determinized to
        // fixed, host-independent results; see the handlers in lib.rs.
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#720)
        | Sysno::mbind
        | Sysno::set_mempolicy
        | Sysno::get_mempolicy
        | Sysno::set_mempolicy_home_node
        | Sysno::migrate_pages
        | Sysno::move_pages
        | Sysno::sched_setscheduler
        | Sysno::sched_setparam
        | Sysno::sched_getscheduler
        | Sysno::sched_getparam
        | Sysno::sched_rr_get_interval
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#724): Deterministic EPERM for privileged mount and
        // namespace administration syscalls. These create, enter, or
        // reconfigure mount and other namespaces, resolve files by kernel
        // handle, configure global filesystem-event notification, or set the
        // host clock -- global kernel state a deterministic container pins for
        // the whole run. They are capability-gated (CAP_SYS_ADMIN /
        // CAP_DAC_READ_SEARCH / CAP_SYS_TIME), so a fixed -EPERM is the errno an
        // unprivileged process receives for the privileged operations and a
        // deliberate deterministic refusal for the few unprivileged sub-modes
        // (user-namespace unshare, non-clone open_tree) that would otherwise
        // perturb the pinned container. Refusing in Detcore (rather than the
        // legacy pass-through, which forwarded them to the real kernel) removes a
        // host dependency and a global-state isolation hole, and is
        // bitwise-identical across --verify and record/replay. Dispatched by
        // Sysno in lib.rs before the typed match below.
        | Sysno::mount
        | Sysno::umount2
        | Sysno::mount_setattr
        | Sysno::move_mount
        | Sysno::open_tree
        | Sysno::fsopen
        | Sysno::fsmount
        | Sysno::fsconfig
        | Sysno::fspick
        | Sysno::unshare
        | Sysno::setns
        | Sysno::open_by_handle_at
        | Sysno::fanotify_init
        | Sysno::fanotify_mark
        | Sysno::settimeofday
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#731): Deterministic ENOSYS for the
        // asynchronous and message-passing I/O and IPC interfaces Detcore does
        // not model. Linux native AIO (io_setup/io_destroy/io_submit/io_cancel/
        // io_getevents/io_pgetevents) has kernel-driven asynchronous completion
        // that lives outside the guest's logical time; POSIX message queues
        // (mq_*) and System V message queues (msg*) are global, key/name-
        // addressed kernel objects that persist across runs and are shared with
        // the whole host. Forwarding any of them (the legacy pass-through) is
        // nondeterministic and a container-isolation hole. A fixed -ENOSYS is
        // exactly the errno a kernel built without AIO, CONFIG_POSIX_MQUEUE, or
        // CONFIG_SYSVIPC returns, so guest-visible behavior is unchanged versus
        // such a kernel; it is never forwarded to the host and is bitwise-
        // identical across --verify and record/replay. This mirrors the
        // existing io_uring ENOSYS refusal. These are untyped (Syscall::Other)
        // in the pinned Reverie, so the dispatcher matches on the Sysno before
        // the typed match below. System V semaphores (sem*) and shared memory
        // (shm*) are deliberately left unclassified: they are common
        // intra-container synchronization/sharing primitives whose refusal would
        // be a real capability regression and needs a dedicated decision.
        | Sysno::io_setup
        | Sysno::io_destroy
        | Sysno::io_submit
        | Sysno::io_cancel
        | Sysno::io_getevents
        | Sysno::io_pgetevents
        | Sysno::mq_open
        | Sysno::mq_unlink
        | Sysno::mq_timedsend
        | Sysno::mq_timedreceive
        | Sysno::mq_notify
        | Sysno::mq_getsetattr
        | Sysno::msgget
        | Sysno::msgsnd
        | Sysno::msgrcv
        | Sysno::msgctl
        // ===== BATCH 51: fail-closed utility syscalls with no deterministic effect =====
        // These three previously fail-closed --strict (aborting real programs such
        // as chrt, ionice, and flock) even though none can change guest-visible
        // computation under Hermit. Detcore replaces the Linux scheduler, presents a
        // single virtual CPU, and serializes guest threads, so a thread's I/O
        // priority (ioprio_set) and its Linux scheduling attributes (sched_getattr)
        // are inert, and an advisory whole-file lock (flock) is never contended
        // within the serialized container. They are determinized to fixed,
        // host-independent results; see the handlers in lib.rs. sched_setattr and
        // ioprio_get remain Unsupported until a task owns their write/read pair.
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#791)
        | Sysno::flock
        | Sysno::ioprio_set
        | Sysno::sched_getattr => SyscallClassification::Determinized,

        // ===== BEGIN PASS-THRU SYSCALLS =====
        // These existing and triaged passthroughs are conditionally repeatable under
        // Hermit's fixed-container, stable-filesystem, and serialization assumptions.
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#503): Confirm the stable-state boundary for these promotions.
        Sysno::access
        | Sysno::brk
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#663)
        | Sysno::chown
        | Sysno::getcwd
        | Sysno::getegid
        | Sysno::geteuid
        | Sysno::getgid
        | Sysno::getpid
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#663)
        | Sysno::getpgid
        | Sysno::getpgrp
        | Sysno::getppid
        | Sysno::getsid
        | Sysno::gettid
        | Sysno::getuid
        | Sysno::lseek
        | Sysno::mprotect
        | Sysno::readlink
        | Sysno::set_robust_list
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#663)
        | Sysno::setpgid
        | Sysno::set_tid_address
        | Sysno::sigaltstack
        // capget/capset/getgroups observe or update kernel credential state that starts
        // from the fixed container identity on each run.
        | Sysno::capget
        | Sysno::capset
        | Sysno::getgroups
        // chdir/fchdir/faccessat2/umask are deterministic process-state transitions or
        // checks given a fixed namespace, credential set, and filesystem image.
        | Sysno::chdir
        | Sysno::faccessat2
        | Sysno::fchdir
        | Sysno::umask
        // chmod/fchmodat/linkat/mkdir/mkdirat/renameat2/rmdir/symlinkat/unlink/unlinkat
        // repeat given stable guest-visible filesystem state with no external mutation.
        | Sysno::chmod
        | Sysno::fchmodat
        | Sysno::linkat
        | Sysno::mkdir
        | Sysno::mkdirat
        | Sysno::renameat2
        | Sysno::rmdir
        | Sysno::symlinkat
        | Sysno::unlink
        | Sysno::unlinkat
        // getxattr/lgetxattr/removexattr/setxattr are deterministic for stable objects
        // and do not introduce asynchronous state or new kernel objects.
        | Sysno::getxattr
        | Sysno::lgetxattr
        | Sysno::removexattr
        | Sysno::setxattr
        // fdatasync/ftruncate have deterministic results for stable guest-owned files;
        // physical flush latency is outside guest logical time.
        | Sysno::fdatasync
        | Sysno::ftruncate
        // Fixed credentials, process-local unlocks, and guest-owned filesystem
        // flushes are repeatable under the fixed-container model.
        // TODO-HUMAN-REVIEW(PR-654): Verify deterministic passthrough assumptions.
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::fsync
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::getresgid
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::getresuid
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::munlock
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::munlockall
        // These synchronous extent and pathname operations are repeatable for guest-owned
        // files in a fixed namespace with adequate space and no external mutation.
        // TODO-HUMAN-REVIEW(PR-675): Verify stable-filesystem passthrough assumptions.
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::fallocate
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::readlinkat
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::rename
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::renameat
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::truncate
        // Stable guest-owned metadata and synchronous writeback operations are
        // repeatable in Hermit's fixed mount namespace and filesystem image.
        // TODO-HUMAN-REVIEW(#683): Confirm the metadata/writeback passthrough boundary.
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::faccessat
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::fchmod
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::fchmodat2
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::fchown
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::fchownat
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::fgetxattr
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::flistxattr
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::fremovexattr
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::fsetxattr
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::lchown
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::link
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::listxattr
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::llistxattr
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::lremovexattr
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::lsetxattr
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::msync
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::readahead
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::symlink
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::sync_file_range
        // Ptrace executes rt_sigreturn directly; DBI has dedicated injected-sigreturn
        // handling, while KVM deterministically reports its current lack of signal support.
        | Sysno::rt_sigreturn => SyscallClassification::PassThrough,
        // ===== END PASS-THRU SYSCALLS =====

        // ===== ISSUE-REVIEWED PASS-THROUGH SYSCALLS =====
        // Every matching classification issue recommends PASS-THRU. These remain
        // conditional on Hermit's fixed-container and stable-state assumptions.
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-643): Review issue-backed pass-through promotions.
        Sysno::chroot
        | Sysno::get_thread_area
        | Sysno::mknod
        | Sysno::mknodat
        | Sysno::mlock
        | Sysno::mlock2
        | Sysno::mlockall
        | Sysno::modify_ldt
        | Sysno::personality
        | Sysno::pkey_alloc
        | Sysno::pkey_free
        | Sysno::pkey_mprotect
        | Sysno::sched_get_priority_max
        | Sysno::sched_get_priority_min
        | Sysno::set_thread_area
        | Sysno::sync
        | Sysno::syncfs
        => SyscallClassification::PassThrough,
        // ===== END ISSUE-REVIEWED PASS-THROUGH SYSCALLS =====

        // ===== UNSUPPORTED SYSCALLS =====
        // These require a deterministic handler or further investigation. Normal mode
        // records their use for an aggregate warning and preserves legacy forwarding;
        // --panic-on-unsupported-syscalls stops at the first use.
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-643): Review issue-backed unsupported classifications.
        Sysno::acct
        | Sysno::add_key
        | Sysno::adjtimex
        | Sysno::bpf
        | Sysno::cachestat
        | Sysno::clock_adjtime
        | Sysno::close_range
        | Sysno::copy_file_range
        | Sysno::futex_requeue
        | Sysno::futex_wait
        | Sysno::futex_waitv
        | Sysno::futex_wake
        | Sysno::get_robust_list
        | Sysno::getitimer
        | Sysno::ioprio_get
        | Sysno::kcmp
        | Sysno::keyctl
        | Sysno::landlock_add_rule
        | Sysno::landlock_create_ruleset
        | Sysno::landlock_restrict_self
        | Sysno::listmount
        | Sysno::lsm_get_self_attr
        | Sysno::lsm_list_modules
        | Sysno::lsm_set_self_attr
        | Sysno::map_shadow_stack
        | Sysno::memfd_secret
        | Sysno::mincore
        | Sysno::name_to_handle_at
        | Sysno::openat2
        | Sysno::perf_event_open
        | Sysno::pidfd_getfd
        | Sysno::pidfd_open
        | Sysno::pidfd_send_signal
        | Sysno::preadv
        | Sysno::preadv2
        | Sysno::process_mrelease
        | Sysno::process_vm_readv
        | Sysno::process_vm_writev
        | Sysno::ptrace
        | Sysno::pwritev
        | Sysno::pwritev2
        | Sysno::readv
        | Sysno::remap_file_pages
        | Sysno::request_key
        | Sysno::restart_syscall
        | Sysno::sched_setattr
        | Sysno::seccomp
        | Sysno::semctl
        | Sysno::semget
        | Sysno::semop
        | Sysno::semtimedop
        | Sysno::sendfile
        | Sysno::setfsgid
        | Sysno::setfsuid
        | Sysno::setgid
        | Sysno::setgroups
        | Sysno::setregid
        | Sysno::setresgid
        | Sysno::setresuid
        | Sysno::setreuid
        | Sysno::setuid
        | Sysno::shmat
        | Sysno::shmctl
        | Sysno::shmdt
        | Sysno::shmget
        | Sysno::shutdown
        | Sysno::splice
        | Sysno::statmount
        | Sysno::sysfs
        | Sysno::syslog
        | Sysno::tee
        | Sysno::ustat
        | Sysno::vmsplice => SyscallClassification::Unsupported,
        // ===== END UNSUPPORTED SYSCALLS =====

        // `Sysno` is `#[non_exhaustive]` outside its crate. The const ABI guards above
        // make changes to the pinned table a compile error; this arm only satisfies the
        // external-enum language requirement and deliberately fails closed.
        _unexpected => panic!("unclassified Sysno outside pinned ABI"),
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#715): Deterministic ENOSYS refusal set.
/// Syscalls the pinned modern x86_64 kernel leaves unimplemented (routed to
/// `sys_ni_syscall`, which returns `-ENOSYS`). Detcore refuses them with a fixed
/// `ENOSYS` so the result is deterministic by construction rather than depending
/// on the host kernel actually being modern; the guest-visible errno is identical
/// to the legacy pass-through on any current kernel.
pub(crate) const fn is_unimplemented_enosys_syscall(sysno: Sysno) -> bool {
    matches!(
        sysno,
        Sysno::_sysctl
            | Sysno::afs_syscall
            | Sysno::create_module
            | Sysno::get_kernel_syms
            | Sysno::getpmsg
            | Sysno::lookup_dcookie
            | Sysno::nfsservctl
            | Sysno::putpmsg
            | Sysno::query_module
            | Sysno::security
            | Sysno::tuxcall
            | Sysno::uselib
            | Sysno::vserver
    )
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#722): Deterministic EPERM refusal set.
/// Privileged system-administration syscalls that mutate global kernel or host
/// state (loading/unloading kernel modules, kexec, reboot, enabling/disabling
/// swap, raw I/O port access, pivoting the root mount, setting the host or
/// domain name, tty hangup, and disk quotas). A deterministic guest must never
/// perturb this global state, and it does not hold the capabilities these
/// operations require against the host kernel, so Detcore refuses them with a
/// fixed `EPERM`. That is the same errno an unprivileged process receives, it
/// is never forwarded to the host (unlike the legacy pass-through), and it is
/// deterministic by construction rather than depending on host privilege or
/// configuration. These are untyped (`Syscall::Other`) in the pinned Reverie,
/// so the dispatcher matches on the `Sysno` before the typed match.
pub(crate) const fn is_privileged_admin_refused_syscall(sysno: Sysno) -> bool {
    matches!(
        sysno,
        Sysno::init_module
            | Sysno::finit_module
            | Sysno::delete_module
            | Sysno::kexec_load
            | Sysno::kexec_file_load
            | Sysno::reboot
            | Sysno::swapon
            | Sysno::swapoff
            | Sysno::ioperm
            | Sysno::iopl
            | Sysno::pivot_root
            | Sysno::sethostname
            | Sysno::setdomainname
            | Sysno::vhangup
            | Sysno::quotactl
            | Sysno::quotactl_fd
    )
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#724): Deterministic EPERM refusal set.
/// Privileged mount and namespace administration syscalls that create, enter,
/// or reconfigure mount and other namespaces, resolve files by kernel handle,
/// configure global filesystem-event notification, or set the host clock. A
/// deterministic container pins the guest's namespaces, mount hierarchy, and
/// virtual clock for the entire run, so Detcore refuses these with a fixed
/// `EPERM`. That is the errno an unprivileged process receives for the
/// capability-gated operations (`CAP_SYS_ADMIN`, `CAP_DAC_READ_SEARCH`,
/// `CAP_SYS_TIME`) and a deliberate deterministic refusal for the few
/// unprivileged sub-modes (a user-namespace `unshare`, a non-clone
/// `open_tree`) that would otherwise perturb the pinned container. The result
/// is never forwarded to the host and is deterministic by construction. These
/// are untyped (`Syscall::Other`) in the pinned Reverie, so the dispatcher
/// matches on the `Sysno` before the typed match.
pub(crate) const fn is_mount_ns_admin_refused_syscall(sysno: Sysno) -> bool {
    matches!(
        sysno,
        Sysno::mount
            | Sysno::umount2
            | Sysno::mount_setattr
            | Sysno::move_mount
            | Sysno::open_tree
            | Sysno::fsopen
            | Sysno::fsmount
            | Sysno::fsconfig
            | Sysno::fspick
            | Sysno::unshare
            | Sysno::setns
            | Sysno::open_by_handle_at
            | Sysno::fanotify_init
            | Sysno::fanotify_mark
            | Sysno::settimeofday
    )
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#731): Deterministic ENOSYS refusal set.
/// Asynchronous and message-passing I/O and IPC interfaces Detcore does not
/// model: Linux native AIO (`io_setup`/`io_destroy`/`io_submit`/`io_cancel`/
/// `io_getevents`/`io_pgetevents`), POSIX message queues (`mq_*`), and System V
/// message queues (`msg*`). AIO completion is kernel-driven and lives outside
/// the guest's logical time, and the message-queue families operate on global,
/// key/name-addressed kernel objects that persist across runs and are shared
/// with the whole host. Forwarding them (the legacy pass-through) is
/// nondeterministic and a container-isolation hole, so Detcore refuses them with
/// a fixed `ENOSYS`: exactly the errno a kernel built without AIO,
/// `CONFIG_POSIX_MQUEUE`, or `CONFIG_SYSVIPC` returns. The result is never
/// forwarded to the host and is bitwise-identical across `--verify` and
/// record/replay, mirroring the existing `io_uring` ENOSYS refusal. System V
/// semaphores and shared memory are deliberately excluded (common
/// intra-container primitives that need a dedicated decision). These are untyped
/// (`Syscall::Other`) in the pinned Reverie, so the dispatcher matches on the
/// `Sysno` before the typed match.
pub(crate) const fn is_unsupported_async_ipc_syscall(sysno: Sysno) -> bool {
    matches!(
        sysno,
        Sysno::io_setup
            | Sysno::io_destroy
            | Sysno::io_submit
            | Sysno::io_cancel
            | Sysno::io_getevents
            | Sysno::io_pgetevents
            | Sysno::mq_open
            | Sysno::mq_unlink
            | Sysno::mq_timedsend
            | Sysno::mq_timedreceive
            | Sysno::mq_notify
            | Sysno::mq_getsetattr
            | Sysno::msgget
            | Sysno::msgsnd
            | Sysno::msgrcv
            | Sysno::msgctl
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pinned_sysno_has_an_explicit_classification() {
        let mut counts = [0usize; 3];
        // syscalls 0.6.18 `Sysno::iter()` omits `last()` due its strict loop bound.
        for sysno in Sysno::iter().chain(std::iter::once(Sysno::last())) {
            match classify_syscall(sysno) {
                SyscallClassification::Determinized => counts[0] += 1,
                SyscallClassification::PassThrough => counts[1] += 1,
                SyscallClassification::Unsupported => counts[2] += 1,
            }
        }

        assert_eq!(counts, [209, 91, 73]);
        assert_eq!(counts.iter().sum::<usize>(), EXPECTED_X86_64_SYSNO_COUNT);
    }

    #[test]
    fn representative_policies_stay_in_their_reviewed_sections() {
        assert_eq!(
            classify_syscall(Sysno::futex),
            SyscallClassification::Determinized
        );
        assert_eq!(
            classify_syscall(Sysno::nanosleep),
            SyscallClassification::Determinized
        );
        assert_eq!(
            classify_syscall(Sysno::lseek),
            SyscallClassification::PassThrough
        );
        assert_eq!(
            classify_syscall(Sysno::ppoll),
            SyscallClassification::Determinized
        );
        // select is the timeval sibling of pselect6 and must stay Determinized
        // (routed through handle_select); regression for #800.
        assert_eq!(
            classify_syscall(Sysno::select),
            SyscallClassification::Determinized
        );
        assert_eq!(
            classify_syscall(Sysno::pselect6),
            SyscallClassification::Determinized
        );
        assert_eq!(
            classify_syscall(Sysno::arch_prctl),
            SyscallClassification::Determinized
        );
        assert_eq!(
            classify_syscall(Sysno::prlimit64),
            SyscallClassification::Determinized
        );
        assert_eq!(
            classify_syscall(Sysno::pwrite64),
            SyscallClassification::Determinized
        );
        assert_eq!(
            classify_syscall(Sysno::madvise),
            SyscallClassification::Determinized
        );
        assert_eq!(
            classify_syscall(Sysno::writev),
            SyscallClassification::Determinized
        );
        assert_eq!(
            classify_syscall(Sysno::times),
            SyscallClassification::Determinized
        );
        // BATCH 51: these previously fail-closed --strict; now determinized.
        for sysno in [Sysno::flock, Sysno::ioprio_set, Sysno::sched_getattr] {
            assert_eq!(classify_syscall(sysno), SyscallClassification::Determinized);
        }
        // Their read/write siblings deliberately remain Unsupported for now.
        for sysno in [Sysno::ioprio_get, Sysno::sched_setattr] {
            assert_eq!(classify_syscall(sysno), SyscallClassification::Unsupported);
        }
        // recvmmsg is the multi-message sibling of recvmsg and must stay
        // Determinized (routed through handle_sendrecv); regression for #788.
        assert_eq!(
            classify_syscall(Sysno::recvmmsg),
            SyscallClassification::Determinized
        );
        assert_eq!(
            classify_syscall(Sysno::recvmsg),
            SyscallClassification::Determinized
        );
        for sysno in [
            Sysno::epoll_pwait2,
            Sysno::clock_settime,
            Sysno::getpeername,
            Sysno::getsockname,
            Sysno::getsockopt,
            Sysno::getpriority,
            Sysno::getrlimit,
            Sysno::kill,
            Sysno::listen,
            Sysno::prctl,
            Sysno::rt_sigpending,
            Sysno::setitimer,
            Sysno::setpriority,
            Sysno::process_madvise,
            Sysno::setrlimit,
            Sysno::setsockopt,
            Sysno::tgkill,
        ] {
            assert_eq!(classify_syscall(sysno), SyscallClassification::Determinized);
        }
        // KVM ratchet round 11: signal-sending siblings of kill/tgkill. tkill
        // (routed through handle_tkill) and rt_sigqueueinfo/rt_tgsigqueueinfo
        // (routed through handle_rt_sigqueueinfo/handle_rt_tgsigqueueinfo) must
        // stay Determinized rather than fail-closing under --strict.
        for sysno in [
            Sysno::tkill,
            Sysno::rt_sigqueueinfo,
            Sysno::rt_tgsigqueueinfo,
        ] {
            assert_eq!(classify_syscall(sysno), SyscallClassification::Determinized);
        }
        for sysno in [
            Sysno::capget,
            Sysno::capset,
            Sysno::chown,
            Sysno::chdir,
            Sysno::chmod,
            Sysno::faccessat,
            Sysno::faccessat2,
            Sysno::fchdir,
            Sysno::fchmod,
            Sysno::fchmodat,
            Sysno::fchmodat2,
            Sysno::fchown,
            Sysno::fchownat,
            Sysno::fdatasync,
            Sysno::fallocate,
            Sysno::fgetxattr,
            Sysno::flistxattr,
            Sysno::fremovexattr,
            Sysno::fsetxattr,
            Sysno::ftruncate,
            Sysno::fsync,
            Sysno::getresgid,
            Sysno::getresuid,
            Sysno::munlock,
            Sysno::munlockall,
            Sysno::readlinkat,
            Sysno::rename,
            Sysno::renameat,
            Sysno::getgroups,
            Sysno::getppid,
            Sysno::getxattr,
            Sysno::lchown,
            Sysno::getpgid,
            Sysno::getpgrp,
            Sysno::getsid,
            Sysno::setpgid,
            Sysno::lgetxattr,
            Sysno::link,
            Sysno::linkat,
            Sysno::listxattr,
            Sysno::llistxattr,
            Sysno::lremovexattr,
            Sysno::lsetxattr,
            Sysno::mkdir,
            Sysno::mkdirat,
            Sysno::msync,
            Sysno::removexattr,
            Sysno::readahead,
            Sysno::renameat2,
            Sysno::readlinkat,
            Sysno::rmdir,
            Sysno::rt_sigreturn,
            Sysno::setxattr,
            Sysno::symlink,
            Sysno::symlinkat,
            Sysno::sync_file_range,
            Sysno::truncate,
            Sysno::umask,
            Sysno::unlink,
            Sysno::unlinkat,
        ] {
            assert_eq!(classify_syscall(sysno), SyscallClassification::PassThrough);
        }
        for sysno in [Sysno::add_key, Sysno::keyctl, Sysno::request_key] {
            assert_eq!(classify_syscall(sysno), SyscallClassification::Unsupported);
        }
        for sysno in [
            Sysno::chroot,
            Sysno::get_thread_area,
            Sysno::mknod,
            Sysno::mknodat,
            Sysno::mlock,
            Sysno::mlock2,
            Sysno::mlockall,
            Sysno::modify_ldt,
            Sysno::personality,
            Sysno::pkey_alloc,
            Sysno::pkey_free,
            Sysno::pkey_mprotect,
            Sysno::sched_get_priority_max,
            Sysno::sched_get_priority_min,
            Sysno::set_thread_area,
            Sysno::sync,
            Sysno::syncfs,
        ] {
            assert_eq!(classify_syscall(sysno), SyscallClassification::PassThrough);
        }
        // Batch 3: NUMA memory-placement and Linux CPU-scheduling policy are
        // determinized to fixed, host-independent results (single virtual NUMA
        // node + Detcore scheduler).
        for sysno in [
            Sysno::mbind,
            Sysno::set_mempolicy,
            Sysno::get_mempolicy,
            Sysno::set_mempolicy_home_node,
            Sysno::migrate_pages,
            Sysno::move_pages,
            Sysno::sched_setscheduler,
            Sysno::sched_setparam,
            Sysno::sched_getscheduler,
            Sysno::sched_getparam,
            Sysno::sched_rr_get_interval,
        ] {
            assert_eq!(classify_syscall(sysno), SyscallClassification::Determinized);
        }
        // Batch 7: asynchronous and message-passing I/O and IPC interfaces
        // Detcore does not model are refused with a deterministic ENOSYS (Linux
        // native AIO, POSIX message queues, System V message queues); see
        // is_unsupported_async_ipc_syscall.
        for sysno in [
            Sysno::io_setup,
            Sysno::io_destroy,
            Sysno::io_submit,
            Sysno::io_cancel,
            Sysno::io_getevents,
            Sysno::io_pgetevents,
            Sysno::mq_open,
            Sysno::mq_unlink,
            Sysno::mq_timedsend,
            Sysno::mq_timedreceive,
            Sysno::mq_notify,
            Sysno::mq_getsetattr,
            Sysno::msgget,
            Sysno::msgsnd,
            Sysno::msgrcv,
            Sysno::msgctl,
        ] {
            assert_eq!(classify_syscall(sysno), SyscallClassification::Determinized);
            assert!(is_unsupported_async_ipc_syscall(sysno));
        }
        // System V semaphores and shared memory are deliberately still
        // unsupported (intra-container primitives pending a dedicated decision).
        for sysno in [Sysno::semget, Sysno::semop, Sysno::shmget, Sysno::shmat] {
            assert_eq!(classify_syscall(sysno), SyscallClassification::Unsupported);
            assert!(!is_unsupported_async_ipc_syscall(sysno));
        }
    }

    #[test]
    fn unimplemented_enosys_syscalls_are_determinized_and_consistent() {
        // Every syscall in the deterministic ENOSYS-refusal set must classify as
        // Determinized, and the helper used by the dispatcher must agree exactly
        // with that classification across the whole pinned table.
        let refused = [
            Sysno::_sysctl,
            Sysno::afs_syscall,
            Sysno::create_module,
            Sysno::get_kernel_syms,
            Sysno::getpmsg,
            Sysno::lookup_dcookie,
            Sysno::nfsservctl,
            Sysno::putpmsg,
            Sysno::query_module,
            Sysno::security,
            Sysno::tuxcall,
            Sysno::uselib,
            Sysno::vserver,
        ];
        for sysno in refused {
            assert_eq!(
                classify_syscall(sysno),
                SyscallClassification::Determinized,
                "{sysno:?} should be Determinized (deterministic ENOSYS refusal)"
            );
            assert!(
                is_unimplemented_enosys_syscall(sysno),
                "{sysno:?} should be in the ENOSYS-refusal helper set"
            );
        }
        // The helper must not claim any syscall outside the reviewed set.
        for sysno in Sysno::iter().chain(std::iter::once(Sysno::last())) {
            if is_unimplemented_enosys_syscall(sysno) {
                assert!(
                    refused.contains(&sysno),
                    "{sysno:?} is flagged by the helper but not in the reviewed refusal set"
                );
            }
        }
    }

    #[test]
    fn privileged_admin_syscalls_are_determinized_and_consistent() {
        // Every syscall in the deterministic EPERM-refusal set must classify as
        // Determinized, and the helper used by the dispatcher must agree exactly
        // with that classification across the whole pinned table.
        let refused = [
            Sysno::init_module,
            Sysno::finit_module,
            Sysno::delete_module,
            Sysno::kexec_load,
            Sysno::kexec_file_load,
            Sysno::reboot,
            Sysno::swapon,
            Sysno::swapoff,
            Sysno::ioperm,
            Sysno::iopl,
            Sysno::pivot_root,
            Sysno::sethostname,
            Sysno::setdomainname,
            Sysno::vhangup,
            Sysno::quotactl,
            Sysno::quotactl_fd,
        ];
        for sysno in refused {
            assert_eq!(
                classify_syscall(sysno),
                SyscallClassification::Determinized,
                "{sysno:?} should be Determinized (deterministic EPERM refusal)"
            );
            assert!(
                is_privileged_admin_refused_syscall(sysno),
                "{sysno:?} should be in the EPERM-refusal helper set"
            );
        }
        // The helper must not claim any syscall outside the reviewed set.
        for sysno in Sysno::iter().chain(std::iter::once(Sysno::last())) {
            if is_privileged_admin_refused_syscall(sysno) {
                assert!(
                    refused.contains(&sysno),
                    "{sysno:?} is flagged by the helper but not in the reviewed refusal set"
                );
            }
        }
    }

    #[test]
    fn mount_ns_admin_syscalls_are_determinized_and_consistent() {
        // Every syscall in the deterministic mount/namespace EPERM-refusal set
        // must classify as Determinized, and the helper used by the dispatcher
        // must agree exactly with that classification across the pinned table.
        let refused = [
            Sysno::mount,
            Sysno::umount2,
            Sysno::mount_setattr,
            Sysno::move_mount,
            Sysno::open_tree,
            Sysno::fsopen,
            Sysno::fsmount,
            Sysno::fsconfig,
            Sysno::fspick,
            Sysno::unshare,
            Sysno::setns,
            Sysno::open_by_handle_at,
            Sysno::fanotify_init,
            Sysno::fanotify_mark,
            Sysno::settimeofday,
        ];
        for sysno in refused {
            assert_eq!(
                classify_syscall(sysno),
                SyscallClassification::Determinized,
                "{sysno:?} should be Determinized (deterministic EPERM refusal)"
            );
            assert!(
                is_mount_ns_admin_refused_syscall(sysno),
                "{sysno:?} should be in the mount/namespace EPERM-refusal helper set"
            );
        }
        // The helper must not claim any syscall outside the reviewed set.
        for sysno in Sysno::iter().chain(std::iter::once(Sysno::last())) {
            if is_mount_ns_admin_refused_syscall(sysno) {
                assert!(
                    refused.contains(&sysno),
                    "{sysno:?} is flagged by the helper but not in the reviewed refusal set"
                );
            }
        }
    }
}
