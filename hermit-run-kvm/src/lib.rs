/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Hermit's KVM run, and the one place its Detcore instantiation is rooted.
//!
//! `detcore::Detcore` under `reverie_kvm`'s tool runtime is a large
//! instantiation. In its own crate it builds in a separate rustc process,
//! beside the other backends' instantiations. The entry point,
//! [`run_detcore`], is not generic and returns a boxed future, so its callers
//! instantiate nothing of it (see `hermit-record` for why it is boxed).
//!
//! `reverie_kvm` is an x86-64 VMM and empty elsewhere, and so is this crate.
#![cfg(target_arch = "x86_64")]

use std::future::Future;
use std::pin::Pin;

use reverie_kvm::KvmBackend;
use reverie_kvm::ToolRunCompletion;

/// The future [`run_detcore`] returns. It borrows the backend it runs on.
pub type RunDetcore<'a> = Pin<
    Box<dyn Future<Output = reverie_kvm::Result<ToolRunCompletion<detcore::GlobalState>>> + 'a>,
>;

/// Runs the static ELF already installed in BACKEND to completion under
/// Detcore with CONFIG. With CAPTURE_OUTPUT the guest's stdout and stderr are
/// returned in the completion instead of written through.
///
/// The future is boxed so the async body, and everything it instantiates, is
/// compiled in this crate rather than in its caller.
pub fn run_detcore(
    backend: &mut KvmBackend,
    config: detcore::Config,
    capture_output: bool,
) -> RunDetcore<'_> {
    Box::pin(
        backend.run_static_elf_with_tool_completion::<detcore::Detcore>(config, capture_output),
    )
}
