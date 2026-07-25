/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::os::unix::fs::FileTypeExt;

use reverie::Errno;
use reverie::Guest;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Ftruncate;
use reverie::syscalls::Getdents;
use reverie::syscalls::Getdents64;
use reverie::syscalls::Ioctl;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Pread64;
use reverie::syscalls::Read;
use reverie::syscalls::ReadAddr;
use reverie::syscalls::Readlink;
use reverie::syscalls::Statx;
use reverie::syscalls::Syscall;
use reverie::syscalls::family::StatFamily;
use reverie::syscalls::family::WriteFamily;
use reverie::syscalls::ioctl;

use super::Recorder;
use crate::event::FtruncateEvent;
use crate::event::ReadEvent;
use crate::event::StatEvent;
use crate::event::SyscallEvent;
use crate::event::WriteEvent;
use crate::event::deterministic_ioctl_error;

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

fn vectored_offset(low: u64, high: u64) -> i64 {
    if std::mem::size_of::<usize>() == 8 {
        low as i64
    } else {
        ((high << 32) | (low & u32::MAX as u64)) as i64
    }
}

fn write_advances_output_offset(syscall: WriteFamily) -> bool {
    match syscall {
        WriteFamily::Write(_) | WriteFamily::Writev(_) => true,
        WriteFamily::Pwritev2(call) => vectored_offset(call.pos_l(), call.pos_h()) == -1,
        WriteFamily::Pwrite64(_) | WriteFamily::Pwritev(_) => false,
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
    syscall: WriteFamily,
    metadata: &std::fs::Metadata,
    count: i64,
) -> Option<i64> {
    if !metadata.file_type().is_file() {
        return None;
    }

    let fdinfo = std::fs::read_to_string(format!("/proc/{pid}/fdinfo/{}", syscall.fd())).ok()?;
    let position = fdinfo
        .lines()
        .find_map(|line| line.strip_prefix("pos:\t"))
        .and_then(|value| value.parse::<i64>().ok())?;
    let status_flags = fdinfo
        .lines()
        .find_map(|line| line.strip_prefix("flags:\t"))
        .and_then(|value| i32::from_str_radix(value, 8).ok())
        .unwrap_or_default();
    let write_flags = match syscall {
        WriteFamily::Pwritev2(call) => call.flags(),
        _ => 0,
    };
    if uses_append_offset(status_flags, write_flags) {
        return i64::try_from(metadata.len()).ok()?.checked_sub(count);
    }

    match syscall {
        WriteFamily::Pwrite64(call) => Some(call.offset()),
        WriteFamily::Pwritev(call) => Some(vectored_offset(call.pos_l(), call.pos_h())),
        WriteFamily::Pwritev2(call) => {
            let offset = vectored_offset(call.pos_l(), call.pos_h());
            (offset != -1)
                .then_some(offset)
                .or_else(|| position.checked_sub(count))
        }
        WriteFamily::Write(_) | WriteFamily::Writev(_) => position.checked_sub(count),
    }
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
                let addr = syscall.buf().ok_or(Errno::EFAULT)?;
                guest.memory().read_exact(addr, &mut buf)?;
                Ok(SyscallEvent::ReadV2(ReadEvent {
                    consumed_sigpipe_count: consumed_sigpipe_count(
                        guest.pid().as_raw(),
                        syscall.fd(),
                        &buf,
                    ),
                    bytes: buf,
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
                let addr = syscall.buf().ok_or(Errno::EFAULT)?;
                guest.memory().read_exact(addr, &mut buf)?;
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

        let metadata = std::fs::metadata(format!(
            "/proc/{}/fd/{}",
            guest.pid().as_raw(),
            syscall.fd()
        ))
        .ok();
        let (output_fd, shares_output_ofd) = metadata.as_ref().map_or((None, false), |metadata| {
            let stdout_matches = self
                .stdout
                .is_some_and(|identity| identity.matches(metadata));
            let stderr_matches = self
                .stderr
                .is_some_and(|identity| identity.matches(metadata));
            let candidate = (metadata.file_type().is_file() && (stdout_matches || stderr_matches))
                .then(|| crate::fd::duplicate_guest_fd(guest.pid(), syscall.fd()).ok())
                .flatten();
            let stdout_shares = stdout_matches
                && candidate.as_ref().is_some_and(|candidate| {
                    self.output_ofd_matches(libc::STDOUT_FILENO, candidate)
                });
            let stderr_shares = stderr_matches
                && candidate.as_ref().is_some_and(|candidate| {
                    self.output_ofd_matches(libc::STDERR_FILENO, candidate)
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
        let output_offset = match (output_fd, result, metadata.as_ref()) {
            (Some(_), Ok(count), Some(metadata)) => {
                output_file_offset(guest.pid().as_raw(), syscall, metadata, count)
            }
            _ => None,
        };
        let advances_output_offset =
            output_offset.is_some() && shares_output_ofd && write_advances_output_offset(syscall);
        let generated_sigpipe = result == Err(Errno::EPIPE)
            && metadata.is_some_and(|metadata| {
                metadata.file_type().is_fifo() || metadata.file_type().is_socket()
            });
        self.record_event(
            guest,
            Ok(SyscallEvent::WriteV2(WriteEvent {
                result,
                output_fd,
                output_offset,
                generated_sigpipe,
                advances_output_offset,
            })),
        );

        result
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
        let output_fd = if result.is_ok() {
            metadata.as_ref().and_then(|metadata| {
                if self
                    .stdout
                    .is_some_and(|identity| identity.matches(metadata))
                {
                    Some(libc::STDOUT_FILENO)
                } else if self
                    .stderr
                    .is_some_and(|identity| identity.matches(metadata))
                {
                    Some(libc::STDERR_FILENO)
                } else {
                    None
                }
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
