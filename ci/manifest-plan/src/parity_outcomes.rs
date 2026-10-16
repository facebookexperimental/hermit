// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! Outcome parity against ptrace: for one finished run, every manifest cell
//! whose outcome on a candidate backend differs from the same test and mode's
//! outcome on ptrace (`test-harness parity outcomes`).
//!
//! This is coarser than [`crate::parity`], which compares the verify logs of
//! two passing runs message by message. Here a cell's OUTCOME is its final
//! row's `outcome` in the run's `results.jsonl` (`PASS`, `FAIL`, `ERROR`, ...),
//! or, when the run has no row for it, the manifest's own status: `disabled`,
//! `not-selected` (enabled, but full validation does not run it), `no-mode`
//! (the test has no such mode), or `not-run` (full validation selects it, but
//! this run has no row, as when one of its lanes did not run). A cell with no
//! row on either side is never a difference.
//!
//! The cells considered are every (test, mode) whose ptrace cell full
//! validation selects, plus every (test, mode) for which the run has a row on
//! any candidate backend. A baseline is this command's own `--json` output;
//! [`gate`] reports every cell that matched ptrace in the baseline and no
//! longer does, which is the regression a step may not introduce.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

use crate::parity::PARITY_REFERENCE_BACKEND;
use crate::parity::ParityBackend;
use crate::parity::SideStatus;
use crate::parity::mode_side_status;
use crate::runner::ManifestSet;
use crate::runner::Population;
use crate::runner::Selection;

/// One cell whose candidate outcome differs from ptrace's.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OutcomeDifference {
    pub test: String,
    pub mode: String,
    pub backend: String,
    /// ptrace's outcome.
    pub reference: String,
    /// The candidate's outcome.
    pub candidate: String,
    /// The manifest's reason, when an outcome comes from the manifest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl OutcomeDifference {
    fn key(&self) -> (&str, &str, &str) {
        (&self.test, &self.mode, &self.backend)
    }
}

/// Every difference of one run, with the counts that give it scale.
#[derive(Debug)]
pub struct OutcomeReport {
    pub results_files: Vec<PathBuf>,
    pub rows: usize,
    /// The distinct `hermit_sha` values of the rows: the commit (or commits)
    /// the run tested, which the manifests compared against should match.
    pub hermit_shas: BTreeSet<String>,
    /// (test, mode) pairs considered.
    pub cells: usize,
    /// Of those, the pairs with a ptrace row in this run.
    pub reference_rows: usize,
    /// Final rows naming a test and mode these manifests do not declare,
    /// as when the run tested another commit; they are not compared.
    pub undeclared_rows: usize,
    pub differences: Vec<OutcomeDifference>,
}

#[derive(Deserialize)]
struct ResultRow {
    test: String,
    mode: String,
    backend: Option<String>,
    outcome: String,
    attempt: u64,
    hermit_sha: Option<String>,
}

/// One run's result files, row count, tested commits and final outcomes.
struct RunOutcomes {
    files: Vec<PathBuf>,
    rows: usize,
    hermit_shas: BTreeSet<String>,
    finals: BTreeMap<(String, String, String), String>,
}

/// The final row of every (test, mode, backend) in `e2e_root`'s
/// `<lane>/<node>/results.jsonl` files: the highest attempt.
fn final_outcomes(e2e_root: &Path) -> Result<RunOutcomes, String> {
    let mut files = Vec::new();
    for lane in read_dir_sorted(e2e_root)? {
        if lane.is_dir() {
            for node in read_dir_sorted(&lane)? {
                let results = node.join("results.jsonl");
                if results.is_file() {
                    files.push(results);
                }
            }
        }
    }
    if files.is_empty() {
        return Err(format!(
            "{} holds no <lane>/<node>/results.jsonl; pass a run's e2e result root",
            e2e_root.display()
        ));
    }
    let mut rows = 0;
    let mut hermit_shas = BTreeSet::new();
    let mut finals: BTreeMap<(String, String, String), (u64, String, PathBuf)> = BTreeMap::new();
    for file in &files {
        let text = fs::read_to_string(file)
            .map_err(|error| format!("cannot read {}: {error}", file.display()))?;
        for (index, line) in text.lines().enumerate() {
            let row: ResultRow = serde_json::from_str(line)
                .map_err(|error| format!("{}:{}: {error}", file.display(), index + 1))?;
            rows += 1;
            hermit_shas.extend(row.hermit_sha);
            let Some(backend) = row.backend else {
                continue;
            };
            let key = (row.test, row.mode, backend);
            match finals.get(&key) {
                Some((attempt, _, earlier)) if *attempt == row.attempt && earlier != file => {
                    return Err(format!(
                        "{}@{}/{} has attempt {} in both {} and {}",
                        key.0,
                        key.2,
                        key.1,
                        row.attempt,
                        earlier.display(),
                        file.display()
                    ));
                }
                Some((attempt, _, _)) if *attempt > row.attempt => {}
                _ => {
                    finals.insert(key, (row.attempt, row.outcome, file.clone()));
                }
            }
        }
    }
    let finals = finals
        .into_iter()
        .map(|(key, (_, outcome, _))| (key, outcome))
        .collect();
    Ok(RunOutcomes {
        files,
        rows,
        hermit_shas,
        finals,
    })
}

fn read_dir_sorted(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut entries = fs::read_dir(dir)
        .map_err(|error| format!("cannot read {}: {error}", dir.display()))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("cannot read {}: {error}", dir.display()))?;
    entries.sort();
    Ok(entries)
}

/// A side's outcome label and, when it comes from the manifest, the reason.
fn manifest_outcome(status: SideStatus) -> (String, Option<String>) {
    match status {
        SideStatus::NoVerifyMode { reason } => ("no-mode".into(), Some(reason)),
        SideStatus::Disabled { reason } => ("disabled".into(), Some(reason)),
        SideStatus::NotSelected { reason } => ("not-selected".into(), Some(reason)),
        SideStatus::Selected => (
            "not-run".into(),
            Some("full validation selects this cell, but the run has no row for it".into()),
        ),
    }
}

/// Every cell of the run under `e2e_root` whose outcome on a candidate in
/// `backends` differs from ptrace's.
pub fn compare(
    manifests: &ManifestSet,
    e2e_root: &Path,
    backends: &[ParityBackend],
) -> Result<OutcomeReport, String> {
    let RunOutcomes {
        files: results_files,
        rows,
        hermit_shas,
        finals,
    } = final_outcomes(e2e_root)?;
    let selected: BTreeSet<(String, String, String)> = manifests
        .select(&Selection {
            population: Some(Population::Required),
            ..Selection::default()
        })?
        .into_iter()
        .filter_map(|cell| {
            cell.id
                .backend
                .map(|backend| (cell.id.test, cell.id.mode, backend))
        })
        .collect();
    let mut cells = 0;
    let mut reference_rows = 0;
    let mut differences = Vec::new();
    for (_, _, _, test) in manifests.all_tests() {
        for mode in test.modes.keys() {
            let row = |backend: &str| finals.get(&(test.id.clone(), mode.clone(), backend.into()));
            let reference_selected = selected.contains(&(
                test.id.clone(),
                mode.clone(),
                PARITY_REFERENCE_BACKEND.to_string(),
            ));
            let any_candidate_row = ParityBackend::ALL
                .iter()
                .any(|backend| row(backend.as_str()).is_some());
            if !reference_selected && !any_candidate_row && row(PARITY_REFERENCE_BACKEND).is_none()
            {
                continue;
            }
            cells += 1;
            let side = |backend: &str| -> Result<(String, Option<String>, bool), String> {
                if let Some(outcome) = row(backend) {
                    return Ok((outcome.clone(), None, true));
                }
                let selected =
                    selected.contains(&(test.id.clone(), mode.clone(), backend.to_string()));
                let (label, reason) =
                    manifest_outcome(mode_side_status(test, mode, backend, selected)?);
                Ok((label, reason, false))
            };
            let (reference, _, reference_has_row) = side(PARITY_REFERENCE_BACKEND)?;
            reference_rows += usize::from(reference_has_row);
            for backend in backends {
                let (candidate, reason, candidate_has_row) = side(backend.as_str())?;
                if (reference_has_row || candidate_has_row) && candidate != reference {
                    differences.push(OutcomeDifference {
                        test: test.id.clone(),
                        mode: mode.clone(),
                        backend: backend.as_str().to_string(),
                        reference: reference.clone(),
                        candidate,
                        reason,
                    });
                }
            }
        }
    }
    let declared: BTreeSet<(&str, &str)> = manifests
        .all_tests()
        .flat_map(|(_, _, _, test)| {
            test.modes
                .keys()
                .map(|mode| (test.id.as_str(), mode.as_str()))
        })
        .collect();
    let undeclared_rows = finals
        .keys()
        .filter(|(test, mode, _)| !declared.contains(&(test.as_str(), mode.as_str())))
        .count();
    differences.sort();
    Ok(OutcomeReport {
        results_files,
        rows,
        hermit_shas,
        cells,
        reference_rows,
        undeclared_rows,
        differences,
    })
}

/// A baseline compared with a later run's differences.
#[derive(Debug, Default)]
pub struct GateReport {
    /// Matched ptrace in the baseline; differs now.
    pub regressions: Vec<OutcomeDifference>,
    /// Differed in the baseline; matches now.
    pub newly_matching: Vec<OutcomeDifference>,
    /// Differs in both, with a different candidate or reference outcome:
    /// (baseline, now).
    pub changed: Vec<(OutcomeDifference, OutcomeDifference)>,
}

/// Parse a baseline: this command's `--json` output, one difference per line.
pub fn parse_baseline(text: &str) -> Result<Vec<OutcomeDifference>, String> {
    let mut seen = BTreeSet::new();
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            let row: OutcomeDifference = serde_json::from_str(line)
                .map_err(|error| format!("baseline line {}: {error}", index + 1))?;
            if !seen.insert((row.test.clone(), row.mode.clone(), row.backend.clone())) {
                return Err(format!(
                    "baseline line {} repeats {}@{}/{}",
                    index + 1,
                    row.test,
                    row.backend,
                    row.mode
                ));
            }
            Ok(row)
        })
        .collect()
}

/// Compare `now` with `baseline`, over the candidate backends `backends`.
pub fn gate(
    baseline: &[OutcomeDifference],
    now: &[OutcomeDifference],
    backends: &[ParityBackend],
) -> GateReport {
    let in_scope = |row: &&OutcomeDifference| {
        backends
            .iter()
            .any(|backend| backend.as_str() == row.backend)
    };
    let before: BTreeMap<_, _> = baseline
        .iter()
        .filter(in_scope)
        .map(|row| (row.key(), row))
        .collect();
    let after: BTreeMap<_, _> = now
        .iter()
        .filter(in_scope)
        .map(|row| (row.key(), row))
        .collect();
    let mut report = GateReport::default();
    for (key, row) in &after {
        match before.get(key) {
            None => report.regressions.push((*row).clone()),
            Some(old) if old.candidate != row.candidate || old.reference != row.reference => {
                report.changed.push(((*old).clone(), (*row).clone()))
            }
            Some(_) => {}
        }
    }
    for (key, row) in &before {
        if !after.contains_key(key) {
            report.newly_matching.push((*row).clone());
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn difference(test: &str, backend: &str, candidate: &str) -> OutcomeDifference {
        OutcomeDifference {
            test: test.into(),
            mode: "verify".into(),
            backend: backend.into(),
            reference: "PASS".into(),
            candidate: candidate.into(),
            reason: None,
        }
    }

    #[test]
    fn gate_names_regressions_improvements_and_changes_within_scope() {
        let baseline = [
            difference("a", "kvm", "disabled"),
            difference("b", "kvm", "disabled"),
            difference("c", "dbt", "FAIL"),
        ];
        let now = [
            difference("a", "kvm", "FAIL"),
            difference("d", "kvm", "FAIL"),
            difference("e", "dbt", "FAIL"),
        ];
        let report = gate(&baseline, &now, &[ParityBackend::Kvm]);
        assert_eq!(report.regressions, [difference("d", "kvm", "FAIL")]);
        assert_eq!(report.newly_matching, [difference("b", "kvm", "disabled")]);
        assert_eq!(
            report.changed,
            [(
                difference("a", "kvm", "disabled"),
                difference("a", "kvm", "FAIL")
            )]
        );
    }

    #[test]
    fn baseline_round_trips_and_refuses_a_repeated_cell() {
        let mut row = difference("a", "kvm", "disabled");
        row.reason = Some("why".into());
        let line = serde_json::to_string(&row).unwrap();
        assert_eq!(parse_baseline(&format!("{line}\n")).unwrap(), [row]);
        let error = parse_baseline(&format!("{line}\n{line}\n")).unwrap_err();
        assert!(error.contains("repeats a@kvm/verify"), "{error}");
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hermit-parity-outcomes-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_rows(root: &Path, node: &str, rows: &[serde_json::Value]) {
        let dir = root.join("portable").join(node);
        fs::create_dir_all(&dir).unwrap();
        let text = rows
            .iter()
            .map(|row| format!("{row}\n"))
            .collect::<String>();
        fs::write(dir.join("results.jsonl"), text).unwrap();
    }

    fn row(test: &str, backend: Option<&str>, outcome: &str, attempt: u64) -> serde_json::Value {
        serde_json::json!({
            "test": test, "mode": "verify", "backend": backend,
            "outcome": outcome, "attempt": attempt, "lane": "portable",
        })
    }

    #[test]
    fn final_outcome_is_the_last_attempt_and_a_duplicate_final_is_refused() {
        let root = scratch("finals");
        write_rows(
            &root,
            "manifest_a",
            &[
                row("t", Some("dbt"), "FAIL", 1),
                row("t", Some("dbt"), "PASS", 2),
                row("t", None, "PASS", 1),
            ],
        );
        let run = final_outcomes(&root).unwrap();
        assert_eq!((run.files.len(), run.rows), (1, 3));
        assert!(run.hermit_shas.is_empty());
        assert_eq!(
            run.finals,
            BTreeMap::from([(("t".into(), "verify".into(), "dbt".into()), "PASS".into())])
        );
        write_rows(&root, "manifest_b", &[row("t", Some("dbt"), "PASS", 2)]);
        let error = final_outcomes(&root).err().unwrap();
        assert!(error.contains("has attempt 2 in both"), "{error}");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn compare_names_manifest_and_result_differences_against_the_real_manifests() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let manifests = ManifestSet::load(&repo).unwrap();
        let selected: BTreeSet<(String, String)> = manifests
            .select(&Selection {
                population: Some(Population::Required),
                mode: Some("verify".into()),
                ..Selection::default()
            })
            .unwrap()
            .into_iter()
            .filter_map(|cell| cell.id.backend.map(|backend| (cell.id.test, backend)))
            .collect();
        let both = |backend: &str| {
            selected
                .iter()
                .find(|(test, side)| {
                    side == backend && selected.contains(&(test.clone(), "ptrace".into()))
                })
                .map(|(test, _)| test.clone())
        };
        let disabled_on_kvm = manifests
            .all_tests()
            .map(|(_, _, _, test)| test)
            .find(|test| {
                selected.contains(&(test.id.clone(), "ptrace".into()))
                    && test.modes["verify"].backends_disabled.contains_key("kvm")
            })
            .map(|test| test.id.clone())
            .expect("some ptrace-selected verify cell is disabled on kvm");
        let failing = both("kvm").expect("some verify cell runs on ptrace and kvm");
        let matching = selected
            .iter()
            .filter(|(test, side)| side == "kvm" && test != &failing)
            .find(|(test, _)| selected.contains(&(test.clone(), "ptrace".into())))
            .map(|(test, _)| test.clone())
            .unwrap();
        let root = scratch("compare");
        write_rows(
            &root,
            "manifest_x",
            &[
                row(&disabled_on_kvm, Some("ptrace"), "PASS", 1),
                row(&failing, Some("ptrace"), "PASS", 1),
                row(&failing, Some("kvm"), "FAIL", 1),
                row(&matching, Some("ptrace"), "PASS", 1),
                row(&matching, Some("kvm"), "PASS", 1),
            ],
        );
        let report = compare(&manifests, &root, &[ParityBackend::Kvm]).unwrap();
        let find = |test: &str| {
            report
                .differences
                .iter()
                .find(|row| row.test == test && row.mode == "verify")
        };
        let disabled = find(&disabled_on_kvm).unwrap();
        assert_eq!(
            (disabled.reference.as_str(), disabled.candidate.as_str()),
            ("PASS", "disabled")
        );
        assert_eq!(
            disabled.reason.as_deref(),
            Some(
                manifests
                    .all_tests()
                    .find(|(_, _, _, test)| test.id == disabled_on_kvm)
                    .unwrap()
                    .3
                    .modes["verify"]
                    .backends_disabled["kvm"]
                    .as_str()
            )
        );
        let failed = find(&failing).unwrap();
        assert_eq!(
            (
                failed.reference.as_str(),
                failed.candidate.as_str(),
                failed.reason.as_deref()
            ),
            ("PASS", "FAIL", None)
        );
        assert!(find(&matching).is_none());
        // Every other ptrace-selected cell has no ptrace row in this run, and
        // a candidate without a row either is never a difference.
        assert!(report.differences.iter().all(|row| row.reference == "PASS"));
        assert_eq!((report.reference_rows, report.undeclared_rows), (3, 0));
        assert!(report.differences.iter().all(|row| row.backend == "kvm"));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn empty_root_is_refused() {
        let root = scratch("empty");
        let error = final_outcomes(&root).err().unwrap();
        assert!(
            error.contains("holds no <lane>/<node>/results.jsonl"),
            "{error}"
        );
        fs::remove_dir_all(&root).unwrap();
    }
}
