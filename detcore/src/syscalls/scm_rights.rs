/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Descriptors a guest receives over `SCM_RIGHTS`.
//!
//! `recvmsg` and `recvmmsg` are the only calls that install descriptors
//! another process sent: Linux's `scm_detach_fds` installs each descriptor
//! that fits the receiver's control buffer, with `O_CLOEXEC` when the call
//! passed `MSG_CMSG_CLOEXEC`, writes one `SCM_RIGHTS` message holding exactly
//! the numbers it installed, releases the rest and sets `MSG_CTRUNC`; a
//! `MSG_PEEK` receive installs them too. The control data the call leaves at
//! the control buffer it was given, read up to the `msg_controllen` Linux
//! writes back, therefore names exactly the descriptors that now exist
//! ([`ReceiveGeometry::delivered`]).
//!
//! Where the tool shares the guest's descriptor table (the in-guest
//! backends), Detcore discovers such a descriptor when a call first names it
//! (`FileMetadata::discover_fd_from_current_process`). Elsewhere (ptrace,
//! KVM) nothing would, and every later call through it that consults
//! Detcore's table failed with `EBADF`
//! (<https://github.com/rrnewton/hermit/issues/4547>). So there Detcore records
//! each received descriptor as the receive returns
//! ([`Detcore::record_received_descriptors`]), from the facts the in-guest
//! discovery takes (`fcntl(F_GETFD)`, `fcntl(F_GETFL)` and `fstat`, here
//! injected), through the same `FileMetadata::add_discovered_fd`, so the
//! record is the description the in-guest discovery makes: a directory, for
//! one, gets a sorted stream of its own. Unlike that discovery, a received
//! descriptor is never given the container's stdio resource (it is not the
//! container's stdio), nor rebuilt as a fixed procfs file or held for the
//! session's `/proc/cpuinfo` decision (those are an exec image's own
//! descriptors); a rendered procfs placeholder is adopted through the
//! placeholder registry instead, as `Detcore::adopt_received_placeholders`
//! adopts one when it is first named.
//!
//! An anonymous inode has no file type, so `fstat` does not tell its kind.
//! Its descriptor link does: one that reads `anon_inode:inotify` (an injected
//! `readlinkat` of `/proc/thread-self/fd/<fd>`) is recorded as an inotify
//! instance, as the in-guest discovery records it, so its reads renumber
//! rename cookies (`crate::syscalls::inotify`).
//!
//! Not recorded, as before, each named in a warning: any other anonymous
//! inode (eventfd, signalfd, timerfd, epoll, pidfd, userfaultfd, ...), which
//! an ordinary record would mishandle
//! (<https://github.com/rrnewton/hermit/issues/4107>'s signalfd mode), and
//! one whose link cannot be read; a receive whose buffers overlap so that the
//! call may have overwritten its
//! own inputs, whose vector cannot be read in full, or whose inputs changed
//! before the attempt that succeeded; and every receive on the background
//! path. Under record and replay and without sequentialized threads no
//! receive is recorded, logged at debug level only (`ReceiptMode`).
//!
//! Every received descriptor is a description of its own in Detcore's model,
//! as in the in-guest discovery, even when it is an alias of a description
//! the process already holds (it sent the descriptor to itself, say): Linux
//! then shares the status flags, the directory position and lock ownership
//! between the two, and Detcore's two models do not
//! (<https://github.com/rrnewton/hermit/issues/4670>). Detcore cannot tell such
//! an alias apart on every backend, and joining them only where it can would
//! make the guest-visible result depend on the backend.

use std::os::fd::RawFd;

use reverie::Error;
use reverie::Guest;
use reverie::syscalls;
use reverie::syscalls::Addr;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::PathPtr;
use reverie::syscalls::Syscall;
use reverie::syscalls::linux_abi::Mmsghdr;
use reverie::syscalls::linux_abi::Msghdr;
use tracing::debug;
use tracing::warn;

use crate::record_or_replay::RecordOrReplay;
use crate::syscalls::rendered_procfs::placeholder_identity;
use crate::syscalls::rendered_procfs::placeholders_exist;
use crate::tool_local::Detcore;

/// The bytes `received_descriptor_link` gives the path
/// `/proc/thread-self/fd/<fd>`: 21, up to 10 digits and the terminating NUL.
const LINK_PATH: usize = 32;
/// The bytes it gives the link read there. A link that fills them is longer
/// than `anon_inode:inotify` (18 bytes), so it is not read whole.
const LINK_TARGET: usize = 64;

/// The bytes of one received control buffer Detcore reads. One receive
/// delivers at most one `SCM_RIGHTS` message, of at most `SCM_MAX_FD` (253)
/// descriptors, beside a few fixed-size messages, far below this.
const MAX_CONTROL_BYTES: usize = 64 * 1024;

/// The descriptors carried by the `SCM_RIGHTS` messages in a sent or
/// received control buffer.
pub(crate) fn received_descriptors(control: &[u8]) -> Vec<RawFd> {
    const HEADER: usize = size_of::<libc::cmsghdr>();
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

/// The descriptors a `struct msghdr`, read after a receive, says Linux
/// installed through `SCM_RIGHTS`: Linux updates `msg_controllen` to the
/// control bytes it wrote, so bytes past it (a reused buffer's earlier
/// messages) are not read.
fn delivered_through<M: MemoryAccess>(memory: &M, header: &Msghdr) -> Vec<RawFd> {
    if header.msg_control.is_null() || header.msg_controllen == 0 {
        return Vec::new();
    }
    let Some(address) = Addr::<u8>::from_raw(header.msg_control as usize) else {
        return Vec::new();
    };
    let mut control = vec![0; header.msg_controllen.min(MAX_CONTROL_BYTES)];
    if memory.read_exact(address, &mut control).is_err() {
        return Vec::new();
    }
    received_descriptors(&control)
}

/// The descriptors `call`, which returned `ret`, installed through
/// `SCM_RIGHTS`, in the order Linux reports them: those of a `recvmsg`, or of
/// the `ret` messages a `recvmmsg` received. `None` for any other call and
/// for a failed one. Read from guest memory after the call; a header that
/// cannot be read names none.
pub(crate) fn delivered_descriptors<M: MemoryAccess>(
    memory: &M,
    call: &Syscall,
    ret: i64,
) -> Option<Vec<RawFd>> {
    if ret < 0 {
        return None;
    }
    match call {
        Syscall::Recvmsg(call) => {
            let header: Option<Msghdr> = call
                .msg()
                .and_then(|address| memory.read_value(address).ok());
            Some(header.map_or_else(Vec::new, |header| delivered_through(memory, &header)))
        }
        Syscall::Recvmmsg(call) => {
            let Some(address) = call.mmsg() else {
                return Some(Vec::new());
            };
            let count = (ret as usize).min(libc::UIO_MAXIOV as usize);
            // SAFETY: `mmsghdr` is a plain C record; all-zero values are
            // valid staging values that `read_values` overwrites.
            let mut messages: Vec<Mmsghdr> =
                (0..count).map(|_| unsafe { std::mem::zeroed() }).collect();
            if memory.read_values(address.into(), &mut messages).is_err() {
                return Some(Vec::new());
            }
            Some(
                messages
                    .iter()
                    .flat_map(|message| delivered_through(memory, &message.msg_hdr))
                    .collect(),
            )
        }
        _ => None,
    }
}

/// Whether a `struct stat` can be a rendered procfs placeholder: an empty
/// regular file with no link, as `discover_fd_from_current_process` and
/// `Detcore::adopt_received_placeholders` require too.
fn may_be_placeholder(stat: &libc::stat) -> bool {
    stat.st_mode & libc::S_IFMT == libc::S_IFREG && stat.st_nlink == 0 && stat.st_size == 0
}

/// A guest address range, `(start, length)`.
type Range = (usize, usize);

/// The most `recvmmsg` entries whose geometry Detcore reads before a call.
/// Linux processes up to `vlen` entries (`do_recvmmsg` has no `UIO_MAXIOV`
/// clamp, unlike `sendmmsg`), as many as there are datagrams; a larger
/// vector is not snapshotted, and its descriptors are not recorded.
const MAX_SNAPSHOT_ENTRIES: usize = 64 * 1024;

/// The input-only fields of one message: Linux reads them and never writes
/// them back, so their values after the call are the ones the receive used.
#[derive(Clone, Debug, PartialEq, Eq)]
struct MessageFields {
    name: usize,
    iov: usize,
    iovlen: usize,
    control: usize,
    /// The iovec array's segments, `(base, length)`.
    segments: Vec<Range>,
}

/// One message of a receive, as its inputs were before the call.
#[derive(Clone, Debug, PartialEq, Eq)]
struct MessageInput {
    /// The header Linux reads and writes back into: the `msghdr`, or for
    /// `recvmmsg` the whole `mmsghdr` entry (its `msg_len` too).
    header: Range,
    /// The control buffer and its capacity (`msg_controllen` on input).
    control: Range,
    /// The name buffer Linux may write (`msg_name`, `msg_namelen` on input).
    name: Range,
    /// The iovec array Linux reads.
    iov_array: Range,
    fields: MessageFields,
}

/// Reads the input-only fields of `message`; `None` if its iovec array
/// cannot be read (Linux then fails that message with EFAULT).
fn message_fields<M: MemoryAccess>(memory: &M, message: &Msghdr) -> Option<MessageFields> {
    let mut segments = Vec::new();
    if !message.msg_iov.is_null() && message.msg_iovlen > 0 {
        let count = message.msg_iovlen.min(libc::UIO_MAXIOV as usize);
        // SAFETY: `iovec` is a plain C record; all-zero values are valid
        // staging values that `read_values` overwrites.
        let mut iovecs: Vec<libc::iovec> =
            (0..count).map(|_| unsafe { std::mem::zeroed() }).collect();
        memory
            .read_values(
                Addr::<libc::iovec>::from_raw(message.msg_iov as usize)?,
                &mut iovecs,
            )
            .ok()?;
        segments.extend(
            iovecs
                .iter()
                .map(|iov| (iov.iov_base as usize, iov.iov_len)),
        );
    }
    Some(MessageFields {
        name: message.msg_name as usize,
        iov: message.msg_iov as usize,
        iovlen: message.msg_iovlen,
        control: message.msg_control as usize,
        segments,
    })
}

/// Whether any output range overlaps any input range other than its own
/// counterpart: `outputs` pairs each range with whether an identical input
/// range is its own (a header written back into itself, a control buffer
/// filled with its own control data). Counted with sorted interval ends:
/// the inputs overlapping `[start, end)` are those starting before `end`
/// less those ending at or before `start`.
fn outputs_overlap_inputs(inputs: &[Range], outputs: &[(Range, bool)]) -> bool {
    let live = |range: &&Range| range.1 > 0;
    let mut starts: Vec<usize> = inputs.iter().filter(live).map(|range| range.0).collect();
    let mut ends: Vec<usize> = inputs
        .iter()
        .filter(live)
        .map(|range| range.0.saturating_add(range.1))
        .collect();
    starts.sort_unstable();
    ends.sort_unstable();
    outputs.iter().any(|&((start, length), own)| {
        if length == 0 {
            return false;
        }
        let end = start.saturating_add(length);
        let overlapping =
            starts.partition_point(|&s| s < end) - ends.partition_point(|&e| e <= start);
        overlapping > usize::from(own)
    })
}

/// The input geometry of a `recvmsg` or `recvmmsg`, read before the call:
/// each message's header, control buffer, name buffer and iovecs, and a
/// `recvmmsg`'s timeout. Read before because Linux overwrites some inputs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReceiveGeometry {
    messages: Vec<MessageInput>,
    /// A `recvmmsg`, whose result counts messages.
    batch: bool,
    /// A `recvmmsg` timeout Linux writes the remaining time back into.
    timeout: Option<Range>,
    /// Why the geometry cannot be trusted, if it cannot.
    unusable: Option<&'static str>,
}

/// What a completed receive delivered over `SCM_RIGHTS`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Delivered {
    /// The descriptors Linux reports installing, in order.
    pub(crate) fds: Vec<RawFd>,
    /// Why the receive's descriptors cannot be named, if they cannot: then
    /// `fds` is empty, and none of the call's descriptors is recorded.
    pub(crate) refused: Option<&'static str>,
}

impl ReceiveGeometry {
    /// The geometry of `call`, a `recvmsg` or `recvmmsg`, read from guest
    /// memory before it runs; `None` for any other call or an unreadable
    /// `recvmsg` header. A `recvmmsg` vector is read in full, up to the
    /// `vlen` Linux may process (at most [`MAX_SNAPSHOT_ENTRIES`] entries);
    /// an entry or iovec array that cannot be read makes the geometry
    /// unusable, and the call's descriptors are not recorded.
    pub(crate) fn before<M: MemoryAccess>(memory: &M, call: &Syscall) -> Option<Self> {
        let message_input = |header: Range, message: &Msghdr| -> Option<MessageInput> {
            Some(MessageInput {
                header,
                control: (message.msg_control as usize, message.msg_controllen),
                name: (message.msg_name as usize, message.msg_namelen as usize),
                iov_array: (
                    message.msg_iov as usize,
                    message.msg_iovlen.min(libc::UIO_MAXIOV as usize) * size_of::<libc::iovec>(),
                ),
                fields: message_fields(memory, message)?,
            })
        };
        match call {
            Syscall::Recvmsg(call) => {
                let address = call.msg()?;
                let message: Msghdr = memory.read_value(address).ok()?;
                let header = (address.as_raw(), size_of::<Msghdr>());
                let (messages, unusable) = match message_input(header, &message) {
                    Some(input) => (vec![input], None),
                    None => (vec![], Some("its iovec array cannot be read")),
                };
                Some(ReceiveGeometry {
                    messages,
                    batch: false,
                    timeout: None,
                    unusable,
                })
            }
            Syscall::Recvmmsg(call) => {
                let base = call.mmsg().map_or(0, |vector| vector.as_raw());
                let timeout = call
                    .timeout()
                    .map(|timeout| (timeout.as_raw(), size_of::<libc::timespec>()));
                if call.vlen() as usize > MAX_SNAPSHOT_ENTRIES {
                    return Some(ReceiveGeometry {
                        messages: Vec::new(),
                        batch: true,
                        timeout,
                        unusable: Some("its vector is too long to read before the call"),
                    });
                }
                let mut messages = Vec::new();
                let mut unusable = None;
                for index in 0..call.vlen() as usize {
                    let at = base.saturating_add(index * size_of::<Mmsghdr>());
                    let Some(Ok(entry)) =
                        Addr::<Mmsghdr>::from_raw(at).map(|entry| memory.read_value(entry))
                    else {
                        unusable = Some("an entry of its vector cannot be read");
                        break;
                    };
                    let entry: Mmsghdr = entry;
                    match message_input((at, size_of::<Mmsghdr>()), &entry.msg_hdr) {
                        Some(input) => messages.push(input),
                        None => {
                            unusable = Some("an iovec array cannot be read");
                            break;
                        }
                    }
                }
                Some(ReceiveGeometry {
                    messages,
                    batch: true,
                    timeout,
                    unusable,
                })
            }
            _ => None,
        }
    }

    /// Whether an output range of the call overlaps an input range other
    /// than its own: every range Linux may write for any entry it may
    /// process (a header's written-back fields and `msg_len`, a name
    /// buffer, an iovec segment, a control buffer, the timeout) against
    /// every range it reads (headers, iovec arrays, control buffers).
    fn overlapping(&self) -> bool {
        let mut inputs = Vec::new();
        let mut outputs = Vec::new();
        for message in &self.messages {
            inputs.extend([message.header, message.iov_array, message.control]);
            outputs.extend([
                (message.header, true),
                (message.control, true),
                (message.name, false),
            ]);
            outputs.extend(
                message
                    .fields
                    .segments
                    .iter()
                    .map(|&segment| (segment, false)),
            );
        }
        outputs.extend(self.timeout.map(|timeout| (timeout, false)));
        outputs_overlap_inputs(&inputs, &outputs)
    }

    /// Whether the input-only fields read now, after the call, are the
    /// ones read before it. After a successful attempt that ran in the
    /// caller's turn no other guest thread ran, so these are the values that
    /// attempt used: a sibling that changed them during an earlier attempt's
    /// wait (`EAGAIN`, then polling) is detected here.
    fn still_named<M: MemoryAccess>(&self, memory: &M) -> bool {
        self.messages.iter().all(|message| {
            Addr::<Msghdr>::from_raw(message.header.0)
                .and_then(|at| memory.read_value(at).ok())
                .and_then(|now: Msghdr| message_fields(memory, &now))
                .is_some_and(|now| now == message.fields)
        })
    }

    /// The descriptors a receive with this geometry, which returned `ret`,
    /// installed: for each delivered message (one for `recvmsg`, the first
    /// `ret` for `recvmmsg`), the control bytes Linux reports writing (the
    /// `msg_controllen` it writes back) at the control buffer named before
    /// the call. None are named if an output of the call overlaps one of its
    /// inputs, if the inputs changed before the attempt that succeeded, or if
    /// Linux reports more control bytes than the buffer named before the call
    /// held.
    pub(crate) fn delivered<M: MemoryAccess>(&self, memory: &M, ret: i64) -> Delivered {
        let refused = |reason| Delivered {
            fds: Vec::new(),
            refused: Some(reason),
        };
        if ret < 0 {
            return Delivered::default();
        }
        if let Some(reason) = self.unusable {
            return refused(reason);
        }
        if self.overlapping() {
            return refused("a buffer it writes overlaps a buffer or header it reads");
        }
        if !self.still_named(memory) {
            return refused("its buffers changed before the attempt that succeeded");
        }
        let count = if self.batch {
            (ret as usize).min(self.messages.len())
        } else {
            self.messages.len().min(1)
        };
        let mut fds = Vec::new();
        for message in &self.messages[..count] {
            if message.control.0 == 0 || message.control.1 == 0 {
                continue;
            }
            let written = message.header.0 + std::mem::offset_of!(Msghdr, msg_controllen);
            let Some(length) =
                Addr::<usize>::from_raw(written).and_then(|at| memory.read_value(at).ok())
            else {
                return refused("its control length cannot be read back");
            };
            if length > message.control.1 {
                return refused("its control buffer grew before the attempt that succeeded");
            }
            let length = length.min(MAX_CONTROL_BYTES);
            if length == 0 {
                continue;
            }
            let mut control = vec![0; length];
            match Addr::<u8>::from_raw(message.control.0)
                .map(|at| memory.read_exact(at, &mut control))
            {
                Some(Ok(())) => fds.extend(received_descriptors(&control)),
                _ => return refused("its control data cannot be read back"),
            }
        }
        Delivered { fds, refused: None }
    }
}

/// How this run handles descriptors received over `SCM_RIGHTS`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReceiptMode {
    /// The tool shares the guest's descriptor table and discovers a received
    /// descriptor when a call first names it.
    Discovered,
    /// Detcore records each one as the receive returns.
    Recorded,
    /// Detcore keeps no record of them, for the reason given.
    Unrecorded(&'static str),
}

impl<T: RecordOrReplay> Detcore<T> {
    /// How this run handles received descriptors. Recorded only where the
    /// tool does not share the guest's descriptor table (it discovers them
    /// there), with sequentialized threads (otherwise another thread could
    /// close a number, or `dup2` another file over it, between the receive
    /// and the facts taken here; a received rendered procfs descriptor is
    /// refused by name there when first named,
    /// `Detcore::adopt_received_placeholders`), and not under record and
    /// replay (replay stands in a placeholder for each received descriptor
    /// and serves its calls from the log, so facts taken from the kernel
    /// would differ between the two runs).
    fn receipt_mode<G: Guest<Self>>(&self, guest: &G) -> ReceiptMode {
        if self.cfg.backend.tool_shares_guest_descriptor_table {
            ReceiptMode::Discovered
        } else if self.cfg.recordreplay_modes {
            ReceiptMode::Unrecorded("under record and replay")
        } else if !guest.config().sequentialize_threads {
            ReceiptMode::Unrecorded("without sequentialized threads")
        } else {
            ReceiptMode::Recorded
        }
    }

    /// Whether a receive through `call` completes in the caller's turn,
    /// decided before the call: the descriptor is one Detcore polls in turn
    /// (`IOAction::NonblockizeRetry`) or one already nonblocking
    /// (`IOAction::PassThru`). Any other receive runs on the background path
    /// (`BlockingExternalIO`: a physically blocking socket, or one Detcore
    /// keeps no model of), where other tasks run between the kernel's
    /// completion and the caller's continuation and could close or replace a
    /// delivered number, or rewrite the control data; its descriptors are
    /// never recorded.
    pub(crate) fn receipt_is_observable<G, C>(&self, guest: &mut G, call: C) -> bool
    where
        G: Guest<Self>,
        C: crate::syscalls::helpers::NonblockableSyscall + Into<Syscall>,
    {
        matches!(
            crate::syscalls::helpers::ioaction_based_on_fd_status(guest, call),
            Ok(crate::syscalls::helpers::IOAction::NonblockizeRetry
                | crate::syscalls::helpers::IOAction::PassThru)
        )
    }

    /// Whether the tool discovers received descriptors itself (it shares the
    /// guest's descriptor table), so that a receive records none.
    pub(crate) fn discovers_received_descriptors<G: Guest<Self>>(&self, guest: &G) -> bool {
        self.receipt_mode(guest) == ReceiptMode::Discovered
    }

    /// Record in this thread's descriptor table each descriptor a receive
    /// installed (`delivered`), where this run records received descriptors
    /// and the receive completed in the caller's turn (`in_turn`,
    /// [`Self::receipt_is_observable`]), and return those it recorded. The
    /// guest's result is never changed. A descriptor left out of the table, as
    /// every received descriptor was before, is named in a warning: for each
    /// receive whose layout cannot be trusted, each receive on the background
    /// path, each number that already has a record, each anonymous inode
    /// other than an inotify instance, and each descriptor whose facts cannot
    /// be taken. Under record and replay
    /// and without sequentialized threads none is recorded, logged at debug
    /// level only (a warning per call, or per tool process, would differ
    /// between runs that share a process). `cloexec` is whether the receive
    /// asked for its descriptors close-on-exec (`MSG_CMSG_CLOEXEC`), which a
    /// descriptor listed without its facts keeps.
    pub(crate) async fn record_received_descriptors<G: Guest<Self>>(
        &self,
        guest: &mut G,
        delivered: &Delivered,
        in_turn: bool,
        cloexec: bool,
    ) -> Vec<RawFd> {
        match self.receipt_mode(guest) {
            ReceiptMode::Discovered => return Vec::new(),
            ReceiptMode::Unrecorded(why) => {
                if !delivered.fds.is_empty() || delivered.refused.is_some() {
                    // A listing of this table cannot come from the model now
                    // (`syscalls::files::descriptor_directory`).
                    guest.thread_state().note_unaccounted_receipt();
                }
                if !delivered.fds.is_empty() {
                    debug!(
                        "[detcore] descriptors {:?}, received over SCM_RIGHTS, are not recorded \
                         ({why})",
                        delivered.fds
                    );
                }
                return Vec::new();
            }
            ReceiptMode::Recorded => {}
        }
        if let Some(reason) = delivered.refused {
            guest.thread_state().note_unaccounted_receipt();
            warn!(
                "[detcore] the descriptors a receive got over SCM_RIGHTS, if any, are not \
                 recorded: {reason}"
            );
            return Vec::new();
        }
        if !in_turn {
            if !delivered.fds.is_empty() {
                guest.thread_state().note_unaccounted_receipt();
                warn!(
                    "[detcore] descriptors {:?}, received over SCM_RIGHTS on the background \
                     path, are not recorded: another task may have closed or replaced their \
                     numbers, or rewritten the control data, before this one resumed",
                    delivered.fds
                );
            }
            return Vec::new();
        }
        let mut recorded = Vec::new();
        for &fd in &delivered.fds {
            if fd < 0 {
                continue;
            }
            match self.record_received_descriptor(guest, fd).await {
                Ok(true) => recorded.push(fd),
                Ok(false) => {}
                Err(error) => {
                    // Open, but without its facts: listed, never modelled.
                    guest.thread_state().note_unmodelled_descriptor(fd, cloexec);
                    warn!(
                        "[detcore] descriptor {fd}, received over SCM_RIGHTS, is not recorded: \
                         {error}"
                    )
                }
            }
        }
        recorded
    }

    /// Record descriptor `fd`, which Linux has just installed through
    /// `SCM_RIGHTS`, from the kernel's facts about it.
    /// Whether it was recorded.
    async fn record_received_descriptor<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
    ) -> Result<bool, Error> {
        // Linux gave the receiver free numbers. A record of one already
        // describes something Detcore does not know to be gone: kept as it is.
        if guest.thread_state().with_detfd(fd, |_| ()).is_ok() {
            warn!(
                "[detcore] descriptor {fd}, received over SCM_RIGHTS, already has a record, \
                 which is kept"
            );
            return Ok(false);
        }
        // Passive probes, retried when a signal stopped them before they ran.
        let descriptor_flags = guest
            .inject_with_retry(
                syscalls::Fcntl::new()
                    .with_fd(fd)
                    .with_cmd(syscalls::FcntlCmd::F_GETFD),
            )
            .await? as i32;
        let status_flags = guest
            .inject_with_retry(
                syscalls::Fcntl::new()
                    .with_fd(fd)
                    .with_cmd(syscalls::FcntlCmd::F_GETFL),
            )
            .await? as i32;
        let stat = self.inject_fstat(guest, fd).await?;
        // An anonymous inode (an eventfd, signalfd, timerfd, epoll set,
        // inotify instance, pidfd, userfaultfd, ...) has no file type, and
        // `fstat` cannot tell which it is. Recorded as an ordinary file, its
        // reads and flag changes would take paths that are wrong for it (a
        // blocking signalfd read held in the caller's turn, an
        // `EFD_SEMAPHORE` eventfd read twice). Its link names an inotify
        // instance, which is recorded as one, so its reads renumber rename
        // cookies (`crate::syscalls::inotify`); any other is left unrecorded,
        // as before.
        let link = if stat.st_mode & libc::S_IFMT == 0 {
            self.received_descriptor_link(guest, fd).await
        } else {
            None
        };
        if stat.st_mode & libc::S_IFMT == 0
            && link.as_deref() != Some(crate::syscalls::inotify::INOTIFY_LINK)
        {
            // Open, and listed in the table's descriptor directory, but not
            // modelled.
            guest
                .thread_state()
                .note_unmodelled_descriptor(fd, descriptor_flags & libc::FD_CLOEXEC != 0);
            match &link {
                Some(link) => warn!(
                    "[detcore] descriptor {fd}, received over SCM_RIGHTS, is an anonymous inode \
                     ({}) that an ordinary record would mishandle: not recorded",
                    String::from_utf8_lossy(link)
                ),
                None => warn!(
                    "[detcore] descriptor {fd}, received over SCM_RIGHTS, is an anonymous inode \
                     whose link cannot be read, so its kind is unknown: not recorded"
                ),
            }
            return Ok(false);
        }
        if may_be_placeholder(&stat)
            && placeholders_exist()
            && self.renders_procfs(guest)
            && let Some(path) = placeholder_identity(stat.st_dev, stat.st_ino)
        {
            self.adopt_received_placeholder(guest, fd, path, (stat.st_dev, stat.st_ino))
                .await?;
            return Ok(true);
        }
        // Every received descriptor is a description of its own in the model,
        // even where it is an alias of one this process holds (see the
        // module documentation), made exactly as the in-guest discovery
        // makes it: a directory gets a sorted stream of its own.
        guest.thread_state().add_discovered_fd(
            fd,
            descriptor_flags,
            status_flags,
            &stat,
            link.as_deref(),
        )?;
        // A placeholder descriptor, which its identity and its tag tell apart
        // (`descriptor_directory`).
        self.recognize_received_placeholder(guest, fd, &stat, status_flags)
            .await;
        Ok(true)
    }

    /// What descriptor `fd` of the calling thread links to, read in the
    /// guest (`readlinkat` of `/proc/thread-self/fd/<fd>`, injected), or
    /// `None` where that read fails or the link is longer than any anonymous
    /// inode's. The path and the result share one buffer, staged where
    /// `inject_fstat` stages its own
    /// ([`crate::syscalls::scratch::inject_with_scratch`]).
    async fn received_descriptor_link<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
    ) -> Option<Vec<u8>> {
        let mut buffer = [0_u8; LINK_PATH + LINK_TARGET];
        let spelled = format!("/proc/thread-self/fd/{fd}");
        buffer
            .get_mut(..spelled.len())?
            .copy_from_slice(spelled.as_bytes());
        let (length, bytes) =
            crate::syscalls::scratch::inject_with_scratch(guest, buffer, |base| {
                // SAFETY: the buffer holds LINK_PATH + LINK_TARGET bytes.
                let target = unsafe { base.add(LINK_PATH) };
                syscalls::Readlinkat::new()
                    .with_dirfd(libc::AT_FDCWD)
                    .with_path(PathPtr::from_ptr(base.as_raw() as *const libc::c_char))
                    .with_buf(Some(target.cast::<libc::c_char>()))
                    .with_buf_len(LINK_TARGET)
                    .into()
            })
            .await
            .ok()?;
        let length = usize::try_from(length)
            .ok()
            .filter(|length| *length < LINK_TARGET)?;
        Some(bytes[LINK_PATH..LINK_PATH + length].to_vec())
    }
}

#[cfg(test)]
mod tests {
    use reverie::syscalls::AddrMut;
    use reverie::syscalls::LocalMemory;
    use reverie::syscalls::Timespec;

    use super::*;

    /// One control message's bytes, padded as `CMSG_SPACE` pads it.
    fn message(level: i32, kind: i32, fds: &[RawFd]) -> Vec<u8> {
        let length = size_of::<libc::cmsghdr>() + size_of_val(fds);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&length.to_ne_bytes());
        bytes.extend_from_slice(&level.to_ne_bytes());
        bytes.extend_from_slice(&kind.to_ne_bytes());
        fds.iter()
            .for_each(|fd| bytes.extend_from_slice(&fd.to_ne_bytes()));
        bytes.resize(length.next_multiple_of(size_of::<usize>()), 0);
        bytes
    }

    #[test]
    fn rights_are_found_in_every_control_message() {
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

    /// A `msghdr` whose control buffer is `control`, with `msg_controllen`
    /// set to `delivered`, as Linux leaves it after a receive.
    fn header(control: &mut [u8], delivered: usize) -> Msghdr {
        // SAFETY: all-zero is a valid `msghdr`.
        let mut header: Msghdr = unsafe { std::mem::zeroed() };
        header.msg_control = control.as_mut_ptr().cast();
        header.msg_controllen = delivered;
        header
    }

    /// Only the bytes Linux reports writing are read: a reused buffer's
    /// earlier `SCM_RIGHTS` message past `msg_controllen` names nothing, as
    /// on a receive that delivered no descriptor.
    #[test]
    fn only_the_delivered_control_bytes_name_descriptors() {
        let mut control = message(libc::SOL_SOCKET, libc::SCM_RIGHTS, &[9, 10]);
        let length = control.len();
        let memory = LocalMemory::new();
        assert_eq!(
            delivered_through(&memory, &header(&mut control, length)),
            vec![9, 10]
        );
        assert!(delivered_through(&memory, &header(&mut control, 0)).is_empty());
        // A null control buffer names nothing whatever its length says.
        let mut empty = header(&mut control, length);
        empty.msg_control = std::ptr::null_mut();
        assert!(delivered_through(&memory, &empty).is_empty());
    }

    /// MSG_CTRUNC: Linux writes one message holding only the descriptors it
    /// installed (`cmsg_len` is `CMSG_LEN` of those) and reports the bytes
    /// it consumed, at most the buffer's length, so a buffer with room for
    /// one of three descriptors names exactly that one.
    #[test]
    fn a_truncated_receive_names_only_the_installed_descriptor() {
        // CMSG_LEN(sizeof(int)): room for one descriptor, no padding.
        let room = size_of::<libc::cmsghdr>() + size_of::<RawFd>();
        let mut control = message(libc::SOL_SOCKET, libc::SCM_RIGHTS, &[12]);
        control.truncate(room);
        let memory = LocalMemory::new();
        assert_eq!(
            delivered_through(&memory, &header(&mut control, room)),
            vec![12]
        );
        // Room for none: Linux writes no message and reports 0 bytes.
        let mut nothing = vec![0u8; size_of::<libc::cmsghdr>()];
        assert!(delivered_through(&memory, &header(&mut nothing, 0)).is_empty());
    }

    /// `recvmsg` reads the one header after the call; `recvmmsg` the headers
    /// of the messages it reports, not the vector's later entries; other
    /// calls and failures name none.
    #[test]
    fn receives_name_the_descriptors_of_the_messages_they_delivered() {
        let memory = LocalMemory::new();
        let mut first = message(libc::SOL_SOCKET, libc::SCM_RIGHTS, &[3]);
        let mut second = message(libc::SOL_SOCKET, libc::SCM_RIGHTS, &[4, 5]);
        let mut header_one = header(&mut first, 0);
        header_one.msg_controllen = first.len();
        let recvmsg = Syscall::Recvmsg(
            syscalls::Recvmsg::new()
                .with_sockfd(7)
                .with_msg(AddrMut::from_ptr(&raw mut header_one)),
        );
        assert_eq!(delivered_descriptors(&memory, &recvmsg, 1), Some(vec![3]));
        assert_eq!(delivered_descriptors(&memory, &recvmsg, -1), None);

        // SAFETY: all-zero is a valid `mmsghdr`.
        let mut vector: [Mmsghdr; 2] = unsafe { std::mem::zeroed() };
        vector[0].msg_hdr = header(&mut first, 0);
        vector[0].msg_hdr.msg_controllen = first.len();
        vector[1].msg_hdr = header(&mut second, 0);
        vector[1].msg_hdr.msg_controllen = second.len();
        let recvmmsg = Syscall::Recvmmsg(
            syscalls::Recvmmsg::new()
                .with_fd(7)
                .with_mmsg(AddrMut::from_ptr(vector.as_mut_ptr().cast()))
                .with_vlen(2),
        );
        assert_eq!(
            delivered_descriptors(&memory, &recvmmsg, 2),
            Some(vec![3, 4, 5])
        );
        assert_eq!(delivered_descriptors(&memory, &recvmmsg, 1), Some(vec![3]));
        assert_eq!(delivered_descriptors(&memory, &recvmmsg, 0), Some(vec![]));
        let read = Syscall::Read(syscalls::Read::new().with_fd(7));
        assert_eq!(delivered_descriptors(&memory, &read, 1), None);
    }

    /// A `recvmsg` whose header names `control`.
    fn recvmsg_of(header: &mut Msghdr) -> Syscall {
        Syscall::Recvmsg(
            syscalls::Recvmsg::new()
                .with_sockfd(7)
                .with_msg(AddrMut::from_ptr(header as *mut Msghdr)),
        )
    }

    /// The control buffer is the one named before the call: a header whose
    /// `msg_control` the receive overwrote still names the descriptors at
    /// the buffer Linux wrote them to, and only the bytes it reports.
    #[test]
    fn the_control_buffer_named_before_the_call_is_read() {
        let memory = LocalMemory::new();
        let mut control = message(libc::SOL_SOCKET, libc::SCM_RIGHTS, &[8, 9]);
        let length = control.len();
        // On the heap, written below only through this pointer, as the
        // kernel and a sibling thread write the guest's header.
        let header = Box::into_raw(Box::new(header(&mut control, length)));
        // SAFETY: `header` is a live allocation this test owns.
        let call = recvmsg_of(unsafe { &mut *header });
        let geometry = ReceiveGeometry::before(&memory, &call).unwrap();
        let delivered = geometry.delivered(&memory, 1);
        assert_eq!(delivered.fds, vec![8, 9]);
        assert_eq!(delivered.refused, None);
        assert_eq!(geometry.delivered(&memory, -1), Delivered::default());
        // Linux reports 0 control bytes: a reused buffer names nothing.
        // SAFETY: as above.
        unsafe { (*header).msg_controllen = 0 };
        assert_eq!(geometry.delivered(&memory, 1), Delivered::default());
        // More bytes than the buffer named before the call held: the
        // capacity changed before the attempt that succeeded.
        // SAFETY: as above.
        unsafe { (*header).msg_controllen = length + 8 };
        assert!(geometry.delivered(&memory, 1).refused.is_some());
        // The pointer changed before the attempt that succeeded (a sibling
        // during an earlier attempt's wait): nothing is named.
        // SAFETY: as above.
        unsafe {
            (*header).msg_controllen = length;
            (*header).msg_control = std::ptr::dangling_mut();
        }
        let delivered = geometry.delivered(&memory, 1);
        assert!(delivered.fds.is_empty() && delivered.refused.is_some());
        // SAFETY: made by `Box::into_raw` above, and no longer read.
        drop(unsafe { Box::from_raw(header) });
    }

    /// Outputs against inputs: only a range written back into itself is
    /// exempt, counted over sorted interval ends.
    #[test]
    fn an_output_may_overlap_only_its_own_input() {
        let header = (1000, 56);
        let control = (2000, 24);
        assert!(!outputs_overlap_inputs(
            &[header, control],
            &[(header, true), (control, true), ((3000, 16), false)]
        ));
        // A name buffer over the control buffer, or over a header.
        assert!(outputs_overlap_inputs(
            &[header, control],
            &[((2010, 4), false)]
        ));
        assert!(outputs_overlap_inputs(
            &[header, control],
            &[((1040, 8), false)]
        ));
        // A control buffer that is also another message's.
        assert!(outputs_overlap_inputs(
            &[control, control],
            &[(control, true)]
        ));
        // Touching ends do not overlap; empty ranges never do.
        assert!(!outputs_overlap_inputs(
            &[header],
            &[((1056, 8), false), ((1000, 0), false)]
        ));
    }

    /// Whatever the call may write over what it reads refuses the whole
    /// receive: a control buffer inside its own header, a name buffer over
    /// its control buffer, and a timeout over a control buffer.
    #[test]
    fn a_receive_whose_outputs_overlap_its_inputs_names_nothing() {
        let memory = LocalMemory::new();
        #[repr(C, align(8))]
        struct Both {
            header: Msghdr,
            control: [u8; 24],
        }
        // SAFETY: all-zero is a valid `Both`.
        let mut both: Both = unsafe { std::mem::zeroed() };
        let inside = (&raw mut both.header).cast::<u8>().wrapping_add(16);
        both.header.msg_control = inside.cast();
        both.header.msg_controllen = 20;
        let call = recvmsg_of(&mut both.header);
        let geometry = ReceiveGeometry::before(&memory, &call).unwrap();
        assert!(geometry.delivered(&memory, 1).refused.is_some());

        let mut control = message(libc::SOL_SOCKET, libc::SCM_RIGHTS, &[4]);
        let mut header = header(&mut control, 0);
        header.msg_controllen = control.len();
        header.msg_name = control.as_mut_ptr().cast();
        header.msg_namelen = 8;
        let call = recvmsg_of(&mut header);
        let geometry = ReceiveGeometry::before(&memory, &call).unwrap();
        assert!(geometry.delivered(&memory, 1).refused.is_some());

        let mut control = message(libc::SOL_SOCKET, libc::SCM_RIGHTS, &[5]);
        // SAFETY: all-zero is a valid `mmsghdr`.
        let mut vector: [Mmsghdr; 1] = unsafe { std::mem::zeroed() };
        vector[0].msg_hdr = super::tests::header(&mut control, 0);
        vector[0].msg_hdr.msg_controllen = control.len();
        let timeout = control.as_ptr().cast::<Timespec>();
        let call = Syscall::Recvmmsg(
            syscalls::Recvmmsg::new()
                .with_fd(7)
                .with_mmsg(AddrMut::from_ptr(vector.as_mut_ptr()))
                .with_vlen(1)
                .with_timeout(Addr::from_ptr(timeout)),
        );
        let geometry = ReceiveGeometry::before(&memory, &call).unwrap();
        assert!(geometry.delivered(&memory, 1).refused.is_some());
    }

    /// A batch names the descriptors of the messages it reports, however
    /// many (recvmmsg has no `UIO_MAXIOV` clamp), bounded by the vector it was
    /// given; two messages sharing a control buffer refuse the whole batch.
    #[test]
    fn a_batch_names_every_reported_message_up_to_its_vector() {
        let memory = LocalMemory::new();
        let count = libc::UIO_MAXIOV as usize + 76;
        let mut controls: Vec<Vec<u8>> = (0..count)
            .map(|index| message(libc::SOL_SOCKET, libc::SCM_RIGHTS, &[index as RawFd + 3]))
            .collect();
        // SAFETY: all-zero is a valid `mmsghdr`.
        let mut vector: Vec<Mmsghdr> = (0..count).map(|_| unsafe { std::mem::zeroed() }).collect();
        for (entry, control) in vector.iter_mut().zip(controls.iter_mut()) {
            entry.msg_hdr = header(control, 0);
            entry.msg_hdr.msg_controllen = control.len();
        }
        let call = Syscall::Recvmmsg(
            syscalls::Recvmmsg::new()
                .with_fd(7)
                .with_mmsg(AddrMut::from_ptr(vector.as_mut_ptr()))
                .with_vlen(count as u32),
        );
        let geometry = ReceiveGeometry::before(&memory, &call).unwrap();
        let delivered = geometry.delivered(&memory, count as i64);
        assert_eq!(delivered.fds.len(), count);
        assert_eq!(delivered.fds[count - 1], count as RawFd + 2);
        assert_eq!(geometry.delivered(&memory, 2).fds, vec![3, 4]);
        // More reported than given: the vector bounds it.
        assert_eq!(
            geometry.delivered(&memory, count as i64 + 9).fds.len(),
            count
        );
        // Two entries sharing one control buffer.
        vector[1].msg_hdr.msg_control = vector[0].msg_hdr.msg_control;
        let geometry = ReceiveGeometry::before(&memory, &call).unwrap();
        let delivered = geometry.delivered(&memory, 3);
        assert!(delivered.fds.is_empty() && delivered.refused.is_some());
    }

    #[test]
    fn only_an_empty_unlinked_regular_file_may_be_a_placeholder() {
        // SAFETY: all-zero is a valid `stat`.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        stat.st_mode = libc::S_IFREG | 0o600;
        assert!(may_be_placeholder(&stat));
        stat.st_nlink = 1;
        assert!(!may_be_placeholder(&stat));
        stat.st_nlink = 0;
        stat.st_size = 1;
        assert!(!may_be_placeholder(&stat));
        stat.st_size = 0;
        stat.st_mode = libc::S_IFIFO | 0o600;
        assert!(!may_be_placeholder(&stat));
    }
}
