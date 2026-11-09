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

/// Binary streams do not carry the reserved-input provenance required by
/// Chosen execution. This is an admission refusal, never a guest errno or
/// a replay desync; the typed inner Tool is authoritative even when Config's
/// mode booleans are false.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChosenBinaryInputRefusal;

impl std::fmt::Display for ChosenBinaryInputRefusal {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output.write_str(
            "HERMIT_CHOSEN_BINARY_INPUT_PROVENANCE: Chosen guest IDs require reserved-input provenance, which the binary record/replay stream does not carry; the guest has not started. Use Namespace guest IDs for binary record/replay, or use plain Chosen execution and its qualified JSON schedule recording. Unknown custom Tool compositions must declare their actual plain stream role; Config flags cannot override a recorder or replayer.",
        )
    }
}

impl std::error::Error for ChosenBinaryInputRefusal {}

/// Pure decision used by the operational static facility admission hook and
/// typed public launch wrappers. Calling subscriptions is only a query and
/// must never execute this policy's exit path.
pub fn require_chosen_input_stream(
    config: &Config,
    role: reverie::ToolStreamRole,
) -> Result<(), ChosenBinaryInputRefusal> {
    if matches!(
        config.guest_ids,
        detcore_model::pid::GuestIds::Chosen { .. }
    ) && (role != reverie::ToolStreamRole::Plain
        || config.recordreplay_modes
        || config.replaying)
    {
        return Err(ChosenBinaryInputRefusal);
    }
    Ok(())
}

/// Operational pre-start admission for raw embedding entrypoints. These have
/// no CLI error classifier, so retain the existing policy-refusal exit 122.
/// Pure Tool queries must use neither this function nor another exit path.
pub fn admit_chosen_input_stream(config: &Config, role: reverie::ToolStreamRole) {
    if let Err(refusal) = require_chosen_input_stream(config, role) {
        crate::scheduler::exit_on_scheduler_refusal(&refusal);
    }
}

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

    fn stream_role() -> reverie::ToolStreamRole {
        reverie::ToolStreamRole::Plain
    }

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

#[cfg(test)]
mod input_stream_tests {
    use super::*;

    #[test]
    fn chosen_binary_refusal_uses_the_typed_stream_even_with_false_mode_flags() {
        let config = Config {
            guest_ids: detcore_model::pid::GuestIds::Chosen {
                base: 5000,
                refuse_host_ids: true,
            },
            recordreplay_modes: false,
            replaying: false,
            ..Config::default()
        };
        assert_eq!(NoopTool::stream_role(), reverie::ToolStreamRole::Plain);
        require_chosen_input_stream(&config, NoopTool::stream_role()).unwrap();
        for role in [
            reverie::ToolStreamRole::BinaryRecord,
            reverie::ToolStreamRole::BinaryReplay,
            reverie::ToolStreamRole::Unknown,
        ] {
            let refusal = require_chosen_input_stream(&config, role).unwrap_err();
            assert!(
                refusal
                    .to_string()
                    .starts_with("HERMIT_CHOSEN_BINARY_INPUT_PROVENANCE:")
            );
        }
        for (recordreplay_modes, replaying) in [(true, false), (false, true), (true, true)] {
            let config = Config {
                recordreplay_modes,
                replaying,
                ..config.clone()
            };
            assert!(require_chosen_input_stream(&config, reverie::ToolStreamRole::Plain).is_err());
        }
    }

    #[test]
    fn namespace_stream_admission_and_subscription_queries_remain_unconditional() {
        for role in [
            reverie::ToolStreamRole::BinaryRecord,
            reverie::ToolStreamRole::BinaryReplay,
            reverie::ToolStreamRole::Unknown,
            reverie::ToolStreamRole::Plain,
        ] {
            for (recordreplay_modes, replaying) in
                [(false, false), (true, false), (false, true), (true, true)]
            {
                require_chosen_input_stream(
                    &Config {
                        recordreplay_modes,
                        replaying,
                        ..Config::default()
                    },
                    role,
                )
                .unwrap();
            }
        }
        // Querying a forbidden configuration is legal: only the operational
        // facility admission hook may refuse it.
        let config = Config {
            guest_ids: detcore_model::pid::GuestIds::Chosen {
                base: 5000,
                refuse_host_ids: true,
            },
            recordreplay_modes: true,
            ..Config::default()
        };
        let _ = <crate::Detcore as Tool>::subscriptions(&config);
    }
}
