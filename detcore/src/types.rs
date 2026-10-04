/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Widely-shared type definitions.

use serde::Deserialize;
use serde::Serialize;

/// Child population addressed by a wait syscall whose exit readiness Detcore can model.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum ChildWaitSelector {
    /// One direct child process.
    Exact(DetPid),
    /// Any direct child process.
    Any,
    /// Any direct child process currently in this process group.
    ProcessGroup(DetPid),
}

/// Linux's clone-child filtering for terminal wait events.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum ChildWaitExitClass {
    /// Children whose clone exit signal is SIGCHLD (the normal wait population).
    Sigchld,
    /// Children whose clone exit signal is zero or a signal other than SIGCHLD.
    Clone,
    /// Both normal and clone children (`__WALL`).
    Any,
}

/// Scheduler-owned selection criteria for a terminal child wait.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct ChildWaitSpec {
    /// PID or process-group population selected by the syscall.
    pub selector: ChildWaitSelector,
    /// Restrict to children owned by this exact task (`__WNOTHREAD`).
    pub owner: Option<DetTid>,
    /// Filter normal versus clone children.
    pub exit_class: ChildWaitExitClass,
}

/// Scheduler-owned lifecycle state for an exact child-process wait.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ExactChildWaitState {
    /// The requested process is not a known direct child of the caller.
    Unknown,
    /// At least one thread in the child process remains logically live.
    Running,
    /// Detcore observed logical exit, but this backend cannot report physical waitability.
    LogicallyExited,
    /// The backend has not yet reported the child's final kernel exit status.
    PhysicalExitPending,
    /// The backend has reported the child's final kernel exit status.
    PhysicallyExited,
}

pub use detcore_model::fd::*;
pub use detcore_model::futex::*;
pub use detcore_model::pid::*;
pub use detcore_model::schedule::SigWrapper;
pub use detcore_model::schedule::*;
pub use detcore_model::time::*;
