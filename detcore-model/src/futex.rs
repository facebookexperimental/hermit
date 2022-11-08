/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use crate::pid::DetPid;

// TODO: use newtype pattern here.
/// Because we don't yet support inter-process sharing of futexes, a futex is uniquely
/// identified by Pid and virtual address (within that Pid's address space).
pub type FutexID = (DetPid, usize);
