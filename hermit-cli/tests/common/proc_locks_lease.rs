/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Shared by the integration fixture and the manifest runner. The lease only
// coordinates cooperating /proc/locks fixtures; it cannot isolate arbitrary
// host OFD holders.
use std::ffi::OsStr;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::ErrorKind;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;
use std::time::Instant;

pub(crate) const PROC_LOCKS_LEASE_NAME: &str = "hermit-proc-locks-determinism.lock";

pub(crate) fn effective_uid() -> u32 {
    // SAFETY: `geteuid` takes no arguments and has no memory-safety preconditions.
    unsafe { libc::geteuid() }
}

pub(crate) fn validated_runtime_directory(
    configured: Option<&OsStr>,
    fallback: &Path,
    expected_uid: u32,
) -> io::Result<PathBuf> {
    let configured = configured.map(Path::new);
    let directory = match configured {
        Some(path) if !path.as_os_str().is_empty() && path.is_absolute() => path,
        _ => fallback,
    };
    if !directory.is_absolute() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "proc-locks runtime directory is not absolute",
        ));
    }
    let metadata = fs::symlink_metadata(directory)?;
    if !metadata.file_type().is_dir() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "proc-locks runtime path is not a directory",
        ));
    }
    if metadata.uid() != expected_uid {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            "proc-locks runtime directory is not owned by the current user",
        ));
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            "proc-locks runtime directory is accessible by another user",
        ));
    }
    Ok(directory.to_path_buf())
}

pub(crate) fn open_proc_locks_snapshot_lease_file(
    runtime_directory: &Path,
    expected_uid: u32,
) -> io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(runtime_directory.join(PROC_LOCKS_LEASE_NAME))?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "proc-locks snapshot lease is not a regular file",
        ));
    }
    if metadata.uid() != expected_uid {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            "proc-locks snapshot lease is not owned by the current user",
        ));
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            "proc-locks snapshot lease is accessible by another user",
        ));
    }
    Ok(file)
}

// The pinned launcher records the host file it safely opened. A mount-source
// replacement or a different inode must fail before any snapshot guest runs.
pub(crate) fn validate_proc_locks_host_identity(
    file: &File,
    expected: Option<&OsStr>,
) -> io::Result<()> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let expected = expected.to_str().ok_or_else(|| {
        io::Error::new(ErrorKind::InvalidInput, "non-UTF8 proc-locks host identity")
    })?;
    let (device, inode) = expected.split_once(':').ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidInput,
            "malformed proc-locks host identity",
        )
    })?;
    let parse = |value: &str| -> io::Result<u64> {
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "malformed proc-locks host identity",
            ));
        }
        value
            .parse()
            .map_err(|error| io::Error::new(ErrorKind::InvalidInput, error))
    };
    let expected = (parse(device)?, parse(inode)?);
    let metadata = file.metadata()?;
    if (metadata.dev(), metadata.ino()) != expected {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "mounted proc-locks lease differs from the host inode",
        ));
    }
    Ok(())
}

/// Acquire the already authenticated inode within the caller's existing
/// execution deadline. Opening O_NONBLOCK alone does not make flock bounded.
/// The returned descriptor releases the lease on drop, including error unwinds.
pub(crate) fn lock_until(file: File, deadline: Instant) -> io::Result<File> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                ErrorKind::TimedOut,
                "proc-locks lease exhausted the cell execution deadline before guest launch",
            ));
        }
        // SAFETY: file owns a valid descriptor and stays alive in the returned
        // guard. LOCK_NB prevents contention from blocking beyond the deadline.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    ErrorKind::TimedOut,
                    "proc-locks lease exhausted the cell execution deadline before guest launch",
                ));
            }
            return Ok(file);
        }
        let error = io::Error::last_os_error();
        if !matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) {
            return Err(error);
        }
        thread::sleep(
            Duration::from_millis(10).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}
