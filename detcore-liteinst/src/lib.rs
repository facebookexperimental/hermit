/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! In-guest Detcore runtime for the LiteInst backend.
//!
//! Hermit preloads this library into the guest when `HERMIT_LITEINST_IN_GUEST=1`
//! selects LiteInst's in-guest runtime. When the dynamic loader loads it, its
//! constructor installs Detcore's `Tool` inside the guest process before the
//! program's `main`. The loader, the C library's initialization, `IFUNC`
//! resolvers and the constructors of the program's own shared libraries run
//! earlier, unmonitored. A loader that fails to load the library runs the
//! guest without it; Hermit reports that the guest never connected only after
//! the guest exits. Once Detcore is installed, patched system calls and the
//! SIGSYS fallback dispatch to Detcore in the guest, and Detcore's global
//! state stays in Hermit, reached over the coordinator RPC socket that
//! `reverie_liteinst::LiteinstBackend::run_with_preload` creates. No ptrace
//! tracer sits on the system-call path.
#![deny(missing_docs)]

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-3635): Review the
// in-guest Detcore constructor boundary.

/// Hermit validates that this constructor is registered in `.init_array`
/// before it preloads the library, so a DSO that would load without
/// installing Detcore is refused instead of running the guest unmonitored.
#[used]
#[unsafe(link_section = ".init_array")]
static DETCORE_LITEINST_INIT: unsafe extern "C" fn() = detcore_liteinst_initialize;

/// Installs Detcore in the current guest process.
///
/// The `.init_array` entry above runs this once per process image, before
/// `main`. It exits the process with status 127 when the coordinator socket is
/// missing or Detcore cannot be installed, so a guest never runs past a failed
/// installation.
///
/// # Safety
///
/// Only the dynamic loader may call this, from `.init_array`, while the process
/// is still single-threaded and before any seccomp filter is active.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn detcore_liteinst_initialize() {
    let Some(socket) = std::env::var_os(reverie_liteinst::COORDINATOR_ENV) else {
        fail("the coordinator socket environment variable is missing");
    };
    // SAFETY: the loader runs constructors before any application thread
    // exists and before the application can install a seccomp filter, which is
    // the window `install_tool` requires.
    if let Err(error) = unsafe { reverie_liteinst::install_tool::<detcore::Detcore>(socket) } {
        fail(&error.to_string());
    }
}

fn fail(message: &str) -> ! {
    eprintln!("detcore-liteinst: initialization failed: {message}");
    // SAFETY: `_exit` takes no pointers and does not return.
    unsafe { libc::_exit(127) }
}
