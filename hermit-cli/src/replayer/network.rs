/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use reverie::Errno;
use reverie::Guest;
use reverie::syscalls::AddrMut;
use reverie::syscalls::EpollWait;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Poll;
use reverie::syscalls::PollFd;
use reverie::syscalls::Ppoll;
use reverie::syscalls::Recvfrom;
use reverie::syscalls::Recvmsg;
use reverie::syscalls::Timespec;
use reverie::syscalls::family::SockOptFamily;

use super::Replayer;
use crate::event::PollEvent;
use crate::event::PpollEvent;

fn replay_pollfds<M: MemoryAccess>(
    memory: &mut M,
    fds_address: Option<AddrMut<'_, PollFd>>,
    nfds: usize,
    result: Result<i64, Errno>,
    fds_pointer_present: bool,
    fds: Option<Vec<PollFd>>,
) -> Result<(), Errno> {
    assert_eq!(
        fds_address.is_some(),
        fds_pointer_present,
        "recorded pollfd pointer shape diverged during replay"
    );

    if matches!(result, Ok(_) | Err(Errno::EINTR)) {
        assert_eq!(fds.is_some(), fds_pointer_present);
        if let Ok(updated) = result {
            assert!(updated >= 0);
            assert!((updated as usize) <= nfds);
        }
    } else if !matches!(result, Err(Errno::EFAULT)) {
        assert!(fds.is_none());
    }

    if let Some(fds) = fds {
        assert!(fds.len() <= nfds);
        if matches!(result, Ok(_) | Err(Errno::EINTR)) {
            assert_eq!(fds.len(), nfds);
        }
        let address = fds_address.expect("recorded pollfd output requires a pointer");
        let write_result = memory.write_values(address, &fds);
        if matches!(result, Err(Errno::EFAULT)) {
            // The same page boundary that made Linux return EFAULT can make
            // this replay write fault after restoring an earlier prefix. Keep
            // going so ppoll can also restore its captured timeout, then return
            // the recorded errno unchanged.
            if let Err(error) = write_result {
                tracing::trace!(?error, "partial pollfd replay write returned an error");
            }
        } else {
            write_result?;
        }
    }
    Ok(())
}

fn replay_poll_event<M: MemoryAccess>(
    memory: &mut M,
    fds_address: Option<AddrMut<'_, PollFd>>,
    nfds: usize,
    event: PollEvent,
) -> Result<i64, Errno> {
    let PollEvent {
        result,
        fds_pointer_present,
        fds,
    } = event;
    replay_pollfds(memory, fds_address, nfds, result, fds_pointer_present, fds)?;
    result
}

fn replay_ppoll_event<M: MemoryAccess>(
    memory: &mut M,
    fds_address: Option<AddrMut<'_, libc::pollfd>>,
    timeout_address: Option<AddrMut<'_, Timespec>>,
    nfds: usize,
    event: PpollEvent,
) -> Result<i64, Errno> {
    let PpollEvent {
        result,
        fds_pointer_present,
        fds,
        timeout_pointer_present,
        timeout,
    } = event;

    replay_pollfds(
        memory,
        fds_address.map(|address| address.cast::<PollFd>()),
        nfds,
        result,
        fds_pointer_present,
        fds,
    )?;

    assert_eq!(
        timeout_address.is_some(),
        timeout_pointer_present,
        "recorded ppoll timeout pointer shape diverged during replay"
    );
    if !matches!(result, Err(Errno::EFAULT)) {
        assert_eq!(timeout.is_some(), timeout_pointer_present);
    }
    if let Some(timeout) = timeout {
        let address = timeout_address.expect("recorded ppoll timeout requires a pointer");
        // Linux preserves the ppoll result when remaining-time copyout faults.
        // Restore the exact captured value when possible, but never replace the
        // recorded result with a replay-only memory error.
        if let Err(error) = memory.write_value(address, &timeout) {
            tracing::trace!(?error, "ppoll timeout replay write returned an error");
        }
    }

    result
}

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

fn cmsg_align(length: usize) -> Option<usize> {
    let alignment = std::mem::size_of::<usize>();
    length
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
}

fn scm_rights_fds(control: &[u8]) -> Vec<i32> {
    let header_size = std::mem::size_of::<libc::cmsghdr>();
    let data_offset = cmsg_align(header_size).unwrap();
    let mut offset: usize = 0;
    let mut fds = Vec::new();

    while offset
        .checked_add(header_size)
        .is_some_and(|end| end <= control.len())
    {
        // The recorded control buffer has native cmsghdr layout but may not be
        // aligned as a Vec<u8>, so read the header without assuming alignment.
        let header = unsafe {
            std::ptr::read_unaligned(control.as_ptr().add(offset).cast::<libc::cmsghdr>())
        };
        let length = header.cmsg_len;
        let Some(end) = offset.checked_add(length) else {
            break;
        };
        if length < data_offset || end > control.len() {
            break;
        }

        if header.cmsg_level == libc::SOL_SOCKET && header.cmsg_type == libc::SCM_RIGHTS {
            let (fd_bytes, _) =
                control[offset + data_offset..end].as_chunks::<{ std::mem::size_of::<i32>() }>();
            for bytes in fd_bytes {
                let fd = i32::from_ne_bytes(*bytes);
                if fd >= 0 {
                    fds.push(fd);
                }
            }
        }

        let Some(aligned_length) = cmsg_align(length) else {
            break;
        };
        let Some(next) = offset.checked_add(aligned_length) else {
            break;
        };
        if next <= offset {
            break;
        }
        offset = next;
    }
    fds
}

impl Replayer {
    pub(super) async fn handle_epoll_wait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: EpollWait,
    ) -> Result<i64, Errno> {
        let event = next_event!(guest, EpollWait)?;
        if event.replay_kernel_side_effect {
            let actual = guest.inject(syscall).await;
            assert_eq!(
                actual,
                Ok(event.updated as i64),
                "replayed epoll_wait kernel side effect diverged"
            );
        }
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
        replay_poll_event(
            &mut guest.memory(),
            syscall.fds(),
            syscall.nfds() as usize,
            event,
        )
    }

    pub(super) async fn handle_ppoll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Ppoll,
    ) -> Result<i64, Errno> {
        let event = next_event!(guest, Ppoll)?;
        replay_ppoll_event(
            &mut guest.memory(),
            syscall.fds(),
            syscall.timeout(),
            syscall.nfds() as usize,
            event,
        )
    }

    pub(super) async fn handle_sockopt_family<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: SockOptFamily,
    ) -> Result<i64, Errno> {
        let event = next_event!(guest, SockOpt)?;

        // A NULL value buffer is valid when the recorded value is empty.
        if let Some(address) = syscall.value() {
            guest
                .memory()
                .write_exact(address.cast::<u8>(), &event.value)?;
        } else if !event.value.is_empty() {
            return Err(Errno::EFAULT);
        }

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
        let cloexec = syscall.flags() & libc::MSG_CMSG_CLOEXEC != 0;
        for fd in scm_rights_fds(&event.control) {
            self.reserve_replay_fd(guest, fd, cloexec).await;
        }

        let message_address = syscall.msg().ok_or(Errno::EFAULT)?;
        let mut message: libc::msghdr = guest.memory().read_value(message_address)?;
        let iovecs = crate::read_iovecs(&guest.memory(), &message)?;
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

#[cfg(test)]
mod tests {
    use reverie::syscalls::LocalMemory;
    use reverie::syscalls::PollFlags;

    use super::*;

    #[test]
    fn replay_poll_restores_outputs_before_returning_efault() {
        let mut output = libc::pollfd {
            fd: 3,
            events: libc::POLLIN,
            revents: 0x1234,
        };
        let event = PollEvent {
            result: Err(Errno::EFAULT),
            fds_pointer_present: true,
            fds: Some(vec![PollFd {
                fd: 3,
                events: PollFlags::POLLIN,
                revents: PollFlags::POLLIN,
            }]),
        };
        let result = replay_poll_event(
            &mut LocalMemory::new(),
            AddrMut::<libc::pollfd>::from_raw((&mut output as *mut libc::pollfd) as usize)
                .map(|address| address.cast::<PollFd>()),
            1,
            event,
        );

        assert_eq!(result, Err(Errno::EFAULT));
        assert_eq!(output.revents, libc::POLLIN);
    }

    #[test]
    fn replay_ppoll_restores_outputs_and_exact_timeout_before_efault() {
        let mut output = libc::pollfd {
            fd: 5,
            events: libc::POLLIN,
            revents: 0x1234,
        };
        let mut timeout = Timespec {
            tv_sec: 3,
            tv_nsec: 456_789_123,
        };
        let recorded_timeout = Timespec {
            tv_sec: 3,
            tv_nsec: 456_780_001,
        };
        let event = PpollEvent {
            result: Err(Errno::EFAULT),
            fds_pointer_present: true,
            fds: Some(vec![PollFd {
                fd: 5,
                events: PollFlags::POLLIN,
                revents: PollFlags::POLLIN,
            }]),
            timeout_pointer_present: true,
            timeout: Some(recorded_timeout),
        };
        let result = replay_ppoll_event(
            &mut LocalMemory::new(),
            AddrMut::from_raw((&mut output as *mut libc::pollfd) as usize),
            AddrMut::from_raw((&mut timeout as *mut Timespec) as usize),
            1,
            event,
        );

        assert_eq!(result, Err(Errno::EFAULT));
        assert_eq!(output.revents, libc::POLLIN);
        assert_eq!(timeout, recorded_timeout);
    }

    #[test]
    fn replay_poll_restores_outputs_before_returning_eintr() {
        let mut output = libc::pollfd {
            fd: 11,
            events: libc::POLLIN,
            revents: 0x1234,
        };
        let event = PollEvent {
            result: Err(Errno::EINTR),
            fds_pointer_present: true,
            fds: Some(vec![PollFd {
                fd: 11,
                events: PollFlags::POLLIN,
                revents: PollFlags::empty(),
            }]),
        };
        let result = replay_poll_event(
            &mut LocalMemory::new(),
            AddrMut::<libc::pollfd>::from_raw((&mut output as *mut libc::pollfd) as usize)
                .map(|address| address.cast::<PollFd>()),
            1,
            event,
        );

        assert_eq!(result, Err(Errno::EINTR));
        assert_eq!(output.revents, 0);
    }

    #[test]
    fn replay_einval_performs_no_pollfd_write() {
        let event = PollEvent {
            result: Err(Errno::EINVAL),
            fds_pointer_present: true,
            fds: None,
        };
        let result = replay_poll_event(
            &mut LocalMemory::new(),
            AddrMut::from_raw(1),
            usize::MAX,
            event,
        );

        assert_eq!(result, Err(Errno::EINVAL));
    }
}

#[cfg(test)]
mod current_main_tests {
    use std::io;

    use reverie::syscalls::LocalMemory;
    use reverie::syscalls::MemoryAccess;
    use reverie::syscalls::PollFlags;

    use super::*;

    struct FailSecondWrite {
        memory: LocalMemory,
        writes: usize,
    }

    impl MemoryAccess for FailSecondWrite {
        fn read_vectored(
            &self,
            read_from: &[io::IoSlice],
            write_to: &mut [io::IoSliceMut],
        ) -> Result<usize, Errno> {
            self.memory.read_vectored(read_from, write_to)
        }

        fn write_vectored(
            &mut self,
            _read_from: &[io::IoSlice],
            _write_to: &mut [io::IoSliceMut],
        ) -> Result<usize, Errno> {
            unreachable!("write_exact dispatches through the overridden write method")
        }

        fn write(&mut self, address: AddrMut<u8>, bytes: &[u8]) -> Result<usize, Errno> {
            self.writes += 1;
            if self.writes == 2 {
                Err(Errno::EFAULT)
            } else {
                self.memory.write(address, bytes)
            }
        }
    }

    #[test]
    fn replay_ppoll_restores_exact_timeout_and_successful_pollfds() {
        let mut output_fd = libc::pollfd {
            fd: 7,
            events: libc::POLLIN,
            revents: 0,
        };
        let mut output_timeout = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let recorded_timeout = Timespec {
            tv_sec: 3,
            tv_nsec: 456_789_123,
        };
        let event = PpollEvent {
            result: Ok(1),
            fds_pointer_present: true,
            fds: Some(vec![PollFd {
                fd: 7,
                events: PollFlags::POLLIN,
                revents: PollFlags::POLLIN,
            }]),
            timeout_pointer_present: true,
            timeout: Some(recorded_timeout),
        };

        let result = replay_ppoll_event(
            &mut LocalMemory::new(),
            AddrMut::from_raw((&mut output_fd as *mut libc::pollfd) as usize),
            AddrMut::from_raw((&mut output_timeout as *mut Timespec) as usize),
            1,
            event,
        );

        assert_eq!(result, Ok(1));
        assert_eq!(output_fd.revents, libc::POLLIN);
        assert_eq!(output_timeout, recorded_timeout);
    }

    #[test]
    fn replay_ppoll_restores_exact_timeout_before_recorded_error() {
        let mut output_timeout = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let recorded_timeout = Timespec {
            tv_sec: 2,
            tv_nsec: 345_678_901,
        };
        let event = PpollEvent {
            result: Err(Errno::EINTR),
            fds_pointer_present: false,
            fds: None,
            timeout_pointer_present: true,
            timeout: Some(recorded_timeout),
        };

        let result = replay_ppoll_event(
            &mut LocalMemory::new(),
            None,
            AddrMut::from_raw((&mut output_timeout as *mut Timespec) as usize),
            0,
            event,
        );

        assert_eq!(result, Err(Errno::EINTR));
        assert_eq!(output_timeout, recorded_timeout);
    }

    #[test]
    fn replay_ppoll_writes_ready_fds_and_preserves_result_on_timeout_copyout_error() {
        let mut output_fd = libc::pollfd {
            fd: 7,
            events: libc::POLLIN,
            revents: 0,
        };
        let mut output_timeout = Timespec {
            tv_sec: 9,
            tv_nsec: 876_543_210,
        };
        let original_timeout = output_timeout;
        let event = PpollEvent {
            result: Ok(1),
            fds_pointer_present: true,
            fds: Some(vec![PollFd {
                fd: 7,
                events: PollFlags::POLLIN,
                revents: PollFlags::POLLIN,
            }]),
            timeout_pointer_present: true,
            timeout: Some(Timespec {
                tv_sec: 3,
                tv_nsec: 456_789_123,
            }),
        };
        let mut memory = FailSecondWrite {
            memory: LocalMemory::new(),
            writes: 0,
        };

        let result = replay_ppoll_event(
            &mut memory,
            AddrMut::from_raw((&mut output_fd as *mut libc::pollfd) as usize),
            AddrMut::from_raw((&mut output_timeout as *mut Timespec) as usize),
            1,
            event,
        );

        assert_eq!(result, Ok(1));
        assert_eq!(memory.writes, 2);
        assert_eq!(output_fd.revents, libc::POLLIN);
        assert_eq!(output_timeout, original_timeout);
    }

    #[test]
    fn replay_poll_restores_fds_before_returning_a_recorded_error() {
        // THE ORDERING IS THE FIX. The old replayer returned the recorded error
        // before writing anything, so a guest that recorded a partial `revents`
        // copy-out saw its own pre-syscall sentinels instead.
        let mut observed = [PollFd {
            fd: 7,
            events: PollFlags::POLLIN,
            revents: PollFlags::empty(),
        }];
        let event = PollEvent {
            result: Err(Errno::EFAULT),
            fds_pointer_present: true,
            fds: Some(vec![PollFd {
                fd: 7,
                events: PollFlags::POLLIN,
                revents: PollFlags::POLLIN,
            }]),
        };

        let result = replay_poll_event(
            &mut LocalMemory::new(),
            AddrMut::from_raw(observed.as_mut_ptr() as usize),
            observed.len(),
            event,
        );

        assert_eq!(result, Err(Errno::EFAULT));
        assert_eq!(
            observed[0].revents,
            PollFlags::POLLIN,
            "the recorded partial copy-out must be restored even though the call errored"
        );
    }

    #[test]
    fn replay_poll_early_error_with_pollfd_pointer_performs_no_fd_write() {
        let event = PollEvent {
            result: Err(Errno::EINVAL),
            fds_pointer_present: true,
            fds: None,
        };
        let mut memory = FailSecondWrite {
            memory: LocalMemory::new(),
            writes: 0,
        };

        let result = replay_poll_event(&mut memory, AddrMut::from_raw(1), usize::MAX, event);

        assert_eq!(result, Err(Errno::EINVAL));
        assert_eq!(memory.writes, 0);
    }

    #[test]
    fn replay_ppoll_early_error_with_pollfd_pointer_performs_no_fd_write() {
        let event = PpollEvent {
            result: Err(Errno::EINVAL),
            fds_pointer_present: true,
            fds: None,
            timeout_pointer_present: false,
            timeout: None,
        };
        let mut memory = FailSecondWrite {
            memory: LocalMemory::new(),
            writes: 0,
        };

        let result = replay_ppoll_event(&mut memory, AddrMut::from_raw(1), None, usize::MAX, event);

        assert_eq!(result, Err(Errno::EINVAL));
        assert_eq!(memory.writes, 0);
    }
}
