/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use reverie::syscalls::ioctl;
use reverie::syscalls::PollFd;
use reverie::syscalls::StatBuf;
use reverie::syscalls::StatxBuf;
use reverie::syscalls::Timespec;
use reverie::syscalls::Timeval;
use reverie::syscalls::Timezone;
use reverie::Errno;
use reverie::RdtscResult;
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
    Write(i64),
    Mmap(MmapEvent),
    /// A syscall whose only value we care about is the return value. For many
    /// syscalls, this is often the only output of the syscall and thus it is the
    /// only piece of information that needs to be recorded.
    Return(i64),
    Stat(StatEvent),
    Statx(StatxBuf),
    Rdtsc(RdtscResult),
    Ioctl(ioctl::Output),
    Timespec(TimespecEvent),
    Timeofday((Timeval, Timezone)),
    Poll(PollEvent),
    SockOpt(SockOptEvent),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MmapEvent {
    /// The address where the memory shall be mapped.
    pub addr: usize,
    /// The contents of the memory map. Note that this may be less than the
    /// requested `length`.
    pub buf: Vec<u8>,
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
pub struct SockOptEvent {
    /// The (possibly truncated) value.
    pub value: Vec<u8>,

    /// The length of the value. If this is the same as `value.len()`, then
    /// no truncation of the value occurred.
    pub length: libc::socklen_t,
}
