/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Detcore's side of the access trace (`--record-access-trace-to`); the
//! record types are `detcore_model::access_trace`.
//!
//! The trace is made in three places:
//! - The guest thread notes the descriptor slots its syscall changes
//!   (`TracedCall`), and snapshots the descriptors the call names right
//!   after its prehook event (`FdInfo`). At the posthook, `accesses`, a
//!   pure function of `CallFacts`, turns these, the arguments and the
//!   result into the record's accesses.
//! - The record rides to the scheduler as an optional payload of the
//!   prehook, posthook and signal-delivery `TraceSchedEvent` requests the
//!   thread sends anyway ([`AccessPayload`]).
//! - The scheduler, which numbers the recorded events, completes each record
//!   and writes it ([`AccessTraceWriter`]).
//!
//! Nothing here sends a scheduler request, charges virtual time or changes
//! state a scheduling decision reads: the trace observes the run.

use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::io::BufWriter;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use detcore_model::access_trace::ACCESS_TRACE_DEFAULT_MAX_BYTES;
use detcore_model::access_trace::Access;
use detcore_model::access_trace::ByteRange;
use detcore_model::access_trace::ChannelId;
use detcore_model::access_trace::ChannelKind;
use detcore_model::access_trace::ChannelOp;
use detcore_model::access_trace::CursorOp;
use detcore_model::access_trace::DataOp;
use detcore_model::access_trace::EdgeKind;
use detcore_model::access_trace::EdgeRecord;
use detcore_model::access_trace::EventIndex;
use detcore_model::access_trace::EventRef;
use detcore_model::access_trace::FdOp;
use detcore_model::access_trace::FutexOp;
use detcore_model::access_trace::LifetimeOp;
use detcore_model::access_trace::MemoryOp;
use detcore_model::access_trace::ObservedSignalDisposition;
use detcore_model::access_trace::RETURNED_DIGEST_CAP;
use detcore_model::access_trace::RETURNED_DIGEST_SEED;
use detcore_model::access_trace::ReturnedBytes;
use detcore_model::access_trace::SignalRecord;
use detcore_model::access_trace::SignalSender;
use detcore_model::access_trace::SyncObject;
use detcore_model::access_trace::SyscallRecord;
use detcore_model::access_trace::Task;
use detcore_model::access_trace::TimerId;
use detcore_model::access_trace::TimerOp;
use detcore_model::access_trace::TraceEnd;
use detcore_model::access_trace::TraceHeader;
use detcore_model::access_trace::TraceRecord;
use detcore_model::access_trace::returned_digest;
use reverie::syscalls::Addr;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Sysno;
use serde::Deserialize;
use serde::Serialize;
use tracing::warn;

use crate::fd::DetFd;
use crate::fd::FdType;
use crate::resources::ResourceID;
use crate::types::DetInode;
use crate::types::DetPid;
use crate::types::DetTid;
use crate::types::FdSlot;
use crate::types::FilesId;
use crate::types::FutexID;
use crate::types::MmId;
use crate::types::Op;
use crate::types::OpenFileId;
use crate::types::SchedEvent;
use crate::types::SyscallPhase;

// ---------------------------------------------------------------------------
// The payload a traced schedule event carries.

/// What a traced schedule event carries to the scheduler, beside the event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccessPayload {
    /// At a syscall's prehook event: what the registers say.
    SyscallEntry(SyscallEntry),
    /// At its posthook event: the outcome.
    SyscallExit(Box<SyscallOutcome>),
    /// At a signal's `SignalReceived` event.
    Signal(SignalNote),
}

/// A syscall as its prehook sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyscallEntry {
    /// The calling thread's process.
    pub pid: DetPid,
    /// The thread's syscall count including this call.
    pub nth: u64,
    /// The syscall.
    pub sysno: Sysno,
    /// Raw argument registers.
    pub args: [u64; 6],
    /// What a call that may never return (exit, exit_group, execve) does to
    /// its thread or process (`entry_accesses`); its posthook, if it comes,
    /// replaces them. JSON, as [`SyscallOutcome::accesses`].
    #[serde(with = "accesses_as_json")]
    pub accesses: Vec<Access>,
}

/// The accesses a call that may not reach its posthook is known for at its
/// prehook: exit ends its thread, exit_group its process, and execve
/// replaces its process's image.
pub(crate) fn entry_accesses(sysno: Sysno, pid: DetPid, tid: DetTid) -> Vec<Access> {
    let access = |target, op| vec![Access::Lifetime { target, op }];
    match sysno {
        Sysno::exit => access(Task::Thread(tid), LifetimeOp::Exit),
        Sysno::exit_group => access(Task::Process(pid), LifetimeOp::Exit),
        Sysno::execve | Sysno::execveat => access(Task::Process(pid), LifetimeOp::Exec),
        _ => Vec::new(),
    }
}

/// A syscall's outcome, as its posthook sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyscallOutcome {
    /// The value the guest receives: the result or the negated errno.
    pub result: i64,
    /// What `accesses` found. Carried as JSON, the trace's own encoding:
    /// the record types use tagged enums and skipped fields, which RPC's
    /// positional bincode cannot carry.
    #[serde(with = "accesses_as_json")]
    pub accesses: Vec<Access>,
    /// The bytes returned through guest buffers.
    pub returned: Option<ReturnedBytes>,
    /// The first bytes of a write to fd 1 or 2.
    pub out_bytes: Option<Vec<u8>>,
}

/// [`SyscallOutcome::accesses`] as a JSON string.
mod accesses_as_json {
    use detcore_model::access_trace::Access;
    use serde::Deserialize;
    use serde::Deserializer;
    use serde::Serializer;
    use serde::de::Error as _;
    use serde::ser::Error as _;

    pub fn serialize<S: Serializer>(accesses: &[Access], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&serde_json::to_string(accesses).map_err(S::Error::custom)?)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Access>, D::Error> {
        serde_json::from_str(&String::deserialize(d)?).map_err(D::Error::custom)
    }
}

/// A signal event and the disposition observed before submitting it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignalNote {
    /// Signal number.
    pub signo: i32,
    /// `si_code`.
    pub code: i32,
    /// `si_addr` of a kernel-generated fault signal.
    pub addr: Option<u64>,
    /// `si_pid` of a signal sent by kill, sigqueue, tkill or tgkill.
    pub sender: Option<SignalSender>,
    /// `si_tid` of a POSIX timer's expiry (`SI_TIMER`): the timer's id.
    pub timer: Option<i32>,
    /// Installed disposition sampled at the event, not its eventual outcome.
    pub observed_disposition: ObservedSignalDisposition,
}

/// Read the fields the trace keeps from a raw `siginfo_t`
/// (`include/uapi/asm-generic/siginfo.h`): `si_signo` at byte 0, `si_code`
/// at byte 8, and the union at byte 16, which holds `si_addr` for a fault and
/// `si_pid` for a user-sent signal.
pub(crate) fn signal_note(
    signo: i32,
    siginfo: Option<&[u8]>,
    observed_disposition: ObservedSignalDisposition,
) -> SignalNote {
    let field = |info: &[u8], at: usize, len: usize| info.get(at..at + len).map(<[u8]>::to_vec);
    let int = |info: &[u8], at: usize| {
        field(info, at, 4).map(|b| i32::from_ne_bytes(b.try_into().expect("four bytes")))
    };
    let Some(info) = siginfo else {
        return SignalNote {
            signo,
            code: 0,
            addr: None,
            sender: None,
            timer: None,
            observed_disposition,
        };
    };
    let code = int(info, 8).unwrap_or(0);
    let fault = matches!(
        signo,
        libc::SIGSEGV | libc::SIGBUS | libc::SIGILL | libc::SIGFPE | libc::SIGTRAP
    );
    // A positive si_code is the kernel's (`si_fromuser`, kernel/signal.c).
    let addr = (code > 0 && fault)
        .then(|| field(info, 16, 8))
        .flatten()
        .map(|b| u64::from_ne_bytes(b.try_into().expect("eight bytes")));
    // Only kill (SI_USER), sigqueue (SI_QUEUE) and tkill/tgkill (SI_TKILL)
    // put the sender's pid there; a timer (SI_TIMER) puts its timer id.
    let sender = matches!(code, libc::SI_USER | libc::SI_QUEUE | libc::SI_TKILL)
        .then(|| int(info, 16))
        .flatten()
        .map(|pid| SignalSender {
            pid: DetPid::from_raw(pid),
            tid: None,
        });
    let timer = (code == libc::SI_TIMER).then(|| int(info, 16)).flatten();
    SignalNote {
        signo,
        code,
        addr,
        sender,
        timer,
        observed_disposition,
    }
}

// ---------------------------------------------------------------------------
// The guest thread's side.

/// What the trace needs to know of one descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FdInfo {
    pub(crate) open_file: OpenFileId,
    pub(crate) ty: FdType,
    pub(crate) inode: Option<DetInode>,
    pub(crate) fifo: bool,
    pub(crate) pidfd_target: Option<DetPid>,
}

impl FdInfo {
    pub(crate) fn of(detfd: &DetFd) -> Self {
        let inode = match detfd.resource() {
            Some(ResourceID::FileContents(inode)) => Some(inode),
            _ => None,
        };
        let fifo = detfd
            .stat()
            .is_some_and(|stat| stat.mode & libc::S_IFMT == libc::S_IFIFO);
        FdInfo {
            open_file: detfd.open_file_id(),
            ty: detfd.ty(),
            inode,
            fifo,
            pidfd_target: detfd.pidfd_target(),
        }
    }

    fn channel(&self) -> Option<ChannelKind> {
        match self.ty {
            FdType::Pipe => Some(ChannelKind::Pipe),
            FdType::Socket => Some(ChannelKind::Socket),
            FdType::Eventfd => Some(ChannelKind::Eventfd),
            FdType::Regular if self.fifo => Some(ChannelKind::Fifo),
            _ => None,
        }
    }

    fn channel_id(&self) -> Option<ChannelId> {
        self.fifo
            .then_some(self.inode.map(ChannelId::Inode))
            .flatten()
    }
}

/// One change the running syscall made to a descriptor slot, as Detcore's
/// descriptor table applied it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FdNote {
    pub(crate) slot: FdSlot,
    /// The open file the slot held before a close, or holds after an open or
    /// a dup.
    pub(crate) open_file: Option<OpenFileId>,
    pub(crate) op: FdOp,
    /// For a close: what the slot named, so a close the call's arguments do
    /// not name (close_range, a dup2 onto an open number) still reaches the
    /// channel behind it.
    pub(crate) released: Option<FdInfo>,
}

/// What a traced syscall has gathered between its prehook and its posthook.
#[derive(Debug, Default)]
pub(crate) struct CallState {
    /// The descriptors the call names, right after its prehook event.
    pub(crate) fds: BTreeMap<i32, FdInfo>,
    /// The slot changes the call made, in order.
    pub(crate) notes: Vec<FdNote>,
}

/// A thread's traced syscall in progress. Inactive unless the run records
/// an access trace, and between a traced call's posthook and the next
/// prehook. Descriptor-table methods take `&ThreadState`, hence the mutex;
/// only the owning thread uses it.
#[derive(Debug, Default)]
pub(crate) struct TracedCall(Mutex<Option<CallState>>);

impl Clone for TracedCall {
    /// A copied thread state (a new thread, a migration) has no call in
    /// progress.
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl TracedCall {
    /// Start a traced call whose named descriptors were `fds`.
    pub(crate) fn begin(&self, fds: BTreeMap<i32, FdInfo>) {
        *self.0.lock().expect("traced call mutex poisoned") = Some(CallState {
            fds,
            notes: Vec::new(),
        });
    }

    /// Whether a traced call is in progress, so a slot change is noted.
    pub(crate) fn is_active(&self) -> bool {
        self.0.lock().expect("traced call mutex poisoned").is_some()
    }

    /// Note a slot change of the call in progress, if there is one.
    pub(crate) fn note(&self, note: FdNote) {
        if let Some(state) = self.0.lock().expect("traced call mutex poisoned").as_mut() {
            state.notes.push(note);
        }
    }

    /// End the call in progress and return what it gathered.
    pub(crate) fn take(&self) -> Option<CallState> {
        self.0.lock().expect("traced call mutex poisoned").take()
    }
}

/// Values a completed call wrote to guest memory that name resources.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Written {
    /// timer_create's new timer id.
    pub(crate) timer_id: Option<i32>,
    /// pipe, pipe2 or socketpair's two descriptors, in array order.
    pub(crate) fd_pair: Option<[i32; 2]>,
    /// waitid's `si_pid`.
    pub(crate) waited_pid: Option<i32>,
    /// clone3's `clone_args.flags`.
    pub(crate) clone3_flags: Option<u64>,
}

/// Where, in guest memory, [`Written`] values are, for a call that succeeded
/// with `result`: the address and the field to read there.
pub(crate) fn written_locations(
    sysno: Sysno,
    args: [u64; 6],
    result: i64,
) -> Vec<(u64, WrittenField)> {
    if result < 0 {
        return Vec::new();
    }
    match sysno {
        Sysno::timer_create => vec![(args[2], WrittenField::TimerId)],
        // aarch64 has only pipe2 (include/uapi/asm-generic/unistd.h).
        #[cfg(target_arch = "x86_64")]
        Sysno::pipe => vec![(args[0], WrittenField::FdPair)],
        Sysno::pipe2 => vec![(args[0], WrittenField::FdPair)],
        Sysno::socketpair => vec![(args[3], WrittenField::FdPair)],
        // `si_pid` is the union's first member, at byte 16 of the siginfo.
        Sysno::waitid if args[2] != 0 => vec![(args[2] + 16, WrittenField::WaitedPid)],
        Sysno::clone3 => vec![(args[0], WrittenField::Clone3Flags)],
        _ => Vec::new(),
    }
}

/// Read the [`Written`] values of a call that returned `result`. A value the
/// guest's memory no longer holds readable is left out.
pub(crate) fn read_written<M: MemoryAccess>(
    memory: &M,
    sysno: Sysno,
    args: [u64; 6],
    result: i64,
) -> Written {
    let mut written = Written::default();
    for (addr, field) in written_locations(sysno, args, result) {
        let int =
            |at: u64| Addr::<i32>::from_raw(at as usize).and_then(|a| memory.read_value(a).ok());
        match field {
            WrittenField::TimerId => written.timer_id = int(addr),
            WrittenField::WaitedPid => written.waited_pid = int(addr),
            WrittenField::FdPair => {
                written.fd_pair = int(addr).zip(int(addr + 4)).map(|(a, b)| [a, b]);
            }
            WrittenField::Clone3Flags => {
                written.clone3_flags =
                    Addr::<u64>::from_raw(addr as usize).and_then(|a| memory.read_value(a).ok());
            }
        }
    }
    written
}

/// One value [`written_locations`] names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WrittenField {
    TimerId,
    FdPair,
    WaitedPid,
    Clone3Flags,
}

/// How a call uses one descriptor it names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Any use the trace does not break down further.
    Use,
    Read,
    ReadAt(Option<u64>),
    Write,
    WriteAt(Option<u64>),
    Seek,
    Truncate,
    Shutdown,
    TimerSet,
    TimerGet,
}

/// The descriptors a call uses as operands, each with its role. A positional
/// offset of -1 (preadv2, pwritev2) means the cursor.
fn operand_fds(sysno: Sysno, args: [u64; 6]) -> Vec<(i32, Role)> {
    use Sysno::*;
    let fd = |i: usize| args[i] as i32;
    let at = |i: usize| (args[i] as i64 >= 0).then_some(args[i]);
    match sysno {
        read | readv | recvfrom | recvmsg | recvmmsg => vec![(fd(0), Role::Read)],
        pread64 | preadv => vec![(fd(0), Role::ReadAt(Some(args[3])))],
        preadv2 => vec![(fd(0), at(3).map_or(Role::Read, |o| Role::ReadAt(Some(o))))],
        write | writev | sendto | sendmsg | sendmmsg => vec![(fd(0), Role::Write)],
        pwrite64 | pwritev => vec![(fd(0), Role::WriteAt(Some(args[3])))],
        pwritev2 => vec![(fd(0), at(3).map_or(Role::Write, |o| Role::WriteAt(Some(o))))],
        lseek => vec![(fd(0), Role::Seek)],
        ftruncate | fallocate => vec![(fd(0), Role::Truncate)],
        shutdown => vec![(fd(0), Role::Shutdown)],
        timerfd_settime => vec![(fd(0), Role::TimerSet)],
        timerfd_gettime => vec![(fd(0), Role::TimerGet)],
        sendfile => vec![(fd(0), Role::Write), (fd(1), Role::Read)],
        splice | copy_file_range => vec![(fd(0), Role::ReadAt(None)), (fd(2), Role::WriteAt(None))],
        tee => vec![(fd(0), Role::Read), (fd(1), Role::Write)],
        epoll_ctl => vec![(fd(0), Role::Use), (fd(2), Role::Use)],
        mmap if args[3] & libc::MAP_ANONYMOUS as u64 == 0 => vec![(fd(4), Role::Use)],
        signalfd4 if fd(0) >= 0 => vec![(fd(0), Role::Use)],
        fstat | fsync | fdatasync | fchmod | fchown | flock | fcntl | ioctl | getdents64
        | fadvise64 | fstatfs | fchdir | syncfs | accept | accept4 | connect | bind | listen
        | getsockname | getpeername | setsockopt | getsockopt | epoll_pwait | epoll_pwait2
        | inotify_add_watch | inotify_rm_watch | fgetxattr | fsetxattr | flistxattr
        | fremovexattr | pidfd_send_signal | pidfd_getfd | dup | dup3 => {
            vec![(fd(0), Role::Use)]
        }
        // The legacy calls only x86_64 has. aarch64's generic table
        // (include/uapi/asm-generic/unistd.h) has getdents64, epoll_pwait and
        // dup3 above instead, and names sync_file_range sync_file_range2.
        #[cfg(target_arch = "x86_64")]
        getdents | sync_file_range | epoll_wait | dup2 => vec![(fd(0), Role::Use)],
        #[cfg(target_arch = "aarch64")]
        sync_file_range2 => vec![(fd(0), Role::Use)],
        _ => Vec::new(),
    }
}

/// Every descriptor a call names, whose state before the call the trace
/// needs: its operands, the descriptor close releases, and the one dup2 or
/// dup3 replaces.
pub(crate) fn named_fds(sysno: Sysno, args: [u64; 6]) -> Vec<i32> {
    let mut fds: Vec<i32> = operand_fds(sysno, args)
        .into_iter()
        .map(|(fd, _)| fd)
        .collect();
    match sysno {
        Sysno::close => fds.push(args[0] as i32),
        #[cfg(target_arch = "x86_64")]
        Sysno::dup2 => fds.push(args[1] as i32),
        Sysno::dup3 => fds.push(args[1] as i32),
        _ => {}
    }
    fds.retain(|fd| *fd >= 0);
    fds.sort_unstable();
    fds.dedup();
    fds
}

/// What a call that reads a clock or a timer returns, for its
/// [`ReturnedBytes`]: the result itself when it is the time (`time`,
/// `alarm`, `times`), and the guest buffers the kernel or Detcore filled.
/// Buffers are x86-64 sizes: timespec and timeval 16 bytes, itimerspec and
/// itimerval 32, timezone 8, tms 32. A remaining-time buffer counts only
/// when a sleep was interrupted, the one case Linux writes it: do_nanosleep
/// copies it out (`nanosleep_copyout`, kernel/time/hrtimer.c:2072 and
/// :2121 at v6.16) and returns -ERESTART_RESTARTBLOCK, which the posthook
/// sees as is when the call will restart and as -EINTR when a handler ran.
/// An absolute clock_nanosleep never writes it; an old-value
/// buffer only when the guest passed one.
pub(crate) fn time_outputs(
    sysno: Sysno,
    args: [u64; 6],
    result: i64,
) -> (Option<i64>, Vec<ByteRange>) {
    use Sysno::*;
    const TIMESPEC: u64 = 16;
    const TIMEVAL: u64 = 16;
    const ITIMER: u64 = 32;
    const TIMEZONE: u64 = 8;
    const TMS: u64 = 32;
    let at = |addr: u64, len: u64| {
        (addr != 0).then_some(ByteRange {
            start: addr,
            end: addr.saturating_add(len),
        })
    };
    let ok = result >= 0;
    /// Linux's restart code for an interrupted relative sleep
    /// (include/linux/errno.h).
    const ERESTART_RESTARTBLOCK: i64 = 516;
    let interrupted = result == -(libc::EINTR as i64) || result == -ERESTART_RESTARTBLOCK;
    let buffers: Vec<Option<ByteRange>> = match sysno {
        clock_gettime | clock_getres if ok => vec![at(args[1], TIMESPEC)],
        gettimeofday if ok => vec![at(args[0], TIMEVAL), at(args[1], TIMEZONE)],
        nanosleep if interrupted => vec![at(args[1], TIMESPEC)],
        clock_nanosleep if interrupted && args[1] & libc::TIMER_ABSTIME as u64 == 0 => {
            vec![at(args[3], TIMESPEC)]
        }
        timerfd_gettime | timer_gettime | getitimer if ok => vec![at(args[1], ITIMER)],
        timerfd_settime | timer_settime if ok => vec![at(args[3], ITIMER)],
        setitimer if ok => vec![at(args[2], ITIMER)],
        times if ok => vec![at(args[0], TMS)],
        _ => Vec::new(),
    };
    // time and alarm are x86_64-only; aarch64's C library builds them on
    // clock_gettime and setitimer, whose outputs are above.
    #[cfg(target_arch = "x86_64")]
    let returns_time = matches!(sysno, time | alarm | times);
    #[cfg(not(target_arch = "x86_64"))]
    let returns_time = sysno == times;
    let value = (ok && returns_time).then_some(result);
    (value, buffers.into_iter().flatten().collect())
}

/// Everything [`accesses`] reads.
pub(crate) struct CallFacts<'a> {
    pub(crate) sysno: Sysno,
    pub(crate) args: [u64; 6],
    pub(crate) result: i64,
    pub(crate) pid: DetPid,
    pub(crate) tid: DetTid,
    pub(crate) files: FilesId,
    pub(crate) mm: MmId,
    /// The descriptors [`named_fds`] lists, right after the prehook event.
    pub(crate) fds: &'a BTreeMap<i32, FdInfo>,
    /// The slot changes the call made.
    pub(crate) notes: &'a [FdNote],
    /// The guest buffers the call moved, and which way.
    pub(crate) buffers: &'a [(MemoryOp, ByteRange)],
    pub(crate) written: &'a Written,
    /// The thread's key for a futex word (`ThreadState::futex_id`).
    pub(crate) futex_id: &'a dyn Fn(u64, bool) -> FutexID,
}

const PAGE: u64 = 4096;

fn page_range(start: u64, len: u64) -> ByteRange {
    ByteRange {
        start,
        end: start.saturating_add(len.div_ceil(PAGE).saturating_mul(PAGE)),
    }
}

/// The record's accesses: section 4 of the M5 design applied to one call.
pub(crate) fn accesses(f: &CallFacts<'_>) -> Vec<Access> {
    let mut out = Vec::new();
    let ok = f.result >= 0;
    let slot = |fd: i32| FdSlot { files: f.files, fd };

    for (fd, role) in operand_fds(f.sysno, f.args) {
        if fd < 0 {
            continue;
        }
        let info = f.fds.get(&fd);
        out.push(Access::Fd {
            slot: slot(fd),
            open_file: info.map(|info| info.open_file),
            op: FdOp::Use,
        });
        if let Some(info) = info {
            data_accesses(f, info, role, &mut out);
        }
    }

    // A close that released nothing Detcore models (EBADF, a double close)
    // is still a close of the slot.
    if f.sysno == Sysno::close {
        let fd = f.args[0] as i32;
        let noted = f
            .notes
            .iter()
            .any(|note| note.slot.fd == fd && note.op == FdOp::Close);
        if !noted {
            out.push(Access::Fd {
                slot: slot(fd),
                open_file: f.fds.get(&fd).map(|info| info.open_file),
                op: FdOp::Close,
            });
        }
    }

    for note in ordered_notes(f) {
        out.push(Access::Fd {
            slot: note.slot,
            open_file: note.open_file,
            op: note.op.clone(),
        });
        if note.op == FdOp::Close
            && let Some(info) = &note.released
            && let Some(channel) = info.channel()
        {
            out.push(Access::Channel {
                open_file: info.open_file,
                id: info.channel_id(),
                channel,
                op: ChannelOp::Close,
            });
        }
    }

    futex_accesses(f, &mut out);
    if ok {
        memory_accesses(f, &mut out);
    }
    timer_accesses(f, &mut out);
    lifetime_accesses(f, &mut out);
    for (op, range) in f.buffers {
        out.push(Access::Memory {
            mm: f.mm,
            range: *range,
            op: *op,
        });
    }
    out
}

/// The notes in record order: a pipe's or socketpair's two opens in the
/// order of its descriptor array, so the first is `fds[0]`.
fn ordered_notes<'a>(f: &CallFacts<'a>) -> Vec<&'a FdNote> {
    let mut notes: Vec<&FdNote> = f.notes.iter().collect();
    if let Some(pair) = f.written.fd_pair {
        notes.sort_by_key(|note| pair.iter().position(|fd| *fd == note.slot.fd));
    }
    notes
}

fn data_accesses(f: &CallFacts<'_>, info: &FdInfo, role: Role, out: &mut Vec<Access>) {
    if let Some(channel) = info.channel() {
        let op = match role {
            Role::Read | Role::ReadAt(_) => ChannelOp::Read,
            Role::Write | Role::WriteAt(_) => ChannelOp::Write,
            Role::Shutdown => ChannelOp::Shutdown,
            _ => return,
        };
        out.push(Access::Channel {
            open_file: info.open_file,
            id: info.channel_id(),
            channel,
            op,
        });
        return;
    }
    if info.ty == FdType::Timerfd {
        let op = match role {
            Role::Read => TimerOp::Read,
            Role::TimerSet => TimerOp::Set,
            Role::TimerGet => TimerOp::Get,
            _ => return,
        };
        out.push(Access::Timer {
            timer: TimerId::Fd {
                open_file: info.open_file,
            },
            op,
        });
        return;
    }
    if !matches!(info.ty, FdType::Regular | FdType::Memfd) {
        return;
    }
    let cursor = |op| Access::OpenFile {
        open_file: info.open_file,
        op,
    };
    // A positional range covers what the call moved, or, when it moved
    // nothing (end of file, an error), the bytes it asked for.
    let range = |offset: Option<u64>| {
        offset.map(|start| {
            let moved = u64::try_from(f.result).unwrap_or(0);
            let len = if moved > 0 {
                moved
            } else if matches!(f.sysno, Sysno::pread64 | Sysno::pwrite64) {
                f.args[2]
            } else {
                0
            };
            ByteRange {
                start,
                end: start.saturating_add(len),
            }
        })
    };
    let contents = |range: Option<ByteRange>, op| {
        info.inode
            .map(|inode| Access::FileContents { inode, range, op })
    };
    match role {
        Role::Read => {
            out.push(cursor(CursorOp::Read));
            out.extend(contents(None, DataOp::Read));
        }
        Role::Write => {
            out.push(cursor(CursorOp::Write));
            out.extend(contents(None, DataOp::Write));
        }
        Role::ReadAt(offset) => out.extend(contents(range(offset), DataOp::Read)),
        Role::WriteAt(offset) => out.extend(contents(range(offset), DataOp::Write)),
        Role::Seek => out.push(cursor(CursorOp::Seek)),
        Role::Truncate => out.extend(contents(None, DataOp::Write)),
        Role::Use | Role::Shutdown | Role::TimerSet | Role::TimerGet => {}
    }
}

/// `FUTEX_LOCK_PI2`, which the libc crate may not name.
const FUTEX_LOCK_PI2: i32 = 13;

fn futex_accesses(f: &CallFacts<'_>, out: &mut Vec<Access>) {
    if f.sysno != Sysno::futex {
        return;
    }
    let op = f.args[1] as i32;
    let private = op & libc::FUTEX_PRIVATE_FLAG != 0;
    let cmd = op & !(libc::FUTEX_PRIVATE_FLAG | libc::FUTEX_CLOCK_REALTIME);
    let (uaddr, uaddr2) = (f.args[0], f.args[4]);
    let words: &[(u64, FutexOp)] = match cmd {
        libc::FUTEX_WAIT | libc::FUTEX_WAIT_BITSET | libc::FUTEX_WAIT_REQUEUE_PI => {
            &[(uaddr, FutexOp::Wait)]
        }
        libc::FUTEX_WAKE | libc::FUTEX_WAKE_BITSET => &[(uaddr, FutexOp::Wake)],
        libc::FUTEX_REQUEUE | libc::FUTEX_CMP_REQUEUE | libc::FUTEX_CMP_REQUEUE_PI => {
            &[(uaddr, FutexOp::Wake), (uaddr2, FutexOp::RequeueTarget)]
        }
        libc::FUTEX_WAKE_OP => &[(uaddr, FutexOp::Wake), (uaddr2, FutexOp::ModifyWake)],
        libc::FUTEX_LOCK_PI | libc::FUTEX_TRYLOCK_PI | FUTEX_LOCK_PI2 => &[(uaddr, FutexOp::Lock)],
        libc::FUTEX_UNLOCK_PI => &[(uaddr, FutexOp::Unlock)],
        _ => &[],
    };
    for (addr, op) in words {
        out.push(Access::Futex {
            futex: (f.futex_id)(*addr, private),
            op: *op,
        });
    }
}

fn memory_accesses(f: &CallFacts<'_>, out: &mut Vec<Access>) {
    let a = f.args;
    let mut push = |range: ByteRange, op| {
        out.push(Access::Memory {
            mm: f.mm,
            range,
            op,
        })
    };
    match f.sysno {
        Sysno::mmap => push(page_range(f.result as u64, a[1]), MemoryOp::Map),
        Sysno::munmap => push(page_range(a[0], a[1]), MemoryOp::Unmap),
        Sysno::mprotect | Sysno::pkey_mprotect => push(page_range(a[0], a[1]), MemoryOp::Protect),
        Sysno::madvise => push(page_range(a[0], a[1]), MemoryOp::Advise),
        Sysno::mremap => {
            push(page_range(a[0], a[1]), MemoryOp::Unmap);
            push(page_range(f.result as u64, a[2]), MemoryOp::Map);
        }
        _ => {}
    }
}

fn timer_accesses(f: &CallFacts<'_>, out: &mut Vec<Access>) {
    let posix = |id: i32| TimerId::Posix { process: f.pid, id };
    let interval = |which: i32| TimerId::Interval {
        process: f.pid,
        which,
    };
    let ok = f.result >= 0;
    let access = match f.sysno {
        Sysno::timer_create if ok => f.written.timer_id.map(|id| (posix(id), TimerOp::Create)),
        Sysno::timer_settime => Some((posix(f.args[0] as i32), TimerOp::Set)),
        Sysno::timer_gettime | Sysno::timer_getoverrun => {
            Some((posix(f.args[0] as i32), TimerOp::Get))
        }
        Sysno::timer_delete => Some((posix(f.args[0] as i32), TimerOp::Delete)),
        Sysno::setitimer => Some((interval(f.args[0] as i32), TimerOp::Set)),
        Sysno::getitimer => Some((interval(f.args[0] as i32), TimerOp::Get)),
        // x86_64 only; on aarch64 alarm(3) is a setitimer call.
        #[cfg(target_arch = "x86_64")]
        Sysno::alarm => Some((interval(libc::ITIMER_REAL), TimerOp::Set)),
        Sysno::timerfd_create if ok => f
            .notes
            .iter()
            .find(|note| note.slot.fd as i64 == f.result && matches!(note.op, FdOp::Open { .. }))
            .and_then(|note| note.open_file)
            .map(|open_file| (TimerId::Fd { open_file }, TimerOp::Create)),
        _ => None,
    };
    if let Some((timer, op)) = access {
        out.push(Access::Timer { timer, op });
    }
}

fn lifetime_accesses(f: &CallFacts<'_>, out: &mut Vec<Access>) {
    let a = f.args;
    let raw = |i: usize| a[i] as i32;
    let created = |flags: u64| {
        (f.result > 0).then(|| {
            let child = DetPid::from_raw(f.result as i32);
            if flags & libc::CLONE_THREAD as u64 != 0 {
                Task::Thread(child)
            } else {
                Task::Process(child)
            }
        })
    };
    let signal =
        |target: Option<Task>, signo: i32| target.map(|t| (t, LifetimeOp::Signal { signo }));
    let access = match f.sysno {
        Sysno::clone => created(a[0]).map(|t| (t, LifetimeOp::Create)),
        Sysno::clone3 => f
            .written
            .clone3_flags
            .and_then(created)
            .map(|t| (t, LifetimeOp::Create)),
        // x86_64 only; on aarch64 fork(2) and vfork(2) are clone calls
        // with SIGCHLD (and CLONE_VM | CLONE_VFORK) as the flags, above.
        #[cfg(target_arch = "x86_64")]
        Sysno::fork | Sysno::vfork => created(0).map(|t| (t, LifetimeOp::Create)),
        Sysno::exit => Some((Task::Thread(f.tid), LifetimeOp::Exit)),
        Sysno::exit_group => Some((Task::Process(f.pid), LifetimeOp::Exit)),
        Sysno::execve | Sysno::execveat => Some((Task::Process(f.pid), LifetimeOp::Exec)),
        Sysno::kill => {
            let target = match raw(0) {
                pid if pid > 0 => Some(Task::Process(DetPid::from_raw(pid))),
                // The caller's own group, which the writer resolves.
                0 => Some(Task::ProcessGroup(OWN_PROCESS_GROUP)),
                -1 => None,
                pid => Some(Task::ProcessGroup(DetPid::from_raw(pid.wrapping_neg()))),
            };
            signal(target, raw(1))
        }
        Sysno::tkill => signal(Some(Task::Thread(DetPid::from_raw(raw(0)))), raw(1)),
        Sysno::tgkill | Sysno::rt_tgsigqueueinfo => {
            signal(Some(Task::Thread(DetPid::from_raw(raw(1)))), raw(2))
        }
        Sysno::rt_sigqueueinfo => signal(Some(Task::Process(DetPid::from_raw(raw(0)))), raw(1)),
        Sysno::pidfd_send_signal => signal(
            f.fds
                .get(&raw(0))
                .and_then(|info| info.pidfd_target)
                .map(Task::Process),
            raw(1),
        ),
        Sysno::wait4 if f.result > 0 => Some((
            Task::Process(DetPid::from_raw(f.result as i32)),
            LifetimeOp::Wait,
        )),
        Sysno::waitid if f.result == 0 => f
            .written
            .waited_pid
            .filter(|pid| *pid > 0)
            .map(|pid| (Task::Process(DetPid::from_raw(pid)), LifetimeOp::Wait)),
        _ => None,
    };
    if let Some((target, op)) = access {
        out.push(Access::Lifetime { target, op });
    }
}

/// kill(0)'s target as the guest thread writes it: the caller's own process
/// group, which only the scheduler's process table knows.
pub(crate) const OWN_PROCESS_GROUP: DetPid = DetPid::from_raw(0);

/// The bytes returned through guest buffers, from the buffers' contents in
/// order. `read` reads at most `want` bytes of a buffer and fails as the
/// guest read would. For a call whose return value is output too (`time`, `alarm`,
/// `times`), `value`'s eight little-endian bytes come first.
pub(crate) fn summarize_output<E>(
    value: Option<i64>,
    buffers: &[ByteRange],
    mut read: impl FnMut(u64, usize) -> Result<Vec<u8>, E>,
) -> Option<ReturnedBytes> {
    let value = value.map(i64::to_le_bytes);
    let value_len = value.map_or(0, |bytes| bytes.len() as u64);
    let len: u64 = value_len + buffers.iter().map(|b| b.end - b.start).sum::<u64>();
    if len == 0 {
        return None;
    }
    let mut digest = value.map_or(RETURNED_DIGEST_SEED, |bytes| {
        returned_digest(RETURNED_DIGEST_SEED, &bytes)
    });
    let mut left = RETURNED_DIGEST_CAP - value_len;
    for buffer in buffers {
        if left == 0 {
            break;
        }
        let want = (buffer.end - buffer.start).min(left);
        let bytes = read(buffer.start, want as usize).ok()?;
        digest = returned_digest(digest, &bytes);
        left -= want;
    }
    Some(ReturnedBytes {
        len,
        digest,
        truncated: len > RETURNED_DIGEST_CAP,
    })
}

/// Whether `sysno` creates a thread or process and returns its tid: clone,
/// clone3, and x86_64's legacy fork and vfork. aarch64's generic table
/// (include/uapi/asm-generic/unistd.h) has no fork or vfork; its C library
/// calls clone with SIGCHLD (and CLONE_VM | CLONE_VFORK) instead.
fn creates_a_task(sysno: Sysno) -> bool {
    match sysno {
        Sysno::clone | Sysno::clone3 => true,
        #[cfg(target_arch = "x86_64")]
        Sysno::fork | Sysno::vfork => true,
        _ => false,
    }
}

/// Whether `sysno` returns two connected descriptors: pipe2, socketpair,
/// and x86_64's legacy pipe.
fn creates_a_channel_pair(sysno: Sysno) -> bool {
    match sysno {
        Sysno::pipe2 | Sysno::socketpair => true,
        #[cfg(target_arch = "x86_64")]
        Sysno::pipe => true,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// The scheduler's side.

/// Writes the trace as the scheduler records events. It lives in the
/// scheduler, so records are numbered and ordered under the scheduler's lock.
pub struct AccessTraceWriter {
    /// `None` once a write failed: the trace then ends without its end line,
    /// which marks it incomplete.
    out: Option<Box<dyn Write + Send>>,
    from: EventIndex,
    records: u64,
    /// `--access-trace-max-records`: the records to write at most.
    max_records: Option<u64>,
    /// The bytes written so far, the header's included.
    bytes: u64,
    /// `--access-trace-max-bytes`, or [`ACCESS_TRACE_DEFAULT_MAX_BYTES`]
    /// without it: the bytes of header and records to write at most.
    max_bytes: u64,
    /// Whether `max_bytes` is the default rather than the option's, for the
    /// warning that says the trace stopped.
    default_max_bytes: bool,
    /// Whether a record was left out for a cap; the trace then ends
    /// without its end line, which marks it cut short.
    capped: bool,
    /// The trace's path, for the warnings that say why it ends early.
    name: String,
    /// Each thread's syscall between its prehook and its posthook.
    open: BTreeMap<DetTid, SyscallRecord>,
    /// The channel identity of each end of a pipe or socketpair.
    pairs: BTreeMap<OpenFileId, ChannelId>,
    /// What the synchronization edges need (design section 7).
    edges: EdgeState,
}

/// The writer's view of who synchronized with whom, kept to emit an edge
/// once its later end is recorded.
#[derive(Debug, Default)]
struct EdgeState {
    /// Each thread's first and latest recorded event.
    first_event: BTreeMap<DetTid, EventIndex>,
    last_event: BTreeMap<DetTid, EventIndex>,
    /// The process of each thread, from its syscalls.
    process_of: BTreeMap<DetTid, DetPid>,
    /// The latest event of any thread of each process.
    process_last: BTreeMap<DetPid, EventRef>,
    /// Edges that end at their waiter's next posthook: a futex wake, a
    /// requeue, an exit's clear-child-tid wake.
    at_posthook: BTreeMap<DetTid, Vec<PendingEdge>>,
    /// Creation edges whose child has not run yet.
    at_first_event: BTreeMap<DetTid, PendingEdge>,
    /// Signals sent and not yet delivered, in send order.
    signals: Vec<PendingSignal>,
}

/// An edge whose `to` end is not recorded yet.
#[derive(Debug, Clone)]
struct PendingEdge {
    kind: EdgeKind,
    from: EventRef,
    object: SyncObject,
}

/// A signal a thread sent (kill, tkill, tgkill, rt_sigqueueinfo,
/// rt_tgsigqueueinfo) or a timer it armed (timer_settime), waiting for the
/// delivery it causes.
#[derive(Debug, Clone, PartialEq)]
struct PendingSignal {
    from: EventRef,
    sender: DetPid,
    signo: i32,
    target: Task,
    /// For a timer: its id. A timer's expiry carries it as `si_tid`.
    timer: Option<i32>,
    /// The call's posthook, once recorded: it succeeded.
    post: Option<EventIndex>,
    /// For a timer arm: its timer was deleted since.
    deleted: bool,
    /// For a group target: the members listed at its posthook, less the
    /// ones reaped since.
    members: Option<Vec<DetPid>>,
    /// For a group target: the processes it reached already.
    reached: Vec<DetPid>,
}

/// Sends and arms kept waiting for their delivery, at most. Past it, one
/// goes: the oldest arm of a deleted timer, else the oldest send, else the
/// oldest arm, because a live timer's arm can cause expiries for the rest
/// of the run.
const MAX_PENDING_SIGNALS: usize = 4096;

impl PendingSignal {
    /// Whether a group send has reached every member listed at its posthook.
    fn reached_all(&self) -> bool {
        self.members
            .as_ref()
            .is_some_and(|members| members.iter().all(|m| self.reached.contains(m)))
    }
}

/// The process table facts the writer resolves a group signal with.
pub trait ProcessGroups {
    /// The process group of `pid`.
    fn group_of(&self, pid: DetPid) -> Option<DetPid>;
    /// The processes of `group`, in pid order.
    fn members(&self, group: DetPid) -> Vec<DetPid>;
}

impl std::fmt::Debug for AccessTraceWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccessTraceWriter")
            .field("writing", &self.out.is_some())
            .field("from", &self.from)
            .field("records", &self.records)
            .field("open", &self.open.len())
            .finish_non_exhaustive()
    }
}

impl AccessTraceWriter {
    /// Create the trace at `path` and write its header.
    pub fn create(
        path: &Path,
        from: EventIndex,
        max_records: Option<u64>,
        max_bytes: Option<u64>,
    ) -> io::Result<Self> {
        let mut writer = Self::new(
            Box::new(BufWriter::with_capacity(1 << 20, File::create(path)?)),
            from,
            max_records,
            max_bytes,
        )?;
        writer.name = path.display().to_string();
        Ok(writer)
    }

    /// Write the header of a trace to `out`.
    pub(crate) fn new(
        mut out: Box<dyn Write + Send>,
        from: EventIndex,
        max_records: Option<u64>,
        max_bytes: Option<u64>,
    ) -> io::Result<Self> {
        let header = TraceRecord::Header(TraceHeader::new(from)).to_line()?;
        out.write_all(&header)?;
        Ok(AccessTraceWriter {
            out: Some(out),
            from,
            records: 0,
            max_records,
            bytes: header.len() as u64,
            max_bytes: max_bytes.unwrap_or(ACCESS_TRACE_DEFAULT_MAX_BYTES),
            default_max_bytes: max_bytes.is_none(),
            capped: false,
            name: "the access trace".to_string(),
            open: BTreeMap::new(),
            pairs: BTreeMap::new(),
            edges: EdgeState::default(),
        })
    }

    /// Take the event the scheduler just recorded at `index`, and its payload.
    pub fn observe(
        &mut self,
        index: EventIndex,
        event: &SchedEvent,
        payload: Option<AccessPayload>,
        groups: &dyn ProcessGroups,
    ) {
        let tid = event.dettid;
        self.note_event(tid, index);
        let Some(payload) = payload else {
            return;
        };
        match (payload, &event.op) {
            (AccessPayload::SyscallEntry(entry), Op::Syscall(_, SyscallPhase::Prehook)) => {
                self.edges.process_of.insert(tid, entry.pid);
                self.edges
                    .process_last
                    .insert(entry.pid, EventRef { tid, event: index });
                self.note_signal_send(EventRef { tid, event: index }, &entry);
                // A call that never reached its posthook (a restarted wait,
                // a successful execve) ends when its thread enters the next.
                if let Some(unfinished) = self.open.remove(&tid) {
                    self.finish_record(unfinished);
                }
                self.open.insert(
                    tid,
                    SyscallRecord {
                        pre: index,
                        post: None,
                        tid,
                        pid: entry.pid,
                        nth: entry.nth,
                        sysno: entry.sysno,
                        args: entry.args,
                        result: None,
                        accesses: entry.accesses,
                        returned: None,
                        out_bytes: None,
                    },
                );
            }
            (AccessPayload::SyscallExit(outcome), Op::Syscall(_, SyscallPhase::Posthook)) => {
                let Some(mut record) = self.open.remove(&tid) else {
                    return;
                };
                let outcome = *outcome;
                record.post = Some(index);
                record.result = Some(outcome.result);
                record.accesses = resolve_groups(outcome.accesses, record.pid, groups);
                record.returned = outcome.returned;
                record.out_bytes = outcome.out_bytes;
                let to = EventRef { tid, event: index };
                for edge in self.edges.at_posthook.remove(&tid).unwrap_or_default() {
                    self.write_edge(edge, to);
                }
                self.syscall_edges(&record, to);
                self.finish_record(record);
            }
            (AccessPayload::Signal(note), Op::SignalReceived(_)) => {
                let to = EventRef { tid, event: index };
                let sent = self.take_signal_send(&note, tid, groups);
                if let Some(from) = sent {
                    self.write_edge(
                        PendingEdge {
                            kind: EdgeKind::Signal,
                            from,
                            object: SyncObject::Signal(note.signo),
                        },
                        to,
                    );
                }
                if index >= self.from {
                    let sender = note.sender.map(|sender| SignalSender {
                        tid: sent.map(|from| from.tid),
                        ..sender
                    });
                    self.write(TraceRecord::Signal(SignalRecord {
                        event: index,
                        tid,
                        signo: note.signo,
                        code: note.code,
                        addr: note.addr,
                        sender,
                        observed_disposition: note.observed_disposition,
                    }));
                }
            }
            (payload, op) => warn!(
                "[access trace] dropped a payload that does not match its event {:?}: {:?}",
                op, payload
            ),
        }
    }

    /// A thread's event at `index`: the first one of a new thread ends the
    /// creation edge that started it.
    fn note_event(&mut self, tid: DetTid, index: EventIndex) {
        let to = EventRef { tid, event: index };
        if let std::collections::btree_map::Entry::Vacant(first) = self.edges.first_event.entry(tid)
        {
            first.insert(index);
            if let Some(edge) = self.edges.at_first_event.remove(&tid) {
                self.write_edge(edge, to);
            }
        }
        self.edges.last_event.insert(tid, index);
        if let Some(pid) = self.edges.process_of.get(&tid).copied() {
            self.edges.process_last.insert(pid, to);
        }
    }

    /// `waker`'s operation released `waiter` through `object`: a futex wake
    /// or requeue (from the waker's syscall) or an exit's wake (from the
    /// exiting thread's last event). The edge ends at the waiter's next
    /// posthook, the return of the wait it was released from.
    pub fn note_wake(&mut self, kind: EdgeKind, waker: DetTid, waiter: DetTid, object: SyncObject) {
        let syscall = self.open.get(&waker).map(|record| record.pre);
        let last = self.edges.last_event.get(&waker).copied();
        let from = match kind {
            EdgeKind::ExitJoin => last,
            _ => syscall.or(last),
        };
        let Some(event) = from else {
            return;
        };
        self.edges
            .at_posthook
            .entry(waiter)
            .or_default()
            .push(PendingEdge {
                kind,
                from: EventRef { tid: waker, event },
                object,
            });
    }

    /// The edges a finished syscall ends or starts: a clone's child, a
    /// reaped child's exit, and a failed send that will deliver nothing.
    fn syscall_edges(&mut self, record: &SyscallRecord, to: EventRef) {
        let result = record.result.unwrap_or(-1);
        let from = EventRef {
            tid: record.tid,
            event: record.pre,
        };
        match record.sysno {
            sysno if creates_a_task(sysno) && result > 0 => {
                let child = DetTid::from_raw(result as i32);
                let edge = PendingEdge {
                    kind: EdgeKind::Create,
                    from,
                    object: SyncObject::Thread(child),
                };
                // The child may have run before its parent's clone returned.
                match self.edges.first_event.get(&child).copied() {
                    Some(first) if first > record.pre => self.write_edge(
                        edge,
                        EventRef {
                            tid: child,
                            event: first,
                        },
                    ),
                    _ => {
                        self.edges.first_event.remove(&child);
                        self.edges.at_first_event.insert(child, edge);
                    }
                }
            }
            Sysno::wait4 | Sysno::waitid => {
                let mut reaped = Vec::new();
                for access in &record.accesses {
                    if let Access::Lifetime {
                        target: Task::Process(child),
                        op: LifetimeOp::Wait,
                    } = access
                    {
                        reaped.push(*child);
                        if let Some(exit) = self.edges.process_last.get(child).copied()
                            && exit.event < to.event
                        {
                            self.write_edge(
                                PendingEdge {
                                    kind: EdgeKind::ChildExit,
                                    from: exit,
                                    object: SyncObject::Process(*child),
                                },
                                to,
                            );
                        }
                    }
                }
                for child in reaped {
                    self.forget_reaped(child);
                }
            }
            _ => {}
        }
        self.settle_signal_send(record);
    }

    /// At a send's posthook: drop it if it failed; otherwise mark it
    /// completed and fix a group target and its members. A timer_delete
    /// marks its timer's arms deleted: one expiry dequeued before the
    /// delete can still be delivered after it.
    fn settle_signal_send(&mut self, record: &SyscallRecord) {
        let result = record.result.unwrap_or(-1);
        if record.sysno == Sysno::timer_delete && result == 0 {
            let timer = Some(record.args[0] as i32);
            for signal in &mut self.edges.signals {
                if signal.timer == timer && signal.sender == record.pid {
                    signal.deleted = true;
                }
            }
        }
        let from = EventRef {
            tid: record.tid,
            event: record.pre,
        };
        let Some(position) = self.edges.signals.iter().position(|s| s.from == from) else {
            return;
        };
        if result < 0 {
            self.edges.signals.remove(position);
            return;
        }
        let signal = &mut self.edges.signals[position];
        signal.post = record.post;
        if matches!(signal.target, Task::ProcessGroup(_)) {
            let mut members = Vec::new();
            for access in &record.accesses {
                if let Access::Lifetime { target, .. } = access {
                    match target {
                        Task::ProcessGroup(group) => signal.target = Task::ProcessGroup(*group),
                        Task::Process(member) => members.push(*member),
                        Task::Thread(_) => {}
                    }
                }
            }
            signal.members = Some(members);
            if signal.reached_all() {
                self.edges.signals.remove(position);
            }
        }
    }

    /// `child` was reaped, so nothing pending can reach it any more: drop
    /// the sends to it or its threads and its timers' arms, and take it out
    /// of the group sends still waiting for it.
    fn forget_reaped(&mut self, child: DetPid) {
        let process_of = &self.edges.process_of;
        self.edges.signals.retain_mut(|signal| match signal.target {
            _ if signal.timer.is_some() => signal.sender != child,
            Task::Process(target) => target != child,
            Task::Thread(thread) => process_of.get(&thread) != Some(&child),
            Task::ProcessGroup(_) => {
                if let Some(members) = &mut signal.members {
                    members.retain(|member| *member != child);
                }
                !signal.reached_all()
            }
        });
    }

    /// Make room for one more pending send (see [`MAX_PENDING_SIGNALS`]).
    fn evict_pending_signal(&mut self) {
        let signals = &self.edges.signals;
        let victim = signals
            .iter()
            .position(|signal| signal.deleted)
            .or_else(|| signals.iter().position(|signal| signal.timer.is_none()))
            .unwrap_or(0);
        self.edges.signals.remove(victim);
    }

    /// Note a signal a syscall entry sends, or a timer it arms.
    fn note_signal_send(&mut self, from: EventRef, entry: &SyscallEntry) {
        let raw = |i: usize| entry.args[i] as i32;
        let pid = DetPid::from_raw;
        let (target, signo, timer) = match entry.sysno {
            Sysno::kill => {
                let target = match raw(0) {
                    p if p > 0 => Task::Process(pid(p)),
                    0 => Task::ProcessGroup(OWN_PROCESS_GROUP),
                    -1 => return,
                    p => Task::ProcessGroup(pid(p.wrapping_neg())),
                };
                (target, raw(1), None)
            }
            Sysno::tkill => (Task::Thread(pid(raw(0))), raw(1), None),
            Sysno::tgkill | Sysno::rt_tgsigqueueinfo => (Task::Thread(pid(raw(1))), raw(2), None),
            Sysno::rt_sigqueueinfo => (Task::Process(pid(raw(0))), raw(1), None),
            // The expiry's signal comes from the sigevent timer_create
            // named, so a timer arm matches any signal carrying its id.
            Sysno::timer_settime => (Task::Process(entry.pid), 0, Some(raw(0))),
            _ => return,
        };
        if signo == 0 && timer.is_none() {
            return;
        }
        if self.edges.signals.len() == MAX_PENDING_SIGNALS {
            self.evict_pending_signal();
        }
        self.edges.signals.push(PendingSignal {
            from,
            sender: entry.pid,
            signo,
            target,
            timer,
            post: None,
            deleted: false,
            members: None,
            reached: Vec::new(),
        });
    }

    /// The send that caused `note`'s delivery to `tid`, if one is pending.
    ///
    /// A candidate is a send of that signal, to that thread, its process or
    /// its group, from the process `si_pid` names. The delivery takes the
    /// earliest candidate and uses up that one alone (a group send only for
    /// this process, until it has reached every member listed at its
    /// posthook). That holds for standard signals too, whose sends merge:
    /// the k-th delivery is never matched with a send later than the
    /// latest cause among the first k deliveries, so no edge claims more
    /// order than the run had. Taking the latest send instead is unsound:
    /// on ptrace the kernel dequeues a signal before the stop at which its
    /// delivery is recorded, so a send recorded in between can queue a
    /// second instance, and a send to the process and one to the thread
    /// queue separately and deliver twice.
    ///
    /// The induction needs the candidates' sends to have run in the order
    /// of their prehooks. A send runs in its prehook's turn (handle_kill and
    /// handle_tgkill request no turn before record_or_replay), except that
    /// a `--happens-before` checkpoint requests one between them. So the
    /// earliest candidate gives an edge only when its posthook comes before
    /// every later candidate's prehook, which a later call of the same
    /// thread always satisfies; otherwise it is used up without an edge.
    /// The merged sends a delivery leaves pending stay until the cap or
    /// reaping removes them: they can give weaker edges, never false ones.
    ///
    /// A timer's expiry takes the oldest live arm of the timer whose id it
    /// carries, for the same reason. A timer holds one queued signal, so
    /// once an expiry recorded after a newer arm's posthook has matched the
    /// oldest arm, that arm can cause no more; after timer_delete, at most
    /// one expiry dequeued before it remains.
    fn take_signal_send(
        &mut self,
        note: &SignalNote,
        tid: DetTid,
        groups: &dyn ProcessGroups,
    ) -> Option<EventRef> {
        let process = self.edges.process_of.get(&tid).copied();
        let signals = &mut self.edges.signals;
        if let Some(timer) = note.timer {
            let process = process?;
            let arms: Vec<usize> = (0..signals.len())
                .filter(|&i| signals[i].timer == Some(timer) && signals[i].sender == process)
                .collect();
            let oldest = *arms.first()?;
            let from = signals[oldest].from;
            if signals[oldest].deleted {
                signals.retain(|s| !(s.timer == Some(timer) && s.sender == process && s.deleted));
            } else if arms[1..].iter().any(|&i| signals[i].post.is_some()) {
                signals.remove(oldest);
            }
            return Some(from);
        }
        let sender = note.sender?.pid;
        let group = process.and_then(|p| groups.group_of(p));
        let candidate = |signal: &PendingSignal| {
            let target = match signal.target {
                Task::ProcessGroup(OWN_PROCESS_GROUP) => {
                    Task::ProcessGroup(groups.group_of(signal.sender).unwrap_or(OWN_PROCESS_GROUP))
                }
                target => target,
            };
            signal.timer.is_none()
                && signal.signo == note.signo
                && signal.sender == sender
                && match target {
                    Task::Thread(thread) => thread == tid,
                    Task::Process(target) => Some(target) == process,
                    Task::ProcessGroup(target) => {
                        Some(target) == group
                            && process.is_some_and(|p| !signal.reached.contains(&p))
                    }
                }
        };
        let candidates: Vec<usize> = (0..signals.len())
            .filter(|&i| candidate(&signals[i]))
            .collect();
        let chosen = *candidates.first()?;
        let post = signals[chosen].post;
        let ran_first = candidates[1..]
            .iter()
            .all(|&i| post.is_some_and(|post| post < signals[i].from.event));
        let signal = &mut signals[chosen];
        let from = ran_first.then_some(signal.from);
        let used_up = match signal.target {
            Task::ProcessGroup(_) => {
                signal.reached.extend(process);
                signal.reached_all()
            }
            _ => true,
        };
        if used_up {
            signals.remove(chosen);
        }
        from
    }

    /// Write `edge` ending at `to`, unless `to` is before
    /// `--access-trace-from`.
    fn write_edge(&mut self, edge: PendingEdge, to: EventRef) {
        // Every edge source names an event recorded before its later end;
        // a backward edge would be a writer bug, not a property of the run.
        debug_assert!(
            edge.from.event < to.event,
            "a backward edge {edge:?} to {to:?}"
        );
        if to.event >= self.from && edge.from.event < to.event {
            self.write(TraceRecord::Edge(EdgeRecord {
                kind: edge.kind,
                from: edge.from,
                to,
                object: edge.object,
            }));
        }
    }

    /// Name the channels a finished record created, fill in the channel
    /// identities of its accesses, and write it unless it ends before
    /// `--access-trace-from`.
    fn finish_record(&mut self, mut record: SyscallRecord) {
        if creates_a_channel_pair(record.sysno) && record.result == Some(0) {
            let opened: Vec<OpenFileId> = record
                .accesses
                .iter()
                .filter_map(|access| match access {
                    Access::Fd {
                        open_file: Some(open_file),
                        op: FdOp::Open { .. },
                        ..
                    } => Some(*open_file),
                    _ => None,
                })
                .collect();
            if let [first, second] = opened[..] {
                let id = ChannelId::Pair(first);
                self.pairs.insert(first, id);
                self.pairs.insert(second, id);
            }
        }
        for access in &mut record.accesses {
            if let Access::Channel { open_file, id, .. } = access
                && id.is_none()
            {
                *id = self.pairs.get(open_file).copied();
            }
        }
        if record.post.unwrap_or(record.pre) >= self.from {
            self.write(TraceRecord::Syscall(record));
        }
    }

    fn write(&mut self, record: TraceRecord) {
        let Some(out) = self.out.as_mut() else {
            return;
        };
        let line = record.to_line();
        let cap = if let Some(max) = self.max_records.filter(|max| self.records >= *max) {
            Some(format!("--access-trace-max-records={max}"))
        } else if let Ok(line) = &line
            && self.bytes.saturating_add(line.len() as u64) > self.max_bytes
        {
            Some(format!(
                "--access-trace-max-bytes={}{}",
                self.max_bytes,
                if self.default_max_bytes {
                    " (the default)"
                } else {
                    ""
                }
            ))
        } else {
            None
        };
        if let Some(cap) = cap {
            // A cap: stop here, before the first record past it, and leave
            // the end line out. Said once, as out is gone after.
            warn!(
                "[access trace] {} reached {}; it stops here, without its end line",
                self.name, cap
            );
            if let Err(error) = out.flush() {
                warn!("[access trace] flushing {} failed ({})", self.name, error);
            }
            self.out = None;
            self.capped = true;
            return;
        }
        match line.and_then(|line| {
            out.write_all(&line)?;
            Ok(line.len() as u64)
        }) {
            Ok(written) => {
                self.records += 1;
                self.bytes += written;
            }
            Err(error) => {
                warn!(
                    "[access trace] writing the trace failed ({}); it ends here, without its end line",
                    error
                );
                self.out = None;
            }
        }
    }

    /// Write the calls that never returned and the end line, and flush.
    /// `events` is the number of events the run recorded.
    pub fn finish(mut self, events: u64) -> io::Result<()> {
        for record in std::mem::take(&mut self.open).into_values() {
            self.finish_record(record);
        }
        let records = self.records;
        if self.capped {
            return Ok(());
        }
        let Some(mut out) = self.out.take() else {
            return Err(io::Error::other(
                "an earlier write failed; the trace has no end line",
            ));
        };
        TraceRecord::End(TraceEnd { events, records }).write_line(&mut out)?;
        out.flush()
    }
}

/// A writer whose trace stays in memory, and the bytes it wrote, for the
/// scheduler's tests.
#[cfg(test)]
pub(crate) fn in_memory_writer() -> (AccessTraceWriter, std::sync::Arc<Mutex<Vec<u8>>>) {
    #[derive(Clone)]
    struct Shared(std::sync::Arc<Mutex<Vec<u8>>>);
    impl Write for Shared {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let bytes = std::sync::Arc::new(Mutex::new(Vec::new()));
    let writer =
        AccessTraceWriter::new(Box::new(Shared(bytes.clone())), 0, None, None).expect("in memory");
    (writer, bytes)
}

/// The edges of a finished in-memory trace.
#[cfg(test)]
pub(crate) fn edges_of(bytes: &Mutex<Vec<u8>>) -> Vec<EdgeRecord> {
    let bytes = bytes.lock().unwrap().clone();
    detcore_model::access_trace::read_access_trace(&bytes[..])
        .expect("trace parses")
        .records
        .into_iter()
        .filter_map(|record| match record {
            TraceRecord::Edge(edge) => Some(edge),
            _ => None,
        })
        .collect()
}

/// Add one process target per member to each group signal, and resolve the
/// caller's own group.
fn resolve_groups(
    accesses: Vec<Access>,
    caller: DetPid,
    groups: &dyn ProcessGroups,
) -> Vec<Access> {
    let mut out = Vec::with_capacity(accesses.len());
    for access in accesses {
        let Access::Lifetime {
            target: Task::ProcessGroup(group),
            op,
        } = access
        else {
            out.push(access);
            continue;
        };
        let group = if group == OWN_PROCESS_GROUP {
            match groups.group_of(caller) {
                Some(group) => group,
                None => continue,
            }
        } else {
            group
        };
        out.push(Access::Lifetime {
            target: Task::ProcessGroup(group),
            op,
        });
        for member in groups.members(group) {
            out.push(Access::Lifetime {
                target: Task::Process(member),
                op,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use detcore_model::access_trace::TraceRecord;
    use detcore_model::access_trace::read_access_trace;

    use super::*;

    fn tid(n: i32) -> DetTid {
        DetTid::from_raw(n)
    }

    fn files() -> FilesId {
        FilesId::initial(tid(3))
    }

    fn slot(fd: i32) -> FdSlot {
        FdSlot { files: files(), fd }
    }

    fn file(creator: i32, sequence: u64) -> OpenFileId {
        OpenFileId::new(tid(creator), sequence)
    }

    fn info(open_file: OpenFileId, ty: FdType) -> FdInfo {
        FdInfo {
            open_file,
            ty,
            inode: Some(DetInode::mint(40)),
            fifo: false,
            pidfd_target: None,
        }
    }

    fn mm() -> MmId {
        MmId::initial(tid(3))
    }

    fn private_key(addr: u64, private: bool) -> FutexID {
        if private {
            FutexID::private(mm(), addr as usize)
        } else {
            FutexID::private(MmId::initial(tid(99)), addr as usize)
        }
    }

    /// The accesses of one call, with the given descriptors before it and
    /// slot changes during it.
    fn run(
        sysno: Sysno,
        args: [u64; 6],
        result: i64,
        fds: &[(i32, FdInfo)],
        notes: &[FdNote],
        written: Written,
    ) -> Vec<Access> {
        let fds: BTreeMap<i32, FdInfo> = fds.iter().cloned().collect();
        accesses(&CallFacts {
            sysno,
            args,
            result,
            pid: DetPid::from_raw(3),
            tid: tid(5),
            files: files(),
            mm: mm(),
            fds: &fds,
            notes,
            buffers: &[],
            written: &written,
            futex_id: &private_key,
        })
    }

    fn note(fd: i32, open_file: Option<OpenFileId>, op: FdOp) -> FdNote {
        FdNote {
            slot: slot(fd),
            open_file,
            op,
            released: None,
        }
    }

    /// The note of a close that released `info`.
    fn closed(fd: i32, info: &FdInfo) -> FdNote {
        FdNote {
            released: Some(info.clone()),
            ..note(fd, Some(info.open_file), FdOp::Close)
        }
    }

    /// Every payload crosses RPC as Reverie's positional bincode, which a
    /// debug build checks by decoding each request it encodes.
    #[test]
    fn payloads_round_trip_through_rpc_bincode() {
        let payloads = [
            entry(Sysno::futex, 7),
            AccessPayload::SyscallExit(Box::new(SyscallOutcome {
                result: -11,
                accesses: vec![
                    Access::Fd {
                        slot: slot(6),
                        open_file: None,
                        op: FdOp::Open {
                            path: Some("/tmp/x".into()),
                        },
                    },
                    Access::Futex {
                        futex: FutexID::private(mm(), 0x1000),
                        op: FutexOp::Wait,
                    },
                ],
                returned: Some(ReturnedBytes {
                    len: 3,
                    digest: 9,
                    truncated: false,
                }),
                out_bytes: Some(b"hi".to_vec()),
            })),
            AccessPayload::Signal(signal_note(
                libc::SIGUSR1,
                None,
                ObservedSignalDisposition::Handler,
            )),
            AccessPayload::Signal(signal_note(
                libc::SIGUSR1,
                None,
                ObservedSignalDisposition::Ignored,
            )),
            AccessPayload::Signal(signal_note(
                libc::SIGUSR1,
                None,
                ObservedSignalDisposition::Default,
            )),
            AccessPayload::Signal(signal_note(
                libc::SIGUSR1,
                None,
                ObservedSignalDisposition::Unknown,
            )),
        ];
        for payload in payloads.clone() {
            let request = crate::tool_global::GlobalRequest::TraceSchedEvent(
                prehook(5, Sysno::futex),
                DetPid::from_raw(3),
                false,
                Some(Box::new(payload)),
            );
            let wire = bincode::serde::encode_to_vec(&request, bincode::config::legacy()).unwrap();
            let (back, _): (crate::tool_global::GlobalRequest, usize) =
                bincode::serde::decode_from_slice(&wire, bincode::config::legacy()).unwrap();
            assert_eq!(back, request);
        }
        for payload in payloads {
            let wire = bincode::serde::encode_to_vec(&payload, bincode::config::legacy()).unwrap();
            let (back, read): (AccessPayload, usize) =
                bincode::serde::decode_from_slice(&wire, bincode::config::legacy()).unwrap();
            assert_eq!(read, wire.len());
            assert_eq!(back, payload);
        }
    }

    #[test]
    fn close_releases_the_open_file_the_slot_held() {
        let pipe = info(file(7, 2), FdType::Pipe);
        let got = run(
            Sysno::close,
            [6, 0, 0, 0, 0, 0],
            0,
            &[(6, pipe.clone())],
            &[closed(6, &pipe)],
            Written::default(),
        );
        assert_eq!(
            got,
            vec![
                Access::Fd {
                    slot: slot(6),
                    open_file: Some(file(7, 2)),
                    op: FdOp::Close
                },
                Access::Channel {
                    open_file: file(7, 2),
                    id: None,
                    channel: ChannelKind::Pipe,
                    op: ChannelOp::Close
                },
            ]
        );

        // A second close of the number releases nothing; it is still a close.
        let again = run(
            Sysno::close,
            [6, 0, 0, 0, 0, 0],
            -libc::EBADF as i64,
            &[],
            &[],
            Written::default(),
        );
        assert_eq!(
            again,
            vec![Access::Fd {
                slot: slot(6),
                open_file: None,
                op: FdOp::Close
            }]
        );
    }

    #[test]
    fn dup2_onto_an_open_number_closes_it_then_dups() {
        let notes = [
            closed(5, &info(file(3, 1), FdType::Regular)),
            note(5, Some(file(7, 3)), FdOp::Dup { source: 6 }),
        ];
        let got = run(
            Sysno::dup3,
            [6, 5, libc::O_CLOEXEC as u64, 0, 0, 0],
            5,
            &[
                (6, info(file(7, 3), FdType::Regular)),
                (5, info(file(3, 1), FdType::Regular)),
            ],
            &notes,
            Written::default(),
        );
        assert_eq!(
            got,
            vec![
                Access::Fd {
                    slot: slot(6),
                    open_file: Some(file(7, 3)),
                    op: FdOp::Use
                },
                Access::Fd {
                    slot: slot(5),
                    open_file: Some(file(3, 1)),
                    op: FdOp::Close
                },
                Access::Fd {
                    slot: slot(5),
                    open_file: Some(file(7, 3)),
                    op: FdOp::Dup { source: 6 }
                },
            ]
        );
        assert_eq!(named_fds(Sysno::dup3, [6, 5, 0, 0, 0, 0]), vec![5, 6]);
        #[cfg(target_arch = "x86_64")]
        assert_eq!(named_fds(Sysno::dup2, [6, 5, 0, 0, 0, 0]), vec![5, 6]);
    }

    /// close_range lists every slot it released, and the pipe end behind
    /// one, though its arguments name no descriptor.
    #[test]
    fn close_range_lists_every_slot_it_released() {
        let regular = info(file(3, 1), FdType::Regular);
        let pipe = info(file(3, 2), FdType::Pipe);
        let got = run(
            Sysno::close_range,
            [3, u32::MAX as u64, 0, 0, 0, 0],
            0,
            &[],
            &[closed(3, &regular), closed(4, &pipe)],
            Written::default(),
        );
        assert_eq!(
            got,
            vec![
                Access::Fd {
                    slot: slot(3),
                    open_file: Some(file(3, 1)),
                    op: FdOp::Close
                },
                Access::Fd {
                    slot: slot(4),
                    open_file: Some(file(3, 2)),
                    op: FdOp::Close
                },
                Access::Channel {
                    open_file: file(3, 2),
                    id: None,
                    channel: ChannelKind::Pipe,
                    op: ChannelOp::Close
                },
            ]
        );
    }

    #[test]
    fn a_reused_number_names_the_new_open_file() {
        let path = Some("/tmp/misc".to_string());
        let open = run(
            Sysno::openat,
            [libc::AT_FDCWD as u64, 0x2000, libc::O_RDWR as u64, 0, 0, 0],
            6,
            &[],
            &[note(6, Some(file(7, 3)), FdOp::Open { path: path.clone() })],
            Written::default(),
        );
        assert_eq!(
            open,
            vec![Access::Fd {
                slot: slot(6),
                open_file: Some(file(7, 3)),
                op: FdOp::Open { path }
            }]
        );
        // A later lseek through the number reaches the new open file.
        let seek = run(
            Sysno::lseek,
            [6, 2048, libc::SEEK_SET as u64, 0, 0, 0],
            2048,
            &[(6, info(file(7, 3), FdType::Regular))],
            &[],
            Written::default(),
        );
        assert_eq!(
            seek,
            vec![
                Access::Fd {
                    slot: slot(6),
                    open_file: Some(file(7, 3)),
                    op: FdOp::Use
                },
                Access::OpenFile {
                    open_file: file(7, 3),
                    op: CursorOp::Seek
                },
            ]
        );
    }

    #[test]
    fn reads_and_writes_reach_cursor_contents_and_channels() {
        let regular = info(file(3, 1), FdType::Regular);
        let pread = run(
            Sysno::pread64,
            [4, 0x1000, 100, 512, 0, 0],
            0,
            &[(4, regular.clone())],
            &[],
            Written::default(),
        );
        assert_eq!(
            pread[1],
            Access::FileContents {
                inode: DetInode::mint(40),
                range: Some(ByteRange {
                    start: 512,
                    end: 612
                }),
                op: DataOp::Read
            },
            "an empty pread covers the bytes it asked for"
        );
        let write = run(
            Sysno::write,
            [4, 0x1000, 10, 0, 0, 0],
            10,
            &[(4, regular)],
            &[],
            Written::default(),
        );
        assert_eq!(
            write[1..],
            [
                Access::OpenFile {
                    open_file: file(3, 1),
                    op: CursorOp::Write
                },
                Access::FileContents {
                    inode: DetInode::mint(40),
                    range: None,
                    op: DataOp::Write
                },
            ]
        );
        let fifo = FdInfo {
            fifo: true,
            ..info(file(3, 9), FdType::Regular)
        };
        let read = run(
            Sysno::read,
            [7, 0x1000, 10, 0, 0, 0],
            -libc::EAGAIN as i64,
            &[(7, fifo)],
            &[],
            Written::default(),
        );
        assert_eq!(
            read[1],
            Access::Channel {
                open_file: file(3, 9),
                id: Some(ChannelId::Inode(DetInode::mint(40))),
                channel: ChannelKind::Fifo,
                op: ChannelOp::Read
            }
        );
    }

    #[test]
    fn futex_operations_name_their_words() {
        let wait = |op: i32| {
            run(
                Sysno::futex,
                [0x1000, op as u64, 0, 0, 0x2000, 0],
                0,
                &[],
                &[],
                Written::default(),
            )
        };
        let private = libc::FUTEX_PRIVATE_FLAG;
        assert_eq!(
            wait(libc::FUTEX_WAIT | private),
            vec![Access::Futex {
                futex: FutexID::private(mm(), 0x1000),
                op: FutexOp::Wait
            }]
        );
        assert_eq!(
            wait(libc::FUTEX_WAKE),
            vec![Access::Futex {
                futex: private_key(0x1000, false),
                op: FutexOp::Wake
            }],
            "a shared futex takes the shared key"
        );
        assert_eq!(
            wait(libc::FUTEX_CMP_REQUEUE | private),
            vec![
                Access::Futex {
                    futex: FutexID::private(mm(), 0x1000),
                    op: FutexOp::Wake
                },
                Access::Futex {
                    futex: FutexID::private(mm(), 0x2000),
                    op: FutexOp::RequeueTarget
                },
            ]
        );
        assert_eq!(
            wait(libc::FUTEX_WAKE_OP | private)[1],
            Access::Futex {
                futex: FutexID::private(mm(), 0x2000),
                op: FutexOp::ModifyWake
            }
        );
        assert_eq!(
            wait(libc::FUTEX_UNLOCK_PI | private),
            vec![Access::Futex {
                futex: FutexID::private(mm(), 0x1000),
                op: FutexOp::Unlock
            }]
        );
        assert_eq!(
            wait(libc::FUTEX_FD),
            vec![],
            "an unknown command names nothing"
        );
    }

    #[test]
    fn munmap_and_mremap_change_page_ranges() {
        let munmap = run(
            Sysno::munmap,
            [0x7000, 100, 0, 0, 0, 0],
            0,
            &[],
            &[],
            Written::default(),
        );
        assert_eq!(
            munmap,
            vec![Access::Memory {
                mm: mm(),
                range: ByteRange {
                    start: 0x7000,
                    end: 0x8000
                },
                op: MemoryOp::Unmap
            }]
        );
        let failed = run(
            Sysno::munmap,
            [0x7001, 100, 0, 0, 0, 0],
            -libc::EINVAL as i64,
            &[],
            &[],
            Written::default(),
        );
        assert_eq!(failed, vec![], "a failed munmap unmapped nothing");
        let mremap = run(
            Sysno::mremap,
            [0x7000, 0x1000, 0x3000, libc::MREMAP_MAYMOVE as u64, 0, 0],
            0x9000,
            &[],
            &[],
            Written::default(),
        );
        assert_eq!(
            mremap,
            vec![
                Access::Memory {
                    mm: mm(),
                    range: ByteRange {
                        start: 0x7000,
                        end: 0x8000
                    },
                    op: MemoryOp::Unmap
                },
                Access::Memory {
                    mm: mm(),
                    range: ByteRange {
                        start: 0x9000,
                        end: 0xc000
                    },
                    op: MemoryOp::Map
                },
            ]
        );
    }

    #[test]
    fn timers_are_named_per_process_or_by_their_timerfd() {
        let process = DetPid::from_raw(3);
        let create = run(
            Sysno::timer_create,
            [libc::CLOCK_MONOTONIC as u64, 0, 0x3000, 0, 0, 0],
            0,
            &[],
            &[],
            Written {
                timer_id: Some(2),
                ..Written::default()
            },
        );
        assert_eq!(
            create,
            vec![Access::Timer {
                timer: TimerId::Posix { process, id: 2 },
                op: TimerOp::Create
            }]
        );
        for (sysno, op) in [
            (Sysno::timer_settime, TimerOp::Set),
            (Sysno::timer_gettime, TimerOp::Get),
            (Sysno::timer_delete, TimerOp::Delete),
        ] {
            assert_eq!(
                run(sysno, [2, 0, 0, 0, 0, 0], 0, &[], &[], Written::default()),
                vec![Access::Timer {
                    timer: TimerId::Posix { process, id: 2 },
                    op
                }],
                "{sysno}"
            );
        }
        #[cfg(target_arch = "x86_64")]
        assert_eq!(
            run(
                Sysno::alarm,
                [5, 0, 0, 0, 0, 0],
                0,
                &[],
                &[],
                Written::default()
            ),
            vec![Access::Timer {
                timer: TimerId::Interval {
                    process,
                    which: libc::ITIMER_REAL
                },
                op: TimerOp::Set
            }]
        );
        let timerfd = info(file(3, 4), FdType::Timerfd);
        let read = run(
            Sysno::read,
            [9, 0x1000, 8, 0, 0, 0],
            8,
            &[(9, timerfd)],
            &[],
            Written::default(),
        );
        assert_eq!(
            read[1],
            Access::Timer {
                timer: TimerId::Fd {
                    open_file: file(3, 4)
                },
                op: TimerOp::Read
            }
        );
        let created = run(
            Sysno::timerfd_create,
            [libc::CLOCK_MONOTONIC as u64, 0, 0, 0, 0, 0],
            9,
            &[],
            &[note(9, Some(file(3, 4)), FdOp::Open { path: None })],
            Written::default(),
        );
        assert_eq!(
            created[1],
            Access::Timer {
                timer: TimerId::Fd {
                    open_file: file(3, 4)
                },
                op: TimerOp::Create
            }
        );
    }

    #[test]
    fn lifetime_operations_name_threads_processes_and_groups() {
        let thread = run(
            Sysno::clone,
            [(libc::CLONE_VM | libc::CLONE_THREAD) as u64, 0, 0, 0, 0, 0],
            8,
            &[],
            &[],
            Written::default(),
        );
        assert_eq!(
            thread,
            vec![Access::Lifetime {
                target: Task::Thread(tid(8)),
                op: LifetimeOp::Create
            }]
        );
        let kill = |pid: i64| {
            run(
                Sysno::kill,
                [pid as u64, 9, 0, 0, 0, 0],
                0,
                &[],
                &[],
                Written::default(),
            )
        };
        let sigkill = LifetimeOp::Signal { signo: 9 };
        assert_eq!(
            kill(4),
            vec![Access::Lifetime {
                target: Task::Process(DetPid::from_raw(4)),
                op: sigkill
            }]
        );
        assert_eq!(
            kill(-4),
            vec![Access::Lifetime {
                target: Task::ProcessGroup(DetPid::from_raw(4)),
                op: sigkill
            }]
        );
        assert_eq!(kill(-1), vec![]);
        assert_eq!(
            run(
                Sysno::waitid,
                [libc::P_ALL as u64, 0, 0x5000, libc::WEXITED as u64, 0, 0],
                0,
                &[],
                &[],
                Written {
                    waited_pid: Some(4),
                    ..Written::default()
                },
            ),
            vec![Access::Lifetime {
                target: Task::Process(DetPid::from_raw(4)),
                op: LifetimeOp::Wait
            }]
        );
    }

    struct Groups;

    /// Processes 3 and 4 form group 3.
    impl ProcessGroups for Groups {
        fn group_of(&self, pid: DetPid) -> Option<DetPid> {
            [3, 4]
                .contains(&pid.as_raw())
                .then_some(DetPid::from_raw(3))
        }
        fn members(&self, group: DetPid) -> Vec<DetPid> {
            if group == DetPid::from_raw(3) {
                vec![DetPid::from_raw(3), DetPid::from_raw(4)]
            } else {
                Vec::new()
            }
        }
    }

    #[test]
    fn a_group_signal_lists_each_member() {
        let op = LifetimeOp::Signal { signo: 15 };
        let resolved = resolve_groups(
            vec![Access::Lifetime {
                target: Task::ProcessGroup(OWN_PROCESS_GROUP),
                op,
            }],
            DetPid::from_raw(3),
            &Groups,
        );
        let target = |t| Access::Lifetime { target: t, op };
        assert_eq!(
            resolved,
            vec![
                target(Task::ProcessGroup(DetPid::from_raw(3))),
                target(Task::Process(DetPid::from_raw(3))),
                target(Task::Process(DetPid::from_raw(4))),
            ]
        );
    }

    #[test]
    fn returned_bytes_digest_at_most_the_cap() {
        let small = [
            ByteRange { start: 0, end: 2 },
            ByteRange { start: 10, end: 13 },
        ];
        let summary =
            summarize_output::<()>(None, &small, |at, want| Ok(vec![at as u8; want])).unwrap();
        assert_eq!(summary.len, 5);
        assert!(!summary.truncated);
        assert_eq!(
            summary.digest,
            returned_digest(RETURNED_DIGEST_SEED, &[0, 0, 10, 10, 10])
        );
        let big = [ByteRange {
            start: 0,
            end: RETURNED_DIGEST_CAP + 1,
        }];
        let mut asked = 0;
        let summary = summarize_output::<()>(None, &big, |_, want| {
            asked += want;
            Ok(vec![1; want])
        })
        .unwrap();
        assert!(summary.truncated);
        assert_eq!(asked as u64, RETURNED_DIGEST_CAP);
        assert_eq!(summarize_output::<()>(None, &[], |_, _| Ok(vec![])), None);
        assert_eq!(summarize_output(None, &small, |_, _| Err(())), None);
    }

    /// `si_code` of a fault at an unmapped address
    /// (`include/uapi/asm-generic/siginfo.h`).
    const SEGV_MAPERR: i32 = 1;

    /// Clock and timer reads carry what they returned: two clock_gettime
    /// calls whose timespec differs get different digests, and the same
    /// bytes the same digest. A remaining time counts only when a sleep was
    /// interrupted, and time's result is its output.
    #[test]
    fn clock_and_timer_reads_carry_what_they_returned() {
        let (value, buffers) = time_outputs(
            Sysno::clock_gettime,
            [libc::CLOCK_MONOTONIC as u64, 0x1000, 0, 0, 0, 0],
            0,
        );
        assert_eq!(value, None);
        assert_eq!(
            buffers,
            vec![ByteRange {
                start: 0x1000,
                end: 0x1010
            }]
        );
        let timespec = |sec: i64, nsec: i64| {
            let mut bytes = sec.to_le_bytes().to_vec();
            bytes.extend(nsec.to_le_bytes());
            bytes
        };
        let read_as = |bytes: Vec<u8>| {
            move |_: u64, want: usize| -> Result<Vec<u8>, ()> { Ok(bytes[..want].to_vec()) }
        };
        let earlier = summarize_output(None, &buffers, read_as(timespec(5, 100))).unwrap();
        let later = summarize_output(None, &buffers, read_as(timespec(5, 250))).unwrap();
        let again = summarize_output(None, &buffers, read_as(timespec(5, 100))).unwrap();
        assert_eq!(earlier.len, 16);
        assert_ne!(earlier.digest, later.digest);
        assert_eq!(earlier, again);

        let failed = time_outputs(Sysno::clock_gettime, [1, 0x1000, 0, 0, 0, 0], -14);
        assert_eq!(failed, (None, vec![]), "a failed read returned nothing");
        let rem = [1, 0, 0x2000, 0x3000, 0, 0];
        assert_eq!(time_outputs(Sysno::clock_nanosleep, rem, 0).1, vec![]);
        for interrupted in [-(libc::EINTR as i64), -516] {
            assert_eq!(
                time_outputs(Sysno::clock_nanosleep, rem, interrupted).1,
                vec![ByteRange {
                    start: 0x3000,
                    end: 0x3010
                }],
                "{interrupted}"
            );
        }
        let absolute = [1, libc::TIMER_ABSTIME as u64, 0x2000, 0x3000, 0, 0];
        assert_eq!(
            time_outputs(Sysno::clock_nanosleep, absolute, -(libc::EINTR as i64)).1,
            vec![],
            "an absolute sleep leaves its remaining time unwritten"
        );
        assert_eq!(
            time_outputs(Sysno::gettimeofday, [0x1000, 0x2000, 0, 0, 0, 0], 0)
                .1
                .len(),
            2
        );
        for (sysno, args, start) in [
            (Sysno::timer_gettime, [0, 0x4000, 0, 0, 0, 0], 0x4000),
            (Sysno::timerfd_gettime, [9, 0x4000, 0, 0, 0, 0], 0x4000),
            (Sysno::getitimer, [0, 0x4000, 0, 0, 0, 0], 0x4000),
            (Sysno::timer_settime, [0, 0, 0x5000, 0x4000, 0, 0], 0x4000),
            (Sysno::setitimer, [0, 0x5000, 0x4000, 0, 0, 0], 0x4000),
        ] {
            assert_eq!(
                time_outputs(sysno, args, 0).1,
                vec![ByteRange {
                    start,
                    end: start + 32
                }],
                "{sysno}"
            );
        }
        // times' result is output on every architecture; time's on x86_64.
        #[cfg(target_arch = "x86_64")]
        assert_eq!(
            time_outputs(Sysno::time, [0, 0, 0, 0, 0, 0], 1_700_000_000),
            (Some(1_700_000_000), vec![])
        );
        let (value, buffers) = time_outputs(Sysno::times, [0, 0, 0, 0, 0, 0], 1_700_000_000);
        assert_eq!((value, buffers.clone()), (Some(1_700_000_000), vec![]));
        let first = summarize_output::<()>(value, &buffers, |_, _| unreachable!()).unwrap();
        let second =
            summarize_output::<()>(Some(1_700_000_001), &[], |_, _| unreachable!()).unwrap();
        assert_eq!(first.len, 8);
        assert_ne!(first.digest, second.digest);
    }

    #[test]
    fn signal_note_reads_fault_address_and_sender() {
        let mut info = [0u8; 128];
        info[0..4].copy_from_slice(&libc::SIGSEGV.to_ne_bytes());
        info[8..12].copy_from_slice(&SEGV_MAPERR.to_ne_bytes());
        info[16..24].copy_from_slice(&0x10u64.to_ne_bytes());
        let segv = signal_note(
            libc::SIGSEGV,
            Some(&info),
            ObservedSignalDisposition::Default,
        );
        assert_eq!(
            (segv.code, segv.addr, segv.sender),
            (SEGV_MAPERR, Some(0x10), None)
        );
        // A SIGSEGV that kill sent (SI_USER) carries no fault address, even
        // though the union's bytes are set: there they are si_pid and si_uid.
        let mut info = [0u8; 128];
        info[8..12].copy_from_slice(&libc::SI_USER.to_ne_bytes());
        info[16..24].copy_from_slice(&0x0000_0007_0000_0004u64.to_ne_bytes());
        let sent = signal_note(
            libc::SIGSEGV,
            Some(&info),
            ObservedSignalDisposition::Default,
        );
        assert_eq!(sent.addr, None, "a user-sent SIGSEGV has no si_addr");
        assert_eq!(sent.sender.map(|s| s.pid), Some(DetPid::from_raw(4)));
        let mut info = [0u8; 128];
        info[8..12].copy_from_slice(&libc::SI_USER.to_ne_bytes());
        info[16..20].copy_from_slice(&4i32.to_ne_bytes());
        let usr1 = signal_note(
            libc::SIGUSR1,
            Some(&info),
            ObservedSignalDisposition::Handler,
        );
        assert_eq!(usr1.addr, None);
        assert_eq!(usr1.sender.map(|s| s.pid), Some(DetPid::from_raw(4)));
        let mut info = [0u8; 128];
        info[8..12].copy_from_slice(&libc::SI_TIMER.to_ne_bytes());
        info[16..20].copy_from_slice(&3i32.to_ne_bytes());
        let timer = signal_note(
            libc::SIGUSR1,
            Some(&info),
            ObservedSignalDisposition::Handler,
        );
        assert_eq!(timer.sender, None, "a timer expiry has no sending process");
        let none = signal_note(libc::SIGUSR1, None, ObservedSignalDisposition::Handler);
        assert_eq!((none.code, none.addr, none.sender), (0, None, None));
    }

    fn prehook(t: i32, sysno: Sysno) -> SchedEvent {
        SchedEvent::syscall(tid(t), sysno, SyscallPhase::Prehook)
    }

    fn posthook(t: i32, sysno: Sysno) -> SchedEvent {
        SchedEvent::syscall(tid(t), sysno, SyscallPhase::Posthook)
    }

    fn entry(sysno: Sysno, nth: u64) -> AccessPayload {
        AccessPayload::SyscallEntry(SyscallEntry {
            pid: DetPid::from_raw(3),
            nth,
            sysno,
            args: [0; 6],
            accesses: entry_accesses(sysno, DetPid::from_raw(3), tid(3)),
        })
    }

    fn exit(result: i64, accesses: Vec<Access>) -> AccessPayload {
        AccessPayload::SyscallExit(Box::new(SyscallOutcome {
            result,
            accesses,
            returned: None,
            out_bytes: None,
        }))
    }

    /// A trace file in memory.
    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A signal observation can precede another thread's sigaction and the
    /// eventual delivery. This models that handoff through the production
    /// classifier, payload construction and writer, without claiming a live
    /// guest replay: the trace must name the earlier disposition explicitly.
    #[test]
    fn signal_trace_names_the_disposition_observed_before_a_handoff() {
        let signal = libc::SIGUSR1;
        let bit = crate::syscalls::kernel_sigset_bit(signal);
        let shared_dispositions = Mutex::new((0, bit));
        let observed =
            crate::classify_traced_disposition(signal, Some(*shared_dispositions.lock().unwrap()));
        let payload = AccessPayload::Signal(signal_note(signal, None, observed));
        let mut event = prehook(3, Sysno::getpid);
        event.op = Op::SignalReceived(crate::types::SigWrapper(signal));
        let contents = Shared::default();
        let mut writer = AccessTraceWriter::new(Box::new(contents.clone()), 0, None, None).unwrap();
        writer.observe(0, &event, Some(payload), &Groups);

        // Thread B changes the shared disposition while A is held after its
        // SignalReceived event. A's earlier observation must not turn into a
        // claim about the action Linux will take when A resumes.
        *shared_dispositions.lock().unwrap() = (bit, 0);
        writer.finish(1).unwrap();
        let bytes = contents.0.lock().unwrap().clone();
        let trace = read_access_trace(bytes.as_slice()).unwrap();
        assert_eq!(trace.records.len(), 1);
        let record = serde_json::to_value(&trace.records[0]).unwrap();
        assert_eq!(record["observed_disposition"], "handler", "{record}");
        assert!(record.get("delivery").is_none(), "{record}");
        assert_eq!(
            serde_json::to_value(crate::classify_traced_disposition(
                signal,
                Some(*shared_dispositions.lock().unwrap()),
            ))
            .unwrap(),
            "ignored"
        );
    }

    /// Unreadable state is unknown, while each readable disposition retains
    /// its exact meaning through the production signal payload and writer.
    #[test]
    fn signal_trace_preserves_unknown_and_known_dispositions() {
        let signal = libc::SIGUSR1;
        let bit = crate::syscalls::kernel_sigset_bit(signal);
        for (dispositions, expected) in [
            (None, "unknown"),
            (Some((0, bit)), "handler"),
            (Some((bit, 0)), "ignored"),
            (Some((0, 0)), "default"),
        ] {
            let observed = crate::classify_traced_disposition(signal, dispositions);
            let payload = AccessPayload::Signal(signal_note(signal, None, observed));
            let mut event = prehook(3, Sysno::getpid);
            event.op = Op::SignalReceived(crate::types::SigWrapper(signal));
            let contents = Shared::default();
            let mut writer =
                AccessTraceWriter::new(Box::new(contents.clone()), 0, None, None).unwrap();
            writer.observe(0, &event, Some(payload), &Groups);
            writer.finish(1).unwrap();
            let bytes = contents.0.lock().unwrap().clone();
            let trace = read_access_trace(bytes.as_slice()).unwrap();
            assert_eq!(trace.records.len(), 1);
            let record = serde_json::to_value(&trace.records[0]).unwrap();
            assert_eq!(
                serde_json::to_value(observed).unwrap(),
                expected,
                "{record}"
            );
            assert_eq!(record["observed_disposition"], expected, "{record}");
            assert!(record.get("delivery").is_none(), "{record}");
        }
    }

    /// `--access-trace-max-records`: past the cap the trace stops without
    /// its end line, so a reader sees it cut short; a run whose records all
    /// fit still gets it.
    #[test]
    fn a_capped_trace_stops_without_its_end_line() {
        let run = |max: Option<u64>| {
            let file_contents = Shared::default();
            let mut writer =
                AccessTraceWriter::new(Box::new(file_contents.clone()), 0, max, None).unwrap();
            for n in 0..3 {
                let getpid = || entry(Sysno::getpid, n + 1);
                writer.observe(2 * n, &prehook(3, Sysno::getpid), Some(getpid()), &Groups);
                writer.observe(
                    2 * n + 1,
                    &posthook(3, Sysno::getpid),
                    Some(exit(3, vec![])),
                    &Groups,
                );
            }
            writer.finish(6).unwrap();
            let bytes = file_contents.0.lock().unwrap().clone();
            read_access_trace(&bytes[..]).unwrap()
        };
        let capped = run(Some(2));
        assert_eq!(capped.records.len(), 2, "{:?}", capped.records);
        assert!(capped.end.is_none(), "{:?}", capped.end);
        let fits = run(Some(3));
        assert_eq!(fits.records.len(), 3);
        assert_eq!(fits.end.map(|end| end.records), Some(3));
        let nothing = run(Some(0));
        assert!(nothing.records.is_empty() && nothing.end.is_none());
    }

    /// `--access-trace-max-bytes`: the trace holds at most that many bytes of
    /// header and records. The record that would pass it is not written, and
    /// the trace stops there without its end line; one that fits exactly is
    /// written. Without the option the cap is
    /// [`ACCESS_TRACE_DEFAULT_MAX_BYTES`], so no trace is unbounded.
    #[test]
    fn a_trace_stops_before_its_byte_cap() {
        let run = |max_bytes: Option<u64>| {
            let file_contents = Shared::default();
            let mut writer =
                AccessTraceWriter::new(Box::new(file_contents.clone()), 0, None, max_bytes)
                    .unwrap();
            assert_eq!(
                writer.max_bytes,
                max_bytes.unwrap_or(ACCESS_TRACE_DEFAULT_MAX_BYTES)
            );
            for n in 0..3 {
                let getpid = || entry(Sysno::getpid, n + 1);
                writer.observe(2 * n, &prehook(3, Sysno::getpid), Some(getpid()), &Groups);
                writer.observe(
                    2 * n + 1,
                    &posthook(3, Sysno::getpid),
                    Some(exit(3, vec![])),
                    &Groups,
                );
            }
            writer.finish(6).unwrap();
            file_contents.0.lock().unwrap().clone()
        };
        let whole = run(None);
        let lines: Vec<&[u8]> = whole.split_inclusive(|b| *b == b'\n').collect();
        assert_eq!(lines.len(), 5, "header, three records and the end line");
        let through = |n: usize| -> u64 { lines[..n].iter().map(|l| l.len() as u64).sum() };
        // Room for the header and exactly two records.
        let two = run(Some(through(3)));
        assert_eq!(two, whole[..through(3) as usize].to_vec());
        let trace = read_access_trace(&two[..]).unwrap();
        assert_eq!(trace.records.len(), 2);
        assert!(trace.end.is_none(), "{:?}", trace.end);
        // One byte short of the second record: only the first.
        let one = read_access_trace(&run(Some(through(3) - 1))[..]).unwrap();
        assert_eq!((one.records.len(), one.end), (1, None));
        // Every record fits: the trace is whole, end line included.
        assert_eq!(run(Some(through(4))), whole);
    }

    #[test]
    fn writer_numbers_records_names_pairs_and_marks_the_end() {
        let file_contents = Shared::default();
        let mut writer =
            AccessTraceWriter::new(Box::new(file_contents.clone()), 2, None, None).unwrap();
        let open = |fd, f| Access::Fd {
            slot: slot(fd),
            open_file: Some(f),
            op: FdOp::Open { path: None },
        };
        // Before `from`: not written, but its pair is still learned.
        writer.observe(
            0,
            &prehook(3, Sysno::socketpair),
            Some(entry(Sysno::socketpair, 1)),
            &Groups,
        );
        writer.observe(
            1,
            &posthook(3, Sysno::socketpair),
            Some(exit(0, vec![open(3, file(3, 1)), open(4, file(3, 2))])),
            &Groups,
        );
        // A write on the second end gets the pair's identity.
        writer.observe(
            2,
            &prehook(5, Sysno::write),
            Some(entry(Sysno::write, 1)),
            &Groups,
        );
        let send = Access::Channel {
            open_file: file(3, 2),
            id: None,
            channel: ChannelKind::Socket,
            op: ChannelOp::Write,
        };
        writer.observe(
            3,
            &posthook(5, Sysno::write),
            Some(exit(1, vec![send])),
            &Groups,
        );
        // A call that never returns.
        writer.observe(
            4,
            &prehook(3, Sysno::exit_group),
            Some(entry(Sysno::exit_group, 2)),
            &Groups,
        );
        writer.finish(5).unwrap();

        let bytes = file_contents.0.lock().unwrap().clone();
        let trace = read_access_trace(&bytes[..]).unwrap();
        assert_eq!(trace.header.from, 2);
        assert_eq!(
            trace.end.as_ref().map(|e| (e.events, e.records)),
            Some((5, 2))
        );
        let [TraceRecord::Syscall(write), TraceRecord::Syscall(exit)] = &trace.records[..] else {
            panic!("{:?}", trace.records);
        };
        assert_eq!((write.pre, write.post, write.result), (2, Some(3), Some(1)));
        assert_eq!(
            write.accesses,
            vec![Access::Channel {
                open_file: file(3, 2),
                id: Some(ChannelId::Pair(file(3, 1))),
                channel: ChannelKind::Socket,
                op: ChannelOp::Write,
            }]
        );
        assert_eq!((exit.pre, exit.post, exit.result), (4, None, None));
        assert_eq!(
            exit.accesses,
            vec![Access::Lifetime {
                target: Task::Process(DetPid::from_raw(3)),
                op: LifetimeOp::Exit
            }],
            "a call that never returns keeps what its prehook knew"
        );
    }

    fn at(t: i32, event: EventIndex) -> EventRef {
        EventRef { tid: tid(t), event }
    }

    fn entry_with(sysno: Sysno, pid: i32, args: [u64; 6]) -> Option<AccessPayload> {
        Some(AccessPayload::SyscallEntry(SyscallEntry {
            pid: DetPid::from_raw(pid),
            nth: 1,
            sysno,
            args,
            accesses: Vec::new(),
        }))
    }

    #[test]
    fn a_clone_starts_its_child_whichever_runs_first() {
        for child_first in [true, false] {
            let (mut writer, bytes) = in_memory_writer();
            writer.observe(
                10,
                &prehook(3, Sysno::clone),
                entry_with(Sysno::clone, 3, [0; 6]),
                &Groups,
            );
            let (child_event, parent_post) = if child_first { (11, 12) } else { (12, 11) };
            let child = SchedEvent::branches(tid(8), 1);
            let parent = posthook(3, Sysno::clone);
            if child_first {
                writer.observe(child_event, &child, None, &Groups);
                writer.observe(parent_post, &parent, Some(exit(8, vec![])), &Groups);
            } else {
                writer.observe(parent_post, &parent, Some(exit(8, vec![])), &Groups);
                writer.observe(child_event, &child, None, &Groups);
            }
            writer.finish(13).unwrap();
            assert_eq!(
                edges_of(&bytes),
                vec![EdgeRecord {
                    kind: EdgeKind::Create,
                    from: at(3, 10),
                    to: at(8, child_event),
                    object: SyncObject::Thread(tid(8)),
                }],
                "child first: {child_first}"
            );
        }
    }

    #[test]
    fn an_exit_wake_ends_at_the_joiners_wait_return() {
        let (mut writer, bytes) = in_memory_writer();
        let futex = FutexID::private(mm(), 0x5000);
        writer.observe(
            4,
            &prehook(5, Sysno::futex),
            entry_with(Sysno::futex, 3, [0; 6]),
            &Groups,
        );
        writer.observe(
            6,
            &prehook(8, Sysno::exit),
            entry_with(Sysno::exit, 3, [0; 6]),
            &Groups,
        );
        writer.note_wake(EdgeKind::ExitJoin, tid(8), tid(5), SyncObject::Futex(futex));
        writer.observe(
            9,
            &posthook(5, Sysno::futex),
            Some(exit(0, vec![])),
            &Groups,
        );
        writer.finish(10).unwrap();
        assert_eq!(
            edges_of(&bytes),
            vec![EdgeRecord {
                kind: EdgeKind::ExitJoin,
                from: at(8, 6),
                to: at(5, 9),
                object: SyncObject::Futex(futex),
            }]
        );
    }

    #[test]
    fn a_reaped_childs_exit_orders_the_wait() {
        let (mut writer, bytes) = in_memory_writer();
        writer.observe(
            2,
            &prehook(9, Sysno::exit_group),
            entry_with(Sysno::exit_group, 9, [0; 6]),
            &Groups,
        );
        writer.observe(
            3,
            &prehook(3, Sysno::wait4),
            entry_with(Sysno::wait4, 3, [9, 0, 0, 0, 0, 0]),
            &Groups,
        );
        let reaped = vec![Access::Lifetime {
            target: Task::Process(DetPid::from_raw(9)),
            op: LifetimeOp::Wait,
        }];
        writer.observe(
            7,
            &posthook(3, Sysno::wait4),
            Some(exit(9, reaped)),
            &Groups,
        );
        writer.finish(8).unwrap();
        assert_eq!(
            edges_of(&bytes),
            vec![EdgeRecord {
                kind: EdgeKind::ChildExit,
                from: at(9, 2),
                to: at(3, 7),
                object: SyncObject::Process(DetPid::from_raw(9)),
            }]
        );
    }

    /// Deliveries of `signo` to `tid`, with siginfo naming `code` and
    /// `union` (si_pid for a send, si_tid for a timer).
    fn delivered(signo: i32, code: i32, union: i32) -> Option<AccessPayload> {
        let mut info = [0u8; 128];
        info[8..12].copy_from_slice(&code.to_ne_bytes());
        info[16..20].copy_from_slice(&union.to_ne_bytes());
        Some(AccessPayload::Signal(signal_note(
            signo,
            Some(&info),
            ObservedSignalDisposition::Handler,
        )))
    }

    fn received(t: i32, signo: i32) -> SchedEvent {
        SchedEvent {
            op: Op::SignalReceived(crate::types::SigWrapper(signo)),
            ..SchedEvent::branches(tid(t), 1)
        }
    }

    /// A syscall `sysno` of thread 3 (process 3) with `args`, from its
    /// prehook at `pre` to its posthook at `pre + 1` with `result` and
    /// `accesses`.
    fn call(
        writer: &mut AccessTraceWriter,
        pre: EventIndex,
        sysno: Sysno,
        args: [u64; 6],
        result: i64,
        accesses: Vec<Access>,
    ) {
        writer.observe(pre, &prehook(3, sysno), entry_with(sysno, 3, args), &Groups);
        writer.observe(
            pre + 1,
            &posthook(3, sysno),
            Some(exit(result, accesses)),
            &Groups,
        );
    }

    /// Thread 4 of process 4 makes a syscall, so the writer knows its
    /// process.
    fn thread_4_runs(writer: &mut AccessTraceWriter, index: EventIndex) {
        writer.observe(
            index,
            &prehook(4, Sysno::getpid),
            entry_with(Sysno::getpid, 4, [0; 6]),
            &Groups,
        );
    }

    fn signal_edges(bytes: &Mutex<Vec<u8>>) -> Vec<(EventRef, EventRef)> {
        edges_of(bytes)
            .into_iter()
            .filter(|edge| edge.kind == EdgeKind::Signal)
            .map(|edge| (edge.from, edge.to))
            .collect()
    }

    /// A send that fails delivers nothing; a delivery after a successful
    /// one takes it. The record names the sending thread only where a
    /// send matched.
    #[test]
    fn a_failed_send_orders_no_delivery() {
        let usr1 = libc::SIGUSR1;
        let kill = [4, usr1 as u64, 0, 0, 0, 0];
        let (mut writer, bytes) = in_memory_writer();
        thread_4_runs(&mut writer, 1);
        call(
            &mut writer,
            2,
            Sysno::kill,
            kill,
            -(libc::EPERM as i64),
            vec![],
        );
        writer.observe(
            4,
            &received(4, usr1),
            delivered(usr1, libc::SI_USER, 3),
            &Groups,
        );
        call(&mut writer, 5, Sysno::kill, kill, 0, vec![]);
        writer.observe(
            7,
            &received(4, usr1),
            delivered(usr1, libc::SI_USER, 3),
            &Groups,
        );
        writer.finish(8).unwrap();
        assert_eq!(signal_edges(&bytes), vec![(at(3, 5), at(4, 7))]);
        let senders: Vec<Option<DetTid>> = read_access_trace(&bytes.lock().unwrap()[..])
            .unwrap()
            .records
            .iter()
            .filter_map(|record| match record {
                TraceRecord::Signal(signal) => Some(signal.sender.and_then(|s| s.tid)),
                _ => None,
            })
            .collect();
        assert_eq!(senders, vec![None, Some(tid(3))]);
    }

    /// Two sends of a standard signal from one sender, then two
    /// deliveries: on ptrace the first delivery can be dequeued before the
    /// second send queues another, so each delivery takes the earliest
    /// send left, and neither edge claims the later send for the first.
    #[test]
    fn two_sends_from_one_sender_order_two_deliveries_in_send_order() {
        let usr1 = libc::SIGUSR1;
        let kill = [4, usr1 as u64, 0, 0, 0, 0];
        let (mut writer, bytes) = in_memory_writer();
        thread_4_runs(&mut writer, 1);
        call(&mut writer, 2, Sysno::kill, kill, 0, vec![]);
        call(&mut writer, 5, Sysno::kill, kill, 0, vec![]);
        writer.observe(
            8,
            &received(4, usr1),
            delivered(usr1, libc::SI_USER, 3),
            &Groups,
        );
        writer.observe(
            9,
            &received(4, usr1),
            delivered(usr1, libc::SI_USER, 3),
            &Groups,
        );
        writer.finish(10).unwrap();
        assert_eq!(
            signal_edges(&bytes),
            vec![(at(3, 2), at(4, 8)), (at(3, 5), at(4, 9))]
        );
    }

    /// Two threads of one process send: the first thread's kill may run
    /// after the second's (a `--happens-before` checkpoint between its
    /// prehook and its kill), so while its posthook is not before the
    /// second's prehook, the first delivery is used up without an edge.
    /// The next takes the second send.
    #[test]
    fn overlapping_sends_from_two_threads_order_nothing() {
        let usr1 = libc::SIGUSR1;
        let kill = [4, usr1 as u64, 0, 0, 0, 0];
        let (mut writer, bytes) = in_memory_writer();
        thread_4_runs(&mut writer, 1);
        writer.observe(
            2,
            &prehook(3, Sysno::kill),
            entry_with(Sysno::kill, 3, kill),
            &Groups,
        );
        writer.observe(
            3,
            &prehook(5, Sysno::kill),
            entry_with(Sysno::kill, 3, kill),
            &Groups,
        );
        writer.observe(4, &posthook(5, Sysno::kill), Some(exit(0, vec![])), &Groups);
        writer.observe(
            5,
            &received(4, usr1),
            delivered(usr1, libc::SI_USER, 3),
            &Groups,
        );
        writer.observe(6, &posthook(3, Sysno::kill), Some(exit(0, vec![])), &Groups);
        writer.observe(
            7,
            &received(4, usr1),
            delivered(usr1, libc::SI_USER, 3),
            &Groups,
        );
        writer.finish(8).unwrap();
        assert_eq!(signal_edges(&bytes), vec![(at(5, 3), at(4, 7))]);
    }

    /// A send to the process and one to the thread queue separately (the
    /// shared and the private pending sets), so the thread gets two
    /// deliveries, one for each send.
    #[test]
    fn a_process_send_and_a_thread_send_each_order_one_delivery() {
        let usr1 = libc::SIGUSR1;
        let (mut writer, bytes) = in_memory_writer();
        thread_4_runs(&mut writer, 1);
        call(
            &mut writer,
            2,
            Sysno::kill,
            [4, usr1 as u64, 0, 0, 0, 0],
            0,
            vec![],
        );
        call(
            &mut writer,
            4,
            Sysno::tgkill,
            [4, 4, usr1 as u64, 0, 0, 0],
            0,
            vec![],
        );
        writer.observe(
            6,
            &received(4, usr1),
            delivered(usr1, libc::SI_TKILL, 3),
            &Groups,
        );
        writer.observe(
            7,
            &received(4, usr1),
            delivered(usr1, libc::SI_USER, 3),
            &Groups,
        );
        writer.finish(8).unwrap();
        assert_eq!(
            signal_edges(&bytes),
            vec![(at(3, 2), at(4, 6)), (at(3, 4), at(4, 7))]
        );
    }

    /// Realtime signals queue: each delivery takes the earliest send left.
    #[test]
    fn realtime_signal_deliveries_take_their_sends_in_order() {
        let rt = 34; // SIGRTMIN + 2
        let send = [4, rt as u64, 0, 0, 0, 0];
        let (mut writer, bytes) = in_memory_writer();
        thread_4_runs(&mut writer, 1);
        call(&mut writer, 2, Sysno::kill, send, 0, vec![]);
        call(&mut writer, 4, Sysno::kill, send, 0, vec![]);
        writer.observe(
            6,
            &received(4, rt),
            delivered(rt, libc::SI_USER, 3),
            &Groups,
        );
        writer.observe(
            7,
            &received(4, rt),
            delivered(rt, libc::SI_USER, 3),
            &Groups,
        );
        writer.observe(
            8,
            &received(4, rt),
            delivered(rt, libc::SI_USER, 3),
            &Groups,
        );
        writer.finish(9).unwrap();
        assert_eq!(
            signal_edges(&bytes),
            vec![(at(3, 2), at(4, 6)), (at(3, 4), at(4, 7))]
        );
    }

    /// A timer's expiry takes its oldest live arm. The oldest arm retires
    /// once an expiry recorded after a newer arm's posthook has matched it;
    /// an expiry between the re-arm's prehook and posthook keeps it. After
    /// timer_delete one more expiry, dequeued before the delete, can still
    /// match, and then nothing does.
    #[test]
    fn a_timer_expiry_takes_its_oldest_live_arm() {
        let usr2 = libc::SIGUSR2;
        let arm = [0, 0, 0x3000, 0, 0, 0];
        let expiry = || delivered(usr2, libc::SI_TIMER, 0);
        let (mut writer, bytes) = in_memory_writer();
        call(&mut writer, 2, Sysno::timer_settime, arm, 0, vec![]);
        writer.observe(
            4,
            &prehook(3, Sysno::timer_settime),
            entry_with(Sysno::timer_settime, 3, arm),
            &Groups,
        );
        writer.observe(5, &received(3, usr2), expiry(), &Groups);
        writer.observe(
            6,
            &posthook(3, Sysno::timer_settime),
            Some(exit(0, vec![])),
            &Groups,
        );
        writer.observe(7, &received(3, usr2), expiry(), &Groups);
        writer.observe(8, &received(3, usr2), expiry(), &Groups);
        call(&mut writer, 9, Sysno::timer_delete, [0; 6], 0, vec![]);
        writer.observe(11, &received(3, usr2), expiry(), &Groups);
        assert!(
            writer.edges.signals.is_empty(),
            "{:?}",
            writer.edges.signals
        );
        writer.observe(12, &received(3, usr2), expiry(), &Groups);
        writer.finish(13).unwrap();
        assert_eq!(
            signal_edges(&bytes),
            vec![
                (at(3, 2), at(3, 5)),
                (at(3, 2), at(3, 7)),
                (at(3, 4), at(3, 8)),
                (at(3, 4), at(3, 11)),
            ]
        );
    }

    /// Past the cap a send goes before a live timer's arm, which can cause
    /// expiries for the rest of the run.
    #[test]
    fn the_cap_evicts_sends_before_a_live_timers_arm() {
        let (mut writer, _) = in_memory_writer();
        call(
            &mut writer,
            0,
            Sysno::timer_settime,
            [0, 0, 0x3000, 0, 0, 0],
            0,
            vec![],
        );
        for n in 1..=MAX_PENDING_SIGNALS as u64 {
            call(
                &mut writer,
                2 * n,
                Sysno::kill,
                [4, 34, 0, 0, 0, 0],
                0,
                vec![],
            );
        }
        let signals = &writer.edges.signals;
        assert_eq!(signals.len(), MAX_PENDING_SIGNALS);
        assert_eq!(signals[0].timer, Some(0), "the arm stayed");
        assert_eq!(signals[1].from, at(3, 4), "the oldest send went");
    }

    /// A signal to the caller's own group reaches each member listed at its
    /// posthook once, and is used up after the last one.
    #[test]
    fn a_group_send_reaches_each_member_once() {
        let usr1 = libc::SIGUSR1;
        let (mut writer, bytes) = in_memory_writer();
        thread_4_runs(&mut writer, 1);
        let group = vec![Access::Lifetime {
            target: Task::ProcessGroup(OWN_PROCESS_GROUP),
            op: LifetimeOp::Signal { signo: usr1 },
        }];
        call(
            &mut writer,
            2,
            Sysno::kill,
            [0, usr1 as u64, 0, 0, 0, 0],
            0,
            group,
        );
        assert_eq!(
            writer.edges.signals[0].members,
            Some(vec![DetPid::from_raw(3), DetPid::from_raw(4)])
        );
        writer.observe(
            4,
            &received(4, usr1),
            delivered(usr1, libc::SI_USER, 3),
            &Groups,
        );
        writer.observe(
            5,
            &received(3, usr1),
            delivered(usr1, libc::SI_USER, 3),
            &Groups,
        );
        assert!(
            writer.edges.signals.is_empty(),
            "{:?}",
            writer.edges.signals
        );
        writer.observe(
            6,
            &received(4, usr1),
            delivered(usr1, libc::SI_USER, 3),
            &Groups,
        );
        writer.finish(7).unwrap();
        assert_eq!(
            signal_edges(&bytes),
            vec![(at(3, 2), at(4, 4)), (at(3, 2), at(3, 5))]
        );
    }

    /// Reaping a member takes it out of a group send still waiting for it,
    /// and drops the sends to it.
    #[test]
    fn reaping_forgets_the_sends_waiting_for_the_child() {
        let usr1 = libc::SIGUSR1;
        let (mut writer, _) = in_memory_writer();
        thread_4_runs(&mut writer, 1);
        let group = vec![Access::Lifetime {
            target: Task::ProcessGroup(OWN_PROCESS_GROUP),
            op: LifetimeOp::Signal { signo: usr1 },
        }];
        call(
            &mut writer,
            2,
            Sysno::kill,
            [0, usr1 as u64, 0, 0, 0, 0],
            0,
            group,
        );
        call(
            &mut writer,
            4,
            Sysno::kill,
            [4, usr1 as u64, 0, 0, 0, 0],
            0,
            vec![],
        );
        writer.observe(
            6,
            &received(3, usr1),
            delivered(usr1, libc::SI_USER, 3),
            &Groups,
        );
        assert_eq!(writer.edges.signals.len(), 2, "a's send waits for 4");
        let reaped = vec![Access::Lifetime {
            target: Task::Process(DetPid::from_raw(4)),
            op: LifetimeOp::Wait,
        }];
        call(&mut writer, 7, Sysno::wait4, [4, 0, 0, 0, 0, 0], 4, reaped);
        assert!(
            writer.edges.signals.is_empty(),
            "{:?}",
            writer.edges.signals
        );
    }
}
