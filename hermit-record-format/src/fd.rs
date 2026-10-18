/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::fd::RawFd;
use std::sync::Once;

use reverie::Pid;

static PIDFD_CAPABILITY_WARNING: Once = Once::new();
static KCMP_CAPABILITY_WARNING: Once = Once::new();

fn is_platform_capability_error(error: &std::io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::ENOSYS) | Some(libc::EPERM) | Some(libc::EACCES)
    )
}

fn warn_pidfd_capability(error: &std::io::Error) {
    if is_platform_capability_error(error) {
        PIDFD_CAPABILITY_WARNING.call_once(|| {
            tracing::warn!(
                %error,
                "pidfd descriptor duplication is unavailable; record/replay fd-state capture may be incomplete"
            );
        });
    }
}

fn warn_kcmp_capability(error: &std::io::Error) {
    if is_platform_capability_error(error) {
        KCMP_CAPABILITY_WARNING.call_once(|| {
            tracing::warn!(
                %error,
                "kcmp open-file-description comparison is unavailable; record/replay fd identity checks are conservative"
            );
        });
    }
}

// TODO-HUMAN-REVIEW(#557): Audit pidfd-based guest descriptor duplication.
pub fn duplicate_guest_fd(pid: Pid, fd: RawFd) -> std::io::Result<OwnedFd> {
    // pidfd_getfd returns a true duplicate of the guest descriptor, preserving
    // its open-file description (including offsets, flags, and socket identity).
    let pidfd = open_pidfd(pid, 0)?;
    duplicate_from_pidfd(pidfd.as_fd(), fd)
}

/// Duplicates descriptor `fd` from the table of guest thread `tid` in process
/// `pid`. A thread made with `CLONE_THREAD` but without `CLONE_FILES` has a
/// table of its own, and a process pidfd reads the leader's. So this reads
/// through a thread pidfd (`PIDFD_THREAD`, Linux 6.9 and later), and where
/// Linux has none, through the leader's pidfd only if the two tasks share one
/// table; otherwise it fails with `EINVAL`.
// TODO-HUMAN-REVIEW(PR-3871): Audit thread-table descriptor duplication.
pub fn duplicate_guest_thread_fd(pid: Pid, tid: Pid, fd: RawFd) -> std::io::Result<OwnedFd> {
    const PIDFD_THREAD: libc::c_int = libc::O_EXCL;
    if tid == pid {
        return duplicate_guest_fd(pid, fd);
    }
    match open_pidfd(tid, PIDFD_THREAD) {
        Ok(pidfd) => duplicate_from_pidfd(pidfd.as_fd(), fd),
        Err(error) if error.raw_os_error() == Some(libc::EINVAL) => {
            if shares_fd_table(pid, tid)? {
                duplicate_guest_fd(pid, fd)
            } else {
                Err(error)
            }
        }
        Err(error) => Err(error),
    }
}

fn open_pidfd(pid: Pid, flags: libc::c_int) -> std::io::Result<OwnedFd> {
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid.as_raw(), flags) as RawFd };
    if pidfd < 0 {
        let error = std::io::Error::last_os_error();
        warn_pidfd_capability(&error);
        return Err(error);
    }
    // SAFETY: pidfd_open returned this descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(pidfd) })
}

fn duplicate_from_pidfd(pidfd: BorrowedFd<'_>, fd: RawFd) -> std::io::Result<OwnedFd> {
    let duplicate =
        unsafe { libc::syscall(libc::SYS_pidfd_getfd, pidfd.as_raw_fd(), fd, 0) as RawFd };
    if duplicate < 0 {
        let error = std::io::Error::last_os_error();
        warn_pidfd_capability(&error);
        return Err(error);
    }
    // SAFETY: pidfd_getfd returned a new descriptor owned by this process.
    let duplicate = unsafe { OwnedFd::from_raw_fd(duplicate) };

    // Prevent the subsequently exec'd guest from inheriting the tracer's
    // endpoint duplicate and perturbing guest fd allocation.
    let cloexec = unsafe { libc::fcntl(duplicate.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    if cloexec == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(duplicate)
}

/// Whether tasks `left` and `right` share one descriptor table.
fn shares_fd_table(left: Pid, right: Pid) -> std::io::Result<bool> {
    const KCMP_FILES: libc::c_int = 2;
    let comparison = unsafe {
        libc::syscall(
            libc::SYS_kcmp,
            left.as_raw(),
            right.as_raw(),
            KCMP_FILES,
            0,
            0,
        )
    };
    if comparison == -1 {
        let error = std::io::Error::last_os_error();
        warn_kcmp_capability(&error);
        Err(error)
    } else {
        Ok(comparison == 0)
    }
}

// TODO-HUMAN-REVIEW(#557): Audit kcmp-based open-file identity checks.
pub fn same_open_file_description(left: RawFd, right: RawFd) -> std::io::Result<bool> {
    const KCMP_FILE: libc::c_int = 0;
    // SAFETY: kcmp only compares descriptor-table entries owned by this process.
    let pid = unsafe { libc::getpid() };
    let comparison = unsafe { libc::syscall(libc::SYS_kcmp, pid, pid, KCMP_FILE, left, right) };
    if comparison == -1 {
        let error = std::io::Error::last_os_error();
        warn_kcmp_capability(&error);
        Err(error)
    } else {
        Ok(comparison == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_capability_errors_are_classified() {
        for errno in [libc::ENOSYS, libc::EPERM, libc::EACCES] {
            assert!(is_platform_capability_error(
                &std::io::Error::from_raw_os_error(errno)
            ));
        }
        assert!(!is_platform_capability_error(
            &std::io::Error::from_raw_os_error(libc::EBADF)
        ));
        // Linux before 6.9 rejects PIDFD_THREAD with EINVAL on every
        // non-leader accept; that expected fallback must not warn.
        assert!(!is_platform_capability_error(
            &std::io::Error::from_raw_os_error(libc::EINVAL)
        ));
    }
}
