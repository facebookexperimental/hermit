/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Everything to do with post-processing hermit/detcore logs.

use core::fmt::Display;
use core::fmt::Formatter;
use core::fmt::Result;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::str::FromStr;
use std::sync::LazyLock;

use clap;
use clap::Parser;
use regex::Regex;
use tempfile::NamedTempFile;

use crate::detlog::DetLogEvent;
use crate::detlog::DetLogRecord;

/// The in-band line a bounded log writer emits when a run's log file reaches
/// its configured size bound.
///
/// This lives beside the comparator, not only beside the writer that emits it,
/// because the comparator is the consumer that must not ignore it. The
/// comparison below walks the two message lists in lockstep, so it can only
/// ever speak for the retained prefix; a pair of logs that were both cut at the
/// bound would agree on that prefix while the discarded tails were never looked
/// at. Recognizing the marker is what keeps that from being reported as a
/// match. `hermit-cli`'s writer emits this exact line and a unit test there
/// binds the two by running the real writer's output through
/// [`log_was_truncated`].
///
/// This is the COMPLETE sentence, not a prefix of it. An earlier version
/// matched only the leading `=== HERMIT LOG TRUNCATED:` fragment anywhere in
/// the text, which made the refusal fire on guest-controlled content: DETLOG
/// records syscall path arguments verbatim, so a guest that merely touched a
/// path containing that fragment poisoned its own `--verify` -- including with
/// the bound already disabled, so the refusal's own remedy was unavailable.
pub const TRUNCATION_MARKER: &str = "=== HERMIT LOG TRUNCATED: reached the configured size bound \
     (HERMIT_LOG_MAX_BYTES). Output beyond this point was DISCARDED. The run itself continued and \
     was NOT affected. ===";

/// Versioned policy token for the only prefix removed by `BitwiseInfoV1`.
pub const STRIP_WALL_CLOCK_PREFIX_V1: &str = "real-wall-clock-prefix/v1";

/// Versioned policy token for lossless host-address ordinalization.
pub const CANON_ADDRESS_ORDINAL_V1: &str = "host-address-to-first-appearance-ordinal/v1";

/// Whether `log_text` is the output of a writer that hit its size bound.
///
/// The question this answers is "was THIS LOG truncated", which is not the same
/// question as "does this text mention the marker". The distinction is the
/// whole point: log text contains guest-controlled bytes, so a predicate that
/// merely searches for the marker is a predicate a guest can satisfy.
///
/// Two anchors make it discriminate, and both are properties of how the marker
/// is produced rather than of what it says:
///
/// 1. **End of file.** `BoundedWriter::announce_truncation` writes the marker
///    at the moment the bound is crossed and every later write is discarded, so
///    on a truncated log the marker is the final bytes. Trailing newlines are
///    ignored; nothing else may follow.
/// 2. **A whole line.** The marker is emitted preceded by its own newline, so
///    it occupies a line by itself. A DETLOG line always carries a
///    `<timestamp> LEVEL target:` prefix and therefore can never equal it. Nor
///    can a guest forge the line break: DETLOG renders path arguments with
///    `Debug`, which escapes a newline to a literal backslash-`n`, so guest
///    bytes cannot start a line at all.
///
/// A genuinely truncated log still satisfies both, so this narrows the
/// predicate to the real condition without weakening it.
pub fn log_was_truncated(log_text: &str) -> bool {
    let trimmed = log_text.trim_end_matches(['\n', '\r']);
    if !trimmed.ends_with(TRUNCATION_MARKER) {
        return false;
    }
    // `ends_with` matched, so this offset is on a character boundary.
    let marker_start = trimmed.len() - TRUNCATION_MARKER.len();
    marker_start == 0 || trimmed.as_bytes()[marker_start - 1] == b'\n'
}

/// Selects the set of log messages compared for determinism.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum LogComparisonMode {
    /// Compare deterministic Detcore and scheduler messages.
    #[default]
    Deterministic,
    /// Compare every INFO message exactly, while leaving any captured DEBUG or
    /// TRACE messages available for diagnostics. This is the observation
    /// envelope used by the `BitwiseInfoV1` verification policy.
    Info,
    /// Compare every captured log message without filtering.
    FullTrace,
}

/// Reader-facing names for the two inputs to a log comparison.
///
/// Standalone `hermit log-diff` keeps the historical `run 1` / `run 2`
/// vocabulary through [`Default`]. Verification callers override these names
/// when the inputs are a recording and its replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComparisonSideLabels {
    /// Name of the left comparison input.
    pub left: String,
    /// Name of the right comparison input.
    pub right: String,
}

impl ComparisonSideLabels {
    /// Construct labels for a caller that knows what each input represents.
    pub fn new(left: impl Into<String>, right: impl Into<String>) -> Self {
        Self {
            left: left.into(),
            right: right.into(),
        }
    }
}

impl Default for ComparisonSideLabels {
    fn default() -> Self {
        Self::new("run 1", "run 2")
    }
}

/// Options for calling `log_diff`.
#[derive(Debug, Parser, Clone)]
pub struct LogDiffOpts {
    /// UNSAFE: strips numbers and temporary paths before comparison.
    ///
    /// This erases timestamps and syscall values that bitwise parity exists to
    /// compare. Never use this option to make a failing parity diff pass; doing
    /// so is cheating. It is only for non-parity diagnostic localization.
    #[clap(long = "unsafe-strip-lines")]
    pub strip_lines: bool,

    /// Canonicalize host memory addresses before comparison WITHOUT erasing them.
    ///
    /// Only addresses a producer has explicitly marked with the
    /// `<hostaddr 0x...>` wrapper (see [`host_addr`]) are canonicalized; each
    /// distinct marked address is rewritten to an ordinal placeholder
    /// `<addr{N}>` assigned by order of first appearance within a single run
    /// (see `canonicalize_addresses_in_line`). Unlike [`Self::strip_lines`],
    /// this discards ONLY the host-specific raw pointer value: it preserves
    /// identity (same address -> same ordinal), ordering (introduction
    /// sequence), and aliasing (two names for one address collapse to one
    /// ordinal), and it leaves every other byte -- virtual-time timestamps,
    /// syscall argument/result values, counts, sizes, flags -- untouched for an
    /// exact comparison. In canonical parity, this preserves the ability to DETECT a
    /// difference (allocation-order or aliasing changes still diverge), which
    /// wholesale stripping throws away.
    ///
    /// The marker is REQUIRED (a bare `0x...` literal is left exact) because
    /// nothing in the compared DETLOG stream can otherwise distinguish a varying
    /// host pointer from a reproducible hex value -- syscall arguments printed
    /// `{:#x}` (e.g. `flock` `operation=0x2` vs `0x6`), guest memory ranges,
    /// content digests, cpuid leaves. A blanket `0x` canonicalization would
    /// collapse those too, silently erasing real syscall-argument divergence:
    /// a "softer strip" and exactly the fake-green this policy exists to prevent.
    #[clap(skip)]
    pub canonicalize_addresses: bool,

    /// The internal message set to compare.
    #[clap(skip)]
    pub comparison: LogComparisonMode,

    /// Reader-facing names for the left and right inputs. Standalone callers
    /// retain the historical defaults; verification paths bind their own.
    #[clap(skip)]
    pub side_labels: ComparisonSideLabels,

    /// Require current producer-owned DETLOG records instead of using the
    /// historical text compatibility path.
    #[clap(skip)]
    pub require_structured_events: bool,

    /// Print both selected logs exactly as they are passed to the comparator.
    ///
    /// The output names the active comparison policy and reflects every
    /// selection, wall-clock-prefix removal, and normalization step.
    #[clap(long)]
    pub print_logs: bool,

    /// Limit the number of differences printed. Set to 0 for no limit.
    #[clap(long, default_value = "20")]
    pub limit: u64,

    /// Before comparison, filter out lines which contain this substring.
    #[clap(long)]
    pub ignore_lines: Vec<String>,

    /// Show this many completed syscalls before each side-specific divergence point.
    /// Set to 0 to omit history.
    #[clap(long, default_value = "0")]
    pub syscall_history: u64,
    /// Disable colored console output for line diffs.
    #[clap(long)]
    pub no_color: bool,

    /// Do not consider "COMMIT" messages for deterministic checks.
    #[clap(long)]
    pub skip_commit: bool,

    /// Do not consider "DETLOG" messages for deterministic checks.
    #[clap(long)]
    pub skip_detlog: bool,

    /// Use git diff instead of the internal, basic log comparison.
    #[clap(long)]
    pub git_diff: bool,

    /// In case --skip-detlog=false this parameter further filters which
    /// "DETLOG" messages will be included for deterministic checks
    #[clap(long, default_values = &["syscall", "syscallresult", "other"])]
    pub include_detlogs: Vec<DetLogFilter>,
}

impl LogDiffOpts {
    fn is_skip(&self, filter: DetLogFilter) -> bool {
        !self.include_detlogs.contains(&filter)
    }

    fn skip_detlog(&self, entry: &LogMessage<'_>) -> bool {
        if self.skip_detlog {
            return true;
        }

        if is_detlog_syscall(entry) && self.is_skip(DetLogFilter::Syscall) {
            return true;
        }
        if is_detlog_syscall_result(entry) && self.is_skip(DetLogFilter::SyscallResult) {
            return true;
        }

        if !is_detlog_syscall(entry)
            && !is_detlog_syscall_result(entry)
            && self.is_skip(DetLogFilter::Other)
        {
            return true;
        }

        false
    }

    fn filter_deterministic<'a>(&self, v: &[LogMessage<'a>]) -> Vec<LogMessage<'a>> {
        v.iter()
            .filter_map(|message| {
                if (is_detlog(message)
                    && !self.skip_detlog(message)
                    && !is_scheduler_committed_time(message))
                    || (is_commit(message)
                        && !self.skip_commit
                        && !is_internal_io_poll_commit(message))
                {
                    Some(*message)
                } else {
                    None
                }
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogNormalization {
    Exact,
    Stripped,
    Canonical,
}

/// The scope and normalization that jointly determine a log comparison.
///
/// Construct this once from [`LogDiffOpts`], then use it for selection,
/// transformation, and the displayed policy name so those three facts cannot
/// drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LogComparisonPolicy {
    comparison: LogComparisonMode,
    normalization: LogNormalization,
}

impl LogComparisonPolicy {
    fn from_options(options: &LogDiffOpts) -> Self {
        let normalization = if options.strip_lines {
            LogNormalization::Stripped
        } else if options.canonicalize_addresses {
            LogNormalization::Canonical
        } else {
            LogNormalization::Exact
        };
        Self {
            comparison: options.comparison,
            normalization,
        }
    }

    fn name(self) -> &'static str {
        match (self.comparison, self.normalization) {
            (LogComparisonMode::Deterministic, LogNormalization::Exact) => "Deterministic",
            (LogComparisonMode::Deterministic, LogNormalization::Stripped) => "Stripped",
            (LogComparisonMode::Deterministic, LogNormalization::Canonical) => {
                "Deterministic with Canonical host-address normalization"
            }
            (LogComparisonMode::Info, LogNormalization::Exact) => "Info",
            (LogComparisonMode::Info, LogNormalization::Stripped) => {
                "Info with Stripped normalization"
            }
            (LogComparisonMode::Info, LogNormalization::Canonical) => "Canonical",
            (LogComparisonMode::FullTrace, LogNormalization::Exact) => "FullTrace",
            (LogComparisonMode::FullTrace, LogNormalization::Stripped) => {
                "FullTrace with Stripped normalization"
            }
            (LogComparisonMode::FullTrace, LogNormalization::Canonical) => {
                "FullTrace with Canonical host-address normalization"
            }
        }
    }
}

/// Indicates which DETLOG entries to be used for log-diff comparison
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetLogFilter {
    ///the start of syscall will be used for logdiff
    Syscall,
    ///the syscall result  will be used for logdiff
    SyscallResult,
    ///all other unspecified DETLOG entries will be used for logdiff
    Other,
}

impl FromStr for DetLogFilter {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "syscall" => Ok(DetLogFilter::Syscall),
            "syscallresult" => Ok(DetLogFilter::SyscallResult),
            "other" => Ok(DetLogFilter::Other),
            _ => Err(anyhow::Error::msg(format!(
                "unknown value {} for DetLogFilter",
                s
            ))),
        }
    }
}

/// N.B. we don't want to specify two different notions of "default", so we use the
/// `Clap` instance above.
impl Default for LogDiffOpts {
    fn default() -> Self {
        let v: Vec<String> = vec![];
        LogDiffOpts::parse_from(v.iter())
    }
}

/// In fully-deterministic modes, many log lines should be fully determinstic across runs.
/// But as that is a work-in-progress, this utility strips known-nondeterministic
/// information from logs.
///
/// This erasure is deliberately lossy and is NOT a parity claim: it backs the
/// `Stripped` comparator only (`bitwise_parity: false`). `BitwiseInfoV1`
/// canonicalizes rather than erases -- see `canonicalize_addresses_in_line`.
/// Lossy as it is, each pattern must still erase only what it names: erasing a
/// neighbouring field turns a real divergence into a reported match.
///
/// Example input/output:
///   `Input:  COMMIT turn 3, dettid 231635 using resources Resources { tid: DetPid { inner: 231635 }, resources: {Path("/proc/231635/fd/1"): W} }`
///   `Output: COMMIT turn <NUM>, dettid <NUM> using resources Resources { tid: DetPid { inner: <NUM> }, resources: {Path("/proc/<pid>/fd/<num>"): W} }`
///
/// As you can see this is overkill and smarter strategies would be possible. For example,
/// ones that remember and post-facto-determinize certain identifiers.
pub fn strip_log_entry(log: &str) -> String {
    // Memory addresses, like 0x7fcfb7e7d450
    //
    // TODO: use a debug allocator that increases only, never reusing. Also, consider
    // post-facto processing all of these into new virtual addresses based on the order they're seen.
    static RE0: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b0[xX][A-Fa-f0-9]+\b").unwrap());

    // Every number, plus common duration suffixes so fractional timing jitter is
    // not left behind. This one is terrible overkill: for `hermit run` itself and
    // for all command tests the full contents of a COMMIT line should already be
    // deterministic, so nothing here should need erasing. It is retained for
    // `spawn_fn_*` variants, which fork from another process and so exercise only
    // a *partial* detcore setup without a true process tree of their own.
    //
    // N.B. RE4 must run BEFORE this pattern, or `800.709_180s` is consumed here as
    // a bare number and never reaches the duration rule.
    static RE1: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\b[\d][\d_]*(?:\.[\d][\d_]*)?(?:ns|us|µs|ms)?\b").unwrap());

    // A quoted /tmp path. `[^"]*` stops at the path's OWN closing quote: a greedy
    // `.*` here ran to the last quote on the line and erased every field after the
    // path, so two entries differing only downstream of a /tmp path compared equal.
    //
    // TODO: only strip this information if the config specified to the host /tmp through.
    // Otherwise we can determinize /tmp access fully.
    static RE2: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"/tmp/[^"]*""#).unwrap());

    // TODO: only strip this one if we're allowing through the host /proc or failing to determinize tids/pids:
    static RE3: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"/proc/[\d]+/").unwrap());

    // TODO: only strip this if we're running a library-based test where we can't
    // guarantee the starting state of the allocator/etc.
    static RE4: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b[\d][\d_.]*s\b").unwrap());

    let log = RE4.replace_all(log, "<NANOSECONDS>");
    let log = RE3.replace_all(&log, "/proc/<PID>/");
    let log = RE0.replace_all(&log, "<ADDR>");
    let log = RE1.replace_all(&log, "<NUM>");
    let log = RE2.replace_all(&log, "/tmp/<somewhere>\"");
    String::from(log)
}

/// Wrap a host memory address so `canonicalize_addresses_in_line` will
/// canonicalize it. Producers that print a genuinely host-specific pointer
/// (one that varies run-to-run, e.g. a supervisor-side allocation) should emit
/// it via this helper -- `<hostaddr 0x7fcfb7e7d450>` -- instead of a bare
/// `0x...` literal. Only marked addresses are canonicalized, so reproducible hex
/// (syscall arguments, guest memory ranges, digests) is compared exactly.
///
/// Note: nothing in detcore's current DETLOG output prints a varying host
/// pointer -- guest addresses are determinized and every logged `0x...` value is
/// reproducible -- so this marker is presently unused in production and exists
/// so a future host-pointer print opts into canonicalization deliberately rather
/// than being swept up by a blanket regex.
pub fn host_addr(addr: usize) -> String {
    format!("<hostaddr {addr:#x}>")
}

/// Rewrite each MARKED host memory address (`<hostaddr 0x...>`, see
/// [`host_addr`]) in `line` to an ordinal placeholder `<addr{N}>`, numbered by
/// order of first appearance within a single run. `map`/`next` carry the per-run
/// assignment state threaded across all of that run's lines, so `next` should
/// start at 1 and the same `map`/`next` must be reused for every line of one run
/// (and a FRESH pair used for the other run).
///
/// Canonical parity strips the wall-clock prefix, canonicalizes these marked
/// addresses, and compares everything else exactly. This step differs
/// from [`strip_log_entry`]'s `<ADDR>` erasure in one decisive way: erasure maps
/// every address to a single token, so two runs that allocate in a DIFFERENT
/// ORDER, or that ALIAS differently (one address printed twice vs. two distinct
/// addresses), compare EQUAL -- the exact defect parity exists to catch. An
/// ordinal assigned by first appearance keeps identity, order, and aliasing, so
/// those cases still diverge while a pure ASLR-shift (same structure, different
/// raw values) compares equal.
///
/// Only the explicit `<hostaddr ...>` marker is canonicalized. A bare `0x...`
/// literal is left byte-for-byte for exact comparison: a blanket regex cannot
/// tell a varying host pointer from a reproducible syscall argument printed
/// `{:#x}` (`flock` `operation=0x2` vs `0x6`, `membarrier` bitmasks), so
/// canonicalizing every hex token would erase real syscall-argument divergence.
/// Decimal values (virtual-time timestamps, counts, sizes) are likewise exact.
fn canonicalize_addresses_in_line(
    line: &str,
    map: &mut HashMap<String, usize>,
    next: &mut usize,
) -> String {
    // Match ONLY the explicit host-address marker emitted by `host_addr`; the
    // captured group is the raw `0x...` value used as the ordinal key.
    static RE_HOSTADDR: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"<hostaddr (0[xX][A-Fa-f0-9]+)>").unwrap());

    RE_HOSTADDR
        .replace_all(line, |caps: &regex::Captures| {
            let addr = &caps[1];
            let ord = match map.get(addr) {
                Some(existing) => *existing,
                None => {
                    let assigned = *next;
                    *next += 1;
                    map.insert(addr.to_string(), assigned);
                    assigned
                }
            };
            format!("<addr{ord}>")
        })
        .into_owned()
}

fn messages_for_comparison(
    messages: &[LogMessage<'_>],
    policy: LogComparisonPolicy,
) -> Vec<String> {
    match policy.normalization {
        LogNormalization::Stripped => messages
            .iter()
            .map(|message| strip_log_entry(message.text))
            .collect(),
        LogNormalization::Canonical => {
            let mut addresses = HashMap::new();
            let mut next_address = 1usize;
            messages
                .iter()
                .map(|message| {
                    canonicalize_addresses_in_line(message.text, &mut addresses, &mut next_address)
                })
                .collect()
        }
        LogNormalization::Exact => messages
            .iter()
            .map(|message| message.text.to_owned())
            .collect(),
    }
}

#[cfg(test)]
fn canonical_info_from_str(contents: &str) -> std::io::Result<Vec<String>> {
    canonical_info_from_str_with_filter(contents, |_| true)
}

fn canonical_info_from_str_with_filter(
    contents: &str,
    keep_record: impl Fn(&str) -> bool,
) -> std::io::Result<Vec<String>> {
    let info = filter_infos(
        &extract_log_messages(contents)?
            .into_iter()
            .filter(|record| keep_record(record.text))
            .collect::<Vec<_>>(),
    );
    let opts = LogDiffOpts {
        canonicalize_addresses: true,
        comparison: LogComparisonMode::Info,
        ..Default::default()
    };
    Ok(messages_for_comparison(
        &info,
        LogComparisonPolicy::from_options(&opts),
    ))
}

/// Print the canonical INFO messages that strict verification would compare for
/// one captured log.
///
/// This removes the real wall-clock prefix and rewrites only explicitly marked
/// host addresses to first-appearance ordinals. It does not run the lossy
/// `--unsafe-strip-lines` transformation: scheduler turns, virtual time, syscall
/// values, counts, flags, and every other substantive byte are preserved.
pub fn write_canonical_info(file: &Path, writer: &mut impl Write) -> std::io::Result<usize> {
    write_canonical_info_with_filter(file, writer, |_| true)
}

/// Render the canonical INFO messages that `BitwiseInfoV1` would compare from
/// bytes already captured by the caller.
///
/// Unlike [`write_canonical_info`], this fixed-policy form requires UTF-8 and
/// current structured DETLOG events and refuses a bounded-writer truncation.
/// A harness can therefore render retained comparison records from the exact
/// immutable bytes it compared without reopening a mutable pathname.
pub fn write_bitwise_info_v1_bytes(
    bytes: &[u8],
    side_label: &str,
    writer: &mut impl Write,
) -> std::io::Result<usize> {
    let contents = std::str::from_utf8(bytes).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{side_label} is not UTF-8: {error}"),
        )
    })?;
    if log_was_truncated(contents) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{side_label} was truncated at the configured size bound"),
        ));
    }
    let records = extract_log_messages(contents)
        .map_err(|error| std::io::Error::new(error.kind(), format!("{side_label} {error}")))?;
    validate_structured_events(side_label, &records, true)?;
    let info = filter_infos(&records);
    let options = bitwise_info_v1_options(ComparisonSideLabels::new(side_label, side_label));
    let messages = messages_for_comparison(&info, LogComparisonPolicy::from_options(&options));
    for message in &messages {
        writeln!(writer, "{message}")?;
    }
    Ok(messages.len())
}

/// Print canonical INFO messages after applying a caller-supplied record filter.
///
/// Every record is parsed and its level tag validated before `keep_record` is
/// consulted. Existing callers should use [`write_canonical_info`]; backend
/// adapters use this form to exclude transport-only records at their boundary.
/// This low-level hook does not name the predicate, so any product verdict or
/// JSON report using it must bind the function to a typed, serialized policy.
pub fn write_canonical_info_with_filter(
    file: &Path,
    writer: &mut impl Write,
    keep_record: impl Fn(&str) -> bool,
) -> std::io::Result<usize> {
    let bytes = std::fs::read(file)?;
    let contents = String::from_utf8_lossy(&bytes);
    let messages = canonical_info_from_str_with_filter(&contents, keep_record)?;
    for message in &messages {
        writeln!(writer, "{message}")?;
    }
    Ok(messages.len())
}

/// Separate a full, continuous log into discrete (possibly-multiline) log messages,
/// stripping off the timestamps in the process.  Return lines tagged with their
/// index number.
/// The timestamp that begins every log record. A record runs from one match to
/// the next, so records may span multiple lines -- which is why comparison is
/// done on records and never on lines or bytes.
static RECORD_START: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"((Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec) \d\d \d\d:\d\d:\d\d\.\d+|\d+-\d\d-\d\dT\d\d:\d\d:\d\d.\d+Z) +")
        .unwrap()
});

/// Byte offsets at which each record starts.
fn record_starts(contents: &str) -> Vec<usize> {
    RECORD_START
        .find_iter(contents)
        .map(|m| m.start())
        .collect()
}

/// How many records in `contents` are known COMPLETE.
///
/// A record is complete only once the *next* record has begun, because nothing
/// else marks its end. In a log still being written the final record may be
/// half-flushed, so it is never counted and never compared: a partial write must
/// not read as a difference. A buffer with one record start has zero complete
/// records, and that is a real answer, not a failure.
pub fn complete_record_count(contents: &str) -> usize {
    record_starts(contents).len().saturating_sub(1)
}

/// The prefix of `contents` holding exactly its first `n` complete records.
///
/// Returns `None` when fewer than `n` complete records are present, so a caller
/// can tell "the runs agree over n records" apart from "n records have not been
/// written yet". Those two must never collapse into one answer.
pub fn take_complete_records(contents: &str, n: usize) -> Option<&str> {
    let starts = record_starts(contents);
    if n == 0 {
        return Some(&contents[..0]);
    }
    // Record n-1 ends where record n begins, so n complete records require n+1 starts.
    starts.get(n).map(|end| &contents[..*end])
}

/// Split a log into tagged records, REFUSING rather than panicking on a line
/// that carries no level tag.
///
/// This used to `panic!`, and a panic is the wrong failure mode for a
/// diagnostic tool -- people reach for `log-diff` when something is ALREADY
/// broken, and unwinding at them is the least helpful thing it can do. Worse,
/// the `--json` consumer could not tell a crash from a real verdict: the report
/// came back `verdict: no_result` with null counts, which reads as "no
/// comparison was reached" rather than "the tool died".
///
/// This is still FAIL-CLOSED -- an unrecognised line refuses the whole
/// comparison rather than being skipped. Skipping would silently change the
/// compared surface, which is exactly what `RecordEnvelopePolicy` exists to
/// make explicit and versioned. Choosing what to exclude is a disclosed policy
/// decision, not something a parser should do on its own initiative.
///
/// The refusal names the offending line, because in practice the cause is a
/// backend emitting its own untagged diagnostics into the same stream --
/// measured: DBT writes fourteen `detcore-dbt: ...` startup lines, which is
/// what makes a ptrace-vs-DBT comparison impossible today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LogMessage<'a> {
    index: usize,
    text: &'a str,
    event: Option<DetLogEvent>,
}

fn extract_log_messages(contents: &str) -> std::io::Result<Vec<LogMessage<'_>>> {
    let ts = &*RECORD_START;
    let tag = Regex::new("^(ERROR|WARN|INFO|DEBUG|TRACE) ").unwrap();
    ts.split(contents) // Not aware of a streaming version of this RE split operation.
        .enumerate()
        .map(|(i, s)| (i, s.trim()))
        .filter(|(_, s)| !s.is_empty())
        .map(|(i, s)| {
            // Only let through lines that start with one of the expected tags:
            if !tag.is_match(s) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "log line {i} has no ERROR/WARN/INFO/DEBUG/TRACE tag, so it cannot be \
                         placed in the compared record stream: {s}"
                    ),
                ));
            }
            let (text, record) = DetLogRecord::split(s).map_err(|error| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("log record {i} has an invalid structured DETLOG result: {error}"),
                )
            })?;
            Ok(LogMessage {
                index: i,
                text,
                event: record.map(|record| record.event),
            })
        })
        .collect()
}

fn is_info(message: &LogMessage<'_>) -> bool {
    message.text.starts_with("INFO ")
}

fn historical_is_commit(line: &str) -> bool {
    line.contains(" COMMIT ")
}

fn historical_is_detlog(line: &str) -> bool {
    line.contains(" DETLOG ")
}

fn is_commit(message: &LogMessage<'_>) -> bool {
    match message.event {
        Some(DetLogEvent::SchedulerCommit { .. }) => true,
        Some(_) => false,
        None => historical_is_commit(message.text),
    }
}

fn is_detlog(message: &LogMessage<'_>) -> bool {
    match message.event {
        Some(
            DetLogEvent::Other
            | DetLogEvent::Syscall
            | DetLogEvent::SyscallResult { .. }
            | DetLogEvent::SchedulerCommittedTime,
        ) => true,
        Some(_) => false,
        None => historical_is_detlog(message.text),
    }
}

/// A scheduler COMMIT turn that only grants the `InternalIOPolling` resource, i.e. a
/// granted retry of a nonblocking poll (poll/epoll_wait/wait4/futex/recv/send...). These
/// grants are internal bookkeeping of Hermit's blocking-via-polling mechanism: how many
/// times a thread is re-granted permission to re-attempt a nonblocking syscall before it
/// stops returning would-block depends on when a concurrent external-IO action (e.g. a
/// child linker process writing to a pipe) becomes ready on the host, which is wall-clock
/// dependent and not tied to the (RCB-deterministic) logical schedule. The corresponding
/// `NONCOMMIT ... polling resource` skips are already excluded from comparison (they are
/// not tagged `COMMIT`); excluding the matching grant-COMMITs keeps the deterministic
/// comparison consistent and focused on guest-observable events (the actual syscall
/// results, still compared via their DETLOG entries).
///
/// SaBRe's inherited stdio pipes emit an outer device-resource turn before the inner polling
/// turn. The scheduler tags that outer turn with `[sabre-internal-pipe-io]`; it is the same
/// host-timing-sensitive operation and is normalized here as well. A SaBRe task with a loopback
/// peer similarly tags the strong yield before each zero-timeout poll with
/// `[sabre-loopback-poll-zero-timeout]`. The scheduler additionally suppresses the per-retry
/// "advance global time for scheduler turn" DETLOG line for these turns at the source (see
/// `Scheduler::bump_global_time`), so the two mechanisms together make the deterministic
/// comparison insensitive to host-timing-dependent polling-loop counts.
fn is_internal_io_poll_commit(message: &LogMessage<'_>) -> bool {
    match message.event {
        Some(DetLogEvent::SchedulerCommit {
            internal_io_poll, ..
        }) => internal_io_poll,
        Some(_) => false,
        None => {
            historical_is_commit(message.text)
                && (message.text.contains("{InternalIOPolling: ")
                    || message.text.contains(" [sabre-internal-pipe-io]")
                    || message.text.contains(" [sabre-loopback-poll-zero-timeout]"))
        }
    }
}

/// The scheduler's per-turn `committed_time` advance bookkeeping. `committed_time` tracks
/// the global logical clock, which still moves forward when an `InternalIOPolling` retry
/// (see `is_internal_io_poll_commit`) advances time -- and the number of those retries is
/// host-timing nondeterministic. That makes the *presence* of this line on a given turn
/// retry-count sensitive, so we exclude it from the deterministic comparison. No
/// guest-observable signal is lost: the value is redundant with the (retained,
/// retry-count-insensitive) "advance global time for scheduler turn" DETLOG line and with
/// the per-turn committed time echoed on each COMMIT line.
fn is_scheduler_committed_time(message: &LogMessage<'_>) -> bool {
    match message.event {
        Some(DetLogEvent::SchedulerCommittedTime) => true,
        Some(_) => false,
        None => message.text.contains("advancing committed_time from "),
    }
}

fn is_detcore(message: &LogMessage<'_>) -> bool {
    static PREFIX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new("^(ERROR|WARN|INFO|DEBUG|TRACE).* detcore:").unwrap());

    PREFIX.is_match(message.text)
}

fn is_detlog_syscall(message: &LogMessage<'_>) -> bool {
    match message.event {
        Some(DetLogEvent::Syscall | DetLogEvent::SyscallResult { .. }) => true,
        Some(_) => false,
        None => historical_is_detlog(message.text) && message.text.contains("[syscall]"),
    }
}

fn is_detlog_syscall_result(message: &LogMessage<'_>) -> bool {
    match message.event {
        Some(DetLogEvent::SyscallResult { .. }) => true,
        Some(_) => false,
        None => is_detlog_syscall(message) && message.text.contains("finish syscall"),
    }
}

fn event_matches_human_record(event: DetLogEvent, text: &str) -> bool {
    match event {
        DetLogEvent::Other
        | DetLogEvent::Syscall
        | DetLogEvent::SyscallResult { .. }
        | DetLogEvent::SchedulerCommittedTime => historical_is_detlog(text),
        DetLogEvent::SchedulerCommit { .. } => historical_is_commit(text),
        DetLogEvent::SchedulerEmptyQueueKick => text.contains(SCHEDULER_EMPTY_QUEUE_KICK),
    }
}

fn validate_structured_events(
    label: &str,
    messages: &[LogMessage<'_>],
    require: bool,
) -> std::io::Result<()> {
    for message in messages {
        let is_semantic_record = historical_is_detlog(message.text)
            || historical_is_commit(message.text)
            || message.text.contains(SCHEDULER_EMPTY_QUEUE_KICK);
        if require && is_semantic_record && message.event.is_none() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "{label} log record {} is missing its structured DETLOG result",
                    message.index
                ),
            ));
        }
        if let Some(event) = message.event
            && !event_matches_human_record(event, message.text)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "{label} log record {} has structured DETLOG kind {:?} that disagrees with its human record",
                    message.index, event
                ),
            ));
        }
    }
    Ok(())
}

// TODO:
// Append together a sequence of messages while truncating if there are too many.
fn _truncate_messages(_v: &[&str]) -> String {
    unimplemented!()
}

fn filter_infos<'a>(v: &[LogMessage<'a>]) -> Vec<LogMessage<'a>> {
    v.iter()
        .filter(|message| is_info(message))
        .copied()
        .collect()
}

fn filter_detcore<'a>(v: &[LogMessage<'a>]) -> Vec<LogMessage<'a>> {
    v.iter()
        .filter(|message| is_detcore(message))
        .copied()
        .collect()
}

/// Historical text of the INFO message the scheduler logs when it finds the
/// run queue empty but is not finished yet. Current records carry
/// [`DetLogEvent::SchedulerEmptyQueueKick`]; this spelling remains only so
/// retained logs stay readable. `Scheduler::step2_process_blocked` in
/// [`crate::scheduler`] emits it and returns `SkipTurn`, so the loop goes
/// around again and still exits later through the ordinary
/// "run queue empty, exiting sched_loop." message.
///
/// Two consequences make this the message worth recording. First, both paths
/// end with the same final scheduler message, so the *last* scheduler line does
/// not say which path a run took; only whether this kick appeared does. Second,
/// whether it appears is decided by host timing — the ptrace supervisor may not
/// yet have reaped a physical process exit when the check runs — so a pair of
/// runs of the same guest can differ here while agreeing on every committed
/// scheduling decision.
///
/// Kept as a constant so the string sits beside the code that reads it; grep
/// for this text to find the producing `info!`.
const SCHEDULER_EMPTY_QUEUE_KICK: &str = "zero threads left anywhere, fizzling.";

/// How many of `v` record the scheduler's empty-run-queue kick.
fn count_empty_queue_kicks(v: &[LogMessage<'_>]) -> usize {
    v.iter()
        .filter(|message| match message.event {
            Some(DetLogEvent::SchedulerEmptyQueueKick) => true,
            Some(_) => false,
            None => message.text.contains(SCHEDULER_EMPTY_QUEUE_KICK),
        })
        .count()
}

/// Historical resource text of the COMMIT record produced when a guest's
/// runtime reads the process's own memory map during bootstrap. Current records
/// carry `runtime_maps_read`; this spelling remains only for retained logs.
///
/// This record carries the second known shape of backend self-nondeterminism,
/// and it is unlike the empty-run-queue kick in a way that matters. Two runs of
/// one guest can agree on every scheduling decision — same turn, same thread,
/// same resource — and still commit that turn at *different virtual times*,
/// because virtual time advances with retired conditional branches and the
/// number of branches the scan executes depends on the map it reads. So the
/// evidence is not the presence of a message but the value inside one.
///
/// A diverging pair prints both times in its diff. A matching pair kept
/// nothing, which left the prior question — does this guest perform the read at
/// all, and therefore can it exhibit the drift — answerable only by collecting
/// divergences over many runs.
const RUNTIME_MAPS_READ_RESOURCE: &str = r#"Path("/proc/self/maps")"#;

/// One run's first memory-map read COMMIT, rendered for the retained line.
/// `None` is spelled out rather than omitted, so a run that never performed the
/// read is distinguishable from a run whose value was simply not reported.
fn describe_maps_commit(first: Option<(u64, Option<u64>)>) -> String {
    match first {
        Some((turn, Some(nanoseconds))) => {
            format!("first at turn {turn}, committed virtual time {nanoseconds}ns")
        }
        Some((turn, None)) => format!("first at turn {turn}, committed virtual time unrecorded"),
        None => "no such record".to_string(),
    }
}

/// COMMIT records in `v` that read the process's own memory map: how many there
/// are, and the turn and committed virtual time of the first.
fn maps_read_commits(v: &[LogMessage<'_>]) -> (usize, Option<(u64, Option<u64>)>) {
    let mut count = 0;
    let mut first = None;
    for message in v {
        let reads_runtime_maps = match message.event {
            Some(DetLogEvent::SchedulerCommit {
                runtime_maps_read, ..
            }) => runtime_maps_read,
            Some(_) => false,
            None => message.text.contains(RUNTIME_MAPS_READ_RESOURCE),
        };
        if !reads_runtime_maps {
            continue;
        }
        let Some(position) = commit_position(message) else {
            continue;
        };
        count += 1;
        if first.is_none() {
            first = Some(position);
        }
    }
    (count, first)
}

fn filter_ignored<'a>(lines: Vec<LogMessage<'a>>, omits: &Vec<String>) -> Vec<LogMessage<'a>> {
    lines
        .into_iter()
        .filter(|message| {
            let mut keep = true;
            for omit in omits {
                if message.text.contains(omit) {
                    keep = false
                }
            }
            keep
        })
        .collect()
}

fn collect_syscalls<'a>(v: &[LogMessage<'a>]) -> Vec<LogMessage<'a>> {
    v.iter()
        .filter(|entry| is_detlog_syscall(entry))
        .copied()
        .collect()
}

fn first_different_message_indices(
    left: &[LogMessage<'_>],
    compared_left: &[String],
    right: &[LogMessage<'_>],
    compared_right: &[String],
) -> Option<(Option<usize>, Option<usize>)> {
    let common = compared_left.len().min(compared_right.len());

    if let Some(position) =
        (0..common).find(|&position| compared_left[position] != compared_right[position])
    {
        return Some((Some(left[position].index), Some(right[position].index)));
    }

    match compared_left.len().cmp(&compared_right.len()) {
        Ordering::Less => Some((None, Some(right[common].index))),
        Ordering::Greater => Some((Some(left[common].index), None)),
        Ordering::Equal => None,
    }
}

/// Keep the content that identifies a differing event while removing only
/// values already carried in separate fields beside it.
///
/// This is deliberately narrower than [`strip_log_entry`]. Syscall arguments,
/// resource names, thread identities, payload bytes, and all other numbers stay
/// exact. Record position is not part of the message and remains a separate
/// observation.
fn first_divergent_message(message: &LogMessage<'_>) -> String {
    static FINISHED_SYSCALL: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(finish syscall #)[0-9][0-9_]*").unwrap());
    static COMMIT_TURN: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(\bCOMMIT turn )[0-9][0-9_]*\b").unwrap());
    static COMMITTED_TIME: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(\b(?:at time|on previously committed) )[0-9][0-9_]*(?:\.[0-9_]+)?(?:ns|s)?\b")
            .unwrap()
    });

    let (first_line, continuation) = match message.text.split_once('\n') {
        Some((first_line, continuation)) => (first_line, Some(continuation)),
        None => (message.text, None),
    };
    let first_line =
        if is_detlog_syscall_result(message) && finished_syscall_number(message).is_some() {
            FINISHED_SYSCALL
                .replace_all(first_line, "${1}<NUM>")
                .into_owned()
        } else {
            first_line.to_string()
        };
    let first_line = if is_commit(message) && commit_position(message).is_some() {
        let first_line = COMMIT_TURN.replace_all(&first_line, "${1}<NUM>");
        COMMITTED_TIME
            .replace_all(&first_line, "${1}<NANOSECONDS>")
            .into_owned()
    } else {
        first_line
    };
    match continuation {
        Some(continuation) => format!("{first_line}\n{continuation}"),
        None => first_line,
    }
}

fn compared_message_at_record(
    records: &[LogMessage<'_>],
    compared: &[String],
    record: Option<usize>,
) -> Option<String> {
    let record = record?;
    let position = records.iter().position(|message| message.index == record)?;
    let original = records.get(position)?;
    let prepared = LogMessage {
        index: original.index,
        text: compared.get(position)?,
        event: original.event,
    };
    Some(first_divergent_message(&prepared))
}

fn parse_underscored_u64(value: &str) -> Option<u64> {
    value.replace('_', "").parse().ok()
}

fn parse_virtual_nanoseconds(value: &str, unit: Option<&str>) -> Option<u64> {
    match unit {
        None | Some("ns") if !value.contains('.') => parse_underscored_u64(value),
        Some("s") => {
            let value = value.replace('_', "");
            let (seconds, fraction) = value.split_once('.').unwrap_or((&value, ""));
            if fraction.len() > 9 || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            let seconds = seconds.parse::<u64>().ok()?;
            let fraction = if fraction.is_empty() {
                0
            } else {
                let digits = fraction.parse::<u64>().ok()?;
                digits.checked_mul(10_u64.pow((9 - fraction.len()) as u32))?
            };
            seconds.checked_mul(1_000_000_000)?.checked_add(fraction)
        }
        _ => None,
    }
}

fn historical_commit_position(message: &str) -> Option<(u64, Option<u64>)> {
    static TURN: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\bCOMMIT turn ([0-9][0-9_]*)\b").unwrap());
    static TIME: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\b(?:at time|on previously committed) ([0-9][0-9_]*(?:\.[0-9_]+)?)(ns|s)?\b")
            .unwrap()
    });

    let turn = parse_underscored_u64(TURN.captures(message)?.get(1)?.as_str())?;
    let virtual_nanoseconds = TIME.captures(message).and_then(|captures| {
        parse_virtual_nanoseconds(
            captures.get(1)?.as_str(),
            captures.get(2).map(|unit| unit.as_str()),
        )
    });
    Some((turn, virtual_nanoseconds))
}

fn commit_position(message: &LogMessage<'_>) -> Option<(u64, Option<u64>)> {
    match message.event {
        Some(DetLogEvent::SchedulerCommit {
            scheduler_turn,
            virtual_nanoseconds,
            ..
        }) => Some((scheduler_turn, Some(virtual_nanoseconds))),
        Some(_) => None,
        None => historical_commit_position(message.text),
    }
}

fn commit_position_at_or_before(
    messages: &[LogMessage<'_>],
    message_index: usize,
) -> Option<(u64, Option<u64>)> {
    messages
        .iter()
        .rev()
        .filter(|message| message.index <= message_index)
        .find_map(commit_position)
}

/// Detcore's own syscall counter from the producer-owned record.
///
/// Retained historical logs fall back to the old `finish syscall #N` spelling.
/// Current verification sets `require_structured_events`, so removing the
/// structured number refuses rather than silently restoring prose authority.
fn historical_finished_syscall_number(line: &str) -> Option<u64> {
    let rest = line.split("finish syscall #").nth(1)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn finished_syscall_number(message: &LogMessage<'_>) -> Option<u64> {
    match message.event {
        Some(DetLogEvent::SyscallResult {
            finished_syscall_number,
        }) => Some(finished_syscall_number),
        Some(_) => None,
        None => historical_finished_syscall_number(message.text),
    }
}

/// How many syscalls the guest had COMPLETED when the divergence appeared.
///
/// `inbound syscall` records carry no number, so this deliberately reads only
/// `finish syscall #N`: the answer is "the guest got this far", and a syscall
/// that was entered but never returned has not got anywhere yet.
///
/// ⚠️ THIS UNIT IS COMPARABLE ACROSS BACKENDS, AND A DIFFERENCE IS A FINDING
/// RATHER THAN AN ARTEFACT. The same `getpgrp` sitting at DETLOG event 40 under
/// ptrace and 39 under DBT is not a limitation of the measurement -- the guest
/// is not supposed to be able to tell which backend it is running on, so a
/// syscall-stream difference between two backends IS the parity divergence.
/// Do not restrict this to within-backend use or label it incomparable; that
/// would hide exactly what parity cells exist to detect.
fn finished_syscall_at_or_before(v: &[LogMessage<'_>], message_index: usize) -> Option<u64> {
    v.iter()
        .rev()
        .filter(|message| message.index <= message_index)
        .find_map(finished_syscall_number)
}

fn syscall_at_or_before<'a>(
    syscalls: &'a [LogMessage<'a>],
    index: usize,
) -> Option<LogMessage<'a>> {
    syscalls
        .iter()
        .rev()
        .find(|message| message.index <= index)
        .copied()
}

fn sentence_case_label(label: &str) -> String {
    let mut characters = label.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().collect::<String>() + characters.as_str(),
        None => String::new(),
    }
}

fn write_syscall_context(
    w: &mut impl std::io::Write,
    left_index: usize,
    right_index: usize,
    left_syscalls: &[LogMessage<'_>],
    right_syscalls: &[LogMessage<'_>],
    labels: &ComparisonSideLabels,
    history_count: u64,
) -> std::io::Result<()> {
    if history_count == 0 {
        return Ok(());
    }

    let left_current = syscall_at_or_before(left_syscalls, left_index);
    let right_current = syscall_at_or_before(right_syscalls, right_index);
    if left_current.is_none() && right_current.is_none() {
        return Ok(());
    }

    writeln!(w, "Divergent syscall context:")?;
    for (label, current) in [
        (labels.left.as_str(), left_current),
        (labels.right.as_str(), right_current),
    ] {
        if let Some(syscall) = current {
            writeln!(
                w,
                "  {label}, log message {}: {}",
                syscall.index, syscall.text
            )?;
        } else {
            writeln!(w, "  {label}: <no syscall observed>")?;
        }
    }

    let history_limit = usize::try_from(history_count).unwrap_or(usize::MAX);
    for (label, index, syscalls) in [
        (labels.left.as_str(), left_index, left_syscalls),
        (labels.right.as_str(), right_index, right_syscalls),
    ] {
        let history_boundary =
            syscall_at_or_before(syscalls, index).map_or(index, |current| current.index);
        let mut history = syscalls
            .iter()
            .rev()
            .filter(|entry| entry.index < history_boundary && is_detlog_syscall_result(entry))
            .take(history_limit)
            .copied()
            .collect::<Vec<_>>();
        history.reverse();
        if !history.is_empty() {
            writeln!(w, "  Prior completed syscalls for {label}:")?;
            for syscall in history {
                writeln!(w, "    {}", syscall.text)?;
            }
        }
    }
    writeln!(w)?;

    Ok(())
}

/// A comparison of two strings.
///
/// Displays comparison result without any formatting
pub struct Comparison<'a> {
    left: &'a str,
    right: &'a str,
    no_color: bool,
}
impl<'a> Comparison<'a> {
    /// Store two values to be compared in future.
    ///
    /// Expensive diffing is deferred until calling `Debug::fmt`.
    pub fn new(no_color: bool, left: &'a str, right: &'a str) -> Comparison<'a> {
        Comparison {
            left,
            right,
            no_color,
        }
    }
}
impl<'a> Display for Comparison<'a> {
    fn fmt(&self, f: &mut Formatter) -> Result {
        if self.no_color {
            writeln!(f, "Diff < left / right > :")?;
            writeln!(f, "<\"{}\"", self.left)?;
            writeln!(f, ">\"{}\"", self.right)
        } else {
            pretty_assertions::Comparison::new(&self.left, &self.right).fmt(f)
        }
    }
}

/// Returns `true` if a difference is found.
///
/// We could use an existing diff library on the entire log, but this provides us more
/// control over how to present the (stripped/unstripped) differences, and to focus on the
/// per-line divergence(s), and potentially focus on the first point of divergence.
//
// Future TODO:
//  - report bulk differences, e.g. leftover lines, but with truncation
//  - report only the first K per-line differences
//  - detect reorderings and/or switch to larger differences for consecutive multi-line mismatches
fn diff_vecs(
    which: &str,
    left: (&[LogMessage<'_>], &[String]),
    right: (&[LogMessage<'_>], &[String]),
    opts: &LogDiffOpts,
    w: &mut impl std::io::Write,
    left_syscalls: &[LogMessage<'_>],
    right_syscalls: &[LogMessage<'_>],
) -> std::io::Result<bool> {
    let (v1, compared_left) = left;
    let (v2, compared_right) = right;
    writeln!(w, "  Comparing {which} messages...\n")?;
    if v1.is_empty() && v2.is_empty() {
        return Ok(false);
    }

    let mut diff_count = 0;
    for (position, (left, right)) in v1.iter().zip(v2.iter()).enumerate() {
        let left_compared = &compared_left[position];
        let right_compared = &compared_right[position];
        if left_compared == right_compared {
            continue;
        }

        if diff_count >= opts.limit && opts.limit != 0 {
            writeln!(
                w,
                "More than {} differences, eliding the rest...",
                opts.limit
            )?;
            break;
        }

        write!(
            w,
            "({which}) Mismatch at log messages {} ({}) and {} ({}): {}",
            left.index,
            opts.side_labels.left,
            right.index,
            opts.side_labels.right,
            Comparison::new(opts.no_color, left_compared, right_compared)
        )?;
        if opts.strip_lines || opts.canonicalize_addresses {
            write!(
                w,
                "({which}) Original entries before normalization: {}",
                Comparison::new(opts.no_color, left.text, right.text)
            )?;
        }
        write_syscall_context(
            w,
            left.index,
            right.index,
            left_syscalls,
            right_syscalls,
            &opts.side_labels,
            opts.syscall_history,
        )?;

        diff_count += 1;
    }

    match v1.len().cmp(&v2.len()) {
        Ordering::Less => {
            writeln!(
                w,
                "{} contains {} extra messages not matched in {}. Displaying up to 10:",
                sentence_case_label(&opts.side_labels.right),
                v2.len() - v1.len(),
                opts.side_labels.left,
            )?;
            diff_count += 1;
            let start = v2.len() - std::cmp::min(10, v2.len() - v1.len());
            for message in &compared_right[start..] {
                writeln!(w, "{message}")?;
            }
        }
        Ordering::Greater => {
            writeln!(
                w,
                "{} contains {} extra messages not matched in {}. Displaying up to 10:",
                sentence_case_label(&opts.side_labels.left),
                v1.len() - v2.len(),
                opts.side_labels.right,
            )?;
            diff_count += 1;
            let start = v1.len() - std::cmp::min(10, v1.len() - v2.len());
            for message in &compared_left[start..] {
                writeln!(w, "{message}")?;
            }
        }
        Ordering::Equal => {}
    }

    Ok(diff_count > 0)
}

fn write_compared_messages(
    writer: &mut impl std::io::Write,
    messages: &[String],
) -> std::io::Result<()> {
    for message in messages {
        writeln!(writer, "{message}")?;
    }
    Ok(())
}

fn write_compared_logs(
    writer: &mut impl std::io::Write,
    policy: LogComparisonPolicy,
    compared_left: &[String],
    compared_right: &[String],
    labels: &ComparisonSideLabels,
) -> std::io::Result<()> {
    writeln!(writer, "Comparison policy: {}", policy.name())?;
    writeln!(writer, "--- begin {} compared log ---", labels.left)?;
    write_compared_messages(writer, compared_left)?;
    writeln!(writer, "--- end {} compared log ---", labels.left)?;
    writeln!(writer, "--- begin {} compared log ---", labels.right)?;
    write_compared_messages(writer, compared_right)?;
    writeln!(writer, "--- end {} compared log ---", labels.right)?;
    Ok(())
}

fn git_diff(
    which: &str,
    left: (&[LogMessage<'_>], &[String]),
    right: (&[LogMessage<'_>], &[String]),
    opts: &LogDiffOpts,
    w: &mut impl std::io::Write,
    left_syscalls: &[LogMessage<'_>],
    right_syscalls: &[LogMessage<'_>],
) -> std::io::Result<bool> {
    let (v1, compared_left) = left;
    let (v2, compared_right) = right;
    writeln!(w, "  Comparing {which} messages...\n")?;

    let mut file1 = NamedTempFile::new()?;
    let mut file2 = NamedTempFile::new()?;

    write_compared_messages(&mut file1, compared_left)?;
    write_compared_messages(&mut file2, compared_right)?;

    match Command::new("git")
        .args(["diff", "--color", "--color-words", "-w"])
        .arg(file1.path())
        .arg(file2.path())
        .status()
    {
        Ok(code) => Ok(!code.success()),
        Err(error) => {
            eprintln!("Error launching git, falling back to basic diff: {error}");
            diff_vecs(
                which,
                (v1, compared_left),
                (v2, compared_right),
                opts,
                w,
                left_syscalls,
                right_syscalls,
            )
        }
    }
}

/// What a log comparison actually compared, alongside whether it differed.
///
/// A bare "no difference found" boolean cannot distinguish *"the two message
/// streams were compared and matched"* from *"there were no messages to
/// compare"*. Both selected lists being empty is a NO-RESULT, not a match, so
/// the counts travel with the verdict and a parity consumer can require nonzero
/// execution before believing a green.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogDiffSummary {
    /// True if a substantive difference was found between the two runs.
    pub diff_found: bool,
    /// Number of messages actually selected for comparison from the first run.
    pub compared_left: usize,
    /// Number of messages actually selected for comparison from the second run.
    pub compared_right: usize,
    /// Scheduler turn at the first different selected message, when the first
    /// run has a preceding scheduler COMMIT message that identifies it.
    pub first_divergent_scheduler_turn: Option<u64>,
    /// Virtual nanoseconds at that same scheduler COMMIT, when its time is
    /// present and parseable.
    pub first_divergent_virtual_nanoseconds: Option<u64>,
    /// 1-based index of the first record that differs. "Diverged somewhere in
    /// the first N records" is a bound, not a location, and on a long run the
    /// two are far apart; this is the location.
    pub first_divergent_record: Option<usize>,
    /// How many syscalls the guest had COMPLETED when the divergence appeared,
    /// as detcore's own `finish syscall #N` counter. The fourth unit: a
    /// divergence located at record 108 is easier to act on when you also know
    /// the guest was 37 syscalls in.
    pub first_divergent_syscall: Option<u64>,
    /// The first differing compared message from the left execution, after
    /// removing only the syscall number, scheduler turn, and committed virtual
    /// time that are already recorded in separate fields.
    pub first_divergent_left_message: Option<String>,
    /// The corresponding first differing compared message from the right
    /// execution, with the same narrowly scoped removals.
    pub first_divergent_right_message: Option<String>,
    /// The reason the comparison was REFUSED rather than performed, so there
    /// is no verdict about whether the runs agree. None means the comparison
    /// ran. Keeping the cause here, rather than only printing it, lets every
    /// caller carry the producer's diagnosis into its typed result.
    ///
    /// [`Self::diff_found`] is also set, because a refusal must never read as a
    /// match on any existing predicate. But the two are not the same fact: a
    /// difference is something observed, whereas a refusal is the absence of an
    /// observation. A caller that reports the outcome must be able to tell them
    /// apart -- otherwise a run that compared nothing is announced as a run
    /// that found the two executions differing, which is a claim nothing
    /// supports.
    ///
    /// Set today by exactly one condition: an input log that ends at the
    /// bounded writer's truncation marker.
    pub refusal_reason: Option<String>,
}

impl LogDiffSummary {
    /// True only when the comparison both ran on a nonempty selection *and*
    /// found no difference. An empty-vs-empty comparison is never a match.
    pub fn matched_with_evidence(&self) -> bool {
        self.refusal_reason.is_none()
            && !self.diff_found
            && self.compared_left > 0
            && self.compared_right > 0
    }
}

/// Process log messages from two files.  Log messages look like this:
///     "Apr 09 06:08:03.100  INFO detcore: [detcore, dtid 2]  finish syscall: close(2) = Ok(0)"
///
/// With some complexities:
///  * Some entries are multi-line (contain newlines).
///  * Some stripping of nondeterministic information is needed for direct comparability.
///  * Certain lines are intended to be deterministic/comparable, in their contents,
///    and others in their *presence* but not their details.
///
/// Reports only whether the two files differ. See [`log_diff_detailed`] when the
/// caller must also know how many messages were actually compared; a bare
/// `false` here cannot distinguish a match from an empty comparison.
//
// TODO: we should replace this with a diff algorithm that can handle insertions while maintaining
// alignment. There's also no reason we can't output the stripped relevant lines and use a separate
// diff tool.
pub fn log_diff(file_a: &Path, file_b: &Path, opts: &LogDiffOpts) -> bool {
    log_diff_detailed(file_a, file_b, opts).diff_found
}

/// Like [`log_diff`], but returns the counted comparison evidence rather than a
/// bare boolean.
pub fn log_diff_detailed(file_a: &Path, file_b: &Path, opts: &LogDiffOpts) -> LogDiffSummary {
    try_log_diff_detailed(file_a, file_b, opts).expect("could not read or compare log inputs")
}

/// Fallible form of [`log_diff_detailed`] for user-facing callers. Missing or
/// unreadable inputs are ordinary command errors, not process panics.
pub fn try_log_diff_detailed(
    file_a: &Path,
    file_b: &Path,
    opts: &LogDiffOpts,
) -> std::io::Result<LogDiffSummary> {
    try_log_diff_detailed_with_filter(file_a, file_b, opts, |_| true)
}

/// Compare two current Hermit logs under the canonical `BitwiseInfoV1` policy.
///
/// Callers provide only the two inputs, their reader-facing labels, and
/// diagnostic-output settings. They cannot weaken record selection or
/// normalization. The fixed policy compares every INFO record, requires
/// current structured DETLOG events, canonicalizes only explicitly marked host
/// addresses to first-appearance ordinals, and otherwise compares the selected
/// messages exactly.
pub fn try_compare_bitwise_info_v1(
    file_a: &Path,
    file_b: &Path,
    side_labels: ComparisonSideLabels,
) -> std::io::Result<LogDiffSummary> {
    try_compare_bitwise_info_v1_with_records(file_a, file_b, side_labels)
        .map(|(summary, _, _)| summary)
}

/// Canonical comparison plus total complete-record counts from both inputs.
pub fn try_compare_bitwise_info_v1_with_records(
    file_a: &Path,
    file_b: &Path,
    side_labels: ComparisonSideLabels,
) -> std::io::Result<(LogDiffSummary, usize, usize)> {
    let bytes_a = std::fs::read(file_a)?;
    let bytes_b = std::fs::read(file_b)?;
    try_compare_bitwise_info_v1_bytes_with_records(&bytes_a, &bytes_b, side_labels)
}

/// Canonical comparison over bytes already captured by the caller.
pub fn try_compare_bitwise_info_v1_bytes_with_records(
    bytes_a: &[u8],
    bytes_b: &[u8],
    side_labels: ComparisonSideLabels,
) -> std::io::Result<(LogDiffSummary, usize, usize)> {
    try_compare_bitwise_info_v1_bytes_with_records_and_diagnostics(
        bytes_a,
        bytes_b,
        side_labels,
        BitwiseInfoV1Diagnostics::default(),
        &mut std::io::stderr(),
    )
}

/// Output-only settings for a canonical `BitwiseInfoV1` comparison.
///
/// These settings control rendered diagnostics only. They cannot change which
/// records are selected, how records are normalized, or the returned verdict
/// and counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BitwiseInfoV1Diagnostics {
    /// Maximum number of differing messages to render. Zero means unlimited.
    pub difference_limit: u64,
    /// Number of completed syscalls to render before each divergent syscall.
    pub syscall_history: u64,
    /// Disable terminal color in rendered differences.
    pub no_color: bool,
    /// Print both selected canonical streams before reporting differences.
    pub print_logs: bool,
}

impl Default for BitwiseInfoV1Diagnostics {
    fn default() -> Self {
        Self {
            difference_limit: 20,
            syscall_history: 5,
            no_color: false,
            print_logs: false,
        }
    }
}

/// Canonical file comparison with caller-selected diagnostic output.
pub fn try_compare_bitwise_info_v1_with_records_and_diagnostics(
    file_a: &Path,
    file_b: &Path,
    side_labels: ComparisonSideLabels,
    diagnostics: BitwiseInfoV1Diagnostics,
    writer: &mut impl Write,
) -> std::io::Result<(LogDiffSummary, usize, usize)> {
    let bytes_a = std::fs::read(file_a)?;
    let bytes_b = std::fs::read(file_b)?;
    try_compare_bitwise_info_v1_bytes_with_records_and_diagnostics(
        &bytes_a,
        &bytes_b,
        side_labels,
        diagnostics,
        writer,
    )
}

/// Canonical file comparison with caller-selected diagnostic output.
pub fn try_compare_bitwise_info_v1_with_diagnostics(
    file_a: &Path,
    file_b: &Path,
    side_labels: ComparisonSideLabels,
    diagnostics: BitwiseInfoV1Diagnostics,
    writer: &mut impl Write,
) -> std::io::Result<LogDiffSummary> {
    try_compare_bitwise_info_v1_with_records_and_diagnostics(
        file_a,
        file_b,
        side_labels,
        diagnostics,
        writer,
    )
    .map(|(summary, _, _)| summary)
}

/// Canonical comparison over captured bytes with output-only diagnostics.
pub fn try_compare_bitwise_info_v1_bytes_with_records_and_diagnostics(
    bytes_a: &[u8],
    bytes_b: &[u8],
    side_labels: ComparisonSideLabels,
    diagnostics: BitwiseInfoV1Diagnostics,
    writer: &mut impl Write,
) -> std::io::Result<(LogDiffSummary, usize, usize)> {
    let str_a = std::str::from_utf8(bytes_a).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} is not UTF-8: {error}", side_labels.left),
        )
    })?;
    let str_b = std::str::from_utf8(bytes_b).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} is not UTF-8: {error}", side_labels.right),
        )
    })?;
    let mut options = bitwise_info_v1_options(side_labels);
    options.limit = diagnostics.difference_limit;
    options.syscall_history = diagnostics.syscall_history;
    options.no_color = diagnostics.no_color;
    options.print_logs = diagnostics.print_logs;
    let records_a = record_count(str_a);
    let records_b = record_count(str_b);
    let summary = log_diff_summary_from_strs_with_filter(str_a, str_b, &options, writer, |_| true)?;
    Ok((summary, records_a, records_b))
}

/// Construct the complete fixed policy used by `BitwiseInfoV1` callers.
fn bitwise_info_v1_options(side_labels: ComparisonSideLabels) -> LogDiffOpts {
    LogDiffOpts {
        strip_lines: false,
        canonicalize_addresses: true,
        comparison: LogComparisonMode::Info,
        side_labels,
        require_structured_events: true,
        print_logs: false,
        limit: 20,
        ignore_lines: Vec::new(),
        syscall_history: 5,
        no_color: false,
        skip_commit: false,
        skip_detlog: false,
        git_diff: false,
        include_detlogs: vec![
            DetLogFilter::Syscall,
            DetLogFilter::SyscallResult,
            DetLogFilter::Other,
        ],
    }
}

/// Fallible log comparison with a caller-supplied record filter.
///
/// Existing callers keep the unfiltered behavior through
/// [`try_log_diff_detailed`]. Backend adapters use this form to apply policy
/// without placing backend names or semantics in Detcore. This low-level hook
/// does not name the predicate, so any product verdict or JSON report using it
/// must bind the function to a typed, serialized policy.
pub fn try_log_diff_detailed_with_filter(
    file_a: &Path,
    file_b: &Path,
    opts: &LogDiffOpts,
    keep_record: impl Fn(&str) -> bool,
) -> std::io::Result<LogDiffSummary> {
    // For now the log-diff mode reads both logs fully into memory. This could be
    // modified in the future for a streaming solution, at least for scrolling through
    // the identical prefixes of very large logs.
    let vec_a = std::fs::read(file_a)?;
    let vec_b = std::fs::read(file_b)?;
    let str_a = String::from_utf8_lossy(&vec_a);
    let str_b = String::from_utf8_lossy(&vec_b);
    log_diff_summary_from_strs_with_filter(str_a, str_b, opts, &mut std::io::stderr(), keep_record)
}

/// Total records in a log, including a final record that may still be being
/// written. Use [`complete_record_count`] when the log is still growing.
pub fn record_count(contents: &str) -> usize {
    record_starts(contents).len()
}

/// Like [`try_log_diff_detailed`], but also reports how many records each log
/// contained. Every log comparison should be able to say what it read, so a
/// caller is never left to infer coverage from a bare verdict.
pub fn try_log_diff_with_records(
    file_a: &Path,
    file_b: &Path,
    opts: &LogDiffOpts,
) -> std::io::Result<(LogDiffSummary, usize, usize)> {
    try_log_diff_with_records_and_filter(file_a, file_b, opts, |_| true)
}

/// Compare two logs with a caller-supplied record filter and report source counts.
/// Product callers must bind the predicate to a typed, serialized policy; this
/// backend-neutral layer intentionally carries no backend policy identity.
pub fn try_log_diff_with_records_and_filter(
    file_a: &Path,
    file_b: &Path,
    opts: &LogDiffOpts,
    keep_record: impl Fn(&str) -> bool,
) -> std::io::Result<(LogDiffSummary, usize, usize)> {
    let vec_a = std::fs::read(file_a)?;
    let vec_b = std::fs::read(file_b)?;
    let str_a = String::from_utf8_lossy(&vec_a);
    let str_b = String::from_utf8_lossy(&vec_b);
    let records_a = record_count(&str_a);
    let records_b = record_count(&str_b);
    let summary = log_diff_summary_from_strs_with_filter(
        &str_a,
        &str_b,
        opts,
        &mut std::io::stderr(),
        keep_record,
    )?;
    Ok((summary, records_a, records_b))
}

/// A comparison of two logs that may still be growing.
///
/// The verdict alone is not reportable. "The runs agree" and "the runs agree so
/// far as anyone has looked" are different claims, and a caller who cannot tell
/// them apart will stop early and conclude the wrong thing. The record counts
/// are therefore part of the result, not optional detail alongside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefixComparison {
    /// The verdict over the compared prefix only.
    pub summary: LogDiffSummary,
    /// Complete records present in the first log when it was read.
    pub records_available_left: usize,
    /// Complete records present in the second log when it was read.
    pub records_available_right: usize,
    /// Records actually compared: the shorter of the two available counts.
    pub records_compared: usize,
}

impl PrefixComparison {
    /// True when one log has complete records the other has not reached yet, so
    /// the comparison is bounded by reading rather than by the runs agreeing.
    pub fn one_side_is_ahead(&self) -> bool {
        self.records_available_left != self.records_available_right
    }
}

/// Compare only the records both logs have finished writing.
///
/// The tail of a log being written may be half-flushed, and a half-flushed
/// record is not a difference -- it is an absence. Truncating both inputs to
/// their common *complete* prefix is what makes this safe to run against a live
/// run: bytes that have not been written yet can never be mistaken for bytes
/// that disagree.
pub fn compare_complete_prefix(
    contents_a: &str,
    contents_b: &str,
    opts: &LogDiffOpts,
    w: &mut impl std::io::Write,
) -> std::io::Result<PrefixComparison> {
    compare_complete_prefix_with_filter(contents_a, contents_b, opts, w, |_| true)
}

/// Compare the complete common prefix of two growing logs under the fixed
/// `BitwiseInfoV1` policy.
///
/// The whole byte buffers must be valid UTF-8, and a terminal bounded-writer
/// marker refuses immediately even though it is not a timestamped record.
/// Structured-record validation covers every complete record currently
/// available on each side, while the verdict compares only the common complete
/// prefix. The unfinished final record remains withheld until a later record
/// proves that it is complete.
pub fn compare_complete_bitwise_info_v1_prefix(
    bytes_a: &[u8],
    bytes_b: &[u8],
    side_labels: ComparisonSideLabels,
    diagnostics: BitwiseInfoV1Diagnostics,
    w: &mut impl std::io::Write,
) -> std::io::Result<PrefixComparison> {
    let contents_a = std::str::from_utf8(bytes_a).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} is not UTF-8: {error}", side_labels.left),
        )
    })?;
    let contents_b = std::str::from_utf8(bytes_b).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} is not UTF-8: {error}", side_labels.right),
        )
    })?;

    let truncated_a = log_was_truncated(contents_a);
    let truncated_b = log_was_truncated(contents_b);
    if truncated_a || truncated_b {
        let which_side = match (truncated_a, truncated_b) {
            (true, true) => "both logs were",
            (true, false) => "the first log was",
            (false, true) => "the second log was",
            (false, false) => unreachable!("guarded by the condition above"),
        };
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "{which_side} truncated at the configured size bound; the discarded tail was never written"
            ),
        ));
    }

    let records_available_left = complete_record_count(contents_a);
    let records_available_right = complete_record_count(contents_b);
    for (label, contents, available) in [
        (
            side_labels.left.as_str(),
            contents_a,
            records_available_left,
        ),
        (
            side_labels.right.as_str(),
            contents_b,
            records_available_right,
        ),
    ] {
        let complete = take_complete_records(contents, available)
            .expect("the complete-record count always identifies its own prefix");
        let records = extract_log_messages(complete)
            .map_err(|error| std::io::Error::new(error.kind(), format!("{label} {error}")))?;
        validate_structured_events(label, &records, true)?;
    }

    let records_compared = records_available_left.min(records_available_right);
    let prefix_a = take_complete_records(contents_a, records_compared)
        .expect("common prefix never exceeds either side's complete record count");
    let prefix_b = take_complete_records(contents_b, records_compared)
        .expect("common prefix never exceeds either side's complete record count");
    let mut options = bitwise_info_v1_options(side_labels);
    options.limit = diagnostics.difference_limit;
    options.syscall_history = diagnostics.syscall_history;
    options.no_color = diagnostics.no_color;
    options.print_logs = diagnostics.print_logs;
    let summary =
        log_diff_summary_from_strs_with_filter(prefix_a, prefix_b, &options, w, |_| true)?;
    Ok(PrefixComparison {
        summary,
        records_available_left,
        records_available_right,
        records_compared,
    })
}

/// Compare the complete common prefix after applying a caller-supplied record filter.
/// Product callers must bind the predicate to a typed, serialized policy; this
/// backend-neutral layer intentionally carries no backend policy identity.
pub fn compare_complete_prefix_with_filter(
    contents_a: &str,
    contents_b: &str,
    opts: &LogDiffOpts,
    w: &mut impl std::io::Write,
    keep_record: impl Fn(&str) -> bool,
) -> std::io::Result<PrefixComparison> {
    let records_available_left = complete_record_count(contents_a);
    let records_available_right = complete_record_count(contents_b);
    let records_compared = records_available_left.min(records_available_right);
    // Both are Some: `records_compared` is at most each side's complete count.
    let prefix_a = take_complete_records(contents_a, records_compared)
        .expect("common prefix never exceeds either side's complete record count");
    let prefix_b = take_complete_records(contents_b, records_compared)
        .expect("common prefix never exceeds either side's complete record count");
    let summary = log_diff_summary_from_strs_with_filter(prefix_a, prefix_b, opts, w, keep_record)?;
    Ok(PrefixComparison {
        summary,
        records_available_left,
        records_available_right,
        records_compared,
    })
}

/// Boolean-only wrapper retained for tests that only ask "did it differ?".
/// Prefer [`log_diff_summary_from_strs`] where the counts matter.
#[cfg(test)]
fn log_diff_from_strs(
    file_a_str: impl AsRef<str>,
    file_b_str: impl AsRef<str>,
    opts: &LogDiffOpts,
    w: &mut impl std::io::Write,
) -> std::io::Result<bool> {
    Ok(log_diff_summary_from_strs(file_a_str, file_b_str, opts, w)?.diff_found)
}

#[cfg(test)]
fn log_diff_summary_from_strs(
    file_a_str: impl AsRef<str>,
    file_b_str: impl AsRef<str>,
    opts: &LogDiffOpts,
    w: &mut impl std::io::Write,
) -> std::io::Result<LogDiffSummary> {
    log_diff_summary_from_strs_with_filter(file_a_str, file_b_str, opts, w, |_| true)
}

/// Compare two in-memory logs after applying a caller-supplied record filter.
///
/// Every record is parsed and its level tag validated before `keep_record` is
/// consulted, so filtering cannot turn malformed input into a successful
/// comparison. Backend-specific transport policy belongs in the backend adapter;
/// this function supplies only the abstract comparison hook. Product verdicts
/// using it must serialize the typed policy bound to this predicate.
pub fn log_diff_summary_from_strs_with_filter(
    file_a_str: impl AsRef<str>,
    file_b_str: impl AsRef<str>,
    opts: &LogDiffOpts,
    w: &mut impl std::io::Write,
    keep_record: impl Fn(&str) -> bool,
) -> std::io::Result<LogDiffSummary> {
    // A log that reached its size bound stops early and says so in-band. The
    // comparison below walks the two selected lists in lockstep, so it speaks
    // only for the retained prefix: two runs both cut at the bound with equal
    // retained message counts would agree on that prefix while the discarded
    // tails were never compared, and reporting that as "no differences found"
    // would assert determinism over a region nothing looked at. Refuse the
    // verdict instead, in both directions of the green predicate --
    // `diff_found` is set AND the compared counts stay zero, so neither
    // `log_diff() == false` nor `matched_with_evidence()` can read as a match.
    let truncated_a = log_was_truncated(file_a_str.as_ref());
    let truncated_b = log_was_truncated(file_b_str.as_ref());
    if truncated_a || truncated_b {
        let which_side = match (truncated_a, truncated_b) {
            (true, true) => "both logs were",
            (true, false) => "the first log was",
            (false, true) => "the second log was",
            (false, false) => unreachable!("guarded by the condition above"),
        };
        let refusal_reason = format!(
            "{which_side} truncated at the configured size bound (the log ends with the bounded writer's truncation marker). The discarded tail was never written, so no comparison of these files can establish that the runs agree past that point. Re-run with a larger HERMIT_LOG_MAX_BYTES, or 0 to disable the bound."
        );
        writeln!(
            w,
            "REFUSING to compare: {refusal_reason} This is a NO-RESULT, not a difference and not a match."
        )?;
        return Ok(LogDiffSummary {
            diff_found: true,
            compared_left: 0,
            compared_right: 0,
            first_divergent_scheduler_turn: None,
            first_divergent_virtual_nanoseconds: None,
            first_divergent_record: None,
            first_divergent_syscall: None,
            first_divergent_left_message: None,
            first_divergent_right_message: None,
            refusal_reason: Some(refusal_reason),
        });
    }

    let extracted_a = extract_log_messages(file_a_str.as_ref())?;
    let extracted_b = extract_log_messages(file_b_str.as_ref())?;
    validate_structured_events(
        opts.side_labels.left.as_str(),
        &extracted_a,
        opts.require_structured_events,
    )?;
    validate_structured_events(
        opts.side_labels.right.as_str(),
        &extracted_b,
        opts.require_structured_events,
    )?;
    let all_a = filter_ignored(
        extracted_a
            .into_iter()
            .filter(|record| keep_record(record.text))
            .collect(),
        &opts.ignore_lines,
    );
    let all_b = filter_ignored(
        extracted_b
            .into_iter()
            .filter(|record| keep_record(record.text))
            .collect(),
        &opts.ignore_lines,
    );

    writeln!(
        w,
        "Logs contain {} | {} messages total",
        all_a.len(),
        all_b.len(),
    )?;

    let detcore_a = filter_detcore(&all_a);
    let detcore_b = filter_detcore(&all_b);
    let infos_a = filter_infos(&all_a);
    let infos_b = filter_infos(&all_b);
    let detlogs_a = opts.filter_deterministic(&detcore_a);
    let detlogs_b = opts.filter_deterministic(&detcore_b);
    let left_syscalls = collect_syscalls(&all_a);
    let right_syscalls = collect_syscalls(&all_b);
    writeln!(
        w,
        "Logs contain {} | {} detcore-specific messages",
        detcore_a.len(),
        detcore_b.len(),
    )?;
    writeln!(
        w,
        "Logs contain {} | {} INFO messages",
        infos_a.len(),
        infos_b.len(),
    )?;
    writeln!(
        w,
        "Logs contain {} | {} DETLOG & scheduler COMMIT messages",
        detlogs_a.len(),
        detlogs_b.len(),
    )?;

    let policy = LogComparisonPolicy::from_options(opts);

    if policy.normalization == LogNormalization::Stripped {
        writeln!(
            w,
            "Normalizing known nondeterministic numerical data before comparison..."
        )?;
    } else if policy.normalization == LogNormalization::Canonical {
        writeln!(
            w,
            "Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly..."
        )?;
    }

    let (which, compared_a, compared_b) = match policy.comparison {
        LogComparisonMode::Deterministic => ("DETLOG", &detlogs_a, &detlogs_b),
        LogComparisonMode::Info => ("INFO", &infos_a, &infos_b),
        LogComparisonMode::FullTrace => ("full trace", &all_a, &all_b),
    };

    // Prepare both complete streams exactly once. Address ordinals are assigned
    // by first appearance across the full selected stream, and every consumer
    // below (printing, position reporting, and comparison) receives these same
    // String values rather than independently rendering the logs.
    let prepared_a = messages_for_comparison(compared_a, policy);
    let prepared_b = messages_for_comparison(compared_b, policy);

    if opts.print_logs {
        write_compared_logs(w, policy, &prepared_a, &prepared_b, &opts.side_labels)?;
    }

    let first_different =
        first_different_message_indices(compared_a, &prepared_a, compared_b, &prepared_b);
    let first_position_candidate = first_different.and_then(|(left_index, right_index)| {
        left_index
            .and_then(|index| commit_position_at_or_before(&all_a, index))
            .or_else(|| right_index.and_then(|index| commit_position_at_or_before(&all_b, index)))
    });

    let first_divergent_syscall_candidate = first_different.and_then(|(left, right)| {
        left.and_then(|index| finished_syscall_at_or_before(&all_a, index))
            .or_else(|| right.and_then(|index| finished_syscall_at_or_before(&all_b, index)))
    });
    let first_divergent_left_message = first_different
        .and_then(|(left, _)| compared_message_at_record(compared_a, &prepared_a, left));
    let first_divergent_right_message = first_different
        .and_then(|(_, right)| compared_message_at_record(compared_b, &prepared_b, right));

    let diff_found = if opts.git_diff {
        git_diff(
            which,
            (compared_a, &prepared_a),
            (compared_b, &prepared_b),
            opts,
            w,
            &left_syscalls,
            &right_syscalls,
        )?
    } else {
        diff_vecs(
            which,
            (compared_a, &prepared_a),
            (compared_b, &prepared_b),
            opts,
            w,
            &left_syscalls,
            &right_syscalls,
        )?
    };

    let summary = LogDiffSummary {
        diff_found,
        compared_left: compared_a.len(),
        compared_right: compared_b.len(),
        first_divergent_scheduler_turn: diff_found
            .then_some(first_position_candidate)
            .flatten()
            .map(|(turn, _)| turn),
        first_divergent_virtual_nanoseconds: diff_found
            .then_some(first_position_candidate)
            .flatten()
            .and_then(|(_, time)| time),
        first_divergent_record: diff_found
            .then_some(first_different)
            .flatten()
            .and_then(|(left_index, right_index)| left_index.or(right_index)),
        first_divergent_syscall: diff_found
            .then_some(first_divergent_syscall_candidate)
            .flatten(),
        first_divergent_left_message: diff_found.then_some(first_divergent_left_message).flatten(),
        first_divergent_right_message: diff_found
            .then_some(first_divergent_right_message)
            .flatten(),
        // This path compared the logs; only the refusal above declines to.
        refusal_reason: None,
    };

    if diff_found {
        writeln!(w, "Done processing logs, differences found.")?;
    } else if summary.compared_left == 0 && summary.compared_right == 0 {
        // Say this plainly rather than letting "no differences" stand in for
        // "nothing was compared": a caller that treats the two alike turns a
        // no-result into a green.
        writeln!(
            w,
            "Done processing logs, but ZERO {which} messages were selected on either side: \
             nothing was compared (no-result, not a match)."
        )?;
    } else {
        writeln!(
            w,
            "Done processing logs, no substantive differences found ({} | {} {which} messages compared).",
            summary.compared_left, summary.compared_right,
        )?;
        // A matching pair keeps only this summary; its logs are not retained.
        // Without the next line there is no record of which shutdown path the
        // two runs took, so establishing whether a guest is exposed to the
        // empty-run-queue timing race at all needs many repeated runs rather
        // than one reading. See [`SCHEDULER_EMPTY_QUEUE_KICK`] for why the count
        // is the only thing that distinguishes the paths.
        //
        // Deliberately emitted here only: a diverging pair already reproduces
        // the messages verbatim in its diff, and widening retention past this
        // one line is how evidence directories stop being navigable.
        writeln!(
            w,
            "Logs contain {} | {} scheduler empty-run-queue kick messages",
            count_empty_queue_kicks(&infos_a),
            count_empty_queue_kicks(&infos_b),
        )?;
        // The other retained record, for the same reason and under the same
        // limit: one line, this record only. Here the committed virtual time is
        // the evidence rather than the record's presence, so it is reported
        // alongside the turn.
        //
        // BOTH runs' values are printed, not just run 1's, and this is a
        // correctness requirement rather than a precaution.
        //
        // Measured: with `strip_lines` -- the lossy comparator that plain
        // `--verify` uses -- known nondeterministic numerical data is normalized
        // before comparison, so a pair whose two runs committed the map read at
        // *different* virtual times is a MATCHING pair and reaches this branch.
        // Quoting one side there would report agreement on precisely the
        // quantity this record exists to expose: a drift that `--verify-strict`
        // catches and `--verify` does not. The counts above can likewise differ
        // on a matching pair -- under the default `Deterministic` mode a kick
        // asymmetry is not compared, so `1 | 0` is a pass.
        //
        // A run with no such record therefore has to read as "no such record"
        // rather than being silently represented by the other run's value.
        let (maps_left, first_left) = maps_read_commits(&infos_a);
        let (maps_right, first_right) = maps_read_commits(&infos_b);
        let positions = if first_left.is_none() && first_right.is_none() {
            String::new()
        } else {
            format!(
                " ({} {}, {} {})",
                opts.side_labels.left,
                describe_maps_commit(first_left),
                opts.side_labels.right,
                describe_maps_commit(first_right),
            )
        };
        writeln!(
            w,
            "Logs contain {maps_left} | {maps_right} scheduler COMMIT records reading /proc/self/maps{positions}",
        )?;
    }
    Ok(summary)
}

#[cfg(test)]
mod test {
    use clap::CommandFactory;
    use clap::Parser;
    use pretty_assertions::assert_eq;

    use super::finished_syscall_at_or_before;
    use super::finished_syscall_number;
    use crate::detlog::DetLogEvent;
    use crate::logdiff::DetLogFilter;

    /// One well-formed log record. Records are delimited by their leading
    /// timestamp, so `body` may contain newlines and still be one record.
    fn record(second: usize, body: &str) -> String {
        format!("Apr 09 06:08:{second:02}.100  INFO detcore: {body}\n")
    }

    fn structured_record(second: usize, body: &str, event: DetLogEvent) -> String {
        record(
            second,
            &format!("{body}{}", crate::detlog::record_suffix(event)),
        )
    }

    fn historical(index: usize, text: &str) -> super::LogMessage<'_> {
        super::LogMessage {
            index,
            text,
            event: None,
        }
    }

    fn indexed_text<'a>(messages: &'a [super::LogMessage<'a>]) -> Vec<(usize, &'a str)> {
        messages
            .iter()
            .map(|message| (message.index, message.text))
            .collect()
    }

    fn info_opts() -> super::LogDiffOpts {
        super::LogDiffOpts {
            comparison: super::LogComparisonMode::Info,
            ..Default::default()
        }
    }

    fn compare(left: &str, right: &str) -> super::PrefixComparison {
        super::compare_complete_prefix(left, right, &info_opts(), &mut Vec::new())
            .expect("comparing in-memory strings cannot fail on I/O")
    }

    fn temp_log(contents: &str) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().expect("create temporary log");
        std::fs::write(file.path(), contents).expect("write temporary log");
        file
    }

    #[test]
    fn bitwise_info_v1_binds_the_complete_policy() {
        let labels = super::ComparisonSideLabels::new("left", "right");
        let options = super::bitwise_info_v1_options(labels.clone());
        assert!(!options.strip_lines);
        assert!(options.canonicalize_addresses);
        assert_eq!(options.comparison, super::LogComparisonMode::Info);
        assert_eq!(options.side_labels, labels);
        assert!(options.require_structured_events);
        assert!(!options.print_logs);
        assert_eq!(options.limit, 20);
        assert!(options.ignore_lines.is_empty());
        assert_eq!(options.syscall_history, 5);
        assert!(!options.no_color);
        assert!(!options.skip_commit);
        assert!(!options.skip_detlog);
        assert!(!options.git_diff);
        assert_eq!(
            options.include_detlogs,
            [
                DetLogFilter::Syscall,
                DetLogFilter::SyscallResult,
                DetLogFilter::Other,
            ]
        );
    }

    #[test]
    fn bitwise_info_v1_matches_and_renders_current_records() -> std::io::Result<()> {
        let left_text = structured_record(
            1,
            &format!("DETLOG allocation={}", super::host_addr(0x1000)),
            DetLogEvent::Other,
        );
        let right_text = structured_record(
            1,
            &format!("DETLOG allocation={}", super::host_addr(0x9000)),
            DetLogEvent::Other,
        );
        let left = temp_log(&left_text);
        let right = temp_log(&right_text);
        let summary = super::try_compare_bitwise_info_v1(
            left.path(),
            right.path(),
            super::ComparisonSideLabels::new("left", "right"),
        )?;
        assert!(summary.matched_with_evidence());
        assert_eq!((summary.compared_left, summary.compared_right), (1, 1));

        let mut rendered = Vec::new();
        assert_eq!(
            super::write_bitwise_info_v1_bytes(left_text.as_bytes(), "left", &mut rendered)?,
            1
        );
        let rendered = String::from_utf8(rendered).unwrap();
        assert!(rendered.contains("<addr1>"));
        assert!(!rendered.contains("0x1000"));
        Ok(())
    }

    #[test]
    fn bitwise_info_v1_reports_first_divergence() -> std::io::Result<()> {
        let left = structured_record(1, "DETLOG payload=left", DetLogEvent::Other);
        let right = structured_record(1, "DETLOG payload=right", DetLogEvent::Other);
        let mut diagnostic = Vec::new();
        let (summary, records_left, records_right) =
            super::try_compare_bitwise_info_v1_bytes_with_records_and_diagnostics(
                left.as_bytes(),
                right.as_bytes(),
                super::ComparisonSideLabels::new("left", "right"),
                super::BitwiseInfoV1Diagnostics {
                    difference_limit: 1,
                    syscall_history: 0,
                    no_color: true,
                    print_logs: false,
                },
                &mut diagnostic,
            )?;
        assert!(summary.diff_found);
        assert!(!summary.refused);
        assert_eq!(summary.first_divergent_record, Some(1));
        assert_eq!((summary.compared_left, summary.compared_right), (1, 1));
        assert_eq!((records_left, records_right), (1, 1));
        assert!(
            String::from_utf8(diagnostic)
                .unwrap()
                .contains("payload=left")
        );
        Ok(())
    }

    #[test]
    fn bitwise_info_v1_diagnostics_do_not_change_the_verdict() -> std::io::Result<()> {
        let left = structured_record(1, "DETLOG payload=left", DetLogEvent::Other);
        let right = structured_record(1, "DETLOG payload=right", DetLogEvent::Other);
        let baseline = super::try_compare_bitwise_info_v1_bytes_with_records(
            left.as_bytes(),
            right.as_bytes(),
            super::ComparisonSideLabels::default(),
        )?;
        let mut diagnostic = Vec::new();
        let varied = super::try_compare_bitwise_info_v1_bytes_with_records_and_diagnostics(
            left.as_bytes(),
            right.as_bytes(),
            super::ComparisonSideLabels::default(),
            super::BitwiseInfoV1Diagnostics {
                difference_limit: 0,
                syscall_history: 10,
                no_color: true,
                print_logs: true,
            },
            &mut diagnostic,
        )?;
        assert_eq!(baseline, varied);
        assert!(!diagnostic.is_empty());
        Ok(())
    }

    #[test]
    fn bitwise_info_v1_refuses_empty_missing_and_unreadable_inputs() -> std::io::Result<()> {
        let empty_left = temp_log("");
        let empty_right = temp_log("");
        let summary = super::try_compare_bitwise_info_v1(
            empty_left.path(),
            empty_right.path(),
            super::ComparisonSideLabels::default(),
        )?;
        assert!(!summary.matched_with_evidence());
        assert_eq!((summary.compared_left, summary.compared_right), (0, 0));

        let missing_parent = tempfile::tempdir()?;
        let missing = missing_parent.path().join("missing.log");
        assert!(
            super::try_compare_bitwise_info_v1(
                &missing,
                empty_right.path(),
                super::ComparisonSideLabels::default(),
            )
            .is_err()
        );
        let directory = tempfile::tempdir()?;
        assert!(
            super::try_compare_bitwise_info_v1(
                directory.path(),
                empty_right.path(),
                super::ComparisonSideLabels::default(),
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn bitwise_info_v1_refuses_invalid_utf8_and_truncation() -> std::io::Result<()> {
        let valid = structured_record(1, "DETLOG payload", DetLogEvent::Other);
        let mut invalid = valid.clone().into_bytes();
        invalid.insert(invalid.len() - 1, 0x80);
        let error = super::try_compare_bitwise_info_v1_bytes_with_records(
            &invalid,
            valid.as_bytes(),
            super::ComparisonSideLabels::new("left", "right"),
        )
        .expect_err("invalid UTF-8 must refuse");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("left"));

        let truncated = temp_log(&format!("{valid}{}\n", super::TRUNCATION_MARKER));
        let complete = temp_log(&valid);
        let summary = super::try_compare_bitwise_info_v1(
            truncated.path(),
            complete.path(),
            super::ComparisonSideLabels::default(),
        )?;
        assert!(summary.diff_found);
        assert!(summary.refused);
        assert_eq!((summary.compared_left, summary.compared_right), (0, 0));
        Ok(())
    }

    #[test]
    fn bitwise_info_v1_requires_current_structured_events() {
        let historical = temp_log(&record(1, "DETLOG stable"));
        let error = super::try_compare_bitwise_info_v1(
            historical.path(),
            historical.path(),
            super::ComparisonSideLabels::new("left", "right"),
        )
        .expect_err("prose-only DETLOG must refuse");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(
            error
                .to_string()
                .contains("missing its structured DETLOG result")
        );
    }

    #[test]
    fn bitwise_info_v1_complete_prefix_withholds_the_unfinished_tail() -> std::io::Result<()> {
        let common = structured_record(1, "DETLOG common", DetLogEvent::Other);
        let left = format!(
            "{common}{}",
            structured_record(2, "DETLOG unfinished-left", DetLogEvent::Other)
        );
        let right = format!(
            "{common}{}",
            structured_record(2, "DETLOG unfinished-right", DetLogEvent::Other)
        );
        let comparison = super::compare_complete_bitwise_info_v1_prefix(
            left.as_bytes(),
            right.as_bytes(),
            super::ComparisonSideLabels::new("left", "right"),
            super::BitwiseInfoV1Diagnostics::default(),
            &mut Vec::new(),
        )?;
        assert_eq!(comparison.records_available_left, 1);
        assert_eq!(comparison.records_available_right, 1);
        assert_eq!(comparison.records_compared, 1);
        assert!(comparison.summary.matched_with_evidence());
        assert_eq!(comparison.summary.compared_left, 1);
        assert_eq!(comparison.summary.compared_right, 1);
        Ok(())
    }

    #[test]
    fn a_record_is_complete_only_once_the_next_one_starts() {
        assert_eq!(super::complete_record_count(""), 0);

        // One record start means one record that is still being written.
        let one = record(1, "first");
        assert_eq!(super::complete_record_count(&one), 0);

        // The second start is what proves the first record finished.
        let two = format!("{}{}", record(1, "first"), record(2, "second"));
        assert_eq!(super::complete_record_count(&two), 1);

        let three = format!("{two}{}", record(3, "third"));
        assert_eq!(super::complete_record_count(&three), 2);
    }

    #[test]
    fn a_multiline_record_counts_once_and_is_not_split_at_its_newlines() {
        let multiline = format!(
            "{}{}",
            record(1, "first\n    continued detail\n    more detail"),
            record(2, "second")
        );
        // Two starts, so one complete record -- the embedded newlines are part
        // of record one, not boundaries of their own.
        assert_eq!(super::complete_record_count(&multiline), 1);

        let prefix = super::take_complete_records(&multiline, 1).unwrap();
        assert!(prefix.contains("continued detail"));
        assert!(prefix.contains("more detail"));
        assert!(!prefix.contains("second"));
    }

    #[test]
    fn asking_past_the_written_end_is_none_not_a_short_answer() {
        let two = format!("{}{}", record(1, "first"), record(2, "second"));
        assert_eq!(super::take_complete_records(&two, 0), Some(""));
        assert!(super::take_complete_records(&two, 1).is_some());
        // Only one record is complete, so two is not yet readable. Returning a
        // truncated prefix here would let "not written yet" pass as "agrees".
        assert_eq!(super::take_complete_records(&two, 2), None);
        assert_eq!(super::take_complete_records(&two, 99), None);
    }

    #[test]
    fn a_half_written_final_record_is_never_a_difference() {
        // Identical complete records; the runs differ only in the tail that
        // neither has finished flushing.
        let left = format!(
            "{}{}{}",
            record(1, "same"),
            record(2, "same"),
            record(3, "TAIL-LEFT")
        );
        let right = format!(
            "{}{}{}",
            record(1, "same"),
            record(2, "same"),
            record(3, "TAIL-RIGHT-AND-LONGER")
        );

        let comparison = compare(&left, &right);
        assert!(
            !comparison.summary.diff_found,
            "an unfinished record must not read as a divergence"
        );
        assert_eq!(comparison.records_compared, 2);
        assert!(!comparison.one_side_is_ahead());
    }

    #[test]
    fn a_difference_inside_the_completed_prefix_is_found() {
        let left = format!(
            "{}{}{}",
            record(1, "same"),
            record(2, "LEFT"),
            record(3, "tail")
        );
        let right = format!(
            "{}{}{}",
            record(1, "same"),
            record(2, "RIGHT"),
            record(3, "tail")
        );

        let comparison = compare(&left, &right);
        assert!(comparison.summary.diff_found);
        assert_eq!(comparison.records_compared, 2);
    }

    #[test]
    fn comparison_is_bounded_by_the_shorter_log_and_says_so() {
        let ahead = format!(
            "{}{}{}{}{}",
            record(1, "same"),
            record(2, "same"),
            record(3, "same"),
            record(4, "same"),
            record(5, "same")
        );
        let behind = format!(
            "{}{}{}",
            record(1, "same"),
            record(2, "same"),
            record(3, "same")
        );

        let comparison = compare(&ahead, &behind);
        assert!(!comparison.summary.diff_found);
        assert_eq!(comparison.records_available_left, 4);
        assert_eq!(comparison.records_available_right, 2);
        // Only what both sides have finished writing.
        assert_eq!(comparison.records_compared, 2);
        assert!(
            comparison.one_side_is_ahead(),
            "the caller must be able to see the comparison was reading-bound"
        );
    }

    #[test]
    fn the_first_differing_record_is_located_not_just_bounded() {
        // 40 identical records, one difference at record 13, then 40 more.
        let build = |marker: &str| {
            (1..=81)
                .map(|index| {
                    let body = if index == 13 { marker } else { "same" };
                    record(index % 60, &format!("record {index} {body}"))
                })
                .collect::<String>()
        };
        let left = build("LEFT");
        let right = build("RIGHT");

        let found = compare(&left, &right).summary.first_divergent_record;
        assert_eq!(
            found,
            Some(13),
            "bisection must name the record, not merely the prefix that contains it"
        );
    }

    #[test]
    fn identical_logs_have_no_first_divergent_record() {
        let same = format!("{}{}{}", record(1, "a"), record(2, "b"), record(3, "c"));
        assert_eq!(compare(&same, &same).summary.first_divergent_record, None);
        // And an empty comparison reports no location rather than record zero.
        assert_eq!(compare("", "").summary.first_divergent_record, None);
    }

    /// An untagged line REFUSES the comparison instead of panicking, and the
    /// refusal names the line. A panic is the wrong failure mode for a tool
    /// people reach for when something is already broken, and the `--json`
    /// consumer could not distinguish a crash from a real `no_result` verdict.
    ///
    /// It still refuses rather than SKIPPING: dropping the line would silently
    /// change the compared surface, which is a disclosed, versioned decision
    /// belonging to the record envelope, not to the parser.
    #[test]
    fn an_untagged_line_is_refused_by_name_rather_than_panicking() {
        // THE REAL SHAPE, and the reason it bites: records are delimited by the
        // wall-clock PREFIX, not by newlines. DBT emits no timestamp prefix at
        // all and writes its own untagged startup lines first, so the whole
        // thing forms one segment beginning with untagged text. A line placed
        // AFTER a timestamped record would simply be absorbed into that
        // record's body and never seen as its own -- which is why this fixture
        // has no timestamp.
        let log = "detcore-dbt: background client thread entered\n";
        let error = super::extract_log_messages(log)
            .expect_err("an untagged line must refuse, not be admitted");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        let message = error.to_string();
        assert!(
            message.contains("detcore-dbt: background client thread entered"),
            "the refusal must name the offending line, got: {message}"
        );
        assert!(
            message.contains("no ERROR/WARN/INFO/DEBUG/TRACE tag"),
            "the refusal must say why, got: {message}"
        );
    }

    /// A fully tagged log is unaffected -- the refusal must not become a
    /// tripwire on ordinary input.
    #[test]
    fn a_fully_tagged_log_still_parses() {
        let log = "2026-08-24T20:16:17.897469Z  INFO detcore: DETLOG a\n\
                   2026-08-24T20:16:17.897470Z  WARN detcore: b\n";
        let records = super::extract_log_messages(log).expect("tagged log parses");
        assert_eq!(records.len(), 2);
    }

    /// Historical logs remain readable through their `finish syscall #N`
    /// spelling. Current verification separately refuses that shape.
    #[test]
    fn historical_syscall_numbers_remain_readable() {
        assert_eq!(
            finished_syscall_number(&historical(
                0,
                "DETLOG [syscall][detcore, dtid 3] finish syscall #37: write(1, 0x5, 6) = Ok(6)"
            )),
            Some(37)
        );
        // `inbound` records carry no number: the guest has not got anywhere yet.
        assert_eq!(
            finished_syscall_number(&historical(
                0,
                "DETLOG [syscall][detcore, dtid 3] inbound syscall: brk(NULL) = ?"
            )),
            None
        );
        assert_eq!(
            finished_syscall_number(&historical(0, "no syscall here")),
            None
        );
    }

    /// Reports the LAST syscall completed at or before the divergence, and
    /// reports NONE when none had completed. The second half is a real state
    /// rather than a gap: a run can diverge during startup, before the guest
    /// has finished a single syscall. Measured on a real 131-line log --
    /// diverging at record 12 reported no syscall, while diverging at record 98
    /// reported syscall 37.
    #[test]
    fn the_syscall_count_is_the_last_one_completed_before_the_divergence() {
        let finished = |n: u64| format!("finish syscall #{n}: write(1, 0x5, 6) = Ok(6)");
        let a = finished(2);
        let b = finished(37);
        let syscalls = vec![historical(10, a.as_str()), historical(90, b.as_str())];
        assert_eq!(finished_syscall_at_or_before(&syscalls, 98), Some(37));
        assert_eq!(finished_syscall_at_or_before(&syscalls, 90), Some(37));
        assert_eq!(finished_syscall_at_or_before(&syscalls, 50), Some(2));
        assert_eq!(
            finished_syscall_at_or_before(&syscalls, 9),
            None,
            "a divergence before any syscall completed has no syscall count, \
             and that is a state rather than a missing value"
        );
    }

    #[test]
    fn structured_positions_and_syscall_counts_are_authoritative() -> std::io::Result<()> {
        let run = |turn: u64, time: u64, syscall: u64, value: u64| {
            format!(
                "{}{}{}",
                structured_record(
                    1,
                    "COMMIT turn 999 at time 999",
                    DetLogEvent::SchedulerCommit {
                        scheduler_turn: turn,
                        virtual_nanoseconds: time,
                        internal_io_poll: false,
                        runtime_maps_read: false,
                    },
                ),
                structured_record(
                    2,
                    "DETLOG [syscall] finish syscall #999: write = Ok(1)",
                    DetLogEvent::SyscallResult {
                        finished_syscall_number: syscall,
                    },
                ),
                structured_record(3, &format!("DETLOG value={value}"), DetLogEvent::Other),
            )
        };
        let options = super::LogDiffOpts {
            require_structured_events: true,
            ..Default::default()
        };

        let original = super::log_diff_summary_from_strs(
            run(17, 123, 37, 1),
            run(17, 123, 37, 2),
            &options,
            &mut Vec::new(),
        )?;
        assert_eq!(original.first_divergent_scheduler_turn, Some(17));
        assert_eq!(original.first_divergent_virtual_nanoseconds, Some(123));
        assert_eq!(original.first_divergent_syscall, Some(37));

        let mutated = super::log_diff_summary_from_strs(
            run(18, 124, 38, 1),
            run(18, 124, 38, 2),
            &options,
            &mut Vec::new(),
        )?;
        assert_eq!(mutated.first_divergent_scheduler_turn, Some(18));
        assert_eq!(mutated.first_divergent_virtual_nanoseconds, Some(124));
        assert_eq!(mutated.first_divergent_syscall, Some(38));
        Ok(())
    }

    #[test]
    fn current_verification_refuses_a_missing_structured_record_by_name() {
        let options = super::LogDiffOpts {
            require_structured_events: true,
            ..Default::default()
        };
        let error = super::log_diff_summary_from_strs(
            record(1, "DETLOG value=1"),
            record(1, "DETLOG value=1"),
            &options,
            &mut Vec::new(),
        )
        .expect_err("current verification must not fall back to prose");
        assert!(
            error
                .to_string()
                .contains("missing its structured DETLOG result"),
            "refusal must name the missing result: {error}"
        );
    }

    #[test]
    fn structured_kind_not_the_human_tag_selects_the_syscall_class() {
        let text = "INFO detcore: DETLOG [syscall] inbound syscall: read = ?";
        let other = super::LogMessage {
            index: 0,
            text,
            event: Some(DetLogEvent::Other),
        };
        let syscall = super::LogMessage {
            event: Some(DetLogEvent::Syscall),
            ..other
        };
        assert!(!super::is_detlog_syscall(&other));
        assert!(super::is_detlog_syscall(&syscall));
    }

    #[test]
    fn structured_scheduler_flags_control_filtering_and_retained_counts() {
        let text = "INFO detcore::scheduler: COMMIT turn 999 at time 999";
        let internal = super::LogMessage {
            index: 0,
            text,
            event: Some(DetLogEvent::SchedulerCommit {
                scheduler_turn: 17,
                virtual_nanoseconds: 123,
                internal_io_poll: true,
                runtime_maps_read: false,
            }),
        };
        let maps_read = super::LogMessage {
            event: Some(DetLogEvent::SchedulerCommit {
                scheduler_turn: 17,
                virtual_nanoseconds: 123,
                internal_io_poll: false,
                runtime_maps_read: true,
            }),
            ..internal
        };

        assert!(
            super::LogDiffOpts::default()
                .filter_deterministic(&[internal])
                .is_empty()
        );
        assert_eq!(
            super::LogDiffOpts::default()
                .filter_deterministic(&[maps_read])
                .len(),
            1
        );
        assert_eq!(super::maps_read_commits(&[internal]), (0, None));
        assert_eq!(
            super::maps_read_commits(&[maps_read]),
            (1, Some((17, Some(123))))
        );
    }

    #[test]
    fn nothing_written_yet_is_a_no_result_not_a_match() {
        // A single unfinished record on each side: zero complete records.
        let comparison = compare(&record(1, "first"), &record(1, "first"));
        assert_eq!(comparison.records_compared, 0);
        assert!(!comparison.summary.diff_found);
        assert!(
            !comparison.summary.matched_with_evidence(),
            "comparing zero records must never report a match"
        );
    }

    #[test]
    fn unsafe_strip_lines_cli_name_and_warning_are_explicit() {
        let options = super::LogDiffOpts::try_parse_from(["log-diff", "--unsafe-strip-lines"])
            .expect("the explicitly unsafe spelling should parse");
        assert!(options.strip_lines);

        assert!(super::LogDiffOpts::try_parse_from(["log-diff", "--strip-lines"]).is_err());

        let mut help = Vec::new();
        super::LogDiffOpts::command()
            .write_long_help(&mut help)
            .expect("write clap help");
        let help = String::from_utf8(help).expect("help is UTF-8");
        assert!(help.contains("--unsafe-strip-lines"));
        assert!(help.contains("erases timestamps and syscall values"));
        assert!(help.contains("make a failing parity diff pass"));
        assert!(help.contains("doing so is cheating"));
        assert!(!help.contains("--strip-lines"));
    }

    #[test]
    fn test_compare_with_no_color() {
        let str1 = "test1";
        let str2 = "test2";

        assert_eq!(
            format!("{}", super::Comparison::new(true, str1, str2))
                .split('\n')
                .collect::<Vec<&str>>(),
            ["Diff < left / right > :", "<\"test1\"", ">\"test2\"", "",]
        );
    }

    #[test]
    fn test_compare_with_color() {
        let str1 = "test1";
        let str2 = "test2";

        assert_eq!(
            format!("{}", super::Comparison::new(false, str1, str2))
                .split('\n')
                .collect::<Vec<&str>>(),
            [
                "\u{1b}[1mDiff\u{1b}[0m \u{1b}[31m< left\u{1b}[0m / \u{1b}[32mright >\u{1b}[0m :",
                "\u{1b}[31m<\"test\u{1b}[0m\u{1b}[1;48;5;52;31m1\u{1b}[0m\u{1b}[31m\"\u{1b}[0m",
                "\u{1b}[32m>\"test\u{1b}[0m\u{1b}[1;48;5;22;32m2\u{1b}[0m\u{1b}[32m\"\u{1b}[0m",
                "",
            ]
        );
    }

    /// The two directions of the truncation refusal, on the SAME pair of logs.
    ///
    /// Both cases feed identical, matching DETLOG content; the only difference
    /// is whether a side carries the bounded writer's marker. So a pass here
    /// cannot come from the comparison being broken in general, and the refusal
    /// cannot come from the content differing.
    #[test]
    fn truncated_logs_are_refused_and_untruncated_logs_still_match() -> std::io::Result<()> {
        let body = "2022-09-06T14:15:47.000000Z INFO detcore: DETLOG [syscall] finish syscall #1: read(3, 0x1000, 1) = Ok(1)\n2022-09-06T14:15:48.000000Z INFO detcore: DETLOG [syscall] finish syscall #2: write(1, 0x2000, 1) = Ok(1)";
        let marked = format!("{body}\n{}\n", super::TRUNCATION_MARKER);
        let options = super::LogDiffOpts {
            no_color: true,
            ..Default::default()
        };

        // Direction 1: no marker on either side -> a real comparison happens,
        // finds no difference, and carries nonzero evidence.
        let clean = super::log_diff_summary_from_strs(body, body, &options, &mut Vec::new())?;
        assert!(!clean.diff_found, "identical untruncated logs must match");
        assert!(
            clean.matched_with_evidence(),
            "the untruncated match must carry nonzero compared counts, got {clean:?}"
        );
        assert!(clean.refusal_reason.is_none());

        // Direction 2: the marker on the left, the right, or both -> refused,
        // even though the retained content is byte-identical to direction 1.
        for (label, left, right) in [
            ("left", marked.as_str(), body),
            ("right", body, marked.as_str()),
            ("both", marked.as_str(), marked.as_str()),
        ] {
            let mut out = Vec::new();
            let summary = super::log_diff_summary_from_strs(left, right, &options, &mut out)?;
            assert!(
                summary.diff_found,
                "{label}: a truncated log must not be reported as a match"
            );
            assert_eq!(
                (summary.compared_left, summary.compared_right),
                (0, 0),
                "{label}: nothing was compared, so the counts must not claim otherwise"
            );
            assert!(
                summary
                    .refusal_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("truncated at the configured size bound")),
                "{label}: the typed result must retain the printed refusal cause"
            );
            assert!(
                !summary.matched_with_evidence(),
                "{label}: the evidence predicate must also refuse"
            );
            let text = String::from_utf8(out).unwrap();
            assert!(
                text.contains("REFUSING to compare"),
                "{label}: the refusal must be stated, got: {text}"
            );
            assert!(
                !text.contains("no substantive differences found"),
                "{label}: a refusal must never print the match line, got: {text}"
            );
        }

        Ok(())
    }

    /// The refusal must fire on "this log was truncated", never on "this log
    /// mentions the marker".
    ///
    /// DETLOG records guest syscall path arguments verbatim, so the log text is
    /// partly guest-controlled. The first version of this refusal searched the
    /// whole text for a marker PREFIX, which let a guest that merely touched a
    /// path containing that prefix refuse its own `--verify` -- and, because
    /// the poisoning came from content rather than from the bound, disabling
    /// the bound did not help. Each case below is a way the marker text can
    /// appear in a log that was NOT truncated.
    #[test]
    fn marker_text_in_guest_content_is_not_truncation() -> std::io::Result<()> {
        let options = super::LogDiffOpts {
            no_color: true,
            ..Default::default()
        };
        let marker = super::TRUNCATION_MARKER;
        // The exact shape the live reproducer produced: a `statx` path argument
        // carrying the marker prefix, mid-line, in a log that ran to completion.
        let guest_path_line = format!(
            "2022-09-06T14:15:47.000000Z INFO detcore: DETLOG [syscall] inbound syscall: \
             statx(-100, 0x7fff -> \"/tmp/{marker} probe\", AtFlags(AT_NO_AUTOMOUNT), 2, 0x7fff) \
             = ?"
        );
        let tail_line = "2022-09-06T14:15:48.000000Z INFO detcore: DETLOG [syscall] finish syscall #2: \
             write(1, 0x2000, 1) = Ok(1)";

        for (label, text) in [
            // The whole marker sentence, inside a DETLOG line, at end of file.
            (
                "marker inside the final DETLOG line",
                guest_path_line.clone(),
            ),
            // The marker on its own line, but the log continues afterwards --
            // so the writer cannot have produced it: it discards everything
            // after announcing.
            (
                "marker on its own line, followed by more log",
                format!("{guest_path_line}\n{marker}\n{tail_line}"),
            ),
            // Ends with the marker text, but not at a line boundary.
            (
                "marker at end of file but mid-line",
                format!("{tail_line}\nsomething {marker}"),
            ),
        ] {
            assert!(
                !super::log_was_truncated(&text),
                "{label}: an untruncated log must not be classified as truncated"
            );
            let mut out = Vec::new();
            let summary = super::log_diff_summary_from_strs(&text, &text, &options, &mut out)?;
            let printed = String::from_utf8(out).unwrap();
            assert!(
                !printed.contains("REFUSING to compare"),
                "{label}: must be compared, not refused, got: {printed}"
            );
            assert!(
                !summary.diff_found,
                "{label}: identical logs must compare equal, got {summary:?}"
            );
            assert!(
                summary.matched_with_evidence(),
                "{label}: the match must carry nonzero compared counts, got {summary:?}"
            );
        }

        // ...and the narrowing did not go so far that real truncation escapes:
        // the same guest content, actually cut at the bound, is still refused.
        let really_truncated = format!("{guest_path_line}\n{marker}\n");
        assert!(
            super::log_was_truncated(&really_truncated),
            "a log ending in the marker line IS truncated and must still be caught"
        );
        let mut out = Vec::new();
        let summary = super::log_diff_summary_from_strs(
            &really_truncated,
            &really_truncated,
            &options,
            &mut out,
        )?;
        assert!(summary.diff_found, "real truncation must still be refused");
        assert_eq!((summary.compared_left, summary.compared_right), (0, 0));
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("REFUSING to compare"),
            "real truncation must still print the refusal"
        );

        Ok(())
    }

    #[test]
    fn test_log_diff_with_color() -> std::io::Result<()> {
        let str1 = "INFO detcore: DETLOG [syscall][detcore, dtid 3]  finish syscall #11: mmap(NULL, 3954880, PROT_READ | PROT_EXEC, MAP_PRIVATE | MAP_DENYWRITE, 3, 0) = Ok(140737347883008)";
        let str2 = "INFO detcore: DETLOG [syscall][detcore, dtid 3]  finish syscall #15: mmap(NULL, 3954880, PROT_READ | PROT_EXEC, MAP_PRIVATE | MAP_DENYWRITE, 3, 0) = Ok(140737347883008)";
        let mut result = Vec::<u8>::new();

        super::log_diff_from_strs(
            str1,
            str2,
            &super::LogDiffOpts {
                limit: 1,
                strip_lines: false,
                canonicalize_addresses: false,
                comparison: super::LogComparisonMode::Deterministic,
                side_labels: super::ComparisonSideLabels::default(),
                require_structured_events: false,
                print_logs: false,
                syscall_history: 5,
                no_color: false,
                skip_commit: false,
                skip_detlog: false,
                git_diff: false,
                ignore_lines: Vec::new(),
                include_detlogs: vec![
                    DetLogFilter::Syscall,
                    DetLogFilter::SyscallResult,
                    DetLogFilter::Other,
                ],
            },
            &mut result,
        )?;

        let output = String::from_utf8(result).unwrap();
        assert!(output.contains("  Comparing DETLOG messages..."));
        assert!(output.contains("Mismatch at log messages 0 (run 1) and 0 (run 2)"));
        assert!(output.contains("run 1, log message 0: INFO detcore: DETLOG [syscall][detcore, dtid 3]  finish syscall #11"));
        assert!(output.contains("run 2, log message 0: INFO detcore: DETLOG [syscall][detcore, dtid 3]  finish syscall #15"));
        assert!(!output.contains("eliding the rest"));

        Ok(())
    }

    #[test]
    fn test_log_diff_reports_each_runs_syscall_context() -> std::io::Result<()> {
        let log_a = r#"2022-09-06T14:15:47.000000Z INFO detcore: DETLOG [syscall] finish syscall #1: read(3, 0x1000, 1) = Ok(1)
2022-09-06T14:15:48.000000Z INFO detcore: DETLOG [syscall] finish syscall #2: write(1, 0x2000, 1) = Ok(1)"#;
        let log_b = r#"2022-09-06T14:15:47.000000Z INFO detcore: DETLOG [syscall] finish syscall #1: read(3, 0x1000, 1) = Ok(1)
2022-09-06T14:15:48.000000Z INFO detcore: DETLOG [syscall] finish syscall #2: write(1, 0x3000, 1) = Ok(1)"#;
        let mut result = Vec::new();
        let options = super::LogDiffOpts {
            no_color: true,
            syscall_history: 1,
            ..Default::default()
        };

        assert!(super::log_diff_from_strs(
            log_a,
            log_b,
            &options,
            &mut result
        )?);

        let output = String::from_utf8(result).unwrap();
        assert!(output.contains("Mismatch at log messages 2 (run 1) and 2 (run 2)"));
        assert!(output.contains("run 1, log message 2: INFO detcore: DETLOG [syscall] finish syscall #2: write(1, 0x2000, 1) = Ok(1)"));
        assert!(output.contains("run 2, log message 2: INFO detcore: DETLOG [syscall] finish syscall #2: write(1, 0x3000, 1) = Ok(1)"));
        assert!(output.contains("Prior completed syscalls for run 1:"));
        assert!(output.contains("Prior completed syscalls for run 2:"));
        assert_eq!(output.matches("finish syscall #1: read").count(), 2);
        Ok(())
    }

    #[test]
    fn custom_side_labels_cover_mismatch_history_and_tail_diagnostics() -> std::io::Result<()> {
        let left = format!(
            "{}{}",
            record(1, "DETLOG [syscall] finish syscall #1: read = Ok(1)"),
            record(2, "DETLOG [syscall] finish syscall #2: write = Ok(1)"),
        );
        let right = format!(
            "{}{}",
            record(1, "DETLOG [syscall] finish syscall #1: read = Ok(1)"),
            record(2, "DETLOG [syscall] finish syscall #2: write = Err(5)"),
        );
        let options = super::LogDiffOpts {
            side_labels: super::ComparisonSideLabels::new("the recording", "the replay"),
            syscall_history: 1,
            no_color: true,
            ..Default::default()
        };
        let mut output = Vec::new();
        assert!(super::log_diff_from_strs(
            &left,
            &right,
            &options,
            &mut output
        )?);
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("Mismatch at log messages 2 (the recording) and 2 (the replay)"));
        assert!(output.contains("the recording, log message 2:"));
        assert!(output.contains("the replay, log message 2:"));
        assert!(output.contains("Prior completed syscalls for the recording:"));
        assert!(output.contains("Prior completed syscalls for the replay:"));
        assert!(!output.contains("run 1") && !output.contains("run 2"));

        let left = record(1, "DETLOG stable");
        let right = format!("{left}{}", record(2, "DETLOG extra"));
        let mut output = Vec::new();
        assert!(super::log_diff_from_strs(
            &left,
            &right,
            &options,
            &mut output
        )?);
        let output = String::from_utf8(output).unwrap();
        assert!(
            output.contains("The replay contains 1 extra messages not matched in the recording.")
        );
        assert!(!output.contains("run 1") && !output.contains("run 2"));

        let mut output = Vec::new();
        assert!(super::log_diff_from_strs(
            &right,
            &left,
            &options,
            &mut output
        )?);
        let output = String::from_utf8(output).unwrap();
        assert!(
            output.contains("The recording contains 1 extra messages not matched in the replay.")
        );
        Ok(())
    }

    fn printed_labeled_log<'a>(output: &'a str, label: &str) -> &'a str {
        let start = format!("--- begin {label} compared log ---\n");
        let end = format!("--- end {label} compared log ---\n");
        output
            .split_once(&start)
            .expect("printed log start marker")
            .1
            .split_once(&end)
            .expect("printed log end marker")
            .0
    }

    #[test]
    fn custom_side_labels_name_printed_logs() -> std::io::Result<()> {
        let options = super::LogDiffOpts {
            side_labels: super::ComparisonSideLabels::new("the recording", "the replay"),
            print_logs: true,
            no_color: true,
            ..Default::default()
        };
        let mut output = Vec::new();
        let summary = super::log_diff_summary_from_strs(
            record(1, "DETLOG recorded"),
            record(1, "DETLOG replayed"),
            &options,
            &mut output,
        )?;
        assert!(summary.diff_found);
        let output = String::from_utf8(output).unwrap();
        assert_eq!(
            printed_labeled_log(&output, "the recording"),
            "INFO detcore: DETLOG recorded\n"
        );
        assert_eq!(
            printed_labeled_log(&output, "the replay"),
            "INFO detcore: DETLOG replayed\n"
        );
        assert!(!output.contains("begin run 1") && !output.contains("begin run 2"));
        Ok(())
    }

    fn printed_log(output: &str, run: u8) -> &str {
        let start = format!("--- begin run {run} compared log ---\n");
        let end = format!("--- end run {run} compared log ---\n");
        output
            .split_once(&start)
            .expect("printed log start marker")
            .1
            .split_once(&end)
            .expect("printed log end marker")
            .0
    }

    #[test]
    fn printed_logs_are_the_exact_selected_comparator_inputs() -> std::io::Result<()> {
        let left = "2026-08-15T01:02:03.000000Z INFO detcore: DETLOG value=101\n\
2026-08-15T01:02:03.000001Z INFO unrelated: omitted value=303";
        let right = "2026-08-15T04:05:06.000000Z INFO detcore: DETLOG value=202\n\
2026-08-15T04:05:06.000001Z INFO unrelated: omitted value=404";

        let exact = super::LogDiffOpts {
            print_logs: true,
            no_color: true,
            ..Default::default()
        };
        let mut exact_output = Vec::new();
        let exact_summary =
            super::log_diff_summary_from_strs(left, right, &exact, &mut exact_output)?;
        let exact_output = String::from_utf8(exact_output).unwrap();

        assert!(exact_summary.diff_found);
        assert!(exact_output.contains("Comparison policy: Deterministic\n"));
        assert_eq!(
            printed_log(&exact_output, 1).as_bytes(),
            b"INFO detcore: DETLOG value=101\n"
        );
        assert_eq!(
            printed_log(&exact_output, 2).as_bytes(),
            b"INFO detcore: DETLOG value=202\n"
        );

        let stripped = super::LogDiffOpts {
            strip_lines: true,
            print_logs: true,
            no_color: true,
            ..Default::default()
        };
        let mut stripped_output = Vec::new();
        let stripped_summary =
            super::log_diff_summary_from_strs(left, right, &stripped, &mut stripped_output)?;
        let stripped_output = String::from_utf8(stripped_output).unwrap();

        assert!(stripped_summary.matched_with_evidence());
        assert!(stripped_output.contains("Comparison policy: Stripped\n"));
        assert_eq!(
            printed_log(&stripped_output, 1).as_bytes(),
            b"INFO detcore: DETLOG value=<NUM>\n"
        );
        assert_eq!(
            printed_log(&stripped_output, 1),
            printed_log(&stripped_output, 2)
        );
        assert_ne!(
            printed_log(&exact_output, 1),
            printed_log(&stripped_output, 1)
        );
        Ok(())
    }

    #[test]
    fn printed_policy_name_tracks_the_selected_scope_and_normalization() -> std::io::Result<()> {
        let log = "2026-08-15T01:02:03.000000Z INFO detcore: DETLOG stable=1 address=<hostaddr 0xaaaa>\n\
2026-08-15T01:02:03.000001Z DEBUG unrelated: diagnostic=2";

        let cases = [
            (
                super::LogDiffOpts {
                    comparison: super::LogComparisonMode::Info,
                    print_logs: true,
                    no_color: true,
                    ..Default::default()
                },
                "Comparison policy: Info\n",
                "INFO detcore: DETLOG stable=1 address=<hostaddr 0xaaaa>\n",
            ),
            (
                super::LogDiffOpts {
                    comparison: super::LogComparisonMode::FullTrace,
                    print_logs: true,
                    no_color: true,
                    ..Default::default()
                },
                "Comparison policy: FullTrace\n",
                "INFO detcore: DETLOG stable=1 address=<hostaddr 0xaaaa>\nDEBUG unrelated: diagnostic=2\n",
            ),
            (
                super::LogDiffOpts {
                    comparison: super::LogComparisonMode::Deterministic,
                    canonicalize_addresses: true,
                    print_logs: true,
                    no_color: true,
                    ..Default::default()
                },
                "Comparison policy: Deterministic with Canonical host-address normalization\n",
                "INFO detcore: DETLOG stable=1 address=<addr1>\n",
            ),
            (
                super::LogDiffOpts {
                    comparison: super::LogComparisonMode::Deterministic,
                    strip_lines: true,
                    print_logs: true,
                    no_color: true,
                    ..Default::default()
                },
                "Comparison policy: Stripped\n",
                "INFO detcore: DETLOG stable=<NUM> address=<hostaddr <ADDR>>\n",
            ),
        ];

        for (options, expected_name, expected_log) in cases {
            let mut output = Vec::new();
            let summary = super::log_diff_summary_from_strs(log, log, &options, &mut output)?;
            let output = String::from_utf8(output).unwrap();
            assert!(summary.matched_with_evidence());
            assert!(output.contains(expected_name), "{output}");
            assert_eq!(printed_log(&output, 1), expected_log);
            assert_eq!(printed_log(&output, 2), expected_log);
        }
        Ok(())
    }

    #[test]
    fn printed_canonical_policy_uses_the_info_scope_and_address_ordinals() -> std::io::Result<()> {
        let left = "2026-08-15T01:02:03.000000Z INFO detcore: DETLOG stable=1\n\
2026-08-15T01:02:03.000001Z INFO unrelated: value=101 address=<hostaddr 0xaaaa>";
        let right = "2026-08-15T04:05:06.000000Z INFO detcore: DETLOG stable=1\n\
2026-08-15T04:05:06.000001Z INFO unrelated: value=202 address=<hostaddr 0xbbbb>";

        let deterministic = super::LogDiffOpts {
            print_logs: true,
            no_color: true,
            ..Default::default()
        };
        let mut deterministic_output = Vec::new();
        let deterministic_summary = super::log_diff_summary_from_strs(
            left,
            right,
            &deterministic,
            &mut deterministic_output,
        )?;
        let deterministic_output = String::from_utf8(deterministic_output).unwrap();
        assert!(deterministic_summary.matched_with_evidence());
        assert!(deterministic_output.contains("Comparison policy: Deterministic\n"));
        assert_eq!(
            printed_log(&deterministic_output, 1).as_bytes(),
            b"INFO detcore: DETLOG stable=1\n"
        );
        assert_eq!(
            printed_log(&deterministic_output, 1),
            printed_log(&deterministic_output, 2)
        );

        let options = super::LogDiffOpts {
            comparison: super::LogComparisonMode::Info,
            canonicalize_addresses: true,
            print_logs: true,
            no_color: true,
            ..Default::default()
        };
        let mut output = Vec::new();
        let summary = super::log_diff_summary_from_strs(left, right, &options, &mut output)?;
        let output = String::from_utf8(output).unwrap();

        assert!(summary.diff_found);
        assert!(output.contains("Comparison policy: Canonical\n"));
        assert_eq!(
            printed_log(&output, 1).as_bytes(),
            b"INFO detcore: DETLOG stable=1\nINFO unrelated: value=101 address=<addr1>\n"
        );
        assert_eq!(
            printed_log(&output, 2).as_bytes(),
            b"INFO detcore: DETLOG stable=1\nINFO unrelated: value=202 address=<addr1>\n"
        );
        Ok(())
    }

    #[test]
    fn test_full_trace_detects_unnormalized_timing_difference() -> std::io::Result<()> {
        let log_a = "INFO detcore: DETLOG [syscall] finish syscall #1: clock_gettime(CLOCK_MONOTONIC, 100) = Ok(0)";
        let log_b = "INFO detcore: DETLOG [syscall] finish syscall #1: clock_gettime(CLOCK_MONOTONIC, 101) = Ok(0)";
        let normalized = super::LogDiffOpts {
            strip_lines: true,
            no_color: true,
            ..Default::default()
        };

        assert!(!super::log_diff_from_strs(
            log_a,
            log_b,
            &normalized,
            &mut Vec::new()
        )?);

        let verbose = super::LogDiffOpts {
            comparison: super::LogComparisonMode::FullTrace,
            strip_lines: false,
            syscall_history: 1,
            no_color: true,
            ..Default::default()
        };
        let mut result = Vec::new();
        assert!(super::log_diff_from_strs(
            log_a,
            log_b,
            &verbose,
            &mut result
        )?);

        let output = String::from_utf8(result).unwrap();
        assert!(output.contains("Comparing full trace messages"));
        assert!(output.contains("clock_gettime(CLOCK_MONOTONIC, 100)"));
        assert!(output.contains("clock_gettime(CLOCK_MONOTONIC, 101)"));
        assert!(output.contains("run 1"));
        assert!(output.contains("run 2"));
        Ok(())
    }

    #[test]
    fn info_scope_compares_info_exactly_without_promoting_debug_diagnostics() -> std::io::Result<()>
    {
        let stable_info = "2026-08-06T01:00:00.000000Z INFO detcore: DETLOG [syscall] finish syscall #1: write(1, 0x2, 1) = Ok(1)";
        let left = format!(
            "{stable_info}\n2026-08-06T01:00:00.000001Z DEBUG detcore: diagnostic host timing=100"
        );
        let right = format!(
            "{stable_info}\n2026-08-06T01:00:00.000002Z DEBUG detcore: diagnostic host timing=200"
        );
        let info = super::LogDiffOpts {
            comparison: super::LogComparisonMode::Info,
            no_color: true,
            ..Default::default()
        };

        // Positive bracket: DEBUG remains present in both captures, but the
        // BitwiseInfoV1 envelope selects exactly the one INFO event per side.
        let matched = super::log_diff_summary_from_strs(&left, &right, &info, &mut Vec::new())?;
        assert!(matched.matched_with_evidence());
        assert_eq!(matched.compared_left, 1);
        assert_eq!(matched.compared_right, 1);

        // Negative bracket: a real INFO payload difference must still fail.
        let divergent_info = right.replace("write(1, 0x2, 1)", "write(1, 0x6, 1)");
        let diverged =
            super::log_diff_summary_from_strs(&left, divergent_info, &info, &mut Vec::new())?;
        assert!(diverged.diff_found);

        // DEBUG comparison remains an explicit diagnostic mode rather than an
        // implicit part of INFO parity.
        let full_trace = super::LogDiffOpts {
            comparison: super::LogComparisonMode::FullTrace,
            no_color: true,
            ..Default::default()
        };
        let debug_diverged =
            super::log_diff_summary_from_strs(left, right, &full_trace, &mut Vec::new())?;
        assert!(debug_diverged.diff_found);
        Ok(())
    }

    #[test]
    fn test_log_diff_compares_detlog() -> std::io::Result<()> {
        let log_file_a = r#"2022-09-06T14:15:47.891501Z  INFO detcore: DETLOG [memory][detcore, dtid 3] 0x602000-0x623000 rw-p 0 0:0 0 [heap] -> 74b43faf7b78ace9443772ef63a30f66feaf9bd320256c82b8bd880634d19d46
2022-09-06T14:15:48.903997Z  INFO detcore: DETLOG [memory][detcore, dtid 3] 0x7ffffffdd000-0x7ffffffff000 rw-p 0 0:0 0 [stack] -> 7984d1aaf386fce67eaa926624ecc1d5a4105828e4f286ee59cc69c0491cd5fe
2022-09-06T14:15:48.904049Z  INFO detcore: DETLOG [syscall][detcore, dtid 3] inbound syscall: write(1, 0x6022a0, 70) = ?
2022-09-06T14:15:48.904049Z  INFO detcore: COMMIT 2
2022-09-06T14:15:48.904782Z  INFO detcore::scheduler: [sched-step5] >>>>>>>"#;

        let log_file_b = r#"2022-09-06T14:15:47.891501Z  INFO detcore: DETLOG [memory][detcore, dtid 3] 0x602000-0x623000 rw-p 0 0:0 0 [heap] -> 74b43faf7b78ace9443772ef63a30f66feaf9bd320256c82b8bd880634d19d46
2022-09-06T14:15:47.903997Z  INFO detcore: DETLOG [memory][detcore, dtid 3] 0x7ffffffdd000-0x7ffffffff000 rw-p 0 0:0 0 [stack] -> 1984d1aaf386fce67eaa926624ecc1d5a4105828e4f286ee59cc69c0491cd5fe
2022-09-06T14:15:47.904049Z  INFO detcore: DETLOG [syscall][detcore, dtid 3] inbound syscall: write(1, 0x6022a0, 70) = ?
2022-09-06T14:15:47.904049Z  INFO detcore: COMMIT 1
2022-09-06T14:15:47.904782Z  INFO detcore::scheduler: [sched-step5] >>>>>>>"#;
        let mut result = Vec::<u8>::new();

        let log_options = super::LogDiffOpts {
            no_color: true,
            git_diff: false,
            ..Default::default()
        };
        super::log_diff_from_strs(log_file_a, log_file_b, &log_options, &mut result)?;

        let output = String::from_utf8(result).unwrap();
        assert!(output.contains("Mismatch at log messages 2 (run 1) and 2 (run 2)"));
        assert!(output.contains("Mismatch at log messages 4 (run 1) and 4 (run 2)"));
        assert!(output.contains("INFO detcore: COMMIT 2"));
        assert!(output.contains("INFO detcore: COMMIT 1"));

        Ok(())
    }

    #[test]
    fn test_filter_deterministic() {
        let opts = super::LogDiffOpts {
            include_detlogs: vec![
                DetLogFilter::Syscall,
                DetLogFilter::SyscallResult,
                DetLogFilter::Other,
            ],
            ..Default::default()
        };

        let v = opts.filter_deterministic(
            &[
                historical(
                    1,
                    "INFO detcore: registers [dtid 3]. user_regs_struct { r15...",
                ),
                historical(
                    2,
                    "INFO DETLOG detcore: registers [dtid 3]. user_regs_struct { r15...",
                ),
                historical(
                    3,
                    "INFO COMMIT turn 5, dettid 2 using resources {Path(\"/proc/2/fd/1\"): W} at time 946684799205300000",
                ),
            ],
        );

        assert_eq!(
            indexed_text(&v),
            vec![
                (
                    2,
                    "INFO DETLOG detcore: registers [dtid 3]. user_regs_struct { r15..."
                ),
                (
                    3,
                    "INFO COMMIT turn 5, dettid 2 using resources {Path(\"/proc/2/fd/1\"): W} at time 946684799205300000",
                ),
            ]
        );
    }

    #[test]
    fn test_filter_deterministic_with_filter() {
        let opts = super::LogDiffOpts {
            include_detlogs: vec![DetLogFilter::Syscall],
            skip_commit: true,
            ..Default::default()
        };

        let v = opts.filter_deterministic(
            &[
                historical(
                    1,
                    "INFO detcore: registers [dtid 3]. user_regs_struct { r15...",
                ),
                historical(2, "INFO DETLOG detcore:[syscall] syscall 1"),
                historical(
                    3,
                    "INFO COMMIT turn 5, dettid 2 using resources {Path(\"/proc/2/fd/1\"): W} at time 946684799205300000",
                ),
            ],
        );
        assert_eq!(
            indexed_text(&v),
            vec![(2, "INFO DETLOG detcore:[syscall] syscall 1")]
        );
    }

    /// Regression: the deterministic comparison must ignore the scheduler bookkeeping emitted
    /// by nonblocking-IO poll retries, whose count is host-timing nondeterministic (e.g. how
    /// many times a thread re-polls a pipe before a child process makes it ready). Only the
    /// `{InternalIOPolling: ...}` COMMIT turn and the `advancing committed_time` clock line
    /// should be dropped; ordinary COMMIT turns and DETLOG entries must be retained.
    #[test]
    fn test_filter_deterministic_drops_io_polling_bookkeeping() {
        let opts = super::LogDiffOpts::default();
        let v = opts.filter_deterministic(&[
            historical(
                0,
                "INFO detcore::scheduler: [sched-step5] >>> COMMIT turn 17, dettid 5 using resources {InternalIOPolling: W}, on previously committed 1s",
            ),
            historical(
                1,
                "DEBUG detcore::scheduler: DETLOG [sched-step1] advancing committed_time from 1 to 2",
            ),
            historical(
                2,
                "INFO detcore: DETLOG [syscall][detcore, dtid 5] finish syscall #9: read(3, 0x1000, 1) = Ok(1)",
            ),
            historical(
                3,
                "INFO detcore::scheduler: [sched-step5] >>> COMMIT turn 18, dettid 5 using resources {Path(\"/proc/5/fd/3\"): R}, on previously committed 2s",
            ),
        ]);
        // The InternalIOPolling COMMIT (0) and the committed_time line (1) are dropped; the
        // guest-observable syscall (2) and the ordinary COMMIT turn (3) survive.
        assert_eq!(
            indexed_text(&v),
            vec![
                (
                    2,
                    "INFO detcore: DETLOG [syscall][detcore, dtid 5] finish syscall #9: read(3, 0x1000, 1) = Ok(1)"
                ),
                (
                    3,
                    "INFO detcore::scheduler: [sched-step5] >>> COMMIT turn 18, dettid 5 using resources {Path(\"/proc/5/fd/3\"): R}, on previously committed 2s"
                ),
            ]
        );
    }

    #[test]
    fn test_filter_deterministic_drops_sabre_internal_pipe_resource_turn() {
        let opts = super::LogDiffOpts::default();
        let v = opts.filter_deterministic(&[
            historical(
                0,
                "INFO detcore::scheduler: [sched-step5] >>> COMMIT turn 17, dettid 5 using resources {Device(ContainerStdout): W}, on previously committed 1s [sabre-internal-pipe-io]",
            ),
            historical(
                1,
                "INFO detcore::scheduler: [sched-step5] >>> COMMIT turn 18, dettid 5 using resources {Device(ContainerStdout): W}, on previously committed 2s",
            ),
        ]);

        assert_eq!(
            indexed_text(&v),
            vec![(
                1,
                "INFO detcore::scheduler: [sched-step5] >>> COMMIT turn 18, dettid 5 using resources {Device(ContainerStdout): W}, on previously committed 2s"
            )]
        );
    }

    #[test]
    fn test_filter_deterministic_drops_sabre_loopback_poll_yield() {
        let opts = super::LogDiffOpts::default();
        let v = opts.filter_deterministic(&[
            historical(
                0,
                "INFO detcore::scheduler: [sched-step5] >>> COMMIT turn 17, dettid 5 using resources {SchedYield: W}, on previously committed 1s [sabre-loopback-poll-zero-timeout]",
            ),
            historical(
                1,
                "INFO detcore::scheduler: [sched-step5] >>> COMMIT turn 18, dettid 5 using resources {SchedYield: W}, on previously committed 2s",
            ),
        ]);

        assert_eq!(
            indexed_text(&v),
            vec![(
                1,
                "INFO detcore::scheduler: [sched-step5] >>> COMMIT turn 18, dettid 5 using resources {SchedYield: W}, on previously committed 2s"
            )]
        );
    }

    /// Regression: two runs that differ only in how many nonblocking-IO poll retries occurred
    /// must compare as deterministic, while a genuine guest-observable divergence must still be
    /// reported. `run_b` below performs one extra poll retry (an extra InternalIOPolling COMMIT
    /// plus its committed_time advance) but the guest syscalls are identical.
    #[test]
    fn test_log_diff_ignores_extra_io_poll_retries() -> std::io::Result<()> {
        let common_head = "2022-09-06T14:15:47.000000Z  INFO detcore: DETLOG [syscall][detcore, dtid 5] inbound syscall: poll(0x1000, 1, -1) = ?";
        let common_tail = "2022-09-06T14:15:47.100000Z  INFO detcore: DETLOG [syscall][detcore, dtid 5] finish syscall #9: poll(0x1000, 1, -1) = Ok(1)";
        let poll_retry = "2022-09-06T14:15:47.050000Z  INFO detcore::scheduler: [sched-step5] >>> COMMIT turn 17, dettid 5 using resources {InternalIOPolling: W}, on previously committed 1s\n2022-09-06T14:15:47.050000Z DEBUG detcore::scheduler: DETLOG [sched-step1] advancing committed_time from 1 to 2";

        let run_a = format!("{common_head}\n{poll_retry}\n{common_tail}");
        // run_b polls one extra time before the fd is ready:
        let run_b = format!("{common_head}\n{poll_retry}\n{poll_retry}\n{common_tail}");

        let opts = super::LogDiffOpts {
            no_color: true,
            strip_lines: true,
            ..Default::default()
        };
        // Differ only in retry count -> deterministic (no diff reported):
        assert!(!super::log_diff_from_strs(
            &run_a,
            &run_b,
            &opts,
            &mut Vec::new()
        )?);

        // But a real divergence in the guest-observable syscall result is still caught. (Use a
        // non-numeric change: numeric-only differences are erased by `strip_lines` normalization.)
        let run_c = run_a.replace("= Ok(1)", "= Err(Errno(EBADF))");
        assert!(super::log_diff_from_strs(
            &run_a,
            &run_c,
            &opts,
            &mut Vec::new()
        )?);
        Ok(())
    }

    /// Canonical parity positive control: two runs that differ ONLY in their raw
    /// host addresses -- same structure, same introduction order, same aliasing
    /// (a pure ASLR shift) -- compare EQUAL after canonicalization. The same
    /// inputs compared RAW (neither stripped nor canonicalized) diverge, proving
    /// canonicalization is doing real work and is not just the identity.
    #[test]
    fn canonical_address_only_difference_compares_equal() -> std::io::Result<()> {
        let run_a =
            "2022-09-06T14:15:47.000000Z  INFO detcore: [t] p=<hostaddr 0x1111> q=<hostaddr 0x2222>
2022-09-06T14:15:48.000000Z  INFO detcore: [t] use <hostaddr 0x1111> then <hostaddr 0x2222>";
        let run_b =
            "2022-09-06T14:15:47.000000Z  INFO detcore: [t] p=<hostaddr 0xaaaa> q=<hostaddr 0xbbbb>
2022-09-06T14:15:48.000000Z  INFO detcore: [t] use <hostaddr 0xaaaa> then <hostaddr 0xbbbb>";

        let canonical = super::LogDiffOpts {
            comparison: super::LogComparisonMode::FullTrace,
            canonicalize_addresses: true,
            no_color: true,
            ..Default::default()
        };
        assert!(
            !super::log_diff_from_strs(run_a, run_b, &canonical, &mut Vec::new())?,
            "address-only (ASLR-shift) difference must compare EQUAL under canonicalization"
        );

        // Positive control: raw comparison (no strip, no canonicalize) diverges on
        // the very same inputs, so the equality above is not vacuous.
        let raw = super::LogDiffOpts {
            comparison: super::LogComparisonMode::FullTrace,
            canonicalize_addresses: false,
            no_color: true,
            ..Default::default()
        };
        assert!(
            super::log_diff_from_strs(run_a, run_b, &raw, &mut Vec::new())?,
            "raw comparison must still see the differing addresses"
        );
        Ok(())
    }

    /// Canonical parity negative control (allocation order): two runs whose only
    /// difference is the ORDER in which two addresses are introduced must compare
    /// UNEQUAL. This is exactly the divergence wholesale stripping hides -- it is
    /// the positive control that canonicalization preserves distinguishability.
    #[test]
    fn canonical_allocation_order_difference_compares_unequal() -> std::io::Result<()> {
        // Both runs: alloc, alloc, then a line pairing the two addresses. In run_b
        // the two addresses are introduced in the opposite order, so the ordinals
        // on the shared "pair" line are swapped.
        let run_a = "2022-09-06T14:15:47.000000Z  INFO detcore: [t] alloc <hostaddr 0x1111>
2022-09-06T14:15:48.000000Z  INFO detcore: [t] alloc <hostaddr 0x2222>
2022-09-06T14:15:49.000000Z  INFO detcore: [t] pair <hostaddr 0x1111> <hostaddr 0x2222>";
        let run_b = "2022-09-06T14:15:47.000000Z  INFO detcore: [t] alloc <hostaddr 0xbbbb>
2022-09-06T14:15:48.000000Z  INFO detcore: [t] alloc <hostaddr 0xaaaa>
2022-09-06T14:15:49.000000Z  INFO detcore: [t] pair <hostaddr 0xaaaa> <hostaddr 0xbbbb>";

        let canonical = super::LogDiffOpts {
            comparison: super::LogComparisonMode::FullTrace,
            canonicalize_addresses: true,
            no_color: true,
            ..Default::default()
        };
        assert!(
            super::log_diff_from_strs(run_a, run_b, &canonical, &mut Vec::new())?,
            "an allocation-order difference must compare UNEQUAL under canonicalization"
        );

        // And wholesale stripping DOES hide it: both addresses collapse to a
        // single <ADDR> token.
        let stripped = super::LogDiffOpts {
            comparison: super::LogComparisonMode::FullTrace,
            strip_lines: true,
            no_color: true,
            ..Default::default()
        };
        assert!(
            !super::log_diff_from_strs(run_a, run_b, &stripped, &mut Vec::new())?,
            "wholesale stripping erases the allocation-order difference (the defect)"
        );
        Ok(())
    }

    /// Canonical parity negative control (aliasing): a run that prints ONE address
    /// twice (aliased) must not match a run that prints TWO distinct addresses in
    /// the same positions. `<addr1>,<addr1>` vs `<addr1>,<addr2>` diverges.
    #[test]
    fn canonical_aliasing_difference_compares_unequal() -> std::io::Result<()> {
        let run_a = "2022-09-06T14:15:47.000000Z  INFO detcore: [t] two <hostaddr 0x1111> <hostaddr 0x1111>";
        let run_b = "2022-09-06T14:15:47.000000Z  INFO detcore: [t] two <hostaddr 0xaaaa> <hostaddr 0xbbbb>";

        let canonical = super::LogDiffOpts {
            comparison: super::LogComparisonMode::FullTrace,
            canonicalize_addresses: true,
            no_color: true,
            ..Default::default()
        };
        assert!(
            super::log_diff_from_strs(run_a, run_b, &canonical, &mut Vec::new())?,
            "an aliasing difference (1,1 vs 1,2) must compare UNEQUAL"
        );
        Ok(())
    }

    /// Canonical parity exact-value control: a reproducible
    /// hex value printed as a syscall argument (`flock` `operation={:#x}`) is
    /// NOT a host address and must be compared EXACTLY. Two runs differing only
    /// in `operation=0x2` vs `0x6` must compare UNEQUAL under canonicalization --
    /// proving the canonicalizer touches only marked `<hostaddr ...>` pointers
    /// and does not become a "softer strip" that swallows syscall-argument
    /// divergence. A marked host address on the SAME line, differing only by an
    /// ASLR shift, must not by itself make the runs diverge.
    #[test]
    fn canonical_syscall_arg_hex_difference_compares_unequal() -> std::io::Result<()> {
        let run_a = "2022-09-06T14:15:47.000000Z  INFO detcore: flock(fd=3, operation=0x2) at <hostaddr 0x1111>";
        let run_b = "2022-09-06T14:15:47.000000Z  INFO detcore: flock(fd=3, operation=0x6) at <hostaddr 0xaaaa>";

        let canonical = super::LogDiffOpts {
            comparison: super::LogComparisonMode::FullTrace,
            canonicalize_addresses: true,
            no_color: true,
            ..Default::default()
        };
        assert!(
            super::log_diff_from_strs(run_a, run_b, &canonical, &mut Vec::new())?,
            "a bare syscall-argument hex difference (0x2 vs 0x6) must compare UNEQUAL: \
             it is reproducible and NOT a host address"
        );

        // Positive control: with ONLY the marked host address differing (same
        // syscall argument), the ASLR shift is canonicalized away and the runs
        // compare EQUAL -- so the divergence above is due to the syscall arg, not
        // the address.
        let addr_only_a = "2022-09-06T14:15:47.000000Z  INFO detcore: flock(fd=3, operation=0x2) at <hostaddr 0x1111>";
        let addr_only_b = "2022-09-06T14:15:47.000000Z  INFO detcore: flock(fd=3, operation=0x2) at <hostaddr 0xaaaa>";
        assert!(
            !super::log_diff_from_strs(addr_only_a, addr_only_b, &canonical, &mut Vec::new())?,
            "an address-only difference alongside an identical syscall arg must compare EQUAL"
        );
        Ok(())
    }

    /// Canonical parity exact-value control: a single virtual-time timestamp difference (a
    /// decimal value, NOT a `0x` address) must compare UNEQUAL -- canonicalization
    /// touches only host addresses and leaves every other byte for exact
    /// comparison. This is the sharp edge: virtual time is compared exactly even
    /// though the wall-clock PREFIX is stripped.
    #[test]
    fn canonical_virtual_time_difference_compares_unequal() -> std::io::Result<()> {
        let run_a = "2022-09-06T14:15:47.000000Z  INFO detcore: COMMIT turn 5 at time 100";
        let run_b = "2022-09-06T14:15:47.000000Z  INFO detcore: COMMIT turn 5 at time 200";

        let canonical = super::LogDiffOpts {
            comparison: super::LogComparisonMode::FullTrace,
            canonicalize_addresses: true,
            no_color: true,
            ..Default::default()
        };
        assert!(
            super::log_diff_from_strs(run_a, run_b, &canonical, &mut Vec::new())?,
            "a virtual-time (decimal) difference must compare UNEQUAL under canonicalization"
        );
        Ok(())
    }

    /// Canonical parity wall-clock control: runs that differ ONLY in the real wall-clock
    /// timestamp PREFIX compare EQUAL -- the prefix is stripped by
    /// `extract_log_messages` before any comparison, so nothing else needs to see
    /// it. This is the one genuinely-irreproducible datum the policy discards.
    #[test]
    fn canonical_wall_clock_prefix_difference_compares_equal() -> std::io::Result<()> {
        let run_a = "2022-09-06T14:15:47.000000Z  INFO detcore: [t] use 0x1111
2022-09-06T14:15:48.000000Z  INFO detcore: [t] use 0x1111";
        // Same message bodies, entirely different wall-clock prefixes (and format).
        let run_b = "Apr 09 06:08:03.100  INFO detcore: [t] use 0x1111
Jun 09 06:49:17.742  INFO detcore: [t] use 0x1111";

        let canonical = super::LogDiffOpts {
            comparison: super::LogComparisonMode::FullTrace,
            canonicalize_addresses: true,
            no_color: true,
            ..Default::default()
        };
        assert!(
            !super::log_diff_from_strs(run_a, run_b, &canonical, &mut Vec::new())?,
            "a wall-clock-prefix-only difference must compare EQUAL"
        );
        Ok(())
    }

    #[test]
    fn one_log_canonical_info_preserves_values_and_only_rewrites_marked_addresses() {
        let log = "2026-08-13T01:02:03.000000Z INFO detcore: COMMIT turn 17 at time 123456 bare=0x2 marked=<hostaddr 0xaaaa>\n\
2026-08-13T01:02:03.000001Z DEBUG detcore: diagnostic=999\n\
2026-08-13T01:02:03.000002Z INFO detcore: DETLOG count=42 bare=0x6 marked=<hostaddr 0xaaaa> other=<hostaddr 0xbbbb>";

        assert_eq!(
            super::canonical_info_from_str(log).expect("fixture log is fully tagged"),
            vec![
                "INFO detcore: COMMIT turn 17 at time 123456 bare=0x2 marked=<addr1>",
                "INFO detcore: DETLOG count=42 bare=0x6 marked=<addr1> other=<addr2>",
            ]
        );
    }

    #[test]
    fn first_log_divergence_reports_preceding_commit_turn_and_virtual_time() -> std::io::Result<()>
    {
        let left = "2026-08-13T01:02:03.000000Z INFO detcore::scheduler: COMMIT turn 17, dettid 2, on previously committed 12.345_678_901s\n\
2026-08-13T01:02:03.000001Z INFO detcore: DETLOG count=42";
        let right = "2026-08-13T01:02:04.000000Z INFO detcore::scheduler: COMMIT turn 17, dettid 2, on previously committed 12.345_678_901s\n\
2026-08-13T01:02:04.000001Z INFO detcore: DETLOG count=43";
        let opts = super::LogDiffOpts {
            comparison: super::LogComparisonMode::Info,
            canonicalize_addresses: true,
            no_color: true,
            ..Default::default()
        };

        let diverged = super::log_diff_summary_from_strs(left, right, &opts, &mut Vec::new())?;
        assert!(diverged.diff_found);
        assert_eq!(diverged.first_divergent_scheduler_turn, Some(17));
        assert_eq!(
            diverged.first_divergent_virtual_nanoseconds,
            Some(12_345_678_901)
        );
        assert_eq!(
            diverged.first_divergent_left_message.as_deref(),
            Some("INFO detcore: DETLOG count=42")
        );
        assert_eq!(
            diverged.first_divergent_right_message.as_deref(),
            Some("INFO detcore: DETLOG count=43")
        );

        let matched = super::log_diff_summary_from_strs(left, left, &opts, &mut Vec::new())?;
        assert!(matched.matched_with_evidence());
        assert_eq!(matched.first_divergent_scheduler_turn, None);
        assert_eq!(matched.first_divergent_virtual_nanoseconds, None);
        assert_eq!(matched.first_divergent_left_message, None);
        assert_eq!(matched.first_divergent_right_message, None);
        Ok(())
    }

    #[test]
    fn first_divergent_messages_keep_event_content_not_separate_positions() -> std::io::Result<()> {
        let left = "2026-08-13T01:02:03.000000Z INFO detcore::scheduler: COMMIT turn 110, dettid 2 using resources {Device(ContainerStdout): W}, on previously committed 1_767_225_600.031_999_250s";
        let right = "2026-08-13T01:02:04.000000Z INFO detcore::scheduler: COMMIT turn 111, dettid 2 using resources {Device(ContainerStdout): W}, on previously committed 1_767_225_600.031_994_250s";
        let opts = super::LogDiffOpts {
            comparison: super::LogComparisonMode::Info,
            canonicalize_addresses: true,
            no_color: true,
            ..Default::default()
        };

        let same_event = super::log_diff_summary_from_strs(left, right, &opts, &mut Vec::new())?;
        assert!(same_event.diff_found);
        assert_eq!(
            same_event.first_divergent_left_message, same_event.first_divergent_right_message,
            "turn and committed-time values are recorded separately, so they must not split one event into two"
        );
        assert_eq!(
            same_event.first_divergent_left_message.as_deref(),
            Some(
                "INFO detcore::scheduler: COMMIT turn <NUM>, dettid 2 using resources {Device(ContainerStdout): W}, on previously committed <NANOSECONDS>"
            )
        );
        assert_eq!(
            super::first_divergent_message(&historical(
                0,
                "INFO detcore::scheduler: COMMIT turn 110 at time 123\npayload at time 456"
            )),
            "INFO detcore::scheduler: COMMIT turn <NUM> at time <NANOSECONDS>\npayload at time 456",
            "only the structured first line carries the separately recorded position"
        );

        let after_longer_shared_prefix = super::log_diff_summary_from_strs(
            format!("2026-08-13T01:02:02.000000Z INFO detcore: shared event\n{left}"),
            format!("2026-08-13T01:02:02.000000Z INFO detcore: shared event\n{right}"),
            &opts,
            &mut Vec::new(),
        )?;
        assert_ne!(
            same_event.first_divergent_record, after_longer_shared_prefix.first_divergent_record,
            "a longer shared trace must move the record observation in this fixture"
        );
        assert_eq!(
            (
                same_event.first_divergent_left_message.as_deref(),
                same_event.first_divergent_right_message.as_deref(),
            ),
            (
                after_longer_shared_prefix
                    .first_divergent_left_message
                    .as_deref(),
                after_longer_shared_prefix
                    .first_divergent_right_message
                    .as_deref(),
            ),
            "the same event after a longer trace must keep the same compared messages"
        );

        let different_event = super::log_diff_summary_from_strs(
            left,
            "2026-08-13T01:02:04.000000Z INFO detcore::scheduler: COMMIT turn 111, dettid 2 using resources {Device(ContainerStderr): W}, on previously committed 1_767_225_600.031_994_250s",
            &opts,
            &mut Vec::new(),
        )?;
        assert_ne!(
            different_event.first_divergent_left_message,
            different_event.first_divergent_right_message,
            "different event content must remain distinguishable"
        );
        Ok(())
    }

    #[test]
    fn log_divergence_without_commit_metadata_reports_no_position() -> std::io::Result<()> {
        let opts = super::LogDiffOpts {
            comparison: super::LogComparisonMode::Info,
            canonicalize_addresses: true,
            no_color: true,
            ..Default::default()
        };
        let summary = super::log_diff_summary_from_strs(
            "INFO detcore: DETLOG count=42",
            "INFO detcore: DETLOG count=43",
            &opts,
            &mut Vec::new(),
        )?;
        assert!(summary.diff_found);
        assert_eq!(summary.first_divergent_scheduler_turn, None);
        assert_eq!(summary.first_divergent_virtual_nanoseconds, None);
        Ok(())
    }

    /// NEGATIVE: two logs with nothing to compare must NOT be reported as a
    /// match with evidence. `diff_found` is legitimately false (there is no
    /// difference between two empty selections), but the counts are zero, so
    /// `matched_with_evidence()` must refuse. This is the "green with zero
    /// executed work" no-result, and it is the reason the counts exist.
    #[test]
    fn empty_selection_is_a_no_result_not_a_match() -> std::io::Result<()> {
        let canonical = super::LogDiffOpts {
            comparison: super::LogComparisonMode::FullTrace,
            canonicalize_addresses: true,
            no_color: true,
            ..Default::default()
        };
        let summary = super::log_diff_summary_from_strs("", "", &canonical, &mut Vec::new())?;
        assert!(!summary.diff_found, "two empty logs do not differ");
        assert_eq!(summary.compared_left, 0);
        assert_eq!(summary.compared_right, 0);
        assert!(
            !summary.matched_with_evidence(),
            "zero compared messages must never count as a verified match"
        );
        Ok(())
    }

    /// POSITIVE: the same predicate must still FIRE on a real comparison, so the
    /// guard is not merely refusing everything.
    #[test]
    fn nonempty_identical_selection_is_a_match_with_evidence() -> std::io::Result<()> {
        let run = "Apr 09 06:08:03.100  INFO detcore: [t] finish syscall: close(2) = Ok(0)
Apr 09 06:08:03.200  INFO detcore: [t] finish syscall: exit_group(0)";
        let canonical = super::LogDiffOpts {
            comparison: super::LogComparisonMode::FullTrace,
            canonicalize_addresses: true,
            no_color: true,
            ..Default::default()
        };
        let summary = super::log_diff_summary_from_strs(run, run, &canonical, &mut Vec::new())?;
        assert!(!summary.diff_found);
        assert_eq!(summary.compared_left, 2);
        assert_eq!(summary.compared_right, 2);
        assert!(
            summary.matched_with_evidence(),
            "a real, nonempty, identical comparison must count as a match"
        );
        Ok(())
    }

    #[test]
    fn test_filter_infos() {
        let v = super::filter_infos(&[
            historical(
                0,
                "DEBUG detcore::scheduler: [sched-step3] advancing committed_time from 946684799165300000 to 946684799205300000",
            ),
            historical(
                1,
                "INFO detcore: registers [dtid 3]. user_regs_struct { r15: 140737354129904, ...",
            ),
        ]);
        assert_eq!(
            indexed_text(&v),
            vec![(
                1,
                "INFO detcore: registers [dtid 3]. user_regs_struct { r15: 140737354129904, ..."
            )]
        );
    }

    #[test]
    fn test_extract_log_messages() {
        let s = "
Jan 09 06:08:03.100  INFO detcore: [detcore, dtid 2]  finish syscall: close(2) = Ok(0)
Feb 09 06:49:17.742 DEBUG detcore::scheduler: [sched-step3] advancing committed_time from 946684799165300000 to 946684799205300000
Apr 09 06:49:17.742  INFO detcore::scheduler: [scheduler] >>>>>>>

 COMMIT turn 5, dettid 2 using resources {Path(\"/proc/2/fd/1\"): W} at time 946684799205300000
Jan 09 06:49:03.100  INFO detcore: registers [dtid 3]. user_regs_struct { r15: 140737354129904, r14: 0, r13: 1, r12: 946684799000118840, rbp: 140737488344736, rbx: 0, r11: 518, r10: 140737488342434, r9: 0, r8: 1, rax: 0, rcx: 0, rdx: 2, rsi: 0, rdi: 140737354052880, orig_rax: 18446744073709551615, rip: 140737351875567, cs: 51, eflags: 66118, rsp: 140737488344064, ss: 43, fs_base: 0, gs_base: 0, ds: 0, es: 0, fs: 0, gs: 0 }
Jun 09 06:49:17.742 TRACE detcore::scheduler: [scheduler] Guest unblocked (<ivar Go>); clear ivars for the next turn on dettid 2
";

        let v = super::extract_log_messages(s).expect("fixture log is fully tagged");
        eprintln!("Split into {} log messages", v.len());
        for x in &v {
            eprintln!("{:?}", x);
        }
        assert_eq!(v.len(), 5);
    }

    #[test]
    fn test_canonicalize_addresses_in_line() {
        use std::collections::HashMap;
        let mut map = HashMap::new();
        let mut next = 1usize;
        // First appearance numbers marked addresses by order; a repeated address
        // reuses its ordinal (identity + aliasing). A BARE `0x...` literal and
        // decimal values are left untouched and compared exactly.
        assert_eq!(
            super::canonicalize_addresses_in_line(
                "a=<hostaddr 0x1111> b=<hostaddr 0x2222> c=<hostaddr 0x1111> raw=0x4444 n=42",
                &mut map,
                &mut next
            ),
            "a=<addr1> b=<addr2> c=<addr1> raw=0x4444 n=42"
        );
        // State threads across lines within one run: a known address keeps its
        // ordinal, a new one continues the count.
        assert_eq!(
            super::canonicalize_addresses_in_line(
                "use <hostaddr 0x2222> then <hostaddr 0x3333>",
                &mut map,
                &mut next
            ),
            "use <addr2> then <addr3>"
        );
        // A bare hex literal is never canonicalized, even one identical to a
        // marked address seen earlier: reproducible hex is compared exactly.
        assert_eq!(
            super::canonicalize_addresses_in_line("bare 0x1111", &mut map, &mut next),
            "bare 0x1111"
        );
    }

    #[test]
    fn test_strip_log() {
        assert_eq!(super::strip_log_entry("800.709_180s"), "<NANOSECONDS>");
        assert_eq!(super::strip_log_entry("98.91618ms"), "<NUM>");
        assert_eq!(super::strip_log_entry("98.91619ms"), "<NUM>");
        assert_eq!(super::strip_log_entry("x86_64"), "x86_64");
        assert_eq!(
            super::strip_log_entry(
                "COMMIT turn 66, dettid 2 using resources {Path(\"/proc/2/fd/1\"): W} at time 946_684_800.709_180_000s"
            ),
            "COMMIT turn <NUM>, dettid <NUM> using resources {Path(\"/proc/<PID>/fd/<NUM>\"): W} at time <NANOSECONDS>"
        );
    }

    /// Erasing a `/tmp` path must consume the path and nothing else.
    ///
    /// The pattern was previously `/tmp/.*"`, whose greedy `.*` ran to the LAST
    /// quote on the line rather than the path's own closing quote. Every field
    /// after the path was therefore erased too, so two entries differing only
    /// downstream of a `/tmp` path compared EQUAL under the stripped
    /// comparator -- a divergence silently reported as a match.
    #[test]
    fn strip_tmp_path_does_not_swallow_rest_of_line() {
        let read = super::strip_log_entry(r#"open path="/tmp/scratch" flags="O_RDONLY""#);
        let write = super::strip_log_entry(r#"open path="/tmp/scratch" flags="O_WRONLY""#);

        assert_eq!(read, r#"open path="/tmp/<somewhere>" flags="O_RDONLY""#);
        assert_eq!(write, r#"open path="/tmp/<somewhere>" flags="O_WRONLY""#);
        assert_ne!(
            read, write,
            "entries differing after a /tmp path must not collapse to equal"
        );
    }

    /// The narrowed pattern must still do its job: two entries whose only
    /// difference is the host-chosen `/tmp` path still compare equal, which is
    /// the whole reason this erasure exists.
    #[test]
    fn strip_tmp_path_still_erases_a_differing_tmp_path() {
        assert_eq!(
            super::strip_log_entry(r#"open path="/tmp/hermit-aaaa/f" flags="O_RDONLY""#),
            super::strip_log_entry(r#"open path="/tmp/hermit-bbbb/f" flags="O_RDONLY""#),
        );
    }

    /// Two distinct `/tmp` paths on one line are each erased individually,
    /// rather than the first one swallowing the second along with everything
    /// between them.
    #[test]
    fn strip_tmp_path_erases_each_path_separately() {
        assert_eq!(
            super::strip_log_entry(r#"rename from="/tmp/a" to="/tmp/b" ok="1""#),
            r#"rename from="/tmp/<somewhere>" to="/tmp/<somewhere>" ok="<NUM>""#
        );
    }

    const KICK_LINE_PREFIX: &str = "Logs contain";
    const KICK_LINE_SUFFIX: &str = "scheduler empty-run-queue kick messages";

    fn kick_opts() -> super::LogDiffOpts {
        super::LogDiffOpts {
            comparison: super::LogComparisonMode::Info,
            canonicalize_addresses: true,
            no_color: true,
            ..Default::default()
        }
    }

    /// A run that passes through the empty-run-queue kick still exits through
    /// the ordinary message afterwards, so both shutdown paths end identically
    /// and only the kick distinguishes them.
    fn log_with_kick() -> &'static str {
        "2026-08-13T01:02:03.000000Z INFO detcore::scheduler: COMMIT turn 17, dettid 2, on previously committed 1s\n\
2026-08-13T01:02:03.000001Z INFO detcore::scheduler: scheduler (step2_process_blocked): zero threads left anywhere, fizzling.\n\
2026-08-13T01:02:03.000002Z INFO detcore::scheduler: [scheduler] run queue empty, exiting sched_loop."
    }

    fn log_without_kick() -> &'static str {
        "2026-08-13T01:02:03.000000Z INFO detcore::scheduler: COMMIT turn 17, dettid 2, on previously committed 1s\n\
2026-08-13T01:02:03.000002Z INFO detcore::scheduler: [scheduler] run queue empty, exiting sched_loop."
    }

    fn run_diff(left: &str, right: &str) -> std::io::Result<(super::LogDiffSummary, String)> {
        let mut out = Vec::new();
        let summary = super::log_diff_summary_from_strs(left, right, &kick_opts(), &mut out)?;
        Ok((
            summary,
            String::from_utf8(out).expect("diff output is utf-8"),
        ))
    }

    /// A matching pair keeps only its summary, so without this count there is no
    /// record of which shutdown path either run took. Both directions of the
    /// pass are covered: both runs kicked, and neither did.
    #[test]
    fn a_matching_pair_records_the_empty_queue_kick_count() -> std::io::Result<()> {
        let (kicked, kicked_out) = run_diff(log_with_kick(), log_with_kick())?;
        assert!(kicked.matched_with_evidence(), "both-kicked pair must pass");
        assert!(
            kicked_out.contains(&format!("{KICK_LINE_PREFIX} 1 | 1 {KICK_LINE_SUFFIX}")),
            "a passing pair that kicked must retain the count, got:\n{kicked_out}"
        );

        let (quiet, quiet_out) = run_diff(log_without_kick(), log_without_kick())?;
        assert!(
            quiet.matched_with_evidence(),
            "neither-kicked pair must pass"
        );
        assert!(
            quiet_out.contains(&format!("{KICK_LINE_PREFIX} 0 | 0 {KICK_LINE_SUFFIX}")),
            "a passing pair that did not kick must say so explicitly, got:\n{quiet_out}"
        );
        Ok(())
    }

    /// The other half of the bracket, and the half that matters: recording the
    /// count must not move any verdict. A pair differing only by the kick was a
    /// divergence before this line existed and must remain one — the count must
    /// never stand in for agreement.
    #[test]
    fn recording_the_kick_count_does_not_move_any_verdict() -> std::io::Result<()> {
        let (diverged, diverged_out) = run_diff(log_with_kick(), log_without_kick())?;
        assert!(
            diverged.diff_found,
            "a pair differing only by the kick must still diverge"
        );
        assert!(
            !diverged_out.contains(KICK_LINE_SUFFIX),
            "the count is scoped to passing pairs; a diverging pair already \
             reproduces the messages in its diff, got:\n{diverged_out}"
        );

        // Identical inputs keep every summary field they had, so the extra
        // writeln! cannot be smuggling a verdict change in behind the text.
        let (matched, _) = run_diff(log_with_kick(), log_with_kick())?;
        assert!(!matched.diff_found);
        assert_eq!(matched.first_divergent_scheduler_turn, None);
        assert_eq!(matched.first_divergent_virtual_nanoseconds, None);
        assert_eq!(matched.first_divergent_record, None);
        assert_eq!(matched.compared_left, matched.compared_right);
        Ok(())
    }

    const MAPS_LINE_SUFFIX: &str = "scheduler COMMIT records reading /proc/self/maps";

    /// A guest whose runtime scans its own memory map during bootstrap. The
    /// committed virtual time is the part that drifts between runs, so it is
    /// parameterised.
    fn log_with_maps_read(committed: &str) -> String {
        format!(
            "2026-08-13T01:02:03.000000Z INFO detcore::scheduler: [sched-step5] >>> COMMIT turn 10, dettid 3 using resources {{Path(\"/proc/self/maps\"): R}}, on previously committed {committed}\n\
2026-08-13T01:02:03.000001Z INFO detcore::scheduler: [scheduler] run queue empty, exiting sched_loop."
        )
    }

    #[test]
    fn custom_side_labels_name_the_maps_read_summary() -> std::io::Result<()> {
        let scanned = log_with_maps_read("12.345_678_901s");
        let options = super::LogDiffOpts {
            comparison: super::LogComparisonMode::Info,
            canonicalize_addresses: true,
            side_labels: super::ComparisonSideLabels::new("the recording", "the replay"),
            no_color: true,
            ..Default::default()
        };
        let mut output = Vec::new();
        let summary = super::log_diff_summary_from_strs(&scanned, &scanned, &options, &mut output)?;
        assert!(summary.matched_with_evidence());
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains(
            "(the recording first at turn 10, committed virtual time 12345678901ns, \
             the replay first at turn 10, committed virtual time 12345678901ns)"
        ));
        assert!(!output.contains("run 1") && !output.contains("run 2"));
        Ok(())
    }

    /// A matching pair discards its logs, so without this record there is no way
    /// to tell a guest that never reads its own memory map — and therefore
    /// cannot drift this way — from one that reads it and happened to agree.
    #[test]
    fn a_matching_pair_records_the_maps_read_commit() -> std::io::Result<()> {
        let scanned = log_with_maps_read("12.345_678_901s");
        let (summary, out) = run_diff(&scanned, &scanned)?;
        assert!(summary.matched_with_evidence(), "the pair must pass");
        assert!(
            out.contains(&format!("Logs contain 1 | 1 {MAPS_LINE_SUFFIX}")),
            "a passing pair that read the map must retain the record, got:\n{out}"
        );
        // The committed virtual time is the evidence, not just the count: it is
        // what a later run is compared against to see drift.
        assert!(
            out.contains(
                "(run 1 first at turn 10, committed virtual time 12345678901ns, \
                 run 2 first at turn 10, committed virtual time 12345678901ns)"
            ),
            "the retained record must carry the turn and the committed virtual \
             time, got:\n{out}"
        );

        let (quiet, quiet_out) = run_diff(log_without_kick(), log_without_kick())?;
        assert!(quiet.matched_with_evidence());
        assert!(
            quiet_out.contains(&format!("Logs contain 0 | 0 {MAPS_LINE_SUFFIX}")),
            "a passing pair that never read the map must say so explicitly, \
             got:\n{quiet_out}"
        );
        Ok(())
    }

    /// The load-bearing half. A pair differing only in the committed virtual
    /// time of the retained record was a divergence before this line existed and
    /// must remain one; recording the value must never stand in for agreeing on
    /// it.
    #[test]
    fn recording_the_maps_read_commit_does_not_move_any_verdict() -> std::io::Result<()> {
        let (diverged, diverged_out) = run_diff(
            &log_with_maps_read("12.345_678_901s"),
            &log_with_maps_read("12.345_678_902s"),
        )?;
        assert!(
            diverged.diff_found,
            "a one-nanosecond difference in the committed time must still diverge"
        );
        assert_eq!(diverged.first_divergent_scheduler_turn, Some(10));
        assert!(
            !diverged_out.contains(MAPS_LINE_SUFFIX),
            "the record is scoped to passing pairs; a diverging pair already \
             prints both times in its diff, got:\n{diverged_out}"
        );
        Ok(())
    }

    /// Every other test here forces `LogComparisonMode::Info`, but the default
    /// is `Deterministic`, which selects a different set of messages. Both
    /// retained lines must appear on that path too, and the kick counts must be
    /// attributed per side — under this mode a kick asymmetry is *not* compared,
    /// so `1 | 0` is a passing pair and quoting a single side would report it as
    /// agreement.
    #[test]
    fn both_records_are_retained_under_the_default_comparison_mode() -> std::io::Result<()> {
        let default_opts = super::LogDiffOpts {
            no_color: true,
            ..Default::default()
        };
        assert_eq!(
            default_opts.comparison,
            super::LogComparisonMode::Deterministic,
            "this test exists to cover the default mode; if the default changes \
             it must be re-pointed, not deleted"
        );

        let mut out = Vec::new();
        let summary = super::log_diff_summary_from_strs(
            log_with_kick(),
            log_without_kick(),
            &default_opts,
            &mut out,
        )?;
        let out = String::from_utf8(out).expect("diff output is utf-8");
        assert!(
            summary.matched_with_evidence(),
            "a kick asymmetry is not compared under the default mode, so this \
             pair must pass; got:\n{out}"
        );
        assert!(
            out.contains(&format!("Logs contain 1 | 0 {KICK_LINE_SUFFIX}")),
            "the asymmetry must be visible per side on the default path, \
             got:\n{out}"
        );
        assert!(
            out.contains(&format!("Logs contain 0 | 0 {MAPS_LINE_SUFFIX}")),
            "the map-read line must also be emitted on the default path, \
             got:\n{out}"
        );

        // The same path, with the map read present, must attribute both sides.
        let scanned = log_with_maps_read("12.345_678_901s");
        let mut out = Vec::new();
        let summary =
            super::log_diff_summary_from_strs(&scanned, &scanned, &default_opts, &mut out)?;
        let out = String::from_utf8(out).expect("diff output is utf-8");
        assert!(summary.matched_with_evidence());
        assert!(
            out.contains(
                "(run 1 first at turn 10, committed virtual time 12345678901ns, \
                 run 2 first at turn 10, committed virtual time 12345678901ns)"
            ),
            "both runs' values must be printed under the default mode, \
             got:\n{out}"
        );
        Ok(())
    }

    /// The reason both sides must be printed, as a reachable case rather than a
    /// precaution. `strip_lines` is the lossy comparator plain `--verify` uses:
    /// it normalizes numeric data before comparing, so two runs that committed
    /// the map read at different virtual times are a *matching* pair. Reporting
    /// only run 1 there would claim agreement on the exact quantity this record
    /// exists to expose — a drift `--verify-strict` catches and `--verify` does
    /// not.
    #[test]
    fn a_stripped_pass_shows_both_runs_diverging_map_read_times() -> std::io::Result<()> {
        let opts = super::LogDiffOpts {
            strip_lines: true,
            no_color: true,
            ..Default::default()
        };
        let mut out = Vec::new();
        let summary = super::log_diff_summary_from_strs(
            log_with_maps_read("12.345_678_901s"),
            log_with_maps_read("12.345_678_902s"),
            &opts,
            &mut out,
        )?;
        let out = String::from_utf8(out).expect("diff output is utf-8");
        assert!(
            summary.matched_with_evidence(),
            "the stripped comparator normalizes the times, so this pair passes; \
             got:\n{out}"
        );
        assert!(
            out.contains(
                "(run 1 first at turn 10, committed virtual time 12345678901ns, \
                 run 2 first at turn 10, committed virtual time 12345678902ns)"
            ),
            "a passing pair whose runs committed at DIFFERENT times must show \
             both values; showing one would report agreement on a real drift, \
             got:\n{out}"
        );
        Ok(())
    }

    /// A run that never performed the read must say so, rather than being
    /// silently represented by the other run's value.
    #[test]
    fn a_run_without_the_maps_read_is_named_not_borrowed() {
        assert_eq!(super::describe_maps_commit(None), "no such record");
        assert_eq!(
            super::describe_maps_commit(Some((10, Some(12_345_678_901)))),
            "first at turn 10, committed virtual time 12345678901ns"
        );
        assert_eq!(
            super::describe_maps_commit(Some((10, None))),
            "first at turn 10, committed virtual time unrecorded"
        );
    }

    /// The record is identified by both halves — a COMMIT *and* that resource —
    /// so neither a COMMIT on another path nor an unrelated mention of the map
    /// is counted.
    #[test]
    fn only_a_maps_read_commit_is_counted() {
        assert_eq!(
            super::maps_read_commits(&[
                historical(
                    0,
                    "INFO detcore::scheduler: [sched-step5] >>> COMMIT turn 18, dettid 5 using resources {Path(\"/proc/5/fd/3\"): R}, on previously committed 2s"
                ),
                historical(
                    1,
                    "INFO detcore: DETLOG [syscall][detcore, dtid 3] finish syscall #257: openat(-100, \"/proc/self/maps\", 0x0) = Ok(4)"
                ),
            ]),
            (0, None),
            "a COMMIT on another path, and a syscall naming the map, are both \
             excluded"
        );
        assert_eq!(
            super::maps_read_commits(&[historical(
                0,
                "INFO detcore::scheduler: [sched-step5] >>> COMMIT turn 10, dettid 3 using resources {Path(\"/proc/self/maps\"): R}, on previously committed 12.345_678_901s"
            )]),
            (1, Some((10, Some(12_345_678_901))))
        );
    }

    /// The count reads the message text, so an unrelated scheduler line must not
    /// be mistaken for a kick.
    #[test]
    fn only_the_kick_message_is_counted() {
        assert_eq!(
            super::count_empty_queue_kicks(&[
                historical(
                    0,
                    "INFO detcore::scheduler: [scheduler] run queue empty, exiting sched_loop."
                ),
                historical(
                    1,
                    "INFO detcore::scheduler: COMMIT turn 18, dettid 2, on previously committed 2s"
                ),
            ]),
            0
        );
        assert_eq!(
            super::count_empty_queue_kicks(&[historical(
                0,
                "INFO detcore::scheduler: scheduler (step2_process_blocked): zero threads left anywhere, fizzling."
            )]),
            1
        );
    }
}
