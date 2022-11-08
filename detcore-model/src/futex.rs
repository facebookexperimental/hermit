// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

use crate::pid::DetPid;

// TODO: use newtype pattern here.
/// Because we don't yet support inter-process sharing of futexes, a futex is uniquely
/// identified by Pid and virtual address (within that Pid's address space).
pub type FutexID = (DetPid, usize);
