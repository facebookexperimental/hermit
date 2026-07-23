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
use reverie::syscalls::EpollWait;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Poll;
use reverie::syscalls::PollFd;
use reverie::syscalls::Ppoll;
use reverie::syscalls::Recvfrom;
use reverie::syscalls::Recvmsg;
use reverie::syscalls::family::SockOptFamily;

use super::Replayer;

fn write_bytes<M: MemoryAccess>(
    memory: &mut M,
    pointer: *mut libc::c_void,
    bytes: &[u8],
) -> Result<(), Errno> {
    if bytes.is_empty() {
        return Ok(());
    }
    let address = AddrMut::<u8>::from_raw(pointer as usize).ok_or(Errno::EFAULT)?;
    memory.write_exact(address.cast(), bytes)
}

fn read_iovecs<M: MemoryAccess>(
    memory: &M,
    message: &libc::msghdr,
) -> Result<Vec<libc::iovec>, Errno> {
    if message.msg_iovlen == 0 {
        return Ok(Vec::new());
    }
    let address = Addr::from_raw(message.msg_iov as usize).ok_or(Errno::EFAULT)?;
    let mut iovecs = vec![
        libc::iovec {
            iov_base: std::ptr::null_mut(),
            iov_len: 0,
        };
        message.msg_iovlen
    ];
    memory.read_values(address, &mut iovecs)?;
    Ok(iovecs)
}

impl Replayer {
    pub(super) async fn handle_epoll_wait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: EpollWait,
    ) -> Result<i64, Errno> {
        let event = next_event!(guest, EpollWait)?;
        assert_eq!(
            event.events.len(),
            event.updated * std::mem::size_of::<libc::epoll_event>()
        );
        assert!(event.updated <= syscall.maxevents() as usize);

        if !event.events.is_empty() {
            guest
                .memory()
                .write_exact(syscall.events().ok_or(Errno::EFAULT)?.cast(), &event.events)?;
        }
        Ok(event.updated as i64)
    }

    pub(super) async fn handle_poll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Poll,
    ) -> Result<i64, Errno> {
        let event = next_event!(guest, Poll)?;

        let nfds = syscall.nfds() as usize;

        assert_eq!(event.fds.len(), nfds);

        // Write out the recorded fds (if any).
        if let Some(addr) = syscall.fds() {
            guest.memory().write_values(addr, &event.fds)?;
        }

        Ok(event.updated as i64)
    }

    pub(super) async fn handle_ppoll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Ppoll,
    ) -> Result<i64, Errno> {
        // `ppoll` shares `poll`'s recorded output (the updated pollfd array and
        // return count), so it reuses the `Poll` event. We restore every
        // recorded `revents` field without consulting live descriptors, and we
        // never inject the call, so the recorded temporary signal mask has no
        // replay effect (a recorded `EINTR` is reproduced via the event's
        // `Result`, handled before we get here).
        let event = next_event!(guest, Poll)?;

        let nfds = syscall.nfds() as usize;
        assert_eq!(event.fds.len(), nfds);

        // Write out the recorded fds (if any). `Ppoll::fds()` is typed as
        // `AddrMut<libc::pollfd>`; cast to the layout-compatible `PollFd`.
        if let Some(addr) = syscall.fds() {
            guest
                .memory()
                .write_values(addr.cast::<PollFd>(), &event.fds)?;
        }

        Ok(event.updated as i64)
    }

    pub(super) async fn handle_sockopt_family<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: SockOptFamily,
    ) -> Result<i64, Errno> {
        let event = next_event!(guest, SockOpt)?;

        // Write out the value.
        guest.memory().write_exact(
            syscall.value().ok_or(Errno::EFAULT)?.cast::<u8>(),
            &event.value,
        )?;

        // Write out the length parameter.
        guest
            .memory()
            .write_value(syscall.value_len().ok_or(Errno::EFAULT)?, &event.length)?;

        Ok(0)
    }

    pub(super) async fn handle_recvmsg<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Recvmsg,
    ) -> Result<i64, Errno> {
        let event = next_event!(guest, Recvmsg)?;
        let message_address = syscall.msg().ok_or(Errno::EFAULT)?;
        let mut message: libc::msghdr = guest.memory().read_value(message_address)?;
        let iovecs = read_iovecs(&guest.memory(), &message)?;
        assert_eq!(iovecs.len(), event.iovs.len());

        for (iovec, bytes) in iovecs.into_iter().zip(&event.iovs) {
            assert!(bytes.len() <= iovec.iov_len);
            write_bytes(&mut guest.memory(), iovec.iov_base, bytes)?;
        }

        assert!(event.name.len() <= message.msg_namelen as usize);
        assert!(event.control.len() <= message.msg_controllen);
        write_bytes(&mut guest.memory(), message.msg_name, &event.name)?;
        write_bytes(&mut guest.memory(), message.msg_control, &event.control)?;

        message.msg_namelen = event.name_len;
        message.msg_controllen = event.control_len;
        message.msg_flags = event.flags;
        guest.memory().write_value(message_address, &message)?;

        Ok(event.result)
    }

    pub(super) async fn handle_recvfrom<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Recvfrom,
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
}
