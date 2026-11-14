/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use reverie::syscalls::Sysno;

#[cfg(target_arch = "x86_64")]
const EXPECTED_SYSNO_COUNT: usize = 383;
#[cfg(target_arch = "aarch64")]
const EXPECTED_SYSNO_COUNT: usize = 345;

// `Sysno` is externally `#[non_exhaustive]`. These assertions make additions,
// removals, or a changed table endpoint fail at compile time instead of silently
// reaching the required final arm.
const _: () = {
    assert!(Sysno::count() == EXPECTED_SYSNO_COUNT);
    assert!(Sysno::last().id() == 469);
};

/// Every syscall in the pinned table for the target architecture, including
/// the final entry.
///
/// `Sysno::iter()` in `syscalls` 0.8.1 stops one short: its loop bound is
/// strict, so `Sysno::last()` is never yielded. Any policy sweep written as
/// `Sysno::iter().filter(..)` therefore silently drops the last row of the
/// table. That is not hypothetical — it dropped `lsm_list_modules` (id 461,
/// Determinized and deterministically refused) out of the `passthru_opt`
/// subscription, letting it execute natively against the host on every
/// `hermit record` / `hermit replay`.
///
/// Use this instead of `Sysno::iter()` for anything that must cover the whole
/// classification table.
pub(crate) fn all_pinned_syscalls() -> impl Iterator<Item = Sysno> {
    Sysno::iter().chain(std::iter::once(Sysno::last()))
}

/// The x86_64 syscalls a host kernel may implement beyond the pinned table,
/// in ascending, contiguous order from `Sysno::last().id() + 1`.
///
/// The pinned `syscalls` table ends at 461 (`lsm_list_modules`), so none of
/// these has a `Sysno` and no `Syscall` can be built for them; they reach
/// Detcore only as raw unknown numbers (`Tool::handle_unknown_syscall`).
/// This list is what lets a refusal or a log line name such a number
/// instead of printing it bare, and the census tests hold it complete
/// against the test host's own kernel headers: a kernel that wires a
/// number neither the table nor this list names fails the test by name,
/// which is the prompt to extend this list (or bump the pin and move the
/// absorbed prefix into the classification below).
#[cfg(target_arch = "x86_64")]
pub(crate) const KNOWN_BEYOND_PINNED_TABLE: &[(i32, &str)] = &[
    (462, "mseal"),
    (463, "setxattrat"),
    (464, "getxattrat"),
    (465, "listxattrat"),
    (466, "removexattrat"),
    (467, "open_tree_attr"),
    (468, "file_getattr"),
    (469, "file_setattr"),
    (470, "listns"),
    (471, "rseq_slice_yield"),
];

/// No aarch64 number beyond its pinned table is curated here yet; the
/// architecture shares the raw-number policy but names nothing past its
/// table.
#[cfg(target_arch = "aarch64")]
pub(crate) const KNOWN_BEYOND_PINNED_TABLE: &[(i32, &str)] = &[];

/// The x86_64 numbers inside the pinned table's range that newer kernels
/// wired into slots the pinned table leaves unnamed. They have no `Sysno`
/// for the same reason the beyond-table numbers do not; they are listed
/// separately only because the beyond-table manifest must stay contiguous
/// from the table's end.
///
/// Unlike the beyond-table numbers, these do not reach Detcore on the
/// ptrace backend: on kernels that wire them, entry code handles them
/// before seccomp/ptrace. Measured with a `SECCOMP_RET_ERRNO` filter
/// returning sentinel errnos for 463 and for 335/336, the filter
/// intercepts 463 only; 335 still delivers `SIGILL` and 336 returns its
/// native errno. The ptrace backend therefore cannot answer `ENOSYS` or
/// refuse them, and under Hermit ptrace they keep their native outcome
/// (335: `SIGILL` outside a uretprobe trampoline; 336: its native errno).
/// Backends that trap the syscall instruction itself (KVM, measured) do
/// see them and route them through the unknown-syscall policy like any
/// post-table number.
///
/// The list exists so the host-completeness census test accounts for
/// these numbers (and fails by name when a kernel wires a further such
/// gap), and so a future syscalls-crate bump that names them forces
/// their classification.
#[cfg(target_arch = "x86_64")]
pub(crate) const KNOWN_TABLE_GAPS: &[(i32, &str)] = &[(335, "uretprobe"), (336, "uprobe")];

/// No in-range aarch64 gap is curated here yet.
#[cfg(target_arch = "aarch64")]
pub(crate) const KNOWN_TABLE_GAPS: &[(i32, &str)] = &[];

/// The display name of a raw syscall number the pinned table cannot name:
/// its curated name ([`KNOWN_BEYOND_PINNED_TABLE`] or
/// [`KNOWN_TABLE_GAPS`]) when listed, else `syscall_<number>`, the
/// histogram summary's fallback for the same key.
pub(crate) fn unknown_syscall_name(nr: i32) -> String {
    KNOWN_BEYOND_PINNED_TABLE
        .iter()
        .chain(KNOWN_TABLE_GAPS.iter())
        .find(|(known, _)| *known == nr)
        .map_or_else(|| format!("syscall_{nr}"), |(_, name)| (*name).to_owned())
}

/// The syscalls `--untrap-passthrough-syscalls` leaves to the kernel without a
/// stop (`Config::untrap_passthrough_syscalls`). Each is `PassThrough`, and
/// Detcore's handler for it injects the call unchanged and returns the kernel's
/// result unchanged, so the guest sees the same result whether or not it is
/// trapped. Each only reads or sets state of the calling process (or, for
/// `capget` and `get_robust_list`, reads another thread's) that the run's
/// container fixes, names no file or descriptor, maps or protects no memory,
/// reads no clock, takes no signal, and never waits for another thread, so
/// the thread that holds the turn always gets it back. Trapped, each would
/// still charge virtual time, log DETLOG lines and be a point where the
/// timeslice can end; those three are what change.
pub(crate) const UNTRAPPED_PASSTHROUGH: &[Sysno] = &[
    // The parent's PID in the guest's PID namespace.
    Sysno::getppid,
    // Swaps the file-creation mask: process state, read back by later
    // trapped file creations.
    Sysno::umask,
    // The supplementary groups of the container's fixed identity.
    Sysno::getgroups,
    // A thread's capability sets, fixed by the container's identity; capset
    // stays trapped.
    Sysno::capget,
    // Constants of each scheduling policy.
    Sysno::sched_get_priority_max,
    Sysno::sched_get_priority_min,
    // A thread's robust-list head; set_robust_list, which Detcore observes,
    // stays trapped.
    Sysno::get_robust_list,
];

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
/// Classifies every syscall in the pinned `Sysno` table of the target
/// architecture.
pub(crate) const fn classify_syscall(sysno: Sysno) -> SyscallClassification {
    match sysno {
        // ===== DETERMINIZED SYSCALLS =====
        // These have a Detcore handler, deterministic replacement, or explicit refusal policy.
        Sysno::accept
        | Sysno::accept4
        | Sysno::bind
        | Sysno::clock_getres
        | Sysno::clock_gettime
        | Sysno::clock_nanosleep
        | Sysno::clone
        | Sysno::clone3
        | Sysno::close
        | Sysno::connect
        | Sysno::dup
        | Sysno::dup3
        | Sysno::epoll_create1
        | Sysno::epoll_ctl
        | Sysno::epoll_pwait
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#773): epoll_pwait2 is epoll_pwait with a
        // `struct timespec *` timeout instead of int-milliseconds. Recent glibc
        // implements epoll_wait/epoll_pwait via epoll_pwait2 when the kernel
        // supports it, so programs fail-close here without it. Untyped
        // (Syscall::Other) in the pinned Reverie; dispatched by Sysno in lib.rs
        // and handled identically to epoll_pwait.
        | Sysno::epoll_pwait2
        | Sysno::eventfd2
        | Sysno::execve
        | Sysno::execveat
        | Sysno::exit
        | Sysno::exit_group
        | Sysno::fadvise64
        | Sysno::fcntl
        | Sysno::fstat
        | Sysno::fstatfs
        | Sysno::futex
        // TODO-HUMAN-REVIEW(PR-852): Review the futex2 feature-absence boundary.
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::futex_requeue
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::futex_wait
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::futex_waitv
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::futex_wake
        | Sysno::getcpu
        | Sysno::getdents64
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-892): Query logical interval-timer state.
        | Sysno::getitimer
        // Successful own IDs come from shared ThreadState after the inner
        // Tool observes the call; nested namespaces retain its answer.
        | Sysno::getpid
        | Sysno::getrandom
        | Sysno::getrusage
        | Sysno::gettid
        | Sysno::gettimeofday
        | Sysno::inotify_add_watch
        | Sysno::inotify_init1
        | Sysno::inotify_rm_watch
        | Sysno::io_uring_enter
        | Sysno::io_uring_register
        | Sysno::io_uring_setup
        | Sysno::ioctl
        | Sysno::madvise
        | Sysno::membarrier
        | Sysno::memfd_create
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#775): Deterministic mincore emulation (all-resident).
        | Sysno::mincore
        | Sysno::mmap
        | Sysno::mremap
        | Sysno::munmap
        | Sysno::nanosleep
        | Sysno::openat
        | Sysno::pipe2
        | Sysno::ppoll
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#686): Review scratch fd sets and scheduler polling.
        | Sysno::pselect6
        | Sysno::prlimit64
        | Sysno::pread64
        | Sysno::lseek
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#683): Confirm positional-write ordering and replay semantics.
        | Sysno::pwrite64
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#794): Scatter/gather I/O. readv/preadv/preadv2/
        // pwritev/pwritev2 are the vectored forms of read/pread64/pwrite64 and
        // writev (which are already determinized). They carry no host-varying or
        // host-global state beyond the file content the scalar forms already
        // handle, so they are determinized the same way: open-file resource
        // ordering plus record/replay of the single kernel operation (readv adds
        // the nonblocking-fd scheduler integration used by read/writev). Handled
        // by the typed match in lib.rs.
        | Sysno::readv
        | Sysno::preadv
        | Sysno::preadv2
        | Sysno::pwritev
        | Sysno::pwritev2
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
        | Sysno::setpgid
        | Sysno::setsid
        | Sysno::signalfd4
        | Sysno::socket
        | Sysno::socketpair
        | Sysno::statfs
        | Sysno::statx
        | Sysno::sysinfo
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::times
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
        | Sysno::utimensat
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
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#818): shutdown is the lone remaining Unsupported
        // member of the socket family (socket/bind/connect/listen/accept/
        // getsockname/getpeername/getsockopt/setsockopt/sendto/recvfrom/sendmsg/
        // recvmsg/sendmmsg/recvmmsg are all Determinized). It half-closes a
        // tracked socket's read and/or write direction; it never blocks, returns
        // no data, and its effect is deterministic given the container's socket
        // state, so handle_shutdown forwards it via record_or_replay exactly like
        // handle_listen/handle_setsockopt (KVM ratchet round 12).
        | Sysno::shutdown
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-874): Review deterministic seccomp probe
        // validation. Hermit returns Linux argument-validation errors for
        // capability probes but refuses real filters because they can block
        // ptrace-runtime syscall injection.
        // QEMU probes SECCOMP_FILTER_FLAG_TSYNC with a NULL filter. Return the
        // Linux validation errno without installing an unenforceable filter.
        | Sysno::seccomp
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
        | Sysno::lookup_dcookie
        | Sysno::nfsservctl
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
        | Sysno::pivot_root
        | Sysno::sethostname
        | Sysno::setdomainname
        | Sysno::vhangup
        | Sysno::quotactl
        | Sysno::quotactl_fd
        // TODO-HUMAN-REVIEW(#547)
        | Sysno::writev
        // ===== BATCH 3: NUMA memory policy and Linux CPU-scheduling policy =====
        // Detcore presents a kernel built without NUMA
        // (https://github.com/rrnewton/hermit/issues/5269): the six
        // memory-policy syscalls are determinized to the fixed `ENOSYS` a
        // `!CONFIG_NUMA` kernel's `cond_syscall` stubs return
        // (`is_memory_policy_syscall`, dispatched in lib.rs before the typed
        // match). Detcore replaces the Linux scheduler with its own, so the
        // scheduling-policy calls below are likewise determinized, to the
        // fixed answers their handlers in lib.rs give. Left as passthrough
        // either family's results would depend on host NUMA topology, host
        // scheduler state, and privilege (all nondeterministic).
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
        | Sysno::open_tree_attr
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
        // TODO-HUMAN-REVIEW(PR-836): Deterministic ENOSYS for host mount
        // introspection. sysfs exposes the host filesystem-type table, while
        // statmount/listmount expose host mount IDs and topology. Returning the
        // pre-feature ENOSYS result keeps guests on portable /proc fallbacks and
        // avoids leaking changing host namespace state.
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::statmount
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::listmount
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
        // the typed match below.
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
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-876): Review the deterministic
        // feature-absence boundary for guest performance monitoring.
        | Sysno::perf_event_open
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-882): Review the deterministic
        // feature-absence boundary for legacy nonlinear memory mappings.
        | Sysno::remap_file_pages
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-859): Extend the System V ENOSYS boundary to
        // semaphore and shared-memory objects whose host IDs and state are not
        // represented in Detcore.
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::semctl
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::semget
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::semop
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::semtimedop
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::shmat
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::shmctl
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::shmdt
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::shmget
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#827): Deterministic ENOSYS for the Landlock
        // unprivileged-sandbox syscalls (landlock_create_ruleset,
        // landlock_add_rule, landlock_restrict_self). Landlock is an LSM whose
        // presence and ABI version depend on the host kernel build
        // (CONFIG_SECURITY_LANDLOCK) and its runtime LSM stacking, so forwarding
        // these (the legacy pass-through) makes a guest that probes or installs
        // a sandbox behave differently across hosts -- a host dependency and,
        // because a ruleset restricts the whole thread tree, a global-state
        // isolation hole. A fixed -ENOSYS is exactly the errno a kernel built
        // without Landlock returns, so a guest sees a consistent "sandbox
        // unavailable" answer (the common best-effort path) regardless of host;
        // it is never forwarded to the host and is bitwise-identical across
        // --verify and record/replay, mirroring the io_uring / async-IPC ENOSYS
        // refusals above. These are untyped (Syscall::Other) in the pinned
        // Reverie, so the dispatcher matches on the Sysno before the typed match
        // below.
        | Sysno::landlock_create_ruleset
        | Sysno::landlock_add_rule
        | Sysno::landlock_restrict_self
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-848): Deterministic ENOSYS for the unmodeled
        // kernel-keyring family. Key serials, quotas, permissions, contents,
        // and request-key upcalls are shared kernel state outside Detcore's
        // model. Presenting a kernel-without-CONFIG_KEYS boundary keeps feature
        // probes host-independent and prevents guest keyring mutations/upcalls.
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::add_key
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::request_key
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::keyctl
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-838): Review close_range descriptor-table
        // synchronization.
        // The close_range handler serializes the kernel operation and removes the
        // same descriptor slots from Detcore's model.
        | Sysno::close_range
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-838): Review regular-file sendfile mediation.
        // The handler serializes destination writes and forwards the copy through
        // record/replay; unsupported endpoint types receive ENOSYS so callers can
        // fall back to determinized read/write loops.
        | Sysno::sendfile
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-887): copy_file_range availability and behavior
        // depend on the host kernel and filesystem pair. Return fixed ENOSYS so
        // callers take their portable read/write fallback.
        | Sysno::copy_file_range
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-855): Fail-closed refusal for zero-copy pipe
        // transfers: EINVAL for splice and tee, ENOSYS for vmsplice. Detcore
        // does not model kernel pipe-buffer ownership or vmsplice page pinning.
        // Fail-closed runs expose the portable read/write fallback boundary;
        // the explicit `hermit run --allow-unsupported-syscalls` opt-out
        // forwards these calls to the host. Record and replay always fail
        // closed, so they always return the refusal.
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::splice
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::tee
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::vmsplice
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-860): Deterministic ENOSYS for host security
        // probes. Detcore does not model host LSM attributes.
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::lsm_get_self_attr
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::lsm_set_self_attr
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-899): Opaque filesystem handles and mount IDs
        // remain outside Detcore's filesystem identity model.
        | Sysno::name_to_handle_at
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-844): Deterministic EPERM for host-global
        // process accounting and cross-process memory access. Detcore does not
        // model host process-accounting state or translate/synchronize target
        // address spaces for process_vm_readv/writev. Forwarding these calls
        // would expose host capabilities, PIDs, lifetimes, and memory, so the
        // deterministic container refuses them at its process boundary.
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::acct
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::process_vm_readv
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::process_vm_writev
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-839): Deterministic ENOSYS for optional memory
        // features that Detcore does not model. These APIs depend on kernel
        // generation/configuration, CET support, or pidfd process lifecycle.
        // Returning the pre-feature errno keeps guest behavior host-independent
        // and lets feature-probing callers use their portable fallbacks.
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::memfd_secret
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::process_mrelease
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::map_shadow_stack
        // TODO-HUMAN-REVIEW(PR-847): Deterministic ENOSYS for host-kernel
        // feature and state probes that Detcore does not model. BPF operations
        // depend on kernel configuration, capabilities, and security policy;
        // cachestat exposes mutable host page-cache residency; and
        // lsm_list_modules exposes the host's active security stack. A fixed
        // ENOSYS presents a stable unavailable-feature boundary and lets callers
        // take their normal fallback paths.
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::bpf
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::cachestat
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::lsm_list_modules
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-853): Deterministic EPERM for privileged host
        // observation/control interfaces. Nested ptrace and kcmp expose host
        // PIDs, permissions, and kernel-object identity.
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::ptrace
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::kcmp
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-862): Review pidfd creation and descriptor-model
        // synchronization. The handler records/replays the kernel operation and
        // registers the result as FdType::Pidfd before later fd operations.
        | Sysno::pidfd_open
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-1175): pidfd_send_signal/pidfd_getfd operate on
        // a pidfd that names one specific process fixed at pidfd_open time, so
        // there is no numeric-PID ambiguity to resolve. Delivery/duplication runs
        // inside the serialized scheduler turn (like tgkill), and the handlers add
        // deterministic argument validation (EBADF for a non-pidfd descriptor,
        // EINVAL for the kernel-reserved nonzero flags) before forwarding.
        // pidfd_getfd models exact OFD aliasing for self targets and fails closed
        // for other targets because their source cache cannot be invalidated.
        // Untyped (Syscall::Other) in the pinned Reverie, so dispatch on the Sysno.
        | Sysno::pidfd_send_signal
        | Sysno::pidfd_getfd
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::readlinkat
        // ===== BATCH 51: fail-closed utility syscalls with no deterministic effect =====
        // These previously fail-closed --strict (aborting real programs such as
        // chrt, ionice, and flock) even though a thread's I/O priority
        // (ioprio_get/ioprio_set) and its Linux scheduling attributes
        // (sched_getattr) cannot change guest-visible computation under Hermit:
        // Detcore replaces the Linux scheduler, presents a single virtual CPU,
        // and serializes guest threads, so those two are genuinely inert and are
        // determinized to fixed, host-independent results.
        //
        // flock is NOT inert and is no longer treated as such. This comment used
        // to claim "an advisory whole-file lock is never contended within the
        // serialized container", and that is false: serializing guest threads
        // stops them EXECUTING simultaneously, it does not stop their lock HOLD
        // INTERVALS from overlapping. A holder that is descheduled -- because it
        // blocked, forked, or used up its timeslice -- keeps holding while
        // another process runs and observes the lock. Under the old no-op,
        // measured on both ptrace and DBI, two processes held the same LOCK_EX
        // at once where native returned EWOULDBLOCK. flock is now forwarded to
        // the kernel (see handle_flock), exactly as fcntl already forwards POSIX
        // record locks.
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#791)
        | Sysno::flock
        | Sysno::ioprio_set
        | Sysno::sched_getattr
        // TODO-HUMAN-REVIEW(PR-857): Review deterministic NTP and kernel-log
        // virtualization. Query-only timex calls report Hermit's virtual clock
        // and an unsynchronized discipline, while mutation modes are refused.
        // syslog exposes an empty ring buffer and never reads host dmesg state.
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::adjtimex
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::clock_adjtime
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::syslog
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-881): Return fixed raw/effective I/O priority defaults.
        | Sysno::ioprio_get
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-841): Linux scheduler attributes are inoperative under Detcore.
        | Sysno::sched_setattr
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#787): BATCH 38. openat2(2) is a modern superset of
        // openat(2); callers are required to fall back to openat on ENOSYS
        // (kernels before 5.6 lack it). Detcore already determinizes openat, so a
        // fixed -ENOSYS for openat2 routes those programs (for example GNU tar's
        // safe directory traversal) onto the determinized openat path with no
        // host dependency and identical behavior across --verify and
        // record/replay. The credential-setting family (setuid/setgid and their
        // re-/res-/fs- variants, and setgroups) is inoperative under Detcore's
        // fixed virtual-root identity: getuid/geteuid/getgid/getegid are
        // virtualized to 0 and Detcore never tracks a credential change, so these
        // succeed as deterministic no-ops (return 0) instead of fail-closing.
        // Success is the errno a real root process sees for these calls, it lets
        // privilege-dropping programs (newgrp/sg/su and daemons) proceed, and it
        // is bitwise-identical across runs. All are untyped (Syscall::Other) in
        // the pinned Reverie, so the dispatcher matches on the Sysno before the
        // typed match below.
        | Sysno::openat2
        | Sysno::setuid
        | Sysno::setgid
        | Sysno::setresuid
        | Sysno::setresgid
        | Sysno::setreuid
        | Sysno::setregid
        | Sysno::setgroups
        | Sysno::setfsuid
        | Sysno::setfsgid
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#1549): Determinize the credential
        // *query* family so it actually implements the fixed virtual-root
        // identity the setuid no-ops above already assume. Previously
        // getuid/geteuid/getgid/getegid/getresuid/getresgid were pass-through
        // and returned 0 only because the ptrace backend runs the guest inside a
        // CLONE_NEWUSER namespace (hermit-cli/src/lib.rs) that maps the real uid
        // to 0. In-process backends (DBT) have no such namespace, so pass-through
        // leaked the host uid/gid and contradicted the credential model.
        // Emulating the query side to the constant virtual-root identity (0)
        // makes the result backend-independent, matches the ptrace golden
        // reference, and is never forwarded to the host, so it is
        // bitwise-identical across --verify and record/replay.
        | Sysno::getuid
        | Sysno::geteuid
        | Sysno::getgid
        | Sysno::getegid
        | Sysno::getresuid
        | Sysno::getresgid
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#1851): Complete the fixed virtual-root identity by
        // emulating file-ownership MUTATION too. The query family above and the
        // credential-setting family are already virtualized to root; leaving the
        // chown family as pass-through contradicted that model, because the host
        // identity backing the guest is not root (`--no-namespace`: EPERM even
        // for 0:0) or is root in a one-uid user namespace (`--tmp=/tmp`: EINVAL
        // for any unmapped uid), and in-process backends have no namespace at
        // all. See is_ownership_change_noop_syscall for the full rationale and
        // the stated semantic boundary. Dispatched by Sysno in lib.rs.
        | Sysno::fchown
        | Sysno::fchownat => SyscallClassification::Determinized,
        // The x86_64-only entry points of the groups above. arm64 has only
        // their generic-table successors (openat, pipe2, ppoll, pselect6,
        // epoll_pwait, dup3, fchownat, readlinkat, ...), classified above.
        #[cfg(target_arch = "x86_64")]
        Sysno::alarm
        | Sysno::arch_prctl
        | Sysno::creat
        | Sysno::dup2
        | Sysno::epoll_create
        | Sysno::epoll_ctl_old
        | Sysno::epoll_wait
        | Sysno::epoll_wait_old
        | Sysno::eventfd
        | Sysno::fork
        | Sysno::futimesat
        | Sysno::getdents
        | Sysno::inotify_init
        | Sysno::lstat
        | Sysno::newfstatat
        | Sysno::open
        | Sysno::pause
        | Sysno::pipe
        | Sysno::poll
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#800): select is the timeval sibling of pselect6.
        | Sysno::select
        | Sysno::signalfd
        | Sysno::stat
        | Sysno::time
        | Sysno::utime
        | Sysno::utimes
        | Sysno::vfork
        // Unimplemented on x86_64 (sys_ni_syscall): the ENOSYS group above.
        | Sysno::_sysctl
        | Sysno::afs_syscall
        | Sysno::create_module
        | Sysno::get_kernel_syms
        | Sysno::getpmsg
        | Sysno::putpmsg
        | Sysno::query_module
        | Sysno::security
        | Sysno::tuxcall
        | Sysno::uselib
        | Sysno::vserver
        // Privileged system administration: the EPERM group above.
        | Sysno::ioperm
        | Sysno::iopl
        // Host mount introspection: the ENOSYS group above.
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::sysfs
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-859): Deterministic ENOSYS for obsolete ustat
        // host-filesystem capacity and free-space introspection.
        | Sysno::ustat
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#877): Canonicalize fresh kernel namespace inode
        // identities while preserving ordinary symlink behavior and errors.
        | Sysno::readlink
        // File-ownership mutation: the virtual-root group above.
        | Sysno::chown
        | Sysno::lchown => SyscallClassification::Determinized,
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-b3a4c): Process groups and sessions are answered
        // from Detcore's model on every backend, not by the backend's kernel or
        // executor: they differed (DBT's launcher makes the guest a group
        // leader, KVM's executor failed getsid; slice B3a-4c of
        // https://github.com/rrnewton/hermit/issues/4266). Dispatched in lib.rs.
        Sysno::getpgid | Sysno::getsid => SyscallClassification::Determinized,
        #[cfg(target_arch = "x86_64")]
        Sysno::getpgrp => SyscallClassification::Determinized,

        // ===== BEGIN PASS-THRU SYSCALLS =====
        // These existing and triaged passthroughs are conditionally repeatable under
        // Hermit's fixed-container, stable-filesystem, and serialization assumptions.
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#503): Confirm the stable-state boundary for these promotions.
        Sysno::brk
        | Sysno::getcwd
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#663)
        | Sysno::getppid
        | Sysno::mprotect
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-889): Review process-local robust-list
        // queries under fixed guest address and PID namespaces.
        | Sysno::get_robust_list
        | Sysno::set_robust_list
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
        | Sysno::fchmodat
        | Sysno::linkat
        | Sysno::mkdirat
        | Sysno::renameat2
        | Sysno::symlinkat
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
        | Sysno::munlock
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::munlockall
        // These synchronous extent and pathname operations are repeatable for guest-owned
        // files in a fixed namespace with adequate space and no external mutation.
        // TODO-HUMAN-REVIEW(PR-675): Verify stable-filesystem passthrough assumptions.
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::fallocate
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
        | Sysno::fgetxattr
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::flistxattr
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::fremovexattr
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::fsetxattr
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
        | Sysno::sync_file_range
        // Ptrace executes rt_sigreturn directly; DBT has dedicated injected-sigreturn
        // handling, while KVM deterministically reports its current lack of signal support.
        | Sysno::rt_sigreturn => SyscallClassification::PassThrough,
        #[cfg(target_arch = "x86_64")]
        Sysno::access
        | Sysno::chmod
        | Sysno::mkdir
        | Sysno::rmdir
        | Sysno::unlink
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::rename
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::link
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::symlink => SyscallClassification::PassThrough,
        // ===== END PASS-THRU SYSCALLS =====

        // ===== ISSUE-REVIEWED PASS-THROUGH SYSCALLS =====
        // Every matching classification issue recommends PASS-THRU. These remain
        // conditional on Hermit's fixed-container and stable-state assumptions.
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-643): Review issue-backed pass-through promotions.
        Sysno::chroot
        | Sysno::mknodat
        | Sysno::mlock
        | Sysno::mlock2
        | Sysno::mlockall
        | Sysno::personality
        | Sysno::pkey_alloc
        | Sysno::pkey_free
        | Sysno::pkey_mprotect
        | Sysno::sched_get_priority_max
        | Sysno::sched_get_priority_min
        | Sysno::sync
        | Sysno::syncfs
        => SyscallClassification::PassThrough,
        #[cfg(target_arch = "x86_64")]
        Sysno::get_thread_area
        | Sysno::mknod
        | Sysno::modify_ldt
        | Sysno::set_thread_area => SyscallClassification::PassThrough,
        // ===== END ISSUE-REVIEWED PASS-THROUGH SYSCALLS =====

        // ===== UNSUPPORTED SYSCALLS =====
        // These require a deterministic handler or further investigation. Normal mode
        // records their use for an aggregate warning and preserves legacy forwarding;
        // --panic-on-unsupported-syscalls stops at the first use.
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-643): Review issue-backed unsupported classifications.
        Sysno::restart_syscall
        | Sysno::file_getattr
        | Sysno::file_setattr
        | Sysno::getxattrat
        | Sysno::listxattrat
        | Sysno::mseal
        | Sysno::removexattrat
        | Sysno::setxattrat
        => SyscallClassification::Unsupported,
        #[cfg(target_arch = "x86_64")]
        Sysno::uprobe | Sysno::uretprobe => SyscallClassification::Unsupported,
        // ===== END UNSUPPORTED SYSCALLS =====

        // ===== AARCH64-ONLY TABLE ENTRIES =====
        // `fstatat` is x86_64's `newfstatat` (number 79 in the generic table)
        // and takes its x86_64 twin's classification.
        #[cfg(target_arch = "aarch64")]
        Sysno::fstatat => SyscallClassification::Determinized,
        // The 32-bit `*_time64` entry points are in the generic table but the
        // 64-bit arm64 kernel does not wire them up, so Detcore refuses them
        // with the kernel's own ENOSYS (see is_unimplemented_enosys_syscall).
        #[cfg(target_arch = "aarch64")]
        Sysno::clock_adjtime64
        | Sysno::clock_getres_time64
        | Sysno::clock_gettime64
        | Sysno::clock_nanosleep_time64
        | Sysno::clock_settime64
        | Sysno::futex_time64
        | Sysno::io_pgetevents_time64
        | Sysno::mq_timedreceive_time64
        | Sysno::mq_timedsend_time64
        | Sysno::ppoll_time64
        | Sysno::pselect6_time64
        | Sysno::recvmmsg_time64
        | Sysno::rt_sigtimedwait_time64
        | Sysno::sched_rr_get_interval_time64
        | Sysno::semtimedop_time64
        | Sysno::timer_gettime64
        | Sysno::timer_settime64
        | Sysno::timerfd_gettime64
        | Sysno::timerfd_settime64
        | Sysno::utimensat_time64 => SyscallClassification::Determinized,
        // ===== END AARCH64-ONLY TABLE ENTRIES =====

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
    match sysno {
        Sysno::lookup_dcookie | Sysno::nfsservctl => true,
        #[cfg(target_arch = "x86_64")]
        Sysno::_sysctl
        | Sysno::afs_syscall
        | Sysno::create_module
        | Sysno::get_kernel_syms
        | Sysno::getpmsg
        | Sysno::putpmsg
        | Sysno::query_module
        | Sysno::security
        | Sysno::tuxcall
        | Sysno::uselib
        | Sysno::vserver => true,
        // The generic table's 32-bit `*_time64` numbers, which a 64-bit arm64
        // kernel leaves unwired.
        #[cfg(target_arch = "aarch64")]
        Sysno::clock_adjtime64
        | Sysno::clock_getres_time64
        | Sysno::clock_gettime64
        | Sysno::clock_nanosleep_time64
        | Sysno::clock_settime64
        | Sysno::futex_time64
        | Sysno::io_pgetevents_time64
        | Sysno::mq_timedreceive_time64
        | Sysno::mq_timedsend_time64
        | Sysno::ppoll_time64
        | Sysno::pselect6_time64
        | Sysno::recvmmsg_time64
        | Sysno::rt_sigtimedwait_time64
        | Sysno::sched_rr_get_interval_time64
        | Sysno::semtimedop_time64
        | Sysno::timer_gettime64
        | Sysno::timer_settime64
        | Sysno::timerfd_gettime64
        | Sysno::timerfd_settime64
        | Sysno::utimensat_time64 => true,
        _ => false,
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-852): Deterministic futex2 ENOSYS refusal set.
/// Detcore models the established `futex(2)` ABI but not futex2's vector waits,
/// sized values, or requeue rules. Refusing the complete family with `ENOSYS`
/// matches kernels without the relevant interfaces and directs feature-probing
/// runtimes to their legacy-futex fallback without exposing the host kernel
/// version or blocking outside Detcore's scheduler.
pub(crate) const fn is_futex2_enosys_syscall(sysno: Sysno) -> bool {
    matches!(
        sysno,
        Sysno::futex_requeue | Sysno::futex_wait | Sysno::futex_waitv | Sysno::futex_wake
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
    match sysno {
        Sysno::init_module
        | Sysno::finit_module
        | Sysno::delete_module
        | Sysno::kexec_load
        | Sysno::kexec_file_load
        | Sysno::reboot
        | Sysno::swapon
        | Sysno::swapoff
        | Sysno::pivot_root
        | Sysno::sethostname
        | Sysno::setdomainname
        | Sysno::vhangup
        | Sysno::quotactl
        | Sysno::quotactl_fd => true,
        // Raw I/O port access exists only on x86.
        #[cfg(target_arch = "x86_64")]
        Sysno::ioperm | Sysno::iopl => true,
        _ => false,
    }
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
            | Sysno::open_tree_attr
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
// TODO-HUMAN-REVIEW(#787): Deterministic no-op success set.
/// Credential-setting syscalls (`setuid`/`setgid` and their `re-`, `res-`, and
/// `fs-` variants, and `setgroups`). Detcore presents a fixed virtual-root
/// identity: `getuid`/`geteuid`/`getgid`/`getegid` are virtualized to `0` and
/// Detcore never tracks a credential change for the guest. These calls are
/// therefore inoperative under that model, so Detcore accepts them as
/// deterministic no-ops returning `0` (for `setfsuid`/`setfsgid`, the previous
/// fs-id, which is the virtual `0`). Success is the errno a real root process
/// receives for these calls, it lets privilege-dropping programs
/// (`newgrp`/`sg`/`su` and daemons) continue instead of fail-closing, and it is
/// never forwarded to the host and bitwise-identical across `--verify` and
/// record/replay. These are untyped (`Syscall::Other`) in the pinned Reverie, so
/// the dispatcher matches on the `Sysno` before the typed match.
pub(crate) const fn is_credential_identity_noop_syscall(sysno: Sysno) -> bool {
    matches!(
        sysno,
        Sysno::setuid
            | Sysno::setgid
            | Sysno::setresuid
            | Sysno::setresgid
            | Sysno::setreuid
            | Sysno::setregid
            | Sysno::setgroups
            | Sysno::setfsuid
            | Sysno::setfsgid
    )
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#1851): File-ownership mutation completed to match the
// fixed virtual-root identity.
/// The `chown` family: `chown`, `fchown`, `fchownat`, `lchown`.
///
/// Detcore presents a **fixed virtual-root identity** — the credential *query*
/// family is emulated to `0` (#1549) and the credential *set* family succeeds as
/// a no-op (#787) — but ownership mutation was left as the one pass-through
/// member of that model, and pass-through contradicts it. A real root process's
/// `chown` to any uid succeeds; the guest instead receives the errno of whatever
/// identity the backend happens to run under:
///
/// * `--no-namespace`: no user namespace at all, so **`EPERM`** even for
///   `chown(path, 0, 0)`, and `stat` reports the real host uid while `getuid`
///   reports 0 — the model is already incoherent there.
/// * `--tmp=/tmp`: a user namespace whose `uid_map` is `0 <caller-uid> 1`, i.e.
///   exactly ONE mapped id. `chown(path, 0, 0)` succeeds, but **`EINVAL`** for
///   any other uid because it is unmapped.
/// * in-process backends (DBI) have no namespace at all, so the answer differs
///   per backend — the same host dependency #1549 removed from the query side.
///
/// Emulating them to a no-op success is the same choice already made for the
/// credential-setting family, for the same reasons: `0` is the value a real root
/// process gets for a permitted ownership change, the result is never forwarded
/// to the host, and it is backend-independent and bitwise-identical across
/// `--verify` and record/replay.
///
/// **Semantic boundary, stated explicitly.** Detcore does not model per-file
/// ownership, so a no-op success is not observable through a later `stat` the
/// way a real `chown` would be. Under `--tmp=/tmp` this is invisible for the
/// dominant case: every file the guest creates already reads back as `0:0`
/// (the user namespace maps the caller to 0), so `chown(path, 0, 0)` agrees with
/// the read-back exactly. It diverges only when a guest chowns to a *foreign*
/// uid and then reads the owner back, which a single-uid container cannot
/// represent in any case. That divergence is strictly smaller than the status
/// quo, where the guest believes it is root and yet cannot chown at all.
pub(crate) const fn is_ownership_change_noop_syscall(sysno: Sysno) -> bool {
    match sysno {
        Sysno::fchown | Sysno::fchownat => true,
        #[cfg(target_arch = "x86_64")]
        Sysno::chown | Sysno::lchown => true,
        _ => false,
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#731): Deterministic ENOSYS refusal set.
/// Asynchronous and message-passing I/O and IPC interfaces Detcore does not
/// model: Linux native AIO (`io_setup`/`io_destroy`/`io_submit`/`io_cancel`/
/// `io_getevents`/`io_pgetevents`), POSIX message queues (`mq_*`), and all three
/// System V IPC families (`msg*`, `sem*`, and `shm*`). AIO completion is
/// kernel-driven and lives outside the guest's logical time, and the IPC
/// families operate on global, key-addressed kernel objects that persist across
/// runs and are shared with the whole host. Forwarding them (the legacy
/// pass-through) is nondeterministic and a container-isolation hole, so Detcore
/// refuses them with a fixed `ENOSYS`: exactly the errno a kernel built without AIO,
/// `CONFIG_POSIX_MQUEUE`, or `CONFIG_SYSVIPC` returns. The result is never
/// forwarded to the host and is bitwise-identical across `--verify` and
/// record/replay, mirroring the existing `io_uring` ENOSYS refusal. These are
/// untyped (`Syscall::Other`) in the pinned Reverie, so the dispatcher matches
/// on the `Sysno` before the typed match.
// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-859): Review the System V semaphore/shared-memory
// capability boundary added to the existing deterministic IPC refusal set.
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
            // AUTONOMOUS-BOT-IMPLEMENTED
            | Sysno::semctl
            // AUTONOMOUS-BOT-IMPLEMENTED
            | Sysno::semget
            // AUTONOMOUS-BOT-IMPLEMENTED
            | Sysno::semop
            // AUTONOMOUS-BOT-IMPLEMENTED
            | Sysno::semtimedop
            // AUTONOMOUS-BOT-IMPLEMENTED
            | Sysno::shmat
            // AUTONOMOUS-BOT-IMPLEMENTED
            | Sysno::shmctl
            // AUTONOMOUS-BOT-IMPLEMENTED
            | Sysno::shmdt
            // AUTONOMOUS-BOT-IMPLEMENTED
            | Sysno::shmget
    )
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-882): Review the deterministic feature-absence
// boundary for legacy nonlinear memory mappings.
/// The obsolete `remap_file_pages` interface creates nonlinear VMA layouts
/// whose availability and implementation vary by host kernel. Detcore does not
/// model page-offset remapping, so it exposes the fixed `ENOSYS` result used by
/// kernels without the legacy interface. Callers can use the documented `mmap`
/// fallback without importing host VMA behavior into a deterministic run.
pub(crate) const fn is_remap_file_pages_enosys_syscall(sysno: Sysno) -> bool {
    matches!(
        sysno,
        // AUTONOMOUS-BOT-IMPLEMENTED
        Sysno::remap_file_pages
    )
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#827): Deterministic ENOSYS refusal set.
/// The Landlock unprivileged-sandbox syscalls (`landlock_create_ruleset`,
/// `landlock_add_rule`, `landlock_restrict_self`). Landlock is an LSM whose
/// availability and ABI version depend on the host kernel build
/// (`CONFIG_SECURITY_LANDLOCK`) and its runtime LSM stacking, so forwarding
/// these to the host (the legacy pass-through) makes a guest that probes or
/// installs a sandbox behave differently across hosts, and -- because a
/// ruleset restricts the whole thread tree -- opens a global-state isolation
/// hole. Detcore refuses them with a fixed `ENOSYS`: exactly the errno a kernel
/// built without Landlock returns, so the guest sees a consistent "sandbox
/// unavailable" answer (the common best-effort path) independent of the host.
/// The result is never forwarded to the host and is bitwise-identical across
/// `--verify` and record/replay, mirroring the io_uring and async-IPC ENOSYS
/// refusals. These are untyped (`Syscall::Other`) in the pinned Reverie, so the
/// dispatcher matches on the `Sysno` before the typed match.
pub(crate) const fn is_landlock_sandbox_syscall(sysno: Sysno) -> bool {
    matches!(
        sysno,
        Sysno::landlock_create_ruleset | Sysno::landlock_add_rule | Sysno::landlock_restrict_self
    )
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-844): Deterministic process-isolation refusal set.
/// Host-global process accounting and cross-process memory operations that
/// Detcore deliberately refuses. `acct` mutates system-wide accounting and
/// writes to a host-selected file. `process_vm_readv` and `process_vm_writev`
/// address another process through host PIDs and lifetime/permission state that
/// Detcore does not model. A fixed `EPERM` enforces the guest process boundary,
/// does not depend on host capabilities or Yama/LSM policy, and never exposes or
/// mutates host process state.
pub(crate) const fn is_process_isolation_refused_syscall(sysno: Sysno) -> bool {
    matches!(
        sysno,
        // AUTONOMOUS-BOT-IMPLEMENTED
        Sysno::acct
            // AUTONOMOUS-BOT-IMPLEMENTED
            | Sysno::process_vm_readv
            // AUTONOMOUS-BOT-IMPLEMENTED
            | Sysno::process_vm_writev
    )
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-853): Deterministic privileged-observation refusal set.
/// Privileged host observation and control interfaces that Detcore deliberately
/// refuses. Nested `ptrace` and `kcmp` depend on untranslated host PIDs,
/// permissions, process lifetimes, and kernel-object identity. A fixed `EPERM`
/// enforces the guest isolation boundary without entering the host.
pub(crate) const fn is_privileged_observation_refused_syscall(sysno: Sysno) -> bool {
    matches!(
        sysno,
        // AUTONOMOUS-BOT-IMPLEMENTED
        Sysno::ptrace
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::kcmp
    )
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-876): Review the deterministic feature-absence
// boundary for guest performance monitoring.
/// Performance monitoring depends on host PMU hardware, kernel configuration,
/// `perf_event_paranoid`, and process capabilities. Detcore does not model
/// counter state, overflow delivery, or perf-event file descriptors, so guest
/// `perf_event_open` calls receive the fixed `ENOSYS` result used by kernels
/// built without performance-event support. Hermit's host-side scheduler PMU
/// remains outside the guest syscall path and is unaffected.
pub(crate) const fn is_perf_event_enosys_syscall(sysno: Sysno) -> bool {
    matches!(
        sysno,
        // AUTONOMOUS-BOT-IMPLEMENTED
        Sysno::perf_event_open
    )
}

/// The NUMA memory-policy syscalls. Detcore presents a kernel built without
/// NUMA (https://github.com/rrnewton/hermit/issues/5269): Linux compiles
/// `mm/mempolicy.o` only under `CONFIG_NUMA` (mm/Makefile) and the six entry
/// points are `cond_syscall` stubs (`kernel/sys_ni.c`), so each returns
/// `ENOSYS` before any argument is looked at — their compat entry points
/// are the same stubs. The dispatcher refuses all six at one gate, by
/// `Sysno`, so the
/// typed calls and the untyped `set_mempolicy_home_node` take the same
/// answer on every backend, and libnuma's probe (`get_mempolicy`) reports
/// NUMA unavailable exactly as it does on such a kernel.
pub(crate) const fn is_memory_policy_syscall(sysno: Sysno) -> bool {
    matches!(
        sysno,
        Sysno::set_mempolicy
            | Sysno::get_mempolicy
            | Sysno::mbind
            | Sysno::migrate_pages
            | Sysno::move_pages
            | Sysno::set_mempolicy_home_node
    )
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-836): Deterministic ENOSYS refusal set.
/// Host filesystem and mount-introspection syscalls. `sysfs` reads the host's
/// filesystem-type table, `statmount` and `listmount` read mount IDs and
/// topology, and obsolete `ustat` reads live capacity counters by host device
/// number. None of these sources is part of a deterministic guest's modeled
/// state, so forwarding them would expose changing host data. A fixed `ENOSYS`
/// directs callers to portable `/proc` or `statfs` fallbacks.
pub(crate) const fn is_mount_introspection_enosys_syscall(sysno: Sysno) -> bool {
    match sysno {
        Sysno::statmount | Sysno::listmount => true,
        #[cfg(target_arch = "x86_64")]
        Sysno::sysfs => true,
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-859): Obsolete host filesystem statistics.
        #[cfg(target_arch = "x86_64")]
        Sysno::ustat => true,
        _ => false,
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-860): Host security/filesystem identity refusal set.
/// Host-backed security probes. LSM self-attributes depend on the kernel's
/// configured and active security modules, so report the standard
/// feature-absence errno and direct callers to portable fallbacks.
pub(crate) const fn is_host_security_identity_probe_syscall(sysno: Sysno) -> bool {
    matches!(
        sysno,
        // AUTONOMOUS-BOT-IMPLEMENTED
        Sysno::lsm_get_self_attr
            // AUTONOMOUS-BOT-IMPLEMENTED
            | Sysno::lsm_set_self_attr
    )
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-855): Fail-closed zero-copy pipe fallback set.
/// Linux zero-copy pipe transfers. Their observable blocking and buffer
/// ownership depend on kernel pipe state, while `vmsplice` can additionally
/// pin guest pages beyond the syscall boundary. Fail-closed runs refuse
/// `splice` and `tee` with `EINVAL` (the descriptor pair cannot be spliced),
/// which is what callers such as GNU grep fall back to read/write on, and
/// `vmsplice` with `ENOSYS`. The explicit compatibility opt-out retains host
/// forwarding.
pub(crate) const fn is_zero_copy_pipe_syscall(sysno: Sysno) -> bool {
    matches!(
        sysno,
        // AUTONOMOUS-BOT-IMPLEMENTED
        Sysno::splice
            // AUTONOMOUS-BOT-IMPLEMENTED
            | Sysno::tee
            // AUTONOMOUS-BOT-IMPLEMENTED
            | Sysno::vmsplice
    )
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-848): Deterministic kernel-keyring refusal set.
/// Kernel key-management syscalls that Detcore does not model. Keyring serials,
/// quotas, permissions, contents, and `request_key` user-space upcalls expose
/// shared host-kernel state. A fixed `ENOSYS` matches a kernel built without
/// `CONFIG_KEYS`, lets feature probes take their established fallback, and
/// prevents the guest from reading, mutating, or triggering host keyrings.
pub(crate) const fn is_kernel_keyring_syscall(sysno: Sysno) -> bool {
    matches!(
        sysno,
        // AUTONOMOUS-BOT-IMPLEMENTED
        Sysno::add_key
            // AUTONOMOUS-BOT-IMPLEMENTED
            | Sysno::request_key
            // AUTONOMOUS-BOT-IMPLEMENTED
            | Sysno::keyctl
    )
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-839): Deterministic ENOSYS refusal set.
/// Optional modern memory features that are outside Detcore's model.
/// `memfd_secret` depends on secret-memory kernel configuration,
/// `map_shadow_stack` depends on CET support, and `process_mrelease` operates
/// on a dying process through a pidfd. A fixed `ENOSYS` matches kernels from
/// before each feature was introduced and lets feature probes use established
/// fallback paths without exposing host configuration or process lifecycle.
pub(crate) const fn is_optional_memory_feature_syscall(sysno: Sysno) -> bool {
    matches!(
        sysno,
        Sysno::memfd_secret | Sysno::process_mrelease | Sysno::map_shadow_stack
    )
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-847): Deterministic host-kernel probe refusal set.
/// Host feature and mutable-state probes that Detcore deliberately presents as
/// unavailable. Forwarding these would expose kernel BPF support and security
/// policy, live page-cache residency, or the host LSM stack. A fixed `ENOSYS`
/// is host-independent and gives feature-detecting callers their normal
/// unavailable-kernel fallback.
pub(crate) const fn is_host_kernel_probe_syscall(sysno: Sysno) -> bool {
    matches!(
        sysno,
        // AUTONOMOUS-BOT-IMPLEMENTED
        Sysno::bpf
            // AUTONOMOUS-BOT-IMPLEMENTED
            | Sysno::cachestat
            // AUTONOMOUS-BOT-IMPLEMENTED
            | Sysno::lsm_list_modules
    )
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-978): Review the aggregate deterministic-refusal
// boundary used to enforce the same fixed refusal in backends that execute
// guest syscalls outside Detcore's `handle_syscall_event` dispatcher.
/// True for every syscall Detcore refuses with a fixed errno under the
/// fail-closed dispatcher policy *without consulting the host* -- the
/// deterministic-refusal
/// boundary. Classifying a syscall this way in the ptrace path only enforces the
/// isolation boundary where the shared Detcore dispatcher runs. A backend that
/// lets a guest execute a syscall natively outside that dispatcher (most
/// importantly the DBT copied pre-exec child fast path, which runs on the
/// DynamoRIO client stack with no Detcore tool) would otherwise reach the host
/// and leak or mutate the very state the classification is meant to hide. Such a
/// backend must enforce the same refusal whenever fail-closed policy is active.
///
/// This is the union of the individual family predicates whose dispatch arm in
/// `Detcore::handle_syscall_event` returns `Err(Error::Errno(..))` unconditionally
/// or under `panic_on_unsupported_syscalls`. It deliberately excludes families
/// Detcore *emulates* with real semantics (for example the credential-identity
/// no-ops, `timer_create`, and the socket-identity handlers), because failing a
/// copied child closed for an emulated syscall would diverge from the ptrace
/// path rather than match it. Keep this in sync with those dispatch arms.
pub(crate) const fn is_strict_only_deterministic_refusal_syscall(sysno: Sysno) -> bool {
    // These families preserve native behavior only under the explicit
    // compatibility opt-out, and fail closed otherwise.
    matches!(sysno, Sysno::rseq)
        || is_zero_copy_pipe_syscall(sysno)
        || is_kernel_keyring_syscall(sysno)
}

pub(crate) const fn is_deterministically_refused_syscall(sysno: Sysno) -> bool {
    is_strict_only_deterministic_refusal_syscall(sysno)
        || is_unimplemented_enosys_syscall(sysno)
        || is_futex2_enosys_syscall(sysno)
        || is_privileged_admin_refused_syscall(sysno)
        || is_mount_ns_admin_refused_syscall(sysno)
        || is_unsupported_async_ipc_syscall(sysno)
        || is_remap_file_pages_enosys_syscall(sysno)
        || is_landlock_sandbox_syscall(sysno)
        || is_process_isolation_refused_syscall(sysno)
        || is_privileged_observation_refused_syscall(sysno)
        || is_perf_event_enosys_syscall(sysno)
        || is_memory_policy_syscall(sysno)
        || is_mount_introspection_enosys_syscall(sysno)
        || is_host_security_identity_probe_syscall(sysno)
        || is_optional_memory_feature_syscall(sysno)
        || is_host_kernel_probe_syscall(sysno)
        || match sysno {
            Sysno::openat2
            | Sysno::copy_file_range
            | Sysno::clock_settime
            | Sysno::io_uring_setup
            | Sysno::io_uring_enter
            | Sysno::io_uring_register
            | Sysno::name_to_handle_at => true,
            #[cfg(target_arch = "x86_64")]
            Sysno::futimesat | Sysno::epoll_ctl_old => true,
            _ => false,
        }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    // ===== REVIEWED CLASSIFICATION MEMBERSHIP =====
    //
    // These lists replace a hand-transcribed `[289, 83, 1]` count triple.
    //
    // The triple could only see the SIZE of each class, so a COUNT-PRESERVING
    // SWAP -- moving one syscall out of `Determinized` and another in -- passed
    // it while changing what Detcore actually determinizes. In a determinism
    // engine a classification test that ratifies the wrong classification is
    // close to the worst kind of green.
    // `census_refuses_a_count_preserving_swap` plants exactly that case and
    // proves this check refuses it.
    //
    // The triple was also a shared mutable counter every reclassification PR had
    // to re-derive from its own base, which is both a guaranteed merge conflict
    // and a correctness trap: PR #1933 asserts a two-generations-stale
    // `[283, 89, 1]`, so it can pass its own census while disagreeing with
    // `main` about what is determinized. Per-name lists make a reclassification
    // a ONE-LINE edit in each of two lists, so independent PRs no longer collide
    // on a single number, and the diff names the syscall that moved.
    //
    // To regenerate after an INTENTIONAL reclassification, print
    // `census_by_class(classify_syscall)` and paste each sorted bucket. Do not
    // edit a list to make a red test green without saying which policy changed
    // and why -- that is the failure mode these lists exist to expose.
    //
    // A name only one architecture's table has carries that `target_arch`
    // cfg. x86_64 keeps legacy entry points (open, stat, poll, ...) that the
    // generic table aarch64 uses (include/uapi/asm-generic/unistd.h) omits.
    // One name is a rename, not an omission: aarch64 has newfstatat too,
    // which the syscalls crate calls fstatat there. The crate's aarch64 table
    // also has the generic table's 32-bit `*_time64` numbers, which a 64-bit
    // kernel leaves unwired.

    /// Syscalls Detcore models, replaces deterministically, or explicitly refuses.
    const DETERMINIZED_SYSCALLS: &[&str] = &[
        #[cfg(target_arch = "x86_64")]
        "_sysctl",
        "accept",
        "accept4",
        "acct",
        "add_key",
        "adjtimex",
        #[cfg(target_arch = "x86_64")]
        "afs_syscall",
        #[cfg(target_arch = "x86_64")]
        "alarm",
        #[cfg(target_arch = "x86_64")]
        "arch_prctl",
        "bind",
        "bpf",
        "cachestat",
        #[cfg(target_arch = "x86_64")]
        "chown",
        "clock_adjtime",
        #[cfg(target_arch = "aarch64")]
        "clock_adjtime64",
        "clock_getres",
        #[cfg(target_arch = "aarch64")]
        "clock_getres_time64",
        "clock_gettime",
        #[cfg(target_arch = "aarch64")]
        "clock_gettime64",
        "clock_nanosleep",
        #[cfg(target_arch = "aarch64")]
        "clock_nanosleep_time64",
        "clock_settime",
        #[cfg(target_arch = "aarch64")]
        "clock_settime64",
        "clone",
        "clone3",
        "close",
        "close_range",
        "connect",
        "copy_file_range",
        #[cfg(target_arch = "x86_64")]
        "creat",
        #[cfg(target_arch = "x86_64")]
        "create_module",
        "delete_module",
        "dup",
        #[cfg(target_arch = "x86_64")]
        "dup2",
        "dup3",
        #[cfg(target_arch = "x86_64")]
        "epoll_create",
        "epoll_create1",
        "epoll_ctl",
        #[cfg(target_arch = "x86_64")]
        "epoll_ctl_old",
        "epoll_pwait",
        "epoll_pwait2",
        #[cfg(target_arch = "x86_64")]
        "epoll_wait",
        #[cfg(target_arch = "x86_64")]
        "epoll_wait_old",
        #[cfg(target_arch = "x86_64")]
        "eventfd",
        "eventfd2",
        "execve",
        "execveat",
        "exit",
        "exit_group",
        "fadvise64",
        "fanotify_init",
        "fanotify_mark",
        "fchown",
        "fchownat",
        "fcntl",
        "finit_module",
        "flock",
        #[cfg(target_arch = "x86_64")]
        "fork",
        "fsconfig",
        "fsmount",
        "fsopen",
        "fspick",
        "fstat",
        #[cfg(target_arch = "aarch64")]
        "fstatat",
        "fstatfs",
        "futex",
        "futex_requeue",
        #[cfg(target_arch = "aarch64")]
        "futex_time64",
        "futex_wait",
        "futex_waitv",
        "futex_wake",
        #[cfg(target_arch = "x86_64")]
        "futimesat",
        #[cfg(target_arch = "x86_64")]
        "get_kernel_syms",
        "get_mempolicy",
        "getcpu",
        #[cfg(target_arch = "x86_64")]
        "getdents",
        "getdents64",
        "getegid",
        "geteuid",
        "getgid",
        "getitimer",
        "getpeername",
        "getpgid",
        #[cfg(target_arch = "x86_64")]
        "getpgrp",
        "getpid",
        #[cfg(target_arch = "x86_64")]
        "getpmsg",
        "getpriority",
        "getrandom",
        "getresgid",
        "getresuid",
        "getrlimit",
        "getrusage",
        "getsid",
        "getsockname",
        "getsockopt",
        "gettid",
        "gettimeofday",
        "getuid",
        "init_module",
        "inotify_add_watch",
        #[cfg(target_arch = "x86_64")]
        "inotify_init",
        "inotify_init1",
        "inotify_rm_watch",
        "io_cancel",
        "io_destroy",
        "io_getevents",
        "io_pgetevents",
        #[cfg(target_arch = "aarch64")]
        "io_pgetevents_time64",
        "io_setup",
        "io_submit",
        "io_uring_enter",
        "io_uring_register",
        "io_uring_setup",
        "ioctl",
        #[cfg(target_arch = "x86_64")]
        "ioperm",
        #[cfg(target_arch = "x86_64")]
        "iopl",
        "ioprio_get",
        "ioprio_set",
        "kcmp",
        "kexec_file_load",
        "kexec_load",
        "keyctl",
        "kill",
        "landlock_add_rule",
        "landlock_create_ruleset",
        "landlock_restrict_self",
        #[cfg(target_arch = "x86_64")]
        "lchown",
        "listen",
        "listmount",
        "lookup_dcookie",
        "lseek",
        "lsm_get_self_attr",
        "lsm_list_modules",
        "lsm_set_self_attr",
        #[cfg(target_arch = "x86_64")]
        "lstat",
        "madvise",
        "map_shadow_stack",
        "mbind",
        "membarrier",
        "memfd_create",
        "memfd_secret",
        "migrate_pages",
        "mincore",
        "mmap",
        "mount",
        "mount_setattr",
        "move_mount",
        "move_pages",
        "mq_getsetattr",
        "mq_notify",
        "mq_open",
        "mq_timedreceive",
        #[cfg(target_arch = "aarch64")]
        "mq_timedreceive_time64",
        "mq_timedsend",
        #[cfg(target_arch = "aarch64")]
        "mq_timedsend_time64",
        "mq_unlink",
        "mremap",
        "msgctl",
        "msgget",
        "msgrcv",
        "msgsnd",
        "munmap",
        "name_to_handle_at",
        "nanosleep",
        #[cfg(target_arch = "x86_64")]
        "newfstatat",
        "nfsservctl",
        #[cfg(target_arch = "x86_64")]
        "open",
        "open_by_handle_at",
        "open_tree",
        "open_tree_attr",
        "openat",
        "openat2",
        #[cfg(target_arch = "x86_64")]
        "pause",
        "perf_event_open",
        "pidfd_getfd",
        "pidfd_open",
        "pidfd_send_signal",
        #[cfg(target_arch = "x86_64")]
        "pipe",
        "pipe2",
        "pivot_root",
        #[cfg(target_arch = "x86_64")]
        "poll",
        "ppoll",
        #[cfg(target_arch = "aarch64")]
        "ppoll_time64",
        "prctl",
        "pread64",
        "preadv",
        "preadv2",
        "prlimit64",
        "process_madvise",
        "process_mrelease",
        "process_vm_readv",
        "process_vm_writev",
        "pselect6",
        #[cfg(target_arch = "aarch64")]
        "pselect6_time64",
        "ptrace",
        #[cfg(target_arch = "x86_64")]
        "putpmsg",
        "pwrite64",
        "pwritev",
        "pwritev2",
        #[cfg(target_arch = "x86_64")]
        "query_module",
        "quotactl",
        "quotactl_fd",
        "read",
        #[cfg(target_arch = "x86_64")]
        "readlink",
        "readlinkat",
        "readv",
        "reboot",
        "recvfrom",
        "recvmmsg",
        #[cfg(target_arch = "aarch64")]
        "recvmmsg_time64",
        "recvmsg",
        "remap_file_pages",
        "request_key",
        "rseq",
        "rt_sigaction",
        "rt_sigpending",
        "rt_sigprocmask",
        "rt_sigqueueinfo",
        "rt_sigsuspend",
        "rt_sigtimedwait",
        #[cfg(target_arch = "aarch64")]
        "rt_sigtimedwait_time64",
        "rt_tgsigqueueinfo",
        "sched_getaffinity",
        "sched_getattr",
        "sched_getparam",
        "sched_getscheduler",
        "sched_rr_get_interval",
        #[cfg(target_arch = "aarch64")]
        "sched_rr_get_interval_time64",
        "sched_setaffinity",
        "sched_setattr",
        "sched_setparam",
        "sched_setscheduler",
        "sched_yield",
        "seccomp",
        #[cfg(target_arch = "x86_64")]
        "security",
        #[cfg(target_arch = "x86_64")]
        "select",
        "semctl",
        "semget",
        "semop",
        "semtimedop",
        #[cfg(target_arch = "aarch64")]
        "semtimedop_time64",
        "sendfile",
        "sendmmsg",
        "sendmsg",
        "sendto",
        "set_mempolicy",
        "set_mempolicy_home_node",
        "setdomainname",
        "setfsgid",
        "setfsuid",
        "setgid",
        "setgroups",
        "sethostname",
        "setitimer",
        "setns",
        "setpgid",
        "setpriority",
        "setregid",
        "setresgid",
        "setresuid",
        "setreuid",
        "setrlimit",
        "setsid",
        "setsockopt",
        "settimeofday",
        "setuid",
        "shmat",
        "shmctl",
        "shmdt",
        "shmget",
        "shutdown",
        #[cfg(target_arch = "x86_64")]
        "signalfd",
        "signalfd4",
        "socket",
        "socketpair",
        "splice",
        #[cfg(target_arch = "x86_64")]
        "stat",
        "statfs",
        "statmount",
        "statx",
        "swapoff",
        "swapon",
        #[cfg(target_arch = "x86_64")]
        "sysfs",
        "sysinfo",
        "syslog",
        "tee",
        "tgkill",
        #[cfg(target_arch = "x86_64")]
        "time",
        "timer_create",
        "timer_delete",
        "timer_getoverrun",
        "timer_gettime",
        #[cfg(target_arch = "aarch64")]
        "timer_gettime64",
        "timer_settime",
        #[cfg(target_arch = "aarch64")]
        "timer_settime64",
        "timerfd_create",
        "timerfd_gettime",
        #[cfg(target_arch = "aarch64")]
        "timerfd_gettime64",
        "timerfd_settime",
        #[cfg(target_arch = "aarch64")]
        "timerfd_settime64",
        "times",
        "tkill",
        #[cfg(target_arch = "x86_64")]
        "tuxcall",
        "umount2",
        "uname",
        "unshare",
        #[cfg(target_arch = "x86_64")]
        "uselib",
        "userfaultfd",
        #[cfg(target_arch = "x86_64")]
        "ustat",
        #[cfg(target_arch = "x86_64")]
        "utime",
        "utimensat",
        #[cfg(target_arch = "aarch64")]
        "utimensat_time64",
        #[cfg(target_arch = "x86_64")]
        "utimes",
        #[cfg(target_arch = "x86_64")]
        "vfork",
        "vhangup",
        "vmsplice",
        #[cfg(target_arch = "x86_64")]
        "vserver",
        "wait4",
        "waitid",
        "write",
        "writev",
    ];

    /// Syscalls intentionally forwarded under documented container assumptions.
    const PASSTHROUGH_SYSCALLS: &[&str] = &[
        #[cfg(target_arch = "x86_64")]
        "access",
        "brk",
        "capget",
        "capset",
        "chdir",
        #[cfg(target_arch = "x86_64")]
        "chmod",
        "chroot",
        "faccessat",
        "faccessat2",
        "fallocate",
        "fchdir",
        "fchmod",
        "fchmodat",
        "fchmodat2",
        "fdatasync",
        "fgetxattr",
        "flistxattr",
        "fremovexattr",
        "fsetxattr",
        "fsync",
        "ftruncate",
        "get_robust_list",
        #[cfg(target_arch = "x86_64")]
        "get_thread_area",
        "getcwd",
        "getgroups",
        "getppid",
        "getxattr",
        "lgetxattr",
        #[cfg(target_arch = "x86_64")]
        "link",
        "linkat",
        "listxattr",
        "llistxattr",
        "lremovexattr",
        "lsetxattr",
        #[cfg(target_arch = "x86_64")]
        "mkdir",
        "mkdirat",
        #[cfg(target_arch = "x86_64")]
        "mknod",
        "mknodat",
        "mlock",
        "mlock2",
        "mlockall",
        #[cfg(target_arch = "x86_64")]
        "modify_ldt",
        "mprotect",
        "msync",
        "munlock",
        "munlockall",
        "personality",
        "pkey_alloc",
        "pkey_free",
        "pkey_mprotect",
        "readahead",
        "removexattr",
        #[cfg(target_arch = "x86_64")]
        "rename",
        "renameat",
        "renameat2",
        #[cfg(target_arch = "x86_64")]
        "rmdir",
        "rt_sigreturn",
        "sched_get_priority_max",
        "sched_get_priority_min",
        "set_robust_list",
        #[cfg(target_arch = "x86_64")]
        "set_thread_area",
        "set_tid_address",
        "setxattr",
        "sigaltstack",
        #[cfg(target_arch = "x86_64")]
        "symlink",
        "symlinkat",
        "sync",
        "sync_file_range",
        "syncfs",
        "truncate",
        "umask",
        #[cfg(target_arch = "x86_64")]
        "unlink",
        "unlinkat",
    ];

    /// Syscalls with no deterministic implementation, using the configured fallback.
    const UNSUPPORTED_SYSCALLS: &[&str] = &[
        "file_getattr",
        "file_setattr",
        "getxattrat",
        "listxattrat",
        "mseal",
        "removexattrat",
        "restart_syscall",
        "setxattrat",
        #[cfg(target_arch = "x86_64")]
        "uprobe",
        #[cfg(target_arch = "x86_64")]
        "uretprobe",
    ];

    /// Bucket every pinned syscall by `classify`, sorted by name.
    ///
    /// Parameterised on the classifier so the negative brackets below can plant
    /// a perturbed classification and prove the comparison refuses it. A census
    /// test that cannot fail is exactly what this replaces.
    fn census_by_class(classify: impl Fn(Sysno) -> SyscallClassification) -> [Vec<String>; 3] {
        let mut buckets: [Vec<String>; 3] = [Vec::new(), Vec::new(), Vec::new()];
        for sysno in all_pinned_syscalls() {
            let bucket = match classify(sysno) {
                SyscallClassification::Determinized => 0,
                SyscallClassification::PassThrough => 1,
                SyscallClassification::Unsupported => 2,
            };
            buckets[bucket].push(sysno.to_string());
        }
        for bucket in buckets.iter_mut() {
            bucket.sort();
        }
        buckets
    }

    /// Every syscall whose class differs from the reviewed membership, by name.
    ///
    /// Returns findings rather than asserting, so the negative brackets can
    /// assert on exactly which syscalls moved and in which direction.
    fn census_mismatches(buckets: &[Vec<String>; 3]) -> Vec<String> {
        let reviewed = [
            DETERMINIZED_SYSCALLS,
            PASSTHROUGH_SYSCALLS,
            UNSUPPORTED_SYSCALLS,
        ];
        let class_names = ["Determinized", "PassThrough", "Unsupported"];
        let mut findings = Vec::new();
        for index in 0..3 {
            let actual: BTreeSet<&str> = buckets[index].iter().map(String::as_str).collect();
            let want: BTreeSet<&str> = reviewed[index].iter().copied().collect();
            for name in want.difference(&actual) {
                findings.push(format!("{name}: left {}", class_names[index]));
            }
            for name in actual.difference(&want) {
                findings.push(format!("{name}: entered {}", class_names[index]));
            }
        }
        findings.sort();
        findings
    }

    #[test]
    fn every_pinned_sysno_has_an_explicit_classification() {
        let buckets = census_by_class(classify_syscall);

        let mismatches = census_mismatches(&buckets);
        assert!(
            mismatches.is_empty(),
            "syscall classification differs from the reviewed membership. Update the \
             lists deliberately, naming the policy change:\n  {}",
            mismatches.join("\n  ")
        );

        // The total is COMPUTED from the census, never transcribed.
        // `EXPECTED_SYSNO_COUNT` is itself bracketed against
        // `Sysno::count()` by the const assertion at the top of this file, so
        // this catches a syscalls-crate table change instead of restating a
        // number the census just produced.
        let counted: usize = buckets.iter().map(Vec::len).sum();
        assert_eq!(counted, EXPECTED_SYSNO_COUNT);

        // And the reviewed lists must themselves cover the whole table, so a
        // syscall cannot be dropped from every list and escape classification.
        let reviewed =
            DETERMINIZED_SYSCALLS.len() + PASSTHROUGH_SYSCALLS.len() + UNSUPPORTED_SYSCALLS.len();
        assert_eq!(reviewed, EXPECTED_SYSNO_COUNT);
    }

    #[test]
    fn census_refuses_a_moved_syscall() {
        // Plant one reclassification: futex Determinized -> PassThrough.
        let perturbed = |sysno: Sysno| {
            if sysno == Sysno::futex {
                SyscallClassification::PassThrough
            } else {
                classify_syscall(sysno)
            }
        };
        let findings = census_mismatches(&census_by_class(perturbed));
        assert_eq!(
            findings,
            vec![
                "futex: entered PassThrough".to_string(),
                "futex: left Determinized".to_string(),
            ],
            "the census must name the moved syscall and both directions"
        );
    }

    #[test]
    fn census_refuses_a_count_preserving_swap() {
        // THE CASE THE OLD `[289, 83, 1]` TRIPLE COULD NOT SEE.
        // Move one syscall out of Determinized and one in from PassThrough:
        // every class size is unchanged, so the count assertion passed while
        // Detcore's actual policy for two syscalls had changed.
        let perturbed = |sysno: Sysno| match sysno {
            Sysno::futex => SyscallClassification::PassThrough,
            Sysno::get_robust_list => SyscallClassification::Determinized,
            other => classify_syscall(other),
        };
        let buckets = census_by_class(perturbed);

        // Positive half of the bracket: confirm the swap really is
        // count-preserving, so this test proves the improvement rather than
        // just restating the negative above. Sizes come from the reviewed
        // lists, not from a transcribed triple.
        assert_eq!(
            [buckets[0].len(), buckets[1].len(), buckets[2].len()],
            [
                DETERMINIZED_SYSCALLS.len(),
                PASSTHROUGH_SYSCALLS.len(),
                UNSUPPORTED_SYSCALLS.len(),
            ],
            "the planted swap must leave all three class sizes unchanged"
        );

        // Membership is not preserved, and the census says so by name.
        let findings = census_mismatches(&buckets);
        assert_eq!(
            findings,
            vec![
                "futex: entered PassThrough".to_string(),
                "futex: left Determinized".to_string(),
                "get_robust_list: entered Determinized".to_string(),
                "get_robust_list: left PassThrough".to_string(),
            ]
        );
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
            SyscallClassification::Determinized
        );
        assert_eq!(
            classify_syscall(Sysno::ppoll),
            SyscallClassification::Determinized
        );
        // select is the timeval sibling of pselect6 and must stay Determinized
        // (routed through handle_select); regression for #800. x86_64 only:
        // aarch64 has pselect6 alone.
        #[cfg(target_arch = "x86_64")]
        assert_eq!(
            classify_syscall(Sysno::select),
            SyscallClassification::Determinized
        );
        assert_eq!(
            classify_syscall(Sysno::pselect6),
            SyscallClassification::Determinized
        );
        // x86_64 only: arch_prctl is an x86 syscall.
        #[cfg(target_arch = "x86_64")]
        assert_eq!(
            classify_syscall(Sysno::arch_prctl),
            SyscallClassification::Determinized
        );
        // aarch64 only: fstatat is the generic table's name for x86_64's
        // newfstatat.
        #[cfg(target_arch = "aarch64")]
        assert_eq!(
            classify_syscall(Sysno::fstatat),
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
        // mincore is deterministically emulated (all pages resident) rather than
        // aborting under --strict; it must not slip back to Unsupported.
        assert_eq!(
            classify_syscall(Sysno::mincore),
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
        for sysno in [
            Sysno::flock,
            Sysno::ioprio_get,
            Sysno::ioprio_set,
            Sysno::sched_getattr,
            Sysno::sched_setattr,
        ] {
            assert_eq!(classify_syscall(sysno), SyscallClassification::Determinized);
        }
        // Debug batch 184: these real-program blockers must remain on explicit
        // deterministic paths rather than falling back to strict fail-closed.
        for sysno in [Sysno::close_range, Sysno::seccomp, Sysno::sendfile] {
            assert_eq!(classify_syscall(sysno), SyscallClassification::Determinized);
        }
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-887)
        assert_eq!(
            classify_syscall(Sysno::copy_file_range),
            SyscallClassification::Determinized
        );
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#877)
        // x86_64 only: aarch64 has readlinkat alone.
        #[cfg(target_arch = "x86_64")]
        assert_eq!(
            classify_syscall(Sysno::readlink),
            SyscallClassification::Determinized
        );
        assert_eq!(
            classify_syscall(Sysno::readlinkat),
            SyscallClassification::Determinized
        );
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
        // shutdown is the lone remaining socket-family syscall; it must stay
        // Determinized (routed through handle_shutdown); regression for #818.
        assert_eq!(
            classify_syscall(Sysno::shutdown),
            SyscallClassification::Determinized
        );
        assert_eq!(
            classify_syscall(Sysno::seccomp),
            SyscallClassification::Determinized
        );
        for sysno in [
            Sysno::epoll_pwait2,
            Sysno::clock_settime,
            Sysno::getpeername,
            Sysno::getsockname,
            Sysno::getsockopt,
            Sysno::getitimer,
            Sysno::getpriority,
            Sysno::getrlimit,
            Sysno::kill,
            Sysno::listen,
            Sysno::prctl,
            Sysno::rt_sigpending,
            Sysno::setitimer,
            Sysno::setpgid,
            Sysno::setpriority,
            Sysno::process_madvise,
            Sysno::setrlimit,
            Sysno::setsockopt,
            Sysno::tgkill,
            Sysno::readv,
            Sysno::preadv,
            Sysno::preadv2,
            Sysno::pwritev,
            Sysno::pwritev2,
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
            // Names under the x86_64 cfg are absent from the aarch64 table,
            // which has only their *at successors.
            Sysno::capget,
            Sysno::capset,
            Sysno::chdir,
            #[cfg(target_arch = "x86_64")]
            Sysno::chmod,
            Sysno::faccessat,
            Sysno::faccessat2,
            Sysno::fchdir,
            Sysno::fchmod,
            Sysno::fchmodat,
            Sysno::fchmodat2,
            Sysno::fdatasync,
            Sysno::fallocate,
            Sysno::fgetxattr,
            Sysno::flistxattr,
            Sysno::fremovexattr,
            Sysno::fsetxattr,
            Sysno::ftruncate,
            Sysno::fsync,
            Sysno::munlock,
            Sysno::munlockall,
            #[cfg(target_arch = "x86_64")]
            Sysno::rename,
            Sysno::renameat,
            Sysno::getgroups,
            Sysno::getppid,
            Sysno::getxattr,
            Sysno::lgetxattr,
            #[cfg(target_arch = "x86_64")]
            Sysno::link,
            Sysno::linkat,
            Sysno::listxattr,
            Sysno::llistxattr,
            Sysno::lremovexattr,
            Sysno::lsetxattr,
            #[cfg(target_arch = "x86_64")]
            Sysno::mkdir,
            Sysno::mkdirat,
            Sysno::msync,
            Sysno::removexattr,
            Sysno::readahead,
            Sysno::renameat2,
            #[cfg(target_arch = "x86_64")]
            Sysno::rmdir,
            Sysno::rt_sigreturn,
            Sysno::setxattr,
            #[cfg(target_arch = "x86_64")]
            Sysno::symlink,
            Sysno::symlinkat,
            Sysno::sync_file_range,
            Sysno::truncate,
            Sysno::umask,
            #[cfg(target_arch = "x86_64")]
            Sysno::unlink,
            Sysno::unlinkat,
        ] {
            assert_eq!(classify_syscall(sysno), SyscallClassification::PassThrough);
        }
        for sysno in [Sysno::add_key, Sysno::keyctl, Sysno::request_key] {
            assert_eq!(classify_syscall(sysno), SyscallClassification::Determinized);
            assert!(is_kernel_keyring_syscall(sysno));
        }
        for sysno in [
            // Names under the x86_64 cfg are x86-only: the thread-area and LDT
            // calls are x86 syscalls, and aarch64 has mknodat alone.
            Sysno::chroot,
            #[cfg(target_arch = "x86_64")]
            Sysno::get_thread_area,
            #[cfg(target_arch = "x86_64")]
            Sysno::mknod,
            Sysno::mknodat,
            Sysno::mlock,
            Sysno::mlock2,
            Sysno::mlockall,
            #[cfg(target_arch = "x86_64")]
            Sysno::modify_ldt,
            Sysno::personality,
            Sysno::pkey_alloc,
            Sysno::pkey_free,
            Sysno::pkey_mprotect,
            Sysno::sched_get_priority_max,
            Sysno::sched_get_priority_min,
            #[cfg(target_arch = "x86_64")]
            Sysno::set_thread_area,
            Sysno::sync,
            Sysno::syncfs,
        ] {
            assert_eq!(classify_syscall(sysno), SyscallClassification::PassThrough);
        }
        // Batch 3: the NUMA memory-policy syscalls are determinized to the
        // `ENOSYS` of a kernel built without NUMA, and Linux CPU-scheduling
        // policy to fixed, host-independent results (Detcore scheduler).
        for sysno in [
            Sysno::mbind,
            Sysno::set_mempolicy,
            Sysno::get_mempolicy,
            Sysno::set_mempolicy_home_node,
            Sysno::migrate_pages,
            Sysno::move_pages,
        ] {
            assert_eq!(classify_syscall(sysno), SyscallClassification::Determinized);
            assert!(is_memory_policy_syscall(sysno), "{sysno:?}");
            assert!(
                is_deterministically_refused_syscall(sysno),
                "{sysno:?} should be in the deterministic-refusal aggregate"
            );
        }
        for sysno in [
            Sysno::sched_setscheduler,
            Sysno::sched_setparam,
            Sysno::sched_getscheduler,
            Sysno::sched_getparam,
            Sysno::sched_rr_get_interval,
        ] {
            assert_eq!(classify_syscall(sysno), SyscallClassification::Determinized);
            assert!(!is_memory_policy_syscall(sysno), "{sysno:?}");
        }
        // Batch 7: asynchronous and message-passing I/O and IPC interfaces
        // Detcore does not model are refused with a deterministic ENOSYS (Linux
        // native AIO, POSIX message queues, and all System V IPC families); see
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
            Sysno::semctl,
            Sysno::semget,
            Sysno::semop,
            Sysno::semtimedop,
            Sysno::shmat,
            Sysno::shmctl,
            Sysno::shmdt,
            Sysno::shmget,
        ] {
            assert_eq!(classify_syscall(sysno), SyscallClassification::Determinized);
            assert!(is_unsupported_async_ipc_syscall(sysno));
        }
    }

    #[test]
    fn clock_discipline_and_kernel_log_syscalls_are_determinized() {
        for sysno in [Sysno::adjtimex, Sysno::clock_adjtime, Sysno::syslog] {
            assert_eq!(classify_syscall(sysno), SyscallClassification::Determinized);
        }
    }

    #[test]
    fn pidfd_family_is_determinized() {
        // pidfd_send_signal and pidfd_getfd are now determinized alongside
        // pidfd_open: the pidfd names a fixed process (no numeric-PID ambiguity),
        // signal delivery forwards through the serialized turn, and getfd either
        // models a self-target OFD alias or refuses an unmodelable foreign source.
        for sysno in [
            Sysno::pidfd_open,
            Sysno::pidfd_getfd,
            Sysno::pidfd_send_signal,
        ] {
            assert_eq!(classify_syscall(sysno), SyscallClassification::Determinized);
        }
    }

    #[test]
    fn get_robust_list_is_a_reviewed_passthrough() {
        assert_eq!(
            classify_syscall(Sysno::get_robust_list),
            SyscallClassification::PassThrough
        );
        assert_eq!(
            classify_syscall(Sysno::set_robust_list),
            SyscallClassification::PassThrough
        );
    }

    #[test]
    fn unimplemented_enosys_syscalls_are_determinized_and_consistent() {
        // Every syscall in the deterministic ENOSYS-refusal set must classify as
        // Determinized, and the helper used by the dispatcher must agree exactly
        // with that classification across the whole pinned table.
        let mut refused = vec![Sysno::lookup_dcookie, Sysno::nfsservctl];
        // x86_64 only: these are absent from the aarch64 table.
        #[cfg(target_arch = "x86_64")]
        refused.extend([
            Sysno::_sysctl,
            Sysno::afs_syscall,
            Sysno::create_module,
            Sysno::get_kernel_syms,
            Sysno::getpmsg,
            Sysno::putpmsg,
            Sysno::query_module,
            Sysno::security,
            Sysno::tuxcall,
            Sysno::uselib,
            Sysno::vserver,
        ]);
        // aarch64 only: the generic table's 32-bit `*_time64` numbers, which a
        // 64-bit arm64 kernel leaves unwired.
        #[cfg(target_arch = "aarch64")]
        refused.extend([
            Sysno::clock_adjtime64,
            Sysno::clock_getres_time64,
            Sysno::clock_gettime64,
            Sysno::clock_nanosleep_time64,
            Sysno::clock_settime64,
            Sysno::futex_time64,
            Sysno::io_pgetevents_time64,
            Sysno::mq_timedreceive_time64,
            Sysno::mq_timedsend_time64,
            Sysno::ppoll_time64,
            Sysno::pselect6_time64,
            Sysno::recvmmsg_time64,
            Sysno::rt_sigtimedwait_time64,
            Sysno::sched_rr_get_interval_time64,
            Sysno::semtimedop_time64,
            Sysno::timer_gettime64,
            Sysno::timer_settime64,
            Sysno::timerfd_gettime64,
            Sysno::timerfd_settime64,
            Sysno::utimensat_time64,
        ]);
        for &sysno in &refused {
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
    fn futex2_syscalls_are_determinized_and_consistent() {
        for sysno in [
            Sysno::futex_requeue,
            Sysno::futex_wait,
            Sysno::futex_waitv,
            Sysno::futex_wake,
        ] {
            assert_eq!(classify_syscall(sysno), SyscallClassification::Determinized);
            assert!(is_futex2_enosys_syscall(sysno));
        }
        assert!(!is_futex2_enosys_syscall(Sysno::futex));
    }

    #[test]
    fn batch38_openat2_and_credential_syscalls_are_determinized() {
        // BATCH 38: openat2 is determinized (dispatcher returns a fixed ENOSYS so
        // callers fall back to the already-determinized openat), and the
        // credential-setting family is determinized to a no-op success under the
        // fixed virtual-root identity. All must classify as Determinized so they
        // no longer fail-close under --strict.
        assert_eq!(
            classify_syscall(Sysno::openat2),
            SyscallClassification::Determinized,
            "openat2 should be Determinized (deterministic ENOSYS fallback)"
        );
        let credentials = [
            Sysno::setuid,
            Sysno::setgid,
            Sysno::setresuid,
            Sysno::setresgid,
            Sysno::setreuid,
            Sysno::setregid,
            Sysno::setgroups,
            Sysno::setfsuid,
            Sysno::setfsgid,
        ];
        for sysno in credentials {
            assert_eq!(
                classify_syscall(sysno),
                SyscallClassification::Determinized,
                "{sysno:?} should be Determinized (deterministic no-op success)"
            );
            assert!(
                is_credential_identity_noop_syscall(sysno),
                "{sysno:?} should be in the credential no-op helper set"
            );
        }
        // The credential *query* family is the other half of the same fixed
        // virtual-root identity: the setters above are no-ops precisely because
        // the getters answer a constant 0. Determinizing the queries is what
        // makes that identity backend-independent — under ptrace the guest
        // already saw 0 via the container's CLONE_NEWUSER uid map
        // (`map_root()`), but an in-process backend has no such namespace, so a
        // pass-through leaked the host uid. These are asserted here, not folded
        // into `credentials`, because they are emulated reads rather than no-op
        // writes and so must stay outside `is_credential_identity_noop_syscall`
        // (the exclusivity check below would otherwise reject them).
        let credential_queries = [
            Sysno::getuid,
            Sysno::geteuid,
            Sysno::getgid,
            Sysno::getegid,
            Sysno::getresuid,
            Sysno::getresgid,
        ];
        for sysno in credential_queries {
            assert_eq!(
                classify_syscall(sysno),
                SyscallClassification::Determinized,
                "{sysno:?} should be Determinized (emulated virtual-root identity)"
            );
            assert!(
                !is_credential_identity_noop_syscall(sysno),
                "{sysno:?} is a credential query, not a credential-setting no-op"
            );
        }
        // The credential helper must not claim any syscall outside the set, and
        // must never overlap the ENOSYS or EPERM refusal helpers.
        for sysno in Sysno::iter().chain(std::iter::once(Sysno::last())) {
            if is_credential_identity_noop_syscall(sysno) {
                assert!(
                    credentials.contains(&sysno),
                    "{sysno:?} is flagged by the credential helper but not in the reviewed set"
                );
                assert!(!is_unimplemented_enosys_syscall(sysno));
                assert!(!is_privileged_admin_refused_syscall(sysno));
                assert!(!is_mount_ns_admin_refused_syscall(sysno));
                assert!(!is_unsupported_async_ipc_syscall(sysno));
            }
        }
    }

    #[test]
    fn landlock_sandbox_syscalls_are_determinized_and_consistent() {
        // Every Landlock sandbox syscall must classify as Determinized (routed
        // to the deterministic ENOSYS refusal), and the helper used by the
        // dispatcher must agree exactly with that classification across the
        // whole pinned table. Regression for #827.
        let refused = [
            Sysno::landlock_create_ruleset,
            Sysno::landlock_add_rule,
            Sysno::landlock_restrict_self,
        ];
        for sysno in refused {
            assert_eq!(
                classify_syscall(sysno),
                SyscallClassification::Determinized,
                "{sysno:?} should be Determinized (deterministic ENOSYS refusal)"
            );
            assert!(
                is_landlock_sandbox_syscall(sysno),
                "{sysno:?} should be in the Landlock ENOSYS-refusal helper set"
            );
        }
        // The helper must not claim any syscall outside the reviewed set.
        for sysno in Sysno::iter().chain(std::iter::once(Sysno::last())) {
            if is_landlock_sandbox_syscall(sysno) {
                assert!(
                    refused.contains(&sysno),
                    "{sysno:?} is flagged by the helper but not in the reviewed refusal set"
                );
            }
        }
    }

    #[test]
    fn mount_introspection_syscalls_are_determinized_and_consistent() {
        let refused = [
            // x86_64 only: sysfs and ustat are absent from the aarch64 table.
            #[cfg(target_arch = "x86_64")]
            Sysno::sysfs,
            Sysno::statmount,
            Sysno::listmount,
            #[cfg(target_arch = "x86_64")]
            Sysno::ustat,
        ];
        for sysno in refused {
            assert_eq!(
                classify_syscall(sysno),
                SyscallClassification::Determinized,
                "{sysno:?} should be Determinized (deterministic ENOSYS refusal)"
            );
            assert!(
                is_mount_introspection_enosys_syscall(sysno),
                "{sysno:?} should be in the mount-introspection refusal set"
            );
        }

        for sysno in Sysno::iter().chain(std::iter::once(Sysno::last())) {
            assert_eq!(
                is_mount_introspection_enosys_syscall(sysno),
                refused.contains(&sysno),
                "{sysno:?} mount-introspection helper membership is inconsistent"
            );
        }
    }

    #[test]
    fn host_security_identity_probes_are_determinized_and_consistent() {
        let refused = [Sysno::lsm_get_self_attr, Sysno::lsm_set_self_attr];
        for sysno in refused {
            assert_eq!(
                classify_syscall(sysno),
                SyscallClassification::Determinized,
                "{sysno:?} should be Determinized (deterministic ENOSYS refusal)"
            );
            assert!(
                is_host_security_identity_probe_syscall(sysno),
                "{sysno:?} should be in the host security/identity refusal set"
            );
        }

        for sysno in Sysno::iter().chain(std::iter::once(Sysno::last())) {
            assert_eq!(
                is_host_security_identity_probe_syscall(sysno),
                refused.contains(&sysno),
                "{sysno:?} host security/identity helper membership is inconsistent"
            );
        }
    }

    #[test]
    fn filesystem_handle_export_is_determinized() {
        assert_eq!(
            classify_syscall(Sysno::name_to_handle_at),
            SyscallClassification::Determinized
        );
    }

    #[test]
    fn zero_copy_pipe_syscalls_are_determinized_and_consistent() {
        let syscalls = [Sysno::splice, Sysno::tee, Sysno::vmsplice];
        for sysno in syscalls {
            assert_eq!(
                classify_syscall(sysno),
                SyscallClassification::Determinized,
                "{sysno:?} should be Determinized"
            );
            assert!(
                is_zero_copy_pipe_syscall(sysno),
                "{sysno:?} should be in the zero-copy pipe set"
            );
        }

        for sysno in Sysno::iter().chain(std::iter::once(Sysno::last())) {
            assert_eq!(
                is_zero_copy_pipe_syscall(sysno),
                syscalls.contains(&sysno),
                "{sysno:?} zero-copy pipe helper membership is inconsistent"
            );
        }
    }

    #[test]
    fn kernel_keyring_syscalls_are_determinized_and_consistent() {
        let refused = [Sysno::add_key, Sysno::request_key, Sysno::keyctl];
        for sysno in refused {
            assert_eq!(
                classify_syscall(sysno),
                SyscallClassification::Determinized,
                "{sysno:?} should be Determinized (deterministic ENOSYS refusal)"
            );
            assert!(
                is_kernel_keyring_syscall(sysno),
                "{sysno:?} should be in the kernel-keyring refusal set"
            );
        }

        for sysno in Sysno::iter().chain(std::iter::once(Sysno::last())) {
            assert_eq!(
                is_kernel_keyring_syscall(sysno),
                refused.contains(&sysno),
                "{sysno:?} kernel-keyring helper membership is inconsistent"
            );
        }
    }

    #[test]
    fn optional_memory_feature_syscalls_are_determinized_and_consistent() {
        let refused = [
            Sysno::memfd_secret,
            Sysno::process_mrelease,
            Sysno::map_shadow_stack,
        ];
        for sysno in refused {
            assert_eq!(
                classify_syscall(sysno),
                SyscallClassification::Determinized,
                "{sysno:?} should be Determinized (deterministic ENOSYS refusal)"
            );
            assert!(
                is_optional_memory_feature_syscall(sysno),
                "{sysno:?} should be in the optional-memory refusal set"
            );
        }

        for sysno in Sysno::iter().chain(std::iter::once(Sysno::last())) {
            assert_eq!(
                is_optional_memory_feature_syscall(sysno),
                refused.contains(&sysno),
                "{sysno:?} optional-memory helper membership is inconsistent"
            );
        }
    }

    #[test]
    fn host_kernel_probe_syscalls_are_determinized_and_consistent() {
        let probes = [Sysno::bpf, Sysno::cachestat, Sysno::lsm_list_modules];
        for sysno in probes {
            assert_eq!(
                classify_syscall(sysno),
                SyscallClassification::Determinized,
                "{sysno:?} should use deterministic ENOSYS"
            );
            assert!(is_host_kernel_probe_syscall(sysno));
        }

        for sysno in Sysno::iter().chain(std::iter::once(Sysno::last())) {
            assert_eq!(
                is_host_kernel_probe_syscall(sysno),
                probes.contains(&sysno),
                "{sysno:?} host-kernel probe membership is inconsistent"
            );
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
            // x86_64 only: raw I/O port access exists only on x86.
            #[cfg(target_arch = "x86_64")]
            Sysno::ioperm,
            #[cfg(target_arch = "x86_64")]
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
    fn process_isolation_syscalls_are_determinized_and_consistent() {
        let refused = [
            Sysno::acct,
            Sysno::process_vm_readv,
            Sysno::process_vm_writev,
        ];
        for sysno in refused {
            assert_eq!(
                classify_syscall(sysno),
                SyscallClassification::Determinized,
                "{sysno:?} should be Determinized (deterministic EPERM refusal)"
            );
            assert!(
                is_process_isolation_refused_syscall(sysno),
                "{sysno:?} should be in the process-isolation refusal set"
            );
        }

        for sysno in Sysno::iter().chain(std::iter::once(Sysno::last())) {
            assert_eq!(
                is_process_isolation_refused_syscall(sysno),
                refused.contains(&sysno),
                "{sysno:?} process-isolation helper membership is inconsistent"
            );
        }
    }

    #[test]
    fn privileged_observation_syscalls_are_determinized_and_consistent() {
        let refused = [Sysno::ptrace, Sysno::kcmp];
        for sysno in refused {
            assert_eq!(
                classify_syscall(sysno),
                SyscallClassification::Determinized,
                "{sysno:?} should be Determinized (deterministic EPERM refusal)"
            );
            assert!(
                is_privileged_observation_refused_syscall(sysno),
                "{sysno:?} should be in the privileged-observation refusal set"
            );
        }

        for sysno in Sysno::iter().chain(std::iter::once(Sysno::last())) {
            assert_eq!(
                is_privileged_observation_refused_syscall(sysno),
                refused.contains(&sysno),
                "{sysno:?} privileged-observation helper membership is inconsistent"
            );
        }
    }

    #[test]
    fn perf_event_open_is_deterministically_unavailable() {
        assert_eq!(
            classify_syscall(Sysno::perf_event_open),
            SyscallClassification::Determinized
        );
        assert!(is_perf_event_enosys_syscall(Sysno::perf_event_open));

        for sysno in Sysno::iter().chain(std::iter::once(Sysno::last())) {
            assert_eq!(
                is_perf_event_enosys_syscall(sysno),
                sysno == Sysno::perf_event_open,
                "{sysno:?} perf-event helper membership is inconsistent"
            );
        }
    }

    #[test]
    fn memory_policy_syscalls_are_determinized_and_consistent() {
        // The six NUMA memory-policy syscalls of a kernel built without
        // NUMA: each is a `cond_syscall` stub there, so each is refused
        // with ENOSYS at the one dispatch gate, and the helper must agree
        // exactly with that set across the whole pinned table
        // (https://github.com/rrnewton/hermit/issues/5269).
        let refused = [
            Sysno::set_mempolicy,
            Sysno::get_mempolicy,
            Sysno::mbind,
            Sysno::migrate_pages,
            Sysno::move_pages,
            Sysno::set_mempolicy_home_node,
        ];
        for sysno in refused {
            assert_eq!(
                classify_syscall(sysno),
                SyscallClassification::Determinized,
                "{sysno:?} should be Determinized (deterministic ENOSYS refusal)"
            );
            assert!(
                is_memory_policy_syscall(sysno),
                "{sysno:?} should be in the memory-policy refusal set"
            );
        }
        for sysno in Sysno::iter().chain(std::iter::once(Sysno::last())) {
            assert_eq!(
                is_memory_policy_syscall(sysno),
                refused.contains(&sysno),
                "{sysno:?} memory-policy helper membership is inconsistent"
            );
        }
        // `getcpu` is not one of them: a kernel without NUMA still answers
        // it, with node 0.
        assert!(!is_memory_policy_syscall(Sysno::getcpu));
    }

    #[test]
    fn remap_file_pages_is_deterministically_unavailable() {
        assert_eq!(
            classify_syscall(Sysno::remap_file_pages),
            SyscallClassification::Determinized
        );
        assert!(is_remap_file_pages_enosys_syscall(Sysno::remap_file_pages));

        for sysno in Sysno::iter().chain(std::iter::once(Sysno::last())) {
            assert_eq!(
                is_remap_file_pages_enosys_syscall(sysno),
                sysno == Sysno::remap_file_pages,
                "{sysno:?} remap-file-pages helper membership is inconsistent"
            );
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
            Sysno::open_tree_attr,
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

    #[test]
    fn deterministic_refusal_aggregate_includes_fixed_error_families() {
        // The aggregate predicate is the union of the refusal families the
        // fail-closed dispatcher rejects with a fixed errno, without consulting
        // the host. Most refuse with ENOSYS or EPERM; splice and tee refuse
        // with EINVAL and vmsplice with ENOSYS.
        // A backend that executes guest syscalls outside Detcore's dispatcher
        // consults this predicate to enforce the same fixed refusal. The DBT
        // copied pre-exec child is the only production consumer; KVM delivers
        // these syscalls to the dispatcher. Representative members of every
        // family must be present.
        let refused_members = [
            Sysno::rseq,
            Sysno::perf_event_open,
            Sysno::splice,
            Sysno::tee,
            Sysno::vmsplice,
            Sysno::keyctl,
            Sysno::add_key,
            Sysno::request_key,
            Sysno::landlock_create_ruleset,
            Sysno::get_mempolicy,
            Sysno::mount,
            Sysno::remap_file_pages,
            Sysno::name_to_handle_at,
            Sysno::openat2,
            Sysno::copy_file_range,
            Sysno::clock_settime,
            // x86_64 only: futimesat and epoll_ctl_old are absent from the
            // aarch64 table.
            #[cfg(target_arch = "x86_64")]
            Sysno::futimesat,
            Sysno::io_uring_setup,
            Sysno::io_uring_enter,
            Sysno::io_uring_register,
            #[cfg(target_arch = "x86_64")]
            Sysno::epoll_ctl_old,
        ];
        for sysno in refused_members {
            assert!(
                is_deterministically_refused_syscall(sysno),
                "{sysno:?} should be in the deterministic-refusal aggregate"
            );
        }

        // Emulated / no-op / host-forwarding families must be EXCLUDED: fail-
        // closing a copied child for these would diverge from the ptrace path,
        // which emulates or forwards them rather than refusing.
        let excluded = [
            Sysno::setuid,       // credential no-op success
            Sysno::setgid,       // credential no-op success
            Sysno::timer_create, // emulated
            Sysno::bind,         // AF_UNIX autobind emulated
            Sysno::read,         // ordinary passthrough
            Sysno::write,        // ordinary passthrough
            Sysno::getpid,       // virtualized
        ];
        for sysno in excluded {
            assert!(
                !is_deterministically_refused_syscall(sysno),
                "{sysno:?} must NOT be in the deterministic-refusal aggregate (it is emulated/forwarded)"
            );
        }

        for sysno in [
            Sysno::rseq,
            Sysno::splice,
            Sysno::tee,
            Sysno::vmsplice,
            Sysno::keyctl,
            Sysno::add_key,
            Sysno::request_key,
        ] {
            assert!(is_strict_only_deterministic_refusal_syscall(sysno));
        }
        for sysno in [Sysno::openat2, Sysno::perf_event_open, Sysno::clock_settime] {
            assert!(!is_strict_only_deterministic_refusal_syscall(sysno));
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#1851)
    #[test]
    fn ownership_change_family_is_determinized_not_passthrough() {
        // POSITIVE: exactly the four ownership-mutation syscalls are in the
        // predicate AND are classified Determinized, so the guard on the lib.rs
        // dispatch arm would be satisfied for them.
        //
        // WHAT THIS TEST CANNOT SEE, stated so nobody reads it as more coverage
        // than it is: it pins MEMBERSHIP, not the arm's RESULT. Measured -- with
        // the arm's body mutated from the emulated success to Err(EPERM), and
        // again with the whole arm deleted, this test stays GREEN. The
        // behavioural contract lives in
        // hermit-cli/tests/chown_virtual_root_identity.rs, which fails under
        // both of those mutations. Do not let this test stand in for it.
        for sysno in [
            // x86_64 only: aarch64 has fchown and fchownat alone.
            #[cfg(target_arch = "x86_64")]
            Sysno::chown,
            Sysno::fchown,
            Sysno::fchownat,
            #[cfg(target_arch = "x86_64")]
            Sysno::lchown,
        ] {
            assert!(
                is_ownership_change_noop_syscall(sysno),
                "{sysno:?} must be in the ownership-change no-op set"
            );
            assert_eq!(
                classify_syscall(sysno),
                SyscallClassification::Determinized,
                "{sysno:?} must be Determinized; PassThrough forwards it to a host \
                 identity that is not the virtual root Detcore reports"
            );
        }

        // NEGATIVE: the predicate must not swallow neighbours. chmod/fchmod are
        // the adjacent metadata mutators and stay PassThrough; the credential
        // families are a DIFFERENT no-op set with its own dispatch arm, and
        // collapsing the two would make either arm's removal untestable.
        for sysno in [
            // x86_64 only: aarch64 has fchmod and fchmodat alone.
            #[cfg(target_arch = "x86_64")]
            Sysno::chmod,
            Sysno::fchmod,
            Sysno::fchmodat,
            Sysno::setuid,
            Sysno::setgid,
            Sysno::getuid,
        ] {
            assert!(
                !is_ownership_change_noop_syscall(sysno),
                "{sysno:?} must NOT be in the ownership-change no-op set"
            );
        }
        // x86_64 only: chown is absent from the aarch64 table; fchownat
        // checks the same on both.
        #[cfg(target_arch = "x86_64")]
        assert!(!is_credential_identity_noop_syscall(Sysno::chown));
        assert!(!is_credential_identity_noop_syscall(Sysno::fchownat));
        assert!(!is_ownership_change_noop_syscall(Sysno::setuid));
    }

    /// The beyond-table manifest continues the pinned table exactly: its
    /// first number is the table's next, every number follows its
    /// predecessor, no name repeats (and none collides with a table name),
    /// and no entry has quietly gained a `Sysno` — one that has belongs in
    /// the classification, not here.
    #[test]
    fn known_beyond_the_table_continues_it_without_gaps_or_overlap() {
        let mut names: BTreeSet<&str> = all_pinned_syscalls().map(|sysno| sysno.name()).collect();
        for (expected, (nr, name)) in
            (Sysno::last().id() + 1..).zip(KNOWN_BEYOND_PINNED_TABLE.iter())
        {
            assert_eq!(
                *nr, expected,
                "KNOWN_BEYOND_PINNED_TABLE must be ascending and contiguous from \
                 Sysno::last().id() + 1; found {nr} where {expected} belongs"
            );
            assert!(
                names.insert(name),
                "{name} (nr {nr}) duplicates a name already in the table or the manifest"
            );
            assert!(
                usize::try_from(*nr).ok().and_then(Sysno::new).is_none(),
                "{name} (nr {nr}) now has a Sysno: classify it and remove it from \
                 KNOWN_BEYOND_PINNED_TABLE"
            );
        }
        for (nr, name) in KNOWN_TABLE_GAPS {
            assert!(
                (Sysno::first().id()..=Sysno::last().id()).contains(nr),
                "gap {name} (nr {nr}) is outside the table's range; it belongs in                  KNOWN_BEYOND_PINNED_TABLE"
            );
            assert!(
                names.insert(name),
                "gap {name} (nr {nr}) duplicates a name already accounted for"
            );
            assert!(
                usize::try_from(*nr).ok().and_then(Sysno::new).is_none(),
                "gap {name} (nr {nr}) now has a Sysno: classify it and remove it                  from KNOWN_TABLE_GAPS"
            );
        }
        #[cfg(target_arch = "x86_64")]
        {
            assert_eq!(unknown_syscall_name(462), "mseal");
            assert_eq!(unknown_syscall_name(471), "rseq_slice_yield");
            assert_eq!(unknown_syscall_name(335), "uretprobe");
            assert_eq!(unknown_syscall_name(336), "uprobe");
        }
        assert_eq!(unknown_syscall_name(999), "syscall_999");
        assert_eq!(unknown_syscall_name(-1), "syscall_-1");
    }

    /// Every syscall number the test host's kernel wires is accounted for:
    /// it is in the pinned table or in [`KNOWN_BEYOND_PINNED_TABLE`]. This
    /// is the tripwire that keeps the manifest complete as kernels grow —
    /// an unaccounted number would reach guests as an unnamed raw number
    /// and, before the TrapToTool policy, executed natively unobserved.
    ///
    /// The numbers come from the running kernel's own headers, preferring
    /// its build tree (`/lib/modules/$(uname -r)/build/.../unistd_64.h`)
    /// and falling back to the installed `/usr/include/asm/unistd_64.h`.
    /// If neither exists, the test fails rather than passing on no
    /// evidence, unless the operator explicitly opts out with
    /// `HERMIT_SKIP_HOST_SYSCALL_COMPLETENESS=1` (a hermetic sandbox with
    /// no kernel headers says so, instead of silently weakening the check
    /// for every other host).
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn every_host_kernel_syscall_number_is_accounted_for() {
        use std::collections::BTreeMap;

        fn parse_nr_defines(text: &str, numbers: &mut BTreeMap<i32, String>) {
            for line in text.lines() {
                let Some(rest) = line.trim().strip_prefix("#define __NR_") else {
                    continue;
                };
                let mut fields = rest.split_whitespace();
                let (Some(name), Some(value)) = (fields.next(), fields.next()) else {
                    continue;
                };
                // Only plain decimal definitions name a wired number;
                // aliases (`__NR_x __NR_y`) and offset expressions belong
                // to another ABI's arithmetic. `__NR_syscalls` is the
                // table's size, not a syscall.
                if name == "syscalls" {
                    continue;
                }
                if let Ok(nr) = value.parse::<i32>() {
                    numbers.entry(nr).or_insert_with(|| name.to_owned());
                }
            }
        }

        let release = std::process::Command::new("uname")
            .arg("-r")
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
            .unwrap_or_default();
        let candidates = [
            format!("/lib/modules/{release}/build/arch/x86/include/generated/uapi/asm/unistd_64.h"),
            format!("/lib/modules/{release}/build/include/uapi/asm-generic/unistd.h"),
            "/usr/include/asm/unistd_64.h".to_owned(),
            "/usr/include/x86_64-linux-gnu/asm/unistd_64.h".to_owned(),
        ];
        let mut host: BTreeMap<i32, String> = BTreeMap::new();
        let mut source = None;
        for candidate in &candidates {
            if release.is_empty() && candidate.contains("/lib/modules/") {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(candidate) {
                parse_nr_defines(&text, &mut host);
                if !host.is_empty() {
                    source = Some(candidate.clone());
                    break;
                }
            }
        }
        let Some(source) = source else {
            assert_eq!(
                std::env::var("HERMIT_SKIP_HOST_SYSCALL_COMPLETENESS").as_deref(),
                Ok("1"),
                "no host syscall table found in {candidates:?}: cannot prove \
                 KNOWN_BEYOND_PINNED_TABLE complete on this host. Install the \
                 kernel headers, or set HERMIT_SKIP_HOST_SYSCALL_COMPLETENESS=1 \
                 to opt out explicitly."
            );
            return;
        };
        // The common (non-x32) numbers only: the x32 ABI shares the header
        // range above __X32_SYSCALL_BIT and is not a table Detcore dispatches.
        host.retain(|nr, _| *nr >= 0 && *nr < 0x4000_0000);
        assert!(
            host.len() > 300,
            "{source} yielded only {} syscall numbers; the parse is broken, not the kernel small",
            host.len()
        );
        let accounted: BTreeSet<i32> = all_pinned_syscalls()
            .map(|sysno| sysno.id())
            .chain(KNOWN_BEYOND_PINNED_TABLE.iter().map(|(nr, _)| *nr))
            .chain(KNOWN_TABLE_GAPS.iter().map(|(nr, _)| *nr))
            .collect();
        let gaps: Vec<String> = host
            .iter()
            .filter(|(nr, _)| !accounted.contains(nr))
            .map(|(nr, name)| format!("{nr} {name}"))
            .collect();
        assert!(
            gaps.is_empty(),
            "{source} wires syscall numbers neither the pinned table nor \
             KNOWN_BEYOND_PINNED_TABLE names: {}. Extend the manifest (or bump \
             the syscalls pin and classify them).",
            gaps.join(", ")
        );
    }
}
