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
/// PMU skid overshoot: `classify_failure` in hermit-cli/src/bin/hermit/mod.rs
/// writes `HERMIT_POLICY_REFUSAL class=policy-refusal cause=skid-overshoot
/// count=<n>`, the same line the manifest runner requires of a skid attempt
/// (`skid_overshoot_only_reports` in ci/manifest-plan/src/runner.rs).
pub const SKID_OVERSHOOT_REFUSAL_PREFIX: &str =
    "HERMIT_POLICY_REFUSAL class=policy-refusal cause=skid-overshoot count=";

/// Line prefixes that mark a second outcome beside a skid refusal: Hermit's
/// other one-line classifications (another policy refusal, an internal
/// failure, a signal death, a run timeout, a log cap, a guest deadlock) and a
/// task panic.
const SKID_RETRY_DISQUALIFYING_PREFIXES: [&str; 7] = [
    "HERMIT_POLICY_REFUSAL",
    "HERMIT_INTERNAL_FAILURE",
    "HERMIT_SIGNAL_DEATH",
    "HERMIT_RUN_TIMEOUT",
    "HERMIT_LOG_CAP",
    "HERMIT_MODEL_DEADLOCK",
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

/// The file `ci/run-nextest-counted.sh` collects retried attempts in, one line
/// each, and counts at the end of its run, so a validate log shows how many
/// skid refusals were retried even when every test then passed. Unset outside
/// that runner.
pub const SKID_RETRY_RECORD_ENV: &str = "HERMIT_SKID_RETRY_RECORD";

/// The file innermost harnesses append one `RetryRecordV1` JSON line per
/// retried test to (see `ledger::retry_rollup`). Set by
/// `ci/run-nextest-counted.sh` beside [`SKID_RETRY_RECORD_ENV`].
pub const RETRY_RECORD_ENV: &str = "HERMIT_RETRY_RECORD_FILE";

/// The distinct verdict when every attempt was a typed skid refusal:
/// the one shared spelling ([`detcore_model::SKID_INFRA_VERDICT`]).
pub const SKID_INFRA_VERDICT: &str = detcore_model::SKID_INFRA_VERDICT;

/// The file `ci/run-nextest-counted.sh` collects exhausted-skid tests in,
/// one `HERMIT-SKID-INFRA: <test>\t<reason>` line each, from which
/// `ci/nextest-test-results.rs` classifies the failed test's terminal
/// attempt as an infrastructure error instead of an ordinary failure.
/// Unset outside that runner.
pub const SKID_INFRA_FILE_ENV: &str = "HERMIT_SKID_INFRA_FILE";

/// Record that the running test exhausted its skid budget on typed
/// refusals only: print the `HERMIT-SKID-INFRA` marker for humans and
/// append it, as one `write()` on an `O_APPEND` descriptor, to the
/// [`SKID_INFRA_FILE_ENV`] sidecar the counted runner reads. Only
/// [`skid_overshoot_refusal`]-verified exhaustion reaches this (see
/// [`retry_typed_skid_refusal`]), so no other failure can claim the
/// verdict. A marker that cannot be written fails the test, so the
/// count never under-reports silently.
pub fn record_skid_infra(name: &str, refusal: &str, max_attempts: u32) {
    use std::io::Write;
    let test = std::thread::current()
        .name()
        .unwrap_or("unnamed")
        .to_owned();
    let reason = format!(
        "all {max_attempts} attempt(s) exited {HERMIT_POLICY_REFUSAL_EXIT} with `{refusal}`; verdict {SKID_INFRA_VERDICT} (not a test failure)"
    );
    eprintln!("SKID-INFRA {name}: {reason}");
    println!("HERMIT-SKID-INFRA: {test}\t{reason}");
    let Some(path) = std::env::var_os(SKID_INFRA_FILE_ENV).filter(|path| !path.is_empty()) else {
        return;
    };
    let line = format!("HERMIT-SKID-INFRA: {test}\t{reason}\n");
    assert!(
        line.len() < 4096,
        "skid-infra marker for {test} is {} bytes; keep it under one PIPE_BUF line",
        line.len()
    );
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap_or_else(|error| panic!("cannot open {SKID_INFRA_FILE_ENV}={path:?}: {error}"));
    let written = file
        .write(line.as_bytes())
        .unwrap_or_else(|error| panic!("cannot write {SKID_INFRA_FILE_ENV}={path:?}: {error}"));
    assert_eq!(
        written,
        line.len(),
        "a skid-infra marker must land in one write() so parallel appends cannot interleave"
    );
}

/// One `RetryRecordV1` JSON line for a test that needed `retries` skid
/// retries and then finished with outcome `final_outcome`: `PASS`,
/// `FAIL`, or [`SKID_INFRA_VERDICT`] when every attempt was a typed skid
/// refusal (its final class stays `skid_overshoot`, never
/// `product_failure`).
pub fn retry_record_json(name: &str, attempts: u64, final_outcome: &str) -> String {
    let retries = attempts.saturating_sub(1);
    let mut outcomes = vec!["ERROR"; retries as usize];
    outcomes.push(final_outcome);
    let mut classes: Vec<Option<&str>> = vec![Some("skid_overshoot"); retries as usize];
    classes.push(match final_outcome {
        "PASS" => None,
        SKID_INFRA_VERDICT => Some("skid_overshoot"),
        _ => Some("product_failure"),
    });
    serde_json::json!({
        "test": name, "mode": "nextest", "backend": null,
        "attempts": attempts, "retries": retries,
        "attempt_outcomes": outcomes, "attempt_classes": classes,
        "final_outcome": final_outcome,
        "flaky_pass": final_outcome == "PASS" && retries > 0,
    })
    .to_string()
}

/// Append the [`RETRY_RECORD_ENV`] line for a finished retried test. A
/// `SKID-INFRA` finish is recorded even at one attempt: it is a verdict,
/// not a retry count.
fn record_retry(name: &str, attempts: u64, final_outcome: &str) {
    use std::io::Write;
    if attempts <= 1 && final_outcome != SKID_INFRA_VERDICT {
        return;
    }
    let Some(path) = std::env::var_os(RETRY_RECORD_ENV).filter(|path| !path.is_empty()) else {
        return;
    };
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
        .unwrap_or_else(|error| panic!("cannot open {RETRY_RECORD_ENV}={path:?}: {error}"));
    file.write_all(format!("{}\n", retry_record_json(name, attempts, final_outcome)).as_bytes())
        .unwrap_or_else(|error| panic!("cannot write {RETRY_RECORD_ENV}={path:?}: {error}"));
}

/// The file the per-test CPU wrapper (ci/manifest-plan/src/bin/
/// nextest-cpu-wrapper.rs) gives this test process. Each retry line written
/// here earns the next attempt one more base CPU budget, so a retried run does
/// not have to fit in what the refused attempt left. Unset outside that
/// wrapper.
pub const SKID_RETRY_CREDIT_ENV: &str = "HERMIT_SKID_RETRY_CREDIT";

/// Append `line` to the [`SKID_RETRY_CREDIT_ENV`] file, and, prefixed with the
/// running test's name, to the [`SKID_RETRY_RECORD_ENV`] file. A line that
/// cannot be written fails the test, so the count never under-reports
/// silently.
pub fn record_skid_retry(line: &str) {
    use std::io::Write;
    if let Some(path) = std::env::var_os(SKID_RETRY_CREDIT_ENV).filter(|path| !path.is_empty()) {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap_or_else(|error| {
                panic!("cannot open {SKID_RETRY_CREDIT_ENV}={path:?}: {error}")
            });
        // One write, so the wrapper never counts a partial line.
        file.write_all(format!("{line}\n").as_bytes())
            .unwrap_or_else(|error| {
                panic!("cannot write {SKID_RETRY_CREDIT_ENV}={path:?}: {error}")
            });
    }
    let Some(path) = std::env::var_os(SKID_RETRY_RECORD_ENV).filter(|path| !path.is_empty()) else {
        return;
    };
    let test = std::thread::current()
        .name()
        .unwrap_or("unnamed")
        .to_owned();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
        .unwrap_or_else(|error| panic!("cannot open {SKID_RETRY_RECORD_ENV}={path:?}: {error}"));
    // One write per line keeps concurrent tests' records whole (O_APPEND).
    file.write_all(format!("{test}\t{line}\n").as_bytes())
        .unwrap_or_else(|error| panic!("cannot write {SKID_RETRY_RECORD_ENV}={path:?}: {error}"));
}

/// Runs `attempt` at most `max_attempts` times, again only after an attempt
/// whose output is a typed skid-overshoot refusal ([`skid_overshoot_refusal`])
/// (https://github.com/rrnewton/hermit/issues/1845). Returns the attempt
/// number and output of the first attempt that is not such a refusal, or of
/// the last attempt, with whatever `attempt` returned beside its output; the
/// caller checks that output as it would check a single run. Each retried
/// attempt prints a `SKID-RETRY` line with its refusal to stderr. When the
/// last attempt is also such a refusal the budget is exhausted: the test's
/// verdict is [`SKID_INFRA_VERDICT`], recorded with [`record_skid_infra`]
/// before the final output returns to the caller (whose assertion on it
/// still fails the raw nextest row, which the counted runner then
/// classifies as an infrastructure error, not a test failure). The
/// verdict is recorded only when the exhausted bound is the one shared
/// skid budget ([`detcore_model::SKID_RETRY_MAX_ATTEMPTS`]): a caller
/// that deliberately runs fewer attempts (a single-attempt probe) has
/// not exhausted that budget, so its refusal stays an ordinary result
/// for the caller to fail on. `validate_final` is the caller's own
/// check of the final refused output (guest stdout/disposition beyond
/// the typed refusal); it runs before anything is recorded, so a final
/// output that fails it stays an ordinary failure with no marker. The
/// skid attempts' guest stdout must also be identical across attempts,
/// as one deterministic run's refusals produce.
pub fn retry_typed_skid_refusal<T>(
    name: &str,
    max_attempts: u32,
    mut attempt: impl FnMut() -> (Output, T),
    validate_final: &dyn Fn(&Output),
) -> (u32, Output, T) {
    let mut first_skid_stdout: Option<Vec<u8>> = None;
    for number in 1..=max_attempts {
        let (output, kept) = attempt();
        let stderr = String::from_utf8_lossy(&output.stderr);
        if let Some(refusal) = skid_overshoot_refusal(output.status.code(), &stderr) {
            let refusal = refusal.to_owned();
            // Every skid attempt of one deterministic run produces the
            // same guest stdout; a final attempt whose stdout differs
            // from the first skid attempt's carries a second outcome
            // beside the refusal and earns no verdict.
            let stdout_stable = match &first_skid_stdout {
                None => {
                    first_skid_stdout = Some(output.stdout.clone());
                    true
                }
                Some(first) => *first == output.stdout,
            };
            if number < max_attempts {
                let line = format!(
                    "SKID-RETRY {name}: attempt {number} of at most {max_attempts} exited \
                     {HERMIT_POLICY_REFUSAL_EXIT} with `{refusal}`; running again \
                     (https://github.com/rrnewton/hermit/issues/1845)"
                );
                eprintln!("{line}");
                record_skid_retry(&line);
                continue;
            }
            if max_attempts == detcore_model::SKID_RETRY_MAX_ATTEMPTS && stdout_stable {
                // The caller's own check of the final output runs before
                // anything is recorded: a check that panics leaves an
                // ordinary failure and no marker.
                validate_final(&output);
                record_skid_infra(name, &refusal, max_attempts);
                record_retry(name, u64::from(number), SKID_INFRA_VERDICT);
            } else {
                record_retry(name, u64::from(number), "FAIL");
            }
            return (number, output, kept);
        }
        record_retry(
            name,
            u64::from(number),
            if output.status.code() == Some(0) {
                "PASS"
            } else {
                "FAIL"
            },
        );
        return (number, output, kept);
    }
    unreachable!("the last attempt always returns")
}
