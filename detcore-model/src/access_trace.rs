/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Record types of the access trace (`--record-access-trace-to`).
//!
//! The access trace is a sidecar of a run that records its schedule. It says
//! what each guest operation touched, keyed by the operation's position in that
//! schedule (the run's own `.events` file), so a reader can line two runs up and
//! name the conflicting operations whose order differs between them.
//!
//! The file is JSON Lines: one [`TraceRecord`] per line, tagged by `"kind"`. The
//! first line is a [`TraceHeader`]. A run that finishes cleanly ends with a
//! [`TraceEnd`]; a trace without one was cut short. A record is written when
//! its last schedule event is recorded, so syscall records follow `post`; a
//! call that never returned is written when its thread enters its next call,
//! or at the end of the run. Readers sort by the index they need.
//!
//! Every event index here is a position in the recorded schedule: the index the
//! scheduler gave the event when it recorded it. Syscall records carry the
//! thread's own syscall count as `nth`, the unit of
//! [`crate::happens_before::Position::SyscallCount`], so `(tid, nth)` names the
//! same operation in two runs of one program while its control flow agrees.
//!
//! The trace observes; it never feeds back into the run. Nothing in these types
//! is read by the scheduler.
//!
//! Not recorded: some operations touch more than their records say.
//! - splice and copy_file_range with a NULL offset move the file cursor, but
//!   record no cursor access;
//! - poll, select, ppoll and the epoll waits record no access to the
//!   descriptors or channels they check;
//! - path operations (unlink, rename, stat by path, the dirfd of the `*at`
//!   calls) record nothing;
//! - FUTEX_WAIT_REQUEUE_PI records no access to its second futex word;
//! - kill(-1) records nothing.

use std::fmt;
use std::io;
use std::io::BufRead;
use std::io::Write;

use reverie_syscalls::Sysno;
use serde::Deserialize;
use serde::Serialize;

use crate::fd::DetInode;
use crate::fd::FdSlot;
use crate::fd::OpenFileId;
use crate::futex::FutexID;
use crate::futex::MmId;
use crate::pid::DetPid;
use crate::pid::DetTid;

/// The `format` string every access trace header carries.
pub const ACCESS_TRACE_FORMAT: &str = "hermit-access-trace";

/// The record format version this crate writes and reads.
///
/// Version 2 names the signal disposition observed at the recorded event.
/// Version 1 claimed a delivery outcome and is rejected rather than reinterpreted.
pub const ACCESS_TRACE_VERSION: u32 = 2;

/// The bytes of header and records a trace holds at most when
/// `--access-trace-max-bytes` is not given: 1 GiB. A record takes about 350
/// bytes, so this is some three million records; a run with more stops its
/// trace there, without the end line, rather than fill the disk.
pub const ACCESS_TRACE_DEFAULT_MAX_BYTES: u64 = 1 << 30;

/// At most this many leading bytes of a write to fd 1 or 2 are kept in
/// [`SyscallRecord::out_bytes`].
pub const OUT_BYTES_CAP: usize = 256;

/// A position in the run's recorded schedule (its `.events` file).
pub type EventIndex = u64;

/// One line of an access trace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TraceRecord {
    /// First line: format, version and the first recorded index.
    Header(TraceHeader),
    /// One completed (or never-returning) syscall.
    Syscall(SyscallRecord),
    /// One signal event and its observed disposition.
    Signal(SignalRecord),
    /// One synchronization edge Detcore observed.
    Edge(EdgeRecord),
    /// One hardware watchpoint hit.
    WatchHit(WatchHitRecord),
    /// Last line of a trace whose run finished cleanly.
    End(TraceEnd),
}

/// Identifies the file and the slice of the schedule it covers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceHeader {
    /// Always [`ACCESS_TRACE_FORMAT`].
    pub format: String,
    /// [`ACCESS_TRACE_VERSION`] of the writer.
    pub version: u32,
    /// `--access-trace-from`: records whose last event lies before this index
    /// are omitted. Resource state built before it (who opened an fd) is
    /// therefore only partly visible.
    pub from: EventIndex,
}

impl TraceHeader {
    /// The header this crate's writer emits for a trace starting at `from`.
    pub fn new(from: EventIndex) -> Self {
        Self {
            format: ACCESS_TRACE_FORMAT.to_string(),
            version: ACCESS_TRACE_VERSION,
            from,
        }
    }
}

/// Closes a complete trace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceEnd {
    /// Schedule events the run recorded in total (the `.events` length).
    pub events: u64,
    /// Records written between the header and this line.
    pub records: u64,
}

/// A thread at one schedule event: one end of an [`EdgeRecord`].
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize
)]
pub struct EventRef {
    /// The thread.
    pub tid: DetTid,
    /// The event of that thread.
    pub event: EventIndex,
}

/// One syscall, from its prehook event to its posthook event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyscallRecord {
    /// Index of the syscall's prehook event.
    pub pre: EventIndex,
    /// Index of its posthook event; `None` when the call never returned to
    /// the guest (`exit`, `exit_group`, a successful `execve`, or a thread
    /// that died inside it).
    pub post: Option<EventIndex>,
    /// Calling thread.
    pub tid: DetTid,
    /// Calling thread's process.
    pub pid: DetPid,
    /// The calling thread's syscall count including this call (1 for its
    /// first). A restarted call is counted, and recorded, again.
    pub nth: u64,
    /// The syscall, written by name so a trace reads the same on any
    /// architecture.
    #[serde(with = "sysno_name")]
    pub sysno: Sysno,
    /// Raw argument registers.
    pub args: [u64; 6],
    /// The value the guest received: the result, or the negated errno.
    /// `None` when [`Self::post`] is `None`.
    pub result: Option<i64>,
    /// Resources the call used, created or released, in the order of its
    /// arguments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accesses: Vec<Access>,
    /// The bytes the kernel returned through guest buffers (read, pread,
    /// readv, recv, recvmsg, ...); `None` when the call returned none. Two
    /// runs whose calls return the same count but different bytes differ
    /// here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub returned: Option<ReturnedBytes>,
    /// The first [`OUT_BYTES_CAP`] bytes a successful write to fd 1 or 2
    /// carried, hex-encoded on disk.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "hex_bytes")]
    pub out_bytes: Option<Vec<u8>>,
}

/// The bytes a syscall returned through guest buffers, summarized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReturnedBytes {
    /// Bytes returned, over all of the call's buffers.
    pub len: u64,
    /// [`returned_digest`] of the first [`RETURNED_DIGEST_CAP`] of them, in
    /// buffer order.
    pub digest: u64,
    /// `len` exceeds [`RETURNED_DIGEST_CAP`], so bytes past the cap are not in
    /// the digest: equal digests with this set prove nothing about them.
    pub truncated: bool,
}

/// At most this many returned bytes enter [`ReturnedBytes::digest`].
pub const RETURNED_DIGEST_CAP: u64 = 64 * 1024;

/// The 64-bit FNV-1a hash [`ReturnedBytes::digest`] uses: fold each buffer's
/// bytes in order into the running value, starting from
/// [`RETURNED_DIGEST_SEED`].
pub fn returned_digest(state: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(state, |hash, &b| {
        (hash ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// The FNV-1a offset basis, the starting value of [`returned_digest`].
pub const RETURNED_DIGEST_SEED: u64 = 0xcbf2_9ce4_8422_2325;

/// A half-open byte range `[start, end)`.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize
)]
pub struct ByteRange {
    /// First byte.
    pub start: u64,
    /// One past the last byte.
    pub end: u64,
}

impl ByteRange {
    /// Whether the two ranges share at least one byte.
    pub fn overlaps(&self, other: &ByteRange) -> bool {
        self.start < other.end && other.start < self.end
    }
}

/// One resource access of a syscall. Section 4 of the M5 design names the
/// identities; [`Access::conflicts_with`] states which pairs conflict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "resource", rename_all = "snake_case")]
pub enum Access {
    /// A descriptor number in one descriptor table.
    Fd {
        /// The table and the number.
        slot: FdSlot,
        /// For `use` and `close`, the open file the number reached before the
        /// call; for `open` and `dup`, the open file it holds after. `None`
        /// when the number held nothing Detcore models.
        open_file: Option<OpenFileId>,
        /// What the call did to the slot.
        op: FdOp,
    },
    /// The cursor of an open file description, shared by dup and fork aliases.
    OpenFile {
        /// The open file description.
        open_file: OpenFileId,
        /// How the call used the cursor.
        op: CursorOp,
    },
    /// File contents.
    FileContents {
        /// The file.
        inode: DetInode,
        /// Bytes the call read or wrote; `None` when Detcore does not know the
        /// offset (a cursor-relative read or write).
        range: Option<ByteRange>,
        /// Read or write.
        op: DataOp,
    },
    /// Data through a pipe, FIFO, socket or eventfd.
    Channel {
        /// The open file the call used.
        open_file: OpenFileId,
        /// An identity shared by both ends of the channel when Hermit knows
        /// both ends (a pipe, a socketpair, a FIFO by its inode); otherwise
        /// `None`, and the open file is the only identity. Sockets connected
        /// with connect and accept, inside the container or to a network
        /// peer outside it, are `None` in this version.
        id: Option<ChannelId>,
        /// What kind of channel.
        channel: ChannelKind,
        /// Direction of the data, or a change of the end's state.
        op: ChannelOp,
    },
    /// A futex word.
    Futex {
        /// The word's key; process-shared futexes match across processes.
        futex: FutexID,
        /// The futex operation on this word.
        op: FutexOp,
    },
    /// A range of one address space.
    Memory {
        /// The address space.
        mm: MmId,
        /// The bytes.
        range: ByteRange,
        /// What the call did to them.
        op: MemoryOp,
    },
    /// A timer.
    Timer {
        /// The timer.
        timer: TimerId,
        /// What the call did to it.
        op: TimerOp,
    },
    /// A thread or process as a whole.
    Lifetime {
        /// The target.
        target: Task,
        /// What the call did to it.
        op: LifetimeOp,
    },
}

/// What a syscall did to a descriptor slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FdOp {
    /// The call used the number (read, write, lseek, fstat, ioctl, ...).
    Use,
    /// The call installed a new open file at the number (open, openat,
    /// socket, pipe, accept, eventfd, ...).
    Open {
        /// The path the guest named, when there was one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    /// The number now refers to the open file of `source` (dup, dup2, dup3,
    /// `F_DUPFD`). Replacing an open number also emits a `close` of it.
    Dup {
        /// The descriptor that was duplicated.
        source: i32,
    },
    /// The call released the number (close, close_range, dup2 onto it, or
    /// exec closing a close-on-exec descriptor).
    Close,
}

/// How a syscall used an open file's cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CursorOp {
    /// Read from the cursor and advanced it.
    Read,
    /// Wrote at the cursor and advanced it.
    Write,
    /// Moved or read the cursor (lseek).
    Seek,
}

/// Read or write of data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataOp {
    /// Read.
    Read,
    /// Wrote (including truncation).
    Write,
}

/// The identity both ends of one channel share.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize
)]
#[serde(rename_all = "snake_case")]
pub enum ChannelId {
    /// A pipe or a socketpair, named by the open file of the end its
    /// creating call returned first (a pipe's read end, `sv[0]` of a
    /// socketpair). Dup and fork aliases keep it.
    Pair(OpenFileId),
    /// A FIFO, named by its inode, which every open of it shares.
    Inode(DetInode),
}

/// Kinds of channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    /// An anonymous pipe.
    Pipe,
    /// A named pipe.
    Fifo,
    /// `SOCK_STREAM`.
    StreamSocket,
    /// `SOCK_DGRAM`.
    DatagramSocket,
    /// `SOCK_SEQPACKET`.
    SeqpacketSocket,
    /// A socket whose type Detcore does not record.
    Socket,
    /// An eventfd counter.
    Eventfd,
}

/// What a syscall did to a channel end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelOp {
    /// Put data in (write, send).
    Write,
    /// Took data out, or found none (read, recv).
    Read,
    /// Shut down one or both directions.
    Shutdown,
    /// Closed this descriptor of the end.
    Close,
}

/// A futex operation on one word.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FutexOp {
    /// Waited on the word (`FUTEX_WAIT`, `FUTEX_WAIT_BITSET`,
    /// `FUTEX_WAIT_REQUEUE_PI`).
    Wait,
    /// Woke waiters of the word (`FUTEX_WAKE`, `FUTEX_WAKE_BITSET`, and the
    /// first word of `FUTEX_WAKE_OP` and of the requeue operations).
    Wake,
    /// Received the waiters a requeue moved (`uaddr2` of `FUTEX_REQUEUE`,
    /// `FUTEX_CMP_REQUEUE`, `FUTEX_CMP_REQUEUE_PI`).
    RequeueTarget,
    /// Modified the word, then conditionally woke its waiters (`uaddr2` of
    /// `FUTEX_WAKE_OP`).
    ModifyWake,
    /// Took a priority-inheritance lock (`FUTEX_LOCK_PI`, `FUTEX_LOCK_PI2`,
    /// `FUTEX_TRYLOCK_PI`).
    Lock,
    /// Released one (`FUTEX_UNLOCK_PI`).
    Unlock,
}

/// What a syscall did to a range of memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryOp {
    /// Mapped the range (mmap, the new range of mremap, shmat).
    Map,
    /// Unmapped it (munmap, the old range of a moving mremap, shmdt).
    Unmap,
    /// Changed its protection (mprotect, pkey_mprotect).
    Protect,
    /// Gave advice that can drop its contents (madvise).
    Advise,
    /// Moved the program break over it (brk).
    Brk,
    /// The kernel read a guest buffer in it for the call (write's buffer).
    SyscallRead,
    /// The kernel wrote a guest buffer in it for the call (read's buffer).
    SyscallWrite,
}

impl MemoryOp {
    /// Whether this operation changes the bytes, or which bytes are mapped
    /// and how: everything but the kernel reading a buffer.
    pub fn writes_or_remaps(self) -> bool {
        self != MemoryOp::SyscallRead
    }
}

/// Identity of a timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimerId {
    /// A POSIX timer (timer_create) of one process.
    Posix {
        /// The owning process.
        process: DetPid,
        /// The id timer_create returned.
        id: i32,
    },
    /// An interval timer of one process (setitimer; `alarm` is
    /// `ITIMER_REAL`, which is 0).
    Interval {
        /// The owning process.
        process: DetPid,
        /// `ITIMER_REAL`, `ITIMER_VIRTUAL` or `ITIMER_PROF`.
        which: i32,
    },
    /// A timerfd, which is an open file.
    Fd {
        /// The timerfd's open file.
        open_file: OpenFileId,
    },
}

/// What a syscall did to a timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimerOp {
    /// Created it.
    Create,
    /// Armed or disarmed it (timer_settime, setitimer, alarm, timerfd_settime).
    Set,
    /// Read its remaining time (timer_gettime, getitimer, timerfd_gettime).
    Get,
    /// Deleted it.
    Delete,
    /// Consumed its expirations (read of a timerfd).
    Read,
}

/// A thread, a process, or a process group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Task {
    /// One thread.
    Thread(DetTid),
    /// A whole thread group.
    Process(DetPid),
    /// Every process of a process group, named by its group id. A
    /// signal to a group (kill with 0 or a negative pid) also lists one
    /// `process` target per member Detcore's process table holds when the
    /// call's posthook is recorded; kill(-1), every process the caller may
    /// signal, lists none.
    ProcessGroup(DetPid),
}

/// What a syscall did to a thread or process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifetimeOp {
    /// Created it (clone, fork, vfork).
    Create,
    /// Ended it (exit ends a thread, exit_group a process).
    Exit,
    /// Replaced its image (execve).
    Exec,
    /// Sent it a signal (kill, tkill, tgkill, rt_sigqueueinfo, pidfd_send_signal).
    Signal {
        /// The signal; 0 probes for existence.
        signo: i32,
    },
    /// Reaped or observed its exit (wait4, waitid).
    Wait,
}

impl Access {
    /// Whether two accesses touch the same resource in a way whose order can
    /// change a result. The two accesses are assumed to come from different
    /// threads; the caller checks that.
    ///
    /// This is the per-resource rule of the design's section 4, applied to
    /// syscall data alone. It is deliberately conservative where Detcore does
    /// not know an extent: an unknown file range overlaps any range.
    pub fn conflicts_with(&self, other: &Access) -> bool {
        use Access::*;
        match (self, other) {
            (Fd { slot: a, op: x, .. }, Fd { slot: b, op: y, .. }) => {
                a == b && !(matches!(x, FdOp::Use) && matches!(y, FdOp::Use))
            }
            // Every cursor operation reads the position, and read, write and
            // seek move it, so any two on one description conflict.
            (OpenFile { open_file: a, .. }, OpenFile { open_file: b, .. }) => a == b,
            (
                FileContents {
                    inode: a,
                    range: ra,
                    op: x,
                },
                FileContents {
                    inode: b,
                    range: rb,
                    op: y,
                },
            ) => {
                a == b
                    && (*x == DataOp::Write || *y == DataOp::Write)
                    && match (ra, rb) {
                        (Some(ra), Some(rb)) => ra.overlaps(rb),
                        _ => true,
                    }
            }
            // Two reads race for the same data and two writes for its order,
            // so any two operations on one channel conflict.
            (
                Channel {
                    open_file: fa,
                    id: ia,
                    ..
                },
                Channel {
                    open_file: fb,
                    id: ib,
                    ..
                },
            ) => fa == fb || (ia.is_some() && ia == ib),
            (Futex { futex: a, op: x }, Futex { futex: b, op: y }) => {
                a == b && !(*x == FutexOp::Wait && *y == FutexOp::Wait)
            }
            (
                Memory {
                    mm: a,
                    range: ra,
                    op: x,
                },
                Memory {
                    mm: b,
                    range: rb,
                    op: y,
                },
            ) => a == b && ra.overlaps(rb) && (x.writes_or_remaps() || y.writes_or_remaps()),
            (Timer { timer: a, op: x }, Timer { timer: b, op: y }) => {
                a == b && !(*x == TimerOp::Get && *y == TimerOp::Get)
            }
            (Lifetime { target: a, op: x }, Lifetime { target: b, op: y }) => {
                a == b && !(*x == LifetimeOp::Wait && *y == LifetimeOp::Wait)
            }
            _ => false,
        }
    }
}

/// The installed signal disposition observed before submitting its schedule event.
///
/// Another thread can change the disposition before the kernel delivers the
/// signal. This observation does not prove that a handler ran, an action took
/// effect, or the process died.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedSignalDisposition {
    /// A guest handler was installed.
    Handler,
    /// `SIG_IGN` was installed.
    Ignored,
    /// `SIG_DFL` was installed, regardless of that signal's default action.
    Default,
    /// The disposition could not be read.
    Unknown,
}

/// The sender of a user-generated signal, from its siginfo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignalSender {
    /// `si_pid`, as a deterministic pid.
    pub pid: DetPid,
    /// The sending thread, when Detcore saw the send.
    pub tid: Option<DetTid>,
}

/// One signal event and the disposition observed before submitting it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignalRecord {
    /// Index of the recorded `SignalReceived` event.
    pub event: EventIndex,
    /// Thread that reported the signal event.
    pub tid: DetTid,
    /// Signal number.
    pub signo: i32,
    /// Raw Linux `si_code`, whose meaning depends on the particular code.
    pub code: i32,
    /// `si_addr` of a kernel-generated SIGSEGV, SIGBUS, SIGILL, SIGFPE or
    /// SIGTRAP; `None` otherwise.
    pub addr: Option<u64>,
    /// Sender of a user-sent signal; `None` for a kernel-generated one.
    pub sender: Option<SignalSender>,
    /// Installed disposition observed before submitting the signal event;
    /// this does not establish the eventual delivery outcome.
    pub observed_disposition: ObservedSignalDisposition,
}

/// Kinds of synchronization edge (design section 7). Data edges through
/// pipes and sockets are derived by the reader from syscall results; they are
/// not recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// A futex wake (`FUTEX_WAKE`, `FUTEX_WAKE_BITSET`, `FUTEX_WAKE_OP`, the
    /// woken part of a requeue) to the woken waiter's wait return.
    FutexWake,
    /// A requeue that moved a waiter to another word, to that waiter's
    /// eventual wait return.
    FutexRequeue,
    /// A thread or process creation to the child's first event.
    Create,
    /// A thread's exit, through its clear-child-tid wake (or the wake of a
    /// robust futex it held), to the woken waiter's return, from the
    /// exiting thread's last event.
    ExitJoin,
    /// A child process's exit, from its last event, to the parent's wait4
    /// or waitid return.
    ChildExit,
    /// A signal send (kill, tkill, tgkill, rt_sigqueueinfo,
    /// rt_tgsigqueueinfo) or a POSIX timer's arm (timer_settime), from its
    /// prehook, to the delivery it caused.
    Signal,
}

/// The object an edge synchronizes through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncObject {
    /// A futex word.
    Futex(FutexID),
    /// A thread (the created child).
    Thread(DetTid),
    /// A process (the exited child).
    Process(DetPid),
    /// A signal number.
    Signal(i32),
}

/// One happens-before edge between two events of two threads. `from` precedes
/// `to` in the recorded schedule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeRecord {
    /// What kind of synchronization.
    #[serde(rename = "edge")]
    pub kind: EdgeKind,
    /// The releasing side: the waker's prehook, the creator's clone, the exit.
    pub from: EventRef,
    /// The acquiring side: the waiter's posthook, the child's first event.
    pub to: EventRef,
    /// The object both sides used.
    pub object: SyncObject,
}

/// One frame of a watchpoint hit's stack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackFrame {
    /// Instruction pointer.
    pub ip: u64,
    /// Function, when symbolized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function: Option<String>,
    /// Source file, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Source line, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

/// A hardware watchpoint hit (reserved for `--watch`; no writer emits it yet).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchHitRecord {
    /// Schedule events recorded before the hit: it lies between event
    /// `after - 1` and event `after`.
    pub after: EventIndex,
    /// Thread whose access hit.
    pub tid: DetTid,
    /// Debug-register slot.
    pub slot: u8,
    /// Watched address that was accessed.
    pub addr: u64,
    /// Instruction pointer after the access.
    pub ip: u64,
    /// Stack, when it was unwound.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stack: Vec<StackFrame>,
}

impl TraceRecord {
    /// Write the record as one JSON line.
    pub fn write_line<W: Write>(&self, out: &mut W) -> io::Result<()> {
        serde_json::to_writer(&mut *out, self).map_err(io::Error::from)?;
        out.write_all(b"\n")
    }

    /// The record as one JSON line, its newline included: the bytes
    /// [`TraceRecord::write_line`] writes.
    pub fn to_line(&self) -> io::Result<Vec<u8>> {
        let mut line = Vec::new();
        self.write_line(&mut line)?;
        Ok(line)
    }
}

/// A whole trace, as [`read_access_trace`] returns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessTrace {
    /// The first line.
    pub header: TraceHeader,
    /// Everything between the header and the end line, in file order.
    pub records: Vec<TraceRecord>,
    /// The end line; `None` when the run did not finish cleanly.
    pub end: Option<TraceEnd>,
}

/// Why a trace could not be read.
#[derive(Debug)]
pub enum AccessTraceError {
    /// Reading the file failed.
    Io(io::Error),
    /// The file is empty.
    Empty,
    /// A line is not a record of this version.
    BadLine {
        /// 1-based line number.
        line: usize,
        /// What serde reported.
        message: String,
    },
    /// The first line is not a header.
    MissingHeader,
    /// The header names another format or version.
    Unsupported {
        /// `format` in the file.
        format: String,
        /// `version` in the file.
        version: u32,
    },
    /// A header or end line appears where it does not belong.
    Misplaced {
        /// 1-based line number.
        line: usize,
    },
}

impl fmt::Display for AccessTraceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AccessTraceError::Io(e) => write!(f, "reading the access trace failed: {e}"),
            AccessTraceError::Empty => write!(
                f,
                "the access trace is empty; rerun with --record-access-trace-to and let the run finish"
            ),
            AccessTraceError::BadLine { line, message } => {
                write!(f, "access trace line {line} is not a record: {message}")
            }
            AccessTraceError::MissingHeader => write!(
                f,
                "the access trace does not start with a header line; it was not written by --record-access-trace-to"
            ),
            AccessTraceError::Unsupported { format, version } => write!(
                f,
                "the access trace is {format} version {version}; this build reads {ACCESS_TRACE_FORMAT} version {ACCESS_TRACE_VERSION}: record it again with this build"
            ),
            AccessTraceError::Misplaced { line } => write!(
                f,
                "access trace line {line} is a header or end line in the middle of the file; the file was concatenated or corrupted"
            ),
        }
    }
}

impl std::error::Error for AccessTraceError {}

impl From<io::Error> for AccessTraceError {
    fn from(e: io::Error) -> Self {
        AccessTraceError::Io(e)
    }
}

/// Read and validate a whole access trace.
pub fn read_access_trace<R: BufRead>(input: R) -> Result<AccessTrace, AccessTraceError> {
    let mut header = None;
    let mut records = Vec::new();
    let mut end = None;
    for (index, line) in input.lines().enumerate() {
        let number = index + 1;
        let line = line?;
        if line.is_empty() {
            continue;
        }
        let record: TraceRecord =
            serde_json::from_str(&line).map_err(|e| AccessTraceError::BadLine {
                line: number,
                message: e.to_string(),
            })?;
        if end.is_some() {
            return Err(AccessTraceError::Misplaced { line: number });
        }
        match (record, header.is_some()) {
            (TraceRecord::Header(h), false) => {
                if h.format != ACCESS_TRACE_FORMAT || h.version != ACCESS_TRACE_VERSION {
                    return Err(AccessTraceError::Unsupported {
                        format: h.format,
                        version: h.version,
                    });
                }
                header = Some(h);
            }
            (_, false) => return Err(AccessTraceError::MissingHeader),
            (TraceRecord::Header(_), true) => {
                return Err(AccessTraceError::Misplaced { line: number });
            }
            (TraceRecord::End(e), true) => end = Some(e),
            (record, true) => records.push(record),
        }
    }
    Ok(AccessTrace {
        header: header.ok_or(AccessTraceError::Empty)?,
        records,
        end,
    })
}

/// [`Sysno`] by name. Its own serde form is the architecture's number.
mod sysno_name {
    use std::str::FromStr;

    use reverie_syscalls::Sysno;
    use serde::Deserialize;
    use serde::Deserializer;
    use serde::Serializer;
    use serde::de::Error;

    pub fn serialize<S: Serializer>(sysno: &Sysno, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(sysno.name())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Sysno, D::Error> {
        let name = String::deserialize(d)?;
        Sysno::from_str(&name).map_err(|_| D::Error::custom(format!("unknown syscall {name:?}")))
    }
}

/// Hex encoding for [`SyscallRecord::out_bytes`]: exact for any bytes, and
/// half the size of serde's array of numbers.
mod hex_bytes {
    use std::fmt::Write;

    use serde::Deserialize;
    use serde::Deserializer;
    use serde::Serializer;
    use serde::de::Error;

    pub fn serialize<S: Serializer>(bytes: &Option<Vec<u8>>, s: S) -> Result<S::Ok, S::Error> {
        match bytes {
            Some(bytes) => {
                let mut text = String::with_capacity(bytes.len() * 2);
                for b in bytes {
                    write!(text, "{b:02x}").expect("writing to a String cannot fail");
                }
                s.serialize_some(&text)
            }
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<u8>>, D::Error> {
        let Some(text) = Option::<String>::deserialize(d)? else {
            return Ok(None);
        };
        if text.len() % 2 != 0 {
            return Err(D::Error::custom("odd-length hex string"));
        }
        // The two-byte slices below are UTF-8 boundaries only for ASCII.
        if !text.is_ascii() {
            return Err(D::Error::custom("invalid hex digit"));
        }
        (0..text.len())
            .step_by(2)
            .map(|i| {
                u8::from_str_radix(&text[i..i + 2], 16)
                    .map_err(|_| D::Error::custom("invalid hex digit"))
            })
            .collect::<Result<Vec<u8>, _>>()
            .map(Some)
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::fd::FilesId;
    use crate::futex::SharedMemoryObjectId;

    fn tid(n: i32) -> DetTid {
        DetTid::from_raw(n)
    }

    fn slot(fd: i32) -> FdSlot {
        FdSlot {
            files: FilesId::initial(tid(3)),
            fd,
        }
    }

    fn file(creator: i32, sequence: u64) -> OpenFileId {
        OpenFileId::new(tid(creator), sequence)
    }

    fn mm() -> MmId {
        MmId::initial(tid(3))
    }

    fn close_record() -> SyscallRecord {
        SyscallRecord {
            pre: 7587,
            post: Some(7588),
            tid: tid(9),
            pid: DetPid::from_raw(3),
            nth: 1412,
            sysno: Sysno::close,
            args: [6, 0, 0, 0, 0, 0],
            result: Some(0),
            accesses: vec![
                Access::Fd {
                    slot: slot(6),
                    open_file: Some(file(7, 2)),
                    op: FdOp::Close,
                },
                Access::Channel {
                    open_file: file(7, 2),
                    id: Some(ChannelId::Pair(file(7, 1))),
                    channel: ChannelKind::Pipe,
                    op: ChannelOp::Close,
                },
            ],
            returned: None,
            out_bytes: None,
        }
    }

    /// One record of every kind and every resource variant.
    fn every_record() -> Vec<TraceRecord> {
        let futex = FutexID::private(mm(), 0x7f00_0000_1000);
        let shared = FutexID::shared(
            SharedMemoryObjectId::Anonymous {
                origin: mm(),
                sequence: 1,
            },
            64,
        );
        vec![
            TraceRecord::Header(TraceHeader::new(100)),
            TraceRecord::Syscall(close_record()),
            TraceRecord::Syscall(SyscallRecord {
                pre: 7589,
                post: Some(7590),
                tid: tid(7),
                pid: DetPid::from_raw(3),
                nth: 388,
                sysno: Sysno::write,
                args: [2, 0x1000, 5, 0, 0, 0],
                result: Some(5),
                accesses: vec![
                    Access::Fd {
                        slot: slot(2),
                        open_file: Some(file(3, 0)),
                        op: FdOp::Use,
                    },
                    Access::OpenFile {
                        open_file: file(3, 0),
                        op: CursorOp::Write,
                    },
                    Access::FileContents {
                        inode: DetInode::mint(12),
                        range: Some(ByteRange { start: 0, end: 5 }),
                        op: DataOp::Write,
                    },
                    Access::Memory {
                        mm: mm(),
                        range: ByteRange {
                            start: 0x1000,
                            end: 0x1005,
                        },
                        op: MemoryOp::SyscallRead,
                    },
                    Access::Channel {
                        open_file: file(3, 6),
                        id: Some(ChannelId::Inode(DetInode::mint(77))),
                        channel: ChannelKind::Fifo,
                        op: ChannelOp::Shutdown,
                    },
                ],
                returned: None,
                out_bytes: Some(b"oops\n".to_vec()),
            }),
            TraceRecord::Syscall(SyscallRecord {
                pre: 7591,
                post: Some(7592),
                tid: tid(7),
                pid: DetPid::from_raw(3),
                nth: 389,
                sysno: Sysno::openat,
                args: [u64::MAX - 99, 0x2000, 0o2, 0, 0, 0],
                result: Some(6),
                accesses: vec![Access::Fd {
                    slot: slot(6),
                    open_file: Some(file(7, 3)),
                    op: FdOp::Open {
                        path: Some("/tmp/misc".to_string()),
                    },
                }],
                returned: Some(ReturnedBytes {
                    len: 4,
                    digest: returned_digest(RETURNED_DIGEST_SEED, b"data"),
                    truncated: false,
                }),
                out_bytes: None,
            }),
            TraceRecord::Syscall(SyscallRecord {
                pre: 7593,
                post: Some(7594),
                tid: tid(7),
                pid: DetPid::from_raw(3),
                nth: 390,
                sysno: Sysno::dup3,
                args: [6, 5, 0, 0, 0, 0],
                result: Some(5),
                accesses: vec![
                    Access::Fd {
                        slot: slot(6),
                        open_file: Some(file(7, 3)),
                        op: FdOp::Use,
                    },
                    Access::Fd {
                        slot: slot(5),
                        open_file: Some(file(3, 1)),
                        op: FdOp::Close,
                    },
                    Access::Fd {
                        slot: slot(5),
                        open_file: Some(file(7, 3)),
                        op: FdOp::Dup { source: 6 },
                    },
                ],
                returned: Some(ReturnedBytes {
                    len: 4,
                    digest: returned_digest(RETURNED_DIGEST_SEED, b"data"),
                    truncated: false,
                }),
                out_bytes: None,
            }),
            TraceRecord::Syscall(SyscallRecord {
                pre: 8000,
                post: Some(8100),
                tid: tid(5),
                pid: DetPid::from_raw(3),
                nth: 12,
                sysno: Sysno::futex,
                args: [0x7f00_0000_1000, 0x80, 2, 0, 0, 0],
                result: Some(-11),
                accesses: vec![
                    Access::Futex {
                        futex,
                        op: FutexOp::Wait,
                    },
                    Access::Futex {
                        futex: shared,
                        op: FutexOp::RequeueTarget,
                    },
                ],
                returned: Some(ReturnedBytes {
                    len: 4,
                    digest: returned_digest(RETURNED_DIGEST_SEED, b"data"),
                    truncated: false,
                }),
                out_bytes: None,
            }),
            TraceRecord::Syscall(SyscallRecord {
                pre: 8200,
                post: Some(8201),
                tid: tid(5),
                pid: DetPid::from_raw(3),
                nth: 13,
                sysno: Sysno::timer_settime,
                args: [0, 0, 0x3000, 0, 0, 0],
                result: Some(0),
                accesses: vec![
                    Access::Timer {
                        timer: TimerId::Posix {
                            process: DetPid::from_raw(3),
                            id: 0,
                        },
                        op: TimerOp::Set,
                    },
                    Access::Timer {
                        timer: TimerId::Interval {
                            process: DetPid::from_raw(3),
                            which: 0,
                        },
                        op: TimerOp::Get,
                    },
                    Access::Timer {
                        timer: TimerId::Fd {
                            open_file: file(5, 4),
                        },
                        op: TimerOp::Read,
                    },
                ],
                returned: Some(ReturnedBytes {
                    len: 4,
                    digest: returned_digest(RETURNED_DIGEST_SEED, b"data"),
                    truncated: false,
                }),
                out_bytes: None,
            }),
            TraceRecord::Syscall(SyscallRecord {
                pre: 9000,
                post: None,
                tid: tid(3),
                pid: DetPid::from_raw(3),
                nth: 600,
                sysno: Sysno::kill,
                args: [4, 9, 0, 0, 0, 0],
                result: None,
                accesses: vec![
                    Access::Lifetime {
                        target: Task::Process(DetPid::from_raw(4)),
                        op: LifetimeOp::Signal { signo: 9 },
                    },
                    Access::Lifetime {
                        target: Task::Thread(tid(5)),
                        op: LifetimeOp::Exit,
                    },
                    Access::Lifetime {
                        target: Task::ProcessGroup(DetPid::from_raw(3)),
                        op: LifetimeOp::Wait,
                    },
                ],
                returned: Some(ReturnedBytes {
                    len: 4,
                    digest: returned_digest(RETURNED_DIGEST_SEED, b"data"),
                    truncated: false,
                }),
                out_bytes: None,
            }),
            TraceRecord::Signal(SignalRecord {
                event: 9001,
                tid: tid(7),
                signo: 11,
                code: 1,
                addr: Some(0x10),
                sender: None,
                observed_disposition: ObservedSignalDisposition::Default,
            }),
            TraceRecord::Signal(SignalRecord {
                event: 9002,
                tid: tid(4),
                signo: 10,
                code: 0,
                addr: None,
                sender: Some(SignalSender {
                    pid: DetPid::from_raw(3),
                    tid: Some(tid(3)),
                }),
                observed_disposition: ObservedSignalDisposition::Handler,
            }),
            TraceRecord::Edge(EdgeRecord {
                kind: EdgeKind::FutexWake,
                from: EventRef {
                    tid: tid(7),
                    event: 8050,
                },
                to: EventRef {
                    tid: tid(5),
                    event: 8100,
                },
                object: SyncObject::Futex(futex),
            }),
            TraceRecord::Edge(EdgeRecord {
                kind: EdgeKind::Create,
                from: EventRef {
                    tid: tid(3),
                    event: 10,
                },
                to: EventRef {
                    tid: tid(5),
                    event: 12,
                },
                object: SyncObject::Thread(tid(5)),
            }),
            TraceRecord::Edge(EdgeRecord {
                kind: EdgeKind::ChildExit,
                from: EventRef {
                    tid: tid(4),
                    event: 20,
                },
                to: EventRef {
                    tid: tid(3),
                    event: 22,
                },
                object: SyncObject::Process(DetPid::from_raw(4)),
            }),
            TraceRecord::Edge(EdgeRecord {
                kind: EdgeKind::Signal,
                from: EventRef {
                    tid: tid(3),
                    event: 30,
                },
                to: EventRef {
                    tid: tid(4),
                    event: 31,
                },
                object: SyncObject::Signal(10),
            }),
            TraceRecord::WatchHit(WatchHitRecord {
                after: 9003,
                tid: tid(5),
                slot: 0,
                addr: 0x7f00_0000_2000,
                ip: 0x40_1000,
                stack: vec![StackFrame {
                    ip: 0x40_1000,
                    function: Some("main".to_string()),
                    file: Some("racewrite.c".to_string()),
                    line: Some(12),
                }],
            }),
            TraceRecord::End(TraceEnd {
                events: 9004,
                records: 14,
            }),
        ]
    }

    #[test]
    fn every_record_kind_round_trips_through_json_lines() {
        let records = every_record();
        let mut buf = Vec::new();
        for record in &records {
            record.write_line(&mut buf).unwrap();
        }
        let text = String::from_utf8(buf.clone()).unwrap();
        assert_eq!(text.lines().count(), records.len());
        for (line, record) in text.lines().zip(&records) {
            let back: TraceRecord = serde_json::from_str(line).unwrap();
            assert_eq!(&back, record);
        }

        let trace = read_access_trace(&buf[..]).unwrap();
        assert_eq!(trace.header, TraceHeader::new(100));
        assert_eq!(trace.records, records[1..records.len() - 1].to_vec());
        assert_eq!(
            trace.end,
            Some(TraceEnd {
                events: 9004,
                records: 14
            })
        );
    }

    #[test]
    fn on_disk_spelling_is_stable() {
        let mut buf = Vec::new();
        TraceRecord::Syscall(close_record())
            .write_line(&mut buf)
            .unwrap();
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            concat!(
                r#"{"kind":"syscall","pre":7587,"post":7588,"tid":9,"pid":3,"nth":1412,"#,
                r#""sysno":"close","args":[6,0,0,0,0,0],"result":0,"accesses":["#,
                r#"{"resource":"fd","slot":{"files":{"creator":3,"generation":0},"fd":6},"#,
                r#""open_file":{"creator":7,"sequence":2},"op":"close"},"#,
                r#"{"resource":"channel","open_file":{"creator":7,"sequence":2},"#,
                r#""id":{"pair":{"creator":7,"sequence":1}},"#,
                r#""channel":"pipe","op":"close"}]}"#,
                "\n"
            )
        );
    }

    #[test]
    fn out_bytes_are_exact_for_non_utf8_output() {
        let mut record = close_record();
        record.out_bytes = Some(vec![0x00, 0xff, b'\n', 0x80]);
        let line = serde_json::to_string(&TraceRecord::Syscall(record.clone())).unwrap();
        assert!(line.contains(r#""out_bytes":"00ff0a80""#), "{line}");
        let back: TraceRecord = serde_json::from_str(&line).unwrap();
        assert_eq!(back, TraceRecord::Syscall(record));
        assert!(serde_json::from_str::<TraceRecord>(&line.replace("00ff0a80", "0ff0a80")).is_err());
        assert!(
            serde_json::from_str::<TraceRecord>(&line.replace("00ff0a80", "zzff0a80")).is_err()
        );
        assert!(
            serde_json::from_str::<TraceRecord>(
                &line.replace(r#""sysno":"close""#, r#""sysno":"nosuchcall""#)
            )
            .is_err()
        );
    }

    /// Malformed output bytes are a parse error at the input line, never a
    /// panic from splitting a UTF-8 code point into two-byte hex pairs.
    #[test]
    fn reader_rejects_non_hex_output_without_panicking() {
        let header = serde_json::to_string(&TraceRecord::Header(TraceHeader::new(0))).unwrap();
        let input = |encoded: &str| {
            let mut call = serde_json::to_value(TraceRecord::Syscall(close_record())).unwrap();
            call["out_bytes"] = serde_json::json!(encoded);
            format!("{header}\n{call}\n")
        };
        for (encoded, expected) in [
            ("", &[][..]),
            ("00ff0a80", &[0x00, 0xff, b'\n', 0x80][..]),
            ("aB09", &[0xab, 0x09][..]),
        ] {
            let trace = read_access_trace(input(encoded).as_bytes()).unwrap();
            let [TraceRecord::Syscall(call)] = trace.records.as_slice() else {
                panic!("{encoded:?}: {:?}", trace.records);
            };
            assert_eq!(call.out_bytes.as_deref(), Some(expected), "{encoded:?}");
        }
        for encoded in ["€a", "a€", "💥", "é", "0", "zz"] {
            let error = read_access_trace(input(encoded).as_bytes())
                .expect_err("malformed output bytes must be refused");
            assert!(
                matches!(error, AccessTraceError::BadLine { line: 2, .. }),
                "{encoded:?}: {error}"
            );
            assert!(error.to_string().contains("hex"), "{encoded:?}: {error}");
        }
    }

    #[test]
    fn returned_digest_is_fnv1a_over_the_buffers_in_order() {
        assert_eq!(
            returned_digest(RETURNED_DIGEST_SEED, b""),
            RETURNED_DIGEST_SEED
        );
        // The published FNV-1a 64 test vector for "a".
        assert_eq!(
            returned_digest(RETURNED_DIGEST_SEED, b"a"),
            0xaf63_dc4c_8601_ec8c
        );
        let split = returned_digest(returned_digest(RETURNED_DIGEST_SEED, b"he"), b"llo");
        assert_eq!(split, returned_digest(RETURNED_DIGEST_SEED, b"hello"));
        assert_ne!(split, returned_digest(RETURNED_DIGEST_SEED, b"hellp"));
    }

    #[test]
    fn reader_refuses_files_it_cannot_trust() {
        let header = serde_json::to_string(&TraceRecord::Header(TraceHeader::new(0))).unwrap();
        let syscall = serde_json::to_string(&TraceRecord::Syscall(close_record())).unwrap();
        let end = serde_json::to_string(&TraceRecord::End(TraceEnd {
            events: 1,
            records: 1,
        }))
        .unwrap();

        assert!(matches!(
            read_access_trace("".as_bytes()),
            Err(AccessTraceError::Empty)
        ));
        assert!(matches!(
            read_access_trace(format!("{syscall}\n").as_bytes()),
            Err(AccessTraceError::MissingHeader)
        ));
        assert!(matches!(
            read_access_trace(format!("{header}\n{header}\n").as_bytes()),
            Err(AccessTraceError::Misplaced { line: 2 })
        ));
        assert!(matches!(
            read_access_trace(format!("{header}\n{end}\n{syscall}\n").as_bytes()),
            Err(AccessTraceError::Misplaced { line: 3 })
        ));
        assert!(matches!(
            read_access_trace(format!("{header}\nnot json\n").as_bytes()),
            Err(AccessTraceError::BadLine { line: 2, .. })
        ));
        let future_version = ACCESS_TRACE_VERSION + 1;
        let future = header.replace(
            &format!("\"version\":{ACCESS_TRACE_VERSION}"),
            &format!("\"version\":{future_version}"),
        );
        let err = read_access_trace(future.as_bytes()).unwrap_err();
        assert!(matches!(
            err,
            AccessTraceError::Unsupported { version, .. } if version == future_version
        ));
        assert!(err.to_string().contains("record it again"), "{err}");

        let cut = read_access_trace(format!("{header}\n{syscall}\n").as_bytes()).unwrap();
        assert_eq!(cut.end, None, "a trace without an end line was cut short");
        assert_eq!(cut.records.len(), 1);
    }

    /// Version 1 called the pre-delivery observation an actual delivery.
    /// It must be refused, not silently reinterpreted as the new contract.
    #[test]
    fn reader_refuses_version_one_delivery_claims() {
        let old = concat!(
            r#"{"kind":"header","format":"hermit-access-trace","version":1,"from":0}"#,
            "\n",
            r#"{"kind":"signal","event":0,"tid":3,"signo":10,"code":0,"addr":null,"sender":null,"delivery":"handler"}"#,
            "\n",
            r#"{"kind":"end","events":1,"records":1}"#,
            "\n",
        );
        let error = read_access_trace(old.as_bytes()).expect_err("version 1 must be refused");
        assert!(
            matches!(error, AccessTraceError::Unsupported { version: 1, .. }),
            "{error}"
        );
        assert!(error.to_string().contains("record it again"), "{error}");
    }

    #[test]
    fn conflicts_follow_the_per_resource_rules() {
        let use6 = Access::Fd {
            slot: slot(6),
            open_file: Some(file(7, 3)),
            op: FdOp::Use,
        };
        let close6 = Access::Fd {
            slot: slot(6),
            open_file: Some(file(7, 3)),
            op: FdOp::Close,
        };
        let use6_other_table = Access::Fd {
            slot: FdSlot {
                files: FilesId::initial(tid(4)),
                fd: 6,
            },
            open_file: None,
            op: FdOp::Close,
        };
        assert!(!use6.conflicts_with(&use6), "two uses commute");
        assert!(use6.conflicts_with(&close6), "close against use");
        assert!(close6.conflicts_with(&close6), "double close");
        assert!(!close6.conflicts_with(&use6_other_table), "other table");

        let futex = FutexID::private(mm(), 0x1000);
        let wait = Access::Futex {
            futex,
            op: FutexOp::Wait,
        };
        let wake = Access::Futex {
            futex,
            op: FutexOp::Wake,
        };
        assert!(!wait.conflicts_with(&wait));
        assert!(wait.conflicts_with(&wake), "lost wake");

        let read = |start, end| Access::FileContents {
            inode: DetInode::mint(1),
            range: Some(ByteRange { start, end }),
            op: DataOp::Read,
        };
        let write = |range| Access::FileContents {
            inode: DetInode::mint(1),
            range,
            op: DataOp::Write,
        };
        assert!(!read(0, 10).conflicts_with(&read(0, 10)));
        assert!(read(0, 10).conflicts_with(&write(Some(ByteRange { start: 9, end: 12 }))));
        assert!(!read(0, 10).conflicts_with(&write(Some(ByteRange { start: 10, end: 12 }))));
        assert!(read(0, 10).conflicts_with(&write(None)), "unknown range");

        let unmap = Access::Memory {
            mm: mm(),
            range: ByteRange {
                start: 0x1000,
                end: 0x3000,
            },
            op: MemoryOp::Unmap,
        };
        let buffer = |op| Access::Memory {
            mm: mm(),
            range: ByteRange {
                start: 0x2ff0,
                end: 0x3010,
            },
            op,
        };
        assert!(unmap.conflicts_with(&buffer(MemoryOp::SyscallRead)));
        assert!(buffer(MemoryOp::SyscallRead).conflicts_with(&buffer(MemoryOp::SyscallWrite)));
        assert!(!buffer(MemoryOp::SyscallRead).conflicts_with(&buffer(MemoryOp::SyscallRead)));

        let kill = Access::Lifetime {
            target: Task::Process(DetPid::from_raw(4)),
            op: LifetimeOp::Signal { signo: 9 },
        };
        let exit = Access::Lifetime {
            target: Task::Process(DetPid::from_raw(4)),
            op: LifetimeOp::Exit,
        };
        assert!(kill.conflicts_with(&exit));
        assert!(
            !kill.conflicts_with(&wake),
            "different resources never conflict"
        );

        let end = |open_file, id, op| Access::Channel {
            open_file,
            id,
            channel: ChannelKind::StreamSocket,
            op,
        };
        let pair = Some(ChannelId::Pair(file(3, 1)));
        assert!(
            end(file(3, 1), pair, ChannelOp::Write).conflicts_with(&end(
                file(3, 2),
                pair,
                ChannelOp::Read
            )),
            "both ends of one socketpair"
        );
        assert!(
            !end(file(3, 1), None, ChannelOp::Write).conflicts_with(&end(
                file(3, 2),
                None,
                ChannelOp::Read
            )),
            "unrelated sockets"
        );
        let fifo = Some(ChannelId::Inode(DetInode::mint(9)));
        assert!(
            end(file(3, 5), fifo, ChannelOp::Close).conflicts_with(&end(
                file(4, 1),
                fifo,
                ChannelOp::Read
            )),
            "two opens of one FIFO"
        );
    }
}
