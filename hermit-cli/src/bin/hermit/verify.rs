/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeSet;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use colored::Colorize;
use detcore::logdiff;
use detcore::logdiff::LogComparisonMode;
use hermit::Context;
use hermit::Error;
use pretty_assertions::Comparison;
use reverie::process::ExitStatus;
use reverie::process::Output;
use serde::Serialize;
use tempfile::NamedTempFile;
use tempfile::TempPath;
use tracing::metadata::LevelFilter;

use super::global_opts::GlobalOpts;

pub(crate) struct ComparedRun<'a> {
    pub output: &'a Output,
    pub log: TempPath,
}

pub(crate) struct ComparisonOptions<'a> {
    pub success_message: &'a str,
    pub failure_message: &'a str,
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
}

/// How strictly two runs' internal logs are compared — the condition a
/// [`Verdict`] rests on.
///
/// A bare "matched" verdict is meaningless without this. The two modes sit at
/// opposite ends of a three-tier treatment of log data:
///
/// - [`Self::Stripped`] normalizes away numeric values, addresses, tmp paths,
///   and — most importantly — the virtual-time timestamps and syscall
///   argument/result values that parity exists to check, so a `Matched` verdict
///   under `Stripped` asserts only "matched after normalizing known-
///   nondeterministic data", NOT parity. STRIPPING DESTROYS THE ABILITY TO
///   DETECT A DIFFERENCE.
/// - [`Self::Canonical`] is the parity mode. It applies exactly three tiers:
///   (1) STRIP the real wall-clock timestamp PREFIX only (genuinely
///   irreproducible; done by `extract_log_messages`); (2) CANONICALIZE host
///   memory addresses to an ordinal by first appearance (1, 2, 3…), preserving
///   identity, ordering, and aliasing while discarding only the host-specific
///   raw pointer; (3) COMPARE EXACTLY everything else — virtual-time timestamps,
///   syscall inputs/results, counts, sizes, flags. CANONICALIZING PRESERVES the
///   ability to detect a difference (allocation-order and aliasing changes still
///   diverge), which is the whole point.
///
/// Carrying the strictness beside the verdict is the same discipline as
/// recording the `-j` a byte count was measured at: the value is uninterpretable
/// without the condition that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogCompareStrictness {
    /// `strip_lines = true`, comparing the deterministic Detcore/scheduler
    /// message subset. Tolerant of limited nondeterminism (numbers, addresses,
    /// tmp paths, and timestamps are normalized before diffing). NOT a parity
    /// claim.
    Stripped,
    /// The parity mode (`BitwiseInfoV1`): `strip_lines = false` and
    /// `canonicalize_addresses = true`, comparing the full captured INFO trace.
    /// Only the real wall-clock timestamp prefix is stripped and host addresses
    /// are canonicalized to first-appearance ordinals; every other byte —
    /// virtual-time timestamps, raw syscall argument/result values, counts,
    /// sizes, flags — must match exactly.
    Canonical,
}

/// Which captured messages actually participated in the log comparison.
///
/// This travels in the typed report so an INFO-parity consumer never has to
/// infer the observation envelope from the requested logging verbosity. In
/// particular, explicitly capturing DEBUG does not silently promote those
/// diagnostics into the `BitwiseInfoV1` verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ComparedLogScope {
    /// The legacy selected DETLOG/scheduler subset used by stripped verification.
    Deterministic,
    /// Every INFO message, exactly; DEBUG/TRACE captures remain diagnostic.
    Info,
    /// Every captured message, selected only by explicit diagnostic verification.
    FullTrace,
}

/// Versioned policy token: the only strippable datum is the real wall-clock
/// timestamp PREFIX. Recorded in [`ComparisonSpec::stripped_prefixes`] so a
/// consumer sees exactly which prefixes were removed, not a bare boolean.
pub const STRIP_WALL_CLOCK_PREFIX_V1: &str = "real-wall-clock-prefix/v1";

/// Versioned policy token: host memory addresses are canonicalized to an ordinal
/// by first appearance (identity/order/aliasing preserved). Recorded in
/// [`ComparisonSpec::canonicalizations`].
pub const CANON_ADDRESS_ORDINAL_V1: &str = "host-address-to-first-appearance-ordinal/v1";

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ComparisonSpec {
    /// The strictness label the comparison ran under.
    pub strictness: LogCompareStrictness,
    /// Whether the internal event stream was compared at all. When
    /// `false` (e.g. KVM concurrent mode) only stdout/stderr/exit status were
    /// compared and the strictness fields describe a log comparison that did not
    /// run — a consumer must not read such a verdict as bitwise log parity.
    pub compare_logs: bool,
    /// The message envelope selected from the captured log. The compared-message
    /// counts refer exactly to this scope.
    pub log_scope: ComparedLogScope,
    /// Concrete: were numeric values, addresses, tmp paths, and timestamps
    /// normalized away wholesale before diffing (the lossy [`Stripped`] path)?
    ///
    /// [`Stripped`]: LogCompareStrictness::Stripped
    pub strip_lines: bool,
    /// Concrete: were host memory addresses canonicalized to first-appearance
    /// ordinals (the tier-2 step of the parity policy) before diffing? Unlike
    /// [`Self::strip_lines`] this is lossless for parity: it discards only the
    /// raw pointer value, keeping identity, order, and aliasing.
    pub canonicalize_addresses: bool,
    /// Concrete: was the complete parity observation envelope compared (vs. the
    /// legacy deterministic subset)? For `BitwiseInfoV1`, that complete envelope
    /// is INFO and [`Self::log_scope`] records whether the explicit diagnostic
    /// full-trace superset was requested.
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
    ) -> Self {
        // Map the strictness label onto the concrete diff flags AND the versioned
        // policy tokens in one place, so the flags the engine sees, the tokens
        // the verdict reports, and the strictness label can never drift apart.
        let (strip_lines, canonicalize_addresses, full_trace, exact_remainder, log_scope) =
            match strictness {
                // Lossy wholesale normalization: numbers/addresses/paths/timestamps
                // erased; the remainder is NOT compared exactly.
                LogCompareStrictness::Stripped => {
                    debug_assert!(!diagnostic_full_trace);
                    (true, false, false, false, ComparedLogScope::Deterministic)
                }
                // Parity (BitwiseInfoV1): strip only the wall-clock prefix,
                // canonicalize addresses, and compare every INFO message exactly.
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
                ),
            };
        let (stripped_prefixes, canonicalizations): (&[&str], &[&str]) = match strictness {
            LogCompareStrictness::Stripped => (
                &[STRIP_WALL_CLOCK_PREFIX_V1, STRIP_UNSAFE_NORMALIZATION_V1],
                // Under stripping, addresses are ERASED (to a single `<ADDR>`
                // token), not canonicalized; there is no ordinal preserved.
                &[],
            ),
            LogCompareStrictness::Canonical => (PARITY_STRIPPED_PREFIXES, PARITY_CANONICALIZATIONS),
        };
        ComparisonSpec {
            strictness,
            compare_logs,
            log_scope,
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

    /// Does this comparison satisfy the `BitwiseInfoV1` parity contract a
    /// determinism / record-replay ratchet must require before it may read a
    /// `Matched` verdict as *true parity*? A bare `verified` is not enough:
    /// `verified` can rest on a stripped compare, a filtered subset, or an
    /// output-only fallback, all of which normalize or omit exactly the data
    /// (virtual-time timestamps, raw syscall argument/result values, whole event
    /// classes) that parity exists to check.
    ///
    /// This requires the EXACT `BitwiseInfoV1` policy shape, not merely
    /// "not stripped": a generic `strip_lines = false` is inadmissible on its
    /// own. All clauses must hold:
    /// - the full INFO event stream (or the explicit all-level diagnostic
    ///   superset) was compared ([`Self::full_trace`] and [`Self::log_scope`]),
    ///   which carries exact virtual timestamps and syscall argument/result values;
    /// - no lossy wholesale normalization ran (`!strip_lines`) and the remainder
    ///   was compared exactly ([`Self::exact_remainder`]);
    /// - addresses were CANONICALIZED, not erased ([`Self::canonicalize_addresses`]),
    ///   so allocation-order and aliasing differences are still detectable;
    /// - the versioned policy tokens are exactly the parity set — only the
    ///   wall-clock prefix stripped, only address-ordinal canonicalization
    ///   applied — so a future extra strip/canonicalization cannot silently pass;
    /// - no ignore/skip filter dropped any line or event class
    ///   (`!ignore_lines && !skip_commit && !skip_detlog`);
    /// - the internal log stream was actually compared, not skipped for an
    ///   output-only fallback ([`Self::compare_logs`]).
    ///
    /// A consumer asking for parity must reject `Matched` under every weaker
    /// comparison; this predicate is that single acceptance rule.
    pub fn is_bitwise_parity(&self) -> bool {
        self.compare_logs
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
            && !self.ignore_lines
            && !self.skip_commit
            && !self.skip_detlog
    }
}

/// The verification verdict: did the two runs match?
///
/// This is deliberately distinct from the guest's exit status. The process exit
/// code of a `--verify` run historically encodes *the guest's* exit status (so
/// `record start --verify -- prog` behaves like `prog` for the common exit-0
/// case), which conflates two independent facts: "did the two runs match" and
/// "what did the guest exit with". A guest that deterministically exits nonzero
/// (e.g. `/bin/false`) makes a *passing* verification exit nonzero; symmetrically
/// a guest that exits zero while its runs diverge could only be told apart from a
/// match by scraping the human-readable banner. Carrying the verdict as its own
/// typed value removes that inference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// The two runs matched on every compared dimension (stdout, stderr, exit
    /// status, and — unless disabled — the internal DETLOG event stream).
    Matched,
    /// The two runs diverged; verification failed.
    Diverged,
    /// Verification did not reach a verdict: the invocation aborted before the
    /// two runs could be compared (a run failed to start, the first run's exit
    /// status was rejected, SaBRe captured no DETLOG, recording failed, ...).
    ///
    /// This is NOT a synonym for `Diverged`. It exists so the `--verify-json`
    /// artifact always describes *this* invocation: without an explicit
    /// no-result state, an early abort would leave whatever the file previously
    /// contained -- including an older `{verified: true}` -- readable as though
    /// it described the run that just failed.
    NoResult,
}

/// How much log evidence the comparison actually consumed.
///
/// A configured-strict comparison proves nothing if it had no data: two empty
/// selections "match" trivially. Carrying the counts with the verdict is what
/// lets a parity consumer require nonzero executed work, exactly as a test
/// result must carry its executed count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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

/// The full outcome of comparing two runs: the verification [`Verdict`] plus the
/// guest exit status, so a caller never has to infer either one from the other.
#[derive(Debug, Clone)]
pub struct VerificationOutcome {
    pub verdict: Verdict,
    /// Exit status of the second (replay / repeat) run, propagated verbatim.
    pub guest_status: ExitStatus,
    /// The exact comparison that produced [`Self::verdict`], carried so a
    /// consumer never has to assume which comparison a "matched" rests on.
    pub comparison: ComparisonSpec,
    /// How many log messages the comparison actually compared, or `None` when
    /// the log comparison was not run at all (output-only fallback). `None` and
    /// `Some(0/0)` are both "no log evidence" and neither can support parity.
    pub compared_log_messages: Option<ComparedLogCounts>,
}

impl VerificationOutcome {
    /// Did verification pass, independent of the guest exit code?
    pub fn verified(&self) -> bool {
        self.verdict == Verdict::Matched
    }

    /// Collapse the outcome to the historical process-exit convention: a match
    /// propagates the guest exit status; a divergence is an error (nonzero
    /// exit). Callers that need to separate the verdict from the guest exit
    /// code must read [`Self::verdict`] / [`Self::verified`] (or the
    /// `--verify-json` report) *before* calling this.
    pub fn into_exit_status(self) -> Result<ExitStatus, Error> {
        match self.verdict {
            Verdict::Matched => Ok(self.guest_status),
            Verdict::Diverged => Err(Error::msg(
                "Mismatch between run 1 and run 2 outputs (logs retained).",
            )),
            // Unreachable in practice: `compare_two_runs` only ever yields
            // Matched/Diverged, and the no-result state is published directly to
            // the JSON artifact rather than carried in an outcome. Fail closed
            // rather than mapping "no verdict" onto a guest exit status.
            Verdict::NoResult => Err(Error::msg(
                "Verification did not reach a verdict (no comparison was performed).",
            )),
        }
    }
}

/// Machine-readable verification report written by `--verify-json`.
///
/// Every field carries the condition it describes: `verified`/`verdict` is the
/// verification result, `comparison` is the comparison that produced it, and
/// `guest_exit_code`/`guest_signal` describe the guest's own termination. A
/// consumer keys its decision on `verified` — but a *parity* consumer must not:
/// `verified` under a stripped comparison, a filtered subset, or an output-only
/// fallback is not a bitwise-parity claim. Such a consumer reads
/// [`Self::bitwise_parity`] (or checks the `comparison` fields directly), which
/// is `true` only when the verdict rests on a full-INFO, unfiltered, unstripped
/// log comparison.
#[derive(Debug, Clone, Serialize)]
pub struct VerificationReport {
    /// True iff the two runs matched (the verdict as a boolean).
    pub verified: bool,
    /// True iff the runs matched *and* the comparison that certified the match
    /// satisfies the bitwise INFO-parity contract (see
    /// [`ComparisonSpec::is_bitwise_parity`]). A determinism / record-replay
    /// ratchet keys on this single boolean; it can never be silently weakened to
    /// a stripped or filtered compare because a stripped/filtered match sets it
    /// `false`.
    pub bitwise_parity: bool,
    /// The verdict as a stable string ("matched" / "diverged").
    pub verdict: Verdict,
    /// The comparison that produced the verdict. Without this a bitwise-parity
    /// consumer cannot distinguish a stripped match from a bitwise one. `null`
    /// when no verdict was reached (see [`Verdict::NoResult`]).
    pub comparison: Option<ComparisonSpec>,
    /// How many messages in [`ComparisonSpec::log_scope`] were actually compared.
    /// `null` means the log comparison did not run. A strict *configuration* is
    /// not proof that the configured comparison had data, so this count is what makes
    /// [`Self::bitwise_parity`] falsifiable.
    pub compared_log_messages: Option<ComparedLogCounts>,
    /// The guest's exit code, if it exited normally.
    pub guest_exit_code: Option<i32>,
    /// The guest's terminating signal number, if it was killed by a signal.
    pub guest_signal: Option<i32>,
}

impl VerificationReport {
    /// The record stamped before verification runs: no verdict has been reached
    /// yet, so nothing may read as verified or as parity.
    pub fn no_result() -> Self {
        VerificationReport {
            verified: false,
            bitwise_parity: false,
            verdict: Verdict::NoResult,
            comparison: None,
            compared_log_messages: None,
            guest_exit_code: None,
            guest_signal: None,
        }
    }
}

impl From<&VerificationOutcome> for VerificationReport {
    fn from(outcome: &VerificationOutcome) -> Self {
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
            comparison: Some(outcome.comparison),
            compared_log_messages: outcome.compared_log_messages,
            guest_exit_code: outcome.guest_status.code(),
            guest_signal: outcome.guest_status.signal(),
        }
    }
}

/// Write the verification report as a single JSON line to `path`.
///
/// This is the exit-code-independent verdict channel: the record it writes is
/// true or false based on whether verification matched, regardless of what the
/// guest exited with.
pub fn write_verification_json(path: &Path, outcome: &VerificationOutcome) -> Result<(), Error> {
    write_report_json(path, &VerificationReport::from(outcome))
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

fn write_report_json(path: &Path, report: &VerificationReport) -> Result<(), Error> {
    use std::io::Write as _;

    let json = serde_json::to_string(report)?;
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
    let file1 = tempfile::Builder::new()
        .prefix(&format!("{}_log_", name1))
        .rand_bytes(5)
        .tempfile()?;
    let file2 = tempfile::Builder::new()
        .prefix(&format!("{}_log_", name2))
        .rand_bytes(5)
        .tempfile()?;

    Ok((file1, file2))
}

pub fn setup_double_run(
    global: &GlobalOpts,
    name1: &str,
    name2: &str,
    strictness: LogCompareStrictness,
) -> ((GlobalOpts, NamedTempFile), (GlobalOpts, NamedTempFile)) {
    let (file1, file2) = temp_log_files(name1, name2).unwrap();

    let path1 = PathBuf::from(file1.path());
    let path2 = PathBuf::from(file2.path());

    // Override global settings.  Unfortunately we lose the log output to the
    // screen.
    let mut global = global.clone();
    global.log_file = Some(path1);
    global.log = Some(verification_log_level(global.log, strictness, false));

    let mut global2 = global.clone();
    global2.log_file = Some(path2);
    ((global, file1), (global2, file2))
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

pub fn compare_two_runs(
    first: ComparedRun<'_>,
    second: ComparedRun<'_>,
    options: ComparisonOptions<'_>,
) -> Result<VerificationOutcome, Error> {
    let ComparedRun {
        output: out1,
        log: log1,
    } = first;
    let ComparedRun {
        output: out2,
        log: log2,
    } = second;
    let mut failed = false;
    // None until the log comparison actually runs; stays None on the
    // output-only (KVM concurrent) fallback so the report can distinguish
    // "compared nothing" from "compared and matched".
    let mut compared_log_messages: Option<ComparedLogCounts> = None;

    // Resolve the strictness label to concrete diff flags once, and carry the
    // resulting spec through to the verdict so the returned outcome records
    // exactly which comparison certified it.
    let spec = ComparisonSpec::new(
        options.strictness,
        options.compare_logs,
        options.diagnostic_full_trace,
    );

    if out1.stdout != out2.stdout {
        failed = true;
        eprintln!("Mismatch in stdout between run 1 and run 2:");
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
        eprintln!("Mismatch in stderr between run 1 and run 2:");
        let str1 = String::from_utf8_lossy(&out1.stderr);
        let str2 = String::from_utf8_lossy(&out2.stderr);
        if str1.lines().count() > 1 {
            display_diff(&str1, &str2);
        } else {
            eprintln!("{}", Comparison::new(&str1, &str2));
        }
    }

    if options.compare_logs {
        eprintln!(
            ":: {} {} and {}",
            "Comparing logs...".yellow().bold(),
            log1.display(),
            log2.display()
        );
        // The comparison semantics come from `spec` (strip_lines + mode); only
        // the printed syscall-history depth still tracks `verbose`. Historically
        // both were flipped together, so the sole bitwise comparison was also the
        // loudest — decoupling them lets a quiet run be bitwise-strict.
        let diff_options = logdiff::LogDiffOpts {
            strip_lines: spec.strip_lines,
            // Thread the tier-2 canonicalization from the spec so the parity
            // (`Canonical`) policy actually rewrites host addresses to ordinals
            // in the engine; without this the verdict would REPORT
            // `canonicalize_addresses = true` while the diff ran with the raw
            // addresses — the exact proxy/binding drift the spec exists to close.
            canonicalize_addresses: spec.canonicalize_addresses,
            comparison: spec.log_comparison_mode(),
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
            diff_options.ignore_lines.is_empty() == !spec.ignore_lines,
            "ComparisonSpec.ignore_lines must match the diff engine's ignore_lines"
        );

        let summary = logdiff::log_diff_detailed(log1.as_ref(), log2.as_ref(), &diff_options);
        compared_log_messages = Some(ComparedLogCounts {
            left: summary.compared_left,
            right: summary.compared_right,
        });
        if summary.diff_found {
            failed = true;
            eprintln!(":: {}", "Log differences found between runs.".red().bold());
            eprintln!(
                ":: {}: {} {}",
                "Respective Logs retained for further inspection".red(),
                log1.display(),
                log2.display()
            );
        }
    } else {
        eprintln!(
            ":: KVM concurrent mode: comparing guest output and exit status; internal syscall trace order is not deterministic"
        );
    }

    if out1.status != out2.status {
        failed = true;
        eprintln!(
            "Mismatch in exit status between run 1 and run 2: {}",
            Comparison::new(&out1.status, &out2.status)
        );
    }

    if failed {
        eprintln!(":: {}", options.failure_message.red().bold());
        let _ = log1.keep()?;
        let _ = log2.keep()?;
        // Divergence is a verification *verdict*, not an I/O error: return it as
        // a value carrying the guest exit status. `Err` stays reserved for
        // genuine failures (e.g. the `.keep()?` above). Callers that want the
        // historical "divergence -> nonzero process exit" behavior use
        // `VerificationOutcome::into_exit_status`.
        Ok(VerificationOutcome {
            verdict: Verdict::Diverged,
            guest_status: out2.status,
            comparison: spec,
            compared_log_messages,
        })
    } else {
        // Allow the NamedTempFiles to be deleted in this case:
        let mut unsupported = unsupported_syscalls_from_log(log1.as_ref())?;
        unsupported.extend(unsupported_syscalls_from_log(log2.as_ref())?);
        if let Some(message) = detcore::format_unsupported_syscall_warning(&unsupported) {
            eprintln!("WARNING: {message}");
        }
        eprintln!(":: {}", options.success_message.green().bold());
        Ok(VerificationOutcome {
            verdict: Verdict::Matched,
            guest_status: out2.status,
            comparison: spec,
            compared_log_messages,
        })
    }
}

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

    fn output(status: i32, stdout: &[u8], stderr: &[u8]) -> Output {
        Output {
            status: ExitStatus::Exited(status),
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        }
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
        compare_two_runs(
            ComparedRun {
                output: left,
                log: left_log,
            },
            ComparedRun {
                output: right,
                log: right_log,
            },
            ComparisonOptions {
                success_message: "verified",
                failure_message: "failed",
                verbose: false,
                strictness,
                compare_logs: true,
                diagnostic_full_trace: false,
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
        format!(
            "2026-08-06T01:00:00.000000Z INFO detcore: [dtid 2] DETLOG [syscall] write(fd=1, count={value})\n"
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
        let report = VerificationReport::from(&outcome);
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
            },
            ComparedRun {
                output: &right,
                log: right_log,
            },
            ComparisonOptions {
                success_message: "verified",
                failure_message: "failed",
                verbose: false,
                strictness: LogCompareStrictness::Stripped,
                compare_logs: false,
                diagnostic_full_trace: false,
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
            // Collapsing a divergence to the legacy exit convention is an error
            // (nonzero process exit), preserving the historical behavior.
            assert!(outcome.into_exit_status().is_err());

            let _ = fs::remove_file(path1);
            let _ = fs::remove_file(path2);
        }
    }

    #[test]
    fn comparison_spec_maps_strictness_to_concrete_flags() {
        let stripped = ComparisonSpec::new(LogCompareStrictness::Stripped, true, false);
        assert!(stripped.strip_lines);
        assert!(!stripped.full_trace);
        assert_eq!(
            stripped.log_comparison_mode(),
            LogComparisonMode::Deterministic
        );

        let canonical = ComparisonSpec::new(LogCompareStrictness::Canonical, true, false);
        assert!(!canonical.strip_lines);
        assert!(canonical.canonicalize_addresses);
        assert!(canonical.exact_remainder);
        assert!(canonical.full_trace);
        assert_eq!(canonical.log_scope, ComparedLogScope::Info);
        assert_eq!(canonical.log_comparison_mode(), LogComparisonMode::Info);

        let diagnostic = ComparisonSpec::new(LogCompareStrictness::Canonical, true, true);
        assert_eq!(diagnostic.log_scope, ComparedLogScope::FullTrace);
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
        assert!(VerificationReport::from(&matched).bitwise_parity);

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
            },
            ComparedRun {
                output: &out,
                log: right,
            },
            ComparisonOptions {
                success_message: "verified",
                failure_message: "failed",
                verbose: true,
                strictness: LogCompareStrictness::Canonical,
                compare_logs: true,
                diagnostic_full_trace: true,
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
        let report = VerificationReport::from(&canonical);
        assert!(!report.verified);
        assert!(!report.comparison.unwrap().strip_lines);

        // Diverged canonical runs retain their logs (`.keep()`); clean them up.
        let _ = fs::remove_file(path1);
        let _ = fs::remove_file(path2);
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
        let report = VerificationReport::from(&outcome);
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
    fn verification_report_json_carries_the_comparison() {
        let out = output(0, b"hello\n", b"");
        // NONEMPTY logs: parity may only be claimed when the comparison had
        // data. This test previously used `empty_logs()` and asserted
        // bitwise_parity = true, which codified a green over ZERO compared
        // events -- see `empty_log_comparison_matches_but_is_never_parity`.
        let (log1, log2) = logs_with_identical_detlog();
        let outcome =
            compare_with(&out, log1, &out, log2, LogCompareStrictness::Canonical).unwrap();

        let json = serde_json::to_string(&VerificationReport::from(&outcome)).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["verified"], serde_json::json!(true));
        assert_eq!(parsed["verdict"], serde_json::json!("matched"));
        // The executed-work evidence travels with the verdict.
        assert!(parsed["compared_log_messages"]["left"].as_u64().unwrap() > 0);
        assert!(parsed["compared_log_messages"]["right"].as_u64().unwrap() > 0);
        // The single boolean a parity ratchet keys on: a matched, full-INFO,
        // unstripped, unfiltered comparison.
        assert_eq!(parsed["bitwise_parity"], serde_json::json!(true));
        assert_eq!(
            parsed["comparison"]["strictness"],
            serde_json::json!("canonical")
        );
        assert_eq!(
            parsed["comparison"]["strip_lines"],
            serde_json::json!(false)
        );
        assert_eq!(parsed["comparison"]["full_trace"], serde_json::json!(true));
        assert_eq!(parsed["comparison"]["log_scope"], serde_json::json!("info"));
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
    // as true bitwise parity ONLY under a full-INFO, unstripped, unfiltered,
    // log-comparing spec — and reject it under every weaker one. This brackets
    // both sides: the one qualifying spec fires, and each single-clause weakening
    // (stripped, output-only, and each ignore/skip filter) is refused. Without
    // this, three different facts (stripped compare, output-only fallback,
    // filtered subset) would all masquerade as the same `verified == true`.
    #[test]
    fn bitwise_parity_contract_accepts_only_full_unfiltered_comparison() {
        // Positive: the exact qualifying comparison the `--verify-strict` path
        // produces.
        let full = ComparisonSpec::new(LogCompareStrictness::Canonical, true, false);
        assert!(
            full.is_bitwise_parity(),
            "a full-INFO unstripped unfiltered comparison must qualify"
        );

        // Negatives: each independent weakening of the qualifying spec must be
        // refused, so no single relaxed dimension can pass as bitwise parity.
        let stripped = ComparisonSpec::new(LogCompareStrictness::Stripped, true, false);
        assert!(
            !stripped.is_bitwise_parity(),
            "a stripped comparison normalizes away the parity-relevant data"
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
            guest_status: ExitStatus::Exited(0),
            comparison: full,
            compared_log_messages: Some(ComparedLogCounts { left: 9, right: 9 }),
        };
        assert!(!VerificationReport::from(&diverged).bitwise_parity);
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
