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

#[cfg(any())]
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

/// How the guest's process and thread IDs are numbered
/// (`Config::guest_ids`).
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize
)]
pub enum GuestIds {
    /// The PID namespace Hermit creates for the run numbers the guest: the
    /// kernel gives each new task the next free ID in that namespace, so the
    /// IDs depend only on the guest's own creation order.
    #[default]
    Namespace,
    /// The host's IDs, which differ from run to run: `--no-namespace` where
    /// Detcore cannot choose them, on a backend whose guest tasks are host
    /// tasks.
    Host,
    /// Detcore chooses every ID (`--no-namespace`): the guest's first process
    /// gets `base`, and automatic choices take the next ID in creation order,
    /// skipping IDs already used by guest-authored `set_tid` calls. Linux
    /// assigns the choices through `clone3`'s `set_tid`. No ID is given out twice in a run,
    /// so the IDs depend only on `base` and the guest's own creation order.
    Chosen {
        /// The first process's ID.
        base: i32,
        /// When an ID cannot be chosen, refuse the run (`--strict`, or an
        /// explicit `--pid-base`) rather than let that task have a host ID.
        refuse_host_ids: bool,
    },
}

impl GuestIds {
    /// The first process's ID, when Detcore chooses the IDs.
    pub fn base(&self) -> Option<i32> {
        match self {
            GuestIds::Chosen { base, .. } => Some(*base),
            GuestIds::Namespace | GuestIds::Host => None,
        }
    }

    /// Whether two runs number their guests alike, whatever each does when an
    /// ID cannot be chosen: a schedule recorded under one replays under the
    /// other.
    pub fn numbers_like(&self, other: &GuestIds) -> bool {
        match (self, other) {
            (GuestIds::Chosen { base: a, .. }, GuestIds::Chosen { base: b, .. }) => a == b,
            (a, b) => a == b,
        }
    }
}

impl fmt::Display for GuestIds {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            GuestIds::Namespace => write!(f, "numbered by the run's PID namespace"),
            GuestIds::Host => write!(f, "host-assigned, different in every run"),
            GuestIds::Chosen { base, .. } => {
                write!(
                    f,
                    "chosen by Hermit from {base} in creation order (clone3 set_tid)"
                )
            }
        }
    }
}
