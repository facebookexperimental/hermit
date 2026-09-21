/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::VecDeque;
use std::ffi::CString;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::io::Seek;
use std::io::SeekFrom;
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::path::Component;
use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use reverie::Pid;
use serde::Deserialize;
use serde::Serialize;

const MAX_DIRECTORY_DEPTH: usize = 4096;
const MAX_SYMLINKS: usize = 40;
static TEMPORARY_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// One symlink encountered while resolving a recorded pathname. The lookup
/// ordinal is independent of pathname encoding and remains stable when a
/// symlink target adds more components to the resolution queue.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ResolvedSymlink {
    pub(crate) lookup_index: u64,
    pub(crate) target: Vec<u8>,
}

/// The object and symlink topology selected by Linux-like component ordering.
pub(crate) struct ResolvedPath {
    pub(crate) object: OwnedFd,
    pub(crate) symlinks: Vec<ResolvedSymlink>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FileIdentity {
    pub(crate) device: libc::dev_t,
    pub(crate) inode: libc::ino_t,
    pub(crate) mode: libc::mode_t,
    pub(crate) size: libc::off_t,
    pub(crate) mtime_seconds: libc::time_t,
    pub(crate) mtime_nanoseconds: libc::c_long,
}

#[derive(Debug)]
enum PendingComponent {
    Root,
    Parent,
    Normal(OsString),
}

/// Pins the tracee's filesystem root as a directory descriptor.
pub(crate) fn open_process_root(pid: Pid) -> io::Result<OwnedFd> {
    open_proc_directory(&format!("/proc/{}/root", pid.as_raw()))
}

/// Pins the tracee's current working directory as a directory descriptor.
pub(crate) fn open_process_cwd(pid: Pid) -> io::Result<OwnedFd> {
    open_proc_directory(&format!("/proc/{}/cwd", pid.as_raw()))
}

/// Pins one tracee descriptor when it currently names a directory.
pub(crate) fn open_process_directory_fd(pid: Pid, fd: RawFd) -> io::Result<OwnedFd> {
    open_proc_directory(&format!("/proc/{}/fd/{fd}", pid.as_raw()))
}

/// Pins one arbitrary tracee descriptor. The procfs magic link is intentionally
/// followed so the returned O_PATH descriptor names the tracee's object rather
/// than the controller's similarly spelled pathname.
pub(crate) fn open_process_fd(pid: Pid, fd: RawFd) -> io::Result<OwnedFd> {
    open_proc_path(&format!("/proc/{}/fd/{fd}", pid.as_raw()), libc::O_PATH)
}

/// Opens an arbitrary controller directory as a pinned O_PATH descriptor.
pub(crate) fn open_directory_path(path: &Path) -> io::Result<OwnedFd> {
    let path = component_cstring(path.as_os_str())?;
    owned_fd(unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    })
}

fn open_proc_directory(path: &str) -> io::Result<OwnedFd> {
    open_proc_path(path, libc::O_PATH | libc::O_DIRECTORY)
}

fn open_proc_path(path: &str, flags: libc::c_int) -> io::Result<OwnedFd> {
    let path = CString::new(path).expect("proc descriptor path cannot contain NUL");
    // Intentionally follow the procfs magic link: the returned fd pins the
    // tracee object, after which all confinement checks are descriptor based.
    let fd = unsafe { libc::open(path.as_ptr(), flags | libc::O_CLOEXEC) };
    owned_fd(fd)
}

fn owned_fd(fd: RawFd) -> io::Result<OwnedFd> {
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: a nonnegative return from open/openat/fcntl is a new owned fd.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

fn duplicate_fd(fd: RawFd) -> io::Result<OwnedFd> {
    owned_fd(unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) })
}

fn component_cstring(component: &OsStr) -> io::Result<CString> {
    CString::new(component.as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "guest pathname component contains NUL",
        )
    })
}

fn open_directory_at(parent: RawFd, component: &OsStr) -> io::Result<OwnedFd> {
    let component = component_cstring(component)?;
    let fd = unsafe {
        libc::openat(
            parent,
            component.as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    owned_fd(fd)
}

fn open_path_at(parent: RawFd, component: &OsStr) -> io::Result<OwnedFd> {
    let component = component_cstring(component)?;
    owned_fd(unsafe {
        libc::openat(
            parent,
            component.as_ptr(),
            libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    })
}

fn mkdir_at(parent: RawFd, component: &OsStr) -> io::Result<()> {
    let component = component_cstring(component)?;
    let result = unsafe { libc::mkdirat(parent, component.as_ptr(), 0o777) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn open_or_create_directory_at(parent: RawFd, component: &OsStr) -> io::Result<OwnedFd> {
    match open_directory_at(parent, component) {
        Ok(directory) => Ok(directory),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match mkdir_at(parent, component) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
            open_directory_at(parent, component)
        }
        Err(error) => Err(error),
    }
}

fn read_link_at(parent: RawFd, component: &OsStr) -> io::Result<OsString> {
    let component = component_cstring(component)?;
    let mut buffer = vec![0u8; 256];
    loop {
        let length = unsafe {
            libc::readlinkat(
                parent,
                component.as_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
            )
        };
        if length < 0 {
            return Err(io::Error::last_os_error());
        }
        let length = length as usize;
        if length < buffer.len() {
            buffer.truncate(length);
            return Ok(OsString::from_vec(buffer));
        }
        buffer.resize(buffer.len() * 2, 0);
    }
}

fn create_symlink_at(parent: RawFd, component: &OsStr, target: &OsStr) -> io::Result<()> {
    let component = component_cstring(component)?;
    let target = component_cstring(target)?;
    let result = unsafe { libc::symlinkat(target.as_ptr(), parent, component.as_ptr()) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn unlink_at(parent: RawFd, component: &OsStr) -> io::Result<()> {
    let component = component_cstring(component)?;
    let result = unsafe { libc::unlinkat(parent, component.as_ptr(), 0) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn directory_identity(fd: RawFd) -> io::Result<(libc::dev_t, libc::ino_t)> {
    let mut stat = MaybeUninit::<libc::stat>::zeroed();
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fstat initialized the complete stat structure on success.
    let stat = unsafe { stat.assume_init() };
    Ok((stat.st_dev, stat.st_ino))
}

pub(crate) fn file_identity(fd: RawFd) -> io::Result<FileIdentity> {
    let mut stat = MaybeUninit::<libc::stat>::zeroed();
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fstat initialized the complete stat structure on success.
    let stat = unsafe { stat.assume_init() };
    Ok(FileIdentity {
        device: stat.st_dev,
        inode: stat.st_ino,
        mode: stat.st_mode,
        size: stat.st_size,
        mtime_seconds: stat.st_mtime,
        mtime_nanoseconds: stat.st_mtime_nsec,
    })
}

fn is_directory(identity: FileIdentity) -> bool {
    identity.mode & libc::S_IFMT == libc::S_IFDIR
}

fn is_symlink(identity: FileIdentity) -> bool {
    identity.mode & libc::S_IFMT == libc::S_IFLNK
}

pub(crate) fn is_regular_file(identity: FileIdentity) -> bool {
    identity.mode & libc::S_IFMT == libc::S_IFREG
}

/// Reopens an O_PATH descriptor for reading without consulting its original
/// pathname. This preserves the selected object across rename/unlink races.
pub(crate) fn open_readable_fd(fd: RawFd) -> io::Result<File> {
    let path = CString::new(format!("/proc/self/fd/{fd}"))
        .expect("controller descriptor path cannot contain NUL");
    let readable = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    owned_fd(readable).map(File::from)
}

fn same_directory(left: RawFd, right: RawFd) -> io::Result<bool> {
    Ok(directory_identity(left)? == directory_identity(right)?)
}

/// Checks ancestry using only pinned directory descriptors. No pathname string
/// is used as a security or confinement boundary.
pub(crate) fn directory_is_beneath(root: &OwnedFd, directory: &OwnedFd) -> io::Result<bool> {
    let mut current = duplicate_fd(directory.as_raw_fd())?;
    for _ in 0..MAX_DIRECTORY_DEPTH {
        if same_directory(current.as_raw_fd(), root.as_raw_fd())? {
            return Ok(true);
        }
        let parent = open_directory_at(current.as_raw_fd(), OsStr::new(".."))?;
        if same_directory(current.as_raw_fd(), parent.as_raw_fd())? {
            return Ok(false);
        }
        current = parent;
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "directory ancestry exceeded safety limit",
    ))
}

fn append_components(queue: &mut VecDeque<PendingComponent>, path: &Path) -> io::Result<()> {
    for component in path.components().rev() {
        let component = match component {
            Component::RootDir => PendingComponent::Root,
            Component::CurDir => continue,
            Component::ParentDir => PendingComponent::Parent,
            Component::Normal(component) => PendingComponent::Normal(component.to_os_string()),
            Component::Prefix(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "non-Unix guest pathname",
                ));
            }
        };
        queue.push_front(component);
    }
    Ok(())
}

fn initial_directory(root: &OwnedFd, start: &OwnedFd, path: &Path) -> io::Result<OwnedFd> {
    if !path.is_absolute() && !directory_is_beneath(root, start)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "relative pathname base is outside replay root",
        ));
    }
    if path.is_absolute() {
        duplicate_fd(root.as_raw_fd())
    } else {
        duplicate_fd(start.as_raw_fd())
    }
}

fn parent_directory(root: &OwnedFd, current: &OwnedFd) -> io::Result<OwnedFd> {
    if same_directory(current.as_raw_fd(), root.as_raw_fd())? {
        return duplicate_fd(root.as_raw_fd());
    }
    let parent = open_directory_at(current.as_raw_fd(), OsStr::new(".."))?;
    if !directory_is_beneath(root, &parent)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "pathname traversal escaped replay root",
        ));
    }
    Ok(parent)
}

/// Resolves an existing pathname using descriptor-only traversal. Symlinks are
/// followed in the same order as Linux, so `link/..` first traverses `link` and
/// only then applies the parent component. The final symlink can optionally be
/// returned without following it for `AT_SYMLINK_NOFOLLOW` classification.
pub(crate) fn resolve_existing_path(
    root: &OwnedFd,
    start: &OwnedFd,
    path: &Path,
    nofollow_final: bool,
) -> io::Result<ResolvedPath> {
    if path.as_os_str().is_empty() {
        return Err(io::Error::from_raw_os_error(libc::ENOENT));
    }

    let mut current = initial_directory(root, start, path)?;
    let mut pending = VecDeque::new();
    append_components(&mut pending, path)?;
    let mut followed_symlinks = 0usize;
    let mut lookup_index = 0u64;
    let mut symlinks = Vec::new();

    while let Some(component) = pending.pop_front() {
        match component {
            PendingComponent::Root => current = duplicate_fd(root.as_raw_fd())?,
            PendingComponent::Parent => current = parent_directory(root, &current)?,
            PendingComponent::Normal(component) => {
                lookup_index = lookup_index.checked_add(1).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "pathname lookup count overflow")
                })?;
                let object = open_path_at(current.as_raw_fd(), &component)?;
                let identity = file_identity(object.as_raw_fd())?;
                let final_component = pending.is_empty();
                if is_symlink(identity) && !(final_component && nofollow_final) {
                    followed_symlinks += 1;
                    if followed_symlinks > MAX_SYMLINKS {
                        return Err(io::Error::from_raw_os_error(libc::ELOOP));
                    }
                    let target = read_link_at(current.as_raw_fd(), &component)?;
                    symlinks.push(ResolvedSymlink {
                        lookup_index,
                        target: target.as_bytes().to_vec(),
                    });
                    append_components(&mut pending, Path::new(&target))?;
                    continue;
                }
                if final_component {
                    return Ok(ResolvedPath { object, symlinks });
                }
                if !is_directory(identity) {
                    return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
                }
                current = object;
            }
        }
    }

    Ok(ResolvedPath {
        object: current,
        symlinks,
    })
}

fn ensure_expected_symlink(
    parent: RawFd,
    component: &OsStr,
    expected: &ResolvedSymlink,
) -> io::Result<OsString> {
    let expected_target = OsString::from_vec(expected.target.clone());
    match read_link_at(parent, component) {
        Ok(actual) if actual == expected_target => Ok(actual),
        Ok(actual) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "recorded exec symlink target mismatch at {component:?}: expected {expected_target:?}, found {actual:?}"
            ),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            create_symlink_at(parent, component, &expected_target)?;
            Ok(expected_target)
        }
        Err(error) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("recorded exec symlink collides at {component:?}: {error}"),
        )),
    }
}

fn temporary_name() -> OsString {
    let sequence = TEMPORARY_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    OsString::from(format!(".hermit-exec-{}-{sequence}", std::process::id()))
}

fn create_temporary_file(parent: RawFd) -> io::Result<(OsString, OwnedFd)> {
    for _ in 0..128 {
        let name = temporary_name();
        let name_c = component_cstring(&name)?;
        let fd = unsafe {
            libc::openat(
                parent,
                name_c.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        match owned_fd(fd) {
            Ok(fd) => return Ok((name, fd)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate confined exec staging file",
    ))
}

/// Creates a collision-free regular file directly beneath a pinned replay
/// root. The returned component can be opened by the stopped guest and then
/// unlinked without ever resolving a controller pathname through guest-created
/// symlinks.
pub(crate) fn create_root_temporary_regular_file(
    root: &OwnedFd,
    source: &mut File,
    expected_digest: detcore::Digest,
    mode: u32,
) -> io::Result<OsString> {
    let (name, temporary_fd) = create_temporary_file(root.as_raw_fd())?;
    let result = (|| {
        source.seek(SeekFrom::Start(0))?;
        let mut output = File::from(temporary_fd);
        io::copy(source, &mut output)?;
        if unsafe { libc::fchmod(output.as_raw_fd(), mode as libc::mode_t) } != 0 {
            return Err(io::Error::last_os_error());
        }
        output.sync_all()?;
        let actual = detcore::Digest::digest_reader(open_readable_fd(output.as_raw_fd())?)?;
        if actual != expected_digest {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "temporary recorded exec digest mismatch: expected {expected_digest}, found {actual}"
                ),
            ));
        }
        Ok(())
    })();
    if let Err(error) = result {
        let _ = unlink_at(root.as_raw_fd(), &name);
        return Err(error);
    }
    Ok(name)
}

pub(crate) fn unlink_root_entry(root: &OwnedFd, component: &OsStr) -> io::Result<()> {
    unlink_at(root.as_raw_fd(), component)
}

fn replace_regular_file_at(
    parent: RawFd,
    component: &OsStr,
    source: &mut File,
    mode: u32,
) -> io::Result<()> {
    let (temporary_name, temporary_fd) = create_temporary_file(parent)?;
    let result = (|| {
        source.seek(SeekFrom::Start(0))?;
        let mut output = File::from(temporary_fd);
        io::copy(source, &mut output)?;
        if unsafe { libc::fchmod(output.as_raw_fd(), mode as libc::mode_t) } != 0 {
            return Err(io::Error::last_os_error());
        }
        output.sync_all()?;
        let temporary = component_cstring(&temporary_name)?;
        let destination = component_cstring(component)?;
        if unsafe { libc::renameat(parent, temporary.as_ptr(), parent, destination.as_ptr()) } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = unlink_at(parent, &temporary_name);
    }
    result
}

fn materialize_regular_file_at(
    parent: RawFd,
    component: &OsStr,
    source: &mut File,
    expected_digest: detcore::Digest,
    mode: u32,
) -> io::Result<()> {
    match open_path_at(parent, component) {
        Ok(existing) => {
            let identity = file_identity(existing.as_raw_fd())?;
            if !is_regular_file(identity) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("recorded exec destination is not a regular file: {component:?}"),
                ));
            }
            let actual_mode = identity.mode & 0o7777;
            let digest = detcore::Digest::digest_reader(open_readable_fd(existing.as_raw_fd())?)?;
            if digest == expected_digest && actual_mode == mode {
                return Ok(());
            }
            replace_regular_file_at(parent, component, source, mode)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            replace_regular_file_at(parent, component, source, mode)
        }
        Err(error) => Err(error),
    }
}

/// Materializes one recorded regular file beneath a pinned replay root. Every
/// parent component is opened with `O_NOFOLLOW`; only symlinks explicitly
/// captured during recording are created/followed, and their targets must match
/// byte-for-byte. Final replacement uses renameat relative to a pinned parent,
/// so even a racing symlink cannot redirect writes outside the replay root.
pub(crate) fn materialize_regular_file(
    root: &OwnedFd,
    start: &OwnedFd,
    path: &Path,
    symlinks: &[ResolvedSymlink],
    source: &mut File,
    expected_digest: detcore::Digest,
    mode: u32,
) -> io::Result<()> {
    if path.as_os_str().is_empty() {
        return Err(io::Error::from_raw_os_error(libc::ENOENT));
    }
    let mut current = initial_directory(root, start, path)?;
    let mut pending = VecDeque::new();
    append_components(&mut pending, path)?;
    let mut symlink_index = 0usize;
    let mut lookup_index = 0u64;
    let mut followed_symlinks = 0usize;

    while let Some(component) = pending.pop_front() {
        match component {
            PendingComponent::Root => current = duplicate_fd(root.as_raw_fd())?,
            PendingComponent::Parent => current = parent_directory(root, &current)?,
            PendingComponent::Normal(component) => {
                lookup_index = lookup_index.checked_add(1).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "pathname lookup count overflow")
                })?;
                if let Some(expected) = symlinks.get(symlink_index)
                    && expected.lookup_index == lookup_index
                {
                    let target =
                        ensure_expected_symlink(current.as_raw_fd(), &component, expected)?;
                    symlink_index += 1;
                    followed_symlinks += 1;
                    if followed_symlinks > MAX_SYMLINKS {
                        return Err(io::Error::from_raw_os_error(libc::ELOOP));
                    }
                    append_components(&mut pending, Path::new(&target))?;
                    continue;
                }
                if symlinks
                    .get(symlink_index)
                    .is_some_and(|expected| expected.lookup_index < lookup_index)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "recorded exec symlink lookup order is invalid",
                    ));
                }
                if pending.is_empty() {
                    materialize_regular_file_at(
                        current.as_raw_fd(),
                        &component,
                        source,
                        expected_digest,
                        mode,
                    )?;
                    if symlink_index != symlinks.len() {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "recorded exec has unused symlink topology",
                        ));
                    }
                    return Ok(());
                }
                current = match open_or_create_directory_at(current.as_raw_fd(), &component) {
                    Ok(directory) => directory,
                    Err(error) => {
                        if read_link_at(current.as_raw_fd(), &component).is_ok() {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                format!(
                                    "unexpected symlink in recorded exec destination at {component:?}"
                                ),
                            ));
                        }
                        return Err(error);
                    }
                };
            }
        }
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "recorded exec path did not contain a file component",
    ))
}

/// Ensures that `path` names a directory beneath `root`, interpreting each
/// component with Linux-like symlink and `..` ordering. Intermediate symlinks
/// are followed; the final component must be a directory rather than a symlink.
/// Missing directory components are created relative to pinned parent fds.
pub(crate) fn ensure_directory_path(
    root: &OwnedFd,
    start: &OwnedFd,
    path: &Path,
) -> io::Result<()> {
    ensure_directory_path_impl(root, start, path, false)
}

/// As [`ensure_directory_path`], but follows an existing final symlink. This
/// matches `chdir(2)` and mount-target lookup, which resolve the final
/// component, while retaining descriptor-rooted confinement.
pub(crate) fn ensure_directory_path_follow_final(
    root: &OwnedFd,
    start: &OwnedFd,
    path: &Path,
) -> io::Result<()> {
    ensure_directory_path_impl(root, start, path, true)
}

fn ensure_directory_path_impl(
    root: &OwnedFd,
    start: &OwnedFd,
    path: &Path,
    follow_final_symlink: bool,
) -> io::Result<()> {
    if !path.is_absolute() && !directory_is_beneath(root, start)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "relative directory base is outside replay root",
        ));
    }

    let mut current = if path.is_absolute() {
        duplicate_fd(root.as_raw_fd())?
    } else {
        duplicate_fd(start.as_raw_fd())?
    };
    let mut pending = VecDeque::new();
    append_components(&mut pending, path)?;
    let mut followed_symlinks = 0usize;

    while let Some(component) = pending.pop_front() {
        match component {
            PendingComponent::Root => current = duplicate_fd(root.as_raw_fd())?,
            PendingComponent::Parent => {
                if same_directory(current.as_raw_fd(), root.as_raw_fd())? {
                    continue;
                }
                let parent = open_directory_at(current.as_raw_fd(), OsStr::new(".."))?;
                if !directory_is_beneath(root, &parent)? {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "directory traversal escaped replay root",
                    ));
                }
                current = parent;
            }
            PendingComponent::Normal(component) => {
                match open_or_create_directory_at(current.as_raw_fd(), &component) {
                    Ok(directory) => current = directory,
                    Err(open_error) => {
                        let target = match read_link_at(current.as_raw_fd(), &component) {
                            Ok(target) if !pending.is_empty() || follow_final_symlink => target,
                            Ok(_) => {
                                return Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "final replay directory component is a symlink",
                                ));
                            }
                            Err(_) => return Err(open_error),
                        };
                        followed_symlinks += 1;
                        if followed_symlinks > MAX_SYMLINKS {
                            return Err(io::Error::from_raw_os_error(libc::ELOOP));
                        }
                        append_components(&mut pending, Path::new(&target))?;
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write as _;
    use std::os::unix::fs::symlink;

    use super::*;

    fn open_test_directory(path: &Path) -> OwnedFd {
        let path = CString::new(path.as_os_str().as_bytes()).unwrap();
        owned_fd(unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        })
        .unwrap()
    }

    #[test]
    fn creates_missing_directory_components() {
        let temporary = tempfile::tempdir().unwrap();
        let root = open_test_directory(temporary.path());

        ensure_directory_path(&root, &root, Path::new("a/b/c")).unwrap();

        assert!(temporary.path().join("a/b/c").is_dir());
    }

    #[test]
    fn rejects_existing_file_and_final_symlink() {
        let temporary = tempfile::tempdir().unwrap();
        fs::write(temporary.path().join("file"), b"keep").unwrap();
        symlink("file", temporary.path().join("link")).unwrap();
        let root = open_test_directory(temporary.path());

        assert!(ensure_directory_path(&root, &root, Path::new("file")).is_err());
        assert!(ensure_directory_path(&root, &root, Path::new("link")).is_err());
        assert_eq!(fs::read(temporary.path().join("file")).unwrap(), b"keep");
        assert!(
            fs::symlink_metadata(temporary.path().join("link"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn follows_intermediate_symlink_before_parent_component() {
        let temporary = tempfile::tempdir().unwrap();
        symlink("real/deep", temporary.path().join("link")).unwrap();
        let root = open_test_directory(temporary.path());

        ensure_directory_path(&root, &root, Path::new("link/../target")).unwrap();

        assert!(temporary.path().join("real/deep").is_dir());
        assert!(temporary.path().join("real/target").is_dir());
        assert!(!temporary.path().join("target").exists());
    }

    #[test]
    fn relative_outside_base_is_rejected_but_absolute_path_ignores_it() {
        let root_dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let root = open_test_directory(root_dir.path());
        let outside = open_test_directory(outside_dir.path());

        assert!(!directory_is_beneath(&root, &outside).unwrap());
        assert!(ensure_directory_path(&root, &outside, Path::new("relative")).is_err());
        ensure_directory_path(&root, &outside, Path::new("/absolute")).unwrap();

        assert!(root_dir.path().join("absolute").is_dir());
        assert!(!outside_dir.path().join("relative").exists());
        assert!(!outside_dir.path().join("absolute").exists());
    }

    #[test]
    fn resolves_symlink_before_parent_component() {
        let temporary = tempfile::tempdir().unwrap();
        fs::create_dir_all(temporary.path().join("real/deep")).unwrap();
        fs::create_dir_all(temporary.path().join("real/target")).unwrap();
        fs::write(temporary.path().join("real/target/prog"), b"program").unwrap();
        symlink("real/deep", temporary.path().join("link")).unwrap();
        let root = open_test_directory(temporary.path());

        let resolved =
            resolve_existing_path(&root, &root, Path::new("link/../target/prog"), false).unwrap();

        assert_eq!(
            detcore::Digest::digest_reader(open_readable_fd(resolved.object.as_raw_fd()).unwrap())
                .unwrap(),
            detcore::Digest::new(b"program")
        );
        assert_eq!(
            resolved.symlinks,
            vec![ResolvedSymlink {
                lookup_index: 1,
                target: b"real/deep".to_vec(),
            }]
        );
    }

    fn source_file(contents: &[u8]) -> (tempfile::NamedTempFile, detcore::Digest) {
        let mut source = tempfile::NamedTempFile::new().unwrap();
        source.write_all(contents).unwrap();
        source.flush().unwrap();
        (source, detcore::Digest::new(contents))
    }

    #[test]
    fn materialization_rejects_parent_symlink_escape() {
        let root_dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), root_dir.path().join("escape")).unwrap();
        let root = open_test_directory(root_dir.path());
        let (mut source, digest) = source_file(b"recorded");

        assert!(
            materialize_regular_file(
                &root,
                &root,
                Path::new("escape/canary"),
                &[],
                source.as_file_mut(),
                digest,
                0o755,
            )
            .is_err()
        );
        assert!(!outside.path().join("canary").exists());
    }

    #[test]
    fn materialization_rejects_final_symlink_escape() {
        let root_dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let canary = outside.path().join("canary");
        fs::write(&canary, b"keep").unwrap();
        symlink(&canary, root_dir.path().join("program")).unwrap();
        let root = open_test_directory(root_dir.path());
        let (mut source, digest) = source_file(b"replace");

        assert!(
            materialize_regular_file(
                &root,
                &root,
                Path::new("program"),
                &[],
                source.as_file_mut(),
                digest,
                0o755,
            )
            .is_err()
        );
        assert_eq!(fs::read(canary).unwrap(), b"keep");
    }

    #[test]
    fn recorded_absolute_symlink_is_followed_inside_pinned_root() {
        let root_dir = tempfile::tempdir().unwrap();
        let root = open_test_directory(root_dir.path());
        let (mut source, digest) = source_file(b"recorded");
        let topology = [ResolvedSymlink {
            lookup_index: 1,
            target: b"/inside".to_vec(),
        }];

        materialize_regular_file(
            &root,
            &root,
            Path::new("link/program"),
            &topology,
            source.as_file_mut(),
            digest,
            0o751,
        )
        .unwrap();

        assert_eq!(
            fs::read_link(root_dir.path().join("link")).unwrap(),
            Path::new("/inside")
        );
        assert_eq!(
            fs::read(root_dir.path().join("inside/program")).unwrap(),
            b"recorded"
        );
    }
}
