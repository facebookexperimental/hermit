/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The three bin adapters consume actual receiver/cleanup errors. The shared
//! fixture models the public RPC lifecycle; it does not run a backend. Stage1
//! covers the private stale producer and the library tests cover the real
//! ptrace completion seam, including its owned terminal context.

use detcore::ReplayDepartureError;
use detcore::TerminalOutcome;
use hermit::BackendRunFailure;
use hermit::Error;
use hermit::ExitStatus;
use hermit::FailureKind;
use hermit::Output;
use hermit::SchedulerEndedRun;
use hermit::SerializableError;
use hermit::canonical_verdict::RunFailure;

use super::HERMIT_INTERNAL_FAILURE_EXIT;
use super::classify_failure;
use super::container::SchedulerOutcome;
use super::container::classify_container_result;
use super::failure_exit_code;
use super::replay_departure_test_fixture::Publication;
use super::replay_departure_test_fixture::accepted_cleanup;
use super::replay_departure_test_fixture::ordinary_cleanup;
use super::replay_departure_test_fixture::receiver_cleanup;
use super::run::rejected_first_run;
use super::verify::run_failure_of;

async fn actual_departure(publication: Publication) -> Error {
    match publication {
        Publication::Accepted => accepted_cleanup().await.unwrap_err(),
        Publication::Advancing => {
            let (cleanup, marker) = receiver_cleanup(publication, false).await;
            assert_eq!(marker, None, "rejected publication is not a finding");
            let error = cleanup.unwrap_err();
            assert!(
                ReplayDepartureError::in_error(&error)
                    .unwrap()
                    .summary()
                    .is_none()
            );
            error
        }
        Publication::None => panic!("ordinary cleanup is a separate positive control"),
    }
}

fn output(status: ExitStatus) -> Output {
    Output {
        status,
        stdout: b"bin-stdout\0\xff\n".to_vec(),
        stderr: b"bin-stderr\0\xfe\n".to_vec(),
    }
}

fn assert_direct_refusal(error: &Error, publication: Publication) {
    assert_eq!(
        failure_exit_code(error),
        detcore_model::HERMIT_POLICY_REFUSAL_EXIT
    );
    assert_eq!(
        classify_failure(error),
        match publication {
            Publication::Accepted =>
                "HERMIT_POLICY_REFUSAL class=policy-refusal cause=replay-departure count=1",
            Publication::Advancing =>
                "HERMIT_POLICY_REFUSAL class=policy-refusal cause=replay-departure-publication",
            Publication::None => panic!("ordinary cleanup is a separate positive control"),
        },
    );
    assert_eq!(run_failure_of(error), Some(RunFailure::PolicyRefusal));
}

/// Use the actual same-image codec and the production container-result adapter,
/// not a hand-made PolicyRefusal or a classifier inferred from Display text.
fn through_container(error: Error, kind: FailureKind) -> Error {
    let encoded = SerializableError::from(error);
    assert_eq!(encoded.kind(), kind);
    assert_eq!(encoded.cleanup_stage(), None);
    let bytes = bincode::serde::encode_to_vec(&encoded, bincode::config::legacy()).unwrap();
    let (decoded, used): (SerializableError, usize) =
        bincode::serde::decode_from_slice(&bytes, bincode::config::legacy()).unwrap();
    assert_eq!(used, bytes.len());
    assert_eq!(decoded, encoded);
    classify_container_result::<()>(Ok(Err(decoded))).unwrap_err()
}

#[derive(Clone, Copy)]
enum ErrorForm {
    Direct,
    Context,
    Tool,
    OwnedTerminal,
}

#[tokio::test]
async fn actual_departure_is_policy_in_cli_verify_and_container_adapters() {
    ordinary_cleanup().await.unwrap();
    for publication in [Publication::Accepted, Publication::Advancing] {
        for form in [
            ErrorForm::Direct,
            ErrorForm::Context,
            ErrorForm::Tool,
            ErrorForm::OwnedTerminal,
        ] {
            let error = actual_departure(publication).await;
            let terminal_output = output(ExitStatus::Exited(
                TerminalOutcome::GuestDeadlock.exit_status(),
            ));
            let error = match form {
                ErrorForm::Direct => error,
                ErrorForm::Context => error.context("returning the actual coordinator cleanup"),
                ErrorForm::Tool => Error::new(reverie::Error::Tool(error)),
                ErrorForm::OwnedTerminal => {
                    // The finding/error is obtained from cleanup, not constructed
                    // here. Only the terminal completion input is modeled; the
                    // library tests separately call the real complete_ptrace_run.
                    let actual = error.downcast::<ReplayDepartureError>().unwrap();
                    Error::new(SchedulerEndedRun::new(
                        TerminalOutcome::GuestDeadlock,
                        Some(terminal_output.clone()),
                    ))
                    .context(actual)
                }
            };
            assert_direct_refusal(&error, publication);
            if matches!(form, ErrorForm::OwnedTerminal) {
                assert_eq!(
                    error
                        .downcast_ref::<SchedulerEndedRun>()
                        .unwrap()
                        .output
                        .as_ref(),
                    Some(&terminal_output),
                );
            }
            let restored = through_container(error, FailureKind::PolicyRefusal);
            // The existing container wire carries the policy class, not the
            // in-process departure type/count. Do not invent the latter here.
            assert!(ReplayDepartureError::in_error(&restored).is_none());
            assert!(restored.is::<super::container::PolicyRefusal>());
            assert_eq!(
                failure_exit_code(&restored),
                detcore_model::HERMIT_POLICY_REFUSAL_EXIT
            );
            assert_eq!(
                classify_failure(&restored),
                "HERMIT_POLICY_REFUSAL class=policy-refusal"
            );
            assert_eq!(run_failure_of(&restored), Some(RunFailure::PolicyRefusal));
        }
    }
}

#[tokio::test]
async fn rejected_first_run_keeps_actual_departure_ahead_of_terminal_context() {
    for publication in [Publication::Accepted, Publication::Advancing] {
        for outcome in [
            None,
            Some(TerminalOutcome::GuestDeadlock),
            Some(TerminalOutcome::VirtualTimeout { bound_ns: 17 }),
        ] {
            for tool in [false, true] {
                let error = actual_departure(publication).await;
                let error = if tool {
                    Error::new(reverie::Error::Tool(error))
                } else {
                    error
                };
                let before = format!("{error:#}");
                let error = rejected_first_run(error, outcome);
                assert!(error.downcast_ref::<SchedulerOutcome>().is_none());
                assert_eq!(format!("{error:#}"), before);
                assert_direct_refusal(&error, publication);
            }
        }
    }
}

#[tokio::test]
async fn ordinary_guest_122_terminal_and_untyped_errors_keep_their_contracts() {
    // A completed guest status is a value, not a failed container's status.
    // Ordinary cleanup must leave 122 on the successful result channel.
    ordinary_cleanup().await.unwrap();
    let expected = output(ExitStatus::Exited(122));
    let actual = classify_container_result(Ok(Ok(expected.clone()))).unwrap();
    assert_eq!(
        SchedulerEndedRun::output_of(Ok(actual)).unwrap(),
        (expected, None)
    );

    for outcome in [
        TerminalOutcome::GuestDeadlock,
        TerminalOutcome::VirtualTimeout { bound_ns: 17 },
    ] {
        ordinary_cleanup().await.unwrap();
        let expected = output(ExitStatus::Exited(outcome.exit_status()));
        let error = Error::new(SchedulerEndedRun::new(outcome, Some(expected.clone())));
        let error = rejected_first_run(error, Some(outcome));
        assert!(ReplayDepartureError::in_error(&error).is_none());
        assert_eq!(error.downcast_ref::<SchedulerOutcome>().unwrap().0, outcome);
        assert_eq!(failure_exit_code(&error), outcome.exit_status());
        assert_eq!(classify_failure(&error), outcome.class_line());
        assert_eq!(
            run_failure_of(&error),
            match outcome {
                TerminalOutcome::GuestDeadlock => Some(RunFailure::Deadlock),
                TerminalOutcome::VirtualTimeout { .. } => None,
            },
        );
        // Neither classification nor the verification adapter consumes the
        // real owned status/output represented by SchedulerEndedRun.
        let owned = error.downcast::<SchedulerEndedRun>().unwrap();
        assert_eq!(owned.outcome, outcome);
        assert_eq!(owned.into_output().unwrap(), expected);
    }

    // Identical departure prose without its type must not fabricate a refusal.
    let text = actual_departure(Publication::Accepted).await.to_string();
    let error = rejected_first_run(Error::msg(text.clone()), None);
    assert_eq!(error.to_string(), text);
    assert_eq!(failure_exit_code(&error), HERMIT_INTERNAL_FAILURE_EXIT);
    assert_eq!(
        classify_failure(&error),
        "HERMIT_INTERNAL_FAILURE class=cli-error"
    );
    assert_eq!(run_failure_of(&error), None);
}

#[tokio::test]
async fn genuine_primary_failures_are_not_reclassified_by_secondary_departure() {
    // This is a real summary write failure produced by the same cleanup which
    // found a departure. No I/O error or finalization pair is fabricated.
    let (cleanup, marker) = receiver_cleanup(Publication::Accepted, true).await;
    assert_eq!(marker, Some(0));
    let error = cleanup.unwrap_err();
    let pair = error
        .downcast_ref::<detcore::FinalizationFailure>()
        .unwrap();
    assert_eq!(
        pair.primary()
            .downcast_ref::<std::io::Error>()
            .unwrap()
            .kind(),
        std::io::ErrorKind::IsADirectory
    );
    assert!(ReplayDepartureError::in_error(pair.secondary()).is_some());
    assert!(ReplayDepartureError::in_error(&error).is_none());
    let before = format!("{error:#}");
    let error = rejected_first_run(error, None);
    assert_eq!(format!("{error:#}"), before);
    assert_eq!(failure_exit_code(&error), HERMIT_INTERNAL_FAILURE_EXIT);
    assert_eq!(
        classify_failure(&error),
        "HERMIT_INTERNAL_FAILURE class=cli-error"
    );
    assert_eq!(run_failure_of(&error), None);
    let restored = through_container(Error::new(reverie::Error::Tool(error)), FailureKind::Error);
    assert_eq!(failure_exit_code(&restored), HERMIT_INTERNAL_FAILURE_EXIT);
    assert_eq!(
        classify_failure(&restored),
        "HERMIT_INTERNAL_FAILURE class=cli-error"
    );
    assert_eq!(run_failure_of(&restored), None);

    for backend in [false, true] {
        for with_departure in [false, true] {
            // Only the pre-existing backend failure is modeled. The I/O error
            // and every departure are obtained from actual production paths.
            let directory = tempfile::tempdir().unwrap();
            let primary: Error = if backend {
                BackendRunFailure.into()
            } else {
                std::fs::read(directory.path().join("absent"))
                    .unwrap_err()
                    .into()
            };
            let cleanup = if with_departure {
                accepted_cleanup().await
            } else {
                ordinary_cleanup().await
            };
            let error = detcore::combine_finalization::<()>(Err(primary), cleanup).unwrap_err();
            assert!(ReplayDepartureError::in_error(&error).is_none());
            if with_departure {
                let pair = error
                    .downcast_ref::<detcore::FinalizationFailure>()
                    .unwrap();
                assert!(ReplayDepartureError::in_error(pair.secondary()).is_some());
                if backend {
                    assert!(pair.primary().is::<BackendRunFailure>());
                } else {
                    assert_eq!(
                        pair.primary()
                            .downcast_ref::<std::io::Error>()
                            .unwrap()
                            .kind(),
                        std::io::ErrorKind::NotFound
                    );
                }
            }
            let before = format!("{error:#}");
            let error = rejected_first_run(error, None);
            assert_eq!(format!("{error:#}"), before);
            assert_eq!(failure_exit_code(&error), HERMIT_INTERNAL_FAILURE_EXIT);
            assert_eq!(
                classify_failure(&error),
                "HERMIT_INTERNAL_FAILURE class=cli-error"
            );
            // The raw composite is opaque to this existing verification adapter;
            // its primary kind is recovered by the actual container classifier.
            assert_eq!(
                run_failure_of(&error),
                if backend && !with_departure {
                    Some(RunFailure::BackendFailure)
                } else {
                    None
                }
            );
            let kind = if backend {
                FailureKind::BackendRunFailure
            } else {
                FailureKind::Error
            };
            let restored = through_container(error, kind);
            assert_eq!(failure_exit_code(&restored), HERMIT_INTERNAL_FAILURE_EXIT);
            assert_eq!(
                classify_failure(&restored),
                "HERMIT_INTERNAL_FAILURE class=cli-error"
            );
            assert_eq!(
                run_failure_of(&restored),
                if backend {
                    Some(RunFailure::BackendFailure)
                } else {
                    None
                }
            );
        }
    }
}
