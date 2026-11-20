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
use std::fmt::Display;
use std::fs;
use std::fs::File;
use std::fs::FileTimes;
use std::fs::OpenOptions;
use std::io;
use std::io::ErrorKind;
use std::os::fd::AsRawFd;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

pub(crate) const PROC_LOCKS_LEASE_NAME: &str = "hermit-proc-locks-determinism.lock";

/// What a refusal of a runtime-directory candidate names.
const RUNTIME_DIRECTORY: &str = "proc-locks runtime directory";
/// What a refusal of the lease file names.
const SNAPSHOT_LEASE: &str = "proc-locks lease file";

/// A refusal that names what it refused: `<what> <path>: <problem>`.
fn refusal(kind: ErrorKind, what: &str, path: &Path, problem: impl Display) -> io::Error {
    io::Error::new(kind, format!("{what} {}: {problem}", path.display()))
}

/// `error` from the operating system with the path it concerns, which its own
/// message does not name. The kind is kept.
fn naming(what: &str, path: &Path, error: io::Error) -> io::Error {
    refusal(error.kind(), what, path, error)
}

pub(crate) fn effective_uid() -> u32 {
    // SAFETY: `geteuid` takes no arguments and has no memory-safety preconditions.
    unsafe { libc::geteuid() }
}

/// The one host path, per user, where the lease lives when the host has no
/// runtime directory for that user: under /tmp, which every process of the
/// user on the host shares. TMPDIR is routinely set per test or per sandbox,
/// and a lease there would serialize nothing.
pub(crate) fn shared_runtime_directory(uid: u32) -> PathBuf {
    PathBuf::from(format!("/tmp/hermit-proc-locks-{uid}"))
}

/// The owner-only directory the lease lives in. The candidates, in order:
/// - `configured` (`XDG_RUNTIME_DIR`), when absolute;
/// - `fallback` (`/run/user/<euid>`);
/// - `shared` ([`shared_runtime_directory`]), created owner-only on demand,
///   so cooperating fixtures on a host without a runtime directory still
///   share one lease file.
///
/// A candidate that does not exist is unusable, and the next one is tried.
/// Sandboxes forward a stale `XDG_RUNTIME_DIR` naming no directory: the
/// fbsource sudo lanes run as root and forward `/run/user/0`, which exists
/// only while root has a login session or lingers (`loginctl enable-linger`).
/// A candidate that exists is validated
/// as before: a directory, not a symlink, owned by the current user, with no
/// group or other access. An unsafe one refuses rather than being skipped, as
/// does any lookup error other than "does not exist", and the lease is never
/// skipped. So a directory another user created at the shared path refuses
/// every acquisition on the host until it is removed.
///
/// The runner chooses per acquisition. An integration node chooses once, when
/// it is staged: run_node.py points XDG_RUNTIME_DIR at the chosen directory,
/// and its fixtures keep it until the node ends. So if `/run/user/<euid>`
/// appears or disappears while the lease is held, the next acquirer, or a
/// staged node's fixtures, can take a different directory from the holder's
/// until the holder releases it.
pub(crate) fn validated_runtime_directory(
    configured: Option<&OsStr>,
    fallback: &Path,
    shared: &Path,
    expected_uid: u32,
) -> io::Result<PathBuf> {
    let configured = configured
        .map(Path::new)
        .filter(|path| !path.as_os_str().is_empty() && path.is_absolute());
    for candidate in configured.into_iter().chain([fallback]) {
        if !candidate.is_absolute() {
            return Err(refusal(
                ErrorKind::InvalidInput,
                RUNTIME_DIRECTORY,
                candidate,
                "not absolute",
            ));
        }
        match fs::symlink_metadata(candidate) {
            Ok(metadata) => return owner_only_directory(candidate, &metadata, expected_uid),
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(naming(RUNTIME_DIRECTORY, candidate, error)),
        }
    }
    if !shared.is_absolute() {
        return Err(refusal(
            ErrorKind::InvalidInput,
            RUNTIME_DIRECTORY,
            shared,
            "not absolute",
        ));
    }
    match fs::DirBuilder::new().mode(0o700).create(shared) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
        Err(error) => return Err(naming(RUNTIME_DIRECTORY, shared, error)),
    }
    let metadata =
        fs::symlink_metadata(shared).map_err(|error| naming(RUNTIME_DIRECTORY, shared, error))?;
    owner_only_directory(shared, &metadata, expected_uid)
}

/// `directory`, if its `metadata` shows an owner-only directory of
/// `expected_uid`: a directory, not a symlink, owned by that user, with no
/// group or other access.
fn owner_only_directory(
    directory: &Path,
    metadata: &fs::Metadata,
    expected_uid: u32,
) -> io::Result<PathBuf> {
    if !directory.is_absolute() {
        return Err(refusal(
            ErrorKind::InvalidInput,
            RUNTIME_DIRECTORY,
            directory,
            "not absolute",
        ));
    }
    if !metadata.file_type().is_dir() {
        return Err(refusal(
            ErrorKind::InvalidInput,
            RUNTIME_DIRECTORY,
            directory,
            "not a directory",
        ));
    }
    owner_only(RUNTIME_DIRECTORY, directory, metadata, expected_uid)?;
    Ok(directory.to_path_buf())
}

/// Refuse `path` unless `metadata` shows it owned by `expected_uid` with no
/// group or other access.
fn owner_only(
    what: &str,
    path: &Path,
    metadata: &fs::Metadata,
    expected_uid: u32,
) -> io::Result<()> {
    if metadata.uid() != expected_uid {
        return Err(refusal(
            ErrorKind::PermissionDenied,
            what,
            path,
            format_args!(
                "owned by uid {}, not by the current user (uid {expected_uid})",
                metadata.uid()
            ),
        ));
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(refusal(
            ErrorKind::PermissionDenied,
            what,
            path,
            format_args!(
                "mode {:04o} gives another user access",
                metadata.mode() & 0o7777
            ),
        ));
    }
    Ok(())
}

pub(crate) fn open_proc_locks_snapshot_lease_file(
    runtime_directory: &Path,
    expected_uid: u32,
) -> io::Result<File> {
    let path = runtime_directory.join(PROC_LOCKS_LEASE_NAME);
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)
        .map_err(|error| naming(SNAPSHOT_LEASE, &path, error))?;
    let metadata = file
        .metadata()
        .map_err(|error| naming(SNAPSHOT_LEASE, &path, error))?;
    if !metadata.file_type().is_file() {
        return Err(refusal(
            ErrorKind::InvalidInput,
            SNAPSHOT_LEASE,
            &path,
            "not a regular file",
        ));
    }
    owner_only(SNAPSHOT_LEASE, &path, &metadata, expected_uid)?;
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

/// Refresh the access and modification times of a lease file just locked.
/// Every acquirer calls it once it holds the lease, the runner through
/// [`lock_until`] and the integration fixture after its own blocking flock.
/// Periodic /tmp cleaners delete files left untouched for days
/// (ci/buck-e2e/cell.sh refreshes its own /tmp locks for the same reason). A
/// lease file in use must never look stale: deleted while held, the next
/// acquirer would lock a new file.
pub(crate) fn refresh_lease_times(file: &File) -> io::Result<()> {
    let now = SystemTime::now();
    let times = FileTimes::new().set_accessed(now).set_modified(now);
    file.set_times(times).map_err(|error| {
        let message = format!("{SNAPSHOT_LEASE}: refreshing its times: {error}");
        io::Error::new(error.kind(), message)
    })
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
            refresh_lease_times(&file)?;
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
