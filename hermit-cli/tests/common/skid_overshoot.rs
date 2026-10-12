/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The typed PMU skid-overshoot refusal of one finished Hermit run, shared by
//! the integration tests that retry such a refusal
//! (https://github.com/rrnewton/hermit/issues/1845). A refusal measures the
//! host's PMU interrupt latency, not the guest
//! (https://github.com/rrnewton/reverie/issues/990).

use std::process::Output;

use detcore_model::HERMIT_POLICY_REFUSAL_EXIT;

/// The prefix of the canonical line Hermit prints when it refuses a run for
/// PMU skid overshoot: `classify_failure` in hermit-cli/src/bin/hermit/main.rs
/// writes `HERMIT_POLICY_REFUSAL class=policy-refusal cause=skid-overshoot
/// count=<n>`, the same line the manifest runner requires of a skid attempt
/// (`skid_overshoot_only_reports` in ci/manifest-plan/src/runner.rs).
pub const SKID_OVERSHOOT_REFUSAL_PREFIX: &str =
    "HERMIT_POLICY_REFUSAL class=policy-refusal cause=skid-overshoot count=";

/// Line prefixes that mark a second outcome beside a skid refusal: Hermit's
/// other one-line classifications (another policy refusal, an internal
/// failure, a signal death, a run timeout, a log cap) and a task panic.
const SKID_RETRY_DISQUALIFYING_PREFIXES: [&str; 6] = [
    "HERMIT_POLICY_REFUSAL",
    "HERMIT_INTERNAL_FAILURE",
    "HERMIT_SIGNAL_DEATH",
    "HERMIT_RUN_TIMEOUT",
    "HERMIT_LOG_CAP",
    "HERMIT_TASK_PANIC",
];

/// The canonical skid-overshoot refusal line of one finished `hermit run`,
/// or `None` unless that run is a typed skid-overshoot refusal and nothing
/// else (https://github.com/rrnewton/hermit/issues/1845). All must hold:
///
/// - the run exited with `HERMIT_POLICY_REFUSAL_EXIT` (122); a signal, a
///   deadline kill (`None`) or any other status never qualifies;
/// - its stderr carries exactly one refusal line, and it is
///   [`SKID_OVERSHOOT_REFUSAL_PREFIX`] followed by a decimal count above zero;
/// - its stderr carries at least one line that starts with
///   `HERMIT_SKID_OVERSHOOT `, Hermit's own report of the late PMU interrupt;
/// - no other line starts with a [`SKID_RETRY_DISQUALIFYING_PREFIXES`] entry
///   and none contains `panicked at`.
///
/// A refusal for any other cause, a refusal beside a second failure, or a
/// marker without the exit status is a result the caller must report as it
/// is, not a reason to run again.
pub fn skid_overshoot_refusal(code: Option<i32>, stderr: &str) -> Option<&str> {
    if code != Some(HERMIT_POLICY_REFUSAL_EXIT) {
        return None;
    }
    let mut refusal = None;
    let mut overshoot_reported = false;
    for line in stderr.lines() {
        let line = line.trim_end();
        if line.starts_with("HERMIT_SKID_OVERSHOOT ") {
            overshoot_reported = true;
        } else if let Some(count) = line.strip_prefix(SKID_OVERSHOOT_REFUSAL_PREFIX) {
            let counted = !count.is_empty()
                && count.bytes().all(|byte| byte.is_ascii_digit())
                && count.parse::<u64>().is_ok_and(|count| count > 0);
            if !counted || refusal.is_some() {
                return None;
            }
            refusal = Some(line);
        } else if line.contains("panicked at")
            || SKID_RETRY_DISQUALIFYING_PREFIXES
                .iter()
                .any(|prefix| line.starts_with(prefix))
        {
            return None;
        }
    }
    refusal.filter(|_| overshoot_reported)
}

/// Runs `attempt` at most `max_attempts` times, again only after an attempt
/// whose output is a typed skid-overshoot refusal ([`skid_overshoot_refusal`])
/// (https://github.com/rrnewton/hermit/issues/1845). Returns the attempt
/// number and output of the first attempt that is not such a refusal, or of
/// the last attempt, with whatever `attempt` returned beside its output; the
/// caller checks that output as it would check a single run. Each retried
/// attempt prints a `SKID-RETRY` line with its refusal to stderr.
pub fn retry_typed_skid_refusal<T>(
    name: &str,
    max_attempts: u32,
    mut attempt: impl FnMut() -> (Output, T),
) -> (u32, Output, T) {
    for number in 1..=max_attempts {
        let (output, kept) = attempt();
        if number < max_attempts {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if let Some(refusal) = skid_overshoot_refusal(output.status.code(), &stderr) {
                eprintln!(
                    "SKID-RETRY {name}: attempt {number} of at most {max_attempts} exited \
                     {HERMIT_POLICY_REFUSAL_EXIT} with `{refusal}`; running again \
                     (https://github.com/rrnewton/hermit/issues/1845)"
                );
                continue;
            }
        }
        return (number, output, kept);
    }
    unreachable!("the last attempt always returns")
}
