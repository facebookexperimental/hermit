/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The one place this binary clones a container.
//!
//! `reverie::process::Container` starts its child with a raw `clone(2)`, which
//! runs no `pthread_atfork` handler, so the child starts with every lock that
//! another thread of the process held at that instant, glibc's malloc arena
//! locks among them, and with no thread left to release them, and it holds
//! every descriptor the process had open, the write end of a pipe another
//! thread was handing to a child among them. `Container::run` therefore
//! requires a process with no other threads. `hermit` clones from its only
//! thread. A unit test is a thread of the test binary beside other tests, so
//! in a test build each clone here first checks that the cloning test runs in
//! a process of its own (`test_isolation::assert_alone_before_container_clone`):
//! a new test that clones a container and forgets `test_isolation::rerun_alone`
//! fails at once, under nextest too, instead of hanging plain `cargo test` once
//! in a while. `hermit-cli/clippy.toml` refuses the `Container` methods that
//! clone anywhere else in this package.

use reverie::process::Container;
use reverie::process::OwnedDeferredContainerRun;
use reverie::process::RunError;
use reverie::process::StartupOwnedFailure;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// The `Container` methods this binary clones with, each behind the test
/// build's check.
pub(crate) trait CheckedClone {
    /// [`Container::run`], after the test build's check. The binary's own
    /// code clones only through
    /// [`checked_run_with_deferred_drop_owned`](Self::checked_run_with_deferred_drop_owned)
    /// (`owned_container::run`); its tests clone through this too.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "only the binary's tests clone with Container::run"
        )
    )]
    fn checked_run<F, T>(&mut self, f: F) -> Result<T, RunError>
    where
        F: FnMut() -> T,
        T: Serialize + DeserializeOwned;

    /// [`Container::run_with_deferred_drop_owned`], after the test build's
    /// check.
    fn checked_run_with_deferred_drop_owned<F, T, D>(
        &mut self,
        run: &mut F,
    ) -> Result<OwnedDeferredContainerRun<T>, StartupOwnedFailure<T>>
    where
        F: FnMut() -> (T, D),
        T: Serialize;
}

#[expect(clippy::disallowed_methods, reason = "this is the checked clone site")]
impl CheckedClone for Container {
    fn checked_run<F, T>(&mut self, f: F) -> Result<T, RunError>
    where
        F: FnMut() -> T,
        T: Serialize + DeserializeOwned,
    {
        before_clone();
        self.run(f)
    }

    fn checked_run_with_deferred_drop_owned<F, T, D>(
        &mut self,
        run: &mut F,
    ) -> Result<OwnedDeferredContainerRun<T>, StartupOwnedFailure<T>>
    where
        F: FnMut() -> (T, D),
        T: Serialize,
    {
        before_clone();
        self.run_with_deferred_drop_owned(run)
    }
}

fn before_clone() {
    #[cfg(test)]
    crate::app::test_isolation::assert_alone_before_container_clone();
}
