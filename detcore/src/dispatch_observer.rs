/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in LICENSE.
 */

//! Optional process-local observation at the actual Tool syscall entry.
//!
//! Records establish entry, not semantic completion. The observer must not
//! change guest state, acquire a scheduler turn or advance virtual time.
//! There is no concrete backend dependency or ordinary logging side effect.

use std::sync::OnceLock;

/// Scalars obtained from the actual Guest and incoming Tool syscall.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Entry {
    /// Guest interface process identity.
    pub pid: i32,
    /// Guest interface thread identity.
    pub tid: i32,
    /// Existing Detcore thread-state identity.
    pub dettid: i32,
    /// Original syscall number received by the Tool.
    pub number: u64,
    /// Guest register instruction pointer, without backend-specific adjustment.
    pub instruction_pointer: u64,
    /// Six original stored scalar syscall arguments.
    pub arguments: [u64; 6],
}

/// A synchronous observation callback; never a replacement syscall handler.
pub type Observer = fn(Entry);
static OBSERVER: OnceLock<Observer> = OnceLock::new();

/// Install one observer before guest entry. Repeated installation is refused.
pub fn install(observer: Observer) -> Result<(), Observer> {
    OBSERVER.set(observer)
}

pub(crate) fn installed() -> Option<Observer> {
    OBSERVER.get().copied()
}
