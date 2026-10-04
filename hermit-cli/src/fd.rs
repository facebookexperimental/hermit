/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::os::fd::AsRawFd;
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
pub(crate) fn duplicate_guest_fd(pid: Pid, fd: RawFd) -> std::io::Result<OwnedFd> {
    // pidfd_getfd returns a true duplicate of the guest descriptor, preserving
    // its open-file description (including offsets, flags, and socket identity).
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid.as_raw(), 0) as RawFd };
    if pidfd < 0 {
        let error = std::io::Error::last_os_error();
        warn_pidfd_capability(&error);
        return Err(error);
    }

    let duplicate = unsafe { libc::syscall(libc::SYS_pidfd_getfd, pidfd, fd, 0) as RawFd };
    let duplicate_error = std::io::Error::last_os_error();
    // SAFETY: pidfd_open returned this descriptor.
    unsafe { libc::close(pidfd) };
    if duplicate < 0 {
        warn_pidfd_capability(&duplicate_error);
        return Err(duplicate_error);
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

// TODO-HUMAN-REVIEW(#557): Audit kcmp-based open-file identity checks.
pub(crate) fn same_open_file_description(left: RawFd, right: RawFd) -> std::io::Result<bool> {
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
    }
}
