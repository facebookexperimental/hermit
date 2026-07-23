/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use reverie::Errno;
use reverie::Guest;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
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
use crate::event::StatEvent;
use crate::event::SyscallEvent;

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
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;

        self.record_event(
            guest,
            result.and_then(|length| {
                let buf = read_iovec_output(&guest.memory(), iov_addr, iovcnt, length as usize)?;
                Ok(SyscallEvent::Readv(buf))
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
                Ok(SyscallEvent::Bytes(buf))
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

        self.record_event(guest, result.map(SyscallEvent::Write));

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
