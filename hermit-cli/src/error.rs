/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use serde::Deserialize;
use serde::Serialize;

pub type Error = anyhow::Error;

pub use anyhow::Context;

/// A serializable error. This is useful for sending an error to the parent
/// process. This works by converting an error into a string via its `Display`
/// implementation. Although we lose type information in the process of
/// converting to a string, this preserves the error message and its error chain.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, Eq, PartialEq)]
pub enum FailureKind {
    /// The child reported a failure it decided to report.
    #[default]
    Error,
    /// The child PANICKED and the panic was caught and reported.
    ///
    /// ⚠️ THIS IS THE ONE BIT THAT PROSE CANNOT CARRY. A caught panic and a
    /// reported error arrive at the parent as the same shape -- a message and a
    /// chain of causes -- so without a discriminant the parent must either
    /// match on English or call them the same thing. It called them the same
    /// thing, which is why a tracer panic and a bad flag were indistinguishable.
    Panic,
    /// The child stopped because a FAIL-CLOSED POLICY refused the run.
    ///
    /// ⚠️ A REFUSAL AND A REPORTED ERROR ARE THE SAME SHAPE HERE TOO, which is
    /// the identical argument that justified `Panic`. Record mode sets
    /// `exit_on_unsupported_syscall` and `shutdown_on_unsupported_syscall:
    /// false`, so it returns a typed `UnsupportedSyscallError` through Reverie
    /// instead of calling `unrecoverable_shutdown`. That path never produces an
    /// exit status of its own, so the status channel -- which is what carries
    /// `HERMIT_POLICY_REFUSAL_EXIT` on the run path -- has nothing to say, and
    /// the refusal arrived indistinguishable from "hermit broke": exit 125,
    /// `class=container-child-exit`. Two configurations of one policy reported
    /// opposite things about the same decision.
    PolicyRefusal,
    /// A ptrace PMU timer interrupt arrived after its target. The count is
    /// carried across the container boundary so the parent can publish the
    /// named infrastructure error instead of leaving the pending no-result.
    SkidOvershoot { count: u64 },
    /// The child stopped because HERMIT'S OWN `--timeout` BOUND was reached.
    ///
    /// ⚠️ THE SAME ARGUMENT A THIRD TIME, and it is not hypothetical: measured
    /// on 2026-08-26, `hermit run --timeout 3 -- ./spin` fired at exactly 3s
    /// with no residue and still reported `exit 125` /
    /// `class=cli-error` -- "hermit broke" -- because `GuestTimedOut` is
    /// produced INSIDE the container child and every type is a string by the
    /// time the parent sees it. Unlike the refusal path there is no status
    /// channel to fall back on either: the timeout unwinds and RETURNS rather
    /// than calling `unrecoverable_shutdown`, precisely because the unwind is
    /// the point.
    RunTimeout,
}

/// ⚠️ THE `kind` FIELD IS THE THIRD AND LAST OF ONE CHAIN OF FLATTENINGS.
/// The same property -- the failure's CLASS -- was destroyed at three different
/// boundaries, in series on one path:
///
///   1. `main`'s `unwrap_or_else` mapped EVERY error to one exit code;
///   2. `with_container` flattened reverie's typed `RunError::ExitStatus` into
///      opaque prose with `.context(..)?`;
///   3. THIS type crosses a process boundary carrying only strings.
///
/// They are three distinct boundaries with three mechanisms, but they answer one
/// question, so one discriminant serves all three. Preserving it here is what
/// lets the parent classify without parsing English.
///
/// Extending a serialized type is safe HERE specifically because the writer and
/// the reader are the SAME BINARY IMAGE -- hermit's container child and its own
/// parent, over a fork-local pipe -- so there is no cross-version compatibility
/// surface.
///
/// ⚠️ `serde(default)` BELOW IS NOT A COMPATIBILITY GUARANTEE, and must not be
/// read as one. reverie serializes this with `bincode::config::legacy()`
/// (`reverie-process/src/container.rs:861`), which is NOT self-describing: a
/// record written by an older binary with two fields would fail to decode
/// outright rather than defaulting the third. The attribute is harmless and
/// keeps the field optional at the Rust level; the property that actually makes
/// this safe is the same-image one above.
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct SerializableError {
    /// The main error.
    error: String,
    /// The chain of causes. This is empty if there are no associated causes.
    context: Vec<String>,
    /// What CLASS of failure this was. See [`FailureKind`].
    #[serde(default)]
    kind: FailureKind,
}

impl SerializableError {
    /// The class this failure was reported as.
    pub fn kind(&self) -> FailureKind {
        self.kind
    }

    /// Re-tag an error as a caught panic. Called at the catch site, which is the
    /// only place that still knows a panic is what happened.
    pub fn into_panic(mut self) -> Self {
        self.kind = FailureKind::Panic;
        self
    }
}

/// Is this error a fail-closed POLICY REFUSAL rather than something that broke?
///
/// ⚠️ THE CHAIN WALK ALONE DOES NOT FIND IT, AND I SHIPPED A FIX THAT ONLY DID THAT.
/// `reverie::Error::Tool` is declared `#[error(transparent)]`, which forwards
/// `source()` to the inner `anyhow::Error`'s source -- PAST that error's own value.
/// `UnsupportedSyscallError` IS that value, so it never appears in
/// `err.chain()` and `.is::<_>()` is false at every link. Reproduced in isolation:
/// a one-entry chain whose Display is right and whose `is::<T>()` is false.
///
/// The consequence was that record mode still reported exit 125
/// `class=cli-error` -- measured against `tests/c/dbt_unsupported_syscall.c`, the
/// path this whole change exists to fix, unchanged. The in-process test passed
/// because it constructs the error directly and never crosses the wrapper.
/// agent(hermit-001)'s codex lane asked for the end-to-end cell that would have
/// caught it; the cell was absent because the fix did not work.
///
/// ⚠️ IT IS STILL A TYPE TEST, NOT A STRING MATCH. Matching the Display text would
/// be the thing `kind` exists to avoid, and it would break the moment the message
/// is reworded. This reaches through the one wrapper that hides the type.
fn is_policy_refusal(err: &Error) -> bool {
    if err
        .chain()
        .any(|cause| cause.is::<detcore::UnsupportedSyscallError>())
    {
        return true;
    }
    // The wrapper case: reverie hands the tool's error back inside `Tool`, whose
    // transparent forwarding hides the value from the chain above.
    err.chain().any(|cause| {
        cause
            .downcast_ref::<reverie::Error>()
            .and_then(|reverie_error| match reverie_error {
                reverie::Error::Tool(inner) => {
                    inner.downcast_ref::<detcore::UnsupportedSyscallError>()
                }
                _ => None,
            })
            .is_some()
    })
}

impl From<Error> for SerializableError {
    fn from(err: Error) -> Self {
        let error = err.to_string();
        let context = err.chain().skip(1).map(ToString::to_string).collect();
        // ⚠️ CLASSIFIED HERE BECAUSE THIS IS THE LAST PLACE THE TYPE EXISTS.
        // Everything below this line is strings: `From<SerializableError> for
        // Error` rebuilds the chain with `Error::msg`, so a downcast on the far
        // side can only ever fail. Detecting the refusal after the boundary
        // would mean matching on English, which is the thing `kind` exists to
        // avoid.
        let kind = if let Some(error) = err.downcast_ref::<crate::SkidOvershootError>() {
            FailureKind::SkidOvershoot {
                count: error.count(),
            }
        } else if is_policy_refusal(&err) {
            FailureKind::PolicyRefusal
        } else if err.downcast_ref::<crate::GuestTimedOut>().is_some() {
            FailureKind::RunTimeout
        } else {
            FailureKind::Error
        };
        Self {
            error,
            context,
            kind,
        }
    }
}

impl From<SerializableError> for Error {
    fn from(mut err: SerializableError) -> Self {
        if let Some(root_cause) = err.context.pop() {
            let mut error = Self::msg(root_cause);

            while let Some(context) = err.context.pop() {
                error = error.context(context);
            }

            error.context(err.error)
        } else {
            Self::msg(err.error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn into_serializable_error() {
        let error = Error::msg("root cause")
            .context("a")
            .context("b")
            .context("c");

        assert_eq!(
            SerializableError::from(error),
            SerializableError {
                error: "c".into(),
                context: vec!["b".into(), "a".into(), "root cause".into(),],
                kind: FailureKind::Error,
            }
        );
    }

    #[test]
    fn from_serializable_error() {
        let error = Error::from(SerializableError {
            error: "c".into(),
            context: vec!["b".into(), "a".into(), "root cause".into()],
            kind: FailureKind::Error,
        });

        assert_eq!(
            error
                .chain()
                .map(ToString::to_string)
                .collect::<Vec<String>>(),
            ["c", "b", "a", "root cause"]
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<String>>(),
        )
    }

    #[test]
    fn skid_overshoot_is_serialized_as_a_policy_refusal() {
        let refusal = SerializableError::from(Error::new(crate::SkidOvershootError::new(2)));
        assert_eq!(refusal.kind(), FailureKind::SkidOvershoot { count: 2 });
        assert!(refusal.error.contains("observed 2 HERMIT_SKID_OVERSHOOT"));

        let contextual = SerializableError::from(
            Error::msg("guest execution also failed").context(crate::SkidOvershootError::new(1)),
        );
        assert_eq!(contextual.kind(), FailureKind::SkidOvershoot { count: 1 });
        assert!(
            contextual
                .error
                .contains("observed 1 HERMIT_SKID_OVERSHOOT")
        );

        let ordinary = SerializableError::from(Error::msg("ordinary failure"));
        assert_eq!(ordinary.kind(), FailureKind::Error);
    }
}
