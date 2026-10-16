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

/// Check an effective configuration before launching a guest or consuming its
/// input. `skid_margin_requested` represents the CLI-only PMU setting; library
/// callers pass false because that setting is absent from [`Config`].
///
/// The check does not rewrite the effective configuration. The CLI resolves
/// an omitted maximum from the runtime's capability before calling this check;
/// direct library callers must supply their intended effective maximum.
/// A schedule-event recording or cooperative target alone does not request a
/// timer. Preemption-point replay does, and is refused without inspecting or
/// silently reducing the requested recording to a cooperative subset.
pub fn refuse_unsupported_preemption(
    config: &Config,
    backend: Backend,
    skid_margin_requested: bool,
) -> Result<(), UnsupportedPreemption> {
    if crate::backend_delivers_preemption_timer(backend) {
        return Ok(());
    }
    let setting = if config.max_timeslice.is_some() {
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
    } else if config.chaos && config.chaos_per_thread_slowdown {
        "--chaos-per-thread-slowdown"
    } else {
        return Ok(());
    };
    Err(UnsupportedPreemption { backend, setting })
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
    fn active_maximum_is_refused_without_normalizing_the_request() {
        for args in [
            vec!["detcore"],
            vec!["detcore", "--max-timeslice=1000"],
            vec!["detcore", "--preemption-timeout=1000"],
            vec!["detcore", "--no-rcb-time"],
        ] {
            let config = Config::parse_from(args);
            let before = serde_json::to_value(&config).unwrap();
            for backend in TIMERLESS {
                let error = refuse_unsupported_preemption(&config, backend, false).unwrap_err();
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
            refuse_unsupported_preemption(&config, Backend::Ptrace, false).unwrap();
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
            (
                vec!["--chaos", "--chaos-per-thread-slowdown"],
                "--chaos-per-thread-slowdown",
            ),
        ] {
            let config = Config::parse_from(
                ["detcore", "--max-timeslice=disabled"]
                    .into_iter()
                    .chain(args),
            );
            for backend in TIMERLESS {
                let error = refuse_unsupported_preemption(&config, backend, false).unwrap_err();
                assert_eq!(error.setting, setting);
            }
            refuse_unsupported_preemption(&config, Backend::Ptrace, false).unwrap();
        }
        let config = Config::parse_from(["detcore", "--max-timeslice=disabled"]);
        for backend in TIMERLESS {
            assert_eq!(
                refuse_unsupported_preemption(&config, backend, true)
                    .unwrap_err()
                    .setting,
                "--skid-margin",
            );
        }
        refuse_unsupported_preemption(&config, Backend::Ptrace, true).unwrap();
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
                "--target-timeslice=1000",
                "--target-timeslice-syscalls-only",
                "--record-preemptions-to=unused",
                "--replay-schedule-from=unused-schedule",
                "--stacktrace-event=1",
                "--chaos-slowdown-max-factor=4.5",
                "--chaos-epoch-length-ns=1000",
            ]);
            for backend in TIMERLESS {
                refuse_unsupported_preemption(&config, backend, false).unwrap();
            }
        }
    }
}
