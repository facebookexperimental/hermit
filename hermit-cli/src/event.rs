/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use reverie::Errno;
use reverie::RdtscResult;
use reverie::syscalls::PollFd;
use reverie::syscalls::StatBuf;
use reverie::syscalls::StatxBuf;
use reverie::syscalls::Timespec;
use reverie::syscalls::Timeval;
use reverie::syscalls::Timezone;
use reverie::syscalls::ioctl;
use serde::Deserialize;
use serde::Serialize;

/// An event. This contains everything needed to verify and reproduce the
/// execution of a syscall.
#[derive(Debug, Serialize, Deserialize)]
pub struct Event {
    /// The event that we use to reconstruct the outputs of the original syscall.
    /// This is `Some` if need to record this syscall. If the syscall is already
    /// deterministic, then this is `None`.
    ///
    /// If a recorded syscall failed, then this is `Some(Err(Errno))`. That is,
    /// the failure should be reproduced during replay.
    pub event: Result<SyscallEvent, Errno>,
}

/// A `SyscallEvent` contains all the necessary information to replay a system
/// call.
///
/// Note that we only need a small amount of information to replay a syscall. The
/// only side effects observable by the user are:
///  1. Mutable pointers
///  2. Return values.
///
/// No registers are modified by the kernel except for `rax` (the return value).
/// Therefore, registers themselves do not need to be recorded since they are
/// strictly inputs. However, any arguments that are pointers that point to
/// mutable data expected to be modified by the kernel need to be recorded. If
/// this rule is applied uniformly for all syscalls, then we should be able to
/// implement full record and replay.
#[derive(Debug, Serialize, Deserialize)]
pub enum SyscallEvent {
    Bytes(Vec<u8>),
    /// The flattened output bytes of a vectored read (`readv`/`preadv`/
    /// `preadv2`). The bytes are stored contiguously in read order; on replay
    /// they are scattered back across the guest's `iovec` buffers. The length of
    /// the vector is exactly the return value of the syscall.
    Readv(Vec<u8>),
    Write(i64),
    Mmap(MmapEvent),
    Recvmsg(RecvmsgEvent),
    /// A syscall whose only value we care about is the return value. For many
    /// syscalls, this is often the only output of the syscall and thus it is the
    /// only piece of information that needs to be recorded.
    Return(i64),
    Stat(StatEvent),
    Statfs(Vec<u8>),
    Statx(StatxBuf),
    Rdtsc(RdtscResult),
    Ioctl(ioctl::Output),
    Timespec(TimespecEvent),
    Timeofday((Timeval, Timezone)),
    Poll(PollEvent),
    SockOpt(SockOptEvent),
    EpollWait(EpollWaitEvent),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MmapEvent {
    /// The address where the memory shall be mapped.
    pub addr: usize,
    /// The contents of the memory map. Note that this may be less than the
    /// requested `length`.
    pub buf: Vec<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RecvmsgEvent {
    pub result: i64,
    pub iovs: Vec<Vec<u8>>,
    pub name: Vec<u8>,
    pub name_len: libc::socklen_t,
    pub control: Vec<u8>,
    pub control_len: usize,
    pub flags: libc::c_int,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct StatEvent {
    #[serde(with = "StatBuf")]
    pub statbuf: libc::stat,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct TimespecEvent {
    pub timespec: Timespec,
}

/// Records the guest-visible outputs of a `poll` or `ppoll` call. Both syscalls
/// have identical output semantics (the updated `pollfd` array plus a return
/// count), so they share this event. `ppoll`'s temporary signal mask only
/// affects which signals can interrupt the wait; a resulting `EINTR` (or a
/// timeout returning 0) is captured by the enclosing `Event`'s `Result` and
/// return count respectively, so no extra fields are needed here.
#[derive(Serialize, Deserialize, Debug)]
pub struct PollEvent {
    /// The complete list of file descriptors. Note that only the `revents` field
    /// in `pollfd` is an output of the syscall. Technically, we only need to
    /// store the `revents` field, but it is easier to store everything for
    /// replay purposes (only one simple call to `process_vm_writev` is needed).
    /// It is possible to do a vectored write, skipping the other fields, but
    /// that is a little more complicated. For programs that need to wait on many
    /// file descriptors at once, they should be using `epoll` instead.
    pub fds: Vec<PollFd>,

    /// The return value (i.e., the number of items in the above list that have
    /// been updated).
    ///
    /// A value of 0 indicates that the call timed out and no file descriptors
    /// were ready.
    pub updated: usize,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct EpollWaitEvent {
    /// Raw initialized epoll_event bytes returned by the kernel.
    pub events: Vec<u8>,
    /// The number of initialized events in the buffer.
    pub updated: usize,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SockOptEvent {
    /// The (possibly truncated) value.
    pub value: Vec<u8>,

    /// The length of the value. If this is the same as `value.len()`, then
    /// no truncation of the value occurred.
    pub length: libc::socklen_t,
}
