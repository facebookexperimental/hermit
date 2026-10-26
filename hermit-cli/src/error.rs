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
    /// A precise PMU timer event (ptrace or KVM) arrived after its target. The
    /// count is carried across the container boundary so the parent can
    /// publish the named infrastructure error instead of leaving the pending
    /// no-result.
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
    /// The backend failed while running the guest ([`crate::BackendRunFailure`]).
    /// Carried so the parent can record the verification run as failed by its
    /// backend instead of leaving the pending no-result stamp.
    BackendRunFailure,
    /// The run ended with a scheduler terminal outcome
    /// ([`crate::SchedulerEndedRun`]): a deadlock under Hermit's model, or its
    /// virtual clock passed `--virtual-timeout`. Its own outcome, neither a
    /// guest result nor an internal failure. Carried for the reason
    /// `RunTimeout` is: the child reports it through this channel, after the
    /// run was unwound and finished, rather than through the outcome's status,
    /// which it exits with when its backend cannot unwind the guest.
    TerminalOutcome(detcore::TerminalOutcome),
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
    /// An unresolved owner is separate from the primary failure class. This
    /// same-image container field is not a cross-version wire compatibility promise.
    cleanup_stage: Option<crate::HermitCleanupStage>,
}

impl SerializableError {
    /// The class this failure was reported as.
    pub fn kind(&self) -> FailureKind {
        self.kind
    }

    /// The unresolved owner phase, if cleanup has not actually completed.
    pub fn cleanup_stage(&self) -> Option<crate::HermitCleanupStage> {
        self.cleanup_stage
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
    // Anyhow typed context is downcastable through Anyhow, but is not
    // necessarily exposed as that type by std::error::Error::source().
    if detcore::ReplayDepartureError::in_error(err).is_some() {
        return true;
    }
    // Detcore's refusal of an unsupported syscall, Detcore's refusal of a
    // self-sentinel pidfd_send_signal its backend could not qualify, and
    // reverie-ptrace's end of a run in which the guest made a syscall through Reverie's untraced
    // private-page stub, which no tool saw
    // (https://github.com/rrnewton/reverie/issues/1064).
    fn refuses(cause: &(dyn std::error::Error + 'static)) -> bool {
        cause.is::<detcore::UnsupportedSyscallError>()
            || cause.is::<detcore::preemptions::RecordAdmissionRefusal>()
            || cause.is::<detcore::preemptions::RecordFormatRefusal>()
            || cause.is::<detcore::ChosenBinaryInputRefusal>()
            || cause.is::<detcore::PidfdSelfQualificationRefusal>()
            || cause.is::<detcore::TerminalReplayRefusal>()
            || cause.is::<detcore::PriorityHistoryRefusal>()
            || cause.is::<detcore::ReplayDepartureError>()
            || cause.is::<reverie_ptrace::UntracedStubSyscall>()
            || is_chosen_root_id_taken(cause)
            || is_kvm_host_resources_refusal(cause)
            || is_kvm_named_refusal(cause)
            || is_kvm_vfork_child_refusal(cause)
    }
    // The KVM backend's end of a run whose exec it could not finish past the
    // exec's point of no return, because a host resource of Hermit's own
    // process ran out (reverie-kvm's `Error::HostResources`, found beneath
    // the wrappers a run's failure is aggregated in). The guest saw no errno;
    // before that point the same shortage is Detcore's refusal of the exec.
    #[cfg(target_arch = "x86_64")]
    fn is_kvm_host_resources_refusal(cause: &(dyn std::error::Error + 'static)) -> bool {
        cause
            .downcast_ref::<reverie_kvm::Error>()
            .is_some_and(reverie_kvm::Error::is_host_resources_refusal)
    }
    #[cfg(not(target_arch = "x86_64"))]
    fn is_kvm_host_resources_refusal(_cause: &(dyn std::error::Error + 'static)) -> bool {
        false
    }
    // The KVM executor's refusals by name (`reverie_kvm::Error::is_named_refusal`,
    // decided by the failure's original cause beneath Reverie's wrappers): an
    // operation it declines because it cannot perform it faithfully, such as a
    // read that would wait in the host, or a shared-file operation it does not
    // implement (`Error::UnsupportedSharedFile`), such as a shared view of a
    // hugetlbfs file. Every other KVM error stays a backend failure, among them
    // `SharedFileCapability`, which also reports internal guards
    // (https://github.com/rrnewton/hermit/issues/5025).
    #[cfg(target_arch = "x86_64")]
    fn is_kvm_named_refusal(cause: &(dyn std::error::Error + 'static)) -> bool {
        cause
            .downcast_ref::<reverie_kvm::Error>()
            .is_some_and(reverie_kvm::Error::is_named_refusal)
    }
    #[cfg(not(target_arch = "x86_64"))]
    fn is_kvm_named_refusal(_cause: &(dyn std::error::Error + 'static)) -> bool {
        false
    }
    // The KVM backend's refusal by name of an operation of a vfork child that
    // its copy of the parent's memory cannot carry back (reverie-kvm's
    // `Error::VforkChildCapability`, found beneath the wrappers a run's
    // failure is aggregated in): a mapping change the child leaves in place
    // when its window ends, a byte the child and another thread of its parent
    // both changed, a changed page whose mapping that thread replaced or
    // removed, a brk that moves the break, and thread creation in the child
    // (https://github.com/rrnewton/hermit/issues/5025).
    #[cfg(target_arch = "x86_64")]
    fn is_kvm_vfork_child_refusal(cause: &(dyn std::error::Error + 'static)) -> bool {
        cause
            .downcast_ref::<reverie_kvm::Error>()
            .is_some_and(|error| {
                matches!(
                    error.primary(),
                    reverie_kvm::Error::VforkChildCapability { .. }
                )
            })
    }
    #[cfg(not(target_arch = "x86_64"))]
    fn is_kvm_vfork_child_refusal(_cause: &(dyn std::error::Error + 'static)) -> bool {
        false
    }
    // The guest's first process could not have its chosen ID
    // (`--no-namespace`, `Command::pid`): another task took it after Hermit
    // checked, such as a run in another network namespace, which the window
    // reservation does not cover. Linux's clone never fails with EEXIST but
    // for a requested ID, so this is that refusal.
    fn is_chosen_root_id_taken(cause: &(dyn std::error::Error + 'static)) -> bool {
        cause
            .downcast_ref::<reverie::process::Error>()
            .is_some_and(|error| {
                error.errno() == reverie::syscalls::Errno::EEXIST
                    && error.context() == reverie::process::Context::Clone
            })
    }
    if err.chain().any(refuses) {
        return true;
    }
    // The wrapper case: reverie hands the tool's error back inside `Tool`, whose
    // transparent forwarding hides the value from the chain above.
    err.chain().any(|cause| {
        cause
            .downcast_ref::<reverie::Error>()
            .is_some_and(|reverie_error| match reverie_error {
                reverie::Error::Tool(inner) => inner.chain().any(refuses),
                _ => false,
            })
    })
}

/// Classify only the backend's primary cause. Its secondary errors and failed
/// cleanup observations are deliberately unreachable from this function.
pub(crate) fn classify_ptrace_primary(error: &reverie::Error) -> FailureKind {
    match error {
        // A terminal outcome completes as its own outcome only when its
        // teardown was clean (`ptrace_completion::clean_terminal_outcome`).
        // Left as the primary of a failed run, the teardown failed or never
        // confirmed, which is Hermit's failure.
        reverie::Error::Tool(inner)
            if inner.is::<crate::SchedulerEndedRun>()
                && detcore::ReplayDepartureError::in_error(inner).is_none() =>
        {
            FailureKind::Error
        }
        reverie::Error::Tool(inner) => classify_unwrapped(inner),
        _ => FailureKind::Error,
    }
}

// Follow only the primary of the finalization pair, including Tool's
// transparent wrapper. Ordinary anyhow downcasts do not traverse its source().
fn finalization_primary(error: &Error) -> Option<&Error> {
    if let Some(pair) = error.downcast_ref::<detcore::FinalizationFailure>() {
        return Some(pair.primary());
    }
    error
        .chain()
        .find_map(|cause| match cause.downcast_ref::<reverie::Error>() {
            Some(reverie::Error::Tool(inner)) => finalization_primary(inner),
            _ => None,
        })
}

fn classify_preserving_cleanup(error: &Error) -> (FailureKind, Option<crate::HermitCleanupStage>) {
    if let Some(primary) = finalization_primary(error) {
        classify_preserving_cleanup(primary)
    } else if let Some(error) = error.downcast_ref::<crate::HermitCleanupUnconfirmed>() {
        (error.primary_kind(), Some(error.stage()))
    } else if let Some(error) = error.downcast_ref::<crate::HermitPtraceFailure>() {
        (error.kind(), None)
    } else {
        (classify_unwrapped(error), None)
    }
}

fn classify_unwrapped(error: &Error) -> FailureKind {
    if let Some(primary) = finalization_primary(error) {
        return classify_preserving_cleanup(primary).0;
    }
    if let Some(error) = error.downcast_ref::<crate::SkidOvershootError>() {
        FailureKind::SkidOvershoot {
            count: error.count(),
        }
    } else if error
        .chain()
        .any(|cause| cause.is::<detcore_model::virtual_timeout::VirtualTimeoutWithDebugger>())
        || is_policy_refusal(error)
        || error
            .downcast_ref::<crate::LiteinstInGuestRefusal>()
            .is_some()
        || error
            .chain()
            .any(|cause| cause.is::<crate::cpu_profile_admission::CpuProfileRefusal>())
        || error
            .chain()
            .any(|cause| cause.is::<crate::preemption_admission::UnsupportedPreemption>())
        || error
            .chain()
            .any(|cause| cause.is::<crate::preemption_admission::UnqualifiedAnnotatedReplay>())
        || error
            .chain()
            .any(|cause| cause.is::<crate::kvm_address_space::KvmAddressSpaceRefusal>())
        || error.chain().any(|cause| {
            cause.is::<crate::sleep_registration_admission::UnreadableSleeperSignalState>()
        })
    {
        FailureKind::PolicyRefusal
    } else if error.downcast_ref::<crate::GuestTimedOut>().is_some() {
        FailureKind::RunTimeout
    } else if let Some(ended) = error.downcast_ref::<crate::SchedulerEndedRun>() {
        FailureKind::TerminalOutcome(ended.outcome)
    } else if error.downcast_ref::<crate::BackendRunFailure>().is_some() {
        FailureKind::BackendRunFailure
    } else {
        FailureKind::Error
    }
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
        let (kind, cleanup_stage) = classify_preserving_cleanup(&err);
        Self {
            error,
            context,
            kind,
            cleanup_stage,
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
pub(crate) async fn terminal_replay_refusal_for_test() -> Error {
    use detcore::preemptions::PreemptionRecord;
    use detcore::preemptions::boundary_recording::BoundaryRecording;
    use detcore::preemptions::boundary_recording::Recording;
    use detcore_model::pid::DetTid;
    use detcore_model::schedule::SchedEvent;
    use detcore_model::schedule::boundary::Record;
    use detcore_model::schedule::boundary::Schedule;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("unconsumed.json");
    let recording = Recording::Boundaries(
        BoundaryRecording::new(
            PreemptionRecord::default(),
            Schedule::new(vec![Record::Event(SchedEvent::branches(
                DetTid::from_raw(1),
                1,
            ))])
            .unwrap(),
        )
        .unwrap(),
    );
    std::fs::write(&path, recording.encode().unwrap()).unwrap();
    detcore::GlobalState::init_for_external_scheduler(&detcore::Config {
        sequentialize_threads: true,
        strict_schedule_boundaries: true,
        replay_schedule_from: Some(path),
        record_preemptions: false,
        ..detcore::Config::default()
    })
    .clean_up(false, &None)
    .await
    .unwrap_err()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_format_refusal_keeps_its_kind_through_context_and_reverie() {
        use detcore::preemptions::RecordFormatRefusal;
        use detcore::preemptions::boundary_recording::Recording;

        // The first format past the newest this Hermit reads.
        let future = format!(
            r#"{{"per_thread":{{}},"global":[],"format_version":{}}}"#,
            detcore::preemptions::PREEMPTION_RECORD_FORMAT + 1
        );
        let records: [&[u8]; 2] = [
            future.as_bytes(),
            br#"{"per_thread":{},"global":[],"format_version":3,"time_skips":[]}"#,
        ];
        for contextual in [false, true] {
            for wrapped in [false, true] {
                let adapt = |mut error: Error| {
                    if contextual {
                        error = error.context("reading schedule before guest startup");
                    }
                    if wrapped {
                        error = Error::new(reverie::Error::Tool(error));
                    }
                    if contextual {
                        error = error.context("outer launch boundary");
                    }
                    error
                };
                for bytes in records {
                    let refusal = Recording::decode(bytes).unwrap_err();
                    assert!(refusal.is::<RecordFormatRefusal>());
                    let prose = adapt(Error::msg(refusal.to_string()));
                    let serialized = SerializableError::from(adapt(refusal));
                    assert_eq!(serialized.kind(), FailureKind::PolicyRefusal);
                    let bytes =
                        bincode::serde::encode_to_vec(&serialized, bincode::config::legacy())
                            .unwrap();
                    let (decoded, used): (SerializableError, usize) =
                        bincode::serde::decode_from_slice(&bytes, bincode::config::legacy())
                            .unwrap();
                    assert_eq!(used, bytes.len());
                    assert_eq!(decoded, serialized);
                    assert_eq!(SerializableError::from(prose).kind(), FailureKind::Error);
                }
                for error in [
                    Recording::decode(b"{").unwrap_err(),
                    Error::new(std::io::Error::from(std::io::ErrorKind::NotFound)),
                ] {
                    assert!(!error.is::<RecordFormatRefusal>());
                    assert_eq!(
                        SerializableError::from(adapt(error)).kind(),
                        FailureKind::Error
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn terminal_refusal_is_typed_direct_and_through_reverie() {
        for wrapped in [false, true] {
            let error = terminal_replay_refusal_for_test().await;
            assert!(error.is::<detcore::TerminalReplayRefusal>());
            let error = if wrapped {
                Error::new(reverie::Error::Tool(error))
            } else {
                error
            };
            let serialized = SerializableError::from(error);
            assert_eq!(serialized.kind(), FailureKind::PolicyRefusal);
            assert!(format!("{serialized:?}").contains("Incomplete"));
            let bytes =
                bincode::serde::encode_to_vec(&serialized, bincode::config::legacy()).unwrap();
            let (decoded, used): (SerializableError, usize) =
                bincode::serde::decode_from_slice(&bytes, bincode::config::legacy()).unwrap();
            assert_eq!(used, bytes.len());
            assert_eq!(decoded, serialized);
        }
        for error in [
            Error::new(std::io::Error::other("recording write failed")),
            Error::msg("terminal replay refusal: slice-boundary protocol refusal"),
        ] {
            assert_eq!(SerializableError::from(error).kind(), FailureKind::Error);
        }
    }

    #[tokio::test]
    async fn finalization_secondary_never_overrides_primary_classification() {
        for (primary, kind) in [
            (
                Error::new(std::io::Error::other("ordinary primary")),
                FailureKind::Error,
            ),
            (
                Error::new(crate::GuestTimedOut {
                    limit: std::time::Duration::from_secs(3),
                    survivors: None,
                }),
                FailureKind::RunTimeout,
            ),
            (
                Error::new(crate::BackendRunFailure),
                FailureKind::BackendRunFailure,
            ),
            (
                Error::new(crate::SchedulerEndedRun::new(
                    detcore::TerminalOutcome::GuestDeadlock,
                    None,
                )),
                FailureKind::TerminalOutcome(detcore::TerminalOutcome::GuestDeadlock),
            ),
            (
                Error::new(crate::SchedulerEndedRun::new(
                    detcore::TerminalOutcome::VirtualTimeout { bound_ns: 9 },
                    None,
                )),
                FailureKind::TerminalOutcome(detcore::TerminalOutcome::VirtualTimeout {
                    bound_ns: 9,
                }),
            ),
            (
                Error::new(crate::SkidOvershootError::new(2)),
                FailureKind::SkidOvershoot { count: 2 },
            ),
        ] {
            let paired = detcore::combine_finalization::<()>(
                Err(primary),
                Err(terminal_replay_refusal_for_test().await),
            )
            .unwrap_err();
            let serialized = SerializableError::from(Error::new(reverie::Error::Tool(paired)));
            assert_eq!(serialized.kind(), kind);
            assert!(
                serialized.error.contains("Incomplete"),
                "secondary must survive serialization"
            );
        }
        let paired = detcore::combine_finalization::<()>(
            Err(terminal_replay_refusal_for_test().await),
            Err(Error::new(std::io::Error::other("secondary write failure"))),
        )
        .unwrap_err();
        let serialized = SerializableError::from(paired);
        assert_eq!(serialized.kind(), FailureKind::PolicyRefusal);
        assert!(serialized.error.contains("secondary write failure"));
    }

    #[test]
    fn an_unreadable_first_pass_refusal_keeps_its_policy_class_across_the_container_boundary() {
        let config = detcore::Config {
            sequentialize_threads: true,
            backend: crate::backend_capabilities(crate::Backend::Kvm),
            sleep_registration: Some(detcore_model::config::SleepRegistration::FirstPass),
            ..detcore::Config::default()
        };
        let refusal =
            crate::sleep_registration_admission::refuse_unreadable_first_pass_launch(&config)
                .unwrap_err();
        let serialized = SerializableError::from(Error::new(refusal).context("launch refused"));
        assert_eq!(serialized.kind(), FailureKind::PolicyRefusal);
        assert!(serialized.context.iter().any(|message| {
            message.contains("unreadable_sleeper_signal_state")
                && message.contains("--sleep-registration=first-pass")
        }));
    }

    #[test]
    fn unsupported_preemption_keeps_its_policy_class_across_the_container_boundary() {
        let refusal = crate::preemption_admission::refuse_unsupported_preemption_with_capabilities(
            &detcore::Config::default(),
            "dbt",
            reverie::BackendCapabilities::DBT,
            true,
            false,
        )
        .unwrap_err();
        let serialized = SerializableError::from(Error::new(refusal).context("launch refused"));
        assert_eq!(serialized.kind(), FailureKind::PolicyRefusal);
        assert!(serialized.context.iter().any(|message| {
            message.contains("unsupported_preemption") && message.contains("--backend=dbt")
        }));
    }

    #[test]
    fn a_kvm_address_space_refusal_keeps_its_policy_class_across_the_container_boundary() {
        let refusal = crate::kvm_address_space::KvmAddressSpaceRefusal::for_limit(
            libc::rlimit {
                rlim_cur: 16 << 30,
                rlim_max: 16 << 30,
            },
            3 << 30,
        )
        .expect("a finite limit is refused");
        let serialized = SerializableError::from(Error::new(refusal).context("launch refused"));
        assert_eq!(serialized.kind(), FailureKind::PolicyRefusal);
        assert!(serialized.context.iter().any(|message| {
            message.contains("kvm_address_space_limit") && message.contains("--backend=kvm")
        }));
    }

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
                cleanup_stage: None,
            }
        );
    }

    #[test]
    fn from_serializable_error() {
        let error = Error::from(SerializableError {
            error: "c".into(),
            context: vec!["b".into(), "a".into(), "root cause".into()],
            kind: FailureKind::Error,
            cleanup_stage: None,
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

    /// The library's in-guest LiteInst refusals run inside the container, so
    /// they reach the CLI only as this serialized form.
    #[test]
    fn in_guest_liteinst_refusal_is_serialized_as_a_policy_refusal() {
        let refusal = SerializableError::from(Error::new(crate::LiteinstInGuestRefusal::new(
            "guest program /static is statically linked",
        )));
        assert_eq!(refusal.kind(), FailureKind::PolicyRefusal);
        assert_eq!(
            refusal.error,
            "--backend=liteinst (in-guest LiteInst) refuses this run: guest program /static is \
             statically linked"
        );
    }

    /// reverie-ptrace ends a run whose guest made a syscall through Reverie's
    /// untraced private-page stub; that is a policy refusal (exit 122), not
    /// an internal failure, both as the backend's primary cause and wrapped
    /// (https://github.com/rrnewton/reverie/issues/1064).
    #[test]
    fn an_untraced_stub_syscall_is_a_policy_refusal() {
        let refusal = || reverie_ptrace::UntracedStubSyscall {
            tid: 7,
            address: 0x7100_0002,
        };
        let primary = reverie::Error::Tool(Error::new(refusal()));
        assert_eq!(
            classify_ptrace_primary(&primary),
            FailureKind::PolicyRefusal
        );
        let wrapped = Error::new(reverie::Error::Tool(Error::new(refusal())))
            .context("the ptrace backend failed");
        assert_eq!(classify_unwrapped(&wrapped), FailureKind::PolicyRefusal);
        let serialized = SerializableError::from(Error::new(refusal()));
        assert_eq!(serialized.kind(), FailureKind::PolicyRefusal);
        assert!(
            serialized.error.contains("0x71000002"),
            "{}",
            serialized.error
        );
    }

    /// A finished run's terminal outcome (a deadlock under Hermit's model, a
    /// virtual timeout) keeps its own outcome and crosses the container
    /// boundary with its kind; one left as the primary of a failed ptrace run
    /// (its teardown failed or never confirmed) is Hermit's failure, so the
    /// unclean teardown is not reported as the outcome.
    #[test]
    fn a_terminal_outcome_keeps_its_kind_only_once_the_run_finished() {
        for outcome in [
            detcore::TerminalOutcome::GuestDeadlock,
            detcore::TerminalOutcome::VirtualTimeout { bound_ns: 5 },
        ] {
            let finished =
                SerializableError::from(Error::new(crate::SchedulerEndedRun::new(outcome, None)));
            assert_eq!(finished.kind(), FailureKind::TerminalOutcome(outcome));
            let primary =
                reverie::Error::Tool(Error::new(crate::SchedulerEndedRun::new(outcome, None)));
            assert_eq!(classify_ptrace_primary(&primary), FailureKind::Error);
        }
    /// Detcore's refusal of a self-sentinel pidfd_send_signal whose backend
    /// could not qualify its spelling is a policy refusal, through ptrace's
    /// Tool wrapper and across the container boundary, and keeps its reason.
    #[test]
    fn an_unqualified_pidfd_self_sentinel_is_a_policy_refusal() {
        let refusal = || detcore::PidfdSelfQualificationRefusal { sentinel: -10001 };
        let primary = reverie::Error::Tool(Error::new(refusal()));
        assert_eq!(
            classify_ptrace_primary(&primary),
            FailureKind::PolicyRefusal
        );
        let wrapped = Error::new(reverie::Error::Tool(Error::new(refusal())))
            .context("the ptrace backend failed");
        assert_eq!(classify_unwrapped(&wrapped), FailureKind::PolicyRefusal);
        let serialized = SerializableError::from(Error::new(refusal()));
        assert_eq!(serialized.kind(), FailureKind::PolicyRefusal);
        assert!(
            serialized.error.contains("self sentinel -10001"),
            "{}",
            serialized.error
        );
    }

    /// An in-guest-trap refusal names that backend and keeps the same class.
    #[test]
    fn in_guest_trap_refusal_is_serialized_as_a_policy_refusal() {
        let refusal =
            SerializableError::from(Error::new(crate::LiteinstInGuestRefusal::for_backend(
                crate::Backend::InGuestTrap,
                "guest program /static is statically linked",
            )));
        assert_eq!(refusal.kind(), FailureKind::PolicyRefusal);
        assert_eq!(
            refusal.error,
            "--backend=in-guest-trap (in-guest LiteInst, site patching off) refuses this run: \
             guest program /static is statically linked"
        );
    }
}
