/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Handles poll, ppoll, epoll, and select system calls.

use reverie::Errno;
use reverie::Guest;
use reverie::Pid;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::EpollWait;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Poll;
use reverie::syscalls::PollFd;
use reverie::syscalls::Ppoll;
use reverie::syscalls::Pselect6;
use reverie::syscalls::Recvfrom;
use reverie::syscalls::Recvmsg;
use reverie::syscalls::Select;
use reverie::syscalls::Syscall;
use reverie::syscalls::Timespec;
use reverie::syscalls::family::SockOptFamily;

use super::Recorder;
use crate::event::EpollWaitEvent;
use crate::event::PollEvent;
use crate::event::PpollEvent;
use crate::event::RecvmsgEvent;
use crate::event::SelectEvent;
use crate::event::SockOptEvent;
use crate::event::SyscallEvent;
use crate::event::fd_set_bytes;

fn read_bytes<M: MemoryAccess>(
    memory: &M,
    pointer: *mut libc::c_void,
    length: usize,
) -> Result<Vec<u8>, Errno> {
    if length == 0 {
        return Ok(Vec::new());
    }
    let address = Addr::<u8>::from_raw(pointer as usize).ok_or(Errno::EFAULT)?;
    let mut bytes = vec![0; length];
    memory.read_exact(address.cast(), &mut bytes)?;
    Ok(bytes)
}

fn pollfd_address<'a>(address: AddrMut<'a, PollFd>, index: usize) -> Option<AddrMut<'a, PollFd>> {
    let offset = index.checked_mul(std::mem::size_of::<PollFd>())?;
    AddrMut::from_raw(address.as_raw().checked_add(offset)?)
}

fn read_pollfds<M: MemoryAccess>(
    memory: &M,
    address: AddrMut<'_, PollFd>,
    nfds: usize,
) -> Result<Vec<PollFd>, Errno> {
    let mut fds = vec![PollFd::default(); nfds];
    memory.read_values(address.into(), &mut fds)?;
    Ok(fds)
}

/// Read as much of an EFAULT result as remains readable without allocating from
/// an untrusted `nfds` up front. Linux may have written earlier `revents`
/// entries before faulting on a later copy-out.
fn read_pollfd_prefix<M: MemoryAccess>(
    memory: &M,
    address: AddrMut<'_, PollFd>,
    nfds: usize,
) -> Vec<PollFd> {
    const ENTRIES_PER_READ: usize = 256;

    let mut fds = Vec::new();
    let mut index = 0;
    while index < nfds {
        let Some(chunk_address) = pollfd_address(address, index) else {
            break;
        };
        let chunk_len = (nfds - index).min(ENTRIES_PER_READ);
        let mut chunk = vec![PollFd::default(); chunk_len];
        let bytes = unsafe {
            std::slice::from_raw_parts_mut(
                chunk.as_mut_ptr().cast::<u8>(),
                std::mem::size_of_val(chunk.as_slice()),
            )
        };
        let bytes_read = match memory.read(chunk_address.cast::<u8>(), bytes) {
            Ok(bytes_read) => bytes_read,
            Err(_) => break,
        };
        let complete_entries = bytes_read / std::mem::size_of::<PollFd>();
        if complete_entries == 0 {
            break;
        }
        fds.extend(chunk.into_iter().take(complete_entries));
        index += complete_entries;
        if bytes_read % std::mem::size_of::<PollFd>() != 0 {
            break;
        }
    }
    fds
}

fn capture_pollfds<M: MemoryAccess>(
    memory: &M,
    address: Option<AddrMut<'_, PollFd>>,
    nfds: usize,
    result: Result<i64, Errno>,
) -> Result<Option<Vec<PollFd>>, Errno> {
    match result {
        Ok(_) | Err(Errno::EINTR) => address
            .map(|address| read_pollfds(memory, address, nfds))
            .transpose(),
        // Best effort is load-bearing: a wholly bad pointer also returns
        // EFAULT. A failed diagnostic read must not replace the recorded errno.
        Err(Errno::EFAULT) => Ok(address.map(|address| read_pollfd_prefix(memory, address, nfds))),
        // In particular, EINVAL for nfds > RLIMIT_NOFILE must not trigger a
        // read or an allocation based on the invalid count.
        _ => Ok(None),
    }
}

fn capture_poll_event<M: MemoryAccess>(
    memory: &M,
    fds_address: Option<AddrMut<'_, PollFd>>,
    nfds: usize,
    result: Result<i64, Errno>,
) -> Result<PollEvent, Errno> {
    Ok(PollEvent {
        result,
        fds_pointer_present: fds_address.is_some(),
        fds: capture_pollfds(memory, fds_address, nfds, result)?,
    })
}

fn capture_ppoll_event<M: MemoryAccess>(
    memory: &M,
    fds_address: Option<AddrMut<'_, libc::pollfd>>,
    timeout_address: Option<AddrMut<'_, Timespec>>,
    nfds: usize,
    result: Result<i64, Errno>,
) -> Result<PpollEvent, Errno> {
    let timeout = match result {
        // The timeout can also be updated when pollfd copy-out later faults.
        // If EFAULT instead came from an unreadable timeout pointer, retain the
        // syscall result and simply record no readable timeout value.
        Err(Errno::EFAULT) => timeout_address.and_then(|address| memory.read_value(address).ok()),
        // Raw ppoll can update its remaining-time argument on success and on
        // error returns such as EINTR, EBADF, or EINVAL. Preserve future errno
        // behavior too: if Linux accepted a readable timeout input before
        // producing the result, its post-call bytes are guest-visible.
        _ => timeout_address
            .map(|address| memory.read_value(address))
            .transpose()?,
    };
    let fds =
        if matches!(result, Err(Errno::EFAULT)) && timeout_address.is_some() && timeout.is_none() {
            // Linux validates the timeout input before polling. If that pointer is
            // unreadable, this EFAULT occurred before any pollfd output copy-out;
            // do not scan a potentially huge valid array looking for outputs that
            // the kernel never attempted to write.
            None
        } else {
            capture_pollfds(
                memory,
                fds_address.map(|address| address.cast::<PollFd>()),
                nfds,
                result,
            )?
        };

    Ok(PpollEvent {
        result,
        fds_pointer_present: fds_address.is_some(),
        fds,
        timeout_pointer_present: timeout_address.is_some(),
        timeout,
    })
}

/// Read up to `length` bytes, stopping at the first unreadable byte. Linux
/// clamps `nfds` to the descriptor table size, so a successful call writes a
/// prefix of the guest's range; any readable bytes past it are unchanged guest
/// memory, which replay restores to the same value.
fn read_byte_prefix<M: MemoryAccess>(
    memory: &M,
    address: AddrMut<'_, u8>,
    length: usize,
) -> Vec<u8> {
    let mut bytes = vec![0; length];
    let mut filled = 0;
    while filled < length {
        let Some(chunk) = address
            .as_raw()
            .checked_add(filled)
            .and_then(AddrMut::<u8>::from_raw)
        else {
            break;
        };
        match memory.read(chunk, &mut bytes[filled..]) {
            Ok(0) | Err(_) => break,
            Ok(count) => filled += count,
        }
    }
    bytes.truncate(filled);
    bytes
}

/// The kernel's descriptor-table size for `tid`, from the `FDSize:` line of
/// `/proc/<tid>/status`. `core_sys_select` clamps `nfds` to this before it
/// reads or writes a set, so no byte past it belongs to the call.
fn guest_max_fds(tid: Pid) -> Option<i32> {
    let status = std::fs::read_to_string(format!("/proc/{}/status", tid.as_raw())).ok()?;
    parse_fd_size(&status)
}

/// A task's table holds at least `NR_OPEN_DEFAULT` (`BITS_PER_LONG`) entries.
/// A smaller value means a task without a table, or the wrong task, and would
/// silently record empty sets, so it is treated as unknown.
fn parse_fd_size(status: &str) -> Option<i32> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("FDSize:"))
        .and_then(|value| value.trim().parse().ok())
        .filter(|&size: &i32| size >= 64)
}

/// The `nfds` the kernel acted on. Without a table size, fall back to the
/// default `fs.nr_open` limit of 1048576 descriptors (128 KiB per set), so a
/// guest passing `INT_MAX` cannot make the recorder allocate gigabytes.
fn select_capture_nfds(nfds: i32, max_fds: Option<i32>) -> i32 {
    nfds.min(max_fds.unwrap_or(1 << 20))
}

fn capture_select_event<M: MemoryAccess>(
    memory: &M,
    nfds: i32,
    fd_sets: [Option<AddrMut<'_, libc::fd_set>>; 3],
    timeout: Option<(AddrMut<'_, u8>, usize)>,
    result: Result<i64, Errno>,
) -> SelectEvent {
    // fs/select.c copies the sets out after a successful wait and can fault
    // partway through; every other result leaves them as the guest wrote them.
    // `nfds` must already be clamped to the descriptor table.
    let copied_out = matches!(result, Ok(_) | Err(Errno::EFAULT));
    let length = fd_set_bytes(nfds);
    let fd_sets = fd_sets.map(|address| {
        address
            .filter(|_| copied_out)
            .map(|address| read_byte_prefix(memory, address.cast(), length))
    });
    // Linux may write the remaining time back on any result that reached the
    // wait, and leaves it alone otherwise; capturing the post-call bytes is
    // right either way, because replaying unchanged bytes changes nothing. An
    // unreadable pointer could not have been written.
    let timeout = timeout
        .map(|(address, length)| read_byte_prefix(memory, address, length))
        .filter(|bytes| !bytes.is_empty());
    SelectEvent {
        result,
        fd_sets,
        timeout,
    }
}

impl Recorder {
    pub(super) async fn handle_epoll_wait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: EpollWait,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;

        let event = result.and_then(|ret| {
            let updated = ret as usize;
            let mut events = vec![0; updated * std::mem::size_of::<libc::epoll_event>()];
            if !events.is_empty() {
                guest
                    .memory()
                    .read_exact(syscall.events().ok_or(Errno::EFAULT)?.cast(), &mut events)?;
            }
            Ok(SyscallEvent::EpollWait(EpollWaitEvent {
                events,
                updated,
                replay_kernel_side_effect: self
                    .epoll_requires_replay_kernel_side_effect(guest.pid(), syscall.epfd()),
            }))
        });

        self.record_event(guest, event);
        result
    }

    pub(super) async fn handle_poll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Poll,
    ) -> Result<i64, Errno> {
        let len = syscall.nfds() as usize;
        let result = guest.inject(syscall).await;

        let event =
            capture_poll_event(&guest.memory(), syscall.fds(), len, result).map(SyscallEvent::Poll);

        self.record_event(guest, event);

        result
    }

    pub(super) async fn handle_ppoll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Ppoll,
    ) -> Result<i64, Errno> {
        let len = syscall.nfds() as usize;
        let timeout_is_zero = syscall
            .timeout()
            .and_then(|address| {
                let timeout: Timespec = guest.memory().read_value(address).ok()?;
                Some(timeout)
            })
            .is_some_and(|timeout| timeout.tv_sec == 0 && timeout.tv_nsec == 0);
        tracing::trace!(
            has_signal_mask = syscall.sigmask().is_some(),
            timeout_is_zero,
            "Recorder observed ppoll input"
        );
        let result = guest.inject(syscall).await;

        let event = capture_ppoll_event(
            &guest.memory(),
            syscall.fds(),
            syscall.timeout(),
            len,
            result,
        )
        .map(SyscallEvent::Ppoll);

        self.record_event(guest, event);

        result
    }

    pub(super) async fn handle_select<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Select,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;
        let event = capture_select_event(
            &guest.memory(),
            select_capture_nfds(syscall.nfds(), guest_max_fds(guest.tid())),
            [syscall.readfds(), syscall.writefds(), syscall.exceptfds()],
            syscall
                .timeout()
                .map(|address| (address.cast(), std::mem::size_of::<libc::timeval>())),
            result,
        );
        self.record_event(guest, Ok(SyscallEvent::Select(event)));
        result
    }

    pub(super) async fn handle_pselect6<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Pselect6,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;
        let event = capture_select_event(
            &guest.memory(),
            select_capture_nfds(syscall.nfds(), guest_max_fds(guest.tid())),
            [syscall.readfds(), syscall.writefds(), syscall.exceptfds()],
            syscall
                .timeout()
                .map(|address| (address.cast(), std::mem::size_of::<Timespec>())),
            result,
        );
        self.record_event(guest, Ok(SyscallEvent::Select(event)));
        result
    }

    pub(super) async fn handle_sockopt_family<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: SockOptFamily,
    ) -> Result<i64, Errno> {
        // The buffer length is both an input and output. If optlen is smaller
        // than the real value, then the value will be truncated.

        let buflen_addr = syscall.value_len().ok_or(Errno::EFAULT)?;

        // `optlen` will be updated after the syscall has been injected.
        let buflen: libc::socklen_t = guest.memory().read_value(buflen_addr)?;

        let result = guest.inject(Syscall::from(syscall)).await;

        let event = result.and_then(|ret| {
            debug_assert_eq!(ret, 0);

            // Linux permits a NULL value buffer when its input length is zero.
            let value = if let Some(address) = syscall.value() {
                let mut value = vec![0u8; buflen as usize];
                guest
                    .memory()
                    .read_exact(address.cast::<u8>(), &mut value)?;
                value
            } else {
                Vec::new()
            };

            // Need to read the (new) length. This might not have been updated,
            // but we don't know until we check it.
            let length: libc::socklen_t = guest.memory().read_value(buflen_addr)?;

            Ok(SyscallEvent::SockOpt(SockOptEvent { value, length }))
        });

        self.record_event(guest, event);

        result
    }

    pub(super) async fn handle_recvmsg<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Recvmsg,
    ) -> Result<i64, Errno> {
        let input = syscall
            .msg()
            .ok_or(Errno::EFAULT)
            .and_then(|address| guest.memory().read_value(address))
            .map(|message: libc::msghdr| (message.msg_namelen as usize, message.msg_controllen));
        let result = guest.inject(syscall).await;

        self.record_event(
            guest,
            result.and_then(|result| {
                let (name_capacity, control_capacity) = input?;
                let message_address = syscall.msg().ok_or(Errno::EFAULT)?;
                let output: libc::msghdr = guest.memory().read_value(message_address)?;
                let iovecs = crate::read_iovecs(&guest.memory(), &output)?;
                let mut remaining = usize::try_from(result).map_err(|_| Errno::EINVAL)?;
                let mut buffers = Vec::with_capacity(iovecs.len());
                for iovec in iovecs {
                    let length = remaining.min(iovec.iov_len);
                    buffers.push(read_bytes(&guest.memory(), iovec.iov_base, length)?);
                    remaining -= length;
                }

                let name_length = name_capacity.min(output.msg_namelen as usize);
                let control_length = control_capacity.min(output.msg_controllen);

                Ok(SyscallEvent::Recvmsg(RecvmsgEvent {
                    result,
                    iovs: buffers,
                    name: read_bytes(&guest.memory(), output.msg_name, name_length)?,
                    name_len: output.msg_namelen,
                    control: read_bytes(&guest.memory(), output.msg_control, control_length)?,
                    control_len: output.msg_controllen,
                    flags: output.msg_flags,
                }))
            }),
        );

        result
    }

    pub(super) async fn handle_recvfrom<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Recvfrom,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;

        // TODO: Handle `addr` and `addr_len` parameters. These are NULL most of
        // the time. Maybe these can be recorded as a separate event SockOpt
        // event if non-NULL.

        // Treat this exactly the same way as a `read` syscall.
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

    // TODO: Add support for select here.
}

#[cfg(test)]
mod tests {
    use reverie::syscalls::LocalMemory;
    use reverie::syscalls::PollFlags;

    use super::*;

    #[test]
    fn capture_poll_keeps_outputs_on_efault() {
        let fds = [
            PollFd {
                fd: 3,
                events: PollFlags::POLLIN,
                revents: PollFlags::POLLIN,
            },
            PollFd {
                fd: 5,
                events: PollFlags::POLLIN,
                revents: PollFlags::empty(),
            },
        ];
        let event = capture_poll_event(
            &LocalMemory::new(),
            AddrMut::from_raw(fds.as_ptr() as usize),
            fds.len(),
            Err(Errno::EFAULT),
        )
        .unwrap();

        assert_eq!(event.result, Err(Errno::EFAULT));
        assert!(event.fds_pointer_present);
        assert_eq!(event.fds.as_deref(), Some(&fds[..]));
    }

    #[test]
    fn capture_ppoll_keeps_timeout_and_outputs_on_efault() {
        let fds = [PollFd {
            fd: 7,
            events: PollFlags::POLLIN,
            revents: PollFlags::POLLIN,
        }];
        let timeout = Timespec {
            tv_sec: 2,
            tv_nsec: 345_678_901,
        };
        let event = capture_ppoll_event(
            &LocalMemory::new(),
            AddrMut::<PollFd>::from_raw(fds.as_ptr() as usize)
                .map(|address| address.cast::<libc::pollfd>()),
            AddrMut::from_raw((&timeout as *const Timespec) as usize),
            fds.len(),
            Err(Errno::EFAULT),
        )
        .unwrap();

        assert_eq!(event.result, Err(Errno::EFAULT));
        assert_eq!(event.fds.as_deref(), Some(&fds[..]));
        assert_eq!(event.timeout, Some(timeout));
    }

    #[test]
    fn capture_poll_keeps_outputs_on_eintr() {
        let fds = [PollFd {
            fd: 9,
            events: PollFlags::POLLIN,
            revents: PollFlags::empty(),
        }];
        let event = capture_poll_event(
            &LocalMemory::new(),
            AddrMut::from_raw(fds.as_ptr() as usize),
            fds.len(),
            Err(Errno::EINTR),
        )
        .unwrap();

        assert_eq!(event.result, Err(Errno::EINTR));
        assert_eq!(event.fds.as_deref(), Some(&fds[..]));
    }

    #[test]
    fn early_einval_does_not_read_or_allocate_pollfds() {
        let event = capture_poll_event(
            &LocalMemory::new(),
            AddrMut::from_raw(1),
            usize::MAX,
            Err(Errno::EINVAL),
        )
        .unwrap();

        assert_eq!(event.result, Err(Errno::EINVAL));
        assert!(event.fds_pointer_present);
        assert!(event.fds.is_none());
    }

    #[test]
    fn ppoll_einval_preserves_timeout_without_reading_pollfds() {
        let timeout = Timespec {
            tv_sec: 1,
            tv_nsec: 234_567_890,
        };
        let event = capture_ppoll_event(
            &LocalMemory::new(),
            AddrMut::from_raw(1),
            AddrMut::from_raw((&timeout as *const Timespec) as usize),
            usize::MAX,
            Err(Errno::EINVAL),
        )
        .unwrap();

        assert_eq!(event.result, Err(Errno::EINVAL));
        assert!(event.fds.is_none());
        assert_eq!(event.timeout, Some(timeout));
    }

    #[test]
    fn capture_ppoll_preserves_exact_timeout_on_eintr() {
        let timeout = Timespec {
            tv_sec: 2,
            tv_nsec: 345_678_901,
        };

        let event = capture_ppoll_event(
            &LocalMemory::new(),
            None,
            AddrMut::from_raw((&timeout as *const Timespec) as usize),
            0,
            Err(Errno::EINTR),
        )
        .unwrap();

        assert_eq!(event.result, Err(Errno::EINTR));
        assert!(!event.fds_pointer_present);
        assert!(event.fds.is_none());
        assert_eq!(event.timeout, Some(timeout));
    }

    /// Linux can fault partway through the `revents` copy-out, leaving earlier
    /// entries written and still returning EFAULT. Those writes are
    /// guest-visible, so they must be captured or replay restores nothing and
    /// the guest keeps its pre-syscall sentinels -- measured as a real
    /// divergence: record `revents0=1` against replay `revents0=4660`, where
    /// 4660 is 0x1234, the value the guest itself wrote before the call.
    #[test]
    fn capture_ppoll_keeps_the_partial_copyout_on_efault() {
        let fds = [
            PollFd {
                fd: 3,
                events: PollFlags::POLLIN,
                revents: PollFlags::POLLIN,
            },
            PollFd {
                fd: 5,
                events: PollFlags::POLLIN,
                revents: PollFlags::empty(),
            },
        ];

        let event = capture_ppoll_event(
            &LocalMemory::new(),
            AddrMut::from_raw(fds.as_ptr() as usize),
            None,
            fds.len(),
            Err(Errno::EFAULT),
        )
        .unwrap();

        assert_eq!(event.result, Err(Errno::EFAULT));
        assert!(event.fds_pointer_present);
        assert_eq!(
            event.fds.as_deref(),
            Some(&fds[..]),
            "a partially completed copy-out must be captured, not discarded"
        );
    }

    #[test]
    fn capture_poll_preserves_ready_fds() {
        let fds = [PollFd {
            fd: 7,
            events: PollFlags::POLLIN,
            revents: PollFlags::POLLIN,
        }];

        let event = capture_poll_event(
            &LocalMemory::new(),
            AddrMut::from_raw(fds.as_ptr() as usize),
            fds.len(),
            Ok(1),
        )
        .unwrap();

        assert_eq!(event.result, Ok(1));
        assert!(event.fds_pointer_present);
        assert_eq!(event.fds.unwrap()[0].revents, PollFlags::POLLIN);
    }

    #[test]
    fn capture_poll_keeps_the_result_on_an_error_return() {
        // The defect this fixes: the old `result.and_then(..)` recorded NOTHING
        // on an error, so the event -- and with it every already-written
        // `revents` entry -- was discarded. The result must now live inside the
        // event so replay has something to restore before returning it.
        let event = capture_poll_event(&LocalMemory::new(), None, 0, Err(Errno::EFAULT)).unwrap();

        assert_eq!(event.result, Err(Errno::EFAULT));
        assert!(!event.fds_pointer_present);
        assert!(event.fds.is_none());
    }

    #[test]
    fn capture_poll_early_error_does_not_read_or_allocate_pollfds() {
        // EINVAL must not trigger the best-effort read. An earlier attempt on
        // the ppoll side read on EVERY error and the read's own EFAULT replaced
        // the kernel's EINVAL, flipping `review-cases invalid-nfds` red. `nfds`
        // is deliberately absurd: a read would try to allocate it and abort.
        let event = capture_poll_event(
            &LocalMemory::new(),
            AddrMut::from_raw(1),
            usize::MAX,
            Err(Errno::EINVAL),
        )
        .unwrap();

        assert_eq!(event.result, Err(Errno::EINVAL));
        assert!(event.fds_pointer_present);
        assert!(event.fds.is_none());
    }

    #[test]
    fn capture_ppoll_preserves_ready_fds_and_unchanged_timeout() {
        let fds = [PollFd {
            fd: 7,
            events: PollFlags::POLLIN,
            revents: PollFlags::POLLIN,
        }];
        let timeout = Timespec {
            tv_sec: 3,
            tv_nsec: 456_789_123,
        };

        let event = capture_ppoll_event(
            &LocalMemory::new(),
            AddrMut::from_raw(fds.as_ptr() as usize),
            AddrMut::from_raw((&timeout as *const Timespec) as usize),
            fds.len(),
            Ok(1),
        )
        .unwrap();

        assert_eq!(event.result, Ok(1));
        assert!(event.fds_pointer_present);
        assert_eq!(event.fds.unwrap()[0].revents, PollFlags::POLLIN);
        assert_eq!(event.timeout, Some(timeout));
    }

    #[test]
    fn capture_ppoll_early_error_does_not_read_or_allocate_pollfds() {
        let event = capture_ppoll_event(
            &LocalMemory::new(),
            AddrMut::from_raw(1),
            None,
            usize::MAX,
            Err(Errno::EINVAL),
        )
        .unwrap();

        assert_eq!(event.result, Err(Errno::EINVAL));
        assert!(event.fds_pointer_present);
        assert!(event.fds.is_none());
        assert!(event.timeout.is_none());
    }

    fn fd_set_with(fds: &[i32]) -> libc::fd_set {
        let mut set: libc::fd_set = unsafe { std::mem::zeroed() };
        for &fd in fds {
            unsafe { libc::FD_SET(fd, &mut set) };
        }
        set
    }

    #[test]
    fn fd_set_bytes_rounds_up_to_whole_longs() {
        assert_eq!(fd_set_bytes(-1), 0);
        assert_eq!(fd_set_bytes(0), 0);
        assert_eq!(fd_set_bytes(1), 8);
        assert_eq!(fd_set_bytes(64), 8);
        assert_eq!(fd_set_bytes(65), 16);
    }

    #[test]
    fn capture_select_keeps_sets_and_timeout_on_success() {
        let mut readfds = fd_set_with(&[3]);
        let mut timeout = libc::timeval {
            tv_sec: 1,
            tv_usec: 234_567,
        };
        let event = capture_select_event(
            &LocalMemory::new(),
            4,
            [
                AddrMut::from_raw((&mut readfds as *mut libc::fd_set) as usize),
                None,
                None,
            ],
            AddrMut::<u8>::from_raw((&mut timeout as *mut libc::timeval) as usize)
                .map(|address| (address, std::mem::size_of::<libc::timeval>())),
            Ok(1),
        );

        assert_eq!(event.result, Ok(1));
        let [read, write, except] = event.fd_sets;
        assert_eq!(read, Some(vec![0b1000, 0, 0, 0, 0, 0, 0, 0]));
        assert!(write.is_none());
        assert!(except.is_none());
        let mut expected = vec![0u8; std::mem::size_of::<libc::timeval>()];
        expected[..8].copy_from_slice(&1i64.to_ne_bytes());
        expected[8..].copy_from_slice(&234_567i64.to_ne_bytes());
        assert_eq!(event.timeout, Some(expected));
    }

    #[test]
    fn select_capture_is_clamped_to_the_descriptor_table() {
        let status = "Name:\tguest\nFDSize:\t64\nGroups:\t\n";
        assert_eq!(parse_fd_size(status), Some(64));
        assert_eq!(parse_fd_size("Name:\tguest\n"), None);
        assert_eq!(parse_fd_size("FDSize:\t0\n"), None);
        let own = Pid::from_raw(unsafe { libc::gettid() });
        assert!(guest_max_fds(own).is_some_and(|size| size >= 64));

        assert_eq!(select_capture_nfds(4, Some(64)), 4);
        assert_eq!(select_capture_nfds(i32::MAX, Some(64)), 64);
        assert_eq!(fd_set_bytes(select_capture_nfds(i32::MAX, Some(64))), 8);
        assert_eq!(select_capture_nfds(i32::MAX, None), 1 << 20);
        assert_eq!(select_capture_nfds(-1, Some(64)), -1);

        // A guest-sized nfds far past its table reads only the table's bytes.
        let mut readfds = fd_set_with(&[3]);
        let event = capture_select_event(
            &LocalMemory::new(),
            select_capture_nfds(i32::MAX, Some(64)),
            [
                AddrMut::from_raw((&mut readfds as *mut libc::fd_set) as usize),
                None,
                None,
            ],
            None,
            Ok(1),
        );
        assert_eq!(event.fd_sets[0], Some(vec![0b1000, 0, 0, 0, 0, 0, 0, 0]));
    }

    #[test]
    fn capture_select_keeps_the_readable_prefix_and_drops_an_unreadable_timeout() {
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                2 * page,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(base, libc::MAP_FAILED);
        assert_eq!(
            unsafe { libc::mprotect(base.cast::<u8>().add(page).cast(), page, libc::PROT_NONE) },
            0
        );
        // The set starts 16 bytes before the inaccessible page, as a partial
        // copy-out before EFAULT would leave it.
        let set = base as usize + page - 16;
        unsafe { std::ptr::write_bytes(set as *mut u8, 0xa5, 16) };
        let event = capture_select_event(
            &LocalMemory::new(),
            1024,
            [AddrMut::from_raw(set), None, None],
            AddrMut::<u8>::from_raw(base as usize + page)
                .map(|address| (address, std::mem::size_of::<Timespec>())),
            Err(Errno::EFAULT),
        );
        unsafe { libc::munmap(base, 2 * page) };

        assert_eq!(event.fd_sets[0], Some(vec![0xa5; 16]));
        assert!(event.timeout.is_none());
    }

    #[test]
    fn capture_select_omits_sets_the_kernel_did_not_copy_out() {
        let mut readfds = fd_set_with(&[3]);
        let event = capture_select_event(
            &LocalMemory::new(),
            4,
            [
                AddrMut::from_raw((&mut readfds as *mut libc::fd_set) as usize),
                None,
                None,
            ],
            None,
            Err(Errno::EBADF),
        );

        assert_eq!(event.result, Err(Errno::EBADF));
        assert!(event.fd_sets.iter().all(Option::is_none));
        assert!(event.timeout.is_none());
    }
}
