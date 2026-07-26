/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Hermit-owned constructor for the Detcore LiteInst preload runtime.

#![deny(missing_docs)]

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-736): Review the LiteInst Detcore constructor boundary.
/// Installs `Detcore` before guest application code starts.
#[used]
#[unsafe(link_section = ".init_array")]
static DETCORE_LITEINST_INIT: unsafe extern "C" fn() = initialize;

unsafe extern "C" fn initialize() {
    let Some(socket) = std::env::var_os(reverie_liteinst::COORDINATOR_ENV) else {
        fail("coordinator socket environment variable is missing");
    };
    if let Err(error) = unsafe { reverie_liteinst::install_tool::<detcore::Detcore>(socket) } {
        eprintln!("detcore-liteinst: initialization failed: {error}");
        unsafe { libc::_exit(127) };
    }
}

fn fail(message: &str) -> ! {
    eprintln!("detcore-liteinst: initialization failed: {message}");
    unsafe { libc::_exit(127) }
}
