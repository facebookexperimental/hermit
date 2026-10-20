/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Cross-backend parity cells: the data model, the selection file and the
//! committed snapshot.
//!
//! A parity cell compares one manifest test's `verify` run on a candidate
//! backend (kvm, liteinst, sabre or dbt) against the same test's `verify` run
//! on ptrace, the reference. A [`ParityCellId`] is therefore a test id plus a
//! candidate backend. It has no mode, because the mode is always `verify`,
//! and ptrace cannot be a member, because it is the reference. It is distinct
//! from [`crate::runner::CellId`], which names one run rather than a
//! comparison between two.
//!
//! The matrix is derived, never hand-written: every manifest test crossed with
//! the four candidate backends. A cell is
//!
//! - APPLICABLE when `verify` is enabled on both ptrace and the candidate, and
//! - SELECTABLE when it is applicable and both of those `verify` cells are
//!   selected by full validation (`Population::Required`). A test's lane is a
//!   property of the test rather than of the backend, so the two cells are then
//!   in the same validate plan.
//!
//! Every cell that falls short carries the manifest's own reason.
//! [`PARITY_SELECTION_PATH`] names the cells the parity report measures, and
//! [`PARITY_CELLS_PATH`] is the generated snapshot of every cell's status.
//!
//! [`post_pass`] is the one mechanism that measures parity, in validate and in
//! pressure-test alike. It runs after the determinism cells, inside the
//! harness process (validate) or the pressure-test process once every cell of
//! the series has finished, and reads only the logs those cells retained: for each
//! cell in scope it compares the ptrace golden with the candidate's log using
//! `hermit log-diff` and writes one [`ParityRecord`] to `parity.jsonl`. It runs
//! no guest and never changes a determinism result or an exit status. See
//! <https://github.com/rrnewton/hermit/issues/3301>.
//!
//! Equal inputs. The ptrace and candidate cells of one test run from
//! different cell directories, and handing their guests those host paths
//! gives them different HOME, fixture and program paths, which alone make
//! ptrace diverge from ptrace. The runner therefore binds each verify cell's
//! input directories to the same guest paths below
//! [`crate::runner::EQUALIZED_INPUT_ROOT`] and names only those paths to the
//! guest. Every [`ParityRecord`] says whether the two runs were given equal
//! inputs, decided from how both were launched ([`inputs_equalized`]), and
//! credit from a comparison whose inputs were not equalized is reported apart
//! from clean credit (see [`ParityRecord::unequalized_credit`]). A backend
//! that cannot be given the reference's inputs at all reports
//! [`ParityVerdict::InputsNotEqualized`] instead of credit.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

use crate::canonical_verdict::Verdict;
use crate::ci_selection::CiSelection;
use crate::logdiff_report::LogDiffReport;
use crate::logdiff_report::LogDiffVerdict;
use crate::runner::CellResult;
use crate::runner::EQUALIZED_INPUTS;
use crate::runner::ManifestSet;
use crate::runner::ModeRecipe;
use crate::runner::ObservedResult;
use crate::runner::Population;
use crate::runner::Selection;
use crate::runner::TestRecipe;

/// The backend every candidate is compared against.
pub const PARITY_REFERENCE_BACKEND: &str = "ptrace";
/// The only mode a parity cell compares.
pub const PARITY_MODE: &str = "verify";
/// Which parity cells the parity report measures.
pub const PARITY_SELECTION_PATH: &str = "tests/e2e/parity-selection.yaml";
/// Generated snapshot of every parity cell's status.
pub const PARITY_CELLS_PATH: &str = "ci/compat-envelope/parity-cells.json";
pub const PARITY_SELECTION_SCHEMA: u64 = 1;
pub const PARITY_CELLS_SCHEMA: u64 = 1;
/// Schema of one `parity.jsonl` line.
pub const PARITY_RECORD_SCHEMA: u64 = 1;
/// The command that rewrites [`PARITY_CELLS_PATH`].
pub const PARITY_CELLS_REGENERATE: &str =
    "cargo run -p hermit-manifest-plan --bin generate-parity-cells -- --write";

/// The selection file must not live here: this directory holds only bucket
/// manifests, and the manifest reader parses every YAML file in it.
const MANIFEST_DIR: &str = "tests/e2e/manifests";
/// Must match the wording in `manifest_metadata`, which states the same fact
/// for the same cells.
const OCCASIONAL_REASON: &str =
    "This test is marked occasional, and full validation does not select occasional tests.";
/// Bound on each message copied into a [`ParityFirstDifference`], so a
/// `parity.jsonl` line stays small whatever the guest logged.
const MESSAGE_LIMIT_BYTES: usize = 512;
const TOKEN_LIMIT_BYTES: usize = 80;
/// The largest `f64` below 1.0. A partial match must never round up to full
/// credit.
const BELOW_ONE: f64 = 1.0 - f64::EPSILON / 2.0;
/// Why a dbt guest cannot be given the ptrace cell's inputs.
const DBT_INPUTS_NOT_EQUALIZABLE: &str = "the dbt backend refuses --bind and --mount (hermit-cli/src/bin/hermit/run.rs: \
     \"its DynamoRIO adapter does not enter the guest mount namespace\"), so its guest cannot be \
     given the ptrace cell's input paths";

/// A candidate backend. ptrace is the reference and is deliberately not
/// representable. Declared in alphabetical order so the derived `Ord` matches
/// the string order.
#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    Eq,
    Hash,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize
)]
#[serde(rename_all = "lowercase")]
pub enum ParityBackend {
    Dbt,
    Kvm,
    Liteinst,
    Sabre,
}

impl ParityBackend {
    pub const ALL: [Self; 4] = [Self::Dbt, Self::Kvm, Self::Liteinst, Self::Sabre];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dbt => "dbt",
            Self::Kvm => "kvm",
            Self::Liteinst => "liteinst",
            Self::Sabre => "sabre",
        }
    }

    /// Parse a candidate backend. ptrace gets its own refusal, because naming
    /// the reference as a candidate is a category error rather than a typo.
    pub fn parse(value: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|backend| backend.as_str() == value)
            .ok_or_else(|| {
                if value == PARITY_REFERENCE_BACKEND {
                    format!(
                        "{PARITY_REFERENCE_BACKEND} is the parity reference, not a candidate backend"
                    )
                } else {
                    format!(
                        "unknown parity backend {value:?}; expected one of dbt, kvm, liteinst, sabre"
                    )
                }
            })
    }

    /// Why this backend's guest cannot be given the same input paths as the
    /// ptrace reference, or `None` if it can. Such a backend's parity rows
    /// report [`ParityVerdict::InputsNotEqualized`] instead of credit.
    pub fn inputs_not_equalizable(self) -> Option<&'static str> {
        match self {
            Self::Dbt => Some(DBT_INPUTS_NOT_EQUALIZABLE),
            Self::Kvm | Self::Liteinst | Self::Sabre => None,
        }
    }
}

impl fmt::Display for ParityBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One comparison: `test_id` on `backend` against `test_id` on ptrace, both in
/// `verify` mode.
#[derive(
    Clone,
    Debug,
    Deserialize,
    Eq,
    Hash,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize
)]
#[serde(deny_unknown_fields)]
pub struct ParityCellId {
    pub test_id: String,
    pub backend: ParityBackend,
}

impl fmt::Display for ParityCellId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.test_id, self.backend)
    }
}

/// How far a parity cell gets, with the reason it stops short.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParityAvailability {
    /// `verify` is not enabled on ptrace, on the candidate, or on both.
    NotApplicable { reason: String },
    /// Applicable, but full validation does not select both `verify` cells.
    NotSelectable { reason: String },
    /// Both `verify` cells are selected by full validation.
    Selectable,
}

impl ParityAvailability {
    pub fn applicable(&self) -> bool {
        !matches!(self, Self::NotApplicable { .. })
    }

    pub fn selectable(&self) -> bool {
        matches!(self, Self::Selectable)
    }
}

/// Every test id crossed with every candidate backend.
#[derive(Clone, Debug)]
pub struct ParityMatrix {
    cells: BTreeMap<ParityCellId, ParityAvailability>,
    categories: BTreeMap<String, String>,
}

impl ParityMatrix {
    pub fn derive(manifests: &ManifestSet) -> Result<Self, String> {
        let selected_by_full: BTreeSet<(String, String)> = manifests
            .select(&Selection {
                population: Some(Population::Required),
                mode: Some(PARITY_MODE.to_string()),
                ..Selection::default()
            })?
            .into_iter()
            .filter_map(|cell| cell.id.backend.map(|backend| (cell.id.test, backend)))
            .collect();
        let mut cells = BTreeMap::new();
        let mut categories = BTreeMap::new();
        for (category, _, _, test) in manifests.all_tests() {
            categories.insert(test.id.clone(), category.to_string());
            for backend in ParityBackend::ALL {
                cells.insert(
                    ParityCellId {
                        test_id: test.id.clone(),
                        backend,
                    },
                    availability(test, backend, &selected_by_full)?,
                );
            }
        }
        if cells.is_empty() {
            return Err("the manifests declare no tests, so the parity matrix is empty".into());
        }
        Ok(Self { cells, categories })
    }

    pub fn get(&self, id: &ParityCellId) -> Option<&ParityAvailability> {
        self.cells.get(id)
    }

    pub fn knows_test(&self, test_id: &str) -> bool {
        self.categories.contains_key(test_id)
    }

    /// The manifest bucket a test belongs to.
    pub fn category(&self, test_id: &str) -> Option<&str> {
        self.categories.get(test_id).map(String::as_str)
    }

    /// Every cell in sort order: test id, then backend.
    pub fn cells(&self) -> impl Iterator<Item = (&ParityCellId, &ParityAvailability)> {
        self.cells.iter()
    }
}

fn availability(
    test: &TestRecipe,
    backend: ParityBackend,
    selected_by_full: &BTreeSet<(String, String)>,
) -> Result<ParityAvailability, String> {
    let Some(recipe) = test.modes.get(PARITY_MODE) else {
        return Ok(ParityAvailability::NotApplicable {
            reason: format!("{} has no {PARITY_MODE} mode", test.id),
        });
    };
    let sides = [PARITY_REFERENCE_BACKEND, backend.as_str()];

    let mut not_applicable = Vec::new();
    for side in sides {
        if recipe.backends_enabled.iter().any(|value| value == side) {
            continue;
        }
        // The manifest loader requires every mode to partition the backends,
        // so a backend that is not enabled has a stated disabled reason.
        let why = recipe.backends_disabled.get(side).ok_or_else(|| {
            format!(
                "{}: {side} {PARITY_MODE} is neither enabled nor disabled",
                test.id
            )
        })?;
        not_applicable.push(format!("{side} {PARITY_MODE} is disabled: {why}"));
    }
    if !not_applicable.is_empty() {
        return Ok(ParityAvailability::NotApplicable {
            reason: not_applicable.join("; "),
        });
    }

    let selection = configured_selection(recipe)
        .map_err(|error| format!("{}: {PARITY_MODE} {error}", test.id))?;
    let mut not_selected = Vec::new();
    for side in sides {
        if selected_by_full.contains(&(test.id.clone(), side.to_string())) {
            continue;
        }
        let why = if let Some(reason) = selection.reason(side) {
            reason.reason.clone()
        } else if let Some(reason) =
            crate::runner::focused_run_type_reason(test, recipe, Some(side))
        {
            reason
        } else if test.occasional {
            OCCASIONAL_REASON.to_string()
        } else {
            return Err(format!(
                "{}/{PARITY_MODE}@{side} is not selected by full validation without a reason",
                test.id
            ));
        };
        not_selected.push(format!(
            "{side} {PARITY_MODE} is not selected by full validation: {why}"
        ));
    }
    Ok(if not_selected.is_empty() {
        ParityAvailability::Selectable
    } else {
        ParityAvailability::NotSelectable {
            reason: not_selected.join("; "),
        }
    })
}

fn configured_selection(recipe: &ModeRecipe) -> Result<CiSelection, String> {
    CiSelection::validate(
        &recipe.backends_enabled.iter().cloned().collect(),
        &recipe.backends_disabled.keys().cloned().collect(),
        &recipe.ci,
        recipe.ci_disabled_reason.as_ref(),
    )
}

/// The parity credit of one comparison.
///
/// `credit = matched_prefix / max(left_len, right_len)`, clamped to
/// `[0.0, 1.0]`, where `matched_prefix` counts the leading compared messages
/// that are equal on both sides. In compared-message units the first
/// difference is at 1-based position `matched_prefix + 1`.
///
/// That position is not [`ParityRecord::first_divergent_record`]. That field
/// is the log-diff report's raw log-record index, which also counts records
/// the comparison did not select, so `(first_divergent_record - 1) /
/// max(left_len, right_len)` is not this credit and can exceed 1.
///
/// - 1.0 only for a full-length match, meaning both sides have the same
///   length and every message matched. A partial match never rounds up to 1.0.
/// - Unequal lengths with an equal prefix give `min / max`, because the extra
///   messages on the longer side are the divergence.
/// - A difference at the first message gives `Some(0.0)`. That is a measured
///   zero.
/// - `None` when nothing was measured, meaning both lengths are zero. An
///   unmeasured comparison is never 0.
///
/// A prefix longer than the shorter side is clamped to it: no side can match
/// more messages than it has.
pub fn credit(matched_prefix: usize, left_len: usize, right_len: usize) -> Option<f64> {
    let longer = left_len.max(right_len);
    if longer == 0 {
        return None;
    }
    let matched = matched_prefix.min(left_len.min(right_len));
    if matched == longer {
        return Some(1.0);
    }
    Some((matched as f64 / longer as f64).clamp(0.0, BELOW_ONE))
}

/// The parity credit a log-diff report supports, or `None` if it measured
/// nothing.
///
/// A match is full credit. A divergence needs the schema-2 matched prefix. A
/// schema-1 divergence carries no prefix, so it is unmeasured rather than
/// guessed. Refusals, empty comparisons and no-result reports are unmeasured.
pub fn credit_from_report(report: &LogDiffReport) -> Option<f64> {
    let selected = &report.selected_messages;
    match report.verdict {
        LogDiffVerdict::Matched => credit(
            report
                .matched_prefix_records
                .unwrap_or_else(|| selected.left.min(selected.right)),
            selected.left,
            selected.right,
        ),
        LogDiffVerdict::Diverged => report
            .matched_prefix_records
            .and_then(|prefix| credit(prefix, selected.left, selected.right)),
        _ => None,
    }
}

/// The outcome of one parity cell in one run. Every verdict but `matched` and
/// `diverged` carries the [`UnavailableClass`] that caused it; the class, not
/// the verdict, decides where the cell counts.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ParityVerdict {
    Matched,
    Diverged,
    /// An operand's own `verify` cell diverged between its two runs, so it
    /// has no deterministic log and no comparison is made
    /// ([`UnavailableClass::DeterminismMismatch`]).
    Nondeterministic,
    /// The ptrace `verify` run left no retained log to compare.
    ReferenceMissing,
    /// The candidate `verify` run left no retained log to compare.
    CandidateMissing,
    /// No comparison verdict could be reached: an operand ended without a
    /// deterministic log, or the parity tool could not compare two logs (see
    /// [`UnavailableClass::LogDiffFailed`]).
    Unavailable,
    /// The candidate backend cannot be given the reference's inputs at all
    /// (see [`ParityBackend::inputs_not_equalizable`]), so any comparison would
    /// measure the path difference rather than the backend.
    InputsNotEqualized,
}

impl ParityVerdict {
    pub fn is_measured(self) -> bool {
        matches!(self, Self::Matched | Self::Diverged)
    }
}

/// Where the two logs first part, from the log-diff report. Left is the ptrace
/// reference and right is the candidate, the same orientation as
/// [`LogDiffReport`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ParityFirstDifference {
    /// Which whitespace-separated token of the message differs first, and how.
    /// For example, ``token 3: `nr=0` vs `nr=1` ``.
    pub field: Option<String>,
    pub syscall: Option<u64>,
    pub scheduler_turn: Option<u64>,
    pub virtual_nanoseconds: Option<u64>,
    /// At most 512 bytes of each message.
    pub reference_message: Option<String>,
    pub candidate_message: Option<String>,
}

/// Deserialize an `Option` field that must be present, as `null` or a value.
/// Serde reads a missing `Option` field as `None` unless the field names its
/// own deserializer, so a row that omits the field is refused rather than
/// read as having no value.
fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

/// One line of `parity.jsonl`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ParityRecord {
    pub schema: u64,
    pub test_id: String,
    pub backend: ParityBackend,
    pub verdict: ParityVerdict,
    /// The one cause of an unmeasured verdict, typed; `None` exactly for a
    /// measured (`matched` or `diverged`) verdict. See [`check_class`].
    /// Required, `null` included.
    #[serde(deserialize_with = "present")]
    pub unavailable_class: Option<UnavailableClass>,
    /// The side [`ParityRecord::unavailable_class`] concerns, or `None` when
    /// it concerns the comparison rather than one side. Required, `null`
    /// included.
    #[serde(deserialize_with = "present")]
    pub operand: Option<ParityOperand>,
    /// Whether the ptrace and candidate runs were given the same input paths
    /// (HOME, XDG_CONFIG_HOME, the fixture directory and the program path),
    /// as [`inputs_equalized`] decides from how both were launched. False
    /// when either side was not launched with the runner's equalized inputs,
    /// and for every row decided before a comparison was chosen: a missing
    /// operand, or operands that cannot be shown to share a `HERMIT_EPOCH`
    /// (the epoch is one of the inputs).
    pub inputs_equalized: bool,
    /// Why an unmeasured verdict was reached. Absent for measured verdicts.
    pub reason: Option<String>,
    /// Clean credit (see [`credit`]): set only for a measured comparison whose
    /// inputs were equalized. `None` when unmeasured, never 0.
    pub credit: Option<f64>,
    /// The same measurement when the inputs were NOT equalized. It is kept out
    /// of `credit` so that a reader summing `credit` never counts a guest path
    /// difference as backend parity. Exactly one of the two is set for a
    /// measured comparison with a matched prefix.
    pub unequalized_credit: Option<f64>,
    /// Raw log-record index of the first difference, from the log-diff report.
    pub first_divergent_record: Option<usize>,
    /// Compared messages on the ptrace reference side.
    pub left_len: Option<usize>,
    /// Compared messages on the candidate side.
    pub right_len: Option<usize>,
    /// Leading compared messages equal on both sides.
    pub matched_prefix: Option<usize>,
    pub first_difference: Option<ParityFirstDifference>,
    pub reference_log: Option<String>,
    pub candidate_log: Option<String>,
    pub run_id: String,
    pub hermit_sha: String,
}

impl ParityRecord {
    /// Record the comparison of two retained logs.
    ///
    /// A backend that cannot be given the reference's inputs becomes
    /// `inputs-not-equalized` whatever the report says, and claiming equal
    /// inputs for it is an error; that is decided first. Otherwise a report
    /// whose verdict is not matched or diverged, or which fails the
    /// cross-backend evidence policy, becomes `unavailable` with class
    /// [`UnavailableClass::LogDiffFailed`], no operand, and the specific cause
    /// as its reason. It never becomes a zero-credit divergence.
    ///
    /// A matched or diverged report that passes the evidence policy but still
    /// contradicts its verdict, for example a match that names a first
    /// divergent record, is an error rather than an `unavailable` row: the
    /// built record must pass [`ParityRecord::validate`].
    ///
    /// `inputs_equalized` must come from how the two cells were launched, not
    /// from the comparison.
    #[allow(clippy::too_many_arguments)]
    pub fn from_comparison(
        cell: &ParityCellId,
        report: &LogDiffReport,
        inputs_equalized: bool,
        reference_log: &str,
        candidate_log: &str,
        run_id: &str,
        hermit_sha: &str,
    ) -> Result<Self, String> {
        if let Some(why) = cell.backend.inputs_not_equalizable() {
            if inputs_equalized {
                return Err(format!(
                    "parity cell {cell}: inputs cannot be equalized: {why}"
                ));
            }
            return Self::unmeasured(
                cell,
                ParityVerdict::InputsNotEqualized,
                UnavailableClass::InputsNotEqualized,
                Some(ParityOperand::Candidate),
                false,
                why,
                Some(reference_log),
                Some(candidate_log),
                run_id,
                hermit_sha,
            );
        }
        // Both logs were deterministic and retained, so a report that yields
        // no usable verdict is the parity tool's failure: it concerns the
        // comparison, not either side.
        let unavailable = |reason: String| {
            Self::unmeasured(
                cell,
                ParityVerdict::Unavailable,
                UnavailableClass::LogDiffFailed,
                None,
                inputs_equalized,
                &reason,
                Some(reference_log),
                Some(candidate_log),
                run_id,
                hermit_sha,
            )
        };
        let verdict = match report.verdict {
            LogDiffVerdict::Matched => ParityVerdict::Matched,
            LogDiffVerdict::Diverged => ParityVerdict::Diverged,
            other => {
                let detail = report
                    .refusal
                    .as_deref()
                    .map(|refusal| format!(": {refusal}"))
                    .unwrap_or_default();
                return unavailable(format!("log-diff verdict was {other:?}{detail}"));
            }
        };
        if let Err(error) = report.require_cross_backend_evidence() {
            return unavailable(format!(
                "log-diff report is not cross-backend evidence: {error}"
            ));
        }
        let selected = &report.selected_messages;
        let first_difference =
            (verdict == ParityVerdict::Diverged).then(|| ParityFirstDifference {
                field: first_differing_field(
                    report.first_divergent_left_message.as_deref(),
                    report.first_divergent_right_message.as_deref(),
                ),
                syscall: report.first_divergent_syscall,
                scheduler_turn: report.first_divergent_scheduler_turn,
                virtual_nanoseconds: report.first_divergent_virtual_nanoseconds,
                reference_message: report
                    .first_divergent_left_message
                    .as_deref()
                    .map(|message| bounded(message, MESSAGE_LIMIT_BYTES)),
                candidate_message: report
                    .first_divergent_right_message
                    .as_deref()
                    .map(|message| bounded(message, MESSAGE_LIMIT_BYTES)),
            });
        let matched_prefix = match verdict {
            ParityVerdict::Matched => Some(
                report
                    .matched_prefix_records
                    .unwrap_or_else(|| selected.left.min(selected.right)),
            ),
            _ => report.matched_prefix_records,
        };
        let measured = credit_from_report(report);
        let record = Self {
            schema: PARITY_RECORD_SCHEMA,
            test_id: cell.test_id.clone(),
            backend: cell.backend,
            verdict,
            unavailable_class: None,
            operand: None,
            inputs_equalized,
            reason: None,
            credit: measured.filter(|_| inputs_equalized),
            unequalized_credit: measured.filter(|_| !inputs_equalized),
            first_divergent_record: report.first_divergent_record,
            left_len: Some(selected.left),
            right_len: Some(selected.right),
            matched_prefix,
            first_difference,
            reference_log: Some(reference_log.to_string()),
            candidate_log: Some(candidate_log.to_string()),
            run_id: run_id.to_string(),
            hermit_sha: hermit_sha.to_string(),
        };
        record.validate()?;
        Ok(record)
    }

    /// Record a cell that could not be measured, with the one typed cause
    /// (`class`, and the side it concerns) that [`check_class`] accepts for
    /// `verdict`, and the specific cause in `reason`.
    #[allow(clippy::too_many_arguments)]
    pub fn unmeasured(
        cell: &ParityCellId,
        verdict: ParityVerdict,
        class: UnavailableClass,
        operand: Option<ParityOperand>,
        inputs_equalized: bool,
        reason: &str,
        reference_log: Option<&str>,
        candidate_log: Option<&str>,
        run_id: &str,
        hermit_sha: &str,
    ) -> Result<Self, String> {
        let record = Self {
            schema: PARITY_RECORD_SCHEMA,
            test_id: cell.test_id.clone(),
            backend: cell.backend,
            verdict,
            unavailable_class: Some(class),
            operand,
            inputs_equalized,
            reason: Some(reason.to_string()),
            credit: None,
            unequalized_credit: None,
            first_divergent_record: None,
            left_len: None,
            right_len: None,
            matched_prefix: None,
            first_difference: None,
            reference_log: reference_log.map(str::to_string),
            candidate_log: candidate_log.map(str::to_string),
            run_id: run_id.to_string(),
            hermit_sha: hermit_sha.to_string(),
        };
        record.validate()?;
        Ok(record)
    }

    /// The measured credit whether or not the inputs were equalized. Report
    /// it next to [`ParityRecord::inputs_equalized`]; only `credit` is clean.
    pub fn measured_credit(&self) -> Option<f64> {
        self.credit.or(self.unequalized_credit)
    }

    /// Refuse a record whose fields contradict its verdict.
    pub fn validate(&self) -> Result<(), String> {
        let at = format!("parity record {}@{}", self.test_id, self.backend);
        if self.schema != PARITY_RECORD_SCHEMA {
            return Err(format!(
                "{at}: schema must be {PARITY_RECORD_SCHEMA}, got {}",
                self.schema
            ));
        }
        if self.test_id.trim().is_empty() || self.run_id.trim().is_empty() {
            return Err(format!("{at}: test_id and run_id must be non-empty"));
        }
        if self.hermit_sha.len() != 40
            || !self
                .hermit_sha
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(format!(
                "{at}: hermit_sha must be 40 lowercase hex digits, got {:?}",
                self.hermit_sha
            ));
        }
        for credit in [self.credit, self.unequalized_credit].into_iter().flatten() {
            if !(0.0..=1.0).contains(&credit) {
                return Err(format!("{at}: credit {credit} is outside [0, 1]"));
            }
        }
        if self.credit.is_some() && self.unequalized_credit.is_some() {
            return Err(format!("{at}: credit and unequalized_credit are exclusive"));
        }
        if self.credit.is_some() && !self.inputs_equalized {
            return Err(format!(
                "{at}: credit is clean credit and needs equalized inputs; this measurement \
                 belongs in unequalized_credit"
            ));
        }
        if self.unequalized_credit.is_some() && self.inputs_equalized {
            return Err(format!(
                "{at}: unequalized_credit is set but the inputs were equalized"
            ));
        }
        // Such a backend is never compared, so none of its records claims
        // equal inputs; `check_class` refuses its measured verdicts and an
        // `inputs-not-equalized` verdict of any other backend.
        if let Some(why) = self.backend.inputs_not_equalizable() {
            if self.inputs_equalized {
                return Err(format!(
                    "{at}: {} inputs cannot be equalized, so it reports no measured \
                     comparison and never equal inputs: {why}",
                    self.backend
                ));
            }
        }
        check_class(
            &at,
            self.backend,
            LedgerVerdict::from(self.verdict),
            self.unavailable_class,
            self.operand,
        )?;
        if self.verdict.is_measured() {
            if self.reason.is_some() {
                return Err(format!("{at}: a measured verdict carries no reason"));
            }
            let (Some(left), Some(right)) = (self.left_len, self.right_len) else {
                return Err(format!("{at}: a measured verdict needs both lengths"));
            };
            if self.reference_log.is_none() || self.candidate_log.is_none() {
                return Err(format!("{at}: a measured verdict needs both log paths"));
            }
            // `credit` clamps the prefix to the shorter side, so without this a
            // prefix no side could have matched would still agree with it.
            if let Some(prefix) = self.matched_prefix.filter(|&p| p > left.min(right)) {
                return Err(format!(
                    "{at}: matched prefix {prefix} exceeds the shorter compared stream of \
                     {left} | {right}"
                ));
            }
            if self.verdict == ParityVerdict::Matched
                && (self.first_divergent_record.is_some() || self.first_difference.is_some())
            {
                return Err(format!("{at}: a match carries no divergence position"));
            }
            let expected = self
                .matched_prefix
                .and_then(|prefix| credit(prefix, left, right));
            let measured = self.measured_credit();
            if measured != expected {
                return Err(format!(
                    "{at}: credit {measured:?} disagrees with matched prefix {:?} of {left} | {right}",
                    self.matched_prefix
                ));
            }
            match self.verdict {
                ParityVerdict::Matched if measured != Some(1.0) => {
                    return Err(format!(
                        "{at}: a match must be full credit, got {measured:?}"
                    ));
                }
                ParityVerdict::Diverged if measured == Some(1.0) => {
                    return Err(format!("{at}: a divergence cannot be full credit"));
                }
                _ => {}
            }
        } else {
            if self
                .reason
                .as_deref()
                .is_none_or(|reason| reason.trim().is_empty())
            {
                return Err(format!("{at}: {:?} needs a reason", self.verdict));
            }
            if self.measured_credit().is_some()
                || self.matched_prefix.is_some()
                || self.first_divergent_record.is_some()
                || self.first_difference.is_some()
            {
                return Err(format!(
                    "{at}: {:?} is unmeasured and carries no credit or divergence",
                    self.verdict
                ));
            }
            if self.left_len.is_some() || self.right_len.is_some() {
                return Err(format!(
                    "{at}: {:?} is unmeasured and carries no compared lengths",
                    self.verdict
                ));
            }
            let missing_log = match self.verdict {
                ParityVerdict::ReferenceMissing => self.reference_log.is_some(),
                ParityVerdict::CandidateMissing => self.candidate_log.is_some(),
                _ => false,
            };
            if missing_log {
                return Err(format!(
                    "{at}: {:?} names the log it says is missing",
                    self.verdict
                ));
            }
        }
        Ok(())
    }
}

fn bounded(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

/// Summarize the first whitespace-separated token at which two messages
/// differ.
fn first_differing_field(reference: Option<&str>, candidate: Option<&str>) -> Option<String> {
    let (reference, candidate) = (reference?, candidate?);
    let mut left = reference.split_whitespace();
    let mut right = candidate.split_whitespace();
    let mut index = 1usize;
    loop {
        match (left.next(), right.next()) {
            (Some(l), Some(r)) if l == r => index += 1,
            (None, None) => {
                return Some("messages differ only in whitespace".to_string());
            }
            (l, r) => {
                let show = |token: Option<&str>| {
                    token.map_or_else(
                        || "end of message".to_string(),
                        |token| format!("`{}`", bounded(token, TOKEN_LIMIT_BYTES)),
                    )
                };
                return Some(format!("token {index}: {} vs {}", show(l), show(r)));
            }
        }
    }
}

/// The cells the parity report measures, as loaded from
/// [`PARITY_SELECTION_PATH`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParitySelection {
    /// The written rule the cells were chosen by.
    pub rule: String,
    pub cells: BTreeSet<ParityCellId>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectionDocument {
    schema: u64,
    rule: String,
    cells: Vec<SelectionEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectionEntry {
    test: String,
    backends: Vec<String>,
}

impl ParitySelection {
    /// Load [`PARITY_SELECTION_PATH`] below `root`.
    pub fn load(root: &Path, matrix: &ParityMatrix) -> Result<Self, String> {
        Self::load_path(root, Path::new(PARITY_SELECTION_PATH), matrix)
    }

    /// Load a selection file at `relative` below `root`. Refuses one placed
    /// under `tests/e2e/manifests`.
    pub fn load_path(root: &Path, relative: &Path, matrix: &ParityMatrix) -> Result<Self, String> {
        refuse_manifest_directory(root, relative)?;
        let path = root.join(relative);
        let text = fs::read_to_string(&path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        Self::parse(&text, matrix).map_err(|error| format!("{}: {error}", relative.display()))
    }

    /// Parse selection text against the matrix.
    ///
    /// Refuses an unknown or retired test id, ptrace or an unknown backend, a
    /// cell that is not applicable (printing why), duplicates or unsorted
    /// entries, an empty rule and an empty selection. A cell that is
    /// applicable but not selectable is accepted. The snapshot marks it, and
    /// it stays unmeasured until both of its `verify` cells are selected.
    pub fn parse(text: &str, matrix: &ParityMatrix) -> Result<Self, String> {
        let document: SelectionDocument =
            serde_yaml::from_str(text).map_err(|error| format!("invalid YAML: {error}"))?;
        if document.schema != PARITY_SELECTION_SCHEMA {
            return Err(format!(
                "schema must be {PARITY_SELECTION_SCHEMA}, got {}",
                document.schema
            ));
        }
        let rule = document.rule.trim().to_string();
        if rule.is_empty() {
            return Err("`rule` must state how the cells were chosen".into());
        }
        if document.cells.is_empty() {
            return Err("selects no cells; a selection that measures nothing is refused".into());
        }
        let mut cells = BTreeSet::new();
        let mut previous_test: Option<&str> = None;
        for entry in &document.cells {
            if previous_test.is_some_and(|previous| previous >= entry.test.as_str()) {
                return Err(format!(
                    "tests must be sorted and unique; {:?} follows {:?}",
                    entry.test,
                    previous_test.unwrap_or_default()
                ));
            }
            previous_test = Some(&entry.test);
            if !matrix.knows_test(&entry.test) {
                return Err(format!(
                    "unknown test id {:?}: no manifest declares it (retired or misspelled)",
                    entry.test
                ));
            }
            if entry.backends.is_empty() {
                return Err(format!("{} lists no backends", entry.test));
            }
            let mut previous_backend = None;
            for name in &entry.backends {
                let backend = ParityBackend::parse(name)
                    .map_err(|error| format!("{}: {error}", entry.test))?;
                if previous_backend.is_some_and(|previous| previous >= backend) {
                    return Err(format!(
                        "{}: backends must be sorted and unique; {backend} is out of order",
                        entry.test
                    ));
                }
                previous_backend = Some(backend);
                let id = ParityCellId {
                    test_id: entry.test.clone(),
                    backend,
                };
                match matrix.get(&id) {
                    Some(ParityAvailability::NotApplicable { reason }) => {
                        return Err(format!("{id} is not applicable: {reason}"));
                    }
                    Some(_) => {}
                    None => return Err(format!("{id} is not a parity cell")),
                }
                cells.insert(id);
            }
        }
        Ok(Self { rule, cells })
    }
}

fn refuse_manifest_directory(root: &Path, relative: &Path) -> Result<(), String> {
    if !relative
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err(format!(
            "parity selection path {} must be relative with plain components only",
            relative.display()
        ));
    }
    let refuse = || {
        Err(format!(
            "parity selection {} is under {MANIFEST_DIR}; that directory holds only bucket \
             manifests, and every YAML file in it is parsed as one. Keep it at \
             {PARITY_SELECTION_PATH}",
            relative.display()
        ))
    };
    if relative.starts_with(MANIFEST_DIR) {
        return refuse();
    }
    // A symlinked directory could still alias the manifest directory.
    if let (Ok(target), Ok(manifests)) = (
        fs::canonicalize(root.join(relative)),
        fs::canonicalize(root.join(MANIFEST_DIR)),
    ) {
        if target.starts_with(manifests) {
            return refuse();
        }
    }
    Ok(())
}

/// The committed snapshot at [`PARITY_CELLS_PATH`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ParityCells {
    pub schema: u64,
    pub regenerate: String,
    pub reference: ParityReference,
    pub selection_path: String,
    pub selection_rule: String,
    /// Backends whose guest cannot be given the reference's inputs, with the
    /// reason. Their parity rows report `inputs-not-equalized` rather than
    /// credit, even for a selected, selectable cell.
    pub inputs_not_equalizable: BTreeMap<ParityBackend, String>,
    pub counts: ParityCounts,
    pub cells: Vec<ParityCellStatus>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ParityReference {
    pub backend: String,
    pub mode: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ParityCounts {
    pub all: ParityCount,
    pub by_backend: BTreeMap<ParityBackend, ParityCount>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ParityCount {
    pub cells: usize,
    pub applicable: usize,
    pub selectable: usize,
    pub selected: usize,
    /// Selected and selectable: the cells a full validation can measure.
    pub selected_selectable: usize,
}

impl ParityCount {
    fn add(&mut self, cell: &ParityCellStatus) {
        self.cells += 1;
        self.applicable += usize::from(cell.applicable);
        self.selectable += usize::from(cell.selectable);
        self.selected += usize::from(cell.selected);
        self.selected_selectable += usize::from(cell.selected && cell.selectable);
    }
}

/// One cell of the snapshot. `reason` explains the furthest level the cell
/// does not reach. For a selected cell, it says so.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ParityCellStatus {
    pub test_id: String,
    pub backend: ParityBackend,
    pub category: String,
    pub applicable: bool,
    pub selectable: bool,
    pub selected: bool,
    pub reason: String,
}

/// Derive the snapshot from the manifests and the selection file under `root`.
pub fn generate(root: &Path) -> Result<ParityCells, String> {
    let manifests = ManifestSet::load(root)?;
    let matrix = ParityMatrix::derive(&manifests)?;
    let selection = ParitySelection::load(root, &matrix)?;
    snapshot(&matrix, &selection)
}

/// Build the snapshot from an already-derived matrix and selection.
pub fn snapshot(matrix: &ParityMatrix, selection: &ParitySelection) -> Result<ParityCells, String> {
    let mut cells = Vec::new();
    let mut all = ParityCount::default();
    let mut by_backend: BTreeMap<ParityBackend, ParityCount> = ParityBackend::ALL
        .into_iter()
        .map(|backend| (backend, ParityCount::default()))
        .collect();
    for (id, availability) in matrix.cells() {
        let selected = selection.cells.contains(id);
        let reason = match (availability, selected) {
            (ParityAvailability::NotApplicable { reason }, false) => {
                format!("not applicable: {reason}")
            }
            (ParityAvailability::NotApplicable { .. }, true) => {
                return Err(format!("{id} is selected but not applicable"));
            }
            (ParityAvailability::NotSelectable { reason }, false) => {
                format!("not selectable: {reason}")
            }
            (ParityAvailability::NotSelectable { reason }, true) => {
                format!("selected by {PARITY_SELECTION_PATH}, but not selectable: {reason}")
            }
            (ParityAvailability::Selectable, false) => {
                format!("selectable, not selected: not listed in {PARITY_SELECTION_PATH}")
            }
            (ParityAvailability::Selectable, true) => {
                format!("selected by {PARITY_SELECTION_PATH}")
            }
        };
        let cell = ParityCellStatus {
            test_id: id.test_id.clone(),
            backend: id.backend,
            category: matrix
                .category(&id.test_id)
                .ok_or_else(|| format!("{id} has no manifest category"))?
                .to_string(),
            applicable: availability.applicable(),
            selectable: availability.selectable(),
            selected,
            reason,
        };
        all.add(&cell);
        by_backend
            .get_mut(&id.backend)
            .expect("every backend is counted")
            .add(&cell);
        cells.push(cell);
    }
    // A selection may name a cell that is not selectable; the snapshot keeps
    // it and says why. A selection made ONLY of such cells has nothing a run
    // could measure, yet would still be written as a parity population.
    if all.cells == 0 || all.selected == 0 || all.selected_selectable == 0 {
        return Err(format!(
            "parity snapshot would be vacuous: {} cells, {} selected, {} of them selectable",
            all.cells, all.selected, all.selected_selectable
        ));
    }
    Ok(ParityCells {
        schema: PARITY_CELLS_SCHEMA,
        regenerate: PARITY_CELLS_REGENERATE.to_string(),
        reference: ParityReference {
            backend: PARITY_REFERENCE_BACKEND.to_string(),
            mode: PARITY_MODE.to_string(),
        },
        selection_path: PARITY_SELECTION_PATH.to_string(),
        selection_rule: selection.rule.clone(),
        inputs_not_equalizable: ParityBackend::ALL
            .into_iter()
            .filter_map(|backend| {
                backend
                    .inputs_not_equalizable()
                    .map(|why| (backend, why.to_string()))
            })
            .collect(),
        counts: ParityCounts { all, by_backend },
        cells,
    })
}

/// Canonical text of the snapshot: the header pretty-printed, and one cell per
/// line so a manifest change shows up as a small line diff.
pub fn canonical_text(snapshot: &ParityCells) -> Result<String, String> {
    // Serializing the struct itself keeps the header in declaration order.
    // The empty cells array is then the one slot the cell lines replace. It
    // cannot occur inside a string value, where its quotes would be escaped.
    const SLOT: &str = "\"cells\": []";
    let header = ParityCells {
        cells: Vec::new(),
        ..snapshot.clone()
    };
    let header = serde_json::to_string_pretty(&header).map_err(|error| error.to_string())?;
    if header.matches(SLOT).count() != 1 {
        return Err("parity snapshot header does not contain exactly one cells slot".into());
    }
    let mut cells = String::from("\"cells\": [\n");
    for (index, cell) in snapshot.cells.iter().enumerate() {
        cells.push_str("    ");
        cells.push_str(&serde_json::to_string(cell).map_err(|error| error.to_string())?);
        cells.push_str(if index + 1 == snapshot.cells.len() {
            "\n"
        } else {
            ",\n"
        });
    }
    cells.push_str("  ]");
    Ok(format!("{}\n", header.replace(SLOT, &cells)))
}

/// Refuse a stale snapshot, naming the first differing line and the fix.
pub fn require_fresh(committed: &str, generated: &str) -> Result<(), String> {
    if committed == generated {
        return Ok(());
    }
    let first = committed
        .lines()
        .zip(generated.lines())
        .position(|(left, right)| left != right)
        .map(|line| line + 1)
        .unwrap_or_else(|| committed.lines().count().min(generated.lines().count()) + 1);
    Err(format!(
        "{PARITY_CELLS_PATH} is stale (first differing line {first}); regenerate with: \
         {PARITY_CELLS_REGENERATE}"
    ))
}

// ---------------------------------------------------------------------------
// The post-pass: `parity.jsonl` from retained verify logs.

/// Activates parity cells for one harness run, beside the selection file:
/// comma-separated `<test-id>@<backend>` entries. A cell that is applicable
/// but not selectable is accepted, so any applicable cell can be measured.
pub const PARITY_SELECT_ENV: &str = "E2E_PARITY_SELECT";
/// The post-pass output, next to the harness's `results.jsonl`.
pub const PARITY_JSONL: &str = "parity.jsonl";
/// Where each ptrace golden and its guest-input sidecar are written, below the
/// artifacts directory: `<test_id>.detlog` and `<test_id>.inputs.json`.
pub const PARITY_GOLDEN_DIR: &str = "parity/golden";
/// Where each comparison's log-diff report and bounded stderr are kept.
pub const PARITY_LOGDIFF_DIR: &str = "parity/logdiff";
pub const PARITY_GOLDEN_SIDECAR_SCHEMA: u64 = 1;
/// The `hermit log-diff --record-envelope` every comparison uses.
pub const PARITY_RECORD_ENVELOPE: &str = "cross-backend-detcore-v1";
/// Wall bound for one `hermit log-diff` comparison.
pub const PARITY_LOG_DIFF_TIMEOUT: Duration = Duration::from_secs(120);
/// Wall bound for all of one post-pass's comparisons. A comparison not
/// started by then is recorded as unavailable, with this as the reason.
pub const PARITY_POST_PASS_BUDGET: Duration = Duration::from_secs(900);
/// The post-pass's own status, next to [`PARITY_JSONL`]: `running` while it
/// works, then `complete` or `failed`. `parity.jsonl` is this run's report
/// only while the status beside it is `complete`.
pub const PARITY_STATUS_JSON: &str = "parity.status.json";
/// Schema 2 names every cell in scope (`scope`), so a reader can say which
/// cells a post-pass that never completed left without a record. Readers still
/// accept [`PARITY_STATUS_SCHEMA_COUNT_ONLY`], which carries only the count.
pub const PARITY_STATUS_SCHEMA: u64 = 2;
/// The first status schema: a `cells` count and no `scope` list.
pub const PARITY_STATUS_SCHEMA_COUNT_ONLY: u64 = 1;
/// The smallest wall bound (`timeout`) of any dagrun step that runs a harness
/// post-pass. dagrun exports only when a step started
/// (`DAGRUN_STEP_STARTED_MONOTONIC_NS`), not its bound, so a harness inside a
/// step ends its comparisons this long after the step started, less
/// [`PARITY_STEP_EXIT_MARGIN`]. The validation DAG test
/// `every_harness_step_leaves_the_parity_post_pass_inside_its_wall_bound`
/// keeps every harness step's timeout at or above it.
pub const PARITY_STEP_WALL_FLOOR: Duration = Duration::from_secs(600);
/// Time left after the last comparison for the harness to exit before the
/// enclosing step's bound.
pub const PARITY_STEP_EXIT_MARGIN: Duration = Duration::from_secs(60);

const VERIFY_LOG_DIR_FLAG: &str = "--verify-log-dir";
/// The first verification run's retained log: after a match, the only log
/// `hermit run --keep-logs` keeps.
const RETAINED_LOG_PREFIX: &str = "run1_log_";
const STDERR_LIMIT_BYTES: usize = 16 * 1024;

/// Parse one `<test-id>@<backend>` cell against the matrix. Refuses an unknown
/// test, ptrace or an unknown backend, and a cell that is not applicable
/// (printing why). An applicable cell that full validation does not select is
/// accepted.
pub fn parse_parity_cell(value: &str, matrix: &ParityMatrix) -> Result<ParityCellId, String> {
    let value = value.trim();
    let (test_id, backend) = value
        .rsplit_once('@')
        .ok_or_else(|| format!("parity cell {value:?} must be <test-id>@<backend>"))?;
    let backend =
        ParityBackend::parse(backend).map_err(|error| format!("parity cell {value:?}: {error}"))?;
    if !matrix.knows_test(test_id) {
        return Err(format!(
            "parity cell {value:?}: no manifest declares test {test_id:?} (retired or misspelled)"
        ));
    }
    let cell = ParityCellId {
        test_id: test_id.to_string(),
        backend,
    };
    match matrix.get(&cell) {
        Some(ParityAvailability::NotApplicable { reason }) => {
            Err(format!("parity cell {cell} is not applicable: {reason}"))
        }
        Some(_) => Ok(cell),
        None => Err(format!("{cell} is not a parity cell")),
    }
}

/// Parse the value of [`PARITY_SELECT_ENV`]. An empty value selects nothing;
/// an empty entry between commas is refused.
pub fn parse_parity_select(
    value: &str,
    matrix: &ParityMatrix,
) -> Result<BTreeSet<ParityCellId>, String> {
    if value.trim().is_empty() {
        return Ok(BTreeSet::new());
    }
    value
        .split(',')
        .map(|entry| {
            if entry.trim().is_empty() {
                Err(format!(
                    "{PARITY_SELECT_ENV} has an empty entry in {value:?}"
                ))
            } else {
                parse_parity_cell(entry, matrix)
            }
        })
        .collect()
}

fn plans(planned_verify: &BTreeSet<(String, String)>, test: &str, backend: &str) -> bool {
    planned_verify.contains(&(test.to_string(), backend.to_string()))
}

/// Whether this process planned the ptrace or the candidate verify cell of
/// `cell`.
fn plans_either_side(planned_verify: &BTreeSet<(String, String)>, cell: &ParityCellId) -> bool {
    plans(planned_verify, &cell.test_id, PARITY_REFERENCE_BACKEND)
        || plans(planned_verify, &cell.test_id, cell.backend.as_str())
}

/// The cells one process reports on, and a warning for each explicitly
/// activated cell it dropped.
///
/// A cell of `selection` or of `explicit` is in scope when this process
/// planned its ptrace verify cell or its candidate verify cell. A test's verify
/// cells all belong to one manifest bucket, so every such cell is reported by
/// exactly one validation node, and the nodes of a profile together give one
/// line per selected cell. A cell whose sides were both planned elsewhere is
/// another process's line; an explicit cell no process plans is dropped with a
/// warning. A cell in scope always gets a line, measured or not.
pub fn post_pass_scope(
    selection: &BTreeSet<ParityCellId>,
    explicit: &BTreeSet<ParityCellId>,
    planned_verify: &BTreeSet<(String, String)>,
) -> (BTreeSet<ParityCellId>, Vec<String>) {
    let scope = selection
        .iter()
        .chain(explicit)
        .filter(|cell| plans_either_side(planned_verify, cell))
        .cloned()
        .collect();
    let warnings = explicit
        .iter()
        .filter(|cell| !plans_either_side(planned_verify, cell))
        .map(|cell| {
            format!(
                "explicitly selected parity cell {cell} is not reported by this run: it planned \
                 neither the {PARITY_REFERENCE_BACKEND} nor the {} verify cell of {}",
                cell.backend, cell.test_id
            )
        })
        .collect();
    (scope, warnings)
}

/// `0` turns off a harness process's own post-pass; unset or `1` leaves it on.
/// The pressure test sets `0` on every cell it launches: each of its cells is
/// a separate harness process holding one side of a comparison, so the
/// pressure test runs the same post-pass itself once every cell has finished.
pub const PARITY_POST_PASS_ENV: &str = "E2E_PARITY_POST_PASS";

/// The cells one run reports on, and what shrank that set without refusing
/// the run.
#[derive(Clone, Debug, Default)]
pub struct ResolvedScope {
    pub cells: BTreeSet<ParityCellId>,
    /// An unreadable selection file, a matrix that cannot be derived when no
    /// cell was activated explicitly, or an explicit cell neither of whose
    /// verify sides the run planned. Callers print these and carry on.
    pub warnings: Vec<String>,
}

/// Resolve one run's parity scope from the committed selection, the
/// explicitly activated cells (the value of [`PARITY_SELECT_ENV`] or of
/// `pressure-test --parity-select`) and the verify cells the run planned,
/// through [`post_pass_scope`]. The harness and the pressure test both call
/// this.
///
/// It refuses only an invalid explicit value, or a matrix that cannot be
/// derived while explicit cells were asked for. A selection file that cannot be
/// read is a warning, because the parity report must never stop the
/// determinism cells.
pub fn resolve_scope(
    root: &Path,
    manifests: &ManifestSet,
    explicit: Option<&str>,
    planned_verify: &BTreeSet<(String, String)>,
) -> Result<ResolvedScope, String> {
    let explicit = explicit.filter(|value| !value.trim().is_empty());
    let matrix = match ParityMatrix::derive(manifests) {
        Ok(matrix) => matrix,
        Err(error) if explicit.is_some() => {
            return Err(format!("cannot derive the parity matrix: {error}"));
        }
        Err(error) => {
            return Ok(ResolvedScope {
                cells: BTreeSet::new(),
                warnings: vec![format!("parity post-pass disabled: {error}")],
            });
        }
    };
    let explicit = explicit
        .map(|value| parse_parity_select(value, &matrix))
        .transpose()?
        .unwrap_or_default();
    let mut warnings = Vec::new();
    let selection = match ParitySelection::load(root, &matrix) {
        Ok(selection) => selection.cells,
        Err(error) => {
            warnings.push(format!("parity selection ignored: {error}"));
            BTreeSet::new()
        }
    };
    let (cells, dropped) = post_pass_scope(&selection, &explicit, planned_verify);
    warnings.extend(dropped);
    Ok(ResolvedScope { cells, warnings })
}

/// The `(test, backend)` verify cells whose logs the post-pass reads: ptrace
/// and the candidate of every cell in scope whose two sides this process both
/// planned, except a backend whose inputs cannot be equalized, which is never
/// compared. A cell with one side planned is reported missing without reading
/// a log, so neither of its logs is retained.
pub fn retention_closure(
    scope: &BTreeSet<ParityCellId>,
    planned_verify: &BTreeSet<(String, String)>,
) -> BTreeSet<(String, String)> {
    scope
        .iter()
        .filter(|cell| cell.backend.inputs_not_equalizable().is_none())
        .filter(|cell| {
            plans(planned_verify, &cell.test_id, PARITY_REFERENCE_BACKEND)
                && plans(planned_verify, &cell.test_id, cell.backend.as_str())
        })
        .flat_map(|cell| {
            [
                (cell.test_id.clone(), PARITY_REFERENCE_BACKEND.to_string()),
                (cell.test_id.clone(), cell.backend.as_str().to_string()),
            ]
        })
        .collect()
}

/// When a post-pass must have finished its comparisons, and which bound that
/// is, for the reason recorded on a comparison it did not start.
#[derive(Clone, Debug)]
pub struct PostPassDeadline {
    pub at: Instant,
    pub bound: String,
}

/// The deadline a harness inside a dagrun step must keep: the step's start
/// (`started`, the value of `DAGRUN_STEP_STARTED_MONOTONIC_NS`) plus
/// [`PARITY_STEP_WALL_FLOOR`] less [`PARITY_STEP_EXIT_MARGIN`]. `now_ns` is
/// `CLOCK_MONOTONIC` read at `now`. `Ok(None)` outside a dagrun step. An
/// unusable start or clock is an error.
pub fn step_deadline(
    started: Option<&str>,
    now_ns: Option<u64>,
    now: Instant,
) -> Result<Option<PostPassDeadline>, String> {
    let Some(started) = started else {
        return Ok(None);
    };
    let started_ns = started.trim().parse::<u64>().map_err(|_| {
        format!(
            "{}={started:?} is not a CLOCK_MONOTONIC nanosecond count",
            dagrun::scheduler::STEP_STARTED_MONOTONIC_NS_ENV
        )
    })?;
    let now_ns = now_ns.ok_or("CLOCK_MONOTONIC cannot be read")?;
    let elapsed = now_ns.checked_sub(started_ns).ok_or_else(|| {
        format!(
            "{}={started_ns} is later than CLOCK_MONOTONIC now ({now_ns})",
            dagrun::scheduler::STEP_STARTED_MONOTONIC_NS_ENV
        )
    })?;
    let allowed = PARITY_STEP_WALL_FLOOR - PARITY_STEP_EXIT_MARGIN;
    Ok(Some(PostPassDeadline {
        at: now + allowed.saturating_sub(Duration::from_nanos(elapsed)),
        bound: format!(
            "the enclosing dagrun step's {} s wall bound less a {} s exit margin, counted from \
             the step's start",
            PARITY_STEP_WALL_FLOOR.as_secs(),
            PARITY_STEP_EXIT_MARGIN.as_secs()
        ),
    }))
}

/// [`step_deadline`] for this process, from its environment and clock. An
/// unusable step start gives a deadline that has already passed, naming why,
/// so no comparison can outlive a step whose start is unknown.
pub fn dagrun_step_deadline() -> Option<PostPassDeadline> {
    let now = Instant::now();
    let started = std::env::var(dagrun::scheduler::STEP_STARTED_MONOTONIC_NS_ENV).ok();
    step_deadline(
        started.as_deref(),
        dagrun::scheduler::monotonic_now_ns(),
        now,
    )
    .unwrap_or_else(|error| {
        Some(PostPassDeadline {
            at: now,
            bound: format!("the enclosing dagrun step's unknown wall bound ({error})"),
        })
    })
}

/// Where and with what one post-pass runs.
#[derive(Clone, Debug)]
pub struct PostPassConfig {
    /// The directory holding the harness's `results.jsonl`.
    pub artifacts: PathBuf,
    /// The directory the post-pass writes below: its [`PARITY_GOLDEN_DIR`],
    /// its [`PARITY_LOGDIFF_DIR`] and its [`PARITY_STATUS_JSON`]. It is
    /// `artifacts` for the harness's own post-pass;
    /// [`PostPassConfig::writing_below`] moves it.
    pub output_dir: PathBuf,
    /// Where the records are written; [`PARITY_JSONL`] in `output_dir` unless
    /// overridden.
    pub output: PathBuf,
    /// The hermit binary whose `log-diff` compares the logs. It runs no guest.
    pub hermit_bin: PathBuf,
    pub run_id: String,
    pub hermit_sha: String,
    pub log_diff_timeout: Duration,
    pub budget: Duration,
    /// A bound outside this process that the comparisons must also keep,
    /// such as the enclosing dagrun step's ([`dagrun_step_deadline`]).
    pub outer_deadline: Option<PostPassDeadline>,
    /// Concurrent `log-diff` comparisons.
    pub jobs: usize,
    /// `(test, backend)` verify cells whose result rows the caller refused
    /// to hand over, with its typed reason. Such a cell has no row a
    /// comparison can trust, so an operand here is never compared, and takes
    /// the verdict and class the harness gives the same condition
    /// ([`ParityRejection::typed`]) and the caller's reason unless
    /// [`PostPassConfig::nondeterministic`] also names it, which is checked
    /// first. Empty for the harness, which hands over every row of its
    /// process.
    pub rejected: BTreeMap<(String, String), ParityRejection>,
    /// `(test, backend)` verify cells the caller saw diverge between their
    /// two runs outside the rows it handed over, such as in a later pressure
    /// repetition or in a refused row that is still the cell's own, with
    /// where. Such an operand is `nondeterministic` like one whose own
    /// history records a mismatch ([`records_mismatch`]). Empty for the
    /// harness.
    pub nondeterministic: BTreeMap<(String, String), String>,
}

/// Why a caller refused to hand over a verify cell's result row
/// ([`PostPassConfig::rejected`]). Each variant is a condition the harness's
/// own post-pass meets too, and the operand takes the verdict and class the
/// harness gives it ([`ParityRejection::typed`]). A defect in the caller's
/// evidence is therefore unmeasured, in the floor as 0, and never an outcome
/// that left no golden. <https://github.com/rrnewton/hermit/issues/3301>
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParityRejection {
    /// The cell left no result row at all, like a cell with no history:
    /// [`UnavailableClass::NoResultRow`] with the side's missing verdict.
    MissingRow(String),
    /// The cell left result rows the caller could not accept: rows that do
    /// not parse or do not match the cell, or evidence that contradicts them
    /// (the harness exit, the verification report, the recorded result).
    /// Like a history [`crate::runner::cell_result_after_retries`] refuses:
    /// [`UnavailableClass::NoResultRow`] with verdict `unavailable`.
    InvalidRow(String),
    /// The cell's row was valid but it did not retain the verify logs the
    /// caller requires, like a passing cell without exactly one nonempty
    /// first-run log: [`UnavailableClass::LogNotRetained`] with the side's
    /// missing verdict.
    LogNotRetained(String),
    /// Evidence other than a row, such as the harness summary, proves the
    /// cell host-inapplicable, so it left no log, like a `HOST-INAPPLICABLE`
    /// row: [`UnavailableClass::HostInapplicable`] with the side's missing
    /// verdict.
    HostInapplicable(String),
}

impl ParityRejection {
    pub fn reason(&self) -> &str {
        match self {
            Self::MissingRow(reason)
            | Self::InvalidRow(reason)
            | Self::LogNotRetained(reason)
            | Self::HostInapplicable(reason) => reason,
        }
    }

    /// The verdict and class of `operand` refused for this reason. `missing`
    /// is the side's verdict for leaving no log at all, `reference-missing`
    /// or `candidate-missing`.
    pub fn typed(
        &self,
        operand: ParityOperand,
        missing: ParityVerdict,
    ) -> (ParityVerdict, UnavailableClass) {
        match (self, operand) {
            (Self::MissingRow(_), _) => (missing, UnavailableClass::NoResultRow),
            (Self::LogNotRetained(_), _) => (missing, UnavailableClass::LogNotRetained),
            (Self::HostInapplicable(_), _) => (missing, UnavailableClass::HostInapplicable),
            (Self::InvalidRow(_), _) => (ParityVerdict::Unavailable, UnavailableClass::NoResultRow),
        }
    }
}

impl PostPassConfig {
    pub fn new(artifacts: &Path, hermit_bin: &Path, run_id: &str, hermit_sha: &str) -> Self {
        Self {
            artifacts: artifacts.to_path_buf(),
            output_dir: artifacts.to_path_buf(),
            output: artifacts.join(PARITY_JSONL),
            hermit_bin: hermit_bin.to_path_buf(),
            run_id: run_id.to_string(),
            hermit_sha: hermit_sha.to_string(),
            log_diff_timeout: PARITY_LOG_DIFF_TIMEOUT,
            budget: PARITY_POST_PASS_BUDGET,
            outer_deadline: None,
            jobs: 1,
            rejected: BTreeMap::new(),
            nondeterministic: BTreeMap::new(),
        }
    }

    /// Write every output below `output_dir` instead of `artifacts`, which is
    /// then only read. A golden the harness already wrote below `artifacts`
    /// is still reused when the reference's retained log is gone.
    pub fn writing_below(mut self, output_dir: &Path) -> Self {
        self.output_dir = output_dir.to_path_buf();
        self.output = output_dir.join(PARITY_JSONL);
        self
    }

    pub fn status_path(&self) -> PathBuf {
        self.output_dir.join(PARITY_STATUS_JSON)
    }
}

/// Where a post-pass stands, in [`PARITY_STATUS_JSON`].
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PostPassState {
    Running,
    Complete,
    Failed,
}

/// The content of [`PARITY_STATUS_JSON`]. Its `records` file is this run's
/// report only while `state` is `complete`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PostPassStatus {
    pub schema: u64,
    pub state: PostPassState,
    pub run_id: String,
    pub hermit_sha: String,
    /// The binary whose `log-diff` made the comparisons, and its SHA-256
    /// (`null` when it cannot be read).
    pub hermit_bin: String,
    pub hermit_bin_sha256: Option<String>,
    /// The `parity.jsonl` this status describes.
    pub records: String,
    /// Cells in scope; a complete report has one line per cell.
    pub cells: usize,
    /// Every cell in scope as `<test>@<backend>`, in scope order. Written in
    /// the `running` state before any comparison, so a post-pass that is
    /// killed still names the cells it owed. Present from schema 2; absent in
    /// a schema-1 status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<Vec<String>>,
    pub summary: Option<String>,
    pub error: Option<String>,
}

impl PostPassStatus {
    /// The status a post-pass writes before it compares anything.
    pub fn running(config: &PostPassConfig, scope: &BTreeSet<ParityCellId>) -> Self {
        Self {
            schema: PARITY_STATUS_SCHEMA,
            state: PostPassState::Running,
            run_id: config.run_id.clone(),
            hermit_sha: config.hermit_sha.clone(),
            hermit_bin: path_text(&config.hermit_bin),
            hermit_bin_sha256: file_sha256(&config.hermit_bin)
                .ok()
                .map(|(_, sha256, _)| sha256),
            records: path_text(&config.output),
            cells: scope.len(),
            scope: Some(scope.iter().map(ToString::to_string).collect()),
            summary: None,
            error: None,
        }
    }

    /// The cells this status names, or `None` for a schema-1 status, which
    /// names only their count. Refuses an unknown schema, a schema-2 status
    /// without a scope or whose scope disagrees with `cells`, a schema-1
    /// status with a scope, and a scope entry that is not one parity cell.
    pub fn checked_scope(&self) -> Result<Option<Vec<ParityCellId>>, String> {
        match (self.schema, &self.scope) {
            (PARITY_STATUS_SCHEMA_COUNT_ONLY, None) => Ok(None),
            (PARITY_STATUS_SCHEMA_COUNT_ONLY, Some(_)) => Err(format!(
                "parity status schema {PARITY_STATUS_SCHEMA_COUNT_ONLY} carries no scope, \
                 but this one has one"
            )),
            (PARITY_STATUS_SCHEMA, None) => Err(format!(
                "parity status schema {PARITY_STATUS_SCHEMA} must name its scope"
            )),
            (PARITY_STATUS_SCHEMA, Some(scope)) => {
                if scope.len() != self.cells {
                    return Err(format!(
                        "parity status names {} scope cell(s) but counts {}",
                        scope.len(),
                        self.cells
                    ));
                }
                let mut seen = BTreeSet::new();
                scope
                    .iter()
                    .map(|text| {
                        let cell = parse_cell_text(text)?;
                        if !seen.insert(cell.clone()) {
                            return Err(format!("parity status names {text} twice"));
                        }
                        Ok(cell)
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .map(Some)
            }
            (other, _) => Err(format!(
                "parity status schema must be {PARITY_STATUS_SCHEMA_COUNT_ONLY} or \
                 {PARITY_STATUS_SCHEMA}, got {other}"
            )),
        }
    }
}

/// Parse `<test>@<backend>`, the [`ParityCellId`] display form, without a
/// matrix: the test id is not checked against the manifests.
pub fn parse_cell_text(text: &str) -> Result<ParityCellId, String> {
    let (test_id, backend) = text
        .rsplit_once('@')
        .ok_or_else(|| format!("parity cell {text:?} is not <test>@<backend>"))?;
    if test_id.trim().is_empty() || test_id != test_id.trim() {
        return Err(format!("parity cell {text:?} has no usable test id"));
    }
    Ok(ParityCellId {
        test_id: test_id.to_string(),
        backend: ParityBackend::parse(backend)
            .map_err(|error| format!("parity cell {text:?}: {error}"))?,
    })
}

/// Remove what an earlier post-pass left below `config`: the records file,
/// the status and the log-diff reports. Goldens stay; each is reused only
/// while its sidecar names the current run. The harness calls this when it
/// runs no post-pass, so an older report never reads as this run's.
pub fn clear_outputs(config: &PostPassConfig) -> Result<(), String> {
    remove_file_if_present(&config.output)?;
    remove_file_if_present(&config.status_path())?;
    let logdiff = config.output_dir.join(PARITY_LOGDIFF_DIR);
    match fs::remove_dir_all(&logdiff) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("cannot remove {}: {error}", logdiff.display())),
    }
}

fn remove_file_if_present(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("cannot remove {}: {error}", path.display())),
    }
}

fn write_status(config: &PostPassConfig, status: &PostPassStatus) -> Result<(), String> {
    let mut text = serde_json::to_vec_pretty(status).map_err(|error| error.to_string())?;
    text.push(b'\n');
    write_atomically(&config.status_path(), &text)
}

/// Remove the previous outputs ([`clear_outputs`]) and mark the post-pass
/// `running` with its scope, before the determinism cells run. A process
/// that is then killed before [`post_pass`] finishes leaves a status naming
/// every cell it owed, so each is reported `record-missing` rather than
/// vanishing from the denominator. [`post_pass`] writes the same status again
/// when it starts.
pub fn mark_running(config: &PostPassConfig, scope: &BTreeSet<ParityCellId>) -> Result<(), String> {
    clear_outputs(config)?;
    write_status(config, &PostPassStatus::running(config, scope))
}

/// Remove the previous outputs ([`clear_outputs`]) and mark the post-pass
/// `failed` with `error`, for a caller whose post-pass failed outside
/// [`post_pass`]: a scope that could not be resolved, or a panic before
/// [`post_pass`] took over. `scope` is every cell the post-pass owed, so
/// [`node_ledger_sources`] reports each as `record-missing` with the error
/// rather than reading the failure as an empty report. An empty `scope`
/// means the cells it owed are unknown; such a status yields no row, so a
/// caller that reads it must report the failure in its own words.
///
/// The failed status is written even when the previous outputs cannot all
/// be removed, so an earlier `complete` status never outlives the failure;
/// the error returned then names what could not be removed.
pub fn mark_failed(
    config: &PostPassConfig,
    scope: &BTreeSet<ParityCellId>,
    error: &str,
) -> Result<(), String> {
    let cleared = clear_outputs(config);
    let mut status = PostPassStatus::running(config, scope);
    status.state = PostPassState::Failed;
    status.error = Some(error.to_string());
    let written = write_status(config, &status);
    match (cleared, written) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(cleared), Err(written)) => Err(format!("{cleared}; {written}")),
    }
}

/// The text of a panic payload, for the status or the line that reports it.
pub fn panic_text(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|text| text.to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a non-text panic payload".to_string())
}

#[cfg(test)]
thread_local! {
    /// Makes the next post-pass on this thread panic after it has marked
    /// itself running, to test that a panic leaves a failed status and no
    /// records.
    static PANIC_INSIDE_POST_PASS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// What one post-pass wrote.
#[derive(Clone, Debug)]
pub struct PostPassReport {
    pub path: PathBuf,
    /// One record per cell in scope, in scope order.
    pub records: Vec<ParityRecord>,
    /// `hermit log-diff` comparisons started. No guest runs in a post-pass.
    pub log_diff_runs: usize,
}

impl PostPassReport {
    /// One line of honest accounting: every verdict count; the cells
    /// measured, and those with no golden, not compared or unmeasured, each
    /// group with its nonzero [`UnavailableClass`] counts, the same counts
    /// the scorecard prints; the mean clean credit over the cells measured
    /// with equal inputs, and apart from it the mean credit over the cells
    /// measured with unequal inputs. Neither mean counts an unmeasured cell,
    /// and neither prints as full credit unless every credit it averages is
    /// full (`mean_credit_text`).
    pub fn summary_line(&self) -> String {
        let count = |verdict| {
            self.records
                .iter()
                .filter(|record| record.verdict == verdict)
                .count()
        };
        let group = |group: UnavailableGroup, label: &str| {
            let classes = UnavailableClass::ALL
                .into_iter()
                .filter(|class| class.group() == group)
                .map(|class| {
                    let cells = self
                        .records
                        .iter()
                        .filter(|record| record.unavailable_class == Some(class))
                        .count();
                    (class, cells)
                })
                .filter(|(_, cells)| *cells > 0)
                .collect::<Vec<_>>();
            let total = classes.iter().map(|(_, cells)| cells).sum::<usize>();
            if total == 0 || group == UnavailableGroup::NotCompared {
                format!("{label} {total}")
            } else {
                let classes = classes
                    .iter()
                    .map(|(class, cells)| format!("{class} {cells}"))
                    .collect::<Vec<_>>();
                format!("{label} {total} ({})", classes.join(", "))
            }
        };
        let mean = |credits: Vec<f64>, inputs: &str| {
            if credits.is_empty() {
                format!("none measured with {inputs} inputs")
            } else {
                format!(
                    "mean credit {} over {} measured with {inputs} inputs",
                    mean_credit_text(&credits),
                    credits.len()
                )
            }
        };
        let clean = mean(
            self.records
                .iter()
                .filter_map(|record| record.credit)
                .collect(),
            "equal",
        );
        let unequalized = mean(
            self.records
                .iter()
                .filter_map(|record| record.unequalized_credit)
                .collect(),
            "unequal",
        );
        let mean = format!("{clean}; {unequalized}");
        format!(
            "parity: {} cell(s) -> {}: matched {}, diverged {}, nondeterministic {}, \
             reference-missing {}, candidate-missing {}, unavailable {}, \
             inputs-not-equalized {}; measured {}; {}; {}; {}; {mean}; \
             {} log-diff comparison(s), 0 guest runs",
            self.records.len(),
            self.path.display(),
            count(ParityVerdict::Matched),
            count(ParityVerdict::Diverged),
            count(ParityVerdict::Nondeterministic),
            count(ParityVerdict::ReferenceMissing),
            count(ParityVerdict::CandidateMissing),
            count(ParityVerdict::Unavailable),
            count(ParityVerdict::InputsNotEqualized),
            count(ParityVerdict::Matched) + count(ParityVerdict::Diverged),
            group(UnavailableGroup::NoGolden, "no golden"),
            group(UnavailableGroup::NotCompared, "not compared"),
            group(UnavailableGroup::Unmeasured, "unmeasured"),
            self.log_diff_runs,
        )
    }
}

/// The mean of `credits` (not empty) to four decimals, never reading as full
/// credit unless every credit is full: a mean below 1 that would print
/// `1.0000` prints `0.9999`, as [`credit`] keeps a partial match below 1.
/// Whether the mean is below 1 is decided from the credits, not from their
/// floating-point mean, which can round up to exactly 1: the largest credit
/// below 1 plus one full credit sums to exactly 2.
fn mean_credit_text(credits: &[f64]) -> String {
    let mean = credits.iter().sum::<f64>() / credits.len() as f64;
    let text = format!("{mean:.4}");
    if text == "1.0000" && credits.iter().any(|credit| *credit < 1.0) {
        "0.9999".to_string()
    } else {
        text
    }
}

/// The guest-visible inputs of one verify run, as launched: read from its
/// result row, and kept beside each ptrace golden. [`inputs_equalized`]
/// compares the reference's with the candidate's.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ParityGuestInputs {
    pub guest_argv: Vec<String>,
    /// Every `--env NAME=VALUE` passed to the guest.
    pub guest_env: BTreeMap<String, String>,
    pub workdir: Option<String>,
    /// Every `--mount=` and `--bind=` argument, verbatim.
    pub mounts: Vec<String>,
    /// `HERMIT_EPOCH` given to the hermit process.
    pub epoch: Option<String>,
}

impl ParityGuestInputs {
    pub fn from_result(row: &CellResult) -> Self {
        let mut inputs = Self {
            guest_argv: row.guest_argv.clone(),
            guest_env: BTreeMap::new(),
            workdir: None,
            mounts: Vec::new(),
            epoch: row.env.get("HERMIT_EPOCH").cloned(),
        };
        let mut args = row.argv.iter().take_while(|arg| arg.as_str() != "--");
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--env" => {
                    if let Some((name, value)) = args.next().and_then(|pair| pair.split_once('=')) {
                        inputs.guest_env.insert(name.to_string(), value.to_string());
                    }
                }
                "--workdir" => inputs.workdir = args.next().cloned(),
                _ if arg.starts_with("--mount=") || arg.starts_with("--bind=") => {
                    inputs.mounts.push(arg.clone());
                }
                _ => {}
            }
        }
        inputs
    }

    /// Whether the run was launched with the runner's equalized inputs: each
    /// directory in [`EQUALIZED_INPUTS`] bound exactly once at its guest
    /// path, the guest environment naming that path, and a recorded
    /// `HERMIT_EPOCH`.
    pub fn is_equalized(&self) -> bool {
        self.epoch.is_some()
            && EQUALIZED_INPUTS.iter().all(|input| {
                self.guest_env.get(input.env).map(String::as_str) == Some(input.guest_path)
                    && self
                        .mounts
                        .iter()
                        .filter(|mount| bind_target(mount) == Some(input.guest_path))
                        .count()
                        == 1
            })
    }

    /// The inputs as the guest sees them. A `--bind=SOURCE:TARGET` is seen
    /// only at its target, so its host source is dropped; every other field,
    /// `--mount=` arguments included, is kept as launched.
    fn guest_view(&self) -> Self {
        let mut view = self.clone();
        for mount in &mut view.mounts {
            if let Some(target) = bind_target(mount) {
                *mount = format!("--bind=:{target}");
            }
        }
        view
    }
}

/// The guest target of a `--bind=` argument, parsed as hermit parses it
/// (reverie-process `Bind`: the text after the first `:`, or the source when
/// there is none). `None` for any other argument.
fn bind_target(mount: &str) -> Option<&str> {
    let bind = mount.strip_prefix("--bind=")?;
    Some(bind.split_once(':').map_or(bind, |(_, target)| target))
}

/// Whether the ptrace reference and the candidate were given equal guest
/// inputs.
///
/// True only when the runner's equalization applied on both sides
/// ([`ParityGuestInputs::is_equalized`]) and the two guests were launched
/// with the same view: the same argv, guest environment, working directory,
/// mounts and bind targets, and `HERMIT_EPOCH`. Only the host sources of the
/// binds may differ; they are the two cell directories.
///
/// It compares launches, not directory contents. Both cells' fixture
/// directories are made by the same preparation from the same sources (under
/// `--prebuilt`, copies of one build), and a comparison of them would not
/// catch a nondeterministic build anyway. A guest that reads
/// `/proc/self/mountinfo` can still see each bind's host source there.
///
/// The epoch is part of the view rather than something the runner rewrites
/// per cell: one harness process gives every cell it runs the same
/// `HERMIT_EPOCH`, and the pressure test samples one epoch per series and
/// passes it to each harness process
/// ([`crate::runner::run_epoch_from_env`]). Equal epochs are therefore the
/// normal case, and operands whose epochs differ or are unrecorded are
/// reported as unavailable before they are compared.
pub fn inputs_equalized(reference: &ParityGuestInputs, candidate: &ParityGuestInputs) -> bool {
    reference.is_equalized()
        && candidate.is_equalized()
        && reference.guest_view() == candidate.guest_view()
}

/// The sidecar written next to each ptrace golden.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ParityGoldenSidecar {
    pub schema: u64,
    pub test_id: String,
    pub backend: String,
    pub run_id: String,
    pub run_index: Option<u64>,
    pub attempt: u64,
    pub hermit_sha: String,
    pub binary_sha256: Option<String>,
    pub outcome: String,
    pub artifact_dir: String,
    /// The retained log the golden was taken from, as an absolute path.
    pub source_log: String,
    pub log_sha256: String,
    pub log_bytes: u64,
    pub guest_inputs: ParityGuestInputs,
    /// SHA-256 of `guest_inputs` serialized as compact JSON.
    pub guest_inputs_sha256: String,
}

/// Paths of one test's golden and sidecar below `artifacts`, or an error when
/// the test id is not a plain relative path.
pub fn golden_paths(artifacts: &Path, test_id: &str) -> Result<(PathBuf, PathBuf), String> {
    let base = artifacts.join(PARITY_GOLDEN_DIR);
    Ok((
        base.join(plain_relative(test_id, ".detlog")?),
        base.join(plain_relative(test_id, ".inputs.json")?),
    ))
}

fn plain_relative(test_id: &str, suffix: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(format!("{test_id}{suffix}"));
    if test_id.is_empty()
        || !path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err(format!(
            "test id {test_id:?} is not a plain relative path; refusing to write below it"
        ));
    }
    Ok(path)
}

/// One side of a comparison, hashed when it was chosen.
#[derive(Clone, Debug)]
struct Operand {
    /// Absolute: resolved against this process's working directory when the
    /// operand was chosen ([`hashed`]), so it names the same file for a
    /// log-diff child running in the artifacts directory and for a later
    /// reader of `parity.jsonl`, which records no working directory.
    log: PathBuf,
    sha256: String,
    bytes: u64,
    /// How the run that wrote the log was launched, `HERMIT_EPOCH` included.
    inputs: ParityGuestInputs,
}

/// Why one side of a cell cannot be compared: the verdict and the typed
/// class its record carries, the side the class concerns, and the specific
/// cause.
#[derive(Clone, Debug)]
struct Unusable {
    verdict: ParityVerdict,
    class: UnavailableClass,
    operand: Option<ParityOperand>,
    reason: String,
}

struct Comparison {
    index: usize,
    cell: ParityCellId,
    reference: Operand,
    candidate: Operand,
    /// [`inputs_equalized`] of the two operands' launches.
    inputs_equalized: bool,
}

/// Write `parity.jsonl` for `scope` from the verify rows of one harness
/// process (every attempt, in any order) and their retained logs.
///
/// It runs no guest. Each cell is decided in this order, and every record
/// that is not `matched` or `diverged` carries the typed
/// [`UnavailableClass`] of the step that decided it:
/// - a) the ptrace reference left no deterministic golden
///   ([`UnavailableGroup::NoGolden`], see `evaluate_history`). A
///   determinism failure on any attempt, or one the caller reports in
///   [`PostPassConfig::nondeterministic`], is
///   [`ParityVerdict::Nondeterministic`]; no golden is written for it;
/// - b) the candidate left no deterministic log, by the same rules;
/// - c) the backend cannot be given the reference's inputs
///   ([`ParityVerdict::InputsNotEqualized`]), whatever its rows or logs;
/// - d) both operands should have a golden, but the cell cannot be measured
///   ([`UnavailableGroup::Unmeasured`]): a missing result row, a log that
///   was not retained or cannot be read, an invalid test id, a
///   `HERMIT_EPOCH` the operands cannot be shown to share, or a golden that
///   could not be written;
/// - e) otherwise one `hermit log-diff` with [`PARITY_RECORD_ENVELOPE`]
///   compares the ptrace golden with the candidate's first-run log, and
///   [`ParityRecord::from_comparison`] records it. A comparison that cannot
///   run or report is [`UnavailableClass::LogDiffFailed`].
///
/// No log is read before d), so an operand that diverged is never compared,
/// whatever logs it retained. Every cell in scope gets exactly one line.
///
/// Before anything that can fail it removes the previous outputs
/// ([`clear_outputs`]) and marks [`PARITY_STATUS_JSON`] `running`; it ends it
/// `complete`, or `failed` with the records file removed. A panic inside is
/// returned as an error.
///
/// `rows` must be one run's history: attempts `1..n` of each cell. The
/// post-pass reads nothing else and never changes a row, a result file or an
/// exit status.
pub fn post_pass(
    config: &PostPassConfig,
    scope: &BTreeSet<ParityCellId>,
    rows: &[CellResult],
) -> Result<PostPassReport, String> {
    let mut status = PostPassStatus::running(config, scope);
    let result = clear_outputs(config)
        .and_then(|()| write_status(config, &status))
        .and_then(|()| {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                #[cfg(test)]
                if PANIC_INSIDE_POST_PASS.with(|armed| armed.replace(false)) {
                    panic!("planted post-pass panic");
                }
                measure(config, scope, rows)
            }))
            .unwrap_or_else(|panic| {
                Err(format!(
                    "the parity post-pass panicked: {}",
                    panic_text(panic.as_ref())
                ))
            })
        })
        .and_then(|report| {
            status.state = PostPassState::Complete;
            status.summary = Some(report.summary_line());
            write_status(config, &status).map(|()| report)
        });
    if let Err(error) = &result {
        let _ = remove_file_if_present(&config.output);
        status.state = PostPassState::Failed;
        status.summary = None;
        status.error = Some(error.clone());
        let _ = write_status(config, &status);
    }
    result
}

fn measure(
    config: &PostPassConfig,
    scope: &BTreeSet<ParityCellId>,
    rows: &[CellResult],
) -> Result<PostPassReport, String> {
    // Refuse before comparing anything if no record could be valid.
    if let Some(cell) = scope.iter().next() {
        ParityRecord::unmeasured(
            cell,
            ParityVerdict::Unavailable,
            UnavailableClass::InfrastructureError,
            Some(ParityOperand::Reference),
            false,
            "configuration probe",
            None,
            None,
            &config.run_id,
            &config.hermit_sha,
        )
        .map_err(|error| format!("parity post-pass configuration: {error}"))?;
    }
    let mut histories: BTreeMap<(String, String), Vec<CellResult>> = BTreeMap::new();
    for row in rows.iter().filter(|row| row.mode == PARITY_MODE) {
        if let Some(backend) = &row.backend {
            histories
                .entry((row.test.clone(), backend.clone()))
                .or_default()
                .push(row.clone());
        }
    }
    for history in histories.values_mut() {
        history.sort_by_key(|row| row.attempt);
    }
    let history = |test: &str, backend: &str| -> Option<&Vec<CellResult>> {
        histories.get(&(test.to_string(), backend.to_string()))
    };
    let rejected = |test: &str, backend: &str| -> Option<&ParityRejection> {
        config
            .rejected
            .get(&(test.to_string(), backend.to_string()))
    };
    let nondeterministic = |test: &str, backend: &str| -> Option<&str> {
        config
            .nondeterministic
            .get(&(test.to_string(), backend.to_string()))
            .map(String::as_str)
    };
    let reference_role = format!("{PARITY_REFERENCE_BACKEND} reference");
    // One test's golden, written once for every candidate that needs it.
    let golden_of = |goldens: &mut BTreeMap<String, Result<Operand, Unusable>>,
                     test: &str,
                     row: &CellResult| {
        goldens
            .entry(test.to_string())
            .or_insert_with(|| reference_golden(config, test, &reference_role, row))
            .clone()
    };

    let mut records: Vec<Option<ParityRecord>> = vec![None; scope.len()];
    let mut comparisons = Vec::new();
    let mut references: BTreeMap<String, Result<CellResult, Unusable>> = BTreeMap::new();
    let mut goldens: BTreeMap<String, Result<Operand, Unusable>> = BTreeMap::new();
    for (index, cell) in scope.iter().enumerate() {
        let test = cell.test_id.as_str();
        let backend = cell.backend.as_str();
        // Decided before a comparison is chosen, so the inputs are not shown
        // to be equal.
        let record = |verdict,
                      class,
                      operand,
                      reason: &str,
                      reference: Option<&Path>,
                      candidate: Option<&Path>| {
            ParityRecord::unmeasured(
                cell,
                verdict,
                class,
                operand,
                false,
                reason,
                reference.map(path_text).as_deref(),
                candidate.map(path_text).as_deref(),
                &config.run_id,
                &config.hermit_sha,
            )
        };
        let unusable_record =
            |unusable: &Unusable, reference: Option<&Path>, candidate: Option<&Path>| {
                record(
                    unusable.verdict,
                    unusable.class,
                    unusable.operand,
                    &unusable.reason,
                    reference,
                    candidate,
                )
            };
        let reference_history = history(test, PARITY_REFERENCE_BACKEND);
        let reference = &*references.entry(cell.test_id.clone()).or_insert_with(|| {
            evaluate_history(
                test,
                ParityOperand::Reference,
                &reference_role,
                reference_history,
                rejected(test, PARITY_REFERENCE_BACKEND),
                nondeterministic(test, PARITY_REFERENCE_BACKEND),
                ParityVerdict::ReferenceMissing,
            )
        });
        // a) The reference left no deterministic golden, so no candidate of
        // this test is compared and neither side's log is read.
        if let Err(unusable) = reference {
            if unusable.class.group() == UnavailableGroup::NoGolden {
                records[index] = Some(unusable_record(unusable, None, None)?);
                continue;
            }
        }
        let candidate_role = format!("{} candidate", cell.backend);
        let candidate_history = history(test, backend);
        let candidate_rejected = rejected(test, backend);
        let candidate_nondeterministic = nondeterministic(test, backend);
        let candidate = evaluate_history(
            test,
            ParityOperand::Candidate,
            &candidate_role,
            candidate_history,
            candidate_rejected,
            candidate_nondeterministic,
            ParityVerdict::CandidateMissing,
        );
        // b) The candidate left no deterministic log, so it is not compared
        // and its logs are not read. A compared backend still names the
        // golden.
        if let Err(unusable) = &candidate {
            if unusable.class.group() == UnavailableGroup::NoGolden {
                let golden = match reference {
                    Ok(row) if cell.backend.inputs_not_equalizable().is_none() => {
                        golden_of(&mut goldens, test, row).ok()
                    }
                    _ => None,
                };
                records[index] = Some(unusable_record(
                    unusable,
                    golden.as_ref().map(|golden| golden.log.as_path()),
                    None,
                )?);
                continue;
            }
        }
        // c) Not compared: whatever its rows or logs, such a backend stops
        // here.
        if let Some(why) = cell.backend.inputs_not_equalizable() {
            records[index] = Some(record(
                ParityVerdict::InputsNotEqualized,
                UnavailableClass::InputsNotEqualized,
                Some(ParityOperand::Candidate),
                why,
                None,
                None,
            )?);
            continue;
        }
        // d) Both operands should have a deterministic log, but the cell
        // cannot be measured. In order: a missing result row, an operand's
        // log (the reference's first), an invalid test id, a HERMIT_EPOCH the
        // operands cannot be shown to share, and a golden that could not be
        // written.
        let candidate_operand = |row: &CellResult| {
            retained_log(
                test,
                ParityOperand::Candidate,
                &candidate_role,
                row,
                ParityVerdict::CandidateMissing,
            )
            .and_then(|log| {
                hashed(&log, ParityGuestInputs::from_result(row)).map_err(|error| Unusable {
                    verdict: ParityVerdict::CandidateMissing,
                    class: UnavailableClass::LogUnreadable,
                    operand: Some(ParityOperand::Candidate),
                    reason: format!(
                        "the {candidate_role} verify cell of {test} retained a log that cannot \
                         be read: {error}"
                    ),
                })
            })
        };
        let (reference_row, candidate_row) = match (reference, &candidate) {
            (Ok(reference_row), Ok(candidate_row)) => (reference_row, candidate_row),
            (Err(unusable), candidate) => {
                let candidate = candidate
                    .as_ref()
                    .ok()
                    .and_then(|row| candidate_operand(row).ok());
                records[index] = Some(unusable_record(
                    unusable,
                    None,
                    candidate.as_ref().map(|operand| operand.log.as_path()),
                )?);
                continue;
            }
            (Ok(reference_row), Err(unusable)) => {
                // A candidate planned elsewhere or not at all: this process
                // retained no ptrace log for it and writes no golden.
                let planned_elsewhere = candidate_history.is_none() && candidate_rejected.is_none();
                let golden = if planned_elsewhere {
                    None
                } else {
                    golden_of(&mut goldens, test, reference_row).ok()
                };
                records[index] = Some(unusable_record(
                    unusable,
                    golden.as_ref().map(|golden| golden.log.as_path()),
                    None,
                )?);
                continue;
            }
        };
        let golden = golden_of(&mut goldens, test, reference_row);
        if let Err(unusable) = &golden {
            if matches!(
                unusable.class,
                UnavailableClass::LogNotRetained | UnavailableClass::LogUnreadable
            ) {
                let candidate = candidate_operand(candidate_row).ok();
                records[index] = Some(unusable_record(
                    unusable,
                    None,
                    candidate.as_ref().map(|operand| operand.log.as_path()),
                )?);
                continue;
            }
        }
        let candidate = match candidate_operand(candidate_row) {
            Ok(candidate) => candidate,
            Err(unusable) => {
                records[index] = Some(unusable_record(
                    &unusable,
                    golden.as_ref().ok().map(|golden| golden.log.as_path()),
                    None,
                )?);
                continue;
            }
        };
        if let Err(unusable) = &golden {
            if unusable.class == UnavailableClass::InvalidTestId {
                records[index] = Some(unusable_record(unusable, None, Some(&candidate.log))?);
                continue;
            }
        }
        let reference_epoch = ParityGuestInputs::from_result(reference_row).epoch;
        let candidate_epoch = candidate.inputs.epoch.clone();
        if reference_epoch.is_none() || reference_epoch != candidate_epoch {
            let operand = match (&reference_epoch, &candidate_epoch) {
                (None, Some(_)) => Some(ParityOperand::Reference),
                (Some(_), None) => Some(ParityOperand::Candidate),
                _ => None,
            };
            let shown =
                |epoch: &Option<String>| epoch.clone().unwrap_or_else(|| "unrecorded".to_string());
            let reason = if reference_epoch.is_some() && candidate_epoch.is_some() {
                "the operands ran with different HERMIT_EPOCH values"
            } else {
                "the operands cannot be shown to share a HERMIT_EPOCH"
            };
            records[index] = Some(record(
                ParityVerdict::Unavailable,
                UnavailableClass::EpochNotShared,
                operand,
                &format!(
                    "{reason}: {PARITY_REFERENCE_BACKEND} reference {}, {} candidate {}",
                    shown(&reference_epoch),
                    cell.backend,
                    shown(&candidate_epoch)
                ),
                golden.as_ref().ok().map(|golden| golden.log.as_path()),
                Some(&candidate.log),
            )?);
            continue;
        }
        let golden = match golden {
            Ok(golden) => golden,
            Err(unusable) => {
                records[index] = Some(unusable_record(&unusable, None, Some(&candidate.log))?);
                continue;
            }
        };
        // e) Compare.
        comparisons.push(Comparison {
            index,
            cell: cell.clone(),
            inputs_equalized: inputs_equalized(&golden.inputs, &candidate.inputs),
            reference: golden,
            candidate,
        });
    }

    let log_diff_runs = AtomicUsize::new(0);
    if !comparisons.is_empty() {
        let logdiff_dir = config.output_dir.join(PARITY_LOGDIFF_DIR);
        fs::create_dir_all(&logdiff_dir)
            .map_err(|error| format!("cannot create {}: {error}", logdiff_dir.display()))?;
        let budget = PostPassDeadline {
            at: Instant::now() + config.budget,
            bound: format!(
                "the parity post-pass budget of {} s",
                config.budget.as_secs()
            ),
        };
        let deadline = match &config.outer_deadline {
            Some(outer) if outer.at < budget.at => outer.clone(),
            _ => budget,
        };
        let next = AtomicUsize::new(0);
        let measured = Mutex::new(Vec::new());
        thread::scope(|threads| {
            for _ in 0..config.jobs.clamp(1, comparisons.len()) {
                threads.spawn(|| {
                    loop {
                        let slot = next.fetch_add(1, Ordering::SeqCst);
                        let Some(comparison) = comparisons.get(slot) else {
                            break;
                        };
                        let record = compare(config, comparison, &deadline, &log_diff_runs);
                        measured
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .push((comparison, record));
                    }
                });
            }
        });
        for (comparison, record) in measured
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
        {
            let record = match record {
                Ok(record) => record,
                Err(error) => unrecorded_comparison(config, comparison, &error)?,
            };
            records[comparison.index] = Some(record);
        }
    }
    let records = records
        .into_iter()
        .zip(scope)
        .map(|(record, cell)| {
            record.ok_or_else(|| format!("parity post-pass produced no record for {cell}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut text = String::new();
    for record in &records {
        text.push_str(&serde_json::to_string(record).map_err(|error| error.to_string())?);
        text.push('\n');
    }
    write_atomically(&config.output, text.as_bytes())?;
    Ok(PostPassReport {
        path: config.output.clone(),
        records,
        log_diff_runs: log_diff_runs.into_inner(),
    })
}

/// The record for a comparison whose report no record can carry. That is
/// this cell's problem, not the post-pass's: the comparison failed, not
/// either side, so it names no operand and counts in the floor as 0.
fn unrecorded_comparison(
    config: &PostPassConfig,
    comparison: &Comparison,
    error: &str,
) -> Result<ParityRecord, String> {
    ParityRecord::unmeasured(
        &comparison.cell,
        ParityVerdict::Unavailable,
        UnavailableClass::LogDiffFailed,
        None,
        comparison.inputs_equalized,
        &format!("the comparison could not be recorded: {error}"),
        Some(&path_text(&comparison.reference.log)),
        Some(&path_text(&comparison.candidate.log)),
        &config.run_id,
        &config.hermit_sha,
    )
}

fn no_result_row(test: &str, role: &str) -> String {
    format!("the {role} verify cell of {test} has no result row in this run")
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// `path` resolved against this process's working directory, for a child
/// that runs in another one.
fn independent_of_cwd(path: &Path) -> Result<PathBuf, String> {
    std::path::absolute(path).map_err(|error| {
        format!(
            "cannot resolve {} against the working directory: {error}",
            path.display()
        )
    })
}

/// A program path as [`independent_of_cwd`] resolves it, except that a name
/// with no `/` byte stays as spelled so `PATH` still finds it. That is the
/// exec rule (`execvp`, and `std::process::Command`'s own classification):
/// any name containing a `/`, such as `hermit/` or `./hermit`, is used as a
/// path and never searched for.
fn program_independent_of_cwd(program: &Path) -> Result<PathBuf, String> {
    use std::os::unix::ffi::OsStrExt;
    if program.as_os_str().as_bytes().contains(&b'/') {
        independent_of_cwd(program)
    } else {
        Ok(program.to_path_buf())
    }
}

/// What a side's lack of a deterministic log means for its cell.
fn no_golden_consequence(operand: ParityOperand) -> &'static str {
    match operand {
        ParityOperand::Reference => "there is no deterministic golden log to compare against",
        ParityOperand::Candidate => "it has no deterministic log to compare",
    }
}

/// Whether `row`, a result row of a verify cell, records that the cell's two
/// runs diverged: its typed result is `determinism-failure`, or one of its
/// attempts retains a current verification report whose verdict is
/// `diverged`, the evidence from which the runner types a verify row
/// `determinism-failure`. The runner consults its terminal evidence first: a
/// row whose terminal error kind is not product-attributed gets no product
/// result, and one with a timed-out attempt is a `timeout`, and either can
/// still retain an attempt whose runs diverged. Such a side has no
/// deterministic log whatever its typed result: decision 1 of
/// <https://github.com/rrnewton/hermit/issues/3301#issuecomment-5895129378>
/// reads "Any mismatch in an operand's attempt history means no golden".
pub fn records_mismatch(row: &CellResult) -> bool {
    row.result == Some(ObservedResult::DeterminismFailure)
        || row
            .attempts
            .iter()
            .any(|attempt| crate::runner::verification_verdict(attempt) == Some(Verdict::Diverged))
}

/// The row of one verify cell whose single retained first-run log is to be
/// compared, or the typed cause that it has none, decided from the typed
/// result fields ([`CellResult::result`], [`CellResult::outcome`] and
/// [`CellResult::error_kind`]) and its attempts' verification reports, and
/// never from reason text. Every cause names `operand`.
///
/// In order:
/// - any attempt of `history` that records a mismatch ([`records_mismatch`]:
///   its result is `determinism-failure`, or one of its attempts retains a
///   `diverged` verification report whatever its typed result), or a mismatch
///   the caller saw outside `history` (`nondeterministic`), is
///   [`UnavailableClass::DeterminismMismatch`] with verdict `nondeterministic`,
///   even when a later attempt passed: such a cell has no deterministic log;
/// - a row the caller rejected (`rejected`) takes the verdict and class the
///   harness gives the same condition ([`ParityRejection::typed`]);
/// - no history, or one [`crate::runner::cell_result_after_retries`] refuses,
///   is [`UnavailableClass::NoResultRow`];
/// - a selected row that passed only the stripped comparison, which is
///   below L2, is [`UnavailableClass::NoResultRow`];
/// - the selected row passed: it is returned. Earlier failures other than a
///   mismatch, such as a timeout, are not evidence that the passing
///   attempt's log is nondeterministic;
/// - the selected row failed with result `crash-error` or `timeout`, did not
///   use the stripped comparator, has exactly one attempt, which retains a
///   strict matched comparison
///   ([`crate::runner::verification_matched_canonically`]), and carries no
///   SaBRe execution-path evidence that the runner calls ineligible
///   ([`crate::runner::execution_path_ineligible`]): it is returned. Its two
///   runs agreed, so its first-run log is deterministic although the cell
///   failed for another reason, such as unexpected stdout, the wrong exit
///   disposition, or a CPU budget reached after both runs completed
///   (<https://github.com/rrnewton/hermit/issues/3455>). One attempt, as
///   every verify row has, keeps the matched comparison and the retained log
///   in the same pair of runs. An earlier row that timed out does not take
///   the log away, and an attempt that diverged in any row was decided by the
///   first item. A row with more than one attempt keeps its own cause below,
///   and so does a SaBRe row whose execution path the runner found incomplete
///   or on fallback or native sites, because its log is no measurement of
///   SaBRe;
/// - otherwise the row's own cause: host-inapplicable, its typed result
///   (timeout, crash, oom, infrastructure-error or sandbox-denied), a `FAIL`
///   with no typed cause ([`UnavailableClass::FailedUntyped`]), or another
///   outcome ([`UnavailableClass::Ended`]).
///
/// `missing` is the verdict for a side that left no log at all:
/// `reference-missing` or `candidate-missing`.
fn evaluate_history(
    test: &str,
    operand: ParityOperand,
    role: &str,
    history: Option<&Vec<CellResult>>,
    rejected: Option<&ParityRejection>,
    nondeterministic: Option<&str>,
    missing: ParityVerdict,
) -> Result<CellResult, Unusable> {
    let unusable = |verdict, class, reason: String| Unusable {
        verdict,
        class,
        operand: Some(operand),
        reason,
    };
    let consequence = no_golden_consequence(operand);
    if let Some(row) = history
        .into_iter()
        .flatten()
        .find(|row| records_mismatch(row))
    {
        let detail = row.reason.as_deref().unwrap_or("no reason recorded");
        let reason = if row.result == Some(ObservedResult::DeterminismFailure) {
            format!(
                "the {role} verify cell of {test} failed determinism on attempt {} (its two \
                 runs diverged), so {consequence}: {detail}",
                row.attempt
            )
        } else {
            let result = row.result.map_or_else(
                || "no typed result".to_string(),
                |result| format!("result {}", result.as_str()),
            );
            format!(
                "the {role} verify cell of {test} retained a diverged verification report on \
                 attempt {} (its two runs diverged) although it ended with {result} ({}), so \
                 {consequence}: {detail}",
                row.attempt,
                row.error_kind.as_deref().unwrap_or("no error kind")
            )
        };
        return Err(unusable(
            ParityVerdict::Nondeterministic,
            UnavailableClass::DeterminismMismatch,
            reason,
        ));
    }
    if let Some(why) = nondeterministic {
        return Err(unusable(
            ParityVerdict::Nondeterministic,
            UnavailableClass::DeterminismMismatch,
            format!("the {role} verify cell of {test} failed determinism: {why}, so {consequence}"),
        ));
    }
    if let Some(rejection) = rejected {
        let (verdict, class) = rejection.typed(operand, missing);
        return Err(unusable(
            verdict,
            class,
            format!("the {role} verify cell of {test}: {}", rejection.reason()),
        ));
    }
    let Some(history) = history else {
        return Err(unusable(
            missing,
            UnavailableClass::NoResultRow,
            no_result_row(test, role),
        ));
    };
    let row = crate::runner::cell_result_after_retries(history).map_err(|error| {
        unusable(
            ParityVerdict::Unavailable,
            UnavailableClass::NoResultRow,
            format!("the {role} verify cell of {test} has no valid result history: {error}"),
        )
    })?;
    let detail = row.reason.as_deref().unwrap_or("no reason recorded");
    let kind = row.error_kind.as_deref().unwrap_or("no error kind");
    let typed = row.result.and_then(|result| match result {
        ObservedResult::Timeout => Some(UnavailableClass::Timeout),
        ObservedResult::CrashError => Some(UnavailableClass::Crash),
        ObservedResult::Oom => Some(UnavailableClass::Oom),
        ObservedResult::InfrastructureError => Some(UnavailableClass::InfrastructureError),
        ObservedResult::SandboxDenied => Some(UnavailableClass::SandboxDenied),
        ObservedResult::Pass
        | ObservedResult::DeterminismFailure
        | ObservedResult::ParityFailure
        | ObservedResult::ReplayFailure => None,
    });
    let result = row.result.map_or_else(
        || "no typed result".to_string(),
        |result| format!("result {}", result.as_str()),
    );
    let stripped = row
        .relaxations
        .iter()
        .any(|relaxation| relaxation.starts_with("comparator=stripped"));
    match (row.outcome.as_str(), typed) {
        // A stripped-comparator PASS is below L2: it does not establish the
        // same-backend canonical determinism a parity comparison stands on.
        ("PASS", _) if stripped => Err(unusable(
            ParityVerdict::Unavailable,
            UnavailableClass::NoResultRow,
            format!(
                "the {role} verify cell of {test} passed only the stripped comparison, which is below L2"
            ),
        )),
        ("PASS", _) => Ok(row.clone()),
        // Its two runs agreed under the strict comparison, so its first-run
        // log is deterministic although the cell failed afterwards: a
        // nonzero exit, unexpected stdout, or a timeout flagged once both
        // runs had completed (https://github.com/rrnewton/hermit/issues/3455).
        // Only a row with exactly one attempt, the shape of every verify row,
        // is admitted, so the matched comparison and the retained log come
        // from the same pair of runs; a mismatch on any row was decided
        // above. A SaBRe row whose execution path the runner found
        // incomplete or on fallback or native sites is no measurement of
        // SaBRe, so it keeps its cause below.
        ("FAIL", Some(UnavailableClass::Crash | UnavailableClass::Timeout))
            if !stripped
                && !crate::runner::execution_path_ineligible(row.execution_path.as_ref())
                && matches!(
                    row.attempts.as_slice(),
                    [attempt] if crate::runner::verification_matched_canonically(attempt)
                ) =>
        {
            Ok(row.clone())
        }
        ("HOST-INAPPLICABLE", _) => Err(unusable(
            missing,
            UnavailableClass::HostInapplicable,
            format!(
                "the {role} verify cell of {test} was host-inapplicable, so it left no log: \
                 {detail}"
            ),
        )),
        (outcome, Some(class)) => Err(unusable(
            ParityVerdict::Unavailable,
            class,
            format!(
                "the {role} verify cell of {test} ended {outcome} with {result} ({kind}): {detail}"
            ),
        )),
        ("FAIL", None) => Err(unusable(
            ParityVerdict::Unavailable,
            UnavailableClass::FailedUntyped,
            format!("the {role} verify cell of {test} failed with {result} ({kind}): {detail}"),
        )),
        (outcome, None) => Err(unusable(
            ParityVerdict::Unavailable,
            UnavailableClass::Ended,
            format!(
                "the {role} verify cell of {test} ended {outcome} with {result} ({kind}): {detail}"
            ),
        )),
    }
}

/// The single retained first-run log of the verify cell `row` that
/// [`evaluate_history`] returned, or why there is none:
/// [`UnavailableClass::LogNotRetained`] when the cell did not retain exactly
/// one nonempty first-run log, and [`UnavailableClass::LogUnreadable`] when
/// its one log exists but cannot be read. Only first-run logs are considered;
/// a second-run log kept for determinism debugging is never read.
fn retained_log(
    test: &str,
    operand: ParityOperand,
    role: &str,
    row: &CellResult,
    missing: ParityVerdict,
) -> Result<PathBuf, Unusable> {
    let unusable = |class, reason: String| Unusable {
        verdict: missing,
        class,
        operand: Some(operand),
        reason,
    };
    let not_retained = |reason: String| unusable(UnavailableClass::LogNotRetained, reason);
    let Some(directory) = row
        .argv
        .iter()
        .position(|arg| arg == VERIFY_LOG_DIR_FLAG)
        .and_then(|flag| row.argv.get(flag + 1))
        .map(PathBuf::from)
    else {
        return Err(not_retained(format!(
            "the {role} verify cell of {test} retained no logs (its argv has no \
             {VERIFY_LOG_DIR_FLAG})"
        )));
    };
    let mut logs = match fs::read_dir(&directory) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(RETAINED_LOG_PREFIX))
            })
            .collect::<Vec<_>>(),
        Err(error) => {
            return Err(not_retained(format!(
                "the {role} verify cell of {test} retained no readable log directory {}: {error}",
                directory.display()
            )));
        }
    };
    logs.sort();
    match logs.as_slice() {
        [log] => match log.metadata() {
            Ok(metadata) if metadata.len() > 0 => Ok(log.clone()),
            Ok(_) => Err(not_retained(format!(
                "the {role} verify cell of {test} retained an empty log {}",
                log.display()
            ))),
            Err(error) => Err(unusable(
                UnavailableClass::LogUnreadable,
                format!(
                    "the {role} verify cell of {test} retained a log that cannot be read: {}: \
                     {error}",
                    log.display()
                ),
            )),
        },
        _ => Err(not_retained(format!(
            "the {role} verify cell of {test} retained {} {RETAINED_LOG_PREFIX}* logs in {}; \
             expected exactly one",
            logs.len(),
            directory.display()
        ))),
    }
}

/// The ptrace golden of `test`, from the reference row `row` that
/// [`evaluate_history`] returned: its single retained log is hashed, then
/// linked or copied below `config.output_dir` beside a sidecar. When that
/// log is gone, a golden already written from the same run, cell attempt and
/// artifact directory is used if it still has its recorded hash: first the
/// one below `config.output_dir`, then the one the harness wrote below
/// `config.artifacts`.
///
/// Its errors are unmeasured classes: the reference's log
/// ([`retained_log`]), a test id that is not a plain relative path
/// ([`UnavailableClass::InvalidTestId`]), or a golden or sidecar that could
/// not be written ([`UnavailableClass::GoldenNotWritten`]).
fn reference_golden(
    config: &PostPassConfig,
    test: &str,
    role: &str,
    row: &CellResult,
) -> Result<Operand, Unusable> {
    let paths = golden_paths(&config.output_dir, test);
    let source = match retained_log(
        test,
        ParityOperand::Reference,
        role,
        row,
        ParityVerdict::ReferenceMissing,
    ) {
        Ok(source) => source,
        Err(unusable) => {
            let reusable = paths.as_ref().ok().and_then(|(golden, sidecar)| {
                existing_golden(golden, sidecar, row).or_else(|| {
                    let (golden, sidecar) = golden_paths(&config.artifacts, test).ok()?;
                    existing_golden(&golden, &sidecar, row)
                })
            });
            return reusable.ok_or(unusable);
        }
    };
    let guest_inputs = ParityGuestInputs::from_result(row);
    let source = hashed(&source, guest_inputs.clone()).map_err(|error| Unusable {
        verdict: ParityVerdict::ReferenceMissing,
        class: UnavailableClass::LogUnreadable,
        operand: Some(ParityOperand::Reference),
        reason: format!(
            "the {role} verify cell of {test} retained a log that cannot be read: {error}"
        ),
    })?;
    let (golden, sidecar_path) = paths.map_err(|reason| Unusable {
        verdict: ParityVerdict::Unavailable,
        class: UnavailableClass::InvalidTestId,
        operand: None,
        reason,
    })?;
    let write = || -> Result<Operand, String> {
        let parent = golden
            .parent()
            .ok_or_else(|| format!("golden {} has no parent", golden.display()))?;
        fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        match fs::remove_file(&golden) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("cannot replace {}: {error}", golden.display())),
        }
        // A hard link costs no disk; a copy is the fallback across filesystems.
        if fs::hard_link(&source.log, &golden).is_err() {
            fs::copy(&source.log, &golden).map_err(|error| {
                format!(
                    "cannot copy {} to {}: {error}",
                    source.log.display(),
                    golden.display()
                )
            })?;
        }
        let operand = hashed(&golden, guest_inputs.clone())?;
        let guest_inputs_sha256 =
            sha256_hex(&serde_json::to_vec(&guest_inputs).map_err(|error| error.to_string())?);
        let sidecar = ParityGoldenSidecar {
            schema: PARITY_GOLDEN_SIDECAR_SCHEMA,
            test_id: test.to_string(),
            backend: PARITY_REFERENCE_BACKEND.to_string(),
            run_id: row.run_id.clone(),
            run_index: row.run_index,
            attempt: row.attempt,
            hermit_sha: row.hermit_sha.clone(),
            binary_sha256: row.binary_sha256.clone(),
            outcome: row.outcome.clone(),
            artifact_dir: row.artifact_dir.clone(),
            source_log: path_text(&source.log),
            log_sha256: operand.sha256.clone(),
            log_bytes: operand.bytes,
            guest_inputs: guest_inputs.clone(),
            guest_inputs_sha256,
        };
        let mut text = serde_json::to_vec_pretty(&sidecar).map_err(|error| error.to_string())?;
        text.push(b'\n');
        write_atomically(&sidecar_path, &text)?;
        Ok(operand)
    };
    write().map_err(|error| Unusable {
        verdict: ParityVerdict::Unavailable,
        class: UnavailableClass::GoldenNotWritten,
        operand: Some(ParityOperand::Reference),
        reason: format!("cannot write the {role} golden: {error}"),
    })
}

/// A golden already written from `row`, if its sidecar still describes it:
/// the same run, attempt and artifact directory, the golden's recorded hash,
/// and the launch `row` records.
fn existing_golden(golden: &Path, sidecar_path: &Path, row: &CellResult) -> Option<Operand> {
    let sidecar: ParityGoldenSidecar =
        serde_json::from_slice(&fs::read(sidecar_path).ok()?).ok()?;
    let operand = hashed(golden, ParityGuestInputs::from_result(row)).ok()?;
    (sidecar.schema == PARITY_GOLDEN_SIDECAR_SCHEMA
        && sidecar.guest_inputs == operand.inputs
        && sidecar.test_id == row.test
        && sidecar.run_id == row.run_id
        && sidecar.attempt == row.attempt
        && sidecar.artifact_dir == row.artifact_dir
        && sidecar.log_sha256 == operand.sha256
        && sidecar.log_bytes == operand.bytes)
        .then_some(operand)
}

/// Run one bounded `hermit log-diff` and turn it into a record.
fn compare(
    config: &PostPassConfig,
    comparison: &Comparison,
    deadline: &PostPassDeadline,
    runs: &AtomicUsize,
) -> Result<ParityRecord, String> {
    let cell = &comparison.cell;
    let reference_log = path_text(&comparison.reference.log);
    let candidate_log = path_text(&comparison.candidate.log);
    // Both operands are deterministic and retained, so every way the
    // comparison fails to run or report is the parity tool's failure: it
    // concerns the comparison, not either side.
    let unavailable = |reason: String| {
        ParityRecord::unmeasured(
            cell,
            ParityVerdict::Unavailable,
            UnavailableClass::LogDiffFailed,
            None,
            comparison.inputs_equalized,
            &reason,
            Some(&reference_log),
            Some(&candidate_log),
            &config.run_id,
            &config.hermit_sha,
        )
    };
    let now = Instant::now();
    if now >= deadline.at {
        return unavailable(format!(
            "{} ran out before this comparison started",
            deadline.bound
        ));
    }
    let base = config.output_dir.join(PARITY_LOGDIFF_DIR);
    let (json, stderr_path) = match (
        plain_relative(&cell.test_id, &format!("@{}.json", cell.backend)),
        plain_relative(&cell.test_id, &format!("@{}.stderr", cell.backend)),
    ) {
        (Ok(json), Ok(stderr)) => (base.join(json), base.join(stderr)),
        (Err(error), _) | (_, Err(error)) => return unavailable(error),
    };
    if let Some(parent) = json.parent() {
        if let Err(error) = fs::create_dir_all(parent) {
            return unavailable(format!("cannot create {}: {error}", parent.display()));
        }
    }
    let _ = fs::remove_file(&json);
    // The child runs in the artifacts directory, so each path it is handed
    // must not depend on this process's working directory: a relative
    // `--results`, `--artifacts`, `E2E_RESULT_ROOT` or `HERMIT_BIN` would
    // otherwise resolve a second time below the artifacts directory. The two
    // logs are operands, which are absolute already.
    let spawn_paths = (|| {
        Ok::<_, String>((
            program_independent_of_cwd(&config.hermit_bin)?,
            independent_of_cwd(&json)?,
            independent_of_cwd(&config.artifacts)?,
        ))
    })();
    let (program, report_path, child_dir) = match spawn_paths {
        Ok(paths) => paths,
        Err(error) => return unavailable(error),
    };
    let timeout = config.log_diff_timeout.min(deadline.at - now);
    runs.fetch_add(1, Ordering::SeqCst);
    let (status, stderr) = match run_bounded(
        Command::new(&program)
            .arg("log-diff")
            .arg(&comparison.reference.log)
            .arg(&comparison.candidate.log)
            .arg("--json")
            .arg(&report_path)
            .args(["--record-envelope", PARITY_RECORD_ENVELOPE])
            .current_dir(&child_dir),
        timeout,
    ) {
        Ok(outcome) => outcome,
        Err(error) => return unavailable(format!("cannot run hermit log-diff: {error}")),
    };
    let _ = fs::write(&stderr_path, &stderr);
    let stderr_tail = bounded(String::from_utf8_lossy(&stderr).trim(), MESSAGE_LIMIT_BYTES);
    let Some(status) = status else {
        return unavailable(format!(
            "hermit log-diff exceeded its {:.1} s bound and was killed",
            timeout.as_secs_f64()
        ));
    };
    let report: LogDiffReport = match fs::read(&json)
        .map_err(|error| error.to_string())
        .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|error| error.to_string()))
    {
        Ok(report) => report,
        Err(error) => {
            return unavailable(format!(
                "hermit log-diff exited {status} without a readable report ({error}); stderr: \
                 {stderr_tail}"
            ));
        }
    };
    let consistent = match status.code() {
        Some(0) => report.verdict == LogDiffVerdict::Matched,
        Some(1) => report.verdict == LogDiffVerdict::Diverged,
        Some(2) => !matches!(
            report.verdict,
            LogDiffVerdict::Matched | LogDiffVerdict::Diverged
        ),
        _ => false,
    };
    if !consistent {
        return unavailable(format!(
            "hermit log-diff exited {status}, which contradicts its report verdict {:?}; \
             stderr: {stderr_tail}",
            report.verdict
        ));
    }
    if matches!(
        report.verdict,
        LogDiffVerdict::Matched | LogDiffVerdict::Diverged
    ) {
        let describes = |input: &crate::logdiff_report::LogDiffInput, operand: &Operand| {
            input.sha256 == operand.sha256 && input.bytes == operand.bytes
        };
        match &report.inputs {
            Some(inputs)
                if describes(&inputs.left, &comparison.reference)
                    && describes(&inputs.right, &comparison.candidate) => {}
            _ => {
                return unavailable(
                    "the log-diff report's inputs are not the compared golden and candidate log"
                        .to_string(),
                );
            }
        }
    }
    ParityRecord::from_comparison(
        cell,
        &report,
        comparison.inputs_equalized,
        &reference_log,
        &candidate_log,
        &config.run_id,
        &config.hermit_sha,
    )
}

/// Run a child with no stdin or stdout, keeping at most
/// [`STDERR_LIMIT_BYTES`] of stderr. `None` status means it was killed at the
/// bound.
fn run_bounded(
    command: &mut Command,
    timeout: Duration,
) -> Result<(Option<ExitStatus>, Vec<u8>), String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    let reader = capped_reader(
        child.stderr.take().ok_or("stderr pipe is missing")?,
        STDERR_LIMIT_BYTES,
    );
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.to_string());
            }
        }
    };
    let stderr = reader.join().unwrap_or_default();
    Ok((status, stderr))
}

/// One side of a comparison: the log at `path`, hashed now, and how the run
/// that wrote it was launched.
fn hashed(path: &Path, inputs: ParityGuestInputs) -> Result<Operand, String> {
    let (log, sha256, bytes) = file_sha256(path)?;
    Ok(Operand {
        log,
        sha256,
        bytes,
        inputs,
    })
}

/// The absolute path, SHA-256 and length of the file at `path`.
fn file_sha256(path: &Path) -> Result<(PathBuf, String, u64), String> {
    let path = &independent_of_cwd(path)?;
    let mut file =
        fs::File::open(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; 1 << 16];
    let mut bytes = 0u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
        bytes += read as u64;
    }
    Ok((path.to_path_buf(), hex(&digest.finalize()), bytes))
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    let name = path
        .file_name()
        .ok_or_else(|| format!("{} has no file name", path.display()))?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));
    fs::write(&temporary, bytes)
        .and_then(|()| fs::rename(&temporary, path))
        .map_err(|error| format!("cannot write {}: {error}", path.display()))
}

// ---- Ledger sources ---------------------------------------------------------
//
// One row per parity cell of one run, for the parity ledger store. hermit
// builds the rows because it owns `ParityRecord` and the scope; dev-hermit's
// `series.py append-parity` wraps each in the `parity-ledger/v1` envelope and
// publishes it. The scorecard reads the published rows back through
// [`ParityLedgerRow`]. See <https://github.com/rrnewton/hermit/issues/3301>.

/// The published envelope's schema.
pub const PARITY_LEDGER_SCHEMA: &str = "parity-ledger/v1";
/// The published envelope's event type.
pub const PARITY_LEDGER_EVENT_TYPE: &str = "parity.record";
/// The lane a pressure-test run's rows are filed under; it has one post-pass.
pub const PRESSURE_TEST_LEDGER_LANE: &str = "pressure-test";
/// The node a pressure-test run's rows are filed under.
pub const PRESSURE_TEST_LEDGER_NODE: &str = "post-pass";

/// One cell's outcome in the ledger: a [`ParityVerdict`], or `record-missing`
/// when the run owed the cell a record and wrote none. Declared from the most
/// to the least adverse, so the derived `Ord` is the deduplication order: when
/// one run reports a cell twice, the smaller verdict wins.
#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    Eq,
    Hash,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize
)]
#[serde(rename_all = "kebab-case")]
pub enum LedgerVerdict {
    RecordMissing,
    Nondeterministic,
    Unavailable,
    ReferenceMissing,
    CandidateMissing,
    InputsNotEqualized,
    Diverged,
    Matched,
}

impl LedgerVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RecordMissing => "record-missing",
            Self::Nondeterministic => "nondeterministic",
            Self::Unavailable => "unavailable",
            Self::ReferenceMissing => "reference-missing",
            Self::CandidateMissing => "candidate-missing",
            Self::InputsNotEqualized => "inputs-not-equalized",
            Self::Diverged => "diverged",
            Self::Matched => "matched",
        }
    }

    pub fn is_measured(self) -> bool {
        matches!(self, Self::Matched | Self::Diverged)
    }

    /// The side a verdict itself names: `reference-missing` and
    /// `candidate-missing` say which log is missing.
    pub fn operand(self) -> Option<ParityOperand> {
        match self {
            Self::ReferenceMissing => Some(ParityOperand::Reference),
            Self::CandidateMissing => Some(ParityOperand::Candidate),
            _ => None,
        }
    }
}

impl From<ParityVerdict> for LedgerVerdict {
    fn from(verdict: ParityVerdict) -> Self {
        match verdict {
            ParityVerdict::Matched => Self::Matched,
            ParityVerdict::Diverged => Self::Diverged,
            ParityVerdict::Nondeterministic => Self::Nondeterministic,
            ParityVerdict::ReferenceMissing => Self::ReferenceMissing,
            ParityVerdict::CandidateMissing => Self::CandidateMissing,
            ParityVerdict::Unavailable => Self::Unavailable,
            ParityVerdict::InputsNotEqualized => Self::InputsNotEqualized,
        }
    }
}

impl fmt::Display for LedgerVerdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a cell of an [`UnavailableClass`] counts. The scorecard's floor
/// divides the credit by every selected cell except the `no-golden` and
/// `not-compared` ones; its mean divides by the measured cells only.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum UnavailableGroup {
    /// An operand's own outcome left no deterministic golden log, so there
    /// was nothing to compare: outside the mean and the floor.
    NoGolden,
    /// The backend's inputs cannot be equalized with the reference's, so it
    /// is never compared: outside the mean and the floor.
    NotCompared,
    /// A golden log could exist but no comparison was made, a harness or
    /// parity-tool defect: in the floor as 0.
    Unmeasured,
    /// The run owed the cell a record and wrote none: in the floor as 0.
    RecordMissing,
}

/// The one cause of a verdict other than `matched` or `diverged`: a closed
/// set, with no umbrella or `other` class. The post-pass derives it from the
/// operands' typed result fields ([`CellResult::result`],
/// [`CellResult::outcome`] and [`CellResult::error_kind`]) and from which
/// step failed, never from reason text; the record's `reason` keeps the
/// specific cause. Declared in the order the scorecard prints the classes,
/// group by group ([`UnavailableClass::group`]).
#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    Eq,
    Hash,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize
)]
#[serde(rename_all = "kebab-case")]
pub enum UnavailableClass {
    // No golden.
    /// An operand's `verify` cell diverged between its two runs on some
    /// attempt (its result `determinism-failure`), so it has no deterministic
    /// log. Only verdict `nondeterministic`.
    DeterminismMismatch,
    /// An operand's final attempt timed out (result `timeout`). A row with
    /// exactly one attempt, whose strict comparison matched, is compared
    /// instead, unless its SaBRe execution path was ineligible.
    Timeout,
    /// An operand's final attempt crashed or exited wrongly (result
    /// `crash-error`). A row with exactly one attempt, whose strict comparison
    /// matched, is compared instead, unless its SaBRe execution path was
    /// ineligible.
    Crash,
    /// An operand's final attempt ran out of memory (result `oom`).
    Oom,
    /// An operand's cell failed for an infrastructure cause (its own typed
    /// result `infrastructure-error`). A caller refusing the cell's evidence
    /// is never this class ([`ParityRejection::typed`]).
    InfrastructureError,
    /// A sandbox denied an operation an operand's cell needed (result
    /// `sandbox-denied`).
    SandboxDenied,
    /// An operand's cell `FAIL`ed with no typed cause parity can name: no
    /// result, or a `replay-failure` or `parity-failure` result. The reason
    /// names the value.
    FailedUntyped,
    /// An operand's cell ended with an outcome other than `PASS`, `FAIL` or
    /// `HOST-INAPPLICABLE`, and no typed cause. The reason names the outcome
    /// and the error kind.
    Ended,
    /// An operand's cell was host-inapplicable, so it left no log: a
    /// `HOST-INAPPLICABLE` row, or a caller's proof of the same
    /// ([`ParityRejection::HostInapplicable`]).
    HostInapplicable,
    // Not compared.
    /// The backend cannot be given the reference's inputs
    /// ([`ParityBackend::inputs_not_equalizable`]). Only verdict
    /// `inputs-not-equalized`, operand `candidate`.
    InputsNotEqualized,
    // Unmeasured.
    /// An operand passed, or failed after a strict matched comparison, but did
    /// not retain exactly one nonempty first-run log (no `--verify-log-dir`,
    /// no readable directory, an empty log, or not exactly one), or the caller
    /// refused its row because it did not retain the verify logs the caller
    /// requires ([`ParityRejection::LogNotRetained`]).
    LogNotRetained,
    /// An operand's single retained log exists but cannot be resolved, read
    /// or hashed. Verdict `reference-missing` or `candidate-missing`.
    LogUnreadable,
    /// An operand has no usable result row: none in the run, a history
    /// [`crate::runner::cell_result_after_retries`] refuses, or a row the
    /// caller refused as missing or invalid ([`ParityRejection::MissingRow`],
    /// [`ParityRejection::InvalidRow`]), or a PASS that used only the stripped
    /// comparator, which is below L2.
    NoResultRow,
    /// The parity tool could not produce a usable comparison of two
    /// deterministic, retained logs. Three cases: (1) `hermit log-diff`
    /// returned a non-verdict report -- `NoResult`, `Refused`,
    /// `NoComparableMessages` or `IdenticalSoFar` (`IdenticalSoFar` should be
    /// impossible in one-shot mode, so seeing it means a tool defect); (2) a
    /// matched or diverged report failed
    /// [`LogDiffReport::require_cross_backend_evidence`]; (3) a report
    /// [`ParityRecord::from_comparison`] cannot record. Every way the
    /// comparison itself fails to run or report (its deadline, its output
    /// directory, its paths, the spawn, its time bound, an unreadable or
    /// contradictory report, a report over the wrong inputs) is also this
    /// class. Verdict `unavailable`, no operand, no credit; in the floor as 0.
    LogDiffFailed,
    /// The test id is not a plain relative path, so no golden can be
    /// written below it. No operand.
    InvalidTestId,
    /// The reference's golden log or its sidecar could not be written.
    /// Operand `reference`.
    GoldenNotWritten,
    /// The operands cannot be shown to share a `HERMIT_EPOCH`: operand the
    /// side that recorded none when exactly one did not, no operand when
    /// both recorded different values or neither recorded one.
    EpochNotShared,
    // Ledger.
    /// The run owed the cell a record and wrote none. Only verdict
    /// `record-missing`, no operand; a record never carries it.
    RecordMissing,
}

impl UnavailableClass {
    pub const ALL: [Self; 18] = [
        Self::DeterminismMismatch,
        Self::Timeout,
        Self::Crash,
        Self::Oom,
        Self::InfrastructureError,
        Self::SandboxDenied,
        Self::FailedUntyped,
        Self::Ended,
        Self::HostInapplicable,
        Self::InputsNotEqualized,
        Self::LogNotRetained,
        Self::LogUnreadable,
        Self::NoResultRow,
        Self::LogDiffFailed,
        Self::InvalidTestId,
        Self::GoldenNotWritten,
        Self::EpochNotShared,
        Self::RecordMissing,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::DeterminismMismatch => "determinism-mismatch",
            Self::Timeout => "timeout",
            Self::Crash => "crash",
            Self::Oom => "oom",
            Self::InfrastructureError => "infrastructure-error",
            Self::SandboxDenied => "sandbox-denied",
            Self::FailedUntyped => "failed-untyped",
            Self::Ended => "ended",
            Self::HostInapplicable => "host-inapplicable",
            Self::InputsNotEqualized => "inputs-not-equalized",
            Self::LogNotRetained => "log-not-retained",
            Self::LogUnreadable => "log-unreadable",
            Self::NoResultRow => "no-result-row",
            Self::LogDiffFailed => "log-diff-failed",
            Self::InvalidTestId => "invalid-test-id",
            Self::GoldenNotWritten => "golden-not-written",
            Self::EpochNotShared => "epoch-not-shared",
            Self::RecordMissing => "record-missing",
        }
    }

    pub fn group(self) -> UnavailableGroup {
        match self {
            Self::DeterminismMismatch
            | Self::Timeout
            | Self::Crash
            | Self::Oom
            | Self::InfrastructureError
            | Self::SandboxDenied
            | Self::FailedUntyped
            | Self::Ended
            | Self::HostInapplicable => UnavailableGroup::NoGolden,
            Self::InputsNotEqualized => UnavailableGroup::NotCompared,
            Self::LogNotRetained
            | Self::LogUnreadable
            | Self::NoResultRow
            | Self::LogDiffFailed
            | Self::InvalidTestId
            | Self::GoldenNotWritten
            | Self::EpochNotShared => UnavailableGroup::Unmeasured,
            Self::RecordMissing => UnavailableGroup::RecordMissing,
        }
    }

    /// The verdicts the class may go with.
    pub fn verdicts(self) -> &'static [LedgerVerdict] {
        match self {
            Self::DeterminismMismatch => &[LedgerVerdict::Nondeterministic],
            Self::InputsNotEqualized => &[LedgerVerdict::InputsNotEqualized],
            Self::RecordMissing => &[LedgerVerdict::RecordMissing],
            Self::LogUnreadable => &[
                LedgerVerdict::ReferenceMissing,
                LedgerVerdict::CandidateMissing,
            ],
            _ => &[
                LedgerVerdict::Unavailable,
                LedgerVerdict::ReferenceMissing,
                LedgerVerdict::CandidateMissing,
            ],
        }
    }

    /// The operands the class may name; `None` is no operand.
    pub fn operands(self) -> &'static [Option<ParityOperand>] {
        const EITHER: &[Option<ParityOperand>] = &[
            Some(ParityOperand::Reference),
            Some(ParityOperand::Candidate),
        ];
        match self {
            Self::InputsNotEqualized => &[Some(ParityOperand::Candidate)],
            Self::LogDiffFailed | Self::InvalidTestId | Self::RecordMissing => &[None],
            Self::GoldenNotWritten => &[Some(ParityOperand::Reference)],
            Self::EpochNotShared => &[
                Some(ParityOperand::Reference),
                Some(ParityOperand::Candidate),
                None,
            ],
            _ => EITHER,
        }
    }
}

impl fmt::Display for UnavailableClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One side of a parity comparison.
#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    Eq,
    Hash,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize
)]
#[serde(rename_all = "lowercase")]
pub enum ParityOperand {
    /// The ptrace reference.
    Reference,
    /// The candidate backend.
    Candidate,
}

impl ParityOperand {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reference => "reference",
            Self::Candidate => "candidate",
        }
    }
}

impl fmt::Display for ParityOperand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

fn shown_operand(operand: Option<ParityOperand>) -> &'static str {
    operand.map_or("null", ParityOperand::as_str)
}

/// The typed reason a record or ledger row gives for its verdict.
///
/// `class` is `None` exactly on a measured (`matched` or `diverged`) verdict.
/// Otherwise it is one class of the closed set, it fits the verdict
/// ([`UnavailableClass::verdicts`]: `determinism-mismatch` goes with exactly
/// `nondeterministic`, and so on), and `operand` is a side the class may
/// concern ([`UnavailableClass::operands`]), which a `reference-missing` or
/// `candidate-missing` verdict also names. A backend declared
/// [`ParityBackend::inputs_not_equalizable`] is never compared: it has no
/// measured verdict and no unmeasured class, and no other backend has an
/// `inputs-not-equalized` verdict. The reason text is never read. dev-hermit's
/// `ci-hub/series/parity_ledger.py` `check_class`, added by slice D5 of
/// <https://github.com/rrnewton/hermit/issues/3301>, makes the same checks
/// with the same messages.
pub fn check_class(
    at: &str,
    backend: ParityBackend,
    verdict: LedgerVerdict,
    class: Option<UnavailableClass>,
    operand: Option<ParityOperand>,
) -> Result<(), String> {
    let why = backend.inputs_not_equalizable();
    if verdict == LedgerVerdict::InputsNotEqualized && why.is_none() {
        return Err(format!(
            "{at}: {backend} inputs can be equalized; a comparison with unequal inputs is \
             measured and reports unequalized_credit"
        ));
    }
    if verdict.is_measured() {
        if let Some(why) = why {
            return Err(format!(
                "{at}: {backend} inputs cannot be equalized, so it reports no measured \
                 comparison and never equal inputs: {why}"
            ));
        }
        if let Some(class) = class {
            return Err(format!(
                "{at}: a {verdict} verdict is measured and carries no unavailable_class, \
                 got {class}"
            ));
        }
        if let Some(operand) = operand {
            return Err(format!(
                "{at}: a {verdict} verdict is measured and names no operand, got {operand}"
            ));
        }
        return Ok(());
    }
    let Some(class) = class else {
        return Err(format!(
            "{at}: verdict {verdict} needs an unavailable_class"
        ));
    };
    let verdicts = class.verdicts();
    if !verdicts.contains(&verdict) {
        return Err(format!(
            "{at}: unavailable_class {class} cannot go with verdict {verdict}; it needs {}",
            verdicts
                .iter()
                .map(|verdict| verdict.as_str())
                .collect::<Vec<_>>()
                .join(" or ")
        ));
    }
    if why.is_some() && class.group() == UnavailableGroup::Unmeasured {
        return Err(format!(
            "{at}: {backend} inputs cannot be equalized, so no comparison is attempted and \
             it carries no unmeasured class, got {class}"
        ));
    }
    let operands = class.operands();
    if !operands.contains(&operand) {
        return Err(format!(
            "{at}: unavailable_class {class} needs operand {}, got {}",
            operands
                .iter()
                .map(|side| shown_operand(*side))
                .collect::<Vec<_>>()
                .join(" or "),
            shown_operand(operand)
        ));
    }
    if let Some(side) = verdict.operand() {
        if operand != Some(side) {
            return Err(format!(
                "{at}: verdict {verdict} names the {side} operand, got {}",
                shown_operand(operand)
            ));
        }
    }
    Ok(())
}

/// How far the node's post-pass got, as the ledger records it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LedgerPostPassState {
    Complete,
    Failed,
    Running,
    /// The node left no [`PARITY_STATUS_JSON`] at all.
    Absent,
    /// [`node_ledger_sources`] refused the node's outputs, so
    /// [`expected_ledger_sources`] owes each of its expected cells a
    /// `record-missing` row naming the refusal.
    Refused,
}

impl From<PostPassState> for LedgerPostPassState {
    fn from(state: PostPassState) -> Self {
        match state {
            PostPassState::Running => Self::Running,
            PostPassState::Complete => Self::Complete,
            PostPassState::Failed => Self::Failed,
        }
    }
}

/// Where the list of cells a node owed came from.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LedgerScopeSource {
    /// The status's own `scope` list (schema 2).
    StatusScope,
    /// A complete schema-1 status, checked by its `cells` count only.
    StatusCount,
    /// The caller's expected scope for the node.
    ExpectedScope,
}

/// Which node's post-pass a row came from, and the evidence behind it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ParityLedgerOrigin {
    pub lane: String,
    pub node: String,
    pub post_pass_state: LedgerPostPassState,
    /// SHA-256 of the node's [`PARITY_STATUS_JSON`] bytes; `null` when absent.
    pub status_sha256: Option<String>,
    /// SHA-256 of the node's [`PARITY_JSONL`] bytes; `null` when it has none.
    pub records_sha256: Option<String>,
    pub hermit_bin_sha256: Option<String>,
    pub scope_source: LedgerScopeSource,
}

/// One row [`ledger_sources`] builds: everything in the published envelope
/// except the fields the dev-hermit writer adds (`schema`, `event_type`,
/// `event_id`, `team`, `host`, `emitted_at`, `producer`, `source_tree_dirty`).
/// `run_id` and `hermit_sha` are the node status's own, `null` for a node
/// that left no status; the writer checks them against `--run-id` and
/// `--tree` before it uses its own.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ParityLedgerSource {
    /// `<test_id as emitted>@<backend>`.
    pub cell: String,
    pub test_id: String,
    pub backend: ParityBackend,
    pub verdict: LedgerVerdict,
    /// The record's [`ParityRecord::unavailable_class`], or
    /// [`UnavailableClass::RecordMissing`] for a `record-missing` row.
    /// Required, `null` included.
    #[serde(deserialize_with = "present")]
    pub unavailable_class: Option<UnavailableClass>,
    /// The record's [`ParityRecord::operand`]; `None` for `record-missing`.
    /// Required, `null` included.
    #[serde(deserialize_with = "present")]
    pub operand: Option<ParityOperand>,
    pub reason: Option<String>,
    pub run_id: Option<String>,
    pub hermit_sha: Option<String>,
    pub source: ParityLedgerOrigin,
    /// The [`ParityRecord`] as written in `parity.jsonl` (its line is
    /// checked to be the canonical encoding, so re-encoding it reproduces the
    /// same bytes); `null` exactly when the verdict is `record-missing`.
    pub record: Option<ParityRecord>,
}

/// The producers that publish parity rows.
#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    Eq,
    Hash,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize
)]
#[serde(rename_all = "kebab-case")]
pub enum ParityProducer {
    Validate,
    PressureTest,
}

impl ParityProducer {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Validate => "validate",
            Self::PressureTest => "pressure-test",
        }
    }
}

impl fmt::Display for ParityProducer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One published row of the parity ledger store, `parity-ledger/v1`: a
/// [`ParityLedgerSource`] wrapped by dev-hermit's writer.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ParityLedgerRow {
    pub schema: String,
    pub event_type: String,
    pub event_id: String,
    pub team: String,
    pub host: String,
    pub emitted_at: String,
    pub producer: ParityProducer,
    pub run_id: String,
    pub hermit_sha: String,
    pub source_tree_dirty: bool,
    pub cell: String,
    pub test_id: String,
    pub backend: ParityBackend,
    pub verdict: LedgerVerdict,
    /// [`ParityLedgerSource::unavailable_class`]. Required, `null` included.
    #[serde(deserialize_with = "present")]
    pub unavailable_class: Option<UnavailableClass>,
    /// [`ParityLedgerSource::operand`]. Required, `null` included.
    #[serde(deserialize_with = "present")]
    pub operand: Option<ParityOperand>,
    pub reason: Option<String>,
    pub source: ParityLedgerOrigin,
    pub record: Option<ParityRecord>,
}

/// `sha256(producer \0 run_id \0 cell \0 lane \0 node)`, the envelope's
/// `event_id`.
pub fn parity_event_id(
    producer: ParityProducer,
    run_id: &str,
    cell: &str,
    lane: &str,
    node: &str,
) -> String {
    sha256_hex(format!("{producer}\0{run_id}\0{cell}\0{lane}\0{node}").as_bytes())
}

fn is_sha1_hex(text: &str) -> bool {
    text.len() == 40
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// A UTC instant read by [`parse_utc_timestamp`]. It orders by the instant
/// it names, so two spellings of one instant are equal and a later instant
/// is greater whatever its spelling.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct UtcInstant {
    /// Days since 1970-01-01 in the proleptic Gregorian calendar.
    days: i64,
    /// Seconds into the day: 86400 only for a leap second.
    second: u32,
    nanosecond: u32,
}

/// Read `text` as an RFC 3339 timestamp in UTC:
/// `YYYY-MM-DDTHH:MM:SS[.F]Z`, where `F` is 1 to 9 digits and `+00:00` may
/// stand for `Z`. Every field is range-checked (the day against its month's
/// length, leap years included), second 60 is accepted only at 23:59, where
/// a leap second falls, and anything else -- another offset, no offset, a
/// space for `T`, lower case, trailing text -- is refused. A row's
/// `emitted_at` is compared by the instant this returns, never as text: as
/// text, `...:00.5Z` sorts before `...:00Z`.
pub fn parse_utc_timestamp(text: &str) -> Result<UtcInstant, String> {
    let bytes = text.as_bytes();
    let number = |from: usize, to: usize| -> Result<u32, String> {
        let digits = bytes
            .get(from..to)
            .filter(|digits| digits.iter().all(u8::is_ascii_digit))
            .ok_or_else(|| format!("{text:?} has no digits at bytes {from}..{to}"))?;
        Ok(digits
            .iter()
            .fold(0, |value, digit| value * 10 + u32::from(digit - b'0')))
    };
    let separator = |at: usize, expected: u8| -> Result<(), String> {
        if bytes.get(at) == Some(&expected) {
            Ok(())
        } else {
            Err(format!(
                "{text:?} does not have {:?} at byte {at}",
                char::from(expected)
            ))
        }
    };
    let year = number(0, 4)?;
    separator(4, b'-')?;
    let month = number(5, 7)?;
    separator(7, b'-')?;
    let day = number(8, 10)?;
    separator(10, b'T')?;
    let hour = number(11, 13)?;
    separator(13, b':')?;
    let minute = number(14, 16)?;
    separator(16, b':')?;
    let second = number(17, 19)?;
    let mut at = 19;
    let mut nanosecond = 0u32;
    if bytes.get(at) == Some(&b'.') {
        let digits = bytes[at + 1..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if !(1..=9).contains(&digits) {
            return Err(format!(
                "{text:?} has {digits} fractional-second digit(s); RFC 3339 needs 1 to 9 here"
            ));
        }
        nanosecond = number(at + 1, at + 1 + digits)? * 10u32.pow(9 - digits as u32);
        at += 1 + digits;
    }
    match &bytes[at..] {
        b"Z" | b"+00:00" => {}
        zone => {
            return Err(format!(
                "{text:?} ends in {:?}, not the UTC designator Z (or +00:00)",
                String::from_utf8_lossy(zone)
            ));
        }
    }
    let leap_year = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let month_days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap_year => 29,
        2 => 28,
        _ => return Err(format!("{text:?} has month {month}")),
    };
    if !(1..=month_days).contains(&day) {
        return Err(format!(
            "{text:?} has day {day}, but {year:04}-{month:02} has {month_days} days"
        ));
    }
    if hour > 23 || minute > 59 {
        return Err(format!("{text:?} has time {hour:02}:{minute:02}"));
    }
    if second > 60 || (second == 60 && (hour, minute) != (23, 59)) {
        return Err(format!(
            "{text:?} has second {second}; 60 is a leap second, only at 23:59"
        ));
    }
    // Days from 1970-01-01 to the civil date (Howard Hinnant's
    // days_from_civil), with the year starting in March so February's
    // leap day is the last day of the shifted year.
    let (year, month, day) = (i64::from(year), i64::from(month), i64::from(day));
    let shifted = if month <= 2 { year - 1 } else { year };
    let era = shifted.div_euclid(400);
    let year_of_era = shifted - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Ok(UtcInstant {
        days: era * 146_097 + day_of_era - 719_468,
        second: hour * 3600 + minute * 60 + second,
        nanosecond,
    })
}

/// The checks a source row and a published row share: the cell names its
/// test and backend, a reason is given for every verdict but `matched` and
/// `diverged`, the class and operand fit the verdict ([`check_class`]), the
/// record is present exactly when the verdict is not `record-missing` and
/// agrees with the row (verdict, cell, class, operand, reason, run and tree),
/// and the record passes [`ParityRecord::validate`] (every credit invariant).
#[allow(clippy::too_many_arguments)]
fn check_ledger_fields(
    at: &str,
    cell: &str,
    test_id: &str,
    backend: ParityBackend,
    verdict: LedgerVerdict,
    class: Option<UnavailableClass>,
    operand: Option<ParityOperand>,
    reason: Option<&str>,
    run_id: Option<&str>,
    hermit_sha: Option<&str>,
    source: &ParityLedgerOrigin,
    record: Option<&ParityRecord>,
) -> Result<(), String> {
    if cell != format!("{test_id}@{backend}") {
        return Err(format!("{at}: cell {cell:?} is not {test_id}@{backend}"));
    }
    if source.lane.is_empty() || source.node.is_empty() {
        return Err(format!("{at}: source lane and node must be non-empty"));
    }
    if !verdict.is_measured() && reason.is_none_or(|reason| reason.trim().is_empty()) {
        return Err(format!("{at}: verdict {verdict} needs a reason"));
    }
    check_class(at, backend, verdict, class, operand)?;
    match source.post_pass_state {
        LedgerPostPassState::Absent if verdict != LedgerVerdict::RecordMissing => {
            return Err(format!(
                "{at}: a node that left no status can only owe record-missing rows, got {verdict}"
            ));
        }
        LedgerPostPassState::Refused if verdict != LedgerVerdict::RecordMissing => {
            return Err(format!(
                "{at}: a node whose outputs were refused can only owe record-missing rows, \
                 got {verdict}"
            ));
        }
        _ => {}
    }
    match (verdict, record) {
        (LedgerVerdict::RecordMissing, None) => Ok(()),
        (LedgerVerdict::RecordMissing, Some(_)) => {
            Err(format!("{at}: a record-missing row carries no record"))
        }
        (_, None) => Err(format!("{at}: verdict {verdict} needs its record")),
        (_, Some(record)) => {
            record
                .validate()
                .map_err(|error| format!("{at}: {error}"))?;
            if LedgerVerdict::from(record.verdict) != verdict {
                return Err(format!(
                    "{at}: row verdict {verdict} disagrees with record verdict {}",
                    LedgerVerdict::from(record.verdict)
                ));
            }
            if record.test_id != test_id || record.backend != backend {
                return Err(format!(
                    "{at}: record names {}@{}",
                    record.test_id, record.backend
                ));
            }
            if record.unavailable_class != class {
                let shown = |class: Option<UnavailableClass>| {
                    class.map_or("null", UnavailableClass::as_str)
                };
                return Err(format!(
                    "{at}: row unavailable_class {} disagrees with the record's {}",
                    shown(class),
                    shown(record.unavailable_class)
                ));
            }
            if record.operand != operand {
                return Err(format!(
                    "{at}: row operand {} disagrees with the record's {}",
                    shown_operand(operand),
                    shown_operand(record.operand)
                ));
            }
            if record.reason.as_deref() != reason {
                return Err(format!("{at}: row reason disagrees with the record's"));
            }
            if run_id.is_some_and(|run_id| run_id != record.run_id) {
                return Err(format!(
                    "{at}: row run_id {:?} disagrees with record run_id {:?}",
                    run_id.unwrap_or_default(),
                    record.run_id
                ));
            }
            if hermit_sha.is_some_and(|sha| sha != record.hermit_sha) {
                return Err(format!(
                    "{at}: row hermit_sha {:?} disagrees with record hermit_sha {:?}",
                    hermit_sha.unwrap_or_default(),
                    record.hermit_sha
                ));
            }
            Ok(())
        }
    }
}

impl ParityLedgerSource {
    pub fn validate(&self) -> Result<(), String> {
        check_ledger_fields(
            &format!("parity ledger source {}", self.cell),
            &self.cell,
            &self.test_id,
            self.backend,
            self.verdict,
            self.unavailable_class,
            self.operand,
            self.reason.as_deref(),
            self.run_id.as_deref(),
            self.hermit_sha.as_deref(),
            &self.source,
            self.record.as_ref(),
        )
    }
}

impl ParityLedgerRow {
    /// Refuse a row that breaks the envelope contract: the schema and event
    /// type, a 40-hex `hermit_sha`, an `emitted_at` that
    /// [`parse_utc_timestamp`] reads, the `event_id` derivation, the checks
    /// shared with [`ParityLedgerSource::validate`], and a record whose run
    /// and tree are the row's own.
    pub fn validate(&self) -> Result<(), String> {
        let at = format!(
            "parity ledger row {} ({} run {})",
            self.cell, self.producer, self.run_id
        );
        if self.schema != PARITY_LEDGER_SCHEMA {
            return Err(format!(
                "{at}: schema must be {PARITY_LEDGER_SCHEMA}, got {:?}",
                self.schema
            ));
        }
        if self.event_type != PARITY_LEDGER_EVENT_TYPE {
            return Err(format!(
                "{at}: event_type must be {PARITY_LEDGER_EVENT_TYPE}, got {:?}",
                self.event_type
            ));
        }
        if self.run_id.trim().is_empty() {
            return Err(format!("{at}: run_id must be non-empty"));
        }
        if !is_sha1_hex(&self.hermit_sha) {
            return Err(format!(
                "{at}: hermit_sha must be 40 lowercase hex digits, got {:?}",
                self.hermit_sha
            ));
        }
        parse_utc_timestamp(&self.emitted_at)
            .map_err(|error| format!("{at}: emitted_at is not an RFC 3339 UTC time: {error}"))?;
        let expected = parity_event_id(
            self.producer,
            &self.run_id,
            &self.cell,
            &self.source.lane,
            &self.source.node,
        );
        if self.event_id != expected {
            return Err(format!(
                "{at}: event_id {} is not sha256(producer, run_id, cell, lane, node) = {expected}",
                self.event_id
            ));
        }
        check_ledger_fields(
            &at,
            &self.cell,
            &self.test_id,
            self.backend,
            self.verdict,
            self.unavailable_class,
            self.operand,
            self.reason.as_deref(),
            Some(&self.run_id),
            Some(&self.hermit_sha),
            &self.source,
            self.record.as_ref(),
        )
    }
}

/// The cells each node of a run was expected to report, keyed by
/// `<lane>/<node>` (the node's directory below the e2e result root). A
/// caller that planned the nodes builds it; a node with no
/// [`PARITY_STATUS_JSON`] owes each of its cells a `record-missing` row.
pub type ExpectedScope = BTreeMap<String, BTreeSet<ParityCellId>>;

/// Parse an expected scope file: a JSON object from `<lane>/<node>` to a list
/// of `<test>@<backend>` cells.
pub fn parse_expected_scope(text: &str) -> Result<ExpectedScope, String> {
    let raw: BTreeMap<String, Vec<String>> = serde_json::from_str(text)
        .map_err(|error| format!("expected parity scope is not a JSON object of lists: {error}"))?;
    raw.into_iter()
        .map(|(node, cells)| {
            let parts = node.split('/').collect::<Vec<_>>();
            if parts.len() != 2
                || parts
                    .iter()
                    .any(|part| part.is_empty() || *part == "." || *part == "..")
            {
                return Err(format!(
                    "expected parity scope key {node:?} is not <lane>/<node>"
                ));
            }
            let mut set = BTreeSet::new();
            for text in &cells {
                if !set.insert(parse_cell_text(text)?) {
                    return Err(format!("expected parity scope {node} names {text} twice"));
                }
            }
            Ok((node, set))
        })
        .collect()
}

/// The ledger rows of one validate run: one per cell each node's post-pass
/// owed, read from every `<lane>/<node>/`[`PARITY_STATUS_JSON`] below
/// `e2e_root` and its sibling [`PARITY_JSONL`].
///
/// - `complete`: every record is validated, and there must be exactly
///   `cells` of them (and exactly the status's `scope`, when it has one).
/// - `failed` or `running`: the records that exist, plus a `record-missing`
///   row for every other cell of the status's scope (or, for a schema-1
///   status, of the node's expected scope), with the reason
///   `parity post-pass <state>: <error>`.
/// - A node of `expected` with no status owes each of its cells a
///   `record-missing` row with `post_pass_state` `absent`.
/// - A cell of a node's expected scope that its status's scope leaves out
///   (or, for a complete schema-1 status, that no record names) owes a
///   `record-missing` row with `scope_source` `expected-scope`.
///
/// Anything that cannot be accounted for exactly is refused rather than
/// guessed: an unreadable or malformed file, a record that fails
/// [`ParityRecord::validate`] or is not the canonical encoding of itself, a
/// record from another run, a count or scope mismatch, a records file with
/// no status, or an unfinished schema-1 status with no expected scope.
pub fn ledger_sources(
    e2e_root: &Path,
    expected: Option<&ExpectedScope>,
) -> Result<Vec<ParityLedgerSource>, String> {
    let (nodes, unlisted) = run_nodes(e2e_root, expected)?;
    if let Some(error) = unlisted.into_iter().next() {
        return Err(error);
    }
    let mut rows = Vec::new();
    for (key, (lane, node, directory)) in &nodes {
        let owed = expected.and_then(|expected| expected.get(key));
        rows.extend(node_ledger_sources(directory, lane, node, owed)?);
    }
    Ok(rows)
}

/// A run's nodes, keyed `<lane>/<node>`: every node directory below
/// `e2e_root`, and every node `expected` names whether or not it has a
/// directory. A directory that cannot be listed, or whose name is not UTF-8,
/// is returned as an error line beside the nodes rather than ending the
/// walk; only a malformed `expected` key is an error.
#[allow(clippy::type_complexity)]
fn run_nodes(
    e2e_root: &Path,
    expected: Option<&ExpectedScope>,
) -> Result<(BTreeMap<String, (String, String, PathBuf)>, Vec<String>), String> {
    let mut nodes = BTreeMap::<String, (String, String, PathBuf)>::new();
    let mut unlisted = Vec::new();
    let lanes = sorted_directories(e2e_root).unwrap_or_else(|error| {
        unlisted.push(error);
        Vec::new()
    });
    for lane in lanes {
        let lane_nodes =
            directory_name(&lane).and_then(|lane_name| Ok((lane_name, sorted_directories(&lane)?)));
        let (lane_name, lane_nodes) = match lane_nodes {
            Ok(listed) => listed,
            Err(error) => {
                unlisted.push(error);
                continue;
            }
        };
        for node in lane_nodes {
            match directory_name(&node) {
                Ok(node_name) => {
                    nodes.insert(
                        format!("{lane_name}/{node_name}"),
                        (lane_name.clone(), node_name, node),
                    );
                }
                Err(error) => unlisted.push(error),
            }
        }
    }
    for key in expected.into_iter().flat_map(|expected| expected.keys()) {
        if !nodes.contains_key(key) {
            let (lane, node) = key
                .split_once('/')
                .ok_or_else(|| format!("expected parity scope key {key:?} is not <lane>/<node>"))?;
            nodes.insert(
                key.clone(),
                (
                    lane.to_string(),
                    node.to_string(),
                    e2e_root.join(lane).join(node),
                ),
            );
        }
    }
    Ok((nodes, unlisted))
}

/// The ledger rows of one post-pass whose [`PARITY_STATUS_JSON`] and
/// [`PARITY_JSONL`] sit directly in `directory`, filed under `lane` and
/// `node`; [`ledger_sources`] applies it to each node, and the pressure test
/// to its results directory. `expected` is the node's expected scope, if the
/// caller knows it.
pub fn node_ledger_sources(
    directory: &Path,
    lane: &str,
    node: &str,
    expected: Option<&BTreeSet<ParityCellId>>,
) -> Result<Vec<ParityLedgerSource>, String> {
    let status_path = directory.join(PARITY_STATUS_JSON);
    let records_path = directory.join(PARITY_JSONL);
    let status_bytes = read_if_present(&status_path)?;
    let records_bytes = read_if_present(&records_path)?;
    let Some(status_bytes) = status_bytes else {
        if records_bytes.is_some() {
            return Err(format!(
                "{} has no {PARITY_STATUS_JSON} beside it, so its records cannot be attributed",
                records_path.display()
            ));
        }
        let origin = ParityLedgerOrigin {
            lane: lane.to_string(),
            node: node.to_string(),
            post_pass_state: LedgerPostPassState::Absent,
            status_sha256: None,
            records_sha256: None,
            hermit_bin_sha256: None,
            scope_source: LedgerScopeSource::ExpectedScope,
        };
        let reason = format!("no parity row: node {lane}/{node} left no {PARITY_STATUS_JSON}");
        return expected
            .into_iter()
            .flatten()
            .map(|cell| missing_row(cell, &reason, None, &origin))
            .collect();
    };
    let at = status_path.display().to_string();
    let status: PostPassStatus = serde_json::from_slice(&status_bytes)
        .map_err(|error| format!("{at} is not a parity status: {error}"))?;
    let scope = status
        .checked_scope()
        .map_err(|error| format!("{at}: {error}"))?;
    if status.run_id.trim().is_empty() || !is_sha1_hex(&status.hermit_sha) {
        return Err(format!(
            "{at}: run_id must be non-empty and hermit_sha 40 lowercase hex digits, got {:?} \
             at {:?}",
            status.run_id, status.hermit_sha
        ));
    }
    let mut records = Vec::new();
    if let Some(bytes) = &records_bytes {
        let text = std::str::from_utf8(bytes)
            .map_err(|error| format!("{} is not UTF-8: {error}", records_path.display()))?;
        let mut seen = BTreeSet::new();
        for (index, line) in text.lines().enumerate() {
            let line_at = format!("{} line {}", records_path.display(), index + 1);
            let record: ParityRecord = serde_json::from_str(line)
                .map_err(|error| format!("{line_at} is not a parity record: {error}"))?;
            record
                .validate()
                .map_err(|error| format!("{line_at}: {error}"))?;
            let canonical = serde_json::to_string(&record).map_err(|error| error.to_string())?;
            if canonical != line {
                return Err(format!(
                    "{line_at} is not the canonical encoding of its record, so it cannot be \
                     published byte-for-byte"
                ));
            }
            if record.run_id != status.run_id || record.hermit_sha != status.hermit_sha {
                return Err(format!(
                    "{line_at} belongs to run {} at {}, not the status's run {} at {}",
                    record.run_id, record.hermit_sha, status.run_id, status.hermit_sha
                ));
            }
            let cell = ParityCellId {
                test_id: record.test_id.clone(),
                backend: record.backend,
            };
            if !seen.insert(cell.clone()) {
                return Err(format!("{line_at} repeats {cell}"));
            }
            records.push((cell, record));
        }
    }
    let state = LedgerPostPassState::from(status.state);
    let (owed, scope_source) = match (&scope, status.state, expected) {
        (Some(scope), _, _) => (Some(scope.clone()), LedgerScopeSource::StatusScope),
        (None, PostPassState::Complete, _) => (None, LedgerScopeSource::StatusCount),
        (None, _, Some(expected)) => (
            Some(expected.iter().cloned().collect()),
            LedgerScopeSource::ExpectedScope,
        ),
        (None, state, None) => {
            return Err(format!(
                "{at}: a schema-{PARITY_STATUS_SCHEMA_COUNT_ONLY} status in state {state:?} \
                 counts {} cell(s) but does not name them, and no expected scope was given",
                status.cells
            ));
        }
    };
    let origin = ParityLedgerOrigin {
        lane: lane.to_string(),
        node: node.to_string(),
        post_pass_state: state,
        status_sha256: Some(sha256_hex(&status_bytes)),
        records_sha256: records_bytes.as_deref().map(sha256_hex),
        hermit_bin_sha256: status.hermit_bin_sha256.clone(),
        scope_source,
    };
    if let Some(owed) = &owed {
        let owed_set = owed.iter().collect::<BTreeSet<_>>();
        if let Some((cell, _)) = records.iter().find(|(cell, _)| !owed_set.contains(cell)) {
            return Err(format!(
                "{at}: record {cell} is outside the post-pass scope"
            ));
        }
    }
    let record_row = |record: &ParityRecord| ParityLedgerSource {
        cell: format!("{}@{}", record.test_id, record.backend),
        test_id: record.test_id.clone(),
        backend: record.backend,
        verdict: record.verdict.into(),
        unavailable_class: record.unavailable_class,
        operand: record.operand,
        reason: record.reason.clone(),
        run_id: Some(status.run_id.clone()),
        hermit_sha: Some(status.hermit_sha.clone()),
        source: origin.clone(),
        record: Some(record.clone()),
    };
    let mut rows = Vec::new();
    if status.state == PostPassState::Complete {
        if records_bytes.is_none() {
            return Err(format!(
                "{at} is complete but {} does not exist",
                records_path.display()
            ));
        }
        if records.len() != status.cells {
            return Err(format!(
                "{at} is complete with {} cell(s) in scope, but {} holds {} record(s)",
                status.cells,
                records_path.display(),
                records.len()
            ));
        }
        rows.extend(records.iter().map(|(_, record)| record_row(record)));
    } else {
        let by_cell = records.iter().cloned().collect::<BTreeMap<_, _>>();
        let error = status.error.as_deref().unwrap_or(match status.state {
            PostPassState::Running => {
                "no error recorded; the process ended before the post-pass finished"
            }
            _ => "no error recorded",
        });
        let state_text = match status.state {
            PostPassState::Running => "running",
            PostPassState::Failed => "failed",
            PostPassState::Complete => "complete",
        };
        let reason = format!("parity post-pass {state_text}: {error}");
        for cell in owed.iter().flatten() {
            rows.push(match by_cell.get(cell) {
                Some(record) => record_row(record),
                None => missing_row(cell, &reason, Some(&status), &origin)?,
            });
        }
    }
    // A cell the caller expected that the status's own scope does not name
    // (or, for a complete schema-1 status, that no record names) was owed
    // by the plan all the same: it is a `record-missing` row filed under the
    // expected scope, so a narrower post-pass scope cannot drop it from the
    // denominator.
    let covered = match &owed {
        Some(owed) => owed.iter().collect::<BTreeSet<_>>(),
        None => records.iter().map(|(cell, _)| cell).collect(),
    };
    let gap_origin = ParityLedgerOrigin {
        scope_source: LedgerScopeSource::ExpectedScope,
        ..origin.clone()
    };
    let gap_reason = format!(
        "no parity row: the post-pass of node {lane}/{node} left this cell out of its scope, \
         and the node's expected scope owes it"
    );
    for cell in expected
        .into_iter()
        .flatten()
        .filter(|cell| !covered.contains(cell))
    {
        rows.push(missing_row(cell, &gap_reason, Some(&status), &gap_origin)?);
    }
    for row in &rows {
        row.validate()?;
    }
    Ok(rows)
}

/// The ledger rows validate appends when [`ledger_sources`] refuses its run.
/// Every node [`ledger_sources`] would read is read on its own by
/// [`node_ledger_sources`]; the rows of each node it accepts are kept, and a
/// node whose outputs it refuses owes each of its `expected` cells a
/// `record-missing` row with `post_pass_state` `refused` and the refusal as
/// its reason. A run whose outputs cannot all be accounted for therefore
/// still reaches the ledger with every cell its plan owed, instead of
/// vanishing from it. Returns the rows and one line per refusal:
/// `<lane>/<node>: <error>` for a refused node, and the error itself for a
/// directory that could not be listed.
pub fn expected_ledger_sources(
    e2e_root: &Path,
    expected: &ExpectedScope,
) -> Result<(Vec<ParityLedgerSource>, Vec<String>), String> {
    let (nodes, mut refused) = run_nodes(e2e_root, Some(expected))?;
    let mut rows = Vec::new();
    for (key, (lane, node, directory)) in &nodes {
        let owed = expected.get(key);
        match node_ledger_sources(directory, lane, node, owed) {
            Ok(node_rows) => rows.extend(node_rows),
            Err(error) => {
                // The bytes that were refused, when they can be read, so the
                // row names exactly what it stands in for.
                let digest = |name: &str| read_if_present(&directory.join(name)).ok().flatten();
                let origin = ParityLedgerOrigin {
                    lane: lane.to_string(),
                    node: node.to_string(),
                    post_pass_state: LedgerPostPassState::Refused,
                    status_sha256: digest(PARITY_STATUS_JSON).as_deref().map(sha256_hex),
                    records_sha256: digest(PARITY_JSONL).as_deref().map(sha256_hex),
                    hermit_bin_sha256: None,
                    scope_source: LedgerScopeSource::ExpectedScope,
                };
                let reason = format!("parity post-pass outputs refused: {error}");
                for cell in owed.into_iter().flatten() {
                    rows.push(missing_row(cell, &reason, None, &origin)?);
                }
                refused.push(format!("{key}: {error}"));
            }
        }
    }
    Ok((rows, refused))
}

fn missing_row(
    cell: &ParityCellId,
    reason: &str,
    status: Option<&PostPassStatus>,
    origin: &ParityLedgerOrigin,
) -> Result<ParityLedgerSource, String> {
    let row = ParityLedgerSource {
        cell: cell.to_string(),
        test_id: cell.test_id.clone(),
        backend: cell.backend,
        verdict: LedgerVerdict::RecordMissing,
        unavailable_class: Some(UnavailableClass::RecordMissing),
        operand: None,
        reason: Some(reason.to_string()),
        run_id: status.map(|status| status.run_id.clone()),
        hermit_sha: status.map(|status| status.hermit_sha.clone()),
        source: origin.clone(),
        record: None,
    };
    row.validate()?;
    Ok(row)
}

fn read_if_present(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("cannot read {}: {error}", path.display())),
    }
}

fn sorted_directories(path: &Path) -> Result<Vec<PathBuf>, String> {
    let mut directories = fs::read_dir(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|error| format!("cannot read {}: {error}", path.display()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    directories.retain(|path| path.is_dir());
    directories.sort();
    Ok(directories)
}

fn directory_name(path: &Path) -> Result<String, String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .ok_or_else(|| format!("{} has no UTF-8 directory name", path.display()))
}

// ---- Appending ledger rows ---------------------------------------------------
//
// validate and the pressure test hand their rows to dev-hermit's
// `series.py append-parity` through the one function below, so both bound
// the writer, probe it and report its outcome the same way.

/// How long [`append_ledger_rows`] waits on dev-hermit's writer.
#[derive(Clone, Copy, Debug)]
pub struct AppendBounds {
    /// The `append-parity --help` capability probe.
    pub probe: Duration,
    /// The append itself.
    pub append: Duration,
}

impl Default for AppendBounds {
    fn default() -> Self {
        Self {
            probe: Duration::from_secs(60),
            append: Duration::from_secs(300),
        }
    }
}

/// Bytes of the writer's stdout, and separately of its stderr, that
/// [`append_ledger_rows`] keeps.
const APPEND_OUTPUT_LIMIT_BYTES: usize = 16 * 1024;

/// Where [`append_ledger_rows`] sends one run's rows.
#[derive(Clone, Copy, Debug)]
pub struct LedgerAppend<'a> {
    /// `<tool root>/ci-hub/series/series.py`.
    pub series: &'a Path,
    /// The dev-hermit checkout whose ledger receives the rows.
    pub parent: &'a Path,
    pub producer: ParityProducer,
    pub run_id: &'a str,
    /// The Hermit tree the run measured.
    pub tree: &'a str,
    /// Whether that tree had uncommitted changes, sent as
    /// `--source-tree-dirty true|false`. The writer never defaults it: a
    /// row stamped clean without the caller saying so would be admitted as
    /// clean evidence by every scorecard that excludes dirty rows. Each
    /// caller passes the verdict its run's other ledger evidence already
    /// carries, never a second probe of the tree.
    pub source_tree_dirty: bool,
    /// Where the rows were read from, named in the line.
    pub source: &'a Path,
}

/// The rows counted by verdict, `diverged 1, record-missing 3`.
pub fn ledger_row_counts(rows: &[ParityLedgerSource]) -> String {
    let mut counts = BTreeMap::<&str, usize>::new();
    for row in rows {
        *counts.entry(row.verdict.as_str()).or_default() += 1;
    }
    counts
        .iter()
        .map(|(verdict, count)| format!("{verdict} {count}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Send `rows` to `series.py append-parity` and return the one `parity:`
/// line that reports the outcome. Parity is measured after determinism and
/// decides nothing, so no outcome here is an error for the caller.
///
/// - The writer is probed with `append-parity --help`, which exits 0 only
///   on a writer that has the subcommand. Exit status 2 is dev-hermit's
///   `series.py` answering an unknown subcommand with its usage, so only
///   that status names the writer as having no `append-parity`. Any other
///   failure of the probe (a traceback, a signal), a missing writer and one
///   that cannot be started are `ERROR:` lines, the probe's with its
///   stderr. In each case the rows are left where they are.
/// - The rows reach the writer's stdin from an unlinked temporary file
///   ([`unlinked_input`]), never a pipe: a script whose prelude turns
///   `SIGPIPE` into `_exit(0)` would otherwise end, reporting success, the
///   moment a writer exited before reading them.
/// - Each call is bounded by [`AppendBounds`]; a writer still running at its
///   bound is killed with its process group, and the line says so.
/// - At most `APPEND_OUTPUT_LIMIT_BYTES` of each of its outputs is kept.
/// - Exit status 0 is the writer's acceptance, and the line says only that:
///   dev-hermit's writer exits 0 both when it wrote the rows to the parent's
///   unpublished spool and when it had already recorded them identically,
///   so the line ends with the writer's own words, which say which.
pub fn append_ledger_rows(
    append: &LedgerAppend<'_>,
    rows: &[ParityLedgerSource],
    bounds: AppendBounds,
) -> String {
    let counts = ledger_row_counts(rows);
    let left = |why: &str| {
        format!(
            "parity: {why}; {} rows left in {} ({counts})",
            rows.len(),
            append.source.display()
        )
    };
    let series = append.series;
    if !series.is_file() {
        return left(&format!("ERROR: {} does not exist", series.display()));
    }
    let mut probe = Command::new("python3");
    probe.arg(series).args(["append-parity", "--help"]);
    match run_bounded_capturing(&mut probe, Stdio::null(), bounds.probe) {
        Err(error) => {
            return left(&format!("ERROR: cannot run {}: {error}", series.display()));
        }
        Ok(Captured { status: None, .. }) => {
            return left(&format!(
                "ERROR: `{} append-parity --help` did not finish within {:?} and was killed",
                series.display(),
                bounds.probe
            ));
        }
        Ok(Captured {
            status: Some(status),
            ..
        }) if status.code() == Some(2) => {
            return left(&format!(
                "the series writer {} has no append-parity (`append-parity --help` {status})",
                series.display()
            ));
        }
        Ok(Captured {
            status: Some(status),
            stderr,
            ..
        }) if !status.success() => {
            return left(&format!(
                "ERROR: `{} append-parity --help` failed ({status}): {}",
                series.display(),
                String::from_utf8_lossy(&stderr).trim()
            ));
        }
        Ok(_) => {}
    }
    let mut payload = Vec::new();
    for row in rows {
        if let Err(error) = serde_json::to_writer(&mut payload, row) {
            return left(&format!("ERROR: cannot encode a parity row: {error}"));
        }
        payload.push(b'\n');
    }
    let input = match unlinked_input(&payload) {
        Ok(input) => input,
        Err(error) => {
            return left(&format!(
                "ERROR: cannot stage the rows for append-parity: {error}"
            ));
        }
    };
    let mut command = Command::new("python3");
    command
        .arg(series)
        .arg("append-parity")
        .arg("--parent")
        .arg(append.parent)
        .arg("--producer")
        .arg(append.producer.as_str())
        .arg("--run-id")
        .arg(append.run_id)
        .arg("--tree")
        .arg(append.tree)
        .arg("--source-tree-dirty")
        .arg(if append.source_tree_dirty {
            "true"
        } else {
            "false"
        });
    let Captured {
        status,
        stdout,
        stderr,
    } = match run_bounded_capturing(&mut command, Stdio::from(input), bounds.append) {
        Ok(outcome) => outcome,
        Err(error) => {
            return left(&format!("ERROR: cannot run {}: {error}", series.display()));
        }
    };
    let Some(status) = status else {
        return left(&format!(
            "ERROR: append-parity did not finish within {:?} and was killed, so the ledger may \
             hold some of these rows",
            bounds.append
        ));
    };
    if !status.success() {
        return left(&format!(
            "ERROR: append-parity refused them ({status}): {}",
            String::from_utf8_lossy(&stderr).trim()
        ));
    }
    let said = String::from_utf8_lossy(&stdout);
    let said = said.trim();
    format!(
        "parity: append-parity accepted {} row(s) from {} ({counts}): {}",
        rows.len(),
        append.source.display(),
        if said.is_empty() {
            "it printed nothing"
        } else {
            said
        }
    )
}

/// A file holding `bytes`, positioned at its start and already unlinked, to
/// hand a child as its stdin. Unlike a pipe it never fails the sender when
/// the child exits without reading (a script whose prelude turns `SIGPIPE`
/// into `_exit(0)` would otherwise end on the spot, reporting success), it
/// never blocks the sender, and nothing is left on disk whatever happens
/// next.
pub fn unlinked_input(bytes: &[u8]) -> Result<fs::File, String> {
    static STAGED: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "hermit-child-stdin-{}-{}",
        std::process::id(),
        STAGED.fetch_add(1, Ordering::Relaxed)
    ));
    unlinked_input_at(&path, bytes)
}

/// [`unlinked_input`] at `path`, which must not exist yet. A file already
/// there is someone else's: staging is refused and that file is left alone.
fn unlinked_input_at(path: &Path, bytes: &[u8]) -> Result<fs::File, String> {
    use std::io::Seek;
    use std::io::Write;

    let mut file = fs::File::options()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("cannot create {}: {error}", path.display()))?;
    // Only a file this call created is removed, and at once: the open
    // descriptor is all a reader needs.
    fs::remove_file(path).map_err(|error| format!("cannot unlink {}: {error}", path.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.rewind())
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    Ok(file)
}

/// A child process that leads its own process group, so that a bounded
/// wait can kill it together with everything it started. The cost of its
/// own group: a terminal's job-control signals (Ctrl-C, Ctrl-Z) go to the
/// foreground group only, so they no longer reach it; it ends on its own or
/// at the bound of [`GroupChild::wait_bounded`].
pub struct GroupChild {
    child: Child,
    group: libc::pid_t,
}

impl GroupChild {
    /// Spawn `command` as the leader of a new process group.
    pub fn spawn(command: &mut Command) -> std::io::Result<Self> {
        use std::os::unix::process::CommandExt;

        let child = command.process_group(0).spawn()?;
        let group = child.id() as libc::pid_t;
        Ok(Self { child, group })
    }

    fn kill_group(&self) {
        // SAFETY: kill(2) only sends a signal; a negative pid names the
        // process group the child leads, which nothing else joins. Once the
        // group is empty the call fails with ESRCH, which is harmless.
        unsafe {
            libc::kill(-self.group, libc::SIGKILL);
        }
    }

    /// Wait at most `timeout` for the leader; `None` means the group was
    /// killed at `timeout`. Once the leader has ended, whatever it left
    /// running in its group is killed too, so a straggler holding a pipe
    /// cannot hold the caller.
    pub fn wait_bounded(mut self, timeout: Duration) -> Result<Option<ExitStatus>, String> {
        let started = Instant::now();
        let status = loop {
            match self.child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if started.elapsed() >= timeout => {
                    self.kill_group();
                    let _ = self.child.wait();
                    break None;
                }
                Ok(None) => thread::sleep(Duration::from_millis(10)),
                Err(error) => {
                    self.kill_group();
                    let _ = self.child.wait();
                    return Err(error.to_string());
                }
            }
        };
        self.kill_group();
        Ok(status)
    }
}

/// Keep at most `limit` bytes of `pipe`, reading it to its end so the writer
/// never blocks on a full pipe.
fn capped_reader(
    mut pipe: impl Read + Send + 'static,
    limit: usize,
) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut kept = Vec::new();
        let mut buffer = [0u8; 8192];
        while let Ok(read) = pipe.read(&mut buffer) {
            if read == 0 {
                break;
            }
            let room = limit.saturating_sub(kept.len());
            kept.extend_from_slice(&buffer[..read.min(room)]);
        }
        kept
    })
}

/// What [`run_bounded_capturing`] kept of one bounded run.
struct Captured {
    /// `None` when the group was killed at its bound.
    status: Option<ExitStatus>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Run `command` as the leader of its own process group, with `stdin`,
/// keeping at most [`APPEND_OUTPUT_LIMIT_BYTES`] of each of stdout and
/// stderr. A `None` status means the group was killed at `timeout`. Once
/// the leader has ended, whatever it left running in its group is killed
/// too, so a straggler holding a pipe cannot hold the caller.
fn run_bounded_capturing(
    command: &mut Command,
    stdin: Stdio,
    timeout: Duration,
) -> Result<Captured, String> {
    let mut child = GroupChild::spawn(
        command
            .stdin(stdin)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )
    .map_err(|error| error.to_string())?;
    let (Some(stdout), Some(stderr)) = (child.child.stdout.take(), child.child.stderr.take())
    else {
        let _ = child.wait_bounded(Duration::ZERO);
        return Err("an output pipe of the child is missing".to_string());
    };
    let stdout = capped_reader(stdout, APPEND_OUTPUT_LIMIT_BYTES);
    let stderr = capped_reader(stderr, APPEND_OUTPUT_LIMIT_BYTES);
    let status = child.wait_bounded(timeout)?;
    Ok(Captured {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::logdiff_report::LOG_DIFF_REPORT_SCHEMA;
    use crate::logdiff_report::LogDiffComparison;
    use crate::logdiff_report::LogDiffInput;
    use crate::logdiff_report::LogDiffInputs;
    use crate::logdiff_report::LogDiffMessageCounts;
    use crate::logdiff_report::LogDiffRecords;
    use crate::logdiff_report::RecordEnvelopePolicy;

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    fn shipped_matrix() -> ParityMatrix {
        ParityMatrix::derive(&ManifestSet::load(&repo_root()).unwrap()).unwrap()
    }

    /// The committed snapshot must be byte-for-byte what the generator
    /// produces from the committed manifests and selection. This is the same
    /// shape as the validation DAG's freshness test, and it runs wherever
    /// that test runs.
    #[test]
    fn committed_parity_cells_snapshot_is_fresh() {
        let generated = canonical_text(&generate(&repo_root()).unwrap()).unwrap();
        let committed = include_str!("../../compat-envelope/parity-cells.json");
        if let Err(error) = require_fresh(committed, &generated) {
            panic!("{error}");
        }
        let parsed: ParityCells = serde_json::from_str(committed).unwrap();
        assert_eq!(parsed.cells.len(), 3700);
        assert_eq!(
            parsed.inputs_not_equalizable.keys().collect::<Vec<_>>(),
            [&ParityBackend::Dbt]
        );
        // The repository's limit for a text file is 2 MiB. This bound was
        // 1 MiB until fold 5 of https://github.com/rrnewton/hermit/issues/3448
        // took the snapshot to 1,099,771 bytes: every test lists one line per
        // backend, and the 139 rr-compat-only replay tests added 556
        // not-applicable lines. 854 KB of it is the 2857 not-applicable lines.
        assert!(
            committed.len() < 1536 * 1024,
            "{PARITY_CELLS_PATH} is {} bytes; it must stay well under 2 MiB",
            committed.len()
        );
    }

    /// Counts recorded in https://github.com/rrnewton/hermit/issues/3301.
    /// If a manifest change moves them on purpose, update these numbers in
    /// the same commit and regenerate the snapshot.
    #[test]
    fn the_shipped_manifests_give_the_recorded_parity_counts() {
        let counts = generate(&repo_root()).unwrap().counts;
        let row = |count: &ParityCount| {
            (
                count.cells,
                count.applicable,
                count.selectable,
                count.selected,
                count.selected_selectable,
            )
        };
        // 2252 cells (563 per backend) since fold 1 of
        // https://github.com/rrnewton/hermit/issues/3448 moved the 189
        // strict compatibility programs into the manifest: each declares its
        // verify cell disabled on the four non-ptrace backends, so the census
        // gains 756 cells and no applicable, selectable or selected one.
        // 2360 (590 per backend) since fold 3 added the 27 rows only the
        // sabre-compat-only run type runs, and 212 more applicable SaBRe
        // cells: that run type's verify cells, none selectable or selected.
        // 2372 (593 per backend) and 3 more applicable SaBRe cells since the
        // rows lua-direct, perl-direct and df-direct kept the SaBRe corpus's
        // single-image argv.
        // 3144 (786 per backend) since fold 4 added the strict-compat-only
        // run type's 193 variant tests, each declaring its verify cell
        // disabled on the four non-ptrace backends like the rows it repeats:
        // 772 more cells and no applicable, selectable or selected one.
        // 3700 (925 per backend) since fold 5 added the rr-compat-only run
        // type's 139 replay variant tests, whose cells on the four non-ptrace
        // backends are all off: 556 more cells and no applicable, selectable
        // or selected one.
        // Six earlier and three socket KVM verify enables add applicable,
        // selectable and selected cells without adding candidate identities.
        // https://github.com/rrnewton/reverie/issues/891
        // The poll-readiness KVM verify enable adds one more:
        // https://github.com/rrnewton/reverie/issues/620
        // One epoll-pwait2 KVM candidate adds exactly one to the same counters:
        // https://github.com/rrnewton/reverie/issues/905
        // One ordinary syncfs KVM candidate adds one to each live counter:
        // https://github.com/rrnewton/reverie/issues/838
        // One pipe owner/signal KVM selection: https://github.com/rrnewton/reverie/pull/910.
        // One msync writeback KVM selection: https://github.com/rrnewton/reverie/issues/891.
        // The 2026-10-03 SaBRe promotion (timeouts::SABRE_2026_10_03_*)
        // enables 120 verify cells that were not applicable and turns 8
        // more on in CI: 120 more applicable, 128 more selectable, and 89
        // more selected (the promoted folded backend-parity-c tests).
        assert_eq!(
            row(&counts.all),
            (
                3700,
                843 + 6 + 3 + 1 + 1 + 1 + 1 + 1 + 120,
                527 + 6 + 3 + 1 + 1 + 1 + 1 + 1 + 128,
                194 + 6 + 3 + 1 + 1 + 1 + 1 + 1 + 89,
                177 + 6 + 3 + 1 + 1 + 1 + 1 + 1 + 89
            )
        );
        let by_backend: Vec<_> = counts
            .by_backend
            .iter()
            .map(|(backend, count)| (backend.as_str(), row(count)))
            .collect();
        assert_eq!(
            by_backend,
            [
                ("dbt", (925, 85, 26, 16, 2)),
                (
                    "kvm",
                    (
                        925,
                        250 + 6 + 3 + 1 + 1 + 1 + 1 + 1,
                        243 + 6 + 3 + 1 + 1 + 1 + 1 + 1,
                        77 + 6 + 3 + 1 + 1 + 1 + 1 + 1,
                        76 + 6 + 3 + 1 + 1 + 1 + 1 + 1
                    )
                ),
                ("liteinst", (925, 149, 146, 99, 98)),
                // The same 120, 128 and 89 SaBRe cells as above.
                ("sabre", (925, 359 + 120, 112 + 128, 2 + 89, 1 + 89)),
            ]
        );
    }

    /// The committed selection is exactly what its written rule derives, so
    /// the rule cannot silently drift from the list.
    #[test]
    fn the_initial_selection_is_exactly_its_rule() {
        let root = repo_root();
        let manifests = ManifestSet::load(&root).unwrap();
        let matrix = ParityMatrix::derive(&manifests).unwrap();
        let selection = ParitySelection::load(&root, &matrix).unwrap();
        // The rule names the tests folded from the retired backend-parity-c
        // bucket, which retired-ids.json records; they now live in c-programs.
        let folded = crate::retired_ids::RetiredIds::load(&root)
            .unwrap()
            .successors_of("backend-parity-c")
            .unwrap();
        let reference_selected: BTreeSet<String> = manifests
            .select(&Selection {
                population: Some(Population::Required),
                category: Some("c-programs".to_string()),
                mode: Some(PARITY_MODE.to_string()),
                backend: Some(PARITY_REFERENCE_BACKEND.to_string()),
                ..Selection::default()
            })
            .unwrap()
            .into_iter()
            .map(|cell| cell.id.test)
            .filter(|test| folded.contains(test))
            .collect();
        assert!(!reference_selected.is_empty());
        let derived: BTreeSet<ParityCellId> = matrix
            .cells()
            .filter(|(id, availability)| {
                availability.applicable() && reference_selected.contains(&id.test_id)
            })
            .map(|(id, _)| id.clone())
            .collect();
        assert_eq!(selection.cells, derived);
        assert!(selection.rule.contains("retired backend-parity-c bucket"));
        assert!(
            selection
                .rule
                .contains(crate::retired_ids::RETIRED_IDS_FILE)
        );
    }

    #[test]
    fn every_cell_carries_a_reason_matching_its_status() {
        let snapshot = generate(&repo_root()).unwrap();
        // `Exact` reasons carry no per-cell detail, so they must be the
        // whole text. A `Detail` reason must name why after its prefix.
        enum Expected {
            Exact(&'static str),
            Detail(&'static str),
        }
        for cell in &snapshot.cells {
            let expected = match (cell.applicable, cell.selectable, cell.selected) {
                (false, false, false) => Expected::Detail("not applicable: "),
                (true, false, false) => Expected::Detail("not selectable: "),
                (true, false, true) => Expected::Detail(
                    "selected by tests/e2e/parity-selection.yaml, but not selectable: ",
                ),
                (true, true, false) => Expected::Exact(
                    "selectable, not selected: not listed in tests/e2e/parity-selection.yaml",
                ),
                (true, true, true) => {
                    Expected::Exact("selected by tests/e2e/parity-selection.yaml")
                }
                other => panic!("impossible status {other:?} for {}", cell.test_id),
            };
            let matches = match expected {
                Expected::Exact(text) => cell.reason == text,
                Expected::Detail(prefix) => cell
                    .reason
                    .strip_prefix(prefix)
                    .is_some_and(|detail| !detail.trim().is_empty()),
            };
            assert!(
                matches,
                "{}@{}: {}",
                cell.test_id, cell.backend, cell.reason
            );
        }
    }

    /// A selection may keep a cell that is not selectable, but a selection of
    /// ONLY such cells gives a population no run can measure. The shipped
    /// selection's not-selectable members (the 14 dbt cells among them) are
    /// that selection.
    #[test]
    fn a_selection_with_nothing_selectable_is_a_vacuous_snapshot() {
        let root = repo_root();
        let matrix = shipped_matrix();
        let shipped = ParitySelection::load(&root, &matrix).unwrap();
        let availability: BTreeMap<&ParityCellId, &ParityAvailability> = matrix.cells().collect();
        let (selectable, not_selectable): (BTreeSet<_>, BTreeSet<_>) = shipped
            .cells
            .iter()
            .cloned()
            .partition(|id| availability[id].selectable());
        assert!(!not_selectable.is_empty());
        assert!(
            not_selectable
                .iter()
                .any(|id| id.backend == ParityBackend::Dbt)
        );
        let unmeasurable = ParitySelection {
            rule: "fixture rule".into(),
            cells: not_selectable.clone(),
        };
        assert_eq!(
            snapshot(&matrix, &unmeasurable).unwrap_err(),
            format!(
                "parity snapshot would be vacuous: {} cells, {} selected, 0 of them selectable",
                matrix.cells().count(),
                not_selectable.len()
            )
        );

        // One selectable cell is enough to make it a population.
        let mut one_measurable = unmeasurable;
        one_measurable
            .cells
            .insert(selectable.first().unwrap().clone());
        let counts = snapshot(&matrix, &one_measurable).unwrap().counts;
        assert_eq!(counts.all.selected, not_selectable.len() + 1);
        assert_eq!(counts.all.selected_selectable, 1);
    }

    #[test]
    fn backends_parse_and_ptrace_is_refused_as_the_reference() {
        for backend in ParityBackend::ALL {
            assert_eq!(ParityBackend::parse(backend.as_str()), Ok(backend));
        }
        let ptrace = ParityBackend::parse("ptrace").unwrap_err();
        assert!(ptrace.contains("parity reference"), "{ptrace}");
        let unknown = ParityBackend::parse("qemu").unwrap_err();
        assert!(unknown.contains("unknown parity backend"), "{unknown}");
        let mut sorted = ParityBackend::ALL.map(ParityBackend::as_str);
        sorted.sort_unstable();
        assert_eq!(sorted, ParityBackend::ALL.map(ParityBackend::as_str));
    }

    fn first_cell(matrix: &ParityMatrix, wanted: fn(&ParityAvailability) -> bool) -> ParityCellId {
        matrix
            .cells()
            .find(|(_, availability)| wanted(availability))
            .map(|(id, _)| id.clone())
            .expect("the shipped matrix has a cell of this kind")
    }

    fn selection_text(test: &str, backends: &str) -> String {
        format!(
            "schema: 1\nrule: fixture rule\ncells:\n  - test: {test}\n    backends: [{backends}]\n"
        )
    }

    fn refusal(matrix: &ParityMatrix, text: &str) -> String {
        ParitySelection::parse(text, matrix).unwrap_err()
    }

    #[test]
    fn the_selection_loader_refuses_what_it_cannot_measure() {
        let matrix = shipped_matrix();
        let applicable = first_cell(&matrix, ParityAvailability::applicable);
        let not_applicable = first_cell(&matrix, |availability| !availability.applicable());
        let not_selectable = first_cell(&matrix, |availability| {
            availability.applicable() && !availability.selectable()
        });

        // Accepted: an applicable cell, including one not yet selectable.
        let accepted = ParitySelection::parse(
            &selection_text(&applicable.test_id, applicable.backend.as_str()),
            &matrix,
        )
        .unwrap();
        assert_eq!(accepted.cells, BTreeSet::from([applicable.clone()]));
        ParitySelection::parse(
            &selection_text(&not_selectable.test_id, not_selectable.backend.as_str()),
            &matrix,
        )
        .unwrap();

        let error = refusal(
            &matrix,
            &selection_text(&not_applicable.test_id, not_applicable.backend.as_str()),
        );
        let ParityAvailability::NotApplicable { reason } = matrix.get(&not_applicable).unwrap()
        else {
            unreachable!()
        };
        assert!(
            error.contains("is not applicable") && error.contains(reason.as_str()),
            "{error}"
        );

        let error = refusal(&matrix, &selection_text("retired/no-such-test", "kvm"));
        assert!(error.contains("unknown test id"), "{error}");
        let error = refusal(&matrix, &selection_text(&applicable.test_id, "ptrace"));
        assert!(error.contains("parity reference"), "{error}");
        let error = refusal(&matrix, &selection_text(&applicable.test_id, "qemu"));
        assert!(error.contains("unknown parity backend"), "{error}");
        let backend = applicable.backend.as_str();
        let error = refusal(
            &matrix,
            &selection_text(&applicable.test_id, &format!("{backend}, {backend}")),
        );
        assert!(error.contains("sorted and unique"), "{error}");
        let error = refusal(&matrix, &selection_text(&applicable.test_id, ""));
        assert!(error.contains("lists no backends"), "{error}");

        let twice = format!(
            "schema: 1\nrule: r\ncells:\n  - test: {t}\n    backends: [{backend}]\n  - test: {t}\n    backends: [{backend}]\n",
            t = applicable.test_id
        );
        assert!(refusal(&matrix, &twice).contains("sorted and unique"));
        assert!(refusal(&matrix, "schema: 1\nrule: r\ncells: []\n").contains("selects no cells"));
        assert!(refusal(&matrix, "schema: 1\nrule: '  '\ncells: []\n").contains("`rule`"));
        assert!(refusal(&matrix, "schema: 2\nrule: r\ncells: []\n").contains("schema must be 1"));
        assert!(
            refusal(&matrix, "schema: 1\nrule: r\nextra: 1\ncells: []\n").contains("invalid YAML")
        );
    }

    #[test]
    fn a_selection_under_the_manifest_directory_is_refused() {
        let matrix = shipped_matrix();
        let root = repo_root();
        for relative in [
            "tests/e2e/manifests/parity-selection.yaml",
            "tests/e2e/manifests/nested/parity-selection.yaml",
        ] {
            let error =
                ParitySelection::load_path(&root, Path::new(relative), &matrix).unwrap_err();
            assert!(error.contains("holds only bucket manifests"), "{error}");
        }
        for relative in [
            "tests/e2e/../e2e/manifests/parity-selection.yaml",
            "./tests/e2e/manifests/parity-selection.yaml",
            "/tmp/parity-selection.yaml",
        ] {
            let error =
                ParitySelection::load_path(&root, Path::new(relative), &matrix).unwrap_err();
            assert!(error.contains("plain components only"), "{error}");
        }
        // The real file loads from its real place.
        ParitySelection::load(&root, &matrix).unwrap();
    }

    #[test]
    fn credit_is_full_only_for_a_full_length_match_and_none_when_unmeasured() {
        // Identical logs.
        assert_eq!(credit(4, 4, 4), Some(1.0));
        // Divergence at compared message 1: a measured zero, not "unmeasured".
        assert_eq!(credit(0, 4, 4), Some(0.0));
        // Divergence at compared message k = 3 of 4: (k - 1) / 4.
        assert_eq!(credit(2, 4, 4), Some(0.5));
        // Unequal lengths, equal prefix: min / max.
        assert_eq!(credit(2, 2, 5), Some(0.4));
        assert_eq!(credit(5, 5, 2), Some(0.4));
        // Nothing compared.
        assert_eq!(credit(0, 0, 0), None);
        // A prefix cannot exceed the shorter side, so clamping cannot fabricate
        // full credit for unequal lengths.
        assert_eq!(credit(9, 4, 4), Some(1.0));
        assert_eq!(credit(9, 2, 5), Some(0.4));
        // A partial match never rounds up to 1.0, however long the logs are.
        let huge = 1usize << 60;
        let almost = credit(huge - 1, huge, huge).unwrap();
        assert!(almost < 1.0, "{almost}");
        // Always in range.
        for (prefix, left, right) in [(0, 1, 0), (1, 1, 3), (3, 3, 3), (7, 9, 8)] {
            let value = credit(prefix, left, right).unwrap();
            assert!((0.0..=1.0).contains(&value));
            assert_eq!(value == 1.0, prefix >= left && left == right);
        }
    }

    fn report(
        schema: u64,
        verdict: LogDiffVerdict,
        left: usize,
        right: usize,
        prefix: Option<usize>,
    ) -> LogDiffReport {
        let mut parsed = LogDiffReport {
            schema,
            verdict,
            refusal: None,
            selected_messages: LogDiffMessageCounts { left, right },
            // Raw records bound the selected messages on each side.
            records: LogDiffRecords {
                compared: left.max(9).min(right.max(9)),
                available_left: left.max(9),
                available_right: right.max(9),
                withheld_incomplete_tail: false,
            },
            inputs: Some(LogDiffInputs {
                left: LogDiffInput {
                    sha256: "a".repeat(64),
                    bytes: 100,
                },
                right: LogDiffInput {
                    sha256: "b".repeat(64),
                    bytes: 100,
                },
            }),
            comparison: LogDiffComparison {
                stream: "info".into(),
                record_envelope: RecordEnvelopePolicy::CrossBackendDetcoreV1,
                unsafe_strip_lines: false,
                canonicalize_host_addresses: true,
                require_structured_events: true,
                ignored_line_substrings: Vec::new(),
                skip_commit: false,
                skip_detlog: false,
                included_detlog_kinds: vec![
                    "syscall".into(),
                    "syscall_result".into(),
                    "other".into(),
                ],
                git_diff: false,
            },
            follow_stopped_because: None,
            first_divergent_record: None,
            matched_prefix_records: None,
            first_divergent_syscall: None,
            first_divergent_scheduler_turn: None,
            first_divergent_virtual_nanoseconds: None,
            first_divergent_left_message: None,
            first_divergent_right_message: None,
        };
        parsed.matched_prefix_records = prefix;
        if verdict == LogDiffVerdict::Diverged {
            // A raw log-record index, as the producer writes it: it also counts
            // records the comparison did not select, so it is NOT
            // `matched_prefix + 1`. Keeping them apart here is what lets the
            // tests below tell a credit derived from the prefix from one
            // derived from this index.
            parsed.first_divergent_record =
                Some(prefix.unwrap_or(0) + 1 + UNSELECTED_RECORDS_BEFORE_DIVERGENCE);
            parsed.first_divergent_syscall = Some(1);
            parsed.first_divergent_left_message = Some("DETLOG syscall nr=0 ret=3".into());
            parsed.first_divergent_right_message = Some("DETLOG syscall nr=0 ret=4".into());
        }
        parsed
    }

    /// Unselected raw records the [`report`] fixture places before the first
    /// difference. Nonzero, so `first_divergent_record != matched_prefix + 1`.
    const UNSELECTED_RECORDS_BEFORE_DIVERGENCE: usize = 4;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn cell() -> ParityCellId {
        ParityCellId {
            test_id: "fixture/parity".into(),
            backend: ParityBackend::Kvm,
        }
    }

    fn record_on(backend: ParityBackend, report: &LogDiffReport, equal: bool) -> ParityRecord {
        let cell = ParityCellId { backend, ..cell() };
        ParityRecord::from_comparison(&cell, report, equal, "ref.log", "cand.log", "run-1", SHA)
            .unwrap()
    }

    /// A kvm record from a run whose inputs were equalized.
    fn record_for(report: &LogDiffReport) -> ParityRecord {
        record_on(ParityBackend::Kvm, report, true)
    }

    #[test]
    fn credit_from_a_report_follows_its_verdict_and_schema() {
        let current = LOG_DIFF_REPORT_SCHEMA;
        assert_eq!(
            credit_from_report(&report(current, LogDiffVerdict::Matched, 3, 3, Some(3))),
            Some(1.0)
        );
        assert_eq!(
            credit_from_report(&report(current, LogDiffVerdict::Diverged, 4, 4, Some(0))),
            Some(0.0)
        );
        // Unequal lengths with an equal prefix: 2 of 5.
        let shorter_side = report(current, LogDiffVerdict::Diverged, 2, 5, Some(2));
        assert_eq!(credit_from_report(&shorter_side), Some(0.4));
        // Credit comes from the prefix (1 of 8), not from the raw record index
        // 6: `6 - 1` is below the shorter side, so credit() would not clamp it
        // and a credit taken from the index would be 5 / 8.
        let early = report(current, LogDiffVerdict::Diverged, 6, 8, Some(1));
        assert_eq!(early.first_divergent_record, Some(6));
        assert_eq!(credit_from_report(&early), Some(0.125));
        // Schema 1: a match is still full credit; a divergence is unmeasured.
        assert_eq!(
            credit_from_report(&report(1, LogDiffVerdict::Matched, 3, 3, None)),
            Some(1.0)
        );
        assert_eq!(
            credit_from_report(&report(1, LogDiffVerdict::Diverged, 4, 4, None)),
            None
        );
        for verdict in [
            LogDiffVerdict::Refused,
            LogDiffVerdict::NoResult,
            LogDiffVerdict::NoComparableMessages,
        ] {
            assert_eq!(
                credit_from_report(&report(current, verdict, 0, 0, None)),
                None
            );
        }
    }

    #[test]
    fn parity_records_carry_credit_only_when_measured() {
        let current = LOG_DIFF_REPORT_SCHEMA;
        let matched = record_for(&report(current, LogDiffVerdict::Matched, 3, 3, Some(3)));
        assert_eq!(matched.verdict, ParityVerdict::Matched);
        assert_eq!(matched.credit, Some(1.0));
        assert_eq!(matched.first_difference, None);

        let diverged = record_for(&report(current, LogDiffVerdict::Diverged, 4, 4, Some(2)));
        assert_eq!(diverged.verdict, ParityVerdict::Diverged);
        // The record carries the report's raw index (2 matched + 1 + 4
        // unselected) and its credit follows the matched prefix, 2 of 4.
        // Derived from the raw index it would be (7 - 1) / 4 = 1.5.
        assert_eq!(diverged.first_divergent_record, Some(7));
        assert_eq!(diverged.matched_prefix, Some(2));
        assert_eq!(diverged.credit, Some(0.5));
        let difference = diverged.first_difference.as_ref().unwrap();
        assert_eq!(
            difference.field.as_deref(),
            Some("token 4: `ret=3` vs `ret=4`")
        );
        assert_eq!(difference.syscall, Some(1));

        // One JSONL line round-trips exactly.
        let line = serde_json::to_string(&diverged).unwrap();
        assert!(!line.contains('\n'));
        assert!(line.contains(r#""verdict":"diverged""#));
        let parsed: ParityRecord = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed, diverged);
        parsed.validate().unwrap();

        // A refused comparison is unavailable with a reason, never a zero.
        let refused = record_for(&report(current, LogDiffVerdict::Refused, 0, 0, None));
        assert_eq!(refused.verdict, ParityVerdict::Unavailable);
        assert_eq!(refused.credit, None);
        assert!(refused.reason.as_deref().unwrap().contains("Refused"));

        // A report that breaks the evidence policy is unavailable too.
        let inconsistent = record_for(&report(current, LogDiffVerdict::Matched, 3, 3, Some(2)));
        assert_eq!(inconsistent.verdict, ParityVerdict::Unavailable);
        assert!(
            inconsistent
                .reason
                .as_deref()
                .unwrap()
                .contains("not cross-backend evidence")
        );

        let missing = ParityRecord::unmeasured(
            &cell(),
            ParityVerdict::ReferenceMissing,
            UnavailableClass::LogNotRetained,
            Some(ParityOperand::Reference),
            false,
            "ptrace verify retained no log",
            None,
            Some("cand.log"),
            "run-1",
            SHA,
        )
        .unwrap();
        assert_eq!(missing.credit, None);
        assert!(
            serde_json::to_string(&missing)
                .unwrap()
                .contains(r#""verdict":"reference-missing""#)
        );
    }

    #[test]
    fn a_parity_record_contradicting_its_verdict_is_refused() {
        let current = LOG_DIFF_REPORT_SCHEMA;
        let matched = record_for(&report(current, LogDiffVerdict::Matched, 3, 3, Some(3)));
        let diverged = record_for(&report(current, LogDiffVerdict::Diverged, 4, 4, Some(2)));
        let prefix = "disagrees with matched prefix";
        let cases: Vec<(&str, ParityRecord, &str)> = vec![
            (
                "a partial-credit match",
                ParityRecord {
                    credit: Some(0.5),
                    ..matched.clone()
                },
                prefix,
            ),
            (
                "a zero-credit match",
                ParityRecord {
                    credit: Some(0.0),
                    ..matched.clone()
                },
                prefix,
            ),
            (
                "an unmeasured match",
                ParityRecord {
                    credit: None,
                    ..matched.clone()
                },
                prefix,
            ),
            (
                "a full-credit divergence",
                ParityRecord {
                    credit: Some(1.0),
                    ..diverged.clone()
                },
                prefix,
            ),
            (
                "credit disagreeing with the prefix",
                ParityRecord {
                    credit: Some(0.25),
                    ..diverged.clone()
                },
                prefix,
            ),
            (
                "a measured verdict with a reason",
                ParityRecord {
                    reason: Some("why".into()),
                    ..diverged.clone()
                },
                "a measured verdict carries no reason",
            ),
            (
                "a short hermit sha",
                ParityRecord {
                    hermit_sha: "abc".into(),
                    ..matched.clone()
                },
                "hermit_sha must be 40 lowercase hex digits",
            ),
            // These three carry the class and operand their verdict needs, so
            // each is refused for the contradiction its label names.
            (
                "an unmeasured verdict with credit",
                ParityRecord {
                    verdict: ParityVerdict::CandidateMissing,
                    unavailable_class: Some(UnavailableClass::LogNotRetained),
                    operand: Some(ParityOperand::Candidate),
                    reason: Some("no log".into()),
                    candidate_log: None,
                    ..matched.clone()
                },
                "CandidateMissing is unmeasured and carries no credit or divergence",
            ),
            (
                "an unmeasured verdict without a reason",
                ParityRecord {
                    verdict: ParityVerdict::Unavailable,
                    unavailable_class: Some(UnavailableClass::LogDiffFailed),
                    operand: None,
                    reason: None,
                    credit: None,
                    matched_prefix: None,
                    ..matched.clone()
                },
                "Unavailable needs a reason",
            ),
            (
                "a missing log that is named",
                ParityRecord {
                    verdict: ParityVerdict::CandidateMissing,
                    unavailable_class: Some(UnavailableClass::LogNotRetained),
                    operand: Some(ParityOperand::Candidate),
                    reason: Some("no log".into()),
                    credit: None,
                    matched_prefix: None,
                    left_len: None,
                    right_len: None,
                    ..matched.clone()
                },
                "CandidateMissing names the log it says is missing",
            ),
        ];
        for (label, record, expected) in cases {
            let Err(error) = record.validate() else {
                panic!("{label} must be refused");
            };
            assert!(error.contains(expected), "{label}: {error}");
        }
    }

    /// Contradictions the credit check alone cannot see: `credit` clamps a
    /// prefix to the shorter side, a match has no divergence to locate, and an
    /// unmeasured verdict compared nothing.
    #[test]
    fn a_parity_record_with_fields_its_verdict_cannot_have_is_refused() {
        let current = LOG_DIFF_REPORT_SCHEMA;
        let matched = record_for(&report(current, LogDiffVerdict::Matched, 3, 3, Some(3)));
        let diverged = record_for(&report(current, LogDiffVerdict::Diverged, 2, 5, Some(2)));
        let unavailable = record_for(&report(current, LogDiffVerdict::Refused, 0, 0, None));
        assert_eq!(unavailable.verdict, ParityVerdict::Unavailable);
        let at = "parity record fixture/parity@kvm";
        let cases: Vec<(&str, ParityRecord, String)> = vec![
            (
                "a match whose prefix exceeds both sides",
                ParityRecord {
                    matched_prefix: Some(4),
                    ..matched.clone()
                },
                format!("{at}: matched prefix 4 exceeds the shorter compared stream of 3 | 3"),
            ),
            (
                "a divergence whose prefix exceeds the shorter side",
                ParityRecord {
                    matched_prefix: Some(3),
                    ..diverged.clone()
                },
                format!("{at}: matched prefix 3 exceeds the shorter compared stream of 2 | 5"),
            ),
            (
                "a match with a divergent record",
                ParityRecord {
                    first_divergent_record: Some(1),
                    ..matched.clone()
                },
                format!("{at}: a match carries no divergence position"),
            ),
            (
                "a match with a first difference",
                ParityRecord {
                    first_difference: diverged.first_difference.clone(),
                    ..matched.clone()
                },
                format!("{at}: a match carries no divergence position"),
            ),
            (
                "an unmeasured verdict with a reference length",
                ParityRecord {
                    left_len: Some(3),
                    ..unavailable.clone()
                },
                format!("{at}: Unavailable is unmeasured and carries no compared lengths"),
            ),
            (
                "an unmeasured verdict with a candidate length",
                ParityRecord {
                    right_len: Some(0),
                    ..unavailable.clone()
                },
                format!("{at}: Unavailable is unmeasured and carries no compared lengths"),
            ),
        ];
        for (label, record, expected) in cases {
            assert_eq!(record.validate().unwrap_err(), expected, "{label}");
        }
        for record in [&matched, &diverged, &unavailable] {
            record.validate().unwrap();
        }

        // A report that contradicts itself this way is refused when recorded,
        // not turned into a row.
        let mut located_match = report(current, LogDiffVerdict::Matched, 3, 3, Some(3));
        located_match.first_divergent_record = Some(2);
        assert_eq!(
            ParityRecord::from_comparison(
                &cell(),
                &located_match,
                true,
                "ref.log",
                "cand.log",
                "run-1",
                SHA
            )
            .unwrap_err(),
            format!("{at}: a match carries no divergence position")
        );
    }

    /// The two credit invariants no earlier check covers, each refused with
    /// its own message (review A, Low 8, at
    /// https://github.com/rrnewton/hermit/issues/3301#issuecomment-5911273090).
    /// Every case passes every earlier check, so the message is the one the
    /// label names. "A divergence cannot be full credit" is the only guard
    /// against a diverged record whose lengths are equal and whose prefix
    /// covers both, the one input for which [`credit`] returns 1.0.
    #[test]
    fn a_parity_record_breaking_a_credit_invariant_is_refused_by_that_invariant() {
        let current = LOG_DIFF_REPORT_SCHEMA;
        let full = report(current, LogDiffVerdict::Matched, 3, 3, Some(3));
        let matched = record_for(&full);
        let unequal_match = record_on(ParityBackend::Kvm, &full, false);
        let diverged = record_for(&report(current, LogDiffVerdict::Diverged, 4, 4, Some(2)));
        assert_eq!(
            (matched.credit, matched.unequalized_credit),
            (Some(1.0), None)
        );
        assert_eq!(
            (unequal_match.credit, unequal_match.unequalized_credit),
            (None, Some(1.0))
        );
        assert!(diverged.first_divergent_record.is_some() && diverged.first_difference.is_some());
        let at = "parity record fixture/parity@kvm";
        let exclusive = format!("{at}: credit and unequalized_credit are exclusive");
        let full_divergence = format!("{at}: a divergence cannot be full credit");
        let cases: Vec<(&str, ParityRecord, String)> = vec![
            (
                "clean credit with unequalized credit beside it",
                ParityRecord {
                    unequalized_credit: Some(1.0),
                    ..matched.clone()
                },
                exclusive.clone(),
            ),
            (
                "unequalized credit with clean credit beside it",
                ParityRecord {
                    credit: Some(1.0),
                    ..unequal_match.clone()
                },
                exclusive,
            ),
            (
                "a divergence over equal lengths whose prefix covers both",
                ParityRecord {
                    verdict: ParityVerdict::Diverged,
                    ..matched.clone()
                },
                full_divergence.clone(),
            ),
            (
                "the same divergence with its position located",
                ParityRecord {
                    verdict: ParityVerdict::Diverged,
                    first_divergent_record: diverged.first_divergent_record,
                    first_difference: diverged.first_difference.clone(),
                    ..matched.clone()
                },
                full_divergence.clone(),
            ),
            (
                "the same divergence measured with unequal inputs",
                ParityRecord {
                    verdict: ParityVerdict::Diverged,
                    ..unequal_match.clone()
                },
                full_divergence,
            ),
            (
                "a match that measured nothing",
                ParityRecord {
                    credit: None,
                    matched_prefix: None,
                    ..matched.clone()
                },
                format!("{at}: a match must be full credit, got None"),
            ),
            (
                "a match whose prefix is empty",
                ParityRecord {
                    credit: Some(0.0),
                    matched_prefix: Some(0),
                    ..matched.clone()
                },
                format!("{at}: a match must be full credit, got Some(0.0)"),
            ),
        ];
        for (label, record, expected) in cases {
            assert_eq!(record.validate().unwrap_err(), expected, "{label}");
        }
        for record in [&matched, &unequal_match, &diverged] {
            record.validate().unwrap();
        }
    }

    /// A mean credit below 1 never prints as full credit (review A, Low 3, at
    /// https://github.com/rrnewton/hermit/issues/3301#issuecomment-5911273090):
    /// one divergence at the last of 100,001 messages is credit
    /// 100000/100001, which four decimals alone round to `1.0000`.
    #[test]
    fn a_mean_credit_below_one_never_prints_as_full_credit() {
        let current = LOG_DIFF_REPORT_SCHEMA;
        let late = report(
            current,
            LogDiffVerdict::Diverged,
            100_000,
            100_001,
            Some(100_000),
        );
        let nearly_full = record_for(&late);
        assert_eq!(nearly_full.verdict, ParityVerdict::Diverged);
        assert_eq!(nearly_full.credit, Some(100_000.0 / 100_001.0));
        assert_eq!(format!("{:.4}", 100_000.0_f64 / 100_001.0), "1.0000");
        let full = record_for(&report(current, LogDiffVerdict::Matched, 3, 3, Some(3)));
        let unequal = record_on(ParityBackend::Kvm, &late, false);
        assert_eq!(unequal.unequalized_credit, nearly_full.credit);
        let summary = |records: Vec<ParityRecord>| {
            PostPassReport {
                path: PathBuf::from("parity.jsonl"),
                records,
                log_diff_runs: 0,
            }
            .summary_line()
        };
        for (records, expected) in [
            (
                vec![nearly_full.clone()],
                "; mean credit 0.9999 over 1 measured with equal inputs; none measured with \
                 unequal inputs;",
            ),
            (
                vec![nearly_full.clone(), full.clone()],
                "; mean credit 0.9999 over 2 measured with equal inputs;",
            ),
            (
                vec![full.clone(), full],
                "; mean credit 1.0000 over 2 measured with equal inputs;",
            ),
            (
                vec![unequal],
                "; none measured with equal inputs; mean credit 0.9999 over 1 measured with \
                 unequal inputs;",
            ),
        ] {
            let line = summary(records);
            assert!(line.contains(expected), "{expected:?} not in {line}");
        }
        // The floating-point mean itself can be exactly 1 over credits that
        // are not all full, so the text is decided from the credits.
        assert_eq!((BELOW_ONE + 1.0) / 2.0, 1.0);
        assert_eq!(mean_credit_text(&[BELOW_ONE, 1.0]), "0.9999");
        assert_eq!(mean_credit_text(&[1.0, 1.0]), "1.0000");
        assert_eq!(mean_credit_text(&[0.5, 1.0]), "0.7500");
        assert_eq!(mean_credit_text(&[0.0]), "0.0000");
    }

    /// The first divergences S0 measured on c-programs/add-key-enosys
    /// (https://github.com/rrnewton/hermit/issues/3301#issuecomment-5874842696):
    /// kvm and liteinst at compared message 14, sabre at 3, of 115 ptrace
    /// messages. Until the runner equalizes inputs, that credit is reported
    /// apart from clean credit, and it is small, not near 1.
    #[test]
    fn credit_from_unequal_inputs_is_kept_apart_from_clean_credit() {
        let current = LOG_DIFF_REPORT_SCHEMA;
        let measured = [
            (ParityBackend::Kvm, 115, 13, 13.0 / 115.0),
            (ParityBackend::Liteinst, 1013, 13, 13.0 / 1013.0),
            (ParityBackend::Sabre, 31, 2, 2.0 / 115.0),
        ];
        for (backend, right, prefix, expected) in measured {
            let diverged = report(current, LogDiffVerdict::Diverged, 115, right, Some(prefix));
            let row = record_on(backend, &diverged, false);
            assert_eq!(row.verdict, ParityVerdict::Diverged, "{backend}");
            assert!(!row.inputs_equalized);
            assert_eq!(
                row.credit, None,
                "{backend}: unequal inputs give no clean credit"
            );
            assert_eq!(row.unequalized_credit, Some(expected), "{backend}");
            assert_eq!(row.measured_credit(), Some(expected));
            assert!(expected < 0.12, "{backend}: {expected}");
            let line = serde_json::to_string(&row).unwrap();
            assert!(line.contains(r#""inputs_equalized":false"#), "{line}");
            assert!(line.contains(r#""credit":null"#), "{line}");
            let parsed: ParityRecord = serde_json::from_str(&line).unwrap();
            assert_eq!(parsed, row);

            // The same comparison with equalized inputs is clean credit.
            let clean = record_on(backend, &diverged, true);
            assert_eq!(clean.credit, Some(expected));
            assert_eq!(clean.unequalized_credit, None);
        }

        // Even a full match is not clean when the inputs differed.
        let matched = report(current, LogDiffVerdict::Matched, 3, 3, Some(3));
        let unequal_match = record_on(ParityBackend::Kvm, &matched, false);
        assert_eq!(unequal_match.credit, None);
        assert_eq!(unequal_match.unequalized_credit, Some(1.0));

        // A row without the flag is refused rather than defaulted.
        let mut value = serde_json::to_value(&unequal_match).unwrap();
        value.as_object_mut().unwrap().remove("inputs_equalized");
        assert!(serde_json::from_value::<ParityRecord>(value).is_err());
    }

    #[test]
    fn a_backend_that_cannot_be_equalized_reports_inputs_not_equalized() {
        let current = LOG_DIFF_REPORT_SCHEMA;
        let matched = report(current, LogDiffVerdict::Matched, 3, 3, Some(3));
        let dbt = record_on(ParityBackend::Dbt, &matched, false);
        assert_eq!(dbt.verdict, ParityVerdict::InputsNotEqualized);
        assert!(!dbt.verdict.is_measured());
        assert_eq!(dbt.measured_credit(), None);
        assert_eq!(dbt.matched_prefix, None);
        assert!(dbt.reason.as_deref().unwrap().contains("--bind"));
        assert_eq!(dbt.candidate_log.as_deref(), Some("cand.log"));
        assert!(
            serde_json::to_string(&dbt)
                .unwrap()
                .contains(r#""verdict":"inputs-not-equalized""#)
        );

        let dbt_cell = ParityCellId {
            backend: ParityBackend::Dbt,
            ..cell()
        };
        let error = ParityRecord::from_comparison(
            &dbt_cell, &matched, true, "ref.log", "cand.log", "run-1", SHA,
        )
        .unwrap_err();
        assert!(error.contains("cannot be equalized"), "{error}");
        for backend in [
            ParityBackend::Kvm,
            ParityBackend::Liteinst,
            ParityBackend::Sabre,
        ] {
            assert_eq!(backend.inputs_not_equalizable(), None);
        }
    }

    /// The first `log-diff-failed` site: a report whose verdict is not a
    /// measurement is `unavailable` with class `log-diff-failed`, no operand,
    /// both logs, and the verdict and any refusal as its reason, unless the
    /// backend is never compared, which is decided first.
    #[test]
    fn a_log_diff_verdict_that_is_not_a_measurement_is_log_diff_failed() {
        let current = LOG_DIFF_REPORT_SCHEMA;
        for verdict in [
            LogDiffVerdict::Refused,
            LogDiffVerdict::NoResult,
            LogDiffVerdict::NoComparableMessages,
            LogDiffVerdict::IdenticalSoFar,
        ] {
            let record = record_for(&report(current, verdict, 0, 0, None));
            record.validate().unwrap();
            assert_eq!(
                (record.verdict, record.unavailable_class, record.operand),
                (
                    ParityVerdict::Unavailable,
                    Some(UnavailableClass::LogDiffFailed),
                    None
                ),
                "{verdict:?}"
            );
            assert_eq!(
                record.reason,
                Some(format!("log-diff verdict was {verdict:?}"))
            );
            assert_eq!(record.measured_credit(), None, "{verdict:?}");
            assert_eq!(
                (
                    record.reference_log.as_deref(),
                    record.candidate_log.as_deref()
                ),
                (Some("ref.log"), Some("cand.log")),
                "{verdict:?}"
            );
        }
        let mut refused = report(current, LogDiffVerdict::Refused, 0, 0, None);
        refused.refusal = Some("detail".into());
        assert_eq!(
            record_for(&refused).reason.as_deref(),
            Some("log-diff verdict was Refused: detail")
        );
        let why = ParityBackend::Dbt.inputs_not_equalizable().unwrap();
        let dbt = record_on(ParityBackend::Dbt, &refused, false);
        assert_eq!(
            (dbt.verdict, dbt.unavailable_class, dbt.operand),
            (
                ParityVerdict::InputsNotEqualized,
                Some(UnavailableClass::InputsNotEqualized),
                Some(ParityOperand::Candidate)
            )
        );
        assert_eq!(dbt.reason.as_deref(), Some(why));
        let dbt_cell = ParityCellId {
            backend: ParityBackend::Dbt,
            ..cell()
        };
        assert_eq!(
            ParityRecord::from_comparison(
                &dbt_cell, &refused, true, "ref.log", "cand.log", "run-1", SHA,
            ),
            Err(format!(
                "parity cell {dbt_cell}: inputs cannot be equalized: {why}"
            ))
        );
    }

    /// The second `log-diff-failed` site: a matched or diverged report that
    /// fails the cross-backend evidence policy is `unavailable` with class
    /// `log-diff-failed` and the policy's refusal as its reason, never a
    /// measured verdict.
    #[test]
    fn a_report_that_is_not_cross_backend_evidence_is_log_diff_failed() {
        let bad = report(
            LOG_DIFF_REPORT_SCHEMA,
            LogDiffVerdict::Matched,
            3,
            3,
            Some(2),
        );
        let refusal = bad.require_cross_backend_evidence().unwrap_err();
        assert!(
            refusal.contains("matched 2 of 3 | 3 selected messages"),
            "{refusal}"
        );
        let record = record_for(&bad);
        record.validate().unwrap();
        assert_eq!(
            (record.verdict, record.unavailable_class, record.operand),
            (
                ParityVerdict::Unavailable,
                Some(UnavailableClass::LogDiffFailed),
                None
            )
        );
        assert_eq!(
            record.reason,
            Some(format!(
                "log-diff report is not cross-backend evidence: {refusal}"
            ))
        );
        assert_eq!((record.credit, record.unequalized_credit), (None, None));
        assert_eq!(
            (record.left_len, record.right_len, record.matched_prefix),
            (None, None, None)
        );
        assert_eq!(
            (
                record.reference_log.as_deref(),
                record.candidate_log.as_deref()
            ),
            (Some("ref.log"), Some("cand.log"))
        );
    }

    /// The third `log-diff-failed` site: a comparison whose report no record
    /// can carry names both logs and no operand; a backend that is never
    /// compared cannot carry that class.
    #[test]
    fn a_comparison_no_record_can_carry_is_log_diff_failed_with_both_logs() {
        let inputs = ParityGuestInputs {
            guest_argv: vec!["/bin/true".into()],
            guest_env: BTreeMap::new(),
            workdir: None,
            mounts: Vec::new(),
            epoch: Some(EPOCH.into()),
        };
        let operand = |log: &str, digit: &str| Operand {
            log: PathBuf::from(log),
            sha256: digit.repeat(64),
            bytes: 100,
            inputs: inputs.clone(),
        };
        let golden = "/artifacts/parity/golden/fixture/parity.detlog";
        let log = "/artifacts/fixture-parity-verify-kvm/run1_log_fixture.log";
        let (reference, candidate) = (operand(golden, "a"), operand(log, "b"));
        let config = PostPassConfig::new(
            Path::new("/artifacts"),
            Path::new("/bin/hermit"),
            "run-1",
            SHA,
        );
        let comparison = |backend, inputs_equalized| Comparison {
            index: 0,
            cell: ParityCellId { backend, ..cell() },
            reference: reference.clone(),
            candidate: candidate.clone(),
            inputs_equalized,
        };
        let record =
            unrecorded_comparison(&config, &comparison(ParityBackend::Kvm, true), "planted")
                .unwrap();
        record.validate().unwrap();
        assert_eq!(
            (record.verdict, record.unavailable_class, record.operand),
            (
                ParityVerdict::Unavailable,
                Some(UnavailableClass::LogDiffFailed),
                None
            )
        );
        assert_eq!(
            record.reason.as_deref(),
            Some("the comparison could not be recorded: planted")
        );
        assert_eq!(
            (
                record.reference_log.as_deref(),
                record.candidate_log.as_deref()
            ),
            (Some(golden), Some(log))
        );
        assert!(record.inputs_equalized);
        assert_eq!(record.measured_credit(), None);
        assert_eq!(
            (record.run_id.as_str(), record.hermit_sha.as_str()),
            ("run-1", SHA)
        );
        let error =
            unrecorded_comparison(&config, &comparison(ParityBackend::Dbt, false), "planted")
                .unwrap_err();
        assert!(error.contains("no comparison is attempted"), "{error}");
    }

    /// Only a backend whose inputs cannot be equalized reports
    /// `inputs-not-equalized`, and such a backend reports nothing else of a
    /// comparison: no measured verdict and no unmeasured class.
    #[test]
    fn inputs_not_equalized_is_for_a_backend_that_cannot_be_equalized_only() {
        let at = |backend: ParityBackend| format!("parity record fixture/parity@{backend}");
        let why = ParityBackend::Dbt.inputs_not_equalizable().unwrap();
        let (ine, candidate) = (
            Some(UnavailableClass::InputsNotEqualized),
            Some(ParityOperand::Candidate),
        );
        for backend in [
            ParityBackend::Kvm,
            ParityBackend::Liteinst,
            ParityBackend::Sabre,
        ] {
            assert_eq!(
                check_class(
                    &at(backend),
                    backend,
                    LedgerVerdict::InputsNotEqualized,
                    ine,
                    candidate
                ),
                Err(format!(
                    "{}: {backend} inputs can be equalized; a comparison with unequal inputs \
                     is measured and reports unequalized_credit",
                    at(backend)
                ))
            );
        }
        let (dbt, kvm) = (ParityBackend::Dbt, ParityBackend::Kvm);
        assert_eq!(
            check_class(
                &at(dbt),
                dbt,
                LedgerVerdict::InputsNotEqualized,
                ine,
                candidate
            ),
            Ok(())
        );
        let never_measured = format!(
            "{}: dbt inputs cannot be equalized, so it reports no measured comparison and \
             never equal inputs: {why}",
            at(dbt)
        );
        for verdict in [LedgerVerdict::Matched, LedgerVerdict::Diverged] {
            assert_eq!(
                check_class(&at(dbt), dbt, verdict, None, None),
                Err(never_measured.clone())
            );
        }
        for class in UnavailableClass::ALL {
            let (verdict, operand) = (class.verdicts()[0], class.operands()[0]);
            let on_dbt = check_class(&at(dbt), dbt, verdict, Some(class), operand);
            if class.group() == UnavailableGroup::Unmeasured {
                assert_eq!(
                    on_dbt,
                    Err(format!(
                        "{}: dbt inputs cannot be equalized, so no comparison is attempted \
                         and it carries no unmeasured class, got {class}",
                        at(dbt)
                    ))
                );
            } else {
                assert_eq!(on_dbt, Ok(()), "{class}");
            }
            assert_eq!(
                check_class(&at(kvm), kvm, verdict, Some(class), operand).is_ok(),
                class != UnavailableClass::InputsNotEqualized,
                "{class}"
            );
        }
        // The record check refuses the same contradictions.
        let matched = report(
            LOG_DIFF_REPORT_SCHEMA,
            LogDiffVerdict::Matched,
            3,
            3,
            Some(3),
        );
        let error = ParityRecord {
            backend: kvm,
            ..record_on(dbt, &matched, false)
        }
        .validate()
        .unwrap_err();
        assert!(error.contains("kvm inputs can be equalized"), "{error}");
        let clean = record_for(&matched);
        for record in [
            ParityRecord {
                backend: dbt,
                inputs_equalized: false,
                credit: None,
                unequalized_credit: Some(1.0),
                ..clean.clone()
            },
            ParityRecord {
                backend: dbt,
                ..clean
            },
        ] {
            assert_eq!(record.validate(), Err(never_measured.clone()));
        }
    }

    #[test]
    fn a_parity_record_contradicting_its_inputs_is_refused() {
        let current = LOG_DIFF_REPORT_SCHEMA;
        let diverged = report(current, LogDiffVerdict::Diverged, 4, 4, Some(2));
        let clean = record_for(&diverged);
        let unequal = record_on(ParityBackend::Kvm, &diverged, false);
        let dbt = record_on(ParityBackend::Dbt, &diverged, false);
        let cases: Vec<(&str, ParityRecord)> = vec![
            (
                "clean credit from unequal inputs",
                ParityRecord {
                    credit: Some(0.5),
                    unequalized_credit: None,
                    ..unequal.clone()
                },
            ),
            (
                "unequalized credit from equal inputs",
                ParityRecord {
                    credit: None,
                    unequalized_credit: Some(0.5),
                    ..clean.clone()
                },
            ),
            (
                "both credits",
                ParityRecord {
                    credit: Some(0.5),
                    ..unequal.clone()
                },
            ),
            (
                "unequalized credit disagreeing with the prefix",
                ParityRecord {
                    unequalized_credit: Some(0.25),
                    ..unequal.clone()
                },
            ),
            (
                "a measured dbt comparison",
                ParityRecord {
                    backend: ParityBackend::Dbt,
                    ..unequal.clone()
                },
            ),
            (
                "dbt with equal inputs",
                ParityRecord {
                    inputs_equalized: true,
                    ..dbt.clone()
                },
            ),
            (
                "inputs-not-equalized on a backend that can be equalized",
                ParityRecord {
                    backend: ParityBackend::Kvm,
                    ..dbt.clone()
                },
            ),
            (
                "an unmeasured verdict with unequalized credit",
                ParityRecord {
                    unequalized_credit: Some(0.5),
                    ..dbt.clone()
                },
            ),
        ];
        for (label, record) in cases {
            assert!(record.validate().is_err(), "{label} must be refused");
        }
        for record in [clean, unequal, dbt] {
            record.validate().unwrap();
        }
    }

    #[test]
    fn canonical_text_puts_one_cell_per_line() {
        let snapshot = generate(&repo_root()).unwrap();
        let text = canonical_text(&snapshot).unwrap();
        let cell_lines = text
            .lines()
            .filter(|line| line.starts_with("    {\"test_id\""))
            .count();
        assert_eq!(cell_lines, snapshot.cells.len());
        let reparsed: ParityCells = serde_json::from_str(&text).unwrap();
        assert_eq!(reparsed, snapshot);
    }

    #[test]
    fn parity_select_accepts_applicable_cells_and_refuses_the_rest() {
        let matrix = shipped_matrix();
        let applicable = first_cell(&matrix, ParityAvailability::selectable);
        let not_selectable = first_cell(&matrix, |availability| {
            availability.applicable() && !availability.selectable()
        });
        let not_applicable = first_cell(&matrix, |availability| !availability.applicable());
        assert_eq!(parse_parity_select("", &matrix), Ok(BTreeSet::new()));
        assert_eq!(parse_parity_select("  ", &matrix), Ok(BTreeSet::new()));
        assert_eq!(
            parse_parity_select(&format!("{applicable}, {not_selectable}"), &matrix),
            Ok(BTreeSet::from([applicable.clone(), not_selectable.clone()]))
        );
        let refusals = [
            (format!("{applicable},,{not_selectable}"), "empty entry"),
            (format!("{}@ptrace", applicable.test_id), "parity reference"),
            (
                format!("{}@qemu", applicable.test_id),
                "unknown parity backend",
            ),
            (
                "retired/no-such-test@kvm".to_string(),
                "no manifest declares",
            ),
            (not_applicable.to_string(), "is not applicable"),
            (applicable.test_id.clone(), "must be <test-id>@<backend>"),
        ];
        for (value, expected) in refusals {
            let error = parse_parity_select(&value, &matrix).unwrap_err();
            assert!(error.contains(expected), "{value:?}: {error}");
        }
    }

    fn parity_cell(test_id: &str, backend: ParityBackend) -> ParityCellId {
        ParityCellId {
            test_id: test_id.to_string(),
            backend,
        }
    }

    #[test]
    fn the_scope_is_every_cell_with_a_planned_side_and_dbt_retains_nothing() {
        let selection = BTreeSet::from([
            parity_cell("t/one", ParityBackend::Kvm),
            parity_cell("t/one", ParityBackend::Liteinst),
            parity_cell("t/two", ParityBackend::Kvm),
            parity_cell("t/three", ParityBackend::Dbt),
            parity_cell("t/six", ParityBackend::Kvm),
        ]);
        let explicit = BTreeSet::from([
            parity_cell("t/four", ParityBackend::Sabre),
            parity_cell("t/five", ParityBackend::Kvm),
        ]);
        let pair = |test: &str, backend: &str| (test.to_string(), backend.to_string());
        let planned = BTreeSet::from([
            pair("t/one", "kvm"),
            pair("t/one", "ptrace"),
            pair("t/two", "ptrace"),
            pair("t/three", "dbt"),
            pair("t/five", "kvm"),
        ]);
        let (scope, warnings) = post_pass_scope(&selection, &explicit, &planned);
        // A cell is this process's when it planned either side: t/one@liteinst
        // and t/two@kvm have only their ptrace side here, t/five@kvm only its
        // candidate. t/six (selected) and t/four (explicit) have neither.
        assert_eq!(
            scope,
            BTreeSet::from([
                parity_cell("t/one", ParityBackend::Kvm),
                parity_cell("t/one", ParityBackend::Liteinst),
                parity_cell("t/two", ParityBackend::Kvm),
                parity_cell("t/three", ParityBackend::Dbt),
                parity_cell("t/five", ParityBackend::Kvm),
            ])
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("t/four@sabre is not reported by this run")
                && warnings[0].contains("neither the ptrace nor the sabre verify cell of t/four"),
            "{warnings:?}"
        );
        // Only a cell with both sides planned here is compared, so only its
        // logs are retained.
        assert_eq!(
            retention_closure(&scope, &planned),
            BTreeSet::from([pair("t/one", "ptrace"), pair("t/one", "kvm")])
        );
        let (nothing, warnings) = post_pass_scope(&selection, &BTreeSet::new(), &BTreeSet::new());
        assert!(nothing.is_empty() && warnings.is_empty());

        // Processes that split the tests between them report every selected
        // cell exactly once.
        let first = BTreeSet::from([
            pair("t/one", "ptrace"),
            pair("t/one", "kvm"),
            pair("t/one", "liteinst"),
            pair("t/three", "dbt"),
        ]);
        let second = BTreeSet::from([
            pair("t/two", "ptrace"),
            pair("t/two", "kvm"),
            pair("t/six", "kvm"),
        ]);
        let (first, _) = post_pass_scope(&selection, &BTreeSet::new(), &first);
        let (second, _) = post_pass_scope(&selection, &BTreeSet::new(), &second);
        assert!(first.is_disjoint(&second));
        assert_eq!(&first | &second, selection);
    }

    #[test]
    fn a_dagrun_step_start_bounds_the_post_pass() {
        let now = Instant::now();
        let second = 1_000_000_000u64;
        assert!(
            step_deadline(None, Some(5 * second), now)
                .unwrap()
                .is_none()
        );
        let fresh = step_deadline(Some("1000000000"), Some(second), now)
            .unwrap()
            .unwrap();
        assert_eq!(
            fresh.at - now,
            PARITY_STEP_WALL_FLOOR - PARITY_STEP_EXIT_MARGIN
        );
        assert!(
            fresh
                .bound
                .contains("600 s wall bound less a 60 s exit margin")
        );
        // 100 s into the step, 440 s remain.
        let later = step_deadline(Some("1000000000"), Some(101 * second), now)
            .unwrap()
            .unwrap();
        assert_eq!(later.at - now, Duration::from_secs(440));
        // Past the bound, no time remains.
        let spent = step_deadline(Some("0"), Some(10_000 * second), now)
            .unwrap()
            .unwrap();
        assert_eq!(spent.at, now);
        for (started, now_ns, expected) in [
            (
                "soon",
                Some(second),
                "is not a CLOCK_MONOTONIC nanosecond count",
            ),
            (
                "5000000000",
                Some(second),
                "is later than CLOCK_MONOTONIC now",
            ),
            ("0", None, "CLOCK_MONOTONIC cannot be read"),
        ] {
            let error = step_deadline(Some(started), now_ns, now).unwrap_err();
            assert!(error.contains(expected), "{started}: {error}");
        }
    }

    #[test]
    fn golden_paths_stay_below_the_artifacts_directory() {
        let (golden, sidecar) = golden_paths(Path::new("/run"), "bucket/test").unwrap();
        assert_eq!(golden, Path::new("/run/parity/golden/bucket/test.detlog"));
        assert_eq!(
            sidecar,
            Path::new("/run/parity/golden/bucket/test.inputs.json")
        );
        for test_id in ["", "../escape", "/absolute", "a/../../b", "./dot"] {
            let error = golden_paths(Path::new("/run"), test_id).unwrap_err();
            assert!(
                error.contains("not a plain relative path"),
                "{test_id:?}: {error}"
            );
        }
    }

    const FAKE_LOG_DIFF: &str = include_str!("../tests/fixtures/fake-parity-log-diff.py");
    const EPOCH: &str = "2021-12-31T23:59:59Z";

    /// A scratch directory holding a fake `hermit` whose only command is the
    /// post-pass's `log-diff`, and the verify cells' retained logs.
    struct Fixture {
        dir: PathBuf,
        hermit: PathBuf,
    }

    impl Fixture {
        fn new(label: &str) -> Self {
            use std::os::unix::fs::PermissionsExt;
            let dir = std::env::temp_dir().join(format!(
                "hermit-parity-post-pass-{}-{label}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(dir.join("artifacts")).unwrap();
            let hermit = dir.join("hermit");
            fs::write(&hermit, FAKE_LOG_DIFF).unwrap();
            fs::set_permissions(&hermit, fs::Permissions::from_mode(0o755)).unwrap();
            Self { dir, hermit }
        }

        fn artifacts(&self) -> PathBuf {
            self.dir.join("artifacts")
        }

        fn config(&self) -> PostPassConfig {
            PostPassConfig::new(&self.artifacts(), &self.hermit, "run-1", SHA)
        }

        fn plant(&self, mode: &str) {
            fs::write(self.dir.join("logdiff-mode"), mode).unwrap();
        }

        fn log_diff_calls(&self) -> usize {
            fs::read_to_string(self.dir.join("log-diff-calls"))
                .map(|text| text.lines().count())
                .unwrap_or(0)
        }

        /// One verify attempt as the runner records it. `log` is the retained
        /// first-run log, written under `--verify-log-dir` when given.
        fn row(
            &self,
            test: &str,
            backend: &str,
            attempt: u64,
            outcome: &str,
            log: Option<&str>,
        ) -> CellResult {
            let cell_dir = self
                .artifacts()
                .join(format!("{}-verify-{backend}", test.replace('/', "-")));
            let mut argv = vec![
                "/fake/hermit".to_string(),
                "--backend".to_string(),
                backend.to_string(),
                "run".to_string(),
                "--env".to_string(),
                "LANG=C".to_string(),
                "--workdir".to_string(),
                "/work".to_string(),
                "--mount=type=bind,source=/data,target=/data".to_string(),
            ];
            if let Some(log) = log {
                let logs = cell_dir.join(format!("verify-logs/verify-{attempt}"));
                fs::create_dir_all(&logs).unwrap();
                fs::write(logs.join("run1_log_fixture.log"), log).unwrap();
                argv.extend([
                    "--keep-logs".to_string(),
                    VERIFY_LOG_DIR_FLAG.to_string(),
                    logs.display().to_string(),
                ]);
            }
            argv.extend([
                "--".to_string(),
                "/bin/guest".to_string(),
                "arg".to_string(),
            ]);
            serde_json::from_value(serde_json::json!({
                "schema": 1,
                "run_id": "run-1",
                "attempt": attempt,
                "hermit_sha": SHA,
                "source_tree_dirty": false,
                "binary_sha256": "c".repeat(64),
                "test": test,
                "category": "fixture",
                "lane": "portable",
                "mode": PARITY_MODE,
                "backend": backend,
                "classification": "required",
                "outcome": outcome,
                "reason": (outcome != "PASS").then(|| format!("fixture {outcome}")),
                "error_kind": null,
                "argv": argv,
                "guest_argv": ["/bin/guest", "arg"],
                "env": {"HERMIT_EPOCH": EPOCH},
                "cwd": "/",
                "shell_command": "fixture",
                "attempts": [],
                "artifact_dir": cell_dir.display().to_string(),
            }))
            .unwrap()
        }
    }

    /// `row` as the runner launches an equalized verify cell: each input
    /// directory of its cell directory bound at its guest path, and the guest
    /// environment naming that path. Inserted before `--keep-logs`, so the
    /// retained-log directory stays the fourth argument from the end.
    fn equalize(mut row: CellResult) -> CellResult {
        let at = row
            .argv
            .iter()
            .position(|arg| arg == "--keep-logs" || arg == "--")
            .unwrap();
        let mut launch = Vec::new();
        for input in EQUALIZED_INPUTS {
            launch.push(format!(
                "--bind={}/{}:{}",
                row.artifact_dir, input.cell_subdir, input.guest_path
            ));
            launch.extend([
                "--env".to_string(),
                format!("{}={}", input.env, input.guest_path),
            ]);
        }
        row.argv.splice(at..at, launch);
        row
    }

    /// `row` with the typed result and error kind the runner derived for it.
    fn typed(mut row: CellResult, result: ObservedResult, error_kind: Option<&str>) -> CellResult {
        row.result = Some(result);
        row.error_kind = error_kind.map(str::to_string);
        row
    }

    /// The `--verify-log-dir` of `row`.
    fn log_dir(row: &CellResult) -> PathBuf {
        let flag = row
            .argv
            .iter()
            .position(|arg| arg == VERIFY_LOG_DIR_FLAG)
            .unwrap();
        PathBuf::from(&row.argv[flag + 1])
    }

    /// `row` with its second run's log kept beside its first run's, as
    /// `--keep-logs` keeps both after a divergence.
    fn keep_second_run(row: CellResult, log: &str) -> CellResult {
        fs::write(log_dir(&row).join("run2_log_fixture.log"), log).unwrap();
        row
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if !std::thread::panicking() {
                let _ = fs::remove_dir_all(&self.dir);
            }
        }
    }

    const REFERENCE: &str = "INFO detcore: open\nINFO detcore: read 3\nINFO detcore: exit 0\n";
    const DIVERGENT: &str = "INFO detcore: open\nINFO detcore: read 4\nINFO detcore: exit 0\n";

    fn read_records(path: &Path) -> Vec<ParityRecord> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// Every cell in scope gets one line, in scope order, from the retained
    /// logs alone: measured cells carry credit and the first divergence,
    /// unmeasured cells a verdict, a typed class and a reason with null
    /// credit. A determinism failure on any attempt of either operand is
    /// `nondeterministic` even when a later attempt passed, and a mismatched
    /// reference writes no golden; another failure followed by a pass is not
    /// evidence against the passing log, which is compared. A cell typed
    /// `crash-error` that failed after its two runs matched under the strict
    /// comparison keeps its first-run log, which is compared
    /// (https://github.com/rrnewton/hermit/issues/3455), unless an attempt
    /// diverged in that row or an earlier one; a `matched` verdict over no log
    /// records, a canonical match that is not bitwise, a first run rejected
    /// before any comparison, an untyped failure, an infrastructure error, a
    /// stripped comparison and a SaBRe run whose execution path was incomplete
    /// or on fallback or native sites leave none, and a cell whose strict
    /// match retained no log is unmeasured.
    #[test]
    fn the_post_pass_measures_each_cell_from_retained_logs_only() {
        let fixture = Fixture::new("measures");
        let noise = "DEBUG reverie: host detail\n";
        let mut rows = vec![
            fixture.row("fx/same", "ptrace", 1, "PASS", Some(REFERENCE)),
            // The candidate's raw records differ outside the selected
            // messages; the selected messages match.
            fixture.row(
                "fx/same",
                "kvm",
                1,
                "PASS",
                Some(&format!("{noise}{REFERENCE}")),
            ),
            // A retried candidate whose first attempt timed out: the passing
            // attempt's log is the one compared, not the first attempt's.
            typed(
                fixture.row("fx/same", "liteinst", 1, "FAIL", Some(REFERENCE)),
                ObservedResult::Timeout,
                Some("wall-timeout"),
            ),
            fixture.row("fx/same", "liteinst", 2, "PASS", Some(DIVERGENT)),
            fixture.row("fx/nolog", "ptrace", 1, "PASS", Some(REFERENCE)),
            fixture.row("fx/nolog", "kvm", 1, "PASS", Some(REFERENCE)),
            fixture.row("fx/failed", "ptrace", 1, "FAIL", Some(REFERENCE)),
            fixture.row("fx/failed", "kvm", 1, "PASS", Some(REFERENCE)),
            {
                // A stripped-comparator pass is below L2 and anchors nothing.
                let mut stripped = fixture.row("fx/stripped", "ptrace", 1, "PASS", Some(REFERENCE));
                stripped.relaxations = vec!["comparator=stripped: fixture policy".into()];
                stripped
            },
            fixture.row("fx/stripped", "kvm", 1, "PASS", Some(REFERENCE)),
            fixture.row("fx/inapplicable", "ptrace", 1, "PASS", Some(REFERENCE)),
            fixture.row("fx/inapplicable", "kvm", 1, "HOST-INAPPLICABLE", None),
            fixture.row("fx/candidate-failed", "ptrace", 1, "PASS", Some(REFERENCE)),
            fixture.row("fx/candidate-failed", "kvm", 1, "FAIL", Some(REFERENCE)),
            fixture.row("fx/not-retained", "ptrace", 1, "PASS", Some(REFERENCE)),
            fixture.row("fx/not-retained", "kvm", 1, "PASS", None),
            // A mismatch then a pass, on the candidate and on the reference.
            fixture.row("fx/df-candidate", "ptrace", 1, "PASS", Some(REFERENCE)),
            typed(
                fixture.row("fx/df-candidate", "kvm", 1, "FAIL", Some(REFERENCE)),
                ObservedResult::DeterminismFailure,
                None,
            ),
            fixture.row("fx/df-candidate", "kvm", 2, "PASS", Some(REFERENCE)),
            typed(
                fixture.row("fx/df-reference", "ptrace", 1, "FAIL", Some(REFERENCE)),
                ObservedResult::DeterminismFailure,
                None,
            ),
            fixture.row("fx/df-reference", "ptrace", 2, "PASS", Some(REFERENCE)),
            fixture.row("fx/df-reference", "kvm", 1, "PASS", Some(REFERENCE)),
        ];
        // Rows from another mode never stand in for a verify cell.
        let mut chaos = fixture.row("fx/absent", "ptrace", 1, "PASS", Some(REFERENCE));
        chaos.mode = "chaos".into();
        rows.push(chaos);
        rows.reverse();
        // A retained log deleted before the post-pass.
        let deleted = rows
            .iter()
            .find(|row| row.test == "fx/nolog" && row.backend.as_deref() == Some("kvm"))
            .unwrap();
        let dir = deleted
            .argv
            .iter()
            .position(|arg| arg == VERIFY_LOG_DIR_FLAG)
            .map(|flag| PathBuf::from(&deleted.argv[flag + 1]))
            .unwrap();
        fs::remove_file(dir.join("run1_log_fixture.log")).unwrap();

        let scope = BTreeSet::from([
            parity_cell("fx/same", ParityBackend::Kvm),
            parity_cell("fx/same", ParityBackend::Liteinst),
            parity_cell("fx/same", ParityBackend::Dbt),
            parity_cell("fx/nolog", ParityBackend::Kvm),
            parity_cell("fx/failed", ParityBackend::Kvm),
            parity_cell("fx/stripped", ParityBackend::Kvm),
            parity_cell("fx/inapplicable", ParityBackend::Kvm),
            parity_cell("fx/candidate-failed", ParityBackend::Kvm),
            parity_cell("fx/not-retained", ParityBackend::Kvm),
            parity_cell("fx/absent", ParityBackend::Kvm),
            parity_cell("fx/df-candidate", ParityBackend::Kvm),
            parity_cell("fx/df-reference", ParityBackend::Kvm),
        ]);
        let config = fixture.config();
        let report = post_pass(&config, &scope, &rows).unwrap();
        assert_eq!(report.log_diff_runs, 2, "only the two measurable cells");
        assert_eq!(fixture.log_diff_calls(), 2);
        assert_eq!(report.path, fixture.artifacts().join(PARITY_JSONL));
        let written = read_records(&report.path);
        assert_eq!(
            written
                .iter()
                .map(|record| serde_json::to_value(record).unwrap())
                .collect::<Vec<_>>(),
            report
                .records
                .iter()
                .map(|record| serde_json::to_value(record).unwrap())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            written
                .iter()
                .map(|record| ParityCellId {
                    test_id: record.test_id.clone(),
                    backend: record.backend,
                })
                .collect::<Vec<_>>(),
            scope.iter().cloned().collect::<Vec<_>>(),
            "one line per cell in scope, in scope order"
        );
        let by_cell = |test: &str, backend: ParityBackend| {
            written
                .iter()
                .find(|record| record.test_id == test && record.backend == backend)
                .unwrap()
        };
        for record in &written {
            record.validate().unwrap();
            assert!(!record.inputs_equalized, "{record:?}");
            assert_eq!(
                record.credit, None,
                "no clean credit before inputs are equalized"
            );
            assert_eq!(record.run_id, "run-1");
            assert_eq!(record.hermit_sha, SHA);
        }

        let matched = by_cell("fx/same", ParityBackend::Kvm);
        assert_eq!(matched.verdict, ParityVerdict::Matched);
        assert_eq!((matched.unavailable_class, matched.operand), (None, None));
        assert_eq!(matched.unequalized_credit, Some(1.0));
        assert_eq!((matched.left_len, matched.right_len), (Some(3), Some(3)));
        assert_eq!(matched.matched_prefix, Some(3));
        assert!(matched.first_difference.is_none());

        let diverged = by_cell("fx/same", ParityBackend::Liteinst);
        assert_eq!(diverged.verdict, ParityVerdict::Diverged, "{diverged:?}");
        assert_eq!((diverged.unavailable_class, diverged.operand), (None, None));
        assert_eq!(diverged.matched_prefix, Some(1));
        assert_eq!(diverged.first_divergent_record, Some(2));
        assert_eq!(diverged.unequalized_credit, credit(1, 3, 3));
        let difference = diverged.first_difference.as_ref().unwrap();
        assert_eq!(
            difference.reference_message.as_deref(),
            Some("INFO detcore: read 3")
        );
        assert_eq!(
            difference.candidate_message.as_deref(),
            Some("INFO detcore: read 4")
        );
        assert!(
            diverged
                .candidate_log
                .as_deref()
                .unwrap()
                .ends_with("verify-logs/verify-2/run1_log_fixture.log"),
            "{diverged:?}"
        );

        let golden_path = fixture
            .artifacts()
            .join(PARITY_GOLDEN_DIR)
            .join("fx/same.detlog");
        assert_eq!(
            matched.reference_log.as_deref(),
            Some(path_text(&golden_path).as_str())
        );
        assert_eq!(fs::read_to_string(&golden_path).unwrap(), REFERENCE);
        let (reference, candidate) = (
            Some(ParityOperand::Reference),
            Some(ParityOperand::Candidate),
        );
        let unmeasured = [
            (
                "fx/same",
                ParityBackend::Dbt,
                ParityVerdict::InputsNotEqualized,
                UnavailableClass::InputsNotEqualized,
                candidate,
                "refuses --bind and --mount",
            ),
            (
                "fx/nolog",
                ParityBackend::Kvm,
                ParityVerdict::CandidateMissing,
                UnavailableClass::LogNotRetained,
                candidate,
                "retained 0 run1_log_* logs",
            ),
            (
                "fx/failed",
                ParityBackend::Kvm,
                ParityVerdict::Unavailable,
                UnavailableClass::FailedUntyped,
                reference,
                "the ptrace reference verify cell of fx/failed failed with no typed result (no \
                 error kind): fixture FAIL",
            ),
            (
                "fx/stripped",
                ParityBackend::Kvm,
                ParityVerdict::Unavailable,
                UnavailableClass::NoResultRow,
                reference,
                "passed only the stripped comparison, which is below L2",
            ),
            (
                "fx/inapplicable",
                ParityBackend::Kvm,
                ParityVerdict::CandidateMissing,
                UnavailableClass::HostInapplicable,
                candidate,
                "host-inapplicable",
            ),
            (
                "fx/candidate-failed",
                ParityBackend::Kvm,
                ParityVerdict::Unavailable,
                UnavailableClass::FailedUntyped,
                candidate,
                "the kvm candidate verify cell of fx/candidate-failed failed with no typed \
                 result (no error kind): fixture FAIL",
            ),
            (
                "fx/not-retained",
                ParityBackend::Kvm,
                ParityVerdict::CandidateMissing,
                UnavailableClass::LogNotRetained,
                candidate,
                "retained no logs",
            ),
            (
                "fx/absent",
                ParityBackend::Kvm,
                ParityVerdict::ReferenceMissing,
                UnavailableClass::NoResultRow,
                reference,
                "has no result row in this run",
            ),
            (
                "fx/df-candidate",
                ParityBackend::Kvm,
                ParityVerdict::Nondeterministic,
                UnavailableClass::DeterminismMismatch,
                candidate,
                "the kvm candidate verify cell of fx/df-candidate failed determinism on \
                 attempt 1 (its two runs diverged)",
            ),
            (
                "fx/df-reference",
                ParityBackend::Kvm,
                ParityVerdict::Nondeterministic,
                UnavailableClass::DeterminismMismatch,
                reference,
                "the ptrace reference verify cell of fx/df-reference failed determinism on \
                 attempt 1 (its two runs diverged)",
            ),
        ];
        for (test, backend, verdict, class, operand, reason) in unmeasured {
            let record = by_cell(test, backend);
            assert_eq!(record.verdict, verdict, "{record:?}");
            assert_eq!(record.unavailable_class, Some(class), "{record:?}");
            assert_eq!(record.operand, operand, "{record:?}");
            assert!(
                record.reason.as_deref().unwrap().contains(reason),
                "{test}@{backend}: {record:?}"
            );
            assert_eq!(record.unequalized_credit, None, "{record:?}");
            assert_eq!(record.matched_prefix, None, "{record:?}");
        }
        // Only a mismatch is called one.
        for record in written
            .iter()
            .filter(|record| record.verdict != ParityVerdict::Nondeterministic)
        {
            assert!(
                !record
                    .reason
                    .as_deref()
                    .unwrap_or("")
                    .contains("determinism"),
                "{record:?}"
            );
        }
        // A candidate that is missing still names the golden it would have
        // been compared with; a missing reference names none.
        assert_eq!(
            by_cell("fx/nolog", ParityBackend::Kvm)
                .reference_log
                .as_deref(),
            Some(
                path_text(
                    &fixture
                        .artifacts()
                        .join(PARITY_GOLDEN_DIR)
                        .join("fx/nolog.detlog")
                )
                .as_str()
            )
        );
        assert_eq!(by_cell("fx/absent", ParityBackend::Kvm).reference_log, None);
        // A reference with no golden names no log, and a mismatched candidate
        // names only the golden; neither mismatched log is read.
        for (test, golden) in [
            ("fx/failed", None),
            ("fx/df-reference", None),
            ("fx/df-candidate", Some("fx/df-candidate.detlog")),
        ] {
            let record = by_cell(test, ParityBackend::Kvm);
            let golden = golden
                .map(|name| path_text(&fixture.artifacts().join(PARITY_GOLDEN_DIR).join(name)));
            assert_eq!(record.reference_log, golden, "{record:?}");
            assert_eq!(record.candidate_log, None, "{record:?}");
        }
        assert!(
            !fixture
                .artifacts()
                .join(PARITY_GOLDEN_DIR)
                .join("fx/absent.detlog")
                .exists()
        );
        for test in ["fx/failed", "fx/df-reference"] {
            let (golden, sidecar) = golden_paths(&fixture.artifacts(), test).unwrap();
            assert!(!golden.exists() && !sidecar.exists(), "{test}");
        }

        // The sidecar describes the golden and the reference's guest inputs.
        let sidecar: ParityGoldenSidecar = serde_json::from_slice(
            &fs::read(
                fixture
                    .artifacts()
                    .join(PARITY_GOLDEN_DIR)
                    .join("fx/same.inputs.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(sidecar.schema, PARITY_GOLDEN_SIDECAR_SCHEMA);
        assert_eq!(
            (sidecar.test_id.as_str(), sidecar.backend.as_str()),
            ("fx/same", "ptrace")
        );
        assert_eq!(sidecar.log_sha256, sha256_hex(REFERENCE.as_bytes()));
        assert_eq!(sidecar.log_bytes, REFERENCE.len() as u64);
        assert_eq!(
            sidecar.guest_inputs,
            ParityGuestInputs {
                guest_argv: vec!["/bin/guest".into(), "arg".into()],
                guest_env: BTreeMap::from([("LANG".into(), "C".into())]),
                workdir: Some("/work".into()),
                mounts: vec!["--mount=type=bind,source=/data,target=/data".into()],
                epoch: Some(EPOCH.into()),
            }
        );
        assert_eq!(
            sidecar.guest_inputs_sha256,
            sha256_hex(&serde_json::to_vec(&sidecar.guest_inputs).unwrap())
        );
        let summary = report.summary_line();
        for part in [
            "12 cell(s)",
            "matched 1, diverged 1, nondeterministic 2, reference-missing 1, candidate-missing 3, \
             unavailable 3, inputs-not-equalized 1",
            "measured 2; no golden 5 (determinism-mismatch 2, failed-untyped 2, \
             host-inapplicable 1); not compared 1; unmeasured 4 (log-not-retained 2, \
             no-result-row 2)",
            "none measured with equal inputs; mean credit 0.6667 over 2 measured with unequal inputs",
            "2 log-diff comparison(s), 0 guest runs",
        ] {
            assert!(summary.contains(part), "{part:?} not in {summary}");
        }

        // A cell that failed after its two runs matched under the strict
        // comparison still has one deterministic log, which is compared
        // (https://github.com/rrnewton/hermit/issues/3455). These cells are
        // measured in a fixture of their own, so every count above stands.
        {
            let (matched, vacuous, canonical, rejected) = (
                matched_report(),
                vacuous_matched_report(),
                canonical_not_strict_report(),
                first_run_rejected_report(),
            );
            for (report, verdict, strict) in [
                (&matched, Verdict::Matched, true),
                (&vacuous, Verdict::Matched, false),
                (&canonical, Verdict::Matched, false),
                (&rejected, Verdict::NoResult, false),
            ] {
                let attempt = verify_attempt("1", "FAIL", false, Some(report));
                assert_eq!(
                    crate::runner::verification_verdict(&attempt),
                    Some(verdict),
                    "the fixture must retain a current report: {report}"
                );
                assert_eq!(
                    crate::runner::verification_matched_canonically(&attempt),
                    strict,
                    "{report}"
                );
            }
            // Its canonical comparison stands and its outputs match, so only
            // its missing bitwise parity keeps it from a strict match.
            let parsed = crate::canonical_verdict::VerificationReport::from_current_json_slice(
                canonical.as_bytes(),
            )
            .unwrap();
            assert_eq!(parsed.require_canonical_comparison(), Ok(()));
            assert_eq!(parsed.require_exact_output_match(), Ok(()));
            let error = parsed.require_canonical_match().unwrap_err();
            assert!(
                error.ends_with("verified=true verdict=matched bitwise_parity=false"),
                "{error}"
            );
            let fixture = Fixture::new("measures-matched-fail");
            // A FAIL row typed crash-error whose one attempt retains `report`.
            let crashed = |test: &str, backend: &str, attempt: u64, report: &str| {
                let mut row = typed(
                    fixture.row(test, backend, attempt, "FAIL", Some(REFERENCE)),
                    ObservedResult::CrashError,
                    None,
                );
                row.attempts = vec![verify_attempt("1", "FAIL", false, Some(report))];
                row
            };
            // Its comparison matched, then publishing the result failed.
            let mut untyped =
                fixture.row("fx/untyped-matched", "ptrace", 1, "FAIL", Some(REFERENCE));
            untyped.error_kind = Some("result-publication".into());
            untyped.attempts = vec![verify_attempt("1", "FAIL", false, Some(&matched))];
            // Its comparison matched, then the runner typed an infrastructure
            // error.
            let mut infrastructure = typed(
                fixture.row("fx/infra-matched", "ptrace", 1, "FAIL", Some(REFERENCE)),
                ObservedResult::InfrastructureError,
                None,
            );
            infrastructure.attempts = vec![verify_attempt("1", "FAIL", false, Some(&matched))];
            let mut stripped = crashed("fx/stripped-crash", "ptrace", 1, &matched);
            stripped.relaxations = vec!["comparator=stripped: fixture policy".into()];
            // A retry that left no log, after a FAIL whose comparison matched.
            let mut error = fixture.row("fx/fail-then-error", "kvm", 2, "ERROR", None);
            error.error_kind = Some("incomplete-verification-evidence".into());
            // An earlier attempt of the same row diverged.
            let mut diverged = crashed("fx/diverged-first", "kvm", 1, &matched);
            diverged.attempts = vec![
                verify_attempt("1", "FAIL", false, Some(DIVERGED_REPORT)),
                verify_attempt("2", "FAIL", false, Some(&matched)),
            ];
            // A SaBRe row whose one attempt matched strictly and recorded the
            // execution path of both runs, as the runner summarizes it.
            let sabre = |test: &str, outcome: &str, fallback_sites: usize, reason: &str| {
                let mut row = typed(
                    fixture.row(test, "sabre", 1, "FAIL", Some(REFERENCE)),
                    ObservedResult::CrashError,
                    None,
                );
                row.argv.insert(4, "--verify".into());
                let execution = serde_json::json!({
                    "schema": 1,
                    "guest_rpc_observed": true,
                    "ptrace_fallback_sites": fallback_sites,
                    "trusted_shared_object_sites": 0,
                    "trusted_shared_objects": [],
                });
                let mut attempt = verify_attempt("1", outcome, false, Some(&matched));
                attempt.argv = row.argv.clone();
                attempt.sabre_path_evidence = Some(format!("{execution}\n{execution}\n"));
                row.execution_path =
                    crate::runner::summarize_sabre_path_evidence(std::slice::from_ref(&attempt))
                        .unwrap();
                row.reason = Some(reason.into());
                row.attempts = vec![attempt];
                row
            };
            // Its attempt passed, and the runner failed the row because the
            // run used fallback sites, so its log measures no SaBRe run.
            let ineligible = sabre(
                "fx/sabre-path-ineligible",
                "PASS",
                3,
                "SaBRe execution path is incomplete or used fallback/native sites",
            );
            assert_eq!(
                ineligible.execution_path.as_ref().unwrap()["eligible"],
                false
            );
            // The same log from a run that stayed on SaBRe's own path.
            let eligible = sabre("fx/sabre-path-eligible", "FAIL", 0, "fixture FAIL");
            assert_eq!(eligible.execution_path.as_ref().unwrap()["eligible"], true);
            // Its comparison matched, but it retained no log.
            let mut no_log = typed(
                fixture.row("fx/matched-no-log", "kvm", 1, "FAIL", None),
                ObservedResult::CrashError,
                None,
            );
            no_log.attempts = vec![verify_attempt("1", "FAIL", false, Some(&matched))];
            let rows = vec![
                crashed("fx/crash-golden", "ptrace", 1, &matched),
                crashed("fx/crash-golden", "kvm", 1, &matched),
                // A mismatch on an earlier attempt is decided first.
                fixture.row("fx/df-then-matched", "ptrace", 1, "PASS", Some(REFERENCE)),
                typed(
                    fixture.row("fx/df-then-matched", "kvm", 1, "FAIL", Some(REFERENCE)),
                    ObservedResult::DeterminismFailure,
                    None,
                ),
                crashed("fx/df-then-matched", "kvm", 2, &matched),
                fixture.row("fx/diverged-first", "ptrace", 1, "PASS", Some(REFERENCE)),
                diverged,
                fixture.row("fx/fail-then-error", "ptrace", 1, "PASS", Some(REFERENCE)),
                crashed("fx/fail-then-error", "kvm", 1, &matched),
                error,
                crashed("fx/weak-match", "ptrace", 1, &vacuous),
                fixture.row("fx/weak-match", "kvm", 1, "PASS", Some(REFERENCE)),
                fixture.row("fx/first-rejected", "ptrace", 1, "PASS", Some(REFERENCE)),
                crashed("fx/first-rejected", "kvm", 1, &rejected),
                untyped,
                fixture.row("fx/untyped-matched", "kvm", 1, "PASS", Some(REFERENCE)),
                stripped,
                fixture.row("fx/stripped-crash", "kvm", 1, "PASS", Some(REFERENCE)),
                infrastructure,
                fixture.row("fx/infra-matched", "kvm", 1, "PASS", Some(REFERENCE)),
                crashed("fx/canonical-not-strict", "ptrace", 1, &canonical),
                fixture.row("fx/canonical-not-strict", "kvm", 1, "PASS", Some(REFERENCE)),
                fixture.row(
                    "fx/sabre-path-ineligible",
                    "ptrace",
                    1,
                    "PASS",
                    Some(REFERENCE),
                ),
                ineligible,
                fixture.row(
                    "fx/sabre-path-eligible",
                    "ptrace",
                    1,
                    "PASS",
                    Some(REFERENCE),
                ),
                eligible,
                fixture.row("fx/matched-no-log", "ptrace", 1, "PASS", Some(REFERENCE)),
                no_log,
            ];
            let scope = BTreeSet::from([
                parity_cell("fx/crash-golden", ParityBackend::Kvm),
                parity_cell("fx/df-then-matched", ParityBackend::Kvm),
                parity_cell("fx/diverged-first", ParityBackend::Kvm),
                parity_cell("fx/fail-then-error", ParityBackend::Kvm),
                parity_cell("fx/weak-match", ParityBackend::Kvm),
                parity_cell("fx/first-rejected", ParityBackend::Kvm),
                parity_cell("fx/untyped-matched", ParityBackend::Kvm),
                parity_cell("fx/stripped-crash", ParityBackend::Kvm),
                parity_cell("fx/infra-matched", ParityBackend::Kvm),
                parity_cell("fx/canonical-not-strict", ParityBackend::Kvm),
                parity_cell("fx/sabre-path-ineligible", ParityBackend::Sabre),
                parity_cell("fx/sabre-path-eligible", ParityBackend::Sabre),
                parity_cell("fx/matched-no-log", ParityBackend::Kvm),
            ]);
            let report = post_pass(&fixture.config(), &scope, &rows).unwrap();
            assert_eq!(report.log_diff_runs, 3, "the three cells with both logs");
            assert_eq!(fixture.log_diff_calls(), 3);
            let written = read_records(&report.path);
            assert_eq!(written.len(), 13);
            let by_test = |test: &str| {
                written
                    .iter()
                    .find(|record| record.test_id == test)
                    .unwrap()
            };
            let golden = |test: &str| {
                path_text(
                    &fixture
                        .artifacts()
                        .join(PARITY_GOLDEN_DIR)
                        .join(format!("{test}.detlog")),
                )
            };
            for record in &written {
                record.validate().unwrap();
            }
            // Each measured cell compares the golden with the candidate's
            // first-run log from the FAIL attempt whose comparison matched.
            for test in [
                "fx/crash-golden",
                "fx/fail-then-error",
                "fx/sabre-path-eligible",
            ] {
                let record = by_test(test);
                assert_eq!(record.verdict, ParityVerdict::Matched, "{record:?}");
                assert_eq!((record.unavailable_class, record.operand), (None, None));
                assert_eq!(record.unequalized_credit, Some(1.0), "{record:?}");
                assert_eq!((record.left_len, record.right_len), (Some(3), Some(3)));
                assert_eq!(record.matched_prefix, Some(3));
                assert!(record.first_difference.is_none(), "{record:?}");
                assert_eq!(record.reference_log, Some(golden(test)), "{record:?}");
                assert!(
                    record
                        .candidate_log
                        .as_deref()
                        .unwrap()
                        .ends_with("verify-logs/verify-1/run1_log_fixture.log"),
                    "{record:?}"
                );
            }
            // The reference that failed after its comparison matched is the
            // golden, and its sidecar records that row.
            let (golden_log, sidecar) =
                golden_paths(&fixture.artifacts(), "fx/crash-golden").unwrap();
            assert_eq!(fs::read_to_string(&golden_log).unwrap(), REFERENCE);
            let sidecar: ParityGoldenSidecar =
                serde_json::from_slice(&fs::read(&sidecar).unwrap()).unwrap();
            assert_eq!(
                (
                    sidecar.backend.as_str(),
                    sidecar.outcome.as_str(),
                    sidecar.attempt
                ),
                ("ptrace", "FAIL", 1)
            );
            let (reference, candidate) = (
                Some(ParityOperand::Reference),
                Some(ParityOperand::Candidate),
            );
            for (test, verdict, class, operand, reason, golden_named) in [
                (
                    "fx/df-then-matched",
                    ParityVerdict::Nondeterministic,
                    UnavailableClass::DeterminismMismatch,
                    candidate,
                    "the kvm candidate verify cell of fx/df-then-matched failed determinism on \
                     attempt 1 (its two runs diverged)",
                    true,
                ),
                (
                    "fx/diverged-first",
                    ParityVerdict::Nondeterministic,
                    UnavailableClass::DeterminismMismatch,
                    candidate,
                    "the kvm candidate verify cell of fx/diverged-first retained a diverged \
                     verification report on attempt 1 (its two runs diverged) although it ended \
                     with result crash-error (no error kind)",
                    true,
                ),
                (
                    "fx/weak-match",
                    ParityVerdict::Unavailable,
                    UnavailableClass::Crash,
                    reference,
                    "the ptrace reference verify cell of fx/weak-match ended FAIL with result \
                     crash-error (no error kind): fixture FAIL",
                    false,
                ),
                (
                    "fx/first-rejected",
                    ParityVerdict::Unavailable,
                    UnavailableClass::Crash,
                    candidate,
                    "the kvm candidate verify cell of fx/first-rejected ended FAIL with result \
                     crash-error (no error kind): fixture FAIL",
                    true,
                ),
                (
                    "fx/untyped-matched",
                    ParityVerdict::Unavailable,
                    UnavailableClass::FailedUntyped,
                    reference,
                    "the ptrace reference verify cell of fx/untyped-matched failed with no typed \
                     result (result-publication): fixture FAIL",
                    false,
                ),
                (
                    "fx/stripped-crash",
                    ParityVerdict::Unavailable,
                    UnavailableClass::Crash,
                    reference,
                    "the ptrace reference verify cell of fx/stripped-crash ended FAIL with result \
                     crash-error (no error kind): fixture FAIL",
                    false,
                ),
                (
                    "fx/infra-matched",
                    ParityVerdict::Unavailable,
                    UnavailableClass::InfrastructureError,
                    reference,
                    "the ptrace reference verify cell of fx/infra-matched ended FAIL with result \
                     infrastructure-error (no error kind): fixture FAIL",
                    false,
                ),
                (
                    "fx/canonical-not-strict",
                    ParityVerdict::Unavailable,
                    UnavailableClass::Crash,
                    reference,
                    "the ptrace reference verify cell of fx/canonical-not-strict ended FAIL with \
                     result crash-error (no error kind): fixture FAIL",
                    false,
                ),
                (
                    "fx/sabre-path-ineligible",
                    ParityVerdict::Unavailable,
                    UnavailableClass::Crash,
                    candidate,
                    "the sabre candidate verify cell of fx/sabre-path-ineligible ended FAIL with \
                     result crash-error (no error kind): SaBRe execution path is incomplete or \
                     used fallback/native sites",
                    true,
                ),
                (
                    "fx/matched-no-log",
                    ParityVerdict::CandidateMissing,
                    UnavailableClass::LogNotRetained,
                    candidate,
                    "the kvm candidate verify cell of fx/matched-no-log retained no logs (its \
                     argv has no --verify-log-dir)",
                    true,
                ),
            ] {
                let record = by_test(test);
                assert_eq!(record.verdict, verdict, "{record:?}");
                assert_eq!(record.unavailable_class, Some(class), "{record:?}");
                assert_eq!(record.operand, operand, "{record:?}");
                assert!(
                    record.reason.as_deref().unwrap().contains(reason),
                    "{test}: {record:?}"
                );
                assert_eq!(record.measured_credit(), None, "{record:?}");
                assert_eq!(record.candidate_log, None, "{record:?}");
                // A candidate with no log still names its reference's
                // golden; a reference with none writes no golden.
                assert_eq!(
                    record.reference_log,
                    golden_named.then(|| golden(test)),
                    "{record:?}"
                );
                let (golden_log, sidecar) = golden_paths(&fixture.artifacts(), test).unwrap();
                assert_eq!(
                    golden_log.exists() && sidecar.exists(),
                    golden_named,
                    "{test}"
                );
            }
            // Only a mismatch is called one.
            for record in written
                .iter()
                .filter(|record| record.verdict != ParityVerdict::Nondeterministic)
            {
                assert!(
                    !record
                        .reason
                        .as_deref()
                        .unwrap_or("")
                        .contains("determinism"),
                    "{record:?}"
                );
            }
            let summary = report.summary_line();
            for part in [
                "13 cell(s)",
                "matched 3, diverged 0, nondeterministic 2, reference-missing 0, \
                 candidate-missing 1, unavailable 7, inputs-not-equalized 0",
                "measured 3; no golden 9 (determinism-mismatch 2, crash 5, \
                 infrastructure-error 1, failed-untyped 1); not compared 0; unmeasured 1 \
                 (log-not-retained 1)",
                "none measured with equal inputs; mean credit 1.0000 over 3 measured with \
                 unequal inputs",
                "3 log-diff comparison(s), 0 guest runs",
            ] {
                assert!(summary.contains(part), "{part:?} not in {summary}");
            }
        }
    }

    /// A current verification report whose verdict is `diverged`, as a verify
    /// attempt retains it: the runner's own located-divergence fixture
    /// (`framework_terminal_refusal_cannot_revive_incidental_divergence`).
    const DIVERGED_REPORT: &str = r#"{"verified":false,"bitwise_parity":false,"verdict":"diverged","comparison":{"strictness":"canonical","compare_logs":true,"record_envelope":"all_records_v1","display_name":"BitwiseInfoV1","compare_io_buffers":true,"log_scope":"info","virtualize_time":true,"strip_lines":false,"canonicalize_addresses":true,"full_trace":true,"exact_remainder":true,"stripped_prefixes":["real-wall-clock-prefix/v1"],"canonicalizations":["host-address-to-first-appearance-ordinal/v1"],"ignore_lines":false,"skip_commit":false,"skip_detlog":false},"compared_log_messages":{"left":1,"right":1},"first_divergent_scheduler_turn":4,"first_divergent_virtual_nanoseconds":7,"first_divergent_record":9,"first_divergent_syscall":2,"first_divergent_left_message":"left","first_divergent_right_message":"right","compared_outputs":{"left":{"exit_code":null,"signal":null,"stdout_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","stdout_bytes":0,"stderr_sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","stderr_bytes":0},"right":{"exit_code":null,"signal":null,"stdout_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","stdout_bytes":0,"stderr_sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","stderr_bytes":0}},"no_result_reason":null,"infrastructure_error":null,"guest_exit_code":null,"guest_signal":null}"#;

    /// One attempt of a verify row as the runner records it, retaining
    /// `report` when given.
    fn verify_attempt(
        index: &str,
        outcome: &str,
        timed_out: bool,
        report: Option<&str>,
    ) -> crate::runner::AttemptResult {
        serde_json::from_value(serde_json::json!({
            "index": index,
            "outcome": outcome,
            "timed_out": timed_out,
            "argv": ["/fake/hermit", "run", "--", "/bin/guest", "arg"],
            "guest_argv": ["/bin/guest", "arg"],
            "env": {},
            "cwd": "/",
            "shell_command": "fixture",
            "verification_report": report,
        }))
        .unwrap()
    }

    /// [`DIVERGED_REPORT`] as an attempt retains it when its two runs matched
    /// under the strict comparison and the guest exited 7 both times: what a
    /// verify cell keeps when it fails after the comparison, for example on
    /// unexpected stdout or the wrong exit status.
    fn matched_report() -> String {
        let mut report: serde_json::Value = serde_json::from_str(DIVERGED_REPORT).unwrap();
        report["verified"] = true.into();
        report["bitwise_parity"] = true.into();
        report["verdict"] = "matched".into();
        for field in [
            "first_divergent_scheduler_turn",
            "first_divergent_virtual_nanoseconds",
            "first_divergent_record",
            "first_divergent_syscall",
            "first_divergent_left_message",
            "first_divergent_right_message",
        ] {
            report[field] = serde_json::Value::Null;
        }
        for side in ["left", "right"] {
            report["compared_outputs"][side]["exit_code"] = 7.into();
        }
        report["guest_exit_code"] = 7.into();
        report.to_string()
    }

    /// [`matched_report`] from a comparison of no log records: its verdict is
    /// `matched` but it is not a strict match, like the output-only comparison
    /// that [`crate::canonical_verdict::VerificationReport::require_canonical_match`]
    /// refuses.
    fn vacuous_matched_report() -> String {
        let mut report: serde_json::Value = serde_json::from_str(&matched_report()).unwrap();
        report["compared_log_messages"] = serde_json::json!({"left": 0, "right": 0});
        report.to_string()
    }

    /// [`matched_report`] without bitwise parity: its canonical comparison
    /// stands and its verdict is `matched`, but it fails only the last clause
    /// of [`crate::canonical_verdict::VerificationReport::require_canonical_match`].
    fn canonical_not_strict_report() -> String {
        let mut report: serde_json::Value = serde_json::from_str(&matched_report()).unwrap();
        report["bitwise_parity"] = false.into();
        report.to_string()
    }

    /// The report of an attempt whose first run exited 7: the default
    /// `--verify` rejects it before a second run, so nothing was compared.
    fn first_run_rejected_report() -> String {
        use crate::canonical_verdict::NoResultReason;
        use crate::canonical_verdict::VerificationReport;
        let mut report = VerificationReport::no_result();
        report.no_result_reason = Some(NoResultReason::FirstRunRejected {
            exit_code: Some(7),
            signal: None,
            stdout_bytes: 0,
            stderr_bytes: 0,
        });
        report.guest_exit_code = Some(7);
        serde_json::to_string(&report).unwrap()
    }

    /// A located divergence decides the side whatever result the runner typed
    /// its row with (review A, Low 4a, at
    /// https://github.com/rrnewton/hermit/issues/3301#issuecomment-5911273090).
    /// The runner gives a row whose terminal error kind is not
    /// product-attributed no result, and a row with a timed-out attempt the
    /// result `timeout`, before it looks for a divergence; either row can
    /// retain an attempt whose two runs diverged, and a later attempt can
    /// pass. Decision 1 of
    /// https://github.com/rrnewton/hermit/issues/3301#issuecomment-5895129378
    /// reads "Any mismatch in an operand's attempt history means no golden",
    /// so neither side is compared: the mismatched reference writes no golden,
    /// and neither passing attempt's log is read.
    #[test]
    fn a_diverged_report_under_any_typed_result_leaves_its_side_without_a_log() {
        let fixture = Fixture::new("diverged-report");
        let diverged = verify_attempt("1", "FAIL", false, Some(DIVERGED_REPORT));
        assert_eq!(
            crate::runner::verification_verdict(&diverged),
            Some(Verdict::Diverged),
            "the fixture must retain a current diverged report"
        );
        // Its comparison diverged, then publishing the result failed.
        let mut untyped_reference =
            fixture.row("fx/untyped-reference", "ptrace", 1, "FAIL", Some(REFERENCE));
        untyped_reference.error_kind = Some("result-publication".into());
        untyped_reference.attempts = vec![diverged.clone()];
        assert_eq!(untyped_reference.result, None);
        // Its comparison diverged, then a second attempt timed out.
        let mut timeout_candidate = typed(
            fixture.row("fx/timeout-candidate", "kvm", 1, "FAIL", Some(REFERENCE)),
            ObservedResult::Timeout,
            Some("wall-timeout"),
        );
        timeout_candidate.attempts = vec![diverged, verify_attempt("2", "FAIL", true, None)];
        let rows = vec![
            untyped_reference,
            fixture.row("fx/untyped-reference", "ptrace", 2, "PASS", Some(REFERENCE)),
            fixture.row("fx/untyped-reference", "kvm", 1, "PASS", Some(REFERENCE)),
            fixture.row("fx/timeout-candidate", "ptrace", 1, "PASS", Some(REFERENCE)),
            timeout_candidate,
            fixture.row("fx/timeout-candidate", "kvm", 2, "PASS", Some(REFERENCE)),
        ];
        assert!(
            rows.iter()
                .all(|row| row.result != Some(ObservedResult::DeterminismFailure))
        );
        let scope = BTreeSet::from([
            parity_cell("fx/untyped-reference", ParityBackend::Kvm),
            parity_cell("fx/timeout-candidate", ParityBackend::Kvm),
        ]);
        let report = post_pass(&fixture.config(), &scope, &rows).unwrap();
        assert_eq!(report.log_diff_runs, 0, "neither cell is compared");
        assert_eq!(fixture.log_diff_calls(), 0);
        let written = read_records(&report.path);
        assert_eq!(written.len(), 2);
        let by_test = |test: &str| {
            written
                .iter()
                .find(|record| record.test_id == test && record.backend == ParityBackend::Kvm)
                .unwrap()
        };
        for (test, operand, reason) in [
            (
                "fx/untyped-reference",
                ParityOperand::Reference,
                "the ptrace reference verify cell of fx/untyped-reference retained a diverged \
                 verification report on attempt 1 (its two runs diverged) although it ended with \
                 no typed result (result-publication), so there is no deterministic golden log \
                 to compare against: fixture FAIL",
            ),
            (
                "fx/timeout-candidate",
                ParityOperand::Candidate,
                "the kvm candidate verify cell of fx/timeout-candidate retained a diverged \
                 verification report on attempt 1 (its two runs diverged) although it ended with \
                 result timeout (wall-timeout), so it has no deterministic log to compare: \
                 fixture FAIL",
            ),
        ] {
            let record = by_test(test);
            record.validate().unwrap();
            assert_eq!(
                record.verdict,
                ParityVerdict::Nondeterministic,
                "{record:?}"
            );
            assert_eq!(
                record.unavailable_class,
                Some(UnavailableClass::DeterminismMismatch),
                "{record:?}"
            );
            assert_eq!(record.operand, Some(operand), "{record:?}");
            assert_eq!(record.reason.as_deref(), Some(reason), "{record:?}");
            assert_eq!(record.measured_credit(), None, "{record:?}");
            assert_eq!(record.candidate_log, None, "{record:?}");
        }
        // The mismatched reference wrote no golden; the candidate's own
        // reference passed, so its record still names that golden.
        let (golden, sidecar) = golden_paths(&fixture.artifacts(), "fx/untyped-reference").unwrap();
        assert!(!golden.exists() && !sidecar.exists());
        assert_eq!(by_test("fx/untyped-reference").reference_log, None);
        let (golden, _) = golden_paths(&fixture.artifacts(), "fx/timeout-candidate").unwrap();
        assert_eq!(
            by_test("fx/timeout-candidate").reference_log.as_deref(),
            Some(path_text(&golden).as_str())
        );
        let summary = report.summary_line();
        for part in [
            "matched 0, diverged 0, nondeterministic 2,",
            "measured 0; no golden 2 (determinism-mismatch 2);",
        ] {
            assert!(summary.contains(part), "{part:?} not in {summary}");
        }
    }

    /// The launch of an equalized verify cell in `cell_dir`, as
    /// [`ParityGuestInputs::from_result`] reads it.
    fn equalized_launch(cell_dir: &str) -> ParityGuestInputs {
        let mut guest_env = BTreeMap::from([
            ("LC_ALL".to_string(), "C".to_string()),
            ("E2E_TMPDIR".to_string(), "/tmp/test".to_string()),
        ]);
        let mut mounts = vec![format!("--bind={cell_dir}/workdir/1:/tmp/test")];
        for input in EQUALIZED_INPUTS {
            guest_env.insert(input.env.to_string(), input.guest_path.to_string());
            mounts.push(format!(
                "--bind={cell_dir}/{}:{}",
                input.cell_subdir, input.guest_path
            ));
        }
        ParityGuestInputs {
            guest_argv: vec!["/tmp/e2e/fixtures/program".into(), "multi".into()],
            guest_env,
            workdir: Some("/tmp/test".into()),
            mounts,
            epoch: Some(EPOCH.into()),
        }
    }

    /// Inputs are equal only when the runner's equalization applied on both
    /// sides and the guests were launched with one view; the host sources of
    /// the binds are the only thing allowed to differ.
    #[test]
    fn equal_inputs_need_equalization_on_both_sides_and_one_guest_view() {
        let reference = equalized_launch("/r/c-programs-mmap-determinism-verify-ptrace");
        let candidate = equalized_launch("/r/c-programs-mmap-determinism-verify-kvm");
        assert!(reference.is_equalized() && candidate.is_equalized());
        assert_ne!(reference, candidate, "the bind sources differ");
        assert!(inputs_equalized(&reference, &candidate));
        assert!(inputs_equalized(&candidate, &reference));
        assert!(inputs_equalized(&reference, &reference));

        let unequal = |label: &str, change: &dyn Fn(&mut ParityGuestInputs)| {
            let mut changed = candidate.clone();
            change(&mut changed);
            assert!(
                !inputs_equalized(&reference, &changed),
                "{label}: {changed:?}"
            );
            assert!(
                !inputs_equalized(&changed, &reference),
                "{label}, reversed: {changed:?}"
            );
        };
        // A change to only one side makes the views differ; the same change to
        // both leaves one view, which is still not the runner's equalization.
        let not_equalized = |label: &str, change: &dyn Fn(&mut ParityGuestInputs)| {
            unequal(label, change);
            let (mut left, mut right) = (reference.clone(), candidate.clone());
            change(&mut left);
            change(&mut right);
            assert!(!left.is_equalized(), "{label}: {left:?}");
            assert!(!inputs_equalized(&left, &right), "{label}, both sides");
        };
        for input in EQUALIZED_INPUTS {
            not_equalized(&format!("{} not bound", input.cell_subdir), &|inputs| {
                inputs
                    .mounts
                    .retain(|mount| bind_target(mount) != Some(input.guest_path));
            });
            not_equalized(&format!("{} bound twice", input.cell_subdir), &|inputs| {
                inputs
                    .mounts
                    .push(format!("--bind=/elsewhere:{}", input.guest_path));
            });
            not_equalized(&format!("{} named by a host path", input.env), &|inputs| {
                inputs.guest_env.insert(
                    input.env.to_string(),
                    format!("/r/shared/{}", input.cell_subdir),
                );
            });
            not_equalized(&format!("{} unset", input.env), &|inputs| {
                inputs.guest_env.remove(input.env);
            });
        }
        unequal("a host program path", &|inputs| {
            inputs.guest_argv[0] = "/r/kvm/fixtures/program".into();
        });
        unequal("another guest argument", &|inputs| {
            inputs.guest_argv[1] = "single".into();
        });
        unequal("another working directory", &|inputs| {
            inputs.workdir = Some("/test".into());
        });
        unequal("another guest variable", &|inputs| {
            inputs.guest_env.insert("TZ".into(), "UTC".into());
        });
        unequal("another bind target", &|inputs| {
            inputs.mounts[0] = "--bind=/r/kvm/workdir/1:/tmp/elsewhere".into();
        });
        unequal("an extra bind", &|inputs| {
            inputs.mounts.push("--bind=/tmp/extra".into());
        });
        unequal("a --mount with another source", &|inputs| {
            inputs
                .mounts
                .push("--mount=type=bind,source=/r/kvm/data,target=/data".into());
        });
        unequal("another epoch", &|inputs| {
            inputs.epoch = Some("2001-01-01T00:00:00+00:00".into());
        });

        // A --mount is compared verbatim, so one with the same source on both
        // sides is part of an equal view.
        let mount = "--mount=type=tmpfs,target=/test".to_string();
        let (mut with_mount, mut other_with_mount) = (reference.clone(), candidate.clone());
        with_mount.mounts.push(mount.clone());
        other_with_mount.mounts.push(mount);
        assert!(inputs_equalized(&with_mount, &other_with_mount));

        // Unrecorded epochs cannot be shown equal, even on both sides.
        let (mut left, mut right) = (reference.clone(), candidate.clone());
        left.epoch = None;
        right.epoch = None;
        assert!(!left.is_equalized());
        assert!(!inputs_equalized(&left, &right));

        // A launch from before the runner equalized inputs: host paths, no
        // binds below /tmp/e2e.
        let fixture = Fixture::new("equal-inputs-view");
        let pre = ParityGuestInputs::from_result(&fixture.row("fx/one", "kvm", 1, "PASS", None));
        assert!(!pre.is_equalized());
        assert!(!inputs_equalized(&pre, &pre));
        let equalized = ParityGuestInputs::from_result(&equalize(
            fixture.row("fx/one", "kvm", 1, "PASS", None),
        ));
        assert!(equalized.is_equalized(), "{equalized:?}");
        assert!(!inputs_equalized(&pre, &equalized));
    }

    /// A comparison whose two runs were both launched with the runner's
    /// equalized inputs earns clean credit; one where either side was not
    /// keeps its measurement in `unequalized_credit`; dbt is never measured.
    #[test]
    fn equalized_launches_earn_clean_credit_and_no_others_do() {
        let fixture = Fixture::new("equalized");
        let rows = vec![
            equalize(fixture.row("fx/one", "ptrace", 1, "PASS", Some(REFERENCE))),
            equalize(fixture.row("fx/one", "kvm", 1, "PASS", Some(REFERENCE))),
            equalize(fixture.row("fx/one", "liteinst", 1, "PASS", Some(DIVERGENT))),
            // A candidate launched without the equalized inputs.
            fixture.row("fx/one", "sabre", 1, "PASS", Some(REFERENCE)),
            fixture.row("fx/one", "dbt", 1, "PASS", Some(REFERENCE)),
            // A reference launched without them.
            fixture.row("fx/two", "ptrace", 1, "PASS", Some(REFERENCE)),
            equalize(fixture.row("fx/two", "kvm", 1, "PASS", Some(REFERENCE))),
        ];
        let scope = BTreeSet::from([
            parity_cell("fx/one", ParityBackend::Kvm),
            parity_cell("fx/one", ParityBackend::Liteinst),
            parity_cell("fx/one", ParityBackend::Sabre),
            parity_cell("fx/one", ParityBackend::Dbt),
            parity_cell("fx/two", ParityBackend::Kvm),
        ]);
        let report = post_pass(&fixture.config(), &scope, &rows).unwrap();
        let record = |test: &str, backend| {
            report
                .records
                .iter()
                .find(|record| record.test_id == test && record.backend == backend)
                .unwrap()
        };
        for record in &report.records {
            record.validate().unwrap();
        }

        let matched = record("fx/one", ParityBackend::Kvm);
        assert_eq!(matched.verdict, ParityVerdict::Matched, "{matched:?}");
        assert!(matched.inputs_equalized);
        assert_eq!(
            (matched.credit, matched.unequalized_credit),
            (Some(1.0), None)
        );

        let diverged = record("fx/one", ParityBackend::Liteinst);
        assert_eq!(diverged.verdict, ParityVerdict::Diverged, "{diverged:?}");
        assert!(diverged.inputs_equalized);
        assert_eq!(
            (diverged.credit, diverged.unequalized_credit),
            (credit(1, 3, 3), None)
        );

        for unequal in [
            record("fx/one", ParityBackend::Sabre),
            record("fx/two", ParityBackend::Kvm),
        ] {
            assert_eq!(unequal.verdict, ParityVerdict::Matched, "{unequal:?}");
            assert!(!unequal.inputs_equalized, "{unequal:?}");
            assert_eq!(
                (unequal.credit, unequal.unequalized_credit),
                (None, Some(1.0)),
                "{unequal:?}"
            );
        }

        let dbt = record("fx/one", ParityBackend::Dbt);
        assert_eq!(dbt.verdict, ParityVerdict::InputsNotEqualized);
        assert!(!dbt.inputs_equalized);
        assert_eq!(dbt.measured_credit(), None);
        assert_eq!(
            (dbt.unavailable_class, dbt.operand),
            (
                Some(UnavailableClass::InputsNotEqualized),
                Some(ParityOperand::Candidate)
            )
        );

        let summary = report.summary_line();
        assert!(
            summary.contains(
                "mean credit 0.6667 over 2 measured with equal inputs; mean credit 1.0000 \
                 over 2 measured with unequal inputs"
            ),
            "{summary}"
        );
    }

    /// A comparison the post-pass cannot trust is unavailable, never a
    /// verdict: a report about other bytes, an exit status contradicting the
    /// report, a comparator that does not finish, or no budget left.
    #[test]
    fn a_comparison_that_cannot_be_trusted_is_unavailable() {
        let fixture = Fixture::new("untrusted");
        let rows = vec![
            fixture.row("fx/one", "ptrace", 1, "PASS", Some(REFERENCE)),
            fixture.row("fx/one", "kvm", 1, "PASS", Some(REFERENCE)),
            fixture.row("fx/one", "liteinst", 1, "PASS", Some(DIVERGENT)),
            fixture.row("fx/empty", "ptrace", 1, "PASS", Some(REFERENCE)),
            fixture.row(
                "fx/empty",
                "kvm",
                1,
                "PASS",
                Some("DEBUG reverie: nothing selected\n"),
            ),
        ];
        let scope = BTreeSet::from([
            parity_cell("fx/one", ParityBackend::Kvm),
            parity_cell("fx/one", ParityBackend::Liteinst),
        ]);
        let run = |config: &PostPassConfig, scope: &BTreeSet<ParityCellId>| {
            post_pass(config, scope, &rows).unwrap()
        };
        let verdicts = |report: &PostPassReport| {
            report
                .records
                .iter()
                .map(|record| record.verdict)
                .collect::<Vec<_>>()
        };
        let honest = run(&fixture.config(), &scope);
        assert_eq!(
            verdicts(&honest),
            [ParityVerdict::Matched, ParityVerdict::Diverged]
        );

        for (mode, reason) in [
            (
                "lie-about-inputs",
                "the log-diff report's inputs are not the compared golden and candidate log",
            ),
            ("contradict-exit", "which contradicts its report verdict"),
        ] {
            fixture.plant(mode);
            let report = run(&fixture.config(), &scope);
            assert_eq!(report.log_diff_runs, 2, "{mode}");
            for record in &report.records {
                assert_eq!(
                    record.verdict,
                    ParityVerdict::Unavailable,
                    "{mode}: {record:?}"
                );
                assert!(
                    record.reason.as_deref().unwrap().contains(reason),
                    "{mode}: {record:?}"
                );
                assert_eq!(record.unequalized_credit, None, "{mode}");
                assert_eq!(
                    (record.unavailable_class, record.operand),
                    (Some(UnavailableClass::LogDiffFailed), None),
                    "{mode}: {record:?}"
                );
            }
        }

        fixture.plant("hang");
        let mut config = fixture.config();
        config.log_diff_timeout = Duration::from_millis(300);
        config.jobs = 2;
        let started = Instant::now();
        let report = run(&config, &scope);
        assert!(started.elapsed() < Duration::from_secs(30));
        for record in &report.records {
            assert_eq!(record.verdict, ParityVerdict::Unavailable, "{record:?}");
            assert_eq!(
                (record.unavailable_class, record.operand),
                (Some(UnavailableClass::LogDiffFailed), None),
                "{record:?}"
            );
            assert!(
                record
                    .reason
                    .as_deref()
                    .unwrap()
                    .contains("exceeded its 0.3 s bound and was killed"),
                "{record:?}"
            );
        }

        fixture.plant("");
        let mut config = fixture.config();
        config.budget = Duration::ZERO;
        let before = fixture.log_diff_calls();
        let report = run(&config, &scope);
        assert_eq!(report.log_diff_runs, 0);
        assert_eq!(fixture.log_diff_calls(), before);
        for record in &report.records {
            assert_eq!(record.verdict, ParityVerdict::Unavailable);
            assert_eq!(
                (record.unavailable_class, record.operand),
                (Some(UnavailableClass::LogDiffFailed), None),
                "{record:?}"
            );
            assert!(
                record
                    .reason
                    .as_deref()
                    .unwrap()
                    .contains("budget of 0 s ran out")
            );
        }

        // Nothing in common to compare is the comparator's refusal, which is
        // unavailable rather than a zero-credit divergence.
        let report = run(
            &fixture.config(),
            &BTreeSet::from([parity_cell("fx/empty", ParityBackend::Kvm)]),
        );
        let record = &report.records[0];
        assert_eq!(record.verdict, ParityVerdict::Unavailable, "{record:?}");
        assert!(
            record
                .reason
                .as_deref()
                .unwrap()
                .contains("NoComparableMessages"),
            "{record:?}"
        );
        assert_eq!(
            (record.unavailable_class, record.operand),
            (Some(UnavailableClass::LogDiffFailed), None),
            "{record:?}"
        );
    }

    /// Once written, a golden stands in for its source log only while it
    /// still has the hash its sidecar recorded for the same cell attempt.
    #[test]
    fn a_golden_is_reused_only_while_it_matches_its_sidecar() {
        let fixture = Fixture::new("reuse");
        let rows = vec![
            fixture.row("fx/one", "ptrace", 1, "PASS", Some(REFERENCE)),
            fixture.row("fx/one", "kvm", 1, "PASS", Some(REFERENCE)),
        ];
        let scope = BTreeSet::from([parity_cell("fx/one", ParityBackend::Kvm)]);
        let verdict = || post_pass(&fixture.config(), &scope, &rows).unwrap().records[0].clone();
        assert_eq!(verdict().verdict, ParityVerdict::Matched);
        let source =
            PathBuf::from(&rows[0].argv[rows[0].argv.len() - 4]).join("run1_log_fixture.log");
        fs::remove_file(&source).unwrap();
        let reused = verdict();
        assert_eq!(reused.verdict, ParityVerdict::Matched, "{reused:?}");

        // A sidecar that records another launch than the row's is not the
        // golden of that row.
        let (golden, sidecar_path) = golden_paths(&fixture.artifacts(), "fx/one").unwrap();
        let recorded = fs::read(&sidecar_path).unwrap();
        let mut sidecar: ParityGoldenSidecar = serde_json::from_slice(&recorded).unwrap();
        sidecar.guest_inputs.workdir = Some("/elsewhere".into());
        fs::write(&sidecar_path, serde_json::to_vec(&sidecar).unwrap()).unwrap();
        let mismatched = verdict();
        assert_eq!(
            mismatched.verdict,
            ParityVerdict::ReferenceMissing,
            "{mismatched:?}"
        );
        assert_eq!(
            (mismatched.unavailable_class, mismatched.operand),
            (
                Some(UnavailableClass::LogNotRetained),
                Some(ParityOperand::Reference)
            )
        );
        fs::write(&sidecar_path, &recorded).unwrap();
        assert_eq!(verdict().verdict, ParityVerdict::Matched);

        fs::write(&golden, DIVERGENT).unwrap();
        let refused = verdict();
        assert_eq!(
            refused.verdict,
            ParityVerdict::ReferenceMissing,
            "{refused:?}"
        );
        assert_eq!(refused.reference_log, None);
        assert_eq!(
            (refused.unavailable_class, refused.operand),
            (
                Some(UnavailableClass::LogNotRetained),
                Some(ParityOperand::Reference)
            )
        );
    }

    /// A comparison no longer fits once an outer bound, such as the
    /// enclosing dagrun step's, runs out: a slow log-diff is killed at that
    /// bound, not at its own, and later comparisons never start.
    #[test]
    fn an_outer_deadline_stops_a_slow_comparison_and_the_ones_after_it() {
        let fixture = Fixture::new("outer-deadline");
        let rows = vec![
            fixture.row("fx/one", "ptrace", 1, "PASS", Some(REFERENCE)),
            fixture.row("fx/one", "kvm", 1, "PASS", Some(REFERENCE)),
            fixture.row("fx/one", "liteinst", 1, "PASS", Some(DIVERGENT)),
            fixture.row("fx/one", "sabre", 1, "PASS", Some(REFERENCE)),
        ];
        let scope = BTreeSet::from([
            parity_cell("fx/one", ParityBackend::Kvm),
            parity_cell("fx/one", ParityBackend::Liteinst),
            parity_cell("fx/one", ParityBackend::Sabre),
        ]);
        fixture.plant("hang");
        let mut config = fixture.config();
        assert_eq!(config.log_diff_timeout, PARITY_LOG_DIFF_TIMEOUT);
        assert_eq!(config.budget, PARITY_POST_PASS_BUDGET);
        config.outer_deadline = Some(PostPassDeadline {
            at: Instant::now() + Duration::from_millis(400),
            bound: "the planted step bound".into(),
        });
        let started = Instant::now();
        let report = post_pass(&config, &scope, &rows).unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(10),
            "the post-pass outlived its outer bound: {elapsed:?}"
        );
        assert_eq!(report.log_diff_runs, 1, "{:?}", report.records);
        let reasons = report
            .records
            .iter()
            .map(|record| {
                assert_eq!(record.verdict, ParityVerdict::Unavailable, "{record:?}");
                assert_eq!(
                    (record.unavailable_class, record.operand),
                    (Some(UnavailableClass::LogDiffFailed), None),
                    "{record:?}"
                );
                record.reason.clone().unwrap()
            })
            .collect::<Vec<_>>();
        assert!(
            reasons[0].contains("hermit log-diff exceeded its 0.") && reasons[0].contains("killed"),
            "{reasons:?}"
        );
        for reason in &reasons[1..] {
            assert_eq!(
                reason,
                "the planted step bound ran out before this comparison started"
            );
        }
        // The nearer of the budget and the outer bound is the one kept.
        fixture.plant("");
        let mut config = fixture.config();
        config.budget = Duration::ZERO;
        config.outer_deadline = Some(PostPassDeadline {
            at: Instant::now() + Duration::from_secs(3600),
            bound: "a distant bound".into(),
        });
        let report = post_pass(&config, &scope, &rows).unwrap();
        assert_eq!(report.log_diff_runs, 0);
        assert!(report.records.iter().all(|record| {
            record
                .reason
                .as_deref()
                .unwrap()
                .contains("budget of 0 s ran out")
        }));
    }

    fn status(config: &PostPassConfig) -> PostPassStatus {
        serde_json::from_slice(&fs::read(config.status_path()).unwrap()).unwrap()
    }

    /// A post-pass removes the previous run's outputs before anything that
    /// can fail, and its status says whether `parity.jsonl` is this run's
    /// report. A failure or panic leaves no records.
    #[test]
    fn a_failed_post_pass_leaves_no_records_and_says_so() {
        let fixture = Fixture::new("status");
        let rows = vec![
            fixture.row("fx/one", "ptrace", 1, "PASS", Some(REFERENCE)),
            fixture.row("fx/one", "kvm", 1, "PASS", Some(REFERENCE)),
        ];
        let scope = BTreeSet::from([parity_cell("fx/one", ParityBackend::Kvm)]);
        let config = fixture.config();
        let report = post_pass(&config, &scope, &rows).unwrap();
        let complete = status(&config);
        assert_eq!(complete.state, PostPassState::Complete);
        assert_eq!(complete.schema, PARITY_STATUS_SCHEMA);
        assert_eq!(complete.cells, 1);
        assert_eq!(complete.scope, Some(vec!["fx/one@kvm".to_string()]));
        assert_eq!(
            complete.checked_scope(),
            Ok(Some(vec![parity_cell("fx/one", ParityBackend::Kvm)]))
        );
        assert_eq!(complete.records, path_text(&config.output));
        assert_eq!(
            complete.summary.as_deref(),
            Some(report.summary_line().as_str())
        );
        assert_eq!(complete.error, None);
        assert_eq!(complete.hermit_bin, path_text(&fixture.hermit));
        assert_eq!(
            complete.hermit_bin_sha256.as_deref(),
            Some(sha256_hex(FAKE_LOG_DIFF.as_bytes()).as_str())
        );
        let logdiff = config.output_dir.join(PARITY_LOGDIFF_DIR);
        assert!(logdiff.join("fx/one@kvm.json").exists());

        // A refused configuration: the earlier report and its log-diff
        // reports are gone, and the status names the failure.
        let mut refused = fixture.config();
        refused.hermit_sha = "not-a-sha".into();
        let error = post_pass(&refused, &scope, &rows).unwrap_err();
        assert!(!config.output.exists(), "a stale parity.jsonl survived");
        assert!(!logdiff.exists(), "stale log-diff reports survived");
        let failed = status(&config);
        assert_eq!(failed.state, PostPassState::Failed);
        assert_eq!(failed.error.as_deref(), Some(error.as_str()));
        assert_eq!(failed.summary, None);
        assert_eq!(failed.scope, Some(vec!["fx/one@kvm".to_string()]));

        // A panic inside is an error, not an unwinding harness.
        post_pass(&config, &scope, &rows).unwrap();
        PANIC_INSIDE_POST_PASS.with(|armed| armed.set(true));
        let error = post_pass(&config, &scope, &rows).unwrap_err();
        assert!(
            error.contains("the parity post-pass panicked: planted post-pass panic"),
            "{error}"
        );
        assert!(!config.output.exists());
        assert_eq!(status(&config).state, PostPassState::Failed);

        // Records that cannot be written: the status still says failed.
        fs::create_dir_all(&config.output).unwrap();
        let error = post_pass(&config, &scope, &rows).unwrap_err();
        assert!(error.contains("cannot remove"), "{error}");
        let failed = status(&config);
        assert_eq!(failed.state, PostPassState::Failed);
        assert!(config.output.is_dir(), "a directory is never removed");
        fs::remove_dir(&config.output).unwrap();

        // Clearing alone removes the records, the status and the log-diff
        // reports, and keeps the goldens.
        post_pass(&config, &scope, &rows).unwrap();
        clear_outputs(&config).unwrap();
        assert!(!config.output.exists());
        assert!(!config.status_path().exists());
        assert!(!logdiff.exists());
        assert!(
            golden_paths(&config.artifacts, "fx/one")
                .unwrap()
                .0
                .exists()
        );
    }

    /// A report no parity record can carry is that cell's problem: it is
    /// unavailable and the other cells are still reported.
    #[test]
    fn a_comparison_no_record_can_carry_is_unavailable_for_that_cell_only() {
        let fixture = Fixture::new("invalid-record");
        let rows = vec![
            fixture.row("fx/one", "ptrace", 1, "PASS", Some(REFERENCE)),
            fixture.row("fx/one", "kvm", 1, "PASS", Some(REFERENCE)),
            fixture.row("fx/one", "liteinst", 1, "PASS", Some(DIVERGENT)),
        ];
        let scope = BTreeSet::from([
            parity_cell("fx/one", ParityBackend::Kvm),
            parity_cell("fx/one", ParityBackend::Liteinst),
        ]);
        fixture.plant("invalid-record");
        let config = fixture.config();
        let report = post_pass(&config, &scope, &rows).unwrap();
        let kvm = &report.records[0];
        assert_eq!(kvm.verdict, ParityVerdict::Unavailable, "{kvm:?}");
        assert!(
            kvm.reason
                .as_deref()
                .unwrap()
                .starts_with("the comparison could not be recorded: "),
            "{kvm:?}"
        );
        assert_eq!(
            (kvm.unavailable_class, kvm.operand),
            (Some(UnavailableClass::LogDiffFailed), None),
            "{kvm:?}"
        );
        assert!(
            kvm.reference_log.is_some() && kvm.candidate_log.is_some(),
            "{kvm:?}"
        );
        assert_eq!(report.records[1].verdict, ParityVerdict::Diverged);
        assert_eq!(read_records(&config.output).len(), 2);
        assert_eq!(status(&config).state, PostPassState::Complete);
    }

    /// A cell whose candidate this process did not run is candidate-missing
    /// without reading the reference's log or writing a golden.
    #[test]
    fn a_candidate_run_elsewhere_is_missing_without_a_golden() {
        let fixture = Fixture::new("candidate-elsewhere");
        let rows = vec![fixture.row("fx/one", "ptrace", 1, "PASS", Some(REFERENCE))];
        let scope = BTreeSet::from([parity_cell("fx/one", ParityBackend::Kvm)]);
        let report = post_pass(&fixture.config(), &scope, &rows).unwrap();
        let record = &report.records[0];
        assert_eq!(
            record.verdict,
            ParityVerdict::CandidateMissing,
            "{record:?}"
        );
        assert_eq!(
            record.reason.as_deref(),
            Some("the kvm candidate verify cell of fx/one has no result row in this run")
        );
        assert_eq!(
            (record.unavailable_class, record.operand),
            (
                Some(UnavailableClass::NoResultRow),
                Some(ParityOperand::Candidate)
            )
        );
        assert_eq!(
            (record.reference_log.as_ref(), record.candidate_log.as_ref()),
            (None, None)
        );
        assert_eq!(fixture.log_diff_calls(), 0);
        assert!(
            !golden_paths(&fixture.artifacts(), "fx/one")
                .unwrap()
                .0
                .exists()
        );
    }

    /// A verify cell whose row the caller rejected is never compared. As the
    /// candidate or the reference, with or without rows, it takes the verdict
    /// and class the harness gives the same condition, with the caller's
    /// reason: a missing or invalid row is `no-result-row`, and logs the
    /// caller requires and did not find are `log-not-retained`. Each is
    /// unmeasured, in the floor as 0, so a rejected reference keeps every
    /// candidate of its test in the floor. Only a cell proven
    /// host-inapplicable, which left no log, has no golden, and no rejection
    /// is `infrastructure-error`.
    /// <https://github.com/rrnewton/hermit/issues/3301#issuecomment-5911273090>
    #[test]
    fn a_rejected_operand_takes_the_harness_class_with_the_callers_reason() {
        let fixture = Fixture::new("rejected");
        let candidates_rejected = [
            "fx/c-inapplicable",
            "fx/c-invalid",
            "fx/c-logs",
            "fx/c-missing",
        ];
        let references_rejected = [
            "fx/r-inapplicable",
            "fx/r-invalid",
            "fx/r-logs",
            "fx/r-missing",
        ];
        let pass =
            |test: &str, backend: &str| fixture.row(test, backend, 1, "PASS", Some(REFERENCE));
        let mut rows = Vec::new();
        for test in candidates_rejected {
            rows.push(pass(test, "ptrace"));
        }
        for test in references_rejected {
            rows.push(pass(test, "kvm"));
        }
        rows.push(pass("fx/r-missing", "sabre"));
        // Rows of a rejected side: the rejection decides, not the rows.
        rows.push(pass("fx/c-invalid", "kvm"));
        rows.push(pass("fx/r-invalid", "ptrace"));
        let mut scope = BTreeSet::from([parity_cell("fx/r-missing", ParityBackend::Sabre)]);
        for test in candidates_rejected.into_iter().chain(references_rejected) {
            scope.insert(parity_cell(test, ParityBackend::Kvm));
        }
        let mut config = fixture.config();
        let rejected = |test: &str, backend: &str, rejection: ParityRejection| {
            ((test.to_string(), backend.to_string()), rejection)
        };
        let why = str::to_string;
        config.rejected = BTreeMap::from([
            rejected(
                "fx/c-inapplicable",
                "kvm",
                ParityRejection::HostInapplicable(why("inapplicable c")),
            ),
            rejected(
                "fx/c-invalid",
                "kvm",
                ParityRejection::InvalidRow(why("invalid c")),
            ),
            rejected(
                "fx/c-logs",
                "kvm",
                ParityRejection::LogNotRetained(why("logs c")),
            ),
            rejected(
                "fx/c-missing",
                "kvm",
                ParityRejection::MissingRow(why("missing c")),
            ),
            rejected(
                "fx/r-inapplicable",
                "ptrace",
                ParityRejection::HostInapplicable(why("inapplicable r")),
            ),
            rejected(
                "fx/r-invalid",
                "ptrace",
                ParityRejection::InvalidRow(why("invalid r")),
            ),
            rejected(
                "fx/r-logs",
                "ptrace",
                ParityRejection::LogNotRetained(why("logs r")),
            ),
            rejected(
                "fx/r-missing",
                "ptrace",
                ParityRejection::MissingRow(why("missing r")),
            ),
        ]);
        let report = post_pass(&config, &scope, &rows).unwrap();
        assert_eq!(fixture.log_diff_calls(), 0);
        let found = report
            .records
            .iter()
            .map(|record| {
                (
                    format!("{}@{}", record.test_id, record.backend),
                    record.verdict,
                    record.unavailable_class,
                    record.operand,
                    record.reason.clone().unwrap_or_default(),
                    // Whether the record names the golden and the candidate's
                    // log.
                    (
                        record.reference_log.is_some(),
                        record.candidate_log.is_some(),
                    ),
                )
            })
            .collect::<Vec<_>>();
        let expect = |cell: &str,
                      verdict: ParityVerdict,
                      class: UnavailableClass,
                      operand: ParityOperand,
                      why: &str,
                      logs: (bool, bool)| {
            let (test, backend) = cell.split_once('@').unwrap();
            let role = match operand {
                ParityOperand::Reference => "ptrace reference".to_string(),
                ParityOperand::Candidate => format!("{backend} candidate"),
            };
            (
                cell.to_string(),
                verdict,
                Some(class),
                Some(operand),
                format!("the {role} verify cell of {test}: {why}"),
                logs,
            )
        };
        let (reference, candidate) = (ParityOperand::Reference, ParityOperand::Candidate);
        assert_eq!(
            found,
            [
                // A rejected candidate: the reference is good, so its golden
                // is written and named for a later `parity compare`.
                expect(
                    "fx/c-inapplicable@kvm",
                    ParityVerdict::CandidateMissing,
                    UnavailableClass::HostInapplicable,
                    candidate,
                    "inapplicable c",
                    (true, false),
                ),
                expect(
                    "fx/c-invalid@kvm",
                    ParityVerdict::Unavailable,
                    UnavailableClass::NoResultRow,
                    candidate,
                    "invalid c",
                    (true, false),
                ),
                expect(
                    "fx/c-logs@kvm",
                    ParityVerdict::CandidateMissing,
                    UnavailableClass::LogNotRetained,
                    candidate,
                    "logs c",
                    (true, false),
                ),
                expect(
                    "fx/c-missing@kvm",
                    ParityVerdict::CandidateMissing,
                    UnavailableClass::NoResultRow,
                    candidate,
                    "missing c",
                    (true, false),
                ),
                // A rejected reference names no golden. Its good candidate
                // is named wherever the cell is unmeasured, and not where
                // the reference left no golden at all.
                expect(
                    "fx/r-inapplicable@kvm",
                    ParityVerdict::ReferenceMissing,
                    UnavailableClass::HostInapplicable,
                    reference,
                    "inapplicable r",
                    (false, false),
                ),
                expect(
                    "fx/r-invalid@kvm",
                    ParityVerdict::Unavailable,
                    UnavailableClass::NoResultRow,
                    reference,
                    "invalid r",
                    (false, true),
                ),
                expect(
                    "fx/r-logs@kvm",
                    ParityVerdict::ReferenceMissing,
                    UnavailableClass::LogNotRetained,
                    reference,
                    "logs r",
                    (false, true),
                ),
                expect(
                    "fx/r-missing@kvm",
                    ParityVerdict::ReferenceMissing,
                    UnavailableClass::NoResultRow,
                    reference,
                    "missing r",
                    (false, true),
                ),
                expect(
                    "fx/r-missing@sabre",
                    ParityVerdict::ReferenceMissing,
                    UnavailableClass::NoResultRow,
                    reference,
                    "missing r",
                    (false, true),
                ),
            ]
        );
        // Only the cells proven host-inapplicable leave the floor. Every other
        // rejection is a harness or evidence defect and counts as 0 in it,
        // each candidate of a rejected reference included.
        for record in &report.records {
            let expected = if record.test_id.ends_with("-inapplicable") {
                UnavailableGroup::NoGolden
            } else {
                UnavailableGroup::Unmeasured
            };
            assert_eq!(
                record.unavailable_class.map(UnavailableClass::group),
                Some(expected),
                "{record:?}"
            );
        }
        for test in candidates_rejected {
            assert!(golden_paths(&fixture.artifacts(), test).unwrap().0.exists());
        }
        for test in references_rejected {
            assert!(!golden_paths(&fixture.artifacts(), test).unwrap().0.exists());
        }
    }

    /// Operands run under different or unrecorded `HERMIT_EPOCH` values are
    /// never compared.
    #[test]
    fn operands_with_different_epochs_are_unavailable() {
        let fixture = Fixture::new("epochs");
        let mut rows = vec![
            fixture.row("fx/one", "ptrace", 1, "PASS", Some(REFERENCE)),
            fixture.row("fx/one", "kvm", 1, "PASS", Some(REFERENCE)),
            fixture.row("fx/one", "liteinst", 1, "PASS", Some(REFERENCE)),
            fixture.row("fx/two", "ptrace", 1, "PASS", Some(REFERENCE)),
            fixture.row("fx/two", "kvm", 1, "PASS", Some(REFERENCE)),
        ];
        rows[1]
            .env
            .insert("HERMIT_EPOCH".into(), "2000-01-01T00:00:00Z".into());
        rows[2].env.remove("HERMIT_EPOCH");
        rows[3].env.remove("HERMIT_EPOCH");
        rows[4].env.remove("HERMIT_EPOCH");
        let scope = BTreeSet::from([
            parity_cell("fx/one", ParityBackend::Kvm),
            parity_cell("fx/one", ParityBackend::Liteinst),
            parity_cell("fx/two", ParityBackend::Kvm),
        ]);
        let report = post_pass(&fixture.config(), &scope, &rows).unwrap();
        assert_eq!(report.log_diff_runs, 0);
        assert_eq!(fixture.log_diff_calls(), 0);
        let reasons = report
            .records
            .iter()
            .map(|record| {
                assert_eq!(record.verdict, ParityVerdict::Unavailable, "{record:?}");
                assert!(record.reference_log.is_some() && record.candidate_log.is_some());
                assert_eq!(
                    record.unavailable_class,
                    Some(UnavailableClass::EpochNotShared),
                    "{record:?}"
                );
                record.reason.clone().unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            reasons,
            [
                format!(
                    "the operands ran with different HERMIT_EPOCH values: ptrace reference \
                     {EPOCH}, kvm candidate 2000-01-01T00:00:00Z"
                ),
                format!(
                    "the operands cannot be shown to share a HERMIT_EPOCH: ptrace reference \
                     {EPOCH}, liteinst candidate unrecorded"
                ),
                "the operands cannot be shown to share a HERMIT_EPOCH: ptrace reference \
                 unrecorded, kvm candidate unrecorded"
                    .to_string(),
            ]
        );
        // The operand is the side that recorded no epoch when only one did.
        assert_eq!(
            report
                .records
                .iter()
                .map(|record| record.operand)
                .collect::<Vec<_>>(),
            [None, Some(ParityOperand::Candidate), None]
        );
    }

    /// A verify cell that recorded a mismatch has no deterministic log even
    /// when both of its runs' logs were kept: as the candidate or as the
    /// reference, and when a later attempt passed or the mismatch was seen in
    /// an earlier repetition, the cell is `nondeterministic` with no credit,
    /// nothing is compared, and a mismatched reference writes no golden.
    #[test]
    fn a_mismatched_operand_is_never_compared_even_with_both_logs_retained() {
        let fixture = Fixture::new("mismatch-logs");
        let mismatch = |test: &str, backend: &str, attempt: u64| {
            keep_second_run(
                typed(
                    fixture.row(test, backend, attempt, "FAIL", Some(REFERENCE)),
                    ObservedResult::DeterminismFailure,
                    None,
                ),
                DIVERGENT,
            )
        };
        let mut rows = vec![
            fixture.row("fx/candidate", "ptrace", 1, "PASS", Some(REFERENCE)),
            mismatch("fx/candidate", "kvm", 1),
            fixture.row("fx/candidate-passed", "ptrace", 1, "PASS", Some(REFERENCE)),
            mismatch("fx/candidate-passed", "kvm", 1),
            fixture.row("fx/candidate-passed", "kvm", 2, "PASS", Some(REFERENCE)),
            mismatch("fx/reference", "ptrace", 1),
        ];
        for backend in ParityBackend::ALL {
            rows.push(fixture.row("fx/reference", backend.as_str(), 1, "PASS", Some(REFERENCE)));
        }
        for test in ["fx/repeated", "fx/repeated-reference"] {
            rows.push(fixture.row(test, "ptrace", 1, "PASS", Some(REFERENCE)));
            rows.push(fixture.row(test, "kvm", 1, "PASS", Some(REFERENCE)));
        }
        // Both runs' logs are on disk for every mismatched attempt.
        for row in rows
            .iter()
            .filter(|row| row.result == Some(ObservedResult::DeterminismFailure))
        {
            for log in ["run1_log_fixture.log", "run2_log_fixture.log"] {
                let log = log_dir(row).join(log);
                assert!(fs::metadata(&log).unwrap().len() > 0, "{}", log.display());
            }
        }
        let mut config = fixture.config();
        let repeated = "repetition 2 attempt 1 recorded a verify mismatch";
        config.nondeterministic = BTreeMap::from([
            (("fx/repeated".into(), "kvm".into()), repeated.to_string()),
            (
                ("fx/repeated-reference".into(), "ptrace".into()),
                repeated.to_string(),
            ),
        ]);
        let mut scope = BTreeSet::from([
            parity_cell("fx/candidate", ParityBackend::Kvm),
            parity_cell("fx/candidate-passed", ParityBackend::Kvm),
            parity_cell("fx/repeated", ParityBackend::Kvm),
            parity_cell("fx/repeated-reference", ParityBackend::Kvm),
        ]);
        scope.extend(ParityBackend::ALL.map(|backend| parity_cell("fx/reference", backend)));
        let report = post_pass(&config, &scope, &rows).unwrap();
        assert_eq!(report.records.len(), 8);
        assert_eq!((report.log_diff_runs, fixture.log_diff_calls()), (0, 0));

        let golden = |test: &str| path_text(&golden_paths(&fixture.artifacts(), test).unwrap().0);
        let on_attempt = |role: &str, test: &str, consequence: &str| {
            format!(
                "the {role} verify cell of {test} failed determinism on attempt 1 (its two runs \
                 diverged), so {consequence}: fixture FAIL"
            )
        };
        let earlier = |role: &str, test: &str, consequence: &str| {
            format!(
                "the {role} verify cell of {test} failed determinism: {repeated}, so {consequence}"
            )
        };
        let (no_log, no_golden) = (
            "it has no deterministic log to compare",
            "there is no deterministic golden log to compare against",
        );
        let (reference, candidate) = (ParityOperand::Reference, ParityOperand::Candidate);
        let mut expected = vec![
            (
                parity_cell("fx/candidate", ParityBackend::Kvm),
                candidate,
                on_attempt("kvm candidate", "fx/candidate", no_log),
                Some(golden("fx/candidate")),
            ),
            (
                parity_cell("fx/candidate-passed", ParityBackend::Kvm),
                candidate,
                on_attempt("kvm candidate", "fx/candidate-passed", no_log),
                Some(golden("fx/candidate-passed")),
            ),
            (
                parity_cell("fx/repeated", ParityBackend::Kvm),
                candidate,
                earlier("kvm candidate", "fx/repeated", no_log),
                Some(golden("fx/repeated")),
            ),
            (
                parity_cell("fx/repeated-reference", ParityBackend::Kvm),
                reference,
                earlier("ptrace reference", "fx/repeated-reference", no_golden),
                None,
            ),
        ];
        for backend in ParityBackend::ALL {
            expected.push((
                parity_cell("fx/reference", backend),
                reference,
                on_attempt("ptrace reference", "fx/reference", no_golden),
                None,
            ));
        }
        for (cell, operand, reason, reference_log) in expected {
            let record = report
                .records
                .iter()
                .find(|record| record.test_id == cell.test_id && record.backend == cell.backend)
                .unwrap();
            record.validate().unwrap();
            assert_eq!(
                (record.verdict, record.unavailable_class, record.operand),
                (
                    ParityVerdict::Nondeterministic,
                    Some(UnavailableClass::DeterminismMismatch),
                    Some(operand)
                ),
                "{cell}"
            );
            assert_eq!(record.reason.as_deref(), Some(reason.as_str()), "{cell}");
            assert_eq!(
                (record.credit, record.unequalized_credit),
                (None, None),
                "{cell}"
            );
            assert!(!record.inputs_equalized, "{cell}");
            // A mismatched log is never named, so never read.
            assert_eq!(
                (
                    record.reference_log.clone(),
                    record.candidate_log.as_deref()
                ),
                (reference_log, None),
                "{cell}"
            );
        }
        // Outside the mean and the floor.
        assert_eq!(
            UnavailableClass::DeterminismMismatch.group(),
            UnavailableGroup::NoGolden
        );
        for test in ["fx/reference", "fx/repeated-reference"] {
            let (golden, sidecar) = golden_paths(&fixture.artifacts(), test).unwrap();
            assert!(!golden.exists() && !sidecar.exists(), "{test}");
        }
        let summary = report.summary_line();
        for part in [
            "8 cell(s)",
            "matched 0, diverged 0, nondeterministic 8, reference-missing 0, candidate-missing 0, \
             unavailable 0, inputs-not-equalized 0",
            "measured 0; no golden 8 (determinism-mismatch 8); not compared 0; unmeasured 0",
            "none measured with equal inputs; none measured with unequal inputs",
            "0 log-diff comparison(s), 0 guest runs",
        ] {
            assert!(summary.contains(part), "{part:?} not in {summary}");
        }
    }

    /// A timeout is not a mismatch: as the candidate or as the reference it
    /// is `unavailable` with class `timeout`, a side with no golden that is
    /// outside the floor, and never called a determinism failure. A timeout
    /// flagged after both runs completed and matched under the strict
    /// comparison keeps its first-run log, which is compared, when that
    /// comparison is the single attempt of the selected row; an earlier row
    /// that timed out does not take the log away, and a row with more than
    /// one attempt keeps its timeout
    /// (https://github.com/rrnewton/hermit/issues/3455).
    #[test]
    fn a_timeout_is_unavailable_with_its_own_class() {
        let fixture = Fixture::new("timeout");
        let timed_out = |test: &str, backend: &str, kind: &str| {
            typed(
                fixture.row(test, backend, 1, "FAIL", Some(REFERENCE)),
                ObservedResult::Timeout,
                Some(kind),
            )
        };
        let rows = vec![
            fixture.row("fx/candidate", "ptrace", 1, "PASS", Some(REFERENCE)),
            timed_out("fx/candidate", "kvm", "wall-timeout"),
            timed_out("fx/reference", "ptrace", "cpu-timeout"),
            fixture.row("fx/reference", "kvm", 1, "PASS", Some(REFERENCE)),
        ];
        let scope = BTreeSet::from([
            parity_cell("fx/candidate", ParityBackend::Kvm),
            parity_cell("fx/reference", ParityBackend::Kvm),
        ]);
        let report = post_pass(&fixture.config(), &scope, &rows).unwrap();
        assert_eq!(fixture.log_diff_calls(), 0);
        let found = report
            .records
            .iter()
            .map(|record| {
                (
                    record.test_id.as_str(),
                    record.verdict,
                    record.unavailable_class,
                    record.operand,
                    record.reason.as_deref().unwrap_or(""),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            found,
            [
                (
                    "fx/candidate",
                    ParityVerdict::Unavailable,
                    Some(UnavailableClass::Timeout),
                    Some(ParityOperand::Candidate),
                    "the kvm candidate verify cell of fx/candidate ended FAIL with result \
                     timeout (wall-timeout): fixture FAIL"
                ),
                (
                    "fx/reference",
                    ParityVerdict::Unavailable,
                    Some(UnavailableClass::Timeout),
                    Some(ParityOperand::Reference),
                    "the ptrace reference verify cell of fx/reference ended FAIL with result \
                     timeout (cpu-timeout): fixture FAIL"
                ),
            ]
        );
        for record in &report.records {
            record.validate().unwrap();
            assert_eq!(record.measured_credit(), None, "{record:?}");
            assert_eq!(record.candidate_log, None, "{record:?}");
            assert!(
                !record.reason.as_deref().unwrap().contains("determinism"),
                "{record:?}"
            );
        }
        assert_eq!(
            UnavailableClass::Timeout.group(),
            UnavailableGroup::NoGolden
        );
        // The timed-out candidate's reference is good, so its golden is
        // written and named; the timed-out reference has none.
        let (golden, _) = golden_paths(&fixture.artifacts(), "fx/candidate").unwrap();
        assert_eq!(
            report.records[0].reference_log.as_deref(),
            Some(path_text(&golden).as_str())
        );
        assert_eq!(report.records[1].reference_log, None);
        let (golden, sidecar) = golden_paths(&fixture.artifacts(), "fx/reference").unwrap();
        assert!(!golden.exists() && !sidecar.exists());
        let summary = report.summary_line();
        for part in [
            "matched 0, diverged 0, nondeterministic 0, reference-missing 0, candidate-missing 0, \
             unavailable 2, inputs-not-equalized 0",
            "measured 0; no golden 2 (timeout 2); not compared 0; unmeasured 0",
        ] {
            assert!(summary.contains(part), "{part:?} not in {summary}");
        }

        // These cells are measured in a fixture of their own, so every count
        // above stands.
        {
            let (matched, canonical) = (matched_report(), canonical_not_strict_report());
            let fixture = Fixture::new("timeout-matched");
            let timed_out = |test: &str, backend: &str, attempt: u64, kind: &str| {
                typed(
                    fixture.row(test, backend, attempt, "FAIL", Some(REFERENCE)),
                    ObservedResult::Timeout,
                    Some(kind),
                )
            };
            // Both runs completed and were compared, then its CPU was found
            // at the budget.
            let mut late = timed_out("fx/late-cpu-timeout", "kvm", 1, "cpu-timeout");
            late.attempts = vec![verify_attempt("1", "FAIL", true, Some(&matched))];
            // An earlier row timed out before any comparison, as the runner
            // records it: an ERROR with no report, here with a partial
            // first-run log. The retry's one attempt matched.
            let mut earlier = typed(
                fixture.row(
                    "fx/earlier-timeout",
                    "ptrace",
                    1,
                    "ERROR",
                    Some("INFO detcore: open\n"),
                ),
                ObservedResult::Timeout,
                Some("wall-timeout"),
            );
            earlier.attempts = vec![verify_attempt("1", "ERROR", true, None)];
            let mut retried = typed(
                fixture.row("fx/earlier-timeout", "ptrace", 2, "FAIL", Some(REFERENCE)),
                ObservedResult::CrashError,
                None,
            );
            retried.attempts = vec![verify_attempt("1", "FAIL", false, Some(&matched))];
            // A row whose first attempt matched and whose last one timed out
            // with no comparison.
            let mut last = timed_out("fx/last-timed-out", "kvm", 1, "wall-timeout");
            last.attempts = vec![
                verify_attempt("1", "FAIL", false, Some(&matched)),
                verify_attempt("2", "ERROR", true, None),
            ];
            // A row whose first attempt timed out with no comparison and
            // whose last one matched: its retained log is the first
            // attempt's, so the match does not vouch for it.
            let mut two_attempts = timed_out("fx/two-attempt-row", "ptrace", 1, "wall-timeout");
            two_attempts.attempts = vec![
                verify_attempt("1", "ERROR", true, None),
                verify_attempt("2", "FAIL", false, Some(&matched)),
            ];
            // A retry whose CPU was found at the budget after its two runs
            // matched canonically but not bitwise, after a FAIL whose
            // comparison matched strictly. The selected retry decides.
            let mut crashed = typed(
                fixture.row("fx/timed-out-retry", "ptrace", 1, "FAIL", Some(REFERENCE)),
                ObservedResult::CrashError,
                None,
            );
            crashed.attempts = vec![verify_attempt("1", "FAIL", false, Some(&matched))];
            let mut retry = timed_out("fx/timed-out-retry", "ptrace", 2, "cpu-timeout");
            retry.attempts = vec![verify_attempt("1", "FAIL", true, Some(&canonical))];
            let rows = vec![
                fixture.row("fx/late-cpu-timeout", "ptrace", 1, "PASS", Some(REFERENCE)),
                late,
                earlier,
                retried,
                fixture.row("fx/earlier-timeout", "kvm", 1, "PASS", Some(REFERENCE)),
                fixture.row("fx/last-timed-out", "ptrace", 1, "PASS", Some(REFERENCE)),
                last,
                two_attempts,
                fixture.row("fx/two-attempt-row", "kvm", 1, "PASS", Some(REFERENCE)),
                crashed,
                retry,
                fixture.row("fx/timed-out-retry", "kvm", 1, "PASS", Some(REFERENCE)),
            ];
            let scope = BTreeSet::from([
                parity_cell("fx/late-cpu-timeout", ParityBackend::Kvm),
                parity_cell("fx/earlier-timeout", ParityBackend::Kvm),
                parity_cell("fx/last-timed-out", ParityBackend::Kvm),
                parity_cell("fx/two-attempt-row", ParityBackend::Kvm),
                parity_cell("fx/timed-out-retry", ParityBackend::Kvm),
            ]);
            let report = post_pass(&fixture.config(), &scope, &rows).unwrap();
            assert_eq!(fixture.log_diff_calls(), 2);
            let by_test = |test: &str| {
                report
                    .records
                    .iter()
                    .find(|record| record.test_id == test)
                    .unwrap()
            };
            let golden = |test: &str| golden_paths(&fixture.artifacts(), test).unwrap();
            for record in &report.records {
                record.validate().unwrap();
                assert!(
                    !record
                        .reason
                        .as_deref()
                        .unwrap_or("")
                        .contains("determinism"),
                    "{record:?}"
                );
            }
            for test in ["fx/late-cpu-timeout", "fx/earlier-timeout"] {
                let record = by_test(test);
                assert_eq!(record.verdict, ParityVerdict::Matched, "{record:?}");
                assert_eq!(record.unequalized_credit, Some(1.0), "{record:?}");
                assert_eq!(
                    record.reference_log,
                    Some(path_text(&golden(test).0)),
                    "{record:?}"
                );
                assert!(
                    record
                        .candidate_log
                        .as_deref()
                        .unwrap()
                        .ends_with("verify-logs/verify-1/run1_log_fixture.log"),
                    "{record:?}"
                );
            }
            // The reference's retry, whose one attempt matched, is the golden,
            // not the partial log of the row that timed out before it.
            let (golden_log, sidecar) = golden("fx/earlier-timeout");
            assert_eq!(fs::read_to_string(&golden_log).unwrap(), REFERENCE);
            let sidecar: ParityGoldenSidecar =
                serde_json::from_slice(&fs::read(&sidecar).unwrap()).unwrap();
            assert_eq!(
                (
                    sidecar.backend.as_str(),
                    sidecar.outcome.as_str(),
                    sidecar.attempt
                ),
                ("ptrace", "FAIL", 2)
            );
            let found = report
                .records
                .iter()
                .filter(|record| record.verdict != ParityVerdict::Matched)
                .map(|record| {
                    (
                        record.test_id.as_str(),
                        record.verdict,
                        record.unavailable_class,
                        record.operand,
                        record.reason.as_deref().unwrap_or(""),
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(
                found,
                [
                    (
                        "fx/last-timed-out",
                        ParityVerdict::Unavailable,
                        Some(UnavailableClass::Timeout),
                        Some(ParityOperand::Candidate),
                        "the kvm candidate verify cell of fx/last-timed-out ended FAIL with \
                         result timeout (wall-timeout): fixture FAIL"
                    ),
                    (
                        "fx/timed-out-retry",
                        ParityVerdict::Unavailable,
                        Some(UnavailableClass::Timeout),
                        Some(ParityOperand::Reference),
                        "the ptrace reference verify cell of fx/timed-out-retry ended FAIL with \
                         result timeout (cpu-timeout): fixture FAIL"
                    ),
                    (
                        "fx/two-attempt-row",
                        ParityVerdict::Unavailable,
                        Some(UnavailableClass::Timeout),
                        Some(ParityOperand::Reference),
                        "the ptrace reference verify cell of fx/two-attempt-row ended FAIL with \
                         result timeout (wall-timeout): fixture FAIL"
                    ),
                ]
            );
            for test in [
                "fx/last-timed-out",
                "fx/timed-out-retry",
                "fx/two-attempt-row",
            ] {
                let record = by_test(test);
                assert_eq!(record.measured_credit(), None, "{record:?}");
                assert_eq!(record.candidate_log, None, "{record:?}");
            }
            // The timed-out candidate's reference is good, so its golden is
            // named; a reference whose selected row keeps its timeout writes
            // none.
            assert_eq!(
                by_test("fx/last-timed-out").reference_log,
                Some(path_text(&golden("fx/last-timed-out").0))
            );
            for test in ["fx/timed-out-retry", "fx/two-attempt-row"] {
                assert_eq!(by_test(test).reference_log, None, "{test}");
                let (golden_log, sidecar) = golden(test);
                assert!(!golden_log.exists() && !sidecar.exists(), "{test}");
            }
            let summary = report.summary_line();
            for part in [
                "5 cell(s)",
                "matched 2, diverged 0, nondeterministic 0, reference-missing 0, \
                 candidate-missing 0, unavailable 3, inputs-not-equalized 0",
                "measured 2; no golden 3 (timeout 3); not compared 0; unmeasured 0",
                "none measured with equal inputs; mean credit 1.0000 over 2 measured with \
                 unequal inputs",
                "2 log-diff comparison(s), 0 guest runs",
            ] {
                assert!(summary.contains(part), "{part:?} not in {summary}");
            }
        }
    }

    /// One cell for each class a post-pass decides: each record carries its
    /// typed class and operand and counts in exactly one group, so the
    /// summary accounts for every cell once.
    #[test]
    fn every_class_a_post_pass_decides_is_typed_and_counted_once() {
        use ParityBackend::Dbt;
        use ParityBackend::Kvm;
        use ParityVerdict as V;
        use UnavailableClass as C;
        let fixture = Fixture::new("every-class");
        let tests = [
            "../c-escape",
            "c/crash",
            "c/diverged",
            "c/ended",
            "c/epoch",
            "c/inapplicable",
            "c/infra",
            "c/matched",
            "c/mismatch",
            "c/no-messages",
            "c/no-row",
            "c/not-retained",
            "c/oom",
            "c/sandbox",
            "c/timeout",
            "c/unreadable",
            "c/untyped",
            "c/unwritable",
        ];
        let mut rows = tests
            .iter()
            .map(|test| fixture.row(test, "ptrace", 1, "PASS", Some(REFERENCE)))
            .collect::<Vec<_>>();
        let kvm = |test: &str, outcome: &str| fixture.row(test, "kvm", 1, outcome, Some(REFERENCE));
        let mut ended = kvm("c/ended", "ERROR");
        ended.error_kind = Some("infrastructure".into());
        let mut epoch = kvm("c/epoch", "PASS");
        epoch
            .env
            .insert("HERMIT_EPOCH".into(), "2000-01-01T00:00:00Z".into());
        // A retained log that exists but cannot be read.
        let unreadable = kvm("c/unreadable", "PASS");
        let log = log_dir(&unreadable).join("run1_log_fixture.log");
        fs::remove_file(&log).unwrap();
        std::os::unix::fs::symlink(fixture.dir.join("no-such-log"), &log).unwrap();
        rows.extend([
            kvm("../c-escape", "PASS"),
            typed(kvm("c/crash", "FAIL"), ObservedResult::CrashError, None),
            fixture.row("c/diverged", "kvm", 1, "PASS", Some(DIVERGENT)),
            ended,
            epoch,
            fixture.row("c/inapplicable", "kvm", 1, "HOST-INAPPLICABLE", None),
            typed(
                kvm("c/infra", "ERROR"),
                ObservedResult::InfrastructureError,
                None,
            ),
            fixture.row("c/matched", "dbt", 1, "PASS", Some(REFERENCE)),
            kvm("c/matched", "PASS"),
            typed(
                kvm("c/mismatch", "FAIL"),
                ObservedResult::DeterminismFailure,
                None,
            ),
            fixture.row(
                "c/no-messages",
                "kvm",
                1,
                "PASS",
                Some("DEBUG reverie: nothing selected\n"),
            ),
            fixture.row("c/not-retained", "kvm", 1, "PASS", None),
            typed(kvm("c/oom", "FAIL"), ObservedResult::Oom, None),
            typed(
                kvm("c/sandbox", "ERROR"),
                ObservedResult::SandboxDenied,
                None,
            ),
            typed(
                kvm("c/timeout", "FAIL"),
                ObservedResult::Timeout,
                Some("wall-timeout"),
            ),
            unreadable,
            typed(
                kvm("c/untyped", "FAIL"),
                ObservedResult::ReplayFailure,
                None,
            ),
            kvm("c/unwritable", "PASS"),
        ]);
        // A directory where the golden goes cannot be replaced by it.
        fs::create_dir_all(
            golden_paths(&fixture.artifacts(), "c/unwritable")
                .unwrap()
                .0,
        )
        .unwrap();
        let mut scope = tests
            .iter()
            .map(|test| parity_cell(test, Kvm))
            .collect::<BTreeSet<_>>();
        scope.insert(parity_cell("c/matched", Dbt));
        let report = post_pass(&fixture.config(), &scope, &rows).unwrap();
        assert_eq!(
            (report.log_diff_runs, fixture.log_diff_calls()),
            (3, 3),
            "only c/matched@kvm, c/diverged and c/no-messages are compared"
        );
        let (reference, candidate) = (
            Some(ParityOperand::Reference),
            Some(ParityOperand::Candidate),
        );
        let found = report
            .records
            .iter()
            .map(|record| {
                (
                    record.test_id.as_str(),
                    record.backend,
                    record.verdict,
                    record.unavailable_class,
                    record.operand,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            found,
            [
                (
                    "../c-escape",
                    Kvm,
                    V::Unavailable,
                    Some(C::InvalidTestId),
                    None
                ),
                ("c/crash", Kvm, V::Unavailable, Some(C::Crash), candidate),
                ("c/diverged", Kvm, V::Diverged, None, None),
                ("c/ended", Kvm, V::Unavailable, Some(C::Ended), candidate),
                (
                    "c/epoch",
                    Kvm,
                    V::Unavailable,
                    Some(C::EpochNotShared),
                    None
                ),
                (
                    "c/inapplicable",
                    Kvm,
                    V::CandidateMissing,
                    Some(C::HostInapplicable),
                    candidate
                ),
                (
                    "c/infra",
                    Kvm,
                    V::Unavailable,
                    Some(C::InfrastructureError),
                    candidate
                ),
                (
                    "c/matched",
                    Dbt,
                    V::InputsNotEqualized,
                    Some(C::InputsNotEqualized),
                    candidate
                ),
                ("c/matched", Kvm, V::Matched, None, None),
                (
                    "c/mismatch",
                    Kvm,
                    V::Nondeterministic,
                    Some(C::DeterminismMismatch),
                    candidate
                ),
                (
                    "c/no-messages",
                    Kvm,
                    V::Unavailable,
                    Some(C::LogDiffFailed),
                    None
                ),
                (
                    "c/no-row",
                    Kvm,
                    V::CandidateMissing,
                    Some(C::NoResultRow),
                    candidate
                ),
                (
                    "c/not-retained",
                    Kvm,
                    V::CandidateMissing,
                    Some(C::LogNotRetained),
                    candidate
                ),
                ("c/oom", Kvm, V::Unavailable, Some(C::Oom), candidate),
                (
                    "c/sandbox",
                    Kvm,
                    V::Unavailable,
                    Some(C::SandboxDenied),
                    candidate
                ),
                (
                    "c/timeout",
                    Kvm,
                    V::Unavailable,
                    Some(C::Timeout),
                    candidate
                ),
                (
                    "c/unreadable",
                    Kvm,
                    V::CandidateMissing,
                    Some(C::LogUnreadable),
                    candidate
                ),
                (
                    "c/untyped",
                    Kvm,
                    V::Unavailable,
                    Some(C::FailedUntyped),
                    candidate
                ),
                (
                    "c/unwritable",
                    Kvm,
                    V::Unavailable,
                    Some(C::GoldenNotWritten),
                    reference
                ),
            ]
        );
        for record in &report.records {
            record.validate().unwrap();
            if record.verdict != V::Nondeterministic {
                assert!(
                    !record
                        .reason
                        .as_deref()
                        .unwrap_or("")
                        .contains("determinism"),
                    "{record:?}"
                );
            }
        }
        // Every class a record can carry, once; record-missing is the
        // ledger's, never a record's.
        for class in C::ALL {
            let cells = report
                .records
                .iter()
                .filter(|record| record.unavailable_class == Some(class))
                .count();
            assert_eq!(cells, usize::from(class != C::RecordMissing), "{class}");
        }
        let in_group = |wanted: UnavailableGroup| {
            report
                .records
                .iter()
                .filter(|record| record.unavailable_class.map(C::group) == Some(wanted))
                .count()
        };
        let measured = report
            .records
            .iter()
            .filter(|record| record.verdict.is_measured())
            .count();
        assert_eq!(
            (
                measured,
                in_group(UnavailableGroup::NoGolden),
                in_group(UnavailableGroup::NotCompared),
                in_group(UnavailableGroup::Unmeasured),
                in_group(UnavailableGroup::RecordMissing),
            ),
            (2, 9, 1, 7, 0)
        );
        let by_cell = |test: &str, backend: ParityBackend| {
            report
                .records
                .iter()
                .find(|record| record.test_id == test && record.backend == backend)
                .unwrap()
        };
        assert_eq!(by_cell("c/matched", Kvm).unequalized_credit, Some(1.0));
        assert_eq!(
            by_cell("c/diverged", Kvm).unequalized_credit,
            credit(1, 3, 3)
        );
        assert_eq!(
            by_cell("c/ended", Kvm).reason.as_deref(),
            Some(
                "the kvm candidate verify cell of c/ended ended ERROR with no typed result \
                 (infrastructure): fixture ERROR"
            )
        );
        assert_eq!(
            by_cell("c/no-messages", Kvm).reason.as_deref(),
            Some("log-diff verdict was NoComparableMessages")
        );
        let escape = by_cell("../c-escape", Kvm);
        assert_eq!(
            escape.reason,
            golden_paths(&fixture.artifacts(), "../c-escape").err()
        );
        assert!(
            escape.reference_log.is_none() && escape.candidate_log.is_some(),
            "{escape:?}"
        );
        let unwritable = by_cell("c/unwritable", Kvm);
        assert!(
            unwritable
                .reason
                .as_deref()
                .unwrap()
                .starts_with("cannot write the ptrace reference golden: cannot replace "),
            "{unwritable:?}"
        );
        let no_row = by_cell("c/no-row", Kvm);
        assert_eq!(
            (&no_row.reference_log, &no_row.candidate_log),
            (&None, &None)
        );
        assert!(
            !golden_paths(&fixture.artifacts(), "c/no-row")
                .unwrap()
                .0
                .exists()
        );
        assert_eq!(
            by_cell("c/inapplicable", Kvm).reference_log,
            Some(path_text(
                &golden_paths(&fixture.artifacts(), "c/inapplicable")
                    .unwrap()
                    .0
            ))
        );
        let summary = report.summary_line();
        for part in [
            "19 cell(s)",
            "matched 1, diverged 1, nondeterministic 1, reference-missing 0, candidate-missing 4, \
             unavailable 11, inputs-not-equalized 1",
            "measured 2; no golden 9 (determinism-mismatch 1, timeout 1, crash 1, oom 1, \
             infrastructure-error 1, sandbox-denied 1, failed-untyped 1, ended 1, \
             host-inapplicable 1); not compared 1; unmeasured 7 (log-not-retained 1, \
             log-unreadable 1, no-result-row 1, log-diff-failed 1, invalid-test-id 1, \
             golden-not-written 1, epoch-not-shared 1)",
            "none measured with equal inputs; mean credit 0.6667 over 2 measured with unequal inputs",
            "3 log-diff comparison(s), 0 guest runs",
        ] {
            assert!(summary.contains(part), "{part:?} not in {summary}");
        }
    }

    /// `path` spelled relative to this process's working directory.
    fn relative_to_cwd(path: &Path) -> PathBuf {
        let cwd = std::env::current_dir().unwrap();
        let mut relative = PathBuf::new();
        for _ in cwd.components().skip(1) {
            relative.push("..");
        }
        relative.join(path.strip_prefix("/").unwrap())
    }

    /// Relative spellings of every path the post-pass is handed are measured
    /// exactly as their absolute spellings are: a relative artifacts
    /// directory (a relative `--results` or `--artifacts`), relative retained
    /// log directories in the rows' `--verify-log-dir` (a relative
    /// `E2E_RESULT_ROOT`) and a relative hermit (`HERMIT_BIN`). The comparison
    /// runs in the artifacts directory and must not resolve any of them a
    /// second time below it, and the records name both logs by absolute
    /// paths, because `parity.jsonl` records no working directory.
    #[test]
    fn relative_artifacts_and_hermit_paths_are_still_measured() {
        // Deeper than the working directory, so that resolving the relative
        // spelling a second time from the artifacts directory cannot climb to
        // `/`, where surplus `..` components would hide the mistake.
        let depth = std::env::current_dir().unwrap().components().count();
        let label = format!("relative-paths{}", "/d".repeat(depth));
        let top = std::env::temp_dir().join(format!(
            "hermit-parity-post-pass-{}-relative-paths",
            std::process::id()
        ));
        let fixture = Fixture::new(&label);
        let mut rows = vec![
            fixture.row("fx/one", "ptrace", 1, "PASS", Some(REFERENCE)),
            fixture.row("fx/one", "kvm", 1, "PASS", Some(REFERENCE)),
            fixture.row("fx/one", "liteinst", 1, "PASS", Some(DIVERGENT)),
        ];
        for row in &mut rows {
            let flag = row
                .argv
                .iter()
                .position(|arg| arg == VERIFY_LOG_DIR_FLAG)
                .unwrap();
            let logs = relative_to_cwd(Path::new(&row.argv[flag + 1]));
            assert!(logs.is_relative() && logs.is_dir(), "{}", logs.display());
            row.argv[flag + 1] = path_text(&logs);
        }
        let scope = BTreeSet::from([
            parity_cell("fx/one", ParityBackend::Kvm),
            parity_cell("fx/one", ParityBackend::Liteinst),
        ]);
        let artifacts = relative_to_cwd(&fixture.artifacts());
        let relative_hermit = relative_to_cwd(&fixture.hermit);
        assert!(artifacts.is_relative() && relative_hermit.is_relative());
        // Each recorded path is absolute and names the file it was measured
        // from.
        let absolute_file = |path: Option<&str>| {
            let path = Path::new(path.expect("a measured record names both logs"));
            assert!(path.is_absolute() && path.is_file(), "{}", path.display());
            fs::read(path).unwrap()
        };
        // The absolute hermit isolates the logs and the report path; the
        // relative one adds the program path.
        for (pass, hermit) in [fixture.hermit.clone(), relative_hermit].iter().enumerate() {
            let config = PostPassConfig::new(&artifacts, hermit, "run-1", SHA);
            let report = post_pass(&config, &scope, &rows).unwrap();
            let verdicts = report
                .records
                .iter()
                .map(|record| (record.backend, record.verdict, record.reason.clone()))
                .collect::<Vec<_>>();
            assert_eq!(
                verdicts,
                [
                    (ParityBackend::Kvm, ParityVerdict::Matched, None),
                    (ParityBackend::Liteinst, ParityVerdict::Diverged, None),
                ],
                "hermit {}",
                hermit.display()
            );
            assert_eq!(fixture.log_diff_calls(), 2 * (pass + 1));
            assert_eq!(read_records(&config.output), report.records);
            for (record, candidate) in report.records.iter().zip([REFERENCE, DIVERGENT]) {
                assert_eq!(
                    absolute_file(record.reference_log.as_deref()),
                    REFERENCE.as_bytes()
                );
                assert_eq!(
                    absolute_file(record.candidate_log.as_deref()),
                    candidate.as_bytes()
                );
            }
            let (_, sidecar) = golden_paths(&config.output_dir, "fx/one").unwrap();
            let sidecar: ParityGoldenSidecar =
                serde_json::from_slice(&fs::read(sidecar).unwrap()).unwrap();
            assert_eq!(
                absolute_file(Some(&sidecar.source_log)),
                REFERENCE.as_bytes()
            );
        }
        drop(fixture);
        let _ = fs::remove_dir_all(&top);
    }

    /// A program name with no `/` stays as spelled, so `PATH` still finds
    /// it; any name containing a `/` is a path, as exec treats it, and is
    /// resolved against this process's working directory.
    #[test]
    fn only_a_program_name_without_a_slash_is_left_for_path() {
        let cwd = std::env::current_dir().unwrap();
        for bare in ["hermit", "hermit-2.0"] {
            assert_eq!(
                program_independent_of_cwd(Path::new(bare)).unwrap(),
                Path::new(bare)
            );
        }
        for (spelled, resolved) in [
            ("./hermit", cwd.join("hermit")),
            ("bin/hermit", cwd.join("bin/hermit")),
            ("hermit/", cwd.join("hermit/")),
            ("/opt/hermit", PathBuf::from("/opt/hermit")),
        ] {
            let program = program_independent_of_cwd(Path::new(spelled)).unwrap();
            assert!(program.is_absolute(), "{spelled}: {}", program.display());
            assert_eq!(
                path_text(&program),
                path_text(&resolved),
                "{spelled} keeps its meaning"
            );
        }
    }

    fn tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut files = BTreeMap::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(dir) = pending.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    files.insert(
                        path.strip_prefix(root).unwrap().to_path_buf(),
                        fs::read(&path).unwrap(),
                    );
                }
            }
        }
        files
    }

    /// A post-pass writing below another directory leaves the harness's own
    /// parity outputs byte-identical, and still reuses the harness's golden
    /// once the reference's retained log is gone.
    #[test]
    fn a_post_pass_writing_elsewhere_leaves_the_runs_parity_outputs_alone() {
        let fixture = Fixture::new("elsewhere");
        let rows = vec![
            fixture.row("fx/one", "ptrace", 1, "PASS", Some(REFERENCE)),
            fixture.row("fx/one", "kvm", 1, "PASS", Some(REFERENCE)),
            fixture.row("fx/one", "liteinst", 1, "PASS", Some(DIVERGENT)),
        ];
        let scope = BTreeSet::from([parity_cell("fx/one", ParityBackend::Kvm)]);
        post_pass(&fixture.config(), &scope, &rows).unwrap();
        let own = fixture.artifacts().join("parity");
        let jsonl = fixture.artifacts().join(PARITY_JSONL);
        let status_json = fixture.artifacts().join(PARITY_STATUS_JSON);
        let before = (
            tree(&own),
            fs::read(&jsonl).unwrap(),
            fs::read(&status_json).unwrap(),
        );

        let source =
            PathBuf::from(&rows[0].argv[rows[0].argv.len() - 4]).join("run1_log_fixture.log");
        fs::remove_file(&source).unwrap();
        let elsewhere = fixture.artifacts().join("parity-compare");
        let config = fixture.config().writing_below(&elsewhere);
        assert_eq!(config.output, elsewhere.join(PARITY_JSONL));
        let wider = BTreeSet::from([
            parity_cell("fx/one", ParityBackend::Kvm),
            parity_cell("fx/one", ParityBackend::Liteinst),
        ]);
        let report = post_pass(&config, &wider, &rows).unwrap();
        assert_eq!(
            report
                .records
                .iter()
                .map(|record| record.verdict)
                .collect::<Vec<_>>(),
            [ParityVerdict::Matched, ParityVerdict::Diverged]
        );
        let (harness_golden, _) = golden_paths(&fixture.artifacts(), "fx/one").unwrap();
        assert_eq!(
            report.records[0].reference_log.as_deref(),
            Some(path_text(&harness_golden).as_str()),
            "the harness's golden stands in for the deleted source log"
        );
        assert!(elsewhere.join(PARITY_STATUS_JSON).exists());
        assert!(
            elsewhere
                .join(PARITY_LOGDIFF_DIR)
                .join("fx/one@liteinst.json")
                .exists()
        );
        assert_eq!(
            (
                tree(&own),
                fs::read(&jsonl).unwrap(),
                fs::read(&status_json).unwrap()
            ),
            before,
            "the run's own parity outputs changed"
        );
    }

    #[test]
    fn the_post_pass_refuses_a_configuration_no_record_could_carry() {
        let fixture = Fixture::new("config");
        let scope = BTreeSet::from([parity_cell("fx/one", ParityBackend::Kvm)]);
        let mut config = fixture.config();
        config.hermit_sha = "not-a-sha".into();
        let error = post_pass(&config, &scope, &[]).unwrap_err();
        assert!(error.contains("parity post-pass configuration"), "{error}");
        assert!(
            !config.output.exists(),
            "no records are written for a refused configuration"
        );
        let failed = status(&config);
        assert_eq!(failed.state, PostPassState::Failed);
        assert_eq!(failed.error.as_deref(), Some(error.as_str()));
    }

    // ---- ledger sources ----------------------------------------------------

    const LEDGER_RUN: &str = "run-ledger";

    fn ledger_diverged(test: &str, backend: ParityBackend, prefix: usize) -> ParityRecord {
        let record = ParityRecord {
            schema: PARITY_RECORD_SCHEMA,
            test_id: test.into(),
            backend,
            verdict: ParityVerdict::Diverged,
            unavailable_class: None,
            operand: None,
            inputs_equalized: false,
            reason: None,
            credit: None,
            unequalized_credit: credit(prefix, 10, 10),
            first_divergent_record: Some(prefix + 1),
            left_len: Some(10),
            right_len: Some(10),
            matched_prefix: Some(prefix),
            first_difference: Some(ParityFirstDifference {
                field: Some("token 2: `a` vs `b`".into()),
                syscall: Some(12),
                scheduler_turn: None,
                virtual_nanoseconds: None,
                reference_message: Some("DETLOG brk a".into()),
                candidate_message: Some("DETLOG brk b".into()),
            }),
            reference_log: Some("ref.log".into()),
            candidate_log: Some("cand.log".into()),
            run_id: LEDGER_RUN.into(),
            hermit_sha: SHA.into(),
        };
        record.validate().unwrap();
        record
    }

    fn ledger_unmeasured(
        test: &str,
        backend: ParityBackend,
        verdict: ParityVerdict,
        class: UnavailableClass,
        operand: Option<ParityOperand>,
        reason: &str,
    ) -> ParityRecord {
        ParityRecord::unmeasured(
            &parity_cell(test, backend),
            verdict,
            class,
            operand,
            false,
            reason,
            None,
            None,
            LEDGER_RUN,
            SHA,
        )
        .unwrap()
    }

    fn ledger_status(
        state: PostPassState,
        scope: Option<&[ParityCellId]>,
        cells: usize,
        error: Option<&str>,
    ) -> PostPassStatus {
        PostPassStatus {
            schema: if scope.is_some() {
                PARITY_STATUS_SCHEMA
            } else {
                PARITY_STATUS_SCHEMA_COUNT_ONLY
            },
            state,
            run_id: LEDGER_RUN.into(),
            hermit_sha: SHA.into(),
            hermit_bin: "/src/hermit".into(),
            hermit_bin_sha256: Some("ab".repeat(32)),
            records: "/results/parity.jsonl".into(),
            cells,
            scope: scope.map(|scope| scope.iter().map(ToString::to_string).collect()),
            summary: None,
            error: error.map(str::to_string),
        }
    }

    fn write_node(dir: &Path, status: Option<&PostPassStatus>, records: Option<&[ParityRecord]>) {
        fs::create_dir_all(dir).unwrap();
        if let Some(status) = status {
            fs::write(
                dir.join(PARITY_STATUS_JSON),
                serde_json::to_vec_pretty(status).unwrap(),
            )
            .unwrap();
        }
        if let Some(records) = records {
            let text = records
                .iter()
                .map(|record| serde_json::to_string(record).unwrap() + "\n")
                .collect::<String>();
            fs::write(dir.join(PARITY_JSONL), text).unwrap();
        }
    }

    /// A scratch e2e result root, removed when the test that made it ends,
    /// unless that test is panicking: a failing test's directory is kept to
    /// debug.
    struct ResultRoot(PathBuf);

    impl std::ops::Deref for ResultRoot {
        type Target = Path;

        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ResultRoot {
        fn drop(&mut self) {
            if !std::thread::panicking() {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
    }

    fn result_root(label: &str) -> ResultRoot {
        let dir = std::env::temp_dir().join(format!(
            "hermit-parity-results-{}-{label}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        ResultRoot(dir)
    }

    /// The scratch result roots of the ledger-row tests do not outlive them.
    #[test]
    fn a_result_root_is_removed_when_its_test_ends() {
        let root = result_root("removed");
        let path = root.to_path_buf();
        write_node(
            &root.join("portable/manifest_c_programs"),
            Some(&ledger_status(PostPassState::Complete, Some(&[]), 0, None)),
            Some(&[]),
        );
        assert!(path.join("portable/manifest_c_programs").is_dir());
        drop(root);
        assert!(!path.exists(), "{} outlived its test", path.display());
    }

    fn five_cells() -> Vec<ParityCellId> {
        vec![
            parity_cell("c-programs/a", ParityBackend::Kvm),
            parity_cell("c-programs/a", ParityBackend::Liteinst),
            parity_cell("c-programs/b", ParityBackend::Kvm),
            parity_cell("c-programs/b", ParityBackend::Sabre),
            parity_cell("c-programs/c", ParityBackend::Kvm),
        ]
    }

    /// A post-pass that failed after writing two of its five records owes
    /// the other three `record-missing` rows, each naming the failure, and
    /// keeps the two it wrote. None vanishes from the denominator.
    #[test]
    fn a_failed_status_owes_record_missing_rows_for_its_unrecorded_scope() {
        let root = result_root("failed");
        let scope = five_cells();
        let written = [
            ledger_diverged("c-programs/a", ParityBackend::Kvm, 3),
            ledger_unmeasured(
                "c-programs/b",
                ParityBackend::Sabre,
                ParityVerdict::Nondeterministic,
                UnavailableClass::DeterminismMismatch,
                Some(ParityOperand::Candidate),
                "the candidate verify cell of c-programs/b failed determinism: run 2 differed",
            ),
        ];
        let node = root.join("portable/manifest_c_programs");
        let failed = ledger_status(PostPassState::Failed, Some(&scope), 5, Some("disk full"));
        write_node(&node, Some(&failed), Some(&written));
        let rows = ledger_sources(&root, None).unwrap();
        assert_eq!(rows.len(), 5, "{rows:#?}");
        let missing = rows
            .iter()
            .filter(|row| row.verdict == LedgerVerdict::RecordMissing)
            .collect::<Vec<_>>();
        assert_eq!(missing.len(), 3);
        for row in &missing {
            assert_eq!(
                row.reason.as_deref(),
                Some("parity post-pass failed: disk full")
            );
            assert_eq!(row.record, None);
            assert_eq!(row.run_id.as_deref(), Some(LEDGER_RUN));
            assert_eq!(row.source.post_pass_state, LedgerPostPassState::Failed);
            assert_eq!(row.source.scope_source, LedgerScopeSource::StatusScope);
            assert_eq!(row.source.lane, "portable");
            assert_eq!(row.source.node, "manifest_c_programs");
        }
        assert_eq!(
            missing
                .iter()
                .map(|row| row.cell.as_str())
                .collect::<Vec<_>>(),
            [
                "c-programs/a@liteinst",
                "c-programs/b@kvm",
                "c-programs/c@kvm"
            ]
        );
        let kept = rows
            .iter()
            .filter_map(|row| row.record.clone())
            .collect::<Vec<_>>();
        assert_eq!(kept, written);
        // Each kept record is the line as written.
        let line = fs::read_to_string(node.join(PARITY_JSONL)).unwrap();
        let first = rows.iter().find(|row| row.record.is_some()).unwrap();
        assert_eq!(
            serde_json::to_string(first.record.as_ref().unwrap()).unwrap(),
            line.lines().next().unwrap()
        );
        assert_eq!(
            first.source.records_sha256.as_deref(),
            Some(sha256_hex(line.as_bytes()).as_str())
        );
        // A running status (the process was killed) is reported the same way.
        let running = ledger_status(PostPassState::Running, Some(&scope), 5, None);
        write_node(&node, Some(&running), None);
        fs::remove_file(node.join(PARITY_JSONL)).unwrap();
        let rows = ledger_sources(&root, None).unwrap();
        assert_eq!(rows.len(), 5);
        assert!(rows.iter().all(|row| {
            row.verdict == LedgerVerdict::RecordMissing
                && row.reason.as_deref()
                    == Some(
                        "parity post-pass running: no error recorded; the process ended \
                         before the post-pass finished",
                    )
        }));
    }

    /// A complete status must account for exactly its cells.
    #[test]
    fn a_complete_status_with_a_count_mismatch_is_refused() {
        let root = result_root("count");
        let scope = five_cells();
        let node = root.join("portable/manifest_c_programs");
        let records = [ledger_diverged("c-programs/a", ParityBackend::Kvm, 3)];
        write_node(
            &node,
            Some(&ledger_status(
                PostPassState::Complete,
                Some(&scope),
                5,
                None,
            )),
            Some(&records),
        );
        let error = ledger_sources(&root, None).unwrap_err();
        assert!(
            error.contains("is complete with 5 cell(s) in scope, but")
                && error.contains("holds 1 record(s)"),
            "{error}"
        );
        // The same mismatch in a schema-1 status, checked by its count.
        write_node(
            &node,
            Some(&ledger_status(PostPassState::Complete, None, 2, None)),
            Some(&records),
        );
        let error = ledger_sources(&root, None).unwrap_err();
        assert!(
            error.contains("is complete with 2 cell(s) in scope"),
            "{error}"
        );
        // A scope whose length disagrees with its count.
        let mut status = ledger_status(PostPassState::Complete, Some(&scope), 5, None);
        status.cells = 4;
        write_node(&node, Some(&status), Some(&records));
        let error = ledger_sources(&root, None).unwrap_err();
        assert!(
            error.contains("names 5 scope cell(s) but counts 4"),
            "{error}"
        );
        // A complete status with no records file at all.
        fs::remove_file(node.join(PARITY_JSONL)).unwrap();
        write_node(
            &node,
            Some(&ledger_status(
                PostPassState::Complete,
                Some(&scope[..1]),
                1,
                None,
            )),
            None,
        );
        let error = ledger_sources(&root, None).unwrap_err();
        assert!(error.contains("is complete but"), "{error}");
        // Records with no status cannot be attributed.
        fs::remove_file(node.join(PARITY_STATUS_JSON)).unwrap();
        write_node(&node, None, Some(&records));
        let error = ledger_sources(&root, None).unwrap_err();
        assert!(
            error.contains("has no parity.status.json beside it"),
            "{error}"
        );
    }

    /// A schema-1 status names only a count. Complete, it is accepted by that
    /// count; unfinished, it needs the caller's expected scope to say which
    /// cells are missing, and is refused without one.
    #[test]
    fn a_schema_one_status_is_accepted_by_its_count() {
        let root = result_root("schema-one");
        let node = root.join("privileged/manifest_c_programs");
        let records = [
            ledger_diverged("c-programs/cpuid", ParityBackend::Kvm, 2),
            ledger_diverged("c-programs/cpuid", ParityBackend::Liteinst, 0),
        ];
        write_node(
            &node,
            Some(&ledger_status(PostPassState::Complete, None, 2, None)),
            Some(&records),
        );
        let rows = ledger_sources(&root, None).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| {
            row.source.scope_source == LedgerScopeSource::StatusCount
                && row.source.post_pass_state == LedgerPostPassState::Complete
                && row.verdict == LedgerVerdict::Diverged
        }));
        assert_eq!(
            rows[1].record.as_ref().unwrap().unequalized_credit,
            Some(0.0)
        );

        write_node(
            &node,
            Some(&ledger_status(PostPassState::Failed, None, 3, Some("boom"))),
            None,
        );
        fs::remove_file(node.join(PARITY_JSONL)).unwrap();
        let error = ledger_sources(&root, None).unwrap_err();
        assert!(
            error.contains("status in state Failed counts 3 cell(s) but does not name them"),
            "{error}"
        );
        let expected = ExpectedScope::from([(
            "privileged/manifest_c_programs".to_string(),
            BTreeSet::from([
                parity_cell("c-programs/cpuid", ParityBackend::Kvm),
                parity_cell("c-programs/cpuid", ParityBackend::Liteinst),
            ]),
        )]);
        let rows = ledger_sources(&root, Some(&expected)).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| {
            row.verdict == LedgerVerdict::RecordMissing
                && row.source.scope_source == LedgerScopeSource::ExpectedScope
                && row.reason.as_deref() == Some("parity post-pass failed: boom")
        }));
    }

    /// A planned node that left no status at all owes each expected cell a
    /// `record-missing` row with `post_pass_state` `absent`.
    #[test]
    fn an_absent_status_owes_its_expected_cells() {
        let root = result_root("absent");
        fs::create_dir_all(root.join("portable/manifest_system_utils")).unwrap();
        let expected = parse_expected_scope(
            r#"{"portable/manifest_c_programs": ["c-programs/a@kvm", "c-programs/a@dbt"]}"#,
        )
        .unwrap();
        let rows = ledger_sources(&root, Some(&expected)).unwrap();
        assert_eq!(rows.len(), 2, "{rows:#?}");
        for row in &rows {
            assert_eq!(row.verdict, LedgerVerdict::RecordMissing);
            assert_eq!(row.source.post_pass_state, LedgerPostPassState::Absent);
            assert_eq!(row.source.scope_source, LedgerScopeSource::ExpectedScope);
            assert_eq!(row.source.status_sha256, None);
            assert_eq!(row.run_id, None);
            assert_eq!(
                row.reason.as_deref(),
                Some("no parity row: node portable/manifest_c_programs left no parity.status.json")
            );
        }
        // Without an expected scope an absent node owes nothing.
        assert!(ledger_sources(&root, None).unwrap().is_empty());
        for bad in [
            r#"{"portable": ["c-programs/a@kvm"]}"#,
            r#"{"portable/x": ["c-programs/a@ptrace"]}"#,
            r#"{"portable/x": ["c-programs/a@kvm", "c-programs/a@kvm"]}"#,
        ] {
            assert!(parse_expected_scope(bad).is_err(), "{bad}");
        }
    }

    /// A cell the node's expected scope owes but its status's scope leaves
    /// out is a `record-missing` row filed under the expected scope, whatever
    /// the status's state; a narrower post-pass scope cannot drop it.
    #[test]
    fn a_cell_the_expected_scope_owes_but_the_status_omits_is_record_missing() {
        let root = result_root("gap");
        let node = root.join("portable/manifest_c_programs");
        let scope = five_cells();
        let expected = scope.iter().cloned().collect::<BTreeSet<_>>();
        let gap_reason = "no parity row: the post-pass of node portable/manifest_c_programs \
                          left this cell out of its scope, and the node's expected scope owes it";
        let written = [ledger_diverged("c-programs/a", ParityBackend::Kvm, 3)];
        // Complete, with a schema-2 scope of one cell: four gaps.
        write_node(
            &node,
            Some(&ledger_status(
                PostPassState::Complete,
                Some(&scope[..1]),
                1,
                None,
            )),
            Some(&written),
        );
        let rows =
            node_ledger_sources(&node, "portable", "manifest_c_programs", Some(&expected)).unwrap();
        assert_eq!(rows.len(), 5, "{rows:#?}");
        assert_eq!(rows[0].record.as_ref(), Some(&written[0]));
        assert_eq!(rows[0].source.scope_source, LedgerScopeSource::StatusScope);
        for row in &rows[1..] {
            assert_eq!(row.verdict, LedgerVerdict::RecordMissing);
            assert_eq!(row.reason.as_deref(), Some(gap_reason));
            assert_eq!(row.source.scope_source, LedgerScopeSource::ExpectedScope);
            assert_eq!(row.source.post_pass_state, LedgerPostPassState::Complete);
            assert_eq!(row.run_id.as_deref(), Some(LEDGER_RUN));
        }
        assert_eq!(
            rows[1..]
                .iter()
                .map(|row| row.cell.as_str())
                .collect::<Vec<_>>(),
            [
                "c-programs/a@liteinst",
                "c-programs/b@kvm",
                "c-programs/b@sabre",
                "c-programs/c@kvm"
            ]
        );
        // Complete schema 1, checked by its count: the cells no record names
        // are the gaps.
        write_node(
            &node,
            Some(&ledger_status(PostPassState::Complete, None, 1, None)),
            Some(&written),
        );
        let rows =
            node_ledger_sources(&node, "portable", "manifest_c_programs", Some(&expected)).unwrap();
        assert_eq!(rows.len(), 5, "{rows:#?}");
        assert_eq!(rows[0].source.scope_source, LedgerScopeSource::StatusCount);
        assert!(rows[1..].iter().all(|row| {
            row.verdict == LedgerVerdict::RecordMissing
                && row.reason.as_deref() == Some(gap_reason)
                && row.source.scope_source == LedgerScopeSource::ExpectedScope
        }));
        // Failed with a scope of two cells: one owed by the status's scope,
        // three by the expected scope alone.
        write_node(
            &node,
            Some(&ledger_status(
                PostPassState::Failed,
                Some(&scope[..2]),
                2,
                Some("disk full"),
            )),
            Some(&written),
        );
        let rows =
            node_ledger_sources(&node, "portable", "manifest_c_programs", Some(&expected)).unwrap();
        assert_eq!(rows.len(), 5, "{rows:#?}");
        let reasons = rows
            .iter()
            .map(|row| {
                (
                    row.cell.as_str(),
                    row.source.scope_source,
                    row.reason.as_deref(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            reasons,
            [
                ("c-programs/a@kvm", LedgerScopeSource::StatusScope, None),
                (
                    "c-programs/a@liteinst",
                    LedgerScopeSource::StatusScope,
                    Some("parity post-pass failed: disk full")
                ),
                (
                    "c-programs/b@kvm",
                    LedgerScopeSource::ExpectedScope,
                    Some(gap_reason)
                ),
                (
                    "c-programs/b@sabre",
                    LedgerScopeSource::ExpectedScope,
                    Some(gap_reason)
                ),
                (
                    "c-programs/c@kvm",
                    LedgerScopeSource::ExpectedScope,
                    Some(gap_reason)
                ),
            ]
        );
        // With no expected scope, the status's own scope is all it owes.
        let rows = node_ledger_sources(&node, "portable", "manifest_c_programs", None).unwrap();
        assert_eq!(rows.len(), 2);
    }

    /// When [`ledger_sources`] refuses a run, validate falls back to
    /// [`expected_ledger_sources`]: the nodes it can read keep their rows, and
    /// a refused node owes each expected cell a `record-missing` row naming
    /// the refusal and the refused bytes.
    #[test]
    fn a_refused_node_owes_record_missing_rows_for_its_expected_cells() {
        let root = result_root("refused");
        // A records file with no status beside it: refused.
        let refused_node = root.join("portable/manifest_c_programs");
        write_node(
            &refused_node,
            None,
            Some(&[ledger_diverged("c-programs/a", ParityBackend::Kvm, 3)]),
        );
        let stray = fs::read(refused_node.join(PARITY_JSONL)).unwrap();
        // A complete node the plan expected.
        let cpuid = [parity_cell("c-programs/cpuid", ParityBackend::Kvm)];
        write_node(
            &root.join("privileged/manifest_c_programs"),
            Some(&ledger_status(
                PostPassState::Complete,
                Some(&cpuid),
                1,
                None,
            )),
            Some(&[ledger_diverged("c-programs/cpuid", ParityBackend::Kvm, 1)]),
        );
        // A complete node on disk that the plan did not name.
        let ls = [parity_cell("system-utils/ls", ParityBackend::Sabre)];
        write_node(
            &root.join("portable/manifest_system_utils"),
            Some(&ledger_status(PostPassState::Complete, Some(&ls), 1, None)),
            Some(&[ledger_diverged("system-utils/ls", ParityBackend::Sabre, 2)]),
        );
        let expected = parse_expected_scope(
            r#"{"portable/manifest_c_programs": ["c-programs/a@kvm", "c-programs/a@liteinst"],
                "privileged/manifest_c_programs": ["c-programs/cpuid@kvm"]}"#,
        )
        .unwrap();
        let error = ledger_sources(&root, Some(&expected)).unwrap_err();
        assert!(
            error.contains("has no parity.status.json beside it"),
            "{error}"
        );
        let (rows, refused) = expected_ledger_sources(&root, &expected).unwrap();
        assert_eq!(refused.len(), 1, "{refused:?}");
        assert!(
            refused[0].starts_with("portable/manifest_c_programs: ")
                && refused[0].contains("has no parity.status.json beside it"),
            "{refused:?}"
        );
        let summary = rows
            .iter()
            .map(|row| (row.cell.as_str(), row.verdict, row.source.post_pass_state))
            .collect::<Vec<_>>();
        assert_eq!(
            summary,
            [
                (
                    "c-programs/a@kvm",
                    LedgerVerdict::RecordMissing,
                    LedgerPostPassState::Refused
                ),
                (
                    "c-programs/a@liteinst",
                    LedgerVerdict::RecordMissing,
                    LedgerPostPassState::Refused
                ),
                (
                    "system-utils/ls@sabre",
                    LedgerVerdict::Diverged,
                    LedgerPostPassState::Complete
                ),
                (
                    "c-programs/cpuid@kvm",
                    LedgerVerdict::Diverged,
                    LedgerPostPassState::Complete
                ),
            ]
        );
        for row in &rows[..2] {
            assert_eq!(
                row.reason.as_deref(),
                Some(format!("parity post-pass outputs refused: {error}").as_str())
            );
            assert_eq!(row.source.status_sha256, None);
            assert_eq!(
                row.source.records_sha256.as_deref(),
                Some(sha256_hex(&stray).as_str())
            );
            assert_eq!(row.source.scope_source, LedgerScopeSource::ExpectedScope);
            assert_eq!(row.run_id, None);
            row.validate().unwrap();
        }
        // A refused node can only owe record-missing rows.
        let mut row = rows[2].clone();
        row.source.post_pass_state = LedgerPostPassState::Refused;
        let error = row.validate().unwrap_err();
        assert!(
            error.contains("a node whose outputs were refused can only owe record-missing rows"),
            "{error}"
        );
        // An e2e root that cannot be listed still owes every expected cell.
        let (rows, refused) =
            expected_ledger_sources(&root.join("no-such-root"), &expected).unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|row| {
            row.verdict == LedgerVerdict::RecordMissing
                && row.source.post_pass_state == LedgerPostPassState::Absent
        }));
        assert_eq!(refused.len(), 1);
        assert!(refused[0].starts_with("cannot read "), "{refused:?}");
    }

    /// `emitted_at` is read as the instant it names, and anything that is
    /// not an RFC 3339 UTC time is refused.
    #[test]
    fn emitted_at_is_read_as_an_rfc3339_utc_instant() {
        let at = |text: &str| parse_utc_timestamp(text).unwrap();
        assert_eq!(
            at("1970-01-01T00:00:00Z"),
            UtcInstant {
                days: 0,
                second: 0,
                nanosecond: 0
            }
        );
        // 2000-01-01 is day 10957 (30 years, 7 of them leap); March 1 follows
        // 31 days of January and 29 of February.
        assert_eq!(at("2000-03-01T00:00:00Z").days, 10957 + 31 + 29);
        assert_eq!(
            at("2026-09-29T04:10:00.25Z"),
            UtcInstant {
                days: 20725,
                second: 4 * 3600 + 10 * 60,
                nanosecond: 250_000_000
            }
        );
        // As text, a fraction sorts before the bare second it follows.
        assert!("2026-09-29T04:10:00.5Z" < "2026-09-29T04:10:00Z");
        assert!(at("2026-09-29T04:10:00.5Z") > at("2026-09-29T04:10:00Z"));
        assert!(at("2026-09-29T04:10:00.000000001Z") > at("2026-09-29T04:10:00Z"));
        assert_eq!(at("2026-09-29T04:10:00+00:00"), at("2026-09-29T04:10:00Z"));
        assert_eq!(at("2026-09-29T04:10:00.5Z"), at("2026-09-29T04:10:00.500Z"));
        assert!(at("2026-09-30T00:00:00Z") > at("2026-09-29T23:59:59.999999999Z"));
        assert!(at("2024-02-29T12:00:00Z") > at("2024-02-28T12:00:00Z"));
        assert!(at("2000-02-29T00:00:00Z") < at("2000-03-01T00:00:00Z"));
        // A leap second falls between 23:59:59 and the next midnight.
        let leap = at("2026-12-31T23:59:60Z");
        assert!(leap > at("2026-12-31T23:59:59.999Z"));
        assert!(leap < at("2027-01-01T00:00:00Z"));
        for bad in [
            "",
            "zzzz not a time",
            "2026-09-29 04:10:00Z",
            "2026-09-29T04:10:00",
            "2026-09-29T04:10:00+01:00",
            "2026-09-29T04:10:00-00:00",
            "2026-09-29T04:10:00z",
            "2026-09-29t04:10:00Z",
            "2026-09-29T04:10:00Zjunk",
            "2026-09-29T04:10Z",
            "2026-9-29T04:10:00Z",
            "2026-13-01T00:00:00Z",
            "2026-00-01T00:00:00Z",
            "2026-09-00T00:00:00Z",
            "2026-09-31T00:00:00Z",
            "2026-02-29T00:00:00Z",
            "1900-02-29T00:00:00Z",
            "2026-09-29T24:00:00Z",
            "2026-09-29T23:60:00Z",
            "2026-09-29T23:58:60Z",
            "2026-09-29T23:59:61Z",
            "2026-09-29T04:10:00.Z",
            "2026-09-29T04:10:00.1234567890Z",
            "+2026-09-29T04:10:00Z",
        ] {
            assert!(parse_utc_timestamp(bad).is_err(), "{bad:?} was accepted");
        }
    }

    /// A row whose `emitted_at` is not an RFC 3339 UTC time is refused: the
    /// reader orders runs by that instant, so it cannot be guessed.
    #[test]
    fn a_ledger_row_with_a_malformed_emitted_at_is_refused() {
        let root = result_root("emitted-at");
        let node = root.join("portable/manifest_c_programs");
        let scope = [parity_cell("c-programs/a", ParityBackend::Kvm)];
        write_node(
            &node,
            Some(&ledger_status(
                PostPassState::Complete,
                Some(&scope),
                1,
                None,
            )),
            Some(&[ledger_diverged("c-programs/a", ParityBackend::Kvm, 3)]),
        );
        let sources = ledger_sources(&root, None).unwrap();
        let row = envelope(&sources[0], ParityProducer::Validate);
        row.validate().unwrap();
        for bad in [
            "zzzz not a time",
            "2026-09-29 04:00:00Z",
            "2026-09-29T04:00:00",
        ] {
            let mut row = row.clone();
            row.emitted_at = bad.into();
            let error = row.validate().unwrap_err();
            assert!(
                error.contains("emitted_at is not an RFC 3339 UTC time"),
                "{bad}: {error}"
            );
        }
    }

    // ---- appending ledger rows ---------------------------------------------

    /// A scratch `series.py` whose `append-parity` behaves as `body` says.
    /// `body` runs after `import os, subprocess, sys, time, json` with `help`
    /// true for the `--help` probe.
    fn fake_series(root: &Path, body: &str) -> PathBuf {
        let series = root.join("series.py");
        fs::write(
            &series,
            format!(
                "import json, os, subprocess, sys, time\n\
                 help = sys.argv[1:] == ['append-parity', '--help']\n\
                 {body}\n"
            ),
        )
        .unwrap();
        series
    }

    fn append_rows(label: &str) -> Vec<ParityLedgerSource> {
        let root = result_root(&format!("{label}-rows"));
        let node = root.join("portable/manifest_c_programs");
        let scope = [
            parity_cell("c-programs/a", ParityBackend::Kvm),
            parity_cell("c-programs/b", ParityBackend::Kvm),
        ];
        write_node(
            &node,
            Some(&ledger_status(
                PostPassState::Failed,
                Some(&scope),
                2,
                Some("x"),
            )),
            Some(&[ledger_diverged("c-programs/a", ParityBackend::Kvm, 3)]),
        );
        ledger_sources(&root, None).unwrap()
    }

    fn append_to(series: &Path, source: &Path) -> LedgerAppend<'static> {
        LedgerAppend {
            series: Box::leak(series.to_path_buf().into_boxed_path()),
            parent: Path::new("/parent"),
            producer: ParityProducer::Validate,
            run_id: LEDGER_RUN,
            tree: SHA,
            source_tree_dirty: false,
            source: Box::leak(source.to_path_buf().into_boxed_path()),
        }
    }

    /// The rows reach `append-parity` on its stdin with the run's identity
    /// and its tree state on its command line, and the writer's own words
    /// end the line, which claims only the writer's acceptance: exit status
    /// 0 also means the writer had already recorded the rows and wrote
    /// nothing (review A Low 1 of
    /// <https://github.com/rrnewton/hermit/issues/3301#issuecomment-5911273090>).
    #[test]
    fn appended_rows_reach_the_writer_with_the_runs_identity() {
        let root = result_root("append-ok");
        let rows = append_rows("append-ok");
        let capture = root.join("captured.json");
        let series = fake_series(
            &root,
            &format!(
                "if help:\n    sys.exit(0)\n\
                 json.dump({{'argv': sys.argv[1:], 'stdin': sys.stdin.read()}}, \
                 open({capture:?}, 'w'))\n\
                 print('wrote 2 rows')"
            ),
        );
        let line = append_ledger_rows(&append_to(&series, &root), &rows, AppendBounds::default());
        assert_eq!(
            line,
            format!(
                "parity: append-parity accepted 2 row(s) from {} (diverged 1, record-missing 1): \
                 wrote 2 rows",
                root.display()
            )
        );
        let captured: serde_json::Value =
            serde_json::from_slice(&fs::read(&capture).unwrap()).unwrap();
        assert_eq!(
            captured["argv"],
            serde_json::json!([
                "append-parity",
                "--parent",
                "/parent",
                "--producer",
                "validate",
                "--run-id",
                LEDGER_RUN,
                "--tree",
                SHA,
                "--source-tree-dirty",
                "false"
            ])
        );
        let expected = rows
            .iter()
            .map(|row| serde_json::to_string(row).unwrap() + "\n")
            .collect::<String>();
        assert_eq!(captured["stdin"], serde_json::json!(expected));

        // A dirty tree is sent as such.
        let line = append_ledger_rows(
            &LedgerAppend {
                source_tree_dirty: true,
                ..append_to(&series, &root)
            },
            &rows,
            AppendBounds::default(),
        );
        assert!(line.ends_with(": wrote 2 rows"), "{line}");
        let captured: serde_json::Value =
            serde_json::from_slice(&fs::read(&capture).unwrap()).unwrap();
        let argv = captured["argv"].as_array().unwrap();
        assert_eq!(
            argv[argv.len() - 2..],
            [
                serde_json::json!("--source-tree-dirty"),
                serde_json::json!("true")
            ]
        );

        // A writer that had already recorded the rows exits 0 as well; the
        // line reports its acceptance in its own words, never an append.
        let recorded = fake_series(
            &root,
            "if help:\n    sys.exit(0)\nsys.stdin.read()\n\
             print('2 parity row(s) already recorded (batch fixture); nothing written')",
        );
        assert_eq!(
            append_ledger_rows(&append_to(&recorded, &root), &rows, AppendBounds::default()),
            format!(
                "parity: append-parity accepted 2 row(s) from {} (diverged 1, record-missing 1): \
                 2 parity row(s) already recorded (batch fixture); nothing written",
                root.display()
            )
        );
        let silent = fake_series(&root, "if help:\n    sys.exit(0)\nsys.stdin.read()");
        assert_eq!(
            append_ledger_rows(&append_to(&silent, &root), &rows, AppendBounds::default()),
            format!(
                "parity: append-parity accepted 2 row(s) from {} (diverged 1, record-missing 1): \
                 it printed nothing",
                root.display()
            )
        );
    }

    /// A writer without `append-parity`, a missing writer, a probe that
    /// fails and a refusing writer are each named, and the rows are left
    /// where they are. Only the probe's exit status 2, dev-hermit's usage
    /// exit for an unknown subcommand, reads as a writer without
    /// `append-parity`; a missing writer and a probe that fails any other
    /// way are `ERROR:` lines, the probe's with its stderr (review A Low 2
    /// of <https://github.com/rrnewton/hermit/issues/3301#issuecomment-5911273090>).
    #[test]
    fn a_writer_that_cannot_append_is_named_and_leaves_the_rows() {
        let root = result_root("append-refused");
        let rows = append_rows("append-refused");
        let left = format!(
            "; 2 rows left in {} (diverged 1, record-missing 1)",
            root.display()
        );
        // dev-hermit's series.py answers an unknown subcommand with its usage
        // and exit status 2.
        let legacy = fake_series(
            &root,
            "sys.stderr.write('usage: series.py {append-cells}\\n')\nsys.exit(2)",
        );
        assert_eq!(
            append_ledger_rows(&append_to(&legacy, &root), &rows, AppendBounds::default()),
            format!(
                "parity: the series writer {} has no append-parity (`append-parity --help` \
                 exit status: 2){left}",
                legacy.display()
            )
        );
        let missing = root.join("absent/series.py");
        assert_eq!(
            append_ledger_rows(&append_to(&missing, &root), &rows, AppendBounds::default()),
            format!("parity: ERROR: {} does not exist{left}", missing.display())
        );
        // A probe that dies with a traceback is not a writer without the
        // subcommand, and the traceback is in the line.
        let broken = root.join("broken");
        fs::create_dir_all(&broken).unwrap();
        let broken = fake_series(
            &broken,
            "if help:\n    raise RuntimeError('fixture probe failure')",
        );
        let line = append_ledger_rows(&append_to(&broken, &root), &rows, AppendBounds::default());
        assert!(
            line.starts_with(&format!(
                "parity: ERROR: `{} append-parity --help` failed (exit status: 1): Traceback",
                broken.display()
            )) && line.contains("RuntimeError: fixture probe failure")
                && line.ends_with(&left),
            "{line}"
        );
        // So is a probe ended by a signal.
        let killed = root.join("killed");
        fs::create_dir_all(&killed).unwrap();
        let killed = fake_series(
            &killed,
            "if help:\n    os.kill(os.getpid(), 9)\nsys.exit(0)",
        );
        let line = append_ledger_rows(&append_to(&killed, &root), &rows, AppendBounds::default());
        assert!(
            line.starts_with(&format!(
                "parity: ERROR: `{} append-parity --help` failed (signal: 9",
                killed.display()
            )) && line.ends_with(&left),
            "{line}"
        );
        let refusing = fake_series(
            &root,
            "if help:\n    sys.exit(0)\nsys.stdin.read()\nsys.stderr.write('fixture refusal\\n')\nsys.exit(1)",
        );
        assert_eq!(
            append_ledger_rows(&append_to(&refusing, &root), &rows, AppendBounds::default()),
            format!(
                "parity: ERROR: append-parity refused them (exit status: 1): fixture refusal{left}"
            )
        );
    }

    /// A writer that hangs, in the probe or in the append, is killed at its
    /// bound together with anything it started, and the line says so; so is
    /// a straggler left holding its output after it exited.
    ///
    /// Each case shortens only the bound of the call it hangs. The call that
    /// must succeed keeps its default bound: a loaded host can take more than
    /// 300 ms just to start Python, which once made the append case report a
    /// killed probe instead of a killed append.
    #[test]
    fn a_hanging_writer_is_killed_at_its_bound() {
        let root = result_root("append-hang");
        let rows = append_rows("append-hang");
        let hung_probe = AppendBounds {
            probe: Duration::from_millis(300),
            ..AppendBounds::default()
        };
        let hung_append = AppendBounds {
            append: Duration::from_millis(300),
            ..AppendBounds::default()
        };
        let left = format!(
            "; 2 rows left in {} (diverged 1, record-missing 1)",
            root.display()
        );
        let probe = fake_series(&root, "time.sleep(60)");
        let started = Instant::now();
        assert_eq!(
            append_ledger_rows(&append_to(&probe, &root), &rows, hung_probe),
            format!(
                "parity: ERROR: `{} append-parity --help` did not finish within 300ms and was \
                 killed{left}",
                probe.display()
            )
        );
        assert!(started.elapsed() < Duration::from_secs(20));
        // The append hangs, and a child it started holds its stdout open.
        let append = fake_series(
            &root,
            "if help:\n    sys.exit(0)\nsubprocess.Popen(['sleep', '60'])\ntime.sleep(60)",
        );
        let started = Instant::now();
        assert_eq!(
            append_ledger_rows(&append_to(&append, &root), &rows, hung_append),
            format!(
                "parity: ERROR: append-parity did not finish within 300ms and was killed, so the \
                 ledger may hold some of these rows{left}"
            )
        );
        assert!(started.elapsed() < Duration::from_secs(20));
        // The writer succeeds but leaves a child holding its stdout.
        let straggler = fake_series(
            &root,
            "if help:\n    sys.exit(0)\nsys.stdin.read()\nsubprocess.Popen(['sleep', '60'])\n\
             print('wrote 2 rows', flush=True)",
        );
        let started = Instant::now();
        assert_eq!(
            append_ledger_rows(
                &append_to(&straggler, &root),
                &rows,
                AppendBounds::default()
            ),
            format!(
                "parity: append-parity accepted 2 row(s) from {} (diverged 1, record-missing 1): \
                 wrote 2 rows",
                root.display()
            )
        );
        assert!(started.elapsed() < Duration::from_secs(20));
    }

    /// A staging path that already exists belongs to someone else: staging
    /// there is refused and that file is left as it was. Before review A's
    /// Info finding of
    /// <https://github.com/rrnewton/hermit/issues/3301#issuecomment-5911273090>
    /// the error path removed it. A free path is staged, unlinked at once,
    /// and reads back from its start.
    #[test]
    fn staging_input_never_removes_a_file_it_did_not_create() {
        use std::io::Read;

        let root = result_root("stage-exists");
        let taken = root.join("taken");
        fs::write(&taken, "someone else's").unwrap();
        let error = unlinked_input_at(&taken, b"rows\n").unwrap_err();
        assert!(
            error.starts_with(&format!("cannot create {}: ", taken.display())),
            "{error}"
        );
        assert_eq!(fs::read_to_string(&taken).unwrap(), "someone else's");
        let free = root.join("free");
        let mut file = unlinked_input_at(&free, b"rows\n").unwrap();
        assert!(!free.exists(), "a staged input is unlinked at once");
        let mut text = String::new();
        file.read_to_string(&mut text).unwrap();
        assert_eq!(text, "rows\n");
    }

    /// A record that breaks a credit invariant is refused with its message,
    /// never clamped or skipped; so is a line that is not its record's
    /// canonical encoding, and a record of another run.
    #[test]
    fn an_invalid_record_refuses_the_node() {
        let root = result_root("invalid");
        let node = root.join("portable/manifest_c_programs");
        let scope = [parity_cell("c-programs/a", ParityBackend::Kvm)];
        let status = ledger_status(PostPassState::Complete, Some(&scope), 1, None);
        let mut matched = ledger_diverged("c-programs/a", ParityBackend::Kvm, 3);
        matched.verdict = ParityVerdict::Matched;
        matched.first_divergent_record = None;
        matched.first_difference = None;
        write_node(&node, Some(&status), None);
        fs::write(
            node.join(PARITY_JSONL),
            serde_json::to_string(&matched).unwrap() + "\n",
        )
        .unwrap();
        let error = ledger_sources(&root, None).unwrap_err();
        assert!(
            error.contains("parity record c-programs/a@kvm: a match must be full credit"),
            "{error}"
        );
        let good = ledger_diverged("c-programs/a", ParityBackend::Kvm, 3);
        let spaced = serde_json::to_string_pretty(&good)
            .unwrap()
            .replace('\n', " ");
        fs::write(node.join(PARITY_JSONL), spaced + "\n").unwrap();
        let error = ledger_sources(&root, None).unwrap_err();
        assert!(error.contains("is not the canonical encoding"), "{error}");
        let mut other = good.clone();
        other.run_id = "another-run".into();
        write_node(&node, Some(&status), Some(&[other]));
        let error = ledger_sources(&root, None).unwrap_err();
        assert!(error.contains("belongs to run another-run"), "{error}");
    }

    /// A post-pass that failed outside [`post_pass`] leaves a failed status
    /// naming the cells it owed and no earlier records, so each owed cell is
    /// a record-missing row with the error rather than an empty report
    /// (review A Low 6 of
    /// <https://github.com/rrnewton/hermit/issues/3301#issuecomment-5911273090>).
    /// With no known scope the failed status names no cell and yields no row.
    #[test]
    fn a_post_pass_failed_outside_post_pass_owes_its_scope() {
        let fixture = Fixture::new("mark-failed");
        let config = fixture.config();
        let scope = BTreeSet::from([
            parity_cell("fx/one", ParityBackend::Kvm),
            parity_cell("fx/two", ParityBackend::Liteinst),
        ]);
        fs::write(&config.output, "stale\n").unwrap();
        mark_failed(&config, &scope, "the scope panicked").unwrap();
        assert!(!config.output.exists(), "a stale parity.jsonl survived");
        let failed = status(&config);
        assert_eq!(failed.state, PostPassState::Failed);
        assert_eq!(failed.error.as_deref(), Some("the scope panicked"));
        assert_eq!(failed.summary, None);
        assert_eq!(
            failed.scope,
            Some(vec![
                "fx/one@kvm".to_string(),
                "fx/two@liteinst".to_string()
            ])
        );
        let rows = node_ledger_sources(&config.output_dir, "lane", "node", None).unwrap();
        let seen = rows
            .iter()
            .map(|row| {
                (
                    row.cell.as_str(),
                    row.verdict.as_str(),
                    row.reason.as_deref().unwrap(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            seen,
            [
                (
                    "fx/one@kvm",
                    "record-missing",
                    "parity post-pass failed: the scope panicked"
                ),
                (
                    "fx/two@liteinst",
                    "record-missing",
                    "parity post-pass failed: the scope panicked"
                ),
            ]
        );
        mark_failed(&config, &BTreeSet::new(), "no scope").unwrap();
        let failed = status(&config);
        assert_eq!(failed.state, PostPassState::Failed);
        assert_eq!((failed.cells, failed.scope), (0, Some(Vec::new())));
        assert_eq!(failed.error.as_deref(), Some("no scope"));
        assert!(
            node_ledger_sources(&config.output_dir, "lane", "node", None)
                .unwrap()
                .is_empty()
        );
    }

    /// The real post-pass output reads back as one row per record, and a
    /// status marked running before the cells ran owes its whole scope.
    #[test]
    fn a_real_post_pass_reads_back_through_ledger_sources() {
        let fixture = Fixture::new("ledger");
        let rows = vec![
            fixture.row("fx/one", "ptrace", 1, "PASS", Some(REFERENCE)),
            fixture.row("fx/one", "kvm", 1, "PASS", Some(REFERENCE)),
        ];
        let scope = BTreeSet::from([
            parity_cell("fx/one", ParityBackend::Kvm),
            parity_cell("fx/one", ParityBackend::Sabre),
        ]);
        let config = fixture.config();
        mark_running(&config, &scope).unwrap();
        let running = node_ledger_sources(&config.output_dir, "lane", "node", None).unwrap();
        assert_eq!(running.len(), 2);
        assert!(
            running
                .iter()
                .all(|row| row.verdict == LedgerVerdict::RecordMissing
                    && row.source.post_pass_state == LedgerPostPassState::Running)
        );
        let report = post_pass(&config, &scope, &rows).unwrap();
        let sources = node_ledger_sources(&config.output_dir, "lane", "node", None).unwrap();
        assert_eq!(
            sources
                .iter()
                .map(|row| row.record.clone().unwrap())
                .collect::<Vec<_>>(),
            report.records
        );
        assert!(sources.iter().all(|row| {
            row.source.scope_source == LedgerScopeSource::StatusScope
                && row.source.hermit_bin_sha256.as_deref()
                    == Some(sha256_hex(FAKE_LOG_DIFF.as_bytes()).as_str())
        }));
        // Source rows survive a JSON round trip unchanged.
        for row in &sources {
            let text = serde_json::to_string(row).unwrap();
            assert_eq!(
                &serde_json::from_str::<ParityLedgerSource>(&text).unwrap(),
                row
            );
        }
    }

    fn envelope(source: &ParityLedgerSource, producer: ParityProducer) -> ParityLedgerRow {
        let run_id = source.run_id.clone().unwrap_or_else(|| LEDGER_RUN.into());
        ParityLedgerRow {
            schema: PARITY_LEDGER_SCHEMA.into(),
            event_type: PARITY_LEDGER_EVENT_TYPE.into(),
            event_id: parity_event_id(
                producer,
                &run_id,
                &source.cell,
                &source.source.lane,
                &source.source.node,
            ),
            team: "hermit".into(),
            host: "fixture-host".into(),
            emitted_at: "2026-09-29T04:00:00Z".into(),
            producer,
            run_id,
            hermit_sha: SHA.into(),
            source_tree_dirty: false,
            cell: source.cell.clone(),
            test_id: source.test_id.clone(),
            backend: source.backend,
            verdict: source.verdict,
            unavailable_class: source.unavailable_class,
            operand: source.operand,
            reason: source.reason.clone(),
            source: source.source.clone(),
            record: source.record.clone(),
        }
    }

    /// The published envelope must agree with its record and its own
    /// identity; a disagreement is refused, not reconciled.
    #[test]
    fn a_ledger_row_that_disagrees_with_its_record_is_refused() {
        let root = result_root("envelope");
        let node = root.join("portable/manifest_c_programs");
        let scope = five_cells();
        write_node(
            &node,
            Some(&ledger_status(
                PostPassState::Failed,
                Some(&scope),
                5,
                Some("x"),
            )),
            Some(&[ledger_diverged("c-programs/a", ParityBackend::Kvm, 3)]),
        );
        let sources = ledger_sources(&root, None).unwrap();
        for source in &sources {
            envelope(source, ParityProducer::Validate)
                .validate()
                .unwrap();
        }
        let diverged = envelope(&sources[0], ParityProducer::Validate);
        assert_eq!(diverged.verdict, LedgerVerdict::Diverged);
        let mut row = diverged.clone();
        row.verdict = LedgerVerdict::Matched;
        let error = row.validate().unwrap_err();
        assert!(
            error.contains("row verdict matched disagrees with record verdict diverged"),
            "{error}"
        );
        let mut row = diverged.clone();
        row.event_id = "0".repeat(64);
        assert!(row.validate().unwrap_err().contains("is not sha256("));
        let mut row = diverged.clone();
        row.run_id = "other-run".into();
        row.event_id = parity_event_id(
            row.producer,
            &row.run_id,
            &row.cell,
            &row.source.lane,
            &row.source.node,
        );
        assert!(
            row.validate()
                .unwrap_err()
                .contains("row run_id \"other-run\" disagrees with record run_id")
        );
        let mut row = diverged.clone();
        row.record = None;
        assert!(row.validate().unwrap_err().contains("needs its record"));
        let missing = envelope(&sources[1], ParityProducer::PressureTest);
        assert_eq!(missing.verdict, LedgerVerdict::RecordMissing);
        let mut row = missing.clone();
        row.reason = None;
        assert!(row.validate().unwrap_err().contains("needs a reason"));
        let mut row = missing;
        row.record = diverged.record.clone();
        assert!(row.validate().unwrap_err().contains("carries no record"));
        let mut text = serde_json::to_value(&diverged).unwrap();
        text["extra"] = serde_json::json!(1);
        assert!(serde_json::from_value::<ParityLedgerRow>(text).is_err());
    }

    /// The deduplication order is the most adverse verdict first.
    #[test]
    fn ledger_verdicts_order_from_most_to_least_adverse() {
        use LedgerVerdict as V;
        let mut verdicts = vec![
            V::Matched,
            V::Diverged,
            V::InputsNotEqualized,
            V::CandidateMissing,
            V::ReferenceMissing,
            V::Unavailable,
            V::RecordMissing,
        ];
        verdicts.sort();
        assert_eq!(
            verdicts
                .iter()
                .map(|verdict| verdict.as_str())
                .collect::<Vec<_>>(),
            [
                "record-missing",
                "unavailable",
                "reference-missing",
                "candidate-missing",
                "inputs-not-equalized",
                "diverged",
                "matched"
            ]
        );
        for verdict in verdicts {
            assert_eq!(
                serde_json::to_value(verdict).unwrap(),
                serde_json::json!(verdict.as_str())
            );
        }
    }
}
