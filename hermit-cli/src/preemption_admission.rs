/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Refuse timer requests that the selected runtime cannot implement.
//!
//! This is launch policy, not a timer implementation or a replacement for
//! Detcore's scheduling policy. Cooperative scheduling at existing guest
//! events remains available without a preemption timer.

use detcore::Config;

use crate::Backend;

/// A named, pre-guest refusal of an unsupported preemption setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedPreemption {
    backend: Backend,
    setting: &'static str,
}

impl std::fmt::Display for UnsupportedPreemption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unsupported_preemption: --backend={} refuses {}: this runtime cannot deliver \
             Detcore preemption timer events; the guest has not started",
            self.backend.as_str(),
            self.setting,
        )?;
        if self.setting == "--max-timeslice" {
            write!(
                f,
                "; to request execution without that bound, pass --max-timeslice=disabled"
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for UnsupportedPreemption {}

/// Check command-line requests before launching a guest or consuming input.
/// `maximum_requested` records parser provenance, including config-file and
/// `--from` inputs. The inherited maximum is not a timer request: it also
/// controls shared clock accounting and cooperative scheduling, and stays
/// unchanged. `skid_margin_requested` represents the CLI-only PMU setting.
///
/// This check never rewrites the configuration. Library APIs without command-
/// line provenance retain their existing admission behavior.
/// A schedule-event recording or cooperative target alone does not request a
/// timer. Preemption-point replay does, and is refused without inspecting or
/// silently reducing the requested recording to a cooperative subset.
pub fn refuse_unsupported_preemption(
    config: &Config,
    backend: Backend,
    maximum_requested: bool,
    skid_margin_requested: bool,
) -> Result<(), UnsupportedPreemption> {
    if crate::backend_delivers_preemption_timer(backend) {
        return Ok(());
    }
    let setting = if maximum_requested && config.max_timeslice.is_some() {
        "--max-timeslice"
    } else if config.imprecise_timers {
        "--imprecise-timers"
    } else if !config.interrupt_at.is_empty() {
        "--interrupt-at"
    } else if config.preemption_stacktrace {
        "--preemption-stacktrace"
    } else if config.preemption_stacktrace_log_file.is_some() {
        "--preemption-stacktrace-log-file"
    } else if config.panic_on_rcb_overshoot {
        "--panic-on-rbc-overshoot (--panic-on-rcb-overshoot)"
    } else if skid_margin_requested {
        "--skid-margin"
    } else if config.replay_preemptions_from.is_some() {
        "--replay-preemptions-from"
    } else {
        return Ok(());
    };
    Err(UnsupportedPreemption { backend, setting })
}

/// A named, pre-guest refusal of a configuration that cannot replay a
/// callback-annotated priority history (`Config::annotated_priority_replay`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnqualifiedAnnotatedReplay {
    /// The selected backend for a command line; `None` for a library launch,
    /// whose configuration is judged on the capabilities it carries.
    backend: Option<Backend>,
    setting: &'static str,
}

impl std::fmt::Display for UnqualifiedAnnotatedReplay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "HERMIT_CALLBACK_CHECKPOINT_UNQUALIFIED_MODE: --replay-preemptions-from names a record \
             with callback checkpoints, which replay only with precise PMU preemption timer \
             events; "
        )?;
        match self.backend {
            Some(backend) => write!(f, "--backend={} {}", backend.as_str(), self.setting)?,
            None => write!(f, "the launch configuration {}", self.setting)?,
        }
        write!(f, "; the guest has not started")
    }
}

impl std::error::Error for UnqualifiedAnnotatedReplay {}

/// Refuse to replay a callback-annotated priority history where its row ends
/// cannot be honored: they are actual callback endpoints at exact RCB counts,
/// guarded one RCB past each. The requirements are
/// `Config::annotated_priority_replay_unqualified`'s, judged here on the
/// command line's configuration with the selected backend's capabilities, so
/// the refusal comes before input is read and names the backend. Without
/// them a replay would neither reach nor bound those endpoints, and silently
/// falling back to the legacy expiry would replay the wrong schedule. A
/// record without checkpoints keeps its existing admission; this check never
/// rewrites the configuration.
pub fn refuse_unqualified_annotated_replay(
    config: &Config,
    backend: Backend,
) -> Result<(), UnqualifiedAnnotatedReplay> {
    let effective = Config {
        backend: crate::backend_capabilities(backend),
        ..config.clone()
    };
    match effective.annotated_priority_replay_unqualified() {
        None => Ok(()),
        Some(setting) => Err(UnqualifiedAnnotatedReplay {
            backend: Some(backend),
            setting,
        }),
    }
}

/// The library launches' backstop for the same requirements, on the
/// prepared configuration a backend is about to run: a library caller passes
/// no command line, and its configuration reaches here unqualified. An
/// annotated configuration must meet `annotated_priority_replay_unqualified`
/// on the capabilities it carries. Its record is not reread here: Detcore
/// refuses a record whose checkpoints disagree with the flag before any
/// thread starts. Nothing is changed or forced.
pub fn refuse_unqualified_annotated_launch(
    config: &Config,
) -> Result<(), UnqualifiedAnnotatedReplay> {
    if !config.annotated_priority_replay {
        return Ok(());
    }
    match config.annotated_priority_replay_unqualified() {
        None => Ok(()),
        Some(setting) => Err(UnqualifiedAnnotatedReplay {
            backend: None,
            setting,
        }),
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    const TIMERLESS: [Backend; 5] = [
        Backend::Dbt,
        Backend::Sabre,
        Backend::Liteinst,
        Backend::InGuestTrap,
        Backend::Kvm,
    ];

    #[test]
    fn explicit_active_maximum_is_refused_without_normalizing_the_request() {
        for args in [
            vec!["detcore"],
            vec!["detcore", "--max-timeslice=1000"],
            vec!["detcore", "--preemption-timeout=1000"],
            vec!["detcore", "--no-rcb-time"],
        ] {
            let config = Config::parse_from(args);
            let before = serde_json::to_value(&config).unwrap();
            for backend in TIMERLESS {
                let error =
                    refuse_unsupported_preemption(&config, backend, true, false).unwrap_err();
                assert_eq!(error.setting, "--max-timeslice");
                let message = error.to_string();
                assert!(message.contains(backend.as_str()), "{message}");
                assert!(message.contains("unsupported_preemption"), "{message}");
                assert!(
                    message.contains("pass --max-timeslice=disabled"),
                    "{message}"
                );
                assert_eq!(serde_json::to_value(&config).unwrap(), before);
            }
            refuse_unsupported_preemption(&config, Backend::Ptrace, true, false).unwrap();
        }
    }

    #[test]
    fn explicit_timer_controls_still_refuse_after_disabling_the_maximum() {
        for (args, setting) in [
            (vec!["--imprecise-timers"], "--imprecise-timers"),
            (vec!["--interrupt-at=3:1000"], "--interrupt-at"),
            (vec!["--preemption-stacktrace"], "--preemption-stacktrace"),
            (
                vec!["--preemption-stacktrace-log-file=unused"],
                "--preemption-stacktrace-log-file",
            ),
            (
                vec!["--panic-on-rbc-overshoot"],
                "--panic-on-rbc-overshoot (--panic-on-rcb-overshoot)",
            ),
            (
                vec!["--panic-on-rcb-overshoot"],
                "--panic-on-rbc-overshoot (--panic-on-rcb-overshoot)",
            ),
            (
                vec!["--replay-preemptions-from=unused"],
                "--replay-preemptions-from",
            ),
        ] {
            let config = Config::parse_from(
                ["detcore", "--max-timeslice=disabled"]
                    .into_iter()
                    .chain(args),
            );
            for backend in TIMERLESS {
                let error =
                    refuse_unsupported_preemption(&config, backend, true, false).unwrap_err();
                assert_eq!(error.setting, setting);
            }
            refuse_unsupported_preemption(&config, Backend::Ptrace, true, false).unwrap();
        }
        let config = Config::parse_from(["detcore", "--max-timeslice=disabled"]);
        for backend in TIMERLESS {
            assert_eq!(
                refuse_unsupported_preemption(&config, backend, true, true)
                    .unwrap_err()
                    .setting,
                "--skid-margin",
            );
        }
        refuse_unsupported_preemption(&config, Backend::Ptrace, true, true).unwrap();
    }

    /// A callback-annotated priority history replays only with precise PMU
    /// preemption timer events and RCB time. Every other configuration is
    /// refused by name, without rewriting it; the qualified one passes.
    #[test]
    fn an_annotated_priority_history_replays_only_with_precise_rcb_timers() {
        let qualified = Config::parse_from([
            "detcore",
            "--chaos",
            "--sequentialize-threads",
            "--max-timeslice=1000000",
            "--replay-preemptions-from=unused",
        ]);
        refuse_unqualified_annotated_replay(&qualified, Backend::Ptrace).unwrap();
        for backend in TIMERLESS {
            let refusal = refuse_unqualified_annotated_replay(&qualified, backend).unwrap_err();
            assert_eq!(
                refusal.setting,
                "cannot deliver Detcore preemption timer events"
            );
            let message = refusal.to_string();
            assert!(
                message.starts_with("HERMIT_CALLBACK_CHECKPOINT_UNQUALIFIED_MODE"),
                "{message}"
            );
            assert!(message.contains(backend.as_str()), "{message}");
        }
        for (flags, setting) in [
            (
                ["--max-timeslice=1000000", "--chaos"],
                "does not sequentialize threads (--no-sequentialize-threads)",
            ),
            (
                ["--max-timeslice=disabled", "--sequentialize-threads"],
                "has its preemption timer disabled (--max-timeslice=disabled)",
            ),
            (
                ["--sequentialize-threads", "--imprecise-timers"],
                "requests imprecise timers (--imprecise-timers)",
            ),
            (
                ["--sequentialize-threads", "--no-rcb-time"],
                "requests logical time without RCBs (--no-rcb-time)",
            ),
        ] {
            let config = Config::parse_from(
                ["detcore", "--replay-preemptions-from=unused"]
                    .into_iter()
                    .chain(flags),
            );
            let flag = flags[1];
            let before = serde_json::to_value(&config).unwrap();
            let refusal =
                refuse_unqualified_annotated_replay(&config, Backend::Ptrace).unwrap_err();
            assert_eq!(refusal.setting, setting, "{flag}");
            assert_eq!(serde_json::to_value(&config).unwrap(), before);
        }
    }

    /// A library launch is judged on the prepared configuration's own
    /// capabilities: an annotated configuration nobody prepared (no timer
    /// events), or one without sequentialized threads, is refused by name,
    /// without being changed; the same configuration with ptrace's
    /// capabilities and every requirement met passes, and an unannotated
    /// configuration keeps its existing admission.
    #[test]
    fn a_library_launch_of_an_annotated_history_is_judged_on_its_own_configuration() {
        let unprepared = Config {
            annotated_priority_replay: true,
            ..Config::parse_from([
                "detcore",
                "--sequentialize-threads",
                "--max-timeslice=1000000",
                "--replay-preemptions-from=unused",
            ])
        };
        let refusal = refuse_unqualified_annotated_launch(&unprepared).unwrap_err();
        assert_eq!(
            refusal.setting,
            "cannot deliver Detcore preemption timer events"
        );
        let message = refusal.to_string();
        assert!(
            message.starts_with("HERMIT_CALLBACK_CHECKPOINT_UNQUALIFIED_MODE")
                && message.contains("the launch configuration cannot deliver"),
            "{message}"
        );
        let prepared = Config {
            backend: crate::backend_capabilities(Backend::Ptrace),
            ..unprepared.clone()
        };
        refuse_unqualified_annotated_launch(&prepared).unwrap();
        let unsequentialized = Config {
            sequentialize_threads: false,
            ..prepared.clone()
        };
        let before = serde_json::to_value(&unsequentialized).unwrap();
        assert_eq!(
            refuse_unqualified_annotated_launch(&unsequentialized)
                .unwrap_err()
                .setting,
            "does not sequentialize threads (--no-sequentialize-threads)"
        );
        assert_eq!(serde_json::to_value(&unsequentialized).unwrap(), before);
        let unannotated = Config {
            annotated_priority_replay: false,
            ..unsequentialized
        };
        refuse_unqualified_annotated_launch(&unannotated).unwrap();
    }

    #[test]
    fn cooperative_and_boundary_event_settings_do_not_require_a_timer() {
        for disabled in [
            "--max-timeslice=disabled",
            "--max-timeslice=0",
            "--preemption-timeout=0",
        ] {
            let config = Config::parse_from([
                "detcore",
                disabled,
                "--chaos",
                "--chaos-per-thread-slowdown",
                "--target-timeslice=1000",
                "--target-timeslice-syscalls-only",
                "--record-preemptions-to=unused",
                "--replay-schedule-from=unused-schedule",
                "--stacktrace-event=1",
                "--chaos-slowdown-max-factor=4.5",
                "--chaos-epoch-length-ns=1000",
            ]);
            for backend in TIMERLESS {
                refuse_unsupported_preemption(&config, backend, true, false).unwrap();
            }
        }
    }
}
