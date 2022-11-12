/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fmt;
use std::str::FromStr;

use nix::unistd;
use serde::Deserialize;
use serde::Serialize;

// Deterministic Pids/Tids:
//--------------------------------------------------------------------------------

/// Deterministic "virtual" version of `reverie::Pid`
#[derive(
    PartialEq, // Silly protection from rustfmt disagreements.
    Debug,
    Eq,
    Clone,
    Copy,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    Default,
)]
pub struct DetPid(i32);

impl fmt::Display for DetPid {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

#[cfg(disabled)]
impl From<reverie_syscalls::Pid> for DetPid {
    fn from(p: reverie_syscalls::Pid) -> Self {
        DetPid(p.into())
    }
}

impl From<unistd::Pid> for DetPid {
    fn from(p: unistd::Pid) -> Self {
        DetPid(p.into())
    }
}

// implementing From<DetPid> for unistd::Pid would violate foreign trait rules
#[allow(clippy::from_over_into)]
impl Into<unistd::Pid> for DetPid {
    fn into(self) -> unistd::Pid {
        unistd::Pid::from_raw(self.0)
    }
}

impl DetPid {
    /// Create a DetPid from a raw pid.
    pub const fn from_raw(pid: i32) -> DetPid {
        DetPid(pid)
    }

    /// Convert to a row integer.
    pub fn as_raw(&self) -> i32 {
        self.0
    }
}

impl FromStr for DetPid {
    type Err = <i32 as FromStr>::Err;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::from_raw(s.parse::<i32>()?))
    }
}

/// Deterministic "virtual" version of `reverie::Tid`
pub type DetTid = DetPid;
