/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use reverie::syscalls::family::StatFamily;
use reverie::syscalls::family::WriteFamily;
use reverie::syscalls::ioctl;
use reverie::syscalls::Getdents;
use reverie::syscalls::Getdents64;
use reverie::syscalls::Ioctl;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Pread64;
use reverie::syscalls::Read;
use reverie::syscalls::Readlink;
use reverie::syscalls::Statx;
use reverie::syscalls::Syscall;
use reverie::Errno;
use reverie::Guest;

use super::Replayer;

impl Replayer {
    // FIXME: Generalize the read-family of syscalls with `ReadFamily`.
    pub(super) async fn handle_read<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Read,
    ) -> Result<i64, Errno> {
        let buf = next_event!(guest, Bytes)?;

        assert!(buf.len() <= syscall.len());

        // Write out the buffer.
        guest
            .memory()
            .write_exact(syscall.buf().unwrap(), &buf)
            .unwrap();
        Ok(buf.len() as i64)
    }

    pub(super) async fn handle_pread64<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Pread64,
    ) -> Result<i64, Errno> {
        let buf = next_event!(guest, Bytes)?;

        assert!(buf.len() <= syscall.len());

        // Write out the buffer.
        guest
            .memory()
            .write_exact(syscall.buf().unwrap(), &buf)
            .unwrap();
        Ok(buf.len() as i64)
    }

    pub(super) async fn handle_write_family<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: WriteFamily,
    ) -> Result<i64, Errno> {
        let count = next_event!(guest, Write)?;

        let fd = syscall.fd();

        if fd == libc::STDOUT_FILENO || fd == libc::STDERR_FILENO {
            // Always let these through since they affect what we get to see.
            //
            // TODO: It would be better to do correct file descriptor tracking
            // to avoid edge cases where a program may close the stderr/stdout
            // file descriptors and immediately open a file. In that case,
            // output would go to a file instead (which should *not* be let
            // through).
            guest.inject_with_retry(Syscall::from(syscall)).await
        } else {
            Ok(count)
        }
    }

    pub(super) async fn handle_stat_family<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: StatFamily,
    ) -> Result<i64, Errno> {
        next_event!(guest, Stat).and_then(|event| {
            let addr = syscall.stat().ok_or(Errno::EFAULT)?;
            guest.memory().write_value(addr.0, &event.statbuf)?;
            // stat calls always return 0 on success.
            Ok(0)
        })
    }

    pub(super) async fn handle_statx<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Statx,
    ) -> Result<i64, Errno> {
        next_event!(guest, Statx).and_then(|buf| {
            let addr = syscall.statx().ok_or(Errno::EFAULT)?;
            guest.memory().write_value(addr.0, &buf.into())?;
            // statx calls always return 0 on success.
            Ok(0)
        })
    }

    pub(super) async fn handle_ioctl<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Ioctl,
    ) -> Result<i64, Errno> {
        let request = syscall.request();

        if request.direction() == ioctl::Direction::Read {
            let output = next_event!(guest, Ioctl)?;
            request.write_output(&mut guest.memory(), &output)?;
            Ok(0)
        } else {
            let ret = next_event!(guest, Return)?;
            Ok(ret)
        }
    }

    pub(super) async fn handle_readlink<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Readlink,
    ) -> Result<i64, Errno> {
        let buf = next_event!(guest, Bytes)?;

        debug_assert!(buf.len() <= syscall.bufsize());

        // Write out the buffer.
        guest
            .memory()
            .write_exact(syscall.buf().unwrap().cast::<u8>(), &buf)?;
        Ok(buf.len() as i64)
    }

    pub(super) async fn handle_getdents<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Getdents,
    ) -> Result<i64, Errno> {
        let buf = next_event!(guest, Bytes)?;

        // Make sure we don't overflow the buffer.
        debug_assert!(buf.len() <= syscall.count() as usize);

        // Write out the buffer.
        guest
            .memory()
            .write_exact(syscall.dirent().unwrap().cast::<u8>(), &buf)?;
        Ok(buf.len() as i64)
    }

    pub(super) async fn handle_getdents64<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Getdents64,
    ) -> Result<i64, Errno> {
        let buf = next_event!(guest, Bytes)?;

        // Make sure we don't overflow the buffer.
        debug_assert!(buf.len() <= syscall.count() as usize);

        // Write out the buffer.
        guest
            .memory()
            .write_exact(syscall.dirent().unwrap().cast::<u8>(), &buf)?;
        Ok(buf.len() as i64)
    }
}
