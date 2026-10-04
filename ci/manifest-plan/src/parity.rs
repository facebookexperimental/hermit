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

use crate::ci_selection::CiSelection;
use crate::logdiff_report::LogDiffReport;
use crate::logdiff_report::LogDiffVerdict;
use crate::runner::CellResult;
use crate::runner::EQUALIZED_INPUTS;
use crate::runner::ManifestSet;
use crate::runner::ModeRecipe;
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

/// The outcome of one parity cell in one run.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ParityVerdict {
    Matched,
    Diverged,
    /// The ptrace `verify` run left no retained log to compare.
    ReferenceMissing,
    /// The candidate `verify` run left no retained log to compare.
    CandidateMissing,
    /// Both logs exist but no comparison verdict could be reached (the
    /// log-diff refused, found nothing comparable, or produced a report that is
    /// not cross-backend evidence).
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

/// One line of `parity.jsonl`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ParityRecord {
    pub schema: u64,
    pub test_id: String,
    pub backend: ParityBackend,
    pub verdict: ParityVerdict,
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
    /// A report whose verdict is not matched or diverged, or which fails the
    /// cross-backend evidence policy, becomes `unavailable` with the reason.
    /// It never becomes a zero-credit divergence. A backend that cannot be
    /// given the reference's inputs becomes `inputs-not-equalized` whatever the
    /// report says, and claiming equal inputs for it is an error.
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
                false,
                why,
                Some(reference_log),
                Some(candidate_log),
                run_id,
                hermit_sha,
            );
        }
        let unavailable = |reason: String| {
            Self::unmeasured(
                cell,
                ParityVerdict::Unavailable,
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

    /// Record a cell that could not be measured.
    #[allow(clippy::too_many_arguments)]
    pub fn unmeasured(
        cell: &ParityCellId,
        verdict: ParityVerdict,
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
        if let Some(why) = self.backend.inputs_not_equalizable() {
            if self.inputs_equalized || self.verdict.is_measured() {
                return Err(format!(
                    "{at}: {} inputs cannot be equalized, so it reports no measured \
                     comparison and never equal inputs: {why}",
                    self.backend
                ));
            }
        } else if self.verdict == ParityVerdict::InputsNotEqualized {
            return Err(format!(
                "{at}: {} inputs can be equalized; a comparison with unequal inputs is \
                 measured and reports unequalized_credit",
                self.backend
            ));
        }
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
pub const PARITY_STATUS_SCHEMA: u64 = 1;
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
/// The first verification run's retained log: the same file the ptrace golden
/// normalization reads.
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
    /// to hand over, with its reason. Such a cell ran but has no row a
    /// comparison can trust, so an operand here is `unavailable` with that
    /// reason rather than missing. Empty for the harness, which hands over
    /// every row of its process.
    pub rejected: BTreeMap<(String, String), String>,
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
    pub summary: Option<String>,
    pub error: Option<String>,
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

fn panic_text(panic: &(dyn std::any::Any + Send)) -> String {
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
    /// One line of honest accounting: every verdict count, the mean clean
    /// credit over the cells measured with equal inputs, and apart from it the
    /// mean credit over the cells measured with unequal inputs. Neither mean
    /// counts an unmeasured cell.
    pub fn summary_line(&self) -> String {
        let count = |verdict| {
            self.records
                .iter()
                .filter(|record| record.verdict == verdict)
                .count()
        };
        let mean = |credits: Vec<f64>, inputs: &str| {
            if credits.is_empty() {
                format!("none measured with {inputs} inputs")
            } else {
                format!(
                    "mean credit {:.4} over {} measured with {inputs} inputs",
                    credits.iter().sum::<f64>() / credits.len() as f64,
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
            "parity: {} cell(s) -> {}: matched {}, diverged {}, reference-missing {}, \
             candidate-missing {}, unavailable {}, inputs-not-equalized {}; {mean}; \
             {} log-diff comparison(s), 0 guest runs",
            self.records.len(),
            self.path.display(),
            count(ParityVerdict::Matched),
            count(ParityVerdict::Diverged),
            count(ParityVerdict::ReferenceMissing),
            count(ParityVerdict::CandidateMissing),
            count(ParityVerdict::Unavailable),
            count(ParityVerdict::InputsNotEqualized),
            self.log_diff_runs,
        )
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

/// Why one side of a cell cannot be compared.
struct Unusable {
    verdict: ParityVerdict,
    reason: String,
    log: Option<PathBuf>,
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
/// It runs no guest. For each measurable cell it runs one `hermit log-diff`
/// with [`PARITY_RECORD_ENVELOPE`] between the ptrace golden and the
/// candidate's first-run log, and records it with
/// [`ParityRecord::from_comparison`]. A missing operand is
/// [`ParityVerdict::ReferenceMissing`] or [`ParityVerdict::CandidateMissing`];
/// an operand that is not a passing verify cell, operands run with different
/// `HERMIT_EPOCH` values, or a comparison that cannot be trusted or recorded,
/// is [`ParityVerdict::Unavailable`]. Every cell in scope gets exactly one
/// line.
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
    let mut status = PostPassStatus {
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
        summary: None,
        error: None,
    };
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
    let rejected = |test: &str, backend: &str| -> Option<&str> {
        config
            .rejected
            .get(&(test.to_string(), backend.to_string()))
            .map(String::as_str)
    };

    let mut records: Vec<Option<ParityRecord>> = vec![None; scope.len()];
    let mut comparisons = Vec::new();
    let mut goldens: BTreeMap<String, Result<Operand, Unusable>> = BTreeMap::new();
    for (index, cell) in scope.iter().enumerate() {
        let unmeasured =
            |verdict, reason: &str, reference: Option<&Path>, candidate: Option<&Path>| {
                // Decided before a comparison is chosen, so the inputs are not
                // shown to be equal.
                ParityRecord::unmeasured(
                    cell,
                    verdict,
                    false,
                    reason,
                    reference.map(path_text).as_deref(),
                    candidate.map(path_text).as_deref(),
                    &config.run_id,
                    &config.hermit_sha,
                )
            };
        if let Some(why) = cell.backend.inputs_not_equalizable() {
            records[index] = Some(unmeasured(
                ParityVerdict::InputsNotEqualized,
                why,
                None,
                None,
            )?);
            continue;
        }
        let candidate_role = format!("{} candidate", cell.backend);
        let candidate_history = history(&cell.test_id, cell.backend.as_str());
        let candidate_rejected = rejected(&cell.test_id, cell.backend.as_str());
        if candidate_history.is_none()
            && candidate_rejected.is_none()
            && history(&cell.test_id, PARITY_REFERENCE_BACKEND).is_some()
        {
            // The candidate was planned elsewhere or not at all, so this
            // process retained no ptrace log for it and writes no golden.
            records[index] = Some(unmeasured(
                ParityVerdict::CandidateMissing,
                &no_result_row(&cell.test_id, &candidate_role),
                None,
                None,
            )?);
            continue;
        }
        let reference = goldens.entry(cell.test_id.clone()).or_insert_with(|| {
            reference_golden(
                config,
                &cell.test_id,
                history(&cell.test_id, PARITY_REFERENCE_BACKEND),
                rejected(&cell.test_id, PARITY_REFERENCE_BACKEND),
            )
        });
        let candidate = retained_operand(
            &cell.test_id,
            &candidate_role,
            candidate_history,
            candidate_rejected,
            ParityVerdict::CandidateMissing,
        )
        .and_then(|(row, log)| {
            hashed(&log, ParityGuestInputs::from_result(&row)).map_err(|reason| Unusable {
                verdict: ParityVerdict::CandidateMissing,
                reason,
                log: None,
            })
        });
        let record = match (&*reference, candidate) {
            (Ok(reference), Ok(candidate))
                if reference.inputs.epoch == candidate.inputs.epoch
                    && reference.inputs.epoch.is_some() =>
            {
                comparisons.push(Comparison {
                    index,
                    cell: cell.clone(),
                    reference: reference.clone(),
                    inputs_equalized: inputs_equalized(&reference.inputs, &candidate.inputs),
                    candidate,
                });
                continue;
            }
            (Ok(reference), Ok(candidate)) => {
                let shown = |epoch: &Option<String>| {
                    epoch.clone().unwrap_or_else(|| "unrecorded".to_string())
                };
                let reason = if reference.inputs.epoch.is_some() && candidate.inputs.epoch.is_some()
                {
                    "the operands ran with different HERMIT_EPOCH values"
                } else {
                    "the operands cannot be shown to share a HERMIT_EPOCH"
                };
                unmeasured(
                    ParityVerdict::Unavailable,
                    &format!(
                        "{reason}: {PARITY_REFERENCE_BACKEND} reference {}, {} candidate {}",
                        shown(&reference.inputs.epoch),
                        cell.backend,
                        shown(&candidate.inputs.epoch)
                    ),
                    Some(&reference.log),
                    Some(&candidate.log),
                )?
            }
            (Err(unusable), candidate) => unmeasured(
                unusable.verdict,
                &unusable.reason,
                unusable.log.as_deref(),
                candidate.as_ref().ok().map(|operand| operand.log.as_path()),
            )?,
            (Ok(reference), Err(unusable)) => unmeasured(
                unusable.verdict,
                &unusable.reason,
                Some(&reference.log),
                unusable.log.as_deref(),
            )?,
        };
        records[index] = Some(record);
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
                // A report no record can carry is this cell's problem, not
                // the post-pass's.
                Err(error) => ParityRecord::unmeasured(
                    &comparison.cell,
                    ParityVerdict::Unavailable,
                    comparison.inputs_equalized,
                    &format!("the comparison could not be recorded: {error}"),
                    Some(&path_text(&comparison.reference.log)),
                    Some(&path_text(&comparison.candidate.log)),
                    &config.run_id,
                    &config.hermit_sha,
                )?,
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

/// The final row of one verify cell, and its single retained first-run log.
/// A cell whose row the caller rejected (`rejected`, with its reason) is
/// unavailable whatever rows it has.
fn retained_operand(
    test: &str,
    role: &str,
    history: Option<&Vec<CellResult>>,
    rejected: Option<&str>,
    missing: ParityVerdict,
) -> Result<(CellResult, PathBuf), Unusable> {
    let unusable = |verdict, reason: String| Unusable {
        verdict,
        reason,
        log: None,
    };
    if let Some(why) = rejected {
        return Err(unusable(
            ParityVerdict::Unavailable,
            format!("the {role} verify cell of {test}: {why}"),
        ));
    }
    let Some(history) = history else {
        return Err(unusable(missing, no_result_row(test, role)));
    };
    let row = crate::runner::cell_result_after_retries(history)
        .map_err(|error| {
            unusable(
                ParityVerdict::Unavailable,
                format!("the {role} verify cell of {test} has no valid result history: {error}"),
            )
        })?
        .clone();
    let detail = || {
        row.reason
            .as_deref()
            .unwrap_or("no reason recorded")
            .to_string()
    };
    match row.outcome.as_str() {
        // A stripped-comparator PASS is below L2: it does not establish the
        // same-backend canonical determinism a parity comparison stands on.
        "PASS"
            if row
                .relaxations
                .iter()
                .any(|relaxation| relaxation.starts_with("comparator=stripped")) =>
        {
            return Err(unusable(
                ParityVerdict::Unavailable,
                format!(
                    "the {role} verify cell of {test} passed only the stripped comparison, which is below L2"
                ),
            ));
        }
        "PASS" => {}
        "HOST-INAPPLICABLE" => {
            return Err(unusable(
                missing,
                format!(
                    "the {role} verify cell of {test} was host-inapplicable, so it left no log: {}",
                    detail()
                ),
            ));
        }
        "FAIL" => {
            return Err(unusable(
                ParityVerdict::Unavailable,
                format!(
                    "the {role} verify cell of {test} failed determinism: {}",
                    detail()
                ),
            ));
        }
        other => {
            return Err(unusable(
                ParityVerdict::Unavailable,
                format!(
                    "the {role} verify cell of {test} ended {other} ({}): {}",
                    row.error_kind.as_deref().unwrap_or("no error kind"),
                    detail()
                ),
            ));
        }
    }
    let Some(directory) = row
        .argv
        .iter()
        .position(|arg| arg == VERIFY_LOG_DIR_FLAG)
        .and_then(|flag| row.argv.get(flag + 1))
        .map(PathBuf::from)
    else {
        return Err(unusable(
            missing,
            format!(
                "the {role} verify cell of {test} retained no logs (its argv has no \
                 {VERIFY_LOG_DIR_FLAG})"
            ),
        ));
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
            return Err(unusable(
                missing,
                format!(
                    "the {role} verify cell of {test} retained no readable log directory {}: {error}",
                    directory.display()
                ),
            ));
        }
    };
    logs.sort();
    match logs.as_slice() {
        [log] if log.metadata().is_ok_and(|metadata| metadata.len() > 0) => {
            let log = log.clone();
            Ok((row, log))
        }
        [log] => Err(unusable(
            missing,
            format!(
                "the {role} verify cell of {test} retained an empty log {}",
                log.display()
            ),
        )),
        _ => Err(unusable(
            missing,
            format!(
                "the {role} verify cell of {test} retained {} {RETAINED_LOG_PREFIX}* logs in {}; \
                 expected exactly one",
                logs.len(),
                directory.display()
            ),
        )),
    }
}

/// Write the ptrace golden and its sidecar below `config.output_dir` from the
/// reference's retained log. When that log is gone, a golden already written
/// from the same run, cell attempt and artifact directory is used if it still
/// has its recorded hash: first the one below `config.output_dir`, then the
/// one the harness wrote below `config.artifacts`.
fn reference_golden(
    config: &PostPassConfig,
    test: &str,
    history: Option<&Vec<CellResult>>,
    rejected: Option<&str>,
) -> Result<Operand, Unusable> {
    let role = format!("{PARITY_REFERENCE_BACKEND} reference");
    let unavailable = |reason: String| Unusable {
        verdict: ParityVerdict::Unavailable,
        reason,
        log: None,
    };
    let (golden, sidecar_path) = golden_paths(&config.output_dir, test).map_err(&unavailable)?;
    let (row, source) = match retained_operand(
        test,
        &role,
        history,
        rejected,
        ParityVerdict::ReferenceMissing,
    ) {
        Ok(found) => found,
        Err(unusable) => {
            let reusable = (unusable.verdict == ParityVerdict::ReferenceMissing)
                .then_some(history)
                .flatten()
                .and_then(|history| crate::runner::cell_result_after_retries(history).ok())
                .and_then(|row| {
                    existing_golden(&golden, &sidecar_path, row).or_else(|| {
                        let (golden, sidecar) = golden_paths(&config.artifacts, test).ok()?;
                        existing_golden(&golden, &sidecar, row)
                    })
                });
            return reusable.ok_or(unusable);
        }
    };
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
        if fs::hard_link(&source, &golden).is_err() {
            fs::copy(&source, &golden).map_err(|error| {
                format!(
                    "cannot copy {} to {}: {error}",
                    source.display(),
                    golden.display()
                )
            })?;
        }
        let guest_inputs = ParityGuestInputs::from_result(&row);
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
            source_log: path_text(&independent_of_cwd(&source)?),
            log_sha256: operand.sha256.clone(),
            log_bytes: operand.bytes,
            guest_inputs,
            guest_inputs_sha256,
        };
        let mut text = serde_json::to_vec_pretty(&sidecar).map_err(|error| error.to_string())?;
        text.push(b'\n');
        write_atomically(&sidecar_path, &text)?;
        Ok(operand)
    };
    write().map_err(|error| unavailable(format!("cannot write the {role} golden: {error}")))
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
    let unavailable = |reason: String| {
        ParityRecord::unmeasured(
            cell,
            ParityVerdict::Unavailable,
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
    let mut pipe = child.stderr.take().ok_or("stderr pipe is missing")?;
    let reader = thread::spawn(move || {
        let mut kept = Vec::new();
        let mut buffer = [0u8; 8192];
        while let Ok(read) = pipe.read(&mut buffer) {
            if read == 0 {
                break;
            }
            let room = STDERR_LIMIT_BYTES.saturating_sub(kept.len());
            kept.extend_from_slice(&buffer[..read.min(room)]);
        }
        kept
    });
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
        assert_eq!(parsed.cells.len(), 2252);
        assert_eq!(
            parsed.inputs_not_equalizable.keys().collect::<Vec<_>>(),
            [&ParityBackend::Dbt]
        );
        assert!(
            committed.len() < 1024 * 1024,
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
        assert_eq!(row(&counts.all), (2252, 628, 527, 194, 177));
        let by_backend: Vec<_> = counts
            .by_backend
            .iter()
            .map(|(backend, count)| (backend.as_str(), row(count)))
            .collect();
        assert_eq!(
            by_backend,
            [
                ("dbt", (563, 85, 26, 16, 2)),
                ("kvm", (563, 250, 243, 77, 76)),
                ("liteinst", (563, 149, 146, 99, 98)),
                ("sabre", (563, 144, 112, 2, 1)),
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
        let cases: Vec<(&str, ParityRecord)> = vec![
            (
                "a partial-credit match",
                ParityRecord {
                    credit: Some(0.5),
                    ..matched.clone()
                },
            ),
            (
                "a zero-credit match",
                ParityRecord {
                    credit: Some(0.0),
                    ..matched.clone()
                },
            ),
            (
                "an unmeasured match",
                ParityRecord {
                    credit: None,
                    ..matched.clone()
                },
            ),
            (
                "a full-credit divergence",
                ParityRecord {
                    credit: Some(1.0),
                    ..diverged.clone()
                },
            ),
            (
                "credit disagreeing with the prefix",
                ParityRecord {
                    credit: Some(0.25),
                    ..diverged.clone()
                },
            ),
            (
                "a measured verdict with a reason",
                ParityRecord {
                    reason: Some("why".into()),
                    ..diverged.clone()
                },
            ),
            (
                "a short hermit sha",
                ParityRecord {
                    hermit_sha: "abc".into(),
                    ..matched.clone()
                },
            ),
            (
                "an unmeasured verdict with credit",
                ParityRecord {
                    verdict: ParityVerdict::CandidateMissing,
                    reason: Some("no log".into()),
                    candidate_log: None,
                    ..matched.clone()
                },
            ),
            (
                "an unmeasured verdict without a reason",
                ParityRecord {
                    verdict: ParityVerdict::Unavailable,
                    reason: None,
                    credit: None,
                    matched_prefix: None,
                    ..matched.clone()
                },
            ),
            (
                "a missing log that is named",
                ParityRecord {
                    verdict: ParityVerdict::CandidateMissing,
                    reason: Some("no log".into()),
                    credit: None,
                    matched_prefix: None,
                    left_len: None,
                    right_len: None,
                    ..matched.clone()
                },
            ),
        ];
        for (label, record) in cases {
            assert!(record.validate().is_err(), "{label} must be refused");
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
    /// unmeasured cells a verdict and a reason with null credit.
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
            // A retried candidate: the passing attempt's log is the one
            // compared, not the first attempt's.
            fixture.row("fx/same", "liteinst", 1, "FAIL", Some(REFERENCE)),
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
        assert_eq!(matched.unequalized_credit, Some(1.0));
        assert_eq!((matched.left_len, matched.right_len), (Some(3), Some(3)));
        assert_eq!(matched.matched_prefix, Some(3));
        assert!(matched.first_difference.is_none());

        let diverged = by_cell("fx/same", ParityBackend::Liteinst);
        assert_eq!(diverged.verdict, ParityVerdict::Diverged, "{diverged:?}");
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
        let unmeasured = [
            (
                "fx/same",
                ParityBackend::Dbt,
                ParityVerdict::InputsNotEqualized,
                "refuses --bind and --mount",
            ),
            (
                "fx/nolog",
                ParityBackend::Kvm,
                ParityVerdict::CandidateMissing,
                "retained 0 run1_log_* logs",
            ),
            (
                "fx/failed",
                ParityBackend::Kvm,
                ParityVerdict::Unavailable,
                "ptrace reference verify cell of fx/failed failed determinism: fixture FAIL",
            ),
            (
                "fx/stripped",
                ParityBackend::Kvm,
                ParityVerdict::Unavailable,
                "passed only the stripped comparison, which is below L2",
            ),
            (
                "fx/inapplicable",
                ParityBackend::Kvm,
                ParityVerdict::CandidateMissing,
                "host-inapplicable",
            ),
            (
                "fx/candidate-failed",
                ParityBackend::Kvm,
                ParityVerdict::Unavailable,
                "kvm candidate verify cell of fx/candidate-failed failed determinism",
            ),
            (
                "fx/not-retained",
                ParityBackend::Kvm,
                ParityVerdict::CandidateMissing,
                "retained no logs",
            ),
            (
                "fx/absent",
                ParityBackend::Kvm,
                ParityVerdict::ReferenceMissing,
                "has no result row in this run",
            ),
        ];
        for (test, backend, verdict, reason) in unmeasured {
            let record = by_cell(test, backend);
            assert_eq!(record.verdict, verdict, "{record:?}");
            assert!(
                record.reason.as_deref().unwrap().contains(reason),
                "{test}@{backend}: {record:?}"
            );
            assert_eq!(record.unequalized_credit, None, "{record:?}");
            assert_eq!(record.matched_prefix, None, "{record:?}");
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
        assert!(
            !fixture
                .artifacts()
                .join(PARITY_GOLDEN_DIR)
                .join("fx/absent.detlog")
                .exists()
        );
        assert!(
            !fixture
                .artifacts()
                .join(PARITY_GOLDEN_DIR)
                .join("fx/failed.detlog")
                .exists()
        );

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
            "10 cell(s)",
            "matched 1, diverged 1, reference-missing 1, candidate-missing 3, unavailable 3, inputs-not-equalized 1",
            "none measured with equal inputs; mean credit 0.6667 over 2 measured with unequal inputs",
            "2 log-diff comparison(s), 0 guest runs",
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
        assert_eq!(verdict().verdict, ParityVerdict::ReferenceMissing);
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

    /// A verify cell whose row the caller rejected ran, so it is
    /// `unavailable` with the caller's reason, never missing and never
    /// compared: as the candidate or the reference, with or without rows.
    #[test]
    fn a_rejected_operand_is_unavailable_with_the_callers_reason() {
        let fixture = Fixture::new("rejected");
        let rows = vec![
            fixture.row("fx/one", "ptrace", 1, "PASS", Some(REFERENCE)),
            fixture.row("fx/two", "kvm", 1, "PASS", Some(REFERENCE)),
            fixture.row("fx/three", "ptrace", 1, "PASS", Some(REFERENCE)),
            fixture.row("fx/three", "kvm", 1, "PASS", Some(REFERENCE)),
        ];
        let scope = BTreeSet::from([
            parity_cell("fx/one", ParityBackend::Kvm),
            parity_cell("fx/two", ParityBackend::Kvm),
            parity_cell("fx/three", ParityBackend::Kvm),
        ]);
        let mut config = fixture.config();
        config.rejected = BTreeMap::from([
            (("fx/one".into(), "kvm".into()), "why one".to_string()),
            (("fx/two".into(), "ptrace".into()), "why two".to_string()),
            (("fx/three".into(), "kvm".into()), "why three".to_string()),
        ]);
        let report = post_pass(&config, &scope, &rows).unwrap();
        assert_eq!(fixture.log_diff_calls(), 0);
        let found = report
            .records
            .iter()
            .map(|record| {
                (
                    record.test_id.as_str(),
                    record.verdict,
                    record.reason.as_deref().unwrap_or(""),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            found,
            [
                (
                    "fx/one",
                    ParityVerdict::Unavailable,
                    "the kvm candidate verify cell of fx/one: why one"
                ),
                (
                    "fx/three",
                    ParityVerdict::Unavailable,
                    "the kvm candidate verify cell of fx/three: why three"
                ),
                (
                    "fx/two",
                    ParityVerdict::Unavailable,
                    "the ptrace reference verify cell of fx/two: why two"
                ),
            ]
        );
        // The reference of a rejected candidate is still good: its golden is
        // written and named, so a later `parity compare` can reuse it.
        assert!(report.records[0].reference_log.is_some());
        assert!(report.records[0].candidate_log.is_none());
        assert!(report.records[2].reference_log.is_none());
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
}
