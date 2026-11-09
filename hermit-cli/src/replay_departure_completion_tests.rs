/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Stage2 of the replay-departure regression: the public associated-type RPC
//! lifecycle, actual coordinator cleanup, and the real library completion and
//! container classifiers. Stage1 in Detcore separately exercises the private
//! stale producer. This module executes no guest process or backend.

use detcore::ReplayDepartureError;

use super::BackendRunFailure;
use super::Error;
use super::ExitStatus;
use super::Output;
use super::SchedulerEndedRun;
use super::complete_ptrace_run;
use super::error::FailureKind;
use super::error::SerializableError;
use super::replay_departure_test_fixture::Publication;
use super::replay_departure_test_fixture::accepted_cleanup;
use super::replay_departure_test_fixture::ordinary_cleanup;
use super::replay_departure_test_fixture::receiver_cleanup;

/// This is the real same-image container representation and its actual bincode
/// codec. Do not reconstruct the policy class by parsing Display after decoding.
fn assert_container_kind(error: Error, expected: FailureKind) {
    let serialized = SerializableError::from(error);
    assert_eq!(serialized.kind(), expected);
    assert_eq!(serialized.cleanup_stage(), None);
    let bytes = bincode::serde::encode_to_vec(&serialized, bincode::config::legacy()).unwrap();
    let (decoded, used): (SerializableError, usize) =
        bincode::serde::decode_from_slice(&bytes, bincode::config::legacy()).unwrap();
    assert_eq!(used, bytes.len());
    assert_eq!(decoded, serialized);
    assert_eq!(decoded.kind(), expected);
}

fn output(status: ExitStatus) -> Output {
    Output {
        status,
        stdout: b"stdout\0\xff\n".to_vec(),
        stderr: b"stderr\0\xfe\n".to_vec(),
    }
}

#[tokio::test]
async fn actual_departure_cleanup_survives_typed_context_tool_and_container_codec() {
    for terminal_context in [false, true] {
        for tool_wrapper in [false, true] {
            let status = ExitStatus::Exited(if terminal_context {
                detcore::TerminalOutcome::GuestDeadlock.exit_status()
            } else {
                0
            });
            let completed = if terminal_context {
                Err(SchedulerEndedRun::new(
                    detcore::TerminalOutcome::GuestDeadlock,
                    Some(output(status)),
                ))
            } else {
                Ok(output(status))
            };
            let error =
                complete_ptrace_run(completed, accepted_cleanup().await, status).unwrap_err();
            assert!(ReplayDepartureError::in_error(&error).is_some());
            assert_eq!(error.is::<SchedulerEndedRun>(), terminal_context);
            let error = if tool_wrapper {
                let wrapped = reverie::Error::Tool(error);
                assert_eq!(
                    super::error::classify_ptrace_primary(&wrapped),
                    FailureKind::PolicyRefusal
                );
                Error::new(wrapped)
            } else {
                error
            };
            assert!(ReplayDepartureError::in_error(&error).is_some());
            assert_container_kind(error, FailureKind::PolicyRefusal);
        }
    }
}

#[tokio::test]
async fn actual_cleanup_preserves_ordinary_statuses_and_owned_terminal_output() {
    for code in [0, 7, 122] {
        let status = ExitStatus::Exited(code);
        let completed = complete_ptrace_run(Ok(status), ordinary_cleanup().await, status);
        assert_eq!(
            SchedulerEndedRun::status_of(completed).unwrap(),
            (status, None)
        );
        let expected = output(status);
        let completed = complete_ptrace_run(Ok(expected.clone()), ordinary_cleanup().await, status);
        assert_eq!(
            SchedulerEndedRun::output_of(completed).unwrap(),
            (expected, None)
        );
    }
    for outcome in [
        detcore::TerminalOutcome::GuestDeadlock,
        detcore::TerminalOutcome::VirtualTimeout { bound_ns: 17 },
    ] {
        let status = ExitStatus::Exited(outcome.exit_status());
        let completed = complete_ptrace_run::<ExitStatus>(
            Err(SchedulerEndedRun::new(outcome, None)),
            ordinary_cleanup().await,
            status,
        );
        assert_eq!(
            SchedulerEndedRun::status_of(completed).unwrap(),
            (status, Some(outcome))
        );
        let completed = complete_ptrace_run::<ExitStatus>(
            Err(SchedulerEndedRun::new(outcome, None)),
            accepted_cleanup().await,
            status,
        );
        let error = SchedulerEndedRun::status_of(completed)
            .expect_err("departure must refuse the status-only terminal result too");
        assert!(ReplayDepartureError::in_error(&error).is_some());
        let owned = error.downcast::<SchedulerEndedRun>().unwrap();
        assert_eq!(owned.outcome, outcome);
        assert!(owned.output.is_none());
        let expected = output(status);
        let completed = complete_ptrace_run::<Output>(
            Err(SchedulerEndedRun::new(outcome, Some(expected.clone()))),
            ordinary_cleanup().await,
            status,
        );
        assert_eq!(
            SchedulerEndedRun::output_of(completed).unwrap(),
            (expected.clone(), Some(outcome))
        );

        let completed = complete_ptrace_run::<Output>(
            Err(SchedulerEndedRun::new(outcome, Some(expected.clone()))),
            accepted_cleanup().await,
            status,
        );
        let error = SchedulerEndedRun::output_of(completed)
            .expect_err("a terminal outcome must not convert departure into successful completion");
        assert!(ReplayDepartureError::in_error(&error).is_some());
        assert_eq!(
            error
                .downcast_ref::<SchedulerEndedRun>()
                .unwrap()
                .output
                .as_ref(),
            Some(&expected)
        );
        let owned = error.downcast::<SchedulerEndedRun>().unwrap();
        assert_eq!(owned.outcome, outcome);
        assert_eq!(owned.into_output().unwrap(), expected);
    }
}

#[tokio::test]
async fn rejected_publication_refuses_real_cleanup_without_fabricating_a_finding() {
    ordinary_cleanup().await.unwrap();
    let (cleanup, marker) = receiver_cleanup(Publication::Advancing, false).await;
    assert_eq!(
        marker, None,
        "a rejected report is not accepted recording provenance"
    );
    let status = ExitStatus::Exited(122);
    let error = complete_ptrace_run(Ok(status), cleanup, status).unwrap_err();
    assert!(
        ReplayDepartureError::in_error(&error)
            .unwrap()
            .summary()
            .is_none()
    );
    assert_container_kind(error, FailureKind::PolicyRefusal);
}

#[tokio::test]
async fn genuine_summary_io_error_remains_primary_over_actual_departure() {
    let (cleanup, marker) = receiver_cleanup(Publication::Accepted, true).await;
    assert_eq!(
        marker,
        Some(0),
        "recording finalization precedes the failed summary write"
    );
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
    let status = ExitStatus::Exited(detcore::TerminalOutcome::GuestDeadlock.exit_status());
    let error = complete_ptrace_run::<Output>(
        Err(SchedulerEndedRun::new(
            detcore::TerminalOutcome::GuestDeadlock,
            Some(output(status)),
        )),
        Err(error),
        status,
    )
    .unwrap_err();
    assert!(
        !error.is::<SchedulerEndedRun>(),
        "genuine cleanup failure stays primary"
    );
    assert!(ReplayDepartureError::in_error(&error).is_none());
    assert_container_kind(Error::new(reverie::Error::Tool(error)), FailureKind::Error);
}

#[tokio::test]
async fn primary_backend_and_io_failures_keep_their_kind_with_actual_departure() {
    for backend in [false, true] {
        // Only the pre-existing primary is modeled here. The secondary is
        // always the actual error obtained from receiver/lifecycle/cleanup.
        let directory = tempfile::tempdir().unwrap();
        let primary: Error = if backend {
            BackendRunFailure.into()
        } else {
            std::fs::read(directory.path().join("absent"))
                .unwrap_err()
                .into()
        };
        let expected = if backend {
            FailureKind::BackendRunFailure
        } else {
            FailureKind::Error
        };
        let error = detcore::combine_finalization::<()>(Err(primary), accepted_cleanup().await)
            .unwrap_err();
        let pair = error
            .downcast_ref::<detcore::FinalizationFailure>()
            .unwrap();
        assert!(ReplayDepartureError::in_error(pair.secondary()).is_some());
        assert!(ReplayDepartureError::in_error(&error).is_none());
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
        assert_container_kind(Error::new(reverie::Error::Tool(error)), expected);
    }
}
