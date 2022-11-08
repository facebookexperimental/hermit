/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Constant values.

#![allow(unused)]

use reverie::Pid;

use crate::types::DetInode;
use crate::types::DetPid;
use crate::types::DetTid;

/// A separate offset for special devices, including file descriptors 0,1,2
/// stdin/stdout/stderr.
pub static DET_SPECIAL_INODE_OFFSET: DetInode = 1000;

/// The starting point for deterministic inodes *other* than those addressed by
/// `DET_SPECIAL_INODE_OFFSET`.
pub static DET_INODE_OFFSET: DetInode = 9000;

pub const DEFAULT_HOSTNAME: &str = "hermetic-container.local";

/// A convention of how we set up our PID namespace leaves us with a starting pid of 3.
pub const ROOT_DETPID: DetPid = DetPid::from_raw(3);
