/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeSet;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;

use colored::Colorize;
use detcore::logdiff;
use detcore::logdiff::ComparisonSideLabels;
use detcore::logdiff::LogComparisonMode;
use detcore_model::summary::RunSummary;
use hermit::Context;
use hermit::Error;
use hermit::HERMIT_VERIFICATION_DIVERGENCE_EXIT;
use hermit::canonical_verdict::BranchCounterVerdict;
use hermit::canonical_verdict::ComparedLogMessages;
use hermit::canonical_verdict::ComparedLogScope;
use hermit::canonical_verdict::ComparedOutput;
use hermit::canonical_verdict::ComparedOutputs;
use hermit::canonical_verdict::ComparisonReport;
use hermit::canonical_verdict::ContainerDisposition;
use hermit::canonical_verdict::ContainerFailure;
pub(crate) use hermit::canonical_verdict::DbtCountedBranchComparison;
use hermit::canonical_verdict::InfrastructureError;
pub(crate) use hermit::canonical_verdict::LogCompareStrictness;
pub(crate) use hermit::canonical_verdict::NoResultReason;
use hermit::canonical_verdict::RecordEnvelopeReport;
use hermit::canonical_verdict::RunFailure;
use hermit::canonical_verdict::RuntimeStats;
pub(crate) use hermit::canonical_verdict::Verdict;
pub(crate) use hermit::canonical_verdict::VerificationReport;
pub(crate) use hermit::canonical_verdict::VerificationRun;
pub(crate) use hermit::canonical_verdict::VerificationRuntime;
use hermit::nonwaiting_write::write_without_waiting;
use pretty_assertions::Comparison;
use reverie::process::ExitStatus;
use reverie::process::Output;
use tempfile::NamedTempFile;
use tempfile::TempPath;
use tracing::metadata::LevelFilter;

use super::container::LogCapExceeded;
use super::global_opts::GlobalOpts;
use super::record_envelope::RecordEnvelope;
use super::record_envelope::RecordEnvelopePolicy;

const FAILED_VERIFY_LOG_DIR_PREFIX: &str = "comparison-";
const FAILED_VERIFY_LOG_PENDING_PREFIX: &str = ".pending-comparison-";
const FAILED_VERIFY_LOG_RETIRING_PREFIX: &str = ".retiring-comparison-";
const FAILED_VERIFY_LOG_LOCK: &str = ".retention.lock";
const FAILED_VERIFY_LOG_ROOT_LOCK: &str = ".retirement.lock";
// Cell-aware validation stores its logs in per-attempt artifact directories.
// This population exists for recent ad-hoc failures whose caller supplied no
// durable destination and, consequently, no cell identity. Sixty-four complete
// comparisons preserve a useful recent debugging window without pretending a
// per-cell policy can be recovered from identity-free files.
const FAILED_VERIFY_LOG_COMPARISONS_TO_KEEP: usize = 64;

#[derive(Clone, Debug)]
pub(crate) struct FailedVerifyLogRetention {
    root: PathBuf,
    keep: usize,
}

impl FailedVerifyLogRetention {
    #[cfg(test)]
    fn new(root: PathBuf, keep: usize) -> Self {
        Self { root, keep }
    }
}

pub(crate) fn default_failed_verify_log_retention() -> FailedVerifyLogRetention {
    let root = dirs::state_dir()
        .map(|path| path.join("hermit").join("verify-failures"))
        .unwrap_or_else(|| std::env::temp_dir().join("hermit-verify-failures"));
    FailedVerifyLogRetention {
        root,
        keep: FAILED_VERIFY_LOG_COMPARISONS_TO_KEEP,
    }
}

pub(crate) struct ComparedRun<'a> {
    pub output: &'a Output,
    pub log: TempPath,
    /// Reader-facing name for this side of the comparison. Run verification
    /// compares two fresh executions; record verification compares one
    /// recording with its replay.
    pub label: &'a str,
}

pub(crate) struct ComparisonOptions {
    /// Controls only how much diff *output* is printed (a larger syscall-history
    /// window), NOT the comparison semantics. Comparison strictness is carried
    /// separately in [`Self::strictness`] so a quiet run can still be
    /// bitwise-strict — the two knobs were historically conflated behind a single
    /// `verbose` flag, which made the only bitwise comparison also the loudest.
    pub verbose: bool,
    /// How strictly the internal event stream is compared. This is the
    /// condition the verdict rests on, and is recorded verbatim in the resulting
    /// [`VerificationOutcome`] so a consumer can tell a stripped match from a
    /// bitwise one.
    pub strictness: LogCompareStrictness,
    pub compare_logs: bool,
    /// Compare DEBUG/TRACE diagnostics in addition to the canonical INFO
    /// envelope. This is reserved for the explicit `--verify-verbose` diagnostic
    /// mode; an ordinary `--verify-strict` verdict must not depend on diagnostic
    /// events merely because the caller requested that they be captured.
    pub diagnostic_full_trace: bool,
    /// Whether the compared records contain the CONTENT of syscall output
    /// buffers, i.e. whether the run hashed them. That is ON BY DEFAULT; it is
    /// false only when the caller passed `--no-detlog-io-buffers`.
    ///
    /// This is not a comparator setting; it describes what the records being
    /// compared can possibly show. See [`ComparisonSpec::compare_io_buffers`].
    pub compare_io_buffers: bool,
    /// Keep captured logs at their selected paths after comparison.
    ///
    /// After a match, only the first side's log is kept, as the golden log,
    /// and the second side's log, which compared equal to it under the
    /// selected policy, is deleted. After a divergence, a
    /// no-result, an error, or a match that did not compare the logs, both
    /// logs are kept, because they are the evidence of what differed or of
    /// what was never checked.
    pub keep_logs: bool,
    /// The caller has already observed a result that replaces a comparator
    /// match: a skid overshoot, which is an infrastructure error, or a DBT
    /// counted-branch or statistics divergence. A match is then not the
    /// terminal verdict, so the logs are retained as for a divergence, and
    /// [`Self::keep_logs`] does not delete the second side's log.
    pub match_overridden: bool,
    /// Where an implicitly retained failed comparison is stored and bounded.
    /// This is absent when the caller explicitly requested `--keep-logs`; an
    /// explicit evidence directory remains entirely caller-owned.
    pub failed_log_retention: Option<FailedVerifyLogRetention>,
    /// The logs belong to the caller (two `hermit run --verify-run`
    /// directories): the comparison neither moves, deletes nor announces
    /// them, whatever it finds, and [`Self::keep_logs`] and
    /// [`Self::failed_log_retention`] do not apply. The caller hands the
    /// comparison links to them, so a log the comparison drops on an early
    /// error is only the link.
    pub caller_owns_logs: bool,
    /// Typed, versioned record envelope applied before selecting messages.
    /// Its policy identity is serialized beside the verdict.
    pub record_envelope: RecordEnvelope,
    /// Whether the Detcore configuration under which BOTH sides ran virtualized
    /// time. See [`ComparisonSpec::virtualize_time`] for what it changes about
    /// the meaning of a match.
    pub virtualize_time: bool,
}

/// Compatibility exports for callers that used the verification module before
/// the canonical policy moved beside the shared comparator.
pub use detcore::logdiff::CANON_ADDRESS_ORDINAL_V1;
pub use detcore::logdiff::STRIP_WALL_CLOCK_PREFIX_V1;

/// Versioned policy token marking the lossy wholesale normalization the
/// [`LogCompareStrictness::Stripped`] mode applies (numbers, addresses, tmp
/// paths, and timestamps erased). Its presence in a spec is disqualifying for
/// parity: it is recorded so a consumer can see WHY a stripped spec is not
/// parity rather than having to infer it.
pub const STRIP_UNSAFE_NORMALIZATION_V1: &str = "unsafe-numeric-address-and-path-normalization/v1";

/// The exact set of stripped-prefix tokens the parity ([`Canonical`]) policy
/// permits: the wall-clock prefix, and nothing else.
///
/// [`Canonical`]: LogCompareStrictness::Canonical
const PARITY_STRIPPED_PREFIXES: &[&str] = &[STRIP_WALL_CLOCK_PREFIX_V1];

/// The exact set of canonicalization tokens the parity ([`Canonical`]) policy
/// requires: address-to-ordinal, and nothing else.
///
/// [`Canonical`]: LogCompareStrictness::Canonical
const PARITY_CANONICALIZATIONS: &[&str] = &[CANON_ADDRESS_ORDINAL_V1];

/// The exact comparison that produced a [`Verdict`], carried beside it so a bare
/// "verified" can always say *which* comparison certified it.
///
/// The high-level [`Self::strictness`] and the concrete flags it expands to are
/// both recorded: a JSON consumer keying a bitwise-parity ratchet on the verdict
/// can require `strip_lines == false`, `full_trace == true`, and an INFO-or-
/// stronger [`Self::log_scope`] directly, rather than having to know how a
/// strictness label maps onto the diff engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComparisonSpec {
    /// The strictness label the comparison ran under.
    pub strictness: LogCompareStrictness,
    /// Human-readable name for the exact comparison policy. Derived in the same
    /// exhaustive match as the diff flags so evidence cannot name a different
    /// policy from the one that ran.
    pub display_name: &'static str,
    /// Whether the internal event stream was compared at all. When
    /// `false` only stdout/stderr/exit status were compared and the strictness
    /// fields describe a log comparison that did not run — a consumer must not
    /// read such a verdict as bitwise log parity.
    pub compare_logs: bool,
    /// Whether the compared records contain the CONTENT of syscall output
    /// buffers, not merely the syscalls' return values.
    ///
    /// ⚠️ WHEN THIS IS `false`, A MATCH IS NOT A CONTENT-PARITY RESULT, and a
    /// consumer must not read it as one. Reverie types many output buffers as
    /// bare pointers, so the record prints the ADDRESS and not the bytes --
    /// `reverie-syscalls/src/syscalls.rs` carries standing TODOs saying exactly
    /// that for `Read` and `Write`. Two runs whose buffers differ while their
    /// return values agree therefore produce character-identical records and
    /// compare equal.
    ///
    /// That is not hypothetical. A `recvmsg` of a netlink RTM_GETLINK dump
    /// returns a stable `Ok(1468)` on every run while four bytes of its payload
    /// vary (host kernel jiffies and a kernel-randomised timer). Measured: the
    /// same guest reports `verdict: matched, bitwise_parity: true` with this
    /// `false`, and `verdict: diverged` with it `true`. On one QEMU/Linux boot,
    /// 44.1% of syscalls (278,824 of 632,228) move bytes through a buffer whose
    /// content the record does not show.
    ///
    /// True by default, and set false only by `--no-detlog-io-buffers`. It is
    /// recorded here rather than inferred so a consumer can require it instead
    /// of assuming it.
    pub compare_io_buffers: bool,
    /// The message envelope selected from the captured log. The compared-message
    /// counts refer exactly to this scope.
    pub log_scope: ComparedLogScope,
    /// Versioned policy selecting which parsed records entered the message
    /// envelope. A caller-defined predicate is disclosed but cannot qualify for
    /// parity.
    pub record_envelope: RecordEnvelopePolicy,
    /// Whether the Detcore configuration under which BOTH sides ran virtualized
    /// time.
    ///
    /// ⚠️ A `bitwise_parity: true` FROM A COMPARISON WITH `virtualize_time:
    /// false` IS NOT A DETERMINISM RESULT, and a consumer must not read it as
    /// one. It says the second side reproduced the first. If the guest read a
    /// host-derived value such as the real monotonic clock, that value was
    /// captured and faithfully reproduced, and a separate invocation would
    /// capture a different one.
    ///
    /// This is not hypothetical. `hermit run --verify` executes the guest twice
    /// with virtual time on, so a match is evidence about the guest; `hermit
    /// record start --verify` runs under
    /// `hermit_cli::metadata::record_or_replay_config`, which deliberately sets
    /// `virtualize_time: false`, so a match says only that the replay reproduced
    /// that recording. Both previously reported `bitwise_parity: true` with
    /// nothing in the report to tell them apart, which is what this field fixes.
    ///
    /// It lives on [`ComparisonSpec`] rather than on the report so it is absent
    /// exactly when there was no comparison: `VerificationReport::comparison` is
    /// `None` for [`Verdict::NoResult`], so a run that never compared anything
    /// cannot publish a time policy describing a comparison that did not happen.
    pub virtualize_time: bool,
    /// Concrete: were numeric values, addresses, tmp paths, and timestamps
    /// normalized away wholesale before diffing (the lossy [`Stripped`] path)?
    ///
    /// [`Stripped`]: LogCompareStrictness::Stripped
    pub strip_lines: bool,
    /// Concrete: were host memory addresses canonicalized to first-appearance
    /// ordinals before diffing? Unlike
    /// [`Self::strip_lines`] this is lossless for parity: it discards only the
    /// raw pointer value, keeping identity, order, and aliasing.
    pub canonicalize_addresses: bool,
    /// Concrete: was the complete parity observation envelope compared (vs. the
    /// legacy deterministic subset)? For `BitwiseInfoV1`, that complete envelope
    /// is every INFO record admitted by [`Self::record_envelope`], and
    /// [`Self::log_scope`] records whether the explicit diagnostic full-trace
    /// superset was requested.
    pub full_trace: bool,
    /// Concrete: was everything OTHER than the stripped prefix and the
    /// canonicalized addresses compared exactly (virtual-time timestamps,
    /// syscall inputs/results, counts, sizes, flags)? True for the parity policy;
    /// false whenever a lossy normalization (e.g. [`Self::strip_lines`]) ran.
    pub exact_remainder: bool,
    /// Versioned tokens for every prefix STRIPPED before comparison. The parity
    /// policy permits exactly `["real-wall-clock-prefix/v1"]`; a lossy stripped
    /// comparison additionally lists the wholesale-normalization token. Recorded
    /// (not inferred) so a consumer can see precisely what was discarded.
    pub stripped_prefixes: &'static [&'static str],
    /// Versioned tokens for every CANONICALIZATION applied before comparison. The
    /// parity policy requires exactly
    /// `["host-address-to-first-appearance-ordinal/v1"]`.
    pub canonicalizations: &'static [&'static str],
    /// Concrete: were any `--ignore-lines` substring filters applied, dropping
    /// matching log lines before the comparison? Bitwise parity requires none.
    pub ignore_lines: bool,
    /// Concrete: were `COMMIT` messages excluded from the comparison? Bitwise
    /// parity requires them included.
    pub skip_commit: bool,
    /// Concrete: were `DETLOG` messages (or any DETLOG class) excluded from the
    /// comparison? Bitwise parity requires the full event stream.
    pub skip_detlog: bool,
}

impl ComparisonSpec {
    /// Build the spec (and, implicitly, the concrete diff flags) from the
    /// requested strictness and whether logs are compared at all. This is the
    /// single place the strictness label maps onto `strip_lines`/`full_trace`,
    /// so the flags the diff engine sees and the flags the verdict reports can
    /// never drift apart.
    pub fn new(
        strictness: LogCompareStrictness,
        compare_logs: bool,
        diagnostic_full_trace: bool,
        compare_io_buffers: bool,
        record_envelope: RecordEnvelopePolicy,
        virtualize_time: bool,
    ) -> Self {
        // Map the strictness label onto the concrete diff flags AND the versioned
        // policy tokens in one place, so the flags the engine sees, the tokens
        // the verdict reports, and the strictness label can never drift apart.
        let (
            strip_lines,
            canonicalize_addresses,
            full_trace,
            exact_remainder,
            log_scope,
            stripped_prefixes,
            canonicalizations,
            display_name,
        ) = match strictness {
            // Lossy wholesale normalization: numbers/addresses/paths/timestamps
            // erased; the remainder is NOT compared exactly.
            LogCompareStrictness::Stripped => {
                debug_assert!(!diagnostic_full_trace);
                (
                    true,
                    false,
                    false,
                    false,
                    ComparedLogScope::Deterministic,
                    &[STRIP_WALL_CLOCK_PREFIX_V1, STRIP_UNSAFE_NORMALIZATION_V1][..],
                    // Under stripping, addresses are ERASED (to a single `<ADDR>`
                    // token), not canonicalized; there is no ordinal preserved.
                    &[][..],
                    "Stripped",
                )
            }
            // Canonical parity: strip only the wall-clock prefix,
            // canonicalize addresses, and compare every INFO message admitted
            // by the selected named record envelope exactly.
            // The explicit verbose diagnostic mode compares the all-level
            // superset without changing the canonicalization policy.
            LogCompareStrictness::Canonical => (
                false,
                true,
                true,
                true,
                if diagnostic_full_trace {
                    ComparedLogScope::FullTrace
                } else {
                    ComparedLogScope::Info
                },
                PARITY_STRIPPED_PREFIXES,
                PARITY_CANONICALIZATIONS,
                if diagnostic_full_trace {
                    "BitwiseFullTraceV1"
                } else {
                    "BitwiseInfoV1"
                },
            ),
        };
        ComparisonSpec {
            strictness,
            display_name,
            compare_logs,
            compare_io_buffers,
            log_scope,
            record_envelope,
            virtualize_time,
            strip_lines,
            canonicalize_addresses,
            full_trace,
            exact_remainder,
            stripped_prefixes,
            canonicalizations,
            // The `--verify` code paths never expose the diff engine's line
            // filters, so the comparison they produce applies none. These are
            // recorded (not merely assumed) so a parity consumer can *require*
            // their absence rather than trust that no CLI surface enables them;
            // `default_log_diff_opts_apply_no_line_filters` binds these values to
            // the actual `LogDiffOpts` the engine sees.
            ignore_lines: false,
            skip_commit: false,
            skip_detlog: false,
        }
    }

    /// The `LogComparisonMode` this spec selects for the diff engine.
    fn log_comparison_mode(&self) -> LogComparisonMode {
        match self.log_scope {
            ComparedLogScope::Deterministic => LogComparisonMode::Deterministic,
            ComparedLogScope::Info => LogComparisonMode::Info,
            ComparedLogScope::FullTrace => LogComparisonMode::FullTrace,
        }
    }

    /// Whether the log engine and serialized policy are exactly the shared
    /// `BitwiseInfoV1` comparison. Observation properties such as output-buffer
    /// hashing and virtual time are recorded by that policy but do not select a
    /// different parser or normalization.
    fn uses_bitwise_info_v1(&self) -> bool {
        self.strictness == LogCompareStrictness::Canonical
            && self.compare_logs
            && self.log_scope == ComparedLogScope::Info
            && self.record_envelope == RecordEnvelopePolicy::AllRecordsV1
            && !self.strip_lines
            && self.canonicalize_addresses
            && self.full_trace
            && self.exact_remainder
            && self.stripped_prefixes == PARITY_STRIPPED_PREFIXES
            && self.canonicalizations == PARITY_CANONICALIZATIONS
            && !self.ignore_lines
            && !self.skip_commit
            && !self.skip_detlog
    }

    /// Does this comparison satisfy the `BitwiseInfoV1` parity contract a
    /// determinism / record-replay ratchet must require before it may read a
    /// `Matched` verdict as *true parity*? A bare `verified` is not enough:
    /// `verified` can rest on a stripped compare, an opaque filtered subset, or
    /// an output-only fallback, all of which normalize or omit exactly the data
    /// (virtual-time timestamps, raw syscall argument/result values, whole event
    /// classes) that parity exists to check.
    ///
    /// This requires the EXACT `BitwiseInfoV1` policy shape, not merely
    /// "not stripped": a generic `strip_lines = false` is inadmissible on its
    /// own. All clauses must hold:
    /// - the full INFO event stream within the named record envelope (or the
    ///   explicit all-level diagnostic superset) was compared
    ///   ([`Self::full_trace`] and [`Self::log_scope`]), which carries exact
    ///   virtual timestamps and syscall argument/result values;
    /// - no lossy wholesale normalization ran (`!strip_lines`) and the remainder
    ///   was compared exactly ([`Self::exact_remainder`]);
    /// - addresses were CANONICALIZED, not erased ([`Self::canonicalize_addresses`]),
    ///   so allocation-order and aliasing differences are still detectable;
    /// - the versioned policy tokens are exactly the parity set — only the
    ///   wall-clock prefix stripped, only address-ordinal canonicalization
    ///   applied — so a future extra strip/canonicalization cannot silently pass;
    /// - the record predicate is one of the explicitly named, versioned
    ///   canonical envelopes ([`Self::record_envelope`]); an opaque
    ///   caller-defined predicate cannot qualify;
    /// - no ad hoc ignore/skip filter dropped any line or event class
    ///   (`!ignore_lines && !skip_commit && !skip_detlog`);
    /// - syscall output-buffer content hashes were present in the records
    ///   ([`Self::compare_io_buffers`]);
    /// - the internal log stream was actually compared, not skipped for an
    ///   output-only fallback ([`Self::compare_logs`]).
    ///
    /// A consumer asking for parity must reject `Matched` under every weaker
    /// comparison or observation envelope; this predicate is that single
    /// acceptance rule.
    pub fn is_bitwise_parity(&self) -> bool {
        self.compare_io_buffers
            && self.compare_logs
            && self.full_trace
            && matches!(
                self.log_scope,
                ComparedLogScope::Info | ComparedLogScope::FullTrace
            )
            && !self.strip_lines
            && self.canonicalize_addresses
            && self.exact_remainder
            && self.stripped_prefixes == PARITY_STRIPPED_PREFIXES
            && self.canonicalizations == PARITY_CANONICALIZATIONS
            && self.record_envelope.is_canonical()
            && !self.ignore_lines
            && !self.skip_commit
            && !self.skip_detlog
    }
}

/// How much log evidence the comparison actually consumed.
///
/// A configured-strict comparison proves nothing if it had no data: two empty
/// selections "match" trivially. Carrying the counts with the verdict is what
/// lets a parity consumer require nonzero executed work, exactly as a test
/// result must carry its executed count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComparedLogCounts {
    /// Messages selected for comparison from the first run.
    pub left: usize,
    /// Messages selected for comparison from the second run.
    pub right: usize,
}

impl ComparedLogCounts {
    /// True when both sides actually contributed messages to the comparison.
    pub fn is_nonzero(&self) -> bool {
        self.left > 0 && self.right > 0
    }
}

fn runtime_stats(summary: &RunSummary) -> RuntimeStats {
    RuntimeStats {
        scheduler_turns: summary.sched_turns,
        virtual_nanoseconds: summary.virttime_elapsed,
        syscalls: summary.syscalls,
        dispatch: summary.dispatch_stats.clone(),
    }
}

pub(crate) fn verification_runtime_from_summaries(
    run1: Option<&RunSummary>,
    run2: Option<&RunSummary>,
) -> Option<VerificationRuntime> {
    (run1.is_some() || run2.is_some()).then(|| VerificationRuntime {
        run1: run1.map(runtime_stats),
        run2: run2.map(runtime_stats),
    })
}

/// The full outcome of comparing two runs: the verification [`Verdict`] plus the
/// guest exit status, so a caller never has to infer either one from the other.
#[derive(Debug, Clone)]
pub struct VerificationOutcome {
    pub verdict: Verdict,
    /// Why verification did not reach a verdict. None for a completed match or
    /// divergence; current no-result producers must provide a typed reason.
    pub no_result_reason: Option<NoResultReason>,
    /// The log comparator's exact refusal (a log cut at its size bound), kept
    /// even when an observed output or exit-status difference selected
    /// [`Verdict::Diverged`] and so left [`Self::no_result_reason`] empty. The
    /// ordinary report does not carry it, because the report schema allows a
    /// `no_result_reason` only beside a `no_result` verdict;
    /// [`write_skid_overshoot_verification_json`] reads it so that an overshoot
    /// cannot erase the refusal.
    pub refused_log_comparison: Option<String>,
    /// Exit status of the second (replay / repeat) run, propagated verbatim.
    pub guest_status: ExitStatus,
    /// The exact common output/log comparison used for [`Self::verdict`],
    /// carried so a consumer never has to assume which comparison a "matched"
    /// rests on. Backend-specific dimensions are carried separately below.
    pub comparison: ComparisonSpec,
    /// How many log messages the comparison actually compared, or `None` when
    /// the log comparison was not run at all (output-only fallback). `None` and
    /// `Some(0/0)` are both "no log evidence" and neither can support parity.
    pub compared_log_messages: Option<ComparedLogCounts>,
    /// Exact guest output and disposition for both runs.
    pub compared_outputs: ComparedOutputs,
    /// Typed whole-process DBT branch clocks, when that backend completed both
    /// runs and produced readable statistics. `None` for non-DBT runs and when
    /// verification did not reach a verdict.
    pub dbt_counted_branches: Option<DbtCountedBranchComparison>,
    /// Runtime totals for the two compared executions when their summaries were readable.
    pub runtime: Option<VerificationRuntime>,
    /// Scheduler turn at the first log divergence, when a preceding COMMIT
    /// identified the turn.
    pub first_divergent_scheduler_turn: Option<u64>,
    /// The first different record's position in each run's log, as
    /// `detcore::logdiff::log_record_events` counts records (`None` for a log
    /// that ended before it), when the logs were compared and differ. Not part
    /// of the report: `hermit::host_input_change` places an open against it.
    pub first_divergent_record_positions: Option<(Option<usize>, Option<usize>)>,
    /// Virtual nanoseconds at that same COMMIT, when the log recorded them.
    pub first_divergent_virtual_nanoseconds: Option<u64>,
    /// 1-based index of the first differing compared record.
    ///
    /// The two fields above are the position of the preceding scheduler COMMIT,
    /// so when no COMMIT precedes the differing record they both collapse to the
    /// origin and say nothing about how far the run got. This one is the
    /// LOCATION of the divergence rather than a bound on it, and it shares a
    /// unit with `compared_log_messages`, so `record / compared` is the fraction
    /// of the log that was deterministic.
    pub first_divergent_record: Option<usize>,
    /// How many syscalls the guest had COMPLETED when the divergence appeared,
    /// read from detcore's own `finish syscall #N` counter.
    ///
    /// The fourth unit, and the one closest to what a person debugging actually
    /// pictures: "the guest got 37 syscalls in" is more legible than a record
    /// index. It is NOT interchangeable with `first_divergent_record` -- one
    /// counts guest work, the other counts compared log records, and the two
    /// move at completely different rates.
    ///
    /// `null` when the logs matched, when no comparison ran, or when no syscall
    /// had completed before the divergence -- which is a real state, not a
    /// missing value: a run can diverge during startup.
    pub first_divergent_syscall: Option<u64>,
    /// First differing compared message from the left execution, with only the
    /// separately recorded syscall number, scheduler turn, and committed time
    /// removed.
    pub first_divergent_left_message: Option<String>,
    /// Corresponding first differing compared message from the right execution.
    pub first_divergent_right_message: Option<String>,
}

impl VerificationOutcome {
    /// Did verification pass, independent of the guest exit code?
    pub fn verified(&self) -> bool {
        self.verdict == Verdict::Matched
    }

    /// Collapse the outcome to the historical process-exit convention: a match
    /// propagates the guest exit status, a divergence exits one, and a refused
    /// comparison remains an error. Callers that need to separate the verdict
    /// from the guest exit code must read [`Self::verdict`] / [`Self::verified`]
    /// (or the `--verify-json` report) *before* calling this.
    pub fn into_exit_status(self) -> Result<ExitStatus, Error> {
        match self.verdict {
            Verdict::Matched => Ok(self.guest_status),
            Verdict::Diverged => Ok(ExitStatus::Exited(HERMIT_VERIFICATION_DIVERGENCE_EXIT)),
            // Still an error, so the historical nonzero process exit is
            // unchanged -- but carry the reason instead of replacing it with a
            // generic message.
            Verdict::NoResult => match self.no_result_reason {
                Some(NoResultReason::ComparisonRefused { detail }) => Err(Error::msg(format!(
                    "Verification did not reach a verdict: {detail}"
                ))),
                Some(reason) => Err(Error::msg(format!(
                    "Verification did not reach a verdict: {reason:?}"
                ))),
                None => Err(Error::msg("Verification did not reach a verdict.")),
            },
            Verdict::InfrastructureError => {
                Err(Error::msg("Verification recorded an infrastructure error."))
            }
        }
    }
}

fn comparison_report(comparison: &ComparisonSpec) -> ComparisonReport {
    ComparisonReport {
        strictness: comparison.strictness,
        display_name: Some(comparison.display_name.to_string()),
        compare_logs: comparison.compare_logs,
        compare_io_buffers: Some(comparison.compare_io_buffers),
        log_scope: Some(comparison.log_scope),
        record_envelope: match comparison.record_envelope {
            RecordEnvelopePolicy::AllRecordsV1 => RecordEnvelopeReport::AllRecordsV1,
            RecordEnvelopePolicy::DbtEvidenceTransportV1 => {
                RecordEnvelopeReport::DbtEvidenceTransportV1
            }
            RecordEnvelopePolicy::CrossBackendDetcoreV1 => {
                RecordEnvelopeReport::CrossBackendDetcoreV1
            }
            RecordEnvelopePolicy::CallerDefined => RecordEnvelopeReport::CallerDefined,
        },
        virtualize_time: Some(comparison.virtualize_time),
        strip_lines: Some(comparison.strip_lines),
        canonicalize_addresses: Some(comparison.canonicalize_addresses),
        full_trace: Some(comparison.full_trace),
        exact_remainder: Some(comparison.exact_remainder),
        stripped_prefixes: Some(
            comparison
                .stripped_prefixes
                .iter()
                .map(|value| (*value).to_string())
                .collect(),
        ),
        canonicalizations: Some(
            comparison
                .canonicalizations
                .iter()
                .map(|value| (*value).to_string())
                .collect(),
        ),
        ignore_lines: Some(comparison.ignore_lines),
        skip_commit: Some(comparison.skip_commit),
        skip_detlog: Some(comparison.skip_detlog),
    }
}

pub(crate) fn verification_report(outcome: &VerificationOutcome) -> VerificationReport {
    VerificationReport {
        verified: outcome.verified(),
        // Bitwise parity is a conjunction: the runs matched AND the
        // comparison was strict enough for the match to *mean* bitwise
        // identity. A `Diverged` verdict is never bitwise parity.
        // Three-way conjunction: the runs matched, the comparison was
        // strict enough for the match to *mean* bitwise identity, AND that
        // comparison actually consumed log evidence. The third conjunct is
        // not redundant: an empty-vs-empty log comparison reports "no
        // difference" under the strictest possible spec, so without a
        // nonzero count a run that produced no DETLOG at all would certify
        // as bitwise parity.
        bitwise_parity: outcome.verified()
            && outcome.comparison.is_bitwise_parity()
            && outcome
                .compared_log_messages
                .is_some_and(|counts| counts.is_nonzero()),
        verdict: outcome.verdict,
        no_result_reason: outcome.no_result_reason.clone(),
        infrastructure_error: None,
        comparison: (outcome.verdict != Verdict::NoResult)
            .then(|| comparison_report(&outcome.comparison)),
        compared_log_messages: if outcome.verdict == Verdict::NoResult {
            None
        } else {
            outcome
                .compared_log_messages
                .map(|counts| ComparedLogMessages {
                    left: u64::try_from(counts.left).expect("compared log count fits u64"),
                    right: u64::try_from(counts.right).expect("compared log count fits u64"),
                })
        },
        compared_outputs: (outcome.verdict != Verdict::NoResult)
            .then(|| outcome.compared_outputs.clone()),
        dbt_counted_branches: if outcome.verdict == Verdict::NoResult {
            None
        } else {
            outcome.dbt_counted_branches
        },
        runtime: outcome.runtime.clone(),
        guest_exit_code: outcome.guest_status.code(),
        guest_signal: outcome.guest_status.signal(),
        first_divergent_scheduler_turn: outcome.first_divergent_scheduler_turn,
        first_divergent_virtual_nanoseconds: outcome.first_divergent_virtual_nanoseconds,
        first_divergent_record: outcome
            .first_divergent_record
            .map(|record| u64::try_from(record).expect("divergent record index fits u64")),
        first_divergent_syscall: outcome.first_divergent_syscall,
        first_divergent_left_message: outcome.first_divergent_left_message.clone(),
        first_divergent_right_message: outcome.first_divergent_right_message.clone(),
        exact_branch_counter: None,
    }
}

/// The `exact_branch_counter` verdict every verification report this process
/// writes carries, once [`record_exact_branch_counter`] has run.
static EXACT_BRANCH_COUNTER: std::sync::OnceLock<BranchCounterVerdict> = std::sync::OnceLock::new();

/// Record, once per process, the `exact_branch_counter` verdict that
/// `hermit host-capabilities` reports, so that every verification report this
/// process writes from then on carries it ([`write_report_json`]). Called by a
/// run on a backend whose virtual clock is the retired-branch counter, after
/// its PMU configuration is installed: Reverie's validation reads that
/// configuration and fixes it for the process.
pub(crate) fn record_exact_branch_counter(verdict: BranchCounterVerdict) {
    let _ = EXACT_BRANCH_COUNTER.set(verdict);
}

/// `report` with the recorded `exact_branch_counter` verdict (`recorded`),
/// unless it already carries one.
fn with_exact_branch_counter(
    report: &VerificationReport,
    recorded: Option<&BranchCounterVerdict>,
) -> VerificationReport {
    let mut report = report.clone();
    if report.exact_branch_counter.is_none() {
        report.exact_branch_counter = recorded.cloned();
    }
    report
}

/// Write the verification report as a single JSON line to `path`.
///
/// This is the exit-code-independent verdict channel: the record it writes is
/// true or false based on whether verification matched, regardless of what the
/// guest exited with.
pub fn write_verification_json(path: &Path, outcome: &VerificationOutcome) -> Result<(), Error> {
    write_report_json(path, &verification_report(outcome))
}

/// Write a completed comparison that cannot be accepted because precise PMU
/// timer delivery passed its target. The comparison remains available for
/// diagnosis, while the canonical verdict makes the refusal unambiguous.
///
/// A comparison the comparator itself refused (a log cut at its size bound)
/// is written as that refusal instead: verdict `no_result` with its
/// `comparison_refused` reason. That holds whether the comparator selected
/// `no_result` or, because an output or exit-status difference was also
/// observed, `diverged`; the overshoot already made that difference
/// unattributable, and the refusal must not disappear with it. The report
/// schema allows a `no_result_reason` only beside a `no_result` verdict, so
/// one report cannot carry both causes, and converting this one to an
/// overshoot would erase the refusal. The refusal is the failure a fresh
/// attempt could hide: a reader that may retry a pure overshoot
/// (<https://github.com/rrnewton/hermit/issues/1845>) must not mistake this
/// attempt for one. The overshoot itself is still reported, by the
/// `HERMIT_SKID_OVERSHOOT` lines on stderr and by the caller's skid-overshoot
/// exit status.
pub fn write_skid_overshoot_verification_json(
    path: &Path,
    outcome: &VerificationOutcome,
    count: u64,
) -> Result<(), Error> {
    assert!(
        count > 0,
        "a skid-overshoot report requires a positive count"
    );
    let refusal = outcome.refused_log_comparison.clone().or_else(|| {
        match (&outcome.verdict, &outcome.no_result_reason) {
            (Verdict::NoResult, Some(NoResultReason::ComparisonRefused { detail })) => {
                Some(detail.clone())
            }
            _ => None,
        }
    });
    if let Some(detail) = refusal {
        let refused = VerificationOutcome {
            verdict: Verdict::NoResult,
            no_result_reason: Some(NoResultReason::ComparisonRefused { detail }),
            ..outcome.clone()
        };
        return write_report_json(path, &verification_report(&refused));
    }
    let mut report = verification_report(outcome);
    report.verified = false;
    report.bitwise_parity = false;
    report.verdict = Verdict::InfrastructureError;
    report.no_result_reason = None;
    report.infrastructure_error = Some(InfrastructureError::SkidOvershoot { count });
    report.dbt_counted_branches = None;
    write_report_json(path, &report)
}

/// Write a divergence that a host file changed during one of the runs
/// explains (`change` is [`InfrastructureError::HostInputChanged`], from
/// `hermit::host_input_change`). The comparison and the divergence position
/// are kept as evidence; the verdict says the divergence is not attributed to
/// Hermit.
pub fn write_host_input_change_verification_json(
    path: &Path,
    outcome: &VerificationOutcome,
    change: InfrastructureError,
) -> Result<(), Error> {
    assert_eq!(
        outcome.verdict,
        Verdict::Diverged,
        "only a divergence can be explained by a host input change"
    );
    assert!(
        matches!(change, InfrastructureError::HostInputChanged { .. }),
        "not a host input change: {change:?}"
    );
    let mut report = verification_report(outcome);
    report.verified = false;
    report.bitwise_parity = false;
    report.verdict = Verdict::InfrastructureError;
    report.no_result_reason = None;
    report.infrastructure_error = Some(change);
    report.dbt_counted_branches = None;
    write_report_json(path, &report)
}

/// Write a named PMU skid infrastructure error when execution did not produce
/// both sides of a comparison. Unlike the pending no-result stamp, this records
/// the cause that is already known.
pub fn write_skid_overshoot_without_comparison_json(
    path: &Path,
    count: u64,
    runtime: Option<VerificationRuntime>,
    guest_status: Option<ExitStatus>,
) -> Result<(), Error> {
    assert!(
        count > 0,
        "a skid-overshoot report requires a positive count"
    );
    let mut report = VerificationReport::no_result();
    report.verdict = Verdict::InfrastructureError;
    report.no_result_reason = None;
    report.infrastructure_error = Some(InfrastructureError::SkidOvershoot { count });
    report.runtime = runtime;
    report.guest_exit_code = guest_status.as_ref().and_then(ExitStatus::code);
    report.guest_signal = guest_status.as_ref().and_then(ExitStatus::signal);
    write_report_json(path, &report)
}

/// Publish an explicit NO-RESULT record to `path` *before* verification starts.
///
/// This is what makes the artifact invocation-bound. `write_verification_json`
/// can only run once a verdict exists, but a `--verify-json` run has several
/// earlier exits (a run that fails to start, a rejected first-run status, a
/// SaBRe capture with zero DETLOG, a failed recording). If the caller reuses a
/// path, every one of those exits would otherwise leave the PREVIOUS
/// invocation's record -- possibly `{"verified":true,"bitwise_parity":true}` --
/// sitting there, readable as if it described the invocation that just failed.
/// Stamping a no-result first means the file always describes *this* run: it is
/// either the terminal verdict or an honest "no verdict reached".
pub fn write_pending_verification_json(path: &Path) -> Result<(), Error> {
    write_report_json(path, &VerificationReport::no_result())
}

/// Forward the first compared run's guest output to Hermit's own stdout and
/// stderr, as an unverified run would. It is written when the comparison
/// verified, or when both runs' outputs agree: a disagreement was already
/// reported as a labelled diff, and printing one side would choose for the
/// reader. `run --verify` and `record start --verify` both emit through this.
pub(crate) fn emit_compared_guest_output(
    verified: bool,
    first: &Output,
    second: &Output,
) -> io::Result<()> {
    emit_compared_guest_output_to(
        verified,
        first,
        second,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    )
}

fn emit_compared_guest_output_to(
    verified: bool,
    first: &Output,
    second: &Output,
    stdout: &mut impl io::Write,
    stderr: &mut impl io::Write,
) -> io::Result<()> {
    if verified || (first.stdout == second.stdout && first.stderr == second.stderr) {
        stdout.write_all(&first.stdout)?;
        stderr.write_all(&first.stderr)?;
    }
    Ok(())
}

/// Execute one real verification run and replace the pending stamp only when
/// the container classifier established an unexpected child exit. Neither a
/// returned guest Output nor an unclassified error is that observation.
pub(crate) fn run_verification_execution<T>(
    path: Option<&Path>,
    run: VerificationRun,
    execute: impl FnOnce() -> Result<T, Error>,
) -> Result<T, Error> {
    execute().map_err(|error| {
        let Some(disposition) = container_failure_of(&error) else {
            return error;
        };
        let Some(path) = path else { return error };
        match write_report_json(path, &container_failed_report(run, disposition)) {
            Ok(()) => error,
            Err(secondary) => {
                retain_verification_error(error, "publishing container failure", secondary)
            }
        }
    })
}

/// The disposition of the container child whose unexpected exit `error`
/// reports ([`super::container::ContainerChildExit`]), or `None` for any other
/// error.
pub(crate) fn container_failure_of(error: &Error) -> Option<ContainerDisposition> {
    let super::container::ContainerChildExit(status) =
        error.downcast_ref::<super::container::ContainerChildExit>()?;
    Some(match *status {
        ExitStatus::Exited(code) => ContainerDisposition::Exited { code },
        ExitStatus::Signaled(signal, core_dumped) => ContainerDisposition::Signaled {
            signal: signal as i32,
            core_dumped,
        },
        ExitStatus::SignaledRaw(signal, core_dumped) => ContainerDisposition::Signaled {
            signal,
            core_dumped,
        },
    })
}

/// The no-result report of verification run `run`, whose container child
/// exited with `disposition` before returning the guest's output.
pub(crate) fn container_failed_report(
    run: VerificationRun,
    disposition: ContainerDisposition,
) -> VerificationReport {
    let mut report = VerificationReport::no_result();
    report.no_result_reason = Some(NoResultReason::ContainerFailed(ContainerFailure {
        run,
        disposition,
    }));
    report
}

/// The typed failure, if any, of a verification run Hermit had started.
///
/// A skid overshoot is not one: it already published its infrastructure
/// error. Neither is an untyped error (a bad flag, an unreadable path), which
/// leaves the pending stamp.
pub(crate) fn run_failure_of(error: &Error) -> Option<RunFailure> {
    if error.downcast_ref::<hermit::SkidOvershootError>().is_some() {
        None
    } else if error
        .downcast_ref::<super::run::DeterminismLossRecorded>()
        .is_some()
    {
        Some(RunFailure::DeterminismLoss)
    } else if error.downcast_ref::<hermit::BackendRunFailure>().is_some() {
        Some(RunFailure::BackendFailure)
    } else if error
        .downcast_ref::<super::container::PolicyRefusal>()
        .is_some()
    {
        Some(RunFailure::PolicyRefusal)
    } else {
        None
    }
}

/// The run_failed detail keeps the failure's message chain, bounded so one
/// pathological message cannot grow the report without limit.
const RUN_FAILED_DETAIL_MAX_BYTES: usize = 4096;

/// Replace the pending stamp with `run_failed` when `error` is a typed failure
/// of verification run `run` ([`RunFailure`]), and return `error` unchanged.
/// Call it only where `run` had been started: a refusal before any run (the
/// host's seccomp filter, an inexact branch counter) is a host condition, not
/// a result of this guest on this backend.
pub(crate) fn publish_run_failure(
    path: Option<&Path>,
    run: VerificationRun,
    error: Error,
) -> Error {
    let (Some(path), Some(failure)) = (path, run_failure_of(&error)) else {
        return error;
    };
    match write_report_json(path, &run_failed_report(run, failure, format!("{error:#}"))) {
        Ok(()) => error,
        Err(secondary) => retain_verification_error(error, "publishing run failure", secondary),
    }
}

pub(crate) fn run_failed_report(
    run: VerificationRun,
    failure: RunFailure,
    mut detail: String,
) -> VerificationReport {
    if detail.len() > RUN_FAILED_DETAIL_MAX_BYTES {
        let mut end = RUN_FAILED_DETAIL_MAX_BYTES;
        while !detail.is_char_boundary(end) {
            end -= 1;
        }
        detail.truncate(end);
    }
    let mut report = VerificationReport::no_result();
    report.no_result_reason = Some(NoResultReason::RunFailed {
        run,
        failure,
        detail,
    });
    report
}

/// While alive, Detcore's terminal deadlock exit during verification run `run`
/// first records the run at `path` as `run_failed` with `deadlock`.
///
/// That exit ends the process from the scheduler (`immediate_fatal_exit`), so
/// no error ever returns to verify and the pending stamp would remain. Only a
/// backend whose scheduler runs in this process (DBT) installs it; a backend
/// whose scheduler runs in a container child reports the same exit as a
/// container failure.
#[cfg(feature = "dbt")]
pub(crate) struct TerminalDeadlockRecording;

#[cfg(feature = "dbt")]
impl Drop for TerminalDeadlockRecording {
    fn drop(&mut self) {
        detcore::set_terminal_deadlock_hook(None);
    }
}

#[cfg(feature = "dbt")]
pub(crate) fn record_terminal_deadlock(
    path: Option<&Path>,
    run: VerificationRun,
) -> Option<TerminalDeadlockRecording> {
    let path = path?.to_owned();
    detcore::set_terminal_deadlock_hook(Some(Box::new(move |deadlock: &str| {
        // Best effort: the process exits next whatever this returns.
        let _ = write_report_json(
            &path,
            &run_failed_report(run, RunFailure::Deadlock, deadlock.trim_end().to_owned()),
        );
    })));
    Some(TerminalDeadlockRecording)
}

/// Keep the original typed error as anyhow's cause and the actual secondary
/// error as context. Classification/downcasts must still find the first cause.
#[derive(Debug)]
pub(crate) struct VerificationFailureContext {
    errors: Vec<(&'static str, Error)>,
}

impl std::fmt::Display for VerificationFailureContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, (operation, error)) in self.errors.iter().enumerate() {
            if index > 0 {
                formatter.write_str("; ")?;
            }
            write!(formatter, "{operation} also failed: {error:#}")?;
        }
        Ok(())
    }
}

pub(crate) fn retain_verification_error(
    mut primary: Error,
    operation: &'static str,
    error: Error,
) -> Error {
    if let Some(context) = primary.downcast_mut::<VerificationFailureContext>() {
        context.errors.push((operation, error));
        primary
    } else {
        primary.context(VerificationFailureContext {
            errors: vec![(operation, error)],
        })
    }
}

pub(crate) fn retain_logs_after_verification_error<const N: usize>(
    mut primary: Error,
    logs: [(&str, TempPath); N],
) -> Error {
    if primary.downcast_ref::<LogCapExceeded>().is_some() {
        return retain_logs_after_log_cap(primary, logs);
    }
    // Attempt every requested retention, even if an earlier one fails.
    eprintln!(":: Verification logs retained:");
    for (label, log) in logs {
        if let Err(secondary) = retain_verification_log(label, log) {
            primary = retain_verification_error(primary, "retaining verification log", secondary);
        }
    }
    primary
}

/// [`retain_logs_after_verification_error`] for a run that the log cap ended.
/// The logs are kept as for any other failure, but the report that names them
/// is attempted without waiting and is left out when stderr cannot take it at
/// once (see [`write_without_waiting`]). Through `eprintln!` it would wait on a
/// full stderr pipe, and panic, exiting 101 instead of 123, on one whose reader
/// has gone (round-3 review of https://github.com/rrnewton/hermit/pull/3686,
/// finding 4).
///
/// Lines are packed into writes of at most `PIPE_BUF` bytes, so that a pipe
/// takes each write whole or not at all; a line longer than that is left out
/// rather than cut.
fn retain_logs_after_log_cap<const N: usize>(
    mut primary: Error,
    logs: [(&str, TempPath); N],
) -> Error {
    let mut lines = vec![":: Verification logs retained:\n".to_owned()];
    // Attempt every requested retention, even if an earlier one fails.
    for (label, log) in logs {
        match log.keep() {
            Ok(path) => lines.push(format!("::   {label}: {}\n", path.display())),
            Err(secondary) => {
                primary = retain_verification_error(
                    primary,
                    "retaining verification log",
                    secondary.into(),
                );
            }
        }
    }
    let mut chunk = String::new();
    for line in lines.iter().filter(|line| line.len() <= libc::PIPE_BUF) {
        if chunk.len() + line.len() > libc::PIPE_BUF {
            write_without_waiting(libc::STDERR_FILENO, chunk.as_bytes());
            chunk.clear();
        }
        chunk.push_str(line);
    }
    if !chunk.is_empty() {
        write_without_waiting(libc::STDERR_FILENO, chunk.as_bytes());
    }
    primary
}

/// Write `report` to `path` atomically: a reader concurrent with the write sees
/// either the old contents or the complete new record, never a truncated one.
/// The directory to stage the temporary record in: always the one the target
/// lives in, so `persist` is a same-filesystem rename.
///
/// `Path::parent` returns an EMPTY path for a bare filename, not `"."`. Falling
/// back to the system temp directory for that case (as this did) puts the
/// staged file on a different filesystem than the target whenever `TMPDIR` and
/// the working directory differ -- the common case, e.g. tmpfs `/tmp` beside a
/// btrfs checkout. `persist` then fails with `EXDEV` and the caller returns an
/// error, leaving whatever the file previously held: a stale
/// `{verified:true}` survives precisely the invocation that was supposed to
/// overwrite it. A bare filename means the working directory, so say so.
fn staging_directory(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

pub fn write_report_json(path: &Path, report: &VerificationReport) -> Result<(), Error> {
    use std::io::Write as _;

    let report = with_exact_branch_counter(report, EXACT_BRANCH_COUNTER.get());
    let json = serde_json::to_string(&report)?;
    // Same directory as the target so the rename below stays within one
    // filesystem and is therefore atomic.
    let mut temp = NamedTempFile::new_in(staging_directory(path))
        .with_context(|| format!("creating a temporary file beside {}", path.display()))?;
    writeln!(temp, "{json}")
        .with_context(|| format!("writing verification verdict for {}", path.display()))?;
    temp.flush()
        .with_context(|| format!("flushing verification verdict for {}", path.display()))?;
    temp.persist(path)
        .map_err(|e| e.error)
        .with_context(|| format!("publishing verification verdict to {}", path.display()))?;
    Ok(())
}

/// Reject an explicit log level that would suppress the events verification compares.
pub(crate) fn validate_log_level(global: &GlobalOpts) -> Result<(), Error> {
    if let Some(level) = global.log
        && level < LevelFilter::INFO
    {
        anyhow::bail!(
            "--verify requires --log=info or a more verbose level; received --log={}",
            level.to_string().to_ascii_lowercase()
        );
    }
    Ok(())
}

/// Resolve the capture verbosity independently from the comparison scope.
///
/// Canonical verification defaults to INFO because INFO is the declared
/// `BitwiseInfoV1` observation envelope. An explicit DEBUG/TRACE request is
/// preserved in the capture for diagnostics, but ordinary canonical comparison
/// still selects INFO. Legacy stripped verification keeps its DEBUG default.
/// The explicit full-trace diagnostic mode requires TRACE regardless of a lower
/// requested level.
pub(crate) fn verification_log_level(
    requested: Option<LevelFilter>,
    strictness: LogCompareStrictness,
    diagnostic_full_trace: bool,
) -> LevelFilter {
    if diagnostic_full_trace {
        requested
            .unwrap_or(LevelFilter::TRACE)
            .max(LevelFilter::TRACE)
    } else {
        requested.unwrap_or(match strictness {
            LogCompareStrictness::Stripped => LevelFilter::DEBUG,
            LogCompareStrictness::Canonical => LevelFilter::INFO,
        })
    }
}

pub fn temp_log_files(name1: &str, name2: &str) -> io::Result<(NamedTempFile, NamedTempFile)> {
    temp_log_files_in(name1, name2, None)
}

/// The directory for the temporary run logs of a verify that names no
/// directory of its own, instead of the system temporary directory.
///
/// A verify that is killed leaves its two logs where they were created: the
/// first cut off, the second often empty. A caller that kills runs at a
/// deadline sets this to a directory it removes afterwards, so the host's
/// temporary directory is not filled. Setting TMPDIR instead would also
/// change the guest's environment; this variable is removed from the
/// environment a guest inherits from Hermit (`--base-env=host`).
pub const VERIFY_TMPDIR_ENV: &str = "HERMIT_VERIFY_TMPDIR";

pub fn temp_log_files_in(
    name1: &str,
    name2: &str,
    directory: Option<&Path>,
) -> io::Result<(NamedTempFile, NamedTempFile)> {
    let fallback = std::env::var_os(VERIFY_TMPDIR_ENV).filter(|value| !value.is_empty());
    let create = |name: &str| {
        let prefix = format!("{}_log_", name);
        let mut builder = tempfile::Builder::new();
        builder.prefix(&prefix).rand_bytes(5);
        match (directory, fallback.as_deref()) {
            (Some(directory), _) => builder.tempfile_in(directory),
            (None, Some(fallback)) => builder.tempfile_in(fallback).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "cannot create a verify log in {VERIFY_TMPDIR_ENV}={}: {error}",
                        Path::new(fallback).display()
                    ),
                )
            }),
            (None, None) => builder.tempfile(),
        }
    };
    let file1 = create(name1)?;
    let file2 = create(name2)?;

    Ok((file1, file2))
}

pub fn setup_double_run(
    global: &GlobalOpts,
    name1: &str,
    name2: &str,
    strictness: LogCompareStrictness,
) -> io::Result<((GlobalOpts, NamedTempFile), (GlobalOpts, NamedTempFile))> {
    let (file1, file2) = temp_log_files(name1, name2)?;

    let path1 = PathBuf::from(file1.path());
    let path2 = PathBuf::from(file2.path());

    // Override global settings.  Unfortunately we lose the log output to the
    // screen.
    let mut global = global.clone();
    global.log_file = Some(path1);
    global.log = Some(verification_log_level(global.log, strictness, false));

    let mut global2 = global.clone();
    global2.log_file = Some(path2);
    Ok(((global, file1), (global2, file2)))
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-644): Review re-emitting aggregate warnings captured by verification.
fn unsupported_syscalls_from_log(path: &Path) -> io::Result<BTreeSet<String>> {
    let mut syscalls = BTreeSet::new();
    for line in std::fs::read_to_string(path)?.lines() {
        let Some((_, remainder)) = line.split_once("syscalls ") else {
            continue;
        };
        let Some((names, _)) = remainder.split_once(" used but not yet supported") else {
            continue;
        };
        for name in names.split(',') {
            if !name.is_empty()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            {
                syscalls.insert(name.to_owned());
            }
        }
    }
    Ok(syscalls)
}

pub(crate) fn retain_verification_logs<const N: usize>(
    logs: [(&str, TempPath); N],
) -> Result<Vec<PathBuf>, Error> {
    let mut retained = Vec::with_capacity(N);
    eprintln!(":: Verification logs retained:");
    for (label, log) in logs {
        retained.push(retain_verification_log(label, log)?);
    }
    Ok(retained)
}

/// How many of a run's last INFO DETLOG records [`signal_termination_report`]
/// shows.
const SIGNAL_REPORT_RECORDS: usize = 20;
/// How much of the end of a run's log [`signal_termination_report`] reads.
const SIGNAL_REPORT_TAIL_BYTES: u64 = 4 << 20;
/// The longest record [`signal_termination_report`] shows before cutting it.
const SIGNAL_REPORT_LINE_BYTES: usize = 400;

/// What the guest of a run was doing when a signal terminated it: the last
/// INFO DETLOG records of its log, the records `--verify` compares, for
/// printing to stderr. `None` unless `status` is a termination by a signal.
/// The DEBUG records a verification log also holds (register dumps, scheduler
/// steps) would otherwise push the guest's last system calls out of the
/// report.
///
/// A verify log lives where `--verify-log-dir` put it, which for a test is
/// usually inside a disposable checkout, and `--keep-logs` keeps it only as
/// long as that checkout. Stderr is what a test runner keeps, so a crash
/// under `--verify` that the caller never reproduces is still diagnosable
/// from these records; see https://github.com/rrnewton/hermit/issues/3833,
/// where it was not.
///
/// The wait status carries only the signal and whether a core was dumped, not
/// the faulting address or thread, so the records are the evidence of where.
/// Each is shown without its machine-readable suffix and cut at
/// [`SIGNAL_REPORT_LINE_BYTES`], so the report stays a few kilobytes.
pub(crate) fn signal_termination_report(
    label: &str,
    status: ExitStatus,
    log: &Path,
) -> Option<String> {
    let (number, name, core_dumped) = match status {
        ExitStatus::Signaled(signal, core_dumped) => {
            (signal as i32, format!("{signal:?}"), core_dumped)
        }
        // A realtime signal, which nix's Signal cannot name
        // (https://github.com/rrnewton/reverie/issues/997).
        ExitStatus::SignaledRaw(signal, core_dumped) => {
            (signal, format!("SIG{signal}"), core_dumped)
        }
        ExitStatus::Exited(_) => return None,
    };
    let core = if core_dumped { ", core dumped" } else { "" };
    let heading = format!(":: The {label} guest was terminated by signal {number} ({name}{core}).");
    let tail = match read_log_tail(log, SIGNAL_REPORT_TAIL_BYTES) {
        Ok(tail) => tail,
        Err(error) => {
            return Some(format!(
                "{heading} Its log {} could not be read: {error}\n",
                log.display()
            ));
        }
    };
    let records: Vec<&str> = tail
        .lines()
        .filter(|line| is_info_detlog(line))
        .map(|line| {
            // The last separator, as the DETLOG reader splits: the human
            // text before it can hold the same characters, in a pathname.
            line.rsplit_once(detcore::detlog::RECORD_SEPARATOR)
                .map_or(line, |(human, _)| human)
        })
        .collect();
    if records.is_empty() {
        return Some(format!(
            "{heading} The last {} bytes of its log {} hold no INFO DETLOG record.\n",
            SIGNAL_REPORT_TAIL_BYTES,
            log.display()
        ));
    }
    let shown = &records[records.len().saturating_sub(SIGNAL_REPORT_RECORDS)..];
    let mut report = format!(
        "{heading} Its last {} INFO DETLOG records, from {}:\n",
        shown.len(),
        log.display()
    );
    for record in shown {
        report.push_str("::   ");
        report.push_str(cut_at_char_boundary(record, SIGNAL_REPORT_LINE_BYTES));
        if record.len() > SIGNAL_REPORT_LINE_BYTES {
            report.push_str(" [cut]");
        }
        report.push('\n');
    }
    Some(report)
}

/// The [`signal_termination_report`]s of a verification's runs, prepared
/// while each run's log exists, because the comparison consumes the logs.
///
/// When this is dropped they are queued, unless [`Self::verified`] discarded
/// them first. So every way a verification ends without verifying its runs
/// reports the signals that ended them: a status `--verify-allow` refuses, a
/// comparison that rejects runs whose statuses `--verify-allow=both` or
/// `failure` admitted, or an error. A verification that verifies its runs has
/// nothing to explain and prints nothing.
///
/// `main` writes the queue with [`write_queued_signal_reports`], after
/// everything else hermit says, so the report can neither delay nor replace
/// the verification's own disposition: nothing waits behind it, and it never
/// waits itself.
#[must_use]
#[derive(Default)]
pub(crate) struct SignalTerminationReports(Vec<String>);

/// The reports of verifications that ended unverified, for
/// [`write_queued_signal_reports`].
static QUEUED_SIGNAL_REPORTS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Write the queued [`SignalTerminationReports`] to stderr. `main` calls this
/// last, just before hermit exits.
///
/// No write waits for the reader of stderr, with or without
/// `--max-log-bytes`: each goes through
/// [`hermit::nonwaiting_write::write_without_waiting_for_a_reader`], and the
/// first one that stderr refuses or that fails ends the report. A pipe nobody
/// reads therefore gets what fits and hermit exits; the line that filled it
/// may be cut.
pub(crate) fn write_queued_signal_reports() {
    let reports = std::mem::take(
        &mut *QUEUED_SIGNAL_REPORTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    write_signal_reports(libc::STDERR_FILENO, &reports);
}

/// Write `reports` to `fd` for [`write_queued_signal_reports`], one line per
/// write.
///
/// ⚠️ ONE WRITE OF A WHOLE REPORT CAN PUT NOTHING IN A PIPE THAT HAS ROOM. A
/// non-blocking write to a Linux pipe adds to the pipe's last page only the
/// part of the write past its last whole page, and only when all of that part
/// fits there; the rest needs an empty page. A report of 7.8 KB written to a
/// one-page pipe that already holds 700 bytes is therefore refused whole,
/// although 3.4 KB of the page are free: whether its first line arrived
/// depended on how much hermit had said before it, and the no-perf-counters
/// warning a GitHub-hosted runner prints was enough to lose it. Records are
/// cut at [`SIGNAL_REPORT_LINE_BYTES`], far less than a page, so each line is
/// added to the last page while it fits.
fn write_signal_reports(fd: libc::c_int, reports: &[String]) {
    for line in reports
        .iter()
        .flat_map(|report| report.split_inclusive('\n'))
    {
        let mut rest = line.as_bytes();
        while !rest.is_empty() {
            match hermit::nonwaiting_write::write_without_waiting_for_a_reader(fd, rest) {
                Ok(written) if written > 0 => rest = &rest[written..],
                _ => return,
            }
        }
    }
}

impl SignalTerminationReports {
    /// Prepare the report of run `label`, if `status` is a termination by a
    /// signal. `log` must still hold the run's log.
    pub(crate) fn prepare(&mut self, label: &str, status: ExitStatus, log: &Path) {
        self.0.extend(signal_termination_report(label, status, log));
    }

    /// The runs verified: discard the reports.
    pub(crate) fn verified(mut self) {
        self.0.clear();
    }
}

impl Drop for SignalTerminationReports {
    fn drop(&mut self) {
        if std::thread::panicking() || self.0.is_empty() {
            return;
        }
        QUEUED_SIGNAL_REPORTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .append(&mut self.0);
    }
}

/// Whether `line` is an INFO DETLOG record. A record starts with its level,
/// after the timestamp a log file gives it.
fn is_info_detlog(line: &str) -> bool {
    let mut words = line.split_whitespace();
    let info = match words.next() {
        Some("INFO") => true,
        Some(_) => words.next() == Some("INFO"),
        None => false,
    };
    info && line.contains(" DETLOG ")
}

/// The last `max` bytes of `path`, from the first line that starts within
/// them, with invalid UTF-8 replaced.
fn read_log_tail(path: &Path, max: u64) -> io::Result<String> {
    use std::io::Read;
    use std::io::Seek;
    use std::io::SeekFrom;

    let mut file = File::open(path)?;
    let start = file.metadata()?.len().saturating_sub(max);
    // From one byte earlier, which tells whether `start` begins a line.
    file.seek(SeekFrom::Start(start.saturating_sub(1)))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    if start > 0 {
        // Drop that byte, and the rest of the line it ends or is part of.
        let first_line_end = bytes.iter().position(|&byte| byte == b'\n');
        bytes.drain(..first_line_end.map_or(bytes.len(), |end| end + 1));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn cut_at_char_boundary(text: &str, max: usize) -> &str {
    let mut end = max.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn retain_verification_log(label: &str, log: TempPath) -> Result<PathBuf, Error> {
    let path = log.keep()?;
    eprintln!("::   {label}: {}", path.display());
    Ok(path)
}

fn flock(file: &File, operation: i32) -> io::Result<()> {
    // SAFETY: flock only reads the supplied descriptor and operation. `file`
    // keeps the descriptor open for at least the duration of the call.
    if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn open_retention_lock(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
}

fn persist_log(log: TempPath, destination: &Path) -> io::Result<()> {
    match log.persist_noclobber(destination) {
        Ok(()) => Ok(()),
        Err(error) if error.error.raw_os_error() == Some(libc::EXDEV) => {
            fs::copy(&*error.path, destination)?;
            drop(error.path);
            Ok(())
        }
        Err(error) => Err(error.error),
    }
}

fn retained_comparison_dirs(root: &Path) -> io::Result<Vec<(PathBuf, std::time::SystemTime)>> {
    let mut directories = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(FAILED_VERIFY_LOG_DIR_PREFIX) {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if !metadata.file_type().is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
            continue;
        }
        directories.push((entry.path(), metadata.modified()?));
    }
    directories.sort_by(|(left_path, left_time), (right_path, right_time)| {
        right_time
            .cmp(left_time)
            .then_with(|| right_path.cmp(left_path))
    });
    Ok(directories)
}

fn finish_interrupted_retirements(root: &Path) -> io::Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(FAILED_VERIFY_LOG_RETIRING_PREFIX)
            && !name.starts_with(FAILED_VERIFY_LOG_PENDING_PREFIX)
        {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_dir() && metadata.uid() == unsafe { libc::geteuid() } {
            fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(())
}

fn retire_failed_verification_logs(root: &Path, current: &Path, keep: usize) -> io::Result<usize> {
    let directories = retained_comparison_dirs(root)?;
    let mut kept = BTreeSet::from([current.to_path_buf()]);
    for (path, _) in &directories {
        if kept.len() >= keep.max(1) {
            break;
        }
        kept.insert(path.clone());
    }

    let mut retired = 0;
    for (path, _) in directories {
        if kept.contains(&path) {
            continue;
        }
        let lock = match open_retention_lock(&path.join(FAILED_VERIFY_LOG_LOCK)) {
            Ok(lock) => lock,
            Err(error) => {
                eprintln!(
                    "WARNING: could not inspect retained verification logs {}: {error}",
                    path.display()
                );
                continue;
            }
        };
        match flock(&lock, libc::LOCK_EX | libc::LOCK_NB) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
            Err(error) => {
                eprintln!(
                    "WARNING: could not lock retained verification logs {}: {error}",
                    path.display()
                );
                continue;
            }
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| io::Error::other("retained verification directory has no name"))?;
        let retiring = tempfile::Builder::new()
            .prefix(&format!("{FAILED_VERIFY_LOG_RETIRING_PREFIX}{name}-"))
            .rand_bytes(12)
            .tempdir_in(root)?
            .keep();
        fs::remove_dir(&retiring)?;
        fs::rename(&path, &retiring)?;
        fs::remove_dir_all(&retiring)?;
        retired += 1;
    }
    Ok(retired)
}

fn retain_failed_verification_logs(
    logs: [(&str, TempPath); 2],
    retention: &FailedVerifyLogRetention,
) -> Result<Vec<PathBuf>, Error> {
    fs::create_dir_all(&retention.root).with_context(|| {
        format!(
            "could not create failed verification log directory {}",
            retention.root.display()
        )
    })?;
    let root = fs::canonicalize(&retention.root).with_context(|| {
        format!(
            "could not resolve failed verification log directory {}",
            retention.root.display()
        )
    })?;
    let root_lock = open_retention_lock(&root.join(FAILED_VERIFY_LOG_ROOT_LOCK))?;
    flock(&root_lock, libc::LOCK_EX)?;
    finish_interrupted_retirements(&root)?;

    let pending = tempfile::Builder::new()
        .prefix(FAILED_VERIFY_LOG_PENDING_PREFIX)
        .rand_bytes(12)
        .tempdir_in(&root)?
        .keep();
    let pending_name = pending
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::other("failed verification directory has no name"))?;
    let final_name = pending_name
        .strip_prefix(FAILED_VERIFY_LOG_PENDING_PREFIX)
        .ok_or_else(|| io::Error::other("failed verification directory has an invalid name"))?;
    let completed = root.join(format!("{FAILED_VERIFY_LOG_DIR_PREFIX}{final_name}"));
    let pair_lock = open_retention_lock(&pending.join(FAILED_VERIFY_LOG_LOCK))?;
    flock(&pair_lock, libc::LOCK_EX)?;

    let mut retained = Vec::with_capacity(logs.len());
    for (index, (label, log)) in logs.into_iter().enumerate() {
        let original_name = log
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(if index == 0 { "run1.log" } else { "run2.log" });
        let destination = pending.join(original_name);
        persist_log(log, &destination)?;
        retained.push((label, destination));
    }
    fs::rename(&pending, &completed)?;
    for (label, path) in &mut retained {
        *path = completed.join(path.file_name().expect("retained log has a name"));
        eprintln!("::   {label}: {}", path.display());
    }
    drop(pair_lock);

    let retired = retire_failed_verification_logs(&root, &completed, retention.keep)?;
    if retired > 0 {
        eprintln!(
            ":: Retired {retired} older failed verification comparison(s); keeping the newest {} plus comparisons with active readers",
            retention.keep.max(1)
        );
    }
    Ok(retained.into_iter().map(|(_, path)| path).collect())
}

pub(crate) fn lock_failed_verification_logs_for_read(
    paths: impl IntoIterator<Item = PathBuf>,
) -> io::Result<Vec<File>> {
    let mut directories = BTreeSet::new();
    for path in paths {
        let path = match fs::canonicalize(path) {
            Ok(path) => path,
            // Follow mode also accepts logs their writers have not created yet.
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let Some(directory) = path.parent() else {
            continue;
        };
        let Some(name) = directory.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with(FAILED_VERIFY_LOG_DIR_PREFIX)
            && directory.join(FAILED_VERIFY_LOG_LOCK).is_file()
        {
            directories.insert(directory.to_path_buf());
        }
    }
    let mut locks = Vec::with_capacity(directories.len());
    for directory in directories {
        let lock = open_retention_lock(&directory.join(FAILED_VERIFY_LOG_LOCK))?;
        flock(&lock, libc::LOCK_SH)?;
        locks.push(lock);
    }
    Ok(locks)
}

pub fn compare_two_runs(
    first: ComparedRun<'_>,
    second: ComparedRun<'_>,
    options: ComparisonOptions,
) -> Result<VerificationOutcome, Error> {
    compare_two_runs_with_unsupported_scan(first, second, options, unsupported_syscalls_from_log)
}

fn compare_two_runs_with_unsupported_scan(
    first: ComparedRun<'_>,
    second: ComparedRun<'_>,
    options: ComparisonOptions,
    scan_unsupported_syscalls: impl Fn(&Path) -> io::Result<BTreeSet<String>>,
) -> Result<VerificationOutcome, Error> {
    let ComparedRun {
        output: out1,
        log: log1,
        label: label1,
    } = first;
    let ComparedRun {
        output: out2,
        log: log2,
        label: label2,
    } = second;
    let output_evidence = |output: &Output| ComparedOutput {
        exit_code: output.status.code(),
        signal: output.status.signal(),
        stdout_sha256: detcore::Digest::new(&output.stdout).to_string(),
        stdout_bytes: u64::try_from(output.stdout.len()).expect("guest stdout length fits u64"),
        stderr_sha256: detcore::Digest::new(&output.stderr).to_string(),
        stderr_bytes: u64::try_from(output.stderr.len()).expect("guest stderr length fits u64"),
    };
    let compared_outputs = ComparedOutputs {
        left: output_evidence(out1),
        right: output_evidence(out2),
    };
    let compared_labels = ComparisonSideLabels::new(label1, label2);
    let mut failed = false;
    // A difference that was actually OBSERVED, as opposed to a comparison that
    // was refused. Only an observed difference can justify a `Diverged`
    // verdict; see the verdict selection at the end of this function.
    let mut observed_divergence = false;
    // The log comparator's exact reason for declining to produce a verdict.
    let mut comparison_refusal_reason = None;
    // None until the log comparison actually runs; stays None on the
    // output-only fallback so the report can distinguish
    // "compared nothing" from "compared and matched".
    let mut compared_log_messages: Option<ComparedLogCounts> = None;
    let mut first_divergent_scheduler_turn = None;
    let mut first_divergent_record_positions = None;
    let mut first_divergent_virtual_nanoseconds = None;
    let mut first_divergent_record = None;
    let mut first_divergent_syscall = None;
    let mut first_divergent_left_message = None;
    let mut first_divergent_right_message = None;

    // Resolve the strictness label to concrete diff flags once, and carry the
    // resulting spec through to the verdict so the returned outcome records
    // exactly which comparison certified it.
    let spec = ComparisonSpec::new(
        options.strictness,
        options.compare_logs,
        options.diagnostic_full_trace,
        options.compare_io_buffers,
        options.record_envelope.policy(),
        options.virtualize_time,
    );

    if out1.stdout != out2.stdout {
        failed = true;
        observed_divergence = true;
        eprintln!("Mismatch in stdout between {label1} and {label2}:");
        let str1 = String::from_utf8_lossy(&out1.stdout);
        let str2 = String::from_utf8_lossy(&out2.stdout);
        if str1.lines().count() > 1 {
            display_diff(&str1, &str2);
        } else {
            eprintln!("{}", Comparison::new(&str1, &str2));
        }
    }

    if out1.stderr != out2.stderr {
        failed = true;
        observed_divergence = true;
        eprintln!("Mismatch in stderr between {label1} and {label2}:");
        let str1 = String::from_utf8_lossy(&out1.stderr);
        let str2 = String::from_utf8_lossy(&out2.stderr);
        if str1.lines().count() > 1 {
            display_diff(&str1, &str2);
        } else {
            eprintln!("{}", Comparison::new(&str1, &str2));
        }
    }

    let log_processing_result = (|| -> Result<(), Error> {
        if options.compare_logs {
            eprintln!(
                ":: {}",
                "Comparing captured verification logs...".yellow().bold()
            );
            // The comparison semantics come from `spec` (strip_lines + mode); only
            // the printed syscall-history depth still tracks `verbose`. Historically
            // both were flipped together, so the sole bitwise comparison was also the
            // loudest — decoupling them lets a quiet run be bitwise-strict.
            let diff_options = logdiff::LogDiffOpts {
                strip_lines: spec.strip_lines,
                // Thread canonical address normalization from the spec so the parity
                // (`Canonical`) policy actually rewrites host addresses to ordinals
                // in the engine; without this the verdict would REPORT
                // `canonicalize_addresses = true` while the diff ran with the raw
                // addresses — the exact proxy/binding drift the spec exists to close.
                canonicalize_addresses: spec.canonicalize_addresses,
                comparison: spec.log_comparison_mode(),
                side_labels: compared_labels.clone(),
                // These logs were produced by this invocation, so accepting the
                // historical prose-only shape would hide a producer regression.
                // Standalone log-diff keeps that compatibility path for retained
                // older logs; a current verification result requires Detcore's
                // structured event records.
                require_structured_events: true,
                syscall_history: if options.verbose { 10 } else { 5 },
                // Thread the filter facts from the spec so what the verdict *reports*
                // (`spec.skip_commit`/`spec.skip_detlog`) is exactly what the diff
                // engine *does*; the remaining filters stay at their no-op defaults.
                skip_commit: spec.skip_commit,
                skip_detlog: spec.skip_detlog,
                ..Default::default()
            };
            // Bind the spec's recorded filter-absence to the engine's real defaults:
            // if `LogDiffOpts::default()` ever grew a filtering default, the spec
            // would silently misreport "no filters", so refuse to run in that case.
            debug_assert!(
                diff_options.ignore_lines.is_empty() != spec.ignore_lines,
                "ComparisonSpec.ignore_lines must match the diff engine's ignore_lines"
            );

            let summary = if spec.uses_bitwise_info_v1() {
                // This path cannot be weakened by caller-selected record or
                // normalization filters while its report names BitwiseInfoV1.
                // Diagnostic settings preserve the existing console output but
                // cannot alter the verdict.
                let mut diagnostic_output = std::io::stderr();
                logdiff::try_compare_bitwise_info_v1_with_diagnostics(
                    log1.as_ref(),
                    log2.as_ref(),
                    compared_labels.clone(),
                    logdiff::BitwiseInfoV1Diagnostics {
                        difference_limit: diff_options.limit,
                        syscall_history: diff_options.syscall_history,
                        no_color: diff_options.no_color,
                        print_logs: diff_options.print_logs,
                    },
                    &mut diagnostic_output,
                )?
            } else {
                logdiff::try_log_diff_detailed_with_filter(
                    log1.as_ref(),
                    log2.as_ref(),
                    &diff_options,
                    options.record_envelope.predicate(),
                )?
            };
            compared_log_messages = Some(ComparedLogCounts {
                left: summary.compared_left,
                right: summary.compared_right,
            });
            // A recvmsg whose buffers could not be observed after the call is
            // logged as an `unobserved` buffer-digest record and compared like
            // any other; every verification path reports how many there were,
            // so the gap is never silent.
            let unobserved = [log1.as_ref(), log2.as_ref()].map(|log: &std::path::Path| {
                logdiff::count_unobserved_receives(log)
                    .map_or_else(|_| "unreadable".to_string(), |count| count.to_string())
            });
            if unobserved.iter().any(|count| count != "0") {
                eprintln!(
                    ":: recvmsg calls whose buffers were unobservable after the call (compared as 'unobserved' buffer-digest entries): run1={}, run2={}",
                    unobserved[0], unobserved[1]
                );
            }
            if summary.diff_found {
                failed = true;
                if let Some(refusal_reason) = summary.refusal_reason.clone() {
                    comparison_refusal_reason = Some(refusal_reason);
                } else {
                    observed_divergence = true;
                }
                first_divergent_scheduler_turn = summary.first_divergent_scheduler_turn;
                first_divergent_record_positions = summary.first_divergent_record_positions;
                first_divergent_virtual_nanoseconds = summary.first_divergent_virtual_nanoseconds;
                // Set inside this `diff_found` arm, matching its two siblings
                // above.
                //
                // The arm is DEFENCE, not the source of the guarantee, and the
                // difference matters to whoever edits this next: a matching pair
                // already yields `None` from the comparator, because
                // `first_divergent_record` is computed from `first_different`,
                // which is `None` when nothing differed. Deleting this arm would
                // therefore NOT produce a `0` today, and no test would catch its
                // removal -- verified by deleting the equivalent gate in
                // `logdiff.rs` and watching
                // `identical_logs_have_no_first_divergent_record` still pass.
                //
                // It is kept because it costs nothing and it is what holds the
                // invariant if the comparator ever starts reporting a position
                // on a match.
                first_divergent_record = summary.first_divergent_record;
                first_divergent_syscall = summary.first_divergent_syscall;
                first_divergent_left_message = summary.first_divergent_left_message.clone();
                first_divergent_right_message = summary.first_divergent_right_message.clone();
                if summary.refusal_reason.is_none() {
                    eprintln!(
                        ":: {}",
                        format!("Log differences found between {label1} and {label2}.")
                            .red()
                            .bold()
                    );
                }
            }
        } else {
            eprintln!(
                ":: Comparing guest output and exit status only; internal logs were not compared"
            );
        }

        if out1.status != out2.status {
            failed = true;
            observed_divergence = true;
            eprintln!(
                "Mismatch in exit status between {label1} and {label2}: {}",
                Comparison::new(&out1.status, &out2.status)
            );
        }

        if !failed {
            let mut unsupported = scan_unsupported_syscalls(log1.as_ref())?;
            unsupported.extend(scan_unsupported_syscalls(log2.as_ref())?);
            if let Some(message) = detcore::format_unsupported_syscall_warning(&unsupported) {
                eprintln!("WARNING: {message}");
            }
        }
        Ok(())
    })();

    if options.caller_owns_logs {
        // Links to the caller's logs: dropping them leaves the logs.
        drop((log1, log2));
        log_processing_result?;
    } else {
        if let Err(error) = log_processing_result {
            if options.keep_logs || failed {
                if let Some(retention) = &options.failed_log_retention {
                    eprintln!(":: Verification logs retained:");
                    retain_failed_verification_logs([(label1, log1), (label2, log2)], retention)?;
                } else {
                    retain_verification_logs([(label1, log1), (label2, log2)])?;
                }
            }
            return Err(error);
        }

        // Divergence historically retained both diagnostics, and it still does.
        // `--keep-logs` extends retention to successful comparisons, but a match
        // needs only one log: once the two logs compared equal under the selected
        // policy, the first is kept as the golden log and the second is deleted.
        // It is deleted only when the logs were actually compared and no result
        // the caller already observed overrides the match; every other outcome
        // keeps both, as the failure path always has.
        let keep_golden_log_only = options.keep_logs
            && !failed
            && !options.match_overridden
            && compared_log_messages.is_some();
        if keep_golden_log_only {
            retain_verification_logs([(label1, log1)])?;
            let matched_log = log2.to_path_buf();
            log2.close().with_context(|| {
                format!(
                    "could not delete {label2}'s log {}, which matched {label1}'s",
                    matched_log.display()
                )
            })?;
            // Deliberately not in the `::   <label>: <path>` shape printed for a
            // retained log: the deleted file has no path left to report.
            eprintln!(
                ":: Deleted {label2}'s log: it matched {label1}'s, which is kept as the golden log"
            );
        } else if options.keep_logs || failed {
            if let Some(retention) = &options.failed_log_retention {
                eprintln!(":: Verification logs retained:");
                retain_failed_verification_logs([(label1, log1), (label2, log2)], retention)?;
            } else {
                retain_verification_logs([(label1, log1), (label2, log2)])?;
            }
        }
    }

    if failed {
        // A refused comparison is a NO-RESULT, not a divergence. A real
        // stdout/stderr/exit-status mismatch still outranks the refusal.
        let verdict = if comparison_refusal_reason.is_some() && !observed_divergence {
            Verdict::NoResult
        } else {
            Verdict::Diverged
        };
        let no_result_reason =
            (verdict == Verdict::NoResult).then(|| NoResultReason::ComparisonRefused {
                detail: comparison_refusal_reason
                    .clone()
                    .expect("a refused comparison retains its producer reason"),
            });

        // Divergence is a verification *verdict*, not an I/O error: return it as
        // a value carrying the guest exit status. Callers that want the
        // historical "divergence -> nonzero process exit" behavior use
        // `VerificationOutcome::into_exit_status`.
        Ok(VerificationOutcome {
            verdict,
            no_result_reason,
            refused_log_comparison: comparison_refusal_reason,
            guest_status: out2.status,
            comparison: spec,
            compared_log_messages,
            compared_outputs,
            dbt_counted_branches: None,
            runtime: None,
            first_divergent_scheduler_turn,
            first_divergent_record_positions,
            first_divergent_virtual_nanoseconds,
            first_divergent_record,
            first_divergent_syscall,
            first_divergent_left_message,
            first_divergent_right_message,
        })
    } else {
        Ok(VerificationOutcome {
            verdict: Verdict::Matched,
            no_result_reason: None,
            refused_log_comparison: None,
            guest_status: out2.status,
            comparison: spec,
            compared_log_messages,
            compared_outputs,
            dbt_counted_branches: None,
            runtime: None,
            first_divergent_scheduler_turn,
            first_divergent_record_positions,
            first_divergent_virtual_nanoseconds,
            first_divergent_record,
            first_divergent_syscall,
            first_divergent_left_message,
            first_divergent_right_message,
        })
    }
}

/// Name only relaxations in the comparison itself. Execution policy such as
/// time or metadata virtualization remains in the typed report fields; folding
/// it into this string would make `none` depend on an open-ended list of run
/// options instead of the comparator contract this evidence describes.
fn comparison_relaxations(comparison: &ComparisonSpec) -> Option<String> {
    let policy_shape_matches = match comparison.strictness {
        LogCompareStrictness::Stripped => {
            comparison.display_name == "Stripped"
                && comparison.strip_lines
                && !comparison.canonicalize_addresses
                && !comparison.full_trace
                && !comparison.exact_remainder
                && comparison.log_scope == ComparedLogScope::Deterministic
                && comparison.stripped_prefixes
                    == [STRIP_WALL_CLOCK_PREFIX_V1, STRIP_UNSAFE_NORMALIZATION_V1]
                && comparison.canonicalizations.is_empty()
        }
        LogCompareStrictness::Canonical => {
            !comparison.strip_lines
                && comparison.canonicalize_addresses
                && comparison.full_trace
                && comparison.exact_remainder
                && matches!(
                    (comparison.log_scope, comparison.display_name),
                    (ComparedLogScope::Info, "BitwiseInfoV1")
                        | (ComparedLogScope::FullTrace, "BitwiseFullTraceV1")
                )
                && comparison.stripped_prefixes == PARITY_STRIPPED_PREFIXES
                && comparison.canonicalizations == PARITY_CANONICALIZATIONS
        }
    };
    if !policy_shape_matches
        || !comparison.record_envelope.is_canonical()
        || comparison.ignore_lines
        || comparison.skip_commit
        || comparison.skip_detlog
    {
        return None;
    }

    let mut relaxations = Vec::new();
    if comparison.strictness == LogCompareStrictness::Stripped {
        relaxations.push(STRIP_UNSAFE_NORMALIZATION_V1);
    }
    if !comparison.compare_io_buffers {
        relaxations.push("--no-detlog-io-buffers");
    }
    Some(if relaxations.is_empty() {
        "none".to_owned()
    } else {
        relaxations.join(",")
    })
}

fn comparison_evidence_line(comparison: &ComparisonSpec) -> Option<String> {
    if !comparison.compare_logs {
        return None;
    }
    Some(match comparison_relaxations(comparison) {
        Some(relaxations) => format!(
            ":: comparison={} relaxations={relaxations}",
            comparison.display_name
        ),
        None => ":: comparison=unknown relaxations=unknown".to_owned(),
    })
}

/// Announce a verification verdict only after its machine-readable report has
/// been published.
///
/// Keeping this outside [`compare_two_runs`] closes the interval in which the
/// CLI previously printed `Success` and an outer bucket timeout could kill it
/// before `write_verification_json` replaced the pending `no_result` report.
pub fn announce_verification_outcome(
    outcome: &VerificationOutcome,
    second_run: SecondRun,
    success_message: &str,
    failure_message: &str,
) {
    // Console output is best-effort, as it was with `eprintln!`; the verdict
    // itself travels through the exit status and the published report.
    let _ = write_verification_announcement(
        &mut io::stderr().lock(),
        outcome,
        second_run,
        success_message,
        failure_message,
    );
}

/// What the second half of a verification was, which decides what a match
/// without virtual time does and does not show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecondRun {
    /// A second, independent execution of the guest (`hermit run --verify`
    /// and the DBT arm).
    Rerun,
    /// A replay of the first execution's recording
    /// (`hermit record start --verify`).
    Replay,
}

/// The phrase that claims bitwise parity on the console. It is printed only
/// when the published report's `bitwise_parity` is true, so no weaker
/// comparison can be read as that claim.
pub(crate) const BITWISE_PARITY_CLAIM: &str = "bitwise parity established";

/// Write the announcement for `outcome` to `out`.
///
/// On a match, the caller's success message is followed by a line that states
/// what the match rests on. The success message is kept byte-for-byte as the
/// callers pass it, because tests and harnesses in this repository and in the
/// parent workspace match `Success: deterministic. Determinism verified.` as a
/// substring and, in at least one place, as an exact line. That message is
/// printed for the lossy `Stripped` comparison of a plain `--verify` as well as
/// for the canonical comparison of `--verify-strict`, so on its own it cannot
/// tell a reader which one ran.
fn write_verification_announcement(
    out: &mut impl io::Write,
    outcome: &VerificationOutcome,
    second_run: SecondRun,
    success_message: &str,
    failure_message: &str,
) -> io::Result<()> {
    // Announce the comparison only after any requested machine-readable report
    // has been durably published. Output-only fallbacks did not compare an
    // internal log and therefore must not claim a comparison policy.
    if let Some(evidence) = comparison_evidence_line(&outcome.comparison) {
        writeln!(out, "{evidence}")?;
    }
    match outcome.verdict {
        Verdict::Matched => {
            writeln!(out, ":: {}", success_message.green().bold())?;
            writeln!(
                out,
                ":: {}",
                matched_comparison_qualifier(outcome, second_run)
            )?;
        }
        Verdict::Diverged => writeln!(out, ":: {}", failure_message.red().bold())?,
        Verdict::NoResult => writeln!(
            out,
            ":: {}",
            "No result: the comparison was refused, so this is neither a match nor a difference."
                .red()
                .bold()
        )?,
        Verdict::InfrastructureError => writeln!(
            out,
            ":: {}",
            "Infrastructure error: the result is neither a match nor a product difference."
                .red()
                .bold()
        )?,
    }
    Ok(())
}

/// Say, in plain words, what a `Matched` verdict compared.
///
/// The bitwise claim is derived from [`verification_report`], the same
/// predicate that sets `bitwise_parity` in `--verify-json`, so the console and
/// the machine-readable report cannot disagree. Stdout and stderr are always
/// compared byte for byte by [`compare_two_runs`]; the guest's log records are
/// compared only when `compare_logs` is set, under the spec's strictness and
/// only within its `log_scope`. When time was not virtualized, the line also
/// says what that leaves unshown, which depends on `second_run`.
fn matched_comparison_qualifier(outcome: &VerificationOutcome, second_run: SecondRun) -> String {
    let comparison = &outcome.comparison;
    let scope = match comparison.log_scope {
        ComparedLogScope::Deterministic => {
            "only the DETLOG and scheduler COMMIT records of the guest's log"
        }
        ComparedLogScope::Info => "the INFO records of the guest's log",
        ComparedLogScope::FullTrace => "every record of the guest's log",
    };
    let records = outcome.compared_log_messages.map_or_else(
        || scope.to_owned(),
        |counts| format!("{scope} ({} vs {})", counts.left, counts.right),
    );
    let compared = if verification_report(outcome).bitwise_parity {
        format!(
            "Compared: stdout and stderr byte for byte, and {records} exactly apart from \
             the wall-clock prefix and canonicalized addresses, under {}: \
             {BITWISE_PARITY_CLAIM}.",
            comparison.display_name
        )
    } else if !comparison.compare_logs {
        "Compared: stdout and stderr only. The guest's log records were not compared, \
         so this is not a bitwise comparison."
            .to_owned()
    } else if comparison.strictness == LogCompareStrictness::Stripped {
        format!(
            "Compared: stdout and stderr byte for byte, and {records} under the lossy \
             Stripped comparison, which erases numbers, addresses, paths and timestamps \
             before comparing. This is not a bitwise comparison; add --verify-strict \
             for one."
        )
    } else {
        format!(
            "Compared: stdout and stderr byte for byte, and {records} under {}, but \
             without all the evidence the parity contract requires (see \
             `bitwise_parity` in --verify-json). This is not a bitwise comparison.",
            comparison.display_name
        )
    };
    match (comparison.virtualize_time, second_run) {
        (true, _) => compared,
        (false, SecondRun::Replay) => format!("{compared} {TIME_NOT_VIRTUALIZED_NOTE}"),
        (false, SecondRun::Rerun) => format!("{compared} {RERUN_TIME_NOT_VIRTUALIZED_NOTE}"),
    }
}

/// Appended to a replay's match when time was not virtualized, which is what
/// `hermit record start --verify` always does (see
/// [`ComparisonSpec::virtualize_time`]). Without it, a record-mode
/// `bitwise parity established` would read as a determinism result.
pub(crate) const TIME_NOT_VIRTUALIZED_NOTE: &str = "Time was not virtualized, so this shows \
    that the second run reproduced the first, not that separate runs of the guest agree.";

/// Appended to a rerun's match when time was not virtualized, for example
/// `hermit run --no-virtualize-time --verify`. Both runs are independent
/// executions, so the match is real, but the guest read the host's clock.
pub(crate) const RERUN_TIME_NOT_VIRTUALIZED_NOTE: &str = "Time was not virtualized: the guest \
    read the host's clock, so these two runs matched, but a guest whose output depends on the \
    time can differ from one run to the next.";

fn display_diff(left: &str, right: &str) {
    for result in diff::lines(left, right) {
        match result {
            diff::Result::Left(s) => {
                eprintln!("- {}", s.red());
            }
            diff::Result::Right(s) => {
                eprintln!("+ {}", s.green());
            }
            diff::Result::Both(s, _) => {
                eprintln!("  {}", s);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// A report larger than a page reaches a one-page pipe that has room for
    /// its first lines. Written as one write it put nothing there: 3600 bytes
    /// waiting plus the 1004 bytes past the report's whole page do not fit in
    /// the page, and a one-page pipe has no empty page for the rest.
    #[test]
    fn signal_reports_fill_a_pipe_that_cannot_take_them_whole() {
        let mut fds = [0; 2];
        // SAFETY: `fds` has room for the two descriptors.
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        let (read_end, write_end) = (fds[0], fds[1]);
        // SAFETY: F_SETPIPE_SZ only resizes the pipe.
        assert_eq!(
            unsafe { libc::fcntl(write_end, libc::F_SETPIPE_SZ, 4096) },
            4096
        );
        let waiting = vec![b'.'; 3600];
        // SAFETY: `waiting` is valid for its length.
        let wrote = unsafe { libc::write(write_end, waiting.as_ptr().cast(), waiting.len()) };
        assert_eq!(wrote, 3600);

        let heading = format!(
            "{:<99}\n",
            ":: The run 1 guest was terminated by signal 10 (SIGUSR1)."
        );
        let record = format!("{:<99}\n", "record");
        let report = format!("{heading}{}", record.repeat(50));
        assert_eq!(report.len() % 4096, 1004);
        write_signal_reports(write_end, &[report]);
        // SAFETY: closes only the pipe's write end, opened above.
        unsafe { libc::close(write_end) };

        let mut delivered = Vec::new();
        let mut buffer = [0u8; 8192];
        loop {
            // SAFETY: `buffer` is valid for its length.
            let read = unsafe { libc::read(read_end, buffer.as_mut_ptr().cast(), buffer.len()) };
            assert!(read >= 0, "{}", std::io::Error::last_os_error());
            if read == 0 {
                break;
            }
            delivered.extend_from_slice(&buffer[..read as usize]);
        }
        // SAFETY: closes only the pipe's read end, opened above.
        unsafe { libc::close(read_end) };
        // The four whole lines that fit: a line is never split across the
        // page's end, because the part past a whole page must fit whole.
        assert_eq!(delivered.len(), 4000);
        assert_eq!(&delivered[3600..3700], heading.as_bytes());
    }

    #[test]
    fn signal_termination_report_shows_the_last_detlog_records() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("run1_log");
        let mut text = String::new();
        for record in 0..25 {
            text.push_str(&format!(
                "2026-10-07T00:00:00.000000Z INFO detcore: [dtid 3] DETLOG [syscall] record {record}{}{{\"schema\":1}}\n",
                detcore::detlog::RECORD_SEPARATOR
            ));
            text.push_str("2026-10-07T00:00:00.000000Z INFO detcore::scheduler: COMMIT turn\n");
            text.push_str(
                "2026-10-07T00:00:00.000000Z DEBUG detcore: DETLOG (post) registers [dtid 3]\n",
            );
        }
        fs::write(&log, text).unwrap();

        let status = ExitStatus::Signaled(reverie::process::Signal::SIGSEGV, true);
        let report = signal_termination_report("run 1", status, &log).unwrap();
        let lines: Vec<&str> = report.lines().collect();
        assert_eq!(
            lines[0],
            format!(
                ":: The run 1 guest was terminated by signal 11 (SIGSEGV, core dumped). \
                 Its last 20 INFO DETLOG records, from {}:",
                log.display()
            )
        );
        assert_eq!(lines.len(), 21, "{report}");
        assert!(lines[1].ends_with("DETLOG [syscall] record 5"), "{report}");
        assert!(
            lines[20].ends_with("DETLOG [syscall] record 24"),
            "{report}"
        );
        assert!(!report.contains("COMMIT") && !report.contains("DETLOG_RECORD"));
        assert!(!report.contains("registers"), "{report}");
    }

    #[test]
    fn signal_termination_report_is_only_for_a_signal() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("run1_log");
        fs::write(&log, "INFO detcore: DETLOG [syscall] exit_group\n").unwrap();
        for code in [0, 1, 139] {
            assert_eq!(
                signal_termination_report("run 1", ExitStatus::Exited(code), &log),
                None
            );
        }
    }

    #[test]
    fn signal_termination_report_names_a_log_it_cannot_use() {
        let dir = tempfile::tempdir().unwrap();
        let status = ExitStatus::Signaled(reverie::process::Signal::SIGKILL, false);

        let missing = dir.path().join("absent");
        let report = signal_termination_report("run 2", status, &missing).unwrap();
        assert!(
            report.starts_with(":: The run 2 guest was terminated by signal 9 (SIGKILL). Its log ")
                && report.contains("could not be read"),
            "{report}"
        );

        let empty = dir.path().join("empty");
        fs::write(&empty, "INFO detcore::scheduler: COMMIT turn\n").unwrap();
        let report = signal_termination_report("run 2", status, &empty).unwrap();
        assert!(report.contains("hold no INFO DETLOG record"), "{report}");
    }

    #[test]
    fn signal_termination_report_keeps_a_separator_inside_the_human_text() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("run1_log");
        // A pathname may hold the separator's characters; only the last
        // separator starts the machine-readable suffix.
        let human = format!(
            "INFO detcore: DETLOG [syscall] openat(AT_FDCWD, \"/tmp/p{}guest\", O_RDONLY) = Ok(3)",
            detcore::detlog::RECORD_SEPARATOR
        );
        fs::write(
            &log,
            format!(
                "{human}{}{{\"schema\":1}}\n",
                detcore::detlog::RECORD_SEPARATOR
            ),
        )
        .unwrap();
        let status = ExitStatus::Signaled(reverie::process::Signal::SIGSEGV, false);
        let report = signal_termination_report("run 1", status, &log).unwrap();
        assert_eq!(
            report.lines().nth(1),
            Some(format!("::   {human}").as_str())
        );
    }

    #[test]
    fn signal_termination_report_cuts_long_records_on_a_character() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("run1_log");
        // "é" is two bytes, so the cut falls inside one unless it is moved.
        let line = format!(
            "INFO detcore: DETLOG x{}",
            "é".repeat(SIGNAL_REPORT_LINE_BYTES)
        );
        fs::write(&log, format!("{line}\n")).unwrap();
        let status = ExitStatus::Signaled(reverie::process::Signal::SIGABRT, false);
        let report = signal_termination_report("run 1", status, &log).unwrap();
        let shown = report.lines().nth(1).unwrap();
        assert!(shown.ends_with(" [cut]"), "{shown}");
        assert!(shown.len() <= "::   ".len() + SIGNAL_REPORT_LINE_BYTES + " [cut]".len());
    }

    #[test]
    fn a_log_tail_starts_at_a_whole_line() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        fs::write(&log, "first line\nsecond line\nthird\n").unwrap();
        // 29 bytes; "second line" starts 18 bytes from the end.
        assert_eq!(read_log_tail(&log, 15).unwrap(), "third\n");
        assert_eq!(read_log_tail(&log, 17).unwrap(), "third\n");
        assert_eq!(read_log_tail(&log, 18).unwrap(), "second line\nthird\n");
        assert_eq!(read_log_tail(&log, 19).unwrap(), "second line\nthird\n");
        assert_eq!(
            read_log_tail(&log, 1 << 20).unwrap(),
            "first line\nsecond line\nthird\n"
        );
    }

    #[test]
    fn a_recorded_branch_counter_verdict_is_stamped_on_every_report_once() {
        let recorded = BranchCounterVerdict {
            present: true,
            evidence: "fixture CPU: Reverie performance-counter validation passed".into(),
        };
        // Nothing recorded (another backend): the report is written as built.
        let report = VerificationReport::no_result();
        assert_eq!(with_exact_branch_counter(&report, None), report);
        // Recorded: every report gains it, the pending no-result stamp included.
        let stamped = with_exact_branch_counter(&report, Some(&recorded));
        assert_eq!(stamped.exact_branch_counter.as_ref(), Some(&recorded));
        // A report that already carries a verdict keeps its own.
        let own = BranchCounterVerdict {
            present: false,
            evidence: "fixture: own verdict".into(),
        };
        let mut carrying = VerificationReport::no_result();
        carrying.exact_branch_counter = Some(own.clone());
        assert_eq!(
            with_exact_branch_counter(&carrying, Some(&recorded)).exact_branch_counter,
            Some(own)
        );
    }

    fn classified_container<T: serde::Serialize + serde::de::DeserializeOwned>(
        mut child: impl FnMut() -> Result<T, Error>,
    ) -> Result<T, Error> {
        // Exercise the real result/exit transport and production classifier.
        // These controls do not need the CLI's child panic-hook installation.
        super::super::container::classify_container_result(
            reverie::process::Container::new()
                .run(|| child().map_err(hermit::SerializableError::from)),
        )
    }

    fn real_container_exit(path: &Path, run: VerificationRun, code: Option<i32>) -> Error {
        run_verification_execution::<()>(Some(path), run, || {
            classified_container(|| {
                // Only the freshly created container child changes disposition
                // or exits. The fallback code makes a blocked/failed signal a
                // different result, so setup failure cannot satisfy the test.
                unsafe {
                    if let Some(code) = code {
                        libc::_exit(code);
                    }
                    let mut mask = std::mem::zeroed();
                    assert_eq!(libc::sigemptyset(&mut mask), 0);
                    assert_eq!(libc::sigaddset(&mut mask, libc::SIGALRM), 0);
                    assert_eq!(
                        libc::sigprocmask(libc::SIG_UNBLOCK, &mask, std::ptr::null_mut()),
                        0
                    );
                    assert_ne!(libc::signal(libc::SIGALRM, libc::SIG_DFL), libc::SIG_ERR);
                    libc::raise(libc::SIGALRM);
                    libc::_exit(99);
                }
            })
        })
        .unwrap_err()
    }

    fn check_real_container_failure(run: VerificationRun) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("verify.json");
        write_pending_verification_json(&path).unwrap();
        if run == VerificationRun::Run2 {
            let first = run_verification_execution(Some(&path), VerificationRun::Run1, || {
                classified_container(|| Ok(output(0, b"first run", b"")))
            })
            .unwrap();
            assert_eq!(first.stdout, b"first run");
            assert_eq!(first.status, ExitStatus::Exited(0));
            assert_eq!(
                VerificationReport::from_current_json_slice(&fs::read(&path).unwrap()).unwrap(),
                VerificationReport::no_result()
            );
        }
        let error = real_container_exit(&path, run, None);
        let actual = error
            .downcast_ref::<super::super::container::ContainerChildExit>()
            .expect("real child must die by SIGALRM, not refuse setup");
        assert_eq!(
            actual.0,
            ExitStatus::Signaled(reverie::process::Signal::SIGALRM, false)
        );
        assert_eq!(super::super::failure_exit_code(&error), 125);
        let report = VerificationReport::from_current_json_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(
            report.no_result_reason,
            Some(NoResultReason::ContainerFailed(ContainerFailure {
                run,
                disposition: ContainerDisposition::Signaled {
                    signal: libc::SIGALRM,
                    core_dumped: false
                }
            }))
        );
        assert!(report.guest_exit_code.is_none() && report.guest_signal.is_none());
        assert!(report.comparison.is_none() && report.compared_outputs.is_none());
        assert!(report.require_canonical_comparison().is_err());
    }

    #[test]
    fn real_container_failure_replaces_run1_stamp() {
        check_real_container_failure(VerificationRun::Run1);
    }

    #[test]
    fn real_container_failure_replaces_run2_stamp_without_claiming_run1_status() {
        check_real_container_failure(VerificationRun::Run2);
    }

    #[test]
    fn real_container_zero_exit_and_deliberate_stops_remain_distinct() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("verify.json");
        for code in [
            0,
            101,
            detcore_model::HERMIT_POLICY_REFUSAL_EXIT,
            hermit::HERMIT_DEADLINE_EXIT,
            hermit::HERMIT_LOG_CAP_EXIT,
            detcore_model::signal_exit_status(libc::SIGTERM),
        ] {
            write_pending_verification_json(&path).unwrap();
            let error = real_container_exit(&path, VerificationRun::Run1, Some(code));
            let report =
                VerificationReport::from_current_json_slice(&fs::read(&path).unwrap()).unwrap();
            match code {
                0 | 101 => {
                    assert!(
                        error
                            .downcast_ref::<super::super::container::ContainerChildExit>()
                            .is_some()
                    );
                    assert_eq!(
                        report.no_result_reason,
                        Some(NoResultReason::ContainerFailed(ContainerFailure {
                            run: VerificationRun::Run1,
                            disposition: ContainerDisposition::Exited { code }
                        }))
                    );
                    assert_eq!(super::super::failure_exit_code(&error), 125);
                }
                122 => {
                    assert!(
                        error
                            .downcast_ref::<super::super::container::PolicyRefusal>()
                            .is_some()
                    );
                    assert_eq!(report, VerificationReport::no_result());
                }
                124 => {
                    assert!(
                        error
                            .downcast_ref::<super::super::container::RunTimeoutMarker>()
                            .is_some()
                    );
                    assert_eq!(report, VerificationReport::no_result());
                }
                123 => {
                    assert!(
                        error
                            .downcast_ref::<super::super::container::LogCapExceeded>()
                            .is_some()
                    );
                    assert_eq!(report, VerificationReport::no_result());
                    assert_eq!(
                        super::super::failure_exit_code(&error),
                        hermit::HERMIT_LOG_CAP_EXIT
                    );
                }
                _ => {
                    assert!(
                        error
                            .downcast_ref::<super::super::container::SignalDeath>()
                            .is_some()
                    );
                    assert_eq!(report, VerificationReport::no_result());
                }
            }
        }
    }

    #[test]
    fn a_typed_run_failure_replaces_the_stamp_and_an_untyped_error_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("verify.json");
        let cases: Vec<(Error, Option<RunFailure>)> = vec![
            (
                Error::new(hermit::BackendRunFailure)
                    .context("drrun started and the DBT run then failed: missing FINAL frames"),
                Some(RunFailure::BackendFailure),
            ),
            (
                Error::new(super::super::run::DeterminismLossRecorded::new(
                    "run 1",
                    "process 3 exited without deregistering",
                )),
                Some(RunFailure::DeterminismLoss),
            ),
            (
                Error::msg("the guest program is statically linked")
                    .context(super::super::container::PolicyRefusal),
                Some(RunFailure::PolicyRefusal),
            ),
            // Already published as its infrastructure error.
            (
                Error::new(hermit::SkidOvershootError::new(2))
                    .context(super::super::container::PolicyRefusal),
                None,
            ),
            (Error::msg("unknown flag --frobnicate"), None),
        ];
        // The type decides, never the text: the same message without its type
        // leaves the stamp.
        for (error, _) in &cases {
            write_pending_verification_json(&path).unwrap();
            drop(publish_run_failure(
                Some(&path),
                VerificationRun::Run1,
                Error::msg(format!("{error:#}")),
            ));
            assert_eq!(
                VerificationReport::from_current_json_slice(&fs::read(&path).unwrap()).unwrap(),
                VerificationReport::no_result()
            );
        }
        for (error, expected) in cases {
            write_pending_verification_json(&path).unwrap();
            let message = format!("{error:#}");
            let returned = publish_run_failure(Some(&path), VerificationRun::Run2, error);
            // The caller's error comes back unchanged, typed as it was.
            assert_eq!(format!("{returned:#}"), message);
            let report =
                VerificationReport::from_current_json_slice(&fs::read(&path).unwrap()).unwrap();
            match expected {
                Some(failure) => assert_eq!(
                    report.no_result_reason,
                    Some(NoResultReason::RunFailed {
                        run: VerificationRun::Run2,
                        failure,
                        detail: message,
                    })
                ),
                None => assert_eq!(report, VerificationReport::no_result()),
            }
        }
    }

    #[test]
    fn returned_guest_signal_and_unknown_error_do_not_become_container_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("verify.json");
        write_pending_verification_json(&path).unwrap();
        let expected = Output {
            status: ExitStatus::Signaled(reverie::process::Signal::SIGALRM, false),
            stdout: Vec::new(),
            stderr: b"guest".to_vec(),
        };
        let returned = run_verification_execution(Some(&path), VerificationRun::Run1, || {
            classified_container(|| Ok(expected.clone()))
        })
        .unwrap();
        assert_eq!(returned.status, expected.status);
        assert_eq!(returned.stderr, expected.stderr);
        let error = run_verification_execution::<()>(Some(&path), VerificationRun::Run1, || {
            classified_container(|| Err(Error::msg("ordinary unknown error")))
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "ordinary unknown error");
        let panic = run_verification_execution::<()>(Some(&path), VerificationRun::Run1, || {
            super::super::container::classify_container_result(Ok(Err(
                hermit::SerializableError::from(Error::msg("caught panic control")).into_panic(),
            )))
        })
        .unwrap_err();
        assert!(
            panic
                .downcast_ref::<super::super::container::ContainerChildPanic>()
                .is_some()
        );
        assert_eq!(
            VerificationReport::from_current_json_slice(&fs::read(path).unwrap()).unwrap(),
            VerificationReport::no_result()
        );
    }

    #[test]
    fn container_failure_keeps_primary_type_and_secondary_errors() {
        let dir = tempfile::tempdir().unwrap();
        let error = real_container_exit(
            &dir.path().join("absent/verify.json"),
            VerificationRun::Run1,
            Some(101),
        );
        assert!(
            error
                .downcast_ref::<super::super::container::ContainerChildExit>()
                .is_some()
        );
        let context = error.downcast_ref::<VerificationFailureContext>().unwrap();
        assert_eq!(context.errors.len(), 1);
        assert_eq!(context.errors[0].0, "publishing container failure");
        assert!(
            context.errors[0]
                .1
                .chain()
                .any(|cause| cause.downcast_ref::<std::io::Error>().is_some())
        );
        let log = tempfile::NamedTempFile::new_in(dir.path())
            .unwrap()
            .into_temp_path();
        fs::write(&log, b"retained failure log").unwrap();
        let retained = log.to_path_buf();
        let error = retain_logs_after_verification_error(error, [("run 1", log)]);
        assert_eq!(fs::read(retained).unwrap(), b"retained failure log");
        // Exercise the error-composition boundary separately: retaining a
        // TempPath can fail only on platform-specific filesystem operations.
        // This control does not claim an actual retention I/O failure occurred.
        let secondary = Error::new(std::io::Error::from_raw_os_error(libc::EACCES));
        let error = retain_verification_error(error, "retaining verification log", secondary);
        assert!(
            error
                .downcast_ref::<super::super::container::ContainerChildExit>()
                .is_some()
        );
        assert_eq!(super::super::failure_exit_code(&error), 125);
        assert!(format!("{error:#}").contains("publishing container failure"));
        let context = error.downcast_ref::<VerificationFailureContext>().unwrap();
        assert_eq!(context.errors.len(), 2);
        assert_eq!(context.errors[0].0, "publishing container failure");
        assert_eq!(context.errors[1].0, "retaining verification log");
        assert_eq!(
            context.errors[1]
                .1
                .downcast_ref::<std::io::Error>()
                .unwrap()
                .raw_os_error(),
            Some(libc::EACCES)
        );

        // A secondary retention error must not replace a deliberate stop's
        // original exit class. This exercises error composition, not a failed
        // filesystem retention operation.
        for (primary, expected_exit) in [
            (
                Error::new(super::super::container::PolicyRefusal),
                detcore_model::HERMIT_POLICY_REFUSAL_EXIT,
            ),
            (
                Error::new(super::super::container::RunTimeoutMarker),
                hermit::HERMIT_DEADLINE_EXIT,
            ),
        ] {
            let error = retain_verification_error(
                primary,
                "retaining verification log",
                Error::new(std::io::Error::from_raw_os_error(libc::EACCES)),
            );
            assert_eq!(super::super::failure_exit_code(&error), expected_exit);
            let context = error.downcast_ref::<VerificationFailureContext>().unwrap();
            assert_eq!(context.errors.len(), 1);
            assert_eq!(context.errors[0].0, "retaining verification log");
            assert_eq!(
                context.errors[0]
                    .1
                    .downcast_ref::<std::io::Error>()
                    .unwrap()
                    .raw_os_error(),
                Some(libc::EACCES)
            );
        }
    }

    /// How the child of [`retention_report_in_child`], which reported its kept
    /// verification log, ended.
    #[derive(Debug, PartialEq, Eq)]
    enum RetentionChild {
        /// The report returned, the error kept its exit class and the log
        /// was kept.
        Returned,
        /// The report panicked. In hermit that is exit 101, not the
        /// primary error's status.
        Panicked,
        /// The report was still running after [`RETENTION_CHILD_BOUND`], and
        /// the child was killed.
        StillRunning,
        /// Any other end, as the raw wait status.
        Other(libc::c_int),
    }

    /// Far longer than a report that does not wait takes on a loaded host. A
    /// report blocked on a full pipe never ends by itself, so waiting longer
    /// would show nothing more.
    const RETENTION_CHILD_BOUND: std::time::Duration = std::time::Duration::from_secs(10);

    /// Set in the re-executed child of [`retention_report_in_child`]: the
    /// primary error, "log cap" or "timeout".
    const RETENTION_CHILD_PRIMARY: &str = "HERMIT_TEST_RETENTION_CHILD_PRIMARY";

    /// Set with [`RETENTION_CHILD_PRIMARY`]: the directory for the child's log.
    const RETENTION_CHILD_DIR: &str = "HERMIT_TEST_RETENTION_CHILD_DIR";

    /// The child's exit code when the report returned, the error kept its
    /// exit class and the log was kept. It is not 0, which is what the test
    /// binary exits with when its name filter matched no test.
    const RETENTION_CHILD_RETURNED: libc::c_int = 10;

    /// The child's exit code when an `eprint!` did not reach its descriptor
    /// 2, so that a blocking or panicking `eprintln!` could not be seen. The
    /// child runs with `--nocapture`, so this would mean libtest captured
    /// its output anyway; the test then fails rather than pass without
    /// testing anything.
    const RETENTION_CHILD_OUTPUT_CAPTURED: libc::c_int = 5;

    /// The first statement of each test that runs [`retention_report_in_child`].
    /// In that child, the variables above are set: report, then exit with
    /// the result instead of returning into the test harness. Elsewhere,
    /// return.
    fn retention_child_entry() {
        let (Ok(primary), Some(dir)) = (
            std::env::var(RETENTION_CHILD_PRIMARY),
            std::env::var_os(RETENTION_CHILD_DIR),
        ) else {
            return;
        };
        let primary = match primary.as_str() {
            "log cap" => log_cap_error,
            "timeout" => timeout_error,
            other => panic!("unknown primary error {other:?}"),
        };
        let code = retention_report_child_body(primary, Path::new(&dir));
        // SAFETY: _exit has no preconditions.
        unsafe { libc::_exit(code) }
    }

    /// Run the test named `test` in a new process of this test binary, with
    /// descriptor 2 a copy of `stderr`, the write end of a pipe. That test
    /// starts with [`retention_child_entry`], so the child calls
    /// `retain_logs_after_verification_error` with the `primary` error for
    /// one log it creates in `dir`. Wait for it for at most
    /// [`RETENTION_CHILD_BOUND`].
    ///
    /// The child runs with `--nocapture`. A forked child would inherit
    /// libtest's output capture when this test runs under `cargo test`
    /// without `--nocapture`, and its `eprint!` would then never reach its
    /// descriptor 2.
    ///
    /// The child keeps SIGPIPE ignored, as hermit's outer process does: the
    /// Rust runtime ignores it at startup and hermit never restores it. A
    /// write to a pipe without a reader therefore fails with EPIPE, which
    /// `eprintln!` turns into a panic. Unit tests never enable the exit
    /// bound that `main` arms for a capped run
    /// (`tracing::bound_log_cap_exit`), so what this observes is the report
    /// itself, not that bound covering for it.
    fn retention_report_in_child(
        test: &str,
        primary: &str,
        stderr: &std::os::fd::OwnedFd,
        dir: &Path,
    ) -> RetentionChild {
        use std::os::unix::process::ExitStatusExt as _;
        // The Command, and with it this process's copy of `stderr`, is
        // dropped at the end of this statement.
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([test, "--exact", "--nocapture", "--test-threads=1"])
            .env(RETENTION_CHILD_PRIMARY, primary)
            .env(RETENTION_CHILD_DIR, dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(stderr.try_clone().unwrap())
            .spawn()
            .unwrap();
        let start = std::time::Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if start.elapsed() >= RETENTION_CHILD_BOUND {
                child.kill().unwrap();
                child.wait().unwrap();
                return RetentionChild::StillRunning;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert_ne!(
            status.code(),
            Some(RETENTION_CHILD_OUTPUT_CAPTURED),
            "eprint! in the child did not reach its descriptor 2"
        );
        match status.code() {
            Some(RETENTION_CHILD_RETURNED) => RetentionChild::Returned,
            Some(101) => RetentionChild::Panicked,
            _ => RetentionChild::Other(status.into_raw()),
        }
    }

    /// The body of the child of [`retention_report_in_child`]; returns its
    /// exit code.
    fn retention_report_child_body(primary: fn() -> Error, dir: &Path) -> i32 {
        // SAFETY: the calls below only change this process's SIGPIPE
        // disposition and descriptors, and read into a local buffer. The
        // test harness's other thread is waiting for this test and writes
        // nothing to descriptor 2 meanwhile.
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_IGN);
            let stderr = libc::fcntl(libc::STDERR_FILENO, libc::F_DUPFD_CLOEXEC, 3);
            let mut probe = [0; 2];
            if stderr < 0
                || libc::pipe2(probe.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) != 0
                || libc::dup2(probe[1], libc::STDERR_FILENO) < 0
            {
                return 6;
            }
            eprint!("probe");
            let mut seen = [0u8; 8];
            if libc::read(probe[0], seen.as_mut_ptr().cast(), seen.len()) != 5
                || &seen[..5] != b"probe"
            {
                return RETENTION_CHILD_OUTPUT_CAPTURED;
            }
            if libc::dup2(stderr, libc::STDERR_FILENO) < 0 {
                return 6;
            }
        }
        let Ok(log) = tempfile::NamedTempFile::new_in(dir) else {
            return 6;
        };
        let log = log.into_temp_path();
        let path = log.to_path_buf();
        let class = super::super::failure_exit_code(&primary());
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            retain_logs_after_verification_error(primary(), [("run 1", log)])
        })) {
            Err(_) => 101,
            Ok(error) if super::super::failure_exit_code(&error) != class => 3,
            // A TempPath that was not kept deletes its file when dropped.
            Ok(_) if !path.exists() => 4,
            Ok(_) => RETENTION_CHILD_RETURNED,
        }
    }

    fn retention_stderr_pipe() -> (std::os::fd::OwnedFd, std::os::fd::OwnedFd) {
        use std::os::fd::FromRawFd;
        let mut fds = [0; 2];
        // SAFETY: pipe2 writes two new descriptors into `fds`.
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        // SAFETY: both descriptors were just created and are owned only here.
        unsafe {
            (
                std::os::fd::OwnedFd::from_raw_fd(fds[0]),
                std::os::fd::OwnedFd::from_raw_fd(fds[1]),
            )
        }
    }

    fn files_in(dir: &Path) -> Vec<PathBuf> {
        fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect()
    }

    fn log_cap_error() -> Error {
        Error::new(super::super::container::LogCapExceeded)
    }

    fn timeout_error() -> Error {
        Error::new(super::super::container::RunTimeoutMarker)
    }

    /// Round-3 review of https://github.com/rrnewton/hermit/pull/3686, finding
    /// 4: after the log cap ends run 1 or run 2, `--verify --keep-logs`
    /// reports the logs it kept. Through `eprintln!`, that report panicked
    /// when stderr was a pipe whose reader had gone, so hermit exited 101
    /// instead of 123. The report is now tried once without waiting and is
    /// omitted when it cannot be delivered; the log is still kept.
    #[test]
    fn log_cap_retention_report_survives_a_stderr_without_a_reader() {
        retention_child_entry();
        let dir = tempfile::tempdir().unwrap();
        let (reader, writer) = retention_stderr_pipe();
        drop(reader);
        assert_eq!(
            retention_report_in_child(
                "verify::tests::log_cap_retention_report_survives_a_stderr_without_a_reader",
                "log cap",
                &writer,
                dir.path()
            ),
            RetentionChild::Returned
        );
        assert_eq!(files_in(dir.path()).len(), 1, "the log was not kept");
    }

    /// The same finding with stderr a full pipe that its reader never
    /// drains: the `eprintln!` report waited for it forever.
    #[test]
    fn log_cap_retention_report_does_not_wait_on_a_full_stderr_pipe() {
        retention_child_entry();
        let dir = tempfile::tempdir().unwrap();
        let (reader, writer) = retention_stderr_pipe();
        // Fill the pipe in non-blocking mode, then make the write end
        // blocking again, as an inherited stderr is.
        // SAFETY: fcntl and write act only on this test's own descriptor,
        // and write reads only the chunk it is given.
        unsafe {
            assert_eq!(
                libc::fcntl(writer.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK),
                0
            );
            for chunk in [&[b'x'; 4096][..], &[b'x'; 1][..]] {
                while libc::write(writer.as_raw_fd(), chunk.as_ptr().cast(), chunk.len()) > 0 {}
                assert_eq!(
                    io::Error::last_os_error().raw_os_error(),
                    Some(libc::EAGAIN)
                );
            }
            assert_eq!(libc::fcntl(writer.as_raw_fd(), libc::F_SETFL, 0), 0);
        }
        assert_eq!(
            retention_report_in_child(
                "verify::tests::log_cap_retention_report_does_not_wait_on_a_full_stderr_pipe",
                "log cap",
                &writer,
                dir.path()
            ),
            RetentionChild::Returned
        );
        drop(reader);
        assert_eq!(files_in(dir.path()).len(), 1, "the log was not kept");
    }

    /// Where stderr can take the report at once, the report reads exactly
    /// as it did before the fix above, for the cap and for every other
    /// failure class (a timeout stands for those here).
    #[test]
    fn retention_report_reads_as_before_when_stderr_has_room() {
        use std::io::Read as _;
        retention_child_entry();
        for class in ["log cap", "timeout"] {
            let dir = tempfile::tempdir().unwrap();
            let (reader, writer) = retention_stderr_pipe();
            assert_eq!(
                retention_report_in_child(
                    "verify::tests::retention_report_reads_as_before_when_stderr_has_room",
                    class,
                    &writer,
                    dir.path()
                ),
                RetentionChild::Returned,
                "{class}"
            );
            drop(writer);
            let mut report = String::new();
            File::from(reader).read_to_string(&mut report).unwrap();
            let kept = files_in(dir.path());
            assert_eq!(kept.len(), 1, "{class}: the log was not kept");
            assert_eq!(
                report,
                format!(
                    ":: Verification logs retained:\n::   run 1: {}\n",
                    kept[0].display()
                ),
                "{class}"
            );
        }
    }

    fn output(status: i32, stdout: &[u8], stderr: &[u8]) -> Output {
        Output {
            status: ExitStatus::Exited(status),
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        }
    }

    #[test]
    fn compared_guest_output_uses_the_common_agreement_policy() {
        let first = output(0, b"stdout\0without newline", b"stderr\xff");
        for verified in [true, false] {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            // The verdict/status need not match for these output bytes to be
            // unambiguous. A log-only divergence still emits them once.
            let second = output(23, &first.stdout, &first.stderr);
            emit_compared_guest_output_to(verified, &first, &second, &mut stdout, &mut stderr)
                .unwrap();
            assert_eq!(stdout, first.stdout);
            assert_eq!(stderr, first.stderr);
        }
        for second in [
            output(0, b"different stdout", &first.stderr),
            output(0, &first.stdout, b"different stderr"),
        ] {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            emit_compared_guest_output_to(false, &first, &second, &mut stdout, &mut stderr)
                .unwrap();
            assert!(stdout.is_empty());
            assert!(stderr.is_empty());
        }
    }

    #[test]
    fn compared_guest_output_preserves_write_error_order() {
        struct FailAfterByte {
            bytes: Vec<u8>,
            errno: i32,
        }
        impl io::Write for FailAfterByte {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if bytes.is_empty() {
                    return Ok(0);
                }
                if self.bytes.is_empty() {
                    self.bytes.push(bytes[0]);
                    Ok(1)
                } else {
                    Err(io::Error::from_raw_os_error(self.errno))
                }
            }

            fn flush(&mut self) -> io::Result<()> {
                panic!("the common output policy does not add a flush")
            }
        }
        let first = output(0, b"out", b"err");
        let mut stdout = FailAfterByte {
            bytes: Vec::new(),
            errno: libc::EPIPE,
        };
        let mut stderr = Vec::new();
        let error = emit_compared_guest_output_to(false, &first, &first, &mut stdout, &mut stderr)
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EPIPE));
        assert_eq!(stdout.bytes, b"o");
        assert!(stderr.is_empty(), "stdout failure must stop before stderr");

        let mut stdout = Vec::new();
        let mut stderr = FailAfterByte {
            bytes: Vec::new(),
            errno: libc::ENOSPC,
        };
        let error = emit_compared_guest_output_to(true, &first, &first, &mut stdout, &mut stderr)
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::ENOSPC));
        assert_eq!(stdout, first.stdout);
        assert_eq!(stderr.bytes, b"e");
    }

    fn empty_logs() -> (TempPath, TempPath) {
        let (left, right) = temp_log_files("verify_left", "verify_right").unwrap();
        (left.into_temp_path(), right.into_temp_path())
    }

    /// Two logs carrying identical, NONEMPTY comparable content. Distinct from
    /// [`empty_logs`]: an empty-vs-empty comparison is a no-result, so any test
    /// asserting a *parity* green must use this.
    fn logs_with_identical_detlog() -> (TempPath, TempPath) {
        let (left, right) = temp_log_files("verify_left", "verify_right").unwrap();
        let left_path = left.into_temp_path();
        let right_path = right.into_temp_path();
        let body = format!("{}{}", detlog_with_value(1), detlog_with_value(2));
        fs::write(&left_path, body.as_bytes()).unwrap();
        fs::write(&right_path, body.as_bytes()).unwrap();
        (left_path, right_path)
    }

    fn global_with_log(log: Option<LevelFilter>) -> GlobalOpts {
        GlobalOpts {
            log,
            log_file: None,
            max_log_bytes: None,
            log_budget: None,
            log_file_handle: None,
            unsafe_ignore_host_seccomp: false,
            backend: None,
        }
    }

    #[test]
    fn verify_rejects_explicit_log_levels_below_info() {
        for level in [LevelFilter::OFF, LevelFilter::ERROR, LevelFilter::WARN] {
            let error = validate_log_level(&global_with_log(Some(level))).unwrap_err();
            assert!(
                error.to_string().contains("requires --log=info"),
                "unexpected error for {level}: {error}"
            );
        }
    }

    #[test]
    fn verify_accepts_default_and_info_or_more_verbose_logs() {
        for level in [
            None,
            Some(LevelFilter::INFO),
            Some(LevelFilter::DEBUG),
            Some(LevelFilter::TRACE),
        ] {
            validate_log_level(&global_with_log(level)).unwrap();
        }
    }

    #[test]
    fn verification_capture_level_honors_info_and_preserves_explicit_debug() {
        assert_eq!(
            verification_log_level(None, LogCompareStrictness::Canonical, false),
            LevelFilter::INFO
        );
        assert_eq!(
            verification_log_level(
                Some(LevelFilter::INFO),
                LogCompareStrictness::Canonical,
                false,
            ),
            LevelFilter::INFO
        );
        assert_eq!(
            verification_log_level(
                Some(LevelFilter::DEBUG),
                LogCompareStrictness::Canonical,
                false,
            ),
            LevelFilter::DEBUG,
            "explicit DEBUG remains captured for diagnostics"
        );
        assert_eq!(
            verification_log_level(None, LogCompareStrictness::Stripped, false),
            LevelFilter::DEBUG,
            "legacy stripped verification keeps its default"
        );
        assert_eq!(
            verification_log_level(
                Some(LevelFilter::INFO),
                LogCompareStrictness::Stripped,
                false,
            ),
            LevelFilter::INFO,
            "an explicit INFO request must not be promoted"
        );
        assert_eq!(
            verification_log_level(
                Some(LevelFilter::INFO),
                LogCompareStrictness::Canonical,
                true,
            ),
            LevelFilter::TRACE,
            "explicit full-trace diagnostics require TRACE capture"
        );
    }

    fn compare_with(
        left: &Output,
        left_log: TempPath,
        right: &Output,
        right_log: TempPath,
        strictness: LogCompareStrictness,
    ) -> Result<VerificationOutcome, Error> {
        compare_with_envelope(
            left,
            left_log,
            right,
            right_log,
            strictness,
            RecordEnvelope::all_records_v1(),
        )
    }

    fn compare_with_envelope(
        left: &Output,
        left_log: TempPath,
        right: &Output,
        right_log: TempPath,
        strictness: LogCompareStrictness,
        record_envelope: RecordEnvelope,
    ) -> Result<VerificationOutcome, Error> {
        compare_two_runs(
            ComparedRun {
                output: left,
                log: left_log,
                label: "run 1",
            },
            ComparedRun {
                output: right,
                log: right_log,
                label: "run 2",
            },
            ComparisonOptions {
                verbose: false,
                strictness,
                compare_logs: true,
                diagnostic_full_trace: false,
                compare_io_buffers: true,
                keep_logs: false,
                match_overridden: false,
                failed_log_retention: None,
                record_envelope,
                virtualize_time: true,
                caller_owns_logs: false,
            },
        )
    }

    // The default (stripped) comparison, matching what a bare `--verify` runs.
    fn compare(
        left: &Output,
        left_log: TempPath,
        right: &Output,
        right_log: TempPath,
    ) -> Result<VerificationOutcome, Error> {
        compare_with(
            left,
            left_log,
            right,
            right_log,
            LogCompareStrictness::Stripped,
        )
    }

    /// A DETLOG log message whose only variable is a numeric syscall value. The
    /// leading tag lets `extract_log_messages` accept it; " DETLOG " + "detcore:"
    /// let it survive the deterministic-message filter.
    fn detlog_with_value(value: u64) -> String {
        let record_suffix = detcore::detlog::record_suffix(detcore::detlog::DetLogEvent::Syscall);
        format!(
            "2026-08-06T01:00:00.000000Z INFO detcore: [dtid 2] DETLOG [syscall] write(fd=1, count={value}){record_suffix}\n"
        )
    }

    #[test]
    fn extracts_unsupported_syscall_warning_union_from_logs() {
        let file = NamedTempFile::new().unwrap();
        fs::write(
            file.path(),
            b"2026 WARN syscalls vmsplice,getppid used but not yet supported\ninvalid\n",
        )
        .unwrap();

        assert_eq!(
            unsupported_syscalls_from_log(file.path()).unwrap(),
            BTreeSet::from(["getppid".to_owned(), "vmsplice".to_owned()])
        );
    }

    /// Two logs carrying identical comparable content that were both cut at the
    /// bounded writer's size bound, i.e. each ends with the truncation marker
    /// on a line of its own.
    fn logs_truncated_at_the_bound() -> (TempPath, TempPath) {
        let (left, right) = temp_log_files("verify_left", "verify_right").unwrap();
        let left_path = left.into_temp_path();
        let right_path = right.into_temp_path();
        let body = format!(
            "{}{}{}\n",
            detlog_with_value(1),
            detlog_with_value(2),
            detcore::logdiff::TRUNCATION_MARKER
        );
        fs::write(&left_path, body.as_bytes()).unwrap();
        fs::write(&right_path, body.as_bytes()).unwrap();
        (left_path, right_path)
    }

    /// A refused comparison must publish `NoResult`, not `Diverged`.
    ///
    /// The banner already said "This is a NO-RESULT, not a difference and not a
    /// match" while the typed field in the same invocation said `diverged`.
    /// Those are contradictory claims about the same run, and the typed one is
    /// what a machine reads. `Diverged` asserts the two executions were
    /// observed to differ; nothing here observed anything.
    #[test]
    fn a_refused_comparison_is_a_no_result_not_a_divergence() {
        let left = output(0, b"hello\n", b"");
        let right = left.clone();
        let (log1, log2) = logs_truncated_at_the_bound();

        let outcome = compare(&left, log1, &right, log2).unwrap();
        assert_eq!(
            outcome.verdict,
            Verdict::NoResult,
            "a refused comparison must not be reported as a divergence"
        );
        assert!(!outcome.verified(), "a no-result is never verified");
        // The failure direction is unchanged: still not a match, still nonzero
        // compared counts of zero, still a nonzero process exit.
        assert_eq!(
            outcome.compared_log_messages,
            Some(ComparedLogCounts { left: 0, right: 0 })
        );
        let report = verification_report(&outcome);
        assert!(!report.verified);
        assert!(!report.bitwise_parity);
        assert!(
            matches!(
                report.no_result_reason.as_ref(),
                Some(NoResultReason::ComparisonRefused { detail })
                    if detail.contains("truncated at the configured size bound")
            ),
            "the typed report must retain the exact comparator refusal: {report:?}"
        );
        assert_eq!(report.comparison, None);
        assert_eq!(report.compared_log_messages, None);
        assert!(
            outcome.into_exit_status().is_err(),
            "a no-result must still exit nonzero; it is not a pass"
        );
    }

    /// The direction that matters most: truncation must never DOWNGRADE a real
    /// divergence into "we didn't look".
    ///
    /// `NoResult` is a weaker claim than `Diverged`, so emitting it when the
    /// runs actually differed would lose a finding. Each case below pairs the
    /// same refused log comparison with a genuine observed difference, and each
    /// must still report `Diverged`.
    #[test]
    fn an_observed_difference_outranks_a_refused_comparison() {
        for (label, left, right) in [
            (
                "stdout",
                output(0, b"hello\n", b""),
                output(0, b"goodbye\n", b""),
            ),
            (
                "stderr",
                output(0, b"hello\n", b""),
                output(0, b"hello\n", b"oops\n"),
            ),
            (
                "exit status",
                output(0, b"hello\n", b""),
                output(3, b"hello\n", b""),
            ),
        ] {
            let (log1, log2) = logs_truncated_at_the_bound();
            let outcome = compare(&left, log1, &right, log2).unwrap();
            assert_eq!(
                outcome.verdict,
                Verdict::Diverged,
                "{label} differed, so the verdict must be Diverged even though the log \
                 comparison was refused"
            );
            assert!(!outcome.verified());
        }
    }

    #[test]
    fn identical_outputs_verify_successfully() {
        let left = output(0, b"hello\n", b"");
        let right = left.clone();
        let (log1, log2) = empty_logs();

        let outcome = compare(&left, log1, &right, log2).unwrap();
        assert_eq!(outcome.verdict, Verdict::Matched);
        assert!(outcome.verified());
        assert_eq!(outcome.guest_status, ExitStatus::Exited(0));
        // The default `--verify` path is a stripped comparison; the verdict says so.
        assert_eq!(
            outcome.comparison.strictness,
            LogCompareStrictness::Stripped
        );
        assert!(outcome.comparison.strip_lines);
        assert!(!outcome.comparison.full_trace);
    }

    // Direction 1 of the exit-code/verdict decoupling: a guest that exits
    // NONZERO but whose two runs match must report VERIFIED. Before the verdict
    // was separated from the exit code, the propagated `Exited(3)` was the only
    // signal a caller had, so a passing verification of `/bin/false`-like
    // programs was indistinguishable from a failure.
    #[test]
    fn nonzero_exit_with_matching_outputs_reports_verified() {
        let left = output(3, b"hello\n", b"oops\n");
        let right = left.clone();
        let (log1, log2) = empty_logs();

        let outcome = compare(&left, log1, &right, log2).unwrap();
        assert_eq!(outcome.verdict, Verdict::Matched);
        assert!(outcome.verified());
        // The guest status is preserved verbatim, carried *beside* the verdict.
        assert_eq!(outcome.guest_status, ExitStatus::Exited(3));
        // The structured report a `--verify-json` consumer would read:
        let report = verification_report(&outcome);
        assert!(report.verified);
        assert_eq!(report.guest_exit_code, Some(3));
        assert_eq!(report.guest_signal, None);
        // The report also carries the comparison that produced the verdict.
        assert_eq!(
            report.comparison.unwrap().strictness,
            LogCompareStrictness::Stripped
        );
        // Collapsing to the legacy exit convention still propagates the guest
        // code; the verdict channel above is what a caller keys on.
        assert_eq!(outcome.into_exit_status().unwrap(), ExitStatus::Exited(3));
    }

    #[test]
    fn production_comparison_callers_bind_truthful_side_labels() {
        for (name, source, left, right) in [
            ("generic run", include_str!("run.rs"), "run 1", "run 2"),
            ("DBT run", include_str!("backends.rs"), "run 1", "run 2"),
            (
                "record/replay",
                include_str!("record_start.rs"),
                "the recording",
                "the replay",
            ),
        ] {
            let production = source
                .rsplit_once("#[cfg(test)]")
                .expect("production/test boundary")
                .0;
            for label in [left, right] {
                let binding = format!("label: \"{label}\"");
                assert_eq!(
                    production.matches(&binding).count(),
                    1,
                    "{name} must bind {label:?} exactly once"
                );
            }
        }

        let production = include_str!("verify.rs")
            .rsplit_once("#[cfg(test)]")
            .expect("production/test boundary")
            .0;
        assert!(production.contains("side_labels: compared_labels.clone()"));
        assert_eq!(
            production
                .matches("retain_verification_logs([(label1, log1), (label2, log2)])")
                .count(),
            2,
            "both retained-log paths must use the caller-bound labels"
        );
        assert_eq!(
            production
                .matches("retain_verification_logs([(label1, log1)])")
                .count(),
            1,
            "the golden-log path after a match must use the caller-bound label"
        );
    }

    #[test]
    fn output_only_mode_ignores_internal_log_order() {
        let left = output(0, b"console", b"warning");
        let right = output(0, b"console", b"warning");
        let (left_log, right_log) = empty_logs();
        fs::write(&left_log, "DETLOG root event A\n").unwrap();
        fs::write(&right_log, "DETLOG root event B\n").unwrap();

        let outcome = compare_two_runs(
            ComparedRun {
                output: &left,
                log: left_log,
                label: "run 1",
            },
            ComparedRun {
                output: &right,
                log: right_log,
                label: "run 2",
            },
            ComparisonOptions {
                verbose: false,
                strictness: LogCompareStrictness::Stripped,
                compare_logs: false,
                diagnostic_full_trace: false,
                compare_io_buffers: false,
                keep_logs: false,
                match_overridden: false,
                failed_log_retention: None,
                record_envelope: RecordEnvelope::all_records_v1(),
                virtualize_time: true,
                caller_owns_logs: false,
            },
        )
        .unwrap();
        assert_eq!(outcome.verdict, Verdict::Matched);
        assert_eq!(outcome.guest_status, ExitStatus::Exited(0));
        // The verdict records that the log stream was NOT compared, so no
        // consumer can mistake this for a bitwise log-parity result.
        assert!(!outcome.comparison.compare_logs);
    }

    #[test]
    fn stdout_stderr_and_status_mismatches_fail_verification() {
        let baseline = output(0, b"hello\n", b"");
        let mismatches = [
            output(0, b"different\n", b""),
            output(0, b"hello\n", b"different\n"),
            output(1, b"hello\n", b""),
        ];

        for mismatch in mismatches {
            let (log1, log2) = empty_logs();
            let path1 = log1.to_path_buf();
            let path2 = log2.to_path_buf();

            let outcome = compare(&baseline, log1, &mismatch, log2).unwrap();
            assert_eq!(outcome.verdict, Verdict::Diverged);
            assert!(!outcome.verified());
            assert_eq!(
                outcome.into_exit_status().unwrap(),
                ExitStatus::Exited(HERMIT_VERIFICATION_DIVERGENCE_EXIT)
            );

            let _ = fs::remove_file(path1);
            let _ = fs::remove_file(path2);
        }
    }

    #[test]
    fn comparison_spec_maps_strictness_to_concrete_flags() {
        let stripped = ComparisonSpec::new(
            LogCompareStrictness::Stripped,
            true,
            false,
            true,
            RecordEnvelopePolicy::AllRecordsV1,
            true,
        );
        assert!(stripped.strip_lines);
        assert!(!stripped.full_trace);
        assert_eq!(stripped.display_name, "Stripped");
        assert_eq!(
            stripped.stripped_prefixes,
            [STRIP_WALL_CLOCK_PREFIX_V1, STRIP_UNSAFE_NORMALIZATION_V1]
        );
        assert_eq!(
            comparison_evidence_line(&stripped).as_deref(),
            Some(
                ":: comparison=Stripped relaxations=unsafe-numeric-address-and-path-normalization/v1"
            )
        );
        assert_eq!(
            stripped.log_comparison_mode(),
            LogComparisonMode::Deterministic
        );

        let canonical = ComparisonSpec::new(
            LogCompareStrictness::Canonical,
            true,
            false,
            true,
            RecordEnvelopePolicy::AllRecordsV1,
            true,
        );
        assert!(!canonical.strip_lines);
        assert!(canonical.canonicalize_addresses);
        assert!(canonical.exact_remainder);
        assert!(canonical.full_trace);
        assert_eq!(canonical.display_name, "BitwiseInfoV1");
        assert_eq!(canonical.stripped_prefixes, PARITY_STRIPPED_PREFIXES);
        assert_eq!(canonical.canonicalizations, PARITY_CANONICALIZATIONS);
        assert_eq!(
            comparison_evidence_line(&canonical).as_deref(),
            Some(":: comparison=BitwiseInfoV1 relaxations=none")
        );
        assert_eq!(canonical.log_scope, ComparedLogScope::Info);
        assert_eq!(canonical.log_comparison_mode(), LogComparisonMode::Info);

        let no_io_buffers = ComparisonSpec {
            compare_io_buffers: false,
            ..canonical
        };
        assert_eq!(
            comparison_evidence_line(&no_io_buffers).as_deref(),
            Some(":: comparison=BitwiseInfoV1 relaxations=--no-detlog-io-buffers")
        );

        let invalid_policy_shape = ComparisonSpec {
            skip_commit: true,
            ..canonical
        };
        assert_eq!(
            comparison_evidence_line(&invalid_policy_shape).as_deref(),
            Some(":: comparison=unknown relaxations=unknown")
        );

        let invalid_display_name = ComparisonSpec {
            display_name: "Stripped",
            ..canonical
        };
        assert_eq!(
            comparison_evidence_line(&invalid_display_name).as_deref(),
            Some(":: comparison=unknown relaxations=unknown")
        );

        let diagnostic = ComparisonSpec::new(
            LogCompareStrictness::Canonical,
            true,
            true,
            true,
            RecordEnvelopePolicy::AllRecordsV1,
            true,
        );
        assert_eq!(diagnostic.log_scope, ComparedLogScope::FullTrace);
        assert_eq!(diagnostic.display_name, "BitwiseFullTraceV1");
        assert_eq!(
            comparison_evidence_line(&diagnostic).as_deref(),
            Some(":: comparison=BitwiseFullTraceV1 relaxations=none")
        );
        let output_only = ComparisonSpec {
            compare_logs: false,
            ..canonical
        };
        assert_eq!(comparison_evidence_line(&output_only), None);
        assert_eq!(
            diagnostic.log_comparison_mode(),
            LogComparisonMode::FullTrace
        );
    }

    #[test]
    fn bitwise_info_ignores_debug_diagnostics_but_rejects_real_info_divergence() {
        let out = output(0, b"hello\n", b"");
        let make_logs = |right_info: u64| {
            let (left, right) = empty_logs();
            fs::write(
                &left,
                format!(
                    "{}2026-08-06T01:00:00.000001Z DEBUG detcore: diagnostic host timing=100\n",
                    detlog_with_value(7)
                ),
            )
            .unwrap();
            fs::write(
                &right,
                format!(
                    "{}2026-08-06T01:00:00.000002Z DEBUG detcore: diagnostic host timing=200\n",
                    detlog_with_value(right_info)
                ),
            )
            .unwrap();
            (left, right)
        };

        // Positive INFO bracket: the captured DEBUG diagnostics differ, while
        // the one INFO event on each side matches exactly.
        let (left, right) = make_logs(7);
        let matched =
            compare_with(&out, left, &out, right, LogCompareStrictness::Canonical).unwrap();
        assert_eq!(matched.verdict, Verdict::Matched);
        assert_eq!(matched.comparison.log_scope, ComparedLogScope::Info);
        assert_eq!(
            matched.compared_log_messages,
            Some(ComparedLogCounts { left: 1, right: 1 })
        );
        assert!(verification_report(&matched).bitwise_parity);

        // Negative INFO bracket: changing the actual INFO payload must fail even
        // though DEBUG remains outside the parity envelope.
        let (left, right) = make_logs(8);
        let left_path = left.to_path_buf();
        let right_path = right.to_path_buf();
        let info_diverged =
            compare_with(&out, left, &out, right, LogCompareStrictness::Canonical).unwrap();
        assert_eq!(info_diverged.verdict, Verdict::Diverged);
        let _ = fs::remove_file(left_path);
        let _ = fs::remove_file(right_path);

        // DEBUG is still available as an explicit diagnostic comparison. The
        // same matching INFO / differing DEBUG captures fail only when that
        // full-trace scope is requested.
        let (left, right) = make_logs(7);
        let left_path = left.to_path_buf();
        let right_path = right.to_path_buf();
        let debug_diverged = compare_two_runs(
            ComparedRun {
                output: &out,
                log: left,
                label: "run 1",
            },
            ComparedRun {
                output: &out,
                log: right,
                label: "run 2",
            },
            ComparisonOptions {
                verbose: true,
                strictness: LogCompareStrictness::Canonical,
                compare_logs: true,
                diagnostic_full_trace: true,
                compare_io_buffers: false,
                keep_logs: false,
                match_overridden: false,
                failed_log_retention: None,
                record_envelope: RecordEnvelope::all_records_v1(),
                virtualize_time: true,
                caller_owns_logs: false,
            },
        )
        .unwrap();
        assert_eq!(debug_diverged.verdict, Verdict::Diverged);
        assert_eq!(
            debug_diverged.comparison.log_scope,
            ComparedLogScope::FullTrace
        );
        let _ = fs::remove_file(left_path);
        let _ = fs::remove_file(right_path);
    }

    // The core of the strip-lines/verdict decoupling: two runs whose logs differ
    // ONLY in a numeric syscall value (a stand-in for a virtual-time timestamp or
    // a raw syscall argument) are reported MATCHED under the default stripped
    // comparison — because `strip_lines` normalizes the number away — but DIVERGED
    // under a bitwise comparison. The identical guest outputs are held constant so
    // the log comparison alone drives each verdict. A bare "verified" therefore
    // cannot say which comparison certified it; the carried `ComparisonSpec` can.
    #[test]
    fn stripped_matches_but_bitwise_diverges_on_numeric_only_log_difference() {
        let out = output(0, b"hello\n", b"");

        // Stripped: the numeric difference is normalized away -> Matched.
        let (log1, log2) = empty_logs();
        fs::write(&log1, detlog_with_value(100)).unwrap();
        fs::write(&log2, detlog_with_value(200)).unwrap();
        let stripped =
            compare_with(&out, log1, &out, log2, LogCompareStrictness::Stripped).unwrap();
        assert_eq!(stripped.verdict, Verdict::Matched);
        assert!(stripped.verified());
        assert!(stripped.comparison.strip_lines);
        assert!(!stripped.comparison.full_trace);

        // Canonical: the same inputs, but every byte compared (a decimal value,
        // untouched by address canonicalization) -> Diverged. The verdict flips
        // on the comparison mode alone, and the outcome records it.
        let (log1, log2) = empty_logs();
        let path1 = log1.to_path_buf();
        let path2 = log2.to_path_buf();
        fs::write(&path1, detlog_with_value(100)).unwrap();
        fs::write(&path2, detlog_with_value(200)).unwrap();
        let canonical =
            compare_with(&out, log1, &out, log2, LogCompareStrictness::Canonical).unwrap();
        assert_eq!(canonical.verdict, Verdict::Diverged);
        assert!(!canonical.verified());
        assert_eq!(
            canonical.comparison.strictness,
            LogCompareStrictness::Canonical
        );
        assert!(!canonical.comparison.strip_lines);
        assert!(canonical.comparison.full_trace);
        // A `--verify-json` consumer reads the strictness from the report and so
        // can refuse to treat a stripped match as parity.
        let report = verification_report(&canonical);
        assert!(!report.verified);
        assert_eq!(report.comparison.unwrap().strip_lines, Some(false));

        assert!(path1.exists(), "divergent run-1 log must be retained");
        assert!(path2.exists(), "divergent run-2 log must be retained");
        fs::remove_file(path1).unwrap();
        fs::remove_file(path2).unwrap();
    }

    // A plain `--verify` (Stripped) and `--verify-strict` (Canonical) print the
    // same success sentence, which is kept verbatim for its many consumers. The
    // line after it must say which comparison ran, and only a match whose
    // published report has `bitwise_parity` may claim bitwise parity. This
    // checks the bytes the announcement actually writes, not a helper alone.
    #[test]
    fn stripped_success_does_not_claim_bitwise_parity_but_strict_success_does() {
        const SUCCESS: &str = "Success: deterministic. Determinism verified.";
        let announce_as = |outcome: &VerificationOutcome, second_run: SecondRun| {
            let mut bytes = Vec::new();
            write_verification_announcement(
                &mut bytes,
                outcome,
                second_run,
                SUCCESS,
                "Failure: nondeterministic.",
            )
            .unwrap();
            String::from_utf8(bytes).unwrap()
        };
        let announce = |outcome: &VerificationOutcome| announce_as(outcome, SecondRun::Rerun);
        let out = output(0, b"hello\n", b"");

        let (log1, log2) = empty_logs();
        fs::write(&log1, detlog_with_value(100)).unwrap();
        fs::write(&log2, detlog_with_value(100)).unwrap();
        let stripped =
            compare_with(&out, log1, &out, log2, LogCompareStrictness::Stripped).unwrap();
        assert_eq!(stripped.verdict, Verdict::Matched);
        assert!(!verification_report(&stripped).bitwise_parity);
        let text = announce(&stripped);
        assert!(text.contains(SUCCESS), "{text}");
        assert!(!text.contains(BITWISE_PARITY_CLAIM), "{text}");
        assert!(
            text.contains(
                "only the DETLOG and scheduler COMMIT records of the guest's log (1 vs 1) \
                 under the lossy Stripped comparison"
            ),
            "{text}"
        );
        assert!(
            text.contains("This is not a bitwise comparison; add --verify-strict for one."),
            "{text}"
        );
        assert!(!text.contains(TIME_NOT_VIRTUALIZED_NOTE), "{text}");
        assert!(!text.contains(RERUN_TIME_NOT_VIRTUALIZED_NOTE), "{text}");

        let (log1, log2) = empty_logs();
        fs::write(&log1, detlog_with_value(100)).unwrap();
        fs::write(&log2, detlog_with_value(100)).unwrap();
        let canonical =
            compare_with(&out, log1, &out, log2, LogCompareStrictness::Canonical).unwrap();
        assert_eq!(canonical.verdict, Verdict::Matched);
        assert!(verification_report(&canonical).bitwise_parity);
        let text = announce(&canonical);
        assert!(text.contains(SUCCESS), "{text}");
        assert!(
            text.contains(&format!("under BitwiseInfoV1: {BITWISE_PARITY_CLAIM}.")),
            "{text}"
        );
        assert!(
            text.contains("the INFO records of the guest's log (1 vs 1) exactly"),
            "{text}"
        );
        assert!(!text.contains("not a bitwise comparison"), "{text}");
        assert!(!text.contains(TIME_NOT_VIRTUALIZED_NOTE), "{text}");
        assert!(!text.contains(RERUN_TIME_NOT_VIRTUALIZED_NOTE), "{text}");
        // With virtual time on, a replay's match carries no time note either.
        let text = announce_as(&canonical, SecondRun::Replay);
        assert!(!text.contains(TIME_NOT_VIRTUALIZED_NOTE), "{text}");
        assert!(!text.contains(RERUN_TIME_NOT_VIRTUALIZED_NOTE), "{text}");

        // `record start --verify --verify-strict` compares with virtual time
        // off. Parity can still hold, but the line must say that the match is
        // replay fidelity, not agreement between separate runs.
        let time_off = VerificationOutcome {
            comparison: ComparisonSpec {
                virtualize_time: false,
                ..canonical.comparison
            },
            ..canonical.clone()
        };
        assert!(verification_report(&time_off).bitwise_parity);
        let text = announce_as(&time_off, SecondRun::Replay);
        assert!(
            text.contains(&format!(
                "{BITWISE_PARITY_CLAIM}. {TIME_NOT_VIRTUALIZED_NOTE}"
            )),
            "{text}"
        );
        assert!(!text.contains(RERUN_TIME_NOT_VIRTUALIZED_NOTE), "{text}");

        // `run --no-virtualize-time --verify-strict` compares two independent
        // runs, so the replay wording ("the second run reproduced the first")
        // would be false there. It says instead that the guest read the
        // host's clock.
        let text = announce_as(&time_off, SecondRun::Rerun);
        assert!(
            text.contains(&format!(
                "{BITWISE_PARITY_CLAIM}. {RERUN_TIME_NOT_VIRTUALIZED_NOTE}"
            )),
            "{text}"
        );
        assert!(!text.contains(TIME_NOT_VIRTUALIZED_NOTE), "{text}");
        assert!(!text.contains("reproduced the first"), "{text}");

        // A full-trace comparison names the whole log as its scope.
        let full_trace = VerificationOutcome {
            comparison: ComparisonSpec {
                log_scope: ComparedLogScope::FullTrace,
                ..canonical.comparison
            },
            ..canonical.clone()
        };
        let text = announce(&full_trace);
        assert!(
            text.contains("every record of the guest's log (1 vs 1)"),
            "{text}"
        );

        // A canonical spec without the evidence parity needs (here, no
        // compared records) must not inherit the claim from its policy name.
        let no_evidence = VerificationOutcome {
            compared_log_messages: Some(ComparedLogCounts { left: 0, right: 0 }),
            ..canonical.clone()
        };
        let text = announce(&no_evidence);
        assert!(!text.contains(BITWISE_PARITY_CLAIM), "{text}");
        assert!(text.contains("This is not a bitwise comparison."), "{text}");

        // An output-only fallback compared no log at all.
        let output_only = VerificationOutcome {
            comparison: ComparisonSpec {
                compare_logs: false,
                ..canonical.comparison
            },
            compared_log_messages: None,
            ..canonical.clone()
        };
        let text = announce(&output_only);
        assert!(!text.contains(BITWISE_PARITY_CLAIM), "{text}");
        assert!(text.contains("Compared: stdout and stderr only."), "{text}");

        // A divergence prints no qualifier and no claim.
        let diverged = VerificationOutcome {
            verdict: Verdict::Diverged,
            ..canonical
        };
        let text = announce(&diverged);
        assert!(!text.contains("Compared:"), "{text}");
        assert!(!text.contains(BITWISE_PARITY_CLAIM), "{text}");
    }

    /// Compare two in-memory logs with the canonical comparator and report
    /// which of the two log paths survived the comparison.
    fn compare_for_retention(
        directory: &Path,
        left_log: &str,
        right_log: &str,
        compare_logs: bool,
        keep_logs: bool,
        match_overridden: bool,
    ) -> (Verdict, PathBuf, PathBuf) {
        let output = output(0, b"hello\n", b"");
        let (left, right) = temp_log_files_in("run1", "run2", Some(directory)).unwrap();
        fs::write(left.path(), left_log).unwrap();
        fs::write(right.path(), right_log).unwrap();
        let left_path = left.path().to_path_buf();
        let right_path = right.path().to_path_buf();
        let outcome = compare_two_runs(
            ComparedRun {
                output: &output,
                log: left.into_temp_path(),
                label: "run 1",
            },
            ComparedRun {
                output: &output,
                log: right.into_temp_path(),
                label: "run 2",
            },
            ComparisonOptions {
                verbose: false,
                strictness: LogCompareStrictness::Canonical,
                compare_logs,
                diagnostic_full_trace: false,
                compare_io_buffers: false,
                keep_logs,
                match_overridden,
                failed_log_retention: None,
                record_envelope: RecordEnvelope::all_records_v1(),
                virtualize_time: true,
                caller_owns_logs: false,
            },
        )
        .unwrap();
        (outcome.verdict, left_path, right_path)
    }

    /// Assert that a retained log is a regular file holding exactly `expected`.
    fn assert_retained_log(path: &Path, expected: &str, side: &str) {
        let metadata = fs::symlink_metadata(path)
            .unwrap_or_else(|error| panic!("{side} log {} is missing: {error}", path.display()));
        assert!(metadata.file_type().is_file(), "{side} log is not a file");
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            expected,
            "{side} log was not retained byte for byte"
        );
    }

    /// `--keep-logs` retains one golden log after a match and both logs after
    /// a divergence. Once the two logs compared equal, run 2's log is deleted
    /// and run 1's is kept as the golden log.
    /// A divergence retains both whether or not `--keep-logs` was given, and a
    /// match the caller has already overridden (a skid overshoot, or a DBT
    /// counted-branch or statistics divergence) retains both, as a divergence
    /// does. Without `--keep-logs`, a match retains nothing.
    #[test]
    fn verification_log_retention_matches_the_cli_contract() {
        let directory = tempfile::tempdir().unwrap();

        for (
            left_value,
            right_value,
            keep_logs,
            match_overridden,
            expected,
            run1_retained,
            run2_retained,
        ) in [
            (7, 7, false, false, Verdict::Matched, false, false),
            (7, 8, false, false, Verdict::Diverged, true, true),
            (7, 7, true, false, Verdict::Matched, true, false),
            (7, 8, true, false, Verdict::Diverged, true, true),
            (7, 7, false, true, Verdict::Matched, false, false),
            (7, 7, true, true, Verdict::Matched, true, true),
            (7, 8, true, true, Verdict::Diverged, true, true),
        ] {
            let case = format!(
                "values {left_value}/{right_value}, keep_logs={keep_logs}, \
                 match_overridden={match_overridden}"
            );
            let (verdict, left_path, right_path) = compare_for_retention(
                directory.path(),
                &detlog_with_value(left_value),
                &detlog_with_value(right_value),
                true,
                keep_logs,
                match_overridden,
            );
            assert_eq!(verdict, expected, "verdict mismatch for {case}");
            if run1_retained {
                assert_retained_log(&left_path, &detlog_with_value(left_value), "run-1");
            } else {
                assert!(!left_path.exists(), "run-1 log must be deleted for {case}");
            }
            if run2_retained {
                assert_retained_log(&right_path, &detlog_with_value(right_value), "run-2");
            } else {
                assert!(!right_path.exists(), "run-2 log must be deleted for {case}");
            }
            for (path, retained) in [(left_path, run1_retained), (right_path, run2_retained)] {
                if retained {
                    fs::remove_file(path).unwrap();
                }
            }
        }
        assert_eq!(
            fs::read_dir(directory.path()).unwrap().count(),
            0,
            "every retained log was accounted for"
        );
    }

    /// A match that never compared the logs has not verified run 2's log
    /// against run 1's, so neither is a golden copy of the other and
    /// `--keep-logs` retains both.
    #[test]
    fn requested_logs_survive_a_match_that_did_not_compare_them() {
        let directory = tempfile::tempdir().unwrap();
        let (verdict, left_path, right_path) = compare_for_retention(
            directory.path(),
            &detlog_with_value(7),
            &detlog_with_value(8),
            false,
            true,
            false,
        );
        assert_eq!(verdict, Verdict::Matched);
        assert_retained_log(&left_path, &detlog_with_value(7), "run-1");
        assert_retained_log(&right_path, &detlog_with_value(8), "run-2");
    }

    /// A refused comparison is a no-result, not a match, so `--keep-logs`
    /// retains both logs, exactly as before one-golden-log retention.
    #[test]
    fn requested_logs_survive_a_refused_comparison() {
        let directory = tempfile::tempdir().unwrap();
        let body = format!(
            "{}{}{}\n",
            detlog_with_value(1),
            detlog_with_value(2),
            detcore::logdiff::TRUNCATION_MARKER
        );
        let (verdict, left_path, right_path) =
            compare_for_retention(directory.path(), &body, &body, true, true, false);
        assert_eq!(verdict, Verdict::NoResult);
        assert_retained_log(&left_path, &body, "run-1");
        assert_retained_log(&right_path, &body, "run-2");
    }

    /// The DBT backend observes a counted-branch or statistics divergence
    /// before the log comparison, so its logs can match while the verdict is
    /// a divergence. It forces retention and marks the match overridden; both
    /// logs must then land in the bounded failure directory, as they did
    /// before one-golden-log retention, and neither stays at its source path.
    #[test]
    fn an_overridden_match_keeps_both_logs_in_the_failure_directory() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let root = temporary.path().join("verify-failures");
        fs::create_dir(&source).unwrap();
        let output = output(0, b"same output\n", b"");
        let (left, right) = temp_log_files_in("run1", "run2", Some(&source)).unwrap();
        fs::write(left.path(), detlog_with_value(7)).unwrap();
        fs::write(right.path(), detlog_with_value(7)).unwrap();
        let left_name = left.path().file_name().unwrap().to_owned();
        let right_name = right.path().file_name().unwrap().to_owned();
        let outcome = compare_two_runs(
            ComparedRun {
                output: &output,
                log: left.into_temp_path(),
                label: "run 1",
            },
            ComparedRun {
                output: &output,
                log: right.into_temp_path(),
                label: "run 2",
            },
            ComparisonOptions {
                verbose: false,
                strictness: LogCompareStrictness::Canonical,
                compare_logs: true,
                diagnostic_full_trace: false,
                compare_io_buffers: true,
                keep_logs: true,
                match_overridden: true,
                failed_log_retention: Some(FailedVerifyLogRetention::new(root.clone(), 2)),
                record_envelope: RecordEnvelope::all_records_v1(),
                virtualize_time: true,
                caller_owns_logs: false,
            },
        )
        .unwrap();
        assert_eq!(outcome.verdict, Verdict::Matched);
        assert_eq!(fs::read_dir(&source).unwrap().count(), 0);
        let directories = retained_comparison_dirs(&root).unwrap();
        assert_eq!(directories.len(), 1, "one retained comparison");
        let retained = &directories[0].0;
        assert_retained_log(&retained.join(&left_name), &detlog_with_value(7), "run-1");
        assert_retained_log(&retained.join(&right_name), &detlog_with_value(7), "run-2");
    }

    fn run_failed_comparison(source: &Path, retention: FailedVerifyLogRetention, value: u64) {
        let output = output(0, b"same output\n", b"");
        let (left, right) = temp_log_files_in("run1", "run2", Some(source)).unwrap();
        fs::write(left.path(), detlog_with_value(value)).unwrap();
        fs::write(right.path(), detlog_with_value(value + 1)).unwrap();
        let outcome = compare_two_runs(
            ComparedRun {
                output: &output,
                log: left.into_temp_path(),
                label: "run 1",
            },
            ComparedRun {
                output: &output,
                log: right.into_temp_path(),
                label: "run 2",
            },
            ComparisonOptions {
                verbose: false,
                strictness: LogCompareStrictness::Canonical,
                compare_logs: true,
                diagnostic_full_trace: false,
                compare_io_buffers: true,
                keep_logs: false,
                match_overridden: false,
                failed_log_retention: Some(retention),
                record_envelope: RecordEnvelope::all_records_v1(),
                virtualize_time: true,
                caller_owns_logs: false,
            },
        )
        .unwrap();
        assert_eq!(outcome.verdict, Verdict::Diverged);
    }

    #[test]
    fn failed_comparison_materialization_holds_the_retained_count_at_the_bound() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let root = temporary.path().join("verify-failures");
        fs::create_dir(&source).unwrap();
        let retention = FailedVerifyLogRetention::new(root.clone(), 2);

        for value in 0..3 {
            run_failed_comparison(&source, retention.clone(), value);
        }

        let directories = retained_comparison_dirs(&root).unwrap();
        assert_eq!(
            directories.len(),
            2,
            "the third failure must retire the oldest"
        );
        for (directory, _) in directories {
            let logs = fs::read_dir(directory)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name() != FAILED_VERIFY_LOG_LOCK)
                .collect::<Vec<_>>();
            assert_eq!(logs.len(), 2, "each retained failure keeps both logs");
        }
    }

    #[test]
    fn failed_comparison_retirement_refuses_a_log_diff_reader() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let root = temporary.path().join("verify-failures");
        fs::create_dir(&source).unwrap();
        let retention = FailedVerifyLogRetention::new(root.clone(), 2);
        run_failed_comparison(&source, retention.clone(), 0);
        run_failed_comparison(&source, retention.clone(), 2);

        let oldest = retained_comparison_dirs(&root).unwrap().pop().unwrap().0;
        let protected_log = fs::read_dir(&oldest)
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| entry.file_name() != FAILED_VERIFY_LOG_LOCK)
            .unwrap()
            .path();
        let read_locks = lock_failed_verification_logs_for_read([protected_log]).unwrap();
        run_failed_comparison(&source, retention.clone(), 4);
        assert!(oldest.is_dir(), "an active reader must prevent retirement");
        assert_eq!(retained_comparison_dirs(&root).unwrap().len(), 3);

        drop(read_locks);
        run_failed_comparison(&source, retention, 6);
        assert!(
            !oldest.exists(),
            "the unlocked old evidence is retired later"
        );
        assert_eq!(retained_comparison_dirs(&root).unwrap().len(), 2);
    }

    #[test]
    fn failed_comparison_retirement_protects_symlinked_readers() {
        for file_alias in [false, true] {
            let temporary = tempfile::tempdir().unwrap();
            let source = temporary.path().join("source");
            let root = temporary.path().join("verify-failures");
            fs::create_dir(&source).unwrap();
            let retention = FailedVerifyLogRetention::new(root.clone(), 2);
            run_failed_comparison(&source, retention.clone(), 0);
            run_failed_comparison(&source, retention.clone(), 2);

            let oldest = retained_comparison_dirs(&root).unwrap().pop().unwrap().0;
            let protected_log = fs::read_dir(&oldest)
                .unwrap()
                .filter_map(Result::ok)
                .find(|entry| entry.file_name() != FAILED_VERIFY_LOG_LOCK)
                .unwrap()
                .path();
            let alias = temporary.path().join("reader-path");
            let input = if file_alias {
                std::os::unix::fs::symlink(&protected_log, &alias).unwrap();
                alias
            } else {
                std::os::unix::fs::symlink(&oldest, &alias).unwrap();
                alias.join(protected_log.file_name().unwrap())
            };
            let read_locks = lock_failed_verification_logs_for_read([input]).unwrap();
            run_failed_comparison(&source, retention.clone(), 4);
            assert!(
                oldest.is_dir(),
                "a reader through a symlink must prevent retirement (file_alias={file_alias})"
            );
            assert_eq!(retained_comparison_dirs(&root).unwrap().len(), 3);

            drop(read_locks);
            run_failed_comparison(&source, retention, 6);
            assert!(
                !oldest.exists(),
                "the aliased evidence must be retired after its reader releases the lock"
            );
            assert_eq!(retained_comparison_dirs(&root).unwrap().len(), 2);
        }
    }

    #[test]
    fn requested_logs_survive_unsupported_syscall_scan_io_error() {
        let directory = tempfile::tempdir().unwrap();
        let output = output(0, b"hello\n", b"");
        let (left, right) = temp_log_files_in("left", "right", Some(directory.path())).unwrap();
        fs::write(left.path(), detlog_with_value(7)).unwrap();
        fs::write(right.path(), detlog_with_value(7)).unwrap();
        let left_path = left.path().to_path_buf();
        let right_path = right.path().to_path_buf();

        let error = compare_two_runs_with_unsupported_scan(
            ComparedRun {
                output: &output,
                log: left.into_temp_path(),
                label: "run 1",
            },
            ComparedRun {
                output: &output,
                log: right.into_temp_path(),
                label: "run 2",
            },
            ComparisonOptions {
                verbose: false,
                strictness: LogCompareStrictness::Canonical,
                compare_logs: true,
                diagnostic_full_trace: false,
                compare_io_buffers: false,
                keep_logs: true,
                match_overridden: false,
                failed_log_retention: None,
                record_envelope: RecordEnvelope::all_records_v1(),
                virtualize_time: true,
                caller_owns_logs: false,
            },
            |_| Err(io::Error::other("injected unsupported-syscall scan error")),
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("injected unsupported-syscall scan error")
        );
        assert!(left_path.exists(), "run-1 log must survive the scan error");
        assert!(right_path.exists(), "run-2 log must survive the scan error");
        assert_eq!(
            fs::read_to_string(&left_path).unwrap(),
            detlog_with_value(7)
        );
        assert_eq!(
            fs::read_to_string(&right_path).unwrap(),
            detlog_with_value(7)
        );
        fs::remove_file(left_path).unwrap();
        fs::remove_file(right_path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn requested_logs_survive_log_comparison_io_error() {
        let directory = tempfile::tempdir().unwrap();
        let output = output(0, b"hello\n", b"");
        let (left, right) = temp_log_files_in("left", "right", Some(directory.path())).unwrap();
        let mut left_bytes = detlog_with_value(7).into_bytes();
        left_bytes.push(0xff);
        let right_bytes = detlog_with_value(7).into_bytes();
        fs::write(left.path(), &left_bytes).unwrap();
        fs::write(right.path(), &right_bytes).unwrap();
        let left_path = left.path().to_path_buf();
        let right_path = right.path().to_path_buf();
        // Invalid UTF-8 reaches the real comparator's I/O error path even when
        // the test can read files whose permission bits are all clear.
        assert!(
            std::str::from_utf8(&fs::read(&left_path).unwrap()).is_err(),
            "the run-1 log must contain invalid UTF-8"
        );

        let result = compare_two_runs(
            ComparedRun {
                output: &output,
                log: left.into_temp_path(),
                label: "run 1",
            },
            ComparedRun {
                output: &output,
                log: right.into_temp_path(),
                label: "run 2",
            },
            ComparisonOptions {
                verbose: false,
                strictness: LogCompareStrictness::Canonical,
                compare_logs: true,
                diagnostic_full_trace: false,
                compare_io_buffers: false,
                keep_logs: true,
                match_overridden: false,
                failed_log_retention: None,
                record_envelope: RecordEnvelope::all_records_v1(),
                virtualize_time: true,
                caller_owns_logs: false,
            },
        );

        let error = result.expect_err("invalid UTF-8 must fail comparison");
        assert_eq!(
            error
                .downcast_ref::<io::Error>()
                .expect("the real comparator must return an I/O error")
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert!(
            left_path.exists(),
            "run-1 log must survive the comparison error"
        );
        assert!(
            right_path.exists(),
            "run-2 log must survive the comparison error"
        );
        for (path, expected) in [
            (&left_path, left_bytes.as_slice()),
            (&right_path, right_bytes.as_slice()),
        ] {
            assert!(fs::symlink_metadata(path).unwrap().file_type().is_file());
            assert_eq!(fs::read(path).unwrap(), expected);
        }
        fs::remove_file(left_path).unwrap();
        fs::remove_file(right_path).unwrap();
    }

    #[test]
    fn verification_report_carries_first_log_divergence_position() {
        let output = output(0, b"hello\n", b"");
        let (left, right) = empty_logs();
        let left_path = left.to_path_buf();
        let right_path = right.to_path_buf();
        let commit =
            detcore::detlog::record_suffix(detcore::detlog::DetLogEvent::SchedulerCommit {
                scheduler_turn: 23,
                virtual_nanoseconds: 4_567_890_123,
                internal_io_poll: false,
                runtime_maps_read: false,
            });
        let other = detcore::detlog::record_suffix(detcore::detlog::DetLogEvent::Other);
        fs::write(
            &left_path,
            format!(
                "2026-08-13T01:02:03.000000Z INFO detcore::scheduler: COMMIT turn 23, dettid 2, on previously committed 4.567_890_123s{commit}\n\
                 2026-08-13T01:02:03.000001Z INFO detcore: DETLOG value=1{other}\n"
            ),
        )
        .unwrap();
        fs::write(
            &right_path,
            format!(
                "2026-08-13T01:02:04.000000Z INFO detcore::scheduler: COMMIT turn 23, dettid 2, on previously committed 4.567_890_123s{commit}\n\
                 2026-08-13T01:02:04.000001Z INFO detcore: DETLOG value=2{other}\n"
            ),
        )
        .unwrap();

        let outcome = compare_with(
            &output,
            left,
            &output,
            right,
            LogCompareStrictness::Canonical,
        )
        .unwrap();
        let report = verification_report(&outcome);
        assert_eq!(report.verdict, Verdict::Diverged);
        assert_eq!(report.first_divergent_scheduler_turn, Some(23));
        assert_eq!(
            report.first_divergent_virtual_nanoseconds,
            Some(4_567_890_123)
        );
        assert_eq!(
            report.first_divergent_left_message.as_deref(),
            Some("INFO detcore: DETLOG value=1")
        );
        assert_eq!(
            report.first_divergent_right_message.as_deref(),
            Some("INFO detcore: DETLOG value=2")
        );

        let _ = fs::remove_file(left_path);
        let _ = fs::remove_file(right_path);
    }

    #[test]
    fn output_divergence_does_not_discard_the_log_divergence_position() {
        let left_output = output(0, b"left\n", b"");
        let right_output = output(0, b"right\n", b"");
        let (left, right) = empty_logs();
        let left_path = left.to_path_buf();
        let right_path = right.to_path_buf();
        let commit =
            detcore::detlog::record_suffix(detcore::detlog::DetLogEvent::SchedulerCommit {
                scheduler_turn: 23,
                virtual_nanoseconds: 4_567_890_123,
                internal_io_poll: false,
                runtime_maps_read: false,
            });
        fs::write(
            &left_path,
            format!(
                "INFO detcore::scheduler: COMMIT turn 23 at time 4567890123\n\
                 INFO detcore: DETLOG value=1{commit}\n"
            ),
        )
        .unwrap();
        fs::write(
            &right_path,
            format!(
                "INFO detcore::scheduler: COMMIT turn 23 at time 4567890123\n\
                 INFO detcore: DETLOG value=2{commit}\n"
            ),
        )
        .unwrap();

        let outcome = compare_with(
            &left_output,
            left,
            &right_output,
            right,
            LogCompareStrictness::Canonical,
        )
        .unwrap();
        let report = verification_report(&outcome);
        assert_eq!(report.verdict, Verdict::Diverged);
        assert_eq!(report.first_divergent_scheduler_turn, Some(23));
        assert_eq!(
            report.first_divergent_virtual_nanoseconds,
            Some(4_567_890_123)
        );
        assert_eq!(
            report.first_divergent_left_message.as_deref(),
            Some(
                "INFO detcore::scheduler: COMMIT turn <NUM> at time <NANOSECONDS>\nINFO detcore: DETLOG value=1"
            )
        );
        assert_eq!(
            report.first_divergent_right_message.as_deref(),
            Some(
                "INFO detcore::scheduler: COMMIT turn <NUM> at time <NANOSECONDS>\nINFO detcore: DETLOG value=2"
            )
        );

        let _ = fs::remove_file(left_path);
        let _ = fs::remove_file(right_path);
    }

    // The `--verify-json` payload names the comparison in the JSON itself, so a
    // downstream ratchet can gate on bitwise parity without out-of-band knowledge.
    /// FINDING 2, NEGATIVE BRACKET. A comparison that consumed ZERO log
    /// messages must never certify bitwise parity, even though every
    /// configuration field qualifies and the runs "matched": `diff_vecs`
    /// returns "no difference" for two empty selections, so configuration
    /// strictness alone would hand back a green over no work at all.
    #[test]
    fn empty_log_comparison_matches_but_is_never_parity() {
        let out = output(0, b"hello\n", b"");
        let (log1, log2) = empty_logs();
        let outcome =
            compare_with(&out, log1, &out, log2, LogCompareStrictness::Canonical).unwrap();

        // The verdict itself is legitimately Matched: stdout, stderr and exit
        // status all agree. Only the PARITY claim is refused.
        assert_eq!(outcome.verdict, Verdict::Matched);
        assert_eq!(
            outcome.compared_log_messages,
            Some(ComparedLogCounts { left: 0, right: 0 })
        );
        // The spec still reports a fully-qualifying policy...
        assert!(outcome.comparison.is_bitwise_parity());
        // ...and that is exactly why the count is load-bearing.
        let report = verification_report(&outcome);
        assert!(report.verified);
        assert!(
            !report.bitwise_parity,
            "zero compared log messages must never certify bitwise parity"
        );
    }

    /// FINDING 1. Every early exit must leave an invocation-bound record: the
    /// pending stamp overwrites a previous invocation's green, so a stale
    /// `{verified:true}` can never be read as this run's result.
    /// Plant a previous invocation's GREEN verdict, the way a caller reusing one
    /// `--verify-json` path across runs leaves it.
    fn plant_previous_green(path: &Path) {
        fs::write(
            path,
            "{\"verified\":true,\"bitwise_parity\":true,\"verdict\":\"matched\"}\n",
        )
        .unwrap();
    }

    fn read_verdict(path: &Path) -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    /// The staging directory must always be the TARGET's directory, never the
    /// system temp directory.
    ///
    /// `Path::parent` yields an EMPTY path for a bare filename, and the earlier
    /// code treated that as "no directory" and staged in `TMPDIR`. `persist`
    /// then renames across filesystems whenever `TMPDIR` and the working
    /// directory differ -- tmpfs `/tmp` beside a btrfs checkout is the ordinary
    /// case here -- which fails `EXDEV`, so the record is never written and the
    /// PREVIOUS invocation's `{verified:true}` survives. Exactly the stale green
    /// this whole change exists to remove, reachable with
    /// `--verify-json=verdict.json`.
    #[test]
    fn a_bare_filename_stages_beside_its_target_not_in_the_system_temp_dir() {
        // The regression: a bare filename resolves to the working directory.
        assert_eq!(
            staging_directory(Path::new("verdict.json")),
            Path::new("."),
            "a bare filename must stage in the working directory; staging in \
             TMPDIR makes persist() a cross-filesystem rename"
        );
        assert_eq!(
            staging_directory(Path::new("./verdict.json")),
            Path::new(".")
        );

        // The control: a path that DOES name a directory still uses it, so the
        // fix is a corrected fallback rather than a blanket redirect to `.`.
        assert_eq!(
            staging_directory(Path::new("/tmp/run/verdict.json")),
            Path::new("/tmp/run")
        );
        assert_eq!(
            staging_directory(Path::new("sub/verdict.json")),
            Path::new("sub")
        );

        // Whatever it returns must never be empty: NamedTempFile::new_in("")
        // fails, which would turn every write into an error.
        for candidate in ["verdict.json", "./v.json", "/tmp/run/v.json", "sub/v.json"] {
            assert!(
                !staging_directory(Path::new(candidate))
                    .as_os_str()
                    .is_empty(),
                "{candidate}: staging directory must be usable"
            );
        }
    }

    /// End-to-end for the same defect: a BARE filename target, with a previous
    /// green already at it, must be replaced by the no-result stamp.
    ///
    /// Runs from a temporary working directory so the target really is a bare
    /// relative name. Under the old code this failed on any host where the
    /// working directory and `TMPDIR` are on different filesystems.
    #[test]
    fn a_bare_filename_target_is_overwritten_not_left_stale() {
        // `set_current_dir` is process-global; serialize against any other test
        // that touches it.
        static CWD: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = CWD.lock().unwrap_or_else(|e| e.into_inner());

        // Root the working directory in the SOURCE TREE, not in TMPDIR. A
        // plain `tempdir()` lands beside the staged file and the cross-
        // filesystem rename never happens, so the test would pass against the
        // defect -- measured: it did. On this host the checkout and /tmp are
        // distinct btrfs subvolumes (st_dev 46 vs 47), and cross-subvolume
        // rename(2) is EXDEV, which is what makes this end-to-end rather than
        // decorative.
        let dir = tempfile::Builder::new()
            .prefix("verify-json-bare-")
            .tempdir_in(env!("CARGO_MANIFEST_DIR"))
            .unwrap();
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        let outcome = (|| {
            let bare = Path::new("verdict.json");
            plant_previous_green(bare);
            write_pending_verification_json(bare)?;
            Ok::<_, Error>(read_verdict(bare))
        })();

        std::env::set_current_dir(previous).unwrap();
        let now = outcome.expect("staging beside a bare filename must succeed");
        assert_eq!(now["verdict"], serde_json::json!("no_result"));
        assert_eq!(now["verified"], serde_json::json!(false));
    }

    #[test]
    fn pending_stamp_overwrites_a_previous_green_verdict() {
        let file = NamedTempFile::new().unwrap();
        let path = file.path().to_path_buf();

        // A previous, successful invocation left a green record at this path.
        let out = output(0, b"hello\n", b"");
        let (log1, log2) = logs_with_identical_detlog();
        let good = compare_with(&out, log1, &out, log2, LogCompareStrictness::Canonical).unwrap();
        write_verification_json(&path, &good).unwrap();
        let previous: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(previous["verified"], serde_json::json!(true));
        assert_eq!(previous["bitwise_parity"], serde_json::json!(true));

        // A new invocation begins and will abort before reaching a verdict.
        write_pending_verification_json(&path).unwrap();

        let now: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(now["verdict"], serde_json::json!("no_result"));
        assert_eq!(now["verified"], serde_json::json!(false));
        assert_eq!(now["bitwise_parity"], serde_json::json!(false));
        assert_eq!(now["comparison"], serde_json::Value::Null);
        assert_eq!(now["compared_log_messages"], serde_json::Value::Null);
        assert!(
            now.get("dbt_counted_branches").is_none(),
            "no-result must not claim that a DBT branch-clock comparison completed"
        );
    }

    #[test]
    fn shared_report_preserves_the_current_no_result_bytes() {
        let encoded = serde_json::to_string(&VerificationReport::no_result()).unwrap();
        assert_eq!(
            encoded,
            concat!(
                r#"{"verified":false,"bitwise_parity":false,"verdict":"no_result","#,
                r#""no_result_reason":{"kind":"not_run"},"infrastructure_error":null,"comparison":null,"#,
                r#""compared_log_messages":null,"guest_exit_code":null,"guest_signal":null,"#,
                r#""first_divergent_scheduler_turn":null,"#,
                r#""first_divergent_virtual_nanoseconds":null,"#,
                r#""first_divergent_record":null,"first_divergent_syscall":null,"#,
                r#""first_divergent_left_message":null,"first_divergent_right_message":null}"#,
            )
        );
    }

    /// The positive side of FINDING 1: the pending stamp is not a dead end --
    /// a real verdict still publishes over it.
    #[test]
    fn terminal_verdict_replaces_the_pending_stamp() {
        let file = NamedTempFile::new().unwrap();
        let path = file.path().to_path_buf();

        write_pending_verification_json(&path).unwrap();
        let out = output(0, b"hello\n", b"");
        let (log1, log2) = logs_with_identical_detlog();
        let outcome =
            compare_with(&out, log1, &out, log2, LogCompareStrictness::Canonical).unwrap();
        write_verification_json(&path, &outcome).unwrap();

        let published: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(published["verdict"], serde_json::json!("matched"));
        assert_eq!(published["verified"], serde_json::json!(true));
        assert_eq!(published["bitwise_parity"], serde_json::json!(true));
    }

    #[test]
    fn skid_overshoot_refusal_retains_the_completed_comparison() {
        let file = NamedTempFile::new().unwrap();
        let path = file.path().to_path_buf();
        let out = output(0, b"hello\n", b"");
        let (log1, log2) = logs_with_identical_detlog();
        let outcome =
            compare_with(&out, log1, &out, log2, LogCompareStrictness::Canonical).unwrap();

        write_skid_overshoot_verification_json(&path, &outcome, 2).unwrap();

        let report = VerificationReport::from_current_json_value(
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(report.verdict, Verdict::InfrastructureError);
        assert_eq!(
            report.infrastructure_error,
            Some(InfrastructureError::SkidOvershoot { count: 2 })
        );
        assert!(!report.verified);
        assert!(!report.bitwise_parity);
        assert!(report.comparison.is_some());
        assert!(report.compared_log_messages.is_some());
        assert_eq!(report.guest_exit_code, Some(0));
    }

    /// An overshoot that coincides with a refused comparison is written as
    /// the refusal. The log cut at its size bound is a separate failure that
    /// a fresh attempt must not be allowed to hide, and the report schema
    /// allows its reason only beside a `no_result` verdict
    /// (<https://github.com/rrnewton/hermit/issues/1845>).
    ///
    /// That holds whichever verdict the comparator selected: with identical
    /// outputs it selects `no_result`, and when an output or exit-status
    /// difference is also observed it selects `diverged` and its ordinary
    /// report carries no refusal reason. A divergence with intact logs is the
    /// control: it has no refusal to keep, so the overshoot report is written.
    #[test]
    fn skid_overshoot_refusal_keeps_a_refused_comparisons_reason() {
        let same = output(0, b"hello\n", b"");
        for (label, right, comparator_verdict) in [
            ("identical outputs", same.clone(), Verdict::NoResult),
            ("stdout", output(0, b"goodbye\n", b""), Verdict::Diverged),
            (
                "stderr",
                output(0, b"hello\n", b"oops\n"),
                Verdict::Diverged,
            ),
            ("exit status", output(3, b"hello\n", b""), Verdict::Diverged),
        ] {
            let file = NamedTempFile::new().unwrap();
            let path = file.path().to_path_buf();
            let (log1, log2) = logs_truncated_at_the_bound();
            let outcome = compare(&same, log1, &right, log2).unwrap();
            assert_eq!(outcome.verdict, comparator_verdict, "{label}");
            if comparator_verdict == Verdict::Diverged {
                // Without an overshoot nothing changes: the divergence is the
                // report, as `an_observed_difference_outranks_a_refused_comparison`
                // requires.
                let ordinary = verification_report(&outcome);
                assert_eq!(ordinary.verdict, Verdict::Diverged, "{label}");
                assert_eq!(ordinary.no_result_reason, None, "{label}");
            }

            write_skid_overshoot_verification_json(&path, &outcome, 3).unwrap();

            let report = VerificationReport::from_current_json_value(
                serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap(),
            )
            .unwrap();
            assert_eq!(report.verdict, Verdict::NoResult, "{label}");
            assert_eq!(report.infrastructure_error, None, "{label}");
            assert!(
                matches!(
                    report.no_result_reason.as_ref(),
                    Some(NoResultReason::ComparisonRefused { detail })
                        if detail.contains("truncated at the configured size bound")
                ),
                "{label}: the overshoot report must keep the comparator's refusal: {report:?}"
            );
            assert!(!report.verified, "{label}");
            assert!(!report.bitwise_parity, "{label}");
            assert_eq!(report.comparison, None, "{label}");
            assert_eq!(report.compared_outputs, None, "{label}");
        }

        let file = NamedTempFile::new().unwrap();
        let (log1, log2) = empty_logs();
        let outcome = compare(&same, log1, &output(0, b"goodbye\n", b""), log2).unwrap();
        assert_eq!(outcome.verdict, Verdict::Diverged);
        assert_eq!(outcome.refused_log_comparison, None);
        write_skid_overshoot_verification_json(file.path(), &outcome, 3).unwrap();
        let report = VerificationReport::from_current_json_value(
            serde_json::from_str(&fs::read_to_string(file.path()).unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(report.verdict, Verdict::InfrastructureError);
        assert_eq!(
            report.infrastructure_error,
            Some(InfrastructureError::SkidOvershoot { count: 3 })
        );
        assert_eq!(report.no_result_reason, None);
    }

    #[test]
    fn skid_overshoot_before_comparison_is_not_reported_as_no_result() {
        let file = NamedTempFile::new().unwrap();
        write_skid_overshoot_without_comparison_json(
            file.path(),
            1,
            None,
            Some(ExitStatus::Exited(7)),
        )
        .unwrap();

        let report = VerificationReport::from_current_json_value(
            serde_json::from_str(&fs::read_to_string(file.path()).unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(report.verdict, Verdict::InfrastructureError);
        assert_eq!(
            report.infrastructure_error,
            Some(InfrastructureError::SkidOvershoot { count: 1 })
        );
        assert!(report.no_result_reason.is_none());
        assert!(report.comparison.is_none());
        assert_eq!(report.guest_exit_code, Some(7));
    }

    #[test]
    fn verification_report_json_carries_the_comparison() {
        let out = output(0, b"hello\n", b"");
        // NONEMPTY logs: parity may only be claimed when the comparison had
        // data. This test previously used `empty_logs()` and asserted
        // bitwise_parity = true, which codified a green over ZERO compared
        // events -- see `empty_log_comparison_matches_but_is_never_parity`.
        let (log1, log2) = logs_with_identical_detlog();
        let outcome =
            compare_with(&out, log1, &out, log2, LogCompareStrictness::Canonical).unwrap();

        let json = serde_json::to_string(&verification_report(&outcome)).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["verified"], serde_json::json!(true));
        assert_eq!(parsed["verdict"], serde_json::json!("matched"));
        assert!(
            parsed.get("dbt_counted_branches").is_none(),
            "non-DBT reports must preserve the pre-field JSON shape"
        );
        assert_eq!(
            parsed["first_divergent_scheduler_turn"],
            serde_json::Value::Null
        );
        assert_eq!(
            parsed["first_divergent_virtual_nanoseconds"],
            serde_json::Value::Null
        );
        // The executed-work evidence travels with the verdict.
        assert!(parsed["compared_log_messages"]["left"].as_u64().unwrap() > 0);
        assert!(parsed["compared_log_messages"]["right"].as_u64().unwrap() > 0);
        // The single boolean a parity ratchet keys on: a matched, full-INFO,
        // unstripped comparison under a named canonical record envelope.
        assert_eq!(parsed["bitwise_parity"], serde_json::json!(true));
        assert_eq!(
            parsed["comparison"]["strictness"],
            serde_json::json!("canonical")
        );
        assert_eq!(
            parsed["comparison"]["display_name"],
            serde_json::json!("BitwiseInfoV1")
        );
        assert_eq!(
            parsed["comparison"]["strip_lines"],
            serde_json::json!(false)
        );
        assert_eq!(parsed["comparison"]["full_trace"], serde_json::json!(true));
        assert_eq!(parsed["comparison"]["log_scope"], serde_json::json!("info"));
        assert_eq!(
            parsed["comparison"]["record_envelope"],
            serde_json::json!("all_records_v1")
        );
        assert_eq!(
            parsed["comparison"]["compare_logs"],
            serde_json::json!(true)
        );
        // The contract's remaining clauses ("no ignore/skip filters") are carried
        // too, so a consumer can require their absence rather than assume it.
        assert_eq!(
            parsed["comparison"]["ignore_lines"],
            serde_json::json!(false)
        );
        assert_eq!(
            parsed["comparison"]["skip_commit"],
            serde_json::json!(false)
        );
        assert_eq!(
            parsed["comparison"]["skip_detlog"],
            serde_json::json!(false)
        );
    }

    // The bitwise-parity acceptance contract: a consumer must accept a `Matched`
    // as true bitwise parity ONLY under a full-INFO, unstripped, named canonical
    // record envelope and reject every weaker or opaque policy. This brackets
    // both sides: each canonical envelope fires, while stripped, output-only,
    // caller-defined, and ad hoc ignore/skip variants are refused.
    /// A GREEN REPORT SAYS WHAT IT COMPARED AND WHETHER TIME WAS VIRTUAL — AND A
    /// REPORT THAT COMPARED NOTHING SAYS NEITHER.
    ///
    /// Ported from the residual of hermit#2269. `bitwise_parity: true` meant two
    /// different things depending on the subcommand and the report could not tell
    /// them apart: `run --verify` executes the guest twice with virtual time on,
    /// so a match is evidence about the guest, while `record start --verify` runs
    /// with `virtualize_time: false`, so a match says only that the replay
    /// reproduced that recording. The prose in `record start --help` says this;
    /// until now the ARTIFACT did not.
    ///
    /// ⚠️ THE FIRST ASSERTION IS THE POINT: parity and virtual time are
    /// INDEPENDENT. A comparison can qualify for bitwise parity with
    /// `virtualize_time: false`, and that combination is exactly the one a
    /// consumer must not read as a determinism result.
    ///
    /// The second half is why the field lives on `ComparisonSpec` and not on the
    /// report: `comparison` is `None` for `NoResult`, so a run that compared
    /// nothing cannot publish a time policy describing a comparison that did not
    /// happen. Putting it on the report would have re-created, one field over, the
    /// "value present, meaning absent" defect that hermit#2498's typed
    /// `no_result_reason` had just closed.
    #[test]
    fn a_green_report_says_what_it_compared_and_whether_time_was_virtual() {
        for virtualize_time in [true, false] {
            let spec = ComparisonSpec::new(
                LogCompareStrictness::Canonical,
                true,
                false,
                true,
                RecordEnvelopePolicy::AllRecordsV1,
                virtualize_time,
            );
            assert!(
                spec.is_bitwise_parity(),
                "parity must not depend on the time policy: the record/replay path \
                 qualifies for parity while running with virtualize_time false"
            );
            let json = serde_json::to_value(comparison_report(&spec)).unwrap();
            assert_eq!(
                json["virtualize_time"],
                serde_json::json!(virtualize_time),
                "a comparison that ran must publish the time policy it ran under"
            );
        }

        let nothing_compared = serde_json::to_value(VerificationReport::no_result()).unwrap();
        assert!(
            nothing_compared["comparison"].is_null(),
            "no verdict was reached, so no comparison may be described"
        );
        assert!(
            nothing_compared.get("virtualize_time").is_none(),
            "a run that compared nothing must not publish a time policy at all"
        );
    }

    #[test]
    fn bitwise_parity_contract_accepts_only_named_canonical_envelopes() {
        // Positive: the exact qualifying comparison the `--verify-strict` path
        // produces.
        let full = ComparisonSpec::new(
            LogCompareStrictness::Canonical,
            true,
            false,
            true,
            RecordEnvelopePolicy::AllRecordsV1,
            true,
        );
        assert!(
            full.is_bitwise_parity(),
            "a full-INFO unstripped all-records comparison must qualify"
        );
        assert!(
            ComparisonSpec {
                record_envelope: RecordEnvelopePolicy::DbtEvidenceTransportV1,
                ..full
            }
            .is_bitwise_parity(),
            "the named DBT transport envelope is a disclosed canonical policy"
        );

        // Negatives: each independent weakening of the qualifying spec must be
        // refused, so no single relaxed dimension can pass as bitwise parity.
        let stripped = ComparisonSpec::new(
            LogCompareStrictness::Stripped,
            true,
            false,
            false,
            RecordEnvelopePolicy::AllRecordsV1,
            true,
        );
        assert!(
            !stripped.is_bitwise_parity(),
            "a stripped comparison normalizes away the parity-relevant data"
        );

        let missing_io_buffers = ComparisonSpec {
            compare_io_buffers: false,
            ..full
        };
        assert!(
            !missing_io_buffers.is_bitwise_parity(),
            "a comparison without syscall output-buffer content is not bitwise parity"
        );

        let output_only = ComparisonSpec {
            compare_logs: false,
            ..full
        };
        assert!(
            !output_only.is_bitwise_parity(),
            "an output-only fallback never compared the log stream"
        );

        for weakened in [
            ComparisonSpec {
                ignore_lines: true,
                ..full
            },
            ComparisonSpec {
                skip_commit: true,
                ..full
            },
            ComparisonSpec {
                skip_detlog: true,
                ..full
            },
            // full_trace off (Deterministic-mode subset) is also below bitwise.
            ComparisonSpec {
                full_trace: false,
                ..full
            },
            ComparisonSpec {
                log_scope: ComparedLogScope::Deterministic,
                ..full
            },
            ComparisonSpec {
                record_envelope: RecordEnvelopePolicy::CallerDefined,
                ..full
            },
        ] {
            assert!(
                !weakened.is_bitwise_parity(),
                "a filtered/subset comparison must not pass as bitwise parity: {weakened:?}"
            );
        }

        // A divergence is never bitwise parity even under the qualifying spec: the
        // report's boolean is the conjunction of the verdict and the contract.
        let diverged = VerificationOutcome {
            verdict: Verdict::Diverged,
            no_result_reason: None,
            refused_log_comparison: None,
            guest_status: ExitStatus::Exited(0),
            comparison: full,
            compared_log_messages: Some(ComparedLogCounts { left: 9, right: 9 }),
            compared_outputs: ComparedOutputs {
                left: ComparedOutput {
                    exit_code: Some(0),
                    signal: None,
                    stdout_sha256: detcore::Digest::new(b"same stdout").to_string(),
                    stdout_bytes: 11,
                    stderr_sha256: detcore::Digest::new(b"").to_string(),
                    stderr_bytes: 0,
                },
                right: ComparedOutput {
                    exit_code: Some(0),
                    signal: None,
                    stdout_sha256: detcore::Digest::new(b"different stdout").to_string(),
                    stdout_bytes: 16,
                    stderr_sha256: detcore::Digest::new(b"").to_string(),
                    stderr_bytes: 0,
                },
            },
            dbt_counted_branches: None,
            runtime: None,
            first_divergent_scheduler_turn: None,
            first_divergent_record_positions: None,
            first_divergent_virtual_nanoseconds: None,
            first_divergent_record: None,
            first_divergent_syscall: None,
            first_divergent_left_message: None,
            first_divergent_right_message: None,
        };
        assert!(!verification_report(&diverged).bitwise_parity);
    }

    #[test]
    fn report_carries_both_run_runtime_totals_without_changing_the_verdict() {
        let first = RunSummary {
            sched_turns: 12,
            virttime_elapsed: 34,
            syscalls: Some(5),
            ..Default::default()
        };
        let second = RunSummary {
            sched_turns: 13,
            virttime_elapsed: 35,
            syscalls: Some(6),
            ..Default::default()
        };
        let runtime = verification_runtime_from_summaries(Some(&first), Some(&second))
            .expect("two summaries produce runtime totals");
        assert_eq!(runtime.run1.as_ref().unwrap().scheduler_turns, 12);
        assert_eq!(runtime.run2.as_ref().unwrap().syscalls, Some(6));

        let mut report = VerificationReport::no_result();
        report.runtime = Some(runtime);
        let json = serde_json::to_value(report).unwrap();
        assert_eq!(json["verdict"], "no_result");
        assert_eq!(json["runtime"]["run1"]["virtual_nanoseconds"], 34);
        assert_eq!(json["runtime"]["run2"]["syscalls"], 6);
        assert!(json["runtime"]["run1"].get("dispatch").is_none());
    }

    #[test]
    fn report_carries_each_run_dispatch_record() {
        let record = reverie::DispatchStats::new(
            "ptrace",
            reverie::DispatchCounters {
                ptrace_seccomp_stops: Some(9),
                ..reverie::DispatchCounters::ZERO
            },
            reverie::SiteCounters::NONE_PATCHED,
        );
        let first = RunSummary {
            dispatch_stats: Some(record.clone()),
            ..Default::default()
        };
        let runtime = verification_runtime_from_summaries(Some(&first), None)
            .expect("one summary produces runtime totals");
        assert_eq!(runtime.run1.unwrap().dispatch, Some(record.clone()));

        let mut report = VerificationReport::no_result();
        report.runtime = verification_runtime_from_summaries(Some(&first), Some(&first));
        let json = serde_json::to_value(report).unwrap();
        assert_eq!(json["runtime"]["run2"]["dispatch"]["backend"], "ptrace");
        assert_eq!(
            json["runtime"]["run2"]["dispatch"]["schema_version"],
            reverie::DISPATCH_STATS_SCHEMA_VERSION
        );
    }

    #[test]
    fn dbt_transport_only_is_disclosed_and_cannot_qualify_as_parity() {
        let output = output(0, b"hello\n", b"");
        let (left, right) = empty_logs();
        let transport = "1970-01-01T00:00:00.000000Z INFO reverie_dbt::evidence: protected evidence initialized\n";
        fs::write(&left, transport).unwrap();
        fs::write(&right, transport).unwrap();

        let outcome = compare_with_envelope(
            &output,
            left,
            &output,
            right,
            LogCompareStrictness::Canonical,
            RecordEnvelope::dbt_evidence_transport_v1(),
        )
        .unwrap();
        let report = verification_report(&outcome);

        assert_eq!(outcome.verdict, Verdict::Matched);
        assert_eq!(
            outcome.compared_log_messages,
            Some(ComparedLogCounts { left: 0, right: 0 })
        );
        assert_eq!(
            outcome.comparison.record_envelope,
            RecordEnvelopePolicy::DbtEvidenceTransportV1
        );
        assert!(!report.bitwise_parity);
    }

    #[test]
    fn dbt_real_detcore_record_matches_under_the_disclosed_envelope() {
        let output = output(0, b"hello\n", b"");
        let (left, right) = empty_logs();
        let record = detcore::detlog::record_suffix(detcore::detlog::DetLogEvent::Syscall);
        let log = format!(
            "1970-01-01T00:00:00.000000Z INFO reverie_dbt::evidence: protected evidence initialized\n\
             2026-08-21T10:00:00.000000Z INFO detcore: DETLOG [syscall] getpid() = Ok(3){record}\n"
        );
        fs::write(&left, &log).unwrap();
        fs::write(&right, &log).unwrap();

        let outcome = compare_with_envelope(
            &output,
            left,
            &output,
            right,
            LogCompareStrictness::Canonical,
            RecordEnvelope::dbt_evidence_transport_v1(),
        )
        .unwrap();
        let report = verification_report(&outcome);
        let json = serde_json::to_value(&report).unwrap();

        assert_eq!(
            outcome.compared_log_messages,
            Some(ComparedLogCounts { left: 1, right: 1 })
        );
        assert!(report.bitwise_parity);
        assert_eq!(
            json["comparison"]["record_envelope"],
            "dbt_evidence_transport_v1"
        );
    }

    #[test]
    fn caller_defined_record_filter_is_disclosed_but_never_parity() {
        fn keep_everything(_record: &str) -> bool {
            true
        }

        let output = output(0, b"hello\n", b"");
        let (left, right) = logs_with_identical_detlog();
        let outcome = compare_with_envelope(
            &output,
            left,
            &output,
            right,
            LogCompareStrictness::Canonical,
            RecordEnvelope::caller_defined(keep_everything),
        )
        .unwrap();
        let report = verification_report(&outcome);
        let json = serde_json::to_value(&report).unwrap();

        assert!(outcome.verified());
        assert_eq!(
            outcome.comparison.record_envelope,
            RecordEnvelopePolicy::CallerDefined
        );
        assert!(!report.bitwise_parity);
        assert_eq!(json["comparison"]["record_envelope"], "caller_defined");
    }

    // Binds the `ComparisonSpec::new` no-filter assumption (and the
    // `compare_two_runs` debug_assert) to reality: the diff engine's default must
    // actually apply no line filters. If a future default started filtering, the
    // spec would silently misreport "no filters" — this catches that.
    #[test]
    fn default_log_diff_opts_apply_no_line_filters() {
        let default = logdiff::LogDiffOpts::default();
        assert!(default.ignore_lines.is_empty());
        assert!(!default.skip_commit);
        assert!(!default.skip_detlog);
    }
}
