/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A descriptor's `/proc/<pid>/fdinfo/<fd>` as Detcore renders it where the
//! guest's procfs is not the kernel's (package K slice K8-h of
//! <https://github.com/rrnewton/hermit/issues/4213>).
//!
//! Linux prints the file in one `seq_show` (fs/proc/fd.c in v7.1.3): the
//! descriptor's position, status flags, mount and inode, then the locks held
//! through its open file description (`show_fd_locks` in fs/locks.c), then
//! what the file's own `show_fdinfo` prints. [`render`] prints the same lines
//! in the same order and format from [`Fdinfo`], whose values the caller
//! takes from Detcore's model, from calls injected in the reader's turn and
//! from the backend's `descriptor_status` fact
//! (`Detcore::render_own_fdinfo`). The module only formats: every value it
//! prints is already the guest's (determinized identities, virtual time).
//!
//! Where Linux prints lines no field here describes (an epoll set's `tfd:`
//! lines, an inotify instance's watches, a userfaultfd's counters, a
//! pseudo-terminal master's `tty-index`, the lines of an object Detcore does
//! not classify), the caller refuses the read by name, never an empty file
//! (`Detcore::render_own_fdinfo`); the sanitizer of the kernel's text answers
//! an empty one elsewhere (`procfs::sanitize_fdinfo`).

use reverie::LockClass;

/// One `lock:` line of a rendered fdinfo, as `lock_get_status` prints a lock
/// (fs/locks.c in v7.1.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FdinfoLock {
    /// The lock's class.
    pub(crate) class: LockClass,
    /// `MANDATORY` where Linux 5.14 and earlier enforced a record lock; always
    /// false from 5.15 on, where every lock is `ADVISORY`.
    pub(crate) mandatory: bool,
    /// A write (exclusive) lock or lease; false for a read (shared) one.
    pub(crate) write: bool,
    /// The owner's process ID as the reader's PID namespace numbers it: -1
    /// for an open file description lock (`locks_translate_pid`), 0 for a
    /// task the namespace cannot see.
    pub(crate) owner: i64,
    /// The locked file's superblock device, already as the guest sees it.
    pub(crate) device: u64,
    /// The locked file's inode number, already as the guest sees it.
    pub(crate) inode: u64,
    /// A record lock's first byte; 0 for any other lock.
    pub(crate) start: u64,
    /// A record lock's last byte, or `None` for `EOF` (and for any other
    /// lock).
    pub(crate) end: Option<u64>,
}

/// What a file's own `show_fdinfo` adds after the lock lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FdinfoKind {
    /// A file with no `show_fdinfo`: a regular file, a directory, a pipe, a
    /// memfd (shmem prints no seals), a socket other than a Unix domain
    /// socket, a terminal, a device whose driver prints none.
    Plain,
    /// `eventfd_show_fdinfo` (fs/eventfd.c).
    Eventfd {
        /// The counter, which a `read(2)` consumes.
        count: u64,
        /// The eventfd's ID, as the guest numbers it.
        id: u64,
        /// Created with `EFD_SEMAPHORE`.
        semaphore: bool,
    },
    /// `signalfd_show_fdinfo` (fs/signalfd.c): the signals it reads, bit
    /// `n - 1` for signal `n`, without `SIGKILL` and `SIGSTOP`.
    Signalfd {
        /// The mask.
        mask: u64,
    },
    /// `timerfd_show` (fs/timerfd.c).
    Timerfd(TimerfdLines),
    /// `unix_show_fdinfo` (net/unix/af_unix.c): descriptors queued in
    /// `SCM_RIGHTS` messages the socket has not received.
    UnixSocket {
        /// The count.
        scm_fds: u32,
    },
    /// `pidfd_show_fdinfo` (fs/pidfs.c): the process's number in the
    /// reader's PID namespace, or -1 once it has been reaped.
    Pidfd {
        /// The number.
        pid: i64,
    },
}

/// The lines `timerfd_show` prints for a timerfd, as Linux would hold its
/// state at the read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TimerfdLines {
    /// The clock it was created on.
    pub(crate) clockid: i32,
    /// Expirations Linux has counted and no read has taken (`ctx->ticks`).
    pub(crate) ticks: u64,
    /// The flags of the last `timerfd_settime` (`TFD_TIMER_ABSTIME`,
    /// `TFD_TIMER_CANCEL_ON_SET`).
    pub(crate) settime_flags: u32,
    /// The time left to the next expiration, in nanoseconds; 0 when disarmed
    /// or expired.
    pub(crate) value_ns: u64,
    /// The interval, in nanoseconds.
    pub(crate) interval_ns: u64,
}

/// Every value a rendered fdinfo prints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Fdinfo {
    /// The file position (`file->f_pos`).
    pub(crate) pos: i64,
    /// The status flags, with `O_CLOEXEC` where the descriptor has it.
    pub(crate) flags: u32,
    /// The mount ID, as the guest numbers it.
    pub(crate) mnt_id: u64,
    /// The inode number, as the guest's `fstat` numbers it.
    pub(crate) ino: u64,
    /// The `lock:` lines, in Linux's order: flock locks, record locks, leases.
    pub(crate) locks: Vec<FdinfoLock>,
    /// What the file's own `show_fdinfo` adds.
    pub(crate) kind: FdinfoKind,
}

/// The fdinfo text Linux prints for `fdinfo` (fs/proc/fd.c `seq_show`).
pub(crate) fn render(fdinfo: &Fdinfo) -> Vec<u8> {
    use std::fmt::Write as _;
    let mut text = String::new();
    let _ = write!(
        text,
        "pos:\t{}\nflags:\t0{:o}\nmnt_id:\t{}\nino:\t{}\n",
        fdinfo.pos, fdinfo.flags, fdinfo.mnt_id, fdinfo.ino
    );
    for (index, lock) in fdinfo.locks.iter().enumerate() {
        text.push_str("lock:\t");
        render_lock(&mut text, index + 1, lock);
    }
    match &fdinfo.kind {
        FdinfoKind::Plain => {}
        FdinfoKind::Eventfd {
            count,
            id,
            semaphore,
        } => {
            let _ = write!(
                text,
                "eventfd-count: {count:16x}\neventfd-id: {id}\neventfd-semaphore: {}\n",
                u8::from(*semaphore)
            );
        }
        FdinfoKind::Signalfd { mask } => {
            let _ = writeln!(text, "sigmask:\t{mask:016x}");
        }
        FdinfoKind::Timerfd(timer) => {
            let (value_sec, value_nsec) = split_ns(timer.value_ns);
            let (interval_sec, interval_nsec) = split_ns(timer.interval_ns);
            let _ = write!(
                text,
                "clockid: {}\nticks: {}\nsettime flags: 0{:o}\nit_value: ({value_sec}, \
                 {value_nsec})\nit_interval: ({interval_sec}, {interval_nsec})\n",
                timer.clockid, timer.ticks, timer.settime_flags
            );
        }
        FdinfoKind::UnixSocket { scm_fds } => {
            let _ = writeln!(text, "scm_fds: {scm_fds}");
        }
        FdinfoKind::Pidfd { pid } => {
            let _ = write!(text, "Pid:\t{pid}\nNSpid:\t{pid}\n");
        }
    }
    text.into_bytes()
}

/// `lock_get_status(f, fl, id, "", 0)` for a granted lock with a file (no
/// `*NOINODE*`, no `->` waiter prefix), as Linux v7.1.3 prints it.
fn render_lock(text: &mut String, id: usize, lock: &FdinfoLock) {
    use std::fmt::Write as _;
    let _ = write!(text, "{id}: ");
    let record = matches!(
        lock.class,
        LockClass::Posix | LockClass::OpenFileDescription
    );
    match lock.class {
        LockClass::Posix | LockClass::OpenFileDescription => {
            text.push_str(if lock.class == LockClass::Posix {
                "POSIX "
            } else {
                "OFDLCK"
            });
            text.push_str(if lock.mandatory {
                " MANDATORY "
            } else {
                " ADVISORY  "
            });
        }
        LockClass::Flock => text.push_str("FLOCK  ADVISORY  "),
        LockClass::Lease => text.push_str("LEASE  ACTIVE    "),
        // A class a later Reverie adds: Linux's own spelling for one it
        // does not know.
        _ => text.push_str("UNKNOWN UNKNOWN  "),
    }
    text.push_str(if lock.write { "WRITE " } else { "READ " });
    let _ = write!(
        text,
        "{} {:02x}:{:02x}:{} ",
        lock.owner,
        libc::major(lock.device),
        libc::minor(lock.device),
        lock.inode
    );
    if record {
        match lock.end {
            None => {
                let _ = writeln!(text, "{} EOF", lock.start);
            }
            Some(end) => {
                let _ = writeln!(text, "{} {end}", lock.start);
            }
        }
    } else {
        text.push_str("0 EOF\n");
    }
}

/// Nanoseconds as `(seconds, nanoseconds)`, as `ktime_to_timespec64` splits
/// them.
fn split_ns(ns: u64) -> (u64, u64) {
    (ns / 1_000_000_000, ns % 1_000_000_000)
}

#[cfg(test)]
mod tests {
    use reverie::DescriptorStatus;
    use reverie::LockClass;

    use super::*;

    fn plain(pos: i64, flags: u32, mnt_id: u64, ino: u64) -> Fdinfo {
        Fdinfo {
            pos,
            flags,
            mnt_id,
            ino,
            locks: Vec::new(),
            kind: FdinfoKind::Plain,
        }
    }

    fn rendered(fdinfo: &Fdinfo) -> String {
        String::from_utf8(render(fdinfo)).expect("fdinfo is text")
    }

    /// The common lines, with flags as `0%o` prints them, measured natively
    /// on Linux 7.1.3: a pipe's read end (`00`), a memfd (`0100002`), an
    /// `O_CLOEXEC` one (`02100002`), an `O_PATH` directory (`010000000`).
    #[test]
    fn prints_the_common_lines_with_flags_as_linux_does() {
        assert_eq!(
            rendered(&plain(0, 0, 17, 3392079776)),
            "pos:\t0\nflags:\t00\nmnt_id:\t17\nino:\t3392079776\n"
        );
        assert_eq!(
            rendered(&plain(1, 0o100002, 2396, 2656665735)),
            "pos:\t1\nflags:\t0100002\nmnt_id:\t2396\nino:\t2656665735\n"
        );
        assert_eq!(
            rendered(&plain(0, 0o2100002, 0, 51243983)),
            "pos:\t0\nflags:\t02100002\nmnt_id:\t0\nino:\t51243983\n"
        );
        assert_eq!(
            rendered(&plain(0, 0o10000000, 2396, 7)),
            "pos:\t0\nflags:\t010000000\nmnt_id:\t2396\nino:\t7\n"
        );
        assert_eq!(
            rendered(&plain(-1, 2, 1, 1)),
            "pos:\t-1\nflags:\t02\nmnt_id:\t1\nino:\t1\n"
        );
    }

    /// Each file's own lines as Linux 7.1.3 printed them for the same state,
    /// measured natively: an `EFD_SEMAPHORE` eventfd holding 5, a signalfd of
    /// SIGUSR1 and SIGRTMIN+2, a timerfd on CLOCK_MONOTONIC, a Unix socket
    /// with two descriptors queued, a pidfd.
    #[test]
    fn prints_each_files_own_lines_as_linux_does() {
        let with = |kind| Fdinfo {
            kind,
            ..plain(0, 2, 18, 9225)
        };
        assert_eq!(
            rendered(&with(FdinfoKind::Eventfd {
                count: 5,
                id: 60693,
                semaphore: true,
            })),
            "pos:\t0\nflags:\t02\nmnt_id:\t18\nino:\t9225\n\
             eventfd-count:                5\neventfd-id: 60693\neventfd-semaphore: 1\n"
        );
        assert_eq!(
            rendered(&with(FdinfoKind::Eventfd {
                count: 0xfffffffffffffffe,
                id: 1,
                semaphore: false,
            })),
            "pos:\t0\nflags:\t02\nmnt_id:\t18\nino:\t9225\n\
             eventfd-count: fffffffffffffffe\neventfd-id: 1\neventfd-semaphore: 0\n"
        );
        assert_eq!(
            rendered(&with(FdinfoKind::Signalfd {
                mask: (1 << 9) | (1 << 35),
            })),
            "pos:\t0\nflags:\t02\nmnt_id:\t18\nino:\t9225\nsigmask:\t0000000800000200\n"
        );
        assert_eq!(
            rendered(&with(FdinfoKind::Timerfd(TimerfdLines {
                clockid: 1,
                ticks: 0,
                settime_flags: 0,
                value_ns: 99_999_988_159,
                interval_ns: 1_000_000_500,
            }))),
            "pos:\t0\nflags:\t02\nmnt_id:\t18\nino:\t9225\nclockid: 1\nticks: 0\n\
             settime flags: 00\nit_value: (99, 999988159)\nit_interval: (1, 500)\n"
        );
        assert_eq!(
            rendered(&with(FdinfoKind::Timerfd(TimerfdLines {
                clockid: 0,
                ticks: 3,
                settime_flags: 3,
                value_ns: 0,
                interval_ns: 0,
            })))
            .lines()
            .skip(4)
            .collect::<Vec<_>>(),
            [
                "clockid: 0",
                "ticks: 3",
                "settime flags: 03",
                "it_value: (0, 0)",
                "it_interval: (0, 0)"
            ]
        );
        assert_eq!(
            rendered(&with(FdinfoKind::UnixSocket { scm_fds: 2 })),
            "pos:\t0\nflags:\t02\nmnt_id:\t18\nino:\t9225\nscm_fds: 2\n"
        );
        assert_eq!(
            rendered(&with(FdinfoKind::Pidfd { pid: 7 })),
            "pos:\t0\nflags:\t02\nmnt_id:\t18\nino:\t9225\nPid:\t7\nNSpid:\t7\n"
        );
        assert_eq!(
            rendered(&with(FdinfoKind::Pidfd { pid: -1 })),
            "pos:\t0\nflags:\t02\nmnt_id:\t18\nino:\t9225\nPid:\t-1\nNSpid:\t-1\n"
        );
    }

    fn lock(class: LockClass, write: bool, owner: i64, start: u64, end: Option<u64>) -> FdinfoLock {
        FdinfoLock {
            class,
            mandatory: false,
            write,
            owner,
            device: libc::makedev(0, 0x2a),
            inode: 1234,
            start,
            end,
        }
    }

    /// The `lock:` lines `lock_get_status` prints, numbered from 1, with
    /// Linux's column padding, read back by Reverie's parser of Linux's
    /// exact format (`DescriptorStatus::from_fdinfo`), which refuses any
    /// other spelling.
    #[test]
    fn prints_lock_lines_as_lock_get_status_does() {
        let fdinfo = Fdinfo {
            locks: vec![
                lock(LockClass::Flock, true, 3, 0, None),
                lock(LockClass::Posix, true, 3, 0, Some(9)),
                lock(LockClass::OpenFileDescription, false, -1, 100, None),
                lock(LockClass::Lease, false, 0, 0, None),
            ],
            kind: FdinfoKind::Eventfd {
                count: 7,
                id: 2,
                semaphore: false,
            },
            ..plain(0, 0o100002, 1, 1234)
        };
        let text = rendered(&fdinfo);
        assert_eq!(
            text.lines().skip(4).take(4).collect::<Vec<_>>(),
            [
                "lock:\t1: FLOCK  ADVISORY  WRITE 3 00:2a:1234 0 EOF",
                "lock:\t2: POSIX  ADVISORY  WRITE 3 00:2a:1234 0 9",
                "lock:\t3: OFDLCK ADVISORY  READ -1 00:2a:1234 100 EOF",
                "lock:\t4: LEASE  ACTIVE    READ 0 00:2a:1234 0 EOF",
            ]
        );
        let parsed = DescriptorStatus::from_fdinfo(text.as_bytes()).expect("Linux's format");
        assert_eq!(parsed.eventfd_count, Some(7));
        let read_back = parsed
            .locks
            .iter()
            .map(|lock| (lock.class, lock.write, lock.raw_inode, lock.start, lock.end))
            .collect::<Vec<_>>();
        assert_eq!(
            read_back,
            [
                (LockClass::Flock, true, 1234, 0, None),
                (LockClass::Posix, true, 1234, 0, Some(9)),
                (LockClass::OpenFileDescription, false, 1234, 100, None),
                (LockClass::Lease, false, 1234, 0, None),
            ]
        );
        assert!(
            parsed
                .locks
                .iter()
                .all(|lock| lock.raw_device == libc::makedev(0, 0x2a))
        );
    }

    /// A device whose major is wider than two hex digits, as Linux prints
    /// `%02x`: no truncation, no padding beyond two.
    #[test]
    fn prints_wide_devices_unpadded() {
        let mut wide = lock(LockClass::OpenFileDescription, true, -1, 0, None);
        wide.device = libc::makedev(0x103, 0xfffff);
        let fdinfo = Fdinfo {
            locks: vec![wide],
            ..plain(0, 2, 1, 1)
        };
        let text = rendered(&fdinfo);
        assert!(
            text.contains("lock:\t1: OFDLCK ADVISORY  WRITE -1 103:fffff:1234 0 EOF\n"),
            "{text}"
        );
        assert!(DescriptorStatus::from_fdinfo(text.as_bytes()).is_ok());
    }
}
