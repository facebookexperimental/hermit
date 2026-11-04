/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Deterministic file descriptor

use std::collections::VecDeque;
use std::fmt;
use std::hash::Hash;
use std::hash::Hasher;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

use nix::fcntl::OFlag;
use reverie::syscalls::Errno;
use serde::Deserialize;
use serde::Serialize;

use crate::dirents::DirEntry;
use crate::dirents::DirectoryOrder;
use crate::dirents::DirectoryStream;
use crate::procfs::ProcfsFile;
use crate::procfs::ProcfsSnapshotContext;
use crate::procfs::TimerSlackReadPreview;
use crate::procfs_inode::DirectoryInodes;
use crate::resources::ResourceID;
use crate::stat::*;
use crate::tool_global::SnapshotMark;
use crate::types::RawFd;
use crate::types::*;

/// file descriptor type
#[derive(
    PartialEq,
    Eq,
    Debug,
    Default,
    Clone,
    Copy,
    Hash,
    Serialize,
    Deserialize
)]
pub enum FdType {
    /// Regular fd, such as from openat
    #[default]
    Regular,
    /// signalfd
    Signalfd,
    /// eventfd
    Eventfd,
    /// timerfd
    Timerfd,
    /// inotify instance
    Inotify,
    /// epoll instance (from epoll_create/epoll_create1)
    Epoll,
    /// socket fd
    Socket,
    /// pipe fd
    Pipe,
    /// memfd
    Memfd,
    /// pidfd
    Pidfd,
    /// userfaultfd
    Userfaultfd,
    /// Random-number generator device
    Rng,
}

/// Whether a file of mode `mode` and device number `rdev` is one of Linux's
/// random devices: character device 1:8 (`/dev/random`) or 1:9
/// (`/dev/urandom`). Linux names the device by this number, whatever path
/// reached it (a relative or symbolic-link spelling, `/proc/self/fd/N`, a bind
/// mount or a copy of the node), so Detcore classifies the object, not the
/// spelling (https://github.com/rrnewton/hermit/issues/4544).
pub(crate) fn is_random_device(mode: libc::mode_t, rdev: libc::dev_t) -> bool {
    mode & libc::S_IFMT == libc::S_IFCHR
        && libc::major(rdev) == 1
        && matches!(libc::minor(rdev), 8 | 9)
}

/// Which of Hermit's random-device streams an open file description reads.
///
/// Every read of a Linux random device draws fresh bytes, so no two opens read
/// the same ones. Detcore gives each open file description of a random device
/// a stream of its own, named by the task that opened it and by how many
/// random devices its descriptor table had opened before. Aliases of the
/// description (`dup`, `fork`) share the stream and its cursor. The sequence
/// counts only the guest's own opens of random devices, which reach Detcore
/// on every backend, so the name does not depend on what else a backend's
/// loader opens; [`OpenFileId`]'s sequence counts those opens too. A
/// description Detcore discovers rather than sees opened is named by its
/// descriptor number instead ([`RandomDeviceStream::discovered`]), so that a
/// backend that discovers descriptors does not shift the names of later opens.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RandomDeviceStream {
    pub(crate) creator: DetTid,
    pub(crate) sequence: u64,
}

impl RandomDeviceStream {
    /// The sequences of discovered descriptions, apart from the count's.
    const DISCOVERED: u64 = 1 << 63;

    /// The name of a random device found at `fd` of a table owned by `owner`.
    pub(crate) fn discovered(owner: DetTid, fd: RawFd) -> Self {
        Self {
            creator: owner,
            sequence: Self::DISCOVERED | fd as u32 as u64,
        }
    }
}

/// How an active network trace mode classifies a socket's family and type
/// (see `crate::syscalls::network_trace`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum NetworkSocketKind {
    /// Not an IPv4 or IPv6 socket, or no network trace mode was active.
    #[default]
    NotInet,
    /// An IPv4 or IPv6 TCP stream socket. `connect` checks the address
    /// against the socket's family, as Linux does.
    InetStream {
        /// Whether the socket is `AF_INET6`.
        ipv6: bool,
    },
    /// Any other IPv4 or IPv6 socket, such as UDP or raw.
    InetOther,
}

/// `IPPROTO_L2TP` (include/uapi/linux/in.h), which the libc crate does not
/// define.
const IPPROTO_L2TP: i32 = 115;

/// Whether a receive with `MSG_ERRQUEUE` on a socket that `socket(family,
/// socket_type, protocol)` created never waits on Linux: its protocol reads the
/// error queue and fails with `EAGAIN` when that is empty, even on a blocking
/// socket. These do so: IPv4 and IPv6 TCP, UDP, UDP-Lite, ICMP ping and raw
/// sockets (`tcp_recvmsg`, `udp_recvmsg`, `udpv6_recvmsg`, `ping_recvmsg`,
/// `raw_recvmsg`, `rawv6_recvmsg`, through `ip_recv_error` and
/// `ipv6_recv_error`), MPTCP and SCTP (`mptcp_recvmsg`, `sctp_recvmsg`), IPv6
/// L2TP (`l2tp_ip6_recvmsg`), and every packet socket (`packet_recvmsg`,
/// through `sock_recv_errqueue`). Every other socket keeps Detcore's polling:
/// AF_UNIX and AF_NETLINK ignore the flag and wait as for a plain receive, so
/// does IPv4 L2TP (`l2tp_ip_recvmsg`), and no other protocol or family is
/// modelled.
pub(crate) fn error_queue_read_never_waits(family: i32, socket_type: i32, protocol: i32) -> bool {
    // Linux's SOCK_TYPE_MASK: the type without SOCK_NONBLOCK and SOCK_CLOEXEC.
    let socket_type = socket_type & 0xf;
    match family {
        libc::AF_INET | libc::AF_INET6 => {
            let ping = if family == libc::AF_INET {
                libc::IPPROTO_ICMP
            } else {
                libc::IPPROTO_ICMPV6
            };
            match socket_type {
                libc::SOCK_STREAM => matches!(
                    protocol,
                    0 | libc::IPPROTO_TCP | libc::IPPROTO_MPTCP | libc::IPPROTO_SCTP
                ),
                // SCTP is the only seqpacket protocol, so 0 selects it.
                libc::SOCK_SEQPACKET => matches!(protocol, 0 | libc::IPPROTO_SCTP),
                libc::SOCK_DGRAM => {
                    matches!(protocol, 0 | libc::IPPROTO_UDP | libc::IPPROTO_UDPLITE)
                        || protocol == ping
                        || (family == libc::AF_INET6 && protocol == IPPROTO_L2TP)
                }
                // Every protocol of a raw socket reaches raw_recvmsg or rawv6_recvmsg.
                libc::SOCK_RAW => true,
                _ => false,
            }
        }
        libc::AF_PACKET => true,
        _ => false,
    }
}

/// Read provenance authenticated at the container's initial image: a terminal
/// checked with TCGETS, the launcher's actually created controller input pipe,
/// or a readable pipe end the initial image holds on fds 0-2
/// (`FileMetadata::discover_inherited_pipes`).
///
/// The original open-file identity distinguishes aliases from separate opens
/// of the same tty. The raw identity is used only to validate an exec handoff;
/// it is never a guest-visible inode or a scheduler resource.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct InheritedExternalRead {
    pub(crate) origin: OpenFileId,
    pub(crate) dev: u64,
    pub(crate) inode: u64,
    pub(crate) rdev: u64,
    pub(crate) file_type: u32,
    pub(crate) access_mode: i32,
    pub(crate) resource: Option<ResourceID>,
}

impl std::fmt::Debug for InheritedExternalRead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Kernel identity authenticates a private handoff; distinct launches
        // must not compare their host pipe/terminal inode numbers.
        f.debug_struct("InheritedExternalRead")
            .field("origin", &self.origin)
            .field("file_type", &self.file_type)
            .field("access_mode", &self.access_mode)
            .field("resource", &self.resource)
            .finish_non_exhaustive()
    }
}

/// Deterministic file descriptor
///
/// Notice `statbuf` can be cached here, this is because
/// `stat` is valid as long as fd stays open.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetFd {
    /// underlying file descriptor
    pub(crate) fd: RawFd,
    /// Per-slot descriptor flags, currently only `O_CLOEXEC`.
    fd_flags: i32,
    /// State shared by every descriptor referring to the same Linux `struct file`.
    open_file: Arc<Mutex<OpenFileState>>,
}

/// A handle on an open file description that Detcore models, which does not
/// keep it alive: once no modeled descriptor refers to the description, the
/// handle reaches nothing. Where one tool process serves every guest process
/// (ptrace, KVM), a rendered procfs descriptor that reaches the same Linux
/// `struct file` over `SCM_RIGHTS` joins the description through it where no
/// queued message Detcore can trust names it ([`InFlightOpenFile`] covers
/// those): when it is received outside the turn, or after a receive Detcore
/// could not match, or first named (`Detcore::adopt_received_placeholders`).
#[derive(Clone, Debug)]
pub(crate) struct OpenFileHandle(std::sync::Weak<Mutex<OpenFileState>>);

impl OpenFileHandle {
    /// A handle that reaches no description, for tests.
    #[cfg(test)]
    pub(crate) fn dangling() -> Self {
        Self(std::sync::Weak::new())
    }

    /// Descriptor `fd` of the description, if any modeled descriptor still
    /// refers to it: an alias, sharing its cursor, snapshot and status flags,
    /// with `FD_CLOEXEC` as `cloexec` says.
    pub(crate) fn descriptor(&self, fd: RawFd, cloexec: bool) -> Option<DetFd> {
        let open_file = self.0.upgrade()?;
        let mut detfd = DetFd {
            fd,
            fd_flags: 0,
            open_file,
        };
        detfd.set_cloexec(cloexec);
        Some(detfd)
    }

    /// Whether the description is a socket a guest made in this run and
    /// never passed out of the guest ([`DetFd::is_guest_created`]); false once
    /// no modeled descriptor refers to it.
    pub(crate) fn is_guest_created(&self) -> bool {
        self.with_description(|d| d.guest_created).unwrap_or(false)
    }

    /// Runs `f` on the description's model, as [`DetFd::with_description`]
    /// does, if any modeled descriptor still refers to it.
    fn with_description<R>(&self, f: impl FnOnce(&mut OpenFileDescription) -> R) -> Option<R> {
        let open_file = self.0.upgrade()?;
        let mut state = open_file.lock().expect("open file mutex poisoned");
        Some(match &mut *state {
            OpenFileState::Local(description) => f(description),
            OpenFileState::Shared(shared) => with_shared_description(shared.id, f),
        })
    }
}

/// An open file description that a queued `SCM_RIGHTS` message carries
/// (`crate::syscalls::scm_rights`), made by [`DetFd::in_flight_open_file`].
#[derive(Clone, Debug)]
pub(crate) struct InFlightOpenFile(InFlight);

/// How an [`InFlightOpenFile`] refers to its description.
#[derive(Clone, Debug)]
enum InFlight {
    /// Held as Linux holds the `struct file` of each descriptor a message
    /// carries: from the send until the receive installs it or Linux
    /// releases the message. It keeps the description alive, so the receipt
    /// names the description the sender's table modeled even when every
    /// modeled descriptor of it has been closed by then, or lives only in the
    /// table of a sender that has exited, whose state the backend releases
    /// at no particular point of the guest's schedule. Whether it still
    /// holds the description decides nothing: the description's count of
    /// copies in flight does ([`OpenFileDescription::in_flight`]), which only
    /// guest calls change.
    Held(Arc<Mutex<OpenFileState>>),
    /// Under record and replay: the ownership existing recordings were made
    /// with, which keeps nothing alive ([`OpenFileHandle`]). A receipt there
    /// joins only a description the receiving table holds anyway, and no
    /// other decision may see a description only a queued message keeps.
    Legacy(std::sync::Weak<Mutex<OpenFileState>>),
}

impl InFlightOpenFile {
    /// Descriptor `fd` of the description: an alias, sharing its cursor,
    /// snapshot and status flags, with `FD_CLOEXEC` as `cloexec` says. `None`
    /// only for a legacy reference (`InFlight::Legacy`) whose description is
    /// gone.
    pub(crate) fn descriptor(&self, fd: RawFd, cloexec: bool) -> Option<DetFd> {
        let open_file = match &self.0 {
            InFlight::Held(open_file) => Arc::clone(open_file),
            InFlight::Legacy(open_file) => open_file.upgrade()?,
        };
        let mut detfd = DetFd {
            fd,
            fd_flags: 0,
            open_file,
        };
        detfd.set_cloexec(cloexec);
        Some(detfd)
    }

    /// A queue took this copy in flight (`DetFd::queue_rights_to_peer`):
    /// the description counts it until a guest call receives or releases
    /// it ([`OpenFileDescription::in_flight`]).
    fn enter_flight(&self) {
        if let InFlight::Held(open_file) = &self.0 {
            with_state_description(open_file, |d| d.in_flight += 1);
        }
    }

    /// A receive installed this copy in the receiving table, which now holds
    /// the description as a descriptor of its own: it is no longer in
    /// flight. A peek leaves the message, and its copy, queued.
    pub(crate) fn land(&self) {
        if let InFlight::Held(open_file) = &self.0 {
            with_state_description(open_file, |d| d.in_flight = d.in_flight.saturating_sub(1));
        }
    }

    /// Release this copy in flight, as a guest call made Linux release it
    /// (`Detcore::settle_released_rights`): the description no longer counts
    /// it. If no copy is in flight any more and no modeled descriptor names
    /// the description (this is its last reference), the description goes
    /// with it, as Linux's last `fput` releases the `struct file`: its
    /// identity and type, and the descriptors of the messages queued to it,
    /// which go with it in turn. A description that still counts a copy in
    /// flight is never released, whether or not anything still holds that
    /// copy: one that went with a table the backend released, or past
    /// [`MAX_QUEUED_RIGHTS_MESSAGES`], stays counted for the rest of the run.
    pub(crate) fn release(self) -> Option<(OpenFileId, FdType, Vec<SentDescriptor>)> {
        let InFlight::Held(open_file) = self.0 else {
            return None;
        };
        let in_flight = with_state_description(&open_file, |d| {
            d.in_flight = d.in_flight.saturating_sub(1);
            d.in_flight
        });
        if in_flight > 0 {
            return None;
        }
        let state = Arc::into_inner(open_file)?;
        let state = state.into_inner().expect("open file mutex poisoned");
        match state {
            OpenFileState::Local(mut description) => {
                let queued = std::mem::take(&mut description.rights.queued);
                let descriptors = queued
                    .into_iter()
                    .flat_map(|message| message.descriptors)
                    .collect();
                Some((description.id, description.ty, descriptors))
            }
            OpenFileState::Shared(shared) => {
                let ty = with_shared_description(shared.id, |description| description.ty);
                Some((shared.id, ty, Vec::new()))
            }
        }
    }

    /// Let go of this copy without any guest call to say Linux released it:
    /// its queue went with a table the backend released, or with a
    /// description that went that way. The description keeps counting the
    /// copy in flight, so nothing is released in the model; if this was its
    /// last reference, the messages queued to it, for the caller to take
    /// apart in turn without recursing.
    fn into_queue_if_last(self) -> Vec<QueuedMessage> {
        let InFlight::Held(open_file) = self.0 else {
            return Vec::new();
        };
        let Some(state) = Arc::into_inner(open_file) else {
            return Vec::new();
        };
        match state.into_inner().expect("open file mutex poisoned") {
            OpenFileState::Local(mut description) => {
                std::mem::take(&mut description.rights.queued).into()
            }
            OpenFileState::Shared(_) => Vec::new(),
        }
    }
}

/// Runs `f` on the model of the description `open_file` names, as
/// [`DetFd::with_description`] does.
fn with_state_description<R>(
    open_file: &Arc<Mutex<OpenFileState>>,
    f: impl FnOnce(&mut OpenFileDescription) -> R,
) -> R {
    let mut state = open_file.lock().expect("open file mutex poisoned");
    match &mut *state {
        OpenFileState::Local(description) => f(description),
        OpenFileState::Shared(shared) => with_shared_description(shared.id, f),
    }
}

/// The kernel's name for the file behind a descriptor: `(st_dev, st_ino)`, as
/// `fstat` reports it. Every descriptor of one open file description has it.
pub(crate) type FileIdentity = (u64, u64);

/// One descriptor an `SCM_RIGHTS` message carried, as the sender's table knew
/// it when the message was sent (`crate::syscalls::scm_rights`).
#[derive(Clone, Debug)]
pub(crate) struct SentDescriptor {
    /// The file it names.
    pub(crate) file: FileIdentity,
    /// The open file description the sender's table modeled for it, if any
    /// ([`InFlightOpenFile`]).
    pub(crate) open_file: Option<InFlightOpenFile>,
    /// The description, where `procfs::locks::LockLedger` counts this
    /// descriptor as a copy in flight of a description whose locks it
    /// follows, which keeps those locks while the message is queued, as
    /// Linux's in-flight reference keeps them: the receipt
    /// (`PlaceholderCopies::Received`) or the release of this descriptor
    /// (`Detcore::settle_released_rights`) uncounts it.
    pub(crate) lock_copy: Option<OpenFileId>,
    /// Whether a receive Detcore could not match to its queue may have taken
    /// this descriptor's message unseen (`SocketRights::suspects`), when the
    /// receiving socket's last close released it with the queue
    /// ([`DetFd::take_all_queued_rights`]): the receive may hold the
    /// description under an identity Detcore does not know, so its copy's
    /// release cannot be placed (`flock_model`).
    pub(crate) suspect: bool,
}

/// The most messages kept queued to one socket ([`SocketRights::queued`]):
/// far more than a receiver leaves unread in practice. Beyond it the oldest
/// is dropped from the queue, and its receipt names no open file. Linux still
/// holds that message, so nothing it carries is released: its descriptions
/// keep counting their copies in flight ([`OpenFileDescription::in_flight`])
/// for the rest of the run.
const MAX_QUEUED_RIGHTS_MESSAGES: usize = 4096;

/// One message queued to a socket ([`SocketRights::queued`]).
#[derive(Debug, Default)]
pub(crate) struct QueuedMessage {
    /// The descriptors its `SCM_RIGHTS` carried, in the order sent: none for
    /// a message of data alone, which only a queue that holds every message
    /// keeps (`SocketRights::complete`).
    pub(crate) descriptors: Vec<SentDescriptor>,
    /// The bytes it carried; for a stream socket's message, those no read has
    /// taken yet.
    pub(crate) bytes: usize,
}

/// The `SCM_RIGHTS` bookkeeping of one `AF_UNIX` socket: its peer, where
/// Detcore knows it (the other socket of a `socketpair`, or of a connection
/// it saw made and accepted), and the descriptors queued to it
/// (`crate::syscalls::scm_rights`).
#[derive(Debug, Default)]
struct SocketRights {
    /// The socket's type (`SOCK_STREAM`, `SOCK_SEQPACKET`, `SOCK_DGRAM`), as
    /// `socket` or `accept` made it; `None` for one Detcore did not see made.
    unix_type: Option<i32>,
    /// The name a `bind` gave it (`crate::syscalls::scm_rights::unix_socket_name`),
    /// and which bind of that name it was, by which a listener's connections
    /// are found (`crate::syscalls::scm_rights::UnixConnections`).
    unix_name: Option<(Box<[u8]>, u64)>,
    /// The socket at the other end, which this one's messages reach.
    peer: Option<OpenFileHandle>,
    /// Whether the pair is `SOCK_STREAM`, where a send of no bytes carries
    /// no descriptor.
    stream: bool,
    /// Each message sent to this socket with `SCM_RIGHTS`, and, where the
    /// queue holds every message (`Self::complete`), each one of data alone,
    /// not yet known to be received, oldest first. Each holds the
    /// descriptions its message carries ([`InFlightOpenFile`]) until a
    /// receive takes the message or a later one (`DetFd::take_queued_rights`,
    /// `DetFd::take_head_message`), Linux purges the queue
    /// (`DetFd::unpair_rights_peer`), or this socket's description goes with
    /// its last reference (`DetFd::take_all_queued_rights`).
    queued: VecDeque<QueuedMessage>,
    /// Set once a message may have reached this socket without being
    /// queued: one sent to it out of Detcore's sight, out of the queue's
    /// order, or past its bound (`DetFd::lose_peer_lockstep`), or one a
    /// receive took that names no queued message, or a later one than Linux
    /// can have delivered (`DetFd::take_queued_rights`). A call that took
    /// messages Detcore could not match does not set it: those messages are
    /// suspect instead (`Self::suspects`), except under record and replay,
    /// where such a call sets it, as existing recordings were made with
    /// (`DetFd::lose_lockstep_if_queued`). While it is set, only order could
    /// tell an anonymous inode's queued descriptor from an unqueued one's, as
    /// every anonymous inode has one file identity, so none is joined
    /// (`Detcore::record_received_facts`). Never regained: a message sent
    /// without being queued may still be in flight however the queue looks.
    lockstep_lost: bool,
    /// How many messages at the front of the queue a call may have taken
    /// without Detcore matching them, so that Linux may have released them
    /// already ([`DetFd::lose_lockstep_if_queued`]). Each such call takes one
    /// message, the oldest Linux holds, so only the first of these can be
    /// gone after one call, the first two after two, and so on: a message
    /// past them is still queued in Linux. A receipt that matches one of
    /// them is trusted only where no later one among them names the same
    /// files ([`DetFd::take_queued_rights`]), so a message Linux released
    /// is never joined.
    suspects: usize,
    /// Whether the queue holds every message Linux queued to this socket,
    /// those of data alone included, oldest first, so that a call that takes
    /// one message without naming its descriptors (a `read`) took the
    /// oldest queued one (`DetFd::take_head_message`), or on a stream socket
    /// the bytes it reports from the oldest messages on
    /// (`DetFd::take_stream_bytes`): set for both sockets of a `socketpair`,
    /// which nothing but each other can reach while neither is bound to a
    /// name, and only where Detcore notes every send of data alone (not under
    /// record and replay, `Detcore::note_send_without_control`).
    /// Cleared for good ([`give_up_completeness`]) when the queue loses
    /// lockstep, when the socket gets a name another socket can send to (a
    /// `bind`, or `SO_PASSCRED`, which names it at its next send), and when
    /// either socket of the pair is passed to another table, which may hold
    /// it as an open file of its own (`DetFd::give_up_pair_completeness`).
    complete: bool,
    /// Whether only its peer can reach this socket, through sends Detcore
    /// sees: set with `Self::complete` for both sockets of a `socketpair`
    /// (`DetFd::mark_complete_pair`), and cleared for good when the socket
    /// gets a name another socket can send to (a `bind`, or `SO_PASSCRED`,
    /// `DetFd::set_unix_name`, `DetFd::give_up_completeness`) or either
    /// socket of the pair is passed to another table
    /// (`DetFd::give_up_pair_completeness`). Unlike `Self::complete`, losing
    /// track of which message a receive took leaves it set.
    sealed: bool,
    /// Whether a message sent to this socket carried descriptors, or may
    /// have without being queued (`DetFd::lose_peer_lockstep_with_rights`).
    rights_sent: bool,
}

/// Whether `received`, the identities of the descriptors one receive
/// installed from one message, are those `sent` queued, in order: all of
/// them, or a prefix where Linux set `MSG_CTRUNC` (`truncated`) because the
/// control buffer had room for fewer.
fn names_sent_rights(sent: &[SentDescriptor], received: &[FileIdentity], truncated: bool) -> bool {
    !received.is_empty()
        && (received.len() == sent.len() || (truncated && received.len() < sent.len()))
        && received
            .iter()
            .zip(sent)
            .all(|(received, sent)| *received == sent.file)
}

/// Where a stream read of some bytes from a queue that holds every message
/// (`SocketRights::complete`) leaves it ([`stream_walk`]).
struct StreamWalk {
    /// How many messages, from the oldest, it takes in full.
    taken: usize,
    /// Of the message after those, how many bytes it takes.
    part: usize,
    /// The message whose descriptors it detaches, if any: the first it
    /// reaches that still carries some.
    detached: Option<usize>,
}

/// How a stream read of `read` bytes takes from `rights`'s queue, which holds
/// every message, as Linux takes it (`unix_stream_read_generic`,
/// <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/net/unix/af_unix.c?h=v6.17#n3067>):
/// bytes from the oldest messages on, a message's descriptors detached with
/// the first of its bytes a read takes, and no byte past a message whose
/// descriptors it detached. A read of no bytes that reached the socket (a
/// `recvfrom` of none) detaches the oldest message's descriptors and takes
/// nothing. `None` where the queue cannot be what Linux read from: it holds
/// fewer bytes, or the read went past a message whose descriptors it
/// detached.
fn stream_walk(rights: &SocketRights, read: usize) -> Option<StreamWalk> {
    if read == 0 {
        let detached = rights
            .queued
            .front()
            .filter(|head| !head.descriptors.is_empty())
            .map(|_| 0);
        return Some(StreamWalk {
            taken: 0,
            part: 0,
            detached,
        });
    }
    let mut left = read;
    let mut detached = None;
    for (index, message) in rights.queued.iter().enumerate() {
        if detached.is_some() {
            return None;
        }
        if !message.descriptors.is_empty() {
            detached = Some(index);
        }
        if message.bytes > left {
            return Some(StreamWalk {
                taken: index,
                part: left,
                detached,
            });
        }
        left -= message.bytes;
        if left == 0 {
            return Some(StreamWalk {
                taken: index + 1,
                part: 0,
                detached,
            });
        }
    }
    None
}

/// How many bytes a stream read that asked for `asked` takes from `rights`'s
/// queue, which holds every message, as Linux takes them
/// (`unix_stream_read_generic`,
/// <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/net/unix/af_unix.c?h=v6.17#n3067>):
/// bytes from the oldest messages on, until it has `asked`, or has taken
/// bytes of a message that carried descriptors (it stops after the message
/// whose descriptors it detached), or the queue holds no more.
fn stream_read_count(rights: &SocketRights, asked: usize) -> usize {
    let mut read = 0;
    for message in &rights.queued {
        let part = message.bytes.min(asked - read);
        read += part;
        if read == asked || part < message.bytes || !message.descriptors.is_empty() {
            break;
        }
    }
    read
}

/// Takes from `rights`'s queue what `walk` says a stream read took, and
/// returns the descriptors it detached.
fn apply_stream_walk(rights: &mut SocketRights, walk: StreamWalk) -> Vec<SentDescriptor> {
    let detached = walk
        .detached
        .and_then(|index| rights.queued.get_mut(index))
        .map(|message| std::mem::take(&mut message.descriptors))
        .unwrap_or_default();
    rights.queued.drain(..walk.taken);
    if let Some(partly) = rights.queued.front_mut() {
        partly.bytes -= walk.part;
    }
    detached
}

/// The queue of `rights` no longer holds every message Linux queued to its
/// socket (`SocketRights::complete`): it keeps only the messages that carried
/// descriptors, the only ones a receipt can name, and the suspects among them.
fn give_up_completeness(rights: &mut SocketRights) {
    if !rights.complete {
        return;
    }
    rights.complete = false;
    let suspects = rights.suspects;
    let mut position = 0;
    let mut kept_suspects = 0;
    rights.queued.retain(|message| {
        let keep = !message.descriptors.is_empty();
        if keep && position < suspects {
            kept_suspects += 1;
        }
        position += 1;
        keep
    });
    rights.suspects = kept_suspects;
}

/// A socket queued on a socket that is itself queued makes a chain of held
/// descriptions, as long as a guest cares to build. Dropped recursively, a
/// long one would overflow the tool thread's stack, so the queue is drained
/// here and each description it held last is taken apart in turn, its own
/// queue joining the work, as Linux defers each `fput` rather than recursing.
///
/// A queue dropped this way goes with a table the backend released, or with a
/// description that went that way, at no particular point of the guest's
/// schedule: it releases nothing in the model, so the descriptions it held
/// keep counting their copies in flight ([`InFlightOpenFile::into_queue_if_last`]).
impl Drop for SocketRights {
    fn drop(&mut self) {
        let mut pending: Vec<QueuedMessage> = self.queued.drain(..).collect();
        while let Some(message) = pending.pop() {
            for sent in message.descriptors {
                if let Some(open_file) = sent.open_file {
                    pending.extend(open_file.into_queue_if_last());
                }
            }
        }
    }
}

/// What a receive took from a socket's queue ([`DetFd::take_queued_rights`]).
#[derive(Debug)]
pub(crate) struct TakenRights {
    /// The descriptors of the message the receive took, in the order sent.
    pub(crate) sent: Vec<SentDescriptor>,
    /// Whether the queue still followed the socket's messages one for one
    /// (`SocketRights::lockstep_lost`).
    pub(crate) lockstep: bool,
    /// Whether the receipt may join the descriptions the message holds: it
    /// is a message Linux can still have held when the receive ran
    /// (`SocketRights::suspects`). Otherwise the receipt may be of another
    /// message that names the same files, and joins nothing.
    pub(crate) joinable: bool,
    /// The descriptors of the messages queued before it, which calls Detcore
    /// could not match took, for their descriptions to be released.
    pub(crate) forgotten: Vec<SentDescriptor>,
}

/// A socket's receive timeout (`SO_RCVTIMEO`, socket(7)), as the guest set it.
// TODO-HUMAN-REVIEW(PR-3908)
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ReceiveTimeout {
    /// No timeout: a blocking call waits indefinitely. Also what a zero
    /// value sets.
    #[default]
    None,
    /// A negative second count, which Linux stores as a zero timeout, so a
    /// blocking call fails at once with `EAGAIN`. `getsockopt` reads it back
    /// as zero, the same as `None`.
    Immediate,
    /// Wait at most this long. Never so long that Linux stores no timeout.
    After(Duration),
    /// Set by a call whose value Detcore could not read.
    Unknown,
}

impl ReceiveTimeout {
    /// The timeout a successful `setsockopt(SO_RCVTIMEO)` with `value` sets.
    /// (Linux has already refused a microsecond count out of range.)
    ///
    /// Linux stores a second count too large to count in scheduler ticks, at
    /// least `LONG_MAX / HZ - 1`, as no timeout (`MAX_SCHEDULE_TIMEOUT`), so
    /// such an accept restarts after a signal like an untimed one. The model
    /// takes the bound for the largest `HZ`, 1000; on a kernel with a smaller
    /// `HZ`, record's check against the kernel refuses a value between the two.
    pub(crate) fn set_by(value: libc::timeval) -> Self {
        const UNBOUNDED_SECONDS: libc::time_t = libc::time_t::MAX / 1000 - 1;
        if value.tv_sec < 0 {
            Self::Immediate
        } else if (value.tv_sec == 0 && value.tv_usec == 0) || value.tv_sec >= UNBOUNDED_SECONDS {
            Self::None
        } else {
            Self::After(
                Duration::from_secs(value.tv_sec as u64)
                    + Duration::from_micros(value.tv_usec.max(0) as u64),
            )
        }
    }
}

/// Where the model of one open file description lives.
///
/// Every `DetFd` method reaches the model through `DetFd::with_description`,
/// so a handle's state can change from `Local` to `Shared` under every alias
/// at once, escaped clones included, because they all hold the same `Arc`.
// The state always lives inside the one `Arc` allocation its aliases share,
// so boxing `Local` would add a second allocation to every open.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Serialize, Deserialize)]
enum OpenFileState {
    /// Only this process can reach the open file description, so the model
    /// lives here until a copied runtime or new image needs a shared owner.
    Local(OpenFileDescription),
    /// Several guest processes share the open file description, so the one
    /// canonical model lives in Detcore's global state and this handle holds
    /// none. Each access takes the model through the installed
    /// [`SharedOpenFileChannel`] and publishes it back. Promotion precedes
    /// native fork/exec: <https://github.com/rrnewton/hermit/issues/4381>.
    Shared(SharedOpenFile),
}

/// The process-local part of a shared open file description: its identity,
/// and the runtime lock that is never part of the model.
#[derive(Debug, Serialize, Deserialize)]
struct SharedOpenFile {
    id: OpenFileId,
    /// Serves `DetFd::directory_lock` for this process's aliases, as the
    /// description's own lock does for a local description.
    #[serde(skip)]
    directory_lock: Arc<futures::lock::Mutex<()>>,
}

/// The canonical model of an open file description that several guest
/// processes share. Detcore's global state stores it, and a
/// [`SharedOpenFileChannel`] carries it to and from the process that holds
/// the lease. Its contents are private to Detcore.
#[derive(Debug, Serialize, Deserialize)]
pub struct OpenFileModel(OpenFileDescription);

impl OpenFileModel {
    /// The open file description this model describes.
    pub(crate) fn id(&self) -> OpenFileId {
        self.0.id
    }

    #[cfg(test)]
    pub(crate) fn path_for_test(&self) -> Option<PathBuf> {
        self.0.path.clone()
    }
}

/// The canonical model of one shared open file description, taken by one
/// process for the length of one synchronous `DetFd` method call.
#[derive(Debug, Serialize, Deserialize)]
pub struct OpenFileLease {
    /// The open file description the lease covers.
    pub id: OpenFileId,
    /// Names this lease, so that the global state can refuse a late,
    /// duplicate or foreign publish.
    pub sequence: u64,
    /// The canonical model as of the take.
    pub model: OpenFileModel,
}

/// A failure to take or publish a shared open file description.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedOpenFileError(pub String);

impl fmt::Display for SharedOpenFileError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SharedOpenFileError {}

/// Synchronous access to the canonical models of shared open file
/// descriptions.
///
/// A `DetFd` method on a shared handle calls `take`, runs its synchronous
/// body on the model, and calls `publish`, all while it holds the handle's
/// local lock and without awaiting. So a lease never outlives one method
/// call. Detcore names no backend here: the backend that runs Detcore inside
/// each guest process installs an implementation with
/// [`install_shared_open_file_channel`].
pub trait SharedOpenFileChannel: Send + Sync {
    /// Publishes the initial local model. A duplicate identity is refused.
    /// The caller exposes shared handles only after this succeeds.
    fn promote(&self, model: &OpenFileModel) -> Result<(), SharedOpenFileError>;

    /// Takes the canonical model of `id` and starts a lease on it.
    fn take(&self, id: OpenFileId) -> Result<OpenFileLease, SharedOpenFileError>;

    /// Stores `lease.model` as the canonical model of `lease.id` and ends the
    /// lease. A publish that does not name the current lease is refused.
    fn publish(&self, lease: OpenFileLease) -> Result<(), SharedOpenFileError>;
}

static SHARED_OPEN_FILE_CHANNEL: OnceLock<Box<dyn SharedOpenFileChannel>> = OnceLock::new();

/// Installs this process's channel to shared open file descriptions. Only the
/// first channel installed in a process is retained; a later one is returned.
pub fn install_shared_open_file_channel(
    channel: Box<dyn SharedOpenFileChannel>,
) -> Result<(), Box<dyn SharedOpenFileChannel>> {
    SHARED_OPEN_FILE_CHANNEL.set(channel)
}

/// Runs `f` on the canonical model of the shared open file description `id`.
///
/// A missing channel, a failed take or publish, or a lease for another
/// description is an infrastructure failure, not something the guest's
/// syscall can report, so it panics. The model is published even when `f`
/// returns an error value, so the lease always ends; `f` itself decides what
/// it changed. If `f` panics, the lease is never published and the process
/// is lost with it.
fn with_shared_description<R>(id: OpenFileId, f: impl FnOnce(&mut OpenFileDescription) -> R) -> R {
    let channel = SHARED_OPEN_FILE_CHANNEL.get().unwrap_or_else(|| {
        panic!(
            "open file description {id:?} is shared, but no shared open file channel is installed"
        )
    });
    let mut lease = channel.take(id).unwrap_or_else(|error| {
        panic!("taking shared open file description {id:?} failed: {error}")
    });
    assert_eq!(
        lease.id, id,
        "the shared open file channel returned a lease for another description"
    );
    assert_eq!(
        lease.model.0.id, id,
        "the shared open file channel returned the model of another description"
    );
    let result = f(&mut lease.model.0);
    channel.publish(lease).unwrap_or_else(|error| {
        panic!("publishing shared open file description {id:?} failed: {error}")
    });
    result
}

#[derive(Serialize, Deserialize)]
struct OpenFileDescription {
    id: OpenFileId,
    /// fd type
    ty: FdType,
    /// Immutable inherited-read provenance. Neither guest-created PTYs nor
    /// unknown FIFOs acquire it merely by becoming stdio.
    #[serde(default)]
    inherited_external_read: Option<InheritedExternalRead>,
    /// Process named by a pidfd created through `pidfd_open`.
    ///
    /// This is shared by descriptor aliases just like the kernel pidfd object.
    /// `None` means either that this is not a pidfd or that Detcore did not
    /// observe enough provenance to identify its target.
    #[serde(default)]
    pidfd_target: Option<DetPid>,
    /// The task directory this open file is, where it was opened as one: a
    /// `/proc/self` or `/proc/thread-self` spelling with the selector replaced
    /// by the opener's numbers, or a `/proc/<pid>` or `/proc/<pid>/task/<tid>`
    /// spelling itself, and the identity of the task it named then. A name
    /// relative to it is looked up below that task's directory, whoever looks
    /// it up, while that task lives, as Linux binds a task directory to its
    /// task at the open (`Detcore::procfs_spelling_at`). The recorded `path`
    /// keeps the spelling. Shared by descriptor aliases and a fork's copies.
    #[serde(default)]
    procfs_reached: Option<(PathBuf, Option<crate::procfs::TaskIncarnation>)>,
    /// This open file is the `/proc/self` or `/proc/thread-self` link itself
    /// (an `O_PATH | O_NOFOLLOW` open), not the directory it leads to, so no
    /// name is below it (ENOTDIR), whatever its recorded `path` spells.
    #[serde(default)]
    procfs_link_itself: bool,
    /// This open file is a `/proc` directory Detcore serves itself, `/proc`
    /// or a task's descriptor directory, `/proc/<pid>/fd`
    /// (`procfs::fd_directory::PlaceholderDirectory`): which directory, its
    /// position, and whether it was opened with `O_PATH`. The kernel's
    /// descriptor is on the placeholder directory, an empty read-only tmpfs
    /// (`DetConfig::directory_placeholder`). Shared by descriptor aliases and
    /// a fork's copies, as Linux shares the position.
    #[serde(default)]
    placeholder: Option<PlaceholderState>,
    /// File status flags shared by dup and fork aliases.
    status_flags: i32,
    /// File path associated with fd.
    /// This cannot be relied upon. Special devices won't have it, for example.
    path: Option<PathBuf>,
    /// Cached det/virtual inode.
    /// This cannot be relied upon. Special devices won't have it, for example.
    /// However if `ty` indicates a `Regular` file, then there should reliably be an inode.
    inode: Option<DetInode>,
    /// inode is dirty
    dirty: bool,

    /// Irrespective of whether the file descriptor is marked logically blocking by the
    /// user, this tracks whether Detcore has converted the fd to nonblocking for its own
    /// purposes.
    physically_nonblocking: bool,

    /// A FIFO opened by path in `hermit run`, still `Regular` and physically
    /// blocking: its first blocking read or write that finds a guest holding
    /// the other end makes it an internally polled `Pipe`
    /// (`Detcore::fifo_polls_if_a_guest_holds_its_peer`).
    #[serde(default)]
    fifo_awaiting_guest_peer: bool,

    /// A FIFO opened by path that joined internal polling
    /// (`DetFd::fifo_joins_internal_polling`): any process that can reach its
    /// path, inside the container or not, may also hold an end of it.
    #[serde(default)]
    named_fifo: bool,
    /// An inotify instance received over SCM_RIGHTS without the open file
    /// description its sender modelled (an unmatched receipt), whose host
    /// description is nonblocking, in a run where Detcore makes every inotify
    /// instance it creates physically nonblocking: that O_NONBLOCK may be
    /// Detcore's rather than the sender's, so the guest's blocking mode is
    /// unknown (`Detcore::record_received_descriptors`). Cleared once the
    /// guest sets the mode itself (F_SETFL, FIONBIO).
    #[serde(default)]
    blocking_mode_unknown: bool,

    /// cached statbuf
    ///
    /// This is the RAW stat from the file system, NOT determinized.
    ///
    /// Some of these fields will change at runtime. But the following fields will
    /// be constant when `virtualize_metadata` is on, over the life of the DetFd:
    ///  - dev, rdev, blksize
    ///
    /// This should always be `Some` for regular files, as we eagerly populate it.
    stat: Option<DetStat>,
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1096): Review canonical random-device cursor sharing.
    /// Cursor into Hermit's backend-independent random-device byte stream.
    #[serde(default)]
    random_device_offset: u64,
    /// The stream this random-device open file reads.
    #[serde(default)]
    random_device_stream: RandomDeviceStream,
    /// resource
    resource: Option<ResourceID>,
    /// Deterministic snapshot state for selected procfs files.
    procfs: Option<ProcfsFile>,
    /// Signal phase 1's kernel account of the file, recorded at its open by a
    /// process that handles SIGALRM (`sigalrm_phase1::kernel_provenance`).
    #[serde(default)]
    sigalrm_phase1: Option<crate::sigalrm_phase1::Phase1Provenance>,
    /// Sorted directory stream, created by the first `getdents` call that
    /// succeeds on this open file description.
    #[serde(default)]
    directory: Option<DirectoryStream>,
    /// True while `getdents` on this open file reads the host directory one
    /// kernel buffer at a time, because no whole-directory snapshot can stand
    /// in for its position. See `use_host_directory_order`; an `lseek` back
    /// to 0 clears it.
    #[serde(default)]
    directory_host_order: bool,
    /// Held across a whole `getdents` or `lseek` on this open file
    /// description. Reading the host directory takes several injected
    /// syscalls on the shared kernel position, and the kernel's own per-file
    /// position lock covers only one of them; without this, two unsequentialized
    /// threads could interleave their reads and each keep part of the
    /// directory. Sequentialized threads never contend for it.
    ///
    /// This must stay a runtime-agnostic lock that uses no thread-locals.
    /// The DBT backend polls these handlers on DynamoRIO application
    /// threads, outside any Tokio runtime. Tokio's own locks charge each
    /// poll to a per-thread budget kept in a thread-local that has a
    /// destructor, and registering a thread-local destructor on a DynamoRIO
    /// application thread crashes DynamoRIO: the run exits with status 255
    /// and loses its evidence FINAL frames.
    #[serde(skip)]
    directory_lock: Arc<futures::lock::Mutex<()>>,
    /// Logical timestamp of the last packet delivered through this socket.
    socket_receive_timestamp: Option<LogicalTime>,
    /// True when this open file is an `AF_NETLINK`/`NETLINK_SOCK_DIAG` socket,
    /// whose binary dump replies carry host-assigned socket inode numbers that
    /// must be determinized (see `crate::sock_diag`).
    sock_diag: bool,
    /// Whether this open file is a `NETLINK_ROUTE` socket whose link-dump
    /// replies carry live interface counters that must be zeroed (see
    /// `crate::netlink_route`).
    netlink_route: bool,
    /// True when this socket connected to an IPv4 or IPv6 loopback peer.
    #[serde(default)]
    loopback_peer: bool,
    /// True when this socket is a channel of the external network trace, so
    /// its traffic is recorded or replayed (see `crate::syscalls::network_trace`).
    #[serde(default)]
    network_channel: bool,
    /// The socket's `SO_RCVLOWAT` while a network trace mode is active, or
    /// `None` for the default of one byte.
    #[serde(default)]
    network_lowat: Option<usize>,
    /// The socket's family and type, recorded at creation while a network
    /// trace mode is active.
    #[serde(default)]
    network_socket: NetworkSocketKind,
    /// True when this socket originated in `socketpair(2)`, selecting live
    /// internal routing. This does not prove guest-only readiness: an endpoint
    /// may be exported, or a datagram endpoint rebound. See
    /// `syscall_targets_internal_fd` and `is_guest_held`.
    #[serde(default)]
    socketpair_endpoint: bool,
    /// True when record/replay found that this socket's peer is a guest of
    /// this container, so only guest progress can end a wait on it, as for a
    /// socketpair endpoint. See `Detcore::classify_socket_peer`.
    #[serde(default)]
    guest_peer: bool,
    /// True when `accept(2)` or `accept4(2)` returned this socket. Record and
    /// replay keep such a connection only in the log, and replay stands in an
    /// eventfd for it. See `is_accepted_connection`.
    #[serde(default)]
    accepted_connection: bool,
    /// True when a receive with `MSG_ERRQUEUE` on this socket never waits on
    /// Linux, whatever the socket's blocking mode. Set by `socket(2)`
    /// (`error_queue_read_never_waits`) and inherited by the connections
    /// `accept(2)` returns.
    #[serde(default)]
    error_queue_never_waits: bool,
    /// True when only guests hold this object's other end, so only a guest
    /// can change whether it is readable or writable: a pipe or socketpair a
    /// guest created, an eventfd, an `AF_UNIX` socket whose peer is the
    /// holder's own process (a connection it made or accepted), or a pipe or
    /// socket the process received from itself whose object it already held
    /// through a guest-held description. Cleared, permanently, when a guest passes the
    /// object, or any object while it holds one, over `SCM_RIGHTS` on a
    /// socket that is not guest-held itself. A received descriptor that joins
    /// its sender's open file (`crate::syscalls::scm_rights`) shares this
    /// flag, as a dup does; one that joins nothing starts without it. See
    /// `is_guest_held`.
    #[serde(default)]
    guest_held: bool,
    /// True when a guest made this socket with `socket(2)` or
    /// `socketpair(2)` in this run and has not passed it out of the guest
    /// since, so only guests hold it. Never set for a received, inherited or
    /// accepted description. A connection is marked guest-held only when both
    /// its connecting socket and its listener have this flag
    /// (`Detcore::classify_guest_held_peer`): a socket's peer credentials name
    /// its peer, not every holder of its open file. Cleared, permanently,
    /// when a guest passes it over `SCM_RIGHTS` on a socket that is not
    /// guest-held. See `is_guest_created`.
    #[serde(default)]
    guest_created: bool,
    /// True when record/replay runs a blocking `accept(2)` on this socket in
    /// the caller's turn, one nonblocking attempt at a time. `socket(2)` sets
    /// it for a stream socket the container created. See
    /// `is_accept_in_turn_listener`.
    // TODO-HUMAN-REVIEW(PR-3908)
    #[serde(default)]
    accept_in_turn_eligible: bool,
    /// True once a guest's `shutdown(2)` with `SHUT_RD` or `SHUT_RDWR` shut
    /// this socket's read side. A polled blocking receive on it then asks the
    /// kernel whether the call would still wait. Permanent. See
    /// `read_shutdown_requested`.
    #[serde(default)]
    read_shutdown_requested: bool,
    /// True once this socket was named in an `SCM_RIGHTS` message, so a
    /// process outside Detcore's view may hold it. Permanent. See
    /// `is_exported`.
    #[serde(default)]
    exported: bool,
    /// The receive timeout the last successful `setsockopt(SO_RCVTIMEO)` on
    /// this socket set, which a blocking accept in turn waits by. See
    /// `receive_timeout`.
    #[serde(default)]
    receive_timeout: ReceiveTimeout,
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#2373)
    /// The `flock(2)` mode this open file description currently holds, as the
    /// bare `LOCK_SH`/`LOCK_EX` constant, or `None` when it holds no lock.
    ///
    /// `flock` locks are a property of the open file description, not of the
    /// descriptor slot and not of the inode, which is exactly the granularity
    /// this struct models: `dup`/`fork` aliases share one lock, two separate
    /// `open`s of the same file contend with each other.
    ///
    /// Detcore tracks this only so `handle_flock` can tell a first acquisition
    /// (where a failed `LOCK_NB` probe changes nothing) from a *conversion* of
    /// an already-held lock (where Linux drops the old lock before it can fail).
    /// It is a cache of what Detcore itself granted, never an authority: it is
    /// written only after the kernel reports success.
    #[serde(default)]
    flock_mode: Option<i32>,
    /// Whether `flock_mode` describes the kernel state. Descriptors created by
    /// an intercepted syscall start known-unlocked; descriptors discovered
    /// after entering the guest can already carry a lock.
    #[serde(default)]
    flock_mode_known: bool,
    /// Whether Detcore has EVER known this description's lock state.
    ///
    /// This separates two different unknowns that `flock_mode_known == false`
    /// otherwise collapses. A descriptor Detcore never observed -- stdin,
    /// stdout, stderr, a live-discovered fd -- has no cached claim at all, so
    /// there is nothing about it that can be STALE. A descriptor that was known
    /// and then invalidated by a process copy does have a claim that may now be
    /// wrong. Only the second is a reason to refuse `vfork`.
    #[serde(default)]
    flock_mode_ever_known: bool,
    /// Whether Detcore may ever have granted a `flock(2)` `LOCK_SH` or
    /// `LOCK_EX` through this Detcore model of the description.
    ///
    /// Set by every successful grant Detcore serves through the description,
    /// INCLUDING while `flock_mode_known` is false: a forgotten description no
    /// longer records which mode it holds, but it still records that it may
    /// hold one. Descriptions Detcore never observed (`mark_flock_mode_unobserved`)
    /// set it as well, since a lock may have been taken before Detcore saw
    /// them. Never cleared, and copied with the description at fork.
    ///
    /// It records only grants Detcore served through THIS model of the
    /// description. A lock taken natively (by a copied `vfork` child), by a
    /// sibling process whose copy of the model was split from this one at a
    /// fork, or through another model of the same kernel description (an
    /// `SCM_RIGHTS` receiver's) does not set it. False therefore does NOT
    /// mean unlocked; it means no grant through this model, so no Detcore
    /// claim about this description can exist here.
    ///
    /// `has_unsafe_vfork_flock_state` uses it to tell an invalidated
    /// description through which this process's Detcore granted a lock from
    /// one that was invalidated by a fork without such a grant (a pipe a
    /// `make` created before forking a job). In the second case no process
    /// holds a known mode for the description, so a copied `vfork` child that
    /// changes its lock natively makes no Detcore claim stale. A serialized
    /// description without the field reads as true, the conservative value;
    /// that default applies only to self-describing (map-based) formats.
    /// Not printed, so descriptions log as before.
    #[serde(default = "flock_ever_granted_when_absent")]
    flock_ever_granted: bool,
    /// A timerfd's expirations as a function of virtual time, once the guest
    /// has armed or disarmed it through Detcore (`VirtualTimerfd`). `None` for
    /// every other description, and for a timerfd Detcore leaves to the host
    /// (record/replay, or no virtual time).
    #[serde(default)]
    timerfd: Option<VirtualTimerfd>,
    /// The pipe or FIFO this description is an end of, for handing the turn
    /// to a thread polling it once another thread readies it
    /// (`--pipe-wake-latency`, https://github.com/rrnewton/hermit/issues/4380).
    /// Set only while that option is on; `None` otherwise, and for a
    /// descriptor whose channel Detcore does not know.
    #[serde(default)]
    channel: Option<crate::pipe_wake::ChannelEnd>,
    /// True once Detcore read this description's OFD and flock locks from its
    /// fdinfo after a call through it (`Detcore::refresh_description_locks`):
    /// from then on, each table's last close or replacement of it is reported
    /// to `procfs::locks::LockLedger`. Shared by every alias, as the
    /// description is. Not printed, so descriptions log as before.
    #[serde(default)]
    lock_tracked: bool,
    /// True once this description was registered on reception over
    /// `SCM_RIGHTS` (`Detcore::refresh_description_locks`): the sender's
    /// entry in `procfs::locks::LockLedger` may name the same kernel
    /// description under another identity. Not printed.
    #[serde(default)]
    received_for_locks: bool,
    /// True once this description may hold an OFD lock: an `F_OFD_SETLK` or
    /// `F_OFD_SETLKW` went through it, its fdinfo showed one, or its fdinfo
    /// could not be read. DBT refuses a copied `vfork` child, which would run
    /// its lock calls natively, while one does. Not printed.
    #[serde(default)]
    may_hold_ofd_lock: bool,
    /// True when this is an open file of a per-process file that follows
    /// its record's sequence (`ProcfsFile::follows_record_sequence`), which
    /// Linux keeps in the one `struct file` every holder shares, and this
    /// process's Detcore cannot know that sequence: a rendered file rebuilt
    /// from a placeholder that reached this process over `SCM_RIGHTS` when
    /// neither its table nor its registry knows the open file
    /// (`Detcore::adopt_received_placeholder`,
    /// `FileMetadata::adopt_placeholder`), or such a file the kernel opened,
    /// received or found so (`tool_local::kernel_record_file_link`). Every
    /// read, positioned read, vectored read and seek through it is refused by
    /// name (`Detcore::refuse_unknown_record_sequence`), so it never moves a
    /// sequence another holder's Detcore keeps, nor reads a record Linux would
    /// not show (on SaBRe, where a refusal that ended the run would strand it,
    /// `refusal_would_strand_the_run`, the call gets EOPNOTSUPP). Shared by
    /// every alias, copied at fork,
    /// and carried across exec (`ExecProcfsRecord`). Not printed.
    #[serde(default)]
    record_sequence_unknown: bool,
    /// True once Detcore may not see every reference to this open file
    /// description: another process or table got it before the lock ledger
    /// followed its references (a fork copied it, a send over `SCM_RIGHTS`
    /// passed it), or Detcore did not see it made (the launcher's standard
    /// streams, a descriptor discovered in the table, one received as a
    /// description of its own). Its last reference cannot be placed, so a
    /// `flock` through it is not Detcore's to decide (`Detcore::handle_flock`).
    /// Shared by every alias, as the description is. Permanent. Not printed.
    #[serde(default)]
    references_escaped: bool,
    /// The inode a `flock` through this description is kept on, and how
    /// Detcore decides it, once a `flock` took it (`Detcore::flock_identity`).
    /// Not printed.
    #[serde(default)]
    flock_identity: Option<FlockIdentity>,
    /// For an `AF_UNIX` socket: its type, name and peer, and the
    /// descriptors queued to it in `SCM_RIGHTS` messages
    /// (`crate::syscalls::scm_rights`). It names descriptions by handles
    /// that reach them only in the tool process that serves every guest
    /// (ptrace, KVM), so it is never serialized. Not printed.
    #[serde(skip)]
    rights: SocketRights,
    /// How many copies of this description the `SCM_RIGHTS` messages
    /// Detcore queued carry in flight, counted by guest calls alone: the send
    /// that queues a copy adds it, and the receive that installs it or the
    /// call that makes Linux release it takes it away
    /// ([`InFlightOpenFile::release`]). A copy whose queue went any other way
    /// (with a table the backend released, at no particular point of the
    /// guest's schedule, or past [`MAX_QUEUED_RIGHTS_MESSAGES`]) stays
    /// counted for the rest of the run. While any is counted, the
    /// description is never released, nor its socket's port, nor its queue
    /// ([`DetFd::is_last_reference`]). Not printed.
    #[serde(skip)]
    in_flight: usize,
    /// What Linux's fdinfo of this description's file prints that no call
    /// reads back, recorded where the guest set it through Detcore
    /// ([`FdinfoFacts`]). `None` for a description of no such kind, and for
    /// one Detcore did not create (inherited, received without joining the
    /// description its sender's table modeled, or rediscovered after an
    /// exec), whose fdinfo it then cannot render. Shared by every alias,
    /// as the file is. Not printed, so descriptions log as before.
    #[serde(default)]
    fdinfo_facts: Option<FdinfoFacts>,
    /// For one of the container's standard streams whose open file
    /// description the launcher passed on another of them too, as the
    /// launcher's `kcmp(KCMP_FILE)` of the descriptors it handed the guest
    /// answered (`Config::standard_stream_aliases`, which
    /// `FileMetadata::setup_stdio` applies): the group's key, the identity of its
    /// lowest stream, by which `procfs::locks::LockLedger` takes the group as
    /// the one description it is. Shared by every alias. Not printed.
    #[serde(default)]
    stdio_alias_group: Option<OpenFileId>,
}

impl OpenFileDescription {
    /// Escaped, with no reference the lock ledger follows and no flock
    /// identity: Detcore's flock model never decided a call through this
    /// description, and never will (`DetFd::rebind_open_file_id`).
    fn never_the_flock_models(&self) -> bool {
        self.references_escaped && !self.lock_tracked && self.flock_identity.is_none()
    }
}

/// State of an anonymous-inode file that its `/proc/<pid>/fdinfo/<fd>`
/// prints and no call reads back, as the guest set it through Detcore's
/// handlers (package K slice K8-h, `Detcore::render_own_fdinfo`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum FdinfoFacts {
    /// An eventfd: whether `eventfd2` made it with `EFD_SEMAPHORE`
    /// (`eventfd-semaphore`, fs/eventfd.c).
    Eventfd {
        /// `EFD_SEMAPHORE`.
        semaphore: bool,
    },
    /// A signalfd: the signals it reads, as `signalfd4` last set them, bit
    /// `n - 1` for signal `n`, without `SIGKILL` and `SIGSTOP`, which Linux
    /// takes out (`sigmask:`, fs/signalfd.c).
    Signalfd {
        /// The mask.
        mask: u64,
    },
    /// A timerfd: its clock, the flags of its last `timerfd_settime`, and
    /// Linux's own timer (`clockid:`, `settime flags:`, `ticks:` and
    /// `it_value:`, fs/timerfd.c).
    Timerfd {
        /// The clock `timerfd_create` named.
        clockid: i32,
        /// `TFD_TIMER_ABSTIME | TFD_TIMER_CANCEL_ON_SET` of the last setting;
        /// 0 before any.
        settime_flags: u32,
        /// Linux's own timer, as the guest's settings, reads and
        /// `timerfd_gettime` calls left it.
        linux: LinuxTimerfd,
    },
}
/// Written out, not derived, so that `channel` is printed only when it is set:
/// a run without `--pipe-wake-latency` logs every description exactly as
/// before the field existed.
impl std::fmt::Debug for OpenFileDescription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("OpenFileDescription");
        debug.field("id", &self.id);
        debug.field("ty", &self.ty);
        debug.field("inherited_external_read", &self.inherited_external_read);
        debug.field("pidfd_target", &self.pidfd_target);
        debug.field("procfs_reached", &self.procfs_reached);
        if let Some(placeholder) = &self.placeholder {
            debug.field("placeholder", placeholder);
        }
        debug.field("status_flags", &self.status_flags);
        debug.field("path", &self.path);
        debug.field("inode", &self.inode);
        debug.field("dirty", &self.dirty);
        debug.field("physically_nonblocking", &self.physically_nonblocking);
        debug.field("fifo_awaiting_guest_peer", &self.fifo_awaiting_guest_peer);
        debug.field("named_fifo", &self.named_fifo);
        if self.blocking_mode_unknown {
            debug.field("blocking_mode_unknown", &true);
        }
        debug.field("stat", &self.stat);
        debug.field("random_device_offset", &self.random_device_offset);
        debug.field("random_device_stream", &self.random_device_stream);
        debug.field("resource", &self.resource);
        debug.field("procfs", &self.procfs);
        debug.field("sigalrm_phase1", &self.sigalrm_phase1);
        debug.field("directory", &self.directory);
        debug.field("directory_host_order", &self.directory_host_order);
        debug.field("directory_lock", &self.directory_lock);
        debug.field("socket_receive_timestamp", &self.socket_receive_timestamp);
        debug.field("sock_diag", &self.sock_diag);
        debug.field("netlink_route", &self.netlink_route);
        debug.field("loopback_peer", &self.loopback_peer);
        debug.field("network_channel", &self.network_channel);
        debug.field("network_lowat", &self.network_lowat);
        debug.field("network_socket", &self.network_socket);
        debug.field("socketpair_endpoint", &self.socketpair_endpoint);
        debug.field("guest_peer", &self.guest_peer);
        debug.field("accepted_connection", &self.accepted_connection);
        debug.field("guest_held", &self.guest_held);
        debug.field("guest_created", &self.guest_created);
        debug.field("accept_in_turn_eligible", &self.accept_in_turn_eligible);
        debug.field("read_shutdown_requested", &self.read_shutdown_requested);
        debug.field("exported", &self.exported);
        debug.field("receive_timeout", &self.receive_timeout);
        debug.field("flock_mode", &self.flock_mode);
        debug.field("flock_mode_known", &self.flock_mode_known);
        debug.field("flock_mode_ever_known", &self.flock_mode_ever_known);
        debug.field("timerfd", &self.timerfd);
        if let Some(channel) = &self.channel {
            debug.field("channel", channel);
        }
        debug.finish()
    }
}

/// What Detcore keeps of an open file that is a `/proc` directory it serves
/// on a placeholder descriptor (`procfs::fd_directory::PlaceholderDirectory`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PlaceholderState {
    /// Which directory: `/proc`, or a descriptor directory and the task it
    /// is bound to.
    pub(crate) directory: crate::procfs::fd_directory::PlaceholderDirectory,
    /// The kernel position that tags the descriptor
    /// (`procfs::fd_directory::tag`), if it has one.
    pub(crate) tag: Option<u64>,
    /// The guest's position in a descriptor directory, as Linux numbers it.
    /// `/proc`'s position is its directory stream's (`DirectoryStream`).
    pub(crate) position: u64,
    /// Opened with `O_PATH`: it lists nothing, and reads and seeks fail with
    /// `EBADF`.
    pub(crate) path_only: bool,
}

/// A timerfd's expirations counted on Detcore's virtual clock instead of the
/// host's (timerfd_create(2)).
///
/// The host timerfd behind the descriptor is never armed with the guest's
/// setting, so the host cannot make it readable at a host-timed moment.
/// Instead, when a thread of the process observes the descriptor after the
/// virtual deadline (`crate::syscalls::timerfd`), Detcore counts the
/// expirations here and makes the host descriptor readable before the guest's
/// probe runs; a `read` then returns the count kept here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct VirtualTimerfd {
    /// The next expiration not yet counted, or `None` when disarmed.
    pub(crate) deadline: Option<LogicalTime>,
    /// The reload interval in nanoseconds, 0 for a one-shot timer.
    pub(crate) interval_ns: u64,
    /// Expirations counted but not yet read.
    pub(crate) unread: u64,
}

impl VirtualTimerfd {
    /// The state `timerfd_settime` leaves: armed at `deadline` (or disarmed
    /// with `None`), with no expiration pending, as Linux discards the count.
    /// A disarmed timer keeps `interval_ns`, which `timerfd_gettime` reports.
    pub(crate) fn set(deadline: Option<LogicalTime>, interval_ns: u64) -> Self {
        VirtualTimerfd {
            deadline,
            interval_ns,
            unread: 0,
        }
    }

    /// Counts every expiration at or before `now`, as Linux would have by
    /// then, and returns how many were added. A periodic timer moves to its
    /// first expiration after `now`; a one-shot timer disarms.
    pub(crate) fn expire_through(&mut self, now: LogicalTime) -> u64 {
        let Some(deadline) = self.deadline else {
            return 0;
        };
        if now < deadline {
            return 0;
        }
        let elapsed = now.as_nanos() - deadline.as_nanos();
        let added = match elapsed.checked_div(self.interval_ns) {
            // A one-shot timer expires once and disarms.
            None => {
                self.deadline = None;
                1
            }
            Some(periods) => {
                let added = 1 + periods;
                self.deadline = Some(LogicalTime::from_nanos(
                    deadline
                        .as_nanos()
                        .saturating_add(added.saturating_mul(self.interval_ns)),
                ));
                added
            }
        };
        self.unread = self.unread.saturating_add(added);
        added
    }

    /// `timerfd_gettime`'s answer at `now`, after `expire_through(now)`: the
    /// time left to the next expiration (0 when disarmed) and the interval,
    /// both in nanoseconds.
    pub(crate) fn remaining(&self, now: LogicalTime) -> (u64, u64) {
        let left = self.deadline.map_or(0, |deadline| {
            deadline.as_nanos().saturating_sub(now.as_nanos())
        });
        (left, self.interval_ns)
    }
}

/// What Linux's own timer behind a timerfd holds (`ctx->t` and `ctx->ticks`
/// in fs/timerfd.c), which only the timer's fdinfo shows: Linux counts one
/// expiration when its timer fires and leaves it stopped, and counts the
/// periods after that one only when a read or `timerfd_gettime` forwards it,
/// where Detcore's model counts every expiration at each probe
/// ([`VirtualTimerfd::expire_through`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LinuxTimerfd {
    /// Where the timer would fire next, or `None` when it will not.
    pub(crate) deadline: Option<LogicalTime>,
    /// The expirations counted as of the last forward that no read has
    /// taken, without one since then.
    pub(crate) ticks: u64,
}

impl LinuxTimerfd {
    /// The state `timerfd_settime` leaves: armed at `deadline`, nothing
    /// counted.
    pub(crate) fn set(deadline: Option<LogicalTime>) -> Self {
        LinuxTimerfd { deadline, ticks: 0 }
    }

    /// What Linux's `timerfd_gettime` does at `now` to a timer of interval
    /// `interval_ns` (`do_timerfd_gettime`): one that has fired is forwarded
    /// past `now`, counting every period that ended since it fired. A
    /// one-shot timer that has fired is not restarted; its one expiration
    /// stays counted. Nothing for a timer that has not fired.
    pub(crate) fn forward(&mut self, now: LogicalTime, interval_ns: u64) {
        let Some(deadline) = self.deadline else {
            return;
        };
        if now < deadline {
            return;
        }
        let elapsed = now.as_nanos() - deadline.as_nanos();
        match elapsed.checked_div(interval_ns) {
            None => {
                self.ticks = self.ticks.saturating_add(1);
                self.deadline = None;
            }
            Some(periods) => {
                let counted = 1 + periods;
                self.ticks = self.ticks.saturating_add(counted);
                self.deadline = Some(LogicalTime::from_nanos(
                    deadline
                        .as_nanos()
                        .saturating_add(counted.saturating_mul(interval_ns)),
                ));
            }
        }
    }

    /// What a successful read leaves (`timerfd_read_iter`): the timer
    /// forwarded past the read, as Detcore's model is after the probe that
    /// made the read ready (`next`, its [`VirtualTimerfd::deadline`]), and
    /// nothing counted.
    pub(crate) fn after_read(&mut self, next: Option<LogicalTime>) {
        self.deadline = next;
        self.ticks = 0;
    }

    /// What Linux's `timerfd_show` prints at `now`: its count, with the one
    /// expiration of a timer that has fired since its last forward, and the
    /// time left to that timer (`timerfd_get_remaining`, 0 once it has fired
    /// or when it will not), in nanoseconds.
    pub(crate) fn view(&self, now: LogicalTime) -> (u64, u64) {
        match self.deadline {
            Some(deadline) if now >= deadline => (self.ticks.saturating_add(1), 0),
            Some(deadline) => (
                self.ticks,
                deadline.as_nanos().saturating_sub(now.as_nanos()),
            ),
            None => (self.ticks, 0),
        }
    }
}

/// `OpenFileDescription::flock_ever_granted` for a serialized description
/// that lacks the field: may have been granted, the conservative answer.
fn flock_ever_granted_when_absent() -> bool {
    true
}

impl PartialEq for DetFd {
    fn eq(&self, other: &Self) -> bool {
        self.fd == other.fd
    }
}

impl Eq for DetFd {}

impl Hash for DetFd {
    // fd is owned by process and is unique per process/thread
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.fd.hash(state);
    }
}

/// The inode a `flock` through an open file description is kept on, and how
/// Detcore decides it (`Detcore::handle_flock`), as the first `flock` through
/// it found them: they do not change while the description lives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FlockIdentity {
    /// The file's `st_dev`, from `fstat`.
    pub(crate) dev: u64,
    /// The file's `st_ino`, from `fstat`.
    pub(crate) ino: u64,
    /// The file's filesystem, by name, where the filesystem's own
    /// `f_op->flock` decides the call (`flock_lock_inode` never runs), which
    /// Detcore leaves to it; `None` for any other.
    pub(crate) own_flock: Option<String>,
    /// Whether Detcore's flock model decided a call through the description.
    pub(crate) modelled: bool,
}

/// If the flags specify O_NONBLOCK.
fn oflags_nonblocking(flags: i32) -> bool {
    let o_nonblock = OFlag::O_NONBLOCK.bits();
    flags & o_nonblock == o_nonblock
}

impl DetFd {
    /// Publish a local description before another process or image can use
    /// it. Every escaped alias switches together under the existing mutex.
    /// Failure leaves the local model intact and must stop the lifecycle.
    pub(crate) fn promote(&self) -> Result<(), SharedOpenFileError> {
        let mut state = self.open_file.lock().expect("open file mutex poisoned");
        let OpenFileState::Local(description) = &*state else {
            return Ok(());
        };
        let shared = SharedOpenFile {
            id: description.id,
            directory_lock: Arc::clone(&description.directory_lock),
        };
        let OpenFileState::Local(description) =
            std::mem::replace(&mut *state, OpenFileState::Shared(shared))
        else {
            unreachable!()
        };
        let model = OpenFileModel(description);
        let result = SHARED_OPEN_FILE_CHANNEL
            .get()
            .ok_or_else(|| SharedOpenFileError("no shared open file channel installed".into()))
            .and_then(|channel| {
                // Only the backends that run Detcore in each guest process
                // install a channel. They discover received descriptors, so
                // Detcore neither pairs their sockets nor queues on them, and
                // the bookkeeping the model does not serialize is empty.
                debug_assert!(
                    model.0.rights.peer.is_none() && model.0.rights.queued.is_empty(),
                    "a promoted description carries SCM_RIGHTS bookkeeping"
                );
                channel.promote(&model)
            });
        if result.is_err() {
            *state = OpenFileState::Local(model.0);
        }
        result
    }

    pub(crate) fn is_shared(&self) -> bool {
        matches!(
            *self.open_file.lock().expect("open file mutex poisoned"),
            OpenFileState::Shared(_)
        )
    }

    /// A new process needs its own unlocked runtime lock, but the same
    /// canonical identity. The caller interns aliases after copying the table.
    pub(crate) fn fork_shared_handle(&self) -> Self {
        let state = self.open_file.lock().expect("open file mutex poisoned");
        let mut copied = self.clone();
        if let OpenFileState::Shared(shared) = &*state {
            copied.open_file = Arc::new(Mutex::new(OpenFileState::Shared(SharedOpenFile {
                id: shared.id,
                directory_lock: Arc::default(),
            })));
        }
        copied
    }

    /// Restore an authenticated survivor from the existing exec handover.
    pub(crate) fn shared_survivor(fd: RawFd, id: OpenFileId) -> Self {
        Self {
            fd,
            fd_flags: 0,
            open_file: Arc::new(Mutex::new(OpenFileState::Shared(SharedOpenFile {
                id,
                directory_lock: Arc::default(),
            }))),
        }
    }

    /// A table copy (a fork, or a thread made without `CLONE_FILES`) gave
    /// this description references the lock ledger never saw taken, unless
    /// the ledger follows it (`is_lock_tracked`): mark them escaped
    /// (`mark_references_escaped`). Only a local description is marked here;
    /// a promoted one is never contacted, as a fork precedes the callback in
    /// which its shared model may be leased (DBT), and the flock model takes
    /// every promoted description as escaped anyway (`is_shared`): its model
    /// spans processes by construction.
    pub(crate) fn mark_local_untracked_references_escaped(&self) {
        let mut state = self.open_file.lock().expect("open file mutex poisoned");
        if let OpenFileState::Local(description) = &mut *state
            && !description.lock_tracked
        {
            description.references_escaped = true;
        }
    }

    /// Only a copied local flock cache can become stale at fork. Shared
    /// models retain kernel-observed lock changes without a pre-grant RPC.
    pub(crate) fn forget_local_flock_mode_at_fork(&self) {
        let mut state = self.open_file.lock().expect("open file mutex poisoned");
        if let OpenFileState::Local(description) = &mut *state {
            description.flock_mode = None;
            description.flock_mode_known = false;
        }
    }

    /// Inspect only local ownership and the skipped directory lease. This
    /// never takes a canonical shared-OFD lease or contacts its channel.
    /// The caller separately owns quiescence across this temporary probe.
    pub(crate) fn validate_native_exec_runtime_state(
        &self,
    ) -> Result<(OpenFileId, usize), crate::native_exec::NativeExecCutError> {
        use crate::native_exec::NativeExecCutError;

        let state = crate::native_exec::try_native_exec_lock(&self.open_file, "open_file")?;
        let (id, directory_lock) = match &*state {
            OpenFileState::Local(description) => (description.id, &description.directory_lock),
            OpenFileState::Shared(shared) => (shared.id, &shared.directory_lock),
        };
        let _lease = directory_lock
            .try_lock()
            .ok_or(NativeExecCutError::LiveCapability("directory lease"))?;
        Ok((id, Arc::as_ptr(&self.open_file) as usize))
    }

    /// create a new detfd from rawfd
    pub fn new(fd: RawFd, flags: OFlag, ty: FdType, id: OpenFileId) -> Self {
        let bits = flags.bits();
        DetFd {
            fd,
            fd_flags: bits & OFlag::O_CLOEXEC.bits(),
            open_file: Arc::new(Mutex::new(OpenFileState::Local(OpenFileDescription {
                id,
                ty,
                pidfd_target: None,
                procfs_reached: None,
                procfs_link_itself: false,
                placeholder: None,
                inherited_external_read: None,
                status_flags: bits & !OFlag::O_CLOEXEC.bits(),
                path: None,
                inode: None,
                dirty: false,
                fifo_awaiting_guest_peer: false,
                named_fifo: false,
                stat: None,
                random_device_offset: 0,
                random_device_stream: RandomDeviceStream::default(),
                resource: None,
                procfs: None,
                sigalrm_phase1: None,
                directory: None,
                directory_host_order: false,
                directory_lock: Default::default(),
                socket_receive_timestamp: None,
                sock_diag: false,
                netlink_route: false,
                loopback_peer: false,
                network_channel: false,
                network_lowat: None,
                network_socket: NetworkSocketKind::NotInet,
                socketpair_endpoint: false,
                guest_peer: false,
                accepted_connection: false,
                error_queue_never_waits: false,
                guest_held: false,
                guest_created: false,
                accept_in_turn_eligible: false,
                read_shutdown_requested: false,
                exported: false,
                receive_timeout: ReceiveTimeout::None,
                flock_mode: None,
                flock_mode_known: true,
                flock_mode_ever_known: true,
                flock_ever_granted: false,
                timerfd: None,
                channel: None,
                lock_tracked: false,
                received_for_locks: false,
                may_hold_ofd_lock: false,
                record_sequence_unknown: false,
                references_escaped: false,
                flock_identity: None,
                rights: SocketRights::default(),
                in_flight: 0,
                fdinfo_facts: None,
                stdio_alias_group: None,
                // By default, we assume it matches the flags we were given:
                physically_nonblocking: oflags_nonblocking(bits),
                blocking_mode_unknown: false,
            }))),
        }
    }

    /// Runs `f` on this open file description's model while holding the lock
    /// every alias shares. One call is one atomic step on the model, whether
    /// the model is local or shared (see [`OpenFileState`]). `f` must not
    /// access this description through another `DetFd` method or alias, and
    /// must not await.
    fn with_description<R>(&self, f: impl FnOnce(&mut OpenFileDescription) -> R) -> R {
        let mut state = self.open_file.lock().expect("open file mutex poisoned");
        match &mut *state {
            OpenFileState::Local(description) => f(description),
            OpenFileState::Shared(shared) => with_shared_description(shared.id, f),
        }
    }

    /// A handle on this descriptor's open file description that does not
    /// keep it alive ([`OpenFileHandle`]).
    pub(crate) fn open_file_handle(&self) -> OpenFileHandle {
        OpenFileHandle(Arc::downgrade(&self.open_file))
    }

    /// This descriptor's open file description, for an `SCM_RIGHTS` message
    /// that carries it ([`InFlightOpenFile`]): `held` while the message is
    /// queued, or with the legacy ownership that keeps nothing alive.
    pub(crate) fn in_flight_open_file(&self, held: bool) -> InFlightOpenFile {
        InFlightOpenFile(if held {
            InFlight::Held(Arc::clone(&self.open_file))
        } else {
            InFlight::Legacy(Arc::downgrade(&self.open_file))
        })
    }

    /// update fd
    pub fn with_fd(mut self, fd: RawFd) -> Self {
        self.fd = fd;
        self
    }
    /// change fd type
    pub fn with_type(self, ty: FdType) -> Self {
        self.set_type(ty);
        self
    }
    /// Change the type of this descriptor's open file description, which every
    /// alias shares.
    pub(crate) fn set_type(&self, ty: FdType) {
        self.with_description(|d| d.ty = ty);
    }
    /// Make this descriptor's open file description a random device that
    /// reads `stream`.
    pub(crate) fn set_random_device(&self, stream: RandomDeviceStream) {
        self.with_description(|d| {
            d.ty = FdType::Rng;
            d.random_device_stream = stream;
        });
    }
    /// The stream this random-device open file reads, which every alias shares.
    pub(crate) fn random_device_stream(&self) -> RandomDeviceStream {
        self.with_description(|d| d.random_device_stream)
    }
    /// Set per-slot descriptor flags on a newly duplicated fd.
    pub fn with_fd_flags(mut self, flags: OFlag) -> Self {
        self.fd_flags = flags.bits() & OFlag::O_CLOEXEC.bits();
        self
    }
    /// set path associated with `fd`
    pub fn with_path<P: AsRef<Path>>(self, path: P) -> Self {
        let path = Some(PathBuf::from(path.as_ref()));
        self.with_description(|d| d.path = path);
        self
    }
    /// set virtual inode
    pub fn with_inode(self, inode: DetInode) -> Self {
        self.with_description(|d| d.inode = Some(inode));
        self
    }
    /// set dirty flag
    pub fn with_dirty(self, dirty: bool) -> Self {
        self.with_description(|d| d.dirty = dirty);
        self
    }
    /// update statbuf
    pub fn with_stat<S: Into<Option<DetStat>>>(self, stat: S) -> Self {
        let stat = stat.into();
        self.with_description(|d| d.stat = stat);
        self
    }
    /// set resource id
    pub fn with_resource<S: Into<Option<ResourceID>>>(self, resource: S) -> Self {
        let resource = resource.into();
        self.with_description(|d| d.resource = resource);
        self
    }

    /// Update the scheduler resource shared by aliases of this open file.
    pub(crate) fn set_resource<S: Into<Option<ResourceID>>>(&self, resource: S) {
        let resource = resource.into();
        self.with_description(|d| d.resource = resource);
    }

    pub(crate) fn inherited_external_read(&self) -> Option<InheritedExternalRead> {
        self.with_description(|d| d.inherited_external_read.clone())
    }

    pub(crate) fn set_inherited_external_read(&self, terminal: InheritedExternalRead) {
        self.with_description(|d| d.inherited_external_read = Some(terminal));
    }

    /// If fd is non blocking
    pub fn is_nonblocking(&self) -> bool {
        self.with_description(|d| oflags_nonblocking(d.status_flags))
    }

    /// Whether close-on-exec is set for this descriptor slot.
    pub fn is_cloexec(&self) -> bool {
        self.fd_flags & OFlag::O_CLOEXEC.bits() != 0
    }

    /// Update close-on-exec for this descriptor slot only.
    pub fn set_cloexec(&mut self, enabled: bool) {
        self.fd_flags = if enabled { OFlag::O_CLOEXEC.bits() } else { 0 };
    }

    /// Update both the logical (guest-visible) and physical (scheduler)
    /// nonblocking status for every alias of this open file description. Use this
    /// only when the physical fd genuinely tracks the guest's request; when
    /// Detcore forces the fd physically nonblocking for the scheduler, update the
    /// logical view alone via [`Self::set_logical_nonblocking`].
    pub fn set_nonblocking(&self, enabled: bool) {
        self.with_description(|description| {
            if enabled {
                description.status_flags |= OFlag::O_NONBLOCK.bits();
            } else {
                description.status_flags &= !OFlag::O_NONBLOCK.bits();
            }
            description.physically_nonblocking = enabled;
            // The guest has now set the mode itself.
            description.blocking_mode_unknown = false;
        })
    }

    /// Update only the logical (guest-visible) O_NONBLOCK status flag, leaving
    /// the physical (scheduler) nonblocking state untouched. This lets a guest
    /// clear O_NONBLOCK while Detcore keeps the fd physically nonblocking, which
    /// the scheduler relies on for nonblockize-and-retry.
    pub fn set_logical_nonblocking(&self, enabled: bool) {
        self.with_description(|description| {
            if enabled {
                description.status_flags |= OFlag::O_NONBLOCK.bits();
            } else {
                description.status_flags &= !OFlag::O_NONBLOCK.bits();
            }
            // The guest has now set the mode itself.
            description.blocking_mode_unknown = false;
        })
    }

    /// Makes this descriptor refer to `other`'s open file description object.
    pub(crate) fn share_open_file_with(&mut self, other: &DetFd) {
        self.open_file = Arc::clone(&other.open_file);
    }

    /// Stable identity shared by dup and fork aliases. A shared handle keeps
    /// its identity locally, so this never takes the canonical model.
    pub fn open_file_id(&self) -> OpenFileId {
        match &*self.open_file.lock().expect("open file mutex poisoned") {
            OpenFileState::Local(description) => description.id,
            OpenFileState::Shared(shared) => shared.id,
        }
    }

    /// Give this open file description the identity `id` of the one it is a
    /// copy of in Linux: a placeholder descriptor received over `SCM_RIGHTS`,
    /// whose sender's description Linux shares, so that the locks Detcore
    /// follows by description (`procfs::locks::LockLedger`) are that one's.
    /// Its other state stays its own, as for every received descriptor
    /// (<https://github.com/rrnewton/hermit/issues/4670>). A shared
    /// description (one promoted to Detcore's global state) keeps its
    /// identity; placeholders are never promoted.
    pub(crate) fn adopt_open_file_id(&self, id: OpenFileId) {
        if let OpenFileState::Local(description) =
            &mut *self.open_file.lock().expect("open file mutex poisoned")
        {
            description.id = id;
        }
    }

    /// Number of references that retain this open file description: the
    /// modeled descriptor slots of every table that names it, and the queued
    /// `SCM_RIGHTS` messages that still hold it ([`InFlightOpenFile`]). A
    /// table of an exited process, and a queue that goes with one, count
    /// until the backend releases that table, at no particular point of the
    /// guest's schedule; see [`Self::is_last_reference`] for the decision
    /// that does not depend on it for queued copies.
    pub(crate) fn open_file_alias_count(&self) -> usize {
        Arc::strong_count(&self.open_file)
    }

    /// Whether this descriptor is the description's last reference, so that
    /// its going releases the description, as Linux's last `fput` releases
    /// the `struct file` (a socket's port, a receiving socket's queue): no
    /// other modeled descriptor names it, and no copy of it is in flight
    /// ([`OpenFileDescription::in_flight`]). The copies in flight are those
    /// guest calls queued and have not received or released, whatever holds
    /// them now, so the backend's release of an exited process's table never
    /// decides it for a copy in flight: only the other tables' descriptors
    /// do, as a forked child's table, which counts until the backend releases
    /// it, always did.
    pub(crate) fn is_last_reference(&self) -> bool {
        Arc::strong_count(&self.open_file) == 1 && self.with_description(|d| d.in_flight == 0)
    }

    /// How many copies of this description are in flight
    /// ([`OpenFileDescription::in_flight`]).
    pub(crate) fn copies_in_flight(&self) -> usize {
        self.with_description(|d| d.in_flight)
    }

    /// File type attached to the open file description.
    pub fn ty(&self) -> FdType {
        self.with_description(|d| d.ty)
    }

    /// This timerfd's virtual-time model, if Detcore keeps one.
    pub(crate) fn virtual_timerfd(&self) -> Option<VirtualTimerfd> {
        self.with_description(|d| d.timerfd)
    }

    /// Replace this timerfd's virtual-time model, under every alias.
    pub(crate) fn set_virtual_timerfd(&self, timerfd: Option<VirtualTimerfd>) {
        self.with_description(|d| {
            debug_assert_eq!(d.ty, FdType::Timerfd);
            d.timerfd = timerfd;
        })
    }

    /// What this description's fdinfo prints that no call reads back, where
    /// the guest set it through Detcore ([`FdinfoFacts`]).
    pub(crate) fn fdinfo_facts(&self) -> Option<FdinfoFacts> {
        self.with_description(|d| d.fdinfo_facts)
    }

    /// Record what this description's fdinfo prints that no call reads back,
    /// under every alias.
    pub(crate) fn set_fdinfo_facts(&self, facts: FdinfoFacts) {
        self.with_description(|d| d.fdinfo_facts = Some(facts))
    }

    /// Record the process identity carried by a newly created pidfd.
    pub(crate) fn set_pidfd_target(&self, target: DetPid) {
        self.with_description(|description| {
            debug_assert_eq!(description.ty, FdType::Pidfd);
            description.pidfd_target = Some(target);
        })
    }

    /// Return the process identity carried by this pidfd, when known.
    pub(crate) fn pidfd_target(&self) -> Option<DetPid> {
        self.with_description(|d| d.pidfd_target)
    }

    /// Record where the `/proc` selector spelling this open file was opened
    /// by led (`OpenFileDescription::procfs_reached`).
    /// `incarnation` is the identity of the task the selector reached, where
    /// Detcore asked its process table (`procfs::TaskIncarnation`): a name
    /// below it is that task's while no other task holds its numbers.
    pub(crate) fn set_procfs_reached(
        &self,
        reached: PathBuf,
        incarnation: Option<crate::procfs::TaskIncarnation>,
    ) {
        self.with_description(|d| d.procfs_reached = Some((reached, incarnation)))
    }

    /// Record that this open file is a `/proc` selector link itself
    /// (`OpenFileDescription::procfs_link_itself`).
    pub(crate) fn set_procfs_link_itself(&self) {
        self.with_description(|d| d.procfs_link_itself = true)
    }

    /// Whether this open file is a `/proc` selector link itself.
    pub(crate) fn procfs_link_itself(&self) -> bool {
        self.with_description(|d| d.procfs_link_itself)
    }

    /// The placeholder directory this open file is, if it is one
    /// (`OpenFileDescription::placeholder`).
    pub(crate) fn placeholder(&self) -> Option<PlaceholderState> {
        self.with_description(|d| d.placeholder)
    }

    /// Make this open file a placeholder directory, or change its state.
    pub(crate) fn set_placeholder(&self, state: PlaceholderState) {
        self.with_description(|d| d.placeholder = Some(state))
    }

    /// The task directory this open file is, if Detcore knows it
    /// (`OpenFileDescription::procfs_reached`).
    pub(crate) fn procfs_reached(
        &self,
    ) -> Option<(PathBuf, Option<crate::procfs::TaskIncarnation>)> {
        self.with_description(|d| d.procfs_reached.clone())
    }

    /// The pipe or FIFO this descriptor is an end of, when
    /// `--pipe-wake-latency` is on and Detcore knows it
    /// (`crate::pipe_wake`).
    pub(crate) fn channel(&self) -> Option<crate::pipe_wake::ChannelEnd> {
        self.with_description(|d| d.channel)
    }

    /// Name the pipe or FIFO this descriptor's open file description is an
    /// end of. Shared by every alias of the description.
    pub(crate) fn set_channel(&self, channel: crate::pipe_wake::ChannelEnd) {
        self.with_description(|d| d.channel = Some(channel))
    }

    /// Resource attached to the open file description.
    pub fn resource(&self) -> Option<ResourceID> {
        self.with_description(|d| d.resource.clone())
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1096): Review canonical random-device cursor sharing.
    /// Return the cursor shared by aliases of this random-device open file.
    #[cfg(test)]
    pub(crate) fn random_device_offset(&self) -> u64 {
        self.with_description(|d| d.random_device_offset)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1096): Review canonical random-device cursor sharing.
    /// Advance the cursor shared by aliases of this random-device open file.
    #[cfg(test)]
    pub(crate) fn advance_random_device_offset(&self, count: usize) {
        self.with_description(|description| {
            description.random_device_offset = description
                .random_device_offset
                .saturating_add(count as u64);
        })
    }

    /// Copy from the shared random-device cursor and commit the returned byte count.
    ///
    /// The description stays locked throughout the synchronous copy. The closure
    /// must not access this description through another `DetFd` method or alias.
    /// Clone the descriptor and drop its metadata guard before entering this
    /// transaction; the closure must not acquire Detcore metadata/description
    /// locks or await. This preserves the metadata-before-description lock order.
    /// A large copy can block relaxed-mode aliases for its bounded duration.
    /// An error leaves the cursor unchanged; it does not undo guest memory writes.
    pub(crate) fn with_random_device_stream<R>(
        &self,
        copy: impl FnOnce(u64) -> Result<usize, R>,
    ) -> Result<usize, R> {
        self.with_description(|description| {
            let copied = copy(description.random_device_offset)?;
            description.random_device_offset = description
                .random_device_offset
                .saturating_add(copied as u64);
            Ok(copied)
        })
    }

    /// Path used to open this file description, when it was observable.
    pub(crate) fn path(&self) -> Option<PathBuf> {
        self.with_description(|d| d.path.clone())
    }

    /// Record the resolved path used to open this file description.
    pub(crate) fn set_path<P: AsRef<Path>>(&self, path: P) {
        let path = Some(path.as_ref().to_path_buf());
        self.with_description(|d| d.path = path);
    }

    /// Attach deterministic procfs snapshot state to this open file description.
    pub(crate) fn set_procfs(&self, procfs: ProcfsFile) {
        self.with_description(|d| d.procfs = Some(procfs));
    }

    /// The path `readlink` of this descriptor's link gives for the procfs
    /// file Detcore recorded at its open (`ProcfsFile::link_name`).
    pub(crate) fn procfs_link_name(&self) -> Option<PathBuf> {
        self.with_description(|d| {
            d.procfs
                .as_ref()
                .and_then(ProcfsFile::link_name)
                .map(Path::to_path_buf)
        })
    }

    /// The procfs path of a file Detcore opened with no kernel object behind
    /// it (`ProcfsFile::is_rendered`): this descriptor is a placeholder.
    pub(crate) fn procfs_rendered_path(&self) -> Option<&'static Path> {
        self.with_description(|d| d.procfs.as_ref().and_then(ProcfsFile::opened_path))
    }

    /// The procfs path of a file whose every byte Detcore renders
    /// (`ProcfsFile::rendered_path`), whether Detcore opened it as a
    /// placeholder or the kernel opened it: through a symbolic link or `..`,
    /// after `chroot`, or under record and replay. `files` are the files the
    /// session renders (`Detcore::rendered_files`).
    pub(crate) fn procfs_renderable_path(
        &self,
        files: crate::procfs::RenderedFiles,
    ) -> Option<&'static Path> {
        self.with_description(|d| {
            d.procfs
                .as_ref()
                .and_then(|procfs| procfs.served_rendered_path(files))
        })
    }

    /// Whether Linux reads this procfs file through `read_iter`
    /// (`ProcfsFile::has_read_iter`).
    pub(crate) fn procfs_has_read_iter(&self) -> bool {
        self.with_description(|d| d.procfs.as_ref().is_none_or(ProcfsFile::has_read_iter))
    }

    /// Whether Detcore keeps this procfs file's kernel position at its own
    /// cursor: a placeholder, a fixed file the kernel opened (whose snapshot
    /// Detcore renders without reading the kernel's file), or a process's
    /// `maps` whose snapshot Detcore rendered from the backend's memory map
    /// (`ProcfsFile::snapshot_from_memory_map`), which did not read the
    /// kernel's file either. Linux's `fdinfo` shows that position, and a
    /// descriptor of the open file that Detcore's model does not join (one
    /// received over `SCM_RIGHTS` that Detcore records as an open file of its
    /// own) reads the kernel's file from it. A backend that rediscovers
    /// descriptors after `exec` reads the position back.
    pub(crate) fn procfs_keeps_kernel_position(&self) -> bool {
        self.with_description(|d| {
            d.procfs.as_ref().is_some_and(|procfs| {
                procfs.is_rendered()
                    || procfs.snapshot_is_fixed()
                    || procfs.snapshot_from_memory_map()
            })
        })
    }

    /// Note that this process's `maps` snapshot was rendered from the
    /// backend's memory map (`ProcfsFile::note_rendered_from_memory_map`).
    pub(crate) fn note_procfs_rendered_from_memory_map(&self) {
        self.with_description(|d| {
            if let Some(procfs) = d.procfs.as_mut() {
                procfs.note_rendered_from_memory_map();
            }
        })
    }

    /// Whether this procfs file's snapshot seeks only from the start or the
    /// position (`ProcfsFile::seeks_only_from_start_or_position`).
    pub(crate) fn procfs_seeks_only_from_start_or_position(&self) -> bool {
        self.with_description(|d| {
            d.procfs
                .as_ref()
                .is_some_and(ProcfsFile::seeks_only_from_start_or_position)
        })
    }

    /// Whether a short copy of a read of this procfs file is `EFAULT`
    /// (`ProcfsFile::faults_on_a_short_copy`).
    pub(crate) fn procfs_faults_on_a_short_copy(&self) -> bool {
        self.with_description(|d| {
            d.procfs
                .as_ref()
                .is_some_and(ProcfsFile::faults_on_a_short_copy)
        })
    }

    /// The bytes a procfs file's snapshot starts from in place of a capture
    /// of the kernel's file, if it takes none: a rendered file's template
    /// (`ProcfsFile::rendering_template`), or nothing for a fixed file the
    /// kernel opened (through a symbolic link, after `chroot`, under record
    /// and replay), whose bytes are the description's whatever the kernel's
    /// file holds (`ProcfsFile::snapshot_is_fixed`).
    pub(crate) fn procfs_rendering_template(&self) -> Option<&'static [u8]> {
        self.with_description(|d| {
            d.procfs.as_ref().and_then(|procfs| {
                if procfs.is_rendered() {
                    Some(procfs.rendering_template())
                } else if procfs.snapshot_is_fixed() {
                    Some(&b""[..])
                } else {
                    None
                }
            })
        })
    }

    /// Whether Detcore opened this procfs file without a kernel object behind
    /// it and its first read takes its snapshot at an uptime the placeholder
    /// carries (`ProcfsFile::snapshot_carries_uptime`,
    /// `Detcore::first_read_uptime`).
    pub(crate) fn procfs_rendered_at_first_read(&self) -> bool {
        self.with_description(|d| {
            d.procfs
                .as_ref()
                .is_some_and(|procfs| procfs.is_rendered() && procfs.snapshot_carries_uptime())
        })
    }

    /// Whether this procfs open file description still needs its initial snapshot.
    pub(crate) fn procfs_needs_snapshot(&self) -> bool {
        self.with_description(|d| d.procfs.as_ref().is_some_and(ProcfsFile::needs_snapshot))
    }

    /// Whether every read of this open file description reads the process's
    /// memory again (`ProcfsFile::reads_memory_at_every_read`): a process's
    /// `cmdline` Detcore opened.
    pub(crate) fn procfs_reads_memory_at_every_read(&self) -> bool {
        self.with_description(|d| {
            d.procfs
                .as_ref()
                .is_some_and(ProcfsFile::reads_memory_at_every_read)
        })
    }

    /// Whether reads of this open file description are served from a procfs
    /// snapshot, already taken or still to be taken.
    pub(crate) fn procfs_serves_snapshot(&self) -> bool {
        self.with_description(|d| {
            d.procfs
                .as_ref()
                .is_some_and(|procfs| procfs.needs_snapshot() || procfs.position().1.is_some())
        })
    }

    /// Signal phase 1's kernel account of this file, if one was recorded.
    pub(crate) fn sigalrm_phase1_provenance(
        &self,
    ) -> Option<crate::sigalrm_phase1::Phase1Provenance> {
        self.with_description(|d| d.sigalrm_phase1)
    }

    /// Record signal phase 1's kernel account of this file.
    pub(crate) fn set_sigalrm_phase1_provenance(
        &self,
        provenance: crate::sigalrm_phase1::Phase1Provenance,
    ) {
        self.with_description(|d| d.sigalrm_phase1 = Some(provenance));
    }

    /// Whether this procfs file's snapshot cannot wait to be taken
    /// (`ProcfsFile::capture_cannot_wait`).
    pub(crate) fn procfs_capture_cannot_wait(&self) -> bool {
        self.with_description(|d| {
            d.procfs
                .as_ref()
                .is_some_and(ProcfsFile::capture_cannot_wait)
        })
    }

    /// Whether this procfs snapshot consumes deterministic random bytes.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-955): Review deterministic kernel UUID generation.
    /// Whether this snapshot has maps-format headers needing determinized
    /// device and inode columns.
    pub(crate) fn procfs_needs_mapping_identities(&self) -> bool {
        self.with_description(|d| {
            d.procfs
                .as_ref()
                .is_some_and(ProcfsFile::needs_mapping_identities)
        })
    }

    pub(crate) fn procfs_private_mapping_format(
        &self,
    ) -> Option<crate::procfs::private_maps::Format> {
        self.with_description(|d| {
            d.procfs
                .as_ref()
                .and_then(ProcfsFile::private_mapping_format)
        })
    }

    pub(crate) fn procfs_maps_target(&self) -> Option<crate::procfs::MapsTarget> {
        self.with_description(|d| d.procfs.as_ref().and_then(ProcfsFile::maps_target))
    }

    /// What a process's `maps` was bound to at its open
    /// (`ProcfsFile::memory_map`).
    pub(crate) fn procfs_memory_map(&self) -> Option<crate::procfs::memory_map::Binding> {
        self.with_description(|d| d.procfs.as_ref().and_then(ProcfsFile::memory_map))
    }

    /// Carry the opened Maps/Smaps description, including its old target and cursor,
    /// through the authenticated exec handoff instead of rediscovering a target.
    pub(crate) fn maps_for_exec(&self) -> Option<ProcfsFile> {
        self.with_description(|d| {
            d.procfs
                .as_ref()
                .filter(|p| p.private_mapping_format().is_some())
                .cloned()
        })
    }

    /// Carry a per-process file that follows its record's sequence
    /// (`ProcfsFile::follows_record_sequence`), its sequence included,
    /// through the authenticated exec handoff instead of rediscovering it
    /// with none (`ThreadState::procfs_records_for_exec`).
    pub(crate) fn procfs_record_for_exec(&self) -> Option<ProcfsFile> {
        self.with_description(|d| {
            d.procfs
                .as_ref()
                .filter(|p| p.follows_record_sequence() && p.private_mapping_format().is_none())
                .cloned()
        })
    }

    /// Whether this procfs file lists the guest's own address space, so
    /// its capture must not create a mapping that the listing would show.
    pub(crate) fn procfs_lists_address_space(&self) -> bool {
        self.with_description(|d| {
            d.procfs
                .as_ref()
                .is_some_and(ProcfsFile::lists_address_space)
        })
    }

    /// The view of the mounts this kernel-opened procfs file reads, as its
    /// open pinned it (`ProcfsFile::pinned_mount_view`).
    pub(crate) fn procfs_pinned_mount_view(&self) -> Option<bool> {
        self.with_description(|d| d.procfs.as_ref().and_then(ProcfsFile::pinned_mount_view))
    }

    /// Which rendering of the container mount table this procfs file is, if
    /// it is one (`ProcfsFile::mount_table_rendering`).
    pub(crate) fn procfs_mount_table_rendering(
        &self,
    ) -> Option<crate::procfs::MountTableRendering> {
        self.with_description(|d| {
            d.procfs
                .as_ref()
                .and_then(ProcfsFile::mount_table_rendering)
        })
    }

    pub(crate) fn procfs_needs_mountinfo_identities(&self) -> bool {
        self.with_description(|d| {
            d.procfs
                .as_ref()
                .is_some_and(ProcfsFile::needs_mountinfo_identities)
        })
    }

    /// Whether this descriptor's procfs file is `/proc/locks`, whose rows
    /// Detcore filters to the guest's own (`procfs::locks`).
    pub(crate) fn procfs_lists_locks(&self) -> bool {
        self.with_description(|d| d.procfs.as_ref().is_some_and(ProcfsFile::lists_locks))
    }

    pub(crate) fn procfs_needs_random_uuid(&self) -> bool {
        self.with_description(|d| d.procfs.as_ref().is_some_and(ProcfsFile::needs_random_uuid))
    }

    pub(crate) fn procfs_is_cpuinfo(&self) -> bool {
        self.with_description(|d| d.procfs.as_ref().is_some_and(ProcfsFile::is_cpuinfo))
    }

    /// Whether this is a rendered `/proc/diskstats`, whose snapshot starts
    /// from the host's table as the run first read it
    /// (`ProcfsFile::renders_host_diskstats`).
    pub(crate) fn procfs_renders_host_diskstats(&self) -> bool {
        self.with_description(|d| {
            d.procfs
                .as_ref()
                .is_some_and(ProcfsFile::renders_host_diskstats)
        })
    }

    /// Which of a process's `smaps`, `smaps_rollup` and `numa_maps` this
    /// descriptor is, where Detcore opened it as rendered and renders its
    /// snapshot from the backend's memory map
    /// (`Detcore::render_address_space_file`).
    pub(crate) fn procfs_rendered_address_space_file(
        &self,
    ) -> Option<crate::procfs::AddressSpaceFile> {
        self.with_description(|d| {
            d.procfs
                .as_ref()
                .filter(|procfs| procfs.is_rendered())
                .and_then(ProcfsFile::address_space_file)
        })
    }

    /// Whether this is a task's `arch_status` (`ProcfsFile::is_arch_status`).
    pub(crate) fn procfs_is_arch_status(&self) -> bool {
        self.with_description(|d| d.procfs.as_ref().is_some_and(ProcfsFile::is_arch_status))
    }

    pub(crate) fn procfs_needs_boot_time(&self) -> bool {
        self.with_description(|d| d.procfs.as_ref().is_some_and(ProcfsFile::needs_boot_time))
    }

    /// The kernel process numbers a process's `stat` or `status` named by
    /// number holds in `contents` (`ProcfsFile::kernel_numbers_held`).
    pub(crate) fn procfs_kernel_numbers_held(&self, contents: &[u8]) -> Vec<i32> {
        self.with_description(|d| {
            d.procfs
                .as_ref()
                .map_or_else(Vec::new, |file| file.kernel_numbers_held(contents))
        })
    }

    /// The guest thread whose signal mask the status snapshot about to be
    /// taken reports (`ProcfsFile::status_mask_thread`).
    pub(crate) fn procfs_status_mask_thread(&self, contents: &[u8]) -> Option<i32> {
        self.with_description(|d| {
            d.procfs
                .as_ref()
                .and_then(|file| file.status_mask_thread(contents))
        })
    }

    /// The task a `schedstat` describes (`ProcfsFile::schedstat_thread`).
    pub(crate) fn procfs_schedstat_thread(&self) -> Option<DetTid> {
        self.with_description(|d| d.procfs.as_ref().and_then(ProcfsFile::schedstat_thread))
    }

    /// Before a positioned read at `position`
    /// (`ProcfsFile::drop_schedstat_snapshot_for_read_at`).
    pub(crate) fn procfs_drop_schedstat_snapshot_for_read_at(&self, position: u64) {
        self.with_description(|d| {
            if let Some(procfs) = d.procfs.as_mut() {
                procfs.drop_schedstat_snapshot_for_read_at(position);
            }
        });
    }

    /// The guest's numbers for the kernel's numbers in the snapshot about to
    /// be taken (`ProcfsFile::set_guest_numbers`).
    pub(crate) fn set_procfs_guest_numbers(&self, numbers: std::collections::BTreeMap<i32, i32>) {
        self.with_description(|d| {
            if let Some(file) = d.procfs.as_mut() {
                file.set_guest_numbers(numbers);
            }
        })
    }

    /// Initialize the deterministic snapshot shared by all aliases.
    // TODO-HUMAN-REVIEW(PR-723): Review procfs snapshot identity parameters.
    // TODO-HUMAN-REVIEW(PR-955): Review deterministic UUID snapshot input.
    pub(crate) fn initialize_procfs(&self, contents: Vec<u8>, context: ProcfsSnapshotContext) {
        self.with_description(|d| {
            d.procfs
                .as_mut()
                .expect("procfs fd disappeared while taking its snapshot")
                .initialize(contents, context)
        });
    }

    /// Preview the snapshot bytes at its shared offset, with that offset,
    /// without consuming them.
    pub(crate) fn preview_procfs(&self, maximum: usize) -> Option<(usize, Vec<u8>)> {
        self.with_description(|description| {
            let procfs = description.procfs.as_ref()?;
            let (offset, _) = procfs.position();
            procfs.take_at(offset, maximum).map(|bytes| (offset, bytes))
        })
    }

    /// Advance the shared offset by the bytes a read copied to the guest.
    pub(crate) fn commit_procfs_read(&self, offset: usize, copied: usize) {
        self.with_description(|d| {
            d.procfs
                .as_mut()
                .expect("procfs fd disappeared while committing a read")
                .commit_read(offset, copied)
        });
    }

    /// Whether this is a process's `stat` or a task's `comm` Detcore opened
    /// as rendered, rendered from the task's facts
    /// (`ProcfsFile::renders_task_record`).
    pub(crate) fn procfs_renders_task_record(&self) -> bool {
        self.with_description(|d| {
            d.procfs
                .as_ref()
                .is_some_and(ProcfsFile::renders_task_record)
        })
    }

    /// Whether this is a rendered per-process file that follows its one
    /// record's sequence (`ProcfsFile::follows_record_sequence`).
    pub(crate) fn procfs_follows_record_sequence(&self) -> bool {
        self.with_description(|d| {
            d.procfs
                .as_ref()
                .is_some_and(ProcfsFile::follows_record_sequence)
        })
    }

    /// Whether Linux runs the record's `show` for a read at `position`, and
    /// if so drop the buffered record (`ProcfsFile::drop_stale_task_snapshot`).
    pub(crate) fn procfs_drop_stale_task_snapshot(&self, position: usize) -> bool {
        self.with_description(|d| {
            d.procfs
                .as_mut()
                .is_some_and(|procfs| procfs.drop_stale_task_snapshot(position))
        })
    }

    /// Record where a positioned read left a record's sequence
    /// (`ProcfsFile::note_read_end`).
    pub(crate) fn procfs_note_read_end(&self, position: usize) {
        self.with_description(|d| {
            if let Some(procfs) = d.procfs.as_mut() {
                procfs.note_read_end(position);
            }
        });
    }

    /// Record the outcome of rendering a record for a read at `position`
    /// (`ProcfsFile::note_task_record_rendered`).
    pub(crate) fn procfs_note_task_record_rendered(&self, position: usize, rendered: bool) {
        self.with_description(|d| {
            if let Some(procfs) = d.procfs.as_mut() {
                procfs.note_task_record_rendered(position, rendered);
            }
        });
    }

    /// Move a record's cursor as `seq_lseek` does
    /// (`ProcfsFile::seek_task_record`): whether the caller must run its
    /// `show` now.
    pub(crate) fn seek_procfs_task_record(&self, offset: usize) -> bool {
        self.with_description(|d| {
            d.procfs
                .as_mut()
                .is_some_and(|procfs| procfs.seek_task_record(offset))
        })
    }

    /// Leave a record whose `show` failed at a seek as `seq_lseek` does
    /// (`ProcfsFile::reset_task_record`).
    pub(crate) fn reset_procfs_task_record(&self) {
        self.with_description(|d| {
            if let Some(procfs) = d.procfs.as_mut() {
                procfs.reset_task_record();
            }
        });
    }

    /// Before a fork copies this process's tool state where the tool runs in
    /// each guest process: a per-process file that follows its record's
    /// sequence keeps that sequence in Detcore's global state from then on
    /// (`ProcfsFile::share_sequence`). Only a description this process holds
    /// itself; one already in the global state is shared whole.
    pub(crate) fn share_procfs_sequence(&self) -> bool {
        let mut state = self.open_file.lock().expect("open file mutex poisoned");
        match &mut *state {
            OpenFileState::Local(description) => description
                .procfs
                .as_mut()
                .is_some_and(ProcfsFile::share_sequence),
            OpenFileState::Shared(_) => false,
        }
    }

    /// The identity of this descriptor's open file description, where its
    /// per-process file's record sequence lives in Detcore's global state
    /// between operations (`ProcfsFile::sequence_is_shared`).
    pub(crate) fn procfs_shared_sequence(&self) -> Option<OpenFileId> {
        let state = self.open_file.lock().expect("open file mutex poisoned");
        match &*state {
            OpenFileState::Local(description) => description
                .procfs
                .as_ref()
                .is_some_and(ProcfsFile::sequence_is_shared)
                .then_some(description.id),
            OpenFileState::Shared(_) => None,
        }
    }

    /// This procfs file's record sequence (`ProcfsFile::sequence`).
    pub(crate) fn procfs_sequence(&self) -> Option<crate::procfs::ProcfsSequence> {
        self.with_description(|d| d.procfs.as_ref().map(ProcfsFile::sequence))
    }

    /// Take a procfs file's record sequence as the last operation on its
    /// description left it (`ProcfsFile::adopt_sequence`).
    pub(crate) fn adopt_procfs_sequence(&self, sequence: crate::procfs::ProcfsSequence) {
        self.with_description(|d| {
            if let Some(procfs) = d.procfs.as_mut() {
                procfs.adopt_sequence(sequence);
            }
        });
    }

    /// Read a deterministic procfs snapshot without changing its shared cursor.
    pub(crate) fn take_procfs_at(&self, offset: usize, maximum: usize) -> Option<Vec<u8>> {
        self.with_description(|d| {
            d.procfs
                .as_ref()
                .and_then(|procfs| procfs.take_at(offset, maximum))
        })
    }

    /// Namespace-visible task id and stable proc inode bound at open time.
    pub(crate) fn procfs_timer_slack_binding(&self) -> Option<(i32, u64, u64)> {
        self.with_description(|d| d.procfs.as_ref().and_then(ProcfsFile::timer_slack_binding))
    }

    /// Preview bytes without consuming the snapshot shared by dup/fork aliases.
    pub(crate) fn preview_procfs_timer_slack(
        &self,
        value: u64,
        maximum: usize,
    ) -> Option<TimerSlackReadPreview> {
        self.with_description(|d| {
            d.procfs
                .as_ref()
                .and_then(|procfs| procfs.preview_timer_slack(value, maximum))
        })
    }

    /// Commit bytes only after their guest-memory copy succeeds.
    pub(crate) fn commit_procfs_timer_slack_read(
        &self,
        preview: &TimerSlackReadPreview,
        copied: usize,
    ) {
        self.with_description(|d| {
            d.procfs
                .as_mut()
                .expect("timer-slack procfs state disappeared")
                .commit_timer_slack_read(preview, copied)
        });
    }

    /// Read a fresh timer-slack value at an explicit offset.
    pub(crate) fn take_procfs_timer_slack_at(
        &self,
        value: u64,
        offset: usize,
        maximum: usize,
    ) -> Option<Vec<u8>> {
        self.with_description(|d| {
            d.procfs
                .as_ref()
                .and_then(|procfs| procfs.take_timer_slack_at(value, offset, maximum))
        })
    }

    /// The lock serializing `getdents` and `lseek` on this open file
    /// description across every alias of it. For a shared description it
    /// covers only this process's aliases: the lock protects unsequentialized
    /// threads, and a description is shared only where threads are
    /// sequentialized.
    pub(crate) fn directory_lock(&self) -> Arc<futures::lock::Mutex<()>> {
        match &*self.open_file.lock().expect("open file mutex poisoned") {
            OpenFileState::Local(description) => Arc::clone(&description.directory_lock),
            OpenFileState::Shared(shared) => Arc::clone(&shared.directory_lock),
        }
    }

    /// Whether a `getdents` call has created a directory stream here. Only a
    /// successful read of the host directory creates one, so this also proves
    /// the open file is a directory.
    pub(crate) fn has_directory_stream(&self) -> bool {
        self.with_description(|d| d.directory.is_some())
    }

    /// Whether `getdents` on this open file reads the host directory one
    /// kernel buffer at a time instead of from a sorted stream.
    pub(crate) fn directory_in_host_order(&self) -> bool {
        self.with_description(|d| d.directory_host_order)
    }

    /// Read this open file's directory one kernel buffer at a time until it
    /// is seeked back to 0: its position was moved before the first
    /// `getdents`, or a read of the host directory after the first failed, so
    /// no snapshot of the whole directory could be taken. A guest buffer too
    /// small for the next entry does not land here: the stream answers it
    /// with `EINVAL`, as Linux does. Any stream is dropped, so `lseek`
    /// reaches the kernel again.
    pub(crate) fn use_host_directory_order(&self) {
        self.with_description(|description| {
            description.directory_host_order = true;
            description.directory = None;
        })
    }

    /// Serve this open file's directory as a sorted stream again, from its
    /// next `getdents`: its kernel position is back at 0.
    pub(crate) fn use_directory_stream(&self) {
        self.with_description(|d| d.directory_host_order = false);
    }

    /// Whether the next `getdents` must read the host directory.
    pub(crate) fn directory_needs_snapshot(&self) -> bool {
        self.with_description(|d| {
            d.directory
                .as_ref()
                .is_none_or(DirectoryStream::needs_snapshot)
        })
    }

    /// Install a snapshot freshly read in host order, creating the stream at
    /// position 0 if this is the open file's first `getdents`.
    ///
    /// `mark` is the inode pool's state read just before the host directory
    /// was (see `SnapshotMark`), and `inodes` how to key the inode numbers of
    /// its entries, decided then too.
    pub(crate) fn install_directory_snapshot(
        &self,
        entries: Vec<DirEntry>,
        mark: SnapshotMark,
        inodes: DirectoryInodes,
        order: DirectoryOrder,
    ) {
        self.with_description(|d| {
            let stream = d.directory.get_or_insert_with(DirectoryStream::default);
            stream.install(entries, order);
            stream.note_snapshot_mark(mark);
            stream.note_snapshot_inodes(inodes);
        });
    }

    /// Run `f` on the directory stream shared by every alias of this open file.
    /// `EBADF` if there is none: under `--no-sequentialize-threads`, another
    /// thread can close or replace the descriptor after its caller found one.
    pub(crate) fn with_directory_stream<R>(
        &self,
        f: impl FnOnce(&mut DirectoryStream) -> R,
    ) -> Result<R, Errno> {
        self.with_description(|d| d.directory.as_mut().map(f).ok_or(Errno::EBADF))
    }

    /// The position of this open file's directory stream, 0 before it has
    /// one.
    pub(crate) fn directory_stream_position(&self) -> u64 {
        self.with_description(|d| d.directory.as_ref().map_or(0, DirectoryStream::position))
    }

    /// Move this open file's directory stream to `position`, creating the
    /// stream, without a snapshot, if it has none (`DirectoryStream::seek`):
    /// for a directory Detcore lists from its model alone, whose stream a
    /// seek may come before.
    pub(crate) fn seek_directory_stream(&self, position: u64) {
        self.with_description(|d| {
            d.directory
                .get_or_insert_with(DirectoryStream::default)
                .seek(position)
        })
    }

    /// Return the shared procfs cursor and initialized snapshot length.
    pub(crate) fn procfs_position(&self) -> Option<(usize, Option<usize>)> {
        self.with_description(|d| d.procfs.as_ref().map(ProcfsFile::position))
    }

    /// Whether this is a descriptor's fdinfo Detcore opened as rendered
    /// (`ProcfsFile::rendered_fdinfo_target`).
    pub(crate) fn procfs_is_rendered_fdinfo(&self) -> bool {
        self.with_description(|d| {
            d.procfs
                .as_ref()
                .is_some_and(|procfs| procfs.is_rendered() && procfs.is_fdinfo())
        })
    }

    pub(crate) fn procfs_target_fd(&self) -> Option<i32> {
        self.with_description(|d| d.procfs.as_ref().and_then(ProcfsFile::target_fd))
    }

    /// Move the cursor shared by every alias of a procfs open file as the
    /// guest's `lseek` does (`ProcfsFile::seek_to`).
    pub(crate) fn seek_procfs(&self, offset: usize) {
        self.with_description(|d| {
            d.procfs
                .as_mut()
                .expect("procfs fd disappeared while seeking it")
                .seek_to(offset)
        });
    }

    /// Update the cursor shared by every alias of a procfs open file.
    pub(crate) fn set_procfs_offset(&self, offset: usize) {
        self.with_description(|d| {
            d.procfs
                .as_mut()
                .expect("procfs fd disappeared while updating its offset")
                .set_offset(offset)
        });
    }

    /// Cached stat data attached to the backing object.
    pub fn stat(&self) -> Option<DetStat> {
        self.with_description(|d| d.stat)
    }

    /// Whether Detcore has made the open file description physically nonblocking.
    pub fn physically_nonblocking(&self) -> bool {
        self.with_description(|d| d.physically_nonblocking)
    }

    pub(crate) fn status_flags(&self) -> i32 {
        self.with_description(|d| d.status_flags)
    }

    /// Mark every alias of this open file description physically nonblocking.
    pub fn set_physically_nonblocking(&self) {
        self.with_description(|d| d.physically_nonblocking = true);
    }

    /// Whether the guest's blocking mode of this open file description is
    /// unknown (`OpenFileDescription::blocking_mode_unknown`).
    pub(crate) fn blocking_mode_unknown(&self) -> bool {
        self.with_description(|d| d.blocking_mode_unknown)
    }

    /// Mark the guest's blocking mode of every alias of this open file
    /// description unknown (`OpenFileDescription::blocking_mode_unknown`).
    pub(crate) fn mark_blocking_mode_unknown(&self) {
        self.with_description(|d| d.blocking_mode_unknown = true);
    }

    /// Whether this is a FIFO that may still become an internally polled pipe
    /// (see `OpenFileDescription::fifo_awaiting_guest_peer`).
    pub(crate) fn fifo_awaiting_guest_peer(&self) -> bool {
        self.with_description(|d| d.fifo_awaiting_guest_peer)
    }

    /// Whether this is a FIFO opened by path that joined internal polling
    /// (see `OpenFileDescription::named_fifo`).
    pub(crate) fn is_named_fifo(&self) -> bool {
        self.with_description(|d| d.named_fifo)
    }

    /// Mark a `Regular` FIFO as one that may become an internally polled pipe.
    pub(crate) fn mark_fifo_awaiting_guest_peer(&self) {
        self.with_description(|d| {
            debug_assert_eq!(d.ty, FdType::Regular);
            d.fifo_awaiting_guest_peer = true;
        });
    }

    /// The FIFO is now physically nonblocking (Detcore set `O_NONBLOCK`, here
    /// or through another process sharing the open file description): treat
    /// it from now on as a scheduler-managed pipe, as `pipe2` creates.
    pub(crate) fn fifo_joins_internal_polling(&self) {
        self.with_description(|d| {
            debug_assert!(d.fifo_awaiting_guest_peer);
            d.fifo_awaiting_guest_peer = false;
            d.named_fifo = true;
            d.ty = FdType::Pipe;
            d.physically_nonblocking = true;
        });
    }

    /// Update file status flags for every alias of this open file description.
    pub fn set_status_flags(&self, flags: i32) {
        self.with_description(|description| {
            // F_SETFL cannot change how this description was opened. Access
            // checks must retain these bits even when an alias passes only
            // O_NONBLOCK (whose access-mode bits happen to spell O_RDONLY).
            let mutable = (OFlag::O_APPEND
                | OFlag::O_ASYNC
                | OFlag::O_DIRECT
                | OFlag::O_NOATIME
                | OFlag::O_NONBLOCK)
                .bits();
            description.status_flags = (description.status_flags & !mutable) | (flags & mutable);
            description.physically_nonblocking = oflags_nonblocking(description.status_flags);
            // The guest has now set the mode itself.
            description.blocking_mode_unknown = false;
        })
    }

    // TODO-HUMAN-REVIEW(PR-912): Review open-file sharing of socket receive timestamps.
    /// Record the logical time at which a socket delivered its most recent packet.
    pub(crate) fn set_socket_receive_timestamp(&self, timestamp: LogicalTime) {
        self.with_description(|d| d.socket_receive_timestamp = Some(timestamp));
    }

    /// Return the last receive timestamp shared by every alias of this socket.
    pub(crate) fn socket_receive_timestamp(&self) -> Option<LogicalTime> {
        self.with_description(|d| d.socket_receive_timestamp)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1064)
    /// Mark this open file as a `NETLINK_SOCK_DIAG` socket. Shared across every
    /// dup/fork alias of the same open file description.
    pub(crate) fn set_sock_diag(&self) {
        self.with_description(|d| d.sock_diag = true);
    }

    // TODO-HUMAN-REVIEW(PR-1064)
    /// Whether this open file is a `NETLINK_SOCK_DIAG` socket whose dump replies
    /// must have their socket inode numbers determinized.
    pub(crate) fn is_sock_diag(&self) -> bool {
        self.with_description(|d| d.sock_diag)
    }

    /// Mark this open file as a `NETLINK_ROUTE` socket. Like `set_sock_diag`
    /// this applies to the open file description, so a dup or fork alias of the
    /// same socket is covered too.
    pub(crate) fn set_netlink_route(&self) {
        self.with_description(|d| d.netlink_route = true);
    }

    /// Whether this open file is a `NETLINK_ROUTE` socket whose link-dump
    /// replies carry live interface counters.
    pub(crate) fn is_netlink_route(&self) -> bool {
        self.with_description(|d| d.netlink_route)
    }

    /// Update whether this open file is connected to a loopback peer.
    pub(crate) fn set_loopback_peer(&self, loopback_peer: bool) {
        self.with_description(|d| d.loopback_peer = loopback_peer);
    }

    /// Whether this open file is a socket connected to a loopback peer.
    pub(crate) fn is_loopback_peer(&self) -> bool {
        self.with_description(|d| d.loopback_peer)
    }

    /// Mark this open file as an external network trace channel.
    pub(crate) fn set_network_channel(&self) {
        self.with_description(|d| d.network_channel = true);
    }

    /// Whether this open file is an external network trace channel.
    pub(crate) fn is_network_channel(&self) -> bool {
        self.with_description(|d| d.network_channel)
    }

    /// Record the socket's `SO_RCVLOWAT`.
    pub(crate) fn set_network_lowat(&self, lowat: usize) {
        self.with_description(|d| d.network_lowat = Some(lowat));
    }

    /// The socket's `SO_RCVLOWAT`.
    pub(crate) fn network_lowat(&self) -> usize {
        self.with_description(|d| d.network_lowat.unwrap_or(1))
    }

    /// Record the socket's family and type for the network trace.
    pub(crate) fn set_network_socket(&self, kind: NetworkSocketKind) {
        self.with_description(|d| d.network_socket = kind);
    }

    /// The socket's family and type as the network trace classified it.
    pub(crate) fn network_socket(&self) -> NetworkSocketKind {
        self.with_description(|d| d.network_socket)
    }

    /// Mark this open file as one end of a `socketpair(2)`. Like
    /// `set_netlink_route` this applies to the open file description, so a dup
    /// or fork alias of the same endpoint is covered too -- which is the case
    /// that matters, since the usual way to use a socketpair is to fork and let
    /// each side inherit one end.
    pub(crate) fn set_socketpair_endpoint(&self) {
        self.with_description(|d| d.socketpair_endpoint = true);
    }

    /// Make this socket and `peer`, the two sockets of one `socketpair`, each
    /// other's peer for `SCM_RIGHTS` bookkeeping (`queue_rights_to_peer`);
    /// `stream` says whether they are `SOCK_STREAM`. Kept by every alias,
    /// as the description is.
    pub(crate) fn pair_sockets(&self, peer: &DetFd, stream: bool) {
        self.pair_with(&peer.open_file_handle(), stream);
    }

    /// As [`Self::pair_sockets`], with the other socket named by a handle on
    /// its description: an accepted connection and the socket whose
    /// `connect` made it. Nothing if that socket is gone.
    pub(crate) fn pair_with(&self, peer: &OpenFileHandle, stream: bool) {
        let this = self.open_file_handle();
        if peer
            .with_description(|d| {
                d.rights.peer = Some(this);
                d.rights.stream = stream;
            })
            .is_some()
        {
            self.with_description(|d| {
                d.rights.peer = Some(peer.clone());
                d.rights.stream = stream;
            });
        }
    }

    /// Record that this is an `AF_UNIX` socket of type `unix_type`
    /// (`SOCK_STREAM`, `SOCK_SEQPACKET` or `SOCK_DGRAM`).
    pub(crate) fn set_unix_type(&self, unix_type: i32) {
        self.with_description(|d| d.rights.unix_type = Some(unix_type));
    }

    /// This `AF_UNIX` socket's type, if Detcore saw it made.
    pub(crate) fn unix_type(&self) -> Option<i32> {
        self.with_description(|d| d.rights.unix_type)
    }

    /// Record the name a `bind` gave this `AF_UNIX` socket, and which bind
    /// of that name it was (`generation`).
    pub(crate) fn set_unix_name(&self, name: Box<[u8]>, generation: u64) {
        self.with_description(|d| {
            d.rights.unix_name = Some((name, generation));
            // Any socket can now send to it by name, unseen by the queue.
            give_up_completeness(&mut d.rights);
            d.rights.sealed = false;
        });
    }

    /// This socket and `peer`, the two sockets of a datagram or seqpacket
    /// `socketpair`, which nothing but each other can reach, start with
    /// queues that hold every message (`SocketRights::complete`).
    pub(crate) fn mark_complete_pair(&self, peer: &DetFd) {
        let mark = |d: &mut OpenFileDescription| {
            d.rights.complete = true;
            d.rights.sealed = true;
        };
        self.with_description(mark);
        peer.with_description(mark);
    }

    /// Messages Detcore does not see sent may now reach this socket: a
    /// `bind` gave it a name, or it set `SO_PASSCRED`, which gives it one at
    /// its next send (`unix_autobind`). Its queue no longer holds every
    /// message (`SocketRights::complete`).
    pub(crate) fn give_up_completeness(&self) {
        self.with_description(|d| {
            give_up_completeness(&mut d.rights);
            d.rights.sealed = false;
        });
    }

    /// Another descriptor table may now hold this socket without Detcore
    /// modeling it as this description: it was passed over `SCM_RIGHTS` or
    /// binder, where the receipt may record an open file of its own. A send
    /// through that copy reaches this socket's peer unseen, and a receive
    /// through it takes from this socket's queue unseen, so neither queue
    /// holds every message any more (`SocketRights::complete`).
    pub(crate) fn give_up_pair_completeness(&self) {
        let peer = self.with_description(|d| {
            give_up_completeness(&mut d.rights);
            d.rights.sealed = false;
            d.rights.peer.clone()
        });
        if let Some(peer) = peer {
            peer.with_description(|d| {
                give_up_completeness(&mut d.rights);
                d.rights.sealed = false;
            });
        }
    }

    /// Whether this socket's peer has a queue that holds every message
    /// (`SocketRights::complete`), which a send of data alone through this
    /// socket must join (`Detcore::note_send_without_control`).
    pub(crate) fn peer_queue_is_complete(&self) -> bool {
        self.with_description(|d| d.rights.peer.clone())
            .and_then(|peer| peer.with_description(|d| d.rights.complete))
            .unwrap_or(false)
    }

    /// The name a `bind` gave this `AF_UNIX` socket, which bind of that name
    /// it was, and the socket's type, if Detcore knows all three.
    pub(crate) fn unix_name_and_type(&self) -> Option<(Box<[u8]>, u64, i32)> {
        self.with_description(|d| {
            let (name, generation) = d.rights.unix_name.clone()?;
            Some((name, generation, d.rights.unix_type?))
        })
    }

    /// This datagram socket's messages no longer reach the peer Detcore
    /// paired it with: a `connect` gave it another peer, or none
    /// (`unix_dgram_connect`). Neither socket is the other's peer any more,
    /// so no send through either queues on the other; this socket's queue is
    /// forgotten, as Linux purges its receive queue when its peer changes
    /// (`unix_dgram_disconnected`); and neither queue follows its socket's
    /// messages one for one any more (`SocketRights::lockstep_lost`), since
    /// the old peer can still name this socket, and this socket the old peer.
    /// A connect to the socket that already is its peer changes nothing in
    /// Linux; Detcore forgets the queue then too, which costs only joins.
    /// The same holds when a send through it finds its peer gone
    /// (`unix_dgram_sendmsg` purges the queue, `ECONNREFUSED`). The messages
    /// forgotten, for their descriptions to be released
    /// (`Detcore::settle_released_rights`).
    pub(crate) fn unpair_rights_peer(&self) -> Vec<SentDescriptor> {
        let (peer, purged) = self.with_description(|d| {
            d.rights.lockstep_lost = true;
            d.rights.suspects = 0;
            d.rights.complete = false;
            (
                d.rights.peer.take(),
                d.rights
                    .queued
                    .drain(..)
                    .flat_map(|message| message.descriptors)
                    .collect::<Vec<_>>(),
            )
        });
        if let Some(peer) = peer {
            peer.with_description(|d| {
                d.rights.peer = None;
                d.rights.lockstep_lost = true;
                give_up_completeness(&mut d.rights);
            });
        }
        purged
    }

    /// Every message queued to this socket, taken as its description goes
    /// with its last reference (`FileMetadata::remove_fd`), for their
    /// descriptions to be released (`Detcore::settle_released_rights`), as
    /// Linux releases them when the receiving socket closes
    /// (`unix_release_sock`).
    pub(crate) fn take_all_queued_rights(&self) -> Vec<SentDescriptor> {
        self.with_description(|d| {
            // The oldest messages, or every one once the queue lost lockstep,
            // may have been taken unseen by a receive Detcore could not match.
            let suspects = if d.rights.lockstep_lost {
                d.rights.queued.len()
            } else {
                d.rights.suspects
            };
            d.rights.suspects = 0;
            d.rights
                .queued
                .drain(..)
                .enumerate()
                .flat_map(|(position, message)| {
                    message.descriptors.into_iter().map(move |mut sent| {
                        sent.suspect = position < suspects;
                        sent
                    })
                })
                .collect()
        })
    }

    /// Whether this socket has a peer Detcore knows (`pair_sockets`,
    /// `pair_with`), and if so whether the pair is `SOCK_STREAM`.
    pub(crate) fn rights_peer_is_stream(&self) -> Option<bool> {
        self.with_description(|d| d.rights.peer.is_some().then_some(d.rights.stream))
    }

    /// Queue on this socket's peer one message of `bytes` bytes sent through
    /// this socket, with the descriptors it carried, in order (`sent`): `Ok`
    /// once queued, each copy now counted in flight
    /// ([`OpenFileDescription::in_flight`]), or, for a message of data alone,
    /// also once passed over because the peer's queue does not hold every
    /// message (`SocketRights::complete`), where data alone has no place to
    /// keep; `Err` with the descriptors back when this socket has no peer
    /// Detcore knows, or the peer is gone.
    ///
    /// A stream's message of data alone after another joins it, as a stream's
    /// reads count bytes only. At [`MAX_QUEUED_RIGHTS_MESSAGES`] the queue
    /// gives up its messages of data alone, if it held any
    /// ([`give_up_completeness`]), and no longer holds every message; if it is
    /// still full, it drops the oldest message, unreleased, as Linux still
    /// holds it, and no longer follows its messages one for one.
    pub(crate) fn queue_rights_to_peer(
        &self,
        sent: Vec<SentDescriptor>,
        bytes: usize,
    ) -> Result<(), Vec<SentDescriptor>> {
        let Some(peer) = self.with_description(|d| d.rights.peer.clone()) else {
            return Err(sent);
        };
        // Counted outside the peer's lock: a copy may be of the peer itself.
        let copies: Vec<InFlightOpenFile> = sent
            .iter()
            .filter_map(|sent| sent.open_file.clone())
            .collect();
        let mut message = Some(QueuedMessage {
            descriptors: sent,
            bytes,
        });
        let outcome = peer.with_description(|d| {
            let rights = &mut d.rights;
            let message = message.take().expect("a message is queued once");
            rights.rights_sent |= !message.descriptors.is_empty();
            if rights.queued.len() >= MAX_QUEUED_RIGHTS_MESSAGES {
                // No room to keep messages of data alone: none carried a
                // descriptor, so the queue stays in lockstep without them.
                give_up_completeness(rights);
            }
            if message.descriptors.is_empty() {
                if !rights.complete {
                    return (false, None);
                }
                // A stream's reads count bytes, not messages: data alone that
                // follows data alone joins it.
                if rights.stream
                    && let Some(newest) = rights.queued.back_mut()
                    && newest.descriptors.is_empty()
                {
                    newest.bytes += message.bytes;
                    return (true, None);
                }
            }
            let dropped = (rights.queued.len() >= MAX_QUEUED_RIGHTS_MESSAGES)
                .then(|| {
                    rights.lockstep_lost = true;
                    rights.suspects = rights.suspects.saturating_sub(1);
                    rights.queued.pop_front()
                })
                .flatten();
            rights.queued.push_back(message);
            (true, dropped)
        });
        match outcome {
            Some((queued, dropped)) => {
                if queued {
                    for copy in &copies {
                        copy.enter_flight();
                    }
                }
                // Linux still holds the message dropped to make room: what
                // it carried stays counted in flight.
                drop(dropped);
                Ok(())
            }
            None => Err(message
                .take()
                .map(|message| message.descriptors)
                .unwrap_or_default()),
        }
    }

    /// A message this socket sent may have carried descriptors that were not
    /// queued on its peer, or reached the peer out of the queue's order
    /// (`SocketRights::lockstep_lost`).
    pub(crate) fn lose_peer_lockstep(&self) {
        if let Some(peer) = self.with_description(|d| d.rights.peer.clone()) {
            peer.with_description(|d| {
                d.rights.lockstep_lost = true;
                give_up_completeness(&mut d.rights);
            });
        }
    }

    /// As [`Self::lose_peer_lockstep`], for a message that may have carried
    /// descriptors its peer's queue does not hold, so that a receive
    /// through the peer may install some (`SocketRights::rights_sent`).
    pub(crate) fn lose_peer_lockstep_with_rights(&self) {
        if let Some(peer) = self.with_description(|d| d.rights.peer.clone()) {
            peer.with_description(|d| {
                d.rights.lockstep_lost = true;
                d.rights.rights_sent = true;
                give_up_completeness(&mut d.rights);
            });
        }
    }

    /// A call took up to `taken` messages from this socket that Detcore
    /// could not match to its queue. The queue is kept: Detcore cannot tell
    /// whether the messages the call took carried descriptors, and Linux
    /// still delivers the others, in order. The call may have taken as many
    /// of the oldest queued messages, which are suspect from now on
    /// (`SocketRights::suspects`), and forgotten when a receive takes a later
    /// one (`Self::take_queued_rights`). Where the queue holds every message
    /// (`SocketRights::complete`) but holds none, the call took one it never
    /// held, which may have carried descriptors: the queue is out of
    /// lockstep. Under record and replay (`legacy`), the rule existing
    /// recordings were made with: any such call while something is queued
    /// puts the queue out of lockstep. A stream socket's queue no longer holds
    /// every message after such a call (`give_up_completeness`), as one stream
    /// read takes messages of data alone along with at most one that carried
    /// descriptors.
    pub(crate) fn lose_lockstep_if_queued(&self, taken: usize, legacy: bool) {
        self.with_description(|d| {
            let rights = &mut d.rights;
            if rights.stream && !rights.queued.is_empty() {
                // A stream read takes any number of messages of data alone
                // before the one whose descriptors it detaches: the suspects
                // count only messages that carried descriptors.
                give_up_completeness(rights);
            }
            if !rights.queued.is_empty() {
                if legacy {
                    rights.lockstep_lost = true;
                }
                rights.suspects = rights
                    .suspects
                    .saturating_add(taken)
                    .min(rights.queued.len());
            } else if rights.complete {
                rights.lockstep_lost = true;
                give_up_completeness(rights);
            }
        });
    }

    /// Whether a message is queued to this socket.
    pub(crate) fn has_queued_rights(&self) -> bool {
        self.with_description(|d| !d.rights.queued.is_empty())
    }

    /// Whether a receive through this socket may install descriptors
    /// sent to it: false only where nothing but its peer can reach it,
    /// through sends Detcore sees (`SocketRights::sealed`), and no message
    /// sent to it carried any (`SocketRights::rights_sent`).
    pub(crate) fn may_deliver_rights(&self) -> bool {
        self.with_description(|d| !d.rights.sealed || d.rights.rights_sent)
    }

    /// A call took the oldest message Linux held for this datagram or
    /// seqpacket socket without naming its descriptors: a read with no
    /// control buffer, or a receive whose message Linux delivered with none.
    /// Where the queue holds every message (`SocketRights::complete`) and has
    /// nothing suspect, that is the oldest queued message, which `fits` (what
    /// the call reports of it) must agree with: it is taken, and its
    /// descriptors returned for their descriptions to be released, as Linux
    /// releases them. `None` otherwise, for the caller to count the call as
    /// one that may have taken a message unseen
    /// ([`Self::lose_lockstep_if_queued`]); a queue whose oldest message does
    /// not fit, or that holds none, missed a message, so it is out of
    /// lockstep and no longer holds every message.
    pub(crate) fn take_head_message(
        &self,
        fits: impl FnOnce(&QueuedMessage) -> bool,
    ) -> Option<Vec<SentDescriptor>> {
        self.with_description(|d| {
            let rights = &mut d.rights;
            if !rights.complete || rights.suspects > 0 {
                return None;
            }
            match rights.queued.front() {
                Some(head) if fits(head) => rights.queued.pop_front().map(|head| head.descriptors),
                _ => {
                    rights.lockstep_lost = true;
                    give_up_completeness(rights);
                    None
                }
            }
        })
    }

    /// A receive took one message of data alone from this datagram or
    /// seqpacket socket, delivered without descriptors and without
    /// `MSG_CTRUNC`. A queue of descriptors alone loses nothing to it. One
    /// that holds every message (`SocketRights::complete`) takes its oldest,
    /// which must be of data alone ([`Self::take_head_message`]), or, while
    /// some are suspect, counts one more message that may be gone.
    pub(crate) fn take_data_only_message(&self) {
        let suspect = self.with_description(|d| d.rights.complete && d.rights.suspects > 0);
        if suspect {
            self.lose_lockstep_if_queued(1, false);
        } else {
            let _ = self.take_head_message(|head| head.descriptors.is_empty());
        }
    }

    /// A stream read took `read` bytes from this socket without naming the
    /// descriptors it detached: a read with no control buffer, or a receive
    /// that installed none. Where the queue holds every message
    /// (`SocketRights::complete`) and has nothing suspect, the bytes come
    /// from the oldest messages on ([`stream_walk`]), and `fits` must agree
    /// with the descriptors they detach, if any: then they are taken, and the
    /// descriptors returned for their descriptions to be released, as Linux
    /// releases them. `None` otherwise, for the caller to count the call as
    /// one that may have taken a message unseen
    /// ([`Self::lose_lockstep_if_queued`]); a queue the read does not fit
    /// missed a message, so it is out of lockstep and no longer holds every
    /// message.
    pub(crate) fn take_stream_bytes(
        &self,
        read: usize,
        fits: impl FnOnce(Option<&[SentDescriptor]>) -> bool,
    ) -> Option<Vec<SentDescriptor>> {
        self.with_description(|d| {
            let rights = &mut d.rights;
            if !rights.complete || !rights.stream || rights.suspects > 0 {
                return None;
            }
            match stream_walk(rights, read) {
                Some(walk)
                    if fits(
                        walk.detached
                            .map(|index| rights.queued[index].descriptors.as_slice()),
                    ) =>
                {
                    Some(apply_stream_walk(rights, walk))
                }
                _ => {
                    rights.lockstep_lost = true;
                    give_up_completeness(rights);
                    None
                }
            }
        })
    }

    /// How many bytes a stream read of this socket that asked for `asked`
    /// took, where Linux does not report it: one that took its bytes and then
    /// failed copying an output out (`EFAULT`). Where the queue holds every
    /// message (`SocketRights::complete`) and has nothing suspect, Linux took
    /// what [`stream_read_count`] says, provided it copied every byte; `None`
    /// otherwise.
    pub(crate) fn stream_read_count(&self, asked: usize) -> Option<usize> {
        self.with_description(|d| {
            let rights = &d.rights;
            (rights.complete && rights.stream && rights.suspects == 0)
                .then(|| stream_read_count(rights, asked))
        })
    }

    /// The descriptors of the message queued to this socket that a receive,
    /// which installed descriptors whose identities are `received` from one
    /// message (`truncated` when Linux set `MSG_CTRUNC` for it), took: the
    /// oldest queued message they name ([`names_sent_rights`]), or `None`;
    /// whether the queue still follows the socket's messages one for one
    /// (`SocketRights::lockstep_lost`); and whether the receipt may join the
    /// descriptions that message holds ([`TakenRights::joinable`]).
    ///
    /// Linux delivers a socket's messages in order, so the messages queued
    /// before it were taken by calls Detcore could not match: they are
    /// forgotten with it ([`TakenRights::forgotten`]). A `MSG_PEEK` receive
    /// (`peek`) leaves every message queued, as Linux does.
    ///
    /// The message the receive took is the oldest Linux still held, so it is
    /// at most the first past the suspects (`SocketRights::suspects`). A
    /// match there or among them is trusted only where no later suspect, nor
    /// the first message past them, names the same files: otherwise the
    /// receive may have taken that later one after the matched one was
    /// released unseen, and the matched message is a stale entry, which is
    /// not joined; the messages that may be that receive's own stay suspect.
    /// A match beyond the first message past the suspects names a message
    /// Linux cannot have delivered yet, so this receive took one that was
    /// never queued here, naming the same files, or one a call Detcore did
    /// not count took: `None`, and every message up to the matched one stays
    /// queued, as Linux still holds them, suspect from now on, with the queue
    /// out of lockstep. A peek may name a later message (`SO_PEEK_OFF`): it
    /// is trusted wherever it is, where the queue is in lockstep and no other
    /// queued message names the same files, so that no other message can be
    /// the one peeked. A receipt that names no queued message took one never
    /// queued here: the queue is out of lockstep.
    ///
    /// A stream receive of `bytes` bytes from a queue that holds every
    /// message (`SocketRights::complete`) took those bytes from the oldest
    /// messages on ([`stream_walk`]): the message whose descriptors they
    /// detached is the one taken, where the received descriptors name it; a
    /// queue the receive does not fit missed a message, and is out of lockstep
    /// and no longer holds every message.
    ///
    /// Under record and replay (`legacy`) the rule existing recordings were
    /// made with holds: a match anywhere takes the matched message and
    /// forgets those before it, and a match past the first puts the queue out
    /// of lockstep.
    pub(crate) fn take_queued_rights(
        &self,
        received: &[FileIdentity],
        truncated: bool,
        peek: bool,
        legacy: bool,
        bytes: Option<usize>,
    ) -> Option<TakenRights> {
        self.with_description(|d| {
            let rights = &mut d.rights;
            let names =
                |sent: &QueuedMessage| names_sent_rights(&sent.descriptors, received, truncated);
            // A stream receive whose queue holds every message took its bytes
            // from the oldest messages on, and the descriptors it installed are
            // those of the message whose descriptors those bytes detached.
            if rights.stream
                && rights.complete
                && rights.suspects == 0
                && !peek
                && !legacy
                && let Some(read) = bytes
            {
                match stream_walk(rights, read) {
                    Some(walk)
                        if walk
                            .detached
                            .is_some_and(|index| names(&rights.queued[index])) =>
                    {
                        return Some(TakenRights {
                            sent: apply_stream_walk(rights, walk),
                            lockstep: !rights.lockstep_lost,
                            joinable: true,
                            forgotten: Vec::new(),
                        });
                    }
                    _ => {
                        rights.lockstep_lost = true;
                        give_up_completeness(rights);
                    }
                }
            }
            let Some(index) = rights.queued.iter().position(names) else {
                if !legacy && !peek {
                    rights.lockstep_lost = true;
                    give_up_completeness(rights);
                }
                return None;
            };
            // Where each call takes one message, a message's place is its
            // index; a stream read takes messages of data alone along with one
            // that carried descriptors, so there a message's place counts only
            // those that carried some (the suspects count them alike).
            let stream = rights.stream;
            let mut carried = 0;
            let places: Vec<usize> = rights
                .queued
                .iter()
                .enumerate()
                .map(|(index, message)| {
                    let place = if stream { carried } else { index };
                    carried += usize::from(!message.descriptors.is_empty());
                    place
                })
                .collect();
            let place = places[index];
            let suspects = rights.suspects;
            let ambiguous = rights
                .queued
                .iter()
                .zip(&places)
                .skip(index + 1)
                .take_while(|(_, later)| **later <= suspects)
                .any(|(sent, _)| names(sent));
            let unique = !rights.lockstep_lost && !rights.queued.iter().skip(index + 1).any(names);
            let joinable = (place <= suspects && !ambiguous) || (peek && unique);
            if legacy {
                if index > 0 {
                    rights.lockstep_lost = true;
                }
            } else if place > suspects && !peek {
                // Up to the match, every message stays, suspect. Where messages
                // of data alone go first, a stream's places are its indices.
                if stream {
                    give_up_completeness(rights);
                    rights.suspects = (place + 1).min(rights.queued.len());
                } else {
                    rights.suspects = (index + 1).min(rights.queued.len());
                    give_up_completeness(rights);
                }
                rights.lockstep_lost = true;
                return None;
            }
            let (sent, forgotten) = if peek {
                (
                    rights
                        .queued
                        .get(index)
                        .map(|message| message.descriptors.clone()),
                    Vec::new(),
                )
            } else {
                rights.suspects = if joinable || place > suspects {
                    0
                } else {
                    suspects - place
                };
                let forgotten = rights
                    .queued
                    .drain(..index)
                    .flat_map(|message| message.descriptors)
                    .collect();
                let sent = rights.queued.pop_front().map(|message| message.descriptors);
                rights.suspects = rights.suspects.min(rights.queued.len());
                (sent, forgotten)
            };
            sent.map(|sent| TakenRights {
                sent,
                lockstep: !rights.lockstep_lost,
                joinable,
                forgotten,
            })
        })
    }

    /// Whether this open file originated in `socketpair(2)`, for live internal
    /// routing. Guest-only readiness instead requires `is_guest_held`.
    pub(crate) fn is_socketpair_endpoint(&self) -> bool {
        self.with_description(|d| d.socketpair_endpoint)
    }

    /// Mark this socket as one whose peer is a guest of this container. Like
    /// `set_socketpair_endpoint` it applies to the open file description, so
    /// dup, fork and `SCM_RIGHTS` aliases are covered too.
    pub(crate) fn set_guest_peer(&self) {
        self.with_description(|d| d.guest_peer = true);
    }

    /// Whether this socket uses internal execution routing: a `socketpair(2)`
    /// endpoint, or a socket whose peer record/replay found to be a guest.
    /// Detcore keeps such a socket physically nonblocking
    /// and polls a blocking call on it in the caller's turn, as for a pipe
    /// (`syscall_targets_internal_fd`).
    pub(crate) fn is_internal_socket(&self) -> bool {
        self.with_description(|d| d.socketpair_endpoint || d.guest_peer)
    }

    /// Mark this open file as one whose other end only guests hold (see
    /// `is_guest_held`). It applies to the open file description, so dup and
    /// fork aliases are covered too.
    pub(crate) fn set_guest_held(&self) {
        self.with_description(|d| d.guest_held = true);
    }

    /// Withdraw `set_guest_held`, permanently: a process outside Detcore's
    /// view may now hold this object or its other end.
    pub(crate) fn clear_guest_held(&self) {
        self.with_description(|d| d.guest_held = false);
    }

    /// Whether only guests hold this object's other end, so that only a guest
    /// action can change its readiness: what `Resources::polls_guest_state_only`
    /// asks of every descriptor a poll waits on.
    pub(crate) fn is_guest_held(&self) -> bool {
        self.with_description(|d| d.guest_held)
    }

    /// Mark this socket as one a guest made in this run (see
    /// `is_guest_created`), for its open file description.
    pub(crate) fn set_guest_created(&self) {
        self.with_description(|d| d.guest_created = true);
    }

    /// Withdraw `set_guest_created`, permanently: the socket was passed out
    /// of the guest.
    pub(crate) fn clear_guest_created(&self) {
        self.with_description(|d| d.guest_created = false);
    }

    /// Whether a guest made this socket with `socket(2)` or `socketpair(2)`
    /// in this run and never passed it out of the guest, so only guests hold
    /// it. Production code reads it through an `OpenFileHandle`.
    #[cfg(test)]
    pub(crate) fn is_guest_created(&self) -> bool {
        self.with_description(|d| d.guest_created)
    }

    /// Mark this open file as a connection returned by `accept(2)` or
    /// `accept4(2)`. It applies to the open file description, so dup and fork
    /// aliases are covered too.
    pub(crate) fn set_accepted_connection(&self) {
        self.with_description(|d| d.accepted_connection = true);
    }

    /// Record whether a receive with `MSG_ERRQUEUE` on this socket never
    /// waits. It applies to the open file description, so dup and fork aliases
    /// are covered too.
    pub(crate) fn set_error_queue_never_waits(&self, never_waits: bool) {
        self.with_description(|d| d.error_queue_never_waits = never_waits);
    }

    /// Whether a receive with `MSG_ERRQUEUE` on this socket never waits.
    pub(crate) fn error_queue_never_waits(&self) -> bool {
        self.with_description(|d| d.error_queue_never_waits)
    }

    /// Whether `accept(2)` or `accept4(2)` returned this open file.
    pub(crate) fn is_accepted_connection(&self) -> bool {
        self.with_description(|d| d.accepted_connection)
    }

    /// Mark this open file as a listener whose blocking `accept(2)` runs in
    /// the caller's turn under record/replay. It applies to the open file
    /// description, so dup and fork aliases are covered too.
    pub(crate) fn set_accept_in_turn_eligible(&self) {
        self.with_description(|d| d.accept_in_turn_eligible = true);
    }

    /// Whether a blocking `accept(2)` on this open file runs in the caller's
    /// turn: it was marked eligible and has not been exported.
    pub(crate) fn is_accept_in_turn_listener(&self) -> bool {
        self.with_description(|d| d.accept_in_turn_eligible && !d.exported)
    }

    /// Whether this open file was marked eligible, exported or not.
    pub(crate) fn is_accept_in_turn_eligible(&self) -> bool {
        self.with_description(|d| d.accept_in_turn_eligible)
    }

    /// Mark this socket as one whose read side a guest shut down. It applies
    /// to the open file description, so dup and fork aliases are covered too.
    pub(crate) fn set_read_shutdown_requested(&self) {
        self.with_description(|d| d.read_shutdown_requested = true);
    }

    /// Whether a guest shut this socket's read side down. Only such a datagram
    /// socket can have a blocking receive that Linux ends at once while its
    /// nonblocking attempt fails with `EAGAIN`, because Linux sets a datagram
    /// socket's read shutdown only in its own `shutdown(2)`
    /// (`syscalls::helpers::finish_after_read_shutdown`).
    pub(crate) fn read_shutdown_requested(&self) -> bool {
        self.with_description(|d| d.read_shutdown_requested)
    }

    /// Mark this open file description's locks as followed by
    /// `procfs::locks::LockLedger`. Permanent.
    pub(crate) fn set_lock_tracked(&self) {
        self.with_description(|d| d.lock_tracked = true);
    }

    /// Whether `procfs::locks::LockLedger` follows this description's locks.
    pub(crate) fn is_lock_tracked(&self) -> bool {
        self.with_description(|d| d.lock_tracked)
    }

    /// The alias group of this description, where it is one of the
    /// container's standard streams that is one open file description with
    /// another of them (`FileMetadata::setup_stdio`).
    pub(crate) fn stdio_alias_group(&self) -> Option<OpenFileId> {
        self.with_description(|d| d.stdio_alias_group)
    }

    /// Put this description, one of the container's standard streams, in
    /// alias group `group`.
    pub(crate) fn set_stdio_alias_group(&self, group: OpenFileId) {
        self.with_description(|d| d.stdio_alias_group = Some(group));
    }

    /// Mark this description as registered on reception over `SCM_RIGHTS`.
    /// Permanent.
    pub(crate) fn set_received_for_locks(&self) {
        self.with_description(|d| d.received_for_locks = true);
    }

    /// Whether this description was registered on reception over
    /// `SCM_RIGHTS`.
    pub(crate) fn is_received_for_locks(&self) -> bool {
        self.with_description(|d| d.received_for_locks)
    }

    /// Mark this description as one that may hold an OFD lock. Permanent.
    pub(crate) fn set_may_hold_ofd_lock(&self) {
        self.with_description(|d| d.may_hold_ofd_lock = true);
    }

    /// Whether this description may hold an OFD lock.
    pub(crate) fn may_hold_ofd_lock(&self) -> bool {
        self.with_description(|d| d.may_hold_ofd_lock)
    }

    /// Mark this open file's record sequence as one Detcore cannot know
    /// here (`OpenFileDescription::record_sequence_unknown`). Permanent.
    /// Only a local description, which every procfs file's is.
    pub(crate) fn mark_record_sequence_unknown(&self) {
        let mut state = self.open_file.lock().expect("open file mutex poisoned");
        if let OpenFileState::Local(description) = &mut *state {
            description.record_sequence_unknown = true;
        }
    }

    /// Whether this open file's record sequence is one Detcore cannot know
    /// here (`OpenFileDescription::record_sequence_unknown`). Read without a
    /// lease, so a shared model (a random device, a timerfd), which is never
    /// so marked, answers false at no cost.
    pub(crate) fn record_sequence_unknown(&self) -> bool {
        let state = self.open_file.lock().expect("open file mutex poisoned");
        match &*state {
            OpenFileState::Local(description) => description.record_sequence_unknown,
            OpenFileState::Shared(_) => false,
        }
    }

    /// Give this descriptor's open file, which it alone refers to and which
    /// Detcore just made for a rendered file's placeholder, the identity `id`
    /// of the open file the placeholder belongs to, as this process's
    /// registry names it (`FileMetadata::adopt_placeholder`,
    /// `Detcore::adopt_received_placeholder`), so it takes the record
    /// sequence that open file keeps in Detcore's global state.
    ///
    /// Detcore's flock model keys its locks, waiters and the references it
    /// follows by this identity (`procfs::locks::LockLedger`), so a rebind
    /// must leave it nothing to move, and every caller makes it so
    /// (`OpenFileDescription::never_the_flock_models`): the description is
    /// one Detcore made in the same handler, before any guest call went
    /// through it, for a descriptor it had no record of, discovered in the table
    /// (`FileMetadata::add_discovered_fd`) or received
    /// (`Detcore::rebuild_received_placeholder`). Such a description holds no
    /// lock and waits for none, the ledger follows none of its references,
    /// and it is escaped (`mark_flock_mode_unobserved`), so the model never
    /// decides a flock through it, under either identity: the kernel does,
    /// on every backend that rebinds (those where the tool runs in each guest
    /// process, all of which mirror flock onto the host).
    pub(crate) fn rebind_open_file_id(&self, id: OpenFileId) {
        let mut state = self.open_file.lock().expect("open file mutex poisoned");
        if let OpenFileState::Local(description) = &mut *state {
            debug_assert!(
                description.never_the_flock_models(),
                "a rebound description is escaped and was never the flock model's"
            );
            description.id = id;
        }
    }

    /// Whether this description may take another open file's identity
    /// ([`Self::rebind_open_file_id`]): only one Detcore's flock model never
    /// decided a call through and never will, because it is escaped, the
    /// ledger follows none of its references, and no flock took its inode.
    /// The rebind itself asserts the same predicate in debug builds.
    #[cfg(test)]
    pub(crate) fn may_rebind_open_file_id(&self) -> bool {
        let state = self.open_file.lock().expect("open file mutex poisoned");
        match &*state {
            OpenFileState::Local(description) => description.never_the_flock_models(),
            OpenFileState::Shared(_) => false,
        }
    }

    /// Mark this description as one whose references Detcore may not all see
    /// (`OpenFileDescription::references_escaped`). Permanent.
    pub(crate) fn mark_references_escaped(&self) {
        self.with_description(|d| d.references_escaped = true);
    }

    /// Whether Detcore may not see every reference to this description.
    pub(crate) fn references_escaped(&self) -> bool {
        self.with_description(|d| d.references_escaped)
    }

    /// The inode a `flock` through this description is kept on, once taken.
    pub(crate) fn flock_identity(&self) -> Option<FlockIdentity> {
        self.with_description(|d| d.flock_identity.clone())
    }

    /// Record the inode a `flock` through this description is kept on.
    pub(crate) fn set_flock_identity(&self, identity: FlockIdentity) {
        self.with_description(|d| d.flock_identity = Some(identity));
    }

    /// Whether Detcore's flock model decided a call through this description,
    /// so that its mappings matter to the model (`Detcore::note_flock_mapping`).
    pub(crate) fn is_flock_modelled(&self) -> bool {
        self.with_description(|d| d.flock_identity.as_ref().is_some_and(|id| id.modelled))
    }

    /// Record that Detcore's flock model decided a call through this
    /// description.
    pub(crate) fn set_flock_modelled(&self) {
        self.with_description(|d| {
            if let Some(identity) = &mut d.flock_identity {
                identity.modelled = true;
            }
        });
    }

    /// Mark this open file as passed over `SCM_RIGHTS`. Permanent.
    pub(crate) fn set_exported(&self) {
        self.with_description(|d| d.exported = true);
    }

    /// Whether this open file was passed over `SCM_RIGHTS`.
    pub(crate) fn is_exported(&self) -> bool {
        self.with_description(|d| d.exported)
    }

    /// Record the receive timeout a successful `setsockopt(SO_RCVTIMEO)` set.
    pub(crate) fn set_receive_timeout(&self, timeout: ReceiveTimeout) {
        self.with_description(|d| d.receive_timeout = timeout);
    }

    /// The receive timeout `setsockopt(SO_RCVTIMEO)` calls set through any
    /// descriptor Detcore tracks for this open file description, dup, fork and
    /// exec aliases included. A copy that reached a guest some other way, which
    /// Detcore tracks as a description of its own, does not update it; record
    /// checks the model against the kernel before a blocking accept relies on
    /// it.
    pub(crate) fn receive_timeout(&self) -> ReceiveTimeout {
        self.with_description(|d| d.receive_timeout)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#2373)
    /// Return the known `flock(2)` state for this open file description.
    ///
    /// The outer `None` means Detcore did not observe the descriptor's history.
    /// `Some(None)` means known-unlocked; `Some(Some(mode))` means the kernel
    /// granted `LOCK_SH` or `LOCK_EX` through this handler.
    pub(crate) fn known_flock_mode(&self) -> Option<Option<i32>> {
        self.with_description(|description| {
            description
                .flock_mode_known
                .then_some(description.flock_mode)
        })
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#2373)
    /// Record the `flock(2)` mode this open file description now holds. Every
    /// `dup`/`fork` alias observes the change, matching the kernel, where one
    /// `flock` lock belongs to the whole open file description.
    ///
    /// A grant (`Some`) also marks the description as one that may hold a lock
    /// ([`Self::flock_may_have_been_granted`]), even while its mode is unknown
    /// and the mode itself is therefore not recorded.
    pub(crate) fn set_flock_mode(&self, mode: Option<i32>) {
        self.with_description(|description| {
            if mode.is_some() {
                description.flock_ever_granted = true;
            }
            if description.flock_mode_known {
                description.flock_mode = mode;
            }
        })
    }

    /// Mark the kernel lock state unknown without changing the kernel lock.
    /// This is required for live-discovered and externally received file
    /// descriptions, whose history Detcore did not observe.
    pub(crate) fn forget_flock_mode(&self) {
        self.with_description(|description| {
            description.flock_mode = None;
            description.flock_mode_known = false;
        })
    }

    /// Record that Detcore never observed this description's lock history, as
    /// opposed to having known it and then invalidated it. Used for descriptors
    /// that existed before Detcore began observing the guest.
    pub(crate) fn mark_flock_mode_unobserved(&self) {
        self.forget_flock_mode();
        self.with_description(|description| {
            description.flock_mode_ever_known = false;
            // A lock may have been taken before Detcore saw the description.
            description.flock_ever_granted = true;
            // Another process may hold it too, under a reference Detcore never
            // saw taken.
            description.references_escaped = true;
        })
    }

    /// True when a `flock` lock may ever have been granted through this open
    /// file description, so that it may still hold one. False only for a
    /// description Detcore created and through which it never served a
    /// successful `LOCK_SH` or `LOCK_EX`.
    pub(crate) fn flock_may_have_been_granted(&self) -> bool {
        self.with_description(|description| description.flock_ever_granted)
    }

    /// True when a cached lock claim existed and may now be wrong.
    pub(crate) fn flock_mode_may_be_stale(&self) -> bool {
        self.with_description(|description| {
            description.flock_mode_ever_known && !description.flock_mode_known
        })
    }
}

impl fmt::Display for DetFd {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "DetFd({})", self.fd)
    }
}

/// Restores descriptor aliasing after deserialization: every descriptor with
/// the same `OpenFileId` refers to one open file description object again.
///
/// Serialization writes an `Arc`'s contents, not its identity, so two
/// descriptors that shared one description (after `dup`) come back as two
/// independent copies, and a read through one would no longer move the
/// other's cursor. Within one table an `OpenFileId` names exactly one open
/// file description, so the copies with one id are identical, and the first
/// (lowest descriptor number) is kept. Descriptor flags (`FD_CLOEXEC`) stay
/// per descriptor.
pub(crate) fn intern_open_files(handles: &mut std::collections::HashMap<RawFd, DetFd>) {
    let mut fds: Vec<RawFd> = handles.keys().copied().collect();
    fds.sort_unstable();
    let mut first: std::collections::HashMap<OpenFileId, DetFd> = std::collections::HashMap::new();
    for fd in fds {
        let detfd = handles.get_mut(&fd).expect("listed descriptor");
        match first.get(&detfd.open_file_id()) {
            Some(kept) => detfd.share_open_file_with(kept),
            None => {
                first.insert(detfd.open_file_id(), detfd.clone());
            }
        }
    }
}

impl DetFd {
    /// A copy of this local description's model, for tests of the global
    /// store.
    #[cfg(test)]
    pub(crate) fn model_for_test(&self) -> OpenFileModel {
        self.with_description(|d| {
            OpenFileModel(serde_json::from_value(serde_json::to_value(&*d).unwrap()).unwrap())
        })
    }

    /// Moves this open file description's model out to a test's canonical
    /// store and makes every alias a shared handle, keeping this process's
    /// directory lock. Production promotion uses the acknowledged lease protocol.
    #[cfg(test)]
    fn share_for_test(&self) -> OpenFileModel {
        let mut state = self.open_file.lock().expect("open file mutex poisoned");
        let OpenFileState::Local(description) = &*state else {
            panic!("open file description is already shared");
        };
        let shared = SharedOpenFile {
            id: description.id,
            directory_lock: Arc::clone(&description.directory_lock),
        };
        let OpenFileState::Local(description) =
            std::mem::replace(&mut *state, OpenFileState::Shared(shared))
        else {
            unreachable!("checked above");
        };
        OpenFileModel(description)
    }
}

#[cfg(test)]
pub(crate) fn install_test_shared_open_file_channel() {
    let _ = tests::MemoryChannel::installed();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_devices_are_character_devices_one_eight_and_one_nine() {
        let chr = libc::S_IFCHR | 0o666;
        assert!(is_random_device(chr, libc::makedev(1, 8)));
        assert!(is_random_device(chr, libc::makedev(1, 9)));
        // /dev/null, /dev/zero and /dev/full share major 1.
        for minor in [3, 5, 7] {
            assert!(!is_random_device(chr, libc::makedev(1, minor)));
        }
        // The same numbers on another kind of file name no device.
        for kind in [libc::S_IFBLK, libc::S_IFREG, libc::S_IFIFO] {
            assert!(!is_random_device(kind | 0o666, libc::makedev(1, 9)));
        }
        assert!(!is_random_device(chr, libc::makedev(9, 1)));
    }

    /// The protocols whose receive handler reads the error queue never wait
    /// for it, with the type's SOCK_NONBLOCK and SOCK_CLOEXEC bits or not;
    /// AF_UNIX, AF_NETLINK and IPv4 L2TP wait, and a protocol or family that is
    /// not modelled keeps Detcore's polling.
    #[test]
    fn error_queue_reads_never_wait_on_the_protocols_that_read_the_queue() {
        let flags = libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC;
        for family in [libc::AF_INET, libc::AF_INET6] {
            let ping = if family == libc::AF_INET {
                libc::IPPROTO_ICMP
            } else {
                libc::IPPROTO_ICMPV6
            };
            for (socket_type, protocol) in [
                (libc::SOCK_STREAM, 0),
                (libc::SOCK_STREAM, libc::IPPROTO_TCP),
                (libc::SOCK_STREAM, libc::IPPROTO_MPTCP),
                (libc::SOCK_STREAM, libc::IPPROTO_SCTP),
                (libc::SOCK_SEQPACKET, 0),
                (libc::SOCK_SEQPACKET, libc::IPPROTO_SCTP),
                (libc::SOCK_DGRAM, 0),
                (libc::SOCK_DGRAM, libc::IPPROTO_UDP),
                (libc::SOCK_DGRAM, libc::IPPROTO_UDPLITE),
                (libc::SOCK_DGRAM, ping),
                (libc::SOCK_RAW, libc::IPPROTO_RAW),
                (libc::SOCK_RAW, libc::IPPROTO_UDP),
            ] {
                assert!(error_queue_read_never_waits(family, socket_type, protocol));
                assert!(error_queue_read_never_waits(
                    family,
                    socket_type | flags,
                    protocol
                ));
            }
            for (socket_type, protocol) in [
                (libc::SOCK_DGRAM, libc::IPPROTO_TCP),
                (libc::SOCK_STREAM, libc::IPPROTO_UDP),
                // IPPROTO_SMC: not modelled.
                (libc::SOCK_STREAM, 256),
                (libc::SOCK_RDM, 0),
            ] {
                assert!(!error_queue_read_never_waits(family, socket_type, protocol));
            }
        }
        // ICMP ping is IPv4's protocol only, ICMPv6 IPv6's.
        assert!(!error_queue_read_never_waits(
            libc::AF_INET6,
            libc::SOCK_DGRAM,
            libc::IPPROTO_ICMP
        ));
        assert!(!error_queue_read_never_waits(
            libc::AF_INET,
            libc::SOCK_DGRAM,
            libc::IPPROTO_ICMPV6
        ));
        // l2tp_ip6_recvmsg reads the error queue, l2tp_ip_recvmsg waits.
        assert!(error_queue_read_never_waits(
            libc::AF_INET6,
            libc::SOCK_DGRAM,
            IPPROTO_L2TP
        ));
        assert!(!error_queue_read_never_waits(
            libc::AF_INET,
            libc::SOCK_DGRAM,
            IPPROTO_L2TP
        ));
        for socket_type in [libc::SOCK_RAW, libc::SOCK_DGRAM] {
            assert!(error_queue_read_never_waits(
                libc::AF_PACKET,
                socket_type,
                0
            ));
        }
        for socket_type in [libc::SOCK_STREAM, libc::SOCK_DGRAM, libc::SOCK_SEQPACKET] {
            assert!(!error_queue_read_never_waits(libc::AF_UNIX, socket_type, 0));
        }
        assert!(!error_queue_read_never_waits(
            libc::AF_NETLINK,
            libc::SOCK_RAW,
            libc::NETLINK_ROUTE
        ));
        assert!(!error_queue_read_never_waits(
            libc::AF_VSOCK,
            libc::SOCK_STREAM,
            0
        ));
    }

    #[test]
    fn native_exec_directory_lease_probe_preserves_idle_stream_and_aliases() {
        use crate::native_exec::NativeExecCutError;

        let id = OpenFileId::new(DetTid::from_raw(42), 7);
        let fd = DetFd::new(30, OFlag::O_DIRECTORY, FdType::Regular, id);
        let alias = fd.clone().with_fd(31);
        fd.install_directory_snapshot(
            vec![
                DirEntry {
                    name: b"b".to_vec(),
                    ino: 19,
                    off: 41,
                    ty: libc::DT_REG,
                },
                DirEntry {
                    name: b"a".to_vec(),
                    ino: 17,
                    off: 43,
                    ty: libc::DT_REG,
                },
            ],
            SnapshotMark {
                retirements: 3,
                threads_counted: 7,
            },
            DirectoryInodes::new(11, None),
            DirectoryOrder::NameBytes,
        );
        alias
            .with_directory_stream(|stream| stream.advance(1))
            .unwrap();
        let before = serde_json::to_value(&fd).unwrap();
        assert_eq!(
            fd.validate_native_exec_runtime_state().unwrap(),
            alias.validate_native_exec_runtime_state().unwrap()
        );
        assert_eq!(serde_json::to_value(&fd).unwrap(), before);
        let lock = alias.directory_lock();
        let lease = lock.try_lock().unwrap();
        assert_eq!(
            fd.validate_native_exec_runtime_state(),
            Err(NativeExecCutError::LiveCapability("directory lease"))
        );
        assert!(
            lock.try_lock().is_none(),
            "validation must not release the owned lease"
        );
        drop(lease);
        assert_eq!(serde_json::to_value(&fd).unwrap(), before);
        assert_eq!(
            fd.with_directory_stream(|stream| stream.position())
                .unwrap(),
            1
        );
        let guard = fd.open_file.lock().unwrap();
        assert_eq!(
            alias.validate_native_exec_runtime_state(),
            Err(NativeExecCutError::LockUnavailable("open_file"))
        );
        drop(guard);
        fd.validate_native_exec_runtime_state().unwrap();
    }

    #[test]
    fn native_exec_shared_descriptor_probe_never_takes_canonical_model() {
        let id = OpenFileId::new(DetTid::from_raw(42), 9);
        let fd = DetFd::new(30, OFlag::empty(), FdType::Rng, id);
        fd.advance_random_device_offset(37);
        let model = fd.share_for_test();
        let alias = fd.clone().with_fd(31);
        // Do not install a channel. with_description on this handle would
        // need one; a local runtime probe must not touch canonical state.
        assert_eq!(
            fd.validate_native_exec_runtime_state().unwrap(),
            alias.validate_native_exec_runtime_state().unwrap()
        );
        assert_eq!(model.0.random_device_offset, 37);
        let lock = alias.directory_lock();
        let lease = lock.try_lock().unwrap();
        assert_eq!(
            fd.validate_native_exec_runtime_state(),
            Err(crate::native_exec::NativeExecCutError::LiveCapability(
                "directory lease"
            ))
        );
        drop(lease);
        assert_eq!(model.0.random_device_offset, 37);
    }

    #[test]
    fn random_access_modes_survive_status_updates_through_aliases() {
        for mode in [
            OFlag::O_RDONLY,
            OFlag::O_WRONLY,
            OFlag::O_RDWR,
            OFlag::O_ACCMODE,
            OFlag::O_PATH,
        ] {
            let original = DetFd::new(
                3,
                mode,
                FdType::Rng,
                OpenFileId::new(DetTid::from_raw(1), 0),
            );
            let alias = original.clone().with_fd(4);
            for update in [
                OFlag::O_NONBLOCK,
                OFlag::O_RDWR,
                OFlag::O_WRONLY,
                OFlag::O_PATH | OFlag::O_NONBLOCK,
            ] {
                alias.set_status_flags(update.bits());
                assert_eq!(
                    original.status_flags() & (OFlag::O_ACCMODE | OFlag::O_PATH).bits(),
                    mode.bits()
                );
                assert_eq!(
                    original.is_nonblocking(),
                    update.contains(OFlag::O_NONBLOCK)
                );
            }
        }
    }

    #[test]
    fn random_status_updates_preserve_immutable_flags_and_replace_mutable_flags() {
        let immutable = OFlag::O_RDWR | OFlag::O_SYNC;
        let original = DetFd::new(
            3,
            immutable | OFlag::O_APPEND | OFlag::O_NONBLOCK,
            FdType::Rng,
            OpenFileId::new(DetTid::from_raw(1), 0),
        );
        let alias = original.clone().with_fd(4);
        alias.set_status_flags((OFlag::O_WRONLY | OFlag::O_ASYNC | OFlag::O_NOATIME).bits());
        assert_eq!(
            original.status_flags(),
            (immutable | OFlag::O_ASYNC | OFlag::O_NOATIME).bits()
        );
        assert!(!original.physically_nonblocking());
        alias.set_status_flags(OFlag::O_NONBLOCK.bits());
        assert_eq!(
            original.status_flags(),
            (immutable | OFlag::O_NONBLOCK).bits()
        );
        assert!(original.physically_nonblocking());
    }

    #[test]
    fn dup_shares_open_file_state_but_not_slot_flags() {
        let owner = DetTid::from_raw(10);
        let original = DetFd::new(
            3,
            OFlag::O_NONBLOCK,
            FdType::Socket,
            OpenFileId::new(owner, 0),
        );
        let duplicate = original.clone().with_fd(4).with_fd_flags(OFlag::O_CLOEXEC);

        assert_eq!(original.open_file_id(), duplicate.open_file_id());
        assert!(
            !original.is_cloexec(),
            "dup must not alter the source fd flags"
        );
        assert!(
            duplicate.is_cloexec(),
            "dup3(O_CLOEXEC) applies to the new slot"
        );
        assert!(
            duplicate.is_nonblocking(),
            "dup must preserve shared status flags"
        );

        duplicate.set_status_flags(OFlag::empty().bits());
        assert!(
            !original.is_nonblocking(),
            "status flag changes through one alias must be visible through every alias"
        );

        let timestamp = LogicalTime::from_nanos(2_345_678_901);
        original.set_socket_receive_timestamp(timestamp);
        assert_eq!(duplicate.socket_receive_timestamp(), Some(timestamp));

        assert!(!original.is_sock_diag());
        original.set_sock_diag();
        assert!(
            duplicate.is_sock_diag(),
            "sock_diag marking through one alias must be visible through every alias"
        );

        assert!(!original.is_guest_held());
        duplicate.set_guest_held();
        assert!(
            original.is_guest_held(),
            "guest-held marking through one alias must be visible through every alias"
        );
        original.clear_guest_held();
        assert!(
            !duplicate.is_guest_held(),
            "withdrawing guest-held state through one alias must withdraw it for every alias"
        );

        assert!(!original.is_loopback_peer());
        duplicate.set_loopback_peer(true);
        assert!(
            original.is_loopback_peer(),
            "loopback-peer marking through one alias must be visible through every alias"
        );
        original.set_loopback_peer(false);
        assert!(
            !duplicate.is_loopback_peer(),
            "reconnects through one alias must clear loopback state for every alias"
        );

        assert!(!original.is_internal_socket());
        duplicate.set_guest_peer();
        assert!(
            original.is_internal_socket(),
            "a guest peer belongs to the open file, so every alias must see it"
        );
        assert!(!original.is_socketpair_endpoint());
    }

    /// `flock(2)` locks belong to the open file description, so every `dup`
    /// alias must see one lock, while a separate `open` of the same file is a
    /// distinct contender with its own state.
    #[test]
    fn flock_mode_is_shared_by_dup_aliases_and_private_to_separate_opens() {
        let owner = DetTid::from_raw(10);
        let original = DetFd::new(
            3,
            OFlag::empty(),
            FdType::Regular,
            OpenFileId::new(owner, 0),
        );
        let duplicate = original.clone().with_fd(4);
        let separate_open = DetFd::new(
            5,
            OFlag::empty(),
            FdType::Regular,
            OpenFileId::new(owner, 1),
        );

        assert_eq!(original.known_flock_mode(), Some(None));
        original.set_flock_mode(Some(libc::LOCK_SH));
        assert_eq!(
            duplicate.known_flock_mode(),
            Some(Some(libc::LOCK_SH)),
            "a dup alias shares the open file description's flock lock"
        );
        assert_eq!(
            separate_open.known_flock_mode(),
            Some(None),
            "a separate open of the same file is an independent flock contender"
        );

        duplicate.set_flock_mode(Some(libc::LOCK_EX));
        assert_eq!(original.known_flock_mode(), Some(Some(libc::LOCK_EX)));
        duplicate.set_flock_mode(None);
        assert_eq!(
            original.known_flock_mode(),
            Some(None),
            "LOCK_UN through one alias releases the whole open file description"
        );

        duplicate.forget_flock_mode();
        assert_eq!(
            original.known_flock_mode(),
            None,
            "unknown state must be shared by every alias of the open file description"
        );
        original.set_flock_mode(Some(libc::LOCK_SH));
        assert_eq!(
            duplicate.known_flock_mode(),
            None,
            "a later call cannot make externally mutable lock state reliable again"
        );
    }

    #[test]
    fn pidfd_target_is_shared_by_descriptor_aliases() {
        let target = DetPid::from_raw(41);
        let original = DetFd::new(
            3,
            OFlag::O_CLOEXEC,
            FdType::Pidfd,
            OpenFileId::new(DetTid::from_raw(7), 0),
        );
        let duplicate = original.clone().with_fd(4);

        assert_eq!(original.pidfd_target(), None);
        original.set_pidfd_target(target);
        assert_eq!(original.pidfd_target(), Some(target));
        assert_eq!(duplicate.pidfd_target(), Some(target));
    }

    #[test]
    fn toggling_logical_nonblocking_preserves_physical() {
        // Models FIONBIO on an fd that Detcore forced physically nonblocking for
        // the scheduler: the guest-visible flag changes, but the physical state
        // must survive.
        let owner = DetTid::from_raw(10);
        let fd = DetFd::new(3, OFlag::empty(), FdType::Socket, OpenFileId::new(owner, 0));
        fd.set_physically_nonblocking();
        fd.set_logical_nonblocking(true);
        assert!(fd.is_nonblocking());
        assert!(fd.physically_nonblocking());

        fd.set_logical_nonblocking(false);
        assert!(
            !fd.is_nonblocking(),
            "the guest must observe O_NONBLOCK cleared"
        );
        assert!(
            fd.physically_nonblocking(),
            "the scheduler's physical nonblocking state must be preserved"
        );

        fd.set_logical_nonblocking(true);
        assert!(fd.is_nonblocking());
        assert!(
            fd.physically_nonblocking(),
            "setting the logical flag must not discard physical tracking"
        );

        // The both-flags setter still tracks physical alongside logical.
        fd.set_nonblocking(false);
        assert!(!fd.is_nonblocking());
        assert!(!fd.physically_nonblocking());
    }

    /// An inotify instance received without its sender's open file
    /// description has an unknown blocking mode, shared by every alias, until
    /// the guest sets the mode: F_SETFL (`set_status_flags`), a FIONBIO that
    /// Detcore satisfies logically (`set_logical_nonblocking`) or forwards
    /// (`set_nonblocking`).
    #[test]
    fn an_unknown_blocking_mode_is_known_once_the_guest_sets_it() {
        let owner = DetTid::from_raw(10);
        let setters: [fn(&DetFd); 3] = [
            |fd| fd.set_status_flags(0),
            |fd| fd.set_logical_nonblocking(true),
            |fd| fd.set_nonblocking(false),
        ];
        for (index, set) in setters.into_iter().enumerate() {
            let fd = DetFd::new(
                3,
                OFlag::O_NONBLOCK,
                FdType::Inotify,
                OpenFileId::new(owner, index as u64),
            );
            let alias = fd.clone().with_fd(4);
            assert!(!fd.blocking_mode_unknown());
            fd.mark_blocking_mode_unknown();
            assert!(alias.blocking_mode_unknown(), "setter {index}: alias");
            set(&alias);
            assert!(!fd.blocking_mode_unknown(), "setter {index}");
        }
    }

    #[test]
    fn procfs_offsets_are_shared_by_dup_aliases() {
        let owner = DetTid::from_raw(10);
        let original = DetFd::new(
            3,
            OFlag::empty(),
            FdType::Regular,
            OpenFileId::new(owner, 0),
        );
        // A `seq_file` file, which serves reads at any position.
        original.set_procfs(ProcfsFile::from_path(Path::new("/proc/version")).unwrap());
        original.initialize_procfs(
            Vec::new(),
            ProcfsSnapshotContext {
                virtual_pid: 1,
                ..ProcfsSnapshotContext::default()
            },
        );
        let duplicate = original.clone().with_fd(4);

        assert_eq!(original.preview_procfs(2).unwrap(), (0, b"Li".to_vec()));
        assert_eq!(duplicate.procfs_position().unwrap().0, 0);
        original.commit_procfs_read(0, 2);
        assert_eq!(duplicate.procfs_position().unwrap().0, 2);
        assert_eq!(duplicate.take_procfs_at(4, 1).unwrap(), b"x");
        assert_eq!(original.procfs_position().unwrap().0, 2);

        // A read that copied nothing leaves the shared cursor in place.
        let (offset, _) = duplicate.preview_procfs(4).unwrap();
        duplicate.commit_procfs_read(offset, 0);
        assert_eq!(original.procfs_position().unwrap().0, 2);

        duplicate.set_procfs_offset(0);
        assert_eq!(
            original.preview_procfs(128).unwrap(),
            (
                0,
                crate::procfs::FixedProcfsFile::Version
                    .contents(&crate::machine::MachineDescription::default())
            )
        );
    }

    #[test]
    fn random_device_offsets_are_shared_by_dup_aliases() {
        let owner = DetTid::from_raw(10);
        let original = DetFd::new(3, OFlag::empty(), FdType::Rng, OpenFileId::new(owner, 0));
        let duplicate = original.clone().with_fd(4);

        assert_eq!(original.random_device_offset(), 0);
        duplicate.advance_random_device_offset(50);
        assert_eq!(original.random_device_offset(), 50);
    }

    #[test]
    fn random_device_stream_commits_returned_prefix_for_dup_aliases() {
        let owner = DetTid::from_raw(10);
        let id = OpenFileId::new(owner, 0);
        let original = DetFd::new(3, OFlag::empty(), FdType::Rng, id);
        let duplicate = original.clone().with_fd(4);
        original.advance_random_device_offset(7);

        assert_eq!(
            duplicate.with_random_device_stream(|offset| {
                assert_eq!(offset, 7);
                // A copy requesting eight bytes completed only this prefix.
                Ok::<_, ()>(3)
            }),
            Ok(3)
        );
        assert_eq!(original.random_device_offset(), 10);
        assert_eq!(duplicate.random_device_offset(), 10);
        assert_eq!(
            original.with_random_device_stream(|offset| {
                assert_eq!(offset, 10);
                Ok::<_, ()>(2)
            }),
            Ok(2)
        );
        assert_eq!(original.random_device_offset(), 12);
        assert_eq!(duplicate.random_device_offset(), 12);
        assert_eq!(original.open_file_id(), id);
        assert_eq!(duplicate.open_file_id(), id);
        assert!(Arc::ptr_eq(&original.open_file, &duplicate.open_file));
    }

    #[test]
    fn random_device_stream_preserves_error_identity_and_cursor() {
        let owner = DetTid::from_raw(10);
        let original = DetFd::new(3, OFlag::empty(), FdType::Rng, OpenFileId::new(owner, 0));
        let duplicate = original.clone().with_fd(4);
        original.advance_random_device_offset(11);
        let failure = Arc::new(String::from("copy failure"));

        let error = duplicate
            .with_random_device_stream(|offset| {
                assert_eq!(offset, 11);
                Err(Arc::clone(&failure))
            })
            .unwrap_err();
        assert!(Arc::ptr_eq(&error, &failure));
        assert_eq!(original.random_device_offset(), 11);
        assert_eq!(duplicate.random_device_offset(), 11);
        assert_eq!(original.open_file_id(), duplicate.open_file_id());
    }

    #[test]
    fn random_device_stream_saturates_independently_of_partitioning() {
        let owner = DetTid::from_raw(10);
        for initial in [u64::MAX - 2, u64::MAX] {
            let whole = DetFd::new(3, OFlag::empty(), FdType::Rng, OpenFileId::new(owner, 0));
            let split = DetFd::new(4, OFlag::empty(), FdType::Rng, OpenFileId::new(owner, 1));
            whole.with_description(|d| d.random_device_offset = initial);
            split.with_description(|d| d.random_device_offset = initial);

            assert_eq!(
                whole.with_random_device_stream(|offset| {
                    assert_eq!(offset, initial);
                    Ok::<_, ()>(4)
                }),
                Ok(4)
            );
            assert_eq!(whole.random_device_offset(), u64::MAX);
            assert_eq!(
                split.with_random_device_stream(|offset| {
                    assert_eq!(offset, initial);
                    Ok::<_, ()>(1)
                }),
                Ok(1)
            );
            assert_eq!(split.random_device_offset(), initial.saturating_add(1));
            assert_eq!(
                split.with_random_device_stream(|offset| {
                    assert_eq!(offset, initial.saturating_add(1));
                    Ok::<_, ()>(3)
                }),
                Ok(3)
            );
            assert_eq!(split.random_device_offset(), u64::MAX);
        }
    }

    #[test]
    fn random_device_stream_holds_alias_lock_while_copying() {
        let owner = DetTid::from_raw(10);
        let original = DetFd::new(3, OFlag::empty(), FdType::Rng, OpenFileId::new(owner, 0));
        let duplicate = original.clone().with_fd(4);

        assert_eq!(
            original.with_random_device_stream(|offset| {
                assert_eq!(offset, 0);
                // A nonblocking attempt cannot deadlock the copy, and observes
                // whether another thread could access the shared cursor here.
                assert!(
                    std::thread::spawn(move || {
                        matches!(
                            duplicate.open_file.try_lock(),
                            Err(std::sync::TryLockError::WouldBlock)
                        )
                    })
                    .join()
                    .unwrap()
                );
                Ok::<_, ()>(5)
            }),
            Ok(5)
        );
        assert_eq!(original.random_device_offset(), 5);
    }

    /// A path argument whose conversion checks that no alias's description
    /// lock is held while it converts.
    struct PathProbe {
        alias: DetFd,
    }

    impl AsRef<Path> for PathProbe {
        fn as_ref(&self) -> &Path {
            assert!(
                self.alias.open_file.try_lock().is_ok(),
                "an argument conversion ran under the description lock"
            );
            Path::new("/probe")
        }
    }

    #[test]
    fn argument_conversions_run_before_the_description_lock() {
        let original = DetFd::new(
            3,
            OFlag::O_RDONLY,
            FdType::Regular,
            OpenFileId::new(DetTid::from_raw(11), 0),
        );
        let alias = original.clone().with_fd(4);
        original.set_path(PathProbe {
            alias: alias.clone(),
        });
        let original = original.with_path(PathProbe {
            alias: alias.clone(),
        });
        assert_eq!(alias.path(), Some(PathBuf::from("/probe")));
        drop(original);
    }

    #[test]
    fn separate_opens_have_distinct_identity() {
        let owner = DetTid::from_raw(10);
        let first = DetFd::new(
            3,
            OFlag::empty(),
            FdType::Regular,
            OpenFileId::new(owner, 0),
        );
        let second = DetFd::new(
            4,
            OFlag::empty(),
            FdType::Regular,
            OpenFileId::new(owner, 1),
        );

        assert_ne!(first.open_file_id(), second.open_file_id());
    }

    /// The canonical store of the tests' shared open file descriptions, and
    /// the channel installed for this test process. Each test uses its own
    /// creator task, so tests running in parallel never share a description.
    #[derive(Default)]
    pub(super) struct MemoryChannel {
        store: Mutex<std::collections::HashMap<OpenFileId, Canonical>>,
    }

    struct Canonical {
        /// `None` while a lease holds the model.
        model: Option<OpenFileModel>,
        sequence: u64,
        takes: usize,
    }

    struct InstalledMemoryChannel(&'static MemoryChannel);

    impl SharedOpenFileChannel for InstalledMemoryChannel {
        fn promote(&self, model: &OpenFileModel) -> Result<(), SharedOpenFileError> {
            let mut store = self.0.store.lock().unwrap();
            if store.contains_key(&model.id()) {
                return Err(SharedOpenFileError("duplicate test promotion".into()));
            }
            let copied = serde_json::from_value(serde_json::to_value(model).unwrap()).unwrap();
            store.insert(
                model.id(),
                Canonical {
                    model: Some(copied),
                    sequence: 0,
                    takes: 0,
                },
            );
            Ok(())
        }

        fn take(&self, id: OpenFileId) -> Result<OpenFileLease, SharedOpenFileError> {
            let mut store = self.0.store.lock().unwrap();
            let canonical = store
                .get_mut(&id)
                .ok_or_else(|| SharedOpenFileError(format!("{id:?} is not shared")))?;
            let model = canonical
                .model
                .take()
                .ok_or_else(|| SharedOpenFileError(format!("{id:?} is already leased")))?;
            canonical.sequence += 1;
            canonical.takes += 1;
            Ok(OpenFileLease {
                id,
                sequence: canonical.sequence,
                model,
            })
        }

        fn publish(&self, lease: OpenFileLease) -> Result<(), SharedOpenFileError> {
            let mut store = self.0.store.lock().unwrap();
            let canonical = store
                .get_mut(&lease.id)
                .ok_or_else(|| SharedOpenFileError(format!("{:?} is not shared", lease.id)))?;
            if canonical.model.is_some() || canonical.sequence != lease.sequence {
                return Err(SharedOpenFileError(format!(
                    "{:?}: publish names no current lease",
                    lease.id
                )));
            }
            canonical.model = Some(lease.model);
            Ok(())
        }
    }

    impl MemoryChannel {
        pub(super) fn installed() -> &'static MemoryChannel {
            static CHANNEL: OnceLock<MemoryChannel> = OnceLock::new();
            let channel = CHANNEL.get_or_init(MemoryChannel::default);
            // Only the first installation is kept, and it is this channel.
            let _ = install_shared_open_file_channel(Box::new(InstalledMemoryChannel(channel)));
            channel
        }

        fn share(&self, detfd: &DetFd) {
            let model = detfd.share_for_test();
            let previous = self.store.lock().unwrap().insert(
                model.0.id,
                Canonical {
                    model: Some(model),
                    sequence: 0,
                    takes: 0,
                },
            );
            assert!(previous.is_none());
        }

        fn takes(&self, id: OpenFileId) -> usize {
            self.store.lock().unwrap()[&id].takes
        }

        /// Runs `f` on the canonical model, which must not be leased.
        fn canonical<R>(&self, id: OpenFileId, f: impl FnOnce(&OpenFileDescription) -> R) -> R {
            let store = self.store.lock().unwrap();
            f(&store[&id]
                .model
                .as_ref()
                .expect("a lease outlived its method call")
                .0)
        }
    }

    #[test]
    fn a_shared_handle_reaches_the_canonical_model_through_every_alias() {
        let channel = MemoryChannel::installed();
        let id = OpenFileId::new(DetTid::from_raw(9001), 0);
        let original = DetFd::new(3, OFlag::O_RDWR, FdType::Regular, id);
        let alias = original.clone().with_fd(4);
        // A clone that escaped the descriptor table before the promotion.
        let escaped = original.clone();
        channel.share(&original);

        alias.set_status_flags(OFlag::O_NONBLOCK.bits());
        assert_eq!(channel.takes(id), 1);
        assert!(channel.canonical(id, |model| oflags_nonblocking(model.status_flags)));
        assert!(original.is_nonblocking());
        assert!(escaped.is_nonblocking());
        assert_eq!(
            escaped.status_flags() & OFlag::O_ACCMODE.bits(),
            OFlag::O_RDWR.bits()
        );
        assert_eq!(channel.takes(id), 4);
        assert_eq!(original.open_file_alias_count(), 3);
    }

    /// The fork-time escape marking never leases a promoted description's
    /// shared model: a fork precedes the callback in which DBT may lease it
    /// (a lease outside it panics there), and the flock model takes a promoted
    /// description as escaped anyway (`DetFd::is_shared`). A local description
    /// is marked unless the lock ledger follows it.
    #[test]
    fn the_fork_time_escape_marking_leaves_a_promoted_description_alone() {
        let channel = MemoryChannel::installed();
        let id = OpenFileId::new(DetTid::from_raw(9051), 0);
        let promoted = DetFd::new(3, OFlag::O_RDONLY, FdType::Rng, id);
        promoted.promote().unwrap();
        let takes = channel.takes(id);
        promoted.mark_local_untracked_references_escaped();
        assert_eq!(channel.takes(id), takes);
        assert!(promoted.is_shared());

        let local = DetFd::new(
            4,
            OFlag::O_RDWR,
            FdType::Regular,
            OpenFileId::new(DetTid::from_raw(9051), 1),
        );
        local.mark_local_untracked_references_escaped();
        assert!(local.references_escaped());

        let followed = DetFd::new(
            5,
            OFlag::O_RDWR,
            FdType::Regular,
            OpenFileId::new(DetTid::from_raw(9051), 2),
        );
        followed.set_lock_tracked();
        followed.mark_local_untracked_references_escaped();
        assert!(!followed.references_escaped());
    }

    /// Detcore's flock model keys its state by a description's identity, so
    /// a rebind (`DetFd::rebind_open_file_id`), which gives a description
    /// Detcore just made the identity of an open file its registry names, is
    /// only for a description the model never decided a call through: one
    /// made as discovery or a receipt makes it, escaped and never followed,
    /// before and after the rebind; never one Detcore saw opened, one the
    /// ledger follows, or one whose inode a flock took.
    #[test]
    fn only_a_description_the_flock_model_never_decided_is_rebound() {
        let made = DetFd::new(
            3,
            OFlag::O_RDONLY,
            FdType::Regular,
            OpenFileId::new(DetTid::from_raw(9061), 0),
        );
        made.mark_flock_mode_unobserved();
        assert!(made.may_rebind_open_file_id());
        let joined = OpenFileId::new(DetTid::from_raw(9060), 4);
        made.rebind_open_file_id(joined);
        assert_eq!(made.open_file_id(), joined);
        assert!(made.may_rebind_open_file_id());
        assert!(made.references_escaped());
        assert!(!made.is_lock_tracked());
        assert!(made.flock_identity().is_none());

        let opened = DetFd::new(
            4,
            OFlag::O_RDWR,
            FdType::Regular,
            OpenFileId::new(DetTid::from_raw(9061), 1),
        );
        assert!(!opened.may_rebind_open_file_id());

        let followed = DetFd::new(
            5,
            OFlag::O_RDWR,
            FdType::Regular,
            OpenFileId::new(DetTid::from_raw(9061), 2),
        );
        followed.mark_flock_mode_unobserved();
        followed.set_lock_tracked();
        assert!(!followed.may_rebind_open_file_id());

        let identified = DetFd::new(
            6,
            OFlag::O_RDWR,
            FdType::Regular,
            OpenFileId::new(DetTid::from_raw(9061), 3),
        );
        identified.mark_flock_mode_unobserved();
        identified.set_flock_identity(FlockIdentity {
            dev: 42,
            ino: 1001,
            own_flock: None,
            modelled: false,
        });
        assert!(!identified.may_rebind_open_file_id());
    }

    /// A promoted description has one canonical model, so a grant through a
    /// handle another process forked from it is a grant every handle sees.
    /// `has_unsafe_vfork_flock_state` therefore keeps refusing on a lock a
    /// sibling took after the fork, without any per-process copy of the fact.
    #[test]
    fn promoted_description_shares_its_grant_fact_with_forked_handles() {
        let _channel = MemoryChannel::installed();
        let id = OpenFileId::new(DetTid::from_raw(9041), 0);
        let parent = DetFd::new(3, OFlag::O_RDONLY, FdType::Rng, id);
        parent.promote().unwrap();
        let sibling = parent.fork_shared_handle();
        parent.forget_local_flock_mode_at_fork();
        sibling.forget_local_flock_mode_at_fork();
        assert_eq!(parent.known_flock_mode(), Some(None));
        assert!(!parent.flock_mode_may_be_stale());
        assert!(!parent.flock_may_have_been_granted());

        sibling.set_flock_mode(Some(libc::LOCK_SH));
        assert_eq!(parent.known_flock_mode(), Some(Some(libc::LOCK_SH)));
        assert!(parent.flock_may_have_been_granted());
        sibling.set_flock_mode(None);
        assert_eq!(parent.known_flock_mode(), Some(None));
        assert!(parent.flock_may_have_been_granted());
    }

    #[test]
    fn promoted_fork_handles_keep_one_cursor_without_pregrant_access() {
        let channel = MemoryChannel::installed();
        let id = OpenFileId::new(DetTid::from_raw(9010), 0);
        let parent = DetFd::new(3, OFlag::O_RDONLY, FdType::Rng, id);
        let alias = parent.clone().with_fd(4).with_fd_flags(OFlag::O_CLOEXEC);
        parent
            .with_random_device_stream::<Errno>(|offset| {
                assert_eq!(offset, 0);
                Ok(7)
            })
            .unwrap();
        parent.set_flock_mode(Some(libc::LOCK_SH));
        parent.promote().unwrap();
        alias.promote().unwrap(); // The escaped alias does not publish twice.
        let takes = channel.takes(id);
        let lock = parent.directory_lock();
        let held = lock.try_lock().unwrap();
        let child = alias.fork_shared_handle();
        child.forget_local_flock_mode_at_fork();
        assert_eq!(
            channel.takes(id),
            takes,
            "child metadata construction must be RPC-free"
        );
        assert!(child.directory_lock().try_lock().is_some());
        assert!(child.is_cloexec());
        assert_eq!(child.known_flock_mode(), Some(Some(libc::LOCK_SH)));
        child
            .with_random_device_stream::<Errno>(|offset| {
                assert_eq!(offset, 7);
                Ok(5)
            })
            .unwrap();
        parent
            .with_random_device_stream::<Errno>(|offset| {
                assert_eq!(offset, 12);
                Ok(3)
            })
            .unwrap();
        assert_eq!(
            child.with_random_device_stream::<Errno>(|offset| {
                assert_eq!(offset, 15);
                Err(Errno::EFAULT)
            }),
            Err(Errno::EFAULT)
        );
        child.set_flock_mode(None);
        assert_eq!(parent.known_flock_mode(), Some(None));
        drop(child);
        let successor = DetFd::shared_survivor(3, id);
        successor
            .with_random_device_stream::<Errno>(|offset| {
                assert_eq!(offset, 15);
                Ok(0)
            })
            .unwrap();
        let reopened = DetFd::new(
            3,
            OFlag::O_RDONLY,
            FdType::Rng,
            OpenFileId::new(DetTid::from_raw(9010), 1),
        );
        reopened.promote().unwrap();
        reopened
            .with_random_device_stream::<Errno>(|offset| {
                assert_eq!(offset, 0);
                Ok(1)
            })
            .unwrap();
        successor
            .with_random_device_stream::<Errno>(|offset| {
                assert_eq!(offset, 15);
                Ok(0)
            })
            .unwrap();
        drop(held);
    }

    #[test]
    fn identity_and_directory_lock_never_take_the_shared_model() {
        let channel = MemoryChannel::installed();
        let id = OpenFileId::new(DetTid::from_raw(9002), 0);
        let original = DetFd::new(3, OFlag::O_RDONLY, FdType::Regular, id);
        let local_lock = original.directory_lock();
        channel.share(&original);

        assert_eq!(original.open_file_id(), id);
        assert!(Arc::ptr_eq(&original.directory_lock(), &local_lock));
        assert!(Arc::ptr_eq(
            &original.clone().with_fd(4).directory_lock(),
            &local_lock
        ));
        assert_eq!(channel.takes(id), 0);
    }

    #[test]
    fn a_shared_handle_serializes_only_its_identity() {
        let channel = MemoryChannel::installed();
        let id = OpenFileId::new(DetTid::from_raw(9003), 0);
        let original = DetFd::new(3, OFlag::O_RDONLY, FdType::Regular, id).with_path("/a");
        channel.share(&original);

        let encoded = serde_json::to_string(&original).unwrap();
        assert!(encoded.contains("Shared"), "{encoded}");
        assert!(!encoded.contains("status_flags"), "{encoded}");
        assert!(!encoded.contains("/a"), "{encoded}");
        assert_eq!(channel.takes(id), 0);

        // A restored table interns its shared handles by identity, as it
        // does local ones, without taking the model.
        let mut handles = std::collections::HashMap::new();
        for fd in [3, 4] {
            let restored: DetFd = serde_json::from_str(&encoded).unwrap();
            handles.insert(fd, restored.with_fd(fd));
        }
        intern_open_files(&mut handles);
        assert_eq!(handles[&4].open_file_alias_count(), 2);
        assert_eq!(channel.takes(id), 0);
        assert_eq!(handles[&4].path(), Some(PathBuf::from("/a")));
        assert_eq!(channel.takes(id), 1);
    }

    #[test]
    fn a_failed_random_device_copy_still_ends_the_lease() {
        let channel = MemoryChannel::installed();
        let id = OpenFileId::new(DetTid::from_raw(9004), 0);
        let original = DetFd::new(3, OFlag::empty(), FdType::Rng, id);
        channel.share(&original);

        assert_eq!(
            original.with_random_device_stream(|_| Err::<usize, _>(())),
            Err(())
        );
        assert_eq!(channel.canonical(id, |model| model.random_device_offset), 0);
        assert_eq!(
            original.with_random_device_stream(|offset| {
                assert_eq!(offset, 0);
                Ok::<_, ()>(5)
            }),
            Ok(5)
        );
        assert_eq!(channel.canonical(id, |model| model.random_device_offset), 5);
        assert_eq!(channel.takes(id), 2);
    }

    fn at(ns: u64) -> LogicalTime {
        LogicalTime::from_nanos(ns)
    }

    /// A one-shot timerfd counts one expiration once virtual time reaches its
    /// deadline, then disarms; before the deadline it counts none.
    #[test]
    fn a_one_shot_timerfd_expires_once_at_its_virtual_deadline() {
        let mut timer = VirtualTimerfd::set(Some(at(1_000)), 0);
        assert_eq!(timer.expire_through(at(999)), 0);
        assert_eq!(timer.remaining(at(999)), (1, 0));
        assert_eq!(timer.expire_through(at(1_000)), 1);
        assert_eq!(
            timer,
            VirtualTimerfd {
                deadline: None,
                interval_ns: 0,
                unread: 1
            }
        );
        assert_eq!(timer.expire_through(at(5_000)), 0);
        assert_eq!(timer.remaining(at(5_000)), (0, 0));
    }

    /// A periodic timerfd observed late counts every expiration it missed,
    /// exactly, and its next deadline stays on the original grid: the count
    /// is a function of virtual time, not of how often it was observed.
    #[test]
    fn a_periodic_timerfd_counts_every_missed_expiration_on_its_grid() {
        let mut once = VirtualTimerfd::set(Some(at(1_000)), 300);
        assert_eq!(once.expire_through(at(1_950)), 4);
        assert_eq!(once.deadline, Some(at(2_200)));
        assert_eq!(once.remaining(at(1_950)), (250, 300));

        let mut often = VirtualTimerfd::set(Some(at(1_000)), 300);
        let mut added = 0;
        for now in [1_000, 1_100, 1_300, 1_599, 1_950] {
            added += often.expire_through(at(now));
        }
        assert_eq!(added, 4);
        assert_eq!(often, once);
    }

    /// What a timerfd's fdinfo shows follows Linux's own timer
    /// (`LinuxTimerfd`), which counts one expiration when it fires and the
    /// periods after it only when `timerfd_gettime` or a read forwards it,
    /// whatever Detcore's probes counted meanwhile; a one-shot timer that
    /// fired is not restarted.
    #[test]
    fn a_timerfds_fdinfo_view_follows_linuxs_forwarding() {
        let mut model = VirtualTimerfd::set(Some(at(1_000)), 300);
        let mut linux = LinuxTimerfd::set(Some(at(1_000)));
        assert_eq!(linux.view(at(400)), (0, 600));
        model.expire_through(at(1_950));
        assert_eq!(linux.view(at(1_950)), (1, 0));
        linux.forward(at(1_950), 300);
        assert_eq!(linux.view(at(1_950)), (4, 250));
        assert_eq!(linux.view(at(2_200)), (5, 0));
        assert_eq!(model.expire_through(at(2_300)), 1);
        linux.after_read(model.deadline);
        assert_eq!(linux.view(at(2_300)), (0, 200));

        let mut once_model = VirtualTimerfd::set(Some(at(10)), 0);
        let mut once = LinuxTimerfd::set(Some(at(10)));
        assert_eq!(once.view(at(5)), (0, 5));
        assert_eq!(once.view(at(10)), (1, 0));
        once.forward(at(20), 0);
        assert_eq!(once.view(at(20)), (1, 0));
        once_model.expire_through(at(20));
        once.after_read(once_model.deadline);
        assert_eq!(once.view(at(30)), (0, 0));

        assert_eq!(LinuxTimerfd::set(None).view(at(5)), (0, 0));
    }

    /// A timer disarmed with a zero value keeps its interval for
    /// `timerfd_gettime`, as Linux does, and never expires.
    #[test]
    fn a_disarmed_timerfd_keeps_its_interval_and_never_expires() {
        let mut timer = VirtualTimerfd::set(None, 300);
        assert_eq!(timer.expire_through(at(u64::MAX / 2)), 0);
        assert_eq!(timer.remaining(at(5)), (0, 300));
        assert_eq!(timer.unread, 0);
    }

    /// Setting a timerfd discards its unread expirations, as Linux does.
    #[test]
    fn setting_a_timerfd_discards_unread_expirations() {
        let mut timer = VirtualTimerfd::set(Some(at(10)), 0);
        timer.expire_through(at(20));
        assert_eq!(timer.unread, 1);
        assert_eq!(VirtualTimerfd::set(None, 0).unread, 0);
    }

    /// The guest-held mark (`DetFd::is_guest_held`) lives on the open file, so
    /// a received descriptor carries it only by joining the open file a
    /// guest's send queued on the receiving socket (`take_queued_rights`,
    /// `OpenFileHandle::descriptor`), and then shares the mark and its
    /// withdrawal with the sender's copy. A receipt nothing queued names, such
    /// as one from a process outside the guest, is a description of its own
    /// and starts unmarked.
    #[test]
    fn a_received_descriptor_is_guest_held_only_through_its_senders_open_file() {
        let owner = DetTid::from_raw(10);
        let open =
            |fd, ty, sequence| DetFd::new(fd, OFlag::O_RDWR, ty, OpenFileId::new(owner, sequence));
        let (left, right) = (open(3, FdType::Socket, 0), open(4, FdType::Socket, 1));
        left.pair_sockets(&right, false);
        let pipe_end = open(5, FdType::Pipe, 2);
        pipe_end.set_guest_held();
        assert!(
            left.queue_rights_to_peer(
                vec![SentDescriptor {
                    file: (1, 50),
                    open_file: Some(pipe_end.in_flight_open_file(true)),
                    lock_copy: None,
                    suspect: false,
                }],
                0,
            )
            .is_ok()
        );
        let sent = right
            .take_queued_rights(&[(1, 50)], false, false, false, None)
            .expect("the send queued it")
            .sent;
        let joined = sent[0]
            .open_file
            .as_ref()
            .and_then(|handle| handle.descriptor(6, false))
            .expect("the sender's open file is still modeled");
        assert!(joined.is_guest_held(), "a join shares the sender's mark");
        joined.clear_guest_held();
        assert!(!pipe_end.is_guest_held(), "and its withdrawal");

        assert!(
            right
                .take_queued_rights(&[(1, 60)], false, false, false, None)
                .is_none(),
            "nothing queued names a descriptor a guest did not send"
        );
        assert!(
            !open(7, FdType::Pipe, 3).is_guest_held(),
            "a description of its own starts unmarked"
        );
    }

    /// The provenance a connection needs to be guest-held
    /// (`DetFd::is_guest_created`) lives on the open file too: a duplicate or
    /// a join shares it and its withdrawal, a handle reads it while the
    /// description is modeled, and a description of its own, such as a
    /// received socket that joined nothing, starts without it.
    #[test]
    fn a_socket_is_guest_created_only_through_its_own_open_file() {
        let owner = DetTid::from_raw(10);
        let made = DetFd::new(3, OFlag::O_RDWR, FdType::Socket, OpenFileId::new(owner, 0));
        assert!(!made.is_guest_created(), "nothing starts guest-created");
        made.set_guest_created();
        let handle = made.open_file_handle();
        let joined = handle.descriptor(4, false).expect("still modeled");
        assert!(joined.is_guest_created(), "a join shares it");
        assert!(handle.is_guest_created(), "and a handle reads it");
        joined.clear_guest_created();
        assert!(!made.is_guest_created(), "and its withdrawal");
        assert!(!handle.is_guest_created());
        let received = DetFd::new(5, OFlag::O_RDWR, FdType::Socket, OpenFileId::new(owner, 1));
        assert!(
            !received.is_guest_created(),
            "a description of its own starts without it"
        );
        drop((made, joined));
        assert!(
            !OpenFileHandle::dangling().is_guest_created(),
            "a description no longer modeled is not guest-created"
        );
    }

    /// The descriptors a socket's messages carried are taken in order
    /// (`DetFd::take_queued_rights`): a peek leaves its message queued, a
    /// receive of fewer descriptors than a message carried names it only where
    /// Linux set `MSG_CTRUNC`, a receive that names a message Linux cannot
    /// have delivered yet takes nothing, leaving every message queued, and one
    /// that names no queued message puts the queue out of lockstep.
    #[test]
    fn queued_rights_are_taken_in_order() {
        let owner = DetTid::from_raw(10);
        let socket = |fd, sequence| {
            DetFd::new(
                fd,
                OFlag::O_RDWR,
                FdType::Socket,
                OpenFileId::new(owner, sequence),
            )
        };
        let (left, right) = (socket(3, 0), socket(4, 1));
        left.pair_sockets(&right, false);
        let file = |inode: u64| (1, inode);
        let sent = |inodes: &[u64]| -> Vec<SentDescriptor> {
            inodes
                .iter()
                .map(|&inode| SentDescriptor {
                    file: file(inode),
                    open_file: None,
                    lock_copy: None,
                    suspect: false,
                })
                .collect()
        };
        let inodes = |taken: Option<TakenRights>| {
            taken.map(|taken| {
                taken
                    .sent
                    .iter()
                    .map(|sent| sent.file.1)
                    .collect::<Vec<_>>()
            })
        };

        for message in [&[10][..], &[20, 21], &[30], &[40]] {
            assert!(matches!(
                left.queue_rights_to_peer(sent(message), 1),
                Ok(())
            ));
        }
        assert_eq!(
            inodes(right.take_queued_rights(&[file(10)], false, true, false, None)),
            Some(vec![10]),
            "a peek names the first message"
        );
        assert_eq!(
            inodes(right.take_queued_rights(&[file(10)], false, false, false, None)),
            Some(vec![10]),
            "and leaves it queued"
        );
        assert_eq!(
            right
                .take_queued_rights(&[file(20)], true, false, false, None)
                .map(|taken| (taken.sent.len(), taken.lockstep)),
            Some((2, true)),
            "a truncated receive takes the whole message, in lockstep"
        );
        assert!(
            right
                .take_queued_rights(&[file(10)], false, true, false, None)
                .is_none(),
            "a message taken is no longer queued"
        );
        assert!(
            right
                .take_queued_rights(&[file(40)], false, false, false, None)
                .is_none(),
            "a receipt that names a message Linux cannot have delivered yet (no call \
             took the one before it) took one never queued here: it takes nothing"
        );
        let next = right.take_queued_rights(&[file(30)], false, false, false, None);
        assert_eq!(
            next.as_ref()
                .map(|taken| (taken.lockstep, taken.joinable, taken.forgotten.len())),
            Some((false, true, 0)),
            "Linux still delivers the messages before it, in order, and they are joined"
        );
        assert_eq!(inodes(next), Some(vec![30]));
        assert_eq!(
            inodes(right.take_queued_rights(&[file(40)], false, false, false, None)),
            Some(vec![40])
        );

        // One descriptor of a message that carried two, without MSG_CTRUNC,
        // names no queued message: the message it came with was never queued
        // here, and the queue is out of lockstep. A peek changes nothing.
        let (other_left, other_right) = (socket(5, 3), socket(6, 4));
        other_left.pair_sockets(&other_right, false);
        assert!(matches!(
            other_left.queue_rights_to_peer(sent(&[90, 91]), 1),
            Ok(())
        ));
        assert!(
            other_right
                .take_queued_rights(&[file(90)], false, true, false, None)
                .is_none()
        );
        assert_eq!(
            other_right
                .take_queued_rights(&[file(90), file(91)], false, true, false, None)
                .map(|taken| taken.lockstep),
            Some(true)
        );
        assert!(
            other_right
                .take_queued_rights(&[file(90)], false, false, false, None)
                .is_none()
        );
        assert_eq!(
            other_right
                .take_queued_rights(&[file(90), file(91)], false, false, false, None)
                .map(|taken| (taken.lockstep, taken.joinable)),
            Some((false, true))
        );

        // Under record and replay, such a receipt takes its match and forgets
        // the messages before it, as recordings were made with.
        for message in [&[70][..], &[80]] {
            assert!(matches!(
                left.queue_rights_to_peer(sent(message), 1),
                Ok(())
            ));
        }
        let legacy = right.take_queued_rights(&[file(80)], false, false, true, None);
        assert_eq!(
            legacy
                .as_ref()
                .map(|taken| (taken.joinable, taken.forgotten.len())),
            Some((false, 1))
        );

        // A message sent through `right` reaches `left`; a socket with no
        // known peer queues nothing.
        assert!(matches!(right.queue_rights_to_peer(sent(&[50]), 1), Ok(())));
        assert_eq!(
            inodes(right.take_queued_rights(&[file(50)], false, false, false, None)),
            None
        );
        assert_eq!(
            inodes(left.take_queued_rights(&[file(50)], false, false, false, None)),
            Some(vec![50])
        );
        assert!(matches!(
            socket(5, 2).queue_rights_to_peer(sent(&[60]), 1),
            Err(back) if back.len() == 1
        ));
    }

    /// After calls Detcore could not match took messages, a receipt joins
    /// none that Linux may have released already: the first message past
    /// them, or one among them that no later one among them shadows
    /// (`SocketRights::suspects`).
    #[test]
    fn a_message_linux_may_have_released_is_never_joined() {
        let owner = DetTid::from_raw(10);
        let socket = |fd, sequence| {
            DetFd::new(
                fd,
                OFlag::O_RDWR,
                FdType::Socket,
                OpenFileId::new(owner, sequence),
            )
        };
        let sent = |inode: u64| {
            vec![SentDescriptor {
                file: (1, inode),
                open_file: None,
                lock_copy: None,
                suspect: false,
            }]
        };
        let take = |socket: &DetFd, inode: u64, peek: bool| {
            socket
                .take_queued_rights(&[(1, inode)], false, peek, false, None)
                .map(|taken| (taken.joinable, taken.forgotten.len()))
        };

        // Two messages naming one file, the first taken unseen (an EFAULT
        // after the dequeue): the receipt of the second matches the first,
        // which may be the stale one, so neither is joined, and the second,
        // left in the queue, stays suspect.
        let (left, right) = (socket(3, 0), socket(4, 1));
        left.pair_sockets(&right, false);
        for inode in [10, 10] {
            assert!(matches!(left.queue_rights_to_peer(sent(inode), 1), Ok(())));
        }
        right.lose_lockstep_if_queued(1, false);
        assert_eq!(take(&right, 10, true), Some((false, 0)), "a peek alike");
        assert_eq!(take(&right, 10, false), Some((false, 0)));
        assert!(matches!(left.queue_rights_to_peer(sent(10), 1), Ok(())));
        assert_eq!(
            take(&right, 10, false),
            Some((false, 0)),
            "the received message's own entry is never joined"
        );

        // A data-only message taken unseen: the next message names other
        // files, so its receipt is trusted, and the queue past it is not
        // suspect any more.
        let (left, right) = (socket(3, 2), socket(4, 3));
        left.pair_sockets(&right, false);
        for inode in [20, 21] {
            assert!(matches!(left.queue_rights_to_peer(sent(inode), 1), Ok(())));
        }
        right.lose_lockstep_if_queued(1, false);
        assert_eq!(take(&right, 20, false), Some((true, 0)));
        assert_eq!(take(&right, 21, false), Some((true, 0)));

        // Two calls took messages unseen; the third message is the first
        // Linux can still hold, and its receipt forgets the two before it.
        let (left, right) = (socket(3, 4), socket(4, 5));
        left.pair_sockets(&right, false);
        for inode in [30, 31, 32, 33] {
            assert!(matches!(left.queue_rights_to_peer(sent(inode), 1), Ok(())));
        }
        right.lose_lockstep_if_queued(1, false);
        right.lose_lockstep_if_queued(1, false);
        assert_eq!(take(&right, 32, false), Some((true, 2)));
        assert_eq!(
            take(&right, 33, false),
            Some((true, 0)),
            "after a trusted receipt nothing is suspect"
        );

        // With one call unmatched, a receipt that names the third message
        // names one Linux cannot have delivered yet: it takes nothing, and
        // the messages up to it stay queued, suspect, as Linux still holds
        // them; the next receipt of the first is trusted.
        let (left, right) = (socket(3, 6), socket(4, 7));
        left.pair_sockets(&right, false);
        for inode in [40, 41, 42] {
            assert!(matches!(left.queue_rights_to_peer(sent(inode), 1), Ok(())));
        }
        right.lose_lockstep_if_queued(1, false);
        assert_eq!(take(&right, 42, false), None);
        assert_eq!(take(&right, 40, false), Some((true, 0)));
        assert_eq!(take(&right, 41, false), Some((true, 0)));
        assert_eq!(take(&right, 42, false), Some((true, 0)));
    }

    /// A socketpair of `owner`'s table, and descriptor 5 of a description
    /// of type `ty` to send through it.
    fn pair_and_sent(owner: DetTid, ty: FdType) -> (DetFd, DetFd, DetFd) {
        let detfd =
            |fd, ty, sequence| DetFd::new(fd, OFlag::O_RDWR, ty, OpenFileId::new(owner, sequence));
        let (left, right) = (detfd(3, FdType::Socket, 0), detfd(4, FdType::Socket, 1));
        left.pair_sockets(&right, false);
        (left, right, detfd(5, ty, 2))
    }

    /// Queue `sent` through `left` (`held` or with the legacy ownership),
    /// drop the sender's descriptor, and return a handle that tells whether
    /// the description is still alive.
    fn queue_and_close(left: &DetFd, sent: DetFd, held: bool) -> OpenFileHandle {
        let handle = sent.open_file_handle();
        assert!(matches!(
            left.queue_rights_to_peer(
                vec![SentDescriptor {
                    file: (1, 50),
                    open_file: Some(sent.in_flight_open_file(held)),
                    lock_copy: None,
                    suspect: false,
                }],
                1
            ),
            Ok(())
        ));
        drop(sent);
        handle
    }

    /// A queued message keeps the descriptions it carries alive, as Linux's
    /// in-flight reference keeps each `struct file`: the receipt names the
    /// sender's description after the sender's last descriptor of it went,
    /// by a close or with the table of a sender that exited, whenever the
    /// backend releases that table
    /// (https://github.com/rrnewton/hermit/issues/4930). The description
    /// counts the copy in flight until the receipt lands it in the
    /// receiving table (`OpenFileDescription::in_flight`).
    #[test]
    fn a_queued_message_keeps_its_descriptions_alive() {
        let (left, right, sent) = pair_and_sent(DetTid::from_raw(10), FdType::Eventfd);
        let id = sent.open_file_id();
        let alias = sent.clone();
        let handle = queue_and_close(&left, sent, true);
        assert_eq!(
            alias.open_file_alias_count(),
            2,
            "the queue's reference holds it"
        );
        assert_eq!(alias.copies_in_flight(), 1, "and counts in flight");
        assert!(!alias.is_last_reference(), "so no close releases it");
        drop(alias);
        assert!(
            handle.descriptor(5, false).is_some(),
            "the queued message holds the description"
        );
        let taken = right
            .take_queued_rights(&[(1, 50)], false, false, false, None)
            .unwrap();
        assert!(taken.lockstep && taken.forgotten.is_empty());
        let copy = taken.sent[0].open_file.as_ref().unwrap();
        let received = copy.descriptor(7, true).unwrap();
        copy.land();
        assert_eq!(received.copies_in_flight(), 0, "landed");
        assert_eq!(
            received.open_file_id(),
            id,
            "the receipt is the sender's description"
        );
        drop(taken);
        assert!(
            handle.descriptor(5, false).is_some(),
            "the received descriptor holds it now"
        );
        drop(received);
        assert!(
            handle.descriptor(5, false).is_none(),
            "and nothing else does"
        );
    }

    /// Under record and replay a queued message keeps the ownership existing
    /// recordings were made with: it holds nothing, so a description only it
    /// names is gone for every decision, a receipt's join and a pending
    /// connection's pairing alike.
    #[test]
    fn a_legacy_queued_message_keeps_nothing_alive() {
        let (left, right, sent) = pair_and_sent(DetTid::from_raw(10), FdType::Socket);
        let handle = queue_and_close(&left, sent, false);
        assert!(
            handle.descriptor(5, false).is_none(),
            "the description is gone"
        );
        let taken = right
            .take_queued_rights(&[(1, 50)], false, false, false, None)
            .unwrap();
        assert!(
            taken.sent[0]
                .open_file
                .as_ref()
                .unwrap()
                .descriptor(7, false)
                .is_none(),
            "and the receipt cannot name it"
        );
    }

    /// A receive Detcore could not match, a peek among them, keeps every
    /// queued message, and with it the descriptions it holds: Linux still
    /// delivers them. The queue lets go where Linux releases a message: a
    /// later message taken after calls Detcore could not match forgets the
    /// ones before it, a datagram `connect` (or a send that finds the peer
    /// gone) purges the queue, and the receiving socket's last reference
    /// takes the queue with it. Each release by a guest call uncounts the
    /// copy in flight ([`InFlightOpenFile::release`]).
    #[test]
    fn a_queue_lets_go_only_where_linux_releases_the_messages() {
        let owner = DetTid::from_raw(10);
        let release = |sent: Vec<SentDescriptor>| {
            sent.into_iter()
                .filter_map(|sent| sent.open_file.and_then(InFlightOpenFile::release))
                .count()
        };

        // A receive Detcore could not match keeps the queue, the message it
        // may have taken suspect, in lockstep: no message reached the socket
        // unqueued. Under record and replay it puts the queue out of
        // lockstep, as recordings were made with.
        for legacy in [false, true] {
            let (left, right, sent) = pair_and_sent(owner, FdType::Pipe);
            let handle = queue_and_close(&left, sent, true);
            right.lose_lockstep_if_queued(1, legacy);
            assert!(handle.descriptor(5, false).is_some(), "kept with the queue");
            assert!(matches!(
                right.take_queued_rights(&[(1, 50)], false, false, legacy, None),
                Some(taken) if taken.sent.len() == 1
                    && taken.lockstep == !legacy
                    && taken.joinable
            ));
        }

        // A later message taken after a call Detcore could not match forgets
        // the earlier one, which that call took.
        let (left, right, sent) = pair_and_sent(owner, FdType::Pipe);
        let handle = queue_and_close(&left, sent, true);
        let later = |inode| SentDescriptor {
            file: (1, inode),
            open_file: None,
            lock_copy: None,
            suspect: false,
        };
        assert!(matches!(
            left.queue_rights_to_peer(vec![later(60)], 1),
            Ok(())
        ));
        right.lose_lockstep_if_queued(1, false);
        let taken = right
            .take_queued_rights(&[(1, 60)], false, false, false, None)
            .unwrap();
        assert_eq!(release(taken.forgotten), 1, "the last reference goes");
        assert!(handle.descriptor(5, false).is_none(), "released with it");

        // A datagram `connect` of the receiving socket purges its queue.
        let (left, right, sent) = pair_and_sent(owner, FdType::Pipe);
        let handle = queue_and_close(&left, sent, true);
        let purged = right.unpair_rights_peer();
        assert_eq!(purged.len(), 1);
        assert!(handle.descriptor(5, false).is_some(), "until released");
        assert_eq!(release(purged), 1);
        assert!(
            handle.descriptor(5, false).is_none(),
            "purged with the queue"
        );

        // The receiving socket's last reference takes its queue.
        let (left, right, sent) = pair_and_sent(owner, FdType::Pipe);
        let handle = queue_and_close(&left, sent, true);
        let taken = right.take_all_queued_rights();
        assert!(
            taken.iter().all(|sent| !sent.suspect),
            "no receive took a message unseen"
        );
        assert_eq!(release(taken), 1);
        assert!(
            handle.descriptor(5, false).is_none(),
            "gone with the socket"
        );

        // A message a receive Detcore could not match may have taken is
        // suspect when the socket's last reference takes the queue: the
        // receive may hold its descriptions (`flock_model`). A later one is
        // not, unless the queue lost lockstep, which makes every one suspect.
        for legacy in [false, true] {
            let (left, right, sent) = pair_and_sent(owner, FdType::Pipe);
            queue_and_close(&left, sent, true);
            assert!(matches!(
                left.queue_rights_to_peer(vec![later(60)], 1),
                Ok(())
            ));
            right.lose_lockstep_if_queued(1, legacy);
            let suspects = right
                .take_all_queued_rights()
                .iter()
                .map(|sent| sent.suspect)
                .collect::<Vec<_>>();
            assert_eq!(suspects, [true, legacy], "legacy={legacy}");
        }

        // A release keeps a description another descriptor still names.
        let (left, right, sent) = pair_and_sent(owner, FdType::Pipe);
        let alias = sent.clone();
        queue_and_close(&left, sent, true);
        assert_eq!(release(right.take_all_queued_rights()), 0);
        assert!(alias.is_last_reference(), "now the last reference");
        drop(left);
    }

    /// Whether a descriptor is its description's last reference, which
    /// decides a socket's port and queue at its close, depends only on what
    /// guest calls did, not on when the backend releases a table that holds
    /// a queue (https://github.com/rrnewton/hermit/issues/4930): a copy in
    /// flight in the queue of a socket whose only holder exited counts
    /// whether or not the backend has dropped that holder's table yet, and
    /// for the rest of the run, as does one dropped past the queue's bound.
    #[test]
    fn a_copy_in_flight_decides_the_last_reference_whatever_drops_its_queue() {
        let owner = DetTid::from_raw(10);
        for teardown_first in [false, true] {
            let (left, right, sent) = pair_and_sent(owner, FdType::Socket);
            let alias = sent.clone();
            queue_and_close(&left, sent, true);
            if teardown_first {
                // The receiving socket went with a table the backend released.
                drop(right);
                assert_eq!(alias.open_file_alias_count(), 1, "nothing holds the copy");
            }
            assert!(
                !alias.is_last_reference(),
                "teardown_first={teardown_first}: the copy in flight counts either way"
            );
            assert_eq!(alias.copies_in_flight(), 1);
        }

        // Past the bound, the oldest message is dropped from the queue, and
        // what it carried stays counted.
        let (left, _right, sent) = pair_and_sent(owner, FdType::Socket);
        let alias = sent.clone();
        queue_and_close(&left, sent, true);
        let filler = || SentDescriptor {
            file: (1, 60),
            open_file: None,
            lock_copy: None,
            suspect: false,
        };
        for _ in 0..MAX_QUEUED_RIGHTS_MESSAGES {
            assert!(matches!(
                left.queue_rights_to_peer(vec![filler()], 1),
                Ok(())
            ));
        }
        assert_eq!(alias.open_file_alias_count(), 1, "dropped from the queue");
        assert!(!alias.is_last_reference(), "but never released");
    }

    /// Where a socket's queue holds every message (the two sockets of a
    /// datagram `socketpair`), a call that takes one without naming its
    /// descriptors (a `read`) takes the oldest queued one, data alone or not,
    /// and releases what it carried, as Linux does
    /// ([`DetFd::take_head_message`]); a queue that may miss a message (a
    /// bound socket, or one out of lockstep) takes nothing.
    #[test]
    fn a_complete_queue_gives_up_the_message_a_read_takes() {
        let owner = DetTid::from_raw(10);
        let (left, right, sent) = pair_and_sent(owner, FdType::Pipe);
        assert!(
            right.may_deliver_rights(),
            "before the pair is known complete"
        );
        left.mark_complete_pair(&right);
        assert!(!right.may_deliver_rights(), "an empty complete queue");
        assert!(matches!(left.queue_rights_to_peer(Vec::new(), 3), Ok(())));
        assert!(!right.may_deliver_rights(), "data alone carries none");
        let alias = sent.clone();
        queue_and_close(&left, sent, true);
        assert!(right.may_deliver_rights(), "a message with a descriptor");
        let data_only = right.take_head_message(|head| head.bytes == 3);
        assert!(matches!(data_only, Some(ref sent) if sent.is_empty()));
        let carried = right.take_head_message(|head| head.bytes == 1).unwrap();
        assert_eq!(carried.len(), 1);
        assert!(right.may_deliver_rights(), "once sent one, for good");
        assert!(!alias.is_last_reference());
        assert_eq!(
            carried
                .into_iter()
                .filter_map(|sent| sent.open_file.and_then(InFlightOpenFile::release))
                .count(),
            0
        );
        assert!(alias.is_last_reference(), "released at the read");

        // A pair whose queue loses track of which message a receive took,
        // with no descriptor ever sent, still cannot deliver one.
        let (left, right, _) = pair_and_sent(owner, FdType::Pipe);
        left.mark_complete_pair(&right);
        assert!(matches!(left.queue_rights_to_peer(Vec::new(), 3), Ok(())));
        assert!(
            right
                .take_head_message(|head| !head.descriptors.is_empty())
                .is_none()
        );
        assert!(
            !right.may_deliver_rights(),
            "out of lockstep, never sent one"
        );
        left.lose_peer_lockstep();
        assert!(
            !right.may_deliver_rights(),
            "a send of data alone outside the queue's order carries none"
        );
        left.lose_peer_lockstep_with_rights();
        assert!(
            right.may_deliver_rights(),
            "a send that may not have queued its own"
        );

        // A message that does not fit what the call reports leaves the queue
        // out of lockstep, taking nothing.
        let (left, right, sent) = pair_and_sent(owner, FdType::Pipe);
        left.mark_complete_pair(&right);
        queue_and_close(&left, sent, true);
        assert!(right.take_head_message(|head| head.bytes == 2).is_none());
        assert!(right.take_head_message(|_| true).is_none(), "for good");

        // A bound socket can be sent to by name: its queue may miss one.
        let (left, right, sent) = pair_and_sent(owner, FdType::Pipe);
        left.mark_complete_pair(&right);
        right.set_unix_name(Box::from(&b"\0bound"[..]), 1);
        assert!(right.may_deliver_rights(), "a bound socket may receive any");
        queue_and_close(&left, sent, true);
        assert!(right.take_head_message(|_| true).is_none());
        assert!(matches!(left.queue_rights_to_peer(Vec::new(), 1), Ok(())));
        assert!(
            right
                .take_queued_rights(&[(1, 50)], false, false, false, None)
                .is_some()
                && !right.has_queued_rights(),
            "and data alone is not queued"
        );
    }

    /// A queue that no longer holds every message keeps only the messages
    /// that carried descriptors, and the suspects among them
    /// (`give_up_completeness`): here two calls took messages unseen, the
    /// message of data alone and the first descriptor's, so a receipt of the
    /// second descriptor's message forgets the first. At the queue's bound, a
    /// queue that held every message gives up the messages of data alone
    /// first, before it drops anything Linux still holds, and stays in
    /// lockstep. A stream's runs of data alone are one message.
    #[test]
    fn a_queue_that_gives_up_completeness_keeps_its_descriptors() {
        let owner = DetTid::from_raw(10);
        let (left, right, sent) = pair_and_sent(owner, FdType::Pipe);
        left.mark_complete_pair(&right);
        let carried = |inode| {
            vec![SentDescriptor {
                file: (1, inode),
                open_file: None,
                lock_copy: None,
                suspect: false,
            }]
        };
        assert!(matches!(left.queue_rights_to_peer(Vec::new(), 1), Ok(())));
        queue_and_close(&left, sent, true);
        assert!(matches!(left.queue_rights_to_peer(Vec::new(), 1), Ok(())));
        assert!(matches!(left.queue_rights_to_peer(carried(60), 1), Ok(())));
        right.lose_lockstep_if_queued(1, false);
        right.lose_lockstep_if_queued(1, false);
        left.give_up_pair_completeness();
        let taken = right
            .take_queued_rights(&[(1, 60)], false, false, false, None)
            .unwrap();
        assert!(taken.joinable && taken.lockstep);
        assert_eq!(
            taken
                .forgotten
                .iter()
                .map(|sent| sent.file)
                .collect::<Vec<_>>(),
            [(1, 50)],
            "the suspect message that carried a descriptor is forgotten"
        );

        let (left, right, _) = pair_and_sent(owner, FdType::Pipe);
        left.mark_complete_pair(&right);
        for _ in 1..MAX_QUEUED_RIGHTS_MESSAGES {
            assert!(matches!(left.queue_rights_to_peer(Vec::new(), 1), Ok(())));
        }
        assert!(matches!(left.queue_rights_to_peer(carried(70), 1), Ok(())));
        assert!(matches!(left.queue_rights_to_peer(carried(80), 1), Ok(())));
        assert_eq!(
            right
                .take_queued_rights(&[(1, 70)], false, false, false, None)
                .map(|taken| (taken.lockstep, taken.joinable)),
            Some((true, true)),
            "nothing that carried a descriptor was dropped, and the queue stays in lockstep"
        );
        assert!(
            right
                .take_queued_rights(&[(1, 80)], false, false, false, None)
                .is_some()
        );

        let (left, right, _) = pair_and_sent(owner, FdType::Pipe);
        left.pair_sockets(&right, true);
        left.mark_complete_pair(&right);
        for _ in 0..2 * MAX_QUEUED_RIGHTS_MESSAGES {
            assert!(matches!(left.queue_rights_to_peer(Vec::new(), 1), Ok(())));
        }
        assert_eq!(
            right
                .take_stream_bytes(2 * MAX_QUEUED_RIGHTS_MESSAGES, |detached| detached
                    .is_none())
                .map(|released| released.len()),
            Some(0),
            "a stream's data alone is one run of bytes, held whole"
        );
    }

    /// A receive that took one message of data alone from a datagram or
    /// seqpacket socket (`DetFd::take_data_only_message`) loses nothing from a
    /// queue of descriptors alone; from a queue that holds every message it
    /// takes the oldest, which must be of data alone, or, while some are
    /// suspect, counts one more message that may be gone.
    #[test]
    fn a_receipt_of_data_alone_takes_only_from_a_complete_queue() {
        let owner = DetTid::from_raw(10);
        let (left, right, sent) = pair_and_sent(owner, FdType::Pipe);
        queue_and_close(&left, sent, true);
        right.take_data_only_message();
        assert!(matches!(
            right.take_queued_rights(&[(1, 50)], false, false, false, None),
            Some(taken) if taken.joinable && taken.lockstep
        ));

        let (left, right, sent) = pair_and_sent(owner, FdType::Pipe);
        left.mark_complete_pair(&right);
        assert!(matches!(left.queue_rights_to_peer(Vec::new(), 1), Ok(())));
        queue_and_close(&left, sent, true);
        right.take_data_only_message();
        assert!(
            right
                .take_head_message(|head| !head.descriptors.is_empty())
                .is_some(),
            "the message of data alone went first"
        );

        let (left, right, sent) = pair_and_sent(owner, FdType::Pipe);
        left.mark_complete_pair(&right);
        queue_and_close(&left, sent, true);
        right.take_data_only_message();
        assert!(
            right.take_head_message(|_| true).is_none(),
            "a queue whose oldest message carried a descriptor missed one"
        );

        let (left, right, sent) = pair_and_sent(owner, FdType::Pipe);
        left.mark_complete_pair(&right);
        assert!(matches!(left.queue_rights_to_peer(Vec::new(), 1), Ok(())));
        queue_and_close(&left, sent, true);
        right.lose_lockstep_if_queued(1, false);
        right.take_data_only_message();
        assert!(matches!(
            right.take_queued_rights(&[(1, 50)], false, false, false, None),
            Some(taken) if taken.joinable && taken.forgotten.is_empty()
        ));
    }

    /// A stream socket's queue that holds every message gives up bytes from
    /// its oldest messages on (`DetFd::take_stream_bytes`): a message's
    /// descriptors go with the first of its bytes a read takes, a read of no
    /// bytes that reached the socket detaches the oldest message's descriptors
    /// alone, and a read that takes more bytes than the queue holds, or goes
    /// past a message whose descriptors it detached, misses a message, leaving
    /// the queue out of lockstep without the messages of data alone. A
    /// receipt of descriptors takes the message its bytes reach
    /// (`DetFd::take_queued_rights` with the receive's bytes), and a peek the
    /// first message that carried descriptors.
    #[test]
    fn a_stream_queue_gives_up_bytes_from_its_oldest_messages() {
        let owner = DetTid::from_raw(10);
        let stream_pair = |sequence| {
            let socket = |fd, sequence| {
                DetFd::new(
                    fd,
                    OFlag::O_RDWR,
                    FdType::Socket,
                    OpenFileId::new(owner, sequence),
                )
            };
            let (left, right) = (socket(3, sequence), socket(4, sequence + 1));
            left.pair_sockets(&right, true);
            left.mark_complete_pair(&right);
            (left, right)
        };
        let carried = |inode| {
            vec![SentDescriptor {
                file: (1, inode),
                open_file: None,
                lock_copy: None,
                suspect: false,
            }]
        };
        let inodes = |taken: Option<Vec<SentDescriptor>>| {
            taken.map(|taken| taken.iter().map(|sent| sent.file.1).collect::<Vec<_>>())
        };

        let (left, right) = stream_pair(0);
        assert!(matches!(left.queue_rights_to_peer(Vec::new(), 3), Ok(())));
        assert!(matches!(left.queue_rights_to_peer(carried(50), 2), Ok(())));
        assert!(matches!(left.queue_rights_to_peer(Vec::new(), 4), Ok(())));
        assert_eq!(inodes(right.take_stream_bytes(2, |_| true)), Some(vec![]));
        assert_eq!(
            inodes(right.take_stream_bytes(2, |_| true)),
            Some(vec![50]),
            "the descriptors go with the first of their message's bytes"
        );
        assert_eq!(inodes(right.take_stream_bytes(5, |_| true)), Some(vec![]));
        assert!(!right.has_queued_rights(), "every byte read");
        assert!(
            right.take_stream_bytes(1, |_| true).is_none(),
            "more than it held"
        );
        assert!(
            matches!(left.queue_rights_to_peer(Vec::new(), 1), Ok(()))
                && !right.has_queued_rights(),
            "no longer holds every message"
        );

        let (left, right) = stream_pair(2);
        assert!(matches!(left.queue_rights_to_peer(carried(60), 1), Ok(())));
        assert!(matches!(left.queue_rights_to_peer(Vec::new(), 5), Ok(())));
        assert!(
            right.take_stream_bytes(3, |_| true).is_none(),
            "no read goes past a message whose descriptors it detached"
        );
        assert!(
            right
                .take_queued_rights(&[(1, 60)], false, false, false, None)
                .is_some()
        );

        let (left, right) = stream_pair(4);
        assert!(matches!(left.queue_rights_to_peer(carried(70), 2), Ok(())));
        assert_eq!(
            inodes(right.take_stream_bytes(0, |detached| detached.is_some())),
            Some(vec![70]),
            "a read of no bytes detaches the oldest message's descriptors"
        );
        assert_eq!(inodes(right.take_stream_bytes(2, |_| true)), Some(vec![]));

        let (left, right) = stream_pair(6);
        assert!(matches!(left.queue_rights_to_peer(Vec::new(), 3), Ok(())));
        assert!(matches!(left.queue_rights_to_peer(carried(80), 1), Ok(())));
        let taken = right
            .take_queued_rights(&[(1, 80)], false, false, false, Some(4))
            .unwrap();
        assert!(taken.joinable && taken.lockstep && taken.forgotten.is_empty());
        assert_eq!(
            taken
                .sent
                .iter()
                .map(|sent| sent.file.1)
                .collect::<Vec<_>>(),
            [80]
        );
        assert!(!right.has_queued_rights());

        // A peek reads past data alone to the first message that carried
        // descriptors, which is first among those that did, even where a later
        // one names the same files.
        let (left, right) = stream_pair(8);
        assert!(matches!(left.queue_rights_to_peer(Vec::new(), 3), Ok(())));
        assert!(matches!(left.queue_rights_to_peer(carried(90), 1), Ok(())));
        assert!(matches!(left.queue_rights_to_peer(carried(90), 1), Ok(())));
        assert_eq!(
            right
                .take_queued_rights(&[(1, 90)], false, true, false, None)
                .map(|taken| taken.joinable),
            Some(true)
        );
    }

    /// A stream read that reports no count (it failed copying an output out
    /// after it took its bytes) took what Linux takes for the bytes it asked
    /// for (`DetFd::stream_read_count`): bytes from the oldest messages on,
    /// stopping where it has them all, after the message whose descriptors
    /// it detached, or where the queue runs out. A queue that does not hold
    /// every message, or holds a suspect one, cannot say.
    #[test]
    fn a_stream_read_takes_what_linux_takes_for_what_it_asked() {
        let owner = DetTid::from_raw(10);
        let stream_pair = |sequence| {
            let socket = |fd, sequence| {
                DetFd::new(
                    fd,
                    OFlag::O_RDWR,
                    FdType::Socket,
                    OpenFileId::new(owner, sequence),
                )
            };
            let (left, right) = (socket(3, sequence), socket(4, sequence + 1));
            left.pair_sockets(&right, true);
            left.mark_complete_pair(&right);
            (left, right)
        };
        let carried = |inode| {
            vec![SentDescriptor {
                file: (1, inode),
                open_file: None,
                lock_copy: None,
                suspect: false,
            }]
        };

        let (left, right) = stream_pair(0);
        assert_eq!(right.stream_read_count(4), Some(0), "an empty queue");
        assert!(matches!(left.queue_rights_to_peer(Vec::new(), 3), Ok(())));
        assert!(matches!(left.queue_rights_to_peer(carried(50), 2), Ok(())));
        assert!(matches!(left.queue_rights_to_peer(Vec::new(), 4), Ok(())));
        assert_eq!(right.stream_read_count(0), Some(0));
        assert_eq!(right.stream_read_count(2), Some(2), "all it asked for");
        assert_eq!(
            right.stream_read_count(4),
            Some(4),
            "into the message that carried descriptors"
        );
        assert_eq!(
            right.stream_read_count(100),
            Some(5),
            "no further than the message whose descriptors it detached"
        );
        assert_eq!(
            right.take_stream_bytes(5, |_| true).map(|sent| sent.len()),
            Some(1)
        );
        assert_eq!(right.stream_read_count(100), Some(4), "the queue runs out");

        let (left, right) = stream_pair(2);
        assert!(matches!(left.queue_rights_to_peer(carried(60), 2), Ok(())));
        assert!(matches!(left.queue_rights_to_peer(carried(61), 2), Ok(())));
        assert_eq!(right.stream_read_count(1), Some(1), "part of a message");
        right.lose_lockstep_if_queued(1, false);
        assert_eq!(
            right.stream_read_count(1),
            None,
            "after a read that may have taken a message unseen"
        );

        let (left, right) = stream_pair(4);
        assert!(matches!(left.queue_rights_to_peer(carried(70), 2), Ok(())));
        right.give_up_completeness();
        assert_eq!(
            right.stream_read_count(1),
            None,
            "a queue that no longer holds every message"
        );
    }

    /// A peek may reach a later message (`SO_PEEK_OFF`). It is trusted
    /// wherever it is while the queue is in lockstep and no other queued
    /// message names the same files; otherwise only within the suspects.
    #[test]
    fn a_peek_may_name_a_later_message() {
        let owner = DetTid::from_raw(10);
        let socket = |fd, sequence| {
            DetFd::new(
                fd,
                OFlag::O_RDWR,
                FdType::Socket,
                OpenFileId::new(owner, sequence),
            )
        };
        let carried = |inode| {
            vec![SentDescriptor {
                file: (1, inode),
                open_file: None,
                lock_copy: None,
                suspect: false,
            }]
        };
        let peek = |socket: &DetFd, inode| {
            socket
                .take_queued_rights(&[(1, inode)], false, true, false, None)
                .map(|taken| taken.joinable)
        };
        let (left, right) = (socket(3, 0), socket(4, 1));
        left.pair_sockets(&right, false);
        for inode in [10, 20] {
            assert!(matches!(
                left.queue_rights_to_peer(carried(inode), 1),
                Ok(())
            ));
        }
        assert_eq!(peek(&right, 20), Some(true), "unique, in lockstep");
        assert!(matches!(left.queue_rights_to_peer(carried(20), 1), Ok(())));
        assert_eq!(peek(&right, 20), Some(false), "another message names it");

        let (left, right) = (socket(5, 2), socket(6, 3));
        left.pair_sockets(&right, false);
        for inode in [10, 20] {
            assert!(matches!(
                left.queue_rights_to_peer(carried(inode), 1),
                Ok(())
            ));
        }
        left.lose_peer_lockstep();
        assert_eq!(peek(&right, 20), Some(false), "out of lockstep");
    }

    /// The last held reference to a queued socket lets go of it with its own
    /// queue ([`InFlightOpenFile::into_queue_if_last`]), and a long chain of
    /// sockets each queued on the one before is dropped without recursing,
    /// so no guest can overflow the tool thread's stack by building one.
    #[test]
    fn a_chain_of_queued_sockets_is_released_without_recursion() {
        let owner = DetTid::from_raw(10);
        let socket = |sequence| {
            DetFd::new(
                3,
                OFlag::O_RDWR,
                FdType::Socket,
                OpenFileId::new(owner, sequence),
            )
        };
        let first = socket(0);
        let (mut left, mut right) = (socket(1), socket(2));
        left.pair_sockets(&right, false);
        // `first` is queued on `right`; each later pair's receiving socket is
        // queued on the pair after it.
        let sentinel = first.open_file_handle();
        assert!(matches!(
            left.queue_rights_to_peer(
                vec![SentDescriptor {
                    file: (1, 0),
                    open_file: Some(first.in_flight_open_file(true)),
                    lock_copy: None,
                    suspect: false,
                }],
                1
            ),
            Ok(())
        ));
        drop(first);
        for sequence in 0..50_000 {
            let (next_left, next_right) = (socket(3 + 2 * sequence), socket(4 + 2 * sequence));
            next_left.pair_sockets(&next_right, false);
            assert!(matches!(
                next_left.queue_rights_to_peer(
                    vec![SentDescriptor {
                        file: (1, 1),
                        open_file: Some(right.in_flight_open_file(true)),
                        lock_copy: None,
                        suspect: false,
                    }],
                    1
                ),
                Ok(())
            ));
            drop(left);
            drop(right);
            left = next_left;
            right = next_right;
        }
        assert!(
            sentinel.descriptor(3, false).is_some(),
            "the chain holds the first socket"
        );
        drop(left);
        drop(right);
        assert!(sentinel.descriptor(3, false).is_none(), "and lets it go");
    }
}
