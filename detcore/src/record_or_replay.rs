/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use reverie::Error;
use reverie::Guest;
use reverie::Subscription;
use reverie::Tool;
use reverie::syscalls::Syscall;
use serde::Deserialize;
use serde::Serialize;

use crate::config::Config;
use crate::tool_global::GlobalState;

/// Helper trait.
pub trait RecordOrReplay: Tool<GlobalState = GlobalState> {}

impl<T> RecordOrReplay for T where T: Tool<GlobalState = GlobalState> {}

/// Whether the record or replay tool `T` is [`NoopTool`], whose
/// `handle_syscall_event` does nothing but inject the call it receives and
/// return the injection's result, unchanged, with no other effect. Detcore
/// then injects a forwarded call itself rather than through the tool: the
/// same thing, without the two boxed futures (the tool's handler and
/// `IntoGuest`'s `inject`) that routing it through the tool costs on every
/// forwarded syscall. Every other tool, including a custom one, still
/// handles each forwarded call itself. This names a record or replay tool,
/// never a backend, and leaves `RecordOrReplay` exactly as it was.
///
/// `T` is recognised by its type name (`detcore::record_or_replay::NoopTool`),
/// among zero-sized types, because `TypeId` would need a `T: 'static` bound
/// on Detcore's generic code, and adding one changes Detcore's public API.
///
/// Known limitation, accepted: a type name is not unique across crates. A
/// zero-sized record or replay tool defined at `record_or_replay::NoopTool`
/// in another library whose crate name is also `detcore` (`[lib] name =
/// "detcore"`) has the same name. If such a tool did more than inject, its
/// `handle_syscall_event` would be bypassed and every call Detcore forwards
/// would be injected directly instead. Detcore has no consumers outside the
/// Hermit workspace, and a collision needs a crate that deliberately takes
/// Detcore's crate name and module path, so this is declared rather than
/// prevented with an API change.
pub(crate) fn injects_only<T>() -> bool {
    std::mem::size_of::<T>() == 0 && std::any::type_name::<T>() == std::any::type_name::<NoopTool>()
}

/// A tool that only injects the syscall it receives.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct NoopTool;

#[reverie::tool]
impl Tool for NoopTool {
    type GlobalState = GlobalState;
    type ThreadState = ();

    fn native_mm_ownership(_cfg: &Config) -> reverie::NativeMmOwnership {
        // This Tool has no worker, codec state, or FFI construction: it only
        // delegates the intercepted operation to the managed Guest.
        reverie::NativeMmOwnership::ManagedRuntimeOnly
    }

    fn subscriptions(_cfg: &Config) -> Subscription {
        // Don't subscribe to anything by default. This noop-tool doesn't care
        // about any syscalls and will be ORed with the detcore subscriptions.
        Subscription::none()
    }

    async fn handle_syscall_event<T: Guest<Self>>(
        &self,
        guest: &mut T,
        call: Syscall,
    ) -> Result<i64, Error> {
        // NOTE: Cannot use tail_inject here as that would prevent any detcore
        // post-hook code from running.
        Ok(guest.inject(call).await?)
    }
}
#[cfg(test)]
mod mapping_capture_tests;
