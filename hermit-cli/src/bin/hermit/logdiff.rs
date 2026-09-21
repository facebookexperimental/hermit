/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use clap::Parser;
use detcore::logdiff;
use hermit::HERMIT_VERIFICATION_DIVERGENCE_EXIT;
use hermit::logdiff_report::LOG_DIFF_REPORT_SCHEMA;
use hermit::logdiff_report::LogDiffComparison;
use hermit::logdiff_report::LogDiffInput;
use hermit::logdiff_report::LogDiffInputs;
use hermit::logdiff_report::LogDiffMessageCounts;
use hermit::logdiff_report::LogDiffRecords;
use hermit::logdiff_report::LogDiffReport;
use hermit::logdiff_report::LogDiffVerdict;
use reverie::process::ExitStatus;
use tempfile::NamedTempFile;

use super::global_opts::GlobalOpts;
use super::record_envelope::RecordEnvelope;
use super::record_envelope::RecordEnvelopeArg;
use super::record_envelope::RecordEnvelopePolicy;
use super::verify::lock_failed_verification_logs_for_read;

fn write_canonical_info(
    file: &Path,
    writer: &mut impl Write,
    record_envelope: RecordEnvelope,
) -> std::io::Result<usize> {
    if record_envelope.policy() == RecordEnvelopePolicy::AllRecordsV1 {
        let bytes = std::fs::read(file)?;
        return logdiff::write_bitwise_info_v1_bytes(&bytes, &file.display().to_string(), writer);
    }
    logdiff::write_canonical_info_with_filter(file, writer, record_envelope.predicate())
}

fn try_log_diff_with_records(
    left: &Path,
    right: &Path,
    options: &logdiff::LogDiffOpts,
    record_envelope: RecordEnvelope,
) -> std::io::Result<(logdiff::LogDiffSummary, usize, usize, LogDiffInputs)> {
    let left_bytes = std::fs::read(left)?;
    let right_bytes = std::fs::read(right)?;
    let left = strict_log_text(&left_bytes, &left.display().to_string())?;
    let right = strict_log_text(&right_bytes, &right.display().to_string())?;
    let summary = logdiff::log_diff_summary_from_strs_with_filter(
        left,
        right,
        options,
        &mut std::io::stderr(),
        record_envelope.predicate(),
    )?;
    let identity = |bytes: &[u8]| LogDiffInput {
        sha256: detcore::Digest::new(bytes).to_string(),
        bytes: bytes.len() as u64,
    };
    Ok((
        summary,
        logdiff::record_count(left),
        logdiff::record_count(right),
        LogDiffInputs {
            left: identity(&left_bytes),
            right: identity(&right_bytes),
        },
    ))
}

fn strict_log_text<'a>(bytes: &'a [u8], label: &str) -> std::io::Result<&'a str> {
    std::str::from_utf8(bytes).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{label} is not UTF-8: {error}"),
        )
    })
}

fn try_bitwise_info_v1_with_records(
    left: &Path,
    right: &Path,
    options: &logdiff::LogDiffOpts,
) -> std::io::Result<(logdiff::LogDiffSummary, usize, usize)> {
    let mut diagnostic_output = std::io::stderr();
    logdiff::try_compare_bitwise_info_v1_with_records_and_diagnostics(
        left,
        right,
        options.side_labels.clone(),
        logdiff::BitwiseInfoV1Diagnostics {
            difference_limit: options.limit,
            syscall_history: options.syscall_history,
            no_color: options.no_color,
            print_logs: options.print_logs,
        },
        &mut diagnostic_output,
    )
}

fn compare_complete_prefix(
    left: &[u8],
    right: &[u8],
    options: &logdiff::LogDiffOpts,
    writer: &mut impl Write,
    record_envelope: RecordEnvelope,
    fixed_bitwise_info_v1: bool,
) -> std::io::Result<logdiff::PrefixComparison> {
    if fixed_bitwise_info_v1 {
        return logdiff::compare_complete_bitwise_info_v1_prefix(
            left,
            right,
            options.side_labels.clone(),
            logdiff::BitwiseInfoV1Diagnostics {
                difference_limit: options.limit,
                syscall_history: options.syscall_history,
                no_color: options.no_color,
                print_logs: options.print_logs,
            },
            writer,
        );
    }
    let left = strict_log_text(left, "left log")?;
    let right = strict_log_text(right, "right log")?;
    logdiff::compare_complete_prefix_with_filter(
        left,
        right,
        options,
        writer,
        record_envelope.predicate(),
    )
}

/// Command-line options for the "logdiff" subcommand.
#[derive(Debug, Parser)]
pub struct LogDiffCLIOpts {
    /// A log to print canonically, or the first of two logs to compare.
    file_a: PathBuf,
    /// Optional second log. With one input, print the canonical INFO stream
    /// used by strict verification; with two inputs, compare them.
    file_b: Option<PathBuf>,

    /// Write one machine-readable JSON line for a two-log comparison.
    #[clap(long, value_name = "FILE")]
    json: Option<PathBuf>,

    /// Compare the canonical INFO stream used by --verify-strict within the
    /// selected record envelope. With --json this comparison is mandatory and
    /// selected automatically.
    #[clap(long)]
    canonical_info: bool,

    /// Versioned record envelope applied before selecting messages. DBT logs
    /// must opt into their transport envelope explicitly; the default preserves
    /// every parsed record.
    #[clap(long, value_enum, default_value = "all-records-v1")]
    record_envelope: RecordEnvelopeArg,

    /// Compare two runs that are still being written, reporting divergence as
    /// soon as it appears instead of waiting for both to finish. Only records
    /// both logs have finished writing are compared, so a half-flushed tail is
    /// never mistaken for a difference.
    #[clap(long)]
    follow: bool,

    /// How often to re-read the logs while following.
    #[clap(long, value_name = "MILLISECONDS", default_value = "500")]
    follow_interval_ms: u64,

    /// Give up following after this long. 0 waits indefinitely. Timing out is
    /// reported as its own outcome, never as agreement.
    #[clap(long, value_name = "SECONDS", default_value = "300")]
    follow_timeout_secs: u64,

    /// Consecutive polls with both logs unchanged before they are treated as
    /// quiescent and following stops.
    #[clap(long, value_name = "POLLS", default_value = "3")]
    follow_settle_polls: u32,

    #[clap(flatten)]
    pub more: logdiff::LogDiffOpts,
}

impl LogDiffCLIOpts {
    /// Construct LogDiffOpts to compare two files.
    pub fn new(a: &Path, b: &Path) -> Self {
        Self {
            file_a: PathBuf::from(a),
            file_b: Some(PathBuf::from(b)),
            json: None,
            canonical_info: false,
            record_envelope: RecordEnvelopeArg::AllRecordsV1,
            follow: false,
            follow_interval_ms: 500,
            follow_timeout_secs: 300,
            follow_settle_polls: 3,
            more: Default::default(),
        }
    }

    fn one_input_uses_only_canonical_options(&self) -> bool {
        let defaults = logdiff::LogDiffOpts::default();
        !self.more.strip_lines
            && self.more.limit == defaults.limit
            && self.more.ignore_lines.is_empty()
            && self.more.syscall_history == defaults.syscall_history
            && !self.more.no_color
            && !self.more.skip_commit
            && !self.more.skip_detlog
            && !self.more.git_diff
            && self.more.include_detlogs == defaults.include_detlogs
    }

    /// Print one log canonically or compare two logs.
    pub fn main(&self, _global: &GlobalOpts) -> ExitStatus {
        let mut inputs = vec![self.file_a.clone()];
        inputs.extend(self.file_b.iter().cloned());
        let _retention_locks = match lock_failed_verification_logs_for_read(inputs) {
            Ok(locks) => locks,
            Err(error) => {
                eprintln!("hermit log-diff: could not protect retained logs: {error}");
                return ExitStatus::Exited(2);
            }
        };
        let record_envelope = self.record_envelope.envelope();
        eprintln!(
            "hermit log-diff: record envelope {}",
            record_envelope.policy().as_str()
        );
        let Some(file_b) = &self.file_b else {
            if self.json.is_some() {
                eprintln!("hermit log-diff: --json requires two input logs");
                return ExitStatus::Exited(2);
            }
            if !self.one_input_uses_only_canonical_options() {
                eprintln!(
                    "hermit log-diff: comparison options require two input logs; one input always prints the canonical INFO stream used by strict verification"
                );
                return ExitStatus::Exited(2);
            }
            return match write_canonical_info(
                &self.file_a,
                &mut std::io::stdout().lock(),
                record_envelope,
            ) {
                Ok(0) => {
                    eprintln!(
                        "hermit log-diff: {} contains no comparable INFO messages",
                        self.file_a.display()
                    );
                    ExitStatus::Exited(2)
                }
                Ok(_) => ExitStatus::Exited(0),
                Err(error) => {
                    eprintln!(
                        "hermit log-diff: could not canonicalize {}: {error}",
                        self.file_a.display()
                    );
                    ExitStatus::Exited(2)
                }
            };
        };

        let mut options = self.more.clone();
        if self.canonical_info || self.json.is_some() {
            if let Err(message) = canonical_comparison_is_unrelaxed(&options) {
                eprintln!("hermit log-diff: {message}");
                return ExitStatus::Exited(2);
            }
            options.comparison = logdiff::LogComparisonMode::Info;
            options.canonicalize_addresses = true;
        }
        if record_envelope.policy() == RecordEnvelopePolicy::CrossBackendDetcoreV1 {
            options.require_structured_events = true;
        }

        if let Some(path) = &self.json
            && let Err(error) = write_json(
                path,
                &pending_json_report(&options, record_envelope.policy()),
            )
        {
            eprintln!(
                "hermit log-diff: could not initialize JSON report at {}: {error}",
                path.display()
            );
            return ExitStatus::Exited(2);
        }

        if self.follow {
            return self.follow_two_runs(file_b, &options, record_envelope);
        }

        let comparison = if (self.canonical_info || self.json.is_some())
            && record_envelope.policy() == RecordEnvelopePolicy::AllRecordsV1
        {
            try_bitwise_info_v1_with_records(&self.file_a, file_b, &options)
                .map(|(summary, left, right)| (summary, left, right, None))
        } else {
            // The DBT transport envelope remains on its current named-filter
            // path. It is a distinct canonical envelope, not an all-record
            // BitwiseInfoV1 comparison.
            try_log_diff_with_records(&self.file_a, file_b, &options, record_envelope)
                .map(|(summary, left, right, inputs)| (summary, left, right, Some(inputs)))
        };
        let (summary, records_left, records_right, inputs) = match comparison {
            Ok(result) => result,
            Err(error) => {
                eprintln!(
                    "hermit log-diff: could not compare {} and {}: {error}",
                    self.file_a.display(),
                    file_b.display()
                );
                // Replace the pending `no_result` with an explicit refusal.
                // Leaving the pending report would tell a JSON consumer that
                // no verdict was reached, which reads as "try again" -- but a
                // refusal is deterministic and re-running changes nothing.
                if let Some(path) = &self.json {
                    let mut report = pending_json_report(&options, record_envelope.policy());
                    report.verdict = LogDiffVerdict::Refused;
                    report.refusal = Some(error.to_string());
                    if let Err(write_error) = write_json(path, &report) {
                        eprintln!(
                            "hermit log-diff: could not record the refusal in {}: {write_error}",
                            path.display()
                        );
                    }
                }
                return ExitStatus::Exited(2);
            }
        };
        let records = LogDiffRecords {
            compared: records_left.min(records_right),
            available_left: records_left,
            available_right: records_right,
            withheld_incomplete_tail: false,
        };
        eprintln!(
            "hermit log-diff: read {} records in full (left {} | right {}); selected {} | {} messages under {}",
            records.compared,
            records_left,
            records_right,
            summary.compared_left,
            summary.compared_right,
            record_envelope.policy().as_str(),
        );
        let mut report = json_report(&summary, &options, records, record_envelope.policy());
        report.inputs = inputs;
        if let Some(path) = &self.json
            && let Err(error) = write_json(path, &report)
        {
            eprintln!(
                "hermit log-diff: could not write JSON report to {}: {error}",
                path.display()
            );
            return ExitStatus::Exited(2);
        }
        if let Some(reason) = summary.refusal_reason.as_deref() {
            eprintln!("hermit log-diff: REFUSED: {reason}");
            ExitStatus::Exited(2)
        } else if summary.diff_found {
            ExitStatus::Exited(HERMIT_VERIFICATION_DIVERGENCE_EXIT)
        } else if summary.matched_with_evidence() {
            ExitStatus::Exited(0)
        } else {
            eprintln!("hermit log-diff: no comparable messages; refusing to report a match");
            ExitStatus::Exited(2)
        }
    }

    /// Compare two runs while they are still being written.
    ///
    /// Exit status is deliberately four-valued, because "they agree", "they
    /// agree as far as I read", and "I ran out of time before either finished"
    /// are three different answers and only one of them is good news:
    ///
    /// * 0 -- both logs stopped growing and never diverged.
    /// * 1 -- a divergence was found; the record index bounding it is reported.
    /// * 2 -- the logs could not be read, or held nothing comparable.
    /// * 3 -- the timeout expired while at least one run was still writing.
    fn follow_two_runs(
        &self,
        file_b: &Path,
        options: &logdiff::LogDiffOpts,
        record_envelope: RecordEnvelope,
    ) -> ExitStatus {
        let interval = Duration::from_millis(self.follow_interval_ms.max(1));
        let settle_polls = self.follow_settle_polls.max(1);
        let deadline = (self.follow_timeout_secs > 0)
            .then(|| Instant::now() + Duration::from_secs(self.follow_timeout_secs));

        let fixed_bitwise_info_v1 = (self.canonical_info || self.json.is_some())
            && record_envelope.policy() == RecordEnvelopePolicy::AllRecordsV1;
        let mut previous_sizes: Option<(usize, usize)> = None;
        let mut unchanged_polls = 0u32;
        let mut last_reported_records = usize::MAX;

        loop {
            let (left, right) = match (read_growing_log(&self.file_a), read_growing_log(file_b)) {
                (Ok(left), Ok(right)) => (left, right),
                (Err(error), _) | (_, Err(error)) => {
                    eprintln!("hermit log-diff: could not read logs while following: {error}");
                    return ExitStatus::Exited(2);
                }
            };

            // Buffer the comparison transcript: printing it on every poll would
            // bury the one line that matters.
            let mut transcript = Vec::new();
            let comparison = match compare_complete_prefix(
                &left,
                &right,
                options,
                &mut transcript,
                record_envelope,
                fixed_bitwise_info_v1,
            ) {
                Ok(comparison) => comparison,
                Err(error) => {
                    let reason = error.to_string();
                    eprintln!("hermit log-diff: comparison failed while following: {reason}");
                    if fixed_bitwise_info_v1 && let Some(path) = &self.json {
                        let mut report = pending_json_report(options, record_envelope.policy());
                        report.verdict = LogDiffVerdict::Refused;
                        report.refusal = Some(reason);
                        if let Err(write_error) = write_json(path, &report) {
                            eprintln!(
                                "hermit log-diff: could not record the refusal in {}: {write_error}",
                                path.display()
                            );
                        }
                    }
                    return ExitStatus::Exited(2);
                }
            };
            let records = LogDiffRecords {
                compared: comparison.records_compared,
                available_left: comparison.records_available_left,
                available_right: comparison.records_available_right,
                withheld_incomplete_tail: true,
            };

            if records.compared != last_reported_records {
                eprintln!(
                    "hermit log-diff: checked {} complete records (left has {} | right has {}); selected {} | {} messages under {}",
                    records.compared,
                    records.available_left,
                    records.available_right,
                    comparison.summary.compared_left,
                    comparison.summary.compared_right,
                    record_envelope.policy().as_str(),
                );
                last_reported_records = records.compared;
            }

            if let Some(reason) = comparison.summary.refusal_reason.as_deref() {
                std::io::stderr().write_all(&transcript).ok();
                eprintln!("hermit log-diff: REFUSED: {reason}");
                return self.finish_follow(
                    options,
                    &comparison.summary,
                    records,
                    LogDiffVerdict::Refused,
                    "refused",
                    ExitStatus::Exited(2),
                );
            } else if comparison.summary.diff_found {
                std::io::stderr().write_all(&transcript).ok();
                let first = comparison.summary.first_divergent_record;
                match first {
                    Some(index) => eprintln!(
                        "hermit log-diff: DIVERGED at record {index} of {} compared",
                        records.compared
                    ),
                    None => eprintln!(
                        "hermit log-diff: DIVERGED within the first {} records compared",
                        records.compared
                    ),
                }
                return self.finish_follow(
                    options,
                    &comparison.summary,
                    records,
                    LogDiffVerdict::Diverged,
                    "diverged",
                    ExitStatus::Exited(HERMIT_VERIFICATION_DIVERGENCE_EXIT),
                );
            }

            let sizes = (left.len(), right.len());
            if previous_sizes == Some(sizes) {
                unchanged_polls += 1;
            } else {
                unchanged_polls = 0;
            }
            previous_sizes = Some(sizes);

            if unchanged_polls >= settle_polls {
                if comparison.summary.matched_with_evidence() {
                    eprintln!(
                        "hermit log-diff: both logs unchanged for {settle_polls} polls; \
                         identical over {} | {} selected messages from {} complete records",
                        comparison.summary.compared_left,
                        comparison.summary.compared_right,
                        records.compared,
                    );
                    return self.finish_follow(
                        options,
                        &comparison.summary,
                        records,
                        LogDiffVerdict::IdenticalSoFar,
                        "quiescent",
                        ExitStatus::Exited(0),
                    );
                }
                eprintln!(
                    "hermit log-diff: both logs unchanged for {settle_polls} polls after reading {} \
                     complete records, but selected {} | {} comparable messages; refusing to report a match",
                    records.compared,
                    comparison.summary.compared_left,
                    comparison.summary.compared_right,
                );
                return self.finish_follow(
                    options,
                    &comparison.summary,
                    records,
                    LogDiffVerdict::NoComparableMessages,
                    "quiescent",
                    ExitStatus::Exited(2),
                );
            }

            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                eprintln!(
                    "hermit log-diff: timed out after {}s with {} complete records read and {} | {} \
                     messages compared while at least one run was still writing; NOT a match -- the rest was never read",
                    self.follow_timeout_secs,
                    records.compared,
                    comparison.summary.compared_left,
                    comparison.summary.compared_right,
                );
                return self.finish_follow(
                    options,
                    &comparison.summary,
                    records,
                    LogDiffVerdict::IdenticalSoFar,
                    "timeout",
                    ExitStatus::Exited(3),
                );
            }

            std::thread::sleep(interval);
        }
    }

    fn finish_follow(
        &self,
        options: &logdiff::LogDiffOpts,
        summary: &logdiff::LogDiffSummary,
        records: LogDiffRecords,
        verdict: LogDiffVerdict,
        stopped_because: &'static str,
        status: ExitStatus,
    ) -> ExitStatus {
        let Some(path) = &self.json else {
            return status;
        };
        let mut report = json_report(
            summary,
            options,
            records,
            self.record_envelope.envelope().policy(),
        );
        report.verdict = verdict;
        report.follow_stopped_because = Some(stopped_because.to_owned());
        if let Err(error) = write_json(path, &report) {
            eprintln!(
                "hermit log-diff: could not write JSON report to {}: {error}",
                path.display()
            );
            return ExitStatus::Exited(2);
        }
        status
    }
}

fn canonical_comparison_is_unrelaxed(options: &logdiff::LogDiffOpts) -> Result<(), &'static str> {
    let defaults = logdiff::LogDiffOpts::default();
    if options.strip_lines {
        return Err("canonical INFO comparison conflicts with --unsafe-strip-lines");
    }
    if !options.ignore_lines.is_empty() {
        return Err("canonical INFO comparison does not permit --ignore-lines");
    }
    if options.skip_commit || options.skip_detlog {
        return Err("canonical INFO comparison does not permit message skipping");
    }
    if options.git_diff {
        return Err("canonical INFO JSON does not support --git-diff");
    }
    if options.include_detlogs != defaults.include_detlogs {
        return Err("canonical INFO comparison does not permit DETLOG filtering");
    }
    Ok(())
}

fn json_report(
    summary: &logdiff::LogDiffSummary,
    options: &logdiff::LogDiffOpts,
    records: LogDiffRecords,
    record_envelope: RecordEnvelopePolicy,
) -> LogDiffReport {
    let verdict = if summary.refusal_reason.is_some() {
        LogDiffVerdict::Refused
    } else if summary.diff_found {
        LogDiffVerdict::Diverged
    } else if summary.compared_left == 0 && summary.compared_right == 0 {
        LogDiffVerdict::NoComparableMessages
    } else {
        LogDiffVerdict::Matched
    };
    let stream = match options.comparison {
        logdiff::LogComparisonMode::Deterministic => "deterministic",
        logdiff::LogComparisonMode::Info => "info",
        logdiff::LogComparisonMode::FullTrace => "full_trace",
    };
    let included_detlog_kinds = options
        .include_detlogs
        .iter()
        .map(|kind| match kind {
            logdiff::DetLogFilter::Syscall => "syscall".to_owned(),
            logdiff::DetLogFilter::SyscallResult => "syscall_result".to_owned(),
            logdiff::DetLogFilter::Other => "other".to_owned(),
        })
        .collect();
    LogDiffReport {
        schema: LOG_DIFF_REPORT_SCHEMA,
        verdict,
        refusal: summary.refusal_reason.clone(),
        selected_messages: LogDiffMessageCounts {
            left: summary.compared_left,
            right: summary.compared_right,
        },
        records,
        inputs: None,
        follow_stopped_because: None,
        first_divergent_record: summary.first_divergent_record,
        first_divergent_syscall: summary.first_divergent_syscall,
        comparison: LogDiffComparison {
            stream: stream.to_owned(),
            record_envelope,
            unsafe_strip_lines: options.strip_lines,
            canonicalize_host_addresses: options.canonicalize_addresses,
            require_structured_events: options.require_structured_events,
            ignored_line_substrings: options.ignore_lines.clone(),
            skip_commit: options.skip_commit,
            skip_detlog: options.skip_detlog,
            included_detlog_kinds,
            git_diff: options.git_diff,
        },
        first_divergent_scheduler_turn: summary.first_divergent_scheduler_turn,
        first_divergent_virtual_nanoseconds: summary.first_divergent_virtual_nanoseconds,
        first_divergent_left_message: summary.first_divergent_left_message.clone(),
        first_divergent_right_message: summary.first_divergent_right_message.clone(),
    }
}

fn pending_json_report(
    options: &logdiff::LogDiffOpts,
    record_envelope: RecordEnvelopePolicy,
) -> LogDiffReport {
    let summary = logdiff::LogDiffSummary {
        diff_found: false,
        compared_left: 0,
        compared_right: 0,
        first_divergent_scheduler_turn: None,
        first_divergent_virtual_nanoseconds: None,
        first_divergent_record: None,
        first_divergent_syscall: None,
        first_divergent_left_message: None,
        first_divergent_right_message: None,
        refusal_reason: None,
    };
    let mut report = json_report(&summary, options, no_records(), record_envelope);
    report.verdict = LogDiffVerdict::NoResult;
    report
}

fn no_records() -> LogDiffRecords {
    LogDiffRecords {
        compared: 0,
        available_left: 0,
        available_right: 0,
        withheld_incomplete_tail: false,
    }
}

/// Read a log that may not exist yet or may be mid-write. A run that has not
/// created its log is "nothing to compare", not an error.
fn read_growing_log(path: &Path) -> std::io::Result<Vec<u8>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

fn write_json(path: &Path, report: &LogDiffReport) -> std::io::Result<()> {
    let directory = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temporary = NamedTempFile::new_in(directory)?;
    serde_json::to_writer(&mut temporary, report).map_err(std::io::Error::other)?;
    temporary.write_all(b"\n")?;
    temporary.flush()?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use detcore::detlog::DetLogEvent;
    use detcore::detlog::record_suffix;

    use super::*;

    fn current_record(second: usize, body: &str) -> String {
        format!(
            "Apr 09 06:08:{second:02}.100  INFO detcore: {body}{}\n",
            detcore::detlog::record_suffix(detcore::detlog::DetLogEvent::Other)
        )
    }

    /// Append `count` records to `path`, one every `gap`, flushing each so a
    /// reader sees a genuinely growing file. Record `diverge_at` carries
    /// `marker`, so two writers with different markers part company there.
    fn spawn_writer(
        path: PathBuf,
        marker: &'static str,
        diverge_at: usize,
        count: usize,
        gap: Duration,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let mut file = std::fs::File::create(&path).unwrap();
            for index in 0..count {
                let body = if index == diverge_at { marker } else { "same" };
                file.write_all(
                    current_record(index % 60, &format!("DETLOG record {index} {body}")).as_bytes(),
                )
                .unwrap();
                file.flush().unwrap();
                std::thread::sleep(gap);
            }
        })
    }

    fn follow_options() -> logdiff::LogDiffOpts {
        logdiff::LogDiffOpts {
            comparison: logdiff::LogComparisonMode::Info,
            ..Default::default()
        }
    }

    fn follow_static(left_bytes: &[u8], right_bytes: &[u8]) -> (ExitStatus, serde_json::Value) {
        let directory = tempfile::tempdir().unwrap();
        let left = directory.path().join("left.log");
        let right = directory.path().join("right.log");
        let json = directory.path().join("follow.json");
        std::fs::write(&left, left_bytes).unwrap();
        std::fs::write(&right, right_bytes).unwrap();

        let mut options = LogDiffCLIOpts::new(&left, &right);
        options.follow = true;
        options.canonical_info = true;
        options.json = Some(json.clone());
        options.follow_interval_ms = 1;
        options.follow_timeout_secs = 1;
        options.follow_settle_polls = 1;
        let status =
            options.follow_two_runs(&right, &follow_options(), RecordEnvelope::all_records_v1());
        let report = serde_json::from_str(&std::fs::read_to_string(json).unwrap()).unwrap();
        (status, report)
    }

    fn assert_canonical_follow_refusal(left: &[u8], right: &[u8]) {
        let expected = logdiff::compare_complete_bitwise_info_v1_prefix(
            left,
            right,
            logdiff::ComparisonSideLabels::default(),
            logdiff::BitwiseInfoV1Diagnostics::default(),
            &mut Vec::new(),
        )
        .expect_err("fixture must make the fixed comparator refuse")
        .to_string();
        let (status, report) = follow_static(left, right);
        assert!(
            matches!(status, ExitStatus::Exited(2)),
            "invalid comparison evidence must refuse, got {status:?}"
        );
        assert_eq!(report["verdict"], "refused");
        assert_eq!(report["refusal"], expected);
    }

    #[test]
    fn canonical_follow_compares_only_the_complete_structured_prefix() {
        let common = current_record(1, "DETLOG common");
        let left = format!("{common}{}", current_record(2, "DETLOG unfinished-left"));
        let right = format!("{common}{}", current_record(2, "DETLOG unfinished-right"));
        let (status, report) = follow_static(left.as_bytes(), right.as_bytes());
        assert!(
            matches!(status, ExitStatus::Exited(0)),
            "the differing unfinished tails must be withheld, got {status:?}"
        );
        assert_eq!(report["verdict"], "identical_so_far");
        assert!(report["refusal"].is_null());
    }

    #[test]
    fn canonical_follow_refuses_prose_only_detlog_records() {
        let prose = b"Apr 09 06:08:01.100  INFO detcore: DETLOG prose\n\
Apr 09 06:08:02.100  INFO detcore: DETLOG unfinished\n";
        assert_canonical_follow_refusal(prose, prose);
    }

    #[test]
    fn canonical_follow_refuses_invalid_utf8() {
        let mut invalid = format!(
            "{}{}",
            current_record(1, "DETLOG bad-byte"),
            current_record(2, "DETLOG unfinished")
        )
        .into_bytes();
        let invalid_index = invalid
            .windows(3)
            .position(|window| window == b"bad")
            .expect("fixture contains its invalidated word");
        invalid[invalid_index] = 0xff;
        let valid = format!(
            "{}{}",
            current_record(1, "DETLOG bad-byte"),
            current_record(2, "DETLOG unfinished")
        );
        assert_canonical_follow_refusal(&invalid, valid.as_bytes());
    }

    #[test]
    fn canonical_follow_refuses_a_terminal_truncation_marker() {
        let complete = format!(
            "{}{}",
            current_record(1, "DETLOG common"),
            current_record(2, "DETLOG unfinished")
        );
        let truncated = format!("{complete}{}\n", logdiff::TRUNCATION_MARKER);
        assert_canonical_follow_refusal(truncated.as_bytes(), complete.as_bytes());
    }

    #[test]
    fn standalone_logdiff_refuses_transport_only_evidence_under_named_dbt_envelope() {
        let directory = tempfile::tempdir().unwrap();
        let transport = "1970-01-01T00:00:00.000000Z INFO reverie_dbt::evidence: protected evidence initialized";
        let left = directory.path().join("left.log");
        let right = directory.path().join("right.log");
        let envelope = RecordEnvelope::dbt_evidence_transport_v1();

        std::fs::write(&left, transport).unwrap();
        std::fs::write(&right, transport).unwrap();
        assert_eq!(
            write_canonical_info(&left, &mut Vec::new(), envelope).unwrap(),
            0
        );
        let (empty, records_left, records_right, _) =
            try_log_diff_with_records(&left, &right, &follow_options(), envelope).unwrap();
        assert!(!empty.matched_with_evidence());

        let report = serde_json::to_value(json_report(
            &empty,
            &follow_options(),
            LogDiffRecords {
                compared: records_left.min(records_right),
                available_left: records_left,
                available_right: records_right,
                withheld_incomplete_tail: false,
            },
            envelope.policy(),
        ))
        .unwrap();
        assert_eq!(report["verdict"], "no_comparable_messages");
        assert_eq!(report["selected_messages"]["left"], 0);
        assert_eq!(report["selected_messages"]["right"], 0);
        assert_eq!(report["records"]["available_left"], 1);
        assert_eq!(report["records"]["available_right"], 1);
        assert_eq!(
            report["comparison"]["record_envelope"],
            "dbt_evidence_transport_v1"
        );
    }

    #[test]
    fn standalone_default_does_not_silently_apply_the_dbt_envelope() {
        let directory = tempfile::tempdir().unwrap();
        let transport = "1970-01-01T00:00:00.000000Z INFO reverie_dbt::evidence: protected evidence initialized";
        let left = directory.path().join("left.log");
        let right = directory.path().join("right.log");
        let envelope = RecordEnvelope::all_records_v1();

        std::fs::write(&left, transport).unwrap();
        std::fs::write(&right, transport).unwrap();
        let (summary, _, _, _) =
            try_log_diff_with_records(&left, &right, &follow_options(), envelope).unwrap();
        assert_eq!((summary.compared_left, summary.compared_right), (1, 1));
        assert!(summary.matched_with_evidence());
    }

    #[test]
    fn standalone_logdiff_admits_one_real_record() {
        let directory = tempfile::tempdir().unwrap();
        let real = "2026-08-21T10:00:00.000000Z INFO detcore: DETLOG [syscall] getpid() = Ok(3)";
        let left = directory.path().join("left.log");
        let right = directory.path().join("right.log");
        let envelope = RecordEnvelope::dbt_evidence_transport_v1();

        std::fs::write(&left, real).unwrap();
        std::fs::write(&right, real).unwrap();
        assert_eq!(
            write_canonical_info(&left, &mut Vec::new(), envelope).unwrap(),
            1
        );
        let (one, _, _, _) =
            try_log_diff_with_records(&left, &right, &follow_options(), envelope).unwrap();
        assert_eq!((one.compared_left, one.compared_right), (1, 1));
        assert!(one.matched_with_evidence());

        let report = serde_json::to_value(json_report(
            &one,
            &follow_options(),
            no_records(),
            envelope.policy(),
        ))
        .unwrap();
        assert_eq!(
            report["comparison"]["record_envelope"],
            "dbt_evidence_transport_v1"
        );
    }

    #[test]
    fn standalone_logdiff_drops_transport_beside_one_real_record() {
        let directory = tempfile::tempdir().unwrap();
        let transport = "1970-01-01T00:00:00.000000Z INFO reverie_dbt::evidence: protected evidence initialized";
        let real = "2026-08-21T10:00:00.000000Z INFO detcore: DETLOG [syscall] getpid() = Ok(3)";
        let left = directory.path().join("left.log");
        let right = directory.path().join("right.log");
        let envelope = RecordEnvelope::dbt_evidence_transport_v1();

        std::fs::write(&left, format!("{transport}\n{real}")).unwrap();
        std::fs::write(&right, format!("{transport}\n{real}")).unwrap();
        assert_eq!(
            write_canonical_info(&left, &mut Vec::new(), envelope).unwrap(),
            1
        );
        let (one, _, _, _) =
            try_log_diff_with_records(&left, &right, &follow_options(), envelope).unwrap();
        assert_eq!((one.compared_left, one.compared_right), (1, 1));
        assert!(one.matched_with_evidence());
    }

    #[test]
    fn standalone_logdiff_ignores_difference_confined_to_transport() {
        let directory = tempfile::tempdir().unwrap();
        let transport = "1970-01-01T00:00:00.000000Z INFO reverie_dbt::evidence: protected evidence initialized";
        let real = "2026-08-21T10:00:00.000000Z INFO detcore: DETLOG [syscall] getpid() = Ok(3)";
        let left = directory.path().join("left.log");
        let right = directory.path().join("right.log");
        let envelope = RecordEnvelope::dbt_evidence_transport_v1();

        std::fs::write(&left, format!("{transport}\n{real}")).unwrap();
        std::fs::write(&right, real).unwrap();
        let (summary, _, _, _) =
            try_log_diff_with_records(&left, &right, &follow_options(), envelope).unwrap();
        assert!(!summary.diff_found);
        assert_eq!((summary.compared_left, summary.compared_right), (1, 1));
    }

    #[test]
    fn follow_missing_logs_reports_no_comparable_messages() {
        let directory = tempfile::tempdir().unwrap();
        let left = directory.path().join("left.log");
        let right = directory.path().join("right.log");
        let json = directory.path().join("follow.json");
        let mut options = LogDiffCLIOpts::new(&left, &right);
        options.follow = true;
        options.follow_interval_ms = 1;
        options.follow_settle_polls = 1;
        options.json = Some(json.clone());

        let status = options.main(&GlobalOpts::try_parse_from(["hermit"]).unwrap());
        assert!(matches!(status, ExitStatus::Exited(2)));
        let report: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(json).unwrap()).unwrap();
        assert_eq!(report["verdict"], "no_comparable_messages");
        assert_eq!(report["follow_stopped_because"], "quiescent");
        assert_eq!(report["records"]["compared"], 0);
    }

    #[test]
    fn bounded_log_refusal_is_json_refused_and_exits_two() {
        let directory = tempfile::tempdir().unwrap();
        let left = directory.path().join("left.log");
        let right = directory.path().join("right.log");
        let json = directory.path().join("comparison.json");
        let truncated = format!("{}\n", logdiff::TRUNCATION_MARKER);
        std::fs::write(&left, &truncated).unwrap();
        std::fs::write(&right, &truncated).unwrap();

        let mut options = LogDiffCLIOpts::new(&left, &right);
        options.json = Some(json.clone());
        let global = GlobalOpts::try_parse_from(["hermit"]).unwrap();
        let status = options.main(&global);
        assert!(
            matches!(status, ExitStatus::Exited(2)),
            "a bounded-log refusal must exit 2, got {status:?}"
        );

        let report: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap();
        assert_eq!(report["verdict"], "refused");
        let refusal = report["refusal"].as_str().unwrap();
        assert!(refusal.contains("both logs were truncated"), "{refusal}");
        assert!(
            refusal.contains("HERMIT_LOG_MAX_BYTES"),
            "the refusal must retain its actionable remedy: {refusal}"
        );
        assert_eq!(report["selected_messages"]["left"], 0);
        assert_eq!(report["selected_messages"]["right"], 0);
    }

    #[test]
    fn cross_backend_envelope_matches_and_detects_a_detcore_divergence() {
        let directory = tempfile::tempdir().unwrap();
        let reference = directory.path().join("ptrace.log");
        let candidate = directory.path().join("kvm.log");
        let record = |time| {
            format!(
                "2026-09-03T10:00:00.000000Z INFO detcore::scheduler: COMMIT turn 7 at time {time}{}\n",
                record_suffix(DetLogEvent::SchedulerCommit {
                    scheduler_turn: 7,
                    virtual_nanoseconds: time,
                    internal_io_poll: false,
                    runtime_maps_read: false,
                })
            )
        };
        let ptrace_transport = "2026-09-03T10:00:00.000001Z INFO reverie_ptrace: backend ready\n";
        let kvm_transport = "2026-09-03T10:00:00.000001Z INFO reverie_kvm: backend ready\n";
        let json = directory.path().join("comparison.json");
        let mut options = LogDiffCLIOpts::new(&reference, &candidate);
        options.json = Some(json.clone());
        options.record_envelope = RecordEnvelopeArg::CrossBackendDetcoreV1;
        let global = GlobalOpts::try_parse_from(["hermit"]).unwrap();
        let read_report =
            || -> LogDiffReport { serde_json::from_slice(&std::fs::read(&json).unwrap()).unwrap() };
        // The shared Hermit target is outside this envelope too, even though
        // it is not launcher or transport text. Its different payload is
        // intentionally excluded; this test makes that boundary observable.
        let shared_reference =
            "2026-09-03T10:00:00.000002Z INFO hermit::verify: reference observation\n";
        let shared_candidate =
            "2026-09-03T10:00:00.000002Z INFO hermit::verify: candidate observation\n";
        let ptrace_bytes =
            format!("{}{ptrace_transport}{shared_reference}", record(17)).into_bytes();
        let kvm_bytes = format!("{}{kvm_transport}{shared_candidate}", record(17)).into_bytes();
        std::fs::write(&reference, &ptrace_bytes).unwrap();
        std::fs::write(&candidate, &kvm_bytes).unwrap();
        assert!(matches!(options.main(&global), ExitStatus::Exited(0)));
        let matched_report = read_report();
        matched_report.require_cross_backend_evidence().unwrap();
        assert_eq!(matched_report.verdict, LogDiffVerdict::Matched);
        assert_eq!(matched_report.selected_messages.left, 1);
        assert_eq!(matched_report.selected_messages.right, 1);
        assert_eq!(matched_report.records.available_left, 3);
        assert_eq!(matched_report.records.available_right, 3);
        let inputs = matched_report.inputs.as_ref().unwrap();
        assert_eq!(
            inputs.left.sha256,
            detcore::Digest::new(&ptrace_bytes).to_string()
        );
        assert_eq!(
            inputs.right.sha256,
            detcore::Digest::new(&kvm_bytes).to_string()
        );
        assert_eq!(inputs.left.bytes, ptrace_bytes.len() as u64);
        assert_eq!(inputs.right.bytes, kvm_bytes.len() as u64);

        // Change only a shared Detcore value. Transport text remains different
        // in both controls; the named envelope still preserves every Detcore value.
        std::fs::write(
            &candidate,
            format!("{}{kvm_transport}{shared_candidate}", record(18)),
        )
        .unwrap();
        assert!(matches!(
            options.main(&global),
            ExitStatus::Exited(HERMIT_VERIFICATION_DIVERGENCE_EXIT)
        ));
        let diverged_report = read_report();
        diverged_report.require_cross_backend_evidence().unwrap();
        assert_eq!(diverged_report.verdict, LogDiffVerdict::Diverged);
        assert_eq!(diverged_report.first_divergent_scheduler_turn, Some(7));
        assert_eq!(
            diverged_report.first_divergent_virtual_nanoseconds,
            Some(17)
        );
        assert_ne!(
            inputs.right.sha256,
            diverged_report.inputs.unwrap().right.sha256
        );

        // These invalid payload bytes used to collapse to the same replacement
        // character before comparison. Exercise the production CLI and writer
        // in both directions, including two distinct malformed byte streams.
        let invalid = |byte| {
            let mut bytes = record(17).into_bytes();
            let position = bytes.iter().position(|value| *value == b'C').unwrap();
            bytes.insert(position, byte);
            bytes
        };
        for (left, right) in [
            (invalid(0x80), kvm_bytes.clone()),
            (ptrace_bytes.clone(), invalid(0x81)),
            (invalid(0x80), invalid(0x81)),
        ] {
            std::fs::write(&reference, &left).unwrap();
            std::fs::write(&candidate, &right).unwrap();
            assert!(matches!(options.main(&global), ExitStatus::Exited(2)));
            let refused = read_report();
            assert_eq!(refused.verdict, LogDiffVerdict::Refused);
            assert!(refused.refusal.unwrap().contains("not UTF-8"));
            for (path, bytes) in [(&reference, &left), (&candidate, &right)] {
                if std::str::from_utf8(bytes).is_err() {
                    let error = write_canonical_info(
                        path,
                        &mut Vec::new(),
                        RecordEnvelope::cross_backend_detcore_v1(),
                    )
                    .unwrap_err();
                    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
                }
            }
            let error = compare_complete_prefix(
                &left,
                &right,
                &follow_options(),
                &mut Vec::new(),
                RecordEnvelope::cross_backend_detcore_v1(),
                false,
            )
            .unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn follow_reports_divergence_before_either_run_finishes() {
        let directory = tempfile::tempdir().unwrap();
        let left = directory.path().join("left.log");
        let right = directory.path().join("right.log");
        let json = directory.path().join("follow.json");

        // 200 records each, diverging at record 5. If follow mode waited for
        // completion this would take ~4s; it should answer in a fraction of it.
        let gap = Duration::from_millis(20);
        let writers = [
            spawn_writer(left.clone(), "LEFT", 5, 200, gap),
            spawn_writer(right.clone(), "RIGHT", 5, 200, gap),
        ];

        let mut options = LogDiffCLIOpts::new(&left, &right);
        options.follow = true;
        options.follow_interval_ms = 10;
        options.follow_timeout_secs = 30;
        options.json = Some(json.clone());

        let started = Instant::now();
        let status =
            options.follow_two_runs(&right, &follow_options(), RecordEnvelope::all_records_v1());
        let elapsed = started.elapsed();

        assert!(
            matches!(
                status,
                ExitStatus::Exited(HERMIT_VERIFICATION_DIVERGENCE_EXIT)
            ),
            "divergence must exit 1, got {status:?}"
        );

        let report: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap();
        assert_eq!(report["verdict"], "diverged");
        assert_eq!(report["follow_stopped_because"], "diverged");
        assert!(
            report["records"]["withheld_incomplete_tail"]
                .as_bool()
                .unwrap()
        );

        // The divergence is located, not merely bounded. Records are 1-based
        // and the writers differ at 0-based index 5, i.e. the 6th record.
        assert_eq!(
            report["first_divergent_record"], 6,
            "the report must name the record that differs"
        );

        // The whole point: it answered without reading either run to the end.
        let compared = report["records"]["compared"].as_u64().unwrap();
        assert!(
            (6..200).contains(&compared),
            "must bound the divergence after it is written and long before the \
             runs end, compared={compared}"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "must not wait for completion, took {elapsed:?}"
        );

        for writer in writers {
            writer.join().unwrap();
        }
    }

    #[test]
    fn follow_timing_out_is_not_reported_as_agreement() {
        let directory = tempfile::tempdir().unwrap();
        let left = directory.path().join("left.log");
        let right = directory.path().join("right.log");
        let json = directory.path().join("follow.json");

        // Identical and still being written when the timeout expires.
        let gap = Duration::from_millis(30);
        let writers = [
            spawn_writer(left.clone(), "same", usize::MAX, 200, gap),
            spawn_writer(right.clone(), "same", usize::MAX, 200, gap),
        ];

        let mut options = LogDiffCLIOpts::new(&left, &right);
        options.follow = true;
        options.follow_interval_ms = 100;
        options.follow_timeout_secs = 1;
        options.json = Some(json.clone());

        let status =
            options.follow_two_runs(&right, &follow_options(), RecordEnvelope::all_records_v1());
        assert!(
            matches!(status, ExitStatus::Exited(3)),
            "an unfinished follow must not share an exit code with success, got {status:?}"
        );

        let report: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap();
        assert_eq!(
            report["verdict"], "identical_so_far",
            "agreement over a prefix is never `matched`"
        );
        assert_eq!(report["follow_stopped_because"], "timeout");
        assert!(report["records"]["compared"].as_u64().unwrap() > 0);

        for writer in writers {
            writer.join().unwrap();
        }
    }

    #[test]
    fn every_report_carries_the_record_count_it_was_based_on() {
        let options = logdiff::LogDiffOpts::default();
        let summary = logdiff::LogDiffSummary {
            diff_found: false,
            compared_left: 4,
            compared_right: 4,
            first_divergent_scheduler_turn: None,
            first_divergent_virtual_nanoseconds: None,
            first_divergent_record: None,
            first_divergent_syscall: None,
            first_divergent_left_message: None,
            first_divergent_right_message: None,
            refusal_reason: None,
        };
        let records = LogDiffRecords {
            compared: 40,
            available_left: 41,
            available_right: 40,
            withheld_incomplete_tail: true,
        };
        let value = serde_json::to_value(json_report(
            &summary,
            &options,
            records,
            RecordEnvelopePolicy::AllRecordsV1,
        ))
        .unwrap();
        assert_eq!(value["records"]["compared"], 40);
        assert_eq!(value["records"]["available_left"], 41);
        assert_eq!(value["records"]["available_right"], 40);
        assert_eq!(value["comparison"]["record_envelope"], "all_records_v1");

        // Even the placeholder written before any comparison names its zero.
        let pending = serde_json::to_value(pending_json_report(
            &options,
            RecordEnvelopePolicy::AllRecordsV1,
        ))
        .unwrap();
        assert_eq!(pending["verdict"], "no_result");
        assert_eq!(pending["records"]["compared"], 0);
        assert_eq!(pending["comparison"]["record_envelope"], "all_records_v1");
    }

    #[test]
    fn one_log_and_two_log_json_forms_parse() {
        let one = LogDiffCLIOpts::try_parse_from(["log-diff", "run.log"]).unwrap();
        assert_eq!(one.file_b, None);
        assert_eq!(one.record_envelope, RecordEnvelopeArg::AllRecordsV1);

        let two = LogDiffCLIOpts::try_parse_from([
            "log-diff",
            "left.log",
            "right.log",
            "--record-envelope",
            "dbt-evidence-transport-v1",
            "--print-logs",
            "--json",
            "diff.json",
        ])
        .unwrap();
        assert_eq!(two.file_b, Some(PathBuf::from("right.log")));
        assert!(two.more.print_logs);
        assert_eq!(two.json, Some(PathBuf::from("diff.json")));
        assert_eq!(
            two.record_envelope,
            RecordEnvelopeArg::DbtEvidenceTransportV1
        );
    }

    #[test]
    fn json_report_distinguishes_divergence_match_and_no_messages() {
        let options = logdiff::LogDiffOpts::default();
        let summary = logdiff::LogDiffSummary {
            diff_found: true,
            compared_left: 8,
            compared_right: 8,
            first_divergent_scheduler_turn: Some(17),
            first_divergent_virtual_nanoseconds: Some(123),
            first_divergent_record: Some(9),
            // A different keyspace from the record index above, deliberately:
            // nine compared records in, only three syscalls completed.
            first_divergent_syscall: Some(3),
            first_divergent_left_message: Some("INFO detcore: left".into()),
            first_divergent_right_message: Some("INFO detcore: right".into()),
            refusal_reason: None,
        };
        let value = serde_json::to_value(json_report(
            &summary,
            &options,
            no_records(),
            RecordEnvelopePolicy::AllRecordsV1,
        ))
        .unwrap();
        assert_eq!(value["verdict"], "diverged");
        assert_eq!(value["selected_messages"]["left"], 8);
        assert_eq!(value["comparison"]["stream"], "deterministic");
        assert_eq!(value["first_divergent_scheduler_turn"], 17);
        assert_eq!(value["first_divergent_virtual_nanoseconds"], 123);
        assert_eq!(value["first_divergent_left_message"], "INFO detcore: left");
        assert_eq!(
            value["first_divergent_right_message"],
            "INFO detcore: right"
        );

        let matched = logdiff::LogDiffSummary {
            diff_found: false,
            first_divergent_scheduler_turn: None,
            first_divergent_virtual_nanoseconds: None,
            ..summary
        };
        assert_eq!(
            serde_json::to_value(json_report(
                &matched,
                &options,
                no_records(),
                RecordEnvelopePolicy::AllRecordsV1,
            ))
            .unwrap()["verdict"],
            "matched"
        );

        let empty = logdiff::LogDiffSummary {
            compared_left: 0,
            compared_right: 0,
            ..matched
        };
        assert_eq!(
            serde_json::to_value(json_report(
                &empty,
                &options,
                no_records(),
                RecordEnvelopePolicy::AllRecordsV1,
            ))
            .unwrap()["verdict"],
            "no_comparable_messages"
        );

        let refused = logdiff::LogDiffSummary {
            diff_found: true,
            refusal_reason: Some("the first log was truncated".into()),
            ..empty
        };
        let refused = serde_json::to_value(json_report(
            &refused,
            &options,
            no_records(),
            RecordEnvelopePolicy::AllRecordsV1,
        ))
        .unwrap();
        assert_eq!(refused["verdict"], "refused");
        assert_eq!(refused["refusal"], "the first log was truncated");
    }

    #[test]
    fn canonical_json_refuses_relaxed_comparisons_and_starts_as_no_result() {
        let options = logdiff::LogDiffOpts::default();
        assert!(canonical_comparison_is_unrelaxed(&options).is_ok());

        let mut stripped = options.clone();
        stripped.strip_lines = true;
        assert!(canonical_comparison_is_unrelaxed(&stripped).is_err());

        let mut ignored = options.clone();
        ignored.ignore_lines.push("difference".to_owned());
        assert!(canonical_comparison_is_unrelaxed(&ignored).is_err());

        let report = pending_json_report(&options, RecordEnvelopePolicy::AllRecordsV1);
        assert_eq!(
            serde_json::to_value(report).unwrap()["verdict"],
            "no_result"
        );
    }

    #[test]
    fn canonical_all_records_uses_the_fixed_shared_comparator() {
        let directory = tempfile::tempdir().unwrap();
        let left = directory.path().join("left.log");
        let right = directory.path().join("right.log");
        let suffix = detcore::detlog::record_suffix(detcore::detlog::DetLogEvent::Other);
        std::fs::write(
            &left,
            format!(
                "Apr 09 06:08:01.100  INFO detcore: DETLOG address={}{}\n",
                logdiff::host_addr(0x1000),
                suffix
            ),
        )
        .unwrap();
        std::fs::write(
            &right,
            format!(
                "Apr 09 06:08:01.100  INFO detcore: DETLOG address={}{}\n",
                logdiff::host_addr(0x9000),
                suffix
            ),
        )
        .unwrap();
        let options = logdiff::LogDiffOpts {
            comparison: logdiff::LogComparisonMode::Info,
            canonicalize_addresses: true,
            ..Default::default()
        };
        let (summary, left_records, right_records) =
            try_bitwise_info_v1_with_records(&left, &right, &options).unwrap();
        assert!(summary.matched_with_evidence());
        assert_eq!((left_records, right_records), (1, 1));
    }

    #[test]
    fn json_report_is_one_line_and_replaces_the_previous_record() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("comparison.json");
        let options = logdiff::LogDiffOpts::default();

        write_json(
            &path,
            &pending_json_report(&options, RecordEnvelopePolicy::AllRecordsV1),
        )
        .unwrap();
        let pending = std::fs::read_to_string(&path).unwrap();
        assert_eq!(pending.lines().count(), 1);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&pending).unwrap()["verdict"],
            "no_result"
        );

        let summary = logdiff::LogDiffSummary {
            diff_found: false,
            compared_left: 3,
            compared_right: 3,
            first_divergent_scheduler_turn: None,
            first_divergent_virtual_nanoseconds: None,
            first_divergent_record: None,
            first_divergent_syscall: None,
            first_divergent_left_message: None,
            first_divergent_right_message: None,
            refusal_reason: None,
        };
        write_json(
            &path,
            &json_report(
                &summary,
                &options,
                no_records(),
                RecordEnvelopePolicy::AllRecordsV1,
            ),
        )
        .unwrap();
        let terminal = std::fs::read_to_string(&path).unwrap();
        assert_eq!(terminal.lines().count(), 1);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&terminal).unwrap()["verdict"],
            "matched"
        );
    }
}
