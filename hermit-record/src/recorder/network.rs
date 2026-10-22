/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Handles poll, ppoll, epoll, and select system calls.

use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::unix::fs::FileExt;

use reverie::Errno;
use reverie::Guest;
use reverie::Pid;
use reverie::syscalls::Accept4;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Close;
use reverie::syscalls::EpollPwait;
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
use crate::event::AcceptEvent;
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

const SOCKADDR_STORAGE_LEN: usize = std::mem::size_of::<libc::sockaddr_storage>();
const USER_PAGE_SIZE: usize = 4096;

fn accepted_fd(fd: i64) -> i32 {
    i32::try_from(fd).expect("accept4 returned a descriptor beyond i32")
}

/// What an emulated `move_addr_to_user` wrote, and the error it returned.
#[derive(Debug, PartialEq, Eq)]
struct PeerAddressCopyOut {
    error: Option<Errno>,
    addr: Vec<u8>,
    addr_len: Option<libc::socklen_t>,
}

/// Copies an accepted peer address out to the guest the way Linux 7.1's
/// `move_addr_to_user` does once the connection has been taken: read the
/// capacity with `read_user` and clamp it to the address length; unless the
/// result is negative, write the full length back first; reject a negative
/// result with `EINVAL`; then copy the address prefix. Every guest access obeys
/// the guest's page permissions. A length that cannot be written fails with
/// `EFAULT` before any address byte is copied, and an address that faults part
/// way fails with `EFAULT` after the length and whatever prefix was already
/// written. Linux 7.0 and earlier copy the address before writing the length,
/// so on those kernels the two faults leave different bytes behind; record
/// deliberately follows the 7.1 order on every host.
fn copy_peer_address_out<M: MemoryAccess>(
    memory: &mut M,
    read_user: impl FnOnce(&M, AddrMut<u8>, &mut [u8]) -> bool,
    sockaddr: AddrMut<u8>,
    addrlen: Option<AddrMut<u8>>,
    address: &[u8],
    kernel_len: libc::socklen_t,
) -> PeerAddressCopyOut {
    let failed = |error, addr, addr_len| PeerAddressCopyOut {
        error: Some(error),
        addr,
        addr_len,
    };
    let Some(addrlen) = addrlen else {
        return failed(Errno::EFAULT, Vec::new(), None);
    };
    let mut capacity = [0; std::mem::size_of::<libc::c_int>()];
    if !read_user(memory, addrlen, &mut capacity) {
        return failed(Errno::EFAULT, Vec::new(), None);
    }
    let len = libc::c_int::from_ne_bytes(capacity).min(kernel_len as libc::c_int);
    let Ok(len) = usize::try_from(len) else {
        return failed(Errno::EINVAL, Vec::new(), None);
    };
    if !write_user_word(memory, addrlen, &capacity, &kernel_len.to_ne_bytes()) {
        return failed(Errno::EFAULT, Vec::new(), None);
    }
    let wanted = &address[..len.min(address.len())];
    let written = write_user_prefix(memory, sockaddr, wanted);
    if written < wanted.len() {
        return failed(Errno::EFAULT, wanted[..written].to_vec(), Some(kernel_len));
    }
    PeerAddressCopyOut {
        error: None,
        addr: wanted.to_vec(),
        addr_len: Some(kernel_len),
    }
}

/// Reads guest memory with the access the kernel's `get_user` has on x86, where
/// every writable user page is also readable. `process_vm_readv` needs a
/// readable mapping, so a range it refuses is read through `/proc/<pid>/mem`,
/// exactly, if every page of it lies in a readable or writable mapping.
/// Execute-only mappings count as unreadable, as they are on hardware with
/// protection keys, and protection keys the guest assigns itself are not
/// consulted. Like the kernel's read, this races with another thread changing
/// the mappings, and either order is a result Linux can give.
fn read_user_bytes<M: MemoryAccess>(
    memory: &M,
    pid: Pid,
    address: AddrMut<u8>,
    buf: &mut [u8],
) -> bool {
    if memory.read_exact_with_user_access(address, buf).is_ok() {
        return true;
    }
    let start = address.as_raw();
    if start.checked_add(buf.len()).is_none() {
        return false;
    }
    let Ok(maps) = std::fs::read_to_string(format!("/proc/{pid}/maps")) else {
        return false;
    };
    user_page_chunks(start, buf.len())
        .iter()
        .all(|&(offset, _)| mapping_is_user_readable(&maps, start + offset))
        && std::fs::File::open(format!("/proc/{pid}/mem"))
            .and_then(|mem| mem.read_exact_at(buf, start as u64))
            .is_ok()
}

/// Whether `address` lies in a mapping of a `/proc/<pid>/maps` listing that
/// the kernel can read from user space on x86: one that is readable or
/// writable.
fn mapping_is_user_readable(maps: &str, address: usize) -> bool {
    maps.lines().any(|line| {
        let mut fields = line.split_whitespace();
        let (Some(range), Some(perms)) = (fields.next(), fields.next()) else {
            return false;
        };
        let Some((start, end)) = range.split_once('-') else {
            return false;
        };
        let (Ok(start), Ok(end)) = (
            usize::from_str_radix(start, 16),
            usize::from_str_radix(end, 16),
        ) else {
            return false;
        };
        (start..end).contains(&address) && matches!(perms.as_bytes(), [b'r', ..] | [_, b'w', ..])
    })
}

/// The peer address Linux's `accept` copies out for the new socket `socket`,
/// and its full length, or `None` if Linux could not name the peer, which
/// makes `accept` fail with `ECONNABORTED`. Linux asks the socket with
/// `getname(.., 2)`. `getpeername` asks with 1, which on IPv4 and IPv6 also
/// refuses a socket the peer has already reset, so for those `SO_PEERNAME`,
/// which asks with 2, answers instead. It wants exactly the family's address
/// size.
fn accepted_peer_address(
    socket: BorrowedFd,
) -> std::io::Result<Option<(Vec<u8>, libc::socklen_t)>> {
    let mut address = [0u8; SOCKADDR_STORAGE_LEN];
    let mut len = SOCKADDR_STORAGE_LEN as libc::socklen_t;
    // SAFETY: `address` holds `len` bytes.
    if unsafe { libc::getpeername(socket.as_raw_fd(), address.as_mut_ptr().cast(), &mut len) } == 0
    {
        return Ok(Some((
            address[..(len as usize).min(SOCKADDR_STORAGE_LEN)].to_vec(),
            len,
        )));
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() != Some(libc::ENOTCONN) {
        return Err(error);
    }
    let mut domain = [0; std::mem::size_of::<libc::c_int>()];
    socket_option(socket, libc::SO_DOMAIN, &mut domain)?;
    let len = match libc::c_int::from_ne_bytes(domain) {
        libc::AF_INET => std::mem::size_of::<libc::sockaddr_in>(),
        libc::AF_INET6 => std::mem::size_of::<libc::sockaddr_in6>(),
        _ => return Ok(None),
    };
    let mut peer = vec![0; len];
    match socket_option(socket, libc::SO_PEERNAME, &mut peer) {
        Ok(()) => Ok(Some((peer, len as libc::socklen_t))),
        Err(error) if error.raw_os_error() == Some(libc::ENOTCONN) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Reads the `SOL_SOCKET` option `name` into all of `value`.
fn socket_option(socket: BorrowedFd, name: libc::c_int, value: &mut [u8]) -> std::io::Result<()> {
    let mut len = value.len() as libc::socklen_t;
    // SAFETY: `value` holds `len` bytes.
    let result = unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            name,
            value.as_mut_ptr().cast(),
            &mut len,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// The page-sized pieces of `[address, address + len)`, in address order.
fn user_page_chunks(address: usize, len: usize) -> Vec<(usize, usize)> {
    let mut chunks = Vec::new();
    let mut offset = 0;
    while offset < len {
        let at = address.wrapping_add(offset);
        let size = (USER_PAGE_SIZE - at % USER_PAGE_SIZE).min(len - offset);
        chunks.push((offset, size));
        offset += size;
    }
    chunks
}

fn write_user_chunk<M: MemoryAccess>(memory: &mut M, address: usize, bytes: &[u8]) -> usize {
    let Some(address) = AddrMut::from_raw(address) else {
        return 0;
    };
    match memory.write_with_user_access(address, bytes) {
        Ok(written) => written,
        Err(Errno::EFAULT) => 0,
        Err(error) => panic!("backend cannot write guest memory with user access: {error}"),
    }
}

/// Writes the longest prefix of `bytes` that the guest's page permissions
/// allow, as `copy_to_user` does, and returns its length.
fn write_user_prefix<M: MemoryAccess>(memory: &mut M, address: AddrMut<u8>, bytes: &[u8]) -> usize {
    let mut written = 0;
    for (offset, size) in user_page_chunks(address.as_raw(), bytes.len()) {
        let chunk = &bytes[offset..offset + size];
        let copied = write_user_chunk(memory, address.as_raw() + offset, chunk);
        written += copied;
        if copied < size {
            break;
        }
    }
    written
}

/// Stores a 4-byte word all or nothing, as `put_user` does: a word that
/// straddles a page boundary is written high page first, and restored to
/// `original` if the low page then refuses the write.
fn write_user_word<M: MemoryAccess>(
    memory: &mut M,
    address: AddrMut<u8>,
    original: &[u8; 4],
    value: &[u8; 4],
) -> bool {
    let chunks = user_page_chunks(address.as_raw(), value.len());
    for (index, &(offset, size)) in chunks.iter().enumerate().rev() {
        let at = address.as_raw() + offset;
        if write_user_chunk(memory, at, &value[offset..offset + size]) < size {
            for &(offset, size) in &chunks[index + 1..] {
                let at = address.as_raw() + offset;
                write_user_chunk(memory, at, &original[offset..offset + size]);
            }
            return false;
        }
    }
    true
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
        self.record_epoll_wait(
            guest,
            Syscall::EpollWait(syscall),
            syscall.epfd(),
            syscall.events().map(|events| events.as_raw()),
        )
        .await
    }

    /// `epoll_pwait` is `epoll_wait` with a temporary signal mask, and records
    /// the same event. It used to run unrecorded, so replay ran it live
    /// against descriptors replay does not refill.
    pub(super) async fn handle_epoll_pwait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: EpollPwait,
    ) -> Result<i64, Errno> {
        self.record_epoll_wait(
            guest,
            Syscall::EpollPwait(syscall),
            syscall.epfd(),
            syscall.events().map(|events| events.as_raw()),
        )
        .await
    }

    /// `epoll_pwait2` is `epoll_pwait` with a timespec timeout. The pinned
    /// Reverie has no typed variant, so it arrives as `Syscall::Other`; it
    /// records the event `epoll_wait` records.
    pub(super) async fn handle_epoll_pwait2<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let Syscall::Other(_, args) = syscall else {
            unreachable!("handle_epoll_pwait2 called for {syscall:?}");
        };
        let events = (args.arg1 != 0).then_some(args.arg1);
        self.record_epoll_wait(guest, syscall, args.arg0 as libc::c_int, events)
            .await
    }

    /// Runs an `epoll_wait`-family `syscall` and records the events it wrote
    /// to `events`.
    async fn record_epoll_wait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
        epfd: libc::c_int,
        events: Option<usize>,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;

        let event = result.and_then(|ret| {
            let updated = ret as usize;
            let mut bytes = vec![0; updated * std::mem::size_of::<libc::epoll_event>()];
            if !bytes.is_empty() {
                let address = events.and_then(Addr::<u8>::from_raw).ok_or(Errno::EFAULT)?;
                guest.memory().read_exact(address, &mut bytes)?;
            }
            Ok(SyscallEvent::EpollWait(EpollWaitEvent {
                events: bytes,
                updated,
                replay_kernel_side_effect: self
                    .epoll_requires_replay_kernel_side_effect(guest.pid(), epfd),
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

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-3871): Record accepted connections for replay.
    pub(super) async fn handle_accept4<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Accept4,
    ) -> Result<i64, Errno> {
        // Without an address buffer Linux writes nothing but the descriptor,
        // and ignores `addrlen`.
        let Some(sockaddr) = syscall.sockaddr() else {
            let result = guest.inject(syscall).await;
            let event = result.map(|fd| {
                SyscallEvent::Accept(AcceptEvent {
                    result: Ok(accepted_fd(fd)),
                    addr: Vec::new(),
                    addr_len: None,
                })
            });
            self.record_event(guest, event);
            return result;
        };

        // Only after Linux has taken the connection off the queue does it read
        // the capacity in `*addrlen`, write the full length back and copy the
        // address out. Another thread can change or unmap both buffers while
        // accept4 waits, and a fault in the copy-out leaves the earlier writes
        // in place while the call fails. So accept without an address, ask the
        // new socket for its peer, and perform that copy-out here, recording
        // exactly the memory the guest saw change. No guest memory is borrowed
        // for this, so the guest's stack and buffers are never touched beyond
        // what Linux writes.
        let fd = match guest
            .inject(syscall.with_sockaddr(None).with_addrlen(None))
            .await
        {
            Ok(fd) => fd,
            Err(error) => {
                self.record_event(guest, Err(error));
                return Err(error);
            }
        };

        // Only a guest thread closing or replacing a descriptor number it was
        // never given could take the new socket away before this, so failing
        // to inspect it is treated as a Hermit failure, not a guest error.
        // The socket is in the accepting thread's table, which is not the
        // leader's when the thread was cloned without CLONE_FILES.
        let pid = guest.pid();
        let peer = crate::fd::duplicate_guest_thread_fd(pid, guest.tid(), accepted_fd(fd))
            .and_then(|socket| accepted_peer_address(socket.as_fd()))
            .unwrap_or_else(|error| {
                panic!("accept4 returned fd {fd}, but its peer address could not be read: {error}")
            });
        let copy_out = match peer {
            Some((address, kernel_len)) => copy_peer_address_out(
                &mut guest.memory(),
                |memory, address, buf| read_user_bytes(memory, pid, address, buf),
                sockaddr.cast(),
                syscall.addrlen().map(|address| address.cast()),
                &address,
                kernel_len,
            ),
            None => PeerAddressCopyOut {
                error: Some(Errno::ECONNABORTED),
                addr: Vec::new(),
                addr_len: None,
            },
        };
        let result = match copy_out.error {
            None => Ok(accepted_fd(fd)),
            Some(error) => {
                // Linux releases the new file when naming the peer or the
                // copy-out fails, so the descriptor is never installed for the
                // guest. Detcore has not seen it either, because the call
                // returns an error. A pending signal, such as the client's
                // SIGCHLD, can interrupt this extra syscall before it runs, so
                // retry it: Linux's close(2) itself never reports ERESTARTSYS.
                let close = Close::new().with_fd(accepted_fd(fd));
                if let Err(close_error) = guest.inject_with_retry(close).await {
                    panic!(
                        "could not close fd {fd} after its accept4 copy-out failed: {close_error}"
                    );
                }
                Err(error)
            }
        };

        self.record_event(
            guest,
            Ok(SyscallEvent::Accept(AcceptEvent {
                result,
                addr: copy_out.addr,
                addr_len: copy_out.addr_len,
            })),
        );

        result.map(i64::from)
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

    /// Guest memory made of whole pages, each readable and writable or not, so
    /// that a test can place a buffer across a permission boundary.
    struct PagedMemory {
        base: usize,
        bytes: Vec<u8>,
        readable: Vec<bool>,
        writable: Vec<bool>,
    }

    impl PagedMemory {
        const BASE: usize = 0x10_0000;

        fn new(pages: usize) -> Self {
            Self {
                base: Self::BASE,
                bytes: vec![0xa5; pages * USER_PAGE_SIZE],
                readable: vec![true; pages],
                writable: vec![true; pages],
            }
        }

        fn page_of(&self, address: usize) -> Option<usize> {
            let offset = address.checked_sub(self.base)?;
            (offset < self.bytes.len()).then_some(offset / USER_PAGE_SIZE)
        }

        fn at(address: usize) -> AddrMut<'static, u8> {
            AddrMut::from_raw(address).unwrap()
        }

        fn slice(&self, address: usize, len: usize) -> &[u8] {
            &self.bytes[address - self.base..][..len]
        }

        fn put(&mut self, address: usize, bytes: &[u8]) {
            let offset = address - self.base;
            self.bytes[offset..offset + bytes.len()].copy_from_slice(bytes);
        }

        /// Copies byte by byte up to the first byte `allowed` refuses.
        fn transfer(&self, address: usize, len: usize, allowed: &[bool]) -> usize {
            (0..len)
                .take_while(|&index| {
                    self.page_of(address + index)
                        .is_some_and(|page| allowed[page])
                })
                .count()
        }
    }

    impl MemoryAccess for PagedMemory {
        fn read_vectored(
            &self,
            read_from: &[std::io::IoSlice],
            write_to: &mut [std::io::IoSliceMut],
        ) -> Result<usize, Errno> {
            let (remote, local) = (&read_from[0], &mut write_to[0]);
            let address = remote.as_ptr() as usize;
            let count = self.transfer(address, remote.len().min(local.len()), &self.readable);
            if count == 0 && !local.is_empty() {
                return Err(Errno::EFAULT);
            }
            local[..count].copy_from_slice(self.slice(address, count));
            Ok(count)
        }

        fn write_vectored(
            &mut self,
            _read_from: &[std::io::IoSlice],
            _write_to: &mut [std::io::IoSliceMut],
        ) -> Result<usize, Errno> {
            panic!("the accept4 copy-out must use user-access writes only")
        }

        fn write_with_user_access(
            &mut self,
            address: AddrMut<u8>,
            bytes: &[u8],
        ) -> Result<usize, Errno> {
            let address = address.as_raw();
            let count = self.transfer(address, bytes.len(), &self.writable);
            if count == 0 && !bytes.is_empty() {
                return Err(Errno::EFAULT);
            }
            self.put(address, &bytes[..count]);
            Ok(count)
        }
    }

    const PEER: [u8; 16] = [2, 0, 0x1f, 0x90, 127, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0];
    const PEER_LEN: libc::socklen_t = PEER.len() as libc::socklen_t;

    fn copy_out(
        memory: &mut PagedMemory,
        sockaddr: usize,
        addrlen: Option<usize>,
        capacity: libc::c_int,
    ) -> PeerAddressCopyOut {
        if let Some(addrlen) = addrlen {
            memory.put(addrlen, &capacity.to_ne_bytes());
        }
        copy_peer_address_out(
            memory,
            |memory, address, buf| memory.read_exact_with_user_access(address, buf).is_ok(),
            PagedMemory::at(sockaddr),
            addrlen.map(PagedMemory::at),
            &PEER,
            PEER_LEN,
        )
    }

    #[test]
    fn accept_copy_out_truncates_to_the_capacity_and_reports_the_full_length() {
        let base = PagedMemory::BASE;
        for (capacity, copied) in [(16, 16), (128, 16), (4, 4), (0, 0)] {
            let mut memory = PagedMemory::new(1);
            let result = copy_out(&mut memory, base, Some(base + 256), capacity);
            assert_eq!(
                result,
                PeerAddressCopyOut {
                    error: None,
                    addr: PEER[..copied].to_vec(),
                    addr_len: Some(PEER_LEN),
                }
            );
            assert_eq!(memory.slice(base, copied), &PEER[..copied]);
            assert_eq!(
                memory.slice(base + copied, 1),
                &[0xa5],
                "capacity {capacity}"
            );
            assert_eq!(memory.slice(base + 256, 4), &PEER_LEN.to_ne_bytes());
        }
    }

    #[test]
    fn accept_copy_out_rejects_a_negative_capacity_without_writing() {
        let base = PagedMemory::BASE;
        let mut memory = PagedMemory::new(1);
        let result = copy_out(&mut memory, base, Some(base + 256), -1);
        assert_eq!(result.error, Some(Errno::EINVAL));
        assert_eq!((result.addr.len(), result.addr_len), (0, None));
        assert_eq!(memory.slice(base, 1), &[0xa5]);
        assert_eq!(memory.slice(base + 256, 4), &(-1i32).to_ne_bytes());
    }

    #[test]
    fn accept_copy_out_faults_on_a_missing_or_unreadable_addrlen() {
        let base = PagedMemory::BASE;
        let mut memory = PagedMemory::new(2);
        let missing = copy_out(&mut memory, base, None, 0);
        assert_eq!(missing.error, Some(Errno::EFAULT));

        memory.readable[1] = false;
        let unreadable = copy_out(&mut memory, base, Some(base + USER_PAGE_SIZE + 8), 16);
        assert_eq!(unreadable.error, Some(Errno::EFAULT));
        for result in [missing, unreadable] {
            assert_eq!((result.addr.len(), result.addr_len), (0, None));
        }
        assert_eq!(memory.slice(base, 1), &[0xa5]);
    }

    #[test]
    fn accept_copy_out_writes_the_length_and_keeps_the_prefix_before_a_fault() {
        let base = PagedMemory::BASE;
        let mut memory = PagedMemory::new(2);
        memory.writable[1] = false;
        let sockaddr = base + USER_PAGE_SIZE - 6;
        let result = copy_out(&mut memory, sockaddr, Some(base + 64), 128);
        assert_eq!(
            result,
            PeerAddressCopyOut {
                error: Some(Errno::EFAULT),
                addr: PEER[..6].to_vec(),
                addr_len: Some(PEER_LEN),
            }
        );
        assert_eq!(memory.slice(sockaddr, 6), &PEER[..6]);
        assert_eq!(memory.slice(sockaddr + 6, 1), &[0xa5]);
        assert_eq!(memory.slice(base + 64, 4), &PEER_LEN.to_ne_bytes());
    }

    #[test]
    fn accept_copy_out_writes_a_straddling_addrlen_all_or_nothing_before_the_address() {
        let base = PagedMemory::BASE;
        let addrlen = base + USER_PAGE_SIZE - 2;
        for read_only_page in [0, 1] {
            let mut memory = PagedMemory::new(2);
            memory.put(addrlen, &16i32.to_ne_bytes());
            memory.writable[read_only_page] = false;
            let sockaddr = if read_only_page == 0 {
                base + USER_PAGE_SIZE + 64
            } else {
                base
            };
            let result = copy_out(&mut memory, sockaddr, Some(addrlen), 16);
            assert_eq!(
                result,
                PeerAddressCopyOut {
                    error: Some(Errno::EFAULT),
                    addr: Vec::new(),
                    addr_len: None,
                },
                "read-only page {read_only_page}"
            );
            assert_eq!(memory.slice(sockaddr, 1), &[0xa5]);
            assert_eq!(memory.slice(addrlen, 4), &16i32.to_ne_bytes());
        }

        let mut memory = PagedMemory::new(2);
        let result = copy_out(&mut memory, base, Some(addrlen), 16);
        assert_eq!(result.error, None);
        assert_eq!(memory.slice(addrlen, 4), &PEER_LEN.to_ne_bytes());
    }

    /// Maps `pages` private anonymous pages, each with its own protection.
    fn map_pages(protections: &[libc::c_int]) -> *mut u8 {
        let len = protections.len() * USER_PAGE_SIZE;
        // SAFETY: a fresh anonymous mapping that nothing else refers to.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(base, libc::MAP_FAILED);
        let base = base.cast::<u8>();
        for (page, &protection) in protections.iter().enumerate() {
            // SAFETY: inside the mapping made above.
            unsafe {
                let at = base.add(page * USER_PAGE_SIZE);
                std::ptr::write_bytes(at, page as u8 + 1, USER_PAGE_SIZE);
                assert_eq!(libc::mprotect(at.cast(), USER_PAGE_SIZE, protection), 0);
            }
        }
        base
    }

    #[test]
    fn a_user_read_reaches_readable_and_write_only_pages_but_not_inaccessible_ones() {
        let page = USER_PAGE_SIZE;
        let base = map_pages(&[libc::PROT_READ, libc::PROT_WRITE, libc::PROT_NONE]) as usize;
        let pid = Pid::this();
        let read = |address: usize| {
            let mut word = [0; 4];
            read_user_bytes(
                &LocalMemory::new(),
                pid,
                PagedMemory::at(address),
                &mut word,
            )
            .then_some(word)
        };
        assert_eq!(read(base + 8), Some([1; 4]), "readable");
        assert_eq!(read(base + page + 8), Some([2; 4]), "write-only");
        assert_eq!(
            read(base + 2 * page - 4),
            Some([2; 4]),
            "write-only page end"
        );
        assert_eq!(
            read(base + page - 2),
            Some([1, 1, 2, 2]),
            "readable into write-only"
        );
        assert_eq!(read(base + 2 * page + 8), None, "inaccessible");
        assert_eq!(
            read(base + 2 * page - 2),
            None,
            "write-only into inaccessible"
        );
        // SAFETY: the mapping made above, no longer referenced.
        assert_eq!(unsafe { libc::munmap(base as *mut _, 3 * page) }, 0);
    }

    #[test]
    fn a_mapping_is_user_readable_if_it_is_readable_or_writable() {
        let maps = "\
            1000-2000 r--p 00000000 00:00 0\n\
            2000-3000 -w-p 00000000 00:00 0\n\
            3000-4000 ---p 00000000 00:00 0\n\
            4000-5000 --xp 00000000 00:00 0\n\
            6000-7000 rw-p 00000000 00:00 0 [stack]\n";
        for (address, readable) in [
            (0x1000, true),
            (0x1fff, true),
            (0x2000, true),
            (0x2fff, true),
            (0x3000, false),
            (0x4000, false),
            (0x5000, false),
            (0x6ffc, true),
            (0x7000, false),
            (0x0fff, false),
        ] {
            assert_eq!(
                mapping_is_user_readable(maps, address),
                readable,
                "{address:#x}"
            );
        }
    }

    fn loopback_listener() -> std::net::TcpListener {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap()
    }

    #[test]
    fn an_accepted_tcp_socket_names_its_peer() {
        let listener = loopback_listener();
        let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        let (address, len) = accepted_peer_address(server.as_fd()).unwrap().unwrap();
        let port = client.local_addr().unwrap().port().to_be_bytes();
        assert_eq!(len, 16);
        assert_eq!(address[..8], [2, 0, port[0], port[1], 127, 0, 0, 1]);
    }

    #[test]
    fn an_accepted_tcp_socket_the_peer_reset_still_names_it_as_accept_does() {
        let listener = loopback_listener();
        let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let port = client.local_addr().unwrap().port().to_be_bytes();
        let linger = libc::linger {
            l_onoff: 1,
            l_linger: 0,
        };
        // SAFETY: `linger` is a valid option value of its own size.
        let set = unsafe {
            libc::setsockopt(
                client.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_LINGER,
                std::ptr::from_ref(&linger).cast(),
                std::mem::size_of::<libc::linger>() as libc::socklen_t,
            )
        };
        assert_eq!(set, 0);
        drop(client);
        let (server, from) = listener.accept().unwrap();
        assert_eq!(
            server.peer_addr().unwrap_err().raw_os_error(),
            Some(libc::ENOTCONN),
            "getpeername refuses a reset socket, so this exercises SO_PEERNAME"
        );
        let (address, len) = accepted_peer_address(server.as_fd()).unwrap().unwrap();
        assert_eq!(from.port().to_be_bytes(), port);
        assert_eq!(len, 16);
        assert_eq!(address[..8], [2, 0, port[0], port[1], 127, 0, 0, 1]);
    }

    #[test]
    fn a_connected_unix_socket_names_its_unbound_peer() {
        let (server, _client) = std::os::unix::net::UnixStream::pair().unwrap();
        let (address, len) = accepted_peer_address(server.as_fd()).unwrap().unwrap();
        let family = (libc::AF_UNIX as libc::sa_family_t).to_ne_bytes();
        assert_eq!((len, address), (2, family.to_vec()));
    }

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
