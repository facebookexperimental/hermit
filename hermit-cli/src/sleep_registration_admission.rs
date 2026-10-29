/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Refuse a queued-sleep registration rule the selected backend cannot honour.
//!
//! First-pass registration (`--sleep-registration=first-pass`, and by default
//! outside race search) parks every plain sleep as a timed waiter. The
//! scheduler leaves a parked sleep alone for a signal Linux would not let end
//! it only where it reads the sleeper's signal mask and dispositions
//! (`Config::reads_sleeper_signal_state`). Where it does not (DBT, SaBRe and
//! LiteInst without the in-guest delivery gate, KVM), a child's
//! default-ignored `SIGCHLD` or a signal the sleeper blocks would end every
//! such sleep with `EINTR`, which Linux never does. The default registers when
//! chosen there. An explicit `first-pass`, or a replayed record that
//! registered at the first pass, is refused here by name rather than replaced
//! silently, so a run does what it was asked to and its record says what it
//! did.

use std::path::Path;

use detcore::Config;
use detcore_model::config::SleepRegistration;

/// A named, pre-guest refusal of first-pass sleep registration on a backend
/// whose sleepers' signal state the scheduler does not read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnreadableSleeperSignalState {
    /// What asked for the first pass: the option, or the replayed record.
    source: FirstPassSource,
    /// Why the scheduler reads no sleeper's signal state in this run.
    unread: Unread,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unread {
    /// `--no-sequentialize-threads`: the scheduler models no signal targets.
    Unsequentialized,
    /// The backend reports no sleeper's signal state, and the in-guest
    /// delivery gate is off.
    Backend,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FirstPassSource {
    /// `--sleep-registration=first-pass`.
    Option,
    /// A replayed record that registered at the first pass, by its option
    /// and path.
    Record { option: &'static str, path: String },
}

impl std::fmt::Display for UnreadableSleeperSignalState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (source, remedy) = match &self.source {
            FirstPassSource::Option => (
                "--sleep-registration=first-pass".to_owned(),
                "pass --sleep-registration=when-chosen, or leave it unset, to register sleeps \
                 when chosen",
            ),
            FirstPassSource::Record { option, path } => (
                format!("the record of {option} {path}"),
                "replay it on a backend that reads that state",
            ),
        };
        let unread = match self.unread {
            Unread::Unsequentialized => "under --no-sequentialize-threads it does not",
            Unread::Backend => "this backend does not report that state",
        };
        write!(
            f,
            "unreadable_sleeper_signal_state: {source} registers queued sleeps at the first \
             pass, which needs the scheduler to read sleepers' signal mask and dispositions \
             (ptrace or e9patch with sequentialized threads, or the in-guest delivery gate); \
             {unread}, so a parked sleep would be ended by signals Linux ignores or the sleeper \
             blocks; {remedy}; the guest has not started"
        )
    }
}

impl std::error::Error for UnreadableSleeperSignalState {}

/// Refuse a launch whose configuration registers queued sleeps at the first
/// pass (`detcore::preemptions::requests_first_pass_sleep_registration`: the
/// explicit option, or a replayed record's rule) on a backend whose sleepers'
/// signal state the scheduler does not read, judged on the final
/// configuration the backend is about to run. The default never asks for the
/// first pass there (`Config::queued_sleeps_registered_at_first_pass`), and
/// `hermit record`/`replay` never register at it, so neither is refused. This
/// check never rewrites the configuration.
pub fn refuse_unreadable_first_pass_launch(
    config: &Config,
) -> Result<(), UnreadableSleeperSignalState> {
    if config.reads_sleeper_signal_state()
        || !detcore::preemptions::requests_first_pass_sleep_registration(config)
    {
        return Ok(());
    }
    let source = if config.sleep_registration == Some(SleepRegistration::FirstPass) {
        FirstPassSource::Option
    } else {
        let (option, path) = match (
            &config.replay_schedule_from,
            &config.replay_preemptions_from,
        ) {
            (Some(path), _) => ("--replay-schedule-from", path),
            (None, Some(path)) => ("--replay-preemptions-from", path),
            (None, None) => unreachable!("only a replayed record asks for the first pass unset"),
        };
        FirstPassSource::Record {
            option,
            path: Path::new(path).display().to_string(),
        }
    };
    let unread = if config.sequentialize_threads {
        Unread::Backend
    } else {
        Unread::Unsequentialized
    };
    Err(UnreadableSleeperSignalState { source, unread })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Backend;

    fn prepared(backend: Backend, sleep_registration: Option<SleepRegistration>) -> Config {
        Config {
            sequentialize_threads: true,
            backend: crate::backend_capabilities(backend),
            sleep_registration,
            ..Config::default()
        }
    }

    const UNREADABLE: [Backend; 5] = [
        Backend::Dbt,
        Backend::Sabre,
        Backend::Liteinst,
        Backend::InGuestTrap,
        Backend::Kvm,
    ];

    /// An explicit `first-pass` is refused by name on every backend whose
    /// sleepers' signal state the scheduler does not read, and admitted on
    /// ptrace and under the in-guest delivery gate. The default and
    /// `when-chosen` are never refused, nor is anything under `hermit
    /// record`/`replay`.
    #[test]
    fn an_explicit_first_pass_is_refused_where_signal_state_is_not_read() {
        for backend in UNREADABLE {
            let name = backend.as_str();
            let refusal = refuse_unreadable_first_pass_launch(&prepared(
                backend,
                Some(SleepRegistration::FirstPass),
            ))
            .expect_err(name)
            .to_string();
            assert!(
                refusal.starts_with(
                    "unreadable_sleeper_signal_state: --sleep-registration=first-pass registers"
                ),
                "{name}: {refusal}"
            );
            assert!(
                refusal.contains("--sleep-registration=when-chosen"),
                "{refusal}"
            );
            assert!(
                refusal.contains("; this backend does not report that state, so"),
                "{refusal}"
            );
            assert!(refusal.ends_with("the guest has not started"), "{refusal}");
            for registration in [None, Some(SleepRegistration::WhenChosen)] {
                assert_eq!(
                    refuse_unreadable_first_pass_launch(&prepared(backend, registration)),
                    Ok(()),
                    "{name}, {registration:?}"
                );
            }
            let gated = Config {
                in_guest_signal_delivery: true,
                ..prepared(backend, Some(SleepRegistration::FirstPass))
            };
            assert_eq!(
                refuse_unreadable_first_pass_launch(&gated),
                Ok(()),
                "{name}"
            );
            let recorded = Config {
                recordreplay_modes: true,
                ..prepared(backend, Some(SleepRegistration::FirstPass))
            };
            assert_eq!(
                refuse_unreadable_first_pass_launch(&recorded),
                Ok(()),
                "{name}"
            );
        }
        for registration in [
            None,
            Some(SleepRegistration::FirstPass),
            Some(SleepRegistration::WhenChosen),
        ] {
            assert_eq!(
                refuse_unreadable_first_pass_launch(&prepared(Backend::Ptrace, registration)),
                Ok(()),
                "ptrace, {registration:?}"
            );
        }
        // Without sequentialized threads the scheduler models no signal
        // targets, on ptrace too, and the refusal says so.
        let unsequentialized = Config {
            sequentialize_threads: false,
            ..prepared(Backend::Ptrace, Some(SleepRegistration::FirstPass))
        };
        let refusal = refuse_unreadable_first_pass_launch(&unsequentialized)
            .expect_err("first-pass without sequentialized threads")
            .to_string();
        assert!(
            refusal.contains("; under --no-sequentialize-threads it does not, so"),
            "{refusal}"
        );
    }

    /// A replay that leaves the option unset takes its record's rule, so a
    /// record that registered at the first pass is refused by name on a
    /// backend that does not read sleepers' signal state, and one that
    /// registered when chosen is not.
    #[test]
    fn a_replayed_first_pass_record_is_refused_where_signal_state_is_not_read() {
        let dir = tempfile::tempdir().unwrap();
        for first_pass in [true, false] {
            let path = dir.path().join(format!("record-{first_pass}.json"));
            let record = detcore::preemptions::PreemptionRecord::from_sched_events(Vec::new())
                .with_queued_sleeps_registered_at_first_pass(first_pass);
            std::fs::write(&path, serde_json::to_string(&record).unwrap()).unwrap();
            for schedule in [true, false] {
                let config = Config {
                    replay_schedule_from: schedule.then(|| path.clone()),
                    replay_preemptions_from: (!schedule).then(|| path.clone()),
                    ..prepared(Backend::Dbt, None)
                };
                let result = refuse_unreadable_first_pass_launch(&config);
                if first_pass {
                    let refusal = result.expect_err("a first-pass record").to_string();
                    let option = if schedule {
                        "--replay-schedule-from"
                    } else {
                        "--replay-preemptions-from"
                    };
                    assert!(
                        refusal.contains(&format!("the record of {option} {}", path.display())),
                        "{refusal}"
                    );
                } else {
                    assert_eq!(result, Ok(()), "a when-chosen record, schedule {schedule}");
                }
                let on_ptrace = Config {
                    backend: crate::backend_capabilities(Backend::Ptrace),
                    ..config
                };
                assert_eq!(refuse_unreadable_first_pass_launch(&on_ptrace), Ok(()));
            }
        }
    }
}
