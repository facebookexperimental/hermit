/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-3464): Review external network record and replay.

//! External network record and replay for outbound TCP clients.
//!
//! With a network trace mode configured, an outbound TCP `connect` becomes a
//! channel of the [`NetworkEngine`] held in global state, and every later
//! operation on that socket is answered by the engine:
//!
//! - Record mode connects for real, pulls whatever the host has received with
//!   a nonblocking `recv` at each guest check, and hands it to the engine,
//!   which stamps it with the global time of that check.
//! - Replay mode never touches the host network. The engine releases each
//!   recorded input once global time and the guest's own transmissions reach
//!   the recorded point, and compares every outbound byte with the recording.
//!
//! A pull and the check it serves travel in one request, so the engine
//! observes both at the same global time. Blocking operations retry on
//! ordinary polling turns, which advance global time, so the release point of
//! every input is reached in replay under any schedule.
//!
//! Host waits that the guest cannot observe as time passing -- the connect
//! handshake and a full send buffer -- block the thread without yielding, so
//! record and replay take the same scheduler turns. They are wall-clock
//! waits of up to a minute each, unlike the virtual-time stall limit replay
//! applies.
//!
//! Anything the engine cannot answer ends the run with the policy-refusal
//! status, naming the reason and the remedy; replay never falls back to the
//! host. That includes every operation that could reach the network outside a
//! channel, because a recording shares the host's network namespace:
//!
//! - on an IPv4 or IPv6 socket, a UDP send, `bind`, `listen`, a TCP Fast Open
//!   send, `epoll` registration, signal-driven I/O, and a nonzero send or
//!   receive timeout, which Linux would apply in wall-clock time;
//! - a socket of any family but `AF_UNIX`, `AF_INET` and `AF_INET6`, such as
//!   netlink or packet sockets, and a raw IPv4 or IPv6 socket;
//! - an abstract `AF_UNIX` address, which names the network namespace rather
//!   than the file system;
//! - receiving a socket of another family than `AF_UNIX` through
//!   `SCM_RIGHTS`, which this module could not classify;
//! - the interface and route ioctls (`SIOCGIFINDEX`, `SIOCGIFCONF`, ...) on a
//!   socket of any family;
//! - opening `/proc/net`, `/proc/sys/net`, their per-process spellings, or a
//!   network interface under `/sys`.
//!
//! Creating an IPv4 or IPv6 socket and closing it stays allowed, because
//! resolvers probe for IPv6 support that way. The checks are the same in both
//! modes.
//!
//! A recording still runs in the host's network namespace, and some routes to
//! that namespace's state remain open: `stat` and `readlink` on the paths
//! above, an implicit abstract `AF_UNIX` autobind under `SO_PASSCRED`, and any
//! host state a program reaches through a file this module does not know.
//! A program that takes one of them can record a run that replays
//! differently or refuses.

use std::ffi::OsStr;
use std::ops::RangeInclusive;
use std::os::unix::io::RawFd;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use detcore_model::HERMIT_POLICY_REFUSAL_EXIT;
use detcore_model::fd::OpenFileId;
use detcore_model::network_engine::NetworkArrival;
use detcore_model::network_engine::NetworkEngineError;
use detcore_model::network_engine::NetworkRecvOutcome;
use detcore_model::network_engine::NetworkReply;
use detcore_model::network_engine::NetworkRequest;
use detcore_model::network_trace::NetworkAddressV1;
use detcore_model::network_trace::NetworkTraceMode;
use reverie::Errno;
use reverie::Error;
use reverie::Guest;
use reverie::Stack;
use reverie::syscalls;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::FcntlCmd;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::ioctl::Request;
use reverie::syscalls::linux_abi::Cmsghdr;
use reverie::syscalls::linux_abi::Mmsghdr;
use reverie::syscalls::linux_abi::Msghdr;

use super::io::PSELECT6_INTERNAL_MAX_NFDS;
use super::io::Pselect6SigmaskArg;
use super::io::ppoll_timeout_duration;
use super::io::pselect6_fd_set_len;
use super::io::read_pselect6_fd_set;
use super::io::select_timeout_duration;
use super::io::write_pselect6_fd_set;
use crate::fd::FdType;
use crate::fd::NetworkSocketKind;
use crate::record_or_replay::RecordOrReplay;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::syscalls::helpers::NonblockableSyscall;
use crate::syscalls::helpers::get_fd;
use crate::syscalls::helpers::millis_duration_to_absolute_timeout;
use crate::syscalls::helpers::poll_nfds;
use crate::syscalls::helpers::record_retry_event;
use crate::tool_global::GlobalRequest;
use crate::tool_global::GlobalResponse;
use crate::tool_global::ResumeStatus;
use crate::tool_global::resource_request;
use crate::tool_global::send_and_update_time;
use crate::tool_global::thread_observe_time;
use crate::tool_global::unrecoverable_shutdown;
use crate::tool_local::Detcore;
use crate::types::LogicalTime;

/// How long record mode waits for the host to finish a TCP handshake.
const CONNECT_WAIT_MILLIS: i32 = 60_000;

/// Size of the scratch buffer a readiness check pulls into.
const POLL_PULL_BYTES: usize = 512;

/// A polled channel and its receive low-water mark.
type PolledChannel = (OpenFileId, usize);

// A `select` naming a channel probes its other descriptors through a pollfd
// array in the pull scratch, one entry per descriptor below `nfds`.
const _: () = assert!(
    PSELECT6_INTERNAL_MAX_NFDS as usize * std::mem::size_of::<libc::pollfd>() <= POLL_PULL_BYTES
);

/// The poll events Linux's `select` reports as readable, writable and
/// exceptional (`POLLIN_SET`, `POLLOUT_SET` and `POLLEX_SET` in
/// `fs/select.c`). Newer kernels include `POLLNVAL` in all three, so a
/// descriptor closed during the wait reports ready in every set it is in;
/// older ones ignore such a descriptor (no bit, no error).
const SELECT_REPORTED: [i16; 3] = [
    libc::POLLRDNORM
        | libc::POLLRDBAND
        | libc::POLLIN
        | libc::POLLHUP
        | libc::POLLERR
        | libc::POLLNVAL,
    libc::POLLWRBAND | libc::POLLWRNORM | libc::POLLOUT | libc::POLLERR | libc::POLLNVAL,
    libc::POLLPRI | libc::POLLNVAL,
];

/// The poll events a descriptor in each `select` set asks for. `POLLERR` and
/// `POLLHUP` need no request.
const SELECT_REQUESTED: [i16; 3] = [
    libc::POLLRDNORM | libc::POLLRDBAND | libc::POLLIN,
    libc::POLLWRBAND | libc::POLLWRNORM | libc::POLLOUT,
    libc::POLLPRI,
];

/// Whether a `poll` has an entry to report.
fn poll_ready(pollfds: &[libc::pollfd]) -> bool {
    pollfds.iter().any(|pollfd| pollfd.revents != 0)
}

/// For each of `select`'s three sets, whether `pollfd`, built from those sets,
/// reports ready in it.
fn select_bits(pollfd: &libc::pollfd) -> [bool; 3] {
    std::array::from_fn(|set| {
        pollfd.events & SELECT_REQUESTED[set] != 0 && pollfd.revents & SELECT_REPORTED[set] != 0
    })
}

/// Whether a `select` has a bit to report.
fn select_ready(pollfds: &[libc::pollfd]) -> bool {
    pollfds
        .iter()
        .any(|pollfd| select_bits(pollfd).contains(&true))
}

/// The pollfd entries for `select`'s descriptor sets: one per descriptor below
/// `nfds` named in any set, asking for that set's events.
fn select_pollfds(nfds: usize, sets: &[Option<Vec<u8>>; 3]) -> Vec<libc::pollfd> {
    (0..nfds)
        .filter_map(|fd| {
            let events = sets
                .iter()
                .zip(SELECT_REQUESTED)
                .filter(|(set, _)| {
                    set.as_ref()
                        .is_some_and(|set| set[fd / 8] & (1 << (fd % 8)) != 0)
                })
                .fold(0, |events, (_, requested)| events | requested);
            (events != 0).then_some(libc::pollfd {
                fd: fd as RawFd,
                events,
                revents: 0,
            })
        })
        .collect()
}

/// `select`'s result sets for `pollfds`, with `len` bytes for each set the
/// call passed, and the number of bits set across them.
fn select_result(
    pollfds: &[libc::pollfd],
    sets: &[Option<Vec<u8>>; 3],
    len: usize,
) -> ([Option<Vec<u8>>; 3], i64) {
    let mut result = sets.clone().map(|set| set.map(|_| vec![0u8; len]));
    let mut count = 0;
    for pollfd in pollfds {
        let fd = pollfd.fd as usize;
        for (set, ready) in result.iter_mut().zip(select_bits(pollfd)) {
            if let (Some(set), true) = (set, ready) {
                set[fd / 8] |= 1 << (fd % 8);
                count += 1;
            }
        }
    }
    (result, count)
}

/// `recv` flags with a modelled meaning.
const RECV_FLAGS: i32 =
    libc::MSG_PEEK | libc::MSG_WAITALL | libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC;

/// `send` flags with a modelled meaning.
const SEND_FLAGS: i32 = libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT | libc::MSG_MORE;

/// The shortest IPv6 `sockaddr` Linux accepts: RFC 2133's, without
/// `sin6_scope_id` (`SIN6_LEN_RFC2133`).
const SOCKADDR_IN6_RFC2133_LEN: usize = 24;

/// The longest `sockaddr` Linux copies in (`sizeof(struct sockaddr_storage)`).
const SOCKADDR_STORAGE_LEN: usize = 128;

/// Bytes of an `SCM_RIGHTS` control buffer this module inspects; Linux caps
/// ancillary data at `net.core.optmem_max`, which defaults to far less.
const MAX_CONTROL_BYTES: usize = 64 * 1024;

/// `SO_RCVTIMEO_OLD`, `SO_SNDTIMEO_OLD`, `SO_RCVTIMEO_NEW` and
/// `SO_SNDTIMEO_NEW` on x86_64.
const TIMEOUT_OPTIONS: [i32; 4] = [20, 21, 66, 67];

/// The remedy for an operation network record/replay does not model.
const UNSUPPORTED_REMEDY: &str = "Network record/replay supports only outbound TCP clients. To \
     let this program use the network without recording it, run it without \
     --record-networking or --replay-networking, with --network=host and without --strict.";

/// The remedy for an operation that would reach the host network namespace.
const NAMESPACE_REMEDY: &str = "Network record/replay supports only outbound TCP clients and \
     AF_UNIX sockets bound to file-system paths. To let this program use the network without \
     recording it, run it without --record-networking or --replay-networking, with \
     --network=host and without --strict.";

/// The remedy for a record run that the host network failed.
const HOST_REMEDY: &str = "Check that the peer is reachable and responding, then record again.";

/// Whether `getsockopt` on a channel may read the host socket in both modes:
/// options that report the socket's configuration, which the guest set or
/// Linux fixes, rather than the state of the connection.
fn is_configuration_option(level: i32, name: i32) -> bool {
    match level {
        // A timeout reads back as zero: a nonzero one is refused.
        libc::SOL_SOCKET => matches!(
            name,
            libc::SO_TYPE
                | libc::SO_DOMAIN
                | libc::SO_PROTOCOL
                | libc::SO_RCVLOWAT
                | libc::SO_KEEPALIVE
                | libc::SO_REUSEADDR
                | libc::SO_REUSEPORT
                | libc::SO_LINGER
                | libc::SO_ACCEPTCONN
                | libc::SO_BROADCAST
                | libc::SO_OOBINLINE
                | libc::SO_RCVTIMEO
                | libc::SO_SNDTIMEO
        ),
        libc::IPPROTO_TCP => matches!(
            name,
            libc::TCP_NODELAY
                | libc::TCP_CORK
                | libc::TCP_KEEPIDLE
                | libc::TCP_KEEPINTVL
                | libc::TCP_KEEPCNT
                | libc::TCP_USER_TIMEOUT
        ),
        _ => false,
    }
}

/// Whether `call`, naming a channel, may take its ordinary path: it closes
/// the descriptor, changes or reads descriptor flags or file metadata, or
/// uses it only as the directory of a path lookup, which a socket fails or
/// ignores. Everything else on a channel is answered by the engine or
/// refused.
fn channel_call_takes_ordinary_path(call: &Syscall) -> bool {
    matches!(
        call,
        Syscall::Close(_)
            | Syscall::Fcntl(_)
            | Syscall::Fstat(_)
            | Syscall::Fstatfs(_)
            | Syscall::Openat(_)
            | Syscall::Mkdirat(_)
            | Syscall::Mknodat(_)
            | Syscall::Fchownat(_)
            | Syscall::Futimesat(_)
            | Syscall::Newfstatat(_)
            | Syscall::Unlinkat(_)
            | Syscall::Readlinkat(_)
            | Syscall::Fchmodat(_)
            | Syscall::Faccessat(_)
            | Syscall::NameToHandleAt(_)
            | Syscall::Execveat(_)
            | Syscall::Statx(_)
            | Syscall::Symlinkat(_)
            | Syscall::Utimensat(_)
    )
}

/// Whether `call`, naming an IPv4 or IPv6 socket of `kind` that is not a
/// channel, could reach the network unrecorded and must be refused.
fn reaches_network_outside_channel(call: &Syscall, kind: NetworkSocketKind) -> bool {
    let fast_open = |flags: i32| flags & libc::MSG_FASTOPEN != 0;
    match kind {
        NetworkSocketKind::NotInet => false,
        NetworkSocketKind::InetOther => matches!(
            call,
            Syscall::Bind(_)
                | Syscall::Listen(_)
                | Syscall::Sendto(_)
                | Syscall::Sendmsg(_)
                | Syscall::Sendmmsg(_)
        ),
        NetworkSocketKind::InetStream { .. } => match call {
            Syscall::Bind(_) | Syscall::Listen(_) => true,
            Syscall::Sendto(c) => fast_open(c.flags() as i32),
            Syscall::Sendmsg(c) => fast_open(c.flags()),
            Syscall::Sendmmsg(c) => fast_open(c.flags()),
            Syscall::Setsockopt(c) => {
                c.level() == libc::IPPROTO_TCP && c.optname() == libc::TCP_FASTOPEN_CONNECT
            }
            _ => false,
        },
    }
}

/// Whether `call` asks for signal-driven I/O, whose `SIGIO` Linux would send
/// when the host socket becomes ready, a moment replay cannot reproduce.
fn requests_signal_driven_io(call: &Syscall) -> bool {
    match call {
        Syscall::Fcntl(c) => {
            matches!(
                c.cmd(),
                FcntlCmd::F_SETFL(flags) if flags & libc::O_ASYNC != 0
            ) || matches!(
                c.cmd(),
                FcntlCmd::F_SETOWN | FcntlCmd::F_SETOWN_EX(_) | FcntlCmd::F_SETSIG(_)
            )
        }
        Syscall::Ioctl(c) => matches!(
            c.request(),
            Request::FIOASYNC(_) | Request::FIOSETOWN(_) | Request::SIOCSPGRP(_)
        ),
        _ => false,
    }
}

/// The socket ioctls that read or change the host's routes and interfaces,
/// from `SIOCADDRT` through the device-private range. Linux answers the
/// interface requests (`SIOCGIFINDEX`, `SIOCGIFCONF`, `SIOCGIFADDR`, ...) on
/// a socket of any family, `AF_UNIX` included, from the caller's network
/// namespace, which a recording shares with the host and a replay does not.
const INTERFACE_IOCTLS: RangeInclusive<usize> = 0x890B..=0x89FF;

/// Whether `call` asks Linux about or changes the host's routes or
/// interfaces. `SIOCETHTOOL` is left out: Detcore already answers it with
/// `ENODEV` in every run without asking Linux.
fn requests_interface_state(call: &syscalls::Ioctl) -> bool {
    let request = call.request();
    !matches!(request, Request::SIOCETHTOOL(_)) && INTERFACE_IOCTLS.contains(&request.into_raw().0)
}

/// Whether `path`, an absolute path the guest opened, names the host's
/// network namespace state: `/proc/net` and its per-process and per-thread
/// spellings, `/proc/sys/net`, or a network interface under `/sys`. A
/// recording reads the host's interfaces, connections and settings there, a
/// replay its own.
fn names_host_network_state(path: &Path) -> bool {
    let mut components: Vec<&OsStr> = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => components.push(part),
            Component::ParentDir => {
                components.pop();
            }
            _ => {}
        }
    }
    let is_task = |name: &OsStr| {
        name == "self"
            || name == "thread-self"
            || name.to_str().is_some_and(|name| {
                !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_digit())
            })
    };
    match components.as_slice() {
        [proc, net, ..] if *proc == "proc" && *net == "net" => true,
        // The per-namespace settings, such as `net/core/somaxconn`.
        [proc, sys, net, ..] if *proc == "proc" && *sys == "sys" && *net == "net" => true,
        [proc, task, net, ..] if *proc == "proc" && is_task(task) && *net == "net" => true,
        [proc, process, tasks, thread, net, ..]
            if *proc == "proc"
                && is_task(process)
                && *tasks == "task"
                && is_task(thread)
                && *net == "net" =>
        {
            true
        }
        // `/sys/class/net/<interface>` and the device directories it links
        // to, `/sys/devices/.../net/<interface>`.
        [sys, rest @ ..] if *sys == "sys" => rest.iter().any(|part| *part == "net"),
        _ => false,
    }
}

/// The traced address of an IPv4 or IPv6 `sockaddr`, or `None` for any other
/// family or a buffer shorter than Linux accepts. An IPv6 address without
/// `sin6_scope_id` has scope zero, as in Linux.
fn address_from_sockaddr(bytes: &[u8]) -> Option<NetworkAddressV1> {
    let family = u16::from_ne_bytes(bytes.get(..2)?.try_into().ok()?);
    if family == libc::AF_INET as u16 && bytes.len() >= size_of::<libc::sockaddr_in>() {
        Some(NetworkAddressV1::Inet4 {
            port: u16::from_be_bytes(bytes[2..4].try_into().ok()?),
            address: bytes[4..8].try_into().ok()?,
        })
    } else if family == libc::AF_INET6 as u16 && bytes.len() >= SOCKADDR_IN6_RFC2133_LEN {
        let scope_id = match bytes.get(24..28) {
            Some(scope_id) => u32::from_ne_bytes(scope_id.try_into().ok()?),
            None => 0,
        };
        Some(NetworkAddressV1::Inet6 {
            port: u16::from_be_bytes(bytes[2..4].try_into().ok()?),
            flowinfo: u32::from_be_bytes(bytes[4..8].try_into().ok()?),
            address: bytes[8..24].try_into().ok()?,
            scope_id,
        })
    } else {
        None
    }
}

/// Copy in a `sockaddr` as Linux's `move_addr_to_kernel` does: a length
/// beyond `sockaddr_storage` or below zero fails with `EINVAL`, and a zero
/// length copies nothing.
fn read_sockaddr<M: MemoryAccess>(
    memory: &M,
    address: Option<AddrMut<'_, libc::sockaddr>>,
    addrlen: i32,
) -> Result<Vec<u8>, Errno> {
    let length = usize::try_from(addrlen)
        .ok()
        .filter(|length| *length <= SOCKADDR_STORAGE_LEN)
        .ok_or(Errno::EINVAL)?;
    let mut bytes = vec![0; length];
    if length > 0 {
        let address = address.ok_or(Errno::EFAULT)?;
        memory.read_exact(Addr::from(address.cast::<u8>()), &mut bytes)?;
    }
    Ok(bytes)
}

/// Whether a `setsockopt` timeout value sets a timeout. Zero clears it and a
/// microsecond count out of range fails with `EDOM`, both without reaching
/// the network. A negative second count sets an immediate timeout, so a
/// blocking call fails at once with `EAGAIN`.
pub(crate) fn sets_a_timeout(timeout: libc::timeval) -> bool {
    (0..1_000_000).contains(&timeout.tv_usec) && (timeout.tv_sec != 0 || timeout.tv_usec != 0)
}

/// Whether the first `addrlen` bytes of a `sockaddr`, of which `bytes` holds
/// at least the first three, name an abstract `AF_UNIX` address, which lives
/// in the network namespace. With `binding`, a bare family also does: `bind`
/// then picks an abstract name itself.
fn names_abstract_unix_address(bytes: &[u8], addrlen: usize, binding: bool) -> bool {
    let Some(family) = bytes.get(..2) else {
        return false;
    };
    if u16::from_ne_bytes([family[0], family[1]]) != libc::AF_UNIX as u16 {
        return false;
    }
    match addrlen {
        2 => binding,
        _ => bytes.get(2) == Some(&0),
    }
}

/// The descriptors carried by the `SCM_RIGHTS` messages in a sent or
/// received control buffer.
pub(crate) fn received_descriptors(control: &[u8]) -> Vec<RawFd> {
    const HEADER: usize = size_of::<Cmsghdr>();
    let align = |length: usize| length.next_multiple_of(size_of::<usize>());
    let mut descriptors = Vec::new();
    let mut offset = 0;
    while let Some(header) = control.get(offset..offset + HEADER) {
        let length = usize::from_ne_bytes(header[..8].try_into().unwrap());
        let level = i32::from_ne_bytes(header[8..12].try_into().unwrap());
        let kind = i32::from_ne_bytes(header[12..16].try_into().unwrap());
        let Some(data) = control.get(offset + HEADER..offset.saturating_add(length)) else {
            break;
        };
        if level == libc::SOL_SOCKET && kind == libc::SCM_RIGHTS {
            descriptors.extend(
                data.as_chunks::<{ size_of::<RawFd>() }>()
                    .0
                    .iter()
                    .map(|fd| RawFd::from_ne_bytes(*fd)),
            );
        }
        if length < HEADER {
            break;
        }
        offset += align(length);
    }
    descriptors
}

/// The `sockaddr` bytes Linux reports for `address`.
fn sockaddr_bytes(address: &NetworkAddressV1) -> Vec<u8> {
    match address {
        NetworkAddressV1::Inet4 { address, port } => {
            let mut bytes = vec![0; size_of::<libc::sockaddr_in>()];
            bytes[..2].copy_from_slice(&(libc::AF_INET as u16).to_ne_bytes());
            bytes[2..4].copy_from_slice(&port.to_be_bytes());
            bytes[4..8].copy_from_slice(address);
            bytes
        }
        NetworkAddressV1::Inet6 {
            address,
            port,
            flowinfo,
            scope_id,
        } => {
            let mut bytes = vec![0; size_of::<libc::sockaddr_in6>()];
            bytes[..2].copy_from_slice(&(libc::AF_INET6 as u16).to_ne_bytes());
            bytes[2..4].copy_from_slice(&port.to_be_bytes());
            bytes[4..8].copy_from_slice(&flowinfo.to_be_bytes());
            bytes[8..24].copy_from_slice(address);
            bytes[24..28].copy_from_slice(&scope_id.to_ne_bytes());
            bytes
        }
    }
}

/// The unspecified address of `address`'s family, as `getsockname` reports
/// for a socket whose connect failed.
fn unspecified_like(address: &NetworkAddressV1) -> NetworkAddressV1 {
    match address {
        NetworkAddressV1::Inet4 { .. } => NetworkAddressV1::Inet4 {
            address: [0; 4],
            port: 0,
        },
        NetworkAddressV1::Inet6 { .. } => NetworkAddressV1::Inet6 {
            address: [0; 16],
            port: 0,
            flowinfo: 0,
            scope_id: 0,
        },
    }
}

/// `SO_RCVLOWAT` as Linux stores it: negative means `INT_MAX`, zero means one.
fn normalized_lowat(value: i32) -> usize {
    if value < 0 {
        i32::MAX as usize
    } else {
        value.max(1) as usize
    }
}

/// The `poll` result for one descriptor: what the socket reports, limited to
/// the requested events plus the ones Linux always reports.
fn masked_revents(readiness: i16, requested: i16) -> i16 {
    readiness & (requested | libc::POLLERR | libc::POLLHUP | libc::POLLNVAL)
}

/// Classify a nonblocking host `recv` into what the recorder saw.
fn arrival_from_pull(result: Result<i64, Errno>, buffer: &[u8]) -> Option<NetworkArrival> {
    match result {
        Ok(0) => Some(NetworkArrival::PeerWriteClosed),
        Ok(received) => Some(NetworkArrival::Bytes(buffer[..received as usize].to_vec())),
        Err(Errno::EAGAIN | Errno::EINTR) => None,
        Err(errno) => Some(NetworkArrival::Error(errno.into_raw())),
    }
}

impl<T: RecordOrReplay> Detcore<T> {
    fn network_mode(&self) -> NetworkTraceMode {
        self.cfg.network_trace.mode
    }

    fn is_network_channel<G: Guest<Self>>(guest: &mut G, fd: RawFd) -> bool {
        Self::network_socket_state(guest, fd).0
    }

    /// Whether `fd` is a channel, and how its socket was classified.
    fn network_socket_state<G: Guest<Self>>(guest: &mut G, fd: RawFd) -> (bool, NetworkSocketKind) {
        guest
            .thread_state()
            .with_detfd(fd, |detfd| {
                (detfd.is_network_channel(), detfd.network_socket())
            })
            .unwrap_or((false, NetworkSocketKind::NotInet))
    }

    fn names_channel<G: Guest<Self>>(guest: &mut G, fds: [RawFd; 2]) -> bool {
        fds.into_iter()
            .any(|fd| Self::is_network_channel(guest, fd))
    }

    /// Whether the network trace handles `call`. Always false with no trace
    /// mode, so ordinary runs never reach this module.
    pub(crate) fn network_trace_owns<G: Guest<Self>>(&self, guest: &mut G, call: &Syscall) -> bool {
        if self.network_mode() == NetworkTraceMode::Off {
            return false;
        }
        match call {
            // A connect may create a channel and a poll may name one; each
            // falls back to its ordinary handler otherwise.
            Syscall::Connect(_) | Syscall::Poll(_) | Syscall::Ppoll(_) => true,
            // A received descriptor may be a socket this module never saw.
            Syscall::Recvmsg(_) | Syscall::Recvmmsg(_) => true,
            // Only AF_UNIX sockets stay out of the host's network namespace.
            Syscall::Socket(c) => c.family() != libc::AF_UNIX,
            Syscall::Select(c) => Self::select_names_channel(
                guest,
                c.nfds(),
                [c.readfds(), c.writefds(), c.exceptfds()],
            ),
            Syscall::Pselect6(c) => Self::select_names_channel(
                guest,
                c.nfds(),
                [c.readfds(), c.writefds(), c.exceptfds()],
            ),
            // The receive low-water mark may be set before the socket connects.
            Syscall::Setsockopt(c)
                if c.level() == libc::SOL_SOCKET && c.optname() == libc::SO_RCVLOWAT =>
            {
                true
            }
            // A registration, timeout or signal owner outlives the connect, so
            // refuse it on any IPv4 or IPv6 socket rather than only on a
            // channel.
            Syscall::EpollCtl(c) => {
                Self::network_socket_state(guest, c.fd()).1 != NetworkSocketKind::NotInet
            }
            Syscall::Setsockopt(c)
                if c.level() == libc::SOL_SOCKET && TIMEOUT_OPTIONS.contains(&c.optname()) =>
            {
                Self::network_socket_state(guest, c.fd()).1 != NetworkSocketKind::NotInet
            }
            // Linux answers these from the network namespace on any socket.
            Syscall::Ioctl(c) if requests_interface_state(c) => true,
            Syscall::Fcntl(_) | Syscall::Ioctl(_) if requests_signal_driven_io(call) => {
                get_fd(*call).is_some_and(|fd| {
                    Self::network_socket_state(guest, fd).1 != NetworkSocketKind::NotInet
                })
            }
            Syscall::Sendfile(c) => Self::names_channel(guest, [c.out_fd(), c.in_fd()]),
            Syscall::Splice(c) => Self::names_channel(guest, [c.fd_in(), c.fd_out()]),
            Syscall::Tee(c) => Self::names_channel(guest, [c.fd_in(), c.fd_out()]),
            Syscall::CopyFileRange(c) => Self::names_channel(guest, [c.fd_in(), c.fd_out()]),
            _ => {
                let Some(fd) = get_fd(*call) else {
                    return false;
                };
                match Self::network_socket_state(guest, fd) {
                    (true, _) => !channel_call_takes_ordinary_path(call),
                    (false, NetworkSocketKind::NotInet) => {
                        Self::names_abstract_unix_destination(guest, call)
                    }
                    (false, kind) => reaches_network_outside_channel(call, kind),
                }
            }
        }
    }

    /// Whether `call`, a `bind` or send, names an abstract `AF_UNIX` address.
    /// An address the guest cannot read fails in Linux before any effect.
    fn names_abstract_unix_destination<G: Guest<Self>>(guest: &mut G, call: &Syscall) -> bool {
        let names = |guest: &mut G, address: Option<Addr<'_, u8>>, length: usize, binding| {
            let mut bytes = [0u8; 3];
            let prefix = &mut bytes[..length.min(3)];
            address.is_some_and(|address| guest.memory().read_exact(address, prefix).is_ok())
                && names_abstract_unix_address(prefix, length, binding)
        };
        let message_names = |guest: &mut G, header: Option<Addr<'_, Msghdr>>| {
            let Some(header) = header.and_then(|header| guest.memory().read_value(header).ok())
            else {
                return false;
            };
            let header: Msghdr = header;
            names(
                guest,
                Addr::from_raw(header.msg_name as usize),
                header.msg_namelen as usize,
                false,
            )
        };
        let length = |addrlen: i32| usize::try_from(addrlen).unwrap_or(0);
        match call {
            Syscall::Bind(c) => names(
                guest,
                c.umyaddr().map(|a| Addr::from(a.cast())),
                length(c.addrlen()),
                true,
            ),
            Syscall::Sendto(c) => names(
                guest,
                c.addr().map(|a| Addr::from(a.cast())),
                length(c.addr_len()),
                false,
            ),
            Syscall::Sendmsg(c) => message_names(guest, c.msg()),
            Syscall::Sendmmsg(c) => {
                let Some(vector) = c.msgvec() else {
                    return false;
                };
                let vector = vector.cast::<Mmsghdr>();
                (0..c.vlen().min(libc::UIO_MAXIOV as u32) as usize).any(|index| {
                    // SAFETY: only read through guest memory access, which
                    // fails on an unmapped address.
                    let entry = unsafe { vector.add(index) };
                    message_names(guest, Some(entry.cast()))
                })
            }
            _ => false,
        }
    }

    /// Whether a `select` descriptor set below `nfds` names a channel. Linux
    /// reads `nfds` bits, not a fixed `fd_set`, so a channel at or above
    /// `FD_SETSIZE` counts too. Only the word holding each channel's bit is
    /// read, which bounds the reads by the open channels rather than `nfds`.
    /// An unreadable word leaves the call to its ordinary path, where Linux
    /// fails it with `EFAULT`.
    fn select_names_channel<G: Guest<Self>>(
        guest: &mut G,
        nfds: i32,
        sets: [Option<AddrMut<'_, libc::fd_set>>; 3],
    ) -> bool {
        const WORD_BITS: usize = u64::BITS as usize;
        let channels = guest.thread_state().network_channel_fds();
        channels
            .into_iter()
            .filter(|&fd| fd >= 0 && fd < nfds)
            .any(|fd| {
                let fd = fd as usize;
                sets.into_iter().flatten().any(|set| {
                    // SAFETY: only read through guest memory access, which
                    // fails on an unmapped address.
                    let word = unsafe { set.cast::<u64>().add(fd / WORD_BITS) };
                    guest
                        .memory()
                        .read_value(word)
                        .is_ok_and(|word| word & (1 << (fd % WORD_BITS)) != 0)
                })
            })
    }

    /// Handle a call that [`Self::network_trace_owns`] accepted.
    pub(crate) async fn handle_network_trace_syscall<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Error> {
        match call {
            Syscall::Socket(c) => return self.network_socket(guest, c).await,
            Syscall::Connect(c) => return self.network_connect(guest, c).await,
            Syscall::Poll(c) => return self.network_poll(guest, c).await,
            Syscall::Ppoll(c) => return self.network_ppoll(guest, c).await,
            Syscall::Setsockopt(c) => return self.network_setsockopt(guest, c).await,
            Syscall::Recvmsg(c) if !Self::is_network_channel(guest, c.sockfd()) => {
                return self.network_recv_outside_channel(guest, call).await;
            }
            Syscall::Recvmmsg(c) if !Self::is_network_channel(guest, c.fd()) => {
                return self.network_recv_outside_channel(guest, call).await;
            }
            Syscall::Ioctl(c) if requests_interface_state(&c) => {
                self.network_refuse(
                    guest,
                    &format!(
                        "network record/replay does not model the interface and route ioctl \
                         {:#x}, which Linux answers from the host's network namespace on a \
                         recording and from a private one on a replay",
                        c.request().into_raw().0
                    ),
                    UNSUPPORTED_REMEDY,
                )
                .await
            }
            Syscall::Fcntl(_) | Syscall::Ioctl(_) if requests_signal_driven_io(&call) => {
                self.network_refuse(
                    guest,
                    "network record/replay does not model signal-driven I/O (O_ASYNC, F_SETOWN, \
                     F_SETSIG) on an IPv4 or IPv6 socket",
                    UNSUPPORTED_REMEDY,
                )
                .await
            }
            Syscall::EpollCtl(_) => {
                self.network_refuse(
                    guest,
                    "network record/replay does not model epoll registration of an IPv4 or \
                     IPv6 socket",
                    UNSUPPORTED_REMEDY,
                )
                .await
            }
            Syscall::Select(c) => return self.network_select(guest, c).await,
            Syscall::Pselect6(c) => return self.network_pselect6(guest, c).await,
            Syscall::Sendfile(_)
            | Syscall::Splice(_)
            | Syscall::Tee(_)
            | Syscall::CopyFileRange(_) => self.network_refuse_on_channel(guest, &call).await,
            _ => {}
        }
        // Every other call the trace owns names one socket.
        let fd = get_fd(call).expect("an owned call names a descriptor");
        let (channel, kind) = Self::network_socket_state(guest, fd);
        if !channel && kind == NetworkSocketKind::NotInet {
            self.network_refuse(
                guest,
                &format!(
                    "network record/replay does not model {} to an abstract AF_UNIX address, \
                     which names the host's network namespace rather than a file",
                    call.name()
                ),
                NAMESPACE_REMEDY,
            )
            .await
        }
        if !channel {
            self.network_refuse(
                guest,
                &format!(
                    "network record/replay does not model {} on an IPv4 or IPv6 socket that is \
                     not a connected TCP client",
                    call.name()
                ),
                UNSUPPORTED_REMEDY,
            )
            .await
        }
        match call {
            Syscall::Read(c) => {
                self.network_recv(
                    guest,
                    c.fd(),
                    c.buf(),
                    c.len(),
                    0,
                    c.signal_interrupt_errno(),
                )
                .await
            }
            Syscall::Recvfrom(c) => {
                let received = self
                    .network_recv(
                        guest,
                        c.fd(),
                        c.buf(),
                        c.len(),
                        c.flags(),
                        c.signal_interrupt_errno(),
                    )
                    .await?;
                // As Linux does, copy out a source address only after a
                // successful receive that asked for one. A connected TCP
                // socket reports none: its length is zero.
                if c.addr().is_some() {
                    let length = c.addr_len().ok_or(Errno::EFAULT)?;
                    let requested: i32 = guest
                        .memory()
                        .read_value(length.cast())
                        .map_err(|_| Errno::EFAULT)?;
                    if requested < 0 {
                        return Err(Errno::EINVAL.into());
                    }
                    guest
                        .memory()
                        .write_value(length, &0u32)
                        .map_err(|_| Errno::EFAULT)?;
                }
                Ok(received)
            }
            Syscall::Write(c) => self.network_send(guest, c.fd(), c.buf(), c.len(), 0).await,
            Syscall::Sendto(c) => {
                // A connected TCP socket ignores the destination address.
                let flags = c.flags() as i32;
                self.network_send(
                    guest,
                    c.fd(),
                    c.buf().map(|buf| Addr::from(buf.cast::<u8>())),
                    c.size(),
                    flags,
                )
                .await
            }
            Syscall::Shutdown(c) => self.network_shutdown(guest, c).await,
            Syscall::Getsockname(c) => {
                self.network_address(guest, c.fd(), c.usockaddr(), c.usockaddr_len(), true)
                    .await
            }
            Syscall::Getpeername(c) => {
                self.network_address(guest, c.fd(), c.usockaddr(), c.usockaddr_len(), false)
                    .await
            }
            Syscall::Getsockopt(c)
                if c.level() == libc::SOL_SOCKET && c.optname() == libc::SO_ERROR =>
            {
                self.network_so_error(guest, c).await
            }
            // Configuration options are the same in record and replay; the
            // host socket answers them in both modes.
            Syscall::Getsockopt(c) if is_configuration_option(c.level(), c.optname()) => {
                self.handle_getsockopt(guest, c).await
            }
            Syscall::Getsockopt(c) => {
                self.network_refuse(
                    guest,
                    &format!(
                        "network trace does not model getsockopt level {} option {} on a \
                         recorded socket",
                        c.level(),
                        c.optname()
                    ),
                    UNSUPPORTED_REMEDY,
                )
                .await
            }
            other => self.network_refuse_on_channel(guest, &other).await,
        }
    }

    /// End the run: the engine cannot answer this operation faithfully.
    async fn network_refuse<G: Guest<Self>>(&self, guest: &mut G, reason: &str, remedy: &str) -> ! {
        eprintln!("hermit: {reason}. {remedy}");
        unrecoverable_shutdown(guest, HERMIT_POLICY_REFUSAL_EXIT).await
    }

    /// After a successful open of `path`, end the run if it names the host's
    /// network namespace state. `resolved`, the path the kernel reports for
    /// the new descriptor, catches a spelling through a symbolic link.
    pub(crate) async fn network_check_open<G: Guest<Self>>(
        &self,
        guest: &mut G,
        path: &Path,
        resolved: impl FnOnce() -> Option<PathBuf>,
    ) {
        if self.network_mode() == NetworkTraceMode::Off {
            return;
        }
        if names_host_network_state(path)
            || resolved().is_some_and(|resolved| names_host_network_state(&resolved))
        {
            self.network_refuse(
                guest,
                &format!(
                    "network record/replay does not model reading {}, which lists the host's \
                     network interfaces and connections on a recording and a private network \
                     namespace's on a replay",
                    path.display()
                ),
                UNSUPPORTED_REMEDY,
            )
            .await
        }
    }

    /// End the run on an engine error, naming its remedy.
    async fn network_refuse_error<G: Guest<Self>>(
        &self,
        guest: &mut G,
        error: &NetworkEngineError,
    ) -> ! {
        self.network_refuse(guest, &error.to_string(), error.remedy())
            .await
    }

    async fn network_refuse_on_channel<G: Guest<Self>>(&self, guest: &mut G, call: &Syscall) -> ! {
        self.network_refuse(
            guest,
            &format!(
                "network trace does not model {} on a recorded socket",
                call.name()
            ),
            UNSUPPORTED_REMEDY,
        )
        .await
    }

    /// `socket`: refuse every family but IPv4 and IPv6, whose sockets reach
    /// the host's network namespace, and raw sockets, and classify IPv4 and
    /// IPv6 sockets for later calls.
    async fn network_socket<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Socket,
    ) -> Result<i64, Error> {
        let family = call.family();
        if family != libc::AF_INET && family != libc::AF_INET6 {
            let sockets = match family {
                libc::AF_NETLINK => "netlink sockets".to_owned(),
                libc::AF_PACKET => "packet sockets".to_owned(),
                other => format!("sockets of address family {other}"),
            };
            self.network_refuse(
                guest,
                &format!(
                    "network record/replay does not model {sockets}, which reach the host's \
                     network namespace"
                ),
                NAMESPACE_REMEDY,
            )
            .await
        }
        let socket_type = call.r#type() & 0xf;
        if socket_type == libc::SOCK_RAW {
            self.network_refuse(
                guest,
                "network record/replay does not model raw IPv4 or IPv6 sockets",
                UNSUPPORTED_REMEDY,
            )
            .await
        }
        let fd = self.handle_socket(guest, call).await?;
        let stream =
            socket_type == libc::SOCK_STREAM && matches!(call.protocol(), 0 | libc::IPPROTO_TCP);
        let kind = if stream {
            NetworkSocketKind::InetStream {
                ipv6: family == libc::AF_INET6,
            }
        } else {
            NetworkSocketKind::InetOther
        };
        guest
            .thread_state()
            .with_detfd(fd as RawFd, |detfd| detfd.set_network_socket(kind))?;
        Ok(fd)
    }

    /// `recvmsg` or `recvmmsg` outside a channel: take the ordinary path, then
    /// refuse if it delivered through `SCM_RIGHTS` a socket of another family
    /// than `AF_UNIX`, which this module never classified and so could not
    /// keep from the network. A descriptor that is no socket stays untraced.
    async fn network_recv_outside_channel<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Error> {
        // Raw addresses: a typed address is not `Send` across the receive.
        let headers: Vec<usize> = match &call {
            Syscall::Recvmsg(c) => c.msg().map(AddrMut::as_raw).into_iter().collect(),
            Syscall::Recvmmsg(c) => c.mmsg().map_or_else(Vec::new, |vector| {
                (0..c.vlen().min(libc::UIO_MAXIOV as u32) as usize)
                    // SAFETY: only read through guest memory access, which
                    // fails on an unmapped address.
                    .map(|index| unsafe { vector.add(index) }.as_raw())
                    .collect()
            }),
            _ => unreachable!("only receives with ancillary data reach here"),
        };
        // The control buffers the guest offered, before Linux overwrites
        // their lengths with what it delivered.
        let offered: Vec<Option<(usize, usize)>> = headers
            .iter()
            .map(|header| {
                let header = Addr::<Msghdr>::from_raw(*header)?;
                let header: Msghdr = guest.memory().read_value(header).ok()?;
                Some((header.msg_control as usize, header.msg_controllen))
            })
            .collect();
        let (result, received) = match call {
            Syscall::Recvmsg(c) => (self.handle_recvmsg(guest, c).await?, 1),
            // Ordinary dispatch first gives a sock_diag reply socket its
            // sanitized answer. That socket is netlink, which `socket` refuses
            // under a trace mode, so none exists here.
            Syscall::Recvmmsg(c) => {
                let result = self.handle_recvmmsg(guest, c).await?;
                (result, result as usize)
            }
            _ => unreachable!("only receives with ancillary data reach here"),
        };
        for (header, offered) in headers.into_iter().zip(offered).take(received) {
            let Some((control, capacity)) = offered else {
                continue;
            };
            let delivered = Addr::<Msghdr>::from_raw(header)
                .ok_or(Errno::EFAULT)
                .and_then(|header| guest.memory().read_value(header))
                .map(|header: Msghdr| header.msg_controllen.min(capacity).min(MAX_CONTROL_BYTES));
            let mut bytes = vec![0; *delivered.as_ref().unwrap_or(&0)];
            let readable = delivered.is_ok()
                && (bytes.is_empty()
                    || Addr::<u8>::from_raw(control).is_some_and(|control| {
                        guest.memory().read_exact(control, &mut bytes).is_ok()
                    }));
            if !readable {
                self.network_refuse(
                    guest,
                    "network record/replay could not read the ancillary data Linux delivered, \
                     so cannot check it for sockets",
                    "The program changed its receive buffers during the call; fix the program.",
                )
                .await
            }
            for fd in received_descriptors(&bytes) {
                match self
                    .network_host_int_option(guest, fd, libc::SO_DOMAIN)
                    .await
                {
                    Ok(libc::AF_UNIX) | Err(Errno::ENOTSOCK) => {}
                    Ok(domain) => {
                        self.network_refuse(
                            guest,
                            &format!(
                                "network record/replay does not model a socket of address \
                                 family {domain} received through SCM_RIGHTS"
                            ),
                            UNSUPPORTED_REMEDY,
                        )
                        .await
                    }
                    Err(errno) => {
                        self.network_refuse(
                            guest,
                            &format!(
                                "network record/replay could not classify descriptor {fd} \
                                 received through SCM_RIGHTS: {errno}"
                            ),
                            UNSUPPORTED_REMEDY,
                        )
                        .await
                    }
                }
            }
        }
        Ok(result)
    }

    /// Perform one engine operation at the current global time.
    async fn network_request<G: Guest<Self>>(
        &self,
        guest: &mut G,
        request: NetworkRequest,
    ) -> NetworkReply {
        let reply = match send_and_update_time(guest, GlobalRequest::Network(request))
            .await
            .1
        {
            GlobalResponse::Network(reply) => reply,
            _ => unreachable!(),
        };
        match reply {
            Ok(reply) => reply,
            Err(error) => self.network_refuse_error(guest, &error).await,
        }
    }

    fn network_id<G: Guest<Self>>(guest: &mut G, fd: RawFd) -> OpenFileId {
        guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.open_file_id())
            .expect("network channel descriptor is tracked")
    }

    /// Wait in the host, without yielding, for `events` on `fd`. Returns the
    /// reported events, or `None` on timeout.
    async fn network_host_wait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        events: i16,
        timeout_millis: i32,
    ) -> Result<Option<i16>, Error> {
        let mut stack = guest.stack().await;
        let pollfd: AddrMut<libc::pollfd> = stack.reserve();
        let _guard = stack.commit()?;
        guest.memory().write_value(
            pollfd,
            &libc::pollfd {
                fd,
                events,
                revents: 0,
            },
        )?;
        loop {
            let call = syscalls::Poll::new()
                .with_fds(Some(pollfd.cast()))
                .with_nfds(1)
                .with_timeout(timeout_millis);
            match guest.inject(call).await {
                Ok(0) => return Ok(None),
                Ok(_) => {
                    let pollfd: libc::pollfd = guest.memory().read_value(pollfd)?;
                    return Ok(Some(pollfd.revents));
                }
                Err(Errno::EINTR) => continue,
                Err(errno) => return Err(errno.into()),
            }
        }
    }

    /// Read an `int` socket option from the host socket.
    async fn network_host_int_option<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        name: i32,
    ) -> Result<i32, Errno> {
        let mut stack = guest.stack().await;
        let value: AddrMut<i32> = stack.reserve();
        let length = stack.push(size_of::<i32>() as libc::socklen_t);
        let _guard = stack.commit()?;
        let call = syscalls::Getsockopt::new()
            .with_fd(fd)
            .with_level(libc::SOL_SOCKET)
            .with_optname(name)
            .with_optval(Some(value.cast()))
            .with_optlen(Some(
                AddrMut::from_raw(length.as_raw()).ok_or(Errno::EFAULT)?,
            ));
        guest.inject(call).await?;
        guest.memory().read_value(value)
    }

    /// The local address of the host socket.
    async fn network_host_local_address<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
    ) -> Result<Option<NetworkAddressV1>, Errno> {
        let mut stack = guest.stack().await;
        let address: AddrMut<libc::sockaddr_in6> = stack.reserve();
        let length = stack.push(size_of::<libc::sockaddr_in6>() as libc::socklen_t);
        let _guard = stack.commit()?;
        let length = AddrMut::<libc::socklen_t>::from_raw(length.as_raw()).ok_or(Errno::EFAULT)?;
        let call = syscalls::Getsockname::new()
            .with_fd(fd)
            .with_usockaddr(Some(address.cast()))
            .with_usockaddr_len(Some(length));
        guest.inject(call).await?;
        let length: libc::socklen_t = guest.memory().read_value(length)?;
        let mut bytes = vec![0; (length as usize).min(size_of::<libc::sockaddr_in6>())];
        guest.memory().read_exact(address.cast(), &mut bytes)?;
        Ok(address_from_sockaddr(&bytes))
    }

    /// One nonblocking host receive into `buffer`, as the recorder sees it.
    async fn network_pull<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        buffer: AddrMut<'_, u8>,
        len: usize,
    ) -> Result<Option<NetworkArrival>, Error> {
        let call = syscalls::Recvfrom::new()
            .with_fd(fd)
            .with_buf(Some(buffer))
            .with_len(len)
            .with_flags(libc::MSG_DONTWAIT);
        let result = guest.inject(call).await;
        if result == Err(Errno::EFAULT) {
            self.network_refuse_bad_buffer(guest).await
        }
        let mut bytes = Vec::new();
        if let Ok(received) = result {
            bytes.resize(received as usize, 0);
            guest.memory().read_exact(buffer, &mut bytes)?;
        }
        Ok(arrival_from_pull(result, &bytes))
    }

    /// End the run: the guest received into memory it cannot write. Linux
    /// would leave the bytes queued, which replay cannot reproduce because the
    /// engine hands bytes over only by consuming them.
    async fn network_refuse_bad_buffer<G: Guest<Self>>(&self, guest: &mut G) -> ! {
        self.network_refuse(
            guest,
            "network trace cannot receive into an unwritable guest buffer",
            "The program passed a bad buffer to a receive call; fix the program.",
        )
        .await
    }

    /// `connect`: an outbound TCP connect to an IPv4 or IPv6 peer becomes a
    /// channel. Linux's argument checks run here, in the same order, so record
    /// and replay fail them alike before the host is reached. A connect on any
    /// other socket takes the ordinary path, unless it names an abstract
    /// `AF_UNIX` address.
    async fn network_connect<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Connect,
    ) -> Result<i64, Error> {
        let fd = call.fd();
        let socket = guest
            .thread_state()
            .with_detfd(fd, |detfd| {
                (detfd.ty() == FdType::Socket && detfd.open_file_id().is_socket()).then(|| {
                    (
                        detfd.open_file_id(),
                        detfd.is_nonblocking(),
                        detfd.is_network_channel(),
                        detfd.network_socket(),
                    )
                })
            })
            .ok()
            .flatten();
        let kind = socket.map_or(NetworkSocketKind::NotInet, |socket| socket.3);
        let ipv6 = match kind {
            NetworkSocketKind::NotInet => {
                if let Ok(bytes) = read_sockaddr(&guest.memory(), call.uservaddr(), call.addrlen())
                    && names_abstract_unix_address(&bytes, bytes.len(), false)
                {
                    self.network_refuse(
                        guest,
                        "network record/replay does not model connect to an abstract AF_UNIX \
                         address, which names the host's network namespace rather than a file",
                        NAMESPACE_REMEDY,
                    )
                    .await
                }
                return self.handle_connect(guest, call).await;
            }
            NetworkSocketKind::InetOther => {
                self.network_refuse(
                    guest,
                    "network record/replay does not model connect on an IPv4 or IPv6 socket \
                     other than TCP",
                    UNSUPPORTED_REMEDY,
                )
                .await
            }
            NetworkSocketKind::InetStream { ipv6 } => ipv6,
        };
        let (id, nonblocking, already_channel, _) =
            socket.expect("an IPv4 or IPv6 socket is tracked");
        let bytes = read_sockaddr(&guest.memory(), call.uservaddr(), call.addrlen())?;
        let family = match bytes.get(..2) {
            Some(family) => u16::from_ne_bytes([family[0], family[1]]),
            None => return Err(Errno::EINVAL.into()),
        };
        if family == libc::AF_UNSPEC as u16 {
            self.network_refuse(
                guest,
                "network record/replay does not model disconnecting a socket with an AF_UNSPEC \
                 connect",
                UNSUPPORTED_REMEDY,
            )
            .await
        }
        if already_channel {
            // Linux reports a finished connect's error once, then EISCONN.
            let NetworkReply::Errno(errno) = self
                .network_request(guest, NetworkRequest::TakeError(id))
                .await
            else {
                unreachable!()
            };
            return Err(Errno::new(if errno == 0 { libc::EISCONN } else { errno }).into());
        }
        let (minimum, expected) = if ipv6 {
            (SOCKADDR_IN6_RFC2133_LEN, libc::AF_INET6)
        } else {
            (size_of::<libc::sockaddr_in>(), libc::AF_INET)
        };
        if bytes.len() < minimum {
            return Err(Errno::EINVAL.into());
        }
        if family != expected as u16 {
            return Err(Errno::EAFNOSUPPORT.into());
        }
        let peer = address_from_sockaddr(&bytes).ok_or(Errno::EINVAL)?;

        let request = if self.network_mode() == NetworkTraceMode::Record {
            if !peer.is_traceable_peer() {
                self.network_refuse_error(guest, &NetworkEngineError::UntraceablePeer(peer))
                    .await
            }
            let errno = match guest.inject(call).await {
                Ok(_) => 0,
                // An interrupted connect continues in Linux; finish it, so the
                // recording never holds a signal the replay cannot reproduce.
                Err(Errno::EINPROGRESS | Errno::EINTR) => {
                    if self
                        .network_host_wait(guest, fd, libc::POLLOUT, CONNECT_WAIT_MILLIS)
                        .await?
                        .is_none()
                    {
                        self.network_refuse(
                            guest,
                            "network connect did not finish in time",
                            HOST_REMEDY,
                        )
                        .await
                    }
                    self.network_host_int_option(guest, fd, libc::SO_ERROR)
                        .await?
                }
                // The argument checks above already ran, so every other
                // error is the host network's answer, which replay repeats.
                Err(errno) => errno.into_raw(),
            };
            let local = if errno == 0 {
                match self.network_host_local_address(guest, fd).await? {
                    Some(local) => Some(local),
                    None => {
                        self.network_refuse(
                            guest,
                            "network connect has no local address",
                            HOST_REMEDY,
                        )
                        .await
                    }
                }
            } else {
                None
            };
            NetworkRequest::RecordConnect {
                id,
                peer,
                local,
                errno,
                nonblocking,
            }
        } else {
            NetworkRequest::ReplayConnect {
                id,
                peer,
                nonblocking,
            }
        };
        let NetworkReply::Errno(errno) = self.network_request(guest, request).await else {
            unreachable!()
        };
        guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.set_network_channel())?;
        if errno == 0 {
            Ok(0)
        } else {
            Err(Errno::new(errno).into())
        }
    }

    /// `setsockopt`: track `SO_RCVLOWAT`, which the engine needs for receive
    /// targets and readiness, and refuse a send or receive timeout, which
    /// Linux would apply in wall-clock time. The host socket applies every
    /// other option, and fails a malformed timeout as Linux does.
    async fn network_setsockopt<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Setsockopt,
    ) -> Result<i64, Error> {
        if call.level() == libc::IPPROTO_TCP && call.optname() == libc::TCP_FASTOPEN_CONNECT {
            self.network_refuse(
                guest,
                "network record/replay does not model TCP Fast Open",
                UNSUPPORTED_REMEDY,
            )
            .await
        }
        if call.level() == libc::SOL_SOCKET
            && TIMEOUT_OPTIONS.contains(&call.optname())
            && call.optlen() as usize >= size_of::<libc::timeval>()
            && let Some(value) = call.optval()
            && let Ok(timeout) = guest.memory().read_value::<_, libc::timeval>(value.cast())
            && sets_a_timeout(timeout)
        {
            self.network_refuse(
                guest,
                "network record/replay does not model socket send or receive timeouts \
                 (SO_RCVTIMEO, SO_SNDTIMEO)",
                UNSUPPORTED_REMEDY,
            )
            .await
        }
        let result = self.handle_setsockopt(guest, call).await?;
        if call.level() == libc::SOL_SOCKET
            && call.optname() == libc::SO_RCVLOWAT
            && let Some(value) = call.optval()
        {
            let value: i32 = guest.memory().read_value(value.cast())?;
            let _ = guest.thread_state().with_detfd(call.fd(), |detfd| {
                detfd.set_network_lowat(normalized_lowat(value))
            });
        }
        Ok(result)
    }

    /// `recv`, `recvfrom` and `read` on a channel.
    async fn network_recv<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        buffer: Option<AddrMut<'_, u8>>,
        len: usize,
        flags: i32,
        interrupted: Errno,
    ) -> Result<i64, Error> {
        if flags & !RECV_FLAGS != 0 {
            self.network_refuse(
                guest,
                &format!("network trace does not model recv flags {flags:#x}"),
                UNSUPPORTED_REMEDY,
            )
            .await
        }
        if len == 0 {
            return Ok(0);
        }
        let buffer = buffer.ok_or(Errno::EFAULT)?;
        let (id, logically_nonblocking, lowat) = guest.thread_state().with_detfd(fd, |detfd| {
            (
                detfd.open_file_id(),
                detfd.is_nonblocking(),
                detfd.network_lowat(),
            )
        })?;
        let nonblocking = logically_nonblocking || flags & libc::MSG_DONTWAIT != 0;
        let target = if nonblocking {
            1
        } else if flags & libc::MSG_WAITALL != 0 {
            len
        } else {
            lowat.min(len)
        };
        let record = self.network_mode() == NetworkTraceMode::Record;
        let mut rsrc = Resources::new(guest.thread_state().dettid);
        rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
        rsrc.fyi("network recv");
        loop {
            if !nonblocking
                && matches!(
                    resource_request(guest, rsrc.clone()).await,
                    ResumeStatus::Signaled(_)
                )
            {
                return Err(interrupted.into());
            }
            let arrivals = if record {
                self.network_pull(guest, fd, buffer, len)
                    .await?
                    .into_iter()
                    .collect()
            } else {
                Vec::new()
            };
            let request = NetworkRequest::Recv {
                id,
                arrivals,
                max_len: len,
                target,
                peek: flags & libc::MSG_PEEK != 0,
            };
            let NetworkReply::Recv(outcome) = self.network_request(guest, request).await else {
                unreachable!()
            };
            match outcome {
                NetworkRecvOutcome::Data(bytes) => {
                    if guest.memory().write_exact(buffer, &bytes).is_err() {
                        self.network_refuse_bad_buffer(guest).await
                    }
                    return Ok(bytes.len() as i64);
                }
                NetworkRecvOutcome::Eof => return Ok(0),
                NetworkRecvOutcome::Error(errno) => return Err(Errno::new(errno).into()),
                NetworkRecvOutcome::WouldBlock if nonblocking => {
                    return Err(Errno::EAGAIN.into());
                }
                NetworkRecvOutcome::WouldBlock => {
                    rsrc.poll_attempt += 1;
                    record_retry_event(guest, syscalls::Recvfrom::new().with_fd(fd)).await;
                }
            }
        }
    }

    /// `send`, `sendto` and `write` on a channel.
    async fn network_send<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        buffer: Option<Addr<'_, u8>>,
        len: usize,
        flags: i32,
    ) -> Result<i64, Error> {
        if flags & !SEND_FLAGS != 0 {
            self.network_refuse(
                guest,
                &format!("network trace does not model send flags {flags:#x}"),
                UNSUPPORTED_REMEDY,
            )
            .await
        }
        let (id, logically_nonblocking) = guest
            .thread_state()
            .with_detfd(fd, |detfd| (detfd.open_file_id(), detfd.is_nonblocking()))?;
        let mut bytes = vec![0; len];
        if len != 0 {
            guest
                .memory()
                .read_exact(buffer.ok_or(Errno::EFAULT)?, &mut bytes)
                .map_err(|_| Errno::EFAULT)?;
        }
        let NetworkReply::Failure(failure) = self
            .network_request(guest, NetworkRequest::SendFailure(id))
            .await
        else {
            unreachable!()
        };
        if let Some(errno) = failure {
            if errno == libc::EPIPE && flags & libc::MSG_NOSIGNAL == 0 {
                self.network_refuse(
                    guest,
                    "network trace does not model SIGPIPE",
                    "Send with MSG_NOSIGNAL, or run without --record-networking.",
                )
                .await
            }
            return Err(Errno::new(errno).into());
        }
        if len == 0 {
            return Ok(0);
        }
        let nonblocking = logically_nonblocking || flags & libc::MSG_DONTWAIT != 0;
        let buffer = buffer.ok_or(Errno::EFAULT)?;
        let mut sent = 0;
        // A blocking send that cannot go on yet yields to the scheduler, so
        // that other guest threads, which may be what the peer waits for,
        // keep running. Record waits until the host accepts more; replay
        // waits until the next fragment is due, which is after as many waits
        // as the recording took and no earlier in global time than the
        // recording accepted it. Both wait the same way, so a replayed send
        // returns at the same point of the guest's execution as the recorded
        // one. `waits` counts the waits since the send started or since its
        // last accepted chunk; after the first wait, every attempt goes on
        // from what the guest holds then.
        let mut rsrc = Resources::new(guest.thread_state().dettid);
        rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
        rsrc.fyi("network send");
        let mut waits = 0;
        let mut waited = false;
        if self.network_mode() == NetworkTraceMode::Record {
            // A nonblocking send the host refuses reports EAGAIN, and the
            // recording holds the refusal so that replay reports it at the
            // same stream offset. A blocking send carries its mark across
            // each wait, and the engine refuses it if any other output event
            // on the channel happened in between, which network replay does
            // not model.
            let mut at = None;
            while sent < len && !(nonblocking && sent > 0) {
                if waited {
                    self.network_send_resume(guest, fd, id, buffer, sent, &mut bytes)
                        .await;
                }
                let call = syscalls::Sendto::new()
                    .with_fd(fd)
                    // SAFETY: `sent < len` stays within the guest's buffer.
                    .with_buf(AddrMut::from_raw(unsafe { buffer.add(sent) }.as_raw()))
                    .with_size(len - sent)
                    .with_flags(
                        (flags & libc::MSG_MORE | libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL) as u32,
                    );
                match guest.inject(call).await {
                    Ok(written) => {
                        let written = written as usize;
                        let request = NetworkRequest::RecordSend {
                            id,
                            bytes: bytes[sent..sent + written].to_vec(),
                            at,
                            waits,
                        };
                        let NetworkReply::Recorded(end) =
                            self.network_request(guest, request).await
                        else {
                            unreachable!()
                        };
                        at = Some(end);
                        sent += written;
                        waits = 0;
                    }
                    Err(Errno::EINTR) => {}
                    Err(Errno::EAGAIN) if nonblocking => {
                        self.network_request(guest, NetworkRequest::RecordRefusedSend(id))
                            .await;
                        return Err(Errno::EAGAIN.into());
                    }
                    Err(Errno::EAGAIN) => {
                        if at.is_none() {
                            // Waiting before the first accepted byte: take
                            // the mark now; an empty send records nothing.
                            let request = NetworkRequest::RecordSend {
                                id,
                                bytes: Vec::new(),
                                at: None,
                                waits: 0,
                            };
                            let NetworkReply::Recorded(mark) =
                                self.network_request(guest, request).await
                            else {
                                unreachable!()
                            };
                            at = Some(mark);
                        }
                        waits += 1;
                        waited = true;
                        self.network_send_wait(guest, fd, &mut rsrc).await;
                    }
                    Err(errno) => {
                        self.network_refuse(
                            guest,
                            &format!("network send failed in the host with {errno}"),
                            HOST_REMEDY,
                        )
                        .await
                    }
                }
            }
        } else {
            while sent < len && !(nonblocking && sent > 0) {
                if waited {
                    self.network_send_resume(guest, fd, id, buffer, sent, &mut bytes)
                        .await;
                }
                let request = NetworkRequest::ReplaySend {
                    id,
                    bytes: bytes[sent..].to_vec(),
                    nonblocking,
                    waits,
                };
                match self.network_request(guest, request).await {
                    NetworkReply::Sent(accepted) => {
                        sent += accepted;
                        waits = 0;
                    }
                    NetworkReply::WouldBlock => return Err(Errno::EAGAIN.into()),
                    NetworkReply::NotYet => {
                        waits += 1;
                        waited = true;
                        self.network_send_wait(guest, fd, &mut rsrc).await;
                    }
                    _ => unreachable!(),
                }
            }
        }
        Ok(sent as i64)
    }

    /// Wait once for a blocking send: publish a retry event and yield one
    /// polling turn to the scheduler. Record and replay wait identically.
    async fn network_send_wait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        rsrc: &mut Resources,
    ) {
        rsrc.poll_attempt += 1;
        record_retry_event(guest, syscalls::Sendto::new().with_fd(fd)).await;
        if matches!(
            resource_request(guest, rsrc.clone()).await,
            ResumeStatus::Signaled(_)
        ) {
            self.network_refuse(
                guest,
                "network record does not model a signal interrupting a send that waits for \
                 buffer space",
                UNSUPPORTED_REMEDY,
            )
            .await
        }
    }

    /// Prepare a blocking send to go on after it waited, while other guest
    /// threads ran. Linux's in-flight send keeps the open file description it
    /// started on and reads the buffer when it copies each part. So `fd` must
    /// still name the channel `id`, and the unsent bytes are read again; the
    /// caller sends, and records, exactly what the guest's memory holds now.
    async fn network_send_resume<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        id: OpenFileId,
        buffer: Addr<'_, u8>,
        sent: usize,
        bytes: &mut [u8],
    ) {
        let same_channel = guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.open_file_id() == id)
            .unwrap_or(false);
        if !same_channel {
            self.network_refuse(
                guest,
                "network trace does not model closing or replacing a descriptor while a send \
                 on it waits for buffer space",
                UNSUPPORTED_REMEDY,
            )
            .await
        }
        // SAFETY: `sent < bytes.len()` stays within the guest's buffer.
        let unsent = unsafe { buffer.add(sent) };
        if guest
            .memory()
            .read_exact(unsent, &mut bytes[sent..])
            .is_err()
        {
            self.network_refuse(
                guest,
                "network trace cannot send from a guest buffer that became unreadable while \
                 the send waited for buffer space",
                "The program unmapped a buffer while sending from it; fix the program.",
            )
            .await
        }
    }

    /// The pollfd array at `address` and, for each entry, its channel and
    /// receive low-water mark. `None` when the array names no channel, so the
    /// call takes its ordinary path. Also `None`, before anything is read,
    /// when `nfds` (counted by [`poll_nfds`]) exceeds the process's
    /// RLIMIT_NOFILE: Linux refuses that count with EINVAL before it reads the
    /// array, and the ordinary path checks the rest of the call in Linux's
    /// order and gets that refusal.
    fn network_pollfds<G: Guest<Self>>(
        guest: &mut G,
        address: Option<AddrMut<'_, libc::pollfd>>,
        nfds: u32,
    ) -> Option<(Vec<libc::pollfd>, Vec<Option<PolledChannel>>)> {
        let address = address?;
        let open_file_limit = guest
            .thread_state()
            .resource_limits
            .lock()
            .expect("resource limits mutex poisoned")
            .get(reverie::syscalls::linux_abi::RLIMIT_NOFILE)
            .map_or(libc::RLIM64_INFINITY, |limit| limit.current);
        if u64::from(nfds) > open_file_limit {
            return None;
        }
        let mut pollfds = vec![
            libc::pollfd {
                fd: -1,
                events: 0,
                revents: 0,
            };
            nfds as usize
        ];
        guest
            .memory()
            .read_values(address.into(), &mut pollfds)
            .ok()?;
        let channels: Vec<_> = pollfds
            .iter()
            .map(|pollfd| Self::polled_channel(guest, pollfd.fd))
            .collect();
        channels
            .iter()
            .any(Option::is_some)
            .then_some((pollfds, channels))
    }

    /// The channel `fd` names and its receive low-water mark, if any.
    fn polled_channel<G: Guest<Self>>(guest: &mut G, fd: RawFd) -> Option<PolledChannel> {
        guest
            .thread_state()
            .with_detfd(fd, |detfd| {
                detfd
                    .is_network_channel()
                    .then(|| (detfd.open_file_id(), detfd.network_lowat()))
            })
            .ok()
            .flatten()
    }

    /// Write the entries a channel wait reported back to the guest's pollfd
    /// array and count the ready ones, as `poll` and `ppoll` return.
    fn network_poll_result<G: Guest<Self>>(
        guest: &mut G,
        address: AddrMut<'_, libc::pollfd>,
        pollfds: &[libc::pollfd],
    ) -> Result<i64, Error> {
        guest
            .memory()
            .write_values(address, pollfds)
            .map_err(|_| Errno::EFAULT)?;
        Ok(pollfds.iter().filter(|pollfd| pollfd.revents != 0).count() as i64)
    }

    /// `poll` naming at least one channel.
    async fn network_poll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Poll,
    ) -> Result<i64, Error> {
        let address = call.fds().map(|fds| fds.cast::<libc::pollfd>());
        let Some((pollfds, channels)) =
            Self::network_pollfds(guest, address, poll_nfds(call.nfds()))
        else {
            return self.handle_poll(guest, call).await;
        };
        let address = address.expect("a pollfd array naming a channel");
        let deadline = millis_duration_to_absolute_timeout(guest, call.timeout()).await;
        let pollfds = self
            .network_wait(
                guest,
                Some(address),
                pollfds,
                &channels,
                call.timeout() == 0,
                deadline,
                call.signal_interrupt_errno(),
                Syscall::Poll(call),
                poll_ready,
            )
            .await?;
        Self::network_poll_result(guest, address, &pollfds)
    }

    /// `ppoll` naming at least one channel.
    async fn network_ppoll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Ppoll,
    ) -> Result<i64, Error> {
        let Some((pollfds, channels)) =
            Self::network_pollfds(guest, call.fds(), poll_nfds(call.nfds()))
        else {
            return self.handle_ppoll(guest, call).await;
        };
        if call.sigmask().is_some() {
            self.network_refuse(
                guest,
                "network trace does not model ppoll with a signal mask on a recorded socket",
                UNSUPPORTED_REMEDY,
            )
            .await
        }
        let timeout = match call.timeout() {
            Some(address) => Some(ppoll_timeout_duration(guest.memory().read_value(address)?)?),
            None => None,
        };
        let address = call.fds().expect("a pollfd array naming a channel");
        let started_at = thread_observe_time(guest).await;
        let result = self
            .network_wait(
                guest,
                Some(address),
                pollfds,
                &channels,
                timeout == Some(Duration::ZERO),
                timeout.map(|timeout| started_at + timeout),
                call.signal_interrupt_errno(),
                Syscall::Ppoll(call),
                poll_ready,
            )
            .await
            .and_then(|pollfds| Self::network_poll_result(guest, address, &pollfds));
        // Linux reports the time not slept, and leaves a zero timeout alone.
        if let (Some(address), Some(timeout)) = (call.timeout(), timeout)
            && !timeout.is_zero()
        {
            self.write_ppoll_remaining(guest, address, timeout, started_at)
                .await?;
        }
        result
    }

    /// `select` naming at least one channel.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#3804): Review select on a recorded socket.
    // https://github.com/rrnewton/hermit/issues/3804
    async fn network_select<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Select,
    ) -> Result<i64, Error> {
        let timeout = match call.timeout() {
            Some(address) => Some(select_timeout_duration(
                guest.memory().read_value(address)?,
            )?),
            None => None,
        };
        let started_at = thread_observe_time(guest).await;
        let deadline = timeout.map(|timeout| started_at + timeout);
        let result = self
            .network_select_wait(
                guest,
                call.nfds(),
                [call.readfds(), call.writefds(), call.exceptfds()],
                timeout,
                deadline,
                Syscall::Select(call),
            )
            .await;
        // Linux reports the time not slept, and leaves a zero timeout alone.
        if timeout.is_some_and(|timeout| !timeout.is_zero()) {
            self.write_select_remaining(guest, call, deadline).await?;
        }
        result
    }

    /// `pselect6` naming at least one channel.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#3804): Review pselect6 on a recorded socket.
    // https://github.com/rrnewton/hermit/issues/3804
    async fn network_pselect6<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Pselect6,
    ) -> Result<i64, Error> {
        // Linux copies the { sigmask, sigsetsize } wrapper first. Glibc's
        // `select` passes none and its `pselect` passes one even with no mask.
        let sigmask = match call.sigmask() {
            Some(argument) => {
                let argument: Pselect6SigmaskArg = guest.memory().read_value(argument.cast())?;
                argument.sigmask
            }
            None => 0,
        };
        let timeout = match call.timeout() {
            Some(address) => Some(ppoll_timeout_duration(guest.memory().read_value(address)?)?),
            None => None,
        };
        // Any signal mask pointer is refused, even one naming an empty set or
        // paired with a sigsetsize Linux would reject with EINVAL.
        if sigmask != 0 {
            self.network_refuse(
                guest,
                "network trace does not model pselect6 with a signal mask pointer on a \
                 recorded socket",
                UNSUPPORTED_REMEDY,
            )
            .await
        }
        let started_at = thread_observe_time(guest).await;
        let deadline = timeout.map(|timeout| started_at + timeout);
        let result = self
            .network_select_wait(
                guest,
                call.nfds(),
                [call.readfds(), call.writefds(), call.exceptfds()],
                timeout,
                deadline,
                Syscall::Pselect6(call),
            )
            .await;
        if timeout.is_some_and(|timeout| !timeout.is_zero()) {
            self.write_pselect6_remaining(guest, call, deadline).await?;
        }
        result
    }

    /// Wait on `select`'s descriptor sets as on the pollfd entries they name,
    /// then write the sets of ready descriptors back and return how many bits
    /// they hold, as Linux's `select` does. A descriptor that is not open
    /// when the call starts fails it with `EBADF`, before any wait or signal
    /// check, and leaves the sets alone; Linux checks its descriptor table
    /// once, up front, and counts an `O_PATH` descriptor as open.
    async fn network_select_wait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        nfds: i32,
        addresses: [Option<AddrMut<'_, libc::fd_set>>; 3],
        timeout: Option<Duration>,
        deadline: Option<LogicalTime>,
        retry: Syscall,
    ) -> Result<i64, Error> {
        if nfds > PSELECT6_INTERNAL_MAX_NFDS {
            self.network_refuse(
                guest,
                &format!(
                    "network trace does not model {} with nfds above \
                     {PSELECT6_INTERNAL_MAX_NFDS} on a recorded socket",
                    retry.name()
                ),
                UNSUPPORTED_REMEDY,
            )
            .await
        }
        let len = pselect6_fd_set_len(nfds)?;
        let mut sets = [None, None, None];
        for (set, address) in sets.iter_mut().zip(addresses) {
            *set = read_pselect6_fd_set(guest, address, len)?;
        }
        let pollfds = select_pollfds(nfds as usize, &sets);
        let channels: Vec<_> = pollfds
            .iter()
            .map(|pollfd| Self::polled_channel(guest, pollfd.fd))
            .collect();
        for (pollfd, channel) in pollfds.iter().zip(&channels) {
            if channel.is_none() {
                let call = syscalls::Fcntl::new()
                    .with_fd(pollfd.fd)
                    .with_cmd(FcntlCmd::F_GETFD);
                if let Err(Errno::EBADF) = guest.inject(call).await {
                    return Err(Errno::EBADF.into());
                }
            }
        }
        let pollfds = self
            .network_wait(
                guest,
                None,
                pollfds,
                &channels,
                timeout == Some(Duration::ZERO),
                deadline,
                Errno::EINTR,
                retry,
                select_ready,
            )
            .await?;
        let (result, count) = select_result(&pollfds, &sets, len);
        for (set, address) in result.iter().zip(addresses) {
            write_pselect6_fd_set(guest, address, set)?;
        }
        Ok(count)
    }

    /// Wait for readiness on pollfd entries naming channels, until `ready`
    /// accepts the reported entries or the timeout expires, and return them.
    /// Each scheduler turn asks the engine about the channels and probes every
    /// other descriptor with a zero-timeout host poll, as a strict
    /// guest-internal poll does; channel entries are masked to -1 for that
    /// probe, which Linux skips. The probe uses the guest's own pollfd array
    /// at `address` and restores it, or, with no array, the pull scratch,
    /// which then bounds the entries to `PSELECT6_INTERNAL_MAX_NFDS`.
    #[allow(clippy::too_many_arguments)]
    async fn network_wait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        address: Option<AddrMut<'_, libc::pollfd>>,
        mut pollfds: Vec<libc::pollfd>,
        channels: &[Option<PolledChannel>],
        zero_timeout: bool,
        deadline: Option<LogicalTime>,
        interrupted: Errno,
        retry: Syscall,
        ready: fn(&[libc::pollfd]) -> bool,
    ) -> Result<Vec<libc::pollfd>, Error> {
        let record = self.network_mode() == NetworkTraceMode::Record;
        let probe: Vec<libc::pollfd> = pollfds
            .iter()
            .zip(channels)
            .map(|(pollfd, channel)| libc::pollfd {
                fd: if channel.is_some() { -1 } else { pollfd.fd },
                events: pollfd.events,
                revents: 0,
            })
            .collect();
        let probe_host = probe.iter().any(|pollfd| pollfd.fd >= 0);
        let mut stack = guest.stack().await;
        let scratch: AddrMut<[u8; POLL_PULL_BYTES]> = stack.reserve();
        let _guard = stack.commit()?;
        let probe_address = match address {
            Some(address) => address,
            None => {
                assert!(probe.len() <= PSELECT6_INTERNAL_MAX_NFDS as usize);
                scratch.cast()
            }
        };
        let mut rsrc = Resources::new(guest.thread_state().dettid);
        rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
        rsrc.fyi("network poll");
        loop {
            if matches!(
                resource_request(guest, rsrc.clone()).await,
                ResumeStatus::Signaled(_)
            ) {
                return Err(interrupted.into());
            }
            let mut request = Vec::new();
            for (pollfd, channel) in pollfds.iter().zip(channels) {
                if let Some((id, lowat)) = channel {
                    let arrivals = if record {
                        self.network_pull(guest, pollfd.fd, scratch.cast(), POLL_PULL_BYTES)
                            .await?
                            .into_iter()
                            .collect()
                    } else {
                        Vec::new()
                    };
                    request.push((*id, *lowat, arrivals));
                }
            }
            let NetworkReply::Events(events) = self
                .network_request(guest, NetworkRequest::Readiness(request))
                .await
            else {
                unreachable!()
            };
            let mut host_revents = vec![0; pollfds.len()];
            if probe_host {
                guest
                    .memory()
                    .write_values(probe_address, &probe)
                    .map_err(|_| Errno::EFAULT)?;
                let call = syscalls::Poll::new()
                    .with_fds(Some(probe_address.cast()))
                    .with_nfds(probe.len() as libc::nfds_t)
                    .with_timeout(0);
                let probed = guest.inject(call).await;
                let mut after = probe.clone();
                let read = guest.memory().read_values(probe_address.into(), &mut after);
                // Restore the guest's descriptors before reporting anything.
                if let Some(address) = address {
                    guest
                        .memory()
                        .write_values(address, &pollfds)
                        .map_err(|_| Errno::EFAULT)?;
                }
                probed?;
                read?;
                for (revents, pollfd) in host_revents.iter_mut().zip(&after) {
                    *revents = pollfd.revents;
                }
            }
            let mut events = events.into_iter();
            for ((pollfd, channel), host) in pollfds.iter_mut().zip(channels).zip(host_revents) {
                pollfd.revents = match channel {
                    Some(_) => masked_revents(events.next().unwrap(), pollfd.events),
                    None => host,
                };
            }
            let expired = zero_timeout
                || match deadline {
                    Some(deadline) => thread_observe_time(guest).await >= deadline,
                    None => false,
                };
            if ready(&pollfds) || expired {
                return Ok(pollfds);
            }
            rsrc.poll_attempt += 1;
            record_retry_event(guest, retry).await;
        }
    }

    /// `shutdown` on a channel; record mode also shuts the host socket so
    /// the peer sees it.
    async fn network_shutdown<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Shutdown,
    ) -> Result<i64, Error> {
        let id = Self::network_id(guest, call.fd());
        if self.network_mode() == NetworkTraceMode::Record {
            let _ = guest.inject(call).await;
        }
        let NetworkReply::Errno(errno) = self
            .network_request(
                guest,
                NetworkRequest::Shutdown {
                    id,
                    how: call.how(),
                },
            )
            .await
        else {
            unreachable!()
        };
        if errno == 0 {
            Ok(0)
        } else {
            Err(Errno::new(errno).into())
        }
    }

    /// `getsockname` (`local`) or `getpeername` on a channel.
    async fn network_address<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        address: Option<AddrMut<'_, libc::sockaddr>>,
        length: Option<AddrMut<'_, libc::socklen_t>>,
        local: bool,
    ) -> Result<i64, Error> {
        let id = Self::network_id(guest, fd);
        let NetworkReply::Addresses(addresses) = self
            .network_request(guest, NetworkRequest::Addresses(id))
            .await
        else {
            unreachable!()
        };
        let (recorded_local, peer) = addresses.expect("network channel has addresses");
        let reported = match (local, recorded_local) {
            (true, Some(recorded_local)) => recorded_local,
            (true, None) => unspecified_like(&peer),
            (false, Some(_)) => peer,
            (false, None) => return Err(Errno::ENOTCONN.into()),
        };
        let bytes = sockaddr_bytes(&reported);
        let length = length.ok_or(Errno::EFAULT)?;
        let capacity: libc::socklen_t = guest.memory().read_value(length)?;
        if (capacity as i32) < 0 {
            return Err(Errno::EINVAL.into());
        }
        let copied = bytes.len().min(capacity as usize);
        if copied != 0 {
            guest
                .memory()
                .write_exact(address.ok_or(Errno::EFAULT)?.cast(), &bytes[..copied])
                .map_err(|_| Errno::EFAULT)?;
        }
        guest
            .memory()
            .write_value(length, &(bytes.len() as libc::socklen_t))?;
        Ok(0)
    }

    /// `getsockopt(SO_ERROR)` on a channel consumes the engine's pending error.
    async fn network_so_error<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Getsockopt,
    ) -> Result<i64, Error> {
        let length = call.optlen().ok_or(Errno::EFAULT)?;
        let capacity: libc::socklen_t = guest.memory().read_value(length)?;
        if (capacity as i32) < 0 {
            return Err(Errno::EINVAL.into());
        }
        let id = Self::network_id(guest, call.fd());
        let NetworkReply::Errno(errno) = self
            .network_request(guest, NetworkRequest::TakeError(id))
            .await
        else {
            unreachable!()
        };
        let bytes = errno.to_ne_bytes();
        let copied = bytes.len().min(capacity as usize);
        if copied != 0 {
            guest
                .memory()
                .write_exact(call.optval().ok_or(Errno::EFAULT)?.cast(), &bytes[..copied])
                .map_err(|_| Errno::EFAULT)?;
        }
        guest
            .memory()
            .write_value(length, &(copied as libc::socklen_t))?;
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sockaddr_round_trips_both_families() {
        let v4 = NetworkAddressV1::Inet4 {
            address: [192, 0, 2, 7],
            port: 8080,
        };
        let v6 = NetworkAddressV1::Inet6 {
            address: [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
            port: 443,
            flowinfo: 5,
            scope_id: 2,
        };
        for address in [v4, v6] {
            assert_eq!(
                address_from_sockaddr(&sockaddr_bytes(&address)),
                Some(address)
            );
        }
    }

    #[test]
    fn sockaddr_matches_the_libc_layout() {
        let bytes = sockaddr_bytes(&NetworkAddressV1::Inet4 {
            address: [127, 0, 0, 1],
            port: 0x1234,
        });
        // SAFETY: `bytes` is exactly one `sockaddr_in`.
        let native: libc::sockaddr_in = unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast()) };
        assert_eq!(native.sin_family, libc::AF_INET as libc::sa_family_t);
        assert_eq!(u16::from_be(native.sin_port), 0x1234);
        assert_eq!(native.sin_addr.s_addr.to_ne_bytes(), [127, 0, 0, 1]);
    }

    #[test]
    fn other_families_and_short_buffers_are_not_traced() {
        let mut unix = vec![0u8; size_of::<libc::sockaddr_un>()];
        unix[..2].copy_from_slice(&(libc::AF_UNIX as u16).to_ne_bytes());
        assert_eq!(address_from_sockaddr(&unix), None);
        let v4 = sockaddr_bytes(&NetworkAddressV1::Inet4 {
            address: [192, 0, 2, 7],
            port: 1,
        });
        assert_eq!(address_from_sockaddr(&v4[..v4.len() - 1]), None);
        assert_eq!(address_from_sockaddr(&[]), None);
    }

    #[test]
    fn lowat_normalisation_follows_linux() {
        assert_eq!(normalized_lowat(-1), i32::MAX as usize);
        assert_eq!(normalized_lowat(0), 1);
        assert_eq!(normalized_lowat(3), 3);
    }

    #[test]
    fn poll_reports_requested_events_and_the_unmaskable_ones() {
        let readiness = libc::POLLIN | libc::POLLRDNORM | libc::POLLOUT | libc::POLLWRNORM;
        assert_eq!(masked_revents(readiness, libc::POLLIN), libc::POLLIN);
        assert_eq!(
            masked_revents(libc::POLLOUT | libc::POLLHUP | libc::POLLERR, libc::POLLIN),
            libc::POLLHUP | libc::POLLERR
        );
    }

    #[test]
    fn select_sets_report_each_set_s_events_as_fs_select_c_does() {
        // Descriptor 1 is in the readable and exceptional sets, 3 in all
        // three, 9 in the writable set; 0 and 2 are in none.
        let set = |fds: &[usize]| {
            let mut set = vec![0u8; 2];
            for fd in fds {
                set[fd / 8] |= 1 << (fd % 8);
            }
            Some(set)
        };
        let sets = [set(&[1, 3]), set(&[3, 9]), set(&[1, 3])];
        let mut pollfds = select_pollfds(10, &sets);
        let read = SELECT_REQUESTED[0];
        let write = SELECT_REQUESTED[1];
        assert_eq!(
            pollfds
                .iter()
                .map(|pollfd| (pollfd.fd, pollfd.events))
                .collect::<Vec<_>>(),
            [
                (1, read | libc::POLLPRI),
                (3, read | write | libc::POLLPRI),
                (9, write)
            ]
        );
        assert!(!select_ready(&pollfds));

        // Readable 1 is not exceptional; readable and writable 3 is two bits;
        // hung-up 9 is not writable.
        pollfds[0].revents = libc::POLLIN | libc::POLLRDNORM;
        pollfds[1].revents = libc::POLLIN | libc::POLLOUT;
        pollfds[2].revents = libc::POLLHUP;
        assert!(select_ready(&pollfds));
        let (result, count) = select_result(&pollfds, &sets, 2);
        assert_eq!(result, [set(&[1, 3]), set(&[3]), set(&[])]);
        assert_eq!(count, 3);

        // A descriptor closed during the wait reports ready in every set it
        // is in; an unpassed set stays unpassed.
        pollfds[0].revents = libc::POLLNVAL;
        pollfds[1].revents = 0;
        pollfds[2].revents = 0;
        let sets = [sets[0].clone(), None, sets[2].clone()];
        let (result, count) = select_result(&pollfds, &sets, 2);
        assert_eq!(result, [set(&[1]), None, set(&[1])]);
        assert_eq!(count, 2);
    }

    #[test]
    fn host_pulls_classify_like_the_recorder() {
        assert_eq!(arrival_from_pull(Err(Errno::EAGAIN), &[]), None);
        assert_eq!(
            arrival_from_pull(Ok(0), &[]),
            Some(NetworkArrival::PeerWriteClosed)
        );
        assert_eq!(
            arrival_from_pull(Ok(2), b"abc"),
            Some(NetworkArrival::Bytes(b"ab".to_vec()))
        );
        assert_eq!(
            arrival_from_pull(Err(Errno::ECONNRESET), &[]),
            Some(NetworkArrival::Error(libc::ECONNRESET))
        );
    }

    #[test]
    fn sockets_outside_a_channel_are_refused_only_where_they_reach_the_network() {
        use NetworkSocketKind::*;
        let bind = Syscall::Bind(syscalls::Bind::new());
        let listen = Syscall::Listen(syscalls::Listen::new());
        let send = Syscall::Sendto(syscalls::Sendto::new());
        let fast_open =
            Syscall::Sendto(syscalls::Sendto::new().with_flags(libc::MSG_FASTOPEN as u32));
        let fast_open_option = Syscall::Setsockopt(
            syscalls::Setsockopt::new()
                .with_level(libc::IPPROTO_TCP)
                .with_optname(libc::TCP_FASTOPEN_CONNECT),
        );
        let read = Syscall::Read(syscalls::Read::new());
        for call in [&bind, &listen, &send, &fast_open] {
            assert!(reaches_network_outside_channel(call, InetOther), "{call:?}");
            assert!(!reaches_network_outside_channel(call, NotInet), "{call:?}");
        }
        for call in [&bind, &listen, &fast_open, &fast_open_option] {
            assert!(
                reaches_network_outside_channel(call, InetStream { ipv6: false }),
                "{call:?}"
            );
        }
        // An unconnected TCP socket fails these in Linux without a packet.
        for call in [&send, &read] {
            assert!(
                !reaches_network_outside_channel(call, InetStream { ipv6: false }),
                "{call:?}"
            );
        }
        assert!(!reaches_network_outside_channel(&read, InetOther));
    }

    #[test]
    fn a_channel_admits_only_descriptor_and_configuration_calls() {
        assert!(channel_call_takes_ordinary_path(&Syscall::Close(
            syscalls::Close::new()
        )));
        assert!(channel_call_takes_ordinary_path(&Syscall::Fcntl(
            syscalls::Fcntl::new()
        )));
        for call in [
            Syscall::Mmap(syscalls::Mmap::new()),
            Syscall::Preadv2(syscalls::Preadv2::new()),
            Syscall::Accept(syscalls::Accept::new()),
        ] {
            assert!(!channel_call_takes_ordinary_path(&call), "{call:?}");
        }
        assert!(is_configuration_option(
            libc::IPPROTO_TCP,
            libc::TCP_NODELAY
        ));
        assert!(is_configuration_option(libc::SOL_SOCKET, libc::SO_RCVTIMEO));
        for (level, name) in [
            (libc::IPPROTO_TCP, libc::TCP_INFO),
            (libc::IPPROTO_TCP, libc::TCP_MAXSEG),
            (libc::SOL_SOCKET, libc::SO_RCVBUF),
            (libc::SOL_SOCKET, libc::SO_ERROR),
        ] {
            assert!(!is_configuration_option(level, name), "{level} {name}");
        }
    }

    #[test]
    fn ipv6_addresses_without_a_scope_id_are_traced_as_linux_accepts_them() {
        let v6 = NetworkAddressV1::Inet6 {
            address: [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
            port: 443,
            flowinfo: 0,
            scope_id: 0,
        };
        let bytes = sockaddr_bytes(&v6);
        assert_eq!(
            address_from_sockaddr(&bytes[..SOCKADDR_IN6_RFC2133_LEN]),
            Some(v6)
        );
        assert_eq!(
            address_from_sockaddr(&bytes[..SOCKADDR_IN6_RFC2133_LEN - 1]),
            None
        );
    }

    #[test]
    fn sockaddr_copy_in_follows_linux() {
        use reverie::syscalls::LocalMemory;
        let memory = LocalMemory::new();
        let mut storage = [7u8; SOCKADDR_STORAGE_LEN + 1];
        let address = AddrMut::from_ptr(storage.as_mut_ptr().cast::<libc::sockaddr>());
        assert_eq!(read_sockaddr(&memory, address, 3), Ok(vec![7; 3]));
        assert_eq!(read_sockaddr(&memory, None, 0), Ok(Vec::new()));
        assert_eq!(read_sockaddr(&memory, None, 2), Err(Errno::EFAULT));
        assert_eq!(read_sockaddr(&memory, address, -1), Err(Errno::EINVAL));
        assert_eq!(
            read_sockaddr(&memory, address, SOCKADDR_STORAGE_LEN as i32).map(|b| b.len()),
            Ok(SOCKADDR_STORAGE_LEN)
        );
        assert_eq!(
            read_sockaddr(&memory, address, SOCKADDR_STORAGE_LEN as i32 + 1),
            Err(Errno::EINVAL)
        );
    }

    #[test]
    fn abstract_unix_addresses_are_recognised() {
        let family = (libc::AF_UNIX as u16).to_ne_bytes();
        let abstract_name = [family[0], family[1], 0];
        let path = [family[0], family[1], b'/'];
        assert!(names_abstract_unix_address(&abstract_name, 10, false));
        assert!(!names_abstract_unix_address(&path, 10, false));
        // A bare family autobinds to an abstract name, and fails a connect.
        assert!(names_abstract_unix_address(&family, 2, true));
        assert!(!names_abstract_unix_address(&family, 2, false));
        let inet = (libc::AF_INET as u16).to_ne_bytes();
        assert!(!names_abstract_unix_address(
            &[inet[0], inet[1], 0],
            16,
            false
        ));
        assert!(!names_abstract_unix_address(&[], 0, true));
    }

    #[test]
    fn rights_are_found_in_every_control_message() {
        fn message(level: i32, kind: i32, fds: &[RawFd]) -> Vec<u8> {
            let length = size_of::<Cmsghdr>() + size_of_val(fds);
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&length.to_ne_bytes());
            bytes.extend_from_slice(&level.to_ne_bytes());
            bytes.extend_from_slice(&kind.to_ne_bytes());
            fds.iter()
                .for_each(|fd| bytes.extend_from_slice(&fd.to_ne_bytes()));
            bytes.resize(length.next_multiple_of(size_of::<usize>()), 0);
            bytes
        }
        let mut control = message(libc::SOL_SOCKET, libc::SCM_CREDENTIALS, &[1, 2, 3]);
        control.extend(message(libc::SOL_SOCKET, libc::SCM_RIGHTS, &[5]));
        control.extend(message(libc::SOL_SOCKET, libc::SCM_RIGHTS, &[6, 7]));
        assert_eq!(received_descriptors(&control), vec![5, 6, 7]);
        // A truncated message carries nothing.
        assert_eq!(received_descriptors(&control[..control.len() - 8]), vec![5]);
        let mut zero_length = control.clone();
        zero_length[..8].copy_from_slice(&0usize.to_ne_bytes());
        assert_eq!(received_descriptors(&zero_length), Vec::<RawFd>::new());
        assert_eq!(received_descriptors(&[]), Vec::<RawFd>::new());
    }

    #[test]
    fn rights_control_length_is_not_truncated_to_wrapper_width() {
        let mut control = [0u8; 24];
        control[..8].copy_from_slice(&((1usize << 32) + 20).to_ne_bytes());
        control[8..12].copy_from_slice(&libc::SOL_SOCKET.to_ne_bytes());
        control[12..16].copy_from_slice(&libc::SCM_RIGHTS.to_ne_bytes());
        control[16..20].copy_from_slice(&42i32.to_ne_bytes());
        assert!(received_descriptors(&control).is_empty());
        // The identical low-width valid message must still yield its FD.
        control[..8].copy_from_slice(&20usize.to_ne_bytes());
        assert_eq!(received_descriptors(&control), vec![42]);
        control[..8].copy_from_slice(&usize::MAX.to_ne_bytes());
        assert!(received_descriptors(&control).is_empty());
    }

    #[test]
    fn every_timeout_but_zero_sets_one() {
        let timeout = |tv_sec, tv_usec| libc::timeval { tv_sec, tv_usec };
        assert!(sets_a_timeout(timeout(1, 0)));
        assert!(sets_a_timeout(timeout(0, 1)));
        assert!(!sets_a_timeout(timeout(0, 0)));
        assert!(sets_a_timeout(timeout(-1, 5)));
        assert!(sets_a_timeout(timeout(-1, 0)));
        assert!(!sets_a_timeout(timeout(1, 1_000_000)));
        assert!(!sets_a_timeout(timeout(1, -1)));
        for option in [libc::SO_RCVTIMEO, libc::SO_SNDTIMEO] {
            assert!(TIMEOUT_OPTIONS.contains(&option));
        }
    }

    #[test]
    fn signal_driven_io_requests_are_recognised() {
        let fcntl = |cmd| Syscall::Fcntl(syscalls::Fcntl::new().with_cmd(cmd));
        let ioctl = |request| Syscall::Ioctl(syscalls::Ioctl::new().with_request(request));
        for call in [
            fcntl(FcntlCmd::F_SETFL(libc::O_ASYNC | libc::O_NONBLOCK)),
            fcntl(FcntlCmd::F_SETOWN),
            fcntl(FcntlCmd::F_SETSIG(libc::SIGIO)),
            ioctl(Request::FIOASYNC(None)),
            ioctl(Request::FIOSETOWN(None)),
            ioctl(Request::SIOCSPGRP(None)),
        ] {
            assert!(requests_signal_driven_io(&call), "{call:?}");
        }
        for call in [
            fcntl(FcntlCmd::F_SETFL(libc::O_NONBLOCK)),
            fcntl(FcntlCmd::F_GETFL),
            ioctl(Request::FIONREAD(None)),
        ] {
            assert!(!requests_signal_driven_io(&call), "{call:?}");
        }
    }

    #[test]
    fn interface_and_route_ioctls_are_recognised() {
        let ioctl = |request| syscalls::Ioctl::new().with_request(request);
        let raw = |request| ioctl(Request::from_raw(request, 0));
        for call in [
            ioctl(Request::SIOCGIFINDEX(None)),
            // SIOCADDRT, SIOCGIFNAME, SIOCGIFCONF, SIOCGIFADDR, SIOCSIFFLAGS,
            // SIOCGSKNS and the last device-private request.
            raw(0x890B),
            raw(0x8910),
            raw(0x8912),
            raw(0x8915),
            raw(0x8914),
            raw(0x894C),
            raw(0x89FF),
        ] {
            assert!(requests_interface_state(&call), "{call:?}");
        }
        for call in [
            ioctl(Request::SIOCETHTOOL(None)),
            ioctl(Request::SIOCGSTAMP(None)),
            ioctl(Request::SIOCSPGRP(None)),
            ioctl(Request::FIONREAD(None)),
            ioctl(Request::FIONBIO(None)),
            raw(0x890A),
            raw(0x8A00),
        ] {
            assert!(!requests_interface_state(&call), "{call:?}");
        }
    }

    #[test]
    fn host_network_state_paths_are_recognised() {
        for path in [
            "/proc/net",
            "/proc/net/dev",
            "/proc/self/net/route",
            "/proc/thread-self/net/if_inet6",
            "/proc/42/net/tcp",
            "/proc/42/task/43/net/snmp",
            "/proc/self/task/43/net/dev",
            "/proc/self/../net/dev",
            "/proc/sys/net/core/somaxconn",
            "/sys/class/net",
            "/sys/class/net/lo/address",
            "/sys/devices/virtual/net/lo/operstate",
            "/sys/devices/pci0000:00/0000:00:03.0/net/eth0/address",
        ] {
            assert!(names_host_network_state(Path::new(path)), "{path}");
        }
        for path in [
            "/proc/self/status",
            "/proc/42/task/43/stat",
            "/proc/sys/kernel/hostname",
            "/proc/self/netfilter",
            "/proc/abc/net/dev",
            "/sys/class/block",
            "/srv/net/dev",
            "/net/dev",
            "/proc/net/../cpuinfo",
        ] {
            assert!(!names_host_network_state(Path::new(path)), "{path}");
        }
    }
}
