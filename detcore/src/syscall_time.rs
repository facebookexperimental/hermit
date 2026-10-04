/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use reverie::syscalls::Sysno;

const CLOCK_NS: u64 = 10_000;
const GETTIMEOFDAY_NS: u64 = 2_000;
const FAST_NS: u64 = 250;
const METADATA_NS: u64 = 3_000;
const SYNC_NS: u64 = 5_000;
const IO_NS: u64 = 10_000;
const MEMORY_NS: u64 = 10_000;
const OPEN_CLOSE_NS: u64 = 25_000;
const NETWORK_NS: u64 = 50_000;
const PROCESS_NS: u64 = 250_000;

/// Returns a deterministic, representative syscall cost in nanoseconds.
///
/// These values model syscall overhead rather than host-observed latency. The fallback is
/// deliberately nonzero so newly intercepted syscalls cannot stop virtual-time progress.
pub(crate) fn cost_ns(sysno: Sysno) -> u64 {
    match sysno {
        Sysno::gettimeofday => GETTIMEOFDAY_NS,

        Sysno::time
        | Sysno::clock_gettime
        | Sysno::clock_getres
        | Sysno::timerfd_gettime
        | Sysno::timer_gettime
        | Sysno::timer_getoverrun => CLOCK_NS,

        Sysno::getpid
        | Sysno::gettid
        | Sysno::getcpu
        | Sysno::arch_prctl
        | Sysno::set_tid_address
        | Sysno::set_robust_list
        | Sysno::rt_sigprocmask
        | Sysno::rt_sigaction
        | Sysno::sigaltstack => FAST_NS,

        Sysno::stat
        | Sysno::lstat
        | Sysno::fstat
        | Sysno::newfstatat
        | Sysno::statx
        | Sysno::uname
        | Sysno::getrusage
        | Sysno::sysinfo
        | Sysno::access
        | Sysno::readlink
        | Sysno::readlinkat
        | Sysno::utime
        | Sysno::utimes
        | Sysno::utimensat
        | Sysno::futimesat
        | Sysno::prlimit64
        | Sysno::prctl => METADATA_NS,

        Sysno::futex
        | Sysno::rseq
        | Sysno::sched_yield
        | Sysno::nanosleep
        | Sysno::clock_nanosleep
        | Sysno::alarm
        | Sysno::pause
        | Sysno::poll
        | Sysno::epoll_pwait
        | Sysno::epoll_wait
        | Sysno::epoll_wait_old
        | Sysno::rt_sigtimedwait => SYNC_NS,

        Sysno::read
        | Sysno::pread64
        | Sysno::write
        | Sysno::lseek
        | Sysno::fadvise64
        | Sysno::fcntl
        | Sysno::ioctl
        | Sysno::getrandom
        | Sysno::getdents
        | Sysno::getdents64 => IO_NS,

        Sysno::mmap
        | Sysno::munmap
        | Sysno::mremap
        | Sysno::brk
        | Sysno::mprotect
        | Sysno::madvise
        | Sysno::userfaultfd => MEMORY_NS,

        Sysno::open
        | Sysno::openat
        | Sysno::creat
        | Sysno::close
        | Sysno::dup
        | Sysno::dup2
        | Sysno::dup3
        | Sysno::pipe
        | Sysno::pipe2
        | Sysno::eventfd
        | Sysno::eventfd2
        | Sysno::signalfd
        | Sysno::signalfd4
        | Sysno::timerfd_create
        | Sysno::timerfd_settime
        | Sysno::timer_create
        | Sysno::timer_settime
        | Sysno::timer_delete
        | Sysno::inotify_init
        | Sysno::inotify_init1
        | Sysno::inotify_add_watch
        | Sysno::inotify_rm_watch
        | Sysno::memfd_create
        | Sysno::io_uring_setup
        | Sysno::io_uring_enter
        | Sysno::io_uring_register
        | Sysno::epoll_create
        | Sysno::epoll_create1
        | Sysno::epoll_ctl
        | Sysno::epoll_ctl_old
        | Sysno::add_key
        | Sysno::request_key
        | Sysno::keyctl => OPEN_CLOSE_NS,

        Sysno::socket
        | Sysno::socketpair
        | Sysno::connect
        | Sysno::bind
        | Sysno::accept
        | Sysno::accept4
        | Sysno::recvfrom
        | Sysno::recvmsg
        | Sysno::sendto
        | Sysno::sendmsg
        | Sysno::sendmmsg => NETWORK_NS,

        Sysno::clone
        | Sysno::clone3
        | Sysno::fork
        | Sysno::vfork
        | Sysno::wait4
        | Sysno::setsid
        | Sysno::execve
        | Sysno::execveat
        | Sysno::exit
        | Sysno::exit_group
        | Sysno::sched_getaffinity
        | Sysno::sched_setaffinity => PROCESS_NS,

        _ => FAST_NS,
    }
}

/// The most syscalls one backend-runtime bootstrap window may leave uncharged on
/// the bootstrapping thread. Once a window reaches this count, every further
/// syscall in it is charged its normal cost, so a window cannot stop the thread's
/// syscall-driven clock indefinitely.
///
/// The LiteInst runtime's own constructor makes more syscalls the larger the
/// image: for each executable mapping it rereads `/proc/self/maps` and
/// allocates a trampoline arena. Uncharged syscalls per window, measured under
/// `--backend liteinst --max-timeslice=disabled` with Reverie dbf2b5c8 (lines
/// of `ldd` output in parentheses): 315 for `tests/c/host_identity.c` and for
/// each image of `hermit-cli/tests/fixtures/clock_trajectory.c`; 401 for
/// `python3` (5); 1,589 for `curl` (28); 3,588 for `gdb` (58); 4,752 for
/// `qemu-system-x86_64` (72); 6,970 for `emacs` (95). The counts vary slightly
/// with the environment: in another build directory the same commit measured
/// 316 for `host_identity.c` and 318 for each `clock_trajectory.c` image,
/// probably because the `/proc/self/maps` lines the runtime rereads name the
/// mapped paths. Beyond a fixed cost the
/// count grows faster than the number of libraries, because each reread of
/// `/proc/self/maps` is longer: about 57 syscalls per `ldd` line for `curl` and
/// 73 for `emacs`. So the margin below shrinks as images grow. The cap must
/// stay at least four times the largest such count; 32,768 is 4.7 times
/// emacs's. A window that reaches the cap says so once, at info level (see
/// `ThreadState::charge_syscall_time`).
pub(crate) const MAX_UNCHARGED_BOOTSTRAP_SYSCALLS: u32 = 32768;

/// Returns true for syscalls whose main result is a clock or timer value: the
/// wall-clock and monotonic clocks, the CPU-time clocks and counters, uptime,
/// the time left on a timer, and the `time` field an `adjtimex` or
/// `clock_adjtime` query returns (glibc's `ntp_gettime` reads the clock that
/// way). Timerfd calls currently pass through to a host timer
/// (https://github.com/rrnewton/hermit/issues/1923), so `timerfd_gettime`
/// reports host time left, not virtual time; it is listed because its result
/// is a timer value.
///
/// Inside a backend-runtime bootstrap window these syscalls are always charged,
/// so two such reads on the bootstrapping thread differ by at least one syscall
/// cost, exactly as outside the window. `clock_getres` is not listed: it reports
/// a constant.
///
/// Some syscalls give the guest virtual time without it being their main
/// result. They are not listed, so inside a window they are bounded only by
/// [`MAX_UNCHARGED_BOOTSTRAP_SYSCALLS`]:
/// - side outputs: the old value written by `setitimer` or `timer_settime`,
///   and the seconds `alarm` returns;
/// - a `read` of a procfs file Detcore renders from virtual time, such as
///   `/proc/uptime` (whole seconds);
/// - `utime` and `utimes` with no times given, and every write that updates a
///   file's modification time (`write`, `writev`, `pwrite64`, `pwritev`,
///   `pwritev2` and `sendfile`), which stamp the file with the virtual time,
///   readable back through `stat`;
/// - socket receive timestamps, which Detcore sets to the virtual time of the
///   receive.
pub(crate) fn observes_virtual_time(sysno: Sysno) -> bool {
    matches!(
        sysno,
        Sysno::gettimeofday
            | Sysno::time
            | Sysno::clock_gettime
            | Sysno::sysinfo
            | Sysno::times
            | Sysno::getrusage
            | Sysno::timerfd_gettime
            | Sysno::timer_gettime
            | Sysno::getitimer
            | Sysno::adjtimex
            | Sysno::clock_adjtime
    )
}

#[cfg(test)]
mod tests {
    use detcore_model::time::DetTime;

    use super::*;

    #[test]
    fn representative_costs_cover_each_category() {
        assert_eq!(cost_ns(Sysno::clock_gettime), 10_000);
        assert_eq!(cost_ns(Sysno::gettimeofday), 2_000);
        assert_eq!(cost_ns(Sysno::getpid), 250);
        assert_eq!(cost_ns(Sysno::statx), 3_000);
        assert_eq!(cost_ns(Sysno::futex), 5_000);
        assert_eq!(cost_ns(Sysno::read), 10_000);
        assert_eq!(cost_ns(Sysno::mmap), 10_000);
        assert_eq!(cost_ns(Sysno::openat), 25_000);
        assert_eq!(cost_ns(Sysno::socket), 50_000);
        assert_eq!(cost_ns(Sysno::clone), 250_000);
    }

    #[test]
    fn unsupported_syscalls_still_advance_time() {
        assert_eq!(cost_ns(Sysno::restart_syscall), FAST_NS);
        assert!(cost_ns(Sysno::restart_syscall) > 0);
    }

    #[test]
    fn dettime_preserves_legacy_serialized_syscall_counts() {
        let mut time = DetTime::zero();
        time.add_syscall();
        time.add_syscall();
        let mut serialized = serde_json::to_value(time).unwrap();
        serialized.as_object_mut().unwrap().remove("syscall_nanos");

        let mut restored: DetTime = serde_json::from_value(serialized).unwrap();
        assert_eq!(restored.without_starting().as_nanos(), 20_000);

        restored.add_syscall_with_cost(cost_ns(Sysno::clock_gettime));
        assert_eq!(restored.without_starting().as_nanos(), 30_000);
    }

    #[test]
    fn dettime_accumulates_mixed_syscall_costs() {
        let mut time = DetTime::zero();
        time.add_syscall_with_cost(cost_ns(Sysno::clock_gettime));
        time.add_syscall_with_cost(cost_ns(Sysno::read));

        assert_eq!(time.without_starting().as_nanos(), 20_000);
    }

    #[test]
    fn dettime_applies_multiplier_to_syscall_costs() {
        let mut time = DetTime::zero().with_multiplier(2.0);
        time.add_syscall_with_cost(cost_ns(Sysno::write));

        assert_eq!(time.without_starting().as_nanos(), 20_000);
    }
}
