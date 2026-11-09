/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Admission for a virtual bound whose scheduler must observe final signal delivery.

use std::num::NonZeroU64;

/// A debugger can suppress a fatal signal after Detcore has marked its process
/// doomed. Until final delivery is reconciled, that combination cannot promise
/// to stop a surviving guest at its virtual bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualTimeoutWithDebugger;

impl std::fmt::Display for VirtualTimeoutWithDebugger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "HERMIT_VIRTUAL_TIMEOUT_WITH_DEBUGGER: --virtual-timeout is not supported with \
             --gdbserver or --dap-listen: a debugger can suppress a fatal signal after Detcore \
             marks its process doomed; remove --virtual-timeout or the debugger option, or \
             use --timeout for a wall-clock bound",
        )
    }
}

impl std::error::Error for VirtualTimeoutWithDebugger {}

/// Refuse the combination before a guest or debugger is started. The caller
/// includes a pending DAP listener in `debugger`, even before it is lowered to
/// `Config::gdbserver`. Neither option alone changes its admission here.
pub fn refuse_debugger(
    bound: Option<NonZeroU64>,
    debugger: bool,
) -> Result<(), VirtualTimeoutWithDebugger> {
    if bound.is_some() && debugger {
        Err(VirtualTimeoutWithDebugger)
    } else {
        Ok(())
    }
}
