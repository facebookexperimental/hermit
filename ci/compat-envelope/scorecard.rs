#!/usr/bin/env -S rust-script --force
//! Keep Hermit's compatibility scorecard derived from the E2E manifest and
//! verify that a validate run produced a fresh passing row for every selected
//! regression cell.
//!
//! ```cargo
//! [dependencies]
//! fs2 = "0.4"
//! hermit-manifest-plan = { path = "../manifest-plan" }
//! libc = "0.2"
//! serde = { version = "1", features = ["derive"] }
//! serde_json = { version = "1", features = ["raw_value"] }
//! sha2 = "0.10"
//! tempfile = "3"
//! ```

#[path = "../../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::FileExt as UnixFileExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use fs2::FileExt;
use hermit_manifest_plan::canonical_verdict;
use hermit_manifest_plan::runner::FailureClass;
use hermit_manifest_plan::runner::ObservedResult;
use hermit_manifest_plan::stress_series::HostCapability;
use hermit_manifest_plan::stress_series::HostCapabilityVerdict;
use hermit_manifest_plan::stress_series::SeriesAttemptDisposition;
use hermit_manifest_plan::stress_series::SeriesCoordinates;
use hermit_manifest_plan::stress_series::SeriesNoVerdictEvidence;
use hermit_manifest_plan::stress_series::SeriesNoVerdictKind;
use hermit_manifest_plan::stress_series::SeriesOutcome;
use hermit_manifest_plan::stress_series::SeriesPayload;
use hermit_manifest_plan::stress_series::SeriesProducer;
use hermit_manifest_plan::stress_series::SeriesRow;
use hermit_manifest_plan::stress_series::SeriesSchema;
use hermit_manifest_plan::stress_series::SourceDepth;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use serde_json::value::RawValue;
use sha2::Digest;
use sha2::Sha256;
use tempfile::NamedTempFile;

const SCORECARD: &str = "SCORECARD.md";
const CELLS: &str = "ci/compat-envelope/cells.json";
const EXPECTED_PLAN: &str = "ci/expected-e2e-plan.json";
const SCHEMA: u64 = 7;
const PRESSURE_SUMMARY_SCHEMA: u64 = 5;
const CELL_RESULT_SCHEMA: u64 = 4;
const SCORECARD_SERIES_SNAPSHOT_SCHEMA: &str = "scorecard-series-snapshot/v1";
const SCORECARD_SERIES_SNAPSHOT_SOURCE: &str = "series";

const USAGE: &str = r#"Usage: ci/compat-envelope/scorecard.rs COMMAND [OPTIONS]

Commands:
  show
      Print the derived compatibility table.
  check
      Refuse if SCORECARD.md or ci/compat-envelope/cells.json is stale.
  update [--allow-green-removal REASON] [--allow-cell-removal]
      Rewrite the two tracked files. Green regressions and cell deletion are
      refused unless the matching explicit flag is present.
  update-observations --summary FILE [--summary FILE ...] [--retained]
      Merge completed clean pressure-test summaries into the cells' checked-in
      observations. By default every summary must name HEAD. --retained admits
      summaries from other readable Hermit commits after checking each commit's
      recorded Detcore tree, without replacing newer last_tested evidence.
      This never changes which cells are green.
  observe-results --results DIR
      Merge the canonical comparison results from ONE validate result directory
      into the cells' checked-in observations, under the `validate` provenance
      so they never mix with pressure-test bounds. Direct top-level local
      validates run this after their ledger and receipt work; ci-hub additionally
      runs it in the checkout that invoked validation after the isolated run.
  import-results --results DIR --current-summary FILE [--current-summary FILE ...]
      Import clean canonical comparisons retained on HEAD's history. A retained
      divergence position is imported only after current results classify it as
      FRESH, DRIFTED, WRONG, or UNCHECKABLE. This reads existing results; it does
      not execute a guest and it never changes scorecard colour.
  project-observations --series-root DIR --refreshed-at STAMP
      Re-derive observations and last_tested from exact comparison and typed
      no-verdict rows in the series store. Historical rows lacking either are
      named and skipped. REFUSES to drop measured evidence when the source
      supplied no rows: reading nothing is what an unpopulated series looks
      like, not a finding that a cell has no evidence. An unreachable root is
      refused outright rather than read as empty.
  project-and-observe-results --snapshot FILE --snapshot-sha256 HEX \
      --results DIR --expected-head SHA --refreshed-at STAMP
      In one locked transaction, project one immutable canonical series
      snapshot and then merge one completed validate result directory. The
      two generated files are replaced as one guarded pair. This command is
      dormant until its parent snapshot provider and callers land together.
  verify-results --results DIR [--lanes portable,privileged]
      Check the tracked files, then require a fresh PASS row at HEAD for every
      selected regression cell in the named lanes. The default is both lanes.
  self-test
      Exercise accepting and refusing result sets without running a guest.
  self-test-and-check
      Run the self-test and exact tracked-file check in one forced compilation.
  --help
      Show this text.

Green means that the cell is selected by full in ci/expected-e2e-plan.json.
Red means that the cell is in the manifest but is not selected by full; red
does not mean failed. Manifest-disabled combinations are Not applicable.
"#;

const PROJECT_AND_OBSERVE_USAGE: &str = r#"Usage: ci/compat-envelope/scorecard.rs project-and-observe-results \
    --snapshot FILE --snapshot-sha256 HEX --results DIR \
    --expected-head SHA --refreshed-at STAMP

Project one immutable scorecard-series-snapshot/v1 input, then merge one exact
completed validation result directory, under one scorecard write-back lock and
one guarded two-file replacement.
"#;

#[derive(Debug)]
struct ProjectAndObserveArgs {
    snapshot: PathBuf,
    snapshot_sha256: String,
    results: PathBuf,
    expected_head: String,
    refreshed_at: String,
}

#[derive(Clone, Debug, Deserialize)]
struct ManifestRow {
    backend: String,
    bucket: String,
    ci: bool,
    #[serde(default)]
    ci_disabled_reason: Option<CiDisabledReasonData>,
    enabled: bool,
    lane: String,
    mode: String,
    /// Why this backend is not enabled for this mode, verbatim from the
    /// manifest. Present exactly when `enabled` is false. See
    /// [`CellStatus::NotApplicable`].
    #[serde(default)]
    not_applicable_reason: Option<String>,
    test: String,
}

#[derive(Clone, Debug, Deserialize)]
struct ExpectedPlan {
    cells: Vec<CellId>,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct CellId {
    lane: String,
    category: String,
    test: String,
    mode: String,
    backend: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct TrackedCells {
    schema: u64,
    /// How the `observations` below relate to the series store. See
    /// [`ObservationProjection`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    projection: Option<ObservationProjection>,
    cells: Vec<TrackedCell>,
}

/// Declares that `observations` are a DERIVED PROJECTION, not the source of
/// truth, and records how stale that projection is.
///
/// ⚠️ WHY THE DEMOTION IS RECORDED IN THE FILE RATHER THAN JUST BELIEVED. Once
/// the series store is the authority, `cells.json` is a cache -- and a cache
/// that does not say when it was refreshed is indistinguishable from current
/// data. `last_tested` remains the per-cell staleness marker; this is the
/// per-file one, and it names the source so a reader can go and check.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ObservationProjection {
    /// Repository-relative path to the authoritative rows. Recorded so a stale
    /// projection can be re-derived without anyone having to remember.
    source: String,
    /// A commit in the Git repository containing `source` whose JSONL shard
    /// population and bytes produced this projection.
    ///
    /// Historical projection blocks predate this field and remain readable.
    /// Every new projection records it so staleness can be distinguished from
    /// an incorrect projection.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_object_id"
    )]
    source_commit: Option<String>,
    /// The Git tree object for the selected source directory at
    /// `source_commit`. Unlike `source`, this identity is independent of the
    /// producer's checkout and current working directory.
    ///
    /// Historical projection blocks predate this field and remain readable,
    /// but a consumer cannot verify which subtree they projected.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_source_tree"
    )]
    source_tree: Option<String>,
    /// When this projection was last refreshed from that source.
    refreshed_at: String,
    /// How many series rows the refresh actually read.
    ///
    /// ⚠️ ZERO IS A REAL AND DIFFERENT ANSWER FROM ABSENT. A refresh that read
    /// zero rows has not established that a cell has no evidence -- it has
    /// established that the source had nothing to say, which is what happens
    /// when the producer has not run yet. Recording the count is what lets a
    /// reader tell "projected from 800 rows" from "projected from nothing".
    rows_read: u64,
    /// True when observations below predate the series store and are therefore
    /// still authoritative rather than derived.
    ///
    /// This remains true while any observation below has no matching series
    /// row, so an initial partial projection cannot silently erase older
    /// evidence.
    #[serde(default)]
    pre_series_corpus: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct TrackedCell {
    #[serde(flatten)]
    id: CellId,
    #[serde(default)]
    enabled: bool,
    status: CellStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ci_disabled_reason: Option<CiDisabledReasonData>,
    /// Why the green-removal ratchet was OVERRIDDEN for this cell, verbatim from
    /// `update --allow-green-removal <reason>`.
    ///
    /// ⚠️ THIS EXISTS BECAUSE THE OVERRIDE USED TO LEAVE NO TRACE. The ratchet at
    /// the top of [`tracked_from`] does refuse a green -> red move -- measured, it
    /// names the cell and exits 2. But `--allow-green-removal` was a bare flag
    /// typed into a shell, so once it was used nothing in the tree recorded that
    /// it had been. A reviewer reading the diff saw a cell flip green -> red and
    /// could not tell "the generator was run normally" from "the generator
    /// refused and the author told it not to". Both readings produce byte-
    /// identical output, which is what made the guard's override invisible rather
    /// than merely permissive.
    ///
    /// Measured on 2026-08-25: the two required-plan reductions on main
    /// (3398c18343, 300 -> 299 and 8ca72c6851, 298 -> 296) both took this path and
    /// both recorded their reason -- one in a 33-line commit message, one in a
    /// `ci_disabled_reason` field. Two conventions, two places, and no gate reads
    /// either. This field is the one place that travels with the cell.
    ///
    /// CLEARED when the cell is green again, so it describes the CURRENT
    /// override and never accumulates history a reader would have to date.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    green_removal_reason: Option<String>,
    /// Why this cell is [`CellStatus::NotApplicable`], verbatim from the
    /// manifest. Present exactly when the status is `NotApplicable`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    not_applicable_reason: Option<String>,
    /// Last recorded exercise of this cell. See [`LastTested`] -- absence means
    /// no writer recorded one, NOT that the cell was never run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_tested: Option<LastTested>,
    /// Comparison evidence, keyed by `(recorded code identity, provenance)`.
    /// Explicit Detcore trees remain in `detcore_tree`. A projected legacy row
    /// without that field omits it and carries exactly one recorded Hermit SHA,
    /// which the versioned projector treats as a distinct identity variant.
    ///
    /// `update-observations`, `observe-results`, `import-results`, and
    /// `project-observations` write it. Direct top-level local validates invoke
    /// `observe-results` after their ledger and receipt work; ci-hub additionally
    /// performs the same write in the checkout that invoked validation.
    ///
    /// The provenances answer different questions and are never merged --
    /// repeat commands exercise a cell at one tree and measure flakiness, while
    /// validate runs it once per commit and supplies the regression signal.
    observations: Vec<Observation>,
    /// What the evidence above says, in one word, readable without opening
    /// another file. DERIVED -- see [`MeasurementState`] and
    /// [`derive_measurement`]. Defaulted for the pre-field corpus; every write
    /// recomputes it and the writer boundary refuses a stored value that
    /// disagrees with the derivation.
    #[serde(default = "default_measurement")]
    measurement: MeasurementState,
}

fn default_measurement() -> MeasurementState {
    MeasurementState::NeverMeasured
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct CiDisabledReasonData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    evidence: Option<String>,
    reason: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum CellStatus {
    Green,
    Red,
    /// The backend is not enabled for this test and mode, so the cell was never
    /// asked to do anything and cannot have failed.
    ///
    /// ⚠️ THIS EXISTS BECAUSE `Red` WAS CARRYING THREE MEANINGS AT ONCE: it
    /// failed, it was never measured, and it does not apply. Measured
    /// 2026-08-25: of 5,317 red cells, exactly TWO carried any observation, and
    /// 4,940 were cells whose backend is not enabled for their mode. A reader
    /// seeing 5,317 reds was reading 93% not-applicable as failure.
    ///
    /// It is DERIVED FROM `enabled`, never asserted independently, and it always
    /// carries the manifest's own reason string -- so it cannot drift from the
    /// manifest and it cannot be set without saying why.
    #[serde(rename = "not-applicable")]
    NotApplicable,
}

impl CellStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Green => "green",
            Self::Red => "red",
            Self::NotApplicable => "not-applicable",
        }
    }
}

/// WHICH MECHANISM produced an observation. These mechanisms answer different
/// questions and their bounds must never be merged into one range.
///
/// HERMIT REPEAT and PRESSURE TEST repeat a cell at ONE fixed tree, so their
/// bounds isolate run-to-run flakiness. They remain distinct because they are
/// separate commands with separate run identifiers.
///
/// VALIDATE runs a cell ONCE per commit, so a single validate observation is a
/// point, not a distribution. Its value is as the regression signal a floor is
/// CHECKED against.
///
/// Merging them would produce one number moving for two unrelated causes -- "the
/// code changed" and "this varies run to run" -- which is the measurement trap
/// this project has repeatedly been bitten by. Observations are therefore keyed
/// by `(recorded code identity, provenance)`, not by identity alone. See
/// [`Observation::detcore_tree`] for the schema-gated legacy representation.
#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    Eq,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize
)]
#[serde(rename_all = "kebab-case")]
enum ObservationProvenance {
    HermitRepeat,
    PressureTest,
    Validate,
}

impl ObservationProvenance {
    fn as_str(self) -> &'static str {
        match self {
            Self::HermitRepeat => "hermit-repeat",
            Self::PressureTest => "pressure-test",
            Self::Validate => "validate",
        }
    }
}

/// What the row's evidence actually says, readable WITHOUT opening another file.
///
/// ⚠️ THIS EXISTS BECAUSE AN EMPTY FIELD HAD FIVE MEANINGS. `last_tested`'s own
/// doc already warns that absence "means NO WRITER RECORDED ONE, not that the
/// cell was never exercised", and `render_evidence_coverage` was added to "state
/// the coverage out loud every time the table is printed". That is a mitigation
/// at the PRINTER: a consumer reading `cells.json` directly still could not tell
/// a never-measured cell from a measured-and-passed one, because both have an
/// empty `observations` and both may lack `last_tested`. Anything downstream
/// then had to guess, and a guess that reads "never measured" as "passed" turns
/// absence of evidence into evidence.
///
/// DERIVED, NEVER ASSERTED. `derive_measurement` computes this from the
/// evidence already on the row, and `enforce_writer_boundary` refuses any write
/// where the stored value disagrees. So it is a cache that cannot lie rather
/// than a second source of truth that can drift from the first.
///
/// The vocabulary is deliberately IDENTICAL to `ci-hub/series/series.py`'s
/// `measurement_state`, so the scorecard and the series store cannot describe
/// the same cell with different words.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum MeasurementState {
    /// No observation has been imported. NOT the same as never run, and NOT the
    /// same as passing.
    NeverMeasured,
    /// Measured, and every recorded result was a pass.
    MeasuredAndPassed,
    /// Measured, nothing passed, but nothing diverged either -- every result was
    /// a crash, timeout or OOM. ⚠️ A NON-VERDICT IS NOT A DIVERGENCE: reading
    /// one as a product failure is how an infrastructure hiccup becomes a false
    /// regression.
    MeasuredNoVerdict,
    /// A real divergence, whose position could not be established. A LEGITIMATE
    /// answer, not an error: refusing it would force a writer to invent a
    /// coordinate it does not have.
    DivergedUnlocated,
    /// A real divergence with at least one located coordinate.
    Diverged,
}

impl MeasurementState {
    fn as_str(self) -> &'static str {
        match self {
            Self::NeverMeasured => "never-measured",
            Self::MeasuredAndPassed => "measured-and-passed",
            Self::MeasuredNoVerdict => "measured-no-verdict",
            Self::DivergedUnlocated => "diverged-unlocated",
            Self::Diverged => "diverged",
        }
    }
}

/// Compute the one correct [`MeasurementState`] for a row from its own evidence.
///
/// A divergence is a determinism/parity/replay failure. Crash, timeout and OOM
/// are measured NON-VERDICTS and deliberately do not count as divergence.
fn derive_measurement(cell: &TrackedCell) -> MeasurementState {
    if cell.observations.is_empty() {
        return MeasurementState::NeverMeasured;
    }
    let mut diverged = false;
    let mut passed = false;
    let mut located = false;
    for observation in &cell.observations {
        for result in &observation.results {
            match result {
                ObservedResult::Pass => passed = true,
                ObservedResult::DeterminismFailure
                | ObservedResult::ParityFailure
                | ObservedResult::ReplayFailure => diverged = true,
                ObservedResult::CrashError
                | ObservedResult::Timeout
                | ObservedResult::Oom
                | ObservedResult::SandboxDenied
                | ObservedResult::InfrastructureError => {}
            }
        }
        located |= !observation.first_divergent_record.is_empty()
            || !observation.first_divergent_syscall.is_empty()
            || !observation.first_divergent_scheduler_turn.is_empty()
            || !observation.first_divergent_virtual_nanoseconds.is_empty();
    }
    if diverged {
        return if located {
            MeasurementState::Diverged
        } else {
            MeasurementState::DivergedUnlocated
        };
    }
    if passed {
        MeasurementState::MeasuredAndPassed
    } else {
        MeasurementState::MeasuredNoVerdict
    }
}

/// Set every row's `measurement` to its derived value. Called before the writer
/// boundary so the guard checks a value the writer actually intended.
fn refresh_measurement(cells: &mut TrackedCells) {
    for cell in &mut cells.cells {
        cell.measurement = derive_measurement(cell);
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct Observation {
    /// The exact recorded Detcore tree for ordinary/current evidence.
    /// Scorecard schema 7 also permits this field to be absent for a projected
    /// legacy series row; its single `hermit_shas` entry is then the recorded
    /// code identity. Other writers always supply a real Detcore tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    detcore_tree: Option<String>,
    /// Exact immutable series events folded into this projected aggregate.
    /// Empty means the observation predates projection ownership or belongs to
    /// another writer and therefore may not be replaced by the projector.
    #[serde(
        default,
        skip_serializing_if = "BTreeSet::is_empty",
        deserialize_with = "deserialize_unique_event_ids"
    )]
    event_ids: BTreeSet<String>,
    /// Defaulted to `pressure-test` so the pre-provenance corpus keeps its
    /// meaning: before this field existed, the pressure test was the ONLY
    /// writer of observations, so that is what any older entry must have been.
    /// (In practice no such entry exists -- every cell's array was empty when
    /// this landed -- but a default that quietly relabelled old data as
    /// validate-sourced would be wrong even with nothing to relabel.)
    #[serde(default = "default_provenance")]
    provenance: ObservationProvenance,
    /// Depth at which this observation was taken, KEYED BY REPOSITORY.
    ///
    /// Per-repo because hermit depth and reverie depth are DIFFERENT KEYSPACES
    /// and a bare integer would be read against whichever one the reader had in
    /// mind. Measured at the time of writing: hermit 1927, reverie 931. Those
    /// numbers are not comparable to each other in any way.
    ///
    /// A repository whose depth could not be resolved is ABSENT from the map
    /// rather than present with a zero or a guess. Hermit is always resolved at
    /// the recorded Hermit SHA. Reverie is resolved at the pin in that exact
    /// revision's Cargo.lock when a checkout containing the pin is reachable.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    depth: BTreeMap<String, SourceDepth>,
    hermit_shas: BTreeSet<String>,
    #[serde(deserialize_with = "deserialize_product_observation_results")]
    results: BTreeSet<ObservedResult>,
    /// The compact receipt for canonical validate evidence. Raw result files
    /// remain in retained history; this keeps the comparison identity and INFO
    /// population needed to score the cell without copying every argv and
    /// environment value into the tracked scorecard.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    canonical_comparisons: BTreeSet<CanonicalComparison>,
    invocations: BTreeSet<ObservedInvocation>,
    #[serde(default, skip_serializing_if = "ObservedPositions::is_empty")]
    first_divergent_scheduler_turn: ObservedPositions,
    #[serde(default, skip_serializing_if = "ObservedPositions::is_empty")]
    first_divergent_virtual_nanoseconds: ObservedPositions,
    /// The prefix of the log that was deterministic, as a compared-record
    /// index. Shares a unit with the report's compared counts, unlike the two
    /// above, which are positions in scheduler and virtual time.
    #[serde(default, skip_serializing_if = "ObservedPositions::is_empty")]
    first_divergent_record: ObservedPositions,
    /// Syscalls the guest completed before diverging. A DIFFERENT KEYSPACE from
    /// the three above -- one real divergence was record 98, syscall 37,
    /// scheduler turn 4 -- so these bounds must never be read against another
    /// coordinate's axis.
    #[serde(default, skip_serializing_if = "ObservedPositions::is_empty")]
    first_divergent_syscall: ObservedPositions,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct CanonicalComparison {
    hermit_sha: String,
    hermit_commits: u64,
    hermit_first_parent: u64,
    run_id: String,
    evidence_sha256: String,
    result: ObservedResult,
    left_info_messages: BTreeSet<u64>,
    right_info_messages: BTreeSet<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct ObservedInvocation {
    hermit_sha: String,
    run_id: String,
    /// Outer framework attempt. The entries in `attempts` are Hermit
    /// invocations inside this one cell attempt and do not identify retries of
    /// the cell itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    attempt: Option<u64>,
    /// Identity of the complete framework row when one exists. Pressure-test
    /// summaries predate this field and therefore leave it absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    evidence_sha256: Option<String>,
    /// `None` is a measured no-verdict: the framework ran the cell but did not
    /// produce a trustworthy product result. Keeping the invocation preserves
    /// that fact without inventing a crash, divergence, or pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    result: Option<ObservedResult>,
    argv: Vec<String>,
    guest_argv: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: String,
    /// Empty in compact stored observations after the importer has verified
    /// that this value reconstructs exactly from cwd, env and argv.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    shell_command: String,
    attempts: Vec<ObservedAttemptInvocation>,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct ObservedAttemptInvocation {
    index: String,
    outcome: String,
    status: Option<i32>,
    signal: Option<i32>,
    timed_out: bool,
    argv: Vec<String>,
    guest_argv: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: String,
    /// Empty in compact stored observations after the importer has verified
    /// that this value reconstructs exactly from cwd, env and argv.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    shell_command: String,
}

/// How deep in a repository's history an observation was taken, so staleness is
/// SUBTRACTABLE -- "measured 47 commits ago" rather than a stare at two opaque
/// forty-hex shas.
///
/// BOTH COUNTS ARE RECORDED BECAUSE THEY DISAGREE, and by a lot. Measured on
/// this repository at the time of writing: `git rev-list --count HEAD` is 1927
/// while `--first-parent` is 1852, a 75-commit gap, because hermit's history
/// contains merges. The plain count is TOTAL REACHABLE ANCESTRY -- every commit
/// that was ever merged in, including whole side branches -- and is NOT a
/// distance along the mainline. `first_parent` is the mainline distance and is
/// the one that matches what "N commits ago" means to a reader. Recording only
/// one would guarantee it eventually gets read as the other.
///
/// SUBTRACTION IS ONLY MEANINGFUL ALONG ANCESTRY. Two commits on divergent
/// branches can carry identical depths while being unrelated, so a difference
/// is a distance only once one commit is known to be an ancestor of the other.
/// Depth complements the recorded tree and sha; it does not replace them.
/// When a cell was last ACTUALLY EXERCISED, by sha and depth. No timestamp: a
/// date cannot tell you whether the code under test changed, and the tree hash
/// and depth can.
///
/// ⚠️ ABSENCE MEANS "NO WRITER HAS RECORDED ONE", NOT "NEVER TESTED". The two
/// are easy to confuse and the difference matters. Only the explicit fold,
/// import, and projection commands write this field. A cell can therefore have
/// retained runs while carrying no imported record. `scorecard.rs show` prints
/// how many cells carry the field precisely so this emptiness stays visible
/// instead of being read as evidence.
///
/// This is recorded for EVERY tested cell, including passing ones, and names
/// the latest admitted comparison directly rather than asking readers to infer
/// recency from an aggregate observation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct LastTested {
    hermit_sha: String,
    /// The staleness key. Content-addressed, so comparing it to `HEAD:detcore`
    /// says whether the result still describes the current code regardless of
    /// how old or recent the run was. A legacy projected row without a recorded
    /// Detcore tree cannot update this field.
    detcore_tree: String,
    /// Keyed by repository: hermit depth and reverie depth are different
    /// keyspaces and a bare number would be read against the wrong one.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    depth: BTreeMap<String, SourceDepth>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct ObservedRange {
    earliest: u64,
    latest: u64,
    /// How many LOCATED positions produced these bounds.
    ///
    /// Without it the bounds invite exactly the over-reading this repository
    /// keeps catching: "earliest 80, latest 500" is a different claim over two
    /// runs than over fifty, and the pair alone cannot tell them apart.
    ///
    /// This is NOT the number of runs, and the difference matters. A run that
    /// passed located nothing and contributes no sample, so `samples` counts
    /// only the runs that actually diverged. It is also per COORDINATE rather
    /// than per observation, because a report can locate a scheduler turn
    /// without locating a virtual nanosecond, which would leave one coordinate
    /// with fewer samples behind it than its sibling.
    ///
    /// `samples == 1` is the honest way to say "one observation, so these
    /// bounds are a point, not a range".
    samples: u64,
}

/// Refuse a projection refresh that would erase evidence it did not replace.
///
/// ⚠️ THIS IS THE WHOLE POINT OF LANDING THE GUARD BEFORE THE DATA. Once
/// `cells.json` is a projection, the natural implementation is "read the series,
/// write what it says". With an empty or unreachable series that writes NOTHING,
/// and the three real divergence coordinates currently on main -- the only
/// located evidence in the file -- disappear. The resulting row is
/// indistinguishable from a cell nobody ever measured, which is precisely the
/// failure `deny_unknown_fields` was load-bearing against one step earlier: a
/// silent default that reads as absence of evidence.
///
/// So a refresh may only DROP an observation when the source actually supplied
/// rows to replace it. Zero rows read is not a statement that a cell has no
/// evidence; it is a statement that the source had nothing to say.
fn enforce_projection_preserves_evidence(
    before: &TrackedCells,
    after: &TrackedCells,
    rows_read: u64,
) -> Result<(), String> {
    if rows_read > 0 {
        return Ok(());
    }
    let index = |cells: &TrackedCells| -> BTreeMap<CellId, TrackedCell> {
        cells
            .cells
            .iter()
            .map(|cell| (cell.id.clone(), cell.clone()))
            .collect()
    };
    let (old, new) = (index(before), index(after));
    for (id, old_cell) in &old {
        let had = located_evidence(old_cell);
        if had == 0 {
            continue;
        }
        let has = new.get(id).map_or(0, located_evidence);
        if has < had {
            return Err(format!(
                "projection refused: it read 0 series rows yet would drop measured \
                 evidence from {}/{}/{} ({had} located coordinate(s) down to {has}). \
                 Reading nothing is not the same as establishing that a cell has no \
                 evidence -- it is what an unpopulated or not-yet-written series \
                 looks like. Populate the series (plan step 4) or leave the \
                 pre-series corpus alone.",
                id.test, id.mode, id.backend
            ));
        }
    }
    Ok(())
}

/// How many located divergence positions a cell holds, across every
/// observation and all four coordinates.
///
/// ⚠️ COUNTED IN POSITIONS, NOT OBSERVATIONS. The first version of the guard
/// above compared `observations.len()`, and an end-to-end run against the real
/// `cells.json` walked straight through it: the naive projection does not
/// delete observations, it BLANKS THE COORDINATES INSIDE THEM. The vector
/// length never moves, so a count-based guard sees a clean refresh while all
/// three located coordinates on main disappear. The quantity that can vanish is
/// the quantity that has to be counted.
fn located_evidence(cell: &TrackedCell) -> usize {
    cell.observations
        .iter()
        .map(|observation| {
            [
                &observation.first_divergent_scheduler_turn,
                &observation.first_divergent_virtual_nanoseconds,
                &observation.first_divergent_record,
                &observation.first_divergent_syscall,
            ]
            .into_iter()
            .filter(|positions| !positions.is_empty())
            .count()
        })
        .sum()
}

/// Every located position for one coordinate, ONE ENTRY PER RUN THAT LOCATED IT.
///
/// ⚠️ THE STORED FORM IS THE POSITIONS; THE RANGE IS DERIVED. Storing only
/// `{earliest, latest, samples}` discards the distribution the pressure test
/// exists to measure: `{earliest 93, latest 94, samples 2}` and fifty runs
/// clustered at 93 with one outlier at 94 are the same triple and completely
/// different findings. Keeping the positions makes the bound recomputable and
/// everything else -- median, clustering, whether a "range" is really a point
/// with one stray -- answerable later without re-running anything.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
struct ObservedPositions {
    /// One entry per run that located this coordinate, in insertion order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    positions: Vec<u64>,
    /// Bounds inherited from before positions were stored.
    ///
    /// ⚠️ DELIBERATELY NOT EXPANDED INTO POSITIONS, and this is the whole
    /// honesty of the migration. `{earliest 93, latest 94, samples 2}` records
    /// that two runs diverged somewhere in [93, 94]; it does NOT record which
    /// run was which, and no rule recovers that. Synthesising two positions
    /// would invent measurements nobody took -- the exact fabrication this
    /// change exists to prevent -- so legacy evidence is carried forward as the
    /// weaker claim it always was, and marked as such.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    legacy_bounds: Option<ObservedRange>,
}

impl ObservedPositions {
    /// The DERIVED `{earliest, latest, samples}` view. `None` when nothing
    /// located this coordinate.
    fn range(&self) -> Option<ObservedRange> {
        let from_positions = if self.positions.is_empty() {
            None
        } else {
            Some(ObservedRange {
                earliest: *self.positions.iter().min().expect("non-empty"),
                latest: *self.positions.iter().max().expect("non-empty"),
                samples: self.positions.len() as u64,
            })
        };
        match (from_positions, self.legacy_bounds) {
            (None, legacy) => legacy,
            (Some(range), None) => Some(range),
            // Both present: widen the bounds and ADD the sample counts, because
            // the legacy triple stands for runs that really happened even though
            // their individual positions are gone.
            (Some(range), Some(legacy)) => Some(ObservedRange {
                earliest: range.earliest.min(legacy.earliest),
                latest: range.latest.max(legacy.latest),
                samples: range.samples + legacy.samples,
            }),
        }
    }

    fn is_empty(&self) -> bool {
        self.positions.is_empty() && self.legacy_bounds.is_none()
    }

    /// Record one run's located position. A run that located nothing is not a
    /// sample of WHERE the divergence was and contributes no entry.
    fn record(&mut self, value: Option<u64>) {
        if let Some(value) = value {
            self.positions.push(value);
        }
    }
}

/// Accept both the current form and the pre-step-5 bare range, without letting
/// a legacy object silently deserialize as "no evidence".
impl<'de> Deserialize<'de> for ObservedPositions {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Current {
            #[serde(default)]
            positions: Vec<u64>,
            #[serde(default)]
            legacy_bounds: Option<ObservedRange>,
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Either {
            // ⚠️ `null` IS A REAL FORM IN THE EXISTING FILE and must be handled
            // explicitly. Before this change the four coordinates were
            // `Option<ObservedRange>` and serialized as `null` when nothing had
            // located them, so the tracked corpus is full of them. Omitting this
            // variant made `update` abort with "data did not match any variant
            // of untagged enum Either" -- caught by running it, not by reading.
            Absent(()),
            // ⚠️ `Current` IS TRIED FIRST AND USES `deny_unknown_fields`, which is
            // load-bearing: without it a legacy `{earliest, latest, samples}`
            // object would match `Current` with every field defaulted and the
            // bounds would be SILENTLY DISCARDED -- evidence loss that looks
            // exactly like a cell that was never measured.
            Current(Current),
            Legacy(ObservedRange),
        }
        Ok(match Either::deserialize(deserializer)? {
            Either::Absent(()) => ObservedPositions::default(),
            Either::Current(current) => ObservedPositions {
                positions: current.positions,
                legacy_bounds: current.legacy_bounds,
            },
            Either::Legacy(range) => ObservedPositions {
                positions: Vec::new(),
                legacy_bounds: Some(range),
            },
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PressureSummary {
    schema: u64,
    hermit_sha: String,
    detcore_tree: String,
    source_tree_dirty: bool,
    rows: Vec<PressureSummaryRow>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PressureSummaryRow {
    cell: CellId,
    /// Which repeat of this cell the row describes.
    ///
    /// ⚠️ THIS FIELD IS WHY THE RANGES WERE UNFILLABLE. `pressure-test.rs run
    /// --repetitions N` emits ONE ROW PER REPETITION, each stamped with its
    /// repetition and carrying its own verification report. This consumer had
    /// no such field and treated the second row for a cell as a duplicate,
    /// refusing the whole summary -- so the one workflow that can produce a
    /// distribution could never reach the scorecard, and `samples` could never
    /// exceed one.
    ///
    /// Repeats are now distinguished. The attempt below additionally separates
    /// a failed observation from a passing retry inside one repetition.
    #[serde(default)]
    repetition: Option<u64>,
    /// Which framework-written attempt within this repetition produced the
    /// row. Older summaries contain one row per repetition and therefore
    /// default to attempt 1.
    #[serde(default = "default_attempt")]
    attempt: u64,
    result: String,
    #[serde(default)]
    verification: Option<canonical_verdict::VerificationReport>,
    #[serde(default)]
    evidence_errors: Vec<String>,
    invocation: Option<PressureInvocation>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PressureInvocation {
    run_id: String,
    argv: Vec<String>,
    guest_argv: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: String,
    shell_command: String,
    attempts: Vec<ObservedAttemptInvocation>,
}

fn is_prelaunch_timeout_disposition(
    timed_out: bool,
    status: Option<i64>,
    signal: Option<i64>,
    error_kind: Option<&str>,
) -> bool {
    timed_out
        && status.is_none()
        && signal.is_none()
        && matches!(
            error_kind,
            Some("incomplete-verification-evidence" | "cpu-timeout" | "wall-timeout")
        )
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ResultRow {
    schema: u64,
    run_id: String,
    #[serde(default = "default_attempt")]
    attempt: u64,
    hermit_sha: String,
    source_tree_dirty: bool,
    binary_sha256: Option<String>,
    test_sha256: String,
    test: String,
    category: String,
    lane: String,
    mode: String,
    backend: Option<String>,
    classification: String,
    outcome: String,
    #[serde(default)]
    result: Option<ObservedResult>,
    #[serde(default)]
    failure_class: Option<FailureClass>,
    #[serde(default)]
    error_kind: Option<String>,
    #[serde(default)]
    timeout_seconds: u64,
    #[serde(default)]
    execution_cpu_timeout_seconds: Option<u64>,
    #[serde(default)]
    execution_wall_timeout_seconds: Option<u64>,
    log_level: Option<String>,
    effective_args: Vec<String>,
    argv: Vec<String>,
    guest_argv: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: String,
    shell_command: String,
    relaxations: Vec<String>,
    attempts: Vec<JsonValue>,
    /// WHERE the cell diverged, as emitted by the harness.
    ///
    /// `#[serde(default)]` for the same reason the sibling copy in
    /// hermit-cli/src/canonical_verdict.rs is tolerant: this reader
    /// aggregates RETAINED result rows, including ones written before the field
    /// existed, so absence has to mean "older row" rather than "broken
    /// producer". The pressure test's copy is deliberately REQUIRED-nullable
    /// instead, because it only ever reads rows it just produced.
    #[serde(default)]
    first_divergent_scheduler_turn: Option<u64>,
    #[serde(default)]
    first_divergent_virtual_nanoseconds: Option<u64>,
    /// The third coordinate, added by hermit#2386. Unlike its two siblings this
    /// one LOCATES the divergence rather than bounding it: they are the
    /// position of the preceding scheduler COMMIT, while this is the index of
    /// the differing record itself.
    #[serde(default)]
    first_divergent_record: Option<u64>,
    #[serde(default)]
    first_divergent_syscall: Option<u64>,
}

#[derive(Debug)]
enum ValidateRowEvidence {
    Matched {
        left_info_messages: BTreeSet<u64>,
        right_info_messages: BTreeSet<u64>,
    },
    Diverged {
        left_info_messages: BTreeSet<u64>,
        right_info_messages: BTreeSet<u64>,
    },
    NotRun {
        reason: String,
        result: Option<ObservedResult>,
    },
    Unavailable {
        reason: String,
        result: Option<ObservedResult>,
    },
}

fn default_attempt() -> u64 {
    1
}

impl ResultRow {
    fn validate_timeout_policy(&self) -> Result<(), String> {
        match (
            self.execution_cpu_timeout_seconds,
            self.execution_wall_timeout_seconds,
        ) {
            (None, None) => Ok(()),
            (Some(cpu), Some(wall)) if cpu > 0 && wall == self.timeout_seconds && wall > cpu => {
                Ok(())
            }
            (cpu, wall) => Err(format!(
                "timeout policy disagrees: timeout_seconds={} execution_cpu_timeout_seconds={cpu:?} execution_wall_timeout_seconds={wall:?}",
                self.timeout_seconds
            )),
        }
    }

    fn require_current_timeout_policy(&self) -> Result<(), String> {
        self.validate_timeout_policy()?;
        if self.execution_cpu_timeout_seconds.is_none()
            || self.execution_wall_timeout_seconds.is_none()
        {
            return Err("current result omitted explicit execution timeout bounds".into());
        }
        Ok(())
    }

    fn id(&self) -> Option<CellId> {
        let backend = match self.backend.as_deref() {
            Some(backend) => backend.to_string(),
            None if self.mode == "naked" => "native".to_string(),
            None => return None,
        };
        Some(CellId {
            lane: self.lane.clone(),
            category: self.category.clone(),
            test: self.test.clone(),
            mode: self.mode.clone(),
            backend,
        })
    }

    fn require_literal_invocation(&self) -> Result<(), String> {
        if self.run_id.trim().is_empty()
            || self.argv.is_empty()
            || self.guest_argv.is_empty()
            || self.env.is_empty()
            || self.cwd.trim().is_empty()
            || self.shell_command.trim().is_empty()
            || self.shell_command != literal_shell_command(&self.cwd, &self.env, &self.argv)
            || self.attempts.is_empty()
            || self.attempts.iter().any(|attempt| {
                attempt
                    .get("argv")
                    .and_then(JsonValue::as_array)
                    .is_none_or(Vec::is_empty)
                    || attempt
                        .get("guest_argv")
                        .and_then(JsonValue::as_array)
                        .is_none_or(Vec::is_empty)
                    || attempt.get("env").and_then(JsonValue::as_object).is_none()
                    || attempt
                        .get("cwd")
                        .and_then(JsonValue::as_str)
                        .is_none_or(str::is_empty)
                    || attempt_shell_command_is_invalid(attempt)
            })
            || self
                .attempts
                .first()
                .and_then(|attempt| attempt.get("argv"))
                != Some(&serde_json::to_value(&self.argv).unwrap())
            || self
                .attempts
                .first()
                .and_then(|attempt| attempt.get("guest_argv"))
                != Some(&serde_json::to_value(&self.guest_argv).unwrap())
            || self.attempts.first().and_then(|attempt| attempt.get("env"))
                != Some(&serde_json::to_value(&self.env).unwrap())
            || self.attempts.first().and_then(|attempt| attempt.get("cwd"))
                != Some(&JsonValue::String(self.cwd.clone()))
            || self
                .attempts
                .first()
                .and_then(|attempt| attempt.get("shell_command"))
                != Some(&JsonValue::String(self.shell_command.clone()))
        {
            return Err("does not bind its PASS/FAIL claim to a literal invocation".into());
        }
        Ok(())
    }

    fn require_provenance(&self) -> Result<(), String> {
        self.require_literal_invocation()?;
        let binary_sha = self
            .binary_sha256
            .as_deref()
            .ok_or("has no Hermit binary identity")?;
        require_sha256("Hermit binary", binary_sha)?;
        require_sha256("test", &self.test_sha256)?;
        if self.effective_args != self.argv.iter().skip(1).cloned().collect::<Vec<_>>() {
            return Err("effective_args does not match literal argv".into());
        }
        if self.mode == "naked" {
            if self.log_level.is_some() {
                return Err("naked result unexpectedly records a Hermit log level".into());
            }
        } else if self.log_level.as_deref().is_none_or(str::is_empty) {
            return Err("Hermit result has no log-level identity".into());
        }
        if self
            .relaxations
            .iter()
            .any(|relaxation| relaxation.trim().is_empty())
            || self.relaxations.iter().collect::<BTreeSet<_>>().len() != self.relaxations.len()
        {
            return Err("relaxations contain an empty or duplicate identity".into());
        }
        Ok(())
    }

    /// Validate the producer-owned outcome classification when this retained
    /// row carries it. Schema-4 rows written before these fields existed remain
    /// readable only when both are absent.
    fn validate_recorded_classification(&self) -> Result<(), String> {
        if self.result.is_none() && self.failure_class.is_none() {
            return Ok(());
        }
        match self.outcome.as_str() {
            "PASS" => {
                if self.result != Some(ObservedResult::Pass) || self.failure_class.is_some() {
                    return Err(format!(
                        "PASS row must carry result=pass and no failure_class, got result={:?} failure_class={:?}",
                        self.result, self.failure_class
                    ));
                }
            }
            "FAIL" => {
                let result = self
                    .result
                    .ok_or("FAIL row has no observed result; the failure kind was lost")?;
                let expected = result
                    .failure_class()
                    .ok_or_else(|| format!("FAIL row cannot carry passing result {result:?}"))?;
                if self.failure_class != Some(expected) {
                    return Err(format!(
                        "observed result {} requires failure_class {:?}, got {:?}",
                        result.as_str(),
                        expected,
                        self.failure_class
                    ));
                }
            }
            "ERROR" => {
                if self.result.is_some() {
                    return Err(format!(
                        "ERROR row must not carry a product observation, got {:?}",
                        self.result
                    ));
                }
                if !matches!(
                    self.failure_class,
                    Some(
                        FailureClass::UnderstoodInfrastructureFailure
                            | FailureClass::UnderstoodPrerequisiteFailure
                            | FailureClass::NoResult
                    )
                ) {
                    return Err(format!(
                        "ERROR row must carry a non-product failure_class, got {:?}",
                        self.failure_class
                    ));
                }
            }
            "HOST-INAPPLICABLE" => {
                if self.result.is_some()
                    || self.failure_class != Some(FailureClass::UnderstoodPrerequisiteFailure)
                {
                    return Err(format!(
                        "HOST-INAPPLICABLE row must carry understood_prerequisite_failure and no observed result, got result={:?} failure_class={:?}",
                        self.result, self.failure_class
                    ));
                }
            }
            other => return Err(format!("unknown cell outcome {other:?}")),
        }
        Ok(())
    }

    /// Return only a result proved by the typed outer disposition. A timeout is
    /// observable even when no canonical comparison completed. Other
    /// unavailable rows remain result-less until their producer supplies a
    /// trustworthy class; an exit status alone is not enough to call a crash.
    fn no_verdict_result(&self) -> Option<ObservedResult> {
        self.attempts
            .iter()
            .any(|attempt| attempt.get("timed_out").and_then(JsonValue::as_bool) == Some(true))
            .then_some(ObservedResult::Timeout)
    }

    /// Return the typed reason when this FAIL records only completed first
    /// runs rejected before comparison. Such a row is execution evidence, but
    /// not product-behavior evidence: it must be named and retained as a
    /// measured no-verdict rather than forced through the comparison path.
    fn typed_no_result_reason(&self) -> Result<Option<String>, String> {
        if self.outcome != "FAIL" || !matches!(self.mode.as_str(), "verify" | "replay" | "chaos") {
            return Ok(None);
        }
        if self.attempts.is_empty() {
            return Err("result row contains no attempt".into());
        }

        // Decide whether this is the narrow typed no-result case before
        // applying its invariants. Ordinary divergence rows legitimately carry
        // coordinates and must continue through the canonical comparison path.
        let mut reports = Vec::new();
        for (index, attempt) in self.attempts.iter().enumerate() {
            let report_text = attempt
                .get("verification_report")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| {
                    format!("attempt {} has no embedded verification report", index + 1)
                })?;
            let recorded_sha = attempt
                .get("verification_report_sha256")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| {
                    format!("attempt {} has no verification-report identity", index + 1)
                })?;
            let actual_sha = format!("{:x}", Sha256::digest(report_text.as_bytes()));
            if recorded_sha != actual_sha {
                return Err(format!(
                    "attempt {} verification-report identity does not match its embedded report",
                    index + 1
                ));
            }
            let raw: JsonValue = serde_json::from_str(report_text).map_err(|error| {
                format!(
                    "attempt {} has an unreadable verification report: {error}",
                    index + 1
                )
            })?;
            if raw.get("verdict").and_then(JsonValue::as_str) != Some("no_result") {
                return Ok(None);
            }
            let report = canonical_verdict::VerificationReport::from_current_json_slice(
                report_text.as_bytes(),
            )
            .map_err(|error| format!("attempt {} {error}", index + 1))?;
            reports.push((index, attempt, report));
        }

        if self.first_divergent_scheduler_turn.is_some()
            || self.first_divergent_virtual_nanoseconds.is_some()
            || self.first_divergent_record.is_some()
            || self.first_divergent_syscall.is_some()
        {
            return Err("no_result row carries a divergence coordinate".into());
        }

        let mut reasons = Vec::new();
        for (index, attempt, report) in reports {
            let attempt_outcome = attempt
                .get("outcome")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| format!("attempt {} has no outcome", index + 1))?;
            if attempt_outcome != "FAIL" {
                return Err(format!(
                    "attempt {} outcome {attempt_outcome} does not agree with FAIL/no_result",
                    index + 1
                ));
            }
            if attempt.get("timed_out").and_then(JsonValue::as_bool) != Some(false) {
                return Err(format!(
                    "attempt {} is timed out or omitted its timeout state",
                    index + 1
                ));
            }
            if attempt
                .get("status")
                .and_then(JsonValue::as_i64)
                .is_none_or(|status| status == 0)
            {
                return Err(format!(
                    "attempt {} has no nonzero wrapper exit status",
                    index + 1
                ));
            }
            if attempt.get("signal").is_none_or(|value| !value.is_null()) {
                return Err(format!(
                    "attempt {} has a signal beside its wrapper exit status",
                    index + 1
                ));
            }
            if attempt
                .get("error_kind")
                .is_none_or(|value| !value.is_null())
            {
                return Err(format!(
                    "attempt {} has an error classification beside FAIL/no_result",
                    index + 1
                ));
            }
            if [
                "first_divergent_scheduler_turn",
                "first_divergent_virtual_nanoseconds",
                "first_divergent_record",
                "first_divergent_syscall",
                "first_divergent_left_message",
                "first_divergent_right_message",
            ]
            .iter()
            .any(|field| attempt.get(field).is_some_and(|value| !value.is_null()))
            {
                return Err(format!(
                    "attempt {} carries divergence evidence beside no_result",
                    index + 1
                ));
            }
            if report.verified
                || report.bitwise_parity
                || report.comparison.is_some()
                || report.compared_log_messages.is_some()
                || report.infrastructure_error.is_some()
                || report.dbt_counted_branches.is_some()
                || report.first_divergent_scheduler_turn.is_some()
                || report.first_divergent_virtual_nanoseconds.is_some()
                || report.first_divergent_record.is_some()
                || report.first_divergent_syscall.is_some()
                || report.first_divergent_left_message.is_some()
                || report.first_divergent_right_message.is_some()
                || report
                    .runtime
                    .as_ref()
                    .is_some_and(|runtime| runtime.run1.is_none() || runtime.run2.is_some())
            {
                return Err(format!(
                    "attempt {} has a contradictory no_result verification report",
                    index + 1
                ));
            }
            let reason = match report.no_result_reason.as_ref() {
                Some(canonical_verdict::NoResultReason::FirstRunRejected {
                    exit_code,
                    signal,
                    ..
                }) => {
                    if exit_code.is_none() == signal.is_none()
                        || report.guest_exit_code != *exit_code
                        || report.guest_signal != *signal
                    {
                        return Err(format!(
                            "attempt {} first-run disposition does not match its no_result reason",
                            index + 1
                        ));
                    }
                    report.no_result_reason.as_ref().unwrap()
                }
                Some(canonical_verdict::NoResultReason::NotRun) => {
                    return Err(format!(
                        "attempt {} did not complete its first run",
                        index + 1
                    ));
                }
                Some(canonical_verdict::NoResultReason::ComparisonRefused { detail }) => {
                    return Err(format!(
                        "attempt {} comparison was refused: {detail}",
                        index + 1
                    ));
                }
                None => {
                    reasons.push("explicitly unspecified no-result cause".into());
                    continue;
                }
            };
            reasons.push(
                serde_json::to_string(reason)
                    .map_err(|error| format!("cannot encode no_result reason: {error}"))?,
            );
        }
        Ok(Some(reasons.join(", ")))
    }

    fn evidence_identity(&self) -> Result<String, String> {
        let evidence = serde_json::json!({
            "run_id": self.run_id,
            "attempt": self.attempt,
            "hermit_sha": self.hermit_sha,
            "binary_sha256": self.binary_sha256,
            "test_sha256": self.test_sha256,
            "test": self.test,
            "category": self.category,
            "lane": self.lane,
            "mode": self.mode,
            "backend": self.backend,
            "classification": self.classification,
            "outcome": self.outcome,
            "timeout_seconds": self.timeout_seconds,
            "log_level": self.log_level,
            "effective_args": self.effective_args,
            "argv": self.argv,
            "guest_argv": self.guest_argv,
            "env": self.env,
            "cwd": self.cwd,
            "shell_command": self.shell_command,
            "relaxations": self.relaxations,
            "attempts": self.attempts,
        });
        let encoded = serde_json::to_vec(&evidence)
            .map_err(|error| format!("cannot encode result evidence: {error}"))?;
        Ok(format!("{:x}", Sha256::digest(encoded)))
    }

    fn require_canonical_pass_evidence(&self) -> Result<(), String> {
        if self.outcome != "PASS" || !matches!(self.mode.as_str(), "verify" | "replay" | "chaos") {
            return Ok(());
        }
        for (index, attempt) in self.attempts.iter().enumerate() {
            let report = attempt
                .get("verification_report")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| {
                    format!("attempt {} has no embedded verification report", index + 1)
                })?;
            let recorded_sha = attempt
                .get("verification_report_sha256")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| {
                    format!("attempt {} has no verification-report identity", index + 1)
                })?;
            let actual_sha = format!("{:x}", Sha256::digest(report.as_bytes()));
            if recorded_sha != actual_sha {
                return Err(format!(
                    "attempt {} verification-report identity does not match its embedded report",
                    index + 1
                ));
            }
            let report: canonical_verdict::VerificationReport = serde_json::from_str(report)
                .map_err(|error| {
                    format!(
                        "attempt {} has an incomplete verification report: {error}",
                        index + 1
                    )
                })?;
            report.require_canonical_match().map_err(|error| {
                format!(
                    "attempt {} cannot support a green result: {error}",
                    index + 1
                )
            })?;
        }
        Ok(())
    }

    /// Require the exact `BitwiseInfoV1` comparison recorded by validate,
    /// whether it matched or diverged. A FAIL is useful evidence only when the
    /// strict comparison actually ran; a red produced by a weaker comparison
    /// would lower the standard just as surely as admitting its green.
    fn bitwise_info_comparison(&self) -> Result<(BTreeSet<u64>, BTreeSet<u64>), String> {
        self.require_provenance()?;
        if !matches!(self.mode.as_str(), "verify" | "replay" | "chaos") {
            return Err(format!(
                "mode {} does not produce a two-run INFO comparison",
                self.mode
            ));
        }
        if !matches!(self.outcome.as_str(), "PASS" | "FAIL") {
            return Err(format!(
                "outcome {} is not a completed comparison",
                self.outcome
            ));
        }
        // `run --verify` and chaos compare independent executions and therefore
        // require virtual time. `record start --verify` compares one recording
        // with its replay; that path deliberately leaves time real and reports
        // `virtualize_time: false`. Require the producer's exact policy for each
        // mode instead of either weakening the field or rejecting every replay
        // result the selected plan can produce.
        let expected_virtualize_time = match self.mode.as_str() {
            "replay" => false,
            "verify" | "chaos" => true,
            _ => unreachable!("comparison-producing modes checked above"),
        };
        let mut left_counts = BTreeSet::new();
        let mut right_counts = BTreeSet::new();
        for (index, attempt) in self.attempts.iter().enumerate() {
            let report_text = attempt
                .get("verification_report")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| {
                    format!("attempt {} has no embedded verification report", index + 1)
                })?;
            let recorded_sha = attempt
                .get("verification_report_sha256")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| {
                    format!("attempt {} has no verification-report identity", index + 1)
                })?;
            let actual_sha = format!("{:x}", Sha256::digest(report_text.as_bytes()));
            if recorded_sha != actual_sha {
                return Err(format!(
                    "attempt {} verification-report identity does not match its embedded report",
                    index + 1
                ));
            }
            let raw: JsonValue = serde_json::from_str(report_text).map_err(|error| {
                format!(
                    "attempt {} has an unreadable verification report: {error}",
                    index + 1
                )
            })?;
            let comparison = raw
                .get("comparison")
                .ok_or_else(|| format!("attempt {} has no comparison", index + 1))?;
            let counts = raw
                .get("compared_log_messages")
                .ok_or_else(|| format!("attempt {} has no INFO-message counts", index + 1))?;
            let left = counts
                .get("left")
                .and_then(JsonValue::as_u64)
                .ok_or_else(|| format!("attempt {} has no left INFO-message count", index + 1))?;
            let right = counts
                .get("right")
                .and_then(JsonValue::as_u64)
                .ok_or_else(|| format!("attempt {} has no right INFO-message count", index + 1))?;
            let exact_bitwise_info = comparison.get("display_name").and_then(JsonValue::as_str)
                == Some("BitwiseInfoV1")
                && comparison.get("strictness").and_then(JsonValue::as_str) == Some("canonical")
                && comparison.get("compare_logs").and_then(JsonValue::as_bool) == Some(true)
                && comparison
                    .get("compare_io_buffers")
                    .and_then(JsonValue::as_bool)
                    == Some(true)
                && comparison.get("log_scope").and_then(JsonValue::as_str) == Some("info")
                && comparison.get("strip_lines").and_then(JsonValue::as_bool) == Some(false)
                && comparison
                    .get("canonicalize_addresses")
                    .and_then(JsonValue::as_bool)
                    == Some(true)
                && comparison.get("canonicalizations")
                    == Some(&serde_json::json!([
                        "host-address-to-first-appearance-ordinal/v1"
                    ]))
                && comparison.get("full_trace").and_then(JsonValue::as_bool) == Some(true)
                && comparison
                    .get("exact_remainder")
                    .and_then(JsonValue::as_bool)
                    == Some(true)
                && comparison.get("ignore_lines").and_then(JsonValue::as_bool) == Some(false)
                && comparison.get("skip_commit").and_then(JsonValue::as_bool) == Some(false)
                && comparison.get("skip_detlog").and_then(JsonValue::as_bool) == Some(false)
                && comparison.get("stripped_prefixes")
                    == Some(&serde_json::json!(["real-wall-clock-prefix/v1"]))
                && comparison
                    .get("virtualize_time")
                    .and_then(JsonValue::as_bool)
                    == Some(expected_virtualize_time);
            if !exact_bitwise_info {
                return Err(format!(
                    "attempt {} did not use the exact BitwiseInfoV1 INFO comparison",
                    index + 1
                ));
            }
            let report = canonical_verdict::VerificationReport::from_current_json_slice(
                report_text.as_bytes(),
            )
            .map_err(|error| format!("attempt {} {error}", index + 1))?;
            report.require_canonical_comparison().map_err(|error| {
                format!(
                    "attempt {} cannot support a scorecard result: {error}",
                    index + 1
                )
            })?;
            let recorded_coordinates = DivergenceCoordinates::from_row(self);
            let report_coordinates = DivergenceCoordinates {
                scheduler_turn: report.first_divergent_scheduler_turn,
                virtual_nanoseconds: report.first_divergent_virtual_nanoseconds,
                record: report.first_divergent_record,
                syscall: report.first_divergent_syscall,
            };
            if recorded_coordinates != report_coordinates {
                return Err(format!(
                    "attempt {} top-level divergence coordinates do not match the embedded verification report",
                    index + 1
                ));
            }
            match self.outcome.as_str() {
                "PASS" => report.require_canonical_match().map_err(|error| {
                    format!(
                        "attempt {} cannot support a green result: {error}",
                        index + 1
                    )
                })?,
                "FAIL"
                    if report.verdict == canonical_verdict::Verdict::Diverged
                        && !report.verified
                        && !report.bitwise_parity => {}
                "FAIL" => {
                    return Err(format!(
                        "attempt {} FAIL is not a canonical divergence: verified={} verdict={} bitwise_parity={}",
                        index + 1,
                        report.verified,
                        report.verdict,
                        report.bitwise_parity
                    ));
                }
                _ => unreachable!("outcome checked above"),
            }
            left_counts.insert(left);
            right_counts.insert(right);
        }
        if left_counts.is_empty() {
            Err("result row contains no comparison attempt".into())
        } else {
            Ok((left_counts, right_counts))
        }
    }

    /// Reduce one cell's sibling attempts to one scorecard result. A canonical
    /// divergence is sticky; otherwise one unavailable or weaker sibling makes
    /// the whole row unavailable because the scorecard stores cell results,
    /// not independently selectable seed results.
    fn comparison_evidence(&self) -> Result<ValidateRowEvidence, String> {
        self.require_provenance()?;
        self.validate_recorded_classification()?;
        let no_verdict_result = self.no_verdict_result();
        let mut left_info_messages = BTreeSet::new();
        let mut right_info_messages = BTreeSet::new();
        let mut divergence_positions = Vec::new();
        let mut saw_canonical_match = false;
        let mut saw_no_result = false;
        let mut saw_not_run = false;
        let mut unavailable = None;

        for (index, attempt) in self.attempts.iter().enumerate() {
            let Some(report_text) = attempt
                .get("verification_report")
                .and_then(JsonValue::as_str)
            else {
                if attempt
                    .get("verification_report_sha256")
                    .is_some_and(|value| !value.is_null())
                {
                    return Err(format!(
                        "attempt {} has no embedded verification report but records its identity",
                        index + 1
                    ));
                }
                let status = attempt.get("status").and_then(JsonValue::as_i64);
                let signal = attempt.get("signal").and_then(JsonValue::as_i64);
                let disposition = matches!((status, signal), (Some(status), None) if status != 0)
                    || matches!((status, signal), (None, Some(_)));
                if attempt.get("outcome").and_then(JsonValue::as_str) != Some("ERROR")
                    || attempt.get("timed_out").and_then(JsonValue::as_bool) != Some(true)
                    || attempt
                        .get("error_kind")
                        .and_then(JsonValue::as_str)
                        .is_none_or(str::is_empty)
                    || !disposition
                {
                    return Err(format!(
                        "attempt {} has no embedded verification report and no complete timeout disposition",
                        index + 1
                    ));
                }
                unavailable.get_or_insert_with(|| {
                    format!(
                        "attempt {} timed out before emitting a verification report",
                        index + 1
                    )
                });
                continue;
            };
            let recorded_sha = attempt
                .get("verification_report_sha256")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| {
                    format!("attempt {} has no verification-report identity", index + 1)
                })?;
            if recorded_sha != format!("{:x}", Sha256::digest(report_text.as_bytes())) {
                return Err(format!(
                    "attempt {} verification-report identity does not match its embedded report",
                    index + 1
                ));
            }
            let raw: JsonValue = serde_json::from_str(report_text).map_err(|error| {
                format!(
                    "attempt {} has an unreadable verification report: {error}",
                    index + 1
                )
            })?;
            let report = canonical_verdict::VerificationReport::from_current_json_slice(
                report_text.as_bytes(),
            )
            .map_err(|error| format!("attempt {} {error}", index + 1))?;

            if matches!(
                report.verdict,
                canonical_verdict::Verdict::Matched | canonical_verdict::Verdict::Diverged
            ) {
                let comparison = raw
                    .get("comparison")
                    .and_then(JsonValue::as_object)
                    .ok_or_else(|| format!("attempt {} has no comparison object", index + 1))?;
                for field in [
                    "display_name",
                    "strictness",
                    "compare_logs",
                    "compare_io_buffers",
                    "log_scope",
                    "record_envelope",
                    "virtualize_time",
                    "strip_lines",
                    "canonicalize_addresses",
                    "canonicalizations",
                    "full_trace",
                    "exact_remainder",
                    "ignore_lines",
                    "skip_commit",
                    "skip_detlog",
                    "stripped_prefixes",
                ] {
                    if !comparison.contains_key(field) {
                        return Err(format!(
                            "attempt {} comparison omitted `{field}`",
                            index + 1
                        ));
                    }
                }
                let counts = raw
                    .get("compared_log_messages")
                    .and_then(JsonValue::as_object)
                    .ok_or_else(|| {
                        format!("attempt {} has no left INFO-message count", index + 1)
                    })?;
                for field in ["left", "right"] {
                    if counts.get(field).and_then(JsonValue::as_u64).is_none() {
                        return Err(format!(
                            "attempt {} has no {field} INFO-message count",
                            index + 1
                        ));
                    }
                }
            }

            match report.verdict {
                canonical_verdict::Verdict::Matched | canonical_verdict::Verdict::Diverged => {
                    match report.verdict {
                        canonical_verdict::Verdict::Matched
                            if report.verified && report.bitwise_parity => {}
                        canonical_verdict::Verdict::Matched => {
                            return Err(format!(
                                "attempt {} typed match report is internally inconsistent",
                                index + 1
                            ));
                        }
                        canonical_verdict::Verdict::Diverged
                            if !report.verified && !report.bitwise_parity => {}
                        canonical_verdict::Verdict::Diverged => {
                            return Err(format!(
                                "attempt {} typed divergence report is internally inconsistent",
                                index + 1
                            ));
                        }
                        _ => unreachable!(),
                    }
                    let mut single = self.clone();
                    single.attempts = vec![attempt.clone()];
                    single.argv = serde_json::from_value(
                        attempt.get("argv").cloned().unwrap_or(JsonValue::Null),
                    )
                    .map_err(|error| format!("attempt {} has invalid argv: {error}", index + 1))?;
                    single.effective_args = single.argv.iter().skip(1).cloned().collect();
                    single.guest_argv = serde_json::from_value(
                        attempt
                            .get("guest_argv")
                            .cloned()
                            .unwrap_or(JsonValue::Null),
                    )
                    .map_err(|error| {
                        format!("attempt {} has invalid guest_argv: {error}", index + 1)
                    })?;
                    single.env = serde_json::from_value(
                        attempt.get("env").cloned().unwrap_or(JsonValue::Null),
                    )
                    .map_err(|error| format!("attempt {} has invalid env: {error}", index + 1))?;
                    single.cwd = attempt
                        .get("cwd")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| format!("attempt {} has invalid cwd", index + 1))?
                        .into();
                    single.shell_command = attempt
                        .get("shell_command")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| format!("attempt {} has invalid shell_command", index + 1))?
                        .into();
                    single.outcome = if report.verdict == canonical_verdict::Verdict::Matched {
                        "PASS".into()
                    } else {
                        "FAIL".into()
                    };
                    single.first_divergent_scheduler_turn = report.first_divergent_scheduler_turn;
                    single.first_divergent_virtual_nanoseconds =
                        report.first_divergent_virtual_nanoseconds;
                    single.first_divergent_record = report.first_divergent_record;
                    single.first_divergent_syscall = report.first_divergent_syscall;
                    match single.bitwise_info_comparison() {
                        Ok((left, right)) => {
                            left_info_messages.extend(left);
                            right_info_messages.extend(right);
                            if report.verdict == canonical_verdict::Verdict::Matched {
                                saw_canonical_match = true;
                            } else {
                                divergence_positions.push(DivergenceCoordinates::from_row(&single));
                            }
                        }
                        Err(error) => {
                            unavailable.get_or_insert(error);
                        }
                    };
                }
                canonical_verdict::Verdict::NoResult => match report.no_result_reason.as_ref() {
                    Some(canonical_verdict::NoResultReason::NotRun) => {
                        saw_no_result = true;
                        if report.verified
                            || report.bitwise_parity
                            || report.comparison.is_some()
                            || report.compared_log_messages.is_some()
                            || report.infrastructure_error.is_some()
                            || report.dbt_counted_branches.is_some()
                            || report.first_divergent_scheduler_turn.is_some()
                            || report.first_divergent_virtual_nanoseconds.is_some()
                            || report.first_divergent_record.is_some()
                            || report.first_divergent_syscall.is_some()
                            || report.first_divergent_left_message.is_some()
                            || report.first_divergent_right_message.is_some()
                            || report.runtime.is_some()
                            || report.guest_exit_code.is_some()
                            || report.guest_signal.is_some()
                            || [
                                "first_divergent_scheduler_turn",
                                "first_divergent_virtual_nanoseconds",
                                "first_divergent_record",
                                "first_divergent_syscall",
                                "first_divergent_left_message",
                                "first_divergent_right_message",
                            ]
                            .iter()
                            .any(|field| attempt.get(field).is_some_and(|value| !value.is_null()))
                        {
                            return Err(format!(
                                "attempt {} has a contradictory no_result verification report",
                                index + 1
                            ));
                        }
                        let status = attempt.get("status").and_then(JsonValue::as_i64);
                        let signal = attempt.get("signal").and_then(JsonValue::as_i64);
                        let timed_out = attempt
                            .get("timed_out")
                            .and_then(JsonValue::as_bool)
                            .ok_or_else(|| {
                                format!("attempt {} omitted its timeout state", index + 1)
                            })?;
                        let disposition = matches!((status, signal), (Some(status), None) if status != 0)
                            || matches!((status, signal), (None, Some(_)));
                        let no_process_timeout = is_prelaunch_timeout_disposition(
                            timed_out,
                            status,
                            signal,
                            attempt.get("error_kind").and_then(JsonValue::as_str),
                        );
                        if attempt.get("outcome").and_then(JsonValue::as_str) != Some("ERROR")
                            || attempt
                                .get("error_kind")
                                .and_then(JsonValue::as_str)
                                .is_none_or(str::is_empty)
                            || !(disposition || no_process_timeout)
                        {
                            return Err(format!(
                                "attempt {} NotRun report has no complete process or pre-launch timeout disposition",
                                index + 1
                            ));
                        }
                        if timed_out {
                            saw_not_run = true;
                            unavailable.get_or_insert_with(|| {
                                format!(
                                    "NO_RESULT: attempt {} did not complete its first run",
                                    index + 1
                                )
                            });
                        } else {
                            unavailable.get_or_insert_with(|| {
                                format!(
                                    "NO_RESULT: attempt {} did not produce a comparison (status={status:?}, signal={signal:?})",
                                    index + 1
                                )
                            });
                        }
                    }
                    Some(canonical_verdict::NoResultReason::FirstRunRejected { .. }) => {
                        saw_no_result = true;
                        let mut single = self.clone();
                        single.outcome = "FAIL".into();
                        single.first_divergent_scheduler_turn = None;
                        single.first_divergent_virtual_nanoseconds = None;
                        single.first_divergent_record = None;
                        single.first_divergent_syscall = None;
                        single.attempts = vec![attempt.clone()];
                        let reason = single
                            .typed_no_result_reason()?
                            .ok_or("FirstRunRejected did not classify as no_result")?;
                        unavailable.get_or_insert(format!("NO_RESULT: {reason}"));
                    }
                    Some(canonical_verdict::NoResultReason::ComparisonRefused { detail }) => {
                        saw_no_result = true;
                        unavailable.get_or_insert(format!(
                            "NO_RESULT: attempt {} comparison was refused: {detail}",
                            index + 1
                        ));
                    }
                    None => {
                        saw_no_result = true;
                        unavailable.get_or_insert_with(|| {
                            format!(
                                "NO_RESULT: attempt {} recorded no specific cause",
                                index + 1
                            )
                        });
                    }
                },
                canonical_verdict::Verdict::InfrastructureError => {
                    let reason = match report.infrastructure_error {
                        Some(canonical_verdict::InfrastructureError::SkidOvershoot { count }) => {
                            format!(
                                "attempt {} recorded {count} HERMIT_SKID_OVERSHOOT report(s)",
                                index + 1
                            )
                        }
                        None => format!("attempt {} recorded an infrastructure error", index + 1),
                    };
                    unavailable.get_or_insert(reason);
                }
            }
        }

        if !divergence_positions.is_empty() {
            let aggregate = DivergenceCoordinates {
                scheduler_turn: divergence_positions
                    .iter()
                    .find_map(|position| position.scheduler_turn),
                virtual_nanoseconds: divergence_positions
                    .iter()
                    .find_map(|position| position.virtual_nanoseconds),
                record: divergence_positions
                    .iter()
                    .find_map(|position| position.record),
                syscall: divergence_positions
                    .iter()
                    .find_map(|position| position.syscall),
            };
            if DivergenceCoordinates::from_row(self) != aggregate {
                return Err(
                    "top-level divergence coordinates do not match the canonical divergence attempts"
                        .into(),
                );
            }
            return Ok(ValidateRowEvidence::Diverged {
                left_info_messages,
                right_info_messages,
            });
        }
        if saw_no_result && !DivergenceCoordinates::from_row(self).is_empty() {
            return Err("no_result row carries a divergence coordinate".into());
        }
        if saw_not_run && !saw_canonical_match {
            return Ok(ValidateRowEvidence::NotRun {
                reason: unavailable.clone().unwrap_or_else(|| {
                    "NO_RESULT: no attempt completed a canonical comparison".into()
                }),
                result: no_verdict_result,
            });
        }
        if let Some(reason) = unavailable {
            return Ok(ValidateRowEvidence::Unavailable {
                reason,
                result: no_verdict_result,
            });
        }
        if !DivergenceCoordinates::from_row(self).is_empty() {
            return Err("matched row carries a divergence coordinate".into());
        }
        if !saw_canonical_match {
            return Ok(ValidateRowEvidence::Unavailable {
                reason: "cell emitted no typed verification report".into(),
                result: no_verdict_result,
            });
        }
        if self.outcome != "PASS" {
            return Ok(ValidateRowEvidence::Unavailable {
                reason: format!(
                    "cell outcome was {} despite matched comparison evidence",
                    self.outcome
                ),
                result: no_verdict_result,
            });
        }
        Ok(ValidateRowEvidence::Matched {
            left_info_messages,
            right_info_messages,
        })
    }
}

struct Derived {
    population: BTreeSet<CellId>,
    enabled: BTreeSet<CellId>,
    ci_disabled_reasons: BTreeMap<CellId, CiDisabledReasonData>,
    not_applicable_reasons: BTreeMap<CellId, String>,
    selected: BTreeSet<CellId>,
    green: BTreeSet<CellId>,
    selected_custom: BTreeSet<CellId>,
}

fn retained_import_cells(derived: &Derived) -> BTreeSet<CellId> {
    derived.enabled.clone()
}

#[derive(Clone)]
struct ResultCandidate {
    evidence_identity: String,
    path: PathBuf,
    row: ResultRow,
}

struct RetainedCellResults {
    id: CellId,
    hermit_sha: String,
    detcore_tree: String,
    depth: BTreeMap<String, SourceDepth>,
    candidates: Vec<ResultCandidate>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct DivergenceCoordinates {
    scheduler_turn: Option<u64>,
    virtual_nanoseconds: Option<u64>,
    record: Option<u64>,
    syscall: Option<u64>,
}

impl DivergenceCoordinates {
    fn from_row(row: &ResultRow) -> Self {
        Self {
            scheduler_turn: row.first_divergent_scheduler_turn,
            virtual_nanoseconds: row.first_divergent_virtual_nanoseconds,
            record: row.first_divergent_record,
            syscall: row.first_divergent_syscall,
        }
    }

    fn is_empty(self) -> bool {
        self.scheduler_turn.is_none()
            && self.virtual_nanoseconds.is_none()
            && self.record.is_none()
            && self.syscall.is_none()
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum RetainedComparisonState {
    Fresh,
    Drifted,
    Wrong,
    Uncheckable,
}

impl RetainedComparisonState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Fresh => "FRESH",
            Self::Drifted => "DRIFTED",
            Self::Wrong => "WRONG",
            Self::Uncheckable => "UNCHECKABLE",
        }
    }
}

struct RetainedDecision {
    state: RetainedComparisonState,
    import: ImportEvidence,
    retained_coordinates: BTreeSet<DivergenceCoordinates>,
    current_coordinates: BTreeSet<DivergenceCoordinates>,
    reason: String,
}

enum ImportEvidence {
    Retained {
        results: Box<RetainedCellResults>,
        store_positions: bool,
    },
    None,
}

#[derive(Clone)]
struct CurrentPressureResult {
    summary: PressureSummary,
    result: ObservedResult,
    coordinates: DivergenceCoordinates,
    missing_retained_logs: bool,
}

struct CurrentPressureEvidence {
    results: BTreeMap<CellId, Vec<CurrentPressureResult>>,
    uncheckable: BTreeMap<CellId, Vec<String>>,
}

const MISSING_RETAINED_VERIFY_LOGS: &str =
    "terminal verify result must retain exactly one nonempty run1 log and one nonempty run2 log";

fn parse_update_observations_args<I>(mut args: I) -> Result<(Vec<PathBuf>, bool), String>
where
    I: Iterator<Item = String>,
{
    let mut summaries = Vec::new();
    let mut retained = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--summary" => summaries.push(PathBuf::from(
                args.next().ok_or("--summary requires a file")?,
            )),
            "--retained" if !retained => retained = true,
            "--retained" => {
                return Err("update-observations accepts --retained only once".into());
            }
            _ => {
                return Err(format!(
                    "unknown update-observations option `{arg}`\n\n{USAGE}"
                ));
            }
        }
    }
    if summaries.is_empty() {
        return Err("update-observations requires --summary FILE".into());
    }
    let distinct = summaries.iter().collect::<BTreeSet<_>>();
    if distinct.len() != summaries.len() {
        return Err("update-observations refuses a duplicate --summary FILE".into());
    }
    Ok((summaries, retained))
}

fn set_project_and_observe_arg(
    slot: &mut Option<String>,
    option: &str,
    value: Option<String>,
) -> Result<(), String> {
    if slot.is_some() {
        return Err(format!(
            "project-and-observe-results accepts {option} only once\n\n{PROJECT_AND_OBSERVE_USAGE}"
        ));
    }
    *slot = Some(value.ok_or_else(|| {
        format!(
            "project-and-observe-results {option} requires a value\n\n{PROJECT_AND_OBSERVE_USAGE}"
        )
    })?);
    Ok(())
}

fn parse_project_and_observe_args<I>(mut args: I) -> Result<ProjectAndObserveArgs, String>
where
    I: Iterator<Item = String>,
{
    let mut snapshot = None;
    let mut snapshot_sha256 = None;
    let mut results = None;
    let mut expected_head = None;
    let mut refreshed_at = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--snapshot" => set_project_and_observe_arg(&mut snapshot, "--snapshot", args.next())?,
            "--snapshot-sha256" => {
                set_project_and_observe_arg(&mut snapshot_sha256, "--snapshot-sha256", args.next())?
            }
            "--results" => set_project_and_observe_arg(&mut results, "--results", args.next())?,
            "--expected-head" => {
                set_project_and_observe_arg(&mut expected_head, "--expected-head", args.next())?
            }
            "--refreshed-at" => {
                set_project_and_observe_arg(&mut refreshed_at, "--refreshed-at", args.next())?
            }
            _ => {
                return Err(format!(
                    "unknown project-and-observe-results option `{arg}`\n\n{PROJECT_AND_OBSERVE_USAGE}"
                ));
            }
        }
    }
    let required = |value: Option<String>, option: &str| {
        value.ok_or_else(|| {
            format!("project-and-observe-results requires {option}\n\n{PROJECT_AND_OBSERVE_USAGE}")
        })
    };
    Ok(ProjectAndObserveArgs {
        snapshot: PathBuf::from(required(snapshot, "--snapshot FILE")?),
        snapshot_sha256: required(snapshot_sha256, "--snapshot-sha256 HEX")?,
        results: PathBuf::from(required(results, "--results DIR")?),
        expected_head: required(expected_head, "--expected-head SHA")?,
        refreshed_at: required(refreshed_at, "--refreshed-at STAMP")?,
    })
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("compatibility scorecard: {message}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1);
    let Some(command) = args.next() else {
        return Err(format!("missing command\n\n{USAGE}"));
    };
    if matches!(command.as_str(), "-h" | "--help" | "help") {
        print!("{USAGE}");
        return Ok(());
    }
    let root = repo_root()?;
    match command.as_str() {
        "show" => {
            no_more(&mut args)?;
            let derived = derive(&root)?;
            print!("{}", render_scorecard(&derived));
            if let Some(mut tracked) = load_existing(&root)? {
                refresh_measurement(&mut tracked);
                print!("{}", render_measurement_section(&tracked));
            }
            print!("{}", render_evidence_coverage(&root)?);
        }
        "check" => {
            no_more(&mut args)?;
            let derived = check_tracked_with_lock(&root)?;
            println!("{}", tracked_current_summary(&derived));
        }
        "update" => {
            let mut allow_green_removal: Option<String> = None;
            let mut allow_cell_removal = false;
            let mut args = args;
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    // TAKES THE REASON AS ITS VALUE rather than pairing a bare
                    // flag with a separate --reason. A bare flag can be used
                    // without one; this cannot, so the justification is not
                    // something the author may forget to supply.
                    "--allow-green-removal" => {
                        let reason = args.next().ok_or_else(|| {
                            "--allow-green-removal requires a reason: \
                             --allow-green-removal \"why this transition is correct\". \
                             The reason is written into ci/compat-envelope/cells.json next to \
                             each cell it moves, so the override is visible in the diff instead \
                             of living only in the shell history of whoever ran it."
                                .to_string()
                        })?;
                        if reason.trim().is_empty() || reason.starts_with("--") {
                            return Err(format!(
                                "--allow-green-removal needs a reason, got `{reason}`"
                            ));
                        }
                        allow_green_removal = Some(reason);
                    }
                    "--allow-cell-removal" => allow_cell_removal = true,
                    _ => return Err(format!("unknown update option `{arg}`\n\n{USAGE}")),
                }
            }
            update_tracked(&root, allow_green_removal.as_deref(), allow_cell_removal)?;
        }
        "update-observations" => {
            let (summaries, retained) = parse_update_observations_args(args)?;
            update_observations(&root, &summaries, retained)?;
        }
        "project-observations" => {
            let mut series_root = None;
            let mut refreshed_at = None;
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--series-root" => {
                        series_root = Some(PathBuf::from(
                            args.next().ok_or("--series-root requires a directory")?,
                        ));
                    }
                    "--refreshed-at" => {
                        refreshed_at =
                            Some(args.next().ok_or("--refreshed-at requires a timestamp")?);
                    }
                    _ => {
                        return Err(format!(
                            "unknown project-observations option `{arg}`\n\n{USAGE}"
                        ));
                    }
                }
            }
            project_observations(
                &root,
                &series_root.ok_or("project-observations requires --series-root DIR")?,
                // Supplied rather than read from the clock, so the same inputs
                // produce the same file. A refresh timestamp the tool invents is
                // a diff on every run that says nothing changed.
                &refreshed_at.ok_or("project-observations requires --refreshed-at STAMP")?,
            )?;
        }
        "project-and-observe-results" => {
            let remaining = args.collect::<Vec<_>>();
            if remaining.len() == 1 && matches!(remaining[0].as_str(), "-h" | "--help") {
                print!("{PROJECT_AND_OBSERVE_USAGE}");
                return Ok(());
            }
            let options = parse_project_and_observe_args(remaining.into_iter())?;
            project_and_observe_results(
                &root,
                &options.snapshot,
                &options.snapshot_sha256,
                &options.results,
                &options.expected_head,
                &options.refreshed_at,
            )?;
        }
        "verify-results" => {
            let mut result_root = None;
            let mut lanes = BTreeSet::from(["portable".to_string(), "privileged".to_string()]);
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--results" => {
                        result_root = Some(PathBuf::from(
                            args.next().ok_or("--results requires a directory")?,
                        ));
                    }
                    "--lanes" => {
                        lanes = args
                            .next()
                            .ok_or("--lanes requires a comma-separated value")?
                            .split(',')
                            .filter(|lane| !lane.is_empty())
                            .map(str::to_string)
                            .collect();
                        if lanes.is_empty()
                            || lanes
                                .iter()
                                .any(|lane| lane != "portable" && lane != "privileged")
                        {
                            return Err("--lanes accepts portable, privileged, or both".into());
                        }
                    }
                    _ => return Err(format!("unknown verify-results option `{arg}`\n\n{USAGE}")),
                }
            }
            let result_root = result_root.ok_or("verify-results requires --results DIR")?;
            check_tracked(&root)?;
            verify_results(&root, &result_root, &lanes)?;
        }
        "observe-results" => {
            let mut result_root = None;
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--results" => {
                        result_root = Some(PathBuf::from(
                            args.next().ok_or("--results requires a directory")?,
                        ));
                    }
                    _ => return Err(format!("unknown observe-results option `{arg}`\n\n{USAGE}")),
                }
            }
            let result_root = result_root.ok_or("observe-results requires --results DIR")?;
            observe_results(&root, &result_root)?;
        }
        "import-results" => {
            let mut result_root = None;
            let mut current_summaries = Vec::new();
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--results" => {
                        result_root = Some(PathBuf::from(
                            args.next().ok_or("--results requires a directory")?,
                        ));
                    }
                    "--current-summary" => {
                        current_summaries.push(PathBuf::from(
                            args.next().ok_or("--current-summary requires a file")?,
                        ));
                    }
                    _ => return Err(format!("unknown import-results option `{arg}`\n\n{USAGE}")),
                }
            }
            let result_root = result_root.ok_or("import-results requires --results DIR")?;
            if current_summaries.is_empty() {
                return Err("import-results requires at least one --current-summary FILE".into());
            }
            import_results(&root, &result_root, &current_summaries)?;
        }
        "self-test" => {
            no_more(&mut args)?;
            self_test()?;
        }
        "self-test-and-check" => {
            no_more(&mut args)?;
            self_test()?;
            let derived = check_tracked_with_lock(&root)?;
            println!("{}", tracked_current_summary(&derived));
        }
        _ => return Err(format!("unknown command `{command}`\n\n{USAGE}")),
    }
    Ok(())
}

fn no_more(args: &mut impl Iterator<Item = String>) -> Result<(), String> {
    match args.next() {
        Some(arg) => Err(format!("unexpected argument `{arg}`\n\n{USAGE}")),
        None => Ok(()),
    }
}

fn repo_root() -> Result<PathBuf, String> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|e| format!("cannot run git rev-parse: {e}"))?;
    if !output.status.success() {
        return Err("not inside a Git checkout".into());
    }
    let root = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    if !root.join(EXPECTED_PLAN).is_file() {
        return Err(format!("{} is not the Hermit checkout", root.display()));
    }
    Ok(root)
}

fn derive(root: &Path) -> Result<Derived, String> {
    let output = Command::new("cargo")
        .args([
            "run",
            "--quiet",
            "-p",
            "hermit-manifest-plan",
            "--",
            "--format",
            "matrix-json",
        ])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cannot run hermit-manifest-plan: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "hermit-manifest-plan failed:\n{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let rows: Vec<ManifestRow> = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("manifest-plan emitted invalid JSON: {e}"))?;
    let expected: ExpectedPlan = read_json(&root.join(EXPECTED_PLAN))?;

    let mut manifest_cells = BTreeSet::new();
    let mut population = BTreeSet::new();
    let mut enabled = BTreeSet::new();
    let mut ci_enabled = BTreeSet::new();
    let mut ci_disabled_reasons = BTreeMap::new();
    let mut not_applicable_reasons = BTreeMap::new();
    for row in rows {
        let comparable = row.mode != "custom";
        let id = CellId {
            lane: row.lane,
            category: row.bucket,
            test: row.test,
            mode: row.mode,
            backend: row.backend,
        };
        if !manifest_cells.insert(id.clone()) {
            return Err(format!(
                "manifest-plan emitted duplicate cell {}",
                display_id(&id)
            ));
        }
        // `custom` is an explicit per-test command, not a mode which applies
        // uniformly to every test/backend pair. Keep selected custom commands
        // in ordinary validation, but do not manufacture 1,680 scorecard cells
        // from combinations that have no product meaning.
        if comparable {
            population.insert(id.clone());
        }
        if row.enabled {
            if comparable {
                enabled.insert(id.clone());
            }
            if row.ci {
                if row.ci_disabled_reason.is_some() {
                    return Err(format!(
                        "CI-enabled cell carries ci_disabled_reason: {}",
                        display_id(&id)
                    ));
                }
                ci_enabled.insert(id);
            } else {
                let reason = row.ci_disabled_reason.ok_or_else(|| {
                    format!(
                        "enabled cell omitted from ordinary CI has no reason: {}",
                        display_id(&id)
                    )
                })?;
                ci_disabled_reasons.insert(id, reason);
            }
        } else {
            // A cell whose backend is not enabled for this mode is NOT
            // APPLICABLE, not failing. It must say why, and the manifest already
            // requires a reason for every disabled backend -- so an absent one
            // here means the plan emitter dropped it, which is worth refusing
            // rather than rendering as an unexplained red.
            if row.ci_disabled_reason.is_some() {
                return Err(format!(
                    "disabled cell carries ci_disabled_reason: {}",
                    display_id(&id)
                ));
            }
            let reason = row.not_applicable_reason.ok_or_else(|| {
                format!(
                    "disabled cell has no not_applicable_reason: {}",
                    display_id(&id)
                )
            })?;
            not_applicable_reasons.insert(id, reason);
        }
    }
    let selected = unique_cell_ids("expected E2E plan", expected.cells)?;
    if selected.is_empty() {
        return Err("expected E2E plan is empty".into());
    }
    for id in &selected {
        if !manifest_cells.contains(id) {
            return Err(format!(
                "expected plan names absent cell {}",
                display_id(id)
            ));
        }
        if !ci_enabled.contains(id) {
            return Err(format!(
                "expected plan names a cell not enabled for ordinary CI: {}",
                display_id(id)
            ));
        }
    }
    let (green, selected_custom) = selected_partition(&selected, &population)?;
    Ok(Derived {
        population,
        enabled,
        ci_disabled_reasons,
        not_applicable_reasons,
        selected,
        green,
        selected_custom,
    })
}

fn selected_green(selected: &BTreeSet<CellId>, population: &BTreeSet<CellId>) -> BTreeSet<CellId> {
    selected
        .iter()
        .filter(|id| population.contains(*id))
        .cloned()
        .collect()
}

fn unique_cell_ids(label: &str, rows: Vec<CellId>) -> Result<BTreeSet<CellId>, String> {
    let physical = rows.len();
    let mut unique = BTreeSet::new();
    let mut duplicates = BTreeSet::new();
    for id in rows {
        if !unique.insert(id.clone()) {
            duplicates.insert(id);
        }
    }
    if !duplicates.is_empty() {
        let names = duplicates
            .iter()
            .map(display_id)
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "{label} contains {physical} physical rows but only {} unique identities; duplicate identities: {names}",
            unique.len()
        ));
    }
    Ok(unique)
}

/// Account for every selected row without forcing nonuniform custom commands
/// into the comparable `cells.json` matrix.
fn selected_partition(
    selected: &BTreeSet<CellId>,
    population: &BTreeSet<CellId>,
) -> Result<(BTreeSet<CellId>, BTreeSet<CellId>), String> {
    let comparable = selected_green(selected, population);
    let custom: BTreeSet<_> = selected
        .iter()
        .filter(|id| id.mode == "custom")
        .cloned()
        .collect();
    if let Some(id) = custom.intersection(&comparable).next() {
        return Err(format!(
            "selected custom command also entered the comparable cells.json denominator: {}",
            display_id(id)
        ));
    }
    let accounted: BTreeSet<_> = comparable.union(&custom).cloned().collect();
    if let Some(id) = selected.difference(&accounted).next() {
        return Err(format!(
            "selected plan row is neither a comparable cells.json cell nor a custom command: {}",
            display_id(id)
        ));
    }
    Ok((comparable, custom))
}

fn tracked_current_summary(derived: &Derived) -> String {
    format!(
        "compatibility scorecard: tracked table and {} comparable cells are current; selected regression denominator {} = {} comparable + {} custom",
        derived.population.len(),
        derived.selected.len(),
        derived.green.len(),
        derived.selected_custom.len()
    )
}

fn read_json<T: for<'a> Deserialize<'a>>(path: &Path) -> Result<T, String> {
    let bytes = fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("invalid JSON in {}: {e}", path.display()))
}

fn render_scorecard(derived: &Derived) -> String {
    let mut backends: BTreeSet<&str> = derived
        .population
        .iter()
        .map(|id| id.backend.as_str())
        .collect();
    let preferred = ["ptrace", "dbt", "kvm", "sabre", "liteinst", "native"];
    let mut ordered = Vec::new();
    for backend in preferred {
        if backends.remove(backend) {
            ordered.push(backend);
        }
    }
    ordered.extend(backends);

    // This is the same derived count printed in the summary table below. Keep
    // the prose on the value rather than restating a hand-maintained snapshot:
    // the old literal still said manifest-disabled cells were red after
    // `NotApplicable` became a separate status.
    let na_total = derived
        .population
        .iter()
        .filter(|id| !derived.enabled.contains(*id))
        .count();
    let status_green_total = derived.green.len();
    let status_red_total = derived.enabled.difference(&derived.green).count();

    let mut out = format!(
        "# Compatibility scorecard\n\n\
This table is derived from the manifest, not from a separately maintained parent-workspace CSV. \
`./ci/compat-envelope/scorecard.rs check` verifies it.\n\n\
**Green** means this manifest cell is selected by full in `ci/expected-e2e-plan.json`; ordinary \
validation therefore requires it to pass. **Red** means the cell is in the manifest but is not \
selected by full. **Red does not mean failed:** a red cell may have passed, failed, produced no \
verdict, or never run. Manifest-disabled combinations are **Not applicable**; they are neither red \
nor omitted. The current generated data counts Green as **{status_green_total}** and Red as \
**{status_red_total}**. The generator classifies the current **{na_total}** manifest-disabled \
combinations as **Not applicable**.\n\n\
Every selected `verify` cell, and every seed in a selected `chaos` cell, runs the same backend \
twice. The manifest runner adds `--verify-strict` when the selected Hermit binary supports it, and \
accepts a result only when the typed report says `verified=true`, `verdict=matched`, \
`bitwise_parity=true`, `strictness=canonical`, `compare_logs=true`, a named canonical \
`record_envelope`, and both INFO-message counts are nonzero. Bare `--verify` remains a Stripped \
comparison when invoked directly and does not satisfy \
this regression plan. These same-backend results do not establish cross-backend parity.\n\n\
| Backend | Green | Red | Not applicable | Total |\n\
| --- | ---: | ---: | ---: | ---: |\n",
    );
    let mut green_total = 0usize;
    let mut total = 0usize;
    for backend in &ordered {
        let backend_total = derived
            .population
            .iter()
            .filter(|id| id.backend == *backend)
            .count();
        let backend_green = derived
            .green
            .iter()
            .filter(|id| id.backend == *backend)
            .count();
        // NOT APPLICABLE IS SUBTRACTED FROM RED, NOT ADDED TO THE TOTAL. The
        // population is unchanged; what changes is that a cell whose backend is
        // not enabled for this mode stops being counted as a failure.
        let backend_na = derived
            .population
            .iter()
            .filter(|id| id.backend == *backend && !derived.enabled.contains(*id))
            .count();
        green_total += backend_green;
        total += backend_total;
        out.push_str(&format!(
            "| `{backend}` | {backend_green} | {} | {backend_na} | {backend_total} |\n",
            backend_total - backend_green - backend_na
        ));
    }
    out.push_str(&format!(
        "| **Total** | **{green_total}** | **{}** | **{na_total}** | **{total}** |\n\n",
        total - green_total - na_total
    ));
    // DENOMINATOR PROVENANCE. Emitted from the derived population, never
    // hand-written, so it cannot go stale and cannot be forgotten.
    //
    // ⚠️ WHY THIS EXISTS. A percentage is meaningless without the population it
    // was taken over, and this table's population CHANGES: adding a backend or
    // a mode to the manifest grows it, removing one shrinks it. Both move the
    // percentage while nothing about the product moves. Worked example with
    // real numbers at the time of writing: dropping `dbt` would remove 1035
    // cells, ALL OF THEM RED, taking the total from 5520 to 4485 and RAISING
    // reported green from 5.07% to 6.24% -- a 23% relative improvement with
    // nothing improved. Restoring it later would move the number back DOWN,
    // which reads as a regression when it is a restoration of honesty.
    //
    // Recording the composition beside the number makes any such change show up
    // as a diff hunk in this generated file, so a new percentage cannot be
    // quoted without the reason it moved sitting next to it.
    let percent = if total == 0 {
        0.0
    } else {
        100.0 * green_total as f64 / total as f64
    };
    let modes: BTreeSet<&str> = derived
        .population
        .iter()
        .map(|id| id.mode.as_str())
        .collect();
    out.push_str(&format!(
        "## Denominator, and why the percentage is not comparable across changes to it\n\n\
Green is **{green_total} of {total}**, which is **{percent:.2}%** — over THIS population and no \
other. The population is every combination the manifest declares, and it is composed of:\n\n\
- backends: {}\n\
- modes: {}\n\n\
⚠️ **{na} of those {total} cells are NOT APPLICABLE** — their backend is not enabled for their \
mode, so they were never asked to run and cannot pass or fail. Over the {applicable} cells that \
CAN run, green is **{applicable_percent:.2}%**.\n\n\
⚠️ **DO NOT QUOTE THAT SECOND FIGURE AS PROGRESS.** It is the same {green_total} green cells \
measured against a smaller denominator. Nothing was fixed to produce it; it is what the first \
figure always meant once the cells that cannot run are excluded. Quote both or neither, and never \
compare one against the other as though something moved.\n\n\
⚠️ **Adding or removing a backend or mode changes this denominator and therefore the percentage, \
without anything about the product changing.** Removing a backend whose cells are mostly red \
RAISES the reported figure; adding honest red cells LOWERS it. Neither is progress. Before \
comparing this percentage against an earlier one, diff the two lists above: if they differ, the \
numbers are not comparable and the difference is not a result.\n\n",
        ordered
            .iter()
            .map(|b| format!("`{b}`"))
            .collect::<Vec<_>>()
            .join(", "),
        modes
            .iter()
            .map(|m| format!("`{m}`"))
            .collect::<Vec<_>>()
            .join(", "),
        na = na_total,
        applicable = total - na_total,
        applicable_percent = if total == na_total {
            0.0
        } else {
            100.0 * green_total as f64 / (total - na_total) as f64
        },
    ));
    out.push_str(
        "The mode view makes the current order of work explicit: expand `verify` first, then \
`replay`, then `chaos`. Each backend cell is `green / total`; an em dash means that mode does \
not exist for that backend. The summary columns use the same Green, Red, and Not applicable \
statuses as the table above.\n\n| Mode",
    );
    for backend in &ordered {
        out.push_str(&format!(" | `{backend}`"));
    }
    out.push_str(" | Green | Red | Not applicable | Total |\n| ---");
    for _ in &ordered {
        out.push_str(" | ---:");
    }
    out.push_str(" | ---: | ---: | ---: | ---: |\n");
    for mode in ["verify", "replay", "chaos", "naked"] {
        let mode_total = derived
            .population
            .iter()
            .filter(|id| id.mode == mode)
            .count();
        let mode_green = derived.green.iter().filter(|id| id.mode == mode).count();
        let mode_na = derived
            .population
            .iter()
            .filter(|id| id.mode == mode && !derived.enabled.contains(*id))
            .count();
        out.push_str(&format!("| `{mode}`"));
        for backend in &ordered {
            let cell_total = derived
                .population
                .iter()
                .filter(|id| id.mode == mode && id.backend == *backend)
                .count();
            if cell_total == 0 {
                out.push_str(" | —");
            } else {
                let cell_green = derived
                    .green
                    .iter()
                    .filter(|id| id.mode == mode && id.backend == *backend)
                    .count();
                out.push_str(&format!(" | {cell_green} / {cell_total}"));
            }
        }
        out.push_str(&format!(
            " | {mode_green} | {} | {mode_na} | {mode_total} |\n",
            mode_total - mode_green - mode_na
        ));
    }
    out.push_str(&format!(
        "| **Total** | | | | | | | **{green_total}** | **{}** | **{na_total}** | **{total}** |\n\n",
        total - green_total - na_total
    ));
    out.push_str(
        "## Cross-backend parity\n\n\
The manifest-backed scorecard does not yet contain cross-backend parity cells. In particular, \
a DBT, KVM, SaBRe, or LiteInst `verify` cell compares that backend with itself, not with ptrace. \
Standalone backend gates exercise selected comparisons, but their results are not counted here. \
Until a cell actually compares a fresh ptrace log with the corresponding backend log, this table \
reports no cross-backend parity number.\n\n\
## Ptrace by manifest category\n\n\
This view uses the same Basic Sanity Milestone 1 contracts as the tables above, but makes the ptrace \
workload mix visible. Each entry is `green / total`; `custom` commands are not part of this \
denominator.\n\n\
| Manifest category | Verify | Replay | Chaos | Green | Total |\n\
| --- | ---: | ---: | ---: | ---: | ---: |\n",
    );
    let categories: BTreeSet<&str> = derived
        .population
        .iter()
        .filter(|id| id.backend == "ptrace")
        .map(|id| id.category.as_str())
        .collect();
    for category in categories {
        let category_cells: Vec<_> = derived
            .population
            .iter()
            .filter(|id| id.backend == "ptrace" && id.category == category)
            .collect();
        let category_green = category_cells
            .iter()
            .filter(|id| derived.green.contains(**id))
            .count();
        out.push_str(&format!("| `{category}`"));
        for mode in ["verify", "replay", "chaos"] {
            let mode_total = category_cells.iter().filter(|id| id.mode == mode).count();
            let mode_green = category_cells
                .iter()
                .filter(|id| id.mode == mode && derived.green.contains(**id))
                .count();
            out.push_str(&format!(" | {mode_green} / {mode_total}"));
        }
        out.push_str(&format!(
            " | {category_green} | {} |\n",
            category_cells.len()
        ));
    }
    out.push('\n');
    let chaos = derived
        .selected
        .iter()
        .filter(|id| id.mode == "chaos")
        .count();
    let custom = derived.selected_custom.len();
    out.push_str(&format!(
        "Ordinary full validation executes {} selected regression cells: the {green_total} green \
compatibility cells above (including {chaos} chaos-mode race-exposure checks), and {custom} \
explicit custom commands outside the comparable denominator. A passing validate must produce a fresh result for \
all of them; a failing green cell is a regression, not permission to move it to red.\n",
        derived.selected.len()
    ));
    if !derived.selected_custom.is_empty() {
        out.push_str(
            "\n### Selected custom commands outside the comparable denominator\n\n\
These rows are part of the selected regression denominator even though they are not rows in \
`ci/compat-envelope/cells.json`. Their exact identities come from \
`ci/expected-e2e-plan.json`; `scorecard.rs check` refuses any selected row that is not accounted \
for by either this table or the comparable green cells above.\n\n\
| Lane | Category | Test | Mode | Backend |\n\
| --- | --- | --- | --- | --- |\n",
        );
        for id in &derived.selected_custom {
            out.push_str(&format!(
                "| `{}` | `{}` | `{}` | `{}` | `{}` |\n",
                id.lane, id.category, id.test, id.mode, id.backend
            ));
        }
    }
    out
}

fn render_measurement_section(tracked: &TrackedCells) -> String {
    let statuses = [
        CellStatus::Green,
        CellStatus::Red,
        CellStatus::NotApplicable,
    ];
    let measurements = [
        MeasurementState::NeverMeasured,
        MeasurementState::MeasuredAndPassed,
        MeasurementState::MeasuredNoVerdict,
        MeasurementState::DivergedUnlocated,
        MeasurementState::Diverged,
    ];
    let count = |status, measurement| {
        tracked
            .cells
            .iter()
            .filter(|cell| cell.status == status && cell.measurement == measurement)
            .count()
    };
    // These current-state claims used to be fixed prose above the generated
    // table. An import changed one count to zero while leaving the prose saying
    // both combinations were present. Derive the claims through the table's
    // own count so another import changes both together.
    let green_never_measured = count(CellStatus::Green, MeasurementState::NeverMeasured);
    let red_measured_and_passed = count(CellStatus::Red, MeasurementState::MeasuredAndPassed);
    let green_never_measured_claim = match green_never_measured {
        0 => "zero Green cells are `never-measured`".to_owned(),
        1 => "1 Green cell is `never-measured`".to_owned(),
        count => format!("{count} Green cells are `never-measured`"),
    };
    let red_measured_and_passed_claim = match red_measured_and_passed {
        1 => "1 Red cell that is `measured-and-passed`".to_owned(),
        count => format!("{count} Red cells that are `measured-and-passed`"),
    };

    let mut out = format!(
        "\n## Status and measurement\n\n\
Selection and observation answer different questions. The Green/Red table says what full validation \
selects. The per-cell `measurement` value says what retained evidence observed: `never-measured`, \
`measured-and-passed`, `measured-no-verdict`, `diverged-unlocated`, or `diverged`. In the current \
generated data, **{green_never_measured_claim}**. Read the generated Status and measurement section \
for the complete current cross-tab; do not use Red as a failed-test count.\n\n\
The current green/`never-measured` count is **{green_never_measured}**, and the current \
red/`measured-and-passed` count is **{red_measured_and_passed}**.\n\n\
Retained history that has not been imported is not counted here. A stored measurement does not \
establish that it describes current code; `show` reports whether the recorded last test still \
matches `HEAD:detcore`.\n\n",
    );
    out.push_str(&format!(
        "The cross-tab includes all **{}** tracked cells; no row is omitted. The current generated \
data contains **{red_measured_and_passed_claim}**. These claims \
use the same counts printed in the table below.\n\n",
        tracked.cells.len(),
    ));
    out.push_str(
        "| Status | `never-measured` | `measured-and-passed` | `measured-no-verdict` | `diverged-unlocated` | `diverged` | Total |\n\
| --- | ---: | ---: | ---: | ---: | ---: | ---: |\n",
    );
    for status in statuses {
        out.push_str(&format!("| `{}`", status.as_str()));
        for measurement in measurements {
            out.push_str(&format!(" | {}", count(status, measurement)));
        }
        let status_total = tracked
            .cells
            .iter()
            .filter(|cell| cell.status == status)
            .count();
        out.push_str(&format!(" | {status_total} |\n"));
    }
    out.push_str("| **Total**");
    for measurement in measurements {
        let measurement_total = tracked
            .cells
            .iter()
            .filter(|cell| cell.measurement == measurement)
            .count();
        out.push_str(&format!(" | **{measurement_total}**"));
    }
    out.push_str(&format!(" | **{}** |\n\n", tracked.cells.len()));

    out.push_str(
        "Cells whose stored `measurement` is not `never-measured` are shown individually so status \
and measurement remain visible together.\n\n\
| Test | Mode | Backend | Status | Measurement |\n\
| --- | --- | --- | --- | --- |\n",
    );
    let mut displayed = 0usize;
    for cell in &tracked.cells {
        if cell.measurement == MeasurementState::NeverMeasured {
            continue;
        }
        displayed += 1;
        out.push_str(&format!(
            "| `{}` | `{}` | `{}` | `{}` | `{}` |\n",
            cell.id.test,
            cell.id.mode,
            cell.id.backend,
            cell.status.as_str(),
            cell.measurement.as_str()
        ));
    }
    if displayed == 0 {
        out.push_str("| _none_ | — | — | — | — |\n");
    }
    out
}

fn tracked_from(
    derived: &Derived,
    existing: Option<TrackedCells>,
    allow_green_removal: Option<&str>,
    allow_cell_removal: bool,
) -> Result<TrackedCells, String> {
    let mut previous = BTreeMap::new();
    let previous_projection = existing.as_ref().and_then(|file| file.projection.clone());
    if let Some(existing) = existing {
        if !(1..=SCHEMA).contains(&existing.schema) {
            return Err(format!(
                "unsupported tracked cell schema {}",
                existing.schema
            ));
        }
        for cell in existing.cells {
            if previous.insert(cell.id.clone(), cell).is_some() {
                return Err("tracked cell file contains a duplicate identity".into());
            }
        }
    }

    let removed: Vec<_> = previous
        .keys()
        .filter(|id| !derived.population.contains(*id))
        .cloned()
        .collect();
    if !removed.is_empty() && !allow_cell_removal {
        return Err(format!(
            "refusing to delete {} tracked cell(s); first is {}. Re-run update with \
             --allow-cell-removal only for an intentional reviewed denominator change",
            removed.len(),
            display_id(&removed[0])
        ));
    }
    let regressed: Vec<_> = previous
        .values()
        .filter(|cell| {
            cell.status == CellStatus::Green
                && derived.population.contains(&cell.id)
                && !derived.green.contains(&cell.id)
        })
        .map(|cell| cell.id.clone())
        .collect();
    if !regressed.is_empty() && allow_green_removal.is_none() {
        return Err(format!(
            "refusing to move {} green cell(s) to red; first is {}. Fix the regression, or use \
             --allow-green-removal <reason> only at an explicit compatibility-standard transition",
            regressed.len(),
            display_id(&regressed[0])
        ));
    }
    // The override is recorded ON the cells it was used for, so it lands in the
    // same diff hunk as the status flip a reviewer is reading.
    let overridden: BTreeSet<_> = if allow_green_removal.is_some() {
        regressed.iter().cloned().collect()
    } else {
        BTreeSet::new()
    };

    let cells = derived
        .population
        .iter()
        .cloned()
        .map(|id| {
            let observations = previous
                .get(&id)
                .map(|cell| cell.observations.clone())
                .unwrap_or_default();
            // Preserved, like observations, for the same reason: ordinary
            // derivation recomputes STATUS from the manifest and the plan, and
            // must not discard measured evidence while doing so.
            let last_tested = previous.get(&id).and_then(|cell| cell.last_tested.clone());
            let enabled = derived.enabled.contains(&id);
            let status = if !enabled {
                CellStatus::NotApplicable
            } else if derived.green.contains(&id) {
                CellStatus::Green
            } else {
                CellStatus::Red
            };
            let ci_disabled_reason = derived.ci_disabled_reasons.get(&id).cloned();
            let not_applicable_reason = derived.not_applicable_reasons.get(&id).cloned();
            // Set when THIS update overrode the ratchet for this cell; otherwise
            // carried forward while the cell stays outside the selected plan,
            // and dropped the moment it is selected again. Result-derived red
            // is not an override: the check is still selected and required.
            let green_removal_reason = if derived.green.contains(&id) {
                None
            } else if overridden.contains(&id) {
                allow_green_removal.map(str::to_string)
            } else {
                previous
                    .get(&id)
                    .and_then(|cell| cell.green_removal_reason.clone())
            };
            let mut cell = TrackedCell {
                id,
                enabled,
                status,
                ci_disabled_reason,
                not_applicable_reason,
                last_tested,
                observations,
                measurement: MeasurementState::NeverMeasured,
                green_removal_reason,
            };
            cell.measurement = derive_measurement(&cell);
            cell
        })
        .collect();
    Ok(TrackedCells {
        schema: SCHEMA,
        // ⚠️ CARRIED FORWARD, NOT RESET. `update` is the ratchet authority; the
        // projection block belongs to the observation writers. Rebuilding it as
        // `None` here silently deleted it on every ordinary derivation, so the
        // file stopped saying when it was last projected and a stale projection
        // became indistinguishable from a fresh one -- the same failure the
        // block exists to prevent. Found by running `update` after
        // `project-observations`, not by reading this function.
        projection: previous_projection,
        cells,
    })
}

/// Which command is writing, for `enforce_writer_boundary`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Writer {
    /// `update`: owns the manifest-derived ratchet fields.
    Update,
    /// The explicit observation commands own measured evidence but cannot alter
    /// scorecard colour.
    Observations,
}

/// REFUSE A WRITE THAT CROSSED THE WRITER BOUNDARY.
///
/// `cells.json` has two authorities living in one tool, and until this existed
/// the split was correct only by convention. `update` derives the ratchet from
/// the manifest and the expected plan; the observation writers merge measured
/// evidence. Neither may touch the other's fields, and a future edit that
/// quietly crossed over would corrupt either the ratchet or the evidence with
/// nothing to catch it.
///
/// This compares the file BEFORE and AFTER rather than auditing each mutation
/// site, so it keeps holding as the code moves. It is deliberately a refusal,
/// not a warning: a boundary that can be crossed with a printed complaint is
/// not a boundary.
fn enforce_writer_boundary(
    before: &TrackedCells,
    after: &TrackedCells,
    writer: Writer,
) -> Result<(), String> {
    let index = |cells: &TrackedCells| -> BTreeMap<CellId, TrackedCell> {
        cells
            .cells
            .iter()
            .map(|cell| (cell.id.clone(), cell.clone()))
            .collect()
    };
    let (old, new) = (index(before), index(after));

    // ⚠️ THE PROJECTION BLOCK IS OBSERVATION-AUTHORITY STATE, so `update` may
    // not silently drop it. It did exactly that before this check existed:
    // `tracked_from` rebuilt the file with `projection: None`, so every
    // ordinary derivation deleted the record of when the observations were last
    // projected, and a stale projection became indistinguishable from a fresh
    // one. Found by running `update` after `project-observations` and looking
    // at the file, which is the only way it shows up.
    if writer == Writer::Update && before.projection.is_some() && after.projection.is_none() {
        return Err(
            "writer boundary violated: `update` dropped the observation projection block.              That block records when the observations were last derived from the series;              deleting it makes a stale projection read as a current one"
                .into(),
        );
    }

    // ⚠️ THE DERIVED FIELD MUST NEVER DISAGREE WITH THE EVIDENCE IT SUMMARISES,
    // and this is checked for EVERY writer rather than split between them.
    // `measurement` is a cache of `derive_measurement`, so a row whose stored
    // value differs from its own observations is a row that lies to any consumer
    // reading it instead of recomputing -- which is the entire reason the field
    // exists. Checking it here, at the one boundary both writers already pass
    // through, is what makes it impossible to write a disagreeing value at all
    // rather than merely discouraged.
    for (id, cell) in &new {
        let derived = derive_measurement(cell);
        if cell.measurement != derived {
            return Err(format!(
                "writer boundary violated: {}/{}/{} stores measurement `{}` but its \
                 own evidence derives `{}`. `measurement` is derived from \
                 `observations`; it is never set independently.",
                id.test,
                id.mode,
                id.backend,
                cell.measurement.as_str(),
                derived.as_str()
            ));
        }
    }

    match writer {
        Writer::Update => {
            // `update` legitimately adds and removes cells when the manifest
            // changes, so only cells present on BOTH sides are compared. What it
            // must never do is alter measured evidence.
            for (id, old_cell) in &old {
                let Some(new_cell) = new.get(id) else {
                    continue;
                };
                if old_cell.observations != new_cell.observations {
                    return Err(format!(
                        "writer boundary violated: `update` changed observations on                          {}/{}/{}. Observations are owned by `update-observations`                          and `observe-results`; `update` may only carry them forward                          verbatim.",
                        id.test, id.mode, id.backend
                    ));
                }
            }
            // A cell the manifest has just introduced cannot already carry
            // evidence; that would mean evidence was invented rather than measured.
            for (id, new_cell) in &new {
                if !old.contains_key(id) && !new_cell.observations.is_empty() {
                    return Err(format!(
                        "writer boundary violated: `update` created {}/{}/{} already                          carrying observations. A new cell has never been measured.",
                        id.test, id.mode, id.backend
                    ));
                }
            }
        }
        Writer::Observations => {
            // An observation writer merges evidence into cells that already
            // exist. It may not change the population, and it may not touch a
            // single ratchet field.
            if old.len() != new.len() {
                return Err(format!(
                    "writer boundary violated: an observation writer changed the cell                      population from {} to {}. Only `update` may add or remove cells.",
                    old.len(),
                    new.len()
                ));
            }
            for (id, old_cell) in &old {
                let Some(new_cell) = new.get(id) else {
                    return Err(format!(
                        "writer boundary violated: an observation writer removed                          {}/{}/{}.",
                        id.test, id.mode, id.backend
                    ));
                };
                let changed = |field: &str| {
                    format!(
                        "writer boundary violated: an observation writer changed `{field}`                          on {}/{}/{}. That field is owned by `update`, which derives it                          from the manifest and the expected plan.",
                        id.test, id.mode, id.backend
                    )
                };
                if old_cell.enabled != new_cell.enabled {
                    return Err(changed("enabled"));
                }
                if old_cell.status != new_cell.status {
                    return Err(changed("status"));
                }
                if old_cell.ci_disabled_reason != new_cell.ci_disabled_reason {
                    return Err(changed("ci_disabled_reason"));
                }
            }
        }
    }
    if before.schema != after.schema && writer == Writer::Observations {
        return Err(
            "writer boundary violated: an observation writer changed the schema version".into(),
        );
    }
    Ok(())
}

fn load_existing(root: &Path) -> Result<Option<TrackedCells>, String> {
    let path = root.join(CELLS);
    if !path.exists() {
        return Ok(None);
    }
    let cells = read_json(&path)?;
    validate_observation_identity_namespace(&cells)?;
    Ok(Some(cells))
}

fn encoded_cells(cells: &TrackedCells) -> Result<String, String> {
    validate_observation_identity_namespace(cells)?;
    // ⚠️ THE NORMALISATION HAPPENS HERE, AT THE SINGLE WRITE CHOKE POINT, AND
    // NOT ONLY AT INGEST. `update` carries already-tracked rows forward without
    // re-reading their results, so an ingest-only fix would clean new rows and
    // leave every historical one — the 78 that have been failing
    // `check-portable-paths.sh` since 2026-08-11 are exactly those. Doing it on
    // encode also means `check_tracked` compares against the normalised form,
    // so the tracked file and the checker cannot disagree.
    //
    // ⚠️ NORMALISE THE TYPED VALUE, NEVER A ROUND-TRIP THROUGH `serde_json::Value`.
    //
    // `Value`'s map is a BTreeMap unless the `preserve_order` feature is on, so
    // `to_value(cells)` -> mutate -> `to_string_pretty(&value)` SORTS EVERY KEY
    // and rewrites all 68k lines of this tracked file for no semantic reason.
    // Measured when I did exactly that: 34268 insertions / 34268 deletions,
    // against 78 real path lines.
    //
    // ⚠️ AND NOTHING CATCHES IT. Key order carries no meaning, so the round-trip
    // is semantically identical: the self-test passes, `check` passes, every
    // consumer still parses, and `update` is still idempotent. The ONLY symptom
    // is the size of the diff. Anyone reaching for a JSON walker here because
    // the typed traversal below looks tedious will get a clean-looking green run
    // and a 34k-line review. Keep the traversal typed.
    let mut normalised = cells.clone();
    for cell in &mut normalised.cells {
        for observation in &mut cell.observations {
            observation.invocations = observation
                .invocations
                .iter()
                .cloned()
                .map(|mut invocation| {
                    normalise_invocation_root(&mut invocation);
                    invocation
                })
                .collect();
        }
    }
    let mut text = serde_json::to_string_pretty(&normalised)
        .map_err(|e| format!("cannot serialize tracked cells: {e}"))?;
    text.push('\n');
    Ok(text)
}

fn wait_for_scorecard_write_lock(
    file: &File,
    path: &Path,
    timeout: Duration,
) -> Result<(), String> {
    let started = Instant::now();
    loop {
        match FileExt::try_lock_exclusive(file) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if started.elapsed() >= timeout {
                    return Err(format!(
                        "timed out after {}s waiting for scorecard write-back lock {}",
                        timeout.as_secs(),
                        path.display()
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => {
                return Err(format!(
                    "cannot acquire scorecard write-back lock {}: {error}",
                    path.display()
                ));
            }
        }
    }
}

fn acquire_scorecard_write_lock(root: &Path) -> Result<File, String> {
    let output = Command::new("git")
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "scorecard-writeback.lock",
        ])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cannot locate the scorecard write-back lock: {e}"))?;
    if !output.status.success() {
        return Err("git rev-parse failed while locating the scorecard write-back lock".into());
    }
    let path = PathBuf::from(
        std::str::from_utf8(&output.stdout)
            .map_err(|e| format!("scorecard write-back lock path is not UTF-8: {e}"))?
            .trim(),
    );
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| {
            format!(
                "cannot open scorecard write-back lock {}: {e}",
                path.display()
            )
        })?;
    wait_for_scorecard_write_lock(&file, &path, Duration::from_secs(30))?;
    Ok(file)
}

fn check_tracked_with_lock(root: &Path) -> Result<Derived, String> {
    let _lock = acquire_scorecard_write_lock(root)?;
    check_tracked(root)
}

fn git_diff_clean(root: &Path, args: &[&str]) -> Result<bool, String> {
    let status = Command::new("git")
        .args(args)
        .current_dir(root)
        .status()
        .map_err(|e| format!("cannot inspect tracked changes: {e}"))?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err("git diff failed while checking tracked changes".into()),
    }
}

fn observation_dirt_error(staged_clean: bool, unrelated_clean: bool) -> Option<&'static str> {
    if !staged_clean {
        Some("observe-results refuses staged changes")
    } else if !unrelated_clean {
        Some("observe-results refuses tracked changes outside the generated scorecard files")
    } else {
        None
    }
}

fn check_observation_worktree(root: &Path) -> Result<(), String> {
    let staged_clean = git_diff_clean(root, &["diff", "--cached", "--quiet", "--no-ext-diff"])?;
    let unrelated_clean = git_diff_clean(
        root,
        &[
            "diff",
            "--quiet",
            "--no-ext-diff",
            "--",
            ".",
            ":(exclude)SCORECARD.md",
            ":(exclude)ci/compat-envelope/cells.json",
        ],
    )?;
    observation_dirt_error(staged_clean, unrelated_clean).map_or(Ok(()), |e| Err(e.into()))
}

#[derive(Clone, PartialEq)]
struct GeneratedFiles {
    scorecard: Vec<u8>,
    cells: Vec<u8>,
}

fn read_generated_files(root: &Path) -> Result<GeneratedFiles, String> {
    Ok(GeneratedFiles {
        scorecard: fs::read(root.join(SCORECARD))
            .map_err(|e| format!("cannot read {SCORECARD}: {e}"))?,
        cells: fs::read(root.join(CELLS)).map_err(|e| format!("cannot read {CELLS}: {e}"))?,
    })
}

fn check_tracked(root: &Path) -> Result<Derived, String> {
    let derived = derive(root)?;
    // ORDER MATTERS, AND IT IS THE FIX FOR A MISDIRECTING FAILURE. `tracked_from` runs FIRST
    // because it is the only step that can name a SEMANTIC cause -- a green cell regressing to
    // red, or a tracked cell disappearing. `compare_file(SCORECARD)` can only ever say "stale;
    // run `update`". Both files derive from the plan, so a plan edit makes BOTH fail at once;
    // with the comparison first, the operator was told to run `update`, and `update` then
    // refused the green regression. Measured 2026-08-25: dropping one green cell from
    // ci/expected-e2e-plan.json produced `check` -> "SCORECARD.md is stale; run update" and
    // `update` -> "refusing to move 1 green cell(s) to red". Following the instruction the tool
    // itself printed could not clear it; the real remedy, --allow-green-removal, was named
    // nowhere in the message the operator actually saw. Deleting a cell already reported its own
    // cause correctly, because that path has no SCORECARD.md difference to mask it -- which is
    // why this looked intermittent rather than ordered.
    let mut cells = tracked_from(&derived, load_existing(root)?, None, false)?;
    // The WRITE path applies this before serialising (see `update_tracked`), so the READ path
    // must too or the two derive different bytes from the same inputs and `check` reports a
    // staleness that `update` cannot clear. Measured 2026-08-25: `update` was a fixed point --
    // three consecutive runs left cells.json byte-identical and exited 0 -- while `check`
    // exited 2 every time telling the operator to run `update`. That made gate.manifest
    // UNSATISFIABLE, and because the gate truncates the lane it took 58 nodes with it on every
    // run. The asymmetry only became reachable once observations were non-empty for the first
    // time, which is why it appeared tonight rather than when it was introduced.
    refresh_measurement(&mut cells);
    let expected_scorecard = format!(
        "{}{}",
        render_scorecard(&derived),
        render_measurement_section(&cells)
    );
    compare_file(&root.join(SCORECARD), &expected_scorecard)?;
    compare_file(&root.join(CELLS), &encoded_cells(&cells)?)?;
    Ok(derived)
}

fn compare_file(path: &Path, expected: &str) -> Result<(), String> {
    let actual = fs::read_to_string(path).map_err(|e| {
        format!(
            "cannot read tracked {}: {e}; run `./ci/compat-envelope/scorecard.rs update`",
            path.display()
        )
    })?;
    if actual != expected {
        return Err(format!(
            "{} is stale; run `./ci/compat-envelope/scorecard.rs update` and review the diff",
            path.display()
        ));
    }
    Ok(())
}

fn update_tracked(
    root: &Path,
    allow_green_removal: Option<&str>,
    allow_cell_removal: bool,
) -> Result<(), String> {
    let derived = derive(root)?;
    let existing = load_existing(root)?;
    let mut cells = tracked_from(
        &derived,
        existing.clone(),
        allow_green_removal,
        allow_cell_removal,
    )?;
    refresh_measurement(&mut cells);
    if let Some(before) = existing.as_ref() {
        enforce_writer_boundary(before, &cells, Writer::Update)?;
    }
    let scorecard = format!(
        "{}{}",
        render_scorecard(&derived),
        render_measurement_section(&cells)
    );
    fs::write(root.join(SCORECARD), scorecard)
        .map_err(|e| format!("cannot write {SCORECARD}: {e}"))?;
    fs::write(root.join(CELLS), encoded_cells(&cells)?)
        .map_err(|e| format!("cannot write {CELLS}: {e}"))?;
    // NOT APPLICABLE IS REPORTED SEPARATELY, NOT FOLDED INTO RED. This line is
    // the number people quote; leaving it as "population minus green" is exactly
    // how 4,940 never-applicable cells were read as failures.
    let not_applicable = derived
        .population
        .iter()
        .filter(|id| !derived.enabled.contains(*id))
        .count();
    println!(
        "compatibility scorecard: wrote {} green / {} red / {} not-applicable / {} total",
        cells
            .cells
            .iter()
            .filter(|cell| cell.status == CellStatus::Green)
            .count(),
        cells
            .cells
            .iter()
            .filter(|cell| cell.status == CellStatus::Red)
            .count(),
        not_applicable,
        derived.population.len()
    );
    Ok(())
}

fn repo_depth_at(root: &Path, revision: &str) -> Option<SourceDepth> {
    let count = |args: &[&str]| -> Option<u64> {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        String::from_utf8(out.stdout)
            .ok()?
            .trim()
            .parse::<u64>()
            .ok()
    };
    Some(SourceDepth {
        commits: count(&["rev-list", "--count", revision])?,
        first_parent: count(&["rev-list", "--count", "--first-parent", revision])?,
    })
}

/// Read the unique full Reverie pin from one recorded Hermit revision.
fn reverie_pin_at(root: &Path, hermit_revision: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["show", &format!("{hermit_revision}:Cargo.lock")])
        .current_dir(root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut pins = BTreeSet::new();
    for line in String::from_utf8(output.stdout).ok()?.lines() {
        if !line.contains("github.com/") || !line.contains("/reverie.git") {
            continue;
        }
        let start = line.find("?rev=")? + "?rev=".len();
        let pin: String = line[start..]
            .chars()
            .take_while(|character| character.is_ascii_hexdigit())
            .collect();
        if pin.len() != 40 {
            return None;
        }
        pins.insert(pin);
    }
    (pins.len() == 1).then(|| pins.into_iter().next().expect("one pin exists"))
}

/// Depths for every repository whose history is part of what was measured.
///
/// Every depth is keyed to recorded source identity, never to a checkout's
/// mutable HEAD. Hermit is mandatory. Reverie is best-effort because the pin is
/// recorded in Hermit's Cargo.lock but its commit graph is only available when
/// a surrounding checkout contains that pin. When either is unavailable, the
/// key is OMITTED rather than a current checkout depth being substituted.
fn source_depths(
    root: &Path,
    hermit_revision: &str,
) -> Result<BTreeMap<String, SourceDepth>, String> {
    let mut depths = BTreeMap::new();
    let hermit = repo_depth_at(root, hermit_revision).ok_or_else(|| {
        format!("cannot read Hermit source depth at recorded SHA {hermit_revision}")
    })?;
    depths.insert("hermit".to_string(), hermit);
    // Sibling first, then the dev-hermit parent layout where hermit checkouts
    // live under worktrees/<slot>/hermit and reverie sits at the top level.
    // Best-effort by design: an unavailable recorded pin or commit graph is
    // reported and omitted rather than replaced with a different revision.
    if let Some(pin) = reverie_pin_at(root, hermit_revision) {
        for candidate in [
            "../reverie",
            "../../reverie",
            "../../../reverie",
            "../../../../reverie",
        ] {
            let path = root.join(candidate);
            if path.join(".git").exists() {
                if let Some(depth) = repo_depth_at(&path, &pin) {
                    depths.insert("reverie".to_string(), depth);
                    break;
                }
            }
        }
    }
    Ok(depths)
}

/// Report how much of the grid carries recorded evidence, and how much of that
/// evidence still describes the CURRENT code.
///
/// This exists so an empty field is never mistaken for a measurement. Absence of
/// `last_tested` means NO WRITER RECORDED ONE, not that the cell was never
/// exercised, and the only way to keep that honest is to state the coverage out
/// loud every time the table is printed.
fn observation_tree_counts(tracked: &TrackedCells, head_tree: &str) -> (usize, usize) {
    tracked
        .cells
        .iter()
        .flat_map(|cell| cell.observations.iter())
        .fold(
            (0, 0),
            |(different, unknown), observation| match observation.detcore_tree.as_deref() {
                Some(tree) if tree != head_tree => (different + 1, unknown),
                None => (different, unknown + 1),
                _ => (different, unknown),
            },
        )
}

fn render_evidence_coverage(root: &Path) -> Result<String, String> {
    let Some(tracked) = load_existing(root)? else {
        return Ok(String::new());
    };
    let head_tree = git_rev_parse(root, "HEAD:detcore").ok();
    let total = tracked.cells.len();
    let stamped = tracked
        .cells
        .iter()
        .filter(|cell| cell.last_tested.is_some())
        .count();
    let (mut current, mut stale) = (0usize, 0usize);
    for cell in &tracked.cells {
        let Some(last) = &cell.last_tested else {
            continue;
        };
        match head_tree.as_deref() {
            Some(head) if head == last.detcore_tree => current += 1,
            Some(_) => stale += 1,
            None => {}
        }
    }
    let observed = tracked
        .cells
        .iter()
        .filter(|cell| !cell.observations.is_empty())
        .count();
    let (different_observations, unknown_observations) = head_tree
        .as_deref()
        .map(|head| observation_tree_counts(&tracked, head))
        .unwrap_or_default();

    let mut out = String::new();
    out.push_str("\nRecorded evidence (not part of the green/red verdict)\n");
    out.push_str(&format!(
        "  cells with a recorded last test : {stamped} of {total}\n"
    ));
    if stamped < total {
        out.push_str(concat!(
            "      the remainder have NO RECORD, which is not the same as never tested:\n",
            "      only the explicit fold commands write it, and validate runs only\n",
            "      the selected plan, so red cells stay blank until a pressure-test\n",
            "      campaign covers them.\n",
        ));
    }
    if head_tree.is_some() && stamped > 0 {
        out.push_str(&format!(
            "      of those, {current} still match HEAD:detcore and {stale} are STALE\n"
        ));
        if stale > 0 {
            out.push_str(concat!(
                "      a stale record describes code that has since changed; do not act\n",
                "      on its divergence point without re-measuring.\n",
            ));
        }
    }
    out.push_str(&format!(
        "  cells with recorded observations: {observed} of {total}\n"
    ));

    // ⚠️ DISAGREEMENT IS INFORMATION, NOT A COLLISION. Owner ruling. A pressure
    // test that stresses a cell harder than validate did and reaches a
    // different result is the system WORKING, so the two provenances are kept
    // in one array and compared here rather than partitioned into places they
    // can never meet. Both results and both sample counts are printed, because
    // "pressure says determinism-failure" means something different at N=1 and
    // at N=40.
    let mut conflicts = Vec::new();
    for cell in &tracked.cells {
        let identities = cell
            .observations
            .iter()
            .map(SeriesObservationIdentity::from_observation)
            .collect::<Result<BTreeSet<_>, _>>()?;
        for identity in identities {
            let at = |p: ObservationProvenance| {
                cell.observations.iter().find(|observation| {
                    SeriesObservationIdentity::from_observation(observation).as_ref()
                        == Ok(&identity)
                        && observation.provenance == p
                })
            };
            let (Some(pressure), Some(validate)) = (
                at(ObservationProvenance::PressureTest),
                at(ObservationProvenance::Validate),
            ) else {
                continue;
            };
            if pressure.results != validate.results {
                conflicts.push((display_id(&cell.id), identity, pressure, validate));
            }
        }
    }
    if !conflicts.is_empty() {
        out.push_str(&format!(
            concat!(
                "\n  !! {} cell(s) where PRESSURE AND VALIDATE DISAGREE at the same recorded code identity.\n",
                "      This is a finding, not an error: the pressure test may simply have\n",
                "      stressed the cell harder. Read both results with their sample counts.\n",
            ),
            conflicts.len()
        ));
        for (id, identity, pressure, validate) in conflicts {
            let n = |o: &Observation| {
                o.first_divergent_record
                    .range()
                    .or_else(|| o.first_divergent_scheduler_turn.range())
                    .map(|r| r.samples)
                    .unwrap_or(0)
            };
            let (identity_kind, identity) = match &identity {
                SeriesObservationIdentity::DetcoreTree(tree) => ("detcore tree", tree),
                SeriesObservationIdentity::HermitCommit(commit) => {
                    ("recorded Hermit commit", commit)
                }
            };
            out.push_str(&format!(
                "      {id} @ {identity_kind} {}\n         pressure: {:?} (positions from {} run(s), {} invocation(s))\n         validate: {:?} (positions from {} run(s), {} invocation(s))\n",
                &identity[..12.min(identity.len())],
                pressure.results,
                n(pressure),
                pressure.invocations.len(),
                validate.results,
                n(validate),
                validate.invocations.len(),
            ));
        }
    }
    if different_observations > 0 {
        out.push_str(&format!(
            "      {different_observations} observation(s) were taken against a DIFFERENT detcore tree\n"
        ));
    }
    if unknown_observations > 0 {
        out.push_str(&format!(
            "      {unknown_observations} observation(s) do not record a detcore tree, so their relation to HEAD:detcore is UNKNOWN\n"
        ));
    }
    Ok(out)
}

fn default_provenance() -> ObservationProvenance {
    ObservationProvenance::PressureTest
}

// `merge_range` was removed by step 5: a stored range is no longer the
// form being merged into. `ObservedPositions::record` appends the run's
// position and `ObservedPositions::range()` derives the bound on demand.

/// What a fold admitted and what it refused, so the caller can report both.
///
/// `skipped` is carried out rather than logged in place because a fold that
/// drops rows silently is strictly worse than one that refuses everything: the
/// caller cannot then tell a thin batch from a broken one.
struct FoldOutcome {
    /// Distinct cells that received an observation.
    cells: usize,
    /// Their exact identities, for callers merging more than one summary.
    cell_ids: BTreeSet<CellId>,
    /// Rows admitted, which exceeds `cells` when a campaign repeated a cell.
    rows: usize,
    /// (cell identity, joined reasons) for every row that marked itself
    /// untrustworthy.
    skipped: Vec<(String, String)>,
}

fn apply_pressure_summary(
    tracked: &mut TrackedCells,
    summary: &PressureSummary,
    head: &str,
    detcore_tree: &str,
    depth: &BTreeMap<String, SourceDepth>,
) -> Result<FoldOutcome, String> {
    if !matches!(summary.schema, 4 | PRESSURE_SUMMARY_SCHEMA) {
        return Err(format!(
            "unsupported pressure summary schema {}",
            summary.schema
        ));
    }
    if summary.source_tree_dirty {
        return Err("dirty pressure results cannot update checked-in observations".into());
    }
    if summary.hermit_sha != head {
        return Err(format!(
            "pressure summary belongs to {}, but HEAD is {head}",
            summary.hermit_sha
        ));
    }
    if summary.detcore_tree != detcore_tree {
        return Err(format!(
            "pressure summary names detcore tree {}, but HEAD contains {detcore_tree}",
            summary.detcore_tree
        ));
    }

    let positions: BTreeMap<_, _> = tracked
        .cells
        .iter()
        .enumerate()
        .map(|(index, cell)| (cell.id.clone(), index))
        .collect();
    if positions.len() != tracked.cells.len() {
        return Err("tracked cell file contains a duplicate identity".into());
    }

    let mut seen = BTreeSet::new();
    let mut skipped: Vec<(String, String)> = Vec::new();
    let mut prepared = Vec::new();
    for row in &summary.rows {
        // Keyed on (cell, repetition, attempt): repeats of one cell and retries
        // within one repetition are both real observations, while two rows
        // claiming the same attempt would double-count.
        if !seen.insert((row.cell.clone(), row.repetition, row.attempt)) {
            skipped.push((
                display_id(&row.cell),
                format!(
                    "duplicate row for repetition {:?}, attempt {}",
                    row.repetition, row.attempt
                ),
            ));
            continue;
        }
        // OPTION B: a row that marks ITSELF untrustworthy is SKIPPED AND NAMED
        // rather than vetoing the whole summary.
        //
        // WHAT THIS GIVES UP, stated plainly because it is a real trade. The
        // old behaviour incidentally protected against a SYSTEMIC fault that
        // damaged some rows detectably and others subtly: the detectable ones
        // vetoed everything, including the quietly-wrong survivors. That
        // protection was never targeted -- it fired by coincidence and it
        // equally voided perfectly good batches, measured at 0 rows admitted
        // from 20 on a real campaign of which 3 were sound.
        //
        // WHAT IT DOES NOT GIVE UP: a row whose artifacts are present but WRONG
        // was admitted under the old behaviour too, because it sets no
        // evidence_errors. B does not widen that surface at all.
        //
        // The systemic case is handled by LOUDNESS instead of by veto: the
        // caller prints every skipped cell with its reasons and both counts, so
        // a batch that skipped 17 of 20 is impossible to mistake for a thin one
        // and an operator can discard it. Silence here would be strictly worse
        // than the old veto, which is why `skipped` is reported and never
        // swallowed.
        //
        // Deliberately placed BEFORE the structural checks below: a row with no
        // readable result row cannot also be expected to carry a well-formed
        // invocation, and reporting the downstream complaint would bury the
        // actual cause.
        if !row.evidence_errors.is_empty() {
            skipped.push((display_id(&row.cell), row.evidence_errors.join("; ")));
            continue;
        }
        let Some(index) = positions.get(&row.cell).copied() else {
            skipped.push((
                display_id(&row.cell),
                "not a cell in the tracked manifest".to_string(),
            ));
            continue;
        };
        // REFUSAL REMOVED BY OWNER RULING. This used to refuse any row whose
        // cell was green, on the grounds that "ordinary validate owns green
        // evidence". Two things were wrong with it. It made the producer and
        // consumer contradict each other outright -- `--repetitions` only
        // repeats GREEN cells, so the one command that can produce a
        // multi-sample range emitted a summary this function rejected on its
        // first row. And it treated a pressure result that disagrees with
        // validate as a collision to be prevented, when it is the most
        // interesting thing the pressure test can produce: stressing a cell
        // harder and finding it red where validate called it green is the
        // system working. Disagreement is surfaced by `show` instead of being
        // refused here.
        let result = match ObservedResult::parse(&row.result) {
            Ok(result) => result,
            Err(why) => {
                skipped.push((display_id(&row.cell), why));
                continue;
            }
        };
        let Some(invocation) = row.invocation.clone() else {
            skipped.push((
                display_id(&row.cell),
                "no literal invocation recorded".to_string(),
            ));
            continue;
        };
        if invocation.run_id.trim().is_empty()
            || invocation.argv.is_empty()
            || invocation.guest_argv.is_empty()
            || invocation.env.is_empty()
            || invocation.cwd.trim().is_empty()
            || invocation.shell_command.trim().is_empty()
            || invocation.shell_command
                != literal_shell_command(&invocation.cwd, &invocation.env, &invocation.argv)
            || invocation.attempts.is_empty()
            || invocation.attempts.iter().any(|attempt| {
                attempt.index.trim().is_empty()
                    || attempt.outcome.trim().is_empty()
                    || attempt.argv.is_empty()
                    || attempt.guest_argv.is_empty()
                    || attempt.env.is_empty()
                    || attempt.cwd.trim().is_empty()
                    || attempt.shell_command.trim().is_empty()
                    || attempt.shell_command
                        != literal_shell_command(&attempt.cwd, &attempt.env, &attempt.argv)
            })
            || invocation.attempts.first().is_some_and(|attempt| {
                attempt.argv != invocation.argv
                    || attempt.guest_argv != invocation.guest_argv
                    || attempt.env != invocation.env
                    || attempt.cwd != invocation.cwd
                    || attempt.shell_command != invocation.shell_command
            })
        {
            skipped.push((
                display_id(&row.cell),
                "incomplete invocation: a field is empty, or shell_command does not \
                 reconstruct from cwd, env and argv"
                    .to_string(),
            ));
            continue;
        }
        let turn = row
            .verification
            .as_ref()
            .and_then(|report| report.first_divergent_scheduler_turn);
        let virtual_nanoseconds = row
            .verification
            .as_ref()
            .and_then(|report| report.first_divergent_virtual_nanoseconds);
        let divergent_record = row
            .verification
            .as_ref()
            .and_then(|report| report.first_divergent_record);
        let divergent_syscall = row
            .verification
            .as_ref()
            .and_then(|report| report.first_divergent_syscall);
        // ⚠️ "COMPARED AND COULD NOT LOCATE" AND "NEVER COMPARED" ARE DIFFERENT
        // FACTS, AND THEY LAND ON THE SAME STATE IF THIS IS NOT CHECKED.
        //
        // A row claiming a divergence result carries no coordinates in two very
        // different situations: two runs were compared and differed but the
        // point could not be located, or no two-run comparison happened at all.
        // Both previously folded to `diverged-unlocated`, which asserts a
        // divergence. The second has no evidence for that assertion.
        //
        // LATENT, NOT LIVE, and said that way on purpose. The producer never
        // emits this shape: `pressure-test.rs` records "missing verification
        // report" whenever a verify or replay cell has no report, and the
        // evidence_errors skip above then catches the row and names it --
        // verified against the real message, "verification recorded no
        // comparison at all (verdict=no_result)". This closes the case where a
        // hand-written or future summary reaches the fold without that error
        // set, so the distinction survives the last hop rather than depending on
        // one producer continuing to be careful. Same class as the absence
        // semantics one layer down in the result row.
        if result.carries_divergence_position() && row.verification.is_none() {
            skipped.push((
                display_id(&row.cell),
                format!(
                    "result {} asserts a divergence with no verification report at all; \
                     a row that never produced a two-run comparison cannot be recorded as \
                     diverged-unlocated, which claims one",
                    row.result
                ),
            ));
            continue;
        }
        if !result.carries_divergence_position()
            && (turn.is_some()
                || virtual_nanoseconds.is_some()
                || divergent_record.is_some()
                || divergent_syscall.is_some())
        {
            skipped.push((
                display_id(&row.cell),
                format!("result {} carries a divergence position", row.result),
            ));
            continue;
        }
        prepared.push((
            index,
            result,
            turn,
            virtual_nanoseconds,
            divergent_record,
            divergent_syscall,
            invocation,
        ));
    }

    // ALL-SKIPPED IS A FAILURE, NOT A QUIET SUCCESS. A batch where every row
    // was untrustworthy tells you nothing about any cell, and returning Ok(0)
    // would let a caller print a cheerful "merged 0" that reads as "nothing to
    // do". The error names every skipped cell, so the refusal carries the same
    // detail the success path would have.
    if prepared.is_empty() && !skipped.is_empty() {
        return Err(format!(
            "every one of the {} offered row(s) was untrustworthy, so nothing was merged:\n{}",
            skipped.len(),
            skipped
                .iter()
                .map(|(cell, why)| format!("  {cell}: {why}"))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    // Distinct from the case above: a summary that OFFERED nothing is a
    // different fault from one whose every row failed, and conflating them
    // would hide an empty campaign behind an evidence complaint.
    if prepared.is_empty() && skipped.is_empty() {
        return Err("pressure summary contains no rows to merge".into());
    }

    let prepared_len = prepared.len();
    let prepared_cell_ids = prepared
        .iter()
        .map(|(index, _, _, _, _, _, _)| tracked.cells[*index].id.clone())
        .collect::<BTreeSet<_>>();
    let prepared_cells = prepared
        .iter()
        .map(|(index, _, _, _, _, _, _)| *index)
        .collect::<BTreeSet<_>>()
        .len();
    for (
        index,
        result,
        turn,
        virtual_nanoseconds,
        divergent_record,
        divergent_syscall,
        invocation,
    ) in prepared
    {
        // Every row here is a cell the pressure test actually exercised,
        // whatever its result, so it gets the same stamp the validate fold
        // applies. This is the ONLY writer that reaches red cells.
        tracked.cells[index].last_tested = Some(LastTested {
            hermit_sha: summary.hermit_sha.clone(),
            detcore_tree: summary.detcore_tree.clone(),
            depth: depth.clone(),
        });
        let observations = &mut tracked.cells[index].observations;
        // Keyed by tree AND provenance. Keying by tree alone would let a
        // validate point and a pressure-test distribution fall into one range.
        let position = observations.iter().position(|observation| {
            observation.detcore_tree.as_deref() == Some(summary.detcore_tree.as_str())
                && observation.provenance == ObservationProvenance::PressureTest
        });
        let observation = match position {
            Some(position) => &mut observations[position],
            None => {
                observations.push(Observation {
                    detcore_tree: Some(summary.detcore_tree.clone()),
                    event_ids: BTreeSet::new(),
                    provenance: ObservationProvenance::PressureTest,
                    depth: depth.clone(),
                    hermit_shas: BTreeSet::new(),
                    results: BTreeSet::new(),
                    canonical_comparisons: BTreeSet::new(),
                    invocations: BTreeSet::new(),
                    first_divergent_scheduler_turn: ObservedPositions::default(),
                    first_divergent_virtual_nanoseconds: ObservedPositions::default(),
                    first_divergent_record: ObservedPositions::default(),
                    first_divergent_syscall: ObservedPositions::default(),
                });
                observations.last_mut().expect("observation was appended")
            }
        };
        observation.hermit_shas.insert(summary.hermit_sha.clone());
        observation.results.insert(result);
        let mut observed_invocation = ObservedInvocation {
            hermit_sha: summary.hermit_sha.clone(),
            run_id: invocation.run_id,
            attempt: None,
            evidence_sha256: None,
            result: Some(result),
            argv: invocation.argv,
            guest_argv: invocation.guest_argv,
            env: invocation.env,
            cwd: invocation.cwd,
            shell_command: invocation.shell_command,
            attempts: invocation.attempts,
        };
        // `encoded_cells` normalises every stored invocation before writing.
        // Normalise the incoming value before set insertion as well, or the
        // same summary differs from its own stored form on the next process
        // invocation and appends every coordinate again.
        normalise_invocation_root(&mut observed_invocation);
        clear_reconstructable_shell_commands(&mut observed_invocation);
        let inserted = !observation
            .invocations
            .iter()
            .any(|existing| pressure_invocation_matches(existing, &observed_invocation))
            && observation.invocations.insert(observed_invocation);
        if inserted {
            observation.first_divergent_scheduler_turn.record(turn);
            observation
                .first_divergent_virtual_nanoseconds
                .record(virtual_nanoseconds);
            observation.first_divergent_record.record(divergent_record);
            observation
                .first_divergent_syscall
                .record(divergent_syscall);
        }
        // Sort by the full key, so a tree carrying both a pressure-test and a
        // validate observation still has a stable tracked-file order.
        observations.sort_by(|left, right| {
            left.detcore_tree
                .cmp(&right.detcore_tree)
                .then(left.provenance.cmp(&right.provenance))
        });
    }
    // DISTINCT ADMITTED CELLS, not rows. Skipped rows must not inflate this
    // count; the caller reports admitted rows, admitted cells, and skips separately.
    Ok(FoldOutcome {
        cells: prepared_cells,
        cell_ids: prepared_cell_ids,
        rows: prepared_len,
        skipped,
    })
}

/// What a validate fold recorded, SPLIT BY WHETHER THE ROW LOCATED ANYTHING.
///
/// Two counts rather than one because the caller's summary line is the only
/// thing most readers see. A single "merged N divergence position(s)" makes
/// N=0 read as "the run was all green", which is wrong precisely when a cell
/// diverged and the comparator could not say where -- the case that needs
/// attention most.
#[derive(Clone, Debug, Default)]
struct ValidateFold {
    /// Rows whose canonical comparison passed.
    passed: usize,
    /// Rows that carried at least one of the four divergence coordinates.
    located: usize,
    /// Rows that diverged and carried none of them.
    unlocated: usize,
    /// Rows that determined no product result: an infrastructure `ERROR`, a
    /// completed FAIL/no_result before comparison, or another non-PASS/non-FAIL
    /// outcome. Counted separately because no canonical product result was
    /// established. Its exact invocation is still measured evidence and is
    /// retained with no product result; counting it separately is what keeps
    /// the run from reading all-green.
    ///
    /// ⚠️ NAMED, NOT JUST COUNTED, following `apply_pressure_summary` -- the sibling
    /// writer already prints every row it drops with its cell and reason, on the
    /// grounds that "a fold that drops rows silently is worse than one that refuses
    /// everything, because the caller cannot then tell a thin batch from a broken
    /// one". A bare count says something did not run without saying WHAT, which
    /// leaves the reader unable to re-run it -- a weaker version of the same defect.
    errored: Vec<String>,
}

impl ValidateFold {
    /// Whether this fold may be reported as an all-green run.
    ///
    /// ⚠️ A FUNCTION RATHER THAN AN INLINE CONDITION, SO THE BRACKET CALLS THE REAL
    /// DECISION. A test that restates the condition keeps passing while the summary
    /// regresses, because the copy and the original drift independently. That is the
    /// same trap as asserting against an expression that merely looks like the
    /// function under test. The summary and the self-test both go through here.
    fn reads_all_green(&self) -> bool {
        self.located == 0 && self.unlocated == 0 && self.errored.is_empty()
    }
}

/// Fold VALIDATE rows into the tracked observations under the `validate`
/// provenance.
///
/// This remains a separate entry point so callers can import a retained result
/// directory directly. Direct top-level local validation invokes it after
/// ledger and receipt publication; ci-hub invokes it in the checkout that
/// requested the isolated run.
///
/// WHAT THESE BOUNDS MEAN, AND WHAT THEY DO NOT. Validate runs a cell once per
/// commit, so a validate observation at one tree is a POINT, not a
/// distribution. Its `samples` will read 1 until the same tree is validated
/// again. Only the pressure test repeats a cell at a fixed tree, so only its
/// bounds describe flakiness. They are stored under different provenance keys
/// precisely so nobody reads one as the other.
fn apply_validate_results(
    tracked: &mut TrackedCells,
    rows: &BTreeMap<CellId, Vec<ResultCandidate>>,
    hermit_sha: &str,
    detcore_tree: &str,
    depth: &BTreeMap<String, SourceDepth>,
    store_invocation: bool,
    store_positions: bool,
) -> Result<ValidateFold, String> {
    let mut updated = tracked.clone();
    let mut fold = ValidateFold::default();
    for (id, candidates) in rows {
        let Some(index) = updated.cells.iter().position(|cell| &cell.id == id) else {
            continue;
        };
        let mut classified = candidates
            .iter()
            .map(|candidate| {
                candidate
                    .row
                    .comparison_evidence()
                    .map(|evidence| (candidate, evidence))
                    .map_err(|error| format!("{} {error}", display_id(id)))
            })
            .collect::<Result<Vec<_>, String>>()?;
        classified.sort_by(|(left, _), (right, _)| {
            left.row
                .run_id
                .cmp(&right.row.run_id)
                .then(left.row.attempt.cmp(&right.row.attempt))
                .then(left.evidence_identity.cmp(&right.evidence_identity))
        });
        let mut distinct = Vec::with_capacity(classified.len());
        let mut identities_by_attempt = BTreeMap::<(String, u64), (String, Option<String>)>::new();
        for (candidate, evidence) in classified {
            let key = (candidate.row.run_id.clone(), candidate.row.attempt);
            let classification_identity = (candidate.row.result.is_some()
                || candidate.row.failure_class.is_some())
            .then(|| {
                serde_json::to_string(&serde_json::json!({
                    "result": candidate.row.result,
                    "failure_class": candidate.row.failure_class,
                    "error_kind": &candidate.row.error_kind,
                }))
                .map_err(|error| format!("cannot encode row classification: {error}"))
            })
            .transpose()?;
            if let Some((identity, classification)) = identities_by_attempt.get(&key) {
                if identity == &candidate.evidence_identity
                    && (classification.is_none()
                        || classification_identity.is_none()
                        || classification == &classification_identity)
                {
                    continue;
                }
                return Err(format!(
                    "{} run {} has conflicting evidence for outer attempt {}",
                    display_id(id),
                    candidate.row.run_id,
                    candidate.row.attempt
                ));
            }
            identities_by_attempt.insert(
                key,
                (candidate.evidence_identity.clone(), classification_identity),
            );
            distinct.push((candidate, evidence));
        }
        for (candidate, evidence) in distinct {
            let row = &candidate.row;
            let located_nothing = row.first_divergent_scheduler_turn.is_none()
                && row.first_divergent_virtual_nanoseconds.is_none()
                && row.first_divergent_record.is_none()
                && row.first_divergent_syscall.is_none();
            let (result, comparison, unavailable_reason) = match evidence {
                ValidateRowEvidence::Matched {
                    left_info_messages,
                    right_info_messages,
                } => (
                    Some(ObservedResult::Pass),
                    Some((left_info_messages, right_info_messages)),
                    None,
                ),
                ValidateRowEvidence::Diverged {
                    left_info_messages,
                    right_info_messages,
                } => (
                    Some(if row.mode == "replay" {
                        ObservedResult::ReplayFailure
                    } else {
                        ObservedResult::DeterminismFailure
                    }),
                    Some((left_info_messages, right_info_messages)),
                    None,
                ),
                ValidateRowEvidence::NotRun { reason, result }
                | ValidateRowEvidence::Unavailable { reason, result } => {
                    (result, None, Some(reason))
                }
            };
            if let Some(reason) = &unavailable_reason {
                fold.errored.push(format!(
                    "{} (outcome={}, reason={reason})",
                    display_id(id),
                    row.outcome,
                ));
            }
            // A no-verdict is still a measurement. Stamp it and retain its exact
            // invocation, but do not manufacture a canonical comparison or a
            // product result that the producer did not establish.
            updated.cells[index].last_tested = Some(LastTested {
                hermit_sha: hermit_sha.to_string(),
                detcore_tree: detcore_tree.to_string(),
                depth: depth.clone(),
            });
            if result == Some(ObservedResult::Pass) && !located_nothing {
                return Err(format!(
                    "{} reports a canonical match yet carries a divergence position",
                    display_id(id)
                ));
            }
            // Same integrity check the pressure path applies to its
            // invocations. A shell_command that does not reconstruct from cwd,
            // env and argv is not a pasteable reproduction, and recording it as
            // one would be worse than recording nothing.
            if row.shell_command != literal_shell_command(&row.cwd, &row.env, &row.argv) {
                return Err(format!(
                    "{}: shell_command does not reconstruct from cwd, env and argv",
                    display_id(id)
                ));
            }
            let attempt_invocations = row
                .attempts
                .iter()
                .map(|attempt| {
                    serde_json::from_value::<ObservedAttemptInvocation>(attempt.clone())
                        .map_err(|e| format!("{}: unreadable attempt record: {e}", display_id(id)))
                })
                .collect::<Result<Vec<_>, String>>()?;
            // A row that reports a comparison but recorded no attempt is
            // self-contradictory: something ran to produce that verdict. The
            // pressure path refuses the same shape.
            if attempt_invocations.is_empty() {
                return Err(format!(
                    "{} reports a comparison but recorded no attempt",
                    display_id(id)
                ));
            }
            if comparison.is_some()
                && result.is_none_or(|result| !result.carries_divergence_position())
                && !located_nothing
            {
                return Err(format!(
                    "{} reports {} yet carries a divergence position",
                    display_id(id),
                    row.outcome
                ));
            }
            let observations = &mut updated.cells[index].observations;
            let position = observations.iter().position(|observation| {
                observation.detcore_tree.as_deref() == Some(detcore_tree)
                    && observation.provenance == ObservationProvenance::Validate
                    && observation.event_ids.is_empty()
            });
            let observation = match position {
                Some(position) => &mut observations[position],
                None => {
                    observations.push(Observation {
                        detcore_tree: Some(detcore_tree.to_string()),
                        event_ids: BTreeSet::new(),
                        provenance: ObservationProvenance::Validate,
                        depth: depth.clone(),
                        hermit_shas: BTreeSet::new(),
                        results: BTreeSet::new(),
                        canonical_comparisons: BTreeSet::new(),
                        invocations: BTreeSet::new(),
                        first_divergent_scheduler_turn: ObservedPositions::default(),
                        first_divergent_virtual_nanoseconds: ObservedPositions::default(),
                        first_divergent_record: ObservedPositions::default(),
                        first_divergent_syscall: ObservedPositions::default(),
                    });
                    observations.last_mut().expect("observation was appended")
                }
            };
            observation.hermit_shas.insert(hermit_sha.to_string());
            if let Some(result) = result {
                observation.results.insert(result);
            }
            if let Some((left_info_messages, right_info_messages)) = comparison {
                let hermit_depth = depth.get("hermit").ok_or_else(|| {
                    format!("{} observation has no Hermit source depth", display_id(id))
                })?;
                observation
                    .canonical_comparisons
                    .insert(CanonicalComparison {
                        hermit_sha: row.hermit_sha.clone(),
                        hermit_commits: hermit_depth.commits,
                        hermit_first_parent: hermit_depth.first_parent,
                        run_id: row.run_id.clone(),
                        evidence_sha256: candidate.evidence_identity.clone(),
                        result: result.expect("canonical evidence has a result"),
                        left_info_messages,
                        right_info_messages,
                    });
            }
            // Record the invocation, exactly as the pressure path does. Without
            // it a validate-sourced bound would have strictly WORSE provenance
            // than a pressure-sourced one: no per-run record, no run_id, and no
            // pasteable command to reproduce the divergence it reports.
            let inserted = if store_invocation || unavailable_reason.is_some() {
                observation.invocations.insert(ObservedInvocation {
                    hermit_sha: row.hermit_sha.clone(),
                    run_id: row.run_id.clone(),
                    attempt: unavailable_reason.as_ref().map(|_| row.attempt),
                    evidence_sha256: unavailable_reason
                        .as_ref()
                        .map(|_| candidate.evidence_identity.clone()),
                    result,
                    argv: row.argv.clone(),
                    guest_argv: row.guest_argv.clone(),
                    env: row.env.clone(),
                    cwd: row.cwd.clone(),
                    shell_command: row.shell_command.clone(),
                    attempts: attempt_invocations,
                })
            } else {
                true
            };
            // Re-importing the same retained evidence must be byte-idempotent.
            // Positions are vectors, so appending them when the invocation set
            // rejected a duplicate would silently inflate the sample count.
            if inserted && store_positions {
                observation
                    .first_divergent_scheduler_turn
                    .record(row.first_divergent_scheduler_turn);
                observation
                    .first_divergent_virtual_nanoseconds
                    .record(row.first_divergent_virtual_nanoseconds);
                observation
                    .first_divergent_record
                    .record(row.first_divergent_record);
                observation
                    .first_divergent_syscall
                    .record(row.first_divergent_syscall);
            }
            observations.sort_by(|left, right| {
                left.detcore_tree
                    .cmp(&right.detcore_tree)
                    .then(left.provenance.cmp(&right.provenance))
            });
            if unavailable_reason.is_none() {
                if result == Some(ObservedResult::Pass) {
                    fold.passed += 1;
                } else if located_nothing || !store_positions {
                    fold.unlocated += 1;
                } else {
                    fold.located += 1;
                }
            }
        }
    }
    *tracked = updated;
    Ok(fold)
}

fn observe_results(root: &Path, results: &Path) -> Result<(), String> {
    let _lock = acquire_scorecard_write_lock(root)?;
    let head = git_head(root)?;
    if !results.is_dir() {
        return Err(format!(
            "result directory does not exist: {}",
            results.display()
        ));
    }
    let mut files = Vec::new();
    find_result_files(results, &mut files)?;
    if files.is_empty() {
        println!(
            "compatibility scorecard: merged 0 pass, 0 located divergence, and 0 unlocated \
             divergence validate observation(s) at {head}"
        );
        println!("compatibility scorecard: generated files unchanged");
        return Ok(());
    }
    check_observation_worktree(root)?;
    let original = read_generated_files(root)?;
    let derived = check_tracked(root)?;
    let detcore_tree = git_rev_parse(root, "HEAD:detcore")?;
    let rows = read_result_candidates(results, &head)?;
    let depth = source_depths(root, &head)?;
    if !depth.contains_key("reverie") {
        println!(
            "  note: no Reverie depth was resolved from the recorded Hermit revision, so \
             Reverie depth is OMITTED rather than guessed. Hermit depth is recorded."
        );
    }
    let mut tracked: TrackedCells = serde_json::from_slice(&original.cells)
        .map_err(|e| format!("cannot parse tracked {CELLS}: {e}"))?;
    let before = tracked.clone();
    let fold = apply_validate_results(
        &mut tracked,
        &rows,
        &head,
        &detcore_tree,
        &depth,
        true,
        true,
    )?;
    refresh_measurement(&mut tracked);
    enforce_writer_boundary(&before, &tracked, Writer::Observations)?;
    let updated = generated_files(&derived, &tracked)?;
    let changed = replace_generated_files_with(
        root,
        &original,
        &updated,
        || {
            check_observation_worktree(root)?;
            let current_head = git_head(root)?;
            if current_head != head {
                return Err(format!(
                    "HEAD moved from {head} to {current_head} during write-back"
                ));
            }
            if read_generated_files(root)? != original {
                return Err("the generated scorecard files changed during write-back".into());
            }
            Ok(())
        },
        |_| Ok(()),
    )?;
    println!(
        "compatibility scorecard: merged {} pass, {} located divergence, and {} unlocated \
         divergence {} observation(s) at {head}",
        fold.passed,
        fold.located,
        fold.unlocated,
        ObservationProvenance::Validate.as_str()
    );
    println!(
        "compatibility scorecard: generated files {}",
        if changed { "changed" } else { "unchanged" }
    );
    // FOUR OUTCOMES, NOT TWO. This used to print the all-green sentence
    // whenever the located count was zero, which said "expected result for an
    // all-green run" over a batch whose cells had diverged without a locatable
    // position -- the one outcome that most needs to be read as a finding.
    //
    // ⚠️ AND `errored` MUST BE IN THE ALL-GREEN CONDITION BELOW. Without it a batch
    // in which EVERY row was an infrastructure failure folds to zero located and
    // zero unlocated and prints the all-green sentence -- the identical collapse,
    // one outcome over, in the line a human actually acts on. An all-green summary
    // is what stops anyone looking, so this is the worst place for it to happen.
    if !fold.errored.is_empty() {
        println!(
            "  ⚠️ {} row(s) DETERMINED NOTHING -- an infrastructure ERROR, a completed \
             FAIL/no_result before comparison, or another non-PASS non-FAIL outcome. \
             NO CANONICAL PRODUCT RESULT WAS ADMITTED for them, so this run is NOT \
             all-green -- and it is NOT a product failure either. Their exact run and \
             attempt were stored as measured no-verdict; no pass, divergence, or crash \
             was invented. Re-run these cells; do not read this as a product result.",
            fold.errored.len()
        );
        for cell in &fold.errored {
            println!("    determined nothing: {cell}");
        }
    }
    if fold.reads_all_green() {
        println!(
            "  no row diverged. That is the expected result for an all-green run \
             and is NOT evidence that the field is unpopulated."
        );
    } else if fold.unlocated > 0 {
        println!(
            "  {} row(s) DIVERGED but located nothing on any of the four coordinate axes. \
             Those cells now read `{}`, which is a comparator gap and is NOT the same \
             finding as a cell that was never compared.",
            fold.unlocated,
            MeasurementState::DivergedUnlocated.as_str()
        );
    }
    Ok(())
}

fn import_results(
    root: &Path,
    results: &Path,
    current_summaries: &[PathBuf],
) -> Result<(), String> {
    let status = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cannot inspect working tree: {e}"))?;
    if !status.status.success() {
        return Err("git status failed while checking the working tree".into());
    }
    let dirty = String::from_utf8_lossy(&status.stdout);
    let unrelated_dirty = dirty
        .lines()
        .filter_map(|line| line.get(3..))
        .filter(|path| *path != SCORECARD && *path != CELLS)
        .collect::<Vec<_>>();
    if !unrelated_dirty.is_empty() {
        return Err(format!(
            "import-results refuses unrelated tracked changes; first is {}",
            unrelated_dirty[0]
        ));
    }

    let derived = derive(root)?;
    let before = load_existing(root)?.ok_or("tracked cell file does not exist")?;
    if before.cells.len() != derived.population.len() {
        return Err(format!(
            "tracked population is {}, derived population is {}; run update before importing",
            before.cells.len(),
            derived.population.len()
        ));
    }
    let import_cells = retained_import_cells(&derived);
    let RetainedImport {
        cells: retained_cells,
        files_scanned,
        rows_scanned,
        terminal_comparisons,
        stale_coordinate_rows,
        stale_coordinates,
        stale_coordinate_cells,
        no_result_cells,
    } = read_retained_results(root, results, &import_cells)?;
    let retained_cell_count = retained_cells.len();
    let current = read_current_pressure_evidence(root, current_summaries, &before)?;
    let mut tracked = tracked_from(&derived, Some(before.clone()), None, false)?;
    // This command is a projection, not an append-only history store. Remove
    // only observations carrying this command's canonical-comparison receipt;
    // series and pressure observations are separate evidence and survive.
    for cell in &mut tracked.cells {
        remove_imported_validate_projection(cell);
    }

    let mut fold = ValidateFold::default();
    let mut historical_without_coordinates = 0usize;
    let mut outcome_counts: BTreeMap<RetainedComparisonState, usize> = BTreeMap::new();
    let mut outcome_rows = Vec::new();
    let mut retained_rows_imported = 0usize;
    let mut current_rows_imported = 0usize;
    let mut current_rows_missing_retained_logs = Vec::new();
    let mut current_depths = BTreeMap::new();
    for current_results in current.results.values() {
        for current_result in current_results {
            let current_sha = &current_result.summary.hermit_sha;
            let hermit_depth = match current_depths.get(current_sha) {
                Some(depth) => *depth,
                None => {
                    let depth = repo_depth_at(root, current_sha).ok_or_else(|| {
                        format!("cannot read Hermit source depth at current SHA {current_sha}")
                    })?;
                    current_depths.insert(current_sha.clone(), depth);
                    depth
                }
            };
            let depth = BTreeMap::from([("hermit".to_string(), hermit_depth)]);
            apply_pressure_summary(
                &mut tracked,
                &current_result.summary,
                &current_result.summary.hermit_sha,
                &current_result.summary.detcore_tree,
                &depth,
            )?;
            current_rows_imported += 1;
            if current_result.missing_retained_logs {
                current_rows_missing_retained_logs
                    .push(display_id(&current_result.summary.rows[0].cell));
            }
        }
    }
    for retained in retained_cells {
        let retained_id = retained.id.clone();
        let has_coordinate = retained.candidates.iter().any(|candidate| {
            candidate.row.outcome == "FAIL"
                && !DivergenceCoordinates::from_row(&candidate.row).is_empty()
        });
        let decision = if has_coordinate {
            Some(retained_coordinate_decision(retained, &current))
        } else {
            historical_without_coordinates += 1;
            let rows = BTreeMap::from([(retained.id.clone(), retained.candidates.clone())]);
            let one = apply_validate_results(
                &mut tracked,
                &rows,
                &retained.hermit_sha,
                &retained.detcore_tree,
                &retained.depth,
                false,
                true,
            )?;
            retained_rows_imported += retained.candidates.len();
            fold.passed += one.passed;
            fold.located += one.located;
            fold.unlocated += one.unlocated;
            fold.errored.extend(one.errored);
            None
        };
        let Some(decision) = decision else { continue };
        *outcome_counts.entry(decision.state).or_default() += 1;
        match decision.import {
            ImportEvidence::Retained {
                results: retained,
                store_positions,
            } => {
                let rows = BTreeMap::from([(retained.id.clone(), retained.candidates.clone())]);
                let one = apply_validate_results(
                    &mut tracked,
                    &rows,
                    &retained.hermit_sha,
                    &retained.detcore_tree,
                    &retained.depth,
                    false,
                    store_positions,
                )?;
                retained_rows_imported += retained.candidates.len();
                fold.passed += one.passed;
                fold.located += one.located;
                fold.unlocated += one.unlocated;
                fold.errored.extend(one.errored);
            }
            ImportEvidence::None => {}
        }
        outcome_rows.push((
            retained_id,
            decision.state,
            decision.retained_coordinates,
            decision.current_coordinates,
            decision.reason,
        ));
    }
    if !fold.errored.is_empty() {
        return Err(format!(
            "retained import selected {} rows that determined nothing; first is {}",
            fold.errored.len(),
            fold.errored[0]
        ));
    }
    refresh_measurement(&mut tracked);
    enforce_writer_boundary(&before, &tracked, Writer::Observations)?;

    let measurement_counts = |cells: &TrackedCells| {
        let mut counts = BTreeMap::new();
        for cell in &cells.cells {
            *counts.entry(cell.measurement.as_str()).or_insert(0usize) += 1;
        }
        counts
    };
    let before_counts = measurement_counts(&before);
    let after_counts = measurement_counts(&tracked);
    let changed = before
        .cells
        .iter()
        .filter_map(|old| {
            let new = tracked.cells.iter().find(|cell| cell.id == old.id)?;
            (old.measurement != new.measurement).then_some((old, new))
        })
        .collect::<Vec<_>>();

    let scorecard = format!(
        "{}{}",
        render_scorecard(&derived),
        render_measurement_section(&tracked)
    );
    fs::write(root.join(SCORECARD), scorecard)
        .map_err(|e| format!("cannot write {SCORECARD}: {e}"))?;
    fs::write(root.join(CELLS), encoded_cells(&tracked)?)
        .map_err(|e| format!("cannot write {CELLS}: {e}"))?;

    println!(
        "compatibility scorecard: found {} enabled cell(s) with {} terminal BitwiseInfoV1 comparison(s) in {} retained results.jsonl file(s) containing {} row(s); imported {} retained row(s) and {} current pressure row(s); no guest was executed",
        retained_cell_count,
        terminal_comparisons,
        files_scanned,
        rows_scanned,
        retained_rows_imported,
        current_rows_imported,
    );
    println!(
        "  excluded as stale: {stale_coordinate_rows} older diverging comparison row(s), carrying {stale_coordinates} coordinate value(s), across {} enabled cell(s) whose newest retained canonical result is a pass",
        stale_coordinate_cells.len()
    );
    for cell in &stale_coordinate_cells {
        println!("    stale coordinate: {cell}");
    }
    println!(
        "  retained comparisons without a divergence coordinate: {historical_without_coordinates}; enabled cells with no retained canonical comparison: {}",
        no_result_cells.len()
    );
    println!(
        "  current canonical divergence row(s) imported from typed reports without retained run logs: {}",
        current_rows_missing_retained_logs.len()
    );
    for cell in &current_rows_missing_retained_logs {
        println!("    missing retained run logs: {cell}");
    }
    println!(
        "  retained coordinate freshness: FRESH={} DRIFTED={} WRONG={} UNCHECKABLE={}",
        outcome_counts
            .get(&RetainedComparisonState::Fresh)
            .copied()
            .unwrap_or(0),
        outcome_counts
            .get(&RetainedComparisonState::Drifted)
            .copied()
            .unwrap_or(0),
        outcome_counts
            .get(&RetainedComparisonState::Wrong)
            .copied()
            .unwrap_or(0),
        outcome_counts
            .get(&RetainedComparisonState::Uncheckable)
            .copied()
            .unwrap_or(0),
    );
    for (id, state, retained_coordinates, current_coordinates, reason) in &outcome_rows {
        println!(
            "    {}: {} retained={:?} current={:?} — {}",
            display_id(id),
            state.as_str(),
            retained_coordinates,
            current_coordinates,
            reason
        );
    }
    println!(
        "  population before: {} cells; measurement {:?}",
        before.cells.len(),
        before_counts
    );
    println!(
        "  population after : {} cells; measurement {:?}",
        tracked.cells.len(),
        after_counts
    );
    for (old, new) in changed {
        println!(
            "  {}: {} -> {} at {}",
            display_id(&new.id),
            old.measurement.as_str(),
            new.measurement.as_str(),
            new.last_tested
                .as_ref()
                .map(|last| last.hermit_sha.as_str())
                .unwrap_or("no recorded SHA")
        );
    }
    Ok(())
}

fn git_is_ancestor(root: &Path, ancestor: &str, descendant: &str) -> Result<bool, String> {
    let status = Command::new("git")
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .current_dir(root)
        .status()
        .map_err(|e| format!("cannot compare Hermit revisions {ancestor} and {descendant}: {e}"))?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        code => Err(format!(
            "git merge-base could not compare Hermit revisions {ancestor} and {descendant} (exit {code:?})"
        )),
    }
}

fn update_observations(
    root: &Path,
    summary_paths: &[PathBuf],
    retained: bool,
) -> Result<(), String> {
    let derived = check_tracked(root)?;
    let status = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cannot inspect working tree: {e}"))?;
    if !status.status.success() {
        return Err("git status failed while checking the working tree".into());
    }
    if !status.stdout.is_empty() {
        return Err("update-observations requires a clean tracked working tree".into());
    }

    let summaries = summary_paths
        .iter()
        .map(|path| read_json(path).map(|summary| (path, summary)))
        .collect::<Result<Vec<(&PathBuf, PressureSummary)>, String>>()?;
    let head = git_head(root)?;
    let head_detcore_tree = git_rev_parse(root, "HEAD:detcore")?;
    let mut tracked = load_existing(root)?.ok_or("tracked cell file does not exist")?;
    let before = tracked.clone();
    let mut changed_cells = BTreeSet::new();
    let mut total_rows = 0usize;
    let mut total_skipped = 0usize;

    for (summary_path, summary) in summaries {
        let (expected_head, expected_detcore_tree, main_ancestry) = if retained {
            let recorded_detcore_tree =
                git_rev_parse(root, &format!("{}:detcore", summary.hermit_sha)).map_err(
                    |error| {
                        format!(
                            "{} names Hermit commit {} whose Detcore tree cannot be read: {error}",
                            summary_path.display(),
                            summary.hermit_sha
                        )
                    },
                )?;
            if recorded_detcore_tree != summary.detcore_tree {
                return Err(format!(
                    "{} names Detcore tree {}, but Hermit {} contains {}",
                    summary_path.display(),
                    summary.detcore_tree,
                    summary.hermit_sha,
                    recorded_detcore_tree
                ));
            }
            (
                summary.hermit_sha.as_str(),
                summary.detcore_tree.as_str(),
                Some(git_is_ancestor(root, &summary.hermit_sha, &head)?),
            )
        } else {
            (head.as_str(), head_detcore_tree.as_str(), Some(true))
        };
        let depth = source_depths(root, &summary.hermit_sha)?;
        if !depth.contains_key("reverie") {
            println!(
                "  note: no Reverie depth was resolved from the recorded Hermit revision, so \
                 Reverie depth is OMITTED rather than guessed. Hermit depth is recorded."
            );
        }
        let previous_last_tested = tracked
            .cells
            .iter()
            .map(|cell| cell.last_tested.clone())
            .collect::<Vec<_>>();
        let outcome = apply_pressure_summary(
            &mut tracked,
            &summary,
            expected_head,
            expected_detcore_tree,
            &depth,
        )?;
        if retained {
            for (previous, current) in previous_last_tested.iter().zip(&mut tracked.cells) {
                if !should_replace_retained_last_tested(
                    previous.as_ref(),
                    &summary.hermit_sha,
                    &summary.detcore_tree,
                    main_ancestry,
                    &depth,
                ) {
                    current.last_tested = previous.clone();
                }
            }
        }
        changed_cells.extend(outcome.cell_ids.iter().cloned());
        total_rows += outcome.rows;
        total_skipped += outcome.skipped.len();
        println!(
            "compatibility scorecard: {} merged {} row(s) across {} cell(s) at {}; {} row(s) skipped",
            summary_path.display(),
            outcome.rows,
            outcome.cells,
            summary.hermit_sha,
            outcome.skipped.len()
        );
        // NAMED INDIVIDUALLY, NEVER JUST COUNTED. A fold that drops rows silently
        // is worse than one that refuses everything, because the caller cannot then
        // tell a thin batch from a broken one. A high skip ratio here is the signal
        // that something systemic went wrong with the campaign.
        for (cell, why) in &outcome.skipped {
            println!("  skipped {cell}: {why}");
        }
    }

    refresh_measurement(&mut tracked);
    enforce_writer_boundary(&before, &tracked, Writer::Observations)?;
    write_observation_files(root, &derived, &tracked)?;
    println!(
        "compatibility scorecard: merged {total_rows} row(s) across {} distinct cell(s) from {} summary file(s); {total_skipped} row(s) skipped",
        changed_cells.len(),
        summary_paths.len()
    );
    Ok(())
}

/// Re-derive the observation projection from the series store.
///
/// The series row's `cell` is already the manifest's exact
/// `test/mode/backend` identity. Retained validate history uses a different
/// input path; the import and this projection preserve one another's evidence.
fn project_observations(root: &Path, series_root: &Path, refreshed_at: &str) -> Result<(), String> {
    let derived = check_tracked(root)?;
    let mut tracked = load_existing(root)?.ok_or("tracked cell file does not exist")?;
    let before = tracked.clone();

    // Resolve and verify the source once, then parse only the captured commit
    // bytes. A later worktree mutation cannot change the rows while leaving the
    // recorded commit behind.
    let snapshot = snapshot_series_source(series_root)?;
    let rows = read_series_rows(&snapshot)?;
    let projection = apply_series_rows(root, &mut tracked, &rows, Some(&snapshot.source))?;
    let skipped = &projection.skipped;

    let rows_read = rows.len() as u64;
    tracked.schema = SCHEMA;
    tracked.projection = Some(ObservationProjection {
        source: snapshot.source.clone(),
        source_commit: Some(snapshot.source_commit.clone()),
        source_tree: Some(snapshot.source_tree.clone()),
        refreshed_at: refreshed_at.to_string(),
        rows_read,
        pre_series_corpus: projection.pre_series_corpus,
    });
    refresh_measurement(&mut tracked);

    // The guard runs BEFORE the write, not after. A projection that has already
    // hit the disk is a projection someone has to notice and revert.
    enforce_projection_preserves_evidence(&before, &tracked, rows_read)?;
    enforce_writer_boundary(&before, &tracked, Writer::Observations)?;
    write_observation_files(root, &derived, &tracked)?;

    println!(
        "compatibility scorecard: projected {} cell(s) from {} evidence row(s), including {} \
         measured no-verdict row(s), representing {} run(s); read {rows_read} canonical series \
         row(s) under {} at {} tree {}",
        projection.cells,
        projection.rows,
        projection.no_verdict_rows,
        projection.runs,
        snapshot.source,
        snapshot.source_commit,
        snapshot.source_tree
    );
    println!(
        "  {} row(s) were already represented by one exact recorded run/attempt; \
         replaced {} prior projector-owned observation(s) by exact event_id",
        projection.represented_rows, projection.replaced_observations
    );
    if rows_read == 0 {
        println!(
            "  note: the series is EMPTY, so every observation here remains PRE-SERIES \
             evidence rather than a projection."
        );
    }
    for line in skipped {
        println!("  skipped {line}");
    }
    Ok(())
}

fn verify_combined_write_state(
    root: &Path,
    expected_head: &str,
    expected_scorecard: &[u8],
    expected_cells: &[u8],
    snapshot: &HeldScorecardSeriesSnapshot,
) -> Result<(), String> {
    check_observation_worktree(root)?;
    let current_head = git_head(root)?;
    if current_head != expected_head {
        return Err(format!(
            "HEAD moved from {expected_head} to {current_head} during combined scorecard write-back"
        ));
    }
    let current = read_generated_files(root)?;
    if current.scorecard != expected_scorecard {
        return Err(format!(
            "{SCORECARD} changed during combined scorecard write-back"
        ));
    }
    if current.cells != expected_cells {
        return Err(format!(
            "{CELLS} changed during combined scorecard write-back"
        ));
    }
    snapshot.verify()
}

fn project_and_observe_results(
    root: &Path,
    snapshot_path: &Path,
    snapshot_sha256: &str,
    results: &Path,
    expected_head: &str,
    refreshed_at: &str,
) -> Result<(), String> {
    project_and_observe_results_with(
        root,
        snapshot_path,
        snapshot_sha256,
        results,
        expected_head,
        refreshed_at,
        || Ok(()),
        |_| Ok(()),
    )
}

#[allow(clippy::too_many_arguments)]
fn project_and_observe_results_with<BeforeGuard, BeforeReplace>(
    root: &Path,
    snapshot_path: &Path,
    snapshot_sha256: &str,
    results: &Path,
    expected_head: &str,
    refreshed_at: &str,
    mut before_guard: BeforeGuard,
    mut before_replace: BeforeReplace,
) -> Result<(), String>
where
    BeforeGuard: FnMut() -> Result<(), String>,
    BeforeReplace: FnMut(usize) -> Result<(), String>,
{
    if !is_object_id(expected_head) {
        return Err("--expected-head requires exactly 40 lowercase hexadecimal characters".into());
    }
    if refreshed_at.trim().is_empty() {
        return Err("--refreshed-at requires a nonempty deterministic stamp".into());
    }
    if !results.is_dir() {
        return Err(format!(
            "result directory does not exist: {}",
            results.display()
        ));
    }

    let _lock = acquire_scorecard_write_lock(root)?;
    let head = git_head(root)?;
    if head != expected_head {
        return Err(format!(
            "combined scorecard write-back expected HEAD {expected_head}, found {head}"
        ));
    }
    check_observation_worktree(root)?;
    let original = read_generated_files(root)?;
    let derived = check_tracked(root)?;
    let snapshot = HeldScorecardSeriesSnapshot::open(snapshot_path, snapshot_sha256)?;
    let detcore_tree = git_rev_parse(root, "HEAD:detcore")?;
    let depth = source_depths(root, &head)?;
    let result_rows = read_result_candidates(results, &head)?;

    let mut tracked: TrackedCells = serde_json::from_slice(&original.cells)
        .map_err(|error| format!("cannot parse tracked {CELLS}: {error}"))?;
    let before = tracked.clone();
    // Validate current inputs on a private copy before either projection can
    // encounter the old compact run matcher. Reconcile the complete snapshot
    // first; only exactly represented events may be suppressed. Existing
    // projected events still undergo their full-source ownership check.
    let mut preview = tracked.clone();
    apply_validate_results(
        &mut preview,
        &result_rows,
        &head,
        &detcore_tree,
        &depth,
        true,
        true,
    )?;
    let current_attempts = current_result_attempts(&preview, &result_rows, &detcore_tree)?;
    let preview_representation =
        direct_representation(&preview, &snapshot.rows, &current_attempts)?;
    remove_replaceable_projected_observations(
        &mut tracked,
        &snapshot.rows,
        Some(&snapshot.snapshot.source.path),
    )?;
    let initial_rows = snapshot
        .rows
        .iter()
        .filter(|row| {
            !preview_representation
                .represented_event_ids
                .contains(&row.event_id)
        })
        .cloned()
        .collect::<Vec<_>>();
    let _initial_projection = apply_series_rows(
        root,
        &mut tracked,
        &initial_rows,
        Some(&snapshot.snapshot.source.path),
    )?;
    let rows_read = snapshot.snapshot.rows_read;
    tracked.schema = SCHEMA;
    tracked.projection = Some(ObservationProjection {
        source: snapshot.snapshot.source.path.clone(),
        source_commit: Some(snapshot.snapshot.source.commit.clone()),
        source_tree: Some(snapshot.snapshot.source.tree.clone()),
        refreshed_at: refreshed_at.to_string(),
        rows_read,
        pre_series_corpus: true,
    });
    enforce_projection_preserves_evidence(&before, &tracked, rows_read)?;
    let fold = apply_validate_results(
        &mut tracked,
        &result_rows,
        &head,
        &detcore_tree,
        &depth,
        true,
        true,
    )?;
    // A normal validate publishes its series row before this local write-back.
    // Match the final direct evidence to the complete snapshot before choosing
    // its stored representation. Exactly represented events are suppressed in
    // favour of the richer direct invocation; zero matches remain explicit
    // pre-series evidence, while conflicting or multiple matches refuse.
    let representation = direct_representation(&tracked, &snapshot.rows, &current_attempts)?;
    remove_replaceable_projected_observations(
        &mut tracked,
        &snapshot.rows,
        Some(&snapshot.snapshot.source.path),
    )?;
    let projected_rows = snapshot
        .rows
        .iter()
        .filter(|row| !representation.represented_event_ids.contains(&row.event_id))
        .cloned()
        .collect::<Vec<_>>();
    let projection = apply_series_rows(
        root,
        &mut tracked,
        &projected_rows,
        Some(&snapshot.snapshot.source.path),
    )?;
    tracked.projection = Some(ObservationProjection {
        source: snapshot.snapshot.source.path.clone(),
        source_commit: Some(snapshot.snapshot.source.commit.clone()),
        source_tree: Some(snapshot.snapshot.source.tree.clone()),
        refreshed_at: refreshed_at.to_string(),
        rows_read,
        // This describes the FINAL post-current state. An exact direct run
        // represented by one snapshot event is not pre-series merely because
        // its richer stored form deliberately has no event_ids.
        pre_series_corpus: rows_read == 0 || representation.has_unrepresented_direct_evidence,
    });
    enforce_projection_preserves_evidence(&before, &tracked, rows_read)?;
    refresh_measurement(&mut tracked);
    enforce_writer_boundary(&before, &tracked, Writer::Observations)?;
    let updated = generated_files(&derived, &tracked)?;
    let partial_scorecard = updated.scorecard.clone();

    let changed = replace_generated_files_with(
        root,
        &original,
        &updated,
        || {
            before_guard()?;
            verify_combined_write_state(
                root,
                &head,
                &original.scorecard,
                &original.cells,
                &snapshot,
            )
        },
        |replacement| {
            before_replace(replacement)?;
            let expected_scorecard = if replacement == 1 {
                &original.scorecard
            } else {
                &partial_scorecard
            };
            verify_combined_write_state(root, &head, expected_scorecard, &original.cells, &snapshot)
        },
    )?;
    println!(
        "compatibility scorecard: projected {} cell(s) from {} canonical series row(s), then merged {} pass, {} located divergence, and {} unlocated divergence validate observation(s) at {head}",
        projection.cells, rows_read, fold.passed, fold.located, fold.unlocated,
    );
    println!(
        "compatibility scorecard: generated files {}",
        if changed { "changed" } else { "unchanged" }
    );
    if !fold.errored.is_empty() {
        println!(
            "  {} current result row(s) determined no canonical product result; their exact invocation evidence was retained",
            fold.errored.len()
        );
        for row in &fold.errored {
            println!("    determined nothing: {row}");
        }
    }
    for skipped in &projection.skipped {
        println!("  skipped {skipped}");
    }
    if !representation.represented_event_ids.is_empty() {
        println!(
            "  {} series event(s) were represented once by richer direct validation evidence",
            representation.represented_event_ids.len()
        );
    }
    Ok(())
}

/// Keep the generated status-and-measurement section in step with an explicit
/// observation write. Since that section is derived from `cells.json`, writing
/// only the latter would make `check` fail immediately after a successful fold.
fn generated_files(derived: &Derived, tracked: &TrackedCells) -> Result<GeneratedFiles, String> {
    Ok(GeneratedFiles {
        scorecard: format!(
            "{}{}",
            render_scorecard(derived),
            render_measurement_section(tracked)
        )
        .into_bytes(),
        cells: encoded_cells(tracked)?.into_bytes(),
    })
}

fn write_observation_files(
    root: &Path,
    derived: &Derived,
    tracked: &TrackedCells,
) -> Result<(), String> {
    let generated = generated_files(derived, tracked)?;
    fs::write(root.join(SCORECARD), generated.scorecard)
        .map_err(|e| format!("cannot write {SCORECARD}: {e}"))?;
    fs::write(root.join(CELLS), generated.cells)
        .map_err(|e| format!("cannot write {CELLS}: {e}"))?;
    Ok(())
}

fn prepared_replacement(path: &Path, bytes: &[u8]) -> Result<NamedTempFile, String> {
    let mut temporary = NamedTempFile::new_in(path.parent().ok_or("generated file has no parent")?)
        .map_err(|e| format!("cannot prepare replacement for {}: {e}", path.display()))?;
    temporary
        .as_file()
        .set_permissions(fs::metadata(path).map_err(|e| e.to_string())?.permissions())
        .and_then(|()| temporary.write_all(bytes))
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|e| format!("cannot prepare replacement for {}: {e}", path.display()))?;
    Ok(temporary)
}

fn replace_generated_files_with(
    root: &Path,
    original: &GeneratedFiles,
    updated: &GeneratedFiles,
    guard: impl FnOnce() -> Result<(), String>,
    mut before_replace: impl FnMut(usize) -> Result<(), String>,
) -> Result<bool, String> {
    if original == updated {
        guard()?;
        return Ok(false);
    }
    let scorecard = root.join(SCORECARD);
    let cells = root.join(CELLS);
    let new_scorecard = prepared_replacement(&scorecard, &updated.scorecard)?;
    let old_scorecard = prepared_replacement(&scorecard, &original.scorecard)?;
    let new_cells = prepared_replacement(&cells, &updated.cells)?;
    guard()?;
    before_replace(1)?;
    new_scorecard
        .persist(&scorecard)
        .map_err(|e| format!("cannot replace {SCORECARD}: {}", e.error))?;
    let second = before_replace(2).and_then(|()| {
        new_cells
            .persist(&cells)
            .map(|_| ())
            .map_err(|e| format!("cannot replace {CELLS}: {}", e.error))
    });
    if let Err(error) = second {
        return match old_scorecard.persist(&scorecard) {
            Ok(_) => Err(format!("{error}; restored the original generated files")),
            Err(rollback) => {
                let rollback_error = rollback.error;
                match rollback.file.keep() {
                    Ok((_, path)) => Err(format!(
                        "{error}; restoring {SCORECARD} also failed: {rollback_error}; restore it from {}",
                        path.display()
                    )),
                    Err(keep) => Err(format!(
                        "{error}; restoring {SCORECARD} also failed: {rollback_error}; preserving its rollback file also failed: {}",
                        keep.error
                    )),
                }
            }
        };
    }
    Ok(true)
}

#[derive(Debug)]
struct ProjectObservationsOutcome {
    cells: usize,
    rows: usize,
    no_verdict_rows: usize,
    runs: u64,
    represented_rows: usize,
    replaced_observations: usize,
    skipped: Vec<String>,
    pre_series_corpus: bool,
}

#[derive(Clone, Debug)]
struct PreparedSeriesRow {
    cell_index: usize,
    observation_identity: SeriesObservationIdentity,
    provenance: ObservationProvenance,
    event_id: String,
    hermit_sha: String,
    run_id: String,
    attempt: u64,
    result: Option<ObservedResult>,
    no_verdict: bool,
    num_runs: u64,
    main_ancestry: Option<bool>,
    depth: BTreeMap<String, SourceDepth>,
    coordinates: Option<SeriesCoordinates>,
    order: (String, String, u64, String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SeriesEvidence {
    result: Option<ObservedResult>,
    no_verdict: bool,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum SeriesObservationIdentity {
    DetcoreTree(String),
    HermitCommit(String),
}

impl SeriesObservationIdentity {
    fn from_observation(observation: &Observation) -> Result<Self, String> {
        match observation.detcore_tree.as_ref() {
            Some(tree) => Ok(Self::DetcoreTree(tree.clone())),
            None if observation.hermit_shas.len() == 1 => Ok(Self::HermitCommit(
                observation
                    .hermit_shas
                    .iter()
                    .next()
                    .expect("one Hermit SHA exists")
                    .clone(),
            )),
            None => Err(format!(
                "legacy projected observation has {} Hermit identities; expected exactly one",
                observation.hermit_shas.len()
            )),
        }
    }

    fn detcore_tree(&self) -> Option<&str> {
        match self {
            Self::DetcoreTree(tree) => Some(tree),
            Self::HermitCommit(_) => None,
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct DirectEvidenceBase {
    cell: String,
    identity: SeriesObservationIdentity,
    provenance: ObservationProvenance,
    hermit_sha: String,
    run_id: String,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum DirectEvidenceKind {
    Result(ObservedResult),
    ExactInvocation {
        attempt: u64,
        evidence_sha256: String,
        result: Option<ObservedResult>,
    },
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct DirectEvidenceKey {
    base: DirectEvidenceBase,
    kind: DirectEvidenceKind,
}

#[derive(Debug)]
struct DirectRepresentation {
    represented_event_ids: BTreeSet<String>,
    has_unrepresented_direct_evidence: bool,
}

type CurrentResultAttempts = BTreeMap<(DirectEvidenceBase, String), u64>;

fn current_result_attempts(
    validated: &TrackedCells,
    rows: &BTreeMap<CellId, Vec<ResultCandidate>>,
    detcore_tree: &str,
) -> Result<CurrentResultAttempts, String> {
    let mut attempts = CurrentResultAttempts::new();
    for (id, candidates) in rows {
        if !validated.cells.iter().any(|cell| &cell.id == id) {
            continue;
        }
        for candidate in candidates {
            let row = &candidate.row;
            let base = DirectEvidenceBase {
                cell: series_cell_key(id),
                identity: SeriesObservationIdentity::DetcoreTree(detcore_tree.to_string()),
                provenance: ObservationProvenance::Validate,
                hermit_sha: row.hermit_sha.clone(),
                run_id: row.run_id.clone(),
            };
            if row.attempt == 0
                || attempts
                    .insert((base, candidate.evidence_identity.clone()), row.attempt)
                    .is_some_and(|previous| previous != row.attempt)
            {
                return Err(format!(
                    "current result has conflicting outer-attempt bindings for run {}",
                    row.run_id
                ));
            }
        }
    }
    Ok(attempts)
}

fn bound_direct_attempts(
    tracked: &TrackedCells,
    current: &CurrentResultAttempts,
) -> Result<BTreeMap<DirectEvidenceKey, BTreeSet<u64>>, String> {
    let mut bound = BTreeMap::<DirectEvidenceKey, BTreeSet<u64>>::new();
    for cell in &tracked.cells {
        for observation in &cell.observations {
            if !observation.event_ids.is_empty() {
                continue;
            }
            let identity = SeriesObservationIdentity::from_observation(observation)?;
            for comparison in &observation.canonical_comparisons {
                let base = DirectEvidenceBase {
                    cell: series_cell_key(&cell.id),
                    identity: identity.clone(),
                    provenance: observation.provenance,
                    hermit_sha: comparison.hermit_sha.clone(),
                    run_id: comparison.run_id.clone(),
                };
                // A compact historical result does not prove an outer attempt.
                // Bind only the actual canonical digest to validated current
                // input; never infer attempt one from a run/result match.
                if let Some(attempt) =
                    current.get(&(base.clone(), comparison.evidence_sha256.clone()))
                {
                    bound
                        .entry(DirectEvidenceKey {
                            base,
                            kind: DirectEvidenceKind::Result(comparison.result),
                        })
                        .or_default()
                        .insert(*attempt);
                }
            }
        }
    }
    Ok(bound)
}

fn direct_evidence_keys(
    tracked: &TrackedCells,
) -> Result<(BTreeMap<DirectEvidenceKey, usize>, bool), String> {
    let mut counts = BTreeMap::<DirectEvidenceKey, usize>::new();
    let mut opaque = false;
    for cell in &tracked.cells {
        let cell_name = series_cell_key(&cell.id);
        for observation in &cell.observations {
            if !observation.event_ids.is_empty() {
                continue;
            }
            let identity = SeriesObservationIdentity::from_observation(observation)?;
            let mut represented_results = BTreeSet::new();
            let mut units = 0usize;
            for comparison in &observation.canonical_comparisons {
                let key = DirectEvidenceKey {
                    base: DirectEvidenceBase {
                        cell: cell_name.clone(),
                        identity: identity.clone(),
                        provenance: observation.provenance,
                        hermit_sha: comparison.hermit_sha.clone(),
                        run_id: comparison.run_id.clone(),
                    },
                    kind: DirectEvidenceKind::Result(comparison.result),
                };
                *counts.entry(key).or_default() += 1;
                represented_results.insert(comparison.result);
                units += 1;
            }
            for invocation in &observation.invocations {
                if let Some(result) = invocation.result {
                    represented_results.insert(result);
                }
                let base = DirectEvidenceBase {
                    cell: cell_name.clone(),
                    identity: identity.clone(),
                    provenance: observation.provenance,
                    hermit_sha: invocation.hermit_sha.clone(),
                    run_id: invocation.run_id.clone(),
                };
                let kind = match (invocation.attempt, invocation.evidence_sha256.as_ref()) {
                    (Some(attempt), Some(evidence_sha256)) => DirectEvidenceKind::ExactInvocation {
                        attempt,
                        evidence_sha256: evidence_sha256.clone(),
                        result: invocation.result,
                    },
                    (None, None) => {
                        let Some(result) = invocation.result else {
                            opaque = true;
                            continue;
                        };
                        if observation.canonical_comparisons.iter().any(|comparison| {
                            comparison.hermit_sha == invocation.hermit_sha
                                && comparison.run_id == invocation.run_id
                                && comparison.result == result
                        }) {
                            continue;
                        }
                        DirectEvidenceKind::Result(result)
                    }
                    _ => {
                        opaque = true;
                        continue;
                    }
                };
                *counts.entry(DirectEvidenceKey { base, kind }).or_default() += 1;
                units += 1;
            }
            if units == 0 || !observation.results.is_subset(&represented_results) {
                opaque = true;
            }
        }
    }
    // Retained history may contain the same compact key in more than one
    // observation. Keep that multiplicity so the caller can refuse only when
    // a snapshot event actually claims the ambiguous base; unrelated history
    // remains opaque evidence rather than disabling every combined write.
    Ok((counts, opaque))
}

fn source_direct_evidence_key(
    row: &SeriesRow,
    tracked: &TrackedCells,
) -> Result<Option<(DirectEvidenceBase, Option<DirectEvidenceKey>)>, String> {
    if row.validate_for_projection().is_err() {
        return Ok(None);
    }
    let matches = tracked
        .cells
        .iter()
        .filter(|cell| series_cell_key(&cell.id) == row.cell())
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Ok(None);
    }
    let cell = matches[0];
    let Some(evidence) = series_evidence(row, &cell.id) else {
        return Ok(None);
    };
    let base = DirectEvidenceBase {
        cell: row.cell().to_string(),
        identity: series_observation_identity(row)?,
        provenance: series_provenance(row.producer),
        hermit_sha: row.series.tree.clone(),
        run_id: row.run_id.clone(),
    };
    let kind = if evidence.no_verdict {
        let exact = row
            .series
            .no_verdict_evidence
            .as_ref()
            .ok_or("series no-verdict row lost its exact evidence identity")?;
        row.series
            .attempt
            .map(|attempt| DirectEvidenceKind::ExactInvocation {
                attempt,
                evidence_sha256: exact.evidence_sha256.clone(),
                result: evidence.result,
            })
    } else if row.series.num_runs == 1 && row.series.attempt.unwrap_or(1) == 1 {
        evidence.result.map(DirectEvidenceKind::Result)
    } else {
        None
    };
    Ok(Some((
        base.clone(),
        kind.map(|kind| DirectEvidenceKey { base, kind }),
    )))
}

fn direct_representation(
    tracked: &TrackedCells,
    rows: &[SeriesRow],
    current_attempts: &CurrentResultAttempts,
) -> Result<DirectRepresentation, String> {
    let (direct, opaque) = direct_evidence_keys(tracked)?;
    let bound_attempts = bound_direct_attempts(tracked, current_attempts)?;
    let mut source = BTreeMap::<
        DirectEvidenceBase,
        Vec<(Option<DirectEvidenceKey>, String, Option<u64>, u64)>,
    >::new();
    for row in rows {
        let Some((base, key)) = source_direct_evidence_key(row, tracked)? else {
            continue;
        };
        source.entry(base).or_default().push((
            key,
            row.event_id.clone(),
            row.series.attempt,
            row.series.num_runs,
        ));
    }

    // A verdict belonging to another current attempt can be ignored here only
    // when that exact event has a unique, independently bound direct record.
    // An event's claimed attempt alone cannot authorize the legacy projector
    // to absorb it into an unrelated compact result.
    let mut represented_verdict_attempts = BTreeMap::new();
    for (key, count) in &direct {
        if !matches!(key.kind, DirectEvidenceKind::Result(_)) || *count != 1 {
            continue;
        }
        let Some(attempt) = bound_attempts
            .get(key)
            .filter(|attempts| attempts.len() == 1)
            .and_then(|attempts| attempts.first().copied())
        else {
            continue;
        };
        let candidates = source.get(&key.base).map(Vec::as_slice).unwrap_or(&[]);
        let exact = candidates
            .iter()
            .filter(|(candidate, _, source_attempt, num_runs)| {
                candidate.as_ref() == Some(key)
                    && source_attempt.unwrap_or(1) == attempt
                    && *num_runs == 1
            })
            .collect::<Vec<_>>();
        if let [(_, event_id, _, _)] = exact.as_slice() {
            represented_verdict_attempts.insert(event_id.clone(), attempt);
        }
    }

    let mut represented_event_ids = BTreeSet::new();
    let mut represented_direct = BTreeSet::new();
    for (direct_key, direct_count) in &direct {
        let candidates = source
            .get(&direct_key.base)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        if !candidates.is_empty() && *direct_count != 1 {
            return Err(format!(
                "series evidence claims direct run {} at {}, but the retained scorecard has {direct_count} records for that exact identity",
                direct_key.base.run_id, direct_key.base.hermit_sha
            ));
        }
        let direct_attempt = match &direct_key.kind {
            DirectEvidenceKind::ExactInvocation { attempt, .. } => Some(*attempt),
            DirectEvidenceKind::Result(_) => bound_attempts
                .get(direct_key)
                .filter(|attempts| attempts.len() == 1)
                .and_then(|attempts| attempts.first().copied()),
        };
        let candidates = candidates
            .iter()
            .filter(|(candidate, event_id, _, num_runs)| {
                // The projector preserves typed no-verdict rows by event ID,
                // so proven different attempts can remain in that form. Its
                // legacy verdict matcher has no such proof and stays strict.
                let separate_no_verdict = matches!(
                    (direct_attempt, candidate.as_ref().map(|key| &key.kind)),
                    (Some(direct), Some(DirectEvidenceKind::ExactInvocation { attempt, .. }))
                        if direct != *attempt && *num_runs == 1
                );
                let separately_represented_verdict = direct_attempt.is_some_and(|direct| {
                    represented_verdict_attempts
                        .get(event_id)
                        .is_some_and(|attempt| *attempt != direct)
                });
                !separate_no_verdict && !separately_represented_verdict
            })
            .collect::<Vec<_>>();
        for (candidate, _, attempt, _) in &candidates {
            if candidate.as_ref() != Some(direct_key)
                || direct_attempt.is_some_and(|direct| direct != attempt.unwrap_or(1))
            {
                return Err(format!(
                    "series evidence for run {} at {} disagrees with its exact direct result or outer attempt",
                    direct_key.base.run_id, direct_key.base.hermit_sha
                ));
            }
        }
        let exact = candidates
            .iter()
            .filter(|(candidate, _, _, _)| candidate.as_ref() == Some(direct_key))
            .collect::<Vec<_>>();
        match exact.as_slice() {
            [] if candidates.is_empty() => {}
            [] => {
                return Err(format!(
                    "series evidence for run {} at {} disagrees with its exact direct result",
                    direct_key.base.run_id, direct_key.base.hermit_sha
                ));
            }
            [(_, event_id, _, _)] => {
                if !represented_event_ids.insert(event_id.clone()) {
                    return Err(format!(
                        "series event {event_id} maps to more than one exact direct result"
                    ));
                }
                represented_direct.insert(direct_key.clone());
            }
            _ => {
                return Err(format!(
                    "series evidence cannot map one-to-one to direct run {} at {}: {} source rows match",
                    direct_key.base.run_id,
                    direct_key.base.hermit_sha,
                    exact.len()
                ));
            }
        }
    }
    Ok(DirectRepresentation {
        represented_event_ids,
        has_unrepresented_direct_evidence: opaque || represented_direct.len() != direct.len(),
    })
}

fn is_object_id(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn deserialize_unique_event_ids<'de, D>(deserializer: D) -> Result<BTreeSet<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let values = Vec::<String>::deserialize(deserializer)?;
    if values.iter().any(|value| value.trim().is_empty()) {
        return Err(serde::de::Error::custom(
            "projected observation event_ids must be nonempty",
        ));
    }
    let unique = values.iter().cloned().collect::<BTreeSet<_>>();
    if unique.len() != values.len() {
        return Err(serde::de::Error::custom(
            "projected observation event_ids must be unique",
        ));
    }
    Ok(unique)
}

fn require_product_observation_results(results: &BTreeSet<ObservedResult>) -> Result<(), String> {
    if let Some(result) = results.iter().find(|result| {
        matches!(
            result,
            ObservedResult::SandboxDenied | ObservedResult::InfrastructureError
        )
    }) {
        return Err(format!(
            "tracked scorecard observation cannot contain non-product result `{}`",
            result.as_str()
        ));
    }
    Ok(())
}

fn deserialize_product_observation_results<'de, D>(
    deserializer: D,
) -> Result<BTreeSet<ObservedResult>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let results = BTreeSet::<ObservedResult>::deserialize(deserializer)?;
    require_product_observation_results(&results).map_err(serde::de::Error::custom)?;
    Ok(results)
}

fn validate_observation_identity_namespace(cells: &TrackedCells) -> Result<(), String> {
    let permits_projected_identity = cells.schema >= 7
        && cells.projection.as_ref().is_some_and(|projection| {
            projection.source_commit.is_some() && projection.source_tree.is_some()
        });
    let mut seen_event_ids = BTreeSet::new();
    for cell in &cells.cells {
        let id = display_id(&cell.id);
        for observation in &cell.observations {
            require_product_observation_results(&observation.results)?;
            let projected = !observation.event_ids.is_empty();
            if (projected || observation.detcore_tree.is_none()) && !permits_projected_identity {
                return Err(format!(
                    "{id} uses projected identity without scorecard schema 7 and complete source identity"
                ));
            }
            if projected
                && (!observation.canonical_comparisons.is_empty()
                    || !observation.invocations.is_empty())
            {
                return Err(format!(
                    "{id} mixes projector-owned event_ids with independent evidence"
                ));
            }
            for event_id in &observation.event_ids {
                if event_id.trim().is_empty() || !seen_event_ids.insert(event_id) {
                    return Err(format!(
                        "invalid or repeated projected event_id {event_id:?}"
                    ));
                }
            }
            if observation.detcore_tree.is_none()
                && (!projected
                    || observation.hermit_shas.len() != 1
                    || !is_object_id(observation.hermit_shas.iter().next().unwrap()))
            {
                return Err(format!(
                    "{id} has no Detcore tree and not exactly one valid recorded Hermit identity"
                ));
            }
            for invocation in &observation.invocations {
                match (invocation.attempt, invocation.evidence_sha256.as_deref()) {
                    (None, None) if invocation.result.is_some() => {}
                    (Some(attempt), Some(evidence_sha256)) if attempt > 0 => {
                        require_sha256("validate-row evidence", evidence_sha256)
                            .map_err(|error| format!("{id} {error}"))?;
                    }
                    (None, None) => {
                        return Err(format!(
                            "{id} has a result-less invocation without exact outer-attempt evidence identity"
                        ));
                    }
                    _ => {
                        return Err(format!(
                            "{id} has an incomplete validate-row invocation identity"
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

fn deserialize_optional_object_id<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    if let Some(value) = &value {
        if !is_object_id(value) {
            return Err(serde::de::Error::custom(format!(
                "source_commit must be a lowercase 40-hex object id, got {value:?}"
            )));
        }
    }
    Ok(value)
}

fn deserialize_optional_source_tree<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    if let Some(value) = &value {
        if !is_object_id(value) {
            return Err(serde::de::Error::custom(format!(
                "source_tree must be a lowercase 40-hex object id, got {value:?}"
            )));
        }
    }
    Ok(value)
}

fn series_provenance(producer: SeriesProducer) -> ObservationProvenance {
    match producer {
        SeriesProducer::HermitRepeat => ObservationProvenance::HermitRepeat,
        SeriesProducer::PressureTest => ObservationProvenance::PressureTest,
        SeriesProducer::Validate => ObservationProvenance::Validate,
    }
}

fn is_naked_native(id: &CellId) -> bool {
    id.mode == "naked" && id.backend == "native"
}

fn series_evidence(row: &SeriesRow, id: &CellId) -> Option<SeriesEvidence> {
    // Native attempt/diversity evidence belongs to the canonical series. The
    // legacy cells.json observation shape cannot represent it without losing
    // facts, so no native outcome is projected through this fold.
    if is_naked_native(id) {
        return None;
    }
    if let Some(evidence) = &row.series.no_verdict_evidence {
        return Some(SeriesEvidence {
            result: evidence
                .attempts
                .iter()
                .any(|attempt| attempt.timed_out)
                .then_some(ObservedResult::Timeout),
            no_verdict: true,
        });
    }
    if row.schema == SeriesSchema::V3 {
        return match row.series.result {
            Some(
                result @ (ObservedResult::Pass
                | ObservedResult::DeterminismFailure
                | ObservedResult::ParityFailure
                | ObservedResult::ReplayFailure),
            ) => Some(SeriesEvidence {
                result: Some(result),
                no_verdict: false,
            }),
            Some(
                ObservedResult::CrashError
                | ObservedResult::Timeout
                | ObservedResult::Oom
                | ObservedResult::SandboxDenied
                | ObservedResult::InfrastructureError,
            )
            | None => None,
        };
    }
    let result = match (row.series.outcome, id.mode.as_str()) {
        (SeriesOutcome::Passed, _) => Some(ObservedResult::Pass),
        (SeriesOutcome::Diverged, "replay") => Some(ObservedResult::ReplayFailure),
        (SeriesOutcome::Diverged, _) => Some(ObservedResult::DeterminismFailure),
        (
            SeriesOutcome::NoResult
            | SeriesOutcome::Timeout
            | SeriesOutcome::Errored
            | SeriesOutcome::Skipped,
            _,
        ) => None,
    }?;
    Some(SeriesEvidence {
        result: Some(result),
        no_verdict: false,
    })
}

fn series_observation_identity(row: &SeriesRow) -> Result<SeriesObservationIdentity, String> {
    match &row.series.detcore_tree {
        Some(tree) if is_object_id(tree) => {
            Ok(SeriesObservationIdentity::DetcoreTree(tree.clone()))
        }
        Some(tree) => Err(format!(
            "detcore tree must be a lowercase 40-hex object id, got {tree:?}"
        )),
        None if is_object_id(&row.series.tree) => Ok(SeriesObservationIdentity::HermitCommit(
            row.series.tree.clone(),
        )),
        None => Err(format!(
            "row records neither a valid detcore_tree nor a valid Hermit tree identity: {:?}",
            row.series.tree
        )),
    }
}

/// Reimporting the same retained code identity must preserve its recorded stamp,
/// even when a different checkout can resolve more or fewer repository depths.
fn should_replace_retained_last_tested(
    previous: Option<&LastTested>,
    hermit_sha: &str,
    detcore_tree: &str,
    main_ancestry: Option<bool>,
    depth: &BTreeMap<String, SourceDepth>,
) -> bool {
    if previous.is_some_and(|previous| {
        previous.hermit_sha == hermit_sha && previous.detcore_tree == detcore_tree
    }) {
        return false;
    }
    should_replace_last_tested_at(previous, hermit_sha, main_ancestry, depth)
}

fn should_replace_last_tested_at(
    previous: Option<&LastTested>,
    hermit_sha: &str,
    main_ancestry: Option<bool>,
    depth: &BTreeMap<String, SourceDepth>,
) -> bool {
    let Some(previous) = previous else {
        return true;
    };
    if previous.hermit_sha == hermit_sha {
        return true;
    }
    if main_ancestry == Some(false) {
        return false;
    }
    let previous_depth = previous
        .depth
        .get("hermit")
        .map(|depth| (depth.first_parent, depth.commits));
    let candidate_depth = depth
        .get("hermit")
        .map(|depth| (depth.first_parent, depth.commits));
    match (previous_depth, candidate_depth) {
        (None, Some(_)) => true,
        (Some(previous), Some(candidate)) => candidate > previous,
        _ => false,
    }
}

fn should_replace_last_tested(previous: Option<&LastTested>, row: &PreparedSeriesRow) -> bool {
    should_replace_last_tested_at(previous, &row.hermit_sha, row.main_ancestry, &row.depth)
}

fn remove_replaceable_projected_observations(
    tracked: &mut TrackedCells,
    rows: &[SeriesRow],
    projection_source: Option<&str>,
) -> Result<usize, String> {
    let owned = tracked
        .cells
        .iter()
        .flat_map(|cell| cell.observations.iter())
        .filter(|observation| !observation.event_ids.is_empty())
        .count();
    if owned == 0 {
        return Ok(0);
    }
    let Some(source) = projection_source else {
        return Err(
            "projected observations carry event_ids, but the caller supplied no immutable source metadata"
                .into(),
        );
    };
    let projection = tracked
        .projection
        .as_ref()
        .ok_or("projected observations carry event_ids without recorded projection metadata")?;
    if tracked.schema < 7 || projection.source_commit.is_none() || projection.source_tree.is_none()
    {
        return Err(
            "projected observations carry event_ids without scorecard schema 7 and complete source identity"
                .into(),
        );
    }
    if projection.source != source {
        return Err(format!(
            "projected observations belong to source {:?}, not requested source {source:?}",
            projection.source
        ));
    }
    let current_event_ids = rows
        .iter()
        .map(|row| row.event_id.as_str())
        .collect::<BTreeSet<_>>();
    if let Some((cell, event_id)) = tracked.cells.iter().find_map(|cell| {
        cell.observations.iter().find_map(|observation| {
            observation
                .event_ids
                .iter()
                .find(|event_id| !current_event_ids.contains(event_id.as_str()))
                .map(|event_id| (cell, event_id))
        })
    }) {
        return Err(format!(
            "{} has projector-owned event_id {event_id:?} absent from source {source:?}",
            display_id(&cell.id)
        ));
    }
    for cell in &mut tracked.cells {
        cell.observations
            .retain(|observation| observation.event_ids.is_empty());
    }
    Ok(owned)
}

fn apply_series_rows(
    ambient_git_root: &Path,
    tracked: &mut TrackedCells,
    rows: &[SeriesRow],
    projection_source: Option<&str>,
) -> Result<ProjectObservationsOutcome, String> {
    let mut updated = tracked.clone();
    let outcome = apply_series_rows_inner(ambient_git_root, &mut updated, rows, projection_source)?;
    *tracked = updated;
    Ok(outcome)
}

fn apply_series_rows_inner(
    _ambient_git_root: &Path,
    tracked: &mut TrackedCells,
    rows: &[SeriesRow],
    projection_source: Option<&str>,
) -> Result<ProjectObservationsOutcome, String> {
    validate_observation_identity_namespace(tracked)?;
    let replaced_observations =
        remove_replaceable_projected_observations(tracked, rows, projection_source)?;
    let mut cell_indices: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (index, cell) in tracked.cells.iter().enumerate() {
        cell_indices
            .entry(series_cell_key(&cell.id))
            .or_default()
            .push(index);
    }

    let mut prepared = Vec::new();
    let mut skipped = Vec::new();
    for row in rows {
        let label = row.label();
        if let Err(why) = row.validate_for_projection() {
            if row.series.no_verdict_evidence.is_some() {
                return Err(format!("{label}: {why}"));
            }
            skipped.push(format!("{label}: {why}"));
            continue;
        }
        let Some(indices) = cell_indices.get(row.cell()) else {
            skipped.push(format!(
                "{label}: no exact test/mode/backend match in the tracked manifest"
            ));
            continue;
        };
        if indices.len() != 1 {
            skipped.push(format!(
                "{label}: test/mode/backend matches {} tracked cells instead of exactly one",
                indices.len()
            ));
            continue;
        }
        let cell_index = indices[0];
        let cell = &tracked.cells[cell_index];
        let provenance = series_provenance(row.producer);
        let evidence = match series_evidence(row, &cell.id) {
            Some(evidence) => evidence,
            None => {
                if is_naked_native(&cell.id) {
                    skipped.push(format!(
                        "{label}: naked/native evidence is canonical-series-only and is not projected into legacy cells.json observations"
                    ));
                } else {
                    skipped.push(format!(
                        "{label}: outcome {:?} produced no comparison and carries no exact no-verdict evidence",
                        row.series.outcome.as_str()
                    ));
                }
                continue;
            }
        };
        let observation_identity = match series_observation_identity(row) {
            Ok(identity) => identity,
            Err(why) => {
                skipped.push(format!("{label}: {why}"));
                continue;
            }
        };
        prepared.push(PreparedSeriesRow {
            cell_index,
            observation_identity,
            provenance,
            event_id: row.event_id.clone(),
            hermit_sha: row.series.tree.clone(),
            run_id: row.run_id.clone(),
            attempt: row.series.attempt.unwrap_or(1),
            result: evidence.result,
            no_verdict: evidence.no_verdict,
            num_runs: row.series.num_runs,
            main_ancestry: row.series.main_ancestry,
            depth: row.series.depth.clone(),
            coordinates: row.series.coordinates.clone(),
            order: (
                row.emitted_at.clone(),
                row.run_id.clone(),
                row.series.run_index,
                row.event_id.clone(),
            ),
        });
    }

    if !rows.is_empty() && prepared.is_empty() {
        return Err(format!(
            "every one of the {} readable series row(s) determined nothing, so the projection was not written:\n{}",
            rows.len(),
            skipped
                .iter()
                .map(|line| format!("  skipped {line}"))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }

    let admitted_rows = prepared.len();
    let no_verdict_rows = prepared.iter().filter(|row| row.no_verdict).count();
    let admitted_runs = prepared.iter().map(|row| row.num_runs).sum();
    let admitted_cells = prepared
        .iter()
        .map(|row| row.cell_index)
        .collect::<BTreeSet<_>>();
    let mut preserved = BTreeMap::new();
    for (cell_index, cell) in tracked.cells.iter().enumerate() {
        for observation in &cell.observations {
            if !observation.event_ids.is_empty() {
                continue;
            }
            let mut runs = BTreeMap::<(String, String), BTreeSet<ObservedResult>>::new();
            for (hermit_sha, run_id, result) in observation
                .canonical_comparisons
                .iter()
                .map(|row| (&row.hermit_sha, &row.run_id, row.result))
                .chain(observation.invocations.iter().filter_map(|row| {
                    row.result
                        .map(|result| (&row.hermit_sha, &row.run_id, result))
                }))
            {
                runs.entry((hermit_sha.clone(), run_id.clone()))
                    .or_default()
                    .insert(result);
            }
            for ((hermit_sha, run_id), results) in runs {
                let entry = preserved
                    .entry((cell_index, observation.provenance, hermit_sha, run_id))
                    .or_insert_with(|| (0usize, BTreeSet::new()));
                entry.0 += 1;
                entry.1.extend(results);
            }
        }
    }
    let source_counts = prepared.iter().fold(BTreeMap::new(), |mut counts, row| {
        *counts
            .entry((
                row.cell_index,
                row.provenance,
                row.hermit_sha.clone(),
                row.run_id.clone(),
            ))
            .or_insert(0usize) += 1;
        counts
    });
    let mut represented_rows = 0usize;
    let mut to_project = Vec::new();
    for row in prepared.iter().cloned() {
        // Projector-owned no-verdict rows carry immutable event IDs instead of
        // the direct writer's literal invocation. Do not conflate the two
        // storage forms or suppress the series event by a run-level match.
        if row.no_verdict {
            to_project.push(row);
            continue;
        }
        let key = (
            row.cell_index,
            row.provenance,
            row.hermit_sha.clone(),
            row.run_id.clone(),
        );
        let Some((matches, results)) = preserved.get(&key) else {
            to_project.push(row);
            continue;
        };
        if row.attempt != 1 {
            return Err(format!(
                "series event {} maps to existing run {} at Hermit {}, but records outer attempt {}; preserved evidence does not retain that attempt identity",
                row.event_id, row.run_id, row.hermit_sha, row.attempt
            ));
        }
        let source_rows = source_counts[&key];
        if *matches != 1 || source_rows != 1 || row.num_runs != 1 {
            return Err(format!(
                "source event {} cannot map one-to-one to preserved run {} attempt {} at Hermit {}: source_rows={} matching_observations={} num_runs={}",
                row.event_id,
                row.run_id,
                row.attempt,
                row.hermit_sha,
                source_rows,
                matches,
                row.num_runs
            ));
        }
        let result = row
            .result
            .expect("comparison series evidence always carries a result");
        if results != &BTreeSet::from([result]) {
            return Err(format!(
                "series event {} maps to existing run {} at Hermit {}, but results {results:?} disagree with {:?}",
                row.event_id, row.run_id, row.hermit_sha, result
            ));
        }
        represented_rows += 1;
    }
    let mut prepared = to_project;

    prepared.sort_by(|left, right| left.order.cmp(&right.order));
    let mut grouped =
        BTreeMap::<(usize, SeriesObservationIdentity, ObservationProvenance), Observation>::new();
    let mut latest_by_cell: BTreeMap<usize, usize> = BTreeMap::new();
    for (prepared_index, row) in prepared.iter().enumerate() {
        let observation = grouped
            .entry((
                row.cell_index,
                row.observation_identity.clone(),
                row.provenance,
            ))
            .or_insert_with(|| Observation {
                detcore_tree: row.observation_identity.detcore_tree().map(str::to_string),
                event_ids: BTreeSet::new(),
                provenance: row.provenance,
                depth: BTreeMap::new(),
                hermit_shas: BTreeSet::new(),
                results: BTreeSet::new(),
                canonical_comparisons: BTreeSet::new(),
                invocations: BTreeSet::new(),
                first_divergent_scheduler_turn: ObservedPositions::default(),
                first_divergent_virtual_nanoseconds: ObservedPositions::default(),
                first_divergent_record: ObservedPositions::default(),
                first_divergent_syscall: ObservedPositions::default(),
            });
        observation.event_ids.insert(row.event_id.clone());
        observation.depth = row.depth.clone();
        observation.hermit_shas.insert(row.hermit_sha.clone());
        if let Some(result) = row.result {
            observation.results.insert(result);
        }
        for _ in 0..row.num_runs {
            observation.first_divergent_scheduler_turn.record(
                row.coordinates
                    .as_ref()
                    .and_then(|coordinates| coordinates.first_divergent_scheduler_turn),
            );
            observation.first_divergent_virtual_nanoseconds.record(
                row.coordinates
                    .as_ref()
                    .and_then(|coordinates| coordinates.first_divergent_virtual_nanoseconds),
            );
            observation.first_divergent_record.record(
                row.coordinates
                    .as_ref()
                    .and_then(|coordinates| coordinates.first_divergent_record),
            );
            observation.first_divergent_syscall.record(
                row.coordinates
                    .as_ref()
                    .and_then(|coordinates| coordinates.first_divergent_syscall),
            );
        }
        latest_by_cell.insert(row.cell_index, prepared_index);
    }

    for ((cell_index, _, _), observation) in grouped {
        let cell = &mut tracked.cells[cell_index];
        cell.observations.push(observation);
        cell.observations.sort_by(|left, right| {
            left.detcore_tree
                .cmp(&right.detcore_tree)
                .then(left.provenance.cmp(&right.provenance))
                .then(left.event_ids.cmp(&right.event_ids))
        });
    }

    for (cell_index, prepared_index) in latest_by_cell {
        let row = &prepared[prepared_index];
        let Some(detcore_tree) = row.observation_identity.detcore_tree() else {
            continue;
        };
        if should_replace_last_tested(tracked.cells[cell_index].last_tested.as_ref(), row) {
            tracked.cells[cell_index].last_tested = Some(LastTested {
                hermit_sha: row.hermit_sha.clone(),
                detcore_tree: detcore_tree.to_string(),
                depth: row.depth.clone(),
            });
        }
    }

    let pre_series_corpus = rows.is_empty()
        || tracked.cells.iter().any(|cell| {
            cell.observations
                .iter()
                .any(|observation| observation.event_ids.is_empty())
        });

    Ok(ProjectObservationsOutcome {
        cells: admitted_cells.len(),
        rows: admitted_rows,
        no_verdict_rows,
        runs: admitted_runs,
        represented_rows,
        replaced_observations,
        skipped,
        pre_series_corpus,
    })
}

#[derive(Debug)]
struct SeriesSourceSnapshot {
    source: String,
    source_commit: String,
    source_tree: String,
    shards: Vec<SeriesSourceShard>,
}

#[derive(Debug)]
struct SeriesSourceShard {
    display_path: PathBuf,
    bytes: Vec<u8>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScorecardSeriesSnapshot {
    schema: String,
    source: ScorecardSeriesSnapshotSource,
    rows_read: u64,
    rows_sha256: String,
    rows: Vec<Box<RawValue>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScorecardSeriesSnapshotSource {
    path: String,
    commit: String,
    tree: String,
    published_rows: u64,
    local_sources: Vec<ScorecardSeriesSnapshotLocalSource>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScorecardSeriesSnapshotLocalSource {
    kind: String,
    path: String,
    device: u64,
    inode: u64,
    rows: u64,
    size: u64,
    sha256: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SnapshotPathIdentity {
    device: u64,
    inode: u64,
    mode: u32,
    links: u64,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

impl SnapshotPathIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            mode: metadata.mode(),
            links: metadata.nlink(),
            size: metadata.size(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SnapshotDirectoryIdentity {
    device: u64,
    inode: u64,
    mode: u32,
}

impl SnapshotDirectoryIdentity {
    fn from_path_identity(identity: SnapshotPathIdentity) -> Self {
        // Creating or renaming a sibling output changes directory timestamps
        // and size without replacing the held directory. The snapshot file
        // itself still uses the complete metadata identity and byte digest.
        Self {
            device: identity.device,
            inode: identity.inode,
            mode: identity.mode,
        }
    }
}

struct HeldScorecardSeriesSnapshot {
    path: PathBuf,
    parent: PathBuf,
    _parent_file: File,
    file: File,
    parent_identity: SnapshotDirectoryIdentity,
    file_identity: SnapshotPathIdentity,
    sha256: String,
    snapshot: ScorecardSeriesSnapshot,
    rows: Vec<SeriesRow>,
}

struct UniqueJsonFields;

impl<'de> Deserialize<'de> for UniqueJsonFields {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(Self)
    }
}

impl<'de> serde::de::Visitor<'de> for UniqueJsonFields {
    type Value = Self;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("JSON with unique fields in every object")
    }

    fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self, E> {
        Ok(self)
    }

    fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Self, E> {
        Ok(self)
    }

    fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Self, E> {
        Ok(self)
    }

    fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Self, E> {
        Ok(self)
    }

    fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Self, E> {
        Ok(self)
    }

    fn visit_unit<E: serde::de::Error>(self) -> Result<Self, E> {
        Ok(self)
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut sequence: A) -> Result<Self, A::Error> {
        while sequence.next_element::<Self>()?.is_some() {}
        Ok(self)
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self, A::Error> {
        let mut fields = BTreeSet::new();
        while let Some(field) = map.next_key::<String>()? {
            if !fields.insert(field.clone()) {
                return Err(serde::de::Error::custom(format!(
                    "duplicate JSON field {field:?}"
                )));
            }
            map.next_value::<Self>()?;
        }
        Ok(self)
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn snapshot_path_identity(path: &Path, kind: &str) -> Result<SnapshotPathIdentity, String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        format!(
            "cannot inspect scorecard snapshot {kind} {}: {error}",
            path.display()
        )
    })?;
    let valid = match kind {
        "directory" => metadata.is_dir(),
        "file" => metadata.is_file(),
        _ => false,
    };
    if !valid {
        return Err(format!(
            "scorecard snapshot {kind} is not a regular {kind}: {}",
            path.display()
        ));
    }
    Ok(SnapshotPathIdentity::from_metadata(&metadata))
}

fn read_held_snapshot_file(file: &File, identity: SnapshotPathIdentity) -> Result<Vec<u8>, String> {
    let size = usize::try_from(identity.size)
        .map_err(|_| "scorecard snapshot is too large to address".to_string())?;
    let mut bytes = vec![0; size];
    let mut offset = 0usize;
    while offset < bytes.len() {
        let read = UnixFileExt::read_at(file, &mut bytes[offset..], offset as u64)
            .map_err(|error| format!("cannot read held scorecard snapshot: {error}"))?;
        if read == 0 {
            return Err("scorecard snapshot changed size while being read".into());
        }
        offset += read;
    }
    let mut extra = [0u8; 1];
    if UnixFileExt::read_at(file, &mut extra, identity.size)
        .map_err(|error| format!("cannot finish reading held scorecard snapshot: {error}"))?
        != 0
    {
        return Err("scorecard snapshot changed size while being read".into());
    }
    Ok(bytes)
}

fn canonical_snapshot_rows_bytes(rows: &[JsonValue]) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    for row in rows {
        serde_json::to_writer(&mut bytes, row)
            .map_err(|error| format!("cannot encode canonical scorecard snapshot row: {error}"))?;
        bytes.push(b'\n');
    }
    Ok(bytes)
}

fn canonical_snapshot_raw_rows_bytes(rows: &[Box<RawValue>]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for row in rows {
        bytes.extend_from_slice(row.get().as_bytes());
        bytes.push(b'\n');
    }
    bytes
}

fn require_exact_object_keys(
    value: &JsonValue,
    expected: &[&str],
    label: &str,
) -> Result<(), String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("{label} must be an object"))?;
    let actual = object.keys().map(String::as_str).collect::<BTreeSet<_>>();
    let expected = expected.iter().copied().collect::<BTreeSet<_>>();
    if actual != expected {
        return Err(format!("{label} has an invalid field set"));
    }
    Ok(())
}

fn validate_scorecard_snapshot(
    snapshot: &ScorecardSeriesSnapshot,
    display: &Path,
) -> Result<Vec<SeriesRow>, String> {
    if snapshot.schema != SCORECARD_SERIES_SNAPSHOT_SCHEMA {
        return Err(format!(
            "scorecard snapshot {} has schema {:?}, expected {SCORECARD_SERIES_SNAPSHOT_SCHEMA}",
            display.display(),
            snapshot.schema
        ));
    }
    if snapshot.source.path != SCORECARD_SERIES_SNAPSHOT_SOURCE
        || !is_object_id(&snapshot.source.commit)
        || !is_object_id(&snapshot.source.tree)
    {
        return Err(format!(
            "scorecard snapshot {} has an invalid source identity",
            display.display()
        ));
    }
    if snapshot.source.published_rows > snapshot.rows_read {
        return Err(format!(
            "scorecard snapshot {} reports more published rows than canonical rows",
            display.display()
        ));
    }

    let mut previous_source: Option<(u8, &str)> = None;
    let mut source_paths = BTreeSet::new();
    let mut declared_source_facts = (0u64, 0u64, 0u64);
    for source in &snapshot.source.local_sources {
        let kind_order = match source.kind.as_str() {
            "live" => 0,
            "published-retained" => 1,
            _ => {
                return Err(format!(
                    "scorecard snapshot {} has an invalid local source kind {:?}",
                    display.display(),
                    source.kind
                ));
            }
        };
        let relative = Path::new(&source.path);
        let components = relative.components().count();
        let valid_path = !source.path.is_empty()
            && !relative.is_absolute()
            && !relative
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
            && match source.kind.as_str() {
                "live" => components == 1,
                "published-retained" => {
                    components == 2
                        && relative.components().next().is_some_and(|component| {
                            component.as_os_str() == std::ffi::OsStr::new("published")
                        })
                }
                _ => false,
            };
        if !valid_path
            || source.inode == 0
            || !is_sha256(&source.sha256)
            || !source_paths.insert(source.path.as_str())
        {
            return Err(format!(
                "scorecard snapshot {} has an invalid local source",
                display.display()
            ));
        }
        let current = (kind_order, source.path.as_str());
        if previous_source.is_some_and(|previous| previous >= current) {
            return Err(format!(
                "scorecard snapshot {} local sources are not in canonical order",
                display.display()
            ));
        }
        declared_source_facts.0 = declared_source_facts
            .0
            .checked_add(source.device)
            .ok_or("scorecard snapshot local-source device total overflowed")?;
        declared_source_facts.1 = declared_source_facts
            .1
            .checked_add(source.rows)
            .ok_or("scorecard snapshot local-source row total overflowed")?;
        declared_source_facts.2 = declared_source_facts
            .2
            .checked_add(source.size)
            .ok_or("scorecard snapshot local-source byte total overflowed")?;
        previous_source = Some(current);
    }

    if snapshot.rows_read != snapshot.rows.len() as u64 {
        return Err(format!(
            "scorecard snapshot {} rows_read does not match its rows",
            display.display()
        ));
    }
    if !is_sha256(&snapshot.rows_sha256) {
        return Err(format!(
            "scorecard snapshot {} has an invalid rows_sha256",
            display.display()
        ));
    }
    // The producer defines rows_sha256 over its exact canonical UTF-8 JSON
    // bytes, one row plus one newline. Preserve those bytes through RawValue:
    // reserializing through Rust would silently change valid Python spellings
    // such as exponent-form floats and would verify a different contract.
    let rows_bytes = canonical_snapshot_raw_rows_bytes(&snapshot.rows);
    let rows_sha256 = format!("{:x}", Sha256::digest(&rows_bytes));
    if rows_sha256 != snapshot.rows_sha256 {
        return Err(format!(
            "scorecard snapshot {} row digest does not match its rows",
            display.display()
        ));
    }

    const ROW_FIELDS: &[&str] = &[
        "schema",
        "event_id",
        "event_type",
        "emitted_at",
        "team",
        "host",
        "producer",
        "run_id",
        "series",
    ];
    const SERIES_FIELDS: &[&str] = &[
        "cell",
        "tree",
        "detcore_tree",
        "outcome",
        "result",
        "failure_class",
        "no_verdict_evidence",
        "pressure_evidence",
        "run_index",
        "attempt",
        "num_runs",
        "last_run_index",
        "main_ancestry",
        "runtime",
        "source_tree_dirty",
        "depth",
        "coordinates",
        "first_divergent_messages",
        "machine_shortname",
        "kernel_version",
        "host_capabilities",
    ];
    let allowed_series_fields = SERIES_FIELDS.iter().copied().collect::<BTreeSet<_>>();
    let mut rows = Vec::with_capacity(snapshot.rows.len());
    let mut event_ids = BTreeSet::new();
    let mut previous_order: Option<(String, String)> = None;
    for (index, raw) in snapshot.rows.iter().enumerate() {
        let label = format!("scorecard snapshot {} row {}", display.display(), index + 1);
        let value: JsonValue = serde_json::from_str(raw.get())
            .map_err(|error| format!("{label} is malformed: {error}"))?;
        require_exact_object_keys(&value, ROW_FIELDS, &label)?;
        let series = value
            .get("series")
            .and_then(JsonValue::as_object)
            .ok_or_else(|| format!("{label} series must be an object"))?;
        if series
            .keys()
            .map(String::as_str)
            .any(|field| !allowed_series_fields.contains(field))
        {
            return Err(format!("{label} series has an unknown field"));
        }
        let mut row: SeriesRow = serde_json::from_value(value)
            .map_err(|error| format!("{label} is malformed: {error}"))?;
        row.validate_for_read()
            .map_err(|error| format!("{label} is invalid: {error}"))?;
        if !event_ids.insert(row.event_id.clone()) {
            return Err(format!(
                "scorecard snapshot {} repeats event_id {:?}",
                display.display(),
                row.event_id
            ));
        }
        let order = (row.emitted_at.clone(), row.event_id.clone());
        if previous_order
            .as_ref()
            .is_some_and(|previous| previous >= &order)
        {
            return Err(format!(
                "scorecard snapshot {} rows are not in canonical emitted_at/event_id order",
                display.display()
            ));
        }
        previous_order = Some(order);
        row.source = format!("{}:rows[{}]", display.display(), index);
        rows.push(row);
    }
    Ok(rows)
}

impl HeldScorecardSeriesSnapshot {
    fn open(path: &Path, expected_sha256: &str) -> Result<Self, String> {
        if !is_sha256(expected_sha256) {
            return Err(
                "--snapshot-sha256 requires exactly 64 lowercase hexadecimal characters".into(),
            );
        }
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let parent_file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(&parent)
            .map_err(|error| {
                format!(
                    "cannot open scorecard snapshot parent without following links {}: {error}",
                    parent.display()
                )
            })?;
        let parent_identity = SnapshotDirectoryIdentity::from_path_identity(
            SnapshotPathIdentity::from_metadata(&parent_file.metadata().map_err(|error| {
                format!("cannot inspect held scorecard snapshot parent: {error}")
            })?),
        );
        if SnapshotDirectoryIdentity::from_path_identity(snapshot_path_identity(
            &parent,
            "directory",
        )?) != parent_identity
        {
            return Err(format!(
                "scorecard snapshot parent changed while being opened: {}",
                parent.display()
            ));
        }
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .map_err(|error| {
                format!(
                    "cannot open regular scorecard snapshot without following links {}: {error}",
                    path.display()
                )
            })?;
        let metadata = file
            .metadata()
            .map_err(|error| format!("cannot inspect held scorecard snapshot: {error}"))?;
        if !metadata.is_file() {
            return Err(format!(
                "scorecard snapshot is not a regular file: {}",
                path.display()
            ));
        }
        let file_identity = SnapshotPathIdentity::from_metadata(&metadata);
        if snapshot_path_identity(path, "file")? != file_identity {
            return Err(format!(
                "scorecard snapshot pathname changed while being opened: {}",
                path.display()
            ));
        }
        let payload = read_held_snapshot_file(&file, file_identity)?;
        let actual_sha256 = format!("{:x}", Sha256::digest(&payload));
        if actual_sha256 != expected_sha256 {
            return Err(format!(
                "scorecard snapshot digest mismatch: expected {expected_sha256}, got {actual_sha256}"
            ));
        }
        // Validate original bytes before any Value conversion can collapse
        // duplicate row, series, or nested detail fields. RawValue still owns
        // the producer's exact byte spelling for the canonical-row digest.
        serde_json::from_slice::<UniqueJsonFields>(&payload)
            .map_err(|error| format!("scorecard snapshot is not valid strict JSON: {error}"))?;
        let snapshot: ScorecardSeriesSnapshot = serde_json::from_slice(&payload)
            .map_err(|error| format!("scorecard snapshot is not valid strict JSON: {error}"))?;
        let rows = validate_scorecard_snapshot(&snapshot, path)?;
        let held = Self {
            path: path.to_path_buf(),
            parent,
            _parent_file: parent_file,
            file,
            parent_identity,
            file_identity,
            sha256: actual_sha256,
            snapshot,
            rows,
        };
        held.verify()?;
        Ok(held)
    }

    fn verify(&self) -> Result<(), String> {
        if SnapshotDirectoryIdentity::from_path_identity(SnapshotPathIdentity::from_metadata(
            &self._parent_file.metadata().map_err(|error| {
                format!("cannot inspect held scorecard snapshot parent: {error}")
            })?,
        )) != self.parent_identity
            || SnapshotDirectoryIdentity::from_path_identity(snapshot_path_identity(
                &self.parent,
                "directory",
            )?) != self.parent_identity
        {
            return Err(format!(
                "scorecard snapshot parent changed while held: {}",
                self.parent.display()
            ));
        }
        let metadata = self
            .file
            .metadata()
            .map_err(|error| format!("cannot inspect held scorecard snapshot: {error}"))?;
        if !metadata.is_file()
            || SnapshotPathIdentity::from_metadata(&metadata) != self.file_identity
            || snapshot_path_identity(&self.path, "file")? != self.file_identity
        {
            return Err(format!(
                "scorecard snapshot pathname or identity changed while held: {}",
                self.path.display()
            ));
        }
        let payload = read_held_snapshot_file(&self.file, self.file_identity)?;
        let actual_sha256 = format!("{:x}", Sha256::digest(&payload));
        if actual_sha256 != self.sha256 {
            return Err(format!(
                "scorecard snapshot content changed while held: {}",
                self.path.display()
            ));
        }
        Ok(())
    }
}

fn scorecard_snapshot_fixture_value(
    source_commit: &str,
    source_tree: &str,
    rows: &[SeriesRow],
) -> Result<JsonValue, String> {
    let row_values = rows
        .iter()
        .map(|row| serde_json::to_value(row).map_err(|error| error.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    let rows_sha256 = format!(
        "{:x}",
        Sha256::digest(canonical_snapshot_rows_bytes(&row_values)?)
    );
    Ok(serde_json::json!({
        "schema": SCORECARD_SERIES_SNAPSHOT_SCHEMA,
        "source": {
            "path": SCORECARD_SERIES_SNAPSHOT_SOURCE,
            "commit": source_commit,
            "tree": source_tree,
            "published_rows": row_values.len(),
            "local_sources": [],
        },
        "rows_read": row_values.len(),
        "rows_sha256": rows_sha256,
        "rows": row_values,
    }))
}

fn write_scorecard_snapshot_fixture(path: &Path, value: &JsonValue) -> Result<String, String> {
    let mut payload = serde_json::to_vec(value)
        .map_err(|error| format!("cannot encode scorecard snapshot fixture: {error}"))?;
    payload.push(b'\n');
    fs::write(path, &payload)
        .map_err(|error| format!("cannot write scorecard snapshot fixture: {error}"))?;
    Ok(format!("{:x}", Sha256::digest(&payload)))
}

/// Capture the committed series source once and verify that the worktree is an
/// exact view of it.
///
/// The returned bytes come from the resolved commit, not from a later worktree
/// read. Consequently a mutation after this function returns cannot change the
/// rows while leaving `source_commit` unchanged. The worktree comparison still
/// matters: it refuses a caller that points at changed, missing, or untracked
/// JSONL shards instead of silently projecting a different tree than the one
/// visible to the caller.
fn snapshot_series_source(series_root: &Path) -> Result<SeriesSourceSnapshot, String> {
    let canonical = fs::canonicalize(series_root).map_err(|e| {
        format!(
            "series root {} does not exist or cannot be resolved: {e}. An unreachable source is \
             REFUSED rather than treated as an empty one -- those are different facts, and only \
             one of them is a statement about the cells.",
            series_root.display()
        )
    })?;
    if !canonical.is_dir() {
        return Err(format!(
            "series root {} is not a directory",
            series_root.display()
        ));
    }

    let top = Command::new("git")
        .args(["--no-replace-objects", "rev-parse", "--show-toplevel"])
        .current_dir(&canonical)
        .output()
        .map_err(|e| {
            format!(
                "cannot locate Git repository for {}: {e}",
                canonical.display()
            )
        })?;
    if !top.status.success() {
        return Err(format!(
            "series root {} is not inside a Git repository; a projection without a source commit is refused",
            canonical.display()
        ));
    }
    let repository_text = std::str::from_utf8(&top.stdout)
        .map_err(|e| format!("Git repository path is not UTF-8: {e}"))?
        .trim();
    let repository = fs::canonicalize(repository_text).map_err(|e| {
        format!(
            "cannot resolve Git repository {} for series root {}: {e}",
            repository_text,
            canonical.display()
        )
    })?;
    let relative_root = canonical.strip_prefix(&repository).map_err(|_| {
        format!(
            "series root {} is outside its reported Git repository {}",
            canonical.display(),
            repository.display()
        )
    })?;
    let source = if relative_root.as_os_str().is_empty() {
        ".".to_string()
    } else {
        relative_root
            .to_str()
            .ok_or_else(|| {
                format!(
                    "series root {} is not representable as a UTF-8 repository-relative path",
                    canonical.display()
                )
            })?
            .to_string()
    };
    let source_commit = git_no_replace_rev_parse(&repository, "HEAD^{commit}")?;
    if !is_object_id(&source_commit) {
        return Err(format!(
            "series source commit must be a lowercase 40-hex object id, got {source_commit:?}"
        ));
    }
    let source_revision = if source == "." {
        format!("{source_commit}^{{tree}}")
    } else {
        format!("{source_commit}:{source}")
    };
    let source_tree = git_no_replace_rev_parse(&repository, &source_revision)?;
    if !is_object_id(&source_tree) {
        return Err(format!(
            "series source tree must be a lowercase 40-hex object id, got {source_tree:?}"
        ));
    }

    let listed = Command::new("git")
        .args([
            "--no-replace-objects",
            "ls-tree",
            "-rz",
            "--name-only",
            &source_commit,
            "--",
        ])
        .arg(relative_root)
        .current_dir(&repository)
        .output()
        .map_err(|e| format!("cannot list series source at {source_commit}: {e}"))?;
    if !listed.status.success() {
        return Err(format!(
            "git ls-tree failed for series source {} at {source_commit}",
            canonical.display()
        ));
    }
    let committed_shards: BTreeSet<PathBuf> = listed
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| {
            std::str::from_utf8(path)
                .map(PathBuf::from)
                .map_err(|e| format!("series source contains a non-UTF-8 path: {e}"))
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .collect();

    let mut worktree_paths = Vec::new();
    collect_shards(&canonical, &mut worktree_paths)?;
    let worktree_shards: BTreeSet<PathBuf> = worktree_paths
        .into_iter()
        .map(|path| {
            path.strip_prefix(&repository)
                .map(Path::to_path_buf)
                .map_err(|_| {
                    format!(
                        "series shard {} is outside its reported Git repository {}",
                        path.display(),
                        repository.display()
                    )
                })
        })
        .collect::<Result<_, _>>()?;
    if let Some(path) = worktree_shards.difference(&committed_shards).next() {
        return Err(format!(
            "series source is not represented exactly by commit {source_commit}; worktree-only JSONL shard {} is absent from that commit",
            path.display()
        ));
    }
    if let Some(path) = committed_shards.difference(&worktree_shards).next() {
        return Err(format!(
            "series source is not represented exactly by commit {source_commit}; committed JSONL shard {} is missing from the worktree",
            path.display()
        ));
    }

    let mut shards = Vec::with_capacity(committed_shards.len());
    for relative_shard in committed_shards {
        let committed = Command::new("git")
            .args([
                "--no-replace-objects",
                "show",
                &format!("{source_commit}:{}", relative_shard.display()),
            ])
            .current_dir(&repository)
            .output()
            .map_err(|e| {
                format!(
                    "cannot read series shard {} from source commit {source_commit}: {e}",
                    relative_shard.display()
                )
            })?;
        if !committed.status.success() {
            return Err(format!(
                "git show failed for series shard {} at source commit {source_commit}",
                relative_shard.display()
            ));
        }
        let worktree_path = repository.join(&relative_shard);
        let working = fs::read(&worktree_path)
            .map_err(|e| format!("cannot read series shard {}: {e}", worktree_path.display()))?;
        if committed.stdout != working {
            return Err(format!(
                "series source is not represented exactly by commit {source_commit}; worktree shard {} differs from the committed snapshot",
                relative_shard.display()
            ));
        }
        let within_source = relative_shard.strip_prefix(relative_root).map_err(|_| {
            format!(
                "committed series shard {} is outside source root {}",
                relative_shard.display(),
                relative_root.display()
            )
        })?;
        shards.push(SeriesSourceShard {
            display_path: series_root.join(within_source),
            bytes: committed.stdout,
        });
    }
    Ok(SeriesSourceSnapshot {
        source,
        source_commit,
        source_tree,
        shards,
    })
}

/// Parse the captured commit bytes into one canonical row per `event_id`.
///
/// Malformed input refuses the whole projection. Repeated event IDs with the
/// same semantic JSON body collapse to one row; differing bodies under one ID
/// are contradictory evidence and refuse instead of depending on traversal
/// order.
fn read_series_rows(snapshot: &SeriesSourceSnapshot) -> Result<Vec<SeriesRow>, String> {
    let mut canonical: BTreeMap<String, (JsonValue, SeriesRow, String)> = BTreeMap::new();
    for shard in &snapshot.shards {
        if !shard.bytes.is_empty() && !shard.bytes.ends_with(b"\n") {
            return Err(format!(
                "series shard {} in source commit {} is truncated: every nonempty shard must end in a newline",
                shard.display_path.display(),
                snapshot.source_commit
            ));
        }
        let text = std::str::from_utf8(&shard.bytes).map_err(|e| {
            format!(
                "series shard {} in source commit {} is not UTF-8: {e}",
                shard.display_path.display(),
                snapshot.source_commit
            )
        })?;
        for (index, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let source = format!("{}:{}", shard.display_path.display(), index + 1);
            let value: JsonValue = serde_json::from_str(line)
                .map_err(|why| format!("malformed series row at {source}: {why}"))?;
            let row: SeriesRow = serde_json::from_value(value.clone())
                .map_err(|why| format!("malformed series row at {source}: {why}"))?;
            row.validate_for_read()
                .map_err(|why| format!("invalid series row at {source}: {why}"))?;
            if let Some((previous_value, _, previous_source)) = canonical.get(&row.event_id) {
                if previous_value == &value {
                    continue;
                }
                return Err(format!(
                    "conflicting series rows share event_id {:?}: {previous_source} and {source}",
                    row.event_id
                ));
            }
            canonical.insert(row.event_id.clone(), (value, row, source));
        }
    }
    Ok(canonical
        .into_values()
        .map(|(_, mut row, source)| {
            row.source = source;
            row
        })
        .collect())
}

fn collect_shards(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("cannot list {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot list {}: {e}", dir.display()))?;
        let path = entry.path();
        if path.is_dir() {
            collect_shards(&path, out)?;
        } else if path.extension().is_some_and(|ext| ext == "jsonl") {
            out.push(path);
        }
    }
    Ok(())
}

fn verify_results(root: &Path, result_root: &Path, lanes: &BTreeSet<String>) -> Result<(), String> {
    let derived = derive(root)?;
    let head = git_head(root)?;
    let expected: BTreeSet<_> = derived
        .selected
        .iter()
        .filter(|id| lanes.contains(&id.lane))
        .cloned()
        .collect();
    if expected.is_empty() {
        return Err("selected lanes contain no regression cells".into());
    }
    let candidates = read_result_candidates(result_root, &head)?;
    let admitted = verify_candidate_set(&expected, candidates)?;
    if admitted != expected.len() {
        return Err(format!(
            "result admission counted {admitted} cells, expected {}",
            expected.len()
        ));
    }

    print!("{}", render_scorecard(&derived));
    if let Some(mut tracked) = load_existing(root)? {
        refresh_measurement(&mut tracked);
        print!("{}", render_measurement_section(&tracked));
    }
    let green_checked = expected
        .iter()
        .filter(|id| derived.green.contains(*id))
        .count();
    let chaos_checked = expected.iter().filter(|id| id.mode == "chaos").count();
    let custom_checked = expected.iter().filter(|id| id.mode == "custom").count();
    println!();
    println!(
        "{}",
        fresh_result_summary(
            expected.len(),
            &head,
            green_checked,
            chaos_checked,
            custom_checked,
        )
    );
    println!("Result directory: {}", result_root.display());
    Ok(())
}

fn fresh_result_summary(
    selected: usize,
    head: &str,
    green: usize,
    chaos: usize,
    custom: usize,
) -> String {
    format!(
        "Fresh result check: {selected}/{selected} selected cells passed at {head} \
({green} compatibility green, including {chaos} chaos; {custom} custom outside the comparable denominator)."
    )
}

fn git_head(root: &Path) -> Result<String, String> {
    git_rev_parse(root, "HEAD")
}

fn git_rev_parse(root: &Path, revision: &str) -> Result<String, String> {
    let output = Command::new("git")
        .args(["rev-parse", revision])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cannot read HEAD: {e}"))?;
    if !output.status.success() {
        return Err(format!("git rev-parse {revision} failed"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git_no_replace_rev_parse(root: &Path, revision: &str) -> Result<String, String> {
    let output = Command::new("git")
        .args(["--no-replace-objects", "rev-parse", revision])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cannot read revision without replacement objects: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git --no-replace-objects rev-parse {revision} failed"
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn read_result_candidates(
    root: &Path,
    head: &str,
) -> Result<BTreeMap<CellId, Vec<ResultCandidate>>, String> {
    if !root.is_dir() {
        return Err(format!(
            "result directory does not exist: {}",
            root.display()
        ));
    }
    let mut files = Vec::new();
    find_result_files(root, &mut files)?;
    if files.is_empty() {
        return Err(format!("no results.jsonl files under {}", root.display()));
    }
    let mut out: BTreeMap<CellId, Vec<ResultCandidate>> = BTreeMap::new();
    for path in files {
        let text = fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        for (index, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let mut row: ResultRow = serde_json::from_str(line)
                .map_err(|e| format!("invalid JSON at {}:{}: {e}", path.display(), index + 1))?;
            row.require_current_timeout_policy().map_err(|error| {
                format!(
                    "invalid timeout policy at {}:{}: {error}",
                    path.display(),
                    index + 1
                )
            })?;
            // Before ANY use, including the provenance checks below, so every
            // downstream consumer and the tracked file agree on one spelling.
            normalise_recorded_root(&mut row);
            if row.schema != CELL_RESULT_SCHEMA {
                return Err(format!(
                    "{}:{} has result schema {}, expected {}",
                    path.display(),
                    index + 1,
                    row.schema,
                    CELL_RESULT_SCHEMA
                ));
            }
            if row.hermit_sha != head || row.source_tree_dirty {
                return Err(format!(
                    "{}:{} is not a clean result for HEAD {} (sha={}, dirty={})",
                    path.display(),
                    index + 1,
                    head,
                    row.hermit_sha,
                    row.source_tree_dirty
                ));
            }
            if row.classification != "required" {
                continue;
            }
            if row.attempt == 0 {
                return Err(format!(
                    "{}:{} has non-positive attempt 0",
                    path.display(),
                    index + 1
                ));
            }
            let evidence_identity = row
                .evidence_identity()
                .map_err(|error| format!("{}:{} {error}", path.display(), index + 1))?;
            let id = row
                .id()
                .ok_or_else(|| format!("{}:{} has no backend", path.display(), index + 1))?;
            out.entry(id).or_default().push(ResultCandidate {
                evidence_identity,
                path: path.clone(),
                row,
            });
        }
    }
    Ok(out)
}

struct RetainedImport {
    cells: Vec<RetainedCellResults>,
    files_scanned: usize,
    rows_scanned: usize,
    terminal_comparisons: usize,
    stale_coordinate_rows: usize,
    stale_coordinates: usize,
    stale_coordinate_cells: BTreeSet<String>,
    no_result_cells: BTreeSet<String>,
}

/// Read retained validate rows without pretending they belong to the current
/// checkout. Each row keeps its own Hermit SHA, and only clean canonical
/// comparisons on HEAD's history are eligible. For each enabled cell, import
/// every terminal comparison at the newest eligible SHA so disagreement at one
/// revision remains visible instead of being resolved by file ordering.
fn read_retained_results(
    root: &Path,
    result_root: &Path,
    eligible: &BTreeSet<CellId>,
) -> Result<RetainedImport, String> {
    if !result_root.is_dir() {
        return Err(format!(
            "result directory does not exist: {}",
            result_root.display()
        ));
    }
    let mut files = Vec::new();
    find_result_files(result_root, &mut files)?;
    files.sort();
    if files.is_empty() {
        return Err(format!(
            "no results.jsonl files under {}",
            result_root.display()
        ));
    }

    let history = git_history_ranks(root)?;
    let retained_workspace = result_root
        .ancestors()
        .find(|path| path.file_name().is_some_and(|name| name == "ignored"))
        .and_then(Path::parent)
        .and_then(Path::to_str)
        .map(str::to_string);
    let mut grouped: BTreeMap<(CellId, String, String), Vec<ResultCandidate>> = BTreeMap::new();
    let mut rows_scanned = 0usize;
    for path in &files {
        let text =
            fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        for (index, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            rows_scanned += 1;
            let raw: JsonValue = serde_json::from_str(line)
                .map_err(|e| format!("invalid JSON at {}:{}: {e}", path.display(), index + 1))?;
            // Retained history includes older result schemas. They cannot carry
            // the complete invocation and comparison receipt required here, so
            // they are outside this import rather than malformed current rows.
            if raw.get("schema").and_then(JsonValue::as_u64) != Some(CELL_RESULT_SCHEMA) {
                continue;
            }
            let mut row: ResultRow = serde_json::from_value(raw).map_err(|e| {
                format!(
                    "invalid schema-{CELL_RESULT_SCHEMA} row at {}:{}: {e}",
                    path.display(),
                    index + 1
                )
            })?;
            row.validate_timeout_policy().map_err(|error| {
                format!(
                    "invalid timeout policy at {}:{}: {error}",
                    path.display(),
                    index + 1
                )
            })?;
            normalise_recorded_root(&mut row);
            if let Some(prefix) = retained_workspace.as_deref() {
                normalise_recorded_prefix(&mut row, prefix);
            }
            if row.classification != "required" || row.source_tree_dirty || row.attempt == 0 {
                continue;
            }
            let Some(id) = row.id() else { continue };
            if !eligible.contains(&id) || !history.contains_key(&row.hermit_sha) {
                continue;
            }
            let evidence_identity = row
                .evidence_identity()
                .map_err(|error| format!("{}:{} {error}", path.display(), index + 1))?;
            grouped
                .entry((id, row.hermit_sha.clone(), row.run_id.clone()))
                .or_default()
                .push(ResultCandidate {
                    evidence_identity,
                    path: path.clone(),
                    row,
                });
        }
    }

    let mut by_cell_and_rank: BTreeMap<CellId, BTreeMap<usize, Vec<ResultCandidate>>> =
        BTreeMap::new();
    for ((id, sha, _run_id), candidates) in grouped {
        let terminal_attempt = candidates
            .iter()
            .map(|candidate| candidate.row.attempt)
            .max()
            .expect("retained candidate group is nonempty");
        let mut distinct = BTreeMap::new();
        for candidate in candidates
            .into_iter()
            .filter(|candidate| candidate.row.attempt == terminal_attempt)
        {
            distinct
                .entry(candidate.evidence_identity.clone())
                .or_insert(candidate);
        }
        if distinct.len() != 1 {
            let details = distinct
                .values()
                .take(4)
                .map(|candidate| candidate.path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!(
                "ambiguous terminal retained evidence for {} at {sha}: {details}",
                display_id(&id)
            ));
        }
        let candidate = distinct
            .into_values()
            .next()
            .expect("distinct terminal evidence is nonempty");
        match candidate.row.comparison_evidence().map_err(|error| {
            format!(
                "malformed retained evidence for {} at {}: {error}",
                display_id(&id),
                candidate.path.display()
            )
        })? {
            ValidateRowEvidence::NotRun { .. } | ValidateRowEvidence::Unavailable { .. } => {
                continue;
            }
            ValidateRowEvidence::Matched { .. } | ValidateRowEvidence::Diverged { .. } => {}
        }
        let rank = *history
            .get(&sha)
            .expect("history membership checked before grouping");
        by_cell_and_rank
            .entry(id)
            .or_default()
            .entry(rank)
            .or_default()
            .push(candidate);
    }

    let no_result_cells = eligible
        .iter()
        .filter(|id| !by_cell_and_rank.contains_key(*id))
        .map(display_id)
        .collect::<BTreeSet<_>>();

    let mut metadata: BTreeMap<String, (String, BTreeMap<String, SourceDepth>)> = BTreeMap::new();
    let mut cells = Vec::new();
    let mut terminal_comparisons = 0usize;
    let mut stale_coordinate_rows = 0usize;
    let mut stale_coordinates = 0usize;
    let mut stale_coordinate_cells = BTreeSet::new();
    for (id, mut ranks) in by_cell_and_rank {
        let latest_rank = *ranks.keys().next().expect("cell has retained evidence");
        let latest_candidates = ranks.get(&latest_rank).expect("latest rank exists");
        let latest_is_pass = latest_candidates
            .iter()
            .all(|candidate| candidate.row.outcome == "PASS");
        if latest_is_pass {
            for candidates in ranks.range((latest_rank + 1)..).map(|(_, rows)| rows) {
                for candidate in candidates {
                    if candidate.row.outcome != "FAIL" {
                        continue;
                    }
                    let coordinate_count = [
                        candidate.row.first_divergent_scheduler_turn,
                        candidate.row.first_divergent_virtual_nanoseconds,
                        candidate.row.first_divergent_record,
                        candidate.row.first_divergent_syscall,
                    ]
                    .into_iter()
                    .flatten()
                    .count();
                    if coordinate_count > 0 {
                        stale_coordinate_rows += 1;
                        stale_coordinates += coordinate_count;
                        stale_coordinate_cells.insert(display_id(&id));
                    }
                }
            }
        }
        let candidates = ranks
            .remove(&latest_rank)
            .expect("latest retained evidence exists");
        let sha = candidates[0].row.hermit_sha.clone();
        if candidates
            .iter()
            .any(|candidate| candidate.row.hermit_sha != sha)
        {
            return Err(format!(
                "latest retained rank mixed Hermit SHAs for {}",
                display_id(&id)
            ));
        }
        let (detcore_tree, depth) = match metadata.get(&sha) {
            Some(value) => value.clone(),
            None => {
                let value = (
                    git_rev_parse(root, &format!("{sha}:detcore"))?,
                    BTreeMap::from([(
                        "hermit".to_string(),
                        repo_depth_at(root, &sha).ok_or_else(|| {
                            format!("cannot read Hermit source depth at retained SHA {sha}")
                        })?,
                    )]),
                );
                metadata.insert(sha.clone(), value.clone());
                value
            }
        };
        terminal_comparisons += candidates.len();
        cells.push(RetainedCellResults {
            id,
            hermit_sha: sha,
            detcore_tree,
            depth,
            candidates,
        });
    }
    Ok(RetainedImport {
        cells,
        files_scanned: files.len(),
        rows_scanned,
        terminal_comparisons,
        stale_coordinate_rows,
        stale_coordinates,
        stale_coordinate_cells,
        no_result_cells,
    })
}

/// Admit the one current DBT shape whose product result is present in the typed
/// verification report even though the raw run logs were not retained.
///
/// The ordinary pressure-observation writer still refuses this row: it cannot
/// claim to have retained artifacts that are absent. `import-results` needs a
/// narrower answer for the four-state coordinate check. A canonical,
/// non-vacuous `verdict=diverged` receipt proves the current divergence and its
/// position, so treating the row only as infrastructure trouble would preserve
/// a retained position that three current reports have already contradicted.
/// No other evidence error is cleared, and a matched or non-canonical report
/// remains refused.
fn admit_current_dbt_divergence_without_retained_logs(
    summary: &mut PressureSummary,
) -> Result<bool, String> {
    let row = summary
        .rows
        .first_mut()
        .ok_or("current pressure summary contains no row")?;
    if row.cell.mode != "verify"
        || row.cell.backend != "dbt"
        || row.result != "infrastructure-error"
        || row.evidence_errors.as_slice() != [MISSING_RETAINED_VERIFY_LOGS]
    {
        return Ok(false);
    }
    let report = row
        .verification
        .as_ref()
        .ok_or("DBT row missing retained run logs also has no verification report")?;
    report.require_canonical_comparison().map_err(|error| {
        format!("DBT row missing retained run logs has no canonical comparison to import: {error}")
    })?;
    if report.verdict != canonical_verdict::Verdict::Diverged
        || report.verified
        || report.bitwise_parity
    {
        return Err(format!(
            "DBT row missing retained run logs is not a canonical divergence: verdict={} verified={} bitwise_parity={}",
            report.verdict, report.verified, report.bitwise_parity
        ));
    }
    let coordinates = DivergenceCoordinates {
        scheduler_turn: report.first_divergent_scheduler_turn,
        virtual_nanoseconds: report.first_divergent_virtual_nanoseconds,
        record: report.first_divergent_record,
        syscall: report.first_divergent_syscall,
    };
    if coordinates.is_empty() {
        return Err("DBT canonical divergence missing retained run logs has no coordinate".into());
    }
    row.result = "determinism-failure".into();
    row.evidence_errors.clear();
    Ok(true)
}

fn checked_current_pressure_result(
    tracked: &TrackedCells,
    mut summary: PressureSummary,
    current_tree: &str,
) -> Result<CurrentPressureResult, String> {
    let missing_retained_logs = admit_current_dbt_divergence_without_retained_logs(&mut summary)?;
    let row = summary
        .rows
        .first()
        .ok_or("current pressure summary contains no row")?;
    if summary.rows.len() != 1 {
        return Err("current pressure check requires exactly one row".into());
    }
    let mut checked = tracked.clone();
    for cell in &mut checked.cells {
        cell.last_tested = None;
        cell.observations.clear();
        cell.measurement = MeasurementState::NeverMeasured;
    }
    apply_pressure_summary(
        &mut checked,
        &summary,
        &summary.hermit_sha,
        current_tree,
        &BTreeMap::new(),
    )?;
    let result = ObservedResult::parse(&row.result)?;
    let coordinates = row
        .verification
        .as_ref()
        .map(|report| DivergenceCoordinates {
            scheduler_turn: report.first_divergent_scheduler_turn,
            virtual_nanoseconds: report.first_divergent_virtual_nanoseconds,
            record: report.first_divergent_record,
            syscall: report.first_divergent_syscall,
        })
        .unwrap_or(DivergenceCoordinates {
            scheduler_turn: None,
            virtual_nanoseconds: None,
            record: None,
            syscall: None,
        });
    Ok(CurrentPressureResult {
        summary,
        result,
        coordinates,
        missing_retained_logs,
    })
}

fn read_current_pressure_evidence(
    root: &Path,
    summaries: &[PathBuf],
    tracked: &TrackedCells,
) -> Result<CurrentPressureEvidence, String> {
    let current_tree = git_rev_parse(root, "HEAD:detcore")?;
    let mut results: BTreeMap<CellId, Vec<CurrentPressureResult>> = BTreeMap::new();
    let mut uncheckable: BTreeMap<CellId, Vec<String>> = BTreeMap::new();
    let mut offered_rows = 0usize;

    for path in summaries {
        let summary: PressureSummary = read_json(path)?;
        offered_rows += summary.rows.len();
        // These summaries are explicit command-line inputs, often produced by
        // separate clean worktrees. Requiring their Hermit commit to be an
        // ancestor of this implementation branch discards independent runs of
        // the exact same Detcore tree. Verify both identities directly instead:
        // the named Hermit commit must exist, it must contain the tree recorded
        // by the summary, and that tree must equal the one being classified.
        let summary_problem = match git_rev_parse(root, &format!("{}:detcore", summary.hermit_sha))
        {
            Err(error) => Some(format!(
                "{} names Hermit commit {} whose Detcore tree cannot be read: {error}",
                path.display(),
                summary.hermit_sha
            )),
            Ok(recorded_tree) if summary.detcore_tree != recorded_tree => Some(format!(
                "{} names detcore tree {}, but {} contains {}",
                path.display(),
                summary.detcore_tree,
                summary.hermit_sha,
                recorded_tree
            )),
            Ok(_) if summary.detcore_tree != current_tree => Some(format!(
                "{} measured detcore tree {}, but HEAD contains {}",
                path.display(),
                summary.detcore_tree,
                current_tree
            )),
            Ok(_) => None,
        };

        for row in &summary.rows {
            if let Some(problem) = &summary_problem {
                uncheckable
                    .entry(row.cell.clone())
                    .or_default()
                    .push(problem.clone());
                continue;
            }
            let one = PressureSummary {
                schema: summary.schema,
                hermit_sha: summary.hermit_sha.clone(),
                detcore_tree: summary.detcore_tree.clone(),
                source_tree_dirty: summary.source_tree_dirty,
                rows: vec![row.clone()],
            };
            match checked_current_pressure_result(tracked, one, &current_tree) {
                Ok(result) => {
                    results.entry(row.cell.clone()).or_default().push(result);
                }
                Err(error) => {
                    uncheckable
                        .entry(row.cell.clone())
                        .or_default()
                        .push(format!("{}: {error}", path.display()));
                }
            }
        }
    }
    if offered_rows == 0 {
        return Err("current pressure summaries contain no rows".into());
    }
    Ok(CurrentPressureEvidence {
        results,
        uncheckable,
    })
}

fn retained_coordinate_decision(
    retained: RetainedCellResults,
    current: &CurrentPressureEvidence,
) -> RetainedDecision {
    let retained_coordinates = retained
        .candidates
        .iter()
        .filter(|candidate| candidate.row.outcome == "FAIL")
        .map(|candidate| DivergenceCoordinates::from_row(&candidate.row))
        .filter(|coordinates| !coordinates.is_empty())
        .collect::<BTreeSet<_>>();
    let offered_current_results = current
        .results
        .get(&retained.id)
        .cloned()
        .unwrap_or_default();
    let mut current_by_run: BTreeMap<
        (String, String, Option<u64>, String),
        BTreeMap<(ObservedResult, DivergenceCoordinates), CurrentPressureResult>,
    > = BTreeMap::new();
    for result in offered_current_results {
        let row = &result.summary.rows[0];
        let invocation = row
            .invocation
            .as_ref()
            .expect("trusted pressure row has an invocation");
        // The pressure producer's run_id identifies the cell, not a campaign:
        // separate retained campaigns can therefore carry the same run_id and
        // repetition. Their literal commands still name distinct working
        // directories. Include that command in the key so three independently
        // executed summaries count as three samples, while a copied summary
        // with the identical invocation is still deduplicated.
        current_by_run
            .entry((
                result.summary.hermit_sha.clone(),
                invocation.run_id.clone(),
                row.repetition,
                invocation.shell_command.clone(),
            ))
            .or_default()
            .entry((result.result, result.coordinates))
            .or_insert(result);
    }
    if current_by_run.values().any(|values| values.len() != 1) {
        return RetainedDecision {
            state: RetainedComparisonState::Uncheckable,
            import: ImportEvidence::Retained {
                results: Box::new(retained),
                store_positions: false,
            },
            retained_coordinates,
            current_coordinates: BTreeSet::new(),
            reason: "one current run identity carries conflicting results".into(),
        };
    }
    let current_results = current_by_run
        .into_values()
        .map(|values| values.into_values().next().expect("one result per run"))
        .collect::<Vec<_>>();
    let current_coordinates = current_results
        .iter()
        .filter(|result| result.result.carries_divergence_position())
        .map(|result| result.coordinates)
        .filter(|coordinates| !coordinates.is_empty())
        .collect::<BTreeSet<_>>();

    if let Some(reasons) = current.uncheckable.get(&retained.id) {
        return RetainedDecision {
            state: RetainedComparisonState::Uncheckable,
            import: ImportEvidence::Retained {
                results: Box::new(retained),
                store_positions: false,
            },
            retained_coordinates,
            current_coordinates,
            reason: reasons.join("; "),
        };
    }
    if current_results.is_empty() {
        return RetainedDecision {
            state: RetainedComparisonState::Uncheckable,
            import: ImportEvidence::Retained {
                results: Box::new(retained),
                store_positions: false,
            },
            retained_coordinates,
            current_coordinates,
            reason: "no current pressure summary row was supplied".into(),
        };
    }

    let matched = current_results
        .iter()
        .filter(|result| result.result == ObservedResult::Pass)
        .count();
    let diverged = current_results
        .iter()
        .filter(|result| result.result.carries_divergence_position())
        .count();
    let no_verdict = current_results.len() - matched - diverged;
    let sample_count = current_results.len();
    if no_verdict > 0 {
        return RetainedDecision {
            state: RetainedComparisonState::Uncheckable,
            import: ImportEvidence::Retained {
                results: Box::new(retained),
                store_positions: false,
            },
            retained_coordinates,
            current_coordinates,
            reason: format!(
                "{sample_count} current run(s): {matched} matched, {diverged} diverged, {no_verdict} produced no verdict"
            ),
        };
    }
    if diverged == 0 {
        if matched < 2 {
            return RetainedDecision {
                state: RetainedComparisonState::Uncheckable,
                import: ImportEvidence::Retained {
                    results: Box::new(retained),
                    store_positions: false,
                },
                retained_coordinates,
                current_coordinates,
                reason: format!(
                    "{sample_count} current run matched; one matching run cannot establish that an intermittent divergence is gone"
                ),
            };
        }
        return RetainedDecision {
            state: RetainedComparisonState::Wrong,
            import: ImportEvidence::None,
            retained_coordinates,
            current_coordinates,
            reason: format!("{sample_count} current runs all matched"),
        };
    }
    if current_coordinates.is_empty()
        || current_results.iter().any(|result| {
            result.result.carries_divergence_position() && result.coordinates.is_empty()
        })
    {
        return RetainedDecision {
            state: RetainedComparisonState::Uncheckable,
            import: ImportEvidence::Retained {
                results: Box::new(retained),
                store_positions: false,
            },
            retained_coordinates,
            current_coordinates,
            reason: format!(
                "{sample_count} current run(s): {matched} matched and {diverged} diverged, but at least one divergence has no coordinate"
            ),
        };
    }
    if current_coordinates == retained_coordinates {
        RetainedDecision {
            state: RetainedComparisonState::Fresh,
            import: ImportEvidence::Retained {
                results: Box::new(retained),
                store_positions: true,
            },
            retained_coordinates,
            current_coordinates,
            reason: format!(
                "{sample_count} current run(s): {matched} matched and {diverged} diverged; every current divergence coordinate equals the retained set"
            ),
        }
    } else {
        RetainedDecision {
            state: RetainedComparisonState::Drifted,
            import: ImportEvidence::None,
            retained_coordinates,
            current_coordinates,
            reason: format!(
                "{sample_count} current run(s): {matched} matched and {diverged} diverged; the current divergence coordinate set differs from the retained set"
            ),
        }
    }
}

fn remove_imported_validate_projection(cell: &mut TrackedCell) {
    let removed_shas = cell
        .observations
        .iter()
        .filter(|observation| {
            observation.provenance == ObservationProvenance::Validate
                && !observation.canonical_comparisons.is_empty()
        })
        .flat_map(|observation| observation.canonical_comparisons.iter())
        .map(|comparison| comparison.hermit_sha.clone())
        .collect::<BTreeSet<_>>();
    cell.observations.retain(|observation| {
        observation.provenance != ObservationProvenance::Validate
            || observation.canonical_comparisons.is_empty()
    });
    if cell
        .last_tested
        .as_ref()
        .is_some_and(|last| removed_shas.contains(&last.hermit_sha))
    {
        cell.last_tested = None;
    }
}

/// Rank commits in the current Hermit history for retained-result import.
///
/// This helper intentionally remains checkout-relative: retained result files
/// are admitted only when their commits are on the current branch. The series
/// projector does not call it and must remain independent of ambient refs and
/// objects.
fn git_history_ranks(root: &Path) -> Result<BTreeMap<String, usize>, String> {
    let output = Command::new("git")
        .args(["rev-list", "--topo-order", "HEAD"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cannot read Hermit history: {e}"))?;
    if !output.status.success() {
        return Err("git rev-list --topo-order HEAD failed".into());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .enumerate()
        .map(|(rank, sha)| (sha.to_string(), rank))
        .collect())
}

fn find_result_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    for entry in fs::read_dir(dir).map_err(|e| format!("cannot list {}: {e}", dir.display()))? {
        let entry = entry.map_err(|e| format!("cannot read entry under {}: {e}", dir.display()))?;
        let path = entry.path();
        let kind = entry
            .file_type()
            .map_err(|e| format!("cannot stat {}: {e}", path.display()))?;
        if kind.is_dir() {
            find_result_files(&path, out)?;
        } else if kind.is_file() && entry.file_name() == "results.jsonl" {
            out.push(path);
        }
    }
    Ok(())
}

fn verify_candidate_set(
    expected: &BTreeSet<CellId>,
    candidates: BTreeMap<CellId, Vec<ResultCandidate>>,
) -> Result<usize, String> {
    let mut missing = Vec::new();
    let mut failed = Vec::new();
    let mut admitted = 0usize;
    let mut binary_identities = BTreeMap::<String, Vec<String>>::new();
    for id in expected {
        let Some(rows) = candidates.get(id) else {
            missing.push(display_id(id));
            continue;
        };
        let run_ids = rows
            .iter()
            .map(|candidate| candidate.row.run_id.as_str())
            .collect::<BTreeSet<_>>();
        if run_ids.len() != 1 {
            return Err(format!(
                "fresh result set mixes run ids for {}: {}",
                display_id(id),
                run_ids.into_iter().collect::<Vec<_>>().join(", ")
            ));
        }
        let terminal_attempt = rows
            .iter()
            .map(|candidate| candidate.row.attempt)
            .max()
            .expect("candidate rows are nonempty");
        let mut distinct = BTreeMap::new();
        for candidate in rows
            .iter()
            .filter(|candidate| candidate.row.attempt == terminal_attempt)
        {
            distinct
                .entry(candidate.evidence_identity.as_str())
                .or_insert(candidate);
        }
        if distinct.len() != 1 {
            let descriptions = distinct
                .values()
                .take(4)
                .map(|candidate| {
                    format!(
                        "{} run={} evidence={}",
                        candidate.path.display(),
                        candidate.row.run_id,
                        &candidate.evidence_identity[..12]
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            return Err(format!(
                "ambiguous distinct evidence for {}: {}",
                display_id(id),
                descriptions
            ));
        }
        let candidate = distinct
            .into_values()
            .next()
            .expect("candidate evidence is nonempty");
        candidate.row.require_provenance().map_err(|error| {
            format!(
                "fresh result for {} in {} {error}",
                display_id(id),
                candidate.path.display()
            )
        })?;
        candidate
            .row
            .require_canonical_pass_evidence()
            .map_err(|error| {
                format!(
                    "fresh result for {} in {} {error}",
                    display_id(id),
                    candidate.path.display()
                )
            })?;
        binary_identities
            .entry(
                candidate
                    .row
                    .binary_sha256
                    .clone()
                    .expect("provenance requires a binary identity"),
            )
            .or_default()
            .push(display_id(id));
        if candidate.row.outcome != "PASS" {
            failed.push(format!(
                "{}={} ({})",
                display_id(id),
                candidate.row.outcome,
                candidate.path.display()
            ));
        } else {
            admitted += 1;
        }
    }
    if binary_identities.len() > 1 {
        let details = binary_identities
            .iter()
            .take(4)
            .map(|(sha, ids)| format!("{}: {}", &sha[..12], ids.join(", ")))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!(
            "fresh result set mixes {} Hermit binary identities at one source SHA: {details}",
            binary_identities.len()
        ));
    }
    if !missing.is_empty() || !failed.is_empty() {
        let mut message = format!(
            "fresh result set refused: {} missing, {} non-passing",
            missing.len(),
            failed.len()
        );
        for item in missing.iter().take(8) {
            message.push_str(&format!("\n  missing: {item}"));
        }
        for item in failed.iter().take(8) {
            message.push_str(&format!("\n  non-passing: {item}"));
        }
        return Err(message);
    }
    Ok(admitted)
}

fn require_sha256(label: &str, value: &str) -> Result<(), String> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("has no valid {label} SHA-256 identity"));
    }
    Ok(())
}

/// The cell key as the SERIES STORE spells it: `test/mode/backend`.
///
/// Distinct from [`display_id`], which renders `lane/category/test/mode@backend`.
/// The store's schema (`ci-hub/series/series.py`, `_CELL_RE`) permits neither `@`
/// nor the lane/category prefix, so the two are not interchangeable and using the
/// wrong one silently matches nothing at all.
fn series_cell_key(id: &CellId) -> String {
    format!("{}/{}/{}", id.test, id.mode, id.backend)
}

fn display_id(id: &CellId) -> String {
    format!(
        "{}/{}/{}/{}@{}",
        id.lane, id.category, id.test, id.mode, id.backend
    )
}

fn attempt_shell_command_is_invalid(attempt: &JsonValue) -> bool {
    let Some(cwd) = attempt.get("cwd").and_then(JsonValue::as_str) else {
        return true;
    };
    let Some(command) = attempt.get("shell_command").and_then(JsonValue::as_str) else {
        return true;
    };
    let Ok(argv) = serde_json::from_value::<Vec<String>>(
        attempt.get("argv").cloned().unwrap_or(JsonValue::Null),
    ) else {
        return true;
    };
    let Ok(env) = serde_json::from_value::<BTreeMap<String, String>>(
        attempt.get("env").cloned().unwrap_or(JsonValue::Null),
    ) else {
        return true;
    };
    command != literal_shell_command(cwd, &env, &argv)
}

fn literal_shell_command(cwd: &str, env: &BTreeMap<String, String>, argv: &[String]) -> String {
    let mut words = vec![
        "cd".into(),
        recorded_shell_quote(cwd),
        "&&".into(),
        "env".into(),
    ];
    words.extend(
        env.iter()
            .map(|(name, value)| recorded_shell_quote(&format!("{name}={value}"))),
    );
    words.extend(argv.iter().map(|arg| recorded_shell_quote(arg)));
    words.join(" ")
}

/// The worktree a result was produced in, rewritten to [`RECORDED_ROOT`] on
/// ingest so a receipt does not carry the machine that produced it.
///
/// WHY THIS EXISTS. `cells.json` is TRACKED, so whatever a row records lands in
/// the repository. The runner writes absolute paths — `argv[0]`, `cwd`, the
/// `HOME=`/`XDG_CONFIG_HOME=`/`E2E_FIXTURE_DIR=` env values, and the derived
/// `shell_command`. The same scorecard command run from two different worktrees
/// therefore produced two different committed files, and
/// `scripts/check-portable-paths.sh` failed on the difference: 78 of its 80
/// violations were one worktree's absolute paths sitting in this file.
///
/// That is a determinism defect in a determinism project — a tool embedding its
/// environment in its own result, the same class as a host-global counter
/// leaking into guest state. Cleaning `cells.json` by hand does not fix it and
/// is worse than leaving it, because the next run regenerates the paths while
/// the next reader believes it was fixed.
///
/// ⚠️ THE PREFIX IS THE ROW'S OWN `cwd`, NOT THE GENERATOR'S ROOT. Rows are
/// aggregated from other worktrees, so stripping the *running* root would
/// normalise only the rows that happened to be local and silently leave every
/// imported one — which is precisely the half-fix that would still regenerate.
///
/// `shell_command` is RECOMPUTED rather than substituted into. It is derived by
/// [`literal_shell_command`], which shell-quotes its inputs; rewriting inside
/// the finished string would desynchronise it from the inputs it is checked
/// against and trip the `shell_command != literal_shell_command(..)` guard.
const RECORDED_ROOT: &str = "/repo";

fn rewrite_recorded_root(value: &mut String, root: &str) {
    fn delimiter(character: char) -> bool {
        matches!(
            character,
            '\u{0009}'..='\u{000d}'
                | '\u{0020}'
                | '\u{0085}'
                | '\u{00a0}'
                | '\u{1680}'
                | '\u{2000}'..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '='
                | ':'
                | ';'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | ','
                | '<'
                | '>'
                | '"'
                | '\''
        )
    }

    let mut rewritten = String::with_capacity(value.len());
    let mut cursor = 0;
    for (offset, _) in value.match_indices(root) {
        let end = offset + root.len();
        let starts_token = value[..offset].chars().next_back().is_none_or(delimiter);
        let ends_token = value[end..]
            .chars()
            .next()
            .is_none_or(|character| character == '/' || delimiter(character));
        if starts_token && ends_token {
            rewritten.push_str(&value[cursor..offset]);
            rewritten.push_str(RECORDED_ROOT);
            cursor = end;
        }
    }
    if cursor != 0 {
        rewritten.push_str(&value[cursor..]);
        *value = rewritten;
    }
}

fn rewrite_recorded_root_json(value: &mut JsonValue, root: &str) {
    match value {
        JsonValue::String(text) => rewrite_recorded_root(text, root),
        JsonValue::Array(items) => items
            .iter_mut()
            .for_each(|item| rewrite_recorded_root_json(item, root)),
        JsonValue::Object(fields) => fields
            .iter_mut()
            .for_each(|(_, field)| rewrite_recorded_root_json(field, root)),
        _ => {}
    }
}

/// Rewrite one attempt in place, then rebuild its `shell_command` from its own
/// rewritten fields so `attempt_shell_command_is_invalid` still agrees.
fn normalise_attempt_root(attempt: &mut JsonValue, root: &str) {
    rewrite_recorded_root_json(attempt, root);
    let (Some(cwd), Some(argv), Some(env)) = (
        attempt
            .get("cwd")
            .and_then(JsonValue::as_str)
            .map(str::to_owned),
        attempt
            .get("argv")
            .cloned()
            .and_then(|v| serde_json::from_value::<Vec<String>>(v).ok()),
        attempt
            .get("env")
            .cloned()
            .and_then(|v| serde_json::from_value::<BTreeMap<String, String>>(v).ok()),
    ) else {
        return;
    };
    if let Some(object) = attempt.as_object_mut() {
        object.insert(
            "shell_command".into(),
            JsonValue::String(literal_shell_command(&cwd, &env, &argv)),
        );
    }
}

/// Rewrite one recorded invocation in place against its OWN `cwd`.
///
/// `shell_command` is REBUILT from the rewritten fields rather than substituted
/// into: it is derived by [`literal_shell_command`], which shell-quotes its
/// inputs, so editing the finished string would desynchronise it from the
/// inputs it is validated against.
fn normalise_invocation_root(invocation: &mut ObservedInvocation) {
    let root = invocation.cwd.clone();
    if !root.starts_with('/') || root == RECORDED_ROOT {
        return;
    }
    for argument in invocation
        .argv
        .iter_mut()
        .chain(invocation.guest_argv.iter_mut())
    {
        rewrite_recorded_root(argument, &root);
    }
    for value in invocation.env.values_mut() {
        rewrite_recorded_root(value, &root);
    }
    invocation.cwd = RECORDED_ROOT.to_string();
    invocation.shell_command =
        literal_shell_command(&invocation.cwd, &invocation.env, &invocation.argv);
    for attempt in &mut invocation.attempts {
        for argument in attempt.argv.iter_mut().chain(attempt.guest_argv.iter_mut()) {
            rewrite_recorded_root(argument, &root);
        }
        for value in attempt.env.values_mut() {
            rewrite_recorded_root(value, &root);
        }
        attempt.cwd = RECORDED_ROOT.to_string();
        attempt.shell_command = literal_shell_command(&attempt.cwd, &attempt.env, &attempt.argv);
    }
}

/// The command string is derived from fields retained beside it. Pressure
/// summaries are checked before this runs, so omitting that duplicate string
/// from a newly stored invocation loses no independent evidence and keeps a
/// repeated campaign inside the repository's file-size limit.
fn clear_reconstructable_shell_commands(invocation: &mut ObservedInvocation) {
    invocation.shell_command.clear();
    for attempt in &mut invocation.attempts {
        attempt.shell_command.clear();
    }
}

/// Compare a stored invocation with an already validated, normalized compact
/// import without rewriting the stored evidence. A present legacy command must
/// reconstruct exactly before its redundant representation can be ignored.
fn pressure_invocation_matches(
    existing: &ObservedInvocation,
    incoming: &ObservedInvocation,
) -> bool {
    let reconstructs =
        |command: &str, cwd: &str, env: &BTreeMap<String, String>, argv: &[String]| {
            command.is_empty() || command == literal_shell_command(cwd, env, argv)
        };
    if !reconstructs(
        &existing.shell_command,
        &existing.cwd,
        &existing.env,
        &existing.argv,
    ) || existing.attempts.iter().any(|attempt| {
        !reconstructs(
            &attempt.shell_command,
            &attempt.cwd,
            &attempt.env,
            &attempt.argv,
        )
    }) {
        return false;
    }
    let mut comparable = existing.clone();
    normalise_invocation_root(&mut comparable);
    clear_reconstructable_shell_commands(&mut comparable);
    comparable == *incoming
}

fn normalise_recorded_root(row: &mut ResultRow) {
    let root = row.cwd.clone();
    // An empty, relative, or already-normalised root has nothing to strip.
    // Guarding on `starts_with('/')` also means a row written by a future
    // producer that already records `/repo` passes through untouched.
    if root.is_empty() || root == RECORDED_ROOT || !root.starts_with('/') {
        return;
    }
    for argument in row
        .argv
        .iter_mut()
        .chain(row.guest_argv.iter_mut())
        .chain(row.effective_args.iter_mut())
    {
        rewrite_recorded_root(argument, &root);
    }
    for value in row.env.values_mut() {
        rewrite_recorded_root(value, &root);
    }
    for attempt in row.attempts.iter_mut() {
        normalise_attempt_root(attempt, &root);
    }
    row.cwd = RECORDED_ROOT.to_string();
    row.shell_command = literal_shell_command(&row.cwd, &row.env, &row.argv);
}

fn normalise_recorded_prefix(row: &mut ResultRow, prefix: &str) {
    if prefix.is_empty() || prefix == RECORDED_ROOT || !prefix.starts_with('/') {
        return;
    }
    for argument in row
        .argv
        .iter_mut()
        .chain(row.guest_argv.iter_mut())
        .chain(row.effective_args.iter_mut())
    {
        rewrite_recorded_root(argument, prefix);
    }
    for value in row.env.values_mut() {
        rewrite_recorded_root(value, prefix);
    }
    for attempt in row.attempts.iter_mut() {
        normalise_attempt_root(attempt, prefix);
    }
    rewrite_recorded_root(&mut row.cwd, prefix);
    row.shell_command = literal_shell_command(&row.cwd, &row.env, &row.argv);
}

fn recorded_shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&byte))
    {
        value.into()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

fn self_test() -> Result<(), String> {
    let (summary_paths, retained) = parse_update_observations_args(
        [
            "--summary",
            "first.json",
            "--retained",
            "--summary",
            "second.json",
        ]
        .into_iter()
        .map(str::to_string),
    )?;
    if summary_paths != vec![PathBuf::from("first.json"), PathBuf::from("second.json")] || !retained
    {
        return Err("multiple retained pressure summaries were not parsed in order".into());
    }
    if parse_update_observations_args(
        ["--summary", "same.json", "--summary", "same.json"]
            .into_iter()
            .map(str::to_string),
    )
    .is_ok()
    {
        return Err("a duplicate pressure summary was accepted".into());
    }
    if parse_update_observations_args(std::iter::empty()).is_ok() {
        return Err("update-observations accepted no summaries".into());
    }

    let current_last_tested = LastTested {
        hermit_sha: "current-sha".into(),
        detcore_tree: "current-tree".into(),
        depth: BTreeMap::from([(
            "hermit".into(),
            SourceDepth {
                commits: 20,
                first_parent: 20,
            },
        )]),
    };
    let older_depth = BTreeMap::from([(
        "hermit".into(),
        SourceDepth {
            commits: 10,
            first_parent: 10,
        },
    )]);
    if should_replace_last_tested_at(
        Some(&current_last_tested),
        "older-sha",
        Some(false),
        &older_depth,
    ) || !should_replace_last_tested_at(None, "older-sha", Some(false), &older_depth)
    {
        return Err(
            "retained pressure evidence did not preserve newer last_tested evidence or fill an absent value"
                .into(),
        );
    }
    let mut additional_depth = current_last_tested.depth.clone();
    additional_depth.insert(
        "reverie".into(),
        SourceDepth {
            commits: 30,
            first_parent: 25,
        },
    );
    for depth in [&additional_depth, &BTreeMap::new()] {
        if should_replace_retained_last_tested(
            Some(&current_last_tested),
            "current-sha",
            "current-tree",
            Some(true),
            depth,
        ) {
            return Err(
                "retained reimport replaced the same code identity's recorded depths".into(),
            );
        }
    }
    if !should_replace_retained_last_tested(
        None,
        "current-sha",
        "current-tree",
        Some(true),
        &additional_depth,
    ) || !should_replace_retained_last_tested(
        Some(&current_last_tested),
        "current-sha",
        "different-tree",
        Some(true),
        &additional_depth,
    ) {
        return Err("retained stamp preservation hid absent or conflicting code identity".into());
    }

    for error_kind in [
        "incomplete-verification-evidence",
        "cpu-timeout",
        "wall-timeout",
    ] {
        if !is_prelaunch_timeout_disposition(true, None, None, Some(error_kind)) {
            return Err(format!(
                "typed pre-launch timeout {error_kind} was not accepted"
            ));
        }
    }
    for (timed_out, status, signal, error_kind) in [
        (false, None, None, Some("cpu-timeout")),
        (true, Some(0), None, Some("cpu-timeout")),
        (true, None, Some(15), Some("wall-timeout")),
        (true, None, None, Some("infrastructure")),
        (true, None, None, None),
    ] {
        if is_prelaunch_timeout_disposition(timed_out, status, signal, error_kind) {
            return Err(format!(
                "invalid pre-launch timeout disposition was accepted: timed_out={timed_out} status={status:?} signal={signal:?} error_kind={error_kind:?}"
            ));
        }
    }
    let summary = fresh_result_summary(172, "fixture-sha", 170, 2, 2);
    let expected_summary = "Fresh result check: 172/172 selected cells passed at fixture-sha \
(170 compatibility green, including 2 chaos; 2 custom outside the comparable denominator).";
    if summary != expected_summary {
        return Err(format!(
            "fresh-result summary obscures overlapping compatibility/chaos counts: {summary}"
        ));
    }

    let id = CellId {
        lane: "portable".into(),
        category: "fixture".into(),
        test: "fixture/pass".into(),
        mode: "verify".into(),
        backend: "ptrace".into(),
    };
    let expected = BTreeSet::from([id.clone()]);
    let candidate = |outcome: &str| {
        let row = ResultRow {
            schema: CELL_RESULT_SCHEMA,
            run_id: "fixture-run".into(),
            attempt: 1,
            hermit_sha: "fixture".into(),
            source_tree_dirty: false,
            binary_sha256: Some("b".repeat(64)),
            test_sha256: "c".repeat(64),
            test: id.test.clone(),
            category: id.category.clone(),
            lane: id.lane.clone(),
            mode: id.mode.clone(),
            backend: Some(id.backend.clone()),
            classification: "required".into(),
            outcome: outcome.into(),
            result: Some(if outcome == "PASS" {
                ObservedResult::Pass
            } else {
                ObservedResult::DeterminismFailure
            }),
            failure_class: (outcome != "PASS").then_some(FailureClass::ProductFailure),
            error_kind: None,
            timeout_seconds: 15,
            execution_cpu_timeout_seconds: Some(10),
            execution_wall_timeout_seconds: Some(15),
            log_level: Some("info".into()),
            effective_args: vec!["run".into()],
            argv: vec!["hermit".into(), "run".into()],
            guest_argv: vec!["fixture".into()],
            env: BTreeMap::from([("LC_ALL".into(), "C".into())]),
            cwd: "/repo".into(),
            shell_command: "cd /repo && env LC_ALL=C hermit run".into(),
            relaxations: Vec::new(),
            first_divergent_scheduler_turn: None,
            first_divergent_virtual_nanoseconds: None,
            first_divergent_record: None,
            first_divergent_syscall: None,
            attempts: vec![{
                let report = serde_json::to_string(&canonical_verdict::VerificationReport {
                    verified: true,
                    bitwise_parity: true,
                    verdict: canonical_verdict::Verdict::Matched,
                    no_result_reason: None,
                    infrastructure_error: None,
                    // `Some`: this fixture is a REACHED verdict (verified,
                    // bitwise_parity, "matched"). `None` is reserved for the
                    // documented producer no-result state.
                    comparison: Some(canonical_verdict::ComparisonReport {
                        strictness: canonical_verdict::LogCompareStrictness::Canonical,
                        display_name: Some("BitwiseInfoV1".into()),
                        compare_logs: true,
                        compare_io_buffers: Some(true),
                        log_scope: Some(canonical_verdict::ComparedLogScope::Info),
                        record_envelope: canonical_verdict::RecordEnvelopeReport::AllRecordsV1,
                        virtualize_time: Some(true),
                        strip_lines: Some(false),
                        canonicalize_addresses: Some(true),
                        full_trace: Some(true),
                        exact_remainder: Some(true),
                        stripped_prefixes: Some(vec!["real-wall-clock-prefix/v1".into()]),
                        canonicalizations: Some(vec![
                            "host-address-to-first-appearance-ordinal/v1".into(),
                        ]),
                        ignore_lines: Some(false),
                        skip_commit: Some(false),
                        skip_detlog: Some(false),
                    }),
                    compared_log_messages: Some(canonical_verdict::ComparedLogMessages {
                        left: 1,
                        right: 1,
                    }),
                    // This fixture predates runtime totals. Keep "not recorded"
                    // distinct from a measured zero.
                    runtime: None,
                    dbt_counted_branches: None,
                    guest_exit_code: Some(0),
                    guest_signal: None,
                    // A matched verdict located no divergence, so both
                    // positions are absent -- the same value a pre-field
                    // report carries.
                    first_divergent_scheduler_turn: None,
                    first_divergent_virtual_nanoseconds: None,
                    first_divergent_record: None,
                    first_divergent_syscall: None,
                    first_divergent_left_message: None,
                    first_divergent_right_message: None,
                })
                .unwrap();
                serde_json::json!({
                    "argv":["hermit","run"],
                    "guest_argv":["fixture"],
                    "env":{"LC_ALL":"C"},
                    "cwd":"/repo",
                    "shell_command":"cd /repo && env LC_ALL=C hermit run",
                    "verification_report": report,
                    "verification_report_sha256": format!("{:x}", Sha256::digest(report.as_bytes()))
                })
            }],
        };
        let evidence_identity = row.evidence_identity().unwrap();
        ResultCandidate {
            evidence_identity,
            path: PathBuf::from("fixture/results.jsonl"),
            row,
        }
    };
    let current_identity = candidate("PASS").row;
    let current_report_text = current_identity.attempts[0]["verification_report"]
        .as_str()
        .unwrap();
    let current_report: JsonValue = serde_json::from_str(current_report_text).unwrap();
    if current_report.get("no_result_reason") != Some(&JsonValue::Null) {
        return Err(
            "a current verification report did not serialize explicit null no_result_reason".into(),
        );
    }
    current_identity
        .bitwise_info_comparison()
        .map_err(|error| format!("explicit-null current comparison was refused: {error}"))?;
    current_identity
        .comparison_evidence()
        .map_err(|error| format!("explicit-null current evidence was refused: {error}"))?;

    // These are the two live scorecard ingestion paths, not the retained-history
    // parser. A report produced now must carry the nullable key explicitly so
    // "producer recorded no specific cause" remains distinguishable from "an
    // older producer did not have this field".
    let mut missing_current_reason = current_identity.clone();
    let mut missing_report = current_report;
    missing_report
        .as_object_mut()
        .unwrap()
        .remove("no_result_reason");
    let missing_report = serde_json::to_string(&missing_report).unwrap();
    missing_current_reason.attempts[0]["verification_report_sha256"] =
        JsonValue::String(format!("{:x}", Sha256::digest(missing_report.as_bytes())));
    missing_current_reason.attempts[0]["verification_report"] = JsonValue::String(missing_report);
    for error in [
        missing_current_reason
            .bitwise_info_comparison()
            .unwrap_err(),
        missing_current_reason.comparison_evidence().unwrap_err(),
    ] {
        if !error.contains("no_result_reason") {
            return Err(format!(
                "a live scorecard reader refused a missing current key without naming it: {error}"
            ));
        }
    }
    // Hash-consistent original bytes still must reject duplicates. A Value
    // would collapse both identical and conflicting duplicate fields here.
    for (needle, replacement) in [
        (r#""verified":true"#, r#""verified":false,"verified":true"#),
        (r#""verified":true"#, r#""verified":true,"verified":true"#),
        (
            r#""no_result_reason":null"#,
            r#""no_result_reason":{"kind":"not_run"},"no_result_reason":null"#,
        ),
        (
            r#""no_result_reason":null"#,
            r#""no_result_reason":null,"no_result_reason":null"#,
        ),
        (r#""left":1"#, r#""left":0,"left":1"#),
        (r#""left":1"#, r#""left":1,"left":1"#),
    ] {
        assert_eq!(current_report_text.matches(needle).count(), 1);
        let duplicated = current_report_text.replacen(needle, replacement, 1);
        let mut duplicate_row = current_identity.clone();
        duplicate_row.attempts[0]["verification_report_sha256"] =
            JsonValue::String(format!("{:x}", Sha256::digest(duplicated.as_bytes())));
        duplicate_row.attempts[0]["verification_report"] = JsonValue::String(duplicated);
        for error in [
            duplicate_row.bitwise_info_comparison().unwrap_err(),
            duplicate_row.comparison_evidence().unwrap_err(),
        ] {
            if !error.contains("duplicate field") {
                return Err(format!(
                    "duplicate-field scorecard refusal lost its cause: {error}"
                ));
            }
        }
    }

    current_identity.validate_timeout_policy()?;
    let mut retained_without_explicit_bounds = current_identity.clone();
    retained_without_explicit_bounds.execution_cpu_timeout_seconds = None;
    retained_without_explicit_bounds.execution_wall_timeout_seconds = None;
    retained_without_explicit_bounds.validate_timeout_policy()?;
    if retained_without_explicit_bounds
        .require_current_timeout_policy()
        .is_ok()
    {
        return Err("a current row without explicit execution bounds was accepted".into());
    }
    let mut half_present_timeout = current_identity.clone();
    half_present_timeout.execution_wall_timeout_seconds = None;
    if half_present_timeout.validate_timeout_policy().is_ok() {
        return Err("a half-present additive timeout policy was accepted".into());
    }
    let mut wrong_cpu_timeout = current_identity.clone();
    wrong_cpu_timeout.execution_cpu_timeout_seconds = Some(15);
    if wrong_cpu_timeout.validate_timeout_policy().is_ok() {
        return Err("an execution CPU bound not below wall was accepted".into());
    }
    let mut wrong_wall_timeout = current_identity.clone();
    wrong_wall_timeout.execution_wall_timeout_seconds = Some(14);
    if wrong_wall_timeout.validate_timeout_policy().is_ok() {
        return Err("an execution wall bound disagreeing with timeout_seconds was accepted".into());
    }
    let mut legacy_identity = current_identity.clone();
    legacy_identity.result = None;
    legacy_identity.failure_class = None;
    if current_identity.evidence_identity()? != legacy_identity.evidence_identity()? {
        return Err(
            "adding producer-owned result classification changed the retained evidence identity"
                .into(),
        );
    }
    verify_candidate_set(
        &expected,
        BTreeMap::from([(id.clone(), vec![candidate("PASS")])]),
    )
    .map_err(|e| format!("positive result bracket failed: {e}"))?;
    let mut false_command = candidate("PASS");
    false_command.row.shell_command = "true".into();
    false_command.row.attempts[0]["shell_command"] = serde_json::json!("true");
    if verify_candidate_set(
        &expected,
        BTreeMap::from([(id.clone(), vec![false_command])]),
    )
    .is_ok()
    {
        return Err("result evidence accepted a shell command unrelated to argv/env".into());
    }
    let duplicate = candidate("PASS");
    let mut duplicate_copy = candidate("PASS");
    duplicate_copy.path = PathBuf::from("fixture/copied-results.jsonl");
    verify_candidate_set(
        &expected,
        BTreeMap::from([(id.clone(), vec![duplicate, duplicate_copy])]),
    )
    .map_err(|e| format!("identical duplicate evidence was not deduplicated: {e}"))?;
    let first = candidate("PASS");
    let mut distinct = candidate("PASS");
    distinct.row.run_id = "different-run".into();
    distinct.row.attempt = 2;
    distinct.evidence_identity = distinct.row.evidence_identity().unwrap();
    if verify_candidate_set(
        &expected,
        BTreeMap::from([(id.clone(), vec![first, distinct])]),
    )
    .is_ok()
    {
        return Err("distinct same-SHA evidence was selected by file ordering".into());
    }
    let first = candidate("PASS");
    let mut distinct_relaxation = candidate("PASS");
    distinct_relaxation.row.relaxations = vec!["fixture-relaxation".into()];
    distinct_relaxation.evidence_identity = distinct_relaxation.row.evidence_identity().unwrap();
    if verify_candidate_set(
        &expected,
        BTreeMap::from([(id.clone(), vec![first, distinct_relaxation])]),
    )
    .is_ok()
    {
        return Err("distinct relaxation evidence was treated as the same receipt".into());
    }
    let second_id = CellId {
        test: "fixture/second".into(),
        ..id.clone()
    };
    let mut second_binary = candidate("PASS");
    second_binary.row.test = second_id.test.clone();
    second_binary.row.binary_sha256 = Some("d".repeat(64));
    second_binary.evidence_identity = second_binary.row.evidence_identity().unwrap();
    if verify_candidate_set(
        &BTreeSet::from([id.clone(), second_id.clone()]),
        BTreeMap::from([
            (id.clone(), vec![candidate("PASS")]),
            (second_id, vec![second_binary]),
        ]),
    )
    .is_ok()
    {
        return Err("one source SHA admitted multiple Hermit binary identities".into());
    }
    if verify_candidate_set(&expected, BTreeMap::new()).is_ok() {
        return Err("negative result bracket accepted a missing row".into());
    }
    if verify_candidate_set(
        &expected,
        BTreeMap::from([(id.clone(), vec![candidate("FAIL")])]),
    )
    .is_ok()
    {
        return Err("negative result bracket accepted a failing row".into());
    }
    let failed_first = candidate("FAIL");
    let mut passed_retry = candidate("PASS");
    passed_retry.row.attempt = 2;
    passed_retry.evidence_identity = passed_retry.row.evidence_identity().unwrap();
    verify_candidate_set(
        &expected,
        BTreeMap::from([(id.clone(), vec![failed_first, passed_retry])]),
    )
    .map_err(|e| format!("a passing retry did not supersede attempt 1 for admission: {e}"))?;
    let mut weak = candidate("PASS").row;
    let mut report: JsonValue =
        serde_json::from_str(weak.attempts[0]["verification_report"].as_str().unwrap()).unwrap();
    report["comparison"]["strictness"] = JsonValue::String("stripped".into());
    let report = serde_json::to_string(&report).unwrap();
    weak.attempts[0]["verification_report_sha256"] =
        JsonValue::String(format!("{:x}", Sha256::digest(report.as_bytes())));
    weak.attempts[0]["verification_report"] = JsonValue::String(report);
    if weak.require_canonical_pass_evidence().is_ok() {
        return Err("negative result bracket accepted a stripped PASS receipt".into());
    }
    let mut missing_report = candidate("PASS").row;
    missing_report.attempts[0]
        .as_object_mut()
        .unwrap()
        .remove("verification_report");
    if missing_report.require_canonical_pass_evidence().is_ok() {
        return Err("result evidence without a canonical report was accepted".into());
    }
    let mut missing_hash = candidate("PASS").row;
    missing_hash.binary_sha256 = None;
    if missing_hash.require_provenance().is_ok() {
        return Err("result evidence without an artifact hash was accepted".into());
    }
    let mut missing_relaxations = serde_json::to_value(&candidate("PASS").row).unwrap();
    missing_relaxations
        .as_object_mut()
        .unwrap()
        .remove("relaxations");
    if serde_json::from_value::<ResultRow>(missing_relaxations).is_ok() {
        return Err("result row without explicit relaxations was admitted by the schema".into());
    }
    let chaos_id = CellId {
        mode: "chaos".into(),
        ..id.clone()
    };
    let population = BTreeSet::from([chaos_id.clone()]);
    let custom = |category: &str, test: &str, backend: &str| CellId {
        lane: "portable".into(),
        category: category.into(),
        test: test.into(),
        mode: "custom".into(),
        backend: backend.into(),
    };
    let custom_ids = BTreeSet::from([
        custom(
            "backend-parity-c",
            "backend-parity-c/environment-and-workdir",
            "ptrace",
        ),
        custom("system-utils", "system-utils/clock-determinism", "liteinst"),
        custom("system-utils", "system-utils/clock-determinism", "ptrace"),
    ]);
    let duplicate_fixture = |duplicate_mode: &str| {
        let mut rows = (0..307)
            .map(|index| CellId {
                lane: "portable".into(),
                category: "fixture".into(),
                test: format!("fixture/test-{index:03}"),
                mode: if index == 0 {
                    duplicate_mode.into()
                } else {
                    "verify".into()
                },
                backend: "ptrace".into(),
            })
            .collect::<Vec<_>>();
        rows.push(rows[0].clone());
        rows
    };
    for mode in ["verify", "custom"] {
        let rows = duplicate_fixture(mode);
        let duplicate = display_id(&rows[0]);
        let error = unique_cell_ids("fixture expected plan", rows)
            .expect_err("308 physical rows with 307 identities must be refused");
        if !error.contains("308 physical rows but only 307 unique identities")
            || !error.contains(&duplicate)
        {
            return Err(format!(
                "duplicate {mode} refusal did not name its count and identity: {error}"
            ));
        }
    }
    let mut selected = custom_ids.clone();
    selected.insert(chaos_id.clone());
    let (green, selected_custom) = selected_partition(&selected, &population)?;
    if green != BTreeSet::from([chaos_id.clone()]) {
        return Err("a selected chaos cell was structurally excluded from green".into());
    }
    if selected_custom != custom_ids {
        return Err(
            "selected custom commands were not kept outside the comparable denominator".into(),
        );
    }
    let selected_fixture = Derived {
        population: population.clone(),
        enabled: population.clone(),
        ci_disabled_reasons: BTreeMap::new(),
        not_applicable_reasons: BTreeMap::new(),
        selected,
        green,
        selected_custom: selected_custom.clone(),
    };
    let rendered = render_scorecard(&selected_fixture);
    let status_prose = rendered
        .split("Every selected")
        .next()
        .ok_or("scorecard omitted its status explanation")?;
    for required in [
        "**Green** means this manifest cell is selected by full in `ci/expected-e2e-plan.json`; ordinary validation therefore requires it to pass.",
        "**Red** means the cell is in the manifest but is not selected by full.",
        "**Red does not mean failed:**",
        "Manifest-disabled combinations are **Not applicable**; they are neither red nor omitted.",
    ] {
        if !status_prose.contains(required) {
            return Err(format!(
                "scorecard omitted declarative status prose: {required}"
            ));
        }
    }
    for forbidden in ["enabled", "Green means passing", "Red means failed"] {
        if status_prose.contains(forbidden) {
            return Err(format!(
                "scorecard status prose retained forbidden wording: {forbidden}"
            ));
        }
    }
    if !status_prose.contains("counts Green as **1** and Red as **0**")
        || !status_prose
            .contains("the current **0** manifest-disabled combinations as **Not applicable**")
    {
        return Err("scorecard status prose did not derive its three status counts".into());
    }
    if !USAGE.contains("selected by full in ci/expected-e2e-plan.json")
        || !USAGE.contains(
            "Red means that the cell is in the manifest but is not selected by full; red\ndoes not mean failed",
        )
        || USAGE.contains("until it is measured, promoted")
    {
        return Err("scorecard help contradicted the declarative status prose".into());
    }
    for id in &selected_custom {
        let row = format!(
            "| `{}` | `{}` | `{}` | `custom` | `{}` |",
            id.lane, id.category, id.test, id.backend
        );
        if !rendered.contains(&row) {
            return Err(format!("scorecard omitted selected custom command {row}"));
        }
    }
    if tracked_current_summary(&selected_fixture)
        != "compatibility scorecard: tracked table and 1 comparable cells are current; selected regression denominator 4 = 1 comparable + 3 custom"
    {
        return Err("check summary did not expose the exact selected denominator".into());
    }
    if selected_green(&BTreeSet::new(), &population).contains(&chaos_id) {
        return Err("an unselected chaos cell was accepted as green".into());
    }
    let unaccounted = CellId {
        mode: "future-mode".into(),
        ..id.clone()
    };
    if selected_partition(&BTreeSet::from([unaccounted]), &population).is_ok() {
        return Err("selected denominator accepted a row in neither existing category".into());
    }
    let custom_id = selected_custom.iter().next().unwrap().clone();
    if selected_partition(
        &BTreeSet::from([custom_id.clone()]),
        &BTreeSet::from([custom_id]),
    )
    .is_ok()
    {
        return Err("selected denominator counted a custom command as comparable".into());
    }
    let visible_reason = CiDisabledReasonData {
        result: Some("determinism-failure".into()),
        evidence: Some("ignored/results/liteinst.jsonl".into()),
        reason: "canonical comparison diverged at scheduler turn 10".into(),
    };
    let visible_red = Derived {
        population: BTreeSet::from([id.clone()]),
        enabled: BTreeSet::from([id.clone()]),
        ci_disabled_reasons: BTreeMap::from([(id.clone(), visible_reason.clone())]),
        not_applicable_reasons: BTreeMap::new(),
        selected: BTreeSet::new(),
        green: BTreeSet::new(),
        selected_custom: BTreeSet::new(),
    };
    if !render_scorecard(&visible_red).contains("counts Green as **0** and Red as **1**") {
        return Err("scorecard status prose did not derive its Red count".into());
    }
    let visible_tracked = tracked_from(&visible_red, None, Some("self-test"), false)?;
    if visible_tracked.cells[0].ci_disabled_reason.as_ref() != Some(&visible_reason)
        || !encoded_cells(&visible_tracked)?.contains("ci_disabled_reason")
    {
        return Err("per-backend CI reason was not emitted into tracked scorecard data".into());
    }
    let mut measured_red = visible_tracked.clone();
    measured_red.cells[0].measurement = MeasurementState::MeasuredAndPassed;
    let measured_section = render_measurement_section(&measured_red);
    let current_counts = "In the current generated data, **zero Green cells are \
`never-measured`**.";
    let parser_counts = "The current green/`never-measured` count is **0**, and the current \
red/`measured-and-passed` count is **1**.";
    if !measured_section.contains(current_counts)
        || !measured_section.contains(parser_counts)
        || !measured_section.contains("**1 Red cell that is `measured-and-passed`**")
        || !measured_section.contains("| `red` | 0 | 1 | 0 | 0 | 0 | 1 |")
    {
        return Err(
            "measurement prose did not use the same green/never-measured and red/measured-and-passed counts as its table"
                .into(),
        );
    }
    let measured_row = format!(
        "| `{}` | `{}` | `{}` | `red` | `measured-and-passed` |",
        id.test, id.mode, id.backend
    );
    if !measured_section.contains(&measured_row) {
        return Err("measurement display did not show red and measured-and-passed together".into());
    }
    let unmeasured_section = render_measurement_section(&visible_tracked);
    if unmeasured_section.contains(&measured_row) {
        return Err(
            "measurement display showed measured-and-passed without that measurement".into(),
        );
    }
    if !unmeasured_section.contains("**zero Green cells are `never-measured`**")
        || !unmeasured_section.contains(
            "The current green/`never-measured` count is **0**, and the current \
red/`measured-and-passed` count is **0**.",
        )
        || !unmeasured_section.contains("**0 Red cells that are `measured-and-passed`**")
    {
        return Err("measurement prose kept a stale cross-combination count".into());
    }
    let mut unmeasured_green = visible_tracked.clone();
    unmeasured_green.cells[0].status = CellStatus::Green;
    let unmeasured_green_section = render_measurement_section(&unmeasured_green);
    if !unmeasured_green_section.contains("**1 Green cell is `never-measured`**")
        || !unmeasured_green_section.contains(
            "The current green/`never-measured` count is **1**, and the current \
red/`measured-and-passed` count is **0**.",
        )
    {
        return Err("measurement prose hard-coded the current zero Green count".into());
    }

    let cross_tab_cells = [
        (CellStatus::Red, MeasurementState::NeverMeasured),
        (CellStatus::Red, MeasurementState::MeasuredAndPassed),
        (CellStatus::Green, MeasurementState::MeasuredNoVerdict),
        (CellStatus::Green, MeasurementState::DivergedUnlocated),
        (CellStatus::NotApplicable, MeasurementState::Diverged),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (status, measurement))| {
        let mut cell = visible_tracked.cells[0].clone();
        cell.id.test = format!("cross-tab-{index}");
        cell.status = status;
        cell.measurement = measurement;
        cell.ci_disabled_reason = None;
        cell.not_applicable_reason = (status == CellStatus::NotApplicable)
            .then(|| "fixture backend is disabled for this mode".into());
        cell
    })
    .collect();
    let cross_tab = render_measurement_section(&TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: cross_tab_cells,
    });
    for spelling in [
        "`never-measured`",
        "`measured-and-passed`",
        "`measured-no-verdict`",
        "`diverged-unlocated`",
        "`diverged`",
    ] {
        if !cross_tab.contains(spelling) {
            return Err(format!("measurement cross-tab omitted state {spelling}"));
        }
    }
    for row in [
        "| `green` | 0 | 0 | 1 | 1 | 0 | 2 |",
        "| `red` | 1 | 1 | 0 | 0 | 0 | 2 |",
        "| `not-applicable` | 0 | 0 | 0 | 0 | 1 | 1 |",
        "| **Total** | **1** | **1** | **1** | **1** | **1** | **5** |",
    ] {
        if !cross_tab.contains(row) {
            return Err(format!(
                "measurement cross-tab did not conserve its five-cell fixture: {row}"
            ));
        }
    }
    let not_applicable = Derived {
        population: BTreeSet::from([id.clone()]),
        enabled: BTreeSet::new(),
        ci_disabled_reasons: BTreeMap::new(),
        not_applicable_reasons: BTreeMap::from([(
            id.clone(),
            "fixture backend is disabled for this mode".into(),
        )]),
        selected: BTreeSet::new(),
        green: BTreeSet::new(),
        selected_custom: BTreeSet::new(),
    };
    let status_section = render_scorecard(&not_applicable);
    if !status_section
        .contains("the current **1** manifest-disabled combinations as **Not applicable**")
        || !status_section.contains("| `ptrace` | 0 | 0 | 1 | 1 |")
        || !status_section.contains("| `verify` | 0 / 1 | 0 | 0 | 1 | 1 |")
    {
        return Err("status prose and tables did not use the same manifest-disabled count".into());
    }
    let old_green = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![TrackedCell {
            id: id.clone(),
            enabled: true,
            status: CellStatus::Green,
            ci_disabled_reason: None,
            not_applicable_reason: None,
            last_tested: None,
            observations: Vec::new(),
            measurement: MeasurementState::NeverMeasured,
            green_removal_reason: None,
        }],
    };
    let regressed = Derived {
        population: BTreeSet::from([id.clone()]),
        enabled: BTreeSet::from([id.clone()]),
        ci_disabled_reasons: BTreeMap::new(),
        not_applicable_reasons: BTreeMap::new(),
        selected: BTreeSet::new(),
        green: BTreeSet::new(),
        selected_custom: BTreeSet::new(),
    };
    if tracked_from(&regressed, Some(old_green), None, false).is_ok() {
        return Err("negative ratchet bracket accepted green-to-red movement".into());
    }
    let intentional = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![TrackedCell {
            id: id.clone(),
            enabled: true,
            status: CellStatus::Green,
            ci_disabled_reason: None,
            not_applicable_reason: None,
            last_tested: None,
            observations: Vec::new(),
            measurement: MeasurementState::NeverMeasured,
            green_removal_reason: None,
        }],
    };
    let overridden = tracked_from(
        &regressed,
        Some(intentional.clone()),
        Some("self-test"),
        false,
    )
    .map_err(|e| format!("explicit compatibility-transition bracket failed: {e}"))?;
    // ⚠️ AN OVERRIDE THAT LEAVES NO TRACE IS THE THING THIS FIELD EXISTS TO STOP,
    // so assert the reason actually landed rather than trusting that it did. The
    // guard refusing is only half of it; a reviewer must be able to see, from the
    // tracked file alone, that the refusal was overridden and why.
    match overridden.cells.iter().find(|cell| cell.id == id) {
        Some(cell) if cell.green_removal_reason.as_deref() == Some("self-test") => {}
        Some(cell) => {
            return Err(format!(
                "green-removal override did not record its reason on {}; got {:?}",
                display_id(&id),
                cell.green_removal_reason
            ));
        }
        None => return Err("green-removal override dropped the cell entirely".into()),
    }
    // And it must CLEAR once the cell is green again, or the file accumulates
    // stale justifications that read as live ones.
    let back_to_green = Derived {
        population: BTreeSet::from([id.clone()]),
        enabled: BTreeSet::from([id.clone()]),
        ci_disabled_reasons: BTreeMap::new(),
        not_applicable_reasons: BTreeMap::new(),
        selected: BTreeSet::new(),
        green: BTreeSet::from([id.clone()]),
        selected_custom: BTreeSet::new(),
    };
    let recovered = tracked_from(&back_to_green, Some(overridden), None, false)
        .map_err(|e| format!("recovery after an override was refused: {e}"))?;
    if let Some(cell) = recovered.cells.iter().find(|cell| cell.id == id) {
        if cell.status == CellStatus::Green && cell.green_removal_reason.is_some() {
            return Err(format!(
                "green cell {} still carries a green-removal reason",
                display_id(&id)
            ));
        }
    }

    let mut observed = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![TrackedCell {
            id: id.clone(),
            enabled: false,
            status: CellStatus::Red,
            ci_disabled_reason: None,
            not_applicable_reason: None,
            last_tested: None,
            observations: Vec::new(),
            measurement: MeasurementState::NeverMeasured,
            green_removal_reason: None,
        }],
    };
    // ⚠️ THE FIXTURE CARRIES ALL FOUR COORDINATES, NOT TWO. It used to hardcode
    // `first_divergent_record` and `first_divergent_syscall` to None, so the
    // pressure bracket below could only ever demonstrate that TWO of the four
    // survive this fold -- and the pressure fold is the one that produced the
    // only located observation the tracked corpus has.
    //
    // The other two are derived from `turn` by DIFFERENT factors so each axis's
    // asserted range below is distinct (turn 10..30, record 50..150, syscall
    // 5..15). Four different keyspaces, so a fold that read one coordinate off
    // another's value fails the bracket instead of passing by coincidence.
    let pressure_verification =
        |result: &str, scheduler_turn, virtual_nanoseconds, record, syscall| {
            canonical_verdict::VerificationReport {
                verified: result == "pass",
                bitwise_parity: result == "pass",
                verdict: if result == "pass" {
                    canonical_verdict::Verdict::Matched
                } else {
                    canonical_verdict::Verdict::Diverged
                },
                no_result_reason: None,
                infrastructure_error: None,
                comparison: Some(canonical_verdict::ComparisonReport {
                    strictness: canonical_verdict::LogCompareStrictness::Canonical,
                    display_name: Some("BitwiseInfoV1".into()),
                    compare_logs: true,
                    compare_io_buffers: Some(true),
                    log_scope: Some(canonical_verdict::ComparedLogScope::Info),
                    record_envelope: canonical_verdict::RecordEnvelopeReport::AllRecordsV1,
                    virtualize_time: Some(true),
                    strip_lines: Some(false),
                    canonicalize_addresses: Some(true),
                    full_trace: Some(true),
                    exact_remainder: Some(true),
                    stripped_prefixes: Some(vec!["real-wall-clock-prefix/v1".into()]),
                    canonicalizations: Some(vec![
                        "host-address-to-first-appearance-ordinal/v1".into(),
                    ]),
                    ignore_lines: Some(false),
                    skip_commit: Some(false),
                    skip_detlog: Some(false),
                }),
                compared_log_messages: Some(canonical_verdict::ComparedLogMessages {
                    left: 100,
                    right: 100,
                }),
                first_divergent_scheduler_turn: scheduler_turn,
                first_divergent_virtual_nanoseconds: virtual_nanoseconds,
                first_divergent_record: record,
                first_divergent_syscall: syscall,
                first_divergent_left_message: None,
                first_divergent_right_message: None,
                runtime: None,
                dbt_counted_branches: None,
                guest_exit_code: Some(0),
                guest_signal: None,
            }
        };
    let pressure_row = |result: &str, turn: Option<u64>, virtual_nanoseconds| PressureSummaryRow {
        cell: id.clone(),
        repetition: None,
        attempt: 1,
        result: result.into(),
        verification: Some(pressure_verification(
            result,
            turn,
            virtual_nanoseconds,
            turn.map(|turn| turn * 5),
            turn.map(|turn| turn / 2),
        )),
        evidence_errors: Vec::new(),
        invocation: Some(PressureInvocation {
            run_id: format!("fixture-{result}"),
            argv: vec!["hermit".into(), "run".into()],
            guest_argv: vec!["fixture".into()],
            env: BTreeMap::from([("LC_ALL".into(), "C".into())]),
            cwd: "/workspace/pressure-fixture".into(),
            shell_command: "cd /workspace/pressure-fixture && env LC_ALL=C hermit run".into(),
            attempts: vec![ObservedAttemptInvocation {
                index: "1".into(),
                outcome: if result == "pass" { "PASS" } else { "FAIL" }.into(),
                status: Some(if result == "pass" { 0 } else { 1 }),
                signal: None,
                timed_out: result == "timeout",
                argv: vec!["hermit".into(), "run".into()],
                guest_argv: vec!["fixture".into()],
                env: BTreeMap::from([("LC_ALL".into(), "C".into())]),
                cwd: "/workspace/pressure-fixture".into(),
                shell_command: "cd /workspace/pressure-fixture && env LC_ALL=C hermit run".into(),
            }],
        }),
    };
    // A fixture depth, so the brackets exercise the recorded-depth path rather
    // than skipping it. Two repos with deliberately different magnitudes,
    // because they are different keyspaces and must never be compared.
    let depth_fixture: BTreeMap<String, SourceDepth> = BTreeMap::from([
        (
            "hermit".to_string(),
            SourceDepth {
                commits: 1927,
                first_parent: 1852,
            },
        ),
        (
            "reverie".to_string(),
            SourceDepth {
                commits: 931,
                first_parent: 916,
            },
        ),
    ]);
    let pressure_summary = |sha: &str, tree: &str, rows| PressureSummary {
        schema: PRESSURE_SUMMARY_SCHEMA,
        hermit_sha: sha.into(),
        detcore_tree: tree.into(),
        source_tree_dirty: false,
        rows,
    };
    // ONE CAMPAIGN, MANY ROWS. This is the shape a real pressure run produces:
    // `summarize --results DIR` writes a single summary covering every repeat,
    // so within-campaign rows are what a distribution is built from and they
    // MERGE. The separate `first` summary below then proves the other half of
    // the rule -- a LATER campaign at the same tree RESETS rather than widening.
    // Each repeat is its OWN run with its own run_id, which is what a real
    // campaign produces and what makes the invocations distinguishable.
    let pressure_repeat = |repetition: u64, result: &str, turn, virtual_nanoseconds| {
        let mut row = pressure_row(result, turn, virtual_nanoseconds);
        row.repetition = Some(repetition);
        if let Some(invocation) = row.invocation.as_mut() {
            invocation.run_id = format!("fixture-{result}-rep{repetition}");
        }
        row
    };
    let mut retried = observed.clone();
    let first_attempt = pressure_repeat(1, "determinism-failure", Some(20), Some(500));
    let mut second_attempt = pressure_repeat(1, "pass", None, None);
    second_attempt.attempt = 2;
    if let Some(invocation) = second_attempt.invocation.as_mut() {
        invocation.run_id = "fixture-pass-rep1-attempt2".into();
    }
    let retried_outcome = apply_pressure_summary(
        &mut retried,
        &pressure_summary("sha-1", "tree-1", vec![first_attempt, second_attempt]),
        "sha-1",
        "tree-1",
        &depth_fixture,
    )
    .map_err(|e| format!("green retry pressure-observation bracket failed: {e}"))?;
    let retry_observation = &retried.cells[0].observations[0];
    if retried_outcome.rows != 2
        || retry_observation.results
            != BTreeSet::from([ObservedResult::Pass, ObservedResult::DeterminismFailure])
        || retry_observation.first_divergent_scheduler_turn.range()
            != Some(ObservedRange {
                earliest: 20,
                latest: 20,
                samples: 1,
            })
        || retry_observation.invocations.len() != 2
    {
        return Err(
            "a passing retry did not preserve the earlier divergence as a separate observation"
                .into(),
        );
    }
    // Kept as a single-row summary for the refusal brackets below, which need a
    // valid summary to corrupt.
    let first = pressure_summary(
        "sha-1",
        "tree-1",
        vec![pressure_row("determinism-failure", Some(20), Some(500))],
    );
    let campaign = pressure_summary(
        "sha-1",
        "tree-1",
        vec![
            pressure_repeat(1, "determinism-failure", Some(20), Some(500)),
            pressure_repeat(2, "determinism-failure", Some(10), Some(900)),
            pressure_repeat(3, "timeout", None, None),
            pressure_repeat(4, "replay-failure", Some(30), Some(1000)),
        ],
    );
    apply_pressure_summary(&mut observed, &campaign, "sha-1", "tree-1", &depth_fixture)
        .map_err(|e| format!("pressure-observation campaign bracket failed: {e}"))?;
    let once = encoded_cells(&observed)?;
    let mut repeated = observed.clone();
    apply_pressure_summary(&mut repeated, &campaign, "sha-1", "tree-1", &depth_fixture)
        .map_err(|e| format!("repeated pressure-observation bracket failed: {e}"))?;
    let twice = encoded_cells(&repeated)?;
    if twice != once {
        let first_difference = once
            .lines()
            .zip(twice.lines())
            .enumerate()
            .find(|(_, (left, right))| left != right)
            .map(|(index, (left, right))| {
                format!("line {}: once={left:?} twice={right:?}", index + 1)
            })
            .unwrap_or_else(|| format!("byte lengths differ: {} vs {}", once.len(), twice.len()));
        return Err(format!(
            "reapplying one pressure summary changed the stored observation: {first_difference}"
        ));
    }
    let mut stored_once: TrackedCells = serde_json::from_str(&once)
        .map_err(|error| format!("cannot reload stored pressure observation: {error}"))?;
    apply_pressure_summary(
        &mut stored_once,
        &campaign,
        "sha-1",
        "tree-1",
        &depth_fixture,
    )
    .map_err(|error| format!("stored pressure-observation bracket failed: {error}"))?;
    let stored_twice = encoded_cells(&stored_once)?;
    if stored_twice != once {
        return Err(
            "reapplying one pressure summary after a write/read round trip duplicated coordinates"
                .into(),
        );
    }
    // Older stored observations retain the derived command text, while new
    // imports omit it. Reload each representation before repeating the same
    // campaign: neither the stored evidence nor the coordinate samples may grow.
    for legacy_fields in [1, 2, 3] {
        let mut legacy: TrackedCells = serde_json::from_str(&once)
            .map_err(|error| format!("cannot load legacy pressure fixture: {error}"))?;
        for cell in &mut legacy.cells {
            for observation in &mut cell.observations {
                observation.invocations = std::mem::take(&mut observation.invocations)
                    .into_iter()
                    .map(|mut invocation| {
                        if legacy_fields & 1 != 0 {
                            invocation.shell_command = literal_shell_command(
                                &invocation.cwd,
                                &invocation.env,
                                &invocation.argv,
                            );
                        }
                        if legacy_fields & 2 != 0 {
                            for attempt in &mut invocation.attempts {
                                attempt.shell_command = literal_shell_command(
                                    &attempt.cwd,
                                    &attempt.env,
                                    &attempt.argv,
                                );
                            }
                        }
                        invocation
                    })
                    .collect();
            }
        }
        let legacy_bytes = encoded_cells(&legacy)?;
        let mut reloaded: TrackedCells = serde_json::from_str(&legacy_bytes)
            .map_err(|error| format!("cannot reload legacy pressure fixture: {error}"))?;
        apply_pressure_summary(&mut reloaded, &campaign, "sha-1", "tree-1", &depth_fixture)
            .map_err(|error| format!("legacy pressure reimport failed: {error}"))?;
        if encoded_cells(&reloaded)? != legacy_bytes {
            return Err(format!(
                "reimport changed legacy pressure evidence or duplicated coordinates (command fields {legacy_fields})"
            ));
        }
    }
    let compact = observed.cells[0].observations[0]
        .invocations
        .iter()
        .next()
        .ok_or("pressure identity fixture has no invocation")?
        .clone();
    let mut legacy = compact.clone();
    legacy.shell_command = literal_shell_command(&legacy.cwd, &legacy.env, &legacy.argv);
    for attempt in &mut legacy.attempts {
        attempt.shell_command = literal_shell_command(&attempt.cwd, &attempt.env, &attempt.argv);
    }
    if !pressure_invocation_matches(&legacy, &compact) {
        return Err("valid legacy pressure invocation differs from its compact form".into());
    }
    for change in [
        "command",
        "attempt-command",
        "attempt-status",
        "attempt-index",
        "run-id",
    ] {
        let mut distinct = legacy.clone();
        match change {
            "command" => distinct.shell_command.push_str(" ; false"),
            "attempt-command" => distinct.attempts[0].shell_command.push_str(" ; false"),
            "attempt-status" => distinct.attempts[0].status = Some(37),
            "attempt-index" => distinct.attempts[0].index.push_str("-different"),
            "run-id" => distinct.run_id.push_str("-different"),
            _ => unreachable!(),
        }
        if pressure_invocation_matches(&distinct, &compact) {
            return Err(format!(
                "pressure invocation identity discarded distinct {change} evidence"
            ));
        }
    }
    let same_engine = pressure_summary("sha-doc", "tree-1", vec![pressure_row("pass", None, None)]);
    apply_pressure_summary(
        &mut observed,
        &same_engine,
        "sha-doc",
        "tree-1",
        &depth_fixture,
    )
    .map_err(|e| format!("same-Detcore-tree pressure-observation bracket failed: {e}"))?;
    let observation = &observed.cells[0].observations[0];
    // SAMPLES IS NOT THE RUN COUNT, which is exactly why it has to be stored
    // rather than derived. This bracket folds FIVE runs -- a four-repetition
    // campaign plus one run at a second hermit sha on the same detcore tree --
    // giving FIVE distinct invocations. Only THREE of them LOCATED a divergence
    // position: the two determinism failures and the replay failure. The pass
    // and the timeout contributed nothing to these bounds.
    //
    // Reporting "earliest 10, latest 30" against an implied five runs would
    // overstate the evidence by two thirds.
    if observation.first_divergent_scheduler_turn.range()
!= Some(ObservedRange {
            earliest: 10,
            latest: 30,
            samples: 3,
        })
        || observation.first_divergent_virtual_nanoseconds.range()
!= Some(ObservedRange {
                earliest: 500,
                latest: 1000,
                samples: 3,
            })
        // The third and fourth coordinates, asserted on the SAME three located
        // runs as the two above. Their ranges are deliberately disjoint from the
        // turn range, so this cannot pass by reading one axis off another.
        || observation.first_divergent_record.range()
!= Some(ObservedRange {
                earliest: 50,
                latest: 150,
                samples: 3,
            })
        || observation.first_divergent_syscall.range()
!= Some(ObservedRange {
                earliest: 5,
                latest: 15,
                samples: 3,
            })
        || observation.provenance != ObservationProvenance::PressureTest
        || observation.results
            != BTreeSet::from([
                ObservedResult::Pass,
                ObservedResult::DeterminismFailure,
                ObservedResult::ReplayFailure,
                ObservedResult::Timeout,
            ])
        || observation.hermit_shas != BTreeSet::from(["sha-1".into(), "sha-doc".into()])
        || observation.invocations.len() != 5
        || !observation.invocations.iter().any(|invocation| {
            invocation.hermit_sha == "sha-doc"
                && invocation.result == Some(ObservedResult::Pass)
                && invocation.run_id == "fixture-pass"
                && invocation.argv == ["hermit", "run"]
                && invocation.guest_argv == ["fixture"]
                && invocation.env == BTreeMap::from([("LC_ALL".into(), "C".into())])
                && invocation.cwd == "/repo"
                && invocation.shell_command.is_empty()
                && literal_shell_command(&invocation.cwd, &invocation.env, &invocation.argv)
                    == "cd /repo && env LC_ALL=C hermit run"
                && invocation.attempts.len() == 1
                && invocation.attempts[0].index == "1"
                && invocation.attempts[0].outcome == "PASS"
                && invocation.attempts[0].status == Some(0)
                && invocation.attempts[0].shell_command.is_empty()
                && literal_shell_command(
                    &invocation.attempts[0].cwd,
                    &invocation.attempts[0].env,
                    &invocation.attempts[0].argv,
                ) == "cd /repo && env LC_ALL=C hermit run"
        })
    {
        return Err(
            "pressure observations did not preserve ranges, outcomes, and literal invocations"
                .into(),
        );
    }

    // VALIDATE BRACKET. A validate fold at the SAME detcore tree must land in
    // its OWN observation and must not touch the pressure-test bounds above.
    // Merging them would produce one range moving for two unrelated causes --
    // "the code changed" and "this varies run to run".
    let validate_id = observed.cells[0].id.clone();
    let validate_attempt = |outcome: &str| {
        let (verified, bitwise_parity, verdict, comparison, counts) = match outcome {
            "PASS" => (
                true,
                true,
                "matched",
                serde_json::json!({
                    "strictness": "canonical",
                    "display_name": "BitwiseInfoV1",
                    "compare_logs": true,
                    "compare_io_buffers": true,
                    "log_scope": "info",
                    "record_envelope": "all_records_v1",
                    "virtualize_time": true,
                    "strip_lines": false,
                    "canonicalize_addresses": true,
                    "full_trace": true,
                    "exact_remainder": true,
                    "stripped_prefixes": ["real-wall-clock-prefix/v1"],
                    "canonicalizations": ["host-address-to-first-appearance-ordinal/v1"],
                    "ignore_lines": false,
                    "skip_commit": false,
                    "skip_detlog": false
                }),
                serde_json::json!({"left": 10, "right": 10}),
            ),
            "FAIL" => (
                false,
                false,
                "diverged",
                serde_json::json!({
                    "strictness": "canonical",
                    "display_name": "BitwiseInfoV1",
                    "compare_logs": true,
                    "compare_io_buffers": true,
                    "log_scope": "info",
                    "record_envelope": "all_records_v1",
                    "virtualize_time": true,
                    "strip_lines": false,
                    "canonicalize_addresses": true,
                    "full_trace": true,
                    "exact_remainder": true,
                    "stripped_prefixes": ["real-wall-clock-prefix/v1"],
                    "canonicalizations": ["host-address-to-first-appearance-ordinal/v1"],
                    "ignore_lines": false,
                    "skip_commit": false,
                    "skip_detlog": false
                }),
                serde_json::json!({"left": 10, "right": 10}),
            ),
            _ => (false, false, "no_result", JsonValue::Null, JsonValue::Null),
        };
        let report = serde_json::to_string(&serde_json::json!({
            "verified": verified,
            "bitwise_parity": bitwise_parity,
            "verdict": verdict,
            "no_result_reason": if matches!(outcome, "PASS" | "FAIL") {
                JsonValue::Null
            } else {
                serde_json::json!({"kind": "not_run"})
            },
            "infrastructure_error": null,
            "comparison": comparison,
            "compared_log_messages": counts,
            "guest_exit_code": if outcome == "PASS" { Some(0) } else { None },
            "guest_signal": null,
            "first_divergent_scheduler_turn": if outcome == "FAIL" { Some(7) } else { None },
            "first_divergent_virtual_nanoseconds": if outcome == "FAIL" { Some(70) } else { None },
            "first_divergent_record": if outcome == "FAIL" { Some(12) } else { None },
            "first_divergent_syscall": if outcome == "FAIL" { Some(9) } else { None },
            "first_divergent_left_message": null,
            "first_divergent_right_message": null
        }))
        .unwrap();
        serde_json::json!({
            "index": "1",
            "outcome": outcome,
            "error_kind": if matches!(outcome, "PASS" | "FAIL") {
                JsonValue::Null
            } else {
                serde_json::json!("incomplete-verification-evidence")
            },
            "status": if outcome == "PASS" {
                serde_json::json!(0)
            } else if outcome == "FAIL" {
                serde_json::json!(1)
            } else {
                JsonValue::Null
            },
            "signal": if matches!(outcome, "PASS" | "FAIL") {
                JsonValue::Null
            } else {
                serde_json::json!(15)
            },
            "timed_out": !matches!(outcome, "PASS" | "FAIL"),
            "argv": ["hermit", "run"],
            "guest_argv": ["fixture"],
            "env": {"LC_ALL": "C"},
            "cwd": "/repo",
            "shell_command": "cd /repo && env LC_ALL=C hermit run",
            "verification_report_sha256": format!("{:x}", Sha256::digest(report.as_bytes())),
            "verification_report": report
        })
    };
    let validate_row = ResultRow {
        schema: CELL_RESULT_SCHEMA,
        run_id: "validate-bracket".into(),
        attempt: 1,
        hermit_sha: "sha-1".into(),
        source_tree_dirty: false,
        binary_sha256: Some("b".repeat(64)),
        test_sha256: "c".repeat(64),
        test: validate_id.test.clone(),
        category: validate_id.category.clone(),
        lane: validate_id.lane.clone(),
        mode: validate_id.mode.clone(),
        backend: Some(validate_id.backend.clone()),
        classification: "required".into(),
        outcome: "FAIL".into(),
        result: Some(ObservedResult::DeterminismFailure),
        failure_class: Some(FailureClass::ProductFailure),
        error_kind: None,
        timeout_seconds: 15,
        execution_cpu_timeout_seconds: Some(10),
        execution_wall_timeout_seconds: Some(15),
        log_level: Some("info".into()),
        effective_args: vec!["run".into()],
        argv: vec!["hermit".into(), "run".into()],
        guest_argv: vec!["fixture".into()],
        env: BTreeMap::from([("LC_ALL".into(), "C".into())]),
        cwd: "/repo".into(),
        shell_command: "cd /repo && env LC_ALL=C hermit run".into(),
        relaxations: Vec::new(),
        first_divergent_scheduler_turn: Some(7),
        first_divergent_virtual_nanoseconds: Some(70),
        first_divergent_record: Some(12),
        first_divergent_syscall: Some(9),
        attempts: vec![validate_attempt("FAIL")],
    };
    let validate_candidate = |run_id: &str, coordinates: DivergenceCoordinates| {
        let mut row = validate_row.clone();
        row.run_id = run_id.into();
        row.first_divergent_scheduler_turn = coordinates.scheduler_turn;
        row.first_divergent_virtual_nanoseconds = coordinates.virtual_nanoseconds;
        row.first_divergent_record = coordinates.record;
        row.first_divergent_syscall = coordinates.syscall;
        let mut report: JsonValue = serde_json::from_str(
            row.attempts[0]["verification_report"]
                .as_str()
                .expect("fixture report is a string"),
        )
        .expect("fixture report is JSON");
        report["first_divergent_scheduler_turn"] = serde_json::json!(coordinates.scheduler_turn);
        report["first_divergent_virtual_nanoseconds"] =
            serde_json::json!(coordinates.virtual_nanoseconds);
        report["first_divergent_record"] = serde_json::json!(coordinates.record);
        report["first_divergent_syscall"] = serde_json::json!(coordinates.syscall);
        let report = serde_json::to_string(&report).expect("fixture report serializes");
        row.attempts[0]["verification_report_sha256"] =
            JsonValue::String(format!("{:x}", Sha256::digest(report.as_bytes())));
        row.attempts[0]["verification_report"] = JsonValue::String(report);
        let evidence_identity = row.evidence_identity().expect("fixture has identity");
        ResultCandidate {
            evidence_identity,
            path: PathBuf::from("fixture/results.jsonl"),
            row,
        }
    };
    let pressure_at = |result: &str, coordinates: DivergenceCoordinates| {
        let mut row = pressure_row(
            result,
            coordinates.scheduler_turn,
            coordinates.virtual_nanoseconds,
        );
        row.verification = Some(pressure_verification(
            result,
            coordinates.scheduler_turn,
            coordinates.virtual_nanoseconds,
            coordinates.record,
            coordinates.syscall,
        ));
        row
    };
    let current_result = |row: PressureSummaryRow| {
        let result = ObservedResult::parse(&row.result).expect("fixture result parses");
        let coordinates = row
            .verification
            .as_ref()
            .map(|report| DivergenceCoordinates {
                scheduler_turn: report.first_divergent_scheduler_turn,
                virtual_nanoseconds: report.first_divergent_virtual_nanoseconds,
                record: report.first_divergent_record,
                syscall: report.first_divergent_syscall,
            })
            .unwrap_or(DivergenceCoordinates {
                scheduler_turn: None,
                virtual_nanoseconds: None,
                record: None,
                syscall: None,
            });
        CurrentPressureResult {
            summary: pressure_summary("sha-1", "tree-1", vec![row]),
            result,
            coordinates,
            missing_retained_logs: false,
        }
    };
    let current_run = |run_id: &str, mut row: PressureSummaryRow| {
        row.invocation
            .as_mut()
            .expect("fixture current row has invocation")
            .run_id = run_id.into();
        current_result(row)
    };
    let current_run_at = |run_id: &str, cwd: &str, mut row: PressureSummaryRow| {
        let invocation = row
            .invocation
            .as_mut()
            .expect("fixture current row has invocation");
        invocation.run_id = run_id.into();
        invocation.cwd = cwd.into();
        invocation.shell_command = format!("cd {cwd} && env LC_ALL=C hermit run");
        invocation.attempts[0].cwd = cwd.into();
        invocation.attempts[0].shell_command = invocation.shell_command.clone();
        current_result(row)
    };
    let retained_cell = |candidates: Vec<ResultCandidate>| RetainedCellResults {
        id: validate_id.clone(),
        hermit_sha: "sha-1".into(),
        detcore_tree: "tree-1".into(),
        depth: depth_fixture.clone(),
        candidates,
    };
    let coordinates = |turn, virtual_nanoseconds, record, syscall| DivergenceCoordinates {
        scheduler_turn: turn,
        virtual_nanoseconds,
        record,
        syscall,
    };

    let mut mismatched_coordinates = validate_candidate(
        "mismatched-coordinate",
        coordinates(Some(3), Some(30), Some(16), Some(7)),
    );
    mismatched_coordinates.row.first_divergent_record = Some(17);
    if mismatched_coordinates.row.bitwise_info_comparison().is_ok() {
        return Err(
            "a top-level divergence coordinate that disagreed with its embedded report was accepted"
                .into(),
        );
    }

    // FOUR RETAINED-COMPARISON OUTCOMES. These use the measured shapes that
    // forced the distinction: 16 stayed 16; 111/119 moved to 117; a single
    // match cannot establish that 407 is gone; and 330 cannot be checked
    // because the current row is rejected.
    let fresh_coordinates = coordinates(Some(3), Some(30), Some(16), Some(7));
    let fresh = retained_coordinate_decision(
        retained_cell(vec![validate_candidate(
            "fresh-retained",
            fresh_coordinates,
        )]),
        &CurrentPressureEvidence {
            results: BTreeMap::from([(
                validate_id.clone(),
                vec![current_run(
                    "fresh-current",
                    pressure_at("determinism-failure", fresh_coordinates),
                )],
            )]),
            uncheckable: BTreeMap::new(),
        },
    );
    if fresh.state != RetainedComparisonState::Fresh
        || !matches!(
            fresh.import,
            ImportEvidence::Retained {
                store_positions: true,
                ..
            }
        )
    {
        return Err("equal retained and current coordinates were not FRESH".into());
    }

    for (label, changed) in [
        (
            "scheduler turn",
            coordinates(Some(4), Some(30), Some(16), Some(7)),
        ),
        (
            "virtual nanoseconds",
            coordinates(Some(3), Some(31), Some(16), Some(7)),
        ),
        ("record", coordinates(Some(3), Some(30), Some(17), Some(7))),
        ("syscall", coordinates(Some(3), Some(30), Some(16), Some(8))),
    ] {
        let changed_decision = retained_coordinate_decision(
            retained_cell(vec![validate_candidate(
                "field-retained",
                fresh_coordinates,
            )]),
            &CurrentPressureEvidence {
                results: BTreeMap::from([(
                    validate_id.clone(),
                    vec![current_run(
                        "changed-current",
                        pressure_at("determinism-failure", changed),
                    )],
                )]),
                uncheckable: BTreeMap::new(),
            },
        );
        if changed_decision.state != RetainedComparisonState::Drifted {
            return Err(format!(
                "changing only the {label} coordinate did not produce DRIFTED"
            ));
        }
    }

    let drifted = retained_coordinate_decision(
        retained_cell(vec![
            validate_candidate(
                "drifted-retained-a",
                coordinates(Some(3), Some(30), Some(111), Some(7)),
            ),
            validate_candidate(
                "drifted-retained-b",
                coordinates(Some(3), Some(30), Some(119), Some(7)),
            ),
        ]),
        &CurrentPressureEvidence {
            results: BTreeMap::from([(
                validate_id.clone(),
                vec![current_run(
                    "drifted-current",
                    pressure_at(
                        "determinism-failure",
                        coordinates(Some(3), Some(30), Some(117), Some(7)),
                    ),
                )],
            )]),
            uncheckable: BTreeMap::new(),
        },
    );
    if drifted.state != RetainedComparisonState::Drifted
        || !matches!(drifted.import, ImportEvidence::None)
    {
        return Err("changed retained coordinates were not replaced as DRIFTED".into());
    }

    let mut pass_row = pressure_at("pass", coordinates(None, None, None, None));
    pass_row.verification = None;
    let one_match = retained_coordinate_decision(
        retained_cell(vec![validate_candidate(
            "wrong-retained",
            coordinates(Some(3), Some(30), Some(407), Some(7)),
        )]),
        &CurrentPressureEvidence {
            results: BTreeMap::from([(
                validate_id.clone(),
                vec![current_run("one-match", pass_row.clone())],
            )]),
            uncheckable: BTreeMap::new(),
        },
    );
    if one_match.state != RetainedComparisonState::Uncheckable
        || !matches!(
            one_match.import,
            ImportEvidence::Retained {
                store_positions: false,
                ..
            }
        )
    {
        return Err(
            "one matching run was treated as proof that a retained coordinate is WRONG".into(),
        );
    }
    let wrong = retained_coordinate_decision(
        retained_cell(vec![validate_candidate(
            "wrong-retained",
            coordinates(Some(3), Some(30), Some(407), Some(7)),
        )]),
        &CurrentPressureEvidence {
            results: BTreeMap::from([(
                validate_id.clone(),
                vec![
                    current_run_at("same-cell-run-id", "/repo/match-one", pass_row.clone()),
                    current_run_at("same-cell-run-id", "/repo/match-two", pass_row),
                ],
            )]),
            uncheckable: BTreeMap::new(),
        },
    );
    if wrong.state != RetainedComparisonState::Wrong
        || !matches!(wrong.import, ImportEvidence::None)
    {
        return Err(
            "two distinct matching runs did not classify a retained divergence as WRONG".into(),
        );
    }

    let mut intermittent_pass = pressure_at("pass", coordinates(None, None, None, None));
    intermittent_pass.verification = None;
    let intermittent = retained_coordinate_decision(
        retained_cell(vec![validate_candidate(
            "intermittent-retained",
            fresh_coordinates,
        )]),
        &CurrentPressureEvidence {
            results: BTreeMap::from([(
                validate_id.clone(),
                vec![
                    current_run("intermittent-match", intermittent_pass),
                    current_run(
                        "intermittent-divergence",
                        pressure_at("determinism-failure", fresh_coordinates),
                    ),
                ],
            )]),
            uncheckable: BTreeMap::new(),
        },
    );
    if intermittent.state != RetainedComparisonState::Fresh {
        return Err(
            "a matching run hid a later current divergence at the retained coordinate".into(),
        );
    }

    let mut uncheckable_row = pressure_at(
        "infrastructure-error",
        coordinates(Some(3), Some(30), Some(330), Some(7)),
    );
    uncheckable_row.evidence_errors = vec![
        "terminal verify result must retain exactly one nonempty run1 log and one nonempty run2 log"
            .into(),
    ];
    let uncheckable_summary = pressure_summary("sha-1", "tree-1", vec![uncheckable_row]);
    let uncheckable_error = checked_current_pressure_result(
        &TrackedCells {
            schema: SCHEMA,
            projection: None,
            cells: vec![TrackedCell {
                id: validate_id.clone(),
                enabled: true,
                status: CellStatus::Red,
                ci_disabled_reason: None,
                not_applicable_reason: None,
                last_tested: None,
                observations: Vec::new(),
                measurement: MeasurementState::NeverMeasured,
                green_removal_reason: None,
            }],
        },
        uncheckable_summary,
        "tree-1",
    )
    .err()
    .ok_or("a current row with missing retained logs was accepted")?;

    // DBT can return a complete typed canonical divergence report while its
    // evidence transport fails to retain the two raw run logs. The general
    // pressure writer above still refuses that shape. This import-only check
    // admits exactly that report so a current coordinate can replace a retained
    // one instead of being misreported as infrastructure trouble.
    let mut dbt_id = validate_id.clone();
    dbt_id.backend = "dbt".into();
    let dbt_tracked = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![TrackedCell {
            id: dbt_id.clone(),
            enabled: true,
            status: CellStatus::Red,
            ci_disabled_reason: None,
            not_applicable_reason: None,
            last_tested: None,
            observations: Vec::new(),
            measurement: MeasurementState::NeverMeasured,
            green_removal_reason: None,
        }],
    };
    let mut dbt_row = pressure_at(
        "infrastructure-error",
        coordinates(Some(3), Some(30), Some(330), Some(7)),
    );
    dbt_row.cell = dbt_id.clone();
    dbt_row.evidence_errors = vec![MISSING_RETAINED_VERIFY_LOGS.into()];
    let admitted_dbt = checked_current_pressure_result(
        &dbt_tracked,
        pressure_summary("sha-1", "tree-1", vec![dbt_row.clone()]),
        "tree-1",
    )
    .map_err(|error| {
        format!("a typed current DBT canonical divergence was not admitted: {error}")
    })?;
    if !admitted_dbt.missing_retained_logs
        || admitted_dbt.result != ObservedResult::DeterminismFailure
        || admitted_dbt.coordinates.record != Some(330)
    {
        return Err(
            "current DBT canonical divergence was not preserved as a located product result".into(),
        );
    }
    let mut extra_error = dbt_row.clone();
    extra_error
        .evidence_errors
        .push("second evidence error".into());
    if checked_current_pressure_result(
        &dbt_tracked,
        pressure_summary("sha-1", "tree-1", vec![extra_error]),
        "tree-1",
    )
    .is_ok()
    {
        return Err("DBT missing-log admission cleared an unrelated evidence error".into());
    }
    let mut weak_dbt = dbt_row;
    weak_dbt
        .verification
        .as_mut()
        .and_then(|report| report.comparison.as_mut())
        .expect("fixture report has comparison")
        .strictness = canonical_verdict::LogCompareStrictness::Stripped;
    if checked_current_pressure_result(
        &dbt_tracked,
        pressure_summary("sha-1", "tree-1", vec![weak_dbt]),
        "tree-1",
    )
    .is_ok()
    {
        return Err("DBT missing-log admission accepted a non-canonical comparison".into());
    }

    let uncheckable = retained_coordinate_decision(
        retained_cell(vec![validate_candidate(
            "uncheckable-retained",
            coordinates(Some(3), Some(30), Some(330), Some(7)),
        )]),
        &CurrentPressureEvidence {
            results: BTreeMap::new(),
            uncheckable: BTreeMap::from([(validate_id.clone(), vec![uncheckable_error])]),
        },
    );
    if uncheckable.state != RetainedComparisonState::Uncheckable {
        return Err("an untrustworthy current comparison was not UNCHECKABLE".into());
    }
    let ImportEvidence::Retained {
        results: uncheckable_results,
        store_positions: false,
    } = uncheckable.import
    else {
        return Err("UNCHECKABLE discarded the retained canonical comparison".into());
    };
    let mut uncheckable_tracked = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![TrackedCell {
            id: validate_id.clone(),
            enabled: true,
            status: CellStatus::Red,
            ci_disabled_reason: None,
            not_applicable_reason: None,
            last_tested: None,
            observations: Vec::new(),
            measurement: MeasurementState::NeverMeasured,
            green_removal_reason: None,
        }],
    };
    let uncheckable_rows = BTreeMap::from([(
        uncheckable_results.id.clone(),
        uncheckable_results.candidates.clone(),
    )]);
    apply_validate_results(
        &mut uncheckable_tracked,
        &uncheckable_rows,
        &uncheckable_results.hermit_sha,
        &uncheckable_results.detcore_tree,
        &uncheckable_results.depth,
        false,
        false,
    )?;
    refresh_measurement(&mut uncheckable_tracked);
    let uncheckable_observation = &uncheckable_tracked.cells[0].observations[0];
    if uncheckable_tracked.cells[0].measurement != MeasurementState::DivergedUnlocated
        || uncheckable_observation.canonical_comparisons.len() != 1
        || uncheckable_observation
            .first_divergent_scheduler_turn
            .range()
            .is_some()
        || uncheckable_observation
            .first_divergent_virtual_nanoseconds
            .range()
            .is_some()
        || uncheckable_observation
            .first_divergent_record
            .range()
            .is_some()
        || uncheckable_observation
            .first_divergent_syscall
            .range()
            .is_some()
    {
        return Err(
            "UNCHECKABLE did not retain the canonical comparison while withholding all four coordinates"
                .into(),
        );
    }
    if checked_current_pressure_result(
        &TrackedCells {
            schema: SCHEMA,
            projection: None,
            cells: vec![TrackedCell {
                id: validate_id.clone(),
                enabled: true,
                status: CellStatus::Red,
                ci_disabled_reason: None,
                not_applicable_reason: None,
                last_tested: None,
                observations: Vec::new(),
                measurement: MeasurementState::NeverMeasured,
                green_removal_reason: None,
            }],
        },
        pressure_summary("sha-1", "tree-1", Vec::new()),
        "tree-1",
    )
    .is_ok()
    {
        return Err("an empty current pressure summary was accepted".into());
    }

    let red_import_fixture = Derived {
        population: BTreeSet::from([validate_id.clone()]),
        enabled: BTreeSet::from([validate_id.clone()]),
        ci_disabled_reasons: BTreeMap::new(),
        not_applicable_reasons: BTreeMap::new(),
        selected: BTreeSet::new(),
        green: BTreeSet::new(),
        selected_custom: BTreeSet::new(),
    };
    if retained_import_cells(&red_import_fixture) != BTreeSet::from([validate_id.clone()]) {
        return Err("an enabled red cell was excluded from retained import".into());
    }

    let rows = BTreeMap::from([(
        validate_id.clone(),
        vec![ResultCandidate {
            evidence_identity: "validate-bracket".into(),
            path: PathBuf::from("fixture/results.jsonl"),
            row: validate_row.clone(),
        }],
    )]);
    apply_validate_results(
        &mut observed,
        &rows,
        "sha-1",
        "tree-1",
        &depth_fixture,
        true,
        true,
    )
    .map_err(|e| format!("validate-observation bracket failed: {e}"))?;
    let pressure = observed.cells[0]
        .observations
        .iter()
        .find(|o| o.provenance == ObservationProvenance::PressureTest)
        .ok_or("validate fold destroyed the pressure-test observation")?;
    if pressure.first_divergent_scheduler_turn.range()
        != Some(ObservedRange {
            earliest: 10,
            latest: 30,
            samples: 3,
        })
    {
        return Err("a validate fold at the same tree contaminated the pressure-test range".into());
    }
    let from_validate = observed.cells[0]
        .observations
        .iter()
        .find(|o| o.provenance == ObservationProvenance::Validate)
        .ok_or("validate fold recorded no validate-provenance observation")?;
    // `samples: 1` is the honest reading of a single validate run: a POINT, not
    // a distribution. Validate runs a cell once per commit, so it cannot
    // produce a range at one tree by itself.
    if from_validate.first_divergent_scheduler_turn.range()
!= Some(ObservedRange {
            earliest: 7,
            latest: 7,
            samples: 1,
        })
        || from_validate.first_divergent_virtual_nanoseconds.range()
!= Some(ObservedRange {
                earliest: 70,
                latest: 70,
                samples: 1,
            })
        // The third coordinate, from hermit#2386. Unlike the two above it
        // LOCATES the divergence rather than bounding it, so it is the one a
        // reader should trust for "how far did the run get".
        || from_validate.first_divergent_record.range()
!= Some(ObservedRange {
                earliest: 12,
                latest: 12,
                samples: 1,
            })
        // The fourth unit, a different keyspace again: 12 records in but only
        // 9 syscalls completed.
        || from_validate.first_divergent_syscall.range()
!= Some(ObservedRange {
                earliest: 9,
                latest: 9,
                samples: 1,
            })
        || from_validate.results != BTreeSet::from([ObservedResult::DeterminismFailure])
    {
        return Err("validate observation did not record its own bounds and result".into());
    }

    let first_write = encoded_cells(&observed)?;
    apply_validate_results(
        &mut observed,
        &rows,
        "sha-1",
        "tree-1",
        &depth_fixture,
        true,
        true,
    )
    .map_err(|e| format!("repeated validate-observation fold failed: {e}"))?;
    if encoded_cells(&observed)? != first_write {
        return Err(
            "applying the same validate result twice changed the tracked observations".into(),
        );
    }

    let command_root = repo_root()?;
    let command_before = read_generated_files(&command_root)?;
    let empty_results = tempfile::tempdir()
        .map_err(|e| format!("cannot create empty result directory fixture: {e}"))?;
    let executable = env::current_exe()
        .map_err(|e| format!("cannot resolve scorecard self-test executable: {e}"))?;
    let output = Command::new(&executable)
        .args(["observe-results", "--results"])
        .arg(empty_results.path())
        .current_dir(&command_root)
        .output()
        .map_err(|e| format!("cannot run empty-result command fixture: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success()
        || !stdout.contains("merged 0 pass, 0 located divergence, and 0 unlocated divergence")
        || !stdout.contains("compatibility scorecard: generated files unchanged")
        || read_generated_files(&command_root)? != command_before
    {
        return Err(format!(
            "empty observe-results command did not succeed unchanged: status={} stdout={stdout:?} stderr={:?}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    // Exercise the actual result-ingest commands in a clean clone. Replay
    // compares one recording with its replay and deliberately reports real
    // time; verify compares independent executions and still requires virtual
    // time.
    let result_command_fixture =
        tempfile::tempdir().map_err(|e| format!("cannot create result-command fixture: {e}"))?;
    let result_command_root = result_command_fixture.path().join("repo");
    let clone = Command::new("git")
        .args(["clone", "--quiet", "--shared"])
        .arg(&command_root)
        .arg(&result_command_root)
        .output()
        .map_err(|e| format!("cannot clone result-command fixture: {e}"))?;
    if !clone.status.success() {
        return Err(format!(
            "cannot clone result-command fixture: {}",
            String::from_utf8_lossy(&clone.stderr).trim()
        ));
    }
    let git_ok = |repo: &Path, args: &[&str]| -> Result<(), String> {
        let status = Command::new("git")
            .args(args)
            .current_dir(repo)
            .status()
            .map_err(|e| format!("cannot run git {}: {e}", args.join(" ")))?;
        status
            .success()
            .then_some(())
            .ok_or_else(|| format!("git {} failed", args.join(" ")))
    };
    let local_agent_utils = format!(
        "submodule.agent-utils.url={}",
        command_root.join("agent-utils").display()
    );
    git_ok(
        &result_command_root,
        &[
            "-c",
            "protocol.file.allow=always",
            "-c",
            &local_agent_utils,
            "submodule",
            "update",
            "--init",
            "--recursive",
            "--",
            "agent-utils",
        ],
    )?;
    let cloned_agent_utils = result_command_root.join("agent-utils");
    if !cloned_agent_utils.join("rs/dagrun/Cargo.toml").is_file() {
        return Err(
            "result-command fixture did not materialize agent-utils/rs/dagrun/Cargo.toml".into(),
        );
    }
    let expected_agent_utils = git_rev_parse(&result_command_root, "HEAD:agent-utils")?;
    let actual_agent_utils = git_head(&cloned_agent_utils)?;
    if actual_agent_utils != expected_agent_utils {
        return Err(format!(
            "result-command fixture agent-utils is {actual_agent_utils}, expected gitlink {expected_agent_utils}"
        ));
    }

    // Give the clone an unrelated sibling Reverie history. Its HEAD is not the
    // pin recorded by this Hermit revision, so it must be omitted rather than
    // substituted. Advancing it after the first write must not change metadata
    // derived from the same result row.
    let reverie_root = result_command_fixture.path().join("reverie");
    fs::create_dir(&reverie_root).map_err(|e| e.to_string())?;
    git_ok(&reverie_root, &["init", "--quiet"])?;
    fs::write(reverie_root.join("fixture"), "recorded\n").map_err(|e| e.to_string())?;
    git_ok(&reverie_root, &["add", "fixture"])?;
    let commit = |message: &str| {
        git_ok(
            &reverie_root,
            &[
                "-c",
                "user.email=scorecard@example.invalid",
                "-c",
                "user.name=Scorecard Self-Test",
                "commit",
                "--quiet",
                "-am",
                message,
            ],
        )
    };
    commit("recorded")?;
    let result_command_before = read_generated_files(&result_command_root)?;
    let fixture_head = git_head(&result_command_root)?;
    let fixture_detcore_tree = git_rev_parse(&result_command_root, "HEAD:detcore")?;
    let replay_id = CellId {
        lane: "portable".into(),
        category: "system-utils".into(),
        test: "system-utils/record-getpid".into(),
        mode: "replay".into(),
        backend: "ptrace".into(),
    };
    let verify_id = CellId {
        mode: "verify".into(),
        ..replay_id.clone()
    };
    let result_root = result_command_root.join("results");
    fs::create_dir_all(&result_root)
        .map_err(|e| format!("cannot create result-command result directory: {e}"))?;
    let result_path = result_root.join("results.jsonl");
    let mut replay_row = candidate("PASS").row;
    replay_row.run_id = "result-command-replay".into();
    replay_row.hermit_sha = fixture_head.clone();
    replay_row.test = replay_id.test.clone();
    replay_row.category = replay_id.category.clone();
    replay_row.lane = replay_id.lane.clone();
    replay_row.mode = replay_id.mode.clone();
    replay_row.backend = Some(replay_id.backend.clone());
    replay_row.argv = vec!["hermit".into(), "record".into(), "start".into()];
    replay_row.effective_args = replay_row.argv.iter().skip(1).cloned().collect();
    replay_row.shell_command =
        literal_shell_command(&replay_row.cwd, &replay_row.env, &replay_row.argv);
    replay_row.attempts = vec![validate_attempt("PASS")];
    replay_row.attempts[0]["argv"] = serde_json::to_value(&replay_row.argv).unwrap();
    replay_row.attempts[0]["shell_command"] = JsonValue::String(replay_row.shell_command.clone());
    let mut report: JsonValue = serde_json::from_str(
        replay_row.attempts[0]["verification_report"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    report["comparison"]["virtualize_time"] = serde_json::json!(false);
    let report = serde_json::to_string(&report).unwrap();
    replay_row.attempts[0]["verification_report_sha256"] =
        JsonValue::String(format!("{:x}", Sha256::digest(report.as_bytes())));
    replay_row.attempts[0]["verification_report"] = JsonValue::String(report);
    let write_result_row = |row: &ResultRow| -> Result<(), String> {
        let mut encoded = serde_json::to_vec(row).map_err(|e| e.to_string())?;
        encoded.push(b'\n');
        fs::write(&result_path, encoded).map_err(|e| e.to_string())
    };
    let run_result_command = |command: &str, summary: Option<&Path>| {
        let mut child = Command::new(&executable);
        child
            .args([command, "--results"])
            .arg(&result_root)
            .current_dir(&result_command_root);
        if let Some(summary) = summary {
            child.arg("--current-summary").arg(summary);
        }
        child.output().map_err(|e| e.to_string())
    };
    let has_current_replay = |cells: &TrackedCells| {
        cells.cells.iter().any(|cell| {
            cell.id == replay_id
                && cell.observations.iter().any(|observation| {
                    observation.canonical_comparisons.iter().any(|comparison| {
                        comparison.hermit_sha == fixture_head
                            && comparison.result == ObservedResult::Pass
                    })
                })
        })
    };
    let restore_generated = || -> Result<(), String> {
        fs::write(
            result_command_root.join(SCORECARD),
            &result_command_before.scorecard,
        )
        .and_then(|()| {
            fs::write(
                result_command_root.join(CELLS),
                &result_command_before.cells,
            )
        })
        .map_err(|e| e.to_string())
    };

    write_result_row(&replay_row)?;
    let observe_output = run_result_command("observe-results", None)?;
    if !observe_output.status.success()
        || !has_current_replay(&read_json(&result_command_root.join(CELLS))?)
    {
        return Err(format!(
            "observe-results did not admit canonical replay evidence with real time: {:?}",
            String::from_utf8_lossy(&observe_output.stderr)
        ));
    }
    let first_observe = read_generated_files(&result_command_root)?;
    fs::write(reverie_root.join("fixture"), "advanced\n").map_err(|e| e.to_string())?;
    commit("advance sibling")?;
    let repeated = run_result_command("observe-results", None)?;
    if !repeated.status.success()
        || !String::from_utf8_lossy(&repeated.stdout)
            .contains("compatibility scorecard: generated files unchanged")
        || read_generated_files(&result_command_root)? != first_observe
    {
        return Err(
            "identical observe-results input changed after the sibling Reverie HEAD advanced"
                .into(),
        );
    }
    restore_generated()?;

    // --- combined immutable-snapshot + current-result transaction ---------
    // This is deliberately dormant: the production callers continue to use
    // their existing commands until the parent snapshot provider lands. The
    // self-test exercises the new command's actual transaction boundary.
    let mut combined_baseline_cells: TrackedCells = read_json(&result_command_root.join(CELLS))?;
    for cell in &mut combined_baseline_cells.cells {
        cell.observations.clear();
        cell.last_tested = None;
    }
    combined_baseline_cells.projection = None;
    refresh_measurement(&mut combined_baseline_cells);
    let combined_derived = derive(&result_command_root)?;
    let combined_baseline = generated_files(&combined_derived, &combined_baseline_cells)?;
    fs::write(
        result_command_root.join(SCORECARD),
        &combined_baseline.scorecard,
    )
    .and_then(|()| fs::write(result_command_root.join(CELLS), &combined_baseline.cells))
    .map_err(|error| format!("cannot write combined transaction baseline: {error}"))?;
    let restore_combined_baseline = || -> Result<(), String> {
        fs::write(
            result_command_root.join(SCORECARD),
            &combined_baseline.scorecard,
        )
        .and_then(|()| fs::write(result_command_root.join(CELLS), &combined_baseline.cells))
        .map_err(|error| error.to_string())
    };
    let snapshot_root = result_command_fixture.path().join("scorecard-snapshots");
    fs::create_dir(&snapshot_root)
        .map_err(|error| format!("cannot create combined snapshot fixture: {error}"))?;
    let source_commit = "a".repeat(40);
    let source_tree = "b".repeat(40);
    let empty_snapshot_value = scorecard_snapshot_fixture_value(&source_commit, &source_tree, &[])?;
    let empty_snapshot_path = snapshot_root.join("empty.json");
    let empty_snapshot_sha =
        write_scorecard_snapshot_fixture(&empty_snapshot_path, &empty_snapshot_value)?;
    HeldScorecardSeriesSnapshot::open(&empty_snapshot_path, &empty_snapshot_sha)
        .map_err(|error| format!("valid empty combined snapshot was refused: {error}"))?;

    // Use the real command in a separately bounded child: a blocking FIFO open
    // must fail this control without stranding the self-test's writer lock.
    let run_snapshot_command = |path: &Path, sha: &str| -> Result<std::process::Output, String> {
        let mut child = Command::new(&executable)
            .arg("project-and-observe-results")
            .arg("--snapshot")
            .arg(path)
            .arg("--snapshot-sha256")
            .arg(sha)
            .arg("--results")
            .arg(&result_root)
            .arg("--expected-head")
            .arg(&fixture_head)
            .arg("--refreshed-at")
            .arg("fixture-refresh")
            .current_dir(&result_command_root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("cannot start snapshot command control: {error}"))?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                status => {
                    let killed = child.kill();
                    let reaped = child.wait();
                    return Err(format!(
                        "snapshot command control did not complete: {status:?}; kill={killed:?}; reap={reaped:?}"
                    ));
                }
            }
        }
        child
            .wait_with_output()
            .map_err(|error| format!("cannot read snapshot command control output: {error}"))
    };
    let fifo_snapshot_path = snapshot_root.join("snapshot.fifo");
    let fifo_name = std::ffi::CString::new(fifo_snapshot_path.as_os_str().as_encoded_bytes())
        .map_err(|error| format!("cannot name snapshot FIFO control: {error}"))?;
    // SAFETY: fifo_name is a live NUL-terminated pathname, with no interior NUL.
    if unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o600) } != 0 {
        return Err(format!(
            "cannot create snapshot FIFO control: {}",
            std::io::Error::last_os_error()
        ));
    }
    let fifo_output = run_snapshot_command(&fifo_snapshot_path, &empty_snapshot_sha)?;
    if fifo_output.status.code() != Some(2)
        || !String::from_utf8_lossy(&fifo_output.stderr).contains("not a regular file")
        || read_generated_files(&result_command_root)? != combined_baseline
    {
        return Err(format!(
            "snapshot FIFO was not refused without changing the generated pair: status={} stderr={:?}",
            fifo_output.status,
            String::from_utf8_lossy(&fifo_output.stderr)
        ));
    }
    // This succeeds only if the refused command released the actual writer
    // lock and an empty relative parent is resolved as the current directory.
    let bare_snapshot_path = Path::new("snapshot.json");
    let bare_snapshot_sha = write_scorecard_snapshot_fixture(
        &result_command_root.join(bare_snapshot_path),
        &empty_snapshot_value,
    )?;
    let bare_output = run_snapshot_command(bare_snapshot_path, &bare_snapshot_sha)?;
    let bare_written: TrackedCells = read_json(&result_command_root.join(CELLS))?;
    if !bare_output.status.success()
        || !has_current_replay(&bare_written)
        || bare_written
            .projection
            .as_ref()
            .is_none_or(|projection| projection.rows_read != 0 || !projection.pre_series_corpus)
    {
        return Err(format!(
            "bare snapshot filename did not produce the expected direct evidence: status={} stderr={:?}",
            bare_output.status,
            String::from_utf8_lossy(&bare_output.stderr)
        ));
    }
    restore_combined_baseline()?;

    let held_parent_path = snapshot_root.join("held-parent");
    fs::create_dir(&held_parent_path).map_err(|error| error.to_string())?;
    let held_parent_snapshot = held_parent_path.join("snapshot.json");
    let held_parent_sha =
        write_scorecard_snapshot_fixture(&held_parent_snapshot, &empty_snapshot_value)?;
    let held_parent = HeldScorecardSeriesSnapshot::open(&held_parent_snapshot, &held_parent_sha)?;
    fs::rename(&held_parent_path, snapshot_root.join("original-parent"))
        .map_err(|error| error.to_string())?;
    fs::create_dir(&held_parent_path).map_err(|error| error.to_string())?;
    write_scorecard_snapshot_fixture(&held_parent_snapshot, &empty_snapshot_value)?;
    let parent_error = held_parent
        .verify()
        .expect_err("a replacement snapshot parent with identical bytes was accepted");
    if !parent_error.contains("parent changed while held") {
        return Err(format!(
            "snapshot parent refusal lost its cause: {parent_error}"
        ));
    }

    // Frozen output from Python json.dumps(sort_keys=True, separators=(",", ":"),
    // ensure_ascii=False). RawValue is load-bearing here: Rust reserialization
    // is not the contract for exponent-form floats or non-ASCII strings.
    let python_canonical_row = r#"{"emitted_at":"2026-09-13T18:31:00Z","event_id":"fixture-python-canonical","event_type":"series.observation","host":"höst","producer":"validate","run_id":"fixture-python","schema":"stress-series/v3","series":{"attempt":1,"cell":"system-utils/record-getpid/replay/ptrace","depth":{"hermit":{"commits":1,"first_parent":1}},"detcore_tree":"dddddddddddddddddddddddddddddddddddddddd","failure_class":null,"host_capabilities":{"cpuid-faulting":{"evidence":"測定","present":true},"kvm":{"evidence":"none","present":false}},"kernel_version":"κernel","machine_shortname":"höst","main_ancestry":true,"num_runs":1,"outcome":"passed","result":"pass","run_index":1,"runtime":{"run1":null,"run2":null,"wall_time_max_ms":1e+20,"wall_time_min_ms":1e-07},"source_tree_dirty":false,"tree":"cccccccccccccccccccccccccccccccccccccccc"},"team":"hermit"}"#;
    let python_rows_sha = "5c1fbe913a2bebe8a98ebe07e7a1ac3c4d17a6c5da239b237a083a7f77cdcd94";
    let mut python_rows_bytes = python_canonical_row.as_bytes().to_vec();
    python_rows_bytes.push(b'\n');
    if format!("{:x}", Sha256::digest(&python_rows_bytes)) != python_rows_sha {
        return Err("frozen Python canonical-row fixture digest changed".into());
    }
    let python_snapshot_payload = format!(
        "{{\"schema\":\"{SCORECARD_SERIES_SNAPSHOT_SCHEMA}\",\"source\":{{\"path\":\"series\",\"commit\":\"{source_commit}\",\"tree\":\"{source_tree}\",\"published_rows\":1,\"local_sources\":[]}},\"rows_read\":1,\"rows_sha256\":\"{python_rows_sha}\",\"rows\":[{python_canonical_row}]}}\n"
    );
    let python_snapshot_path = snapshot_root.join("python-canonical.json");
    fs::write(&python_snapshot_path, python_snapshot_payload.as_bytes())
        .map_err(|error| format!("cannot write Python canonical snapshot fixture: {error}"))?;
    let python_snapshot_sha = format!("{:x}", Sha256::digest(python_snapshot_payload.as_bytes()));
    HeldScorecardSeriesSnapshot::open(&python_snapshot_path, &python_snapshot_sha).map_err(
        |error| {
            format!(
                "Python canonical snapshot with exponent floats and Unicode was refused: {error}"
            )
        },
    )?;
    let wrong_digest = "0".repeat(64);
    let digest_error = HeldScorecardSeriesSnapshot::open(&empty_snapshot_path, &wrong_digest)
        .err()
        .ok_or("combined snapshot accepted a wrong outer digest")?;
    if !digest_error.contains("digest mismatch") {
        return Err(format!(
            "combined snapshot digest refusal lost its cause: {digest_error}"
        ));
    }
    let snapshot_link = snapshot_root.join("snapshot-link.json");
    std::os::unix::fs::symlink(&empty_snapshot_path, &snapshot_link)
        .map_err(|error| format!("cannot create combined snapshot symlink fixture: {error}"))?;
    if HeldScorecardSeriesSnapshot::open(&snapshot_link, &empty_snapshot_sha).is_ok() {
        return Err("combined snapshot reader followed a symlink".into());
    }

    let series_row = SeriesRow {
        source: String::new(),
        schema: SeriesSchema::V3,
        event_id: "fixture-combined-current-result".into(),
        event_type: "series.observation".into(),
        emitted_at: "2026-09-13T18:30:00Z".into(),
        team: "hermit".into(),
        host: "fixture-host".into(),
        producer: SeriesProducer::Validate,
        run_id: replay_row.run_id.clone(),
        series: SeriesPayload {
            cell: series_cell_key(&replay_id),
            tree: fixture_head.clone(),
            detcore_tree: Some(fixture_detcore_tree.clone()),
            outcome: SeriesOutcome::Passed,
            result: Some(ObservedResult::Pass),
            failure_class: None,
            no_verdict_evidence: None,
            pressure_evidence: None,
            run_index: 1,
            attempt: Some(1),
            num_runs: 1,
            last_run_index: None,
            main_ancestry: Some(true),
            runtime: None,
            source_tree_dirty: false,
            depth: source_depths(&result_command_root, &fixture_head)?,
            coordinates: None,
            first_divergent_messages: None,
            machine_shortname: Some("fixture-host".into()),
            kernel_version: Some("fixture-kernel".into()),
            host_capabilities: Some(BTreeMap::from([
                (
                    HostCapability::CpuidFaulting,
                    HostCapabilityVerdict {
                        present: true,
                        evidence: "fixture cpuid probe".into(),
                    },
                ),
                (
                    HostCapability::Kvm,
                    HostCapabilityVerdict {
                        present: false,
                        evidence: "fixture kvm probe".into(),
                    },
                ),
            ])),
        },
    };
    series_row.validate_for_read()?;
    let row_snapshot_value = scorecard_snapshot_fixture_value(
        &source_commit,
        &source_tree,
        std::slice::from_ref(&series_row),
    )?;
    let row_snapshot_path = snapshot_root.join("with-current-row.json");
    let row_snapshot_sha =
        write_scorecard_snapshot_fixture(&row_snapshot_path, &row_snapshot_value)?;
    HeldScorecardSeriesSnapshot::open(&row_snapshot_path, &row_snapshot_sha)
        .map_err(|error| format!("valid row-bearing combined snapshot was refused: {error}"))?;

    // Both digests must match the ambiguous original bytes, so rejection tests
    // duplicate-field validation rather than an unrelated checksum mismatch.
    let canonical_row =
        serde_json::to_string(&row_snapshot_value["rows"][0]).map_err(|error| error.to_string())?;
    let run_field = format!(
        "\"run_id\":{}",
        serde_json::to_string(&series_row.run_id).map_err(|error| error.to_string())?
    );
    for (label, field, different) in [
        ("row", run_field.as_str(), "\"run_id\":\"conflicting-run\""),
        ("series", "\"attempt\":1", "\"attempt\":2"),
        ("nested", "\"present\":true", "\"present\":false"),
    ] {
        if canonical_row.matches(field).count() != 1 {
            return Err(format!(
                "duplicate-field {label} fixture has no unique source field"
            ));
        }
        for (kind, first) in [("equal", field), ("conflicting", different)] {
            let raw_row = canonical_row.replacen(field, &format!("{first},{field}"), 1);
            let rows_sha = format!("{:x}", Sha256::digest(format!("{raw_row}\n").as_bytes()));
            let source = serde_json::to_string(&row_snapshot_value["source"])
                .map_err(|error| error.to_string())?;
            let payload = format!(
                "{{\"schema\":\"{SCORECARD_SERIES_SNAPSHOT_SCHEMA}\",\"source\":{source},\"rows_read\":1,\"rows_sha256\":\"{rows_sha}\",\"rows\":[{raw_row}]}}\n"
            );
            let path = snapshot_root.join(format!("duplicate-{label}-{kind}.json"));
            fs::write(&path, payload.as_bytes()).map_err(|error| error.to_string())?;
            let sha = format!("{:x}", Sha256::digest(payload.as_bytes()));
            let error = HeldScorecardSeriesSnapshot::open(&path, &sha)
                .err()
                .ok_or_else(|| format!("snapshot accepted {kind} duplicate {label} fields"))?;
            if !error.contains("duplicate JSON field") {
                return Err(format!(
                    "duplicate {label} {kind} refusal lost its cause: {error}"
                ));
            }
        }
    }

    let mut wrong_count = row_snapshot_value.clone();
    wrong_count["rows_read"] = serde_json::json!(2);
    let wrong_count_path = snapshot_root.join("wrong-count.json");
    let wrong_count_sha = write_scorecard_snapshot_fixture(&wrong_count_path, &wrong_count)?;
    let wrong_count_error = HeldScorecardSeriesSnapshot::open(&wrong_count_path, &wrong_count_sha)
        .err()
        .ok_or("combined snapshot accepted a false row count")?;
    if !wrong_count_error.contains("rows_read") {
        return Err(format!(
            "combined snapshot row-count refusal lost its cause: {wrong_count_error}"
        ));
    }
    let mut wrong_row_digest = row_snapshot_value.clone();
    wrong_row_digest["rows_sha256"] = serde_json::json!("0".repeat(64));
    let wrong_row_digest_path = snapshot_root.join("wrong-row-digest.json");
    let wrong_row_digest_sha =
        write_scorecard_snapshot_fixture(&wrong_row_digest_path, &wrong_row_digest)?;
    let wrong_row_digest_error =
        HeldScorecardSeriesSnapshot::open(&wrong_row_digest_path, &wrong_row_digest_sha)
            .err()
            .ok_or("combined snapshot accepted a false canonical-row digest")?;
    if !wrong_row_digest_error.contains("row digest") {
        return Err(format!(
            "combined snapshot row-digest refusal lost its cause: {wrong_row_digest_error}"
        ));
    }
    let mut earlier_row = series_row.clone();
    earlier_row.event_id = "fixture-combined-earlier".into();
    earlier_row.emitted_at = "2026-09-13T18:29:59Z".into();
    let noncanonical_value = scorecard_snapshot_fixture_value(
        &source_commit,
        &source_tree,
        &[series_row.clone(), earlier_row],
    )?;
    let noncanonical_path = snapshot_root.join("noncanonical-order.json");
    let noncanonical_sha =
        write_scorecard_snapshot_fixture(&noncanonical_path, &noncanonical_value)?;
    let noncanonical_error =
        HeldScorecardSeriesSnapshot::open(&noncanonical_path, &noncanonical_sha)
            .err()
            .ok_or("combined snapshot accepted noncanonical row order")?;
    if !noncanonical_error.contains("canonical emitted_at/event_id order") {
        return Err(format!(
            "combined snapshot ordering refusal lost its cause: {noncanonical_error}"
        ));
    }

    project_and_observe_results(
        &result_command_root,
        &empty_snapshot_path,
        &empty_snapshot_sha,
        &result_root,
        &fixture_head,
        "fixture-refresh",
    )?;
    let before_publication: TrackedCells = read_json(&result_command_root.join(CELLS))?;
    let count_run_representations =
        |cells: &TrackedCells, id: &CellId, run_id: &str, event_id: &str| {
            cells
                .cells
                .iter()
                .filter(|cell| &cell.id == id)
                .flat_map(|cell| &cell.observations)
                .map(|observation| {
                    usize::from(
                        observation
                            .canonical_comparisons
                            .iter()
                            .any(|comparison| comparison.run_id == run_id)
                            || observation
                                .invocations
                                .iter()
                                .any(|invocation| invocation.run_id == run_id),
                    ) + usize::from(observation.event_ids.contains(event_id))
                })
                .sum::<usize>()
        };
    let count_current_result = |cells: &TrackedCells| {
        count_run_representations(
            cells,
            &replay_id,
            &replay_row.run_id,
            "fixture-combined-current-result",
        )
    };
    if count_current_result(&before_publication) != 1
        || before_publication
            .projection
            .as_ref()
            .is_none_or(|projection| projection.rows_read != 0 || !projection.pre_series_corpus)
    {
        return Err(
            "combined transaction did not retain the exact current result once before series publication"
                .into(),
        );
    }

    project_and_observe_results(
        &result_command_root,
        &row_snapshot_path,
        &row_snapshot_sha,
        &result_root,
        &fixture_head,
        "fixture-refresh",
    )?;
    let after_publication: TrackedCells = read_json(&result_command_root.join(CELLS))?;
    if count_current_result(&after_publication) != 1
        || after_publication
            .projection
            .as_ref()
            .is_none_or(|projection| {
                projection.rows_read != 1
                    || projection.source_commit.as_deref() != Some(source_commit.as_str())
                    || projection.source_tree.as_deref() != Some(source_tree.as_str())
            })
    {
        return Err(
            "combined transaction duplicated or lost the current result after series publication"
                .into(),
        );
    }
    let stable_publication = read_generated_files(&result_command_root)?;
    project_and_observe_results(
        &result_command_root,
        &row_snapshot_path,
        &row_snapshot_sha,
        &result_root,
        &fixture_head,
        "fixture-refresh",
    )?;
    if read_generated_files(&result_command_root)? != stable_publication {
        return Err("repeating the same combined transaction changed its generated pair".into());
    }

    // The event may already be in the parent snapshot on the first local
    // write. It must still collapse to the richer direct invocation exactly
    // once, rather than leaving one projected and one direct observation.
    restore_combined_baseline()?;
    project_and_observe_results(
        &result_command_root,
        &row_snapshot_path,
        &row_snapshot_sha,
        &result_root,
        &fixture_head,
        "fixture-refresh",
    )?;
    let first_write_with_published_row: TrackedCells = read_json(&result_command_root.join(CELLS))?;
    if count_current_result(&first_write_with_published_row) != 1
        || first_write_with_published_row
            .projection
            .as_ref()
            .is_none_or(|projection| projection.pre_series_corpus)
    {
        return Err(
            "combined first write duplicated a current result already present in the snapshot"
                .into(),
        );
    }
    let stable_publication = read_generated_files(&result_command_root)?;

    // Exercise the same exact reconciliation for every current result class.
    // A canonical divergence is keyed by its product result. ERROR and
    // no_result are keyed by the exact outer-attempt evidence digest, because
    // neither establishes a canonical product comparison.
    let bind_attempt_to_row = |attempt: &mut JsonValue, row: &ResultRow| {
        attempt["argv"] = serde_json::to_value(&row.argv).unwrap();
        attempt["guest_argv"] = serde_json::to_value(&row.guest_argv).unwrap();
        attempt["env"] = serde_json::to_value(&row.env).unwrap();
        attempt["cwd"] = JsonValue::String(row.cwd.clone());
        attempt["shell_command"] = JsonValue::String(row.shell_command.clone());
    };
    let replace_attempt_report = |attempt: &mut JsonValue, report: JsonValue| {
        let report = serde_json::to_string(&report).unwrap();
        attempt["verification_report_sha256"] =
            JsonValue::String(format!("{:x}", Sha256::digest(report.as_bytes())));
        attempt["verification_report"] = JsonValue::String(report);
    };

    let mut fail_row = replay_row.clone();
    fail_row.run_id = "result-command-combined-fail".into();
    fail_row.outcome = "FAIL".into();
    fail_row.result = Some(ObservedResult::ReplayFailure);
    fail_row.failure_class = Some(FailureClass::ProductFailure);
    fail_row.error_kind = None;
    fail_row.first_divergent_scheduler_turn = Some(7);
    fail_row.first_divergent_virtual_nanoseconds = Some(70);
    fail_row.first_divergent_record = Some(12);
    fail_row.first_divergent_syscall = Some(9);
    fail_row.attempts = vec![validate_attempt("FAIL")];
    let mut fail_report: JsonValue = serde_json::from_str(
        fail_row.attempts[0]["verification_report"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    fail_report["comparison"]["virtualize_time"] = JsonValue::Bool(false);
    replace_attempt_report(&mut fail_row.attempts[0], fail_report);
    let fail_binding = fail_row.clone();
    bind_attempt_to_row(&mut fail_row.attempts[0], &fail_binding);
    let mut fail_series = series_row.clone();
    fail_series.event_id = "fixture-combined-current-fail".into();
    fail_series.run_id = fail_row.run_id.clone();
    fail_series.series.outcome = SeriesOutcome::Diverged;
    fail_series.series.result = Some(ObservedResult::ReplayFailure);
    fail_series.series.failure_class = Some(FailureClass::ProductFailure);
    fail_series.series.coordinates = Some(SeriesCoordinates {
        first_divergent_scheduler_turn: Some(7),
        first_divergent_virtual_nanoseconds: Some(70),
        first_divergent_record: Some(12),
        first_divergent_syscall: Some(9),
    });
    fail_series.validate_for_read()?;

    let mut error_row = replay_row.clone();
    error_row.run_id = "result-command-combined-error".into();
    error_row.outcome = "ERROR".into();
    error_row.result = None;
    error_row.failure_class = Some(FailureClass::UnderstoodInfrastructureFailure);
    error_row.error_kind = Some("infrastructure".into());
    error_row.first_divergent_scheduler_turn = None;
    error_row.first_divergent_virtual_nanoseconds = None;
    error_row.first_divergent_record = None;
    error_row.first_divergent_syscall = None;
    error_row.attempts = vec![validate_attempt("ERROR")];
    error_row.attempts[0]["timed_out"] = JsonValue::Bool(false);
    error_row.attempts[0]["status"] = serde_json::json!(1);
    error_row.attempts[0]["signal"] = JsonValue::Null;
    error_row.attempts[0]["error_kind"] = JsonValue::String("infrastructure".into());
    let mut infrastructure_report = canonical_verdict::VerificationReport::no_result();
    infrastructure_report.verdict = canonical_verdict::Verdict::InfrastructureError;
    infrastructure_report.no_result_reason = None;
    infrastructure_report.infrastructure_error =
        Some(canonical_verdict::InfrastructureError::SkidOvershoot { count: 1 });
    replace_attempt_report(
        &mut error_row.attempts[0],
        serde_json::to_value(&infrastructure_report).unwrap(),
    );
    let error_binding = error_row.clone();
    bind_attempt_to_row(&mut error_row.attempts[0], &error_binding);
    let error_evidence_identity = error_row.evidence_identity()?;
    let error_report_identity = error_row.attempts[0]["verification_report_sha256"]
        .as_str()
        .unwrap()
        .to_string();
    let mut error_series = series_row.clone();
    error_series.event_id = "fixture-combined-current-error".into();
    error_series.run_id = error_row.run_id.clone();
    error_series.series.outcome = SeriesOutcome::Errored;
    error_series.series.result = None;
    error_series.series.failure_class = Some(FailureClass::UnderstoodInfrastructureFailure);
    error_series.series.coordinates = None;
    error_series.series.no_verdict_evidence = Some(SeriesNoVerdictEvidence {
        evidence_sha256: error_evidence_identity,
        attempts: vec![SeriesAttemptDisposition {
            index: "1".into(),
            kind: SeriesNoVerdictKind::InfrastructureError,
            detail: None,
            attempt_outcome: "ERROR".into(),
            disposition: SeriesOutcome::Errored,
            error_kind: Some("infrastructure".into()),
            status: Some(1),
            signal: None,
            timed_out: false,
            verification_report_sha256: Some(error_report_identity),
        }],
    });
    error_series.validate_for_read()?;

    let mut no_result_row = replay_row.clone();
    no_result_row.run_id = "result-command-combined-no-result".into();
    no_result_row.outcome = "ERROR".into();
    no_result_row.result = None;
    no_result_row.failure_class = Some(FailureClass::NoResult);
    no_result_row.error_kind = Some("incomplete-verification-evidence".into());
    no_result_row.first_divergent_scheduler_turn = None;
    no_result_row.first_divergent_virtual_nanoseconds = None;
    no_result_row.first_divergent_record = None;
    no_result_row.first_divergent_syscall = None;
    no_result_row.attempts = vec![validate_attempt("ERROR")];
    no_result_row.attempts[0]["timed_out"] = JsonValue::Bool(false);
    no_result_row.attempts[0]["status"] = serde_json::json!(125);
    no_result_row.attempts[0]["signal"] = JsonValue::Null;
    let no_result_binding = no_result_row.clone();
    bind_attempt_to_row(&mut no_result_row.attempts[0], &no_result_binding);
    let no_result_evidence_identity = no_result_row.evidence_identity()?;
    let no_result_report_identity = no_result_row.attempts[0]["verification_report_sha256"]
        .as_str()
        .unwrap()
        .to_string();
    let mut no_result_series = series_row.clone();
    no_result_series.event_id = "fixture-combined-current-no-result".into();
    no_result_series.run_id = no_result_row.run_id.clone();
    no_result_series.series.outcome = SeriesOutcome::NoResult;
    no_result_series.series.result = None;
    no_result_series.series.failure_class = Some(FailureClass::NoResult);
    no_result_series.series.coordinates = None;
    no_result_series.series.no_verdict_evidence = Some(SeriesNoVerdictEvidence {
        evidence_sha256: no_result_evidence_identity,
        attempts: vec![SeriesAttemptDisposition {
            index: "1".into(),
            kind: SeriesNoVerdictKind::NotRun,
            detail: None,
            attempt_outcome: "ERROR".into(),
            disposition: SeriesOutcome::NoResult,
            error_kind: Some("incomplete-verification-evidence".into()),
            status: Some(125),
            signal: None,
            timed_out: false,
            verification_report_sha256: Some(no_result_report_identity),
        }],
    });
    no_result_series.validate_for_read()?;

    let no_result_claim = |run_id: &str, attempt: u64, event_id: &str| {
        let mut row = no_result_row.clone();
        row.run_id = run_id.to_string();
        row.attempt = attempt;
        let mut source = no_result_series.clone();
        source.run_id = row.run_id.clone();
        source.event_id = event_id.to_string();
        source.series.attempt = Some(attempt);
        source.series.run_index = attempt;
        source
            .series
            .no_verdict_evidence
            .as_mut()
            .unwrap()
            .evidence_sha256 = row.evidence_identity()?;
        source.validate_for_read()?;
        Ok::<_, String>((row, source))
    };
    let reconcile_control = |label: &str,
                             current: &ResultRow,
                             mut rows: Vec<SeriesRow>,
                             seed: Option<&TrackedCells>,
                             refuse: bool|
     -> Result<TrackedCells, String> {
        restore_combined_baseline()?;
        if let Some(seed) = seed {
            let pair = generated_files(&combined_derived, seed)?;
            fs::write(result_command_root.join(SCORECARD), pair.scorecard)
                .and_then(|()| fs::write(result_command_root.join(CELLS), pair.cells))
                .map_err(|error| error.to_string())?;
        }
        write_result_row(current)?;
        rows.sort_by(|a, b| {
            a.emitted_at
                .cmp(&b.emitted_at)
                .then(a.event_id.cmp(&b.event_id))
        });
        let value = scorecard_snapshot_fixture_value(&source_commit, &source_tree, &rows)?;
        let path = snapshot_root.join(format!("reconcile-{label}.json"));
        let sha = write_scorecard_snapshot_fixture(&path, &value)?;
        let before = read_generated_files(&result_command_root)?;
        let run = || {
            project_and_observe_results(
                &result_command_root,
                &path,
                &sha,
                &result_root,
                &fixture_head,
                "fixture-refresh",
            )
        };
        if refuse {
            let error = run().expect_err("contradictory invocation evidence was accepted");
            if !error.contains("disagree") || read_generated_files(&result_command_root)? != before
            {
                return Err(format!(
                    "{label} refusal changed the pair or lost its cause: {error}"
                ));
            }
            println!("scorecard self-test: reconciliation {label} refused unchanged");
        } else {
            run()?;
            let first = read_generated_files(&result_command_root)?;
            run()?;
            if read_generated_files(&result_command_root)? != first {
                return Err(format!(
                    "repeating reconciliation {label} changed the generated pair"
                ));
            }
            println!("scorecard self-test: reconciliation {label} passed twice identically");
        }
        read_json(&result_command_root.join(CELLS))
    };
    let (same_attempt_no_result, same_attempt_claim) =
        no_result_claim(&replay_row.run_id, 1, "fixture-conflicting-no-result")?;
    let contradictory_rows = vec![series_row.clone(), same_attempt_claim];
    reconcile_control(
        "pass-and-no-result",
        &replay_row,
        contradictory_rows.clone(),
        None,
        true,
    )?;
    reconcile_control(
        "no-result-and-pass",
        &same_attempt_no_result,
        contradictory_rows,
        None,
        true,
    )?;
    let mut conflicting_pass = fail_series.clone();
    conflicting_pass.run_id = replay_row.run_id.clone();
    reconcile_control(
        "pass-and-failure",
        &replay_row,
        vec![series_row.clone(), conflicting_pass],
        None,
        true,
    )?;

    let (different_attempt_no_result, different_attempt_claim) =
        no_result_claim(&replay_row.run_id, 2, "fixture-distinct-attempt-no-result")?;
    let different_rows = vec![series_row.clone(), different_attempt_claim.clone()];
    let different_written = reconcile_control(
        "different-attempt",
        &replay_row,
        different_rows.clone(),
        None,
        false,
    )?;
    let require_retained_no_result =
        |written: &TrackedCells, event_id: &str| -> Result<(), String> {
            let observations = written
                .cells
                .iter()
                .flat_map(|cell| &cell.observations)
                .filter(|observation| observation.event_ids.contains(event_id))
                .collect::<Vec<_>>();
            if count_current_result(written) != 1
                || observations.len() != 1
                || !observations[0].results.is_empty()
                || !observations[0].canonical_comparisons.is_empty()
                || !observations[0].invocations.is_empty()
                || written.projection.as_ref().is_none_or(|projection| {
                    projection.rows_read != 2 || projection.pre_series_corpus
                })
            {
                return Err(format!(
                    "distinct no-result event {event_id} or direct PASS was lost or duplicated"
                ));
            }
            Ok(())
        };
    require_retained_no_result(&different_written, &different_attempt_claim.event_id)?;
    let unrelated_written = reconcile_control(
        "different-run",
        &replay_row,
        vec![series_row.clone(), no_result_series.clone()],
        None,
        false,
    )?;
    require_retained_no_result(&unrelated_written, &no_result_series.event_id)?;

    let mut projected_seed = combined_baseline_cells.clone();
    apply_series_rows(
        &result_command_root,
        &mut projected_seed,
        std::slice::from_ref(&series_row),
        Some("series"),
    )?;
    projected_seed.projection = Some(ObservationProjection {
        source: "series".into(),
        source_commit: Some(source_commit.clone()),
        source_tree: Some(source_tree.clone()),
        refreshed_at: "fixture-before-current".into(),
        rows_read: 1,
        pre_series_corpus: false,
    });
    refresh_measurement(&mut projected_seed);
    let replaced_projection = reconcile_control(
        "previously-projected",
        &replay_row,
        different_rows.clone(),
        Some(&projected_seed),
        false,
    )?;
    require_retained_no_result(&replaced_projection, &different_attempt_claim.event_id)?;
    let unbound_error = direct_representation(
        &before_publication,
        &different_rows,
        &CurrentResultAttempts::new(),
    )
    .expect_err("a compact historical PASS was assumed to prove outer attempt one");
    if !unbound_error.contains("disagree") {
        return Err(format!(
            "unbound outer-attempt refusal lost its cause: {unbound_error}"
        ));
    }
    let mut second_attempt_pass = replay_row.clone();
    second_attempt_pass.attempt = 2;
    reconcile_control(
        "wrong-explicit-attempt",
        &second_attempt_pass,
        vec![series_row.clone()],
        None,
        true,
    )?;

    // The real result directory may retain more than one current outer
    // attempt. Each exact event must reconcile with its own direct record,
    // while conflicting current records for one attempt still refuse.
    for (label, additional, source_rows, refuse) in [
        (
            "multiple-current-attempts",
            &different_attempt_no_result,
            different_rows.clone(),
            false,
        ),
        (
            "conflicting-current-attempts",
            &same_attempt_no_result,
            vec![
                series_row.clone(),
                no_result_claim(&replay_row.run_id, 1, "fixture-current-conflict")?.1,
            ],
            true,
        ),
    ] {
        restore_combined_baseline()?;
        let mut encoded = Vec::new();
        for row in [&replay_row, additional] {
            encoded.extend(serde_json::to_vec(row).map_err(|error| error.to_string())?);
            encoded.push(b'\n');
        }
        fs::write(&result_path, encoded).map_err(|error| error.to_string())?;
        let mut source_rows = source_rows;
        source_rows.sort_by(|a, b| {
            a.emitted_at
                .cmp(&b.emitted_at)
                .then(a.event_id.cmp(&b.event_id))
        });
        let value = scorecard_snapshot_fixture_value(&source_commit, &source_tree, &source_rows)?;
        let path = snapshot_root.join(format!("reconcile-{label}.json"));
        let sha = write_scorecard_snapshot_fixture(&path, &value)?;
        let before = read_generated_files(&result_command_root)?;
        let run = || {
            project_and_observe_results(
                &result_command_root,
                &path,
                &sha,
                &result_root,
                &fixture_head,
                "fixture-refresh",
            )
        };
        if refuse {
            let error =
                run().expect_err("contradictory current outer-attempt records were accepted");
            if !error.contains("conflicting evidence for outer attempt 1")
                || read_generated_files(&result_command_root)? != before
            {
                return Err(format!(
                    "{label} refusal changed the pair or lost its cause: {error}"
                ));
            }
            println!("scorecard self-test: reconciliation {label} refused unchanged");
            continue;
        }
        run()?;
        let first = read_generated_files(&result_command_root)?;
        run()?;
        if read_generated_files(&result_command_root)? != first {
            return Err(
                "repeating the multiple-current-attempt write changed its generated pair".into(),
            );
        }
        let written: TrackedCells = read_json(&result_command_root.join(CELLS))?;
        let observations = &written
            .cells
            .iter()
            .find(|cell| cell.id == replay_id)
            .ok_or("multiple-current-attempt write lost its cell")?
            .observations;
        let invocations = observations
            .iter()
            .flat_map(|observation| &observation.invocations)
            .filter(|invocation| invocation.run_id == replay_row.run_id)
            .collect::<Vec<_>>();
        let no_result_digest = different_attempt_no_result.evidence_identity()?;
        if invocations.len() != 2
            || invocations
                .iter()
                .filter(|invocation| invocation.result == Some(ObservedResult::Pass))
                .count()
                != 1
            || invocations
                .iter()
                .filter(|invocation| {
                    invocation.result.is_none()
                        && invocation.attempt == Some(2)
                        && invocation.evidence_sha256.as_deref() == Some(no_result_digest.as_str())
                })
                .count()
                != 1
            || observations
                .iter()
                .map(|observation| observation.canonical_comparisons.len())
                .sum::<usize>()
                != 1
            || observations
                .iter()
                .any(|observation| !observation.event_ids.is_empty())
            || written
                .projection
                .as_ref()
                .is_none_or(|projection| projection.rows_read != 2 || projection.pre_series_corpus)
        {
            return Err(
                "multiple current attempts were lost, duplicated or assigned an invented result"
                    .into(),
            );
        }
        println!("scorecard self-test: reconciliation {label} passed twice identically");
    }

    for (label, result_row, source_row) in [
        ("FAIL", &fail_row, &fail_series),
        ("ERROR", &error_row, &error_series),
        ("no_result", &no_result_row, &no_result_series),
    ] {
        restore_combined_baseline()?;
        write_result_row(result_row)?;
        let value = scorecard_snapshot_fixture_value(
            &source_commit,
            &source_tree,
            std::slice::from_ref(source_row),
        )?;
        let path = snapshot_root.join(format!("current-{}.json", label.to_ascii_lowercase()));
        let sha = write_scorecard_snapshot_fixture(&path, &value)?;
        project_and_observe_results(
            &result_command_root,
            &path,
            &sha,
            &result_root,
            &fixture_head,
            "fixture-refresh",
        )?;
        let written: TrackedCells = read_json(&result_command_root.join(CELLS))?;
        if count_run_representations(
            &written,
            &replay_id,
            &result_row.run_id,
            &source_row.event_id,
        ) != 1
            || written
                .projection
                .as_ref()
                .is_none_or(|projection| projection.pre_series_corpus)
        {
            return Err(format!(
                "combined transaction did not reconcile {label} direct and series evidence exactly once"
            ));
        }
    }
    restore_combined_baseline()?;
    write_result_row(&replay_row)?;

    // A second real command must wait on the same repository-local lock. It
    // may proceed only after the first writer releases that lock.
    let held_writer = acquire_scorecard_write_lock(&result_command_root)?;
    let mut waiting_writer = Command::new(&executable)
        .arg("project-and-observe-results")
        .arg("--snapshot")
        .arg(&row_snapshot_path)
        .arg("--snapshot-sha256")
        .arg(&row_snapshot_sha)
        .arg("--results")
        .arg(&result_root)
        .arg("--expected-head")
        .arg(&fixture_head)
        .arg("--refreshed-at")
        .arg("fixture-refresh")
        .current_dir(&result_command_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot start second combined writer: {error}"))?;
    std::thread::sleep(Duration::from_millis(150));
    if let Some(status) = waiting_writer
        .try_wait()
        .map_err(|error| format!("cannot inspect waiting combined writer: {error}"))?
    {
        return Err(format!(
            "second combined writer did not serialize on the held lock: {status}"
        ));
    }
    drop(held_writer);
    let waiting_output = waiting_writer
        .wait_with_output()
        .map_err(|error| format!("cannot finish second combined writer: {error}"))?;
    if !waiting_output.status.success()
        || read_generated_files(&result_command_root)? != stable_publication
    {
        return Err(format!(
            "serialized second combined writer failed or changed an idempotent pair: status={} stderr={:?}",
            waiting_output.status,
            String::from_utf8_lossy(&waiting_output.stderr)
        ));
    }

    restore_combined_baseline()?;
    let original_combined_pair = read_generated_files(&result_command_root)?;

    // A valid outer digest must not turn malformed snapshot metadata or rows
    // into admissible input. Row mutations also receive a freshly computed
    // canonical-row digest and matching counts, so each refusal exercises the
    // strict field/event contract rather than stopping at an earlier checksum.
    let recompute_snapshot_rows = |snapshot: &mut JsonValue| -> Result<(), String> {
        let (rows_read, rows_sha256) = {
            let rows = snapshot
                .get("rows")
                .and_then(JsonValue::as_array)
                .ok_or("mutated scorecard snapshot lost its rows array")?;
            (
                rows.len(),
                format!("{:x}", Sha256::digest(canonical_snapshot_rows_bytes(rows)?)),
            )
        };
        snapshot["rows_read"] = serde_json::json!(rows_read);
        snapshot["source"]["published_rows"] = serde_json::json!(rows_read);
        snapshot["rows_sha256"] = JsonValue::String(rows_sha256);
        Ok(())
    };
    let assert_snapshot_refused_unchanged =
        |label: &str, snapshot: &JsonValue, expected_cause: &str| -> Result<(), String> {
            let path = snapshot_root.join(format!("refuse-{label}.json"));
            let sha256 = write_scorecard_snapshot_fixture(&path, snapshot)?;
            let error = project_and_observe_results(
                &result_command_root,
                &path,
                &sha256,
                &result_root,
                &fixture_head,
                "fixture-refresh",
            )
            .err()
            .ok_or_else(|| format!("combined snapshot accepted {label}"))?;
            if !error.contains(expected_cause) {
                return Err(format!(
                    "combined snapshot {label} refusal lost its cause: {error}"
                ));
            }
            if read_generated_files(&result_command_root)? != original_combined_pair {
                return Err(format!(
                    "combined snapshot {label} refusal changed the generated pair"
                ));
            }
            Ok(())
        };

    for (label, field, value) in [
        ("wrong-source-path", "path", serde_json::json!("not-series")),
        (
            "wrong-source-commit",
            "commit",
            serde_json::json!("not-an-object-id"),
        ),
        (
            "wrong-source-tree",
            "tree",
            serde_json::json!("not-an-object-id"),
        ),
    ] {
        let mut mutated = row_snapshot_value.clone();
        mutated["source"][field] = value;
        assert_snapshot_refused_unchanged(label, &mutated, "invalid source identity")?;
    }

    let mut unknown_source_field = row_snapshot_value.clone();
    unknown_source_field["source"]["unexpected"] = serde_json::json!(true);
    assert_snapshot_refused_unchanged(
        "unknown-source-field",
        &unknown_source_field,
        "unknown field",
    )?;

    let mut invalid_snapshot_schema = row_snapshot_value.clone();
    invalid_snapshot_schema["schema"] = serde_json::json!("scorecard-series-snapshot/v2");
    assert_snapshot_refused_unchanged(
        "invalid-snapshot-schema",
        &invalid_snapshot_schema,
        "expected scorecard-series-snapshot/v1",
    )?;

    let mut unknown_snapshot_field = row_snapshot_value.clone();
    unknown_snapshot_field["unexpected"] = serde_json::json!(true);
    assert_snapshot_refused_unchanged(
        "unknown-snapshot-field",
        &unknown_snapshot_field,
        "unknown field",
    )?;

    let mut invalid_row_schema = row_snapshot_value.clone();
    invalid_row_schema["rows"][0]["schema"] = serde_json::json!("stress-series/v4");
    recompute_snapshot_rows(&mut invalid_row_schema)?;
    assert_snapshot_refused_unchanged(
        "invalid-row-schema",
        &invalid_row_schema,
        "unknown variant",
    )?;

    let mut unknown_row_field = row_snapshot_value.clone();
    unknown_row_field["rows"][0]["unexpected"] = serde_json::json!(true);
    recompute_snapshot_rows(&mut unknown_row_field)?;
    assert_snapshot_refused_unchanged(
        "unknown-row-field",
        &unknown_row_field,
        "invalid field set",
    )?;

    let mut unknown_series_field = row_snapshot_value.clone();
    unknown_series_field["rows"][0]["series"]["unexpected"] = serde_json::json!(true);
    recompute_snapshot_rows(&mut unknown_series_field)?;
    assert_snapshot_refused_unchanged(
        "unknown-series-field",
        &unknown_series_field,
        "series has an unknown field",
    )?;

    let mut duplicate_event_id = row_snapshot_value.clone();
    let mut duplicate_row = duplicate_event_id["rows"][0].clone();
    duplicate_row["emitted_at"] = serde_json::json!("2026-09-13T18:30:01Z");
    duplicate_event_id["rows"]
        .as_array_mut()
        .expect("snapshot fixture rows are an array")
        .push(duplicate_row);
    recompute_snapshot_rows(&mut duplicate_event_id)?;
    assert_snapshot_refused_unchanged(
        "duplicate-event-id",
        &duplicate_event_id,
        "repeats event_id",
    )?;

    let moved_head_error = project_and_observe_results_with(
        &result_command_root,
        &empty_snapshot_path,
        &empty_snapshot_sha,
        &result_root,
        &fixture_head,
        "fixture-refresh",
        || {
            git_ok(
                &result_command_root,
                &[
                    "-c",
                    "user.email=scorecard@example.invalid",
                    "-c",
                    "user.name=Scorecard Self-Test",
                    "commit",
                    "--quiet",
                    "--allow-empty",
                    "-m",
                    "move head during combined write",
                ],
            )
        },
        |_| Ok(()),
    )
    .expect_err("combined transaction accepted a moved HEAD");
    if !moved_head_error.contains("HEAD moved")
        || read_generated_files(&result_command_root)? != original_combined_pair
    {
        return Err(format!(
            "moved-HEAD refusal changed the generated pair or lost its cause: {moved_head_error}"
        ));
    }
    git_ok(&result_command_root, &["update-ref", "HEAD", &fixture_head])?;

    for (path, label) in [
        (result_command_root.join(SCORECARD), SCORECARD),
        (result_command_root.join(CELLS), CELLS),
    ] {
        let preimage_error = project_and_observe_results_with(
            &result_command_root,
            &empty_snapshot_path,
            &empty_snapshot_sha,
            &result_root,
            &fixture_head,
            "fixture-refresh",
            || fs::write(&path, b"foreign generated bytes\n").map_err(|error| error.to_string()),
            |_| Ok(()),
        )
        .expect_err("combined transaction accepted a changed generated-file preimage");
        if !preimage_error.contains(label) {
            return Err(format!(
                "changed-{label} refusal lost the changed pathname: {preimage_error}"
            ));
        }
        restore_combined_baseline()?;
    }

    let tampered_snapshot = row_snapshot_value.clone();
    let tamper_error = project_and_observe_results_with(
        &result_command_root,
        &row_snapshot_path,
        &row_snapshot_sha,
        &result_root,
        &fixture_head,
        "fixture-refresh",
        || {
            let mut payload = serde_json::to_vec(&tampered_snapshot).map_err(|e| e.to_string())?;
            payload.extend_from_slice(b" \n");
            fs::write(&row_snapshot_path, payload).map_err(|error| error.to_string())
        },
        |_| Ok(()),
    )
    .expect_err("combined transaction accepted snapshot bytes changed while held");
    if !tamper_error.contains("snapshot") {
        return Err(format!(
            "held-snapshot tamper refusal lost its cause: {tamper_error}"
        ));
    }
    let row_snapshot_sha =
        write_scorecard_snapshot_fixture(&row_snapshot_path, &row_snapshot_value)?;

    let rollback_error = project_and_observe_results_with(
        &result_command_root,
        &row_snapshot_path,
        &row_snapshot_sha,
        &result_root,
        &fixture_head,
        "fixture-refresh-after-rollback",
        || Ok(()),
        |replacement| {
            if replacement == 2 {
                Err("planted combined second-file replacement failure".into())
            } else {
                Ok(())
            }
        },
    )
    .expect_err("combined transaction ignored a second-file replacement failure");
    if !rollback_error.contains("restored the original generated files")
        || read_generated_files(&result_command_root)? != original_combined_pair
    {
        return Err(format!(
            "combined transaction did not roll back its first replacement: {rollback_error}"
        ));
    }
    restore_combined_baseline()?;

    // Two series events that both claim the exact direct run, or one whose
    // result conflicts with it, must refuse rather than silently picking an
    // observation representation.
    let mut duplicate_mapping = series_row.clone();
    duplicate_mapping.event_id = "fixture-combined-duplicate-mapping".into();
    duplicate_mapping.emitted_at = "2026-09-13T18:30:01Z".into();
    let multiple_value = scorecard_snapshot_fixture_value(
        &source_commit,
        &source_tree,
        &[series_row.clone(), duplicate_mapping],
    )?;
    let multiple_path = snapshot_root.join("multiple-mapping.json");
    let multiple_sha = write_scorecard_snapshot_fixture(&multiple_path, &multiple_value)?;
    let multiple_error = project_and_observe_results(
        &result_command_root,
        &multiple_path,
        &multiple_sha,
        &result_root,
        &fixture_head,
        "fixture-refresh",
    )
    .expect_err("multiple source rows were allowed to claim one exact direct run");
    if !multiple_error.contains("cannot map one-to-one") {
        return Err(format!(
            "multiple-mapping refusal lost its cause: {multiple_error}"
        ));
    }
    if read_generated_files(&result_command_root)? != original_combined_pair {
        return Err("multiple-mapping refusal changed the generated pair".into());
    }
    let mut conflicting_mapping = series_row.clone();
    conflicting_mapping.event_id = "fixture-combined-conflicting-mapping".into();
    conflicting_mapping.series.outcome = SeriesOutcome::Diverged;
    conflicting_mapping.series.result = Some(ObservedResult::ReplayFailure);
    conflicting_mapping.series.failure_class = Some(FailureClass::ProductFailure);
    let conflicting_value =
        scorecard_snapshot_fixture_value(&source_commit, &source_tree, &[conflicting_mapping])?;
    let conflicting_path = snapshot_root.join("conflicting-mapping.json");
    let conflicting_sha = write_scorecard_snapshot_fixture(&conflicting_path, &conflicting_value)?;
    let conflicting_error = project_and_observe_results(
        &result_command_root,
        &conflicting_path,
        &conflicting_sha,
        &result_root,
        &fixture_head,
        "fixture-refresh",
    )
    .expect_err("a conflicting series/direct result mapping was accepted");
    if !conflicting_error.contains("disagree") {
        return Err(format!(
            "conflicting-mapping refusal lost its cause: {conflicting_error}"
        ));
    }
    if read_generated_files(&result_command_root)? != original_combined_pair {
        return Err("conflicting-mapping refusal changed the generated pair".into());
    }
    restore_generated()?;

    let mut current_row = pressure_row("pass", None, None);
    current_row.cell = verify_id.clone();
    current_row.verification = None;
    let current_summary = result_command_root.join("current-summary.json");
    fs::write(
        &current_summary,
        serde_json::to_vec(&PressureSummary {
            schema: PRESSURE_SUMMARY_SCHEMA,
            hermit_sha: fixture_head.clone(),
            detcore_tree: fixture_detcore_tree.clone(),
            source_tree_dirty: false,
            rows: vec![current_row],
        })
        .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let imported = run_result_command("import-results", Some(&current_summary))?;
    if !imported.status.success()
        || !has_current_replay(&read_json(&result_command_root.join(CELLS))?)
    {
        return Err(format!(
            "import-results did not admit canonical replay evidence with real time: {:?}",
            String::from_utf8_lossy(&imported.stderr)
        ));
    }
    restore_generated()?;

    let mut malformed_retained = replay_row.clone();
    malformed_retained.run_id = "result-command-malformed-retained".into();
    malformed_retained.attempts[0]["verification_report_sha256"] =
        JsonValue::String("0".repeat(64));
    write_result_row(&malformed_retained)?;
    let refused_retained = run_result_command("import-results", Some(&current_summary))?;
    if refused_retained.status.success()
        || !String::from_utf8_lossy(&refused_retained.stderr)
            .contains("verification-report identity does not match")
        || read_generated_files(&result_command_root)? != result_command_before
    {
        return Err("retained import silently skipped a malformed report identity".into());
    }

    // Seed a canonical PASS for this exact cell and Detcore tree. A later
    // no-verdict invocation must be retained without erasing that prior result
    // or manufacturing a result/comparison for the new invocation. This makes
    // the bracket independent of whatever valid observations cells.json has
    // accumulated since the fixture was written.
    let mut prior_verify_row = replay_row.clone();
    prior_verify_row.run_id = "result-command-verify-prior-pass".into();
    prior_verify_row.mode = verify_id.mode.clone();
    let mut prior_verify_report: JsonValue = serde_json::from_str(
        prior_verify_row.attempts[0]["verification_report"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    prior_verify_report["comparison"]["virtualize_time"] = serde_json::json!(true);
    let prior_verify_report = serde_json::to_string(&prior_verify_report).unwrap();
    prior_verify_row.attempts[0]["verification_report_sha256"] = JsonValue::String(format!(
        "{:x}",
        Sha256::digest(prior_verify_report.as_bytes())
    ));
    prior_verify_row.attempts[0]["verification_report"] = JsonValue::String(prior_verify_report);
    write_result_row(&prior_verify_row)?;
    let seeded = run_result_command("observe-results", None)?;
    if !seeded.status.success() {
        return Err(format!(
            "observe-results did not seed canonical verify evidence: {:?}",
            String::from_utf8_lossy(&seeded.stderr)
        ));
    }
    let seeded_cells: TrackedCells = read_json(&result_command_root.join(CELLS))?;
    let seeded_cell = seeded_cells
        .cells
        .iter()
        .find(|cell| cell.id == verify_id)
        .ok_or("observe-results lost the verify fixture cell")?;
    let seeded_observation = seeded_cell
        .observations
        .iter()
        .find(|observation| {
            observation.detcore_tree.as_deref() == Some(&fixture_detcore_tree)
                && observation.provenance == ObservationProvenance::Validate
                && observation.event_ids.is_empty()
        })
        .ok_or("observe-results did not retain the seeded same-tree verify PASS")?;
    if !seeded_observation.results.contains(&ObservedResult::Pass)
        || !seeded_observation
            .canonical_comparisons
            .iter()
            .any(|comparison| {
                comparison.run_id == prior_verify_row.run_id
                    && comparison.result == ObservedResult::Pass
            })
    {
        return Err("observe-results did not retain the seeded same-tree verify PASS".into());
    }
    let prior_results = seeded_observation.results.clone();
    let prior_comparisons = seeded_observation.canonical_comparisons.clone();

    let mut verify_row = prior_verify_row;
    verify_row.run_id = "result-command-verify".into();
    let mut verify_report: JsonValue = serde_json::from_str(
        verify_row.attempts[0]["verification_report"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    verify_report["comparison"]["virtualize_time"] = serde_json::json!(false);
    let verify_report = serde_json::to_string(&verify_report).unwrap();
    verify_row.attempts[0]["verification_report_sha256"] =
        JsonValue::String(format!("{:x}", Sha256::digest(verify_report.as_bytes())));
    verify_row.attempts[0]["verification_report"] = JsonValue::String(verify_report);
    let verify_evidence_identity = verify_row.evidence_identity()?;
    write_result_row(&verify_row)?;
    let unavailable = run_result_command("observe-results", None)?;
    let unavailable_stdout = String::from_utf8_lossy(&unavailable.stdout);
    let observed_cells: TrackedCells = read_json(&result_command_root.join(CELLS))?;
    let observed_cell = observed_cells
        .cells
        .iter()
        .find(|cell| cell.id == verify_id);
    let final_observation = observed_cell.and_then(|cell| {
        cell.observations.iter().find(|observation| {
            observation.detcore_tree.as_deref() == Some(&fixture_detcore_tree)
                && observation.provenance == ObservationProvenance::Validate
                && observation.event_ids.is_empty()
        })
    });
    let retained_no_verdict = observed_cell.is_some_and(|cell| {
        cell.last_tested
            .as_ref()
            .is_some_and(|last| last.hermit_sha == fixture_head)
    }) && final_observation.is_some_and(|observation| {
        observation.results == prior_results
            && observation.canonical_comparisons == prior_comparisons
            && !observation
                .canonical_comparisons
                .iter()
                .any(|comparison| comparison.run_id == verify_row.run_id)
            && observation.invocations.iter().any(|invocation| {
                invocation.run_id == verify_row.run_id
                    && invocation.attempt == Some(1)
                    && invocation.evidence_sha256.as_deref()
                        == Some(verify_evidence_identity.as_str())
                    && invocation.result.is_none()
            })
    });
    if !unavailable.status.success()
        || !unavailable_stdout.contains("DETERMINED NOTHING")
        || unavailable_stdout.contains("expected result for an all-green run")
        || !retained_no_verdict
    {
        return Err(
            "observe-results did not retain exact verify no-verdict evidence without changing the prior same-tree result".into(),
        );
    }
    restore_generated()?;

    for (staged_clean, unrelated_clean, allowed) in [
        (true, true, true),
        (false, true, false),
        (true, false, false),
    ] {
        if observation_dirt_error(staged_clean, unrelated_clean).is_none() != allowed {
            return Err("observe-results accepted forbidden tracked dirt".into());
        }
    }
    let lock_root =
        tempfile::tempdir().map_err(|e| format!("cannot create scorecard lock fixture: {e}"))?;
    let lock_path = lock_root.path().join("writeback.lock");
    let held = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| format!("cannot open scorecard lock fixture: {e}"))?;
    FileExt::lock_exclusive(&held)
        .map_err(|e| format!("cannot hold scorecard lock fixture: {e}"))?;
    let contender = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|e| format!("cannot reopen scorecard lock fixture: {e}"))?;
    let lock_error = wait_for_scorecard_write_lock(&contender, &lock_path, Duration::ZERO)
        .expect_err("a held scorecard lock must refuse at its bounded deadline");
    if !lock_error.contains("timed out after 0s") {
        return Err(format!(
            "scorecard lock refusal lost its deadline: {lock_error}"
        ));
    }
    drop(held);
    wait_for_scorecard_write_lock(&contender, &lock_path, Duration::ZERO)?;

    let pair_root =
        tempfile::tempdir().map_err(|e| format!("cannot create generated-file fixture: {e}"))?;
    fs::create_dir_all(pair_root.path().join("ci/compat-envelope"))
        .map_err(|e| format!("cannot create generated-file fixture: {e}"))?;
    fs::write(pair_root.path().join(SCORECARD), b"old scorecard\n")
        .and_then(|()| fs::write(pair_root.path().join(CELLS), b"old cells\n"))
        .map_err(|e| format!("cannot write generated-file fixture: {e}"))?;
    let original = read_generated_files(pair_root.path())?;
    let updated = GeneratedFiles {
        scorecard: b"new scorecard\n".to_vec(),
        cells: b"new cells\n".to_vec(),
    };
    let rename_count = std::cell::Cell::new(0);
    let error = replace_generated_files_with(
        pair_root.path(),
        &original,
        &updated,
        || Ok(()),
        |replacement| {
            rename_count.set(replacement);
            if replacement == 2 {
                Err("planted second replacement failure".into())
            } else {
                Ok(())
            }
        },
    )
    .expect_err("the planted second replacement failure must refuse the write");
    if !error.contains("restored the original generated files")
        || read_generated_files(pair_root.path())? != original
    {
        return Err(format!(
            "a second generated-file replacement failure did not preserve the original pair: {error}"
        ));
    }
    if replace_generated_files_with(
        pair_root.path(),
        &original,
        &original,
        || Ok(()),
        |_| Err("an unchanged pair must not be replaced".into()),
    )? {
        return Err("an unchanged generated-file pair was replaced".into());
    }
    // ⚠️ READ THE COORDINATES BACK OUT OF STORAGE, NOT OUT OF THE STRUCT THAT
    // JUST WROTE THEM. Every assertion above this point inspects the in-memory
    // fold, and an in-memory assertion cannot see the write at all: a
    // `#[serde(skip)]`, a `skip_serializing_if` that answers wrongly, a reader
    // that stops accepting the key -- each of those loses the value SILENTLY
    // while every bracket above still passes.
    //
    // `first_divergent_syscall` is named explicitly because it is the one
    // coordinate the tracked corpus has never carried: 0 of 2 observations in
    // ci/compat-envelope/cells.json hold it, and the single located observation
    // there holds the other three. Measured at 4e168f2aa5, both folds DO put it
    // on disk, so that gap is a property of the corpus rather than of the
    // pipeline -- and this bracket is what keeps it that way.
    //
    // Encoded with the same function the writers use and parsed with the same
    // reader `load_existing` uses, so the only thing this can pass on is bytes
    // that really round-trip.
    let mut stored_form = observed.clone();
    refresh_measurement(&mut stored_form);
    let encoded = encoded_cells(&stored_form)
        .map_err(|e| format!("storage round-trip bracket could not encode the cells: {e}"))?;
    let reloaded: TrackedCells = serde_json::from_str(&encoded).map_err(|e| {
        format!("storage round-trip bracket could not re-read the encoded cells: {e}")
    })?;
    for provenance in [
        ObservationProvenance::PressureTest,
        ObservationProvenance::Validate,
    ] {
        let find = |cells: &TrackedCells| {
            cells.cells[0]
                .observations
                .iter()
                .find(|observation| {
                    observation.provenance == provenance
                        && observation.detcore_tree.as_deref() == Some("tree-1")
                })
                .cloned()
        };
        let live = find(&stored_form).ok_or_else(|| {
            format!(
                "storage round-trip bracket has no {} observation to check",
                provenance.as_str()
            )
        })?;
        let stored = find(&reloaded).ok_or_else(|| {
            format!(
                "the {} observation did not survive the write at all",
                provenance.as_str()
            )
        })?;
        // NON-VACUITY FIRST. Comparing two empty coordinates for equality would
        // pass forever while proving nothing, which is the exact shape of a
        // silent instrument. Require the value to be PRESENT on disk before
        // asking whether it matches.
        if stored.first_divergent_syscall.is_empty() {
            return Err(format!(
                "the {} observation reached storage carrying no first_divergent_syscall: the \
                 coordinate is computed and then dropped before anything durable records it",
                provenance.as_str()
            ));
        }
        if stored.first_divergent_record.is_empty()
            || stored.first_divergent_scheduler_turn.is_empty()
            || stored.first_divergent_virtual_nanoseconds.is_empty()
        {
            return Err(format!(
                "the {} observation reached storage missing one of the other three coordinates",
                provenance.as_str()
            ));
        }
        if stored != live {
            return Err(format!(
                "the {} observation changed across the storage round trip: syscall on disk {:?}, \
                 in memory {:?}",
                provenance.as_str(),
                stored.first_divergent_syscall.range(),
                live.first_divergent_syscall.range()
            ));
        }
    }
    if reloaded.cells != stored_form.cells {
        return Err("the tracked cells did not survive their own encoding unchanged".into());
    }

    // ⚠️ THE FOUR REASONS A CELL CAN LACK A DIVERGENCE COORDINATE MUST STAY
    // TELLABLE APART. Two of them look identical in every field except
    // `measurement`, and they need OPPOSITE follow-ups:
    //
    //   never-measured      no comparison was imported         -> inspect retained evidence
    //   diverged-unlocated  it was compared, it DID diverge,
    //                       and no axis could say where       -> fix the comparator
    //
    // Trading a missing coordinate for a misleading state is a worse outcome
    // than the missing coordinate, so this bracket folds one row of each shape
    // and requires the two to disagree.
    let unlocated_id = CellId {
        lane: "portable".into(),
        category: "fixture".into(),
        test: "fixture/unlocated".into(),
        mode: "verify".into(),
        backend: "ptrace".into(),
    };
    let bare_cell = |id: &CellId| TrackedCell {
        id: id.clone(),
        enabled: true,
        status: CellStatus::Red,
        ci_disabled_reason: None,
        not_applicable_reason: None,
        last_tested: None,
        observations: Vec::new(),
        measurement: MeasurementState::NeverMeasured,
        green_removal_reason: None,
    };
    let coordinate_less_row = |id: &CellId, outcome: &str| {
        let mut row = validate_row.clone();
        row.run_id = format!("fixture-coordinate-less-{outcome}");
        row.test = id.test.clone();
        row.category = id.category.clone();
        row.lane = id.lane.clone();
        row.mode = id.mode.clone();
        row.backend = Some(id.backend.clone());
        row.outcome = outcome.to_string();
        match outcome {
            "PASS" => {
                row.result = Some(ObservedResult::Pass);
                row.failure_class = None;
                row.error_kind = None;
            }
            "FAIL" => {
                row.result = Some(ObservedResult::DeterminismFailure);
                row.failure_class = Some(FailureClass::ProductFailure);
                row.error_kind = None;
            }
            "ERROR" => {
                row.result = None;
                row.failure_class = Some(FailureClass::UnderstoodInfrastructureFailure);
                row.error_kind = Some("infrastructure".into());
            }
            _ => {
                row.result = None;
                row.failure_class = None;
                row.error_kind = None;
            }
        }
        row.first_divergent_scheduler_turn = None;
        row.first_divergent_virtual_nanoseconds = None;
        row.first_divergent_record = None;
        row.first_divergent_syscall = None;
        row.attempts = vec![validate_attempt(outcome)];
        if !matches!(outcome, "PASS" | "FAIL") {
            let mut report = canonical_verdict::VerificationReport::no_result();
            report.verdict = canonical_verdict::Verdict::InfrastructureError;
            report.no_result_reason = None;
            report.infrastructure_error =
                Some(canonical_verdict::InfrastructureError::SkidOvershoot { count: 1 });
            let report = serde_json::to_string(&report).unwrap();
            row.attempts[0]["outcome"] = JsonValue::String("ERROR".into());
            row.attempts[0]["error_kind"] = JsonValue::String("infrastructure".into());
            row.attempts[0]["status"] = serde_json::json!(1);
            row.attempts[0]["signal"] = JsonValue::Null;
            row.attempts[0]["timed_out"] = JsonValue::Bool(false);
            row.attempts[0]["verification_report_sha256"] =
                JsonValue::String(format!("{:x}", Sha256::digest(report.as_bytes())));
            row.attempts[0]["verification_report"] = JsonValue::String(report);
        }
        if let Some(report_text) = row.attempts[0]["verification_report"].as_str() {
            let mut report: JsonValue = serde_json::from_str(report_text).unwrap();
            report["first_divergent_scheduler_turn"] = JsonValue::Null;
            report["first_divergent_virtual_nanoseconds"] = JsonValue::Null;
            report["first_divergent_record"] = JsonValue::Null;
            report["first_divergent_syscall"] = JsonValue::Null;
            let report = serde_json::to_string(&report).unwrap();
            row.attempts[0]["verification_report_sha256"] =
                JsonValue::String(format!("{:x}", Sha256::digest(report.as_bytes())));
            row.attempts[0]["verification_report"] = JsonValue::String(report);
        }
        BTreeMap::from([(
            id.clone(),
            vec![ResultCandidate {
                evidence_identity: format!("coordinate-less-{outcome}"),
                path: PathBuf::from("fixture/results.jsonl"),
                row,
            }],
        )])
    };
    let mut unlocated = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![bare_cell(&unlocated_id)],
    };
    let unlocated_fold = apply_validate_results(
        &mut unlocated,
        &coordinate_less_row(&unlocated_id, "FAIL"),
        "sha-1",
        "tree-1",
        &depth_fixture,
        true,
        true,
    )
    .map_err(|e| format!("diverged-unlocated bracket failed: {e}"))?;
    refresh_measurement(&mut unlocated);
    if unlocated_fold.located != 0 || unlocated_fold.unlocated != 1 {
        return Err(format!(
            "a divergence that located nothing was counted as {} located / {} unlocated",
            unlocated_fold.located, unlocated_fold.unlocated
        ));
    }
    if unlocated.cells[0].measurement != MeasurementState::DivergedUnlocated {
        return Err(format!(
            "a cell that was compared and diverged without a locatable position reads `{}`, \
             which is indistinguishable from a cell nothing ever ran on",
            unlocated.cells[0].measurement.as_str()
        ));
    }
    // A PASS carries no divergence coordinate, but it MUST leave a pass
    // observation. Before this bracket, every selected cell with only passing
    // retained comparisons remained `never-measured` because the writer threw
    // away the result it needed to distinguish those states.
    let mut passed = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![bare_cell(&unlocated_id)],
    };
    let passed_fold = apply_validate_results(
        &mut passed,
        &coordinate_less_row(&unlocated_id, "PASS"),
        "sha-1",
        "tree-1",
        &depth_fixture,
        true,
        true,
    )
    .map_err(|e| format!("coordinate-less PASS bracket failed: {e}"))?;
    refresh_measurement(&mut passed);
    if passed_fold.passed != 1
        || passed_fold.located != 0
        || passed_fold.unlocated != 0
        || passed.cells[0].observations.len() != 1
        || passed.cells[0].observations[0].results != BTreeSet::from([ObservedResult::Pass])
    {
        return Err("a canonical PASS did not leave one pass observation".into());
    }
    if passed.cells[0].last_tested.is_none() {
        return Err("a coordinate-less PASS was not stamped as tested".into());
    }
    if passed.cells[0].measurement != MeasurementState::MeasuredAndPassed
        || unlocated.cells[0].measurement == passed.cells[0].measurement
    {
        return Err(
            "a diverged-unlocated cell and a measured-and-passed cell read the same \
             measurement"
                .into(),
        );
    }

    // A cell retry can recover after the first Hermit invocation completed
    // without reaching a comparison. The no-result attempt is real execution
    // evidence, but it has no product result or INFO-message counts to store.
    // Name and retain it as measured no-verdict alongside the later canonical
    // PASS.
    let mut no_result_report = canonical_verdict::VerificationReport::no_result();
    no_result_report.no_result_reason = Some(canonical_verdict::NoResultReason::FirstRunRejected {
        exit_code: Some(1),
        signal: None,
        stdout_bytes: 0,
        stderr_bytes: 0,
    });
    no_result_report.guest_exit_code = Some(1);
    let no_result_report = serde_json::to_string(&no_result_report).unwrap();
    let mut no_result_attempt = validate_attempt("PASS");
    no_result_attempt["outcome"] = JsonValue::String("FAIL".into());
    no_result_attempt["status"] = serde_json::json!(125);
    no_result_attempt["verification_report_sha256"] =
        JsonValue::String(format!("{:x}", Sha256::digest(no_result_report.as_bytes())));
    no_result_attempt["verification_report"] = JsonValue::String(no_result_report);

    let mut no_result_row = validate_row.clone();
    no_result_row.run_id = "fixture-recovered-no-result".into();
    no_result_row.attempt = 1;
    no_result_row.outcome = "FAIL".into();
    no_result_row.result = Some(ObservedResult::CrashError);
    no_result_row.failure_class = Some(FailureClass::ProductFailure);
    no_result_row.error_kind = None;
    no_result_row.first_divergent_scheduler_turn = None;
    no_result_row.first_divergent_virtual_nanoseconds = None;
    no_result_row.first_divergent_record = None;
    no_result_row.first_divergent_syscall = None;
    no_result_row.attempts = vec![no_result_attempt];
    let no_result_identity = no_result_row.evidence_identity().unwrap();

    let mut unspecified_no_result = no_result_row.clone();
    let mut report: JsonValue = serde_json::from_str(
        unspecified_no_result.attempts[0]["verification_report"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    report["no_result_reason"] = JsonValue::Null;
    let report = serde_json::to_string(&report).unwrap();
    unspecified_no_result.attempts[0]["verification_report_sha256"] =
        JsonValue::String(format!("{:x}", Sha256::digest(report.as_bytes())));
    unspecified_no_result.attempts[0]["verification_report"] = JsonValue::String(report);
    if unspecified_no_result.typed_no_result_reason()?.as_deref()
        != Some("explicitly unspecified no-result cause")
    {
        return Err(
            "an explicitly null no_result_reason was not retained as explicit absence".into(),
        );
    }

    let mut recovered_pass_row = validate_row.clone();
    recovered_pass_row.run_id = no_result_row.run_id.clone();
    recovered_pass_row.attempt = 2;
    recovered_pass_row.outcome = "PASS".into();
    recovered_pass_row.result = Some(ObservedResult::Pass);
    recovered_pass_row.failure_class = None;
    recovered_pass_row.error_kind = None;
    recovered_pass_row.first_divergent_scheduler_turn = None;
    recovered_pass_row.first_divergent_virtual_nanoseconds = None;
    recovered_pass_row.first_divergent_record = None;
    recovered_pass_row.first_divergent_syscall = None;
    recovered_pass_row.attempts = vec![validate_attempt("PASS")];
    let recovered_pass_identity = recovered_pass_row.evidence_identity().unwrap();

    let recovered_rows = BTreeMap::from([(
        unlocated_id.clone(),
        vec![
            ResultCandidate {
                evidence_identity: no_result_identity,
                path: PathBuf::from("fixture/results.jsonl"),
                row: no_result_row.clone(),
            },
            ResultCandidate {
                evidence_identity: recovered_pass_identity,
                path: PathBuf::from("fixture/results.jsonl"),
                row: recovered_pass_row.clone(),
            },
        ],
    )]);
    let mut recovered = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![bare_cell(&unlocated_id)],
    };
    let recovered_fold = apply_validate_results(
        &mut recovered,
        &recovered_rows,
        "sha-1",
        "tree-1",
        &depth_fixture,
        true,
        true,
    )
    .map_err(|e| format!("a recovered no-result retry was refused: {e}"))?;
    refresh_measurement(&mut recovered);
    if recovered_fold.passed != 1
        || recovered_fold.errored.len() != 1
        || !recovered_fold.errored[0].contains("NO_RESULT")
        || recovered_fold.reads_all_green()
        || recovered.cells[0].measurement != MeasurementState::MeasuredAndPassed
        || recovered.cells[0].observations.len() != 1
        || recovered.cells[0].observations[0].results != BTreeSet::from([ObservedResult::Pass])
        || recovered.cells[0].observations[0]
            .canonical_comparisons
            .len()
            != 1
    {
        return Err(format!(
            "a recovered no-result retry was not named while retaining only its canonical PASS: {:?}",
            recovered_fold
        ));
    }

    let fold_fixture_rows =
        |fixture_rows: Vec<ResultRow>| -> Result<(TrackedCells, ValidateFold), String> {
            let candidates = fixture_rows
                .into_iter()
                .map(|row| {
                    Ok(ResultCandidate {
                        evidence_identity: row.evidence_identity()?,
                        path: PathBuf::from("fixture/results.jsonl"),
                        row,
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;
            let rows = BTreeMap::from([(unlocated_id.clone(), candidates)]);
            let mut tracked = TrackedCells {
                schema: SCHEMA,
                projection: None,
                cells: vec![bare_cell(&unlocated_id)],
            };
            let fold = apply_validate_results(
                &mut tracked,
                &rows,
                "sha-1",
                "tree-1",
                &depth_fixture,
                true,
                true,
            )?;
            refresh_measurement(&mut tracked);
            Ok((tracked, fold))
        };
    let fold_fixture_row = |row: ResultRow| fold_fixture_rows(vec![row]);

    let mut not_run_row = validate_row.clone();
    not_run_row.run_id = "fixture-recovered-not-run".into();
    not_run_row.attempt = 1;
    not_run_row.outcome = "ERROR".into();
    not_run_row.result = None;
    not_run_row.failure_class = Some(FailureClass::NoResult);
    not_run_row.error_kind = Some("incomplete-verification-evidence".into());
    not_run_row.first_divergent_scheduler_turn = None;
    not_run_row.first_divergent_virtual_nanoseconds = None;
    not_run_row.first_divergent_record = None;
    not_run_row.first_divergent_syscall = None;
    not_run_row.attempts = vec![validate_attempt("ERROR")];
    let mut not_run_pass = recovered_pass_row.clone();
    not_run_pass.run_id = not_run_row.run_id.clone();

    for (label, rows) in [
        (
            "attempt-order",
            vec![not_run_row.clone(), not_run_pass.clone()],
        ),
        (
            "file-order",
            vec![not_run_pass.clone(), not_run_row.clone()],
        ),
    ] {
        let (tracked, fold) = fold_fixture_rows(rows)
            .map_err(|error| format!("recovered NotRun {label} was refused: {error}"))?;
        if fold.passed != 1
            || fold.errored.len() != 1
            || !fold.errored[0].contains("did not complete its first run")
            || fold.reads_all_green()
            || tracked.cells[0].measurement != MeasurementState::MeasuredAndPassed
            || tracked.cells[0].last_tested.is_none()
            || tracked.cells[0].observations.len() != 1
            || tracked.cells[0].observations[0].results
                != BTreeSet::from([ObservedResult::Pass, ObservedResult::Timeout])
            || tracked.cells[0].observations[0].invocations.len() != 2
            || tracked.cells[0].observations[0]
                .invocations
                .iter()
                .filter_map(|invocation| invocation.attempt)
                .collect::<BTreeSet<_>>()
                != BTreeSet::from([1])
        {
            return Err(format!(
                "recovered NotRun {label} did not retain the timeout and later canonical PASS: {fold:?}"
            ));
        }
    }

    // Fixture preparation can consume the whole pre-execution budget before
    // the first Hermit process starts. That attempt has no process disposition,
    // but the producer's typed NotRun report must coexist with a later
    // canonical PASS instead of making the entire write-back unreadable.
    let mut prelaunch_not_run = not_run_row.clone();
    prelaunch_not_run.run_id = "fixture-recovered-prelaunch-not-run".into();
    prelaunch_not_run.attempts[0]["status"] = JsonValue::Null;
    prelaunch_not_run.attempts[0]["signal"] = JsonValue::Null;
    prelaunch_not_run.attempts[0]["timed_out"] = JsonValue::Bool(true);
    let mut prelaunch_pass = recovered_pass_row.clone();
    prelaunch_pass.run_id = prelaunch_not_run.run_id.clone();
    let (tracked, fold) = fold_fixture_rows(vec![prelaunch_not_run.clone(), prelaunch_pass])
        .map_err(|error| format!("recovered pre-launch NotRun was refused: {error}"))?;
    if fold.passed != 1
        || fold.errored.len() != 1
        || !fold.errored[0].contains("did not complete its first run")
        || fold.reads_all_green()
        || tracked.cells[0].measurement != MeasurementState::MeasuredAndPassed
        || tracked.cells[0].observations.len() != 1
        || tracked.cells[0].observations[0].results
            != BTreeSet::from([ObservedResult::Pass, ObservedResult::Timeout])
        || tracked.cells[0].observations[0].invocations.len() != 2
    {
        return Err(format!(
            "a recovered pre-launch NotRun did not retain its timeout and later canonical PASS: {fold:?}"
        ));
    }
    for (label, field, value) in [
        ("not-timed-out", "timed_out", JsonValue::Bool(false)),
        (
            "wrong-error-kind",
            "error_kind",
            JsonValue::String("infrastructure".into()),
        ),
    ] {
        let mut malformed = prelaunch_not_run.clone();
        malformed.attempts[0][field] = value;
        if fold_fixture_rows(vec![malformed]).is_ok() {
            return Err(format!(
                "{label} pre-launch NotRun evidence was accepted without a process disposition"
            ));
        }
    }

    for (label, rows, expected_measurement, expected_results, expected_runs) in [
        ("terminal", vec![not_run_row.clone()]),
        (
            "after-match",
            vec![
                {
                    let mut first = not_run_pass.clone();
                    first.attempt = 1;
                    first
                },
                {
                    let mut terminal = not_run_row.clone();
                    terminal.attempt = 2;
                    terminal
                },
            ],
        ),
        (
            "different-run",
            vec![not_run_row.clone(), {
                let mut unrelated = not_run_pass.clone();
                unrelated.run_id = "fixture-unrelated-run".into();
                unrelated
            }],
        ),
    ]
    .into_iter()
    .map(|(label, rows)| {
        let expected_measurement = if label == "terminal" {
            MeasurementState::MeasuredNoVerdict
        } else {
            MeasurementState::MeasuredAndPassed
        };
        let expected_results = if label == "terminal" {
            BTreeSet::from([ObservedResult::Timeout])
        } else {
            BTreeSet::from([ObservedResult::Pass, ObservedResult::Timeout])
        };
        let expected_runs = if label == "terminal" { 1 } else { 2 };
        (
            label,
            rows,
            expected_measurement,
            expected_results,
            expected_runs,
        )
    }) {
        let (tracked, fold) = fold_fixture_rows(rows)
            .map_err(|error| format!("{label} NotRun evidence was refused: {error}"))?;
        if fold.errored.len() != 1
            || !fold.errored[0].contains("did not complete its first run")
            || tracked.cells[0].measurement != expected_measurement
            || tracked.cells[0].last_tested.is_none()
            || tracked.cells[0].observations.len() != 1
            || tracked.cells[0].observations[0].results != expected_results
            || tracked.cells[0].observations[0].invocations.len() != expected_runs
        {
            return Err(format!(
                "{label} NotRun evidence was not retained as measured no-verdict: {fold:?}"
            ));
        }
    }
    let mut mixed_terminal = not_run_row.clone();
    mixed_terminal.run_id = "fixture-mixed-terminal-not-run".into();
    mixed_terminal
        .attempts
        .push(no_result_row.attempts[0].clone());
    let (tracked, fold) = fold_fixture_rows(vec![mixed_terminal])?;
    if fold.errored.len() != 1
        || !fold.errored[0].contains("did not complete its first run")
        || tracked.cells[0].measurement != MeasurementState::MeasuredNoVerdict
        || tracked.cells[0].observations[0].results != BTreeSet::from([ObservedResult::Timeout])
    {
        return Err("mixed terminal NotRun was not retained as measured no-verdict".into());
    }

    let mut later_divergence = validate_row.clone();
    later_divergence.run_id = not_run_row.run_id.clone();
    later_divergence.attempt = 2;
    let (tracked, fold) = fold_fixture_rows(vec![not_run_row.clone(), later_divergence])?;
    if fold.located != 1
        || fold.errored.len() != 1
        || tracked.cells[0].last_tested.is_none()
        || tracked.cells[0].observations.len() != 1
        || tracked.cells[0].observations[0].results
            != BTreeSet::from([ObservedResult::DeterminismFailure, ObservedResult::Timeout])
    {
        return Err(format!(
            "a later canonical divergence did not remain sticky after NotRun: {fold:?}"
        ));
    }

    for (label, mut malformed) in [
        ("wrong-hash", not_run_row.clone()),
        ("missing-report", not_run_row.clone()),
    ] {
        if label == "wrong-hash" {
            malformed.attempts[0]["verification_report_sha256"] = "0".repeat(64).into();
        } else {
            malformed.attempts[0]
                .as_object_mut()
                .unwrap()
                .remove("verification_report");
        }
        if fold_fixture_rows(vec![malformed, not_run_pass.clone()]).is_ok() {
            return Err(format!(
                "{label} NotRun evidence was authorized by a later canonical PASS"
            ));
        }
    }

    let (tracked, fold) = fold_fixture_rows(vec![not_run_pass.clone(), not_run_pass.clone()])?;
    if fold.passed != 1 || tracked.cells[0].observations.len() != 1 {
        return Err("identical duplicate outer-attempt evidence was counted twice".into());
    }
    let mut conflicting_attempt = validate_row.clone();
    conflicting_attempt.run_id = not_run_pass.run_id.clone();
    conflicting_attempt.attempt = not_run_pass.attempt;
    if !fold_fixture_rows(vec![not_run_pass, conflicting_attempt])
        .expect_err("conflicting outer-attempt evidence was accepted")
        .contains("conflicting evidence for outer attempt")
    {
        return Err("conflicting outer-attempt refusal was not explicit".into());
    }

    let mut atomic_first = recovered_pass_row.clone();
    atomic_first.run_id = "fixture-atomic-refusal".into();
    atomic_first.attempt = 1;
    let mut atomic_malformed = atomic_first.clone();
    atomic_malformed.attempt = 2;
    atomic_malformed.attempts[0]["index"] = serde_json::json!(2);
    let atomic_rows = [atomic_first, atomic_malformed]
        .into_iter()
        .map(|row| {
            Ok(ResultCandidate {
                evidence_identity: row.evidence_identity()?,
                path: PathBuf::from("fixture/results.jsonl"),
                row,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut atomic_tracked = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![bare_cell(&unlocated_id)],
    };
    let atomic_before = encoded_cells(&atomic_tracked)?;
    if !apply_validate_results(
        &mut atomic_tracked,
        &BTreeMap::from([(unlocated_id.clone(), atomic_rows)]),
        "sha-1",
        "tree-1",
        &depth_fixture,
        true,
        true,
    )
    .expect_err("a malformed later attempt was accepted")
    .contains("unreadable attempt record")
        || encoded_cells(&atomic_tracked)? != atomic_before
    {
        return Err("validate evidence mutated the scorecard before a later refusal".into());
    }

    let mut timed_out_no_result = validate_attempt("PASS");
    let timed_out_report =
        serde_json::to_string(&canonical_verdict::VerificationReport::no_result()).unwrap();
    timed_out_no_result["index"] = JsonValue::String("seed-31".into());
    timed_out_no_result["outcome"] = JsonValue::String("ERROR".into());
    timed_out_no_result["error_kind"] =
        JsonValue::String("incomplete-verification-evidence".into());
    timed_out_no_result["status"] = JsonValue::Null;
    timed_out_no_result["signal"] = serde_json::json!(15);
    timed_out_no_result["timed_out"] = JsonValue::Bool(true);
    timed_out_no_result["argv"] = serde_json::json!(["hermit", "run", "--seed=31"]);
    timed_out_no_result["shell_command"] =
        JsonValue::String("cd /repo && env LC_ALL=C hermit run --seed=31".into());
    timed_out_no_result["verification_report_sha256"] =
        JsonValue::String(format!("{:x}", Sha256::digest(timed_out_report.as_bytes())));
    timed_out_no_result["verification_report"] = JsonValue::String(timed_out_report);
    let bind_row_to_first_attempt = |row: &mut ResultRow| {
        row.argv = serde_json::from_value(row.attempts[0]["argv"].clone()).unwrap();
        row.effective_args = row.argv.iter().skip(1).cloned().collect();
        row.guest_argv = serde_json::from_value(row.attempts[0]["guest_argv"].clone()).unwrap();
        row.env = serde_json::from_value(row.attempts[0]["env"].clone()).unwrap();
        row.cwd = row.attempts[0]["cwd"].as_str().unwrap().into();
        row.shell_command = row.attempts[0]["shell_command"].as_str().unwrap().into();
    };

    // Run 1550's exact evidence shape: many canonical matches followed by one
    // timed-out NotRun. Attempt order must not change the whole-cell result.
    for (label, attempts) in [
        (
            "match-then-timeout",
            vec![validate_attempt("PASS"), timed_out_no_result.clone()],
        ),
        (
            "timeout-then-match",
            vec![timed_out_no_result.clone(), validate_attempt("PASS")],
        ),
    ] {
        let mut row = validate_row.clone();
        row.run_id = format!("fixture-run1550-{label}");
        row.mode = "chaos".into();
        row.outcome = "ERROR".into();
        row.result = None;
        row.failure_class = Some(FailureClass::NoResult);
        row.error_kind = Some("incomplete-verification-evidence".into());
        row.first_divergent_scheduler_turn = None;
        row.first_divergent_virtual_nanoseconds = None;
        row.first_divergent_record = None;
        row.first_divergent_syscall = None;
        row.attempts = attempts;
        bind_row_to_first_attempt(&mut row);
        let (tracked, fold) = fold_fixture_row(row)?;
        if fold.errored.len() != 1
            || fold.reads_all_green()
            || tracked.cells[0].measurement != MeasurementState::MeasuredNoVerdict
            || tracked.cells[0].observations.len() != 1
            || tracked.cells[0].observations[0].results != BTreeSet::from([ObservedResult::Timeout])
            || tracked.cells[0].last_tested.is_none()
        {
            return Err(format!(
                "run1550-style {label} evidence was not retained as one timeout no-verdict: {fold:?}"
            ));
        }
    }

    let mut all_not_run = validate_row.clone();
    all_not_run.run_id = "fixture-all-not-run".into();
    all_not_run.mode = "chaos".into();
    all_not_run.outcome = "ERROR".into();
    all_not_run.result = None;
    all_not_run.failure_class = Some(FailureClass::NoResult);
    all_not_run.error_kind = Some("incomplete-verification-evidence".into());
    all_not_run.first_divergent_scheduler_turn = None;
    all_not_run.first_divergent_virtual_nanoseconds = None;
    all_not_run.first_divergent_record = None;
    all_not_run.first_divergent_syscall = None;
    all_not_run.attempts = vec![timed_out_no_result.clone()];
    bind_row_to_first_attempt(&mut all_not_run);
    let (tracked, fold) = fold_fixture_row(all_not_run)?;
    if fold.errored.len() != 1
        || !fold.errored[0].contains("did not complete its first run")
        || tracked.cells[0].measurement != MeasurementState::MeasuredNoVerdict
        || tracked.cells[0].observations[0].results != BTreeSet::from([ObservedResult::Timeout])
    {
        return Err("standalone NotRun was not retained as measured no-verdict".into());
    }

    // A non-timeout NotRun such as KVM exit 125 is still a measured
    // no-verdict, but its exit status does not establish a crash or timeout.
    // Keep it named and retain the exact invocation with no invented result.
    let mut kvm_unavailable = not_run_row.clone();
    kvm_unavailable.run_id = "fixture-kvm-exit-125".into();
    kvm_unavailable.attempts[0]["timed_out"] = JsonValue::Bool(false);
    kvm_unavailable.attempts[0]["status"] = serde_json::json!(125);
    kvm_unavailable.attempts[0]["signal"] = JsonValue::Null;
    let (tracked, fold) = fold_fixture_row(kvm_unavailable)?;
    if fold.errored.len() != 1
        || !fold.errored[0].contains("status=Some(125)")
        || tracked.cells[0].measurement != MeasurementState::MeasuredNoVerdict
        || !tracked.cells[0].observations[0].results.is_empty()
        || tracked.cells[0].observations[0].invocations.len() != 1
        || tracked.cells[0].observations[0]
            .invocations
            .iter()
            .next()
            .is_none_or(|invocation| {
                invocation.result.is_some()
                    || invocation.attempt != Some(1)
                    || invocation.evidence_sha256.is_none()
            })
    {
        return Err(
            "KVM exit 125 was classified as a product result instead of unavailable".into(),
        );
    }

    // One well-formed no-verdict row must not suppress a good neighboring
    // cell. Both identities survive one atomic fold.
    let no_verdict_id = CellId {
        test: "fixture/no-verdict-neighbor".into(),
        ..unlocated_id.clone()
    };
    let mut no_verdict_neighbor = not_run_row.clone();
    no_verdict_neighbor.test = no_verdict_id.test.clone();
    let pass_neighbor = recovered_pass_row.clone();
    let neighbor_rows = BTreeMap::from([
        (
            unlocated_id.clone(),
            vec![ResultCandidate {
                evidence_identity: pass_neighbor.evidence_identity()?,
                path: PathBuf::from("fixture/pass-results.jsonl"),
                row: pass_neighbor,
            }],
        ),
        (
            no_verdict_id.clone(),
            vec![ResultCandidate {
                evidence_identity: no_verdict_neighbor.evidence_identity()?,
                path: PathBuf::from("fixture/no-verdict-results.jsonl"),
                row: no_verdict_neighbor,
            }],
        ),
    ]);
    let mut neighbor_tracked = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![bare_cell(&unlocated_id), bare_cell(&no_verdict_id)],
    };
    let neighbor_fold = apply_validate_results(
        &mut neighbor_tracked,
        &neighbor_rows,
        "sha-1",
        "tree-1",
        &depth_fixture,
        true,
        true,
    )?;
    refresh_measurement(&mut neighbor_tracked);
    if neighbor_fold.passed != 1
        || neighbor_fold.errored.len() != 1
        || neighbor_tracked.cells[0].measurement != MeasurementState::MeasuredAndPassed
        || neighbor_tracked.cells[1].measurement != MeasurementState::MeasuredNoVerdict
        || neighbor_tracked
            .cells
            .iter()
            .any(|cell| cell.last_tested.is_none() || cell.observations.len() != 1)
    {
        return Err(format!(
            "a no-verdict row suppressed its good neighbor: {neighbor_fold:?}"
        ));
    }

    for (label, outcome, attempts) in [
        (
            "match-plus-divergence",
            "FAIL",
            vec![validate_attempt("PASS"), validate_attempt("FAIL")],
        ),
        (
            "divergence-plus-no-result",
            "ERROR",
            vec![validate_attempt("FAIL"), timed_out_no_result.clone()],
        ),
    ] {
        let mut row = validate_row.clone();
        row.run_id = format!("fixture-{label}");
        row.mode = "chaos".into();
        row.outcome = outcome.into();
        if outcome == "ERROR" {
            row.result = None;
            row.failure_class = Some(FailureClass::NoResult);
            row.error_kind = Some("incomplete-verification-evidence".into());
        }
        row.attempts = attempts;
        let (tracked, fold) = fold_fixture_row(row)?;
        if fold.located != 1
            || !fold.errored.is_empty()
            || tracked.cells[0].last_tested.is_none()
            || tracked.cells[0].observations.len() != 1
            || tracked.cells[0].observations[0].results
                != BTreeSet::from([ObservedResult::DeterminismFailure])
        {
            return Err(format!(
                "{label} did not preserve the canonical divergence: {fold:?}"
            ));
        }
    }

    for (outcome, expected_passes, expected_errors) in
        [("PASS", 1usize, 0usize), ("FAIL", 0usize, 1usize)]
    {
        let mut row = validate_row.clone();
        row.run_id = format!("fixture-all-match-{outcome}");
        row.outcome = outcome.into();
        if outcome == "PASS" {
            row.result = Some(ObservedResult::Pass);
            row.failure_class = None;
        } else {
            row.result = Some(ObservedResult::CrashError);
            row.failure_class = Some(FailureClass::ProductFailure);
        }
        row.error_kind = None;
        row.first_divergent_scheduler_turn = None;
        row.first_divergent_virtual_nanoseconds = None;
        row.first_divergent_record = None;
        row.first_divergent_syscall = None;
        row.attempts = vec![validate_attempt("PASS"), validate_attempt("PASS")];
        let (tracked, fold) = fold_fixture_row(row)?;
        if fold.passed != expected_passes
            || fold.errored.len() != expected_errors
            || (outcome == "PASS"
                && (tracked.cells[0].observations.len() != 1
                    || tracked.cells[0].last_tested.is_none()))
            || (outcome == "FAIL"
                && (tracked.cells[0].measurement != MeasurementState::MeasuredNoVerdict
                    || tracked.cells[0].observations.len() != 1
                    || !tracked.cells[0].observations[0].results.is_empty()
                    || tracked.cells[0].last_tested.is_none()))
        {
            return Err(format!(
                "all-match outer {outcome} was classified incorrectly: {fold:?}"
            ));
        }
    }

    let mut timed_out_without_report = timed_out_no_result.clone();
    timed_out_without_report
        .as_object_mut()
        .unwrap()
        .remove("verification_report");
    timed_out_without_report
        .as_object_mut()
        .unwrap()
        .remove("verification_report_sha256");
    let mut mixed_missing_report = validate_row.clone();
    mixed_missing_report.run_id = "fixture-match-plus-missing-timeout-report".into();
    mixed_missing_report.mode = "chaos".into();
    mixed_missing_report.outcome = "ERROR".into();
    mixed_missing_report.result = None;
    mixed_missing_report.failure_class = Some(FailureClass::NoResult);
    mixed_missing_report.error_kind = Some("incomplete-verification-evidence".into());
    mixed_missing_report.first_divergent_scheduler_turn = None;
    mixed_missing_report.first_divergent_virtual_nanoseconds = None;
    mixed_missing_report.first_divergent_record = None;
    mixed_missing_report.first_divergent_syscall = None;
    mixed_missing_report.attempts =
        vec![validate_attempt("PASS"), timed_out_without_report.clone()];
    let (tracked, fold) = fold_fixture_row(mixed_missing_report)?;
    if fold.errored.len() != 1
        || tracked.cells[0].measurement != MeasurementState::MeasuredNoVerdict
        || tracked.cells[0].observations[0].results != BTreeSet::from([ObservedResult::Timeout])
        || tracked.cells[0].last_tested.is_none()
    {
        return Err("a timed-out missing report was not named and retained".into());
    }
    timed_out_without_report["timed_out"] = JsonValue::Bool(false);
    let mut unexplained_missing = validate_row.clone();
    unexplained_missing.run_id = "fixture-unexplained-missing-report".into();
    unexplained_missing.outcome = "ERROR".into();
    unexplained_missing.result = None;
    unexplained_missing.failure_class = Some(FailureClass::NoResult);
    unexplained_missing.error_kind = Some("incomplete-verification-evidence".into());
    unexplained_missing.first_divergent_scheduler_turn = None;
    unexplained_missing.first_divergent_virtual_nanoseconds = None;
    unexplained_missing.first_divergent_record = None;
    unexplained_missing.first_divergent_syscall = None;
    unexplained_missing.attempts = vec![timed_out_without_report];
    bind_row_to_first_attempt(&mut unexplained_missing);
    if !fold_fixture_row(unexplained_missing)
        .expect_err("an unexplained missing report was accepted")
        .contains("no complete timeout disposition")
    {
        return Err("unexplained missing-report refusal did not name its disposition gap".into());
    }

    let assert_recovered_refuses = |mut row: ResultRow, expected: &str| {
        row.run_id = format!("fixture-refuse-{expected}");
        let identity = row.evidence_identity().unwrap();
        let rows = BTreeMap::from([(
            unlocated_id.clone(),
            vec![ResultCandidate {
                evidence_identity: identity,
                path: PathBuf::from("fixture/results.jsonl"),
                row,
            }],
        )]);
        let error = apply_validate_results(
            &mut TrackedCells {
                schema: SCHEMA,
                projection: None,
                cells: vec![bare_cell(&unlocated_id)],
            },
            &rows,
            "sha-1",
            "tree-1",
            &depth_fixture,
            true,
            true,
        )
        .expect_err("malformed no-result evidence was accepted");
        if !error.contains(expected) {
            panic!("expected refusal containing {expected:?}, got {error:?}");
        }
    };
    let replace_embedded_report = |row: &mut ResultRow, report: JsonValue| {
        let report = serde_json::to_string(&report).unwrap();
        row.attempts[0]["verification_report_sha256"] =
            JsonValue::String(format!("{:x}", Sha256::digest(report.as_bytes())));
        row.attempts[0]["verification_report"] = JsonValue::String(report);
    };

    let mut missing_report = no_result_row.clone();
    missing_report.attempts[0]
        .as_object_mut()
        .unwrap()
        .remove("verification_report");
    assert_recovered_refuses(missing_report, "no embedded verification report");

    let mut bad_hash = no_result_row.clone();
    bad_hash.attempts[0]["verification_report_sha256"] = JsonValue::String("0".repeat(64));
    assert_recovered_refuses(bad_hash, "identity does not match");

    let mut malformed_report = no_result_row.clone();
    malformed_report.attempts[0]["verification_report"] = JsonValue::String("{".into());
    malformed_report.attempts[0]["verification_report_sha256"] =
        JsonValue::String(format!("{:x}", Sha256::digest(b"{")));
    assert_recovered_refuses(malformed_report, "unreadable verification report");

    let mut contradictory_outer_class = no_result_row.clone();
    contradictory_outer_class.outcome = "ERROR".into();
    assert_recovered_refuses(
        contradictory_outer_class,
        "ERROR row must not carry a product observation",
    );

    let mut not_run = no_result_row.clone();
    replace_embedded_report(
        &mut not_run,
        serde_json::to_value(canonical_verdict::VerificationReport::no_result()).unwrap(),
    );
    assert_recovered_refuses(
        not_run,
        "NotRun report has no complete process or pre-launch timeout disposition",
    );

    let mut attempt_outcome_mismatch = no_result_row.clone();
    attempt_outcome_mismatch.attempts[0]["outcome"] = JsonValue::String("PASS".into());
    assert_recovered_refuses(
        attempt_outcome_mismatch,
        "does not agree with FAIL/no_result",
    );

    for (name, status, signal, expected) in [
        (
            "zero-status",
            serde_json::json!(0),
            JsonValue::Null,
            "no nonzero wrapper exit status",
        ),
        (
            "missing-status",
            JsonValue::Null,
            JsonValue::Null,
            "no nonzero wrapper exit status",
        ),
        (
            "signal",
            serde_json::json!(125),
            serde_json::json!(9),
            "signal beside its wrapper exit status",
        ),
    ] {
        let mut invalid_disposition = no_result_row.clone();
        invalid_disposition.run_id = format!("fixture-{name}");
        invalid_disposition.attempts[0]["status"] = status;
        invalid_disposition.attempts[0]["signal"] = signal;
        assert_recovered_refuses(invalid_disposition, expected);
    }

    let mut attempt_divergence = no_result_row.clone();
    attempt_divergence.attempts[0]["first_divergent_record"] = serde_json::json!(1);
    assert_recovered_refuses(attempt_divergence, "attempt 1 carries divergence evidence");

    let mut row_divergence = no_result_row.clone();
    row_divergence.first_divergent_record = Some(1);
    assert_recovered_refuses(row_divergence, "row carries a divergence coordinate");

    for (name, field, value) in [
        (
            "report-coordinate",
            "first_divergent_record",
            serde_json::json!(1),
        ),
        (
            "report-message",
            "first_divergent_left_message",
            serde_json::json!("unexpected divergence"),
        ),
        (
            "report-dbt-count",
            "dbt_counted_branches",
            serde_json::json!({"left": 1, "right": 1}),
        ),
        (
            "report-second-run",
            "runtime",
            serde_json::json!({
                "run1": {"scheduler_turns": 1, "virtual_nanoseconds": 1},
                "run2": {"scheduler_turns": 1, "virtual_nanoseconds": 1}
            }),
        ),
    ] {
        let mut contradictory = no_result_row.clone();
        contradictory.run_id = format!("fixture-{name}");
        let mut report: JsonValue = serde_json::from_str(
            contradictory.attempts[0]["verification_report"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        report[field] = value;
        replace_embedded_report(&mut contradictory, report);
        assert_recovered_refuses(contradictory, "contradictory no_result");
    }

    let mut disposition_mismatch = no_result_row.clone();
    let mut report: JsonValue = serde_json::from_str(
        disposition_mismatch.attempts[0]["verification_report"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    report["guest_exit_code"] = serde_json::json!(2);
    replace_embedded_report(&mut disposition_mismatch, report);
    assert_recovered_refuses(disposition_mismatch, "disposition does not match");

    for outcome in ["PASS", "FAIL"] {
        let mut missing_counts = validate_row.clone();
        missing_counts.run_id = format!("fixture-{outcome}-missing-counts");
        missing_counts.outcome = outcome.into();
        if outcome == "PASS" {
            missing_counts.result = Some(ObservedResult::Pass);
            missing_counts.failure_class = None;
        } else {
            missing_counts.result = Some(ObservedResult::DeterminismFailure);
            missing_counts.failure_class = Some(FailureClass::ProductFailure);
        }
        missing_counts.error_kind = None;
        missing_counts.first_divergent_scheduler_turn = None;
        missing_counts.first_divergent_virtual_nanoseconds = None;
        missing_counts.first_divergent_record = None;
        missing_counts.first_divergent_syscall = None;
        missing_counts.attempts = vec![validate_attempt(outcome)];
        let report_text = missing_counts.attempts[0]["verification_report"]
            .as_str()
            .unwrap();
        let mut report: JsonValue = serde_json::from_str(report_text).unwrap();
        report["compared_log_messages"] = JsonValue::Null;
        let report = serde_json::to_string(&report).unwrap();
        missing_counts.attempts[0]["verification_report_sha256"] =
            JsonValue::String(format!("{:x}", Sha256::digest(report.as_bytes())));
        missing_counts.attempts[0]["verification_report"] = JsonValue::String(report);
        assert_recovered_refuses(missing_counts, "no left INFO-message count");
    }

    for (field, weaker_value) in [
        ("display_name", serde_json::json!("Stripped")),
        ("virtualize_time", serde_json::json!(false)),
        ("canonicalizations", serde_json::json!([])),
        ("stripped_prefixes", serde_json::json!([])),
    ] {
        let mut weak_rows = coordinate_less_row(&unlocated_id, "PASS");
        let weak = &mut weak_rows.get_mut(&unlocated_id).unwrap()[0].row;
        let mut report: JsonValue =
            serde_json::from_str(weak.attempts[0]["verification_report"].as_str().unwrap())
                .unwrap();
        report["comparison"][field] = weaker_value;
        let report = serde_json::to_string(&report).unwrap();
        weak.attempts[0]["verification_report_sha256"] =
            JsonValue::String(format!("{:x}", Sha256::digest(report.as_bytes())));
        weak.attempts[0]["verification_report"] = JsonValue::String(report);
        let mut weak_tracked = TrackedCells {
            schema: SCHEMA,
            projection: None,
            cells: vec![bare_cell(&unlocated_id)],
        };
        let weak_fold = apply_validate_results(
            &mut weak_tracked,
            &weak_rows,
            "sha-1",
            "tree-1",
            &depth_fixture,
            true,
            true,
        )
        .map_err(|error| format!("a weaker {field} comparison failed the fold: {error}"))?;
        refresh_measurement(&mut weak_tracked);
        if weak_fold.errored.len() != 1
            || weak_fold.reads_all_green()
            || weak_tracked.cells[0].measurement != MeasurementState::MeasuredNoVerdict
            || weak_tracked.cells[0].observations.len() != 1
            || !weak_tracked.cells[0].observations[0].results.is_empty()
            || weak_tracked.cells[0].last_tested.is_none()
        {
            return Err(format!(
                "a comparison with weakened {field} was not named and retained as measured-no-verdict"
            ));
        }
    }

    // ⚠️ THE THIRD STATE: AN `ERROR` THAT LOCATED NOTHING. Three brackets, because
    // the requirement has three halves and a fix that satisfies one by breaking
    // another is the shape this whole guard exists to refuse:
    //   1. a run containing such a row must NOT read all-green;
    //   2. a genuinely clean run must STILL read all-green;
    //   3. a real failure must STILL read red.
    // The summary is derived from these counts by a pure condition, so the counts
    // and the condition together are the testable surface.
    let mut errored = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![bare_cell(&unlocated_id)],
    };
    let errored_fold = apply_validate_results(
        &mut errored,
        &coordinate_less_row(&unlocated_id, "ERROR"),
        "sha-1",
        "tree-1",
        &depth_fixture,
        true,
        true,
    )
    .map_err(|e| {
        format!(
            "a coordinate-less ERROR failed the fold outright: {e}. It must be reported, \
             not turned into an emergency"
        )
    })?;
    refresh_measurement(&mut errored);
    // 1. NOT all-green. This is the exact condition `observe_results` uses.
    if errored_fold.reads_all_green() {
        return Err(
            "a run whose only row was an infrastructure ERROR still reads as an all-green \
             run; an all-green summary is what stops anyone looking"
                .into(),
        );
    }
    if errored_fold.errored.len() != 1 {
        return Err(format!(
            "a coordinate-less ERROR was counted as {} errored; it must be counted as itself",
            errored_fold.errored.len()
        ));
    }
    // ...and NOT as a product result in either direction.
    if errored_fold.located != 0 || errored_fold.unlocated != 0 {
        return Err(format!(
            "an infrastructure ERROR was folded as a divergence: {} located / {} unlocated. \
             Nothing was compared, so there is no product behaviour to record",
            errored_fold.located, errored_fold.unlocated
        ));
    }
    if errored.cells[0].measurement != MeasurementState::MeasuredNoVerdict
        || errored.cells[0].observations.len() != 1
        || !errored.cells[0].observations[0].results.is_empty()
        || errored.cells[0].last_tested.is_none()
    {
        return Err(
            "an infrastructure ERROR was not retained as a measured no-verdict without a product result"
                .into(),
        );
    }
    let mut error_with_coordinates = coordinate_less_row(&unlocated_id, "ERROR");
    let row = &mut error_with_coordinates.get_mut(&unlocated_id).unwrap()[0].row;
    row.first_divergent_scheduler_turn = Some(7);
    row.first_divergent_virtual_nanoseconds = Some(70);
    row.first_divergent_record = Some(12);
    row.first_divergent_syscall = Some(9);
    let mut report: JsonValue =
        serde_json::from_str(row.attempts[0]["verification_report"].as_str().unwrap()).unwrap();
    report["verified"] = JsonValue::Bool(false);
    report["bitwise_parity"] = JsonValue::Bool(false);
    report["verdict"] = JsonValue::String("infrastructure_error".into());
    report["infrastructure_error"] = serde_json::json!({"kind": "skid_overshoot", "count": 1});
    report["first_divergent_scheduler_turn"] = serde_json::json!(7);
    report["first_divergent_virtual_nanoseconds"] = serde_json::json!(70);
    report["first_divergent_record"] = serde_json::json!(12);
    report["first_divergent_syscall"] = serde_json::json!(9);
    let report = serde_json::to_string(&report).unwrap();
    row.attempts[0]["verification_report_sha256"] =
        JsonValue::String(format!("{:x}", Sha256::digest(report.as_bytes())));
    row.attempts[0]["verification_report"] = JsonValue::String(report);
    let mut retained_comparison_error = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![bare_cell(&unlocated_id)],
    };
    let retained_comparison_error_fold = apply_validate_results(
        &mut retained_comparison_error,
        &error_with_coordinates,
        "sha-1",
        "tree-1",
        &depth_fixture,
        true,
        true,
    )
    .map_err(|e| format!("an infrastructure ERROR with retained coordinates was refused: {e}"))?;
    if retained_comparison_error_fold.errored.len() != 1
        || !retained_comparison_error_fold.errored[0].contains("HERMIT_SKID_OVERSHOOT")
        || retained_comparison_error.cells[0].observations.len() != 1
        || !retained_comparison_error.cells[0].observations[0]
            .results
            .is_empty()
    {
        return Err(format!(
            "an infrastructure ERROR with retained comparison evidence was not named and retained without a product result: {:?}",
            retained_comparison_error_fold.errored
        ));
    }
    // 2. A genuinely clean run STILL reads all-green. The inverse defect -- making
    // every quiet run look suspicious -- is the one that cost real time on a false
    // main-red, so it is bracketed rather than assumed.
    if !passed_fold.reads_all_green() {
        return Err(
            "a genuinely clean run stopped reading as all-green; manufacturing emergencies \
             is the inverse defect, not a safer one"
                .into(),
        );
    }
    // 3. A real failure STILL reads red: the located-FAIL fold is unchanged, and a
    // diverged cell must not be quietly demoted into the new third state.
    if !unlocated_fold.errored.is_empty() {
        return Err(format!(
            "a FAIL that located nothing was miscounted as {} errored; a comparator gap and \
             an infrastructure failure need opposite follow-ups",
            unlocated_fold.errored.len()
        ));
    }
    if errored.cells[0].measurement == unlocated.cells[0].measurement {
        return Err(
            "a cell whose run ERRORED reads the same as one that was compared and diverged \
             without a locatable position"
                .into(),
        );
    }

    // ⚠️ THE CLASS, NOT THE INSTANCE. Recognising the third state by the single
    // string "ERROR" left every other non-PASS, non-FAIL outcome falling through to
    // the silent skip -- the exact behaviour this change exists to remove. Found by
    // agent(hermit-dbg) on review, who measured TIMEOUT, NO_RESULT, SKIP and the
    // lowercase spellings all folding to reads_all_green=true.
    //
    // NO_RESULT is bracketed specifically because it is NOT hypothetical in this
    // tree and because it NAMES the very state at issue: nothing was compared. If
    // this bracket ever passes for the wrong reason, prefer adding outcomes to it
    // over narrowing the branch it guards.
    for outcome in ["NO_RESULT", "TIMEOUT", "SKIP"] {
        let mut other = TrackedCells {
            schema: SCHEMA,
            projection: None,
            cells: vec![bare_cell(&unlocated_id)],
        };
        let other_fold = apply_validate_results(
            &mut other,
            &coordinate_less_row(&unlocated_id, outcome),
            "sha-1",
            "tree-1",
            &depth_fixture,
            true,
            true,
        )
        .map_err(|e| format!("a coordinate-less {outcome} failed the fold outright: {e}"))?;
        if other_fold.reads_all_green() {
            return Err(format!(
                "a run whose only row was a coordinate-less {outcome} reads as an all-green \
                 run; the third state must be recognised by CLASS, not by one outcome string"
            ));
        }
        if other_fold.errored.len() != 1 || !other_fold.errored[0].contains(outcome) {
            return Err(format!(
                "a coordinate-less {outcome} was not counted and named as determining \
                 nothing: {:?}",
                other_fold.errored
            ));
        }
        refresh_measurement(&mut other);
        if other.cells[0].measurement != MeasurementState::MeasuredNoVerdict
            || other.cells[0].observations.len() != 1
            || !other.cells[0].observations[0].results.is_empty()
            || other.cells[0].last_tested.is_none()
        {
            return Err(format!(
                "a coordinate-less {outcome} was not retained as measured-no-verdict"
            ));
        }
    }

    let next_source = pressure_summary(
        "sha-2",
        "tree-2",
        vec![pressure_row("crash-error", None, None)],
    );
    apply_pressure_summary(
        &mut observed,
        &next_source,
        "sha-2",
        "tree-2",
        &depth_fixture,
    )
    .map_err(|e| format!("new-source pressure-observation bracket failed: {e}"))?;
    // Assert the exact KEY SET rather than a count. Observations are keyed by
    // (detcore_tree, provenance), so three distinct keys are expected here: the
    // tree-1 pressure bounds, the tree-1 VALIDATE bounds folded by the bracket
    // above, and the new tree-2 pressure entry. A bare length check would pass
    // for the wrong reason if two of these ever collapsed while a third split.
    let keys: Vec<(Option<String>, ObservationProvenance)> = observed.cells[0]
        .observations
        .iter()
        .map(|o| (o.detcore_tree.clone(), o.provenance))
        .collect();
    if keys
        != vec![
            (
                Some("tree-1".to_string()),
                ObservationProvenance::PressureTest,
            ),
            (Some("tree-1".to_string()), ObservationProvenance::Validate),
            (
                Some("tree-2".to_string()),
                ObservationProvenance::PressureTest,
            ),
        ]
    {
        return Err(format!(
            "observations are not keyed by (tree, provenance) in sorted order: {keys:?}"
        ));
    }
    let preserved = tracked_from(&regressed, Some(observed.clone()), Some("self-test"), false)?;
    if preserved.cells[0].observations != observed.cells[0].observations {
        return Err("ordinary scorecard derivation changed pressure observations".into());
    }

    let mut dirty = first.clone();
    dirty.source_tree_dirty = true;
    let mut refusal_target = observed.clone();
    if apply_pressure_summary(
        &mut refusal_target,
        &dirty,
        "sha-1",
        "tree-1",
        &depth_fixture,
    )
    .is_ok()
    {
        return Err("dirty pressure observations were accepted".into());
    }
    if apply_pressure_summary(
        &mut refusal_target,
        &first,
        "wrong-sha",
        "tree-1",
        &depth_fixture,
    )
    .is_ok()
        || apply_pressure_summary(
            &mut refusal_target,
            &first,
            "sha-1",
            "wrong-tree",
            &depth_fixture,
        )
        .is_ok()
    {
        return Err("pressure observations with wrong source identity were accepted".into());
    }
    let mut missing_invocation = first.clone();
    missing_invocation.rows[0].invocation = None;
    if apply_pressure_summary(
        &mut refusal_target,
        &missing_invocation,
        "sha-1",
        "tree-1",
        &depth_fixture,
    )
    .is_ok()
    {
        return Err("pressure observation without a literal invocation was accepted".into());
    }
    let infrastructure = pressure_summary(
        "sha-1",
        "tree-1",
        vec![PressureSummaryRow {
            cell: id.clone(),
            repetition: None,
            attempt: 1,
            result: "infrastructure-error".into(),
            verification: None,
            evidence_errors: vec!["fixture missing".into()],
            invocation: None,
        }],
    );
    if apply_pressure_summary(
        &mut refusal_target,
        &infrastructure,
        "sha-1",
        "tree-1",
        &depth_fixture,
    )
    .is_ok()
    {
        return Err("infrastructure failure was stored as product behavior".into());
    }
    let sandbox_denied = pressure_summary(
        "sha-1",
        "tree-1",
        vec![pressure_row("sandbox-denied", None, None)],
    );
    if apply_pressure_summary(
        &mut refusal_target,
        &sandbox_denied,
        "sha-1",
        "tree-1",
        &depth_fixture,
    )
    .is_ok()
    {
        return Err("sandbox-denied pressure output was stored as product behavior".into());
    }
    // ⚠️ THE HEADLINE PROPERTY, OWNER RULING: A BATCH OF N CELLS HAS EXACTLY THE
    // SAME EFFECT AS N SEPARATE SINGLE-CELL RUNS. Cells have nothing to do with
    // each other, so one bad row must affect its own cell and nothing else.
    // Asserted by CONSTRUCTION rather than by inspection: fold one three-row
    // batch containing a poisoned row, fold the same rows as three separate
    // single-row summaries, and require the two tracked files to be EQUAL.
    // Anything that entangles rows -- a whole-fold veto, shared mutable state,
    // order dependence -- makes these diverge.
    let mut poisoned = PressureSummaryRow {
        evidence_errors: vec!["fixture missing".into()],
        ..pressure_row("determinism-failure", Some(11), Some(110))
    };
    poisoned.cell.test = "fixture/poisoned".into();
    let batch_rows = vec![
        pressure_repeat(1, "determinism-failure", Some(20), Some(500)),
        poisoned.clone(),
        pressure_repeat(2, "replay-failure", Some(30), Some(1000)),
    ];
    let mut batched = observed.clone();
    let batch_outcome = apply_pressure_summary(
        &mut batched,
        &pressure_summary("sha-1", "tree-1", batch_rows.clone()),
        "sha-1",
        "tree-1",
        &depth_fixture,
    )
    .map_err(|e| format!("batch-equivalence bracket failed on the batch: {e}"))?;
    let mut singly = observed.clone();
    let mut singly_skipped = 0usize;
    for row in batch_rows {
        match apply_pressure_summary(
            &mut singly,
            &pressure_summary("sha-1", "tree-1", vec![row]),
            "sha-1",
            "tree-1",
            &depth_fixture,
        ) {
            Ok(one) => {
                if !one.skipped.is_empty() {
                    singly_skipped += one.skipped.len();
                }
            }
            // A single-row summary whose only row is poisoned is all-skipped,
            // which is correctly an error. It must still leave the tracked file
            // untouched, which is what the equality below proves.
            Err(_) => singly_skipped += 1,
        }
    }
    if batched.cells != singly.cells {
        return Err(
            "a batch of rows did not have the same effect as the same rows folded singly".into(),
        );
    }
    if batch_outcome.skipped.len() != singly_skipped || batch_outcome.skipped.len() != 1 {
        return Err(format!(
            "batch and single folds disagreed on skipped rows ({} vs {})",
            batch_outcome.skipped.len(),
            singly_skipped
        ));
    }
    if batch_outcome.rows != 2 {
        return Err(format!(
            "the two sound rows were not both admitted (rows={})",
            batch_outcome.rows
        ));
    }
    if batch_outcome.cells != 1 {
        return Err(format!(
            "the skipped cell was counted as merged (cells={})",
            batch_outcome.cells
        ));
    }

    // Every divergence coordinate is guarded by the result class. The record
    // and syscall fields were added after the original turn/nanosecond check;
    // omitting either here would let a PASS carry a contradictory divergence.
    for (label, record, syscall) in [("record", Some(1), None), ("syscall", None, Some(1))] {
        let mut contradictory = pressure_row("pass", None, None);
        let verification = contradictory
            .verification
            .as_mut()
            .expect("pressure fixture carries a verification report");
        verification.first_divergent_record = record;
        verification.first_divergent_syscall = syscall;
        if apply_pressure_summary(
            &mut refusal_target,
            &pressure_summary("sha-1", "tree-1", vec![contradictory]),
            "sha-1",
            "tree-1",
            &depth_fixture,
        )
        .is_ok()
        {
            return Err(format!("a pass carrying a divergent {label} was accepted"));
        }
    }

    // OWNER RULING: A GREEN CELL IS ADMISSIBLE. This bracket used to assert the
    // opposite. The refusal made the producer and consumer contradict each
    // other -- `--repetitions` only repeats GREEN cells -- and it suppressed
    // the most informative thing a pressure test can report: stressing a cell
    // harder than validate did and finding it red.
    let mut green_target = observed.clone();
    green_target.cells[0].status = CellStatus::Green;
    let green_outcome =
        apply_pressure_summary(&mut green_target, &first, "sha-1", "tree-1", &depth_fixture)
            .map_err(|e| format!("green-cell admission bracket failed: {e}"))?;
    if green_outcome.rows != 1 || !green_outcome.skipped.is_empty() {
        return Err("a green cell was not admitted on equal terms with a red one".into());
    }
    if green_target.cells[0].status != CellStatus::Green {
        return Err("folding pressure evidence changed a cell's status".into());
    }

    let native = ResultRow {
        schema: CELL_RESULT_SCHEMA,
        run_id: "fixture-run".into(),
        attempt: 1,
        hermit_sha: "fixture".into(),
        source_tree_dirty: false,
        binary_sha256: Some("b".repeat(64)),
        test_sha256: "c".repeat(64),
        test: "fixture/native".into(),
        category: "fixture".into(),
        lane: "portable".into(),
        mode: "naked".into(),
        backend: None,
        classification: "required".into(),
        outcome: "PASS".into(),
        result: Some(ObservedResult::Pass),
        failure_class: None,
        error_kind: None,
        timeout_seconds: 15,
        execution_cpu_timeout_seconds: Some(10),
        execution_wall_timeout_seconds: Some(15),
        log_level: None,
        effective_args: Vec::new(),
        argv: vec!["fixture".into()],
        guest_argv: vec!["fixture".into()],
        env: BTreeMap::from([("LC_ALL".into(), "C".into())]),
        cwd: "/repo".into(),
        shell_command: "cd /repo && env LC_ALL=C fixture".into(),
        relaxations: Vec::new(),
        first_divergent_scheduler_turn: None,
        first_divergent_virtual_nanoseconds: None,
        first_divergent_record: None,
        first_divergent_syscall: None,
        attempts: vec![serde_json::json!({
            "argv":["fixture"],
            "guest_argv":["fixture"],
            "env":{"LC_ALL":"C"},
            "cwd":"/repo",
            "shell_command":"cd /repo && env LC_ALL=C fixture"
        })],
    };
    if native.id().map(|id| id.backend) != Some("native".into()) {
        return Err("native result identity did not map a null backend to `native`".into());
    }
    let mut malformed = native;
    malformed.mode = "verify".into();
    if malformed.id().is_some() {
        return Err("non-native result without a backend was accepted".into());
    }
    // --- writer-boundary bracket -------------------------------------------
    // The two authorities in this file must not touch each other's fields. A
    // guard that is never exercised is a guard nobody knows is broken, so each
    // direction is asserted to REFUSE, and the legal no-op is asserted to pass.
    let boundary_cell = |observations: Vec<Observation>, status: CellStatus| {
        let mut cell = TrackedCell {
            id: CellId {
                lane: "portable".into(),
                category: "fixture".into(),
                test: "fixture/boundary".into(),
                mode: "verify".into(),
                backend: "ptrace".into(),
            },
            enabled: true,
            status,
            ci_disabled_reason: None,
            not_applicable_reason: None,
            last_tested: None,
            measurement: MeasurementState::NeverMeasured,
            green_removal_reason: None,
            observations,
        };
        // ⚠️ DERIVE IT HERE RATHER THAN HARDCODING, or this helper builds rows
        // that contradict themselves and every boundary case below fails for
        // the wrong reason -- a refusal about `measurement` while the test is
        // asserting something about `status`. Caught exactly that way.
        cell.measurement = derive_measurement(&cell);
        cell
    };
    let sample = Observation {
        detcore_tree: Some("tree".into()),
        event_ids: BTreeSet::new(),
        provenance: ObservationProvenance::PressureTest,
        depth: BTreeMap::new(),
        hermit_shas: BTreeSet::new(),
        results: BTreeSet::new(),
        canonical_comparisons: BTreeSet::new(),
        invocations: BTreeSet::new(),
        first_divergent_scheduler_turn: ObservedPositions::default(),
        first_divergent_virtual_nanoseconds: ObservedPositions::default(),
        first_divergent_record: ObservedPositions::default(),
        first_divergent_syscall: ObservedPositions::default(),
    };
    let mut different_tree = sample.clone();
    different_tree.detcore_tree = Some("other-tree".into());
    let mut unknown_tree = sample.clone();
    unknown_tree.detcore_tree = None;
    let tree_count_fixture = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![boundary_cell(
            vec![sample.clone(), different_tree, unknown_tree],
            CellStatus::Red,
        )],
    };
    if observation_tree_counts(&tree_count_fixture, "tree") != (1, 1) {
        return Err("show conflated unknown and different observation trees".into());
    }
    let with_evidence = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![boundary_cell(vec![sample.clone()], CellStatus::Red)],
    };
    let evidence_dropped = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![boundary_cell(Vec::new(), CellStatus::Red)],
    };
    let ratchet_moved = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![boundary_cell(vec![sample.clone()], CellStatus::Green)],
    };
    if enforce_writer_boundary(&with_evidence, &evidence_dropped, Writer::Update).is_ok() {
        return Err("writer boundary allowed `update` to drop measured observations".into());
    }
    if enforce_writer_boundary(&with_evidence, &ratchet_moved, Writer::Observations).is_ok() {
        return Err("writer boundary allowed an observation writer to move `status`".into());
    }
    if enforce_writer_boundary(&with_evidence, &evidence_dropped, Writer::Observations).is_err() {
        return Err(
            "writer boundary refused an observation writer for changing observations, \
             which is the field it owns"
                .into(),
        );
    }
    // ---- step 5: positions are stored, the range is derived ----
    {
        // The range is a VIEW. Three runs at 93, 97, 94 must derive
        // {93, 97, 3} -- and, unlike a stored bound, the individual positions
        // survive so a later reader can ask whether that is a cluster or a
        // spread.
        let mut positions = ObservedPositions::default();
        for value in [93u64, 97, 94] {
            positions.record(Some(value));
        }
        if positions.range()
            != Some(ObservedRange {
                earliest: 93,
                latest: 97,
                samples: 3,
            })
        {
            return Err("derived range did not reproduce the bounds of its positions".into());
        }
        if positions.positions != vec![93, 97, 94] {
            return Err("individual positions were not retained in run order".into());
        }

        // A run that located nothing contributes no sample. Counting it would
        // inflate the denominator with runs that produced no bound.
        let mut sparse = ObservedPositions::default();
        sparse.record(Some(5));
        sparse.record(None);
        if sparse.range().map(|r| r.samples) != Some(1) {
            return Err("a run that located nothing was counted as a sample".into());
        }

        // Nothing located at all is None, not a zero-width range at zero.
        if ObservedPositions::default().range().is_some() {
            return Err("an empty coordinate derived a range".into());
        }

        // ⚠️ THE LEGACY MIGRATION, AND THE POINT IS WHAT IT REFUSES TO DO.
        // `{earliest 93, latest 94, samples 2}` records that two runs diverged
        // somewhere in [93, 94]; it does NOT record which run was which, and no
        // rule recovers that. The bounds must survive as bounds, and NO
        // positions may be invented from them.
        let legacy: ObservedPositions =
            serde_json::from_str(r#"{"earliest":93,"latest":94,"samples":2}"#)
                .map_err(|e| format!("legacy range failed to migrate: {e}"))?;
        if !legacy.positions.is_empty() {
            return Err(
                "legacy bounds were expanded into fabricated positions; they cannot be \
                 recovered and must not be invented"
                    .into(),
            );
        }
        if legacy.range()
            != Some(ObservedRange {
                earliest: 93,
                latest: 94,
                samples: 2,
            })
        {
            return Err("legacy bounds were lost during migration".into());
        }

        // ⚠️ AND THE SILENT-LOSS CASE THAT `deny_unknown_fields` EXISTS FOR.
        // Without it the legacy object would match the current shape with every
        // field defaulted, and the bounds would vanish -- evidence loss that
        // looks exactly like a cell nobody measured. Asserting the round trip
        // is what pins that.
        let reserialised = serde_json::to_string(&legacy)
            .map_err(|e| format!("cannot reserialise migrated legacy bounds: {e}"))?;
        let round_tripped: ObservedPositions = serde_json::from_str(&reserialised)
            .map_err(|e| format!("migrated legacy bounds do not round trip: {e}"))?;
        if round_tripped != legacy {
            return Err("migrated legacy bounds changed across a round trip".into());
        }

        // Positions and legacy bounds coexisting: widen, and ADD the counts,
        // because the legacy triple stands for runs that really happened.
        let mut mixed: ObservedPositions =
            serde_json::from_str(r#"{"earliest":10,"latest":20,"samples":2}"#)
                .map_err(|e| format!("legacy range failed to migrate: {e}"))?;
        mixed.record(Some(30));
        if mixed.range()
            != Some(ObservedRange {
                earliest: 10,
                latest: 30,
                samples: 3,
            })
        {
            return Err("positions and legacy bounds did not combine correctly".into());
        }
    }

    // ---- step 6: `measurement` is derived, and cannot be written to disagree ----
    //
    // ⚠️ EVERY ONE OF THESE IS A REFUSAL OR A DERIVATION, DEMONSTRATED. A field
    // whose guard is never seen refusing is indistinguishable from a field
    // nobody checks, which is the exact failure this scorecard keeps finding
    // elsewhere.
    let diverged_observation = |located: bool| {
        let mut observation = sample.clone();
        observation
            .results
            .insert(ObservedResult::DeterminismFailure);
        if located {
            observation.first_divergent_record.record(Some(98));
        }
        observation
    };
    let passed_observation = || {
        let mut observation = sample.clone();
        observation.results.insert(ObservedResult::Pass);
        observation
    };
    let crashed_observation = || {
        let mut observation = sample.clone();
        observation.results.insert(ObservedResult::CrashError);
        observation
    };

    // The five states are distinguishable FROM THE ROW, which is the whole point.
    for (label, observations, expected) in [
        (
            "never-measured",
            Vec::new(),
            MeasurementState::NeverMeasured,
        ),
        (
            "measured-and-passed",
            vec![passed_observation()],
            MeasurementState::MeasuredAndPassed,
        ),
        (
            "measured-no-verdict",
            vec![crashed_observation()],
            MeasurementState::MeasuredNoVerdict,
        ),
        (
            "diverged-unlocated",
            vec![diverged_observation(false)],
            MeasurementState::DivergedUnlocated,
        ),
        (
            "diverged",
            vec![diverged_observation(true)],
            MeasurementState::Diverged,
        ),
    ] {
        let cell = boundary_cell(observations, CellStatus::Red);
        if cell.measurement != expected {
            return Err(format!(
                "measurement derivation: expected `{}` for the {label} case, got `{}`",
                expected.as_str(),
                cell.measurement.as_str()
            ));
        }
    }

    // ⚠️ A CRASH IS NOT A DIVERGENCE. Reading a non-verdict as a product failure
    // is how an infrastructure hiccup becomes a false regression, so this is
    // asserted separately rather than left implicit in the table above.
    let crashed = boundary_cell(vec![crashed_observation()], CellStatus::Red);
    if crashed.measurement == MeasurementState::Diverged
        || crashed.measurement == MeasurementState::DivergedUnlocated
    {
        return Err("measurement counted a crash as a divergence".into());
    }

    // THE REFUSAL: a stored value that disagrees with the row's own evidence.
    let mut lying = boundary_cell(vec![diverged_observation(true)], CellStatus::Red);
    lying.measurement = MeasurementState::MeasuredAndPassed;
    let lying_cells = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![lying],
    };
    for writer in [Writer::Update, Writer::Observations] {
        if enforce_writer_boundary(&lying_cells, &lying_cells, writer).is_ok() {
            return Err(
                "writer boundary allowed a `measurement` that contradicts the row's own \
                 observations"
                    .into(),
            );
        }
    }

    // And the honest row still passes, so the check above discriminates rather
    // than refusing everything.
    let honest = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![boundary_cell(
            vec![diverged_observation(true)],
            CellStatus::Red,
        )],
    };
    if enforce_writer_boundary(&honest, &honest, Writer::Observations).is_err() {
        return Err("writer boundary refused a row whose measurement matches its evidence".into());
    }

    if enforce_writer_boundary(&with_evidence, &ratchet_moved, Writer::Update).is_err() {
        return Err(
            "writer boundary refused `update` for changing `status`, which is the field \
             it owns"
                .into(),
        );
    }
    let population_grown = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![
            boundary_cell(vec![sample.clone()], CellStatus::Red),
            TrackedCell {
                id: CellId {
                    lane: "portable".into(),
                    category: "fixture".into(),
                    test: "fixture/second".into(),
                    mode: "verify".into(),
                    backend: "ptrace".into(),
                },
                enabled: true,
                status: CellStatus::Red,
                ci_disabled_reason: None,
                not_applicable_reason: None,
                last_tested: None,
                observations: Vec::new(),
                measurement: MeasurementState::NeverMeasured,
                green_removal_reason: None,
            },
        ],
    };
    if enforce_writer_boundary(&with_evidence, &population_grown, Writer::Observations).is_ok() {
        return Err("writer boundary allowed an observation writer to add a cell".into());
    }

    // --- projection bracket -------------------------------------------------
    // Observations are a DERIVED PROJECTION of the series store as of plan step
    // 8. The failure that demotion invites is specific: read the series, write
    // what it says, and when the series is empty write nothing -- silently
    // erasing the only located divergence coordinates in the file and leaving
    // rows indistinguishable from cells nobody ever measured.
    //
    // ⚠️ ASSERTED BY BEHAVIOUR, NOT BY READING THE TYPE. The guard is called
    // with the same before/after pair the real path builds, so a future edit
    // that reorders the write ahead of the check fails here.
    // ⚠️ THE FIXTURE MUST CARRY A LOCATED POSITION, not merely an observation.
    // The guard counts coordinates, so a sample with empty `ObservedPositions`
    // gives it nothing to protect and every case below passes vacuously.
    let mut located = sample.clone();
    located.first_divergent_scheduler_turn.record(Some(68));
    let with_located = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![boundary_cell(vec![located.clone()], CellStatus::Red)],
    };
    // A series row must CREATE an observation; rewriting only observations
    // already present leaves the exact target cell reading `never-measured`.
    // The fixture deliberately omits `detcore_tree`, matching historical rows,
    // and uses a compressed divergence so `num_runs` has to survive as three
    // coordinate samples rather than one.
    let root = repo_root()?;
    let fixture_hermit_tree = git_head(&root)?;
    let fixture_detcore_tree = git_rev_parse(&root, "HEAD:detcore")?;
    let series_row = |cell: &str,
                      outcome: SeriesOutcome,
                      producer: SeriesProducer,
                      num_runs: u64,
                      detcore_tree: Option<String>,
                      coordinate: Option<u64>|
     -> SeriesRow {
        let mode = cell.rsplit('/').nth(1).unwrap_or_default();
        let (result, failure_class) = match outcome {
            SeriesOutcome::Passed => (Some(ObservedResult::Pass), None),
            SeriesOutcome::Diverged => (
                Some(if mode == "replay" {
                    ObservedResult::ReplayFailure
                } else {
                    ObservedResult::DeterminismFailure
                }),
                Some(FailureClass::ProductFailure),
            ),
            SeriesOutcome::NoResult => (None, Some(FailureClass::NoResult)),
            SeriesOutcome::Timeout => (Some(ObservedResult::Timeout), Some(FailureClass::NoResult)),
            SeriesOutcome::Errored => (
                Some(ObservedResult::CrashError),
                Some(FailureClass::ProductFailure),
            ),
            SeriesOutcome::Skipped => (None, Some(FailureClass::UnderstoodPrerequisiteFailure)),
        };
        SeriesRow {
            source: "fixture-series:1".into(),
            schema: SeriesSchema::V3,
            event_id: format!("fixture-{cell}-{}-{}", outcome.as_str(), producer.as_str()),
            event_type: "series.observation".into(),
            emitted_at: "2026-08-27T05:00:00Z".into(),
            team: "hermit".into(),
            host: "fixture-host".into(),
            producer,
            run_id: "fixture-run".into(),
            series: SeriesPayload {
                cell: cell.into(),
                tree: fixture_hermit_tree.clone(),
                detcore_tree,
                outcome,
                result,
                failure_class,
                no_verdict_evidence: None,
                pressure_evidence: None,
                run_index: 1,
                attempt: None,
                num_runs,
                last_run_index: None,
                main_ancestry: Some(true),
                runtime: None,
                source_tree_dirty: false,
                depth: BTreeMap::from([(
                    "hermit".into(),
                    SourceDepth {
                        commits: 10,
                        first_parent: 9,
                    },
                )]),
                coordinates: coordinate.map(|position| SeriesCoordinates {
                    first_divergent_scheduler_turn: Some(position),
                    ..SeriesCoordinates::default()
                }),
                first_divergent_messages: None,
                machine_shortname: Some("fixture-host".into()),
                kernel_version: Some("7.1.3-fixture".into()),
                host_capabilities: Some(BTreeMap::from([
                    (
                        HostCapability::CpuidFaulting,
                        HostCapabilityVerdict {
                            present: true,
                            evidence: "fixture cpuid probe".into(),
                        },
                    ),
                    (
                        HostCapability::Kvm,
                        HostCapabilityVerdict {
                            present: false,
                            evidence: "fixture kvm probe".into(),
                        },
                    ),
                ])),
            },
        }
    };

    // Native attempt/diversity evidence belongs to the canonical series; the
    // legacy cells.json observation shape cannot represent it. Exclude every
    // schema and outcome uniformly. In particular, admitting only PASS while
    // dropping a diversity failure, timeout, or no-result row would make the
    // legacy projection less honest than the canonical series.
    let mut native_cell = boundary_cell(Vec::new(), CellStatus::Red);
    native_cell.id.mode = "naked".into();
    native_cell.id.backend = "native".into();
    let mut native_projection = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![boundary_cell(Vec::new(), CellStatus::Green), native_cell],
    };
    let native_pass = series_row(
        "fixture/boundary/naked/native",
        SeriesOutcome::Passed,
        SeriesProducer::Validate,
        1,
        None,
        None,
    );
    // Until the next schema carries the inner hashes, the producer's failed
    // outer result is the only retained statement that successful attempts did
    // not meet the declared diversity requirement.
    let native_failed_diversity = series_row(
        "fixture/boundary/naked/native",
        SeriesOutcome::Errored,
        SeriesProducer::Validate,
        1,
        None,
        None,
    );
    let native_timeout = series_row(
        "fixture/boundary/naked/native",
        SeriesOutcome::Timeout,
        SeriesProducer::Validate,
        1,
        None,
        None,
    );
    let native_no_result = series_row(
        "fixture/boundary/naked/native",
        SeriesOutcome::NoResult,
        SeriesProducer::Validate,
        1,
        None,
        None,
    );
    let native_diverged = series_row(
        "fixture/boundary/naked/native",
        SeriesOutcome::Diverged,
        SeriesProducer::Validate,
        1,
        None,
        None,
    );
    let native_skipped = series_row(
        "fixture/boundary/naked/native",
        SeriesOutcome::Skipped,
        SeriesProducer::Validate,
        1,
        None,
        None,
    );
    let ptrace_pass = series_row(
        "fixture/boundary/verify/ptrace",
        SeriesOutcome::Passed,
        SeriesProducer::Validate,
        1,
        Some(fixture_detcore_tree.clone()),
        None,
    );
    let native_outcome = apply_series_rows(
        &root,
        &mut native_projection,
        &[
            native_pass,
            native_failed_diversity,
            native_timeout,
            native_no_result,
            native_diverged,
            native_skipped,
            ptrace_pass,
        ],
        None,
    )?;
    refresh_measurement(&mut native_projection);
    if native_outcome.rows != 1
        || native_outcome.cells != 1
        || native_outcome.runs != 1
        || native_projection.cells.len() != 2
        || native_outcome.skipped.len() != 6
        || !native_outcome.skipped.iter().all(|line| {
            line.contains(
                "naked/native evidence is canonical-series-only and is not projected into legacy cells.json observations",
            )
        })
        || !native_projection.cells[1].observations.is_empty()
        || native_projection.cells[1].last_tested.is_some()
        || native_projection.cells[1].measurement != MeasurementState::NeverMeasured
        || native_projection.cells[1].status != CellStatus::Red
        || native_projection.cells[0].observations.len() != 1
        || native_projection.cells[0].observations[0].results
            != BTreeSet::from([ObservedResult::Pass])
        || native_projection.cells[0].measurement != MeasurementState::MeasuredAndPassed
        || native_projection.cells[0].status != CellStatus::Green
    {
        return Err(format!(
            "native rows were not uniformly excluded while ptrace still projected: {native_outcome:?}"
        ));
    }

    let mut native_only = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![native_projection.cells[1].clone()],
    };
    let native_only_before = serde_json::to_vec(&native_only)
        .map_err(|error| format!("cannot encode native-only atomicity fixture: {error}"))?;
    let native_only_error = apply_series_rows(
        &root,
        &mut native_only,
        &[series_row(
            "fixture/boundary/naked/native",
            SeriesOutcome::Passed,
            SeriesProducer::Validate,
            1,
            None,
            None,
        )],
        None,
    )
    .expect_err("a native-only projection with no legacy evidence was accepted");
    let native_only_after = serde_json::to_vec(&native_only)
        .map_err(|error| format!("cannot re-encode native-only atomicity fixture: {error}"))?;
    if !native_only_error.contains("every one of the 1 readable series row(s) determined nothing")
        || !native_only_error.contains(
            "naked/native evidence is canonical-series-only and is not projected into legacy cells.json observations",
        )
        || native_only_after != native_only_before
    {
        return Err(format!(
            "native-only all-skipped projection was not an atomic refusal: error={native_only_error:?}, unchanged={}",
            native_only_after == native_only_before
        ));
    }

    let mut projected_from_series = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![boundary_cell(Vec::new(), CellStatus::Green)],
    };
    let mut exact_failure = series_row(
        "fixture/boundary/verify/ptrace",
        SeriesOutcome::Diverged,
        SeriesProducer::PressureTest,
        3,
        None,
        Some(68),
    );
    exact_failure.series.result = Some(ObservedResult::ParityFailure);
    let projection_rows = vec![
        exact_failure,
        series_row(
            "fixture/boundary/verify/ptrace",
            SeriesOutcome::Passed,
            SeriesProducer::PressureTest,
            2,
            None,
            None,
        ),
        series_row(
            "fixture/boundary/verify/ptrace",
            SeriesOutcome::NoResult,
            SeriesProducer::Validate,
            4,
            None,
            None,
        ),
        series_row(
            "fixture/boundary/verify/ptrace",
            SeriesOutcome::Errored,
            SeriesProducer::Validate,
            6,
            None,
            None,
        ),
    ];
    let projected = apply_series_rows(&root, &mut projected_from_series, &projection_rows, None)?;
    let projected_cell = &projected_from_series.cells[0];
    if projected.cells != 1
        || projected.rows != 2
        || projected.runs != 5
        || projected.skipped.len() != 2
        || !projected
            .skipped
            .iter()
            .all(|line| line.contains("produced no comparison"))
    {
        return Err(format!(
            "series projection lost compressed run counts or admitted no_result: {projected:?}"
        ));
    }
    if projected_cell.measurement != MeasurementState::NeverMeasured {
        return Err(
            "series projection helper changed measurement before the caller refreshed it".into(),
        );
    }
    let projected_observation = projected_cell
        .observations
        .iter()
        .find(|observation| {
            observation.detcore_tree.is_none()
                && observation.hermit_shas == BTreeSet::from([fixture_hermit_tree.clone()])
                && observation.provenance == ObservationProvenance::PressureTest
        })
        .ok_or(
            "a historical row without detcore_tree did not create a recorded-commit observation",
        )?;
    if projected_observation
        .first_divergent_scheduler_turn
        .positions
        != vec![68, 68, 68]
        || projected_observation.results
            != BTreeSet::from([ObservedResult::Pass, ObservedResult::ParityFailure])
    {
        return Err(
            "series projection did not expand num_runs or preserve the framework's exact result"
                .into(),
        );
    }
    if projected_cell.last_tested.is_some() {
        return Err("a legacy row without a Detcore tree stamped last_tested".into());
    }

    // A projection of older series history must not move `last_tested` behind a
    // newer retained comparison already imported from the current branch.
    let older_hermit_tree = git_rev_parse(&root, "HEAD^")?;
    let older_detcore_tree = git_rev_parse(&root, "HEAD^:detcore")?;
    let current_last_tested = LastTested {
        hermit_sha: fixture_hermit_tree.clone(),
        detcore_tree: fixture_detcore_tree.clone(),
        depth: BTreeMap::from([(
            "hermit".into(),
            SourceDepth {
                commits: 11,
                first_parent: 10,
            },
        )]),
    };
    let mut preserve_newer = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![boundary_cell(Vec::new(), CellStatus::Green)],
    };
    preserve_newer.cells[0].last_tested = Some(current_last_tested.clone());
    let mut older_row = series_row(
        "fixture/boundary/verify/ptrace",
        SeriesOutcome::Passed,
        SeriesProducer::PressureTest,
        1,
        Some(older_detcore_tree),
        None,
    );
    older_row.series.tree = older_hermit_tree;
    apply_series_rows(&root, &mut preserve_newer, &[older_row], None)?;
    if preserve_newer.cells[0].last_tested.as_ref() != Some(&current_last_tested) {
        return Err("older series history replaced a newer imported last_tested record".into());
    }

    // When series and retained history describe the same validate observation,
    // refreshing the series fields must retain the compact canonical receipt.
    let canonical = CanonicalComparison {
        hermit_sha: fixture_hermit_tree.clone(),
        hermit_commits: 10,
        hermit_first_parent: 9,
        run_id: "fixture-canonical-run".into(),
        evidence_sha256: "a".repeat(64),
        result: ObservedResult::Pass,
        left_info_messages: BTreeSet::from([12]),
        right_info_messages: BTreeSet::from([12]),
    };
    let mut imported_observation = sample.clone();
    imported_observation.detcore_tree = Some(fixture_detcore_tree.clone());
    imported_observation.provenance = ObservationProvenance::Validate;
    imported_observation
        .canonical_comparisons
        .insert(canonical.clone());
    let mut preserve_import = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![boundary_cell(vec![imported_observation], CellStatus::Green)],
    };
    let mut current_validate_row = series_row(
        "fixture/boundary/verify/ptrace",
        SeriesOutcome::Passed,
        SeriesProducer::Validate,
        1,
        Some(fixture_detcore_tree.clone()),
        None,
    );
    current_validate_row.run_id = canonical.run_id.clone();
    current_validate_row.event_id = "fixture-stable-mapped-event".into();

    // Faithful retained-history shape: the current corpus's duplicate direct
    // keys come from two DISTINCT legacy invocations inside ONE observation.
    // Their omitted outer-attempt and evidence identities collapse to the same
    // compact run/result key even though the invocation payloads differ.
    let mut legacy_invocation = before_publication
        .cells
        .iter()
        .find(|cell| cell.id == replay_id)
        .and_then(|cell| {
            cell.observations
                .iter()
                .flat_map(|observation| &observation.invocations)
                .find(|invocation| invocation.run_id == replay_row.run_id)
        })
        .cloned()
        .ok_or("combined result fixture retained no direct invocation")?;
    let legacy_run_id = "fixture-legacy-duplicate-invocation";
    legacy_invocation.hermit_sha = fixture_hermit_tree.clone();
    legacy_invocation.run_id = legacy_run_id.into();
    legacy_invocation.attempt = None;
    legacy_invocation.evidence_sha256 = None;
    legacy_invocation.result = Some(ObservedResult::Pass);
    let mut distinct_legacy_invocation = legacy_invocation.clone();
    distinct_legacy_invocation
        .argv
        .push("--distinct-legacy-payload".into());
    distinct_legacy_invocation.shell_command = literal_shell_command(
        &distinct_legacy_invocation.cwd,
        &distinct_legacy_invocation.env,
        &distinct_legacy_invocation.argv,
    );
    let mut legacy_observation = sample.clone();
    legacy_observation.detcore_tree = Some(fixture_detcore_tree.clone());
    legacy_observation.provenance = ObservationProvenance::PressureTest;
    legacy_observation
        .hermit_shas
        .insert(fixture_hermit_tree.clone());
    legacy_observation.results.insert(ObservedResult::Pass);
    legacy_observation.invocations =
        BTreeSet::from([legacy_invocation, distinct_legacy_invocation]);
    let duplicate_history = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![boundary_cell(vec![legacy_observation], CellStatus::Green)],
    };
    validate_observation_identity_namespace(&duplicate_history)?;
    let (legacy_keys, _) = direct_evidence_keys(&duplicate_history)?;
    if legacy_keys.len() != 1 || legacy_keys.values().next() != Some(&2) {
        return Err(format!(
            "legacy invocation fixture did not exercise one duplicate compact key: {legacy_keys:?}"
        ));
    }

    let generated_before_legacy_mapping = read_generated_files(&result_command_root)?;
    let mut claimed_source_row = current_validate_row.clone();
    claimed_source_row.producer = SeriesProducer::PressureTest;
    claimed_source_row.run_id = legacy_run_id.into();
    claimed_source_row.event_id = "fixture-claimed-legacy-duplicate".into();
    let mut unrelated_source_row = claimed_source_row.clone();
    unrelated_source_row.run_id = "fixture-unrelated-source-run".into();
    unrelated_source_row.event_id = "fixture-unrelated-source-event".into();
    let unrelated_representation = direct_representation(
        &duplicate_history,
        &[unrelated_source_row],
        &CurrentResultAttempts::new(),
    )?;
    if !unrelated_representation.represented_event_ids.is_empty()
        || !unrelated_representation.has_unrepresented_direct_evidence
        || read_generated_files(&result_command_root)? != generated_before_legacy_mapping
    {
        return Err(
            "unclaimed duplicate retained evidence changed a generated file or was not preserved as opaque history"
                .into(),
        );
    }
    let claimed_duplicate_error = direct_representation(
        &duplicate_history,
        &[claimed_source_row],
        &CurrentResultAttempts::new(),
    )
    .expect_err("a source event claimed duplicate retained direct evidence");
    if !claimed_duplicate_error.contains("2 records for that exact identity")
        || read_generated_files(&result_command_root)? != generated_before_legacy_mapping
    {
        return Err(format!(
            "claimed duplicate retained-evidence refusal changed a generated file or lost its cause: {claimed_duplicate_error}"
        ));
    }

    let preserved = apply_series_rows(&root, &mut preserve_import, &[current_validate_row], None)?;
    if preserved.represented_rows != 1 || preserved.replaced_observations != 0 {
        return Err(format!(
            "stable source event did not map one-to-one to preserved evidence: {preserved:?}"
        ));
    }
    if !preserve_import.cells[0].observations[0]
        .canonical_comparisons
        .contains(&canonical)
    {
        return Err("series projection erased an imported canonical comparison".into());
    }
    let mut collision_observation = sample.clone();
    collision_observation.detcore_tree = Some(fixture_detcore_tree.clone());
    collision_observation.provenance = ObservationProvenance::Validate;
    collision_observation
        .hermit_shas
        .insert(fixture_hermit_tree.clone());
    collision_observation.results.insert(ObservedResult::Pass);
    let mut collision_comparison = canonical.clone();
    collision_comparison.run_id = "fixture-attempt-collision".into();
    collision_observation
        .canonical_comparisons
        .insert(collision_comparison);
    let mut attempt_collision = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![boundary_cell(
            vec![collision_observation],
            CellStatus::Green,
        )],
    };
    let mut wrong_attempt = series_row(
        "fixture/boundary/verify/ptrace",
        SeriesOutcome::Passed,
        SeriesProducer::Validate,
        1,
        Some(fixture_detcore_tree.clone()),
        None,
    );
    wrong_attempt.run_id = "fixture-attempt-collision".into();
    wrong_attempt.series.attempt = Some(2);
    if !apply_series_rows(&root, &mut attempt_collision, &[wrong_attempt], None)
        .expect_err("same-run different-attempt evidence was accepted")
        .contains("records outer attempt 2")
    {
        return Err("same-run different-attempt refusal lost its exact reason".into());
    }
    refresh_measurement(&mut projected_from_series);
    if projected_from_series.cells[0].measurement != MeasurementState::Diverged {
        return Err("a projected comparison did not change never-measured to diverged".into());
    }

    // Rows that never compared must be named and must create neither an
    // observation nor a last_tested stamp. This is the live failure mode where
    // the harness exits before reaching the guest.
    let mut no_comparison = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![boundary_cell(Vec::new(), CellStatus::Green)],
    };
    let no_result = series_row(
        "fixture/boundary/verify/ptrace",
        SeriesOutcome::NoResult,
        SeriesProducer::Validate,
        1,
        Some(fixture_detcore_tree.clone()),
        None,
    );
    let no_comparison_error = apply_series_rows(&root, &mut no_comparison, &[no_result], None)
        .expect_err("a no_result row was accepted as evidence");
    if !no_comparison_error.contains("fixture/boundary/verify/ptrace")
        || !no_comparison_error.contains("produced no comparison")
        || !no_comparison.cells[0].observations.is_empty()
        || no_comparison.cells[0].last_tested.is_some()
    {
        return Err(format!(
            "no_result was not refused and named without creating evidence: {no_comparison_error}"
        ));
    }

    // Current series rows retain one exact outer attempt and the producer's
    // typed no-verdict disposition. They must project to the same measurement
    // as direct observation: timeout only when the producer recorded a timeout,
    // otherwise no invented result. Both rows remain independently identifiable
    // even though they group into one observation by Detcore tree.
    let no_verdict_evidence = |timed_out: bool| SeriesNoVerdictEvidence {
        evidence_sha256: if timed_out {
            "b".repeat(64)
        } else {
            "c".repeat(64)
        },
        attempts: vec![SeriesAttemptDisposition {
            index: "1".into(),
            kind: SeriesNoVerdictKind::NotRun,
            detail: None,
            attempt_outcome: "ERROR".into(),
            disposition: if timed_out {
                SeriesOutcome::Timeout
            } else {
                SeriesOutcome::NoResult
            },
            error_kind: Some("incomplete-verification-evidence".into()),
            status: (!timed_out).then_some(125),
            signal: timed_out.then_some(15),
            timed_out,
            verification_report_sha256: Some("d".repeat(64)),
        }],
    };
    let mut series_timeout = series_row(
        "fixture/boundary/verify/ptrace",
        SeriesOutcome::NoResult,
        SeriesProducer::Validate,
        1,
        Some(fixture_detcore_tree.clone()),
        None,
    );
    series_timeout.event_id = "fixture-series-timeout-attempt-1".into();
    series_timeout.series.attempt = Some(1);
    series_timeout.series.no_verdict_evidence = Some(no_verdict_evidence(true));
    let mut series_unavailable = series_row(
        "fixture/boundary/verify/ptrace",
        SeriesOutcome::NoResult,
        SeriesProducer::Validate,
        1,
        Some(fixture_detcore_tree.clone()),
        None,
    );
    series_unavailable.event_id = "fixture-series-unavailable-attempt-2".into();
    series_unavailable.series.run_index = 2;
    series_unavailable.series.attempt = Some(2);
    series_unavailable.series.no_verdict_evidence = Some(no_verdict_evidence(false));
    let project_series_fixture =
        |rows: &[SeriesRow]| -> Result<(TrackedCells, ProjectObservationsOutcome), String> {
            let mut tracked = TrackedCells {
                schema: SCHEMA,
                projection: None,
                cells: vec![boundary_cell(Vec::new(), CellStatus::Green)],
            };
            let outcome = apply_series_rows(&root, &mut tracked, rows, None)?;
            refresh_measurement(&mut tracked);
            Ok((tracked, outcome))
        };
    let (projected_timeout, _) = project_series_fixture(&[series_timeout.clone()])?;
    let (direct_timeout, _) = fold_fixture_row(not_run_row.clone())?;
    if projected_timeout.cells[0].measurement != direct_timeout.cells[0].measurement
        || projected_timeout.cells[0].observations[0].results
            != direct_timeout.cells[0].observations[0].results
    {
        return Err("RUN1573-style SaBRe timeout changed between direct and series paths".into());
    }
    let (projected_unavailable, _) = project_series_fixture(&[series_unavailable.clone()])?;
    let mut direct_kvm_unavailable = not_run_row.clone();
    direct_kvm_unavailable.run_id = "fixture-series-kvm-exit-125".into();
    direct_kvm_unavailable.attempts[0]["timed_out"] = JsonValue::Bool(false);
    direct_kvm_unavailable.attempts[0]["status"] = serde_json::json!(125);
    direct_kvm_unavailable.attempts[0]["signal"] = JsonValue::Null;
    let (direct_unavailable, _) = fold_fixture_row(direct_kvm_unavailable)?;
    if projected_unavailable.cells[0].measurement != direct_unavailable.cells[0].measurement
        || projected_unavailable.cells[0].observations[0].results
            != direct_unavailable.cells[0].observations[0].results
    {
        return Err("RUN1573-style KVM unavailable changed between direct and series paths".into());
    }

    let mut series_noncanonical = series_unavailable.clone();
    series_noncanonical.event_id = "fixture-series-noncanonical-attempt-3".into();
    series_noncanonical.series.run_index = 3;
    series_noncanonical.series.attempt = Some(3);
    let disposition = &mut series_noncanonical
        .series
        .no_verdict_evidence
        .as_mut()
        .unwrap()
        .attempts[0];
    disposition.kind = SeriesNoVerdictKind::NoncanonicalMatch;
    disposition.attempt_outcome = "PASS".into();
    disposition.error_kind = None;
    disposition.status = Some(0);
    let (projected_noncanonical, _) = project_series_fixture(&[series_noncanonical])?;
    let mut direct_noncanonical_rows = coordinate_less_row(&unlocated_id, "PASS");
    let direct_noncanonical = &mut direct_noncanonical_rows.get_mut(&unlocated_id).unwrap()[0].row;
    let mut report: JsonValue = serde_json::from_str(
        direct_noncanonical.attempts[0]["verification_report"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    report["comparison"]["display_name"] = serde_json::json!("Stripped");
    replace_embedded_report(direct_noncanonical, report);
    let mut direct_noncanonical_tracked = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![bare_cell(&unlocated_id)],
    };
    apply_validate_results(
        &mut direct_noncanonical_tracked,
        &direct_noncanonical_rows,
        "sha-1",
        "tree-1",
        &depth_fixture,
        true,
        true,
    )?;
    refresh_measurement(&mut direct_noncanonical_tracked);
    if projected_noncanonical.cells[0].measurement
        != direct_noncanonical_tracked.cells[0].measurement
        || projected_noncanonical.cells[0].observations[0].results
            != direct_noncanonical_tracked.cells[0].observations[0].results
    {
        return Err("noncanonical comparison changed between direct and series paths".into());
    }

    let (projected_no_verdict, projected_no_verdict_outcome) =
        project_series_fixture(&[series_timeout.clone(), series_unavailable.clone()])?;
    let no_verdict_observation = &projected_no_verdict.cells[0].observations[0];
    if projected_no_verdict_outcome.rows != 2
        || projected_no_verdict_outcome.no_verdict_rows != 2
        || projected_no_verdict.cells[0].measurement != MeasurementState::MeasuredNoVerdict
        || projected_no_verdict.cells[0].last_tested.is_none()
        || no_verdict_observation.results != BTreeSet::from([ObservedResult::Timeout])
        || no_verdict_observation.event_ids
            != BTreeSet::from([
                series_timeout.event_id.clone(),
                series_unavailable.event_id.clone(),
            ])
    {
        return Err(format!(
            "typed series no-verdict evidence did not preserve timeout/unavailable semantics: {projected_no_verdict_outcome:?}"
        ));
    }

    let mut contradictory_timeout = series_timeout;
    contradictory_timeout
        .series
        .no_verdict_evidence
        .as_mut()
        .unwrap()
        .attempts[0]
        .timed_out = false;
    let mut atomic_series = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![boundary_cell(Vec::new(), CellStatus::Green)],
    };
    let before_contradiction = serde_json::to_vec(&atomic_series)
        .map_err(|error| format!("cannot encode no-verdict atomicity fixture: {error}"))?;
    let contradiction = apply_series_rows(
        &root,
        &mut atomic_series,
        &[series_unavailable, contradictory_timeout],
        None,
    )
    .expect_err("a contradictory series no-verdict row was accepted");
    let after_contradiction = serde_json::to_vec(&atomic_series)
        .map_err(|error| format!("cannot re-encode no-verdict atomicity fixture: {error}"))?;
    if !contradiction.contains("not_run evidence must carry")
        || after_contradiction != before_contradiction
    {
        return Err(format!(
            "series no-verdict contradiction was not an atomic refusal: error={contradiction:?}, unchanged={}",
            after_contradiction == before_contradiction
        ));
    }

    // `test/mode/backend` is exact. A similar test at another mode cannot be
    // folded into this cell merely because its test id matches.
    let mut exact_mapping = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![boundary_cell(Vec::new(), CellStatus::Green)],
    };
    let mut legacy_schema = series_row(
        "fixture/boundary/verify/ptrace",
        SeriesOutcome::Passed,
        SeriesProducer::Validate,
        1,
        Some(fixture_detcore_tree.clone()),
        None,
    );
    legacy_schema.schema = SeriesSchema::V1;
    legacy_schema.series.machine_shortname = None;
    legacy_schema.series.kernel_version = None;
    legacy_schema.series.host_capabilities = None;
    let mut wrong_event_type = legacy_schema.clone();
    wrong_event_type.schema = SeriesSchema::V2;
    wrong_event_type.series.machine_shortname = Some("fixture-host".into());
    wrong_event_type.series.kernel_version = Some("7.1.3-fixture".into());
    wrong_event_type.event_type = "run.result".into();
    let mut wrong_team = wrong_event_type.clone();
    wrong_team.event_type = "series.observation".into();
    wrong_team.team = "reverie".into();
    let mut empty_host = wrong_event_type.clone();
    empty_host.event_type = "series.observation".into();
    empty_host.host.clear();
    let exact_rows = vec![
        series_row(
            "fixture/boundary/verify/ptrace",
            SeriesOutcome::Passed,
            SeriesProducer::HermitRepeat,
            1,
            Some(fixture_detcore_tree.clone()),
            None,
        ),
        series_row(
            "fixture/boundary/replay/ptrace",
            SeriesOutcome::Passed,
            SeriesProducer::Validate,
            1,
            Some(fixture_detcore_tree.clone()),
            None,
        ),
        series_row(
            "fixture/boundary/verify/ptrace",
            SeriesOutcome::Passed,
            SeriesProducer::Validate,
            1,
            Some("f".repeat(40)),
            None,
        ),
        legacy_schema,
        wrong_event_type,
        wrong_team,
        empty_host,
    ];
    let exact = apply_series_rows(&root, &mut exact_mapping, &exact_rows, None)?;
    if exact.rows != 2
        || exact.skipped.len() != 5
        || !exact.skipped[0].contains("no exact test/mode/backend match")
        || exact
            .skipped
            .iter()
            .filter(|line| line.contains("stress-series/v1 does not record"))
            .count()
            != 1
        || !exact
            .skipped
            .iter()
            .any(|line| line.contains("event_type must be series.observation"))
        || !exact
            .skipped
            .iter()
            .any(|line| line.contains("team must be hermit"))
        || !exact
            .skipped
            .iter()
            .any(|line| line.contains("host must be nonempty"))
        || !exact_mapping.cells[0]
            .observations
            .iter()
            .any(|observation| {
                observation.detcore_tree.as_deref() == Some(fixture_detcore_tree.as_str())
                    && observation.provenance == ObservationProvenance::HermitRepeat
            })
        || !exact_mapping.cells[0]
            .observations
            .iter()
            .any(|observation| {
                observation
                    .detcore_tree
                    .as_ref()
                    .is_some_and(|tree| tree == &"f".repeat(40))
                    && observation.provenance == ObservationProvenance::Validate
            })
    {
        return Err(format!(
            "series projection did not preserve exact test/mode/backend identity: {exact:?}"
        ));
    }

    // Projection must be a pure fold of the recorded rows. These are the six
    // historical Hermit commits whose presence in one developer's object store
    // used to admit 2,129 rows that an empty or base-only clone skipped. Replace
    // refs make the exact names resolvable in the `full` fixture without having
    // to retain those unrelated commit objects in this repository forever.
    let ambient_commits = [
        "0ecc03c0fd710c599392429d2b8a2d066365c578",
        "35aae1f65126480d5ff1ec6e4cefbe8ab59bbddd",
        "62a5c8fd48c2c640d4735d96a9c434d4b778993d",
        "d452986e9871c875f1e5c3c2c66cd8ff593467df",
        "dcdf94ac6bd7a6daa36c6f32d72852a4e7214882",
        "e6e60d8b7c9b61791195b16454b5057a93caee71",
    ];
    let object_store_fixture = tempfile::tempdir().map_err(|e| e.to_string())?;
    let empty_store = object_store_fixture.path().join("empty");
    let base_store = object_store_fixture.path().join("base");
    let full_store = object_store_fixture.path().join("full");
    fs::create_dir_all(&empty_store).map_err(|e| e.to_string())?;
    for store in [&base_store, &full_store] {
        fs::create_dir_all(store).map_err(|e| e.to_string())?;
        git_ok(store, &["init", "--quiet"])?;
    }
    fs::create_dir_all(full_store.join("detcore")).map_err(|e| e.to_string())?;
    fs::write(full_store.join("detcore/identity"), "fixture\n").map_err(|e| e.to_string())?;
    git_ok(&full_store, &["add", "detcore/identity"])?;
    git_ok(
        &full_store,
        &[
            "-c",
            "user.name=x",
            "-c",
            "user.email=x",
            "commit",
            "-qm",
            "fixture",
        ],
    )?;
    let full_commit = git_rev_parse(&full_store, "HEAD^{commit}")?;
    for commit in ambient_commits {
        let replace_ref = format!("refs/replace/{commit}");
        git_ok(&full_store, &["update-ref", &replace_ref, &full_commit])?;
    }

    let procfs_cell = "c-programs/procfs-positioned-probe/verify/ptrace";
    let legacy_row = |key: usize, outcome: SeriesOutcome, detcore_tree: Option<String>| {
        let commit = ambient_commits.get(key).unwrap_or(&ambient_commits[0]);
        let mut row = series_row(
            procfs_cell,
            outcome,
            SeriesProducer::Validate,
            1,
            detcore_tree,
            None,
        );
        row.schema = SeriesSchema::V2;
        row.event_id = format!("ambient-event-{key}");
        row.run_id = format!("ambient-run-{key}");
        row.emitted_at = format!("2026-08-27T05:00:{key:02}Z");
        row.series.tree = (*commit).into();
        row.series.result = None;
        row.series.failure_class = None;
        row
    };
    let mut ambient_rows = (0..ambient_commits.len())
        .map(|index| legacy_row(index, SeriesOutcome::Passed, None))
        .collect::<Vec<_>>();
    ambient_rows.extend([
        legacy_row(6, SeriesOutcome::Diverged, None),
        legacy_row(7, SeriesOutcome::Passed, Some(ambient_commits[0].into())),
    ]);

    let source_commit = "a".repeat(40);
    let source_tree = "b".repeat(40);
    let source_identity = "fixture-series";
    let mut prior_observation = sample.clone();
    prior_observation.detcore_tree = Some("d".repeat(40));
    prior_observation.hermit_shas = BTreeSet::from(["e".repeat(40)]);
    prior_observation.results = BTreeSet::from([ObservedResult::Pass]);
    let mut object_store_outputs = Vec::new();
    let mut projected_fixture = None;
    for store in [&empty_store, &base_store, &full_store] {
        let mut target_cell = boundary_cell(vec![prior_observation.clone()], CellStatus::Red);
        target_cell.id.test = "c-programs/procfs-positioned-probe".into();
        let mut target = TrackedCells {
            schema: SCHEMA,
            projection: Some(ObservationProjection {
                source: "fixture-series".into(),
                source_commit: Some(source_commit.clone()),
                source_tree: Some(source_tree.clone()),
                refreshed_at: "fixture-stamp".into(),
                rows_read: ambient_rows.len() as u64,
                pre_series_corpus: false,
            }),
            cells: vec![target_cell],
        };
        let outcome = apply_series_rows(store, &mut target, &ambient_rows, Some(source_identity))?;
        refresh_measurement(&mut target);
        if outcome.rows != ambient_rows.len() || !outcome.skipped.is_empty() {
            return Err("object-store-independent projection dropped recorded rows".into());
        }
        let encoded = encoded_cells(&target)?;
        projected_fixture.get_or_insert_with(|| target.clone());
        object_store_outputs.push(encoded);
    }
    if object_store_outputs
        .windows(2)
        .any(|pair| pair[0].as_bytes() != pair[1].as_bytes())
    {
        return Err("empty/base/full object stores changed projection bytes".into());
    }
    let projected_fixture = projected_fixture.expect("one object-store fixture was projected");
    let cell = &projected_fixture.cells[0];
    if !cell.observations.contains(&prior_observation)
        || cell.measurement != MeasurementState::DivergedUnlocated
        || !ambient_commits.iter().all(|commit| {
            cell.observations.iter().any(|observation| {
                observation.detcore_tree.is_none()
                    && observation.hermit_shas == BTreeSet::from([(*commit).into()])
            })
        })
    {
        return Err("object-store-independent projection lost recorded evidence".into());
    }
    if !cell
        .observations
        .iter()
        .any(|observation| observation.detcore_tree.as_deref() == Some(ambient_commits[0]))
        || cell.observations.len() != ambient_commits.len() + 2
    {
        return Err("explicit Detcore identity aliased a recorded Hermit identity".into());
    }
    let mut repeated = projected_fixture.clone();
    let repeated_outcome = apply_series_rows(
        &empty_store,
        &mut repeated,
        &ambient_rows,
        Some(source_identity),
    )?;
    refresh_measurement(&mut repeated);
    if repeated_outcome.replaced_observations != ambient_commits.len() + 1
        || encoded_cells(&repeated)? != object_store_outputs[0]
    {
        return Err("projecting the same source twice was not byte-idempotent".into());
    }
    let mut foreign_event = projected_fixture.clone();
    foreign_event.cells[0]
        .observations
        .iter_mut()
        .find(|observation| !observation.event_ids.is_empty())
        .expect("projected observation exists")
        .event_ids
        .insert("foreign-series-event".into());
    if !apply_series_rows(
        &empty_store,
        &mut foreign_event,
        &ambient_rows,
        Some(source_identity),
    )
    .expect_err("foreign projector-owned event_id was replaced")
    .contains("absent from source")
    {
        return Err("foreign event_id refusal lost its exact reason".into());
    }
    let mut missing_event_ids = projected_fixture.clone();
    missing_event_ids.cells[0]
        .observations
        .iter_mut()
        .find(|observation| observation.detcore_tree.is_none())
        .expect("legacy projected observation exists")
        .event_ids
        .clear();
    if encoded_cells(&missing_event_ids).is_ok() {
        return Err("legacy projected observation without event_ids was accepted".into());
    }
    let mut duplicate_event_ids: JsonValue = serde_json::from_str(&object_store_outputs[0])
        .map_err(|e| format!("cannot decode projected fixture for duplicate-ID test: {e}"))?;
    let event_ids = duplicate_event_ids["cells"][0]["observations"]
        .as_array_mut()
        .and_then(|observations| {
            observations.iter_mut().find_map(|observation| {
                let event_ids = observation.get_mut("event_ids")?.as_array_mut()?;
                (!event_ids.is_empty()).then_some(event_ids)
            })
        })
        .expect("projected JSON observation has event_ids");
    let duplicate_event_id = event_ids[0].clone();
    event_ids.push(duplicate_event_id);
    if serde_json::from_value::<TrackedCells>(duplicate_event_ids).is_ok() {
        return Err("duplicate projected observation event_id was accepted".into());
    }
    for (rendered, typed) in [
        ("sandbox-denied", ObservedResult::SandboxDenied),
        ("infrastructure-error", ObservedResult::InfrastructureError),
    ] {
        let mut direct_json: JsonValue = serde_json::from_str(&object_store_outputs[0])
            .map_err(|e| format!("cannot decode projected fixture for result refusal: {e}"))?;
        direct_json["cells"][0]["observations"][0]["results"] = serde_json::json!([rendered]);
        let direct_error = serde_json::from_value::<TrackedCells>(direct_json)
            .expect_err("direct tracked serde accepted a non-product observation result")
            .to_string();
        if !direct_error.contains(rendered) {
            return Err(format!(
                "direct tracked serde refusal did not name {rendered}: {direct_error}"
            ));
        }

        let mut typed_cells = projected_fixture.clone();
        typed_cells.cells[0].observations[0].results = BTreeSet::from([typed]);
        let encode_error = encoded_cells(&typed_cells)
            .expect_err("scorecard writer accepted a non-product observation result");
        if !encode_error.contains(rendered) {
            return Err(format!(
                "scorecard writer refusal did not name {rendered}: {encode_error}"
            ));
        }
    }

    // The real erasure mode: the observation SURVIVES and its coordinates are
    // blanked. A guard comparing observation counts cannot see this, which is
    // how the first version of it passed the unit case and then walked straight
    // through an end-to-end run against the checked-in file.
    let coordinates_blanked = TrackedCells {
        schema: SCHEMA,
        projection: None,
        cells: vec![boundary_cell(vec![sample.clone()], CellStatus::Red)],
    };
    match enforce_projection_preserves_evidence(&with_located, &coordinates_blanked, 0) {
        Ok(()) => {
            return Err(
                "projection blanked located coordinates while keeping the observation, and the                  guard did not see it. This is the erasure mode the naive implementation                  actually produces"
                    .into(),
            );
        }
        Err(why) => {
            if !why.contains("fixture/boundary") {
                return Err(format!(
                    "coordinate-blanking refusal does not name the cell: {why}"
                ));
            }
        }
    }
    match enforce_projection_preserves_evidence(&with_located, &evidence_dropped, 0) {
        Ok(()) => {
            return Err(
                "projection dropped measured evidence after reading ZERO series rows. An empty \
                 source is not a finding that a cell has no evidence"
                    .into(),
            );
        }
        Err(why) => {
            // The refusal must NAME the cell it protected. "projection refused"
            // with no subject sends the reader back to diff the whole file.
            if !why.contains("fixture/boundary") {
                return Err(format!(
                    "projection refusal does not name the cell whose evidence it protected: {why}"
                ));
            }
        }
    }
    // Rows actually read means the source spoke, so replacement is legitimate --
    // otherwise the guard would freeze the projection permanently and step 4
    // could never correct a stale bound.
    if enforce_projection_preserves_evidence(&with_located, &evidence_dropped, 1).is_err() {
        return Err(
            "projection guard refused a replacement the series actually supplied rows for; it \
             would freeze the projection against every future correction"
                .into(),
        );
    }
    // Adding evidence is never the erasure case, whatever the row count.
    if enforce_projection_preserves_evidence(&evidence_dropped, &with_located, 0).is_err() {
        return Err("projection guard refused a refresh that ADDED evidence".into());
    }
    // `update` is the ratchet authority and must leave the projection block
    // alone. Asserted in BOTH directions: the boundary refuses the drop, and
    // the derivation actually carries it forward -- a refusal alone would just
    // make every ordinary `update` fail.
    let stamped = TrackedCells {
        schema: SCHEMA,
        projection: Some(ObservationProjection {
            source: "fixture-series".into(),
            source_commit: Some("a".repeat(40)),
            source_tree: Some("b".repeat(40)),
            refreshed_at: "fixture-stamp".into(),
            rows_read: 7,
            pre_series_corpus: false,
        }),
        cells: vec![boundary_cell(vec![sample.clone()], CellStatus::Red)],
    };
    let projection_dropped = TrackedCells {
        projection: None,
        ..stamped.clone()
    };
    if enforce_writer_boundary(&stamped, &projection_dropped, Writer::Update).is_ok() {
        return Err(
            "writer boundary allowed `update` to delete the projection block, which is what              turns a stale projection into one nobody can tell from a fresh one"
                .into(),
        );
    }
    if enforce_writer_boundary(&stamped, &stamped, Writer::Update).is_err() {
        return Err("writer boundary refused an `update` that preserved the projection".into());
    }
    // ⚠️ AND THE DERIVATION ITSELF, not only the guard that polices it. The
    // boundary check above passes even when `tracked_from` rebuilds the file
    // with `projection: None`, because it is never handed the two versions to
    // compare unless a projection already exists in the checked-in file -- and
    // none does until plan step 4 lands. So the reintroduced bug would sit
    // undetected in CI for as long as the series stays empty. This calls the
    // derivation directly, which is the only version of this assertion that can
    // fail today.
    let boundary_id = stamped.cells[0].id.clone();
    let derived_fixture = Derived {
        population: BTreeSet::from([boundary_id.clone()]),
        enabled: BTreeSet::from([boundary_id.clone()]),
        ci_disabled_reasons: BTreeMap::new(),
        not_applicable_reasons: BTreeMap::new(),
        selected: BTreeSet::from([boundary_id.clone()]),
        green: BTreeSet::new(),
        selected_custom: BTreeSet::new(),
    };
    let rederived = tracked_from(&derived_fixture, Some(stamped.clone()), None, false)?;
    if rederived.projection != stamped.projection {
        return Err(format!(
            "`update` did not carry the projection block forward: {:?} became {:?}. Every              ordinary derivation would then delete the record of when the observations were              last projected",
            stamped.projection, rederived.projection
        ));
    }
    // An unreachable source and an empty one are different facts. Only the
    // second is a statement about the cells, so the first is refused outright
    // rather than folded into "zero rows".
    if snapshot_series_source(Path::new("/nonexistent/series/root")).is_ok() {
        return Err("an unreachable series root was read as an empty series".into());
    }

    // The projection source is ONE immutable snapshot: resolve a commit,
    // capture its bytes, prove the worktree matches, and never reread the
    // mutable worktree while parsing. The fixture also pins canonical event
    // identity, loud malformed-input refusal, and exact shard population.
    let source_fixture =
        tempfile::tempdir().map_err(|e| format!("cannot create series snapshot fixture: {e}"))?;
    let source_repo = source_fixture.path();
    git_ok(source_repo, &["init", "--quiet"])?;
    let source_dir = source_repo.join("series/hermit/fixture");
    fs::create_dir_all(&source_dir)
        .map_err(|e| format!("cannot create series snapshot fixture: {e}"))?;
    let first_shard = source_dir.join("2026-08-a.jsonl");
    let second_shard = source_dir.join("2026-08-b.jsonl");
    let mut source_row = series_row(
        "fixture/boundary/verify/ptrace",
        SeriesOutcome::Passed,
        SeriesProducer::Validate,
        1,
        Some(fixture_detcore_tree.clone()),
        None,
    );
    source_row.event_id = "fixture-source-event".into();
    source_row.source.clear();
    let source_json = serde_json::to_string(&source_row)
        .map_err(|e| format!("cannot encode series snapshot fixture: {e}"))?;
    fs::write(&first_shard, format!("{source_json}\n"))
        .map_err(|e| format!("cannot write first series snapshot shard: {e}"))?;
    fs::write(&second_shard, format!("  {source_json}  \n"))
        .map_err(|e| format!("cannot write duplicate series snapshot shard: {e}"))?;
    let commit_source = |message: &str| -> Result<(), String> {
        git_ok(source_repo, &["add", "series"])?;
        git_ok(
            source_repo,
            &[
                "-c",
                "user.name=scorecard fixture",
                "-c",
                "user.email=scorecard@example.invalid",
                "commit",
                "--quiet",
                "-m",
                message,
            ],
        )
    };
    commit_source("identical duplicate rows")?;

    let source_snapshot = snapshot_series_source(&source_repo.join("series"))?;
    let expected_source_commit = git_rev_parse(source_repo, "HEAD^{commit}")?;
    let expected_source_tree =
        git_rev_parse(source_repo, &format!("{expected_source_commit}:series"))?;
    if source_snapshot.source_commit != expected_source_commit {
        return Err(format!(
            "series snapshot recorded {} instead of resolved commit {expected_source_commit}",
            source_snapshot.source_commit
        ));
    }
    if source_snapshot.source != "series" || source_snapshot.source_tree != expected_source_tree {
        return Err(format!(
            "series snapshot recorded source {:?} at tree {} instead of repository-relative series at {expected_source_tree}",
            source_snapshot.source, source_snapshot.source_tree
        ));
    }

    let missing_tree = source_repo.join("empty-untracked-series");
    fs::create_dir(&missing_tree)
        .map_err(|e| format!("cannot create missing-tree fixture: {e}"))?;
    let missing_tree_error = snapshot_series_source(&missing_tree)
        .expect_err("an uncommitted empty source directory acquired a Git tree identity");
    if !missing_tree_error.contains("rev-parse") {
        return Err(format!(
            "missing-tree refusal did not name the failed Git lookup: {missing_tree_error}"
        ));
    }

    // Equivalent caller spellings and symlinks must all reduce to the same
    // repository-relative path and immutable tree object.
    let direct_hermit = source_repo.join("hermit");
    let standalone_hermit = source_repo.join("worktrees/standalone");
    fs::create_dir_all(&direct_hermit)
        .map_err(|e| format!("cannot create direct Hermit path fixture: {e}"))?;
    fs::create_dir_all(&standalone_hermit)
        .map_err(|e| format!("cannot create standalone Hermit path fixture: {e}"))?;
    let series_link = source_repo.join("series-link");
    std::os::unix::fs::symlink(source_repo.join("series"), &series_link)
        .map_err(|e| format!("cannot create series symlink fixture: {e}"))?;
    for spelling in [
        direct_hermit.join("../series"),
        standalone_hermit.join("../../series"),
        series_link,
    ] {
        let equivalent = snapshot_series_source(&spelling)?;
        if equivalent.source != "series"
            || equivalent.source_commit != expected_source_commit
            || equivalent.source_tree != expected_source_tree
        {
            return Err(format!(
                "equivalent series source {} recorded {:?} at commit {} tree {}",
                spelling.display(),
                equivalent.source,
                equivalent.source_commit,
                equivalent.source_tree
            ));
        }
    }

    let alternate_dir = source_repo.join("alternate-series/hermit/fixture");
    fs::create_dir_all(&alternate_dir)
        .map_err(|e| format!("cannot create alternate series fixture: {e}"))?;
    let mut alternate_row = source_row.clone();
    alternate_row.event_id = "fixture-alternate-event".into();
    fs::write(
        alternate_dir.join("2026-08.jsonl"),
        format!(
            "{}\n",
            serde_json::to_string(&alternate_row)
                .map_err(|e| format!("cannot encode alternate series row: {e}"))?
        ),
    )
    .map_err(|e| format!("cannot write alternate series shard: {e}"))?;
    git_ok(source_repo, &["add", "alternate-series"])?;
    git_ok(
        source_repo,
        &[
            "-c",
            "user.name=scorecard fixture",
            "-c",
            "user.email=scorecard@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "alternate series",
        ],
    )?;
    let alternate_snapshot = snapshot_series_source(&source_repo.join("alternate-series"))?;
    if alternate_snapshot.source != "alternate-series"
        || alternate_snapshot.source_tree == expected_source_tree
    {
        return Err(format!(
            "alternate tracked source was not distinguished: source={:?} tree={}",
            alternate_snapshot.source, alternate_snapshot.source_tree
        ));
    }

    git_ok(
        source_repo,
        &[
            "--no-replace-objects",
            "reset",
            "--hard",
            "--quiet",
            &expected_source_commit,
        ],
    )?;

    let mut replacement_row = source_row.clone();
    replacement_row.event_id = "fixture-replacement-event".into();
    let replacement_json = serde_json::to_string(&replacement_row)
        .map_err(|e| format!("cannot encode replacement-ref series row: {e}"))?;
    fs::write(&first_shard, format!("{replacement_json}\n"))
        .map_err(|e| format!("cannot write replacement-ref series shard: {e}"))?;
    fs::write(&second_shard, format!("{replacement_json}\n"))
        .map_err(|e| format!("cannot write replacement-ref duplicate shard: {e}"))?;
    commit_source("replacement tree")?;
    let replacement_commit = git_rev_parse(source_repo, "HEAD^{commit}")?;
    git_ok(
        source_repo,
        &[
            "--no-replace-objects",
            "reset",
            "--hard",
            "--quiet",
            &expected_source_commit,
        ],
    )?;
    git_ok(
        source_repo,
        &["replace", &expected_source_commit, &replacement_commit],
    )?;
    let replacement_guarded = snapshot_series_source(&source_repo.join("series"))?;
    let replacement_guarded_rows = read_series_rows(&replacement_guarded)?;
    if replacement_guarded.source_commit != expected_source_commit
        || replacement_guarded.source_tree != expected_source_tree
        || replacement_guarded_rows.len() != 1
        || replacement_guarded_rows[0].event_id != source_row.event_id
    {
        return Err(
            "series snapshot followed a Git replacement ref instead of the recorded commit".into(),
        );
    }
    let replacement_ref = format!("refs/replace/{expected_source_commit}");
    git_ok(source_repo, &["update-ref", "-d", &replacement_ref])?;

    fs::write(&first_shard, "{worktree mutated after snapshot}\n")
        .map_err(|e| format!("cannot mutate snapshotted series shard: {e}"))?;
    let captured_rows = read_series_rows(&source_snapshot)?;
    if captured_rows.len() != 1 || captured_rows[0].event_id != source_row.event_id {
        return Err(format!(
            "immutable snapshot did not collapse identical event IDs to one captured row: {:?}",
            captured_rows
                .iter()
                .map(|row| row.event_id.as_str())
                .collect::<Vec<_>>()
        ));
    }
    let dirty_error = snapshot_series_source(&source_repo.join("series"))
        .expect_err("a changed worktree shard was accepted as its committed snapshot");
    if !dirty_error.contains("differs from the committed snapshot")
        || !dirty_error.contains("2026-08-a.jsonl")
    {
        return Err(format!(
            "changed-shard refusal did not name the mismatch and shard: {dirty_error}"
        ));
    }

    let mut conflicting_row = source_row.clone();
    conflicting_row.emitted_at = "2026-08-27T05:00:01Z".into();
    let conflicting_json = serde_json::to_string(&conflicting_row)
        .map_err(|e| format!("cannot encode conflicting series row: {e}"))?;
    fs::write(&first_shard, format!("{source_json}\n"))
        .map_err(|e| format!("cannot restore first series snapshot shard: {e}"))?;
    fs::write(&second_shard, format!("{conflicting_json}\n"))
        .map_err(|e| format!("cannot write conflicting series snapshot shard: {e}"))?;
    commit_source("conflicting duplicate rows")?;
    let conflicting_snapshot = snapshot_series_source(&source_repo.join("series"))?;
    let conflicting_error = read_series_rows(&conflicting_snapshot)
        .expect_err("conflicting rows under one event_id were collapsed");
    if !conflicting_error.contains("conflicting series rows")
        || !conflicting_error.contains("fixture-source-event")
        || !conflicting_error.contains("2026-08-a.jsonl:1")
        || !conflicting_error.contains("2026-08-b.jsonl:1")
    {
        return Err(format!(
            "conflicting event_id refusal did not name both rows: {conflicting_error}"
        ));
    }

    fs::write(&second_shard, "{not valid json}\n")
        .map_err(|e| format!("cannot write malformed series snapshot shard: {e}"))?;
    commit_source("malformed row")?;
    let malformed_snapshot = snapshot_series_source(&source_repo.join("series"))?;
    let malformed_error = read_series_rows(&malformed_snapshot)
        .expect_err("a malformed committed series row was skipped");
    if !malformed_error.contains("malformed series row")
        || !malformed_error.contains("2026-08-b.jsonl:1")
    {
        return Err(format!(
            "malformed-row refusal did not identify its source: {malformed_error}"
        ));
    }

    fs::write(&second_shard, &source_json)
        .map_err(|e| format!("cannot write truncated series snapshot shard: {e}"))?;
    commit_source("truncated row")?;
    let truncated_snapshot = snapshot_series_source(&source_repo.join("series"))?;
    let truncated_error = read_series_rows(&truncated_snapshot)
        .expect_err("a nonempty shard without a trailing newline was accepted");
    if !truncated_error.contains("must end in a newline")
        || !truncated_error.contains("2026-08-b.jsonl")
    {
        return Err(format!(
            "truncated-shard refusal did not identify its source: {truncated_error}"
        ));
    }

    let mut invalid_row = source_row.clone();
    invalid_row.series.kernel_version = None;
    let invalid_json = serde_json::to_string(&invalid_row)
        .map_err(|e| format!("cannot encode invalid series row: {e}"))?;
    fs::write(&second_shard, format!("{invalid_json}\n"))
        .map_err(|e| format!("cannot write invalid series snapshot shard: {e}"))?;
    commit_source("read-invalid row")?;
    let invalid_snapshot = snapshot_series_source(&source_repo.join("series"))?;
    let invalid_error = read_series_rows(&invalid_snapshot)
        .expect_err("a row rejected by the shared read boundary was admitted");
    if !invalid_error.contains("invalid series row")
        || !invalid_error.contains("2026-08-b.jsonl:1")
        || !invalid_error.contains("missing kernel_version")
    {
        return Err(format!(
            "read-invalid row refusal did not identify its source and reason: {invalid_error}"
        ));
    }

    fs::write(&second_shard, format!("{source_json}\n"))
        .map_err(|e| format!("cannot restore duplicate series snapshot shard: {e}"))?;
    commit_source("restore canonical rows")?;
    let untracked_shard = source_dir.join("untracked.jsonl");
    fs::write(&untracked_shard, format!("{source_json}\n"))
        .map_err(|e| format!("cannot write untracked series snapshot shard: {e}"))?;
    let untracked_error = snapshot_series_source(&source_repo.join("series"))
        .expect_err("an untracked JSONL shard entered the source population");
    if !untracked_error.contains("worktree-only JSONL shard")
        || !untracked_error.contains("untracked.jsonl")
    {
        return Err(format!(
            "untracked-shard refusal did not identify the population mismatch: {untracked_error}"
        ));
    }

    // A legacy file with no `projection` key must still load -- the demotion is
    // additive, and a hard requirement would strand every checked-in scorecard.
    let legacy: TrackedCells = serde_json::from_str(r#"{"schema":6,"cells":[]}"#)
        .map_err(|e| format!("legacy scorecard without a projection block no longer loads: {e}"))?;
    if legacy.projection.is_some() {
        return Err("absent projection block deserialized as present".into());
    }
    let historical_projection: ObservationProjection = serde_json::from_str(
        r#"{"source":"s","refreshed_at":"t","rows_read":3,"pre_series_corpus":false}"#,
    )
    .map_err(|e| format!("historical projection without source_commit no longer loads: {e}"))?;
    if historical_projection.source_commit.is_some() || historical_projection.source_tree.is_some()
    {
        return Err("historical projection invented an immutable source identity".into());
    }
    let historical_commit = "a".repeat(40);
    let pre_tree_projection: ObservationProjection = serde_json::from_str(&format!(
        r#"{{"source":"../../series","source_commit":"{}","refreshed_at":"t","rows_read":3,"pre_series_corpus":false}}"#,
        historical_commit
    ))
    .map_err(|e| format!("projection predating source_tree no longer loads: {e}"))?;
    if pre_tree_projection.source_commit.as_deref() != Some(historical_commit.as_str())
        || pre_tree_projection.source_tree.is_some()
    {
        return Err("projection predating source_tree changed its recorded identity".into());
    }
    let mut namespaced_identity = stamped.clone();
    namespaced_identity.cells[0].observations[0].detcore_tree = None;
    namespaced_identity.cells[0].observations[0].event_ids =
        BTreeSet::from(["fixture-series-event".into()]);
    namespaced_identity.cells[0].observations[0].hermit_shas = BTreeSet::from(["c".repeat(40)]);
    encoded_cells(&namespaced_identity)
        .map_err(|e| format!("schema-7 projection rejected its recorded identity: {e}"))?;
    let mut unversioned_identity = namespaced_identity.clone();
    unversioned_identity.schema = 6;
    if encoded_cells(&unversioned_identity).is_ok() {
        return Err("schema-6 writer accepted an observation without a Detcore tree".into());
    }
    let mut malformed_identity = namespaced_identity;
    malformed_identity.cells[0].observations[0].hermit_shas =
        BTreeSet::from(["not-an-object-id".into()]);
    if encoded_cells(&malformed_identity).is_ok() {
        return Err("projection schema accepted a malformed recorded Hermit identity".into());
    }
    if serde_json::from_str::<ObservationProjection>(
        r#"{"source":"s","source_commit":"not-a-commit","refreshed_at":"t","rows_read":3,"pre_series_corpus":false}"#,
    )
    .is_ok()
    {
        return Err("ObservationProjection accepted a malformed source_commit".into());
    }
    if serde_json::from_str::<ObservationProjection>(
        &format!(
            r#"{{"source":"series","source_commit":"{}","source_tree":"not-a-tree","refreshed_at":"t","rows_read":3,"pre_series_corpus":false}}"#,
            "a".repeat(40)
        ),
    )
    .is_ok()
    {
        return Err("ObservationProjection accepted a malformed source_tree".into());
    }
    // ⚠️ THE SAME LOAD-BEARING REFUSAL AS THE OBSERVED-POSITIONS SCHEMA. Without
    // `deny_unknown_fields` a projection block carrying a misspelled or future
    // key matches with every field defaulted: rows_read 0, pre_series_corpus
    // false, refreshed_at empty -- a projection that claims to be current while
    // having read nothing. That is tonight's defect class living in the schema.
    if serde_json::from_str::<ObservationProjection>(
        r#"{"source":"s","refreshed_at":"t","rows_read":3,"row_count":9}"#,
    )
    .is_ok()
    {
        return Err(
            "ObservationProjection accepted an unknown field; a typo would silently default \
             rows_read to 0 and read as a projection nobody can distinguish from a fresh one"
                .into(),
        );
    }

    // A result-less invocation is valid only when it names the exact outer
    // attempt and complete result row that produced the measured no-verdict.
    // Otherwise making `result` optional would also make a damaged historical
    // invocation silently indistinguishable from intentional no-verdict data.
    let mut missing_no_verdict_identity = passed.clone();
    let invocation = missing_no_verdict_identity.cells[0].observations[0]
        .invocations
        .iter()
        .next()
        .cloned()
        .ok_or("PASS fixture has no invocation")?;
    missing_no_verdict_identity.cells[0].observations[0]
        .invocations
        .clear();
    missing_no_verdict_identity.cells[0].observations[0]
        .invocations
        .insert(ObservedInvocation {
            result: None,
            ..invocation
        });
    if !validate_observation_identity_namespace(&missing_no_verdict_identity)
        .expect_err("a result-less invocation without row identity was accepted")
        .contains("without exact outer-attempt evidence identity")
    {
        return Err("result-less invocation refusal did not name its missing identity".into());
    }

    // ── PATH-INDEPENDENCE BRACKET ──────────────────────────────────────────
    // The encoded artifact must not name the worktree that produced it, and it
    // must not name the worktree ENCODING it either. Both legs are required:
    // the second is the one that hid, because `update` carries tracked rows
    // forward without re-ingesting, so a foreign worktree's path survives every
    // regeneration until something rewrites it on the way out.
    //
    // NEGATIVE LEG FIRST, so this cannot become a guard that never fires: the
    // fixture below is built WITH a foreign absolute root, and the assertions
    // fail if that exact root or one of its descendants survives. A sibling
    // path and an interior substring are negative controls: rewriting either
    // would alias evidence from a different location.
    let foreign_root = "/home/example/checkouts/hermit-42";
    let sibling_root = format!("{foreign_root}-backup");
    let embedded_root = format!("--hermit={foreign_root}/target/debug/hermit");
    let path_root = format!("/usr/bin:{foreign_root}/bin");
    let interior_root = format!("prefix{foreign_root}/target/debug/hermit");
    let foreign_env: BTreeMap<String, String> = [
        ("HOME".to_string(), format!("{foreign_root}/home")),
        ("PATH".to_string(), path_root),
        ("SIBLING".to_string(), sibling_root.clone()),
        ("INTERIOR".to_string(), interior_root.clone()),
    ]
    .into_iter()
    .collect();
    let mut fixture = ObservedInvocation {
        hermit_sha: "fixture-sha".into(),
        run_id: "fixture-run".into(),
        attempt: None,
        evidence_sha256: None,
        result: Some(ObservedResult::Pass),
        argv: vec![
            format!("{foreign_root}/target/debug/hermit"),
            foreign_root.into(),
            embedded_root,
            sibling_root.clone(),
            interior_root.clone(),
            "run".into(),
        ],
        guest_argv: vec!["/bin/true".into()],
        env: foreign_env.clone(),
        cwd: foreign_root.into(),
        shell_command: "stale - must be REBUILT, not substituted into".into(),
        attempts: vec![ObservedAttemptInvocation {
            index: "1".into(),
            outcome: "pass".into(),
            status: Some(0),
            signal: None,
            timed_out: false,
            argv: vec![format!("{foreign_root}/target/debug/hermit")],
            guest_argv: vec!["/bin/true".into()],
            env: foreign_env,
            cwd: foreign_root.into(),
            shell_command: "stale".into(),
        }],
    };
    normalise_invocation_root(&mut fixture);
    let encoded = serde_json::to_string(&fixture)
        .map_err(|e| format!("path-independence fixture will not serialize: {e}"))?;
    if fixture.cwd != RECORDED_ROOT
        || fixture.argv[0] != format!("{RECORDED_ROOT}/target/debug/hermit")
        || fixture.argv[1] != RECORDED_ROOT
        || fixture.argv[2] != format!("--hermit={RECORDED_ROOT}/target/debug/hermit")
        || fixture.env.get("HOME") != Some(&format!("{RECORDED_ROOT}/home"))
        || fixture.env.get("PATH") != Some(&format!("/usr/bin:{RECORDED_ROOT}/bin"))
    {
        return Err("path normalisation did not rewrite the exact root and descendants".into());
    }
    if fixture.argv[3] != sibling_root
        || fixture.argv[4] != interior_root
        || fixture.env.get("SIBLING") != Some(&sibling_root)
        || fixture.env.get("INTERIOR") != Some(&interior_root)
    {
        return Err("path normalisation rewrote a sibling path or interior substring".into());
    }
    let mut encoded_residue = encoded.clone();
    rewrite_recorded_root(&mut encoded_residue, foreign_root);
    if encoded_residue != encoded {
        return Err(format!(
            "encoded cells still name the producing worktree {foreign_root}"
        ));
    }
    for delimiter in [
        " ", "\t", "\n", "\r", "\u{000b}", "\u{000c}", "\u{00a0}", "\u{2003}",
    ] {
        let mut value = format!("{delimiter}{foreign_root}/nested");
        rewrite_recorded_root(&mut value, foreign_root);
        if value != format!("{delimiter}{RECORDED_ROOT}/nested") {
            return Err(format!(
                "path normalisation did not recognise explicit whitespace delimiter {:?}",
                delimiter
            ));
        }
    }
    for control in ['\u{001c}', '\u{001d}', '\u{001e}', '\u{001f}'] {
        let original = format!("{control}{foreign_root}/nested");
        let mut value = original.clone();
        rewrite_recorded_root(&mut value, foreign_root);
        if value != original {
            return Err(format!(
                "path normalisation treated control U+{:04X} as a delimiter",
                control as u32
            ));
        }
    }
    if !encoded.contains(RECORDED_ROOT) {
        return Err(format!(
            "encoded cells dropped the {RECORDED_ROOT} placeholder instead of rewriting to it"
        ));
    }
    // The rewrite must leave `shell_command` derivable from its own siblings at
    // BOTH levels, or the invocation guards reject every row it touched.
    if fixture.shell_command != literal_shell_command(&fixture.cwd, &fixture.env, &fixture.argv) {
        return Err(
            "path normalisation desynchronised shell_command from its own cwd/env/argv".into(),
        );
    }
    let Some(attempt) = fixture.attempts.first() else {
        return Err("path-independence fixture lost its attempts".into());
    };
    if attempt.shell_command != literal_shell_command(&attempt.cwd, &attempt.env, &attempt.argv) {
        return Err("path normalisation desynchronised a nested attempt's shell_command".into());
    }
    // ── divergence-without-a-comparison bracket ──────────────────────────────
    //
    // ⚠️ THIS GUARD HAD NO BRACKET AND NOTHING FAILED WHEN IT WAS DISABLED.
    // A reviewer disabled it outright with `if false && ...` and this self-test
    // stayed green across all eighteen brackets, so nothing pinned the behaviour
    // in either direction. That matters more here than usual: the guard's own
    // thesis is that two different facts must not fold into one, and a guard
    // nothing pins can silently stop working, after which they fold again -- the
    // failure it was written to prevent, arriving by another route.
    //
    // Both directions, because a guard that fires on everything is as wrong as
    // one that fires on nothing.
    {
        let mut refused = TrackedCells {
            schema: SCHEMA,
            projection: None,
            cells: vec![bare_cell(&id)],
        };
        // A row asserting a divergence with NO verification report at all: the
        // comparison never happened, so there is nothing to have located.
        let mut no_report = pressure_row("determinism-failure", Some(10), Some(20));
        no_report.verification = None;
        // ⚠️ EXPECTS AN Err. When every offered row is untrustworthy the fold
        // reports that as an error rather than an empty success, so the bracket
        // asserts on the message. Asserting Ok here would have quietly passed on
        // the wrong path.
        let refused = apply_pressure_summary(
            &mut refused,
            &pressure_summary("sha-nc", "tree-nc", vec![no_report]),
            "sha-nc",
            "tree-nc",
            &depth_fixture,
        );
        match refused {
            Ok(outcome) => {
                return Err(format!(
                    "a divergence-claiming row with no verification report was ADMITTED \
                     ({} row(s)); it asserts a divergence nothing measured",
                    outcome.rows
                ));
            }
            Err(why) if why.contains("no verification report at all") => {}
            Err(why) => {
                return Err(format!(
                    "the row was refused, but not for the reason this guard exists for: {why}"
                ));
            }
        }

        // THE OTHER DIRECTION. An otherwise identical row that DOES carry a
        // verification report must still fold, or the guard has quietly become a
        // refusal of every divergence.
        let mut admitted = TrackedCells {
            schema: SCHEMA,
            projection: None,
            cells: vec![bare_cell(&id)],
        };
        let admitted_fold = apply_pressure_summary(
            &mut admitted,
            &pressure_summary(
                "sha-nc",
                "tree-nc",
                vec![pressure_row("determinism-failure", Some(10), Some(20))],
            ),
            "sha-nc",
            "tree-nc",
            &depth_fixture,
        )
        .map_err(|e| format!("no-comparison bracket, admitted arm, failed: {e}"))?;
        if admitted_fold.rows != 1 {
            return Err(format!(
                "the guard refused a divergence that HAS a verification report; \
                 rows={} skipped={:?}",
                admitted_fold.rows, admitted_fold.skipped
            ));
        }
        refresh_measurement(&mut admitted);
        if admitted.cells[0].measurement != MeasurementState::Diverged {
            return Err(format!(
                "a located divergence with a report folded to {:?}, not Diverged",
                admitted.cells[0].measurement
            ));
        }
    }

    // Idempotence: regenerating an already-normalised file must be a no-op, or
    // `check_tracked` would report drift on every second run.
    let once = fixture.clone();
    normalise_invocation_root(&mut fixture);
    if fixture != once {
        return Err(
            "path normalisation is not idempotent; a second encode would report drift".into(),
        );
    }

    println!(
        "compatibility scorecard self-test: retained-comparison FRESH/DRIFTED/WRONG/UNCHECKABLE, provenance, distinct-evidence, result, selected-chaos, status-measurement-display, ratchet, observation-range, storage-round-trip, coordinate-less-divergence, recovered-no-result, determined-nothing-third-state, non-error-outcome-class, batch-equivalence, green-admission, validate-observation, empty-result command, source-identity, writer-boundary, projection, projection-schema, object-store-independence, path-independence, infrastructure-refusal, and divergence-without-a-comparison brackets pass"
    );
    Ok(())
}
