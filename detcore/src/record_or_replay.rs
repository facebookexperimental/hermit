// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

use reverie::syscalls::Syscall;
use reverie::Error;
use reverie::Guest;
use reverie::Subscription;
use reverie::Tool;
use serde::Deserialize;
use serde::Serialize;

use crate::config::Config;
use crate::tool_global::GlobalState;

/// Helper trait.
pub trait RecordOrReplay: Tool<GlobalState = GlobalState> {}

impl<T> RecordOrReplay for T where T: Tool<GlobalState = GlobalState> {}

/// A tool that only injects the syscall it receives.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct NoopTool;

#[reverie::tool]
impl Tool for NoopTool {
    type GlobalState = GlobalState;
    type ThreadState = ();

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
