/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Module contains macroses that help tracing DETLOG entires for the purpose of verifiying determinism
//! ['detlog'] can be used to write a deterministic log entry at INFO level
//! ['detlog_debug] can be use to write a deterministic log entry at DEBUG level

use std::fmt;
use std::sync::OnceLock;

/// A process-local sink for deterministic INFO records.
pub type DetlogForwarder = for<'a> fn(fmt::Arguments<'a>);

static FORWARDER: OnceLock<DetlogForwarder> = OnceLock::new();

/// Installs a process-local sink for deterministic INFO records.
///
/// Backends whose tool runs in another process can use this to transport the
/// same records that are normally observed through the coordinator's tracing
/// subscriber. Only the first sink installed in a process is retained.
pub fn set_forwarder(forwarder: DetlogForwarder) -> Result<(), DetlogForwarder> {
    FORWARDER.set(forwarder)
}

/// Returns whether a process-local deterministic-record sink is installed.
#[doc(hidden)]
pub fn forwarding_enabled() -> bool {
    FORWARDER.get().is_some()
}

/// Emits one deterministic record through tracing and the process-local sink.
#[doc(hidden)]
pub fn emit_forwarded(message: fmt::Arguments<'_>) {
    tracing::info!("DETLOG {}", message);
    FORWARDER.get().expect("forwarder disappeared")(message);
}

/// Macro used to encapsulate tracing should-be-deterministic information.
/// This is currently at the INFO log level.
#[macro_export]
macro_rules! detlog {
    ($($arg:tt)+) => {{
        if $crate::detlog::forwarding_enabled() {
            $crate::detlog::emit_forwarded(format_args!($($arg)+));
        } else {
            tracing::info!("DETLOG {}", format_args!($($arg)+));
        }
    }};
}

/// Macro used to encapsulate tracing should-be-deterministic information.
/// This variant is at a higher log level and requires that logging verbosity is
/// set to DEBUG.
#[macro_export]
macro_rules! detlog_debug {
    ($($arg:tt)+) => {{
        tracing::debug!("DETLOG {}", format!($($arg)+));
    }};
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_detlog() {
        detlog!("Hello : {}. From {:?}", "World", 31337);
    }
}
