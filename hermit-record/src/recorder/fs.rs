/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::FileExt;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;

use reverie::Errno;
use reverie::Guest;
use reverie::Stack;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Ftruncate;
use reverie::syscalls::Getcwd;
#[cfg(target_arch = "x86_64")]
use reverie::syscalls::Getdents;
use reverie::syscalls::Getdents64;
use reverie::syscalls::Ioctl;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Pread64;
use reverie::syscalls::Read;
use reverie::syscalls::ReadAddr;
#[cfg(target_arch = "x86_64")]
use reverie::syscalls::Readlink;
use reverie::syscalls::Sendfile;
use reverie::syscalls::Statx;
use reverie::syscalls::Syscall;
use reverie::syscalls::family::StatFamily;
use reverie::syscalls::family::WriteFamily;
use reverie::syscalls::ioctl;

use super::Recorder;
use crate::event::FileCloneEvent;
use crate::event::FileCloneImage;
use crate::event::FileExtent;
use crate::event::FtruncateEvent;
use crate::event::ReadEvent;
use crate::event::ReplayFdKind;
use crate::event::SendfileEvent;
use crate::event::StatEvent;
use crate::event::SyscallEvent;
use crate::event::WriteEvent;
use crate::event::deterministic_ioctl_error;
use crate::event::xattr_query_output;
use crate::vectored_offset;

/// Read the first `length` output bytes of a vectored read from the guest's
/// `iovec` array, flattened in read order. `length` is the syscall return value,
/// which may be smaller than the total iovec capacity (a short read), so we stop
/// once `length` bytes have been collected.
fn read_iovec_output<M: MemoryAccess>(
    memory: &M,
    iov_addr: Option<usize>,
    iovcnt: usize,
    length: usize,
) -> Result<Vec<u8>, Errno> {
    let mut buf = vec![0u8; length];
    if length == 0 {
        return Ok(buf);
    }
    let addr = iov_addr
        .and_then(Addr::<libc::iovec>::from_raw)
        .ok_or(Errno::EFAULT)?;
    let mut iovecs = vec![
        libc::iovec {
            iov_base: std::ptr::null_mut(),
            iov_len: 0,
        };
        iovcnt
    ];
    memory.read_values(addr, &mut iovecs)?;

    let mut filled = 0;
    for iovec in iovecs {
        if filled == length {
            break;
        }
        let take = (length - filled).min(iovec.iov_len);
        if take == 0 {
            continue;
        }
        let src = Addr::<u8>::from_raw(iovec.iov_base as usize).ok_or(Errno::EFAULT)?;
        memory.read_exact(src, &mut buf[filled..filled + take])?;
        filled += take;
    }
    Ok(buf)
}

fn consumed_sigpipe_count(pid: libc::pid_t, fd: libc::c_int, bytes: &[u8]) -> u64 {
    let path = format!("/proc/{pid}/fd/{fd}");
    if !std::fs::read_link(path)
        .is_ok_and(|target| target == std::path::Path::new("anon_inode:[signalfd]"))
    {
        return 0;
    }

    let (records, _) = bytes.as_chunks::<{ std::mem::size_of::<libc::signalfd_siginfo>() }>();
    records
        .iter()
        .filter(|info| {
            u32::from_ne_bytes(info[..std::mem::size_of::<u32>()].try_into().unwrap())
                == libc::SIGPIPE as u32
        })
        .count()
        .try_into()
        .expect("SIGPIPE signalfd record count overflow")
}

/// Where a write puts its bytes in a regular file.
#[derive(Clone, Copy, Debug)]
enum WritePlacement {
    /// At the descriptor's position, which the write advances: write, writev,
    /// and sendfile's output.
    Position,
    /// At an explicit offset, leaving the position: pwrite64 and pwritev.
    Explicit(i64),
    /// pwritev2: at an explicit offset unless it is -1, with its flags.
    Pwritev2 { offset: i64, flags: libc::c_int },
}

impl WritePlacement {
    fn of(syscall: WriteFamily) -> Self {
        match syscall {
            WriteFamily::Write(_) | WriteFamily::Writev(_) => Self::Position,
            WriteFamily::Pwrite64(call) => Self::Explicit(call.offset()),
            WriteFamily::Pwritev(call) => {
                Self::Explicit(vectored_offset(call.pos_l(), call.pos_h()))
            }
            WriteFamily::Pwritev2(call) => Self::Pwritev2 {
                offset: vectored_offset(call.pos_l(), call.pos_h()),
                flags: call.flags(),
            },
        }
    }
}

fn write_advances_output_offset(placement: WritePlacement) -> bool {
    match placement {
        WritePlacement::Position => true,
        WritePlacement::Pwritev2 { offset, .. } => offset == -1,
        WritePlacement::Explicit(_) => false,
    }
}

fn uses_append_offset(status_flags: libc::c_int, write_flags: libc::c_int) -> bool {
    write_flags & libc::RWF_APPEND != 0
        || (status_flags & libc::O_APPEND != 0 && write_flags & libc::RWF_NOAPPEND == 0)
}

#[cfg(test)]
fn shares_open_file_description(left: libc::c_int, right: libc::c_int) -> bool {
    match crate::fd::same_open_file_description(left, right) {
        Ok(shared) => shared,
        Err(error) => {
            tracing::debug!(
                %error,
                left,
                right,
                "could not compare captured open-file descriptions"
            );
            false
        }
    }
}

fn output_file_offset(
    pid: libc::pid_t,
    fd: libc::c_int,
    placement: WritePlacement,
    metadata: &std::fs::Metadata,
    count: i64,
) -> Option<i64> {
    if !metadata.file_type().is_file() {
        return None;
    }

    let fdinfo = std::fs::read_to_string(format!("/proc/{pid}/fdinfo/{fd}")).ok()?;
    let position = fdinfo
        .lines()
        .find_map(|line| line.strip_prefix("pos:\t"))
        .and_then(|value| value.parse::<i64>().ok())?;
    let status_flags = fdinfo
        .lines()
        .find_map(|line| line.strip_prefix("flags:\t"))
        .and_then(|value| i32::from_str_radix(value, 8).ok())
        .unwrap_or_default();
    let write_flags = match placement {
        WritePlacement::Pwritev2 { flags, .. } => flags,
        _ => 0,
    };
    if uses_append_offset(status_flags, write_flags) {
        return i64::try_from(metadata.len()).ok()?.checked_sub(count);
    }

    match placement {
        WritePlacement::Explicit(offset) => Some(offset),
        WritePlacement::Pwritev2 { offset, .. } => (offset != -1)
            .then_some(offset)
            .or_else(|| position.checked_sub(count)),
        WritePlacement::Position => position.checked_sub(count),
    }
}

/// Whether a user access can read and write `address`, by the mapping of a
/// `/proc/<pid>/maps` listing that holds it: `(readable, writable)`, where on
/// x86 a writable mapping is also readable. `None` if no mapping holds it.
fn mapping_permissions(maps: &str, address: usize) -> Option<(bool, bool)> {
    maps.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let (range, perms) = (fields.next()?, fields.next()?.as_bytes());
        let (start, end) = range.split_once('-')?;
        let (start, end) = (
            usize::from_str_radix(start, 16).ok()?,
            usize::from_str_radix(end, 16).ok()?,
        );
        let writable = perms.get(1) == Some(&b'w');
        (start..end)
            .contains(&address)
            .then_some((perms.first() == Some(&b'r') || writable, writable))
    })
}

/// The file-size limits (RLIMIT_FSIZE, soft) of the guest process `pid` and
/// of the recorder, in that order, `None` meaning unlimited; `None` overall
/// if either cannot be read.
fn file_size_limits(pid: reverie::Pid) -> Option<(Option<u64>, Option<u64>)> {
    let mut own = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `own` is a valid rlimit for getrlimit to fill.
    if unsafe { libc::getrlimit(libc::RLIMIT_FSIZE, &mut own) } != 0 {
        return None;
    }
    let recorder = (own.rlim_cur != libc::RLIM_INFINITY).then_some(own.rlim_cur);
    let limits = std::fs::read_to_string(format!("/proc/{}/limits", pid.as_raw())).ok()?;
    let guest = limits.lines().find_map(|line| {
        match line
            .strip_prefix("Max file size")?
            .split_whitespace()
            .next()?
        {
            "unlimited" => Some(None),
            limit => limit.parse::<u64>().ok().map(Some),
        }
    })?;
    Some((guest, recorder))
}

/// Whether a write ending at byte `end` stays within both the guest process
/// `pid`'s file-size limit and the recorder's, so that neither process can
/// reach EFBIG or SIGXFSZ and a write the recorder makes for the guest has the
/// guest's outcome.
fn within_file_size_limits(pid: reverie::Pid, end: u64) -> bool {
    file_size_limits(pid).is_some_and(|(guest, recorder)| {
        guest.is_none_or(|limit| end <= limit) && recorder.is_none_or(|limit| end <= limit)
    })
}

/// What `kcmp(KCMP_FILE)` says of a call's input and output descriptors.
#[derive(Debug, PartialEq, Eq)]
enum Descriptions {
    /// Two open file descriptions (kcmp orders them: 1 or 2).
    Distinct,
    /// One open file description (kcmp: 0).
    Shared,
    /// kcmp failed (-1: a kernel without it, or no access) and the two name
    /// one file, so either may be true and neither may be assumed.
    Unknown,
}

impl Descriptions {
    /// From kcmp's answer `order` and, for when kcmp fails, whether the two
    /// descriptors name the same file (`same_file`, by device and inode;
    /// `None` if that could not be read): two files are two descriptions.
    fn of(order: libc::c_long, same_file: Option<bool>) -> Self {
        match (order, same_file) {
            (0, _) => Self::Shared,
            (1 | 2, _) | (_, Some(false)) => Self::Distinct,
            _ => Self::Unknown,
        }
    }
}

/// Whether a copy of at most `count` bytes, written at the position of the
/// file `output` describes, ends within both file-size limits
/// (`within_file_size_limits`). `count` is the only bound: with input and
/// output one file, Linux reads back what the copy has just written and goes
/// on until `count` is used up, past the file's old end.
fn recorder_copy_fits(
    pid: reverie::Pid,
    output: &std::os::fd::OwnedFd,
    count: usize,
) -> Option<bool> {
    // SAFETY: `output` owns its descriptor; SEEK_CUR by 0 only reads the
    // position, where the copy writes.
    let at = u64::try_from(unsafe { libc::lseek(output.as_raw_fd(), 0, libc::SEEK_CUR) }).ok()?;
    Some(within_file_size_limits(pid, at.checked_add(count as u64)?))
}

/// `sendfile(output, input, &position, count)` made by the recorder on its
/// duplicates of the guest's two descriptors, so on the guest's own open file
/// descriptions, shared or not: the kernel makes the guest's copy. SIGXFSZ is
/// blocked on this thread for the call; whether the kernel raised one (at the
/// recorder's file-size limit) is returned beside the result, and the signal
/// is consumed.
fn recorder_sendfile(
    output: &std::os::fd::OwnedFd,
    input: &std::os::fd::OwnedFd,
    mut position: libc::loff_t,
    count: usize,
) -> (Result<i64, Errno>, bool) {
    // SAFETY: sigset operations on locals; pthread_sigmask changes only this
    // thread's mask, restored below; sendfile reads and updates `position`;
    // sigtimedwait with a zero timeout only takes a pending SIGXFSZ.
    unsafe {
        let mut xfsz: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut xfsz);
        libc::sigaddset(&mut xfsz, libc::SIGXFSZ);
        let mut previous: libc::sigset_t = std::mem::zeroed();
        libc::pthread_sigmask(libc::SIG_BLOCK, &xfsz, &mut previous);
        let copied = libc::sendfile(output.as_raw_fd(), input.as_raw_fd(), &mut position, count);
        let result = if copied < 0 {
            Err(Errno::last())
        } else {
            Ok(copied as i64)
        };
        let now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let raised = libc::sigtimedwait(&xfsz, std::ptr::null_mut(), &now) == libc::SIGXFSZ;
        libc::pthread_sigmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut());
        (result, raised)
    }
}

/// Runs `syscall`, whose offset argument holds `position` but is read-only,
/// so that it copies from `position` as Linux does without writing through
/// that argument.
///
/// - Two open file descriptions (`Descriptions::Distinct`): the input's
///   position is moved to `position` through the recorder's own descriptor,
///   the call runs with no offset argument, and the position is put back, so
///   the input's position is unchanged, as the offset form leaves it. The
///   guest makes the call; no guest memory is needed.
/// - A regular-file output otherwise (one description, or kcmp could not
///   tell): the recorder makes the call itself on duplicates of both
///   descriptors (`recorder_sendfile`), when the copy cannot reach either
///   file-size limit (`recorder_copy_fits`). SIGPIPE cannot arise for a
///   regular file.
/// - Otherwise: the offset goes in a scratch slot below the guest's stack and
///   the guest makes the call itself, meeting its own file-size limit.
///
/// With no room below the stack for that slot, a regular-file output is
/// copied by the recorder when its file-size limit is the guest's
/// (`copy_under_the_guests_limit`); anything else fails with EFAULT, as
/// Linux's call does, but without the copy Linux would have made.
async fn copy_at_offset<G: Guest<Recorder>>(
    guest: &mut G,
    pid: reverie::Pid,
    task: reverie::Pid,
    syscall: Sendfile,
    position: libc::loff_t,
) -> Result<Result<i64, Errno>, Errno> {
    // SAFETY: kcmp only compares two of the task's descriptors.
    let order = unsafe {
        libc::syscall(
            libc::SYS_kcmp,
            task.as_raw(),
            task.as_raw(),
            0, // KCMP_FILE
            libc::c_long::from(syscall.in_fd()),
            libc::c_long::from(syscall.out_fd()),
        )
    };
    let metadata =
        |fd: libc::c_int| std::fs::metadata(format!("/proc/{}/fd/{fd}", task.as_raw())).ok();
    let (input_file, output_file) = (metadata(syscall.in_fd()), metadata(syscall.out_fd()));
    let same_file = input_file
        .as_ref()
        .zip(output_file.as_ref())
        .map(|(input, output)| (input.dev(), input.ino()) == (output.dev(), output.ino()));
    let descriptions = Descriptions::of(order, same_file);
    let input = crate::fd::duplicate_guest_thread_fd(pid, task, syscall.in_fd()).ok();
    if descriptions == Descriptions::Distinct
        && let Some(input) = &input
    {
        // SAFETY: `input` owns its descriptor; these only move the shared
        // position, which is put back below.
        let saved = unsafe { libc::lseek(input.as_raw_fd(), 0, libc::SEEK_CUR) };
        if saved >= 0 && unsafe { libc::lseek(input.as_raw_fd(), position, libc::SEEK_SET) } >= 0 {
            let result = guest
                .inject(Syscall::Sendfile(syscall.with_offset(None)))
                .await;
            // SAFETY: as above.
            unsafe { libc::lseek(input.as_raw_fd(), saved, libc::SEEK_SET) };
            return Ok(result);
        }
    }
    // The recorder's own call needs both descriptors and a regular-file
    // output: the call runs in the recorder's process, where SIGPIPE cannot
    // arise for a regular file and the file-size limit is the recorder's.
    let pair = input
        .filter(|_| {
            output_file
                .as_ref()
                .is_some_and(|output| output.file_type().is_file())
        })
        .zip(crate::fd::duplicate_guest_thread_fd(pid, task, syscall.out_fd()).ok());
    if let Some((input, output)) = &pair
        && recorder_copy_fits(pid, output, syscall.count()).unwrap_or(false)
    {
        return Ok(recorder_sendfile(output, input, position, syscall.count()).0);
    }
    let mut stack = guest.stack().await;
    let scratch = stack.reserve::<libc::loff_t>();
    let placed = stack.commit().and_then(|guard| {
        guest
            .memory()
            .write_value(scratch, &position)
            .map(|()| guard)
    });
    match (placed, pair) {
        (Ok(_guard), _) => Ok(guest
            .inject(Syscall::Sendfile(syscall.with_offset(Some(scratch))))
            .await),
        (Err(_), Some((input, output))) => {
            copy_under_the_guests_limit(guest, pid, syscall, &input, &output, position).await
        }
        // Linux would copy and then return EFAULT; this copy cannot be made,
        // so the call returns EFAULT without it (a disclosed difference).
        (Err(_), None) => Err(Errno::EFAULT),
    }
}

/// For a regular-file output whose copy could reach a file-size limit, on a
/// stack with no room for a scratch offset: what Linux does, without guest
/// memory, when the recorder's file-size limit is the guest's. The recorder
/// makes the whole call on its duplicates (`recorder_sendfile`): the kernel
/// validates it as it would the guest's, and copies up to the shared limit.
/// If its next write met the limit, the kernel raised SIGXFSZ in the recorder
/// instead of the guest; the recorder then has the guest write one byte at
/// the limit (pwrite64, whose buffer is never read), so the kernel raises the
/// guest's own SIGXFSZ, with the native signal information, and refuses the
/// write (EFBIG). Different limits cannot be copied this way: EFAULT without
/// the copy.
async fn copy_under_the_guests_limit<G: Guest<Recorder>>(
    guest: &mut G,
    pid: reverie::Pid,
    syscall: Sendfile,
    input: &std::os::fd::OwnedFd,
    output: &std::os::fd::OwnedFd,
    position: libc::loff_t,
) -> Result<Result<i64, Errno>, Errno> {
    let (guest_limit, recorder_limit) = file_size_limits(pid).ok_or(Errno::EFAULT)?;
    if guest_limit != recorder_limit {
        return Err(Errno::EFAULT);
    }
    let (result, raised) = recorder_sendfile(output, input, position, syscall.count());
    if raised && let Some(limit) = guest_limit.and_then(|limit| libc::off_t::try_from(limit).ok()) {
        let buffer = syscall
            .offset()
            .and_then(|offset| reverie::syscalls::Addr::from_raw(offset.as_raw()));
        // The write's own result (EFBIG) is not the guest's; its signal is.
        let _ = guest
            .inject(Syscall::Pwrite64(
                reverie::syscalls::Pwrite64::new()
                    .with_fd(syscall.out_fd())
                    .with_buf(buffer)
                    .with_len(1)
                    .with_offset(limit),
            ))
            .await;
    }
    Ok(result)
}

/// Where in the input `handle_sendfile` reads back the bytes a copy wrote.
enum SendfileStart {
    /// At the offset the write side records for the output, which is the
    /// same file as the input, so the copy may have overwritten its input.
    Output,
    /// At this offset, if the guest's offset argument was readable (when it
    /// was not, the kernel fails the copy with EFAULT).
    Input(Option<i64>),
}

/// The input descriptor `handle_sendfile` reads the copied bytes back from,
/// and where, decided before the copy. Linux requires the input to be
/// readable, while the output may be write-only (`cp`, Python's
/// `shutil.copyfile`), so the bytes are always read through the input. When
/// the output is the same file, the copy may have overwritten its own input
/// (overlapping ranges), so they are read where they landed, at the output's
/// write offset; any other output cannot have changed the input, so they are
/// read where the copy began.
///
/// `None` when either descriptor is not open in task `task`'s table, so the
/// kernel fails the copy with EBADF. An input without a position, which
/// cannot be read back, is refused with EINVAL; Linux 7.1 refuses pipe, socket
/// and procfs inputs itself, and Detcore refuses any input it does not type as
/// a regular file or memfd (ENOSYS), so this is for a kernel that accepts one.
/// Callers handle either error by falling back to read and write, which record
/// their bytes. `offset` is the guest's offset argument, if it passed one,
/// with the value it held if readable.
fn sendfile_source(
    pid: reverie::Pid,
    task: reverie::Pid,
    syscall: Sendfile,
    offset: Option<Option<i64>>,
) -> Result<Option<(std::fs::File, SendfileStart)>, Errno> {
    let metadata = |fd: libc::c_int| std::fs::metadata(format!("/proc/{}/fd/{fd}", task.as_raw()));
    let (Ok(input), Ok(output)) = (metadata(syscall.in_fd()), metadata(syscall.out_fd())) else {
        return Ok(None);
    };
    let same_file = output.file_type().is_file()
        && input.file_type().is_file()
        && (input.dev(), input.ino()) == (output.dev(), output.ino());
    let file = crate::fd::duplicate_guest_thread_fd(pid, task, syscall.in_fd())
        .map(std::fs::File::from)
        .map_err(|error| match error.raw_os_error() {
            Some(libc::EBADF) => Errno::EBADF,
            _ => Errno::EINVAL,
        })?;
    let start = match (same_file, offset) {
        (true, _) => SendfileStart::Output,
        (false, Some(offset)) => SendfileStart::Input(offset),
        (false, None) => {
            // SAFETY: `file` owns its descriptor; SEEK_CUR by 0 only reads
            // the shared position.
            let position = unsafe { libc::lseek(file.as_raw_fd(), 0, libc::SEEK_CUR) };
            if position < 0 {
                return Err(Errno::EINVAL);
            }
            SendfileStart::Input(Some(position))
        }
    };
    Ok(Some((file, start)))
}

const MAX_CLONE_SNAPSHOT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CLONE_SIDECAR_BYTES: u64 = 1024 * 1024 * 1024;
const CLONE_COPY_CHUNK_BYTES: usize = 1024 * 1024;

fn read_dense_clone_snapshot(
    file: &std::fs::File,
    source_offset: u64,
    length: u64,
) -> std::io::Result<FileCloneImage> {
    if length > MAX_CLONE_SNAPSHOT_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::FileTooLarge,
            format!("clone snapshot is {length} bytes; limit is {MAX_CLONE_SNAPSHOT_BYTES}"),
        ));
    }
    let mut bytes = vec![0; length as usize];
    file.read_exact_at(&mut bytes, source_offset)?;
    Ok(FileCloneImage::Extents(
        (!bytes.is_empty())
            .then_some(FileExtent { offset: 0, bytes })
            .into_iter()
            .collect(),
    ))
}

fn clone_snapshot(
    file: &std::fs::File,
    source_offset: u64,
    length: u64,
) -> std::io::Result<FileCloneImage> {
    let mut extents = Vec::new();
    let mut total = 0u64;
    let mut cursor = source_offset;
    let end = source_offset.checked_add(length).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "clone source range overflow",
        )
    })?;

    while cursor < end {
        // SAFETY: file is an owned descriptor and cursor fits off_t on x86_64.
        let data = unsafe {
            libc::lseek(
                file.as_raw_fd(),
                cursor.try_into().unwrap(),
                libc::SEEK_DATA,
            )
        };
        if data == -1 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENXIO) {
                break;
            }
            if error.raw_os_error() == Some(libc::EINVAL) {
                return read_dense_clone_snapshot(file, source_offset, length);
            }
            return Err(error);
        }
        // SAFETY: file is an owned descriptor and data was returned by lseek.
        let hole = unsafe { libc::lseek(file.as_raw_fd(), data, libc::SEEK_HOLE) };
        if hole == -1 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ENXIO) {
                return Err(error);
            }
        }
        let data = data as u64;
        let hole = if hole == -1 {
            end
        } else {
            (hole as u64).min(end)
        };
        if data >= end {
            break;
        }
        let extent_length = hole.saturating_sub(data);
        total = total.checked_add(extent_length).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::FileTooLarge,
                "clone snapshot size overflow",
            )
        })?;
        if total > MAX_CLONE_SNAPSHOT_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::FileTooLarge,
                format!(
                    "clone data extents total {total} bytes; limit is {MAX_CLONE_SNAPSHOT_BYTES}"
                ),
            ));
        }
        let mut bytes = vec![0; extent_length as usize];
        file.read_exact_at(&mut bytes, data)?;
        extents.push(FileExtent {
            offset: data - source_offset,
            bytes,
        });
        cursor = hole;
    }

    Ok(FileCloneImage::Extents(extents))
}

fn spool_clone_snapshot(
    source: &std::fs::File,
    source_offset: u64,
    length: u64,
    data: &std::path::Path,
) -> std::io::Result<FileCloneImage> {
    let directory = data.join("clone");
    std::fs::create_dir_all(&directory)?;

    let sidecar = tempfile::Builder::new()
        .prefix("image-")
        .tempfile_in(&directory)?;
    sidecar.as_file().set_len(length)?;

    let mut total = 0u64;
    let mut cursor = source_offset;
    let end = source_offset.checked_add(length).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "clone source range overflow",
        )
    })?;
    let mut buffer = vec![0; CLONE_COPY_CHUNK_BYTES];
    while cursor < end {
        // SAFETY: source is owned and cursor fits off_t on x86_64.
        let data_offset = unsafe {
            libc::lseek(
                source.as_raw_fd(),
                cursor.try_into().unwrap(),
                libc::SEEK_DATA,
            )
        };
        let (data_offset, hole) = if data_offset == -1 {
            let error = std::io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::ENXIO) => break,
                Some(libc::EINVAL) => (source_offset, end),
                _ => return Err(error),
            }
        } else {
            // SAFETY: source is owned and data_offset came from lseek.
            let hole = unsafe { libc::lseek(source.as_raw_fd(), data_offset, libc::SEEK_HOLE) };
            if hole == -1 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ENXIO) {
                    return Err(error);
                }
            }
            (
                data_offset as u64,
                if hole == -1 {
                    end
                } else {
                    (hole as u64).min(end)
                },
            )
        };
        let extent_length = hole.saturating_sub(data_offset);
        if data_offset >= end {
            break;
        }
        total = total.checked_add(extent_length).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::FileTooLarge,
                "clone sidecar size overflow",
            )
        })?;
        if total > MAX_CLONE_SIDECAR_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::FileTooLarge,
                format!(
                    "clone sidecar data exceeds {MAX_CLONE_SIDECAR_BYTES} byte recording quota"
                ),
            ));
        }

        let mut offset = data_offset;
        while offset < hole {
            let count = usize::try_from((hole - offset).min(buffer.len() as u64)).unwrap();
            source.read_exact_at(&mut buffer[..count], offset)?;
            sidecar
                .as_file()
                .write_all_at(&buffer[..count], offset - source_offset)?;
            offset += count as u64;
        }
        cursor = hole;
    }
    sidecar.as_file().sync_data()?;
    let (_, path) = sidecar.keep().map_err(|error| error.error)?;
    let relative = path.strip_prefix(data).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("clone sidecar escaped recording directory: {error}"),
        )
    })?;

    Ok(FileCloneImage::Sidecar(
        relative.to_string_lossy().into_owned(),
    ))
}

fn capture_clone_snapshot(
    source: &std::fs::File,
    source_offset: u64,
    length: u64,
    data: &std::path::Path,
) -> std::io::Result<FileCloneImage> {
    let inline_error = match clone_snapshot(source, source_offset, length) {
        Ok(snapshot) => return Ok(snapshot),
        Err(error) => error,
    };

    tracing::warn!(
        error = %inline_error,
        "inline clone snapshot unavailable; spooling sparse replay sidecar"
    );
    spool_clone_snapshot(source, source_offset, length, data)
}

fn private_clone_snapshot(
    pid: reverie::Pid,
    destination_fd: libc::c_int,
    source: &std::fs::File,
    source_offset: u64,
    length: u64,
) -> std::io::Result<std::fs::File> {
    let destination = std::fs::read_link(format!("/proc/{}/fd/{destination_fd}", pid.as_raw()))?;
    let relative = destination.strip_prefix("/").map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("clone destination is not an absolute path: {destination:?}"),
        )
    })?;
    let parent = relative.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("clone destination has no parent: {destination:?}"),
        )
    })?;
    let parent = std::path::PathBuf::from(format!("/proc/{}/root", pid.as_raw())).join(parent);
    let private = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_TMPFILE | libc::O_CLOEXEC)
        .mode(0o600)
        .open(&parent)?;

    if length == 0 {
        return Ok(private);
    }
    let range = libc::file_clone_range {
        src_fd: source.as_raw_fd() as i64,
        src_offset: source_offset,
        src_length: length,
        dest_offset: 0,
    };
    // SAFETY: private and source are valid regular-file descriptors, and range
    // points to an initialized file_clone_range for the duration of ioctl.
    let result = unsafe {
        libc::ioctl(
            private.as_raw_fd(),
            libc::FICLONERANGE,
            std::ptr::addr_of!(range),
        )
    };
    if result == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(private)
}

impl Recorder {
    /// Records the vectored read family (`readv`/`preadv`/`preadv2`). Writes only
    /// need their return count (see `handle_write_family`), but vectored reads
    /// scatter output across guest `iovec` buffers, so we capture the exact
    /// returned bytes flattened in read order.
    pub(super) async fn handle_readv_family<G: Guest<Self>>(
        &self,
        guest: &mut G,
        iov_addr: Option<usize>,
        iovcnt: usize,
        fd: libc::c_int,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;

        self.record_event(
            guest,
            result.and_then(|length| {
                let bytes = read_iovec_output(&guest.memory(), iov_addr, iovcnt, length as usize)?;
                Ok(SyscallEvent::ReadvV2(ReadEvent {
                    consumed_sigpipe_count: consumed_sigpipe_count(
                        guest.pid().as_raw(),
                        fd,
                        &bytes,
                    ),
                    replay_fd_kind: self.fd_replay_kind(guest.pid(), fd),
                    bytes,
                }))
            }),
        );

        result
    }

    pub(super) async fn handle_read<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Read,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;

        self.record_event(
            guest,
            result.and_then(|length| {
                let mut buf = vec![0; length as usize];
                // A read that transfers nothing never touches the buffer,
                // which may then be NULL.
                if !buf.is_empty() {
                    let addr = syscall.buf().ok_or(Errno::EFAULT)?;
                    guest.memory().read_exact(addr, &mut buf)?;
                }
                Ok(SyscallEvent::ReadV2(ReadEvent {
                    consumed_sigpipe_count: consumed_sigpipe_count(
                        guest.pid().as_raw(),
                        syscall.fd(),
                        &buf,
                    ),
                    bytes: buf,
                    replay_fd_kind: self.fd_replay_kind(guest.pid(), syscall.fd()),
                }))
            }),
        );

        result
    }

    pub(super) async fn handle_pread64<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Pread64,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;

        self.record_event(
            guest,
            result.and_then(|length| {
                let mut buf = vec![0; length as usize];
                // A read that transfers nothing never touches the buffer,
                // which may then be NULL.
                if !buf.is_empty() {
                    let addr = syscall.buf().ok_or(Errno::EFAULT)?;
                    guest.memory().read_exact(addr, &mut buf)?;
                }
                Ok(SyscallEvent::Bytes(buf))
            }),
        );

        result
    }

    pub(super) async fn handle_write_family<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: WriteFamily,
    ) -> Result<i64, Errno> {
        let result = guest.inject(Syscall::from(syscall)).await;
        let pid = guest.pid();
        let event = self.write_event(
            &*guest,
            pid,
            pid,
            syscall.fd(),
            WritePlacement::of(syscall),
            result,
        );
        self.record_event(guest, Ok(SyscallEvent::WriteV2(event)));

        result
    }

    /// Runs `sendfile` and records what it copied, where its input stopped
    /// and its output side, so replay applies the copy rather than running it
    /// against its own descriptors, which for an input the guest did not create
    /// are not that file (https://github.com/rrnewton/hermit/issues/4416).
    ///
    /// The copied bytes are read back through the input after the copy (see
    /// `sendfile_source`). Whether that read is possible is decided before the
    /// copy runs; a copy that cannot be recorded is refused rather than run.
    pub(super) async fn handle_sendfile<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Sendfile,
    ) -> Result<i64, Errno> {
        let pid = guest.pid();
        let task = guest.tid();
        // The offset argument as Linux sees it: the recorder reads guest
        // memory with debugger rights, which also reach a page the guest
        // cannot read, so the mappings decide. `Some(None)` is an argument
        // Linux cannot read: it fails the call with EFAULT before copying.
        let permissions = syscall.offset().map(|address| {
            let start = address.as_raw();
            let end = start.saturating_add(std::mem::size_of::<libc::loff_t>() - 1);
            std::fs::read_to_string(format!("/proc/{}/maps", pid.as_raw()))
                .ok()
                .and_then(|maps| {
                    let (start, end) = (
                        mapping_permissions(&maps, start)?,
                        mapping_permissions(&maps, end)?,
                    );
                    Some((start.0 && end.0, start.1 && end.1))
                })
                .unwrap_or((false, false))
        });
        let offset = syscall.offset().map(|offset| {
            permissions
                .is_some_and(|(readable, _)| readable)
                .then(|| guest.memory().read_value(offset).ok())
                .flatten()
        });
        // Linux reads the offset argument, copies, then writes the final
        // offset back through the guest's pointer and returns EFAULT if it
        // cannot, whatever the copy returned. A readable but read-only
        // argument therefore gets a real copy and EFAULT. Writing the final
        // offset is the one step a recording cannot replay through that
        // pointer, so the copy is run another way (`copy_at_offset`), its
        // effects are recorded as usual, and the guest gets the EFAULT Linux
        // gives.
        let offset_fault =
            matches!(offset, Some(Some(_))) && permissions.is_some_and(|(_, writable)| !writable);
        let source = match sendfile_source(pid, task, syscall, offset) {
            Ok(source) => source,
            Err(error) => {
                self.record_event(guest, Err(error));
                return Err(error);
            }
        };
        let result = match (offset_fault, offset.flatten()) {
            (true, Some(position)) => {
                match copy_at_offset(guest, pid, task, syscall, position).await {
                    Ok(result) => result,
                    Err(error) => {
                        self.record_event(guest, Err(error));
                        return Err(error);
                    }
                }
            }
            _ => guest.inject(Syscall::Sendfile(syscall)).await,
        };
        let write = self.write_event(
            &*guest,
            pid,
            task,
            syscall.out_fd(),
            WritePlacement::Position,
            result,
        );
        let bytes = match (result, source) {
            (Ok(count), Some((file, start))) if count > 0 => {
                let start = match start {
                    SendfileStart::Output => write.replay_file_offset.or(write.output_offset),
                    SendfileStart::Input(start) => start,
                };
                let mut bytes = vec![0; count as usize];
                match start.and_then(|start| u64::try_from(start).ok()) {
                    Some(start) => file
                        .read_exact_at(&mut bytes, start)
                        .map(|()| bytes)
                        .map_err(|_| Errno::EIO),
                    None => Err(Errno::EIO),
                }
            }
            (Ok(count), None) if count > 0 => Err(Errno::EIO),
            _ => Ok(Vec::new()),
        };
        // A copy that cannot be read back cannot be replayed; the guest sees
        // the error replay will return. Only a file changed from outside the
        // guest between the copy and the read gets here.
        if let Err(error) = bytes {
            self.record_event(guest, Err(error));
            return Err(error);
        }
        let offset_after = match (result, syscall.offset()) {
            (Ok(_), Some(offset)) if !offset_fault => guest.memory().read_value(offset).ok(),
            _ => None,
        };
        self.record_event(
            guest,
            bytes.map(|bytes| {
                SyscallEvent::Sendfile(SendfileEvent {
                    bytes,
                    offset_after,
                    write,
                    offset_fault,
                })
            }),
        );

        if offset_fault {
            Err(Errno::EFAULT)
        } else {
            result
        }
    }

    /// The output side of a write of `result` bytes to `fd` in the table of
    /// task `task` of process `pid`, placed as `placement`: the captured
    /// output it reaches, the regular-file offset replay writes at, and the
    /// SIGPIPE the kernel generated.
    fn write_event<G: Guest<Self>>(
        &self,
        guest: &G,
        pid: reverie::Pid,
        task: reverie::Pid,
        fd: libc::c_int,
        placement: WritePlacement,
        result: Result<i64, Errno>,
    ) -> WriteEvent {
        let metadata = std::fs::metadata(format!("/proc/{}/fd/{fd}", task.as_raw())).ok();
        let outputs = guest.thread_state().outputs;
        let (output_fd, shares_output_ofd) = metadata.as_ref().map_or((None, false), |metadata| {
            let (stdout_matches, stderr_matches) = outputs.matching(metadata);
            let candidate = (metadata.file_type().is_file() && (stdout_matches || stderr_matches))
                .then(|| crate::fd::duplicate_guest_thread_fd(pid, task, fd).ok())
                .flatten();
            let stdout_shares = stdout_matches
                && candidate.as_ref().is_some_and(|candidate| {
                    Self::output_ofd_matches(guest, libc::STDOUT_FILENO, candidate)
                });
            let stderr_shares = stderr_matches
                && candidate.as_ref().is_some_and(|candidate| {
                    Self::output_ofd_matches(guest, libc::STDERR_FILENO, candidate)
                });

            if stdout_shares {
                (Some(libc::STDOUT_FILENO), true)
            } else if stderr_shares {
                (Some(libc::STDERR_FILENO), true)
            } else if stdout_matches {
                (Some(libc::STDOUT_FILENO), false)
            } else if stderr_matches {
                (Some(libc::STDERR_FILENO), false)
            } else {
                (None, false)
            }
        });
        let file_offset = match (result, metadata.as_ref()) {
            (Ok(count), Some(metadata)) => {
                output_file_offset(task.as_raw(), fd, placement, metadata, count)
            }
            _ => None,
        };
        let output_offset = output_fd.and(file_offset);
        let advances_output_offset =
            output_offset.is_some() && shares_output_ofd && write_advances_output_offset(placement);
        let generated_sigpipe = result == Err(Errno::EPIPE)
            && metadata.is_some_and(|metadata| {
                metadata.file_type().is_fifo() || metadata.file_type().is_socket()
            });
        let replay_fd_kind = if result.is_ok() && output_fd.is_none() {
            self.fd_replay_kind(task, fd)
        } else {
            ReplayFdKind::None
        };
        let replay_file_offset = (replay_fd_kind == ReplayFdKind::RegularFile)
            .then_some(file_offset)
            .flatten();
        let replay_file_advances_offset =
            replay_file_offset.is_some() && write_advances_output_offset(placement);
        WriteEvent {
            result,
            output_fd,
            output_offset,
            generated_sigpipe,
            advances_output_offset,
            replay_fd_kind,
            replay_file_offset,
            replay_file_advances_offset,
        }
    }

    // TODO-HUMAN-REVIEW(#557): Audit captured-output ftruncate recording.
    pub(super) async fn handle_ftruncate<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Ftruncate,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;
        let metadata = std::fs::metadata(format!(
            "/proc/{}/fd/{}",
            guest.pid().as_raw(),
            syscall.fd()
        ))
        .ok();
        let outputs = guest.thread_state().outputs;
        let output_fd = if result.is_ok() {
            metadata
                .as_ref()
                .and_then(|metadata| match outputs.matching(metadata) {
                    (true, _) => Some(libc::STDOUT_FILENO),
                    (false, true) => Some(libc::STDERR_FILENO),
                    (false, false) => None,
                })
        } else {
            None
        };
        self.record_event(
            guest,
            Ok(SyscallEvent::FtruncateV2(FtruncateEvent {
                result,
                output_fd,
                length: syscall.length(),
                replay_regular_file: result.is_ok()
                    && metadata.is_some_and(|metadata| metadata.file_type().is_file()),
            })),
        );
        result
    }
    pub(super) async fn handle_stat_family<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: StatFamily,
    ) -> Result<i64, Errno> {
        let result = guest.inject(Syscall::from(syscall)).await;

        self.record_event(
            guest,
            result.and_then(|ret| {
                debug_assert_eq!(ret, 0);
                let statbuf = syscall.stat().ok_or(Errno::EFAULT)?.read(&guest.memory())?;
                Ok(SyscallEvent::Stat(StatEvent { statbuf }))
            }),
        );

        result
    }

    pub(super) async fn handle_statfs<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
        buf: Option<AddrMut<'_, libc::statfs>>,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;

        self.record_event(
            guest,
            result.and_then(|ret| {
                debug_assert_eq!(ret, 0);
                let mut bytes = vec![0; std::mem::size_of::<libc::statfs>()];
                guest
                    .memory()
                    .read_exact(buf.ok_or(Errno::EFAULT)?.cast(), &mut bytes)?;
                Ok(SyscallEvent::Statfs(bytes))
            }),
        );

        result
    }

    pub(super) async fn handle_statx<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Statx,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;

        self.record_event(
            guest,
            result.and_then(|ret| {
                debug_assert_eq!(ret, 0);
                let statbuf = syscall
                    .statx()
                    .ok_or(Errno::EFAULT)?
                    .read(&guest.memory())?;
                Ok(SyscallEvent::Statx(statbuf.into()))
            }),
        );

        result
    }

    /// ioctl is a beast of a syscall. We try to handle the common cases here.
    pub(super) async fn handle_ioctl<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Ioctl,
    ) -> Result<i64, Errno> {
        let request = syscall.request();

        if let Some(error) = deterministic_ioctl_error(&request) {
            self.record_event(guest, Err(error));
            return Err(error);
        }

        let ret = guest.inject(syscall).await.inspect_err(|&err| {
            self.record_event(guest, Err(err));
        })?;

        if matches!(
            request,
            ioctl::Request::FICLONE(_) | ioctl::Request::FICLONERANGE(_)
        ) {
            let (source_fd, source_offset, requested_length, destination_offset, truncate) =
                match request {
                    ioctl::Request::FICLONE(fd) => (fd, 0, None, 0, true),
                    ioctl::Request::FICLONERANGE(Some(address)) => {
                        let range: libc::file_clone_range = guest
                            .memory()
                            .read_value(address)
                            .expect("successful FICLONERANGE has a readable argument");
                        (
                            range.src_fd as libc::c_int,
                            range.src_offset,
                            (range.src_length != 0).then_some(range.src_length),
                            range.dest_offset,
                            false,
                        )
                    }
                    ioctl::Request::FICLONERANGE(None) => {
                        unreachable!("successful FICLONERANGE has a non-null argument")
                    }
                    _ => unreachable!(),
                };
            let source = std::fs::File::from(
                crate::fd::duplicate_guest_fd(guest.pid(), source_fd)
                    .expect("failed to duplicate clone source descriptor"),
            );
            let source_length = source
                .metadata()
                .expect("failed to stat clone source descriptor")
                .len();
            let replacement_length = requested_length.unwrap_or_else(|| {
                source_length
                    .checked_sub(source_offset)
                    .expect("successful clone source offset exceeds file length")
            });
            let private = private_clone_snapshot(
                guest.pid(),
                syscall.fd(),
                &source,
                source_offset,
                replacement_length,
            )
            .unwrap_or_else(|error| panic!("failed to make private clone snapshot: {error}"));
            let image = capture_clone_snapshot(&private, 0, replacement_length, &self.data)
                .unwrap_or_else(|error| panic!("failed to snapshot private clone: {error}"));
            let destination_path = format!("/proc/{}/fd/{}", guest.pid().as_raw(), syscall.fd());
            let length = std::fs::metadata(&destination_path)
                .unwrap_or_else(|error| {
                    panic!("failed to stat cloned destination {destination_path}: {error}")
                })
                .len();
            self.record_event(
                guest,
                Ok(SyscallEvent::FileClone(FileCloneEvent {
                    length,
                    destination_offset,
                    replacement_length,
                    truncate_destination: truncate,
                    image,
                })),
            );
        } else if matches!(
            request,
            ioctl::Request::FIOCLEX | ioctl::Request::FIONCLEX | ioctl::Request::FIONBIO(_)
        ) {
            self.record_event(guest, Ok(SyscallEvent::Return(ret)));
        } else if let Some(output) = request.read_output(&guest.memory()).transpose() {
            // This ioctl request has an associated output.
            self.record_event(guest, output.map(SyscallEvent::Ioctl));
        } else {
            self.record_event(guest, Ok(SyscallEvent::Return(ret)));
        }

        Ok(ret)
    }

    #[cfg(target_arch = "x86_64")]
    pub(super) async fn handle_readlink<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Readlink,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;

        self.record_event(
            guest,
            result.and_then(|length| {
                let mut buf = vec![0; length as usize];
                let addr = syscall.buf().ok_or(Errno::EFAULT)?.cast::<u8>();
                guest.memory().read_exact(addr, &mut buf)?;
                Ok(SyscallEvent::Bytes(buf))
            }),
        );

        result
    }

    // TODO-HUMAN-REVIEW(PR-3601)
    /// Records an xattr query. A sized query records the bytes the kernel
    /// copied out, whose count is the return value. A zero-size query copies
    /// nothing and returns the length the value needs, so that is recorded
    /// instead.
    pub(super) async fn handle_xattr_query<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let (buf, size) = xattr_query_output(&syscall)
            .unwrap_or_else(|| unreachable!("xattr query recording called for {syscall:?}"));
        let result = guest.inject(syscall).await;

        if size == 0 {
            self.record_event(guest, result.map(SyscallEvent::Return));
        } else {
            self.record_event(
                guest,
                result.and_then(|length| {
                    let mut bytes = vec![0; length as usize];
                    // An empty list succeeds without touching the buffer,
                    // which may then be NULL.
                    if !bytes.is_empty() {
                        guest
                            .memory()
                            .read_exact(buf.ok_or(Errno::EFAULT)?, &mut bytes)?;
                    }
                    Ok(SyscallEvent::Bytes(bytes))
                }),
            );
        }

        result
    }

    // TODO-HUMAN-REVIEW(#3598)
    /// Records the path `getcwd` copied out. The raw syscall returns the
    /// length including the terminating NUL, so the recorded bytes include it.
    pub(super) async fn handle_getcwd<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Getcwd,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;

        self.record_event(
            guest,
            result.and_then(|length| {
                let mut buf = vec![0; length as usize];
                let addr = syscall.buf().ok_or(Errno::EFAULT)?.cast::<u8>();
                guest.memory().read_exact(addr, &mut buf)?;
                Ok(SyscallEvent::Bytes(buf))
            }),
        );

        result
    }

    // TODO-HUMAN-REVIEW(#3598)
    /// Record a successful `chdir(2)` or `fchdir(2)` as the directory it
    /// actually entered. The spelled path may go through host symlinks or `..`
    /// components that the replay root lacks, and an fchdir descriptor may be
    /// a replay placeholder, so replay moves to this resolved name instead.
    pub(super) async fn handle_working_directory_change<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;

        self.record_event(
            guest,
            result.map(|_| {
                let cwd = crate::record_replay_path::process_cwd_path(guest.tid()).unwrap_or_else(
                    |error| panic!("could not record the directory {syscall:?} entered: {error}"),
                );
                SyscallEvent::Bytes(cwd.into_os_string().into_vec())
            }),
        );

        result
    }

    #[cfg(target_arch = "x86_64")]
    pub(super) async fn handle_getdents<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Getdents,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;

        self.record_event(
            guest,
            result.and_then(|length| {
                let mut buf = vec![0; length as usize];
                let addr = syscall.dirent().ok_or(Errno::EFAULT)?.cast::<u8>();
                guest.memory().read_exact(addr, &mut buf)?;
                Ok(SyscallEvent::Bytes(buf))
            }),
        );

        result
    }

    pub(super) async fn handle_getdents64<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Getdents64,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;

        self.record_event(
            guest,
            result.and_then(|length| {
                let mut buf = vec![0; length as usize];
                let addr = syscall.dirent().ok_or(Errno::EFAULT)?.cast::<u8>();
                guest.memory().read_exact(addr, &mut buf)?;
                Ok(SyscallEvent::Bytes(buf))
            }),
        );

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether a user access can read and write sendfile's offset argument
    /// decides how it is recorded
    /// (https://github.com/rrnewton/hermit/pull/4422 reviews).
    #[test]
    fn mapping_permissions_follow_the_read_and_write_bits() {
        let maps = "\
            1000-2000 r--p 00000000 00:00 0\n\
            2000-3000 rw-p 00000000 00:00 0\n\
            3000-4000 -w-p 00000000 00:00 0\n\
            4000-5000 ---p 00000000 00:00 0\n\
            5000-6000 --xp 00000000 00:00 0\n";
        for (address, permissions) in [
            (0x0fff, None),
            (0x1000, Some((true, false))),
            (0x1fff, Some((true, false))),
            (0x2000, Some((true, true))),
            (0x2fff, Some((true, true))),
            (0x3000, Some((true, true))),
            (0x4000, Some((false, false))),
            (0x5000, Some((false, false))),
            (0x6000, None),
        ] {
            assert_eq!(
                mapping_permissions(maps, address),
                permissions,
                "{address:#x}"
            );
        }
    }

    /// Only kcmp's 0 proves one open file description, and only 1 or 2, or
    /// two different files, prove two; a failure (-1) on one file proves
    /// neither, so neither route that assumes one is taken
    /// (https://github.com/rrnewton/hermit/pull/4422 reviews).
    #[test]
    fn only_kcmp_answers_decide_whether_descriptions_are_shared() {
        for same_file in [None, Some(true), Some(false)] {
            assert_eq!(Descriptions::of(0, same_file), Descriptions::Shared);
            assert_eq!(Descriptions::of(1, same_file), Descriptions::Distinct);
            assert_eq!(Descriptions::of(2, same_file), Descriptions::Distinct);
        }
        assert_eq!(Descriptions::of(-1, Some(false)), Descriptions::Distinct);
        assert_eq!(Descriptions::of(-1, Some(true)), Descriptions::Unknown);
        assert_eq!(Descriptions::of(-1, None), Descriptions::Unknown);
        assert_eq!(Descriptions::of(3, Some(true)), Descriptions::Unknown);
    }

    #[test]
    fn vectored_offsets_combine_both_abi_halves() {
        if std::mem::size_of::<usize>() == 8 {
            assert_eq!(vectored_offset(0x1_0000_0002, 0), 0x1_0000_0002);
            assert_eq!(vectored_offset(u64::MAX, 0), -1);
        } else {
            assert_eq!(vectored_offset(2, 1), 0x1_0000_0002);
            assert_eq!(vectored_offset(u32::MAX as u64, u32::MAX as u64), -1);
        }
    }

    #[test]
    fn rwf_noappend_overrides_descriptor_append_mode() {
        assert!(uses_append_offset(libc::O_APPEND, 0));
        assert!(uses_append_offset(0, libc::RWF_APPEND));
        assert!(!uses_append_offset(libc::O_APPEND, libc::RWF_NOAPPEND));
    }

    #[test]
    fn kcmp_distinguishes_dup_from_reopened_file() {
        use std::os::fd::AsRawFd as _;
        use std::os::fd::FromRawFd as _;

        let file = tempfile::NamedTempFile::new().unwrap();
        let original = file.as_file().as_raw_fd();
        // SAFETY: original is open and F_DUPFD_CLOEXEC returns a new descriptor.
        let duplicate =
            unsafe { libc::fcntl(original, libc::F_DUPFD_CLOEXEC, libc::STDERR_FILENO + 1) };
        assert_ne!(duplicate, -1);
        // SAFETY: fcntl returned a descriptor owned by this test.
        let duplicate = unsafe { std::os::fd::OwnedFd::from_raw_fd(duplicate) };
        let reopened = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(file.path())
            .unwrap();
        assert!(shares_open_file_description(
            original,
            duplicate.as_raw_fd()
        ));
        assert!(!shares_open_file_description(
            original,
            reopened.as_raw_fd()
        ));
    }
}
