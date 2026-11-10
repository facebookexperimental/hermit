/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Treat all Clippy warnings as errors.
#![deny(clippy::all)]
#![allow(clippy::uninlined_format_args)]
#![allow(
    unexpected_cfgs,
    reason = "`fbcode_build` is supplied by the internal Buck build"
)]

// The preinit section belongs to executable images, never to the shared rlib:
// hermit-cdylib also links the library and must not acquire a preinit array.
#[used]
#[unsafe(link_section = ".preinit_array")]
static CAPTURE_STARTUP_STDIN: unsafe extern "C" fn() = hermit::app::capture_startup_stdin;

fn main() {
    if let Some(status) = hermit::app::executable_entry() {
        std::process::exit(status);
    }
    // Exact non-async #[fbinit::main] initialization/destruction semantics.
    // That macro requires the crate-root function to be named main, so its
    // expansion follows the early executable entry instead of hiding it behind
    // an already initialized application. Preserve its default signal mask.
    if module_path!().contains("::") {
        panic!("fbinit must be performed in the crate root on the main function");
    }
    unsafe { fbinit::perform_init_with_disable_signals(0x8002) };
    let _destroy_guard = unsafe { fbinit::DestroyGuard::new() };
    hermit::app::main();
}
