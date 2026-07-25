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
    /// The syscall retains the legacy fail-closed-or-forward policy pending investigation.
    Unclassified,
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
        | Sysno::prlimit64
        | Sysno::pread64
        | Sysno::read
        | Sysno::recvfrom
        | Sysno::recvmsg
        | Sysno::rseq
        | Sysno::rt_sigaction
        | Sysno::rt_sigprocmask
        | Sysno::rt_sigtimedwait
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
        // TODO-HUMAN-REVIEW(#547)
        | Sysno::writev => SyscallClassification::Determinized,

        // ===== BEGIN PASS-THRU SYSCALLS =====
        // These existing and triaged passthroughs are conditionally repeatable under
        // Hermit's fixed-container, stable-filesystem, and serialization assumptions.
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#503): Confirm the stable-state boundary for these promotions.
        Sysno::access
        | Sysno::brk
        | Sysno::getcwd
        | Sysno::getegid
        | Sysno::geteuid
        | Sysno::getgid
        | Sysno::getpid
        | Sysno::gettid
        | Sysno::getuid
        | Sysno::lseek
        | Sysno::mprotect
        | Sysno::readlink
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
        // Ptrace executes rt_sigreturn directly; DBI has dedicated injected-sigreturn
        // handling, while KVM deterministically reports its current lack of signal support.
        | Sysno::rt_sigreturn => SyscallClassification::PassThrough,
        // ===== END PASS-THRU SYSCALLS =====

        // ===== UNCLASSIFIED (TEMPORARY PASS-THRU) =====
        // TODO/FIXME: These syscalls have not been classified. They temporarily use
        // the legacy passthrough policy and may need deterministic handling. Each must
        // be investigated and moved to DETERMINIZED or PASS-THRU.
        Sysno::_sysctl
        | Sysno::acct
        | Sysno::add_key
        | Sysno::adjtimex
        | Sysno::afs_syscall
        | Sysno::bpf
        | Sysno::cachestat
        | Sysno::chown
        | Sysno::chroot
        | Sysno::clock_adjtime
        | Sysno::clock_settime
        | Sysno::close_range
        | Sysno::copy_file_range
        | Sysno::create_module
        | Sysno::delete_module
        | Sysno::epoll_pwait2
        | Sysno::faccessat
        | Sysno::fallocate
        | Sysno::fanotify_init
        | Sysno::fanotify_mark
        | Sysno::fchmod
        | Sysno::fchmodat2
        | Sysno::fchown
        | Sysno::fchownat
        | Sysno::fgetxattr
        | Sysno::finit_module
        | Sysno::flistxattr
        | Sysno::flock
        | Sysno::fremovexattr
        | Sysno::fsconfig
        | Sysno::fsetxattr
        | Sysno::fsmount
        | Sysno::fsopen
        | Sysno::fspick
        | Sysno::fsync
        | Sysno::futex_requeue
        | Sysno::futex_wait
        | Sysno::futex_waitv
        | Sysno::futex_wake
        | Sysno::get_kernel_syms
        | Sysno::get_mempolicy
        | Sysno::get_robust_list
        | Sysno::get_thread_area
        | Sysno::getitimer
        | Sysno::getpeername
        | Sysno::getpgid
        | Sysno::getpgrp
        | Sysno::getpmsg
        | Sysno::getppid
        | Sysno::getpriority
        | Sysno::getresgid
        | Sysno::getresuid
        | Sysno::getrlimit
        | Sysno::getsid
        | Sysno::getsockname
        | Sysno::getsockopt
        | Sysno::init_module
        | Sysno::io_cancel
        | Sysno::io_destroy
        | Sysno::io_getevents
        | Sysno::io_pgetevents
        | Sysno::io_setup
        | Sysno::io_submit
        | Sysno::ioperm
        | Sysno::iopl
        | Sysno::ioprio_get
        | Sysno::ioprio_set
        | Sysno::kcmp
        | Sysno::kexec_file_load
        | Sysno::kexec_load
        | Sysno::keyctl
        | Sysno::kill
        | Sysno::landlock_add_rule
        | Sysno::landlock_create_ruleset
        | Sysno::landlock_restrict_self
        | Sysno::lchown
        | Sysno::link
        | Sysno::listen
        | Sysno::listmount
        | Sysno::listxattr
        | Sysno::llistxattr
        | Sysno::lookup_dcookie
        | Sysno::lremovexattr
        | Sysno::lsetxattr
        | Sysno::lsm_get_self_attr
        | Sysno::lsm_list_modules
        | Sysno::lsm_set_self_attr
        | Sysno::map_shadow_stack
        | Sysno::mbind
        | Sysno::memfd_secret
        | Sysno::migrate_pages
        | Sysno::mincore
        | Sysno::mknod
        | Sysno::mknodat
        | Sysno::mlock
        | Sysno::mlock2
        | Sysno::mlockall
        | Sysno::modify_ldt
        | Sysno::mount
        | Sysno::mount_setattr
        | Sysno::move_mount
        | Sysno::move_pages
        | Sysno::mq_getsetattr
        | Sysno::mq_notify
        | Sysno::mq_open
        | Sysno::mq_timedreceive
        | Sysno::mq_timedsend
        | Sysno::mq_unlink
        | Sysno::msgctl
        | Sysno::msgget
        | Sysno::msgrcv
        | Sysno::msgsnd
        | Sysno::msync
        | Sysno::munlock
        | Sysno::munlockall
        | Sysno::name_to_handle_at
        | Sysno::nfsservctl
        | Sysno::open_by_handle_at
        | Sysno::open_tree
        | Sysno::openat2
        | Sysno::perf_event_open
        | Sysno::personality
        | Sysno::pidfd_getfd
        | Sysno::pidfd_open
        | Sysno::pidfd_send_signal
        | Sysno::pivot_root
        | Sysno::pkey_alloc
        | Sysno::pkey_free
        | Sysno::pkey_mprotect
        | Sysno::prctl
        | Sysno::preadv
        | Sysno::preadv2
        | Sysno::process_madvise
        | Sysno::process_mrelease
        | Sysno::process_vm_readv
        | Sysno::process_vm_writev
        | Sysno::pselect6
        | Sysno::ptrace
        | Sysno::putpmsg
        | Sysno::pwrite64
        | Sysno::pwritev
        | Sysno::pwritev2
        | Sysno::query_module
        | Sysno::quotactl
        | Sysno::quotactl_fd
        | Sysno::readahead
        | Sysno::readlinkat
        | Sysno::readv
        | Sysno::reboot
        | Sysno::recvmmsg
        | Sysno::remap_file_pages
        | Sysno::rename
        | Sysno::renameat
        | Sysno::request_key
        | Sysno::restart_syscall
        | Sysno::rt_sigpending
        | Sysno::rt_sigqueueinfo
        | Sysno::rt_sigsuspend
        | Sysno::rt_tgsigqueueinfo
        | Sysno::sched_get_priority_max
        | Sysno::sched_get_priority_min
        | Sysno::sched_getattr
        | Sysno::sched_getparam
        | Sysno::sched_getscheduler
        | Sysno::sched_rr_get_interval
        | Sysno::sched_setattr
        | Sysno::sched_setparam
        | Sysno::sched_setscheduler
        | Sysno::seccomp
        | Sysno::security
        | Sysno::select
        | Sysno::semctl
        | Sysno::semget
        | Sysno::semop
        | Sysno::semtimedop
        | Sysno::sendfile
        | Sysno::set_mempolicy
        | Sysno::set_mempolicy_home_node
        | Sysno::set_thread_area
        | Sysno::setdomainname
        | Sysno::setfsgid
        | Sysno::setfsuid
        | Sysno::setgid
        | Sysno::setgroups
        | Sysno::sethostname
        | Sysno::setitimer
        | Sysno::setns
        | Sysno::setpgid
        | Sysno::setpriority
        | Sysno::setregid
        | Sysno::setresgid
        | Sysno::setresuid
        | Sysno::setreuid
        | Sysno::setrlimit
        | Sysno::setsockopt
        | Sysno::settimeofday
        | Sysno::setuid
        | Sysno::shmat
        | Sysno::shmctl
        | Sysno::shmdt
        | Sysno::shmget
        | Sysno::shutdown
        | Sysno::splice
        | Sysno::statmount
        | Sysno::swapoff
        | Sysno::swapon
        | Sysno::symlink
        | Sysno::sync
        | Sysno::sync_file_range
        | Sysno::syncfs
        | Sysno::sysfs
        | Sysno::syslog
        | Sysno::tee
        | Sysno::tgkill
        | Sysno::times
        | Sysno::tkill
        | Sysno::truncate
        | Sysno::tuxcall
        | Sysno::umount2
        | Sysno::unshare
        | Sysno::uselib
        | Sysno::ustat
        | Sysno::vhangup
        | Sysno::vmsplice
        | Sysno::vserver => SyscallClassification::Unclassified,
        // ===== END UNCLASSIFIED =====

        // `Sysno` is `#[non_exhaustive]` outside its crate. The const ABI guards above
        // make changes to the pinned table a compile error; this arm only satisfies the
        // external-enum language requirement and deliberately fails closed.
        _unexpected => panic!("unclassified Sysno outside pinned ABI"),
    }
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
                SyscallClassification::Unclassified => counts[2] += 1,
            }
        }

        assert_eq!(counts, [109, 39, 225]);
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
        assert_eq!(
            classify_syscall(Sysno::arch_prctl),
            SyscallClassification::Determinized
        );
        assert_eq!(
            classify_syscall(Sysno::prlimit64),
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
        for sysno in [
            Sysno::capget,
            Sysno::capset,
            Sysno::chdir,
            Sysno::chmod,
            Sysno::faccessat2,
            Sysno::fchdir,
            Sysno::fchmodat,
            Sysno::fdatasync,
            Sysno::ftruncate,
            Sysno::getgroups,
            Sysno::getxattr,
            Sysno::lgetxattr,
            Sysno::linkat,
            Sysno::mkdir,
            Sysno::mkdirat,
            Sysno::removexattr,
            Sysno::renameat2,
            Sysno::rmdir,
            Sysno::rt_sigreturn,
            Sysno::setxattr,
            Sysno::symlinkat,
            Sysno::umask,
            Sysno::unlink,
            Sysno::unlinkat,
        ] {
            assert_eq!(classify_syscall(sysno), SyscallClassification::PassThrough);
        }
        for sysno in [
            Sysno::add_key,
            Sysno::keyctl,
            Sysno::prctl,
            Sysno::readlinkat,
            Sysno::request_key,
        ] {
            assert_eq!(classify_syscall(sysno), SyscallClassification::Unclassified);
        }
    }
}
