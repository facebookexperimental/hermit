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

use serde::Deserialize;
use serde::Serialize;

/// Delimits the machine-readable record appended to a human DETLOG message.
///
/// The human text remains available to people and historical readers. Current
/// verification consumes the JSON after this delimiter for event class and
/// position rather than recovering those facts from prose.
pub const RECORD_SEPARATOR: &str = " DETLOG_RECORD=";

/// Current schema written beside each structured DETLOG event.
pub const RECORD_SCHEMA: u32 = 1;

/// Producer-owned facts needed by log comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DetLogEvent {
    /// A deterministic record outside the syscall-specific classes below.
    Other,
    /// A syscall entered but not yet completed.
    Syscall,
    /// A completed syscall, carrying Detcore's own counter.
    SyscallResult {
        /// Number of syscalls completed by guest threads at this point.
        finished_syscall_number: u64,
    },
    /// A scheduler turn committed to the guest.
    SchedulerCommit {
        /// Detcore's scheduler turn number.
        scheduler_turn: u64,
        /// Committed virtual time at this turn, in nanoseconds.
        virtual_nanoseconds: u64,
        /// Whether this turn is host-timing-sensitive internal I/O polling.
        internal_io_poll: bool,
        /// Whether this turn reads the guest runtime's `/proc/self/maps`.
        runtime_maps_read: bool,
    },
    /// Per-turn committed-time bookkeeping excluded from deterministic comparison.
    SchedulerCommittedTime,
    /// The scheduler found no runnable thread and took its established kick path.
    SchedulerEmptyQueueKick,
}

/// Versioned serialized form appended to the human log record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetLogRecord {
    /// Serialized record schema.
    pub schema: u32,
    /// Closed event payload.
    pub event: DetLogEvent,
}

impl DetLogRecord {
    /// Construct a record in the current schema.
    pub fn new(event: DetLogEvent) -> Self {
        Self {
            schema: RECORD_SCHEMA,
            event,
        }
    }

    /// Split a human record from its producer-owned structured suffix.
    ///
    /// An absent suffix is the historical format. Once the delimiter is
    /// present, malformed JSON or another schema is an error rather than an
    /// invitation to fall back to the human text.
    pub fn split(message: &str) -> Result<(&str, Option<Self>), String> {
        let Some((human, encoded)) = message.rsplit_once(RECORD_SEPARATOR) else {
            return Ok((message, None));
        };
        let record: Self = serde_json::from_str(encoded)
            .map_err(|error| format!("malformed DETLOG record: {error}"))?;
        if record.schema != RECORD_SCHEMA {
            return Err(format!(
                "unsupported DETLOG record schema {}; expected {}",
                record.schema, RECORD_SCHEMA
            ));
        }
        Ok((human, Some(record)))
    }
}

/// Serialize one event for appending to its human log message.
#[doc(hidden)]
pub fn record_suffix(event: DetLogEvent) -> String {
    let encoded = serde_json::to_string(&DetLogRecord::new(event))
        .expect("DETLOG record serialization cannot fail");
    format!("{RECORD_SEPARATOR}{encoded}")
}

/// A process-local sink for deterministic INFO records.
pub type DetlogForwarder = for<'a> fn(&str, fmt::Arguments<'a>);

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
pub fn emit_forwarded(record_suffix: &str, message: fmt::Arguments<'_>) {
    tracing::info!("DETLOG {}{}", message, record_suffix);
    FORWARDER.get().expect("forwarder disappeared")(record_suffix, message);
}

/// Macro used to encapsulate tracing should-be-deterministic information.
/// This is currently at the INFO log level.
#[macro_export]
macro_rules! detlog {
    (event = $event:expr; $($arg:tt)+) => {{
        if $crate::detlog::forwarding_enabled() || ::tracing::enabled!(::tracing::Level::INFO) {
            let record_suffix = $crate::detlog::record_suffix($event);
            if $crate::detlog::forwarding_enabled() {
                $crate::detlog::emit_forwarded(&record_suffix, format_args!($($arg)+));
            } else {
                ::tracing::info!("DETLOG {}{}", format_args!($($arg)+), record_suffix);
            }
        }
    }};
    ($($arg:tt)+) => {{
        $crate::detlog!(event = $crate::detlog::DetLogEvent::Other; $($arg)+);
    }};
}

/// Whether a [`detlog!`] record emitted at this point would reach anything.
///
/// WHY THIS IS NEEDED, and why it is a macro rather than a function.
///
/// `detlog!` routes to the process-local forwarder when one is installed and to
/// `tracing::info!` otherwise. `tracing` does not evaluate a macro's value
/// expressions when the level is disabled, so work done *inside* a `detlog!`
/// argument is already free when nothing observes the record. Work done
/// *before* the macro is not, and callers that must prepare something expensive
/// to pass in have no way to know they can skip it.
///
/// `tracing`'s level check is per-callsite and keyed on the *calling module's*
/// target, so this has to expand at the caller rather than resolve inside
/// `detcore::detlog`; otherwise a target-scoped filter could enable one and
/// disable the other.
///
/// Use it only to skip preparatory work. It is not a substitute for `detlog!`'s
/// own gating, and a caller that guards a record with it must still emit that
/// record through `detlog!`.
#[macro_export]
macro_rules! detlog_observed {
    () => {
        $crate::detlog::forwarding_enabled() || ::tracing::enabled!(::tracing::Level::INFO)
    };
}

/// Macro used to encapsulate tracing should-be-deterministic information.
/// This variant is at a higher log level and requires that logging verbosity is
/// set to DEBUG.
#[macro_export]
macro_rules! detlog_debug {
    (event = $event:expr; $($arg:tt)+) => {{
        if ::tracing::enabled!(::tracing::Level::DEBUG) {
            let record_suffix = $crate::detlog::record_suffix($event);
            ::tracing::debug!("DETLOG {}{}", format_args!($($arg)+), record_suffix);
        }
    }};
    ($($arg:tt)+) => {{
        $crate::detlog_debug!(event = $crate::detlog::DetLogEvent::Other; $($arg)+);
    }};
}

#[cfg(test)]
mod tests {
    use tracing::Metadata;
    use tracing::span;
    use tracing::subscriber::Interest;

    use super::DetLogEvent;
    use super::DetLogRecord;
    use super::RECORD_SEPARATOR;
    use super::record_suffix;

    #[test]
    fn test_detlog() {
        detlog!("Hello : {}. From {:?}", "World", 31337);
    }

    #[test]
    fn structured_record_round_trips_and_refuses_an_incomplete_current_shape() {
        let suffix = record_suffix(DetLogEvent::SyscallResult {
            finished_syscall_number: 37,
        });
        let line = format!("INFO detcore: DETLOG finish syscall #999{suffix}");
        let (human, record) = DetLogRecord::split(&line).unwrap();
        assert_eq!(human, "INFO detcore: DETLOG finish syscall #999");
        assert_eq!(
            record.unwrap().event,
            DetLogEvent::SyscallResult {
                finished_syscall_number: 37
            }
        );

        let missing_number = format!(
            "INFO detcore: DETLOG finish syscall #999{RECORD_SEPARATOR}{{\"schema\":1,\"event\":{{\"kind\":\"syscall_result\"}}}}"
        );
        assert!(
            DetLogRecord::split(&missing_number)
                .unwrap_err()
                .contains("finished_syscall_number"),
            "an incomplete current record must fail by field name"
        );
    }

    /// Minimal subscriber that reports every callsite as enabled.
    ///
    /// `register_callsite` deliberately answers `sometimes()` rather than
    /// letting the default derive `always()`: an `always`/`never` answer is
    /// cached per callsite for the life of the process, which would leak
    /// between tests.
    struct AlwaysEnabled;

    impl tracing::Subscriber for AlwaysEnabled {
        fn register_callsite(&self, _: &'static Metadata<'static>) -> Interest {
            Interest::sometimes()
        }
        fn enabled(&self, _: &Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
            span::Id::from_u64(1)
        }
        fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
        fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
        fn event(&self, _: &tracing::Event<'_>) {}
        fn enter(&self, _: &span::Id) {}
        fn exit(&self, _: &span::Id) {}
    }

    /// `detlog_observed!` must answer true when a subscriber would take the
    /// record. Callers use it to decide whether to prepare data for a
    /// `detlog!`, so a false negative silently drops determinism evidence.
    #[test]
    fn detlog_observed_is_true_when_a_subscriber_is_listening() {
        tracing::subscriber::with_default(AlwaysEnabled, || {
            assert!(detlog_observed!());
        });
    }

    /// ...and false when nothing is listening, which is the whole point: it is
    /// what lets `detlog_memory_maps` skip enumerating `/proc/<pid>/maps` on
    /// every syscall of a run that writes no log. Measured before this gate
    /// existed, on a QEMU/Linux boot with `RUST_LOG` unset (123 bytes of log
    /// produced): `--detlog-stack` cost 4.36x and `--detlog-heap` 4.76x the
    /// no-flag baseline.
    ///
    /// Uses a distinct callsite from the enabled test above on purpose --
    /// `tracing` caches per-callsite interest, so sharing one callsite between
    /// the two cases would make them order-dependent.
    #[test]
    fn detlog_observed_is_false_when_nothing_is_listening() {
        tracing::subscriber::with_default(tracing::subscriber::NoSubscriber::default(), || {
            assert!(!detlog_observed!());
        });
    }
}
