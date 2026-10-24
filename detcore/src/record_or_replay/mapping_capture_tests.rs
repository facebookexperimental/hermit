/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use reverie::Error;
use reverie::Guest;
use reverie::NativeMmOwnership;
use reverie::Tool;
use reverie::syscalls::Syscall;
use serde::Deserialize;
use serde::Serialize;

use super::NoopTool;
use super::RecordOrReplay;
use crate::Detcore;
use crate::config::Config;
use crate::tool_global::GlobalState;

#[derive(Debug, Default, Serialize, Deserialize)]
struct UntrustedInner;

#[reverie::tool]
impl Tool for UntrustedInner {
    type GlobalState = GlobalState;
    type ThreadState = ();
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct NativeSpawnInner;

#[reverie::tool]
impl Tool for NativeSpawnInner {
    type GlobalState = GlobalState;
    type ThreadState = ();

    async fn handle_syscall_event<T: Guest<Self>>(
        &self,
        guest: &mut T,
        call: Syscall,
    ) -> Result<i64, Error> {
        // A real unmediated native birth would occur if this callback ran.
        // The declaration tests below never invoke it, under Detcore or native.
        std::thread::spawn(|| ())
            .join()
            .expect("test Tool's native worker panicked");
        Ok(guest.inject(call).await?)
    }
}

fn assert_declarations<T: RecordOrReplay>(expected: NativeMmOwnership) {
    let config = Config::default();
    assert_eq!(<T as Tool>::native_mm_ownership(&config), expected);
    assert_eq!(
        <Detcore<T> as Tool>::native_mm_ownership(&config),
        expected,
        "the outer Tool cannot strengthen the inner Tool's declaration"
    );
}

#[test]
fn shipped_noop_inner_explicitly_allows_managed_native_ownership() {
    assert_declarations::<NoopTool>(NativeMmOwnership::ManagedRuntimeOnly);
}

#[test]
fn arbitrary_record_or_replay_inner_does_not_inherit_managed_ownership() {
    assert_declarations::<UntrustedInner>(NativeMmOwnership::Unavailable);
}

#[test]
fn native_spawn_callback_does_not_inherit_managed_ownership() {
    assert_declarations::<NativeSpawnInner>(NativeMmOwnership::Unavailable);
}
