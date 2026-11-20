/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The backend order of the cell matrix, checked over the union of every run
//! type's required cells ([`required_cells`]): full validation's
//! (`ci/expected-e2e-plan.json`) and those of the focused run types, such as
//! `super`, `sabre-compat-only` and the archived backends' `archived-nightly`,
//! for every execution owner (`ci/execution-owners.json`).
//!
//! Two written rules order the backends. Each gives a backend one
//! prerequisite backend ([`prerequisite`]):
//!
//! - **ptrace first** (`tests/e2e/manifests/README.md`, "Matrix symmetry and
//!   the test front door"): any active mode must include ptrace, so the
//!   reference behavior is established before another backend ratchets it. A
//!   dbt, kvm or in-guest-trap cell needs the same test and mode on ptrace.
//! - **The Subset rule** (`docs/ARCHITECTURE.md`): a result may be enabled or
//!   credited for a patcher backend only if the same test and mode pass on
//!   in-guest-trap. A liteinst or sabre cell needs the same test and mode on
//!   in-guest-trap.
//!
//! Selecting a cell is the ratchet: the run type that selects it then requires
//! it to pass. So a selected cell whose prerequisite cell no run type selects
//! is a violation, unless [`EXCEPTIONS_PATH`] names it with the reason it stays
//! selected. That file is an exact list, like `ci/matrix-symmetry-baseline.json`:
//! a violation it does not name and an exception that names no violation both
//! fail the check, and every failure names the cell and the missing
//! prerequisite. The check only reads the selection; it never selects or
//! deselects a cell. A longer chain (liteinst, in-guest-trap, ptrace) is
//! checked one link at a time, so each missing link is reported once, on the
//! cell that needs it.
//!
//! `test-harness validate` (the gate.manifest validation node) and
//! `test-harness sync-cells` run [`check_checkout`]. `test-harness audit-ci`
//! runs it too, but no validation DAG node runs `audit-ci`.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::path::Path;

use serde::Deserialize;

use crate::runner::CellId;
use crate::runner::ManifestSet;
use crate::runner::Population;
use crate::runner::Selection;

/// The selected cells that stay selected without their prerequisite cell,
/// each with its reason.
pub const EXCEPTIONS_PATH: &str = "ci/backend-order-exceptions.json";
/// The only schema [`parse_exceptions`] accepts.
pub const EXCEPTIONS_SCHEMA: u64 = 1;

/// The rule that orders a backend after its prerequisite.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum OrderRule {
    PtraceFirst,
    Subset,
}

impl fmt::Display for OrderRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::PtraceFirst => {
                "ptrace first (tests/e2e/manifests/README.md): the reference behavior is established on ptrace before another backend ratchets it"
            }
            Self::Subset => {
                "the Subset rule (docs/ARCHITECTURE.md): a patcher backend's result counts only if the same test and mode pass on in-guest-trap"
            }
        })
    }
}

/// The backend whose cell, for the same test and mode, a cell on `backend`
/// needs selected, and the rule that requires it; `None` for ptrace, the
/// reference. A backend the order does not place is an error, so a new
/// backend takes its place here before any of its cells can be selected.
pub fn prerequisite(backend: &str) -> Result<Option<(&'static str, OrderRule)>, String> {
    match backend {
        "ptrace" => Ok(None),
        "dbt" | "kvm" | "in-guest-trap" => Ok(Some(("ptrace", OrderRule::PtraceFirst))),
        "liteinst" | "sabre" => Ok(Some(("in-guest-trap", OrderRule::Subset))),
        other => Err(format!(
            "backend {other:?} has no place in the backend order; give it a prerequisite in backend_order::prerequisite"
        )),
    }
}

/// A selected cell whose prerequisite cell is not selected.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Violation {
    pub test: String,
    pub mode: String,
    pub backend: String,
    pub prerequisite: &'static str,
    pub rule: OrderRule,
}

impl Violation {
    fn key(&self) -> (&str, &str, &str, &str) {
        (&self.test, &self.mode, &self.backend, self.prerequisite)
    }
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            test,
            mode,
            backend,
            prerequisite,
            rule,
        } = self;
        write!(
            f,
            "{test}@{backend}/{mode} is selected, but {test}@{prerequisite}/{mode} is not; {rule}"
        )
    }
}

/// Every cell of `cells` whose prerequisite cell is not among them, in
/// (test, mode, backend) order. A naked cell has no backend and no place in
/// the order.
pub fn violations<'a>(
    cells: impl IntoIterator<Item = &'a CellId>,
) -> Result<Vec<Violation>, String> {
    let selected: BTreeSet<(&str, &str, &str)> = cells
        .into_iter()
        .filter_map(|cell| {
            cell.backend
                .as_deref()
                .map(|backend| (cell.test.as_str(), cell.mode.as_str(), backend))
        })
        .collect();
    let mut found = Vec::new();
    for &(test, mode, backend) in &selected {
        let Some((prerequisite, rule)) = prerequisite(backend)? else {
            continue;
        };
        if !selected.contains(&(test, mode, prerequisite)) {
            found.push(Violation {
                test: test.to_string(),
                mode: mode.to_string(),
                backend: backend.to_string(),
                prerequisite,
                rule,
            });
        }
    }
    Ok(found)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExceptionsFile {
    schema: u64,
    /// What the file is, for a reader who opens it cold.
    about: String,
    exceptions: Vec<Exception>,
}

/// One selected cell that stays selected without its prerequisite cell.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Exception {
    pub test: String,
    pub mode: String,
    pub backend: String,
    /// Must be the backend [`prerequisite`] names for `backend`, so an
    /// exception cannot outlive a change to the order unnoticed.
    pub prerequisite: String,
    /// Why the cell stays selected without its prerequisite.
    pub reason: String,
}

impl Exception {
    fn key(&self) -> (&str, &str, &str, &str) {
        (&self.test, &self.mode, &self.backend, &self.prerequisite)
    }
}

/// The exceptions of [`EXCEPTIONS_PATH`]'s text. Refuses another schema, an
/// unknown field, an empty `about` or reason, a prerequisite the order does
/// not give the backend, and a repeated cell.
pub fn parse_exceptions(text: &str) -> Result<Vec<Exception>, String> {
    let file: ExceptionsFile =
        serde_json::from_str(text).map_err(|error| format!("{EXCEPTIONS_PATH}: {error}"))?;
    if file.schema != EXCEPTIONS_SCHEMA {
        return Err(format!(
            "{EXCEPTIONS_PATH}: schema must be {EXCEPTIONS_SCHEMA}, not {}",
            file.schema
        ));
    }
    if file.about.trim().is_empty() {
        return Err(format!("{EXCEPTIONS_PATH}: `about` is empty"));
    }
    let mut seen = BTreeSet::new();
    for (index, exception) in file.exceptions.iter().enumerate() {
        let at = format!(
            "{EXCEPTIONS_PATH}: exception {index} ({}@{}/{})",
            exception.test, exception.backend, exception.mode
        );
        if exception.reason.trim().is_empty() {
            return Err(format!("{at} gives no reason"));
        }
        match prerequisite(&exception.backend).map_err(|error| format!("{at}: {error}"))? {
            Some((expected, _)) if expected == exception.prerequisite => {}
            Some((expected, _)) => {
                return Err(format!(
                    "{at} names prerequisite {:?}, but the backend order gives {} the prerequisite {expected}",
                    exception.prerequisite, exception.backend
                ));
            }
            None => {
                return Err(format!(
                    "{at}: ptrace is the reference and has no prerequisite"
                ));
            }
        }
        if !seen.insert((&exception.test, &exception.mode, &exception.backend)) {
            return Err(format!("{at} repeats an earlier exception"));
        }
    }
    Ok(file.exceptions)
}

/// `violations` against `exceptions`: Ok with the excepted violations when the
/// exceptions name exactly the violations; otherwise Err naming every
/// violation no exception names and every exception that names no violation.
pub fn check<'a>(
    violations: &'a [Violation],
    exceptions: &[Exception],
) -> Result<Vec<&'a Violation>, String> {
    let named: BTreeMap<_, _> = exceptions
        .iter()
        .map(|exception| (exception.key(), exception))
        .collect();
    let found: BTreeSet<_> = violations.iter().map(Violation::key).collect();
    let unexcepted: Vec<_> = violations
        .iter()
        .filter(|violation| !named.contains_key(&violation.key()))
        .collect();
    let stale: Vec<_> = exceptions
        .iter()
        .filter(|exception| !found.contains(&exception.key()))
        .collect();
    if unexcepted.is_empty() && stale.is_empty() {
        return Ok(violations.iter().collect());
    }
    let mut report = format!(
        "backend order: {} selected cell(s) lack their prerequisite cell and no exception names them, and {} exception(s) in {EXCEPTIONS_PATH} name no violation",
        unexcepted.len(),
        stale.len()
    );
    for violation in &unexcepted {
        report.push_str(&format!("\n  VIOLATION {violation}"));
    }
    for exception in &stale {
        report.push_str(&format!(
            "\n  STALE EXCEPTION {}@{}/{} without {}: it names no violation now; remove it",
            exception.test, exception.backend, exception.mode, exception.prerequisite
        ));
    }
    report.push_str(&format!(
        "\nSelect the prerequisite cell once it passes, or deselect the cell, or name it in {EXCEPTIONS_PATH} with the reason it stays selected. This check never changes a selection."
    ));
    Err(report)
}

/// The population the order is checked over: the required cells of every run
/// type (`all_run_types`), for every execution owner. A cell counts as
/// selected when any run type requires it, so a ptrace cell that only the
/// `super` run type runs still counts, and a SaBRe cell, which only the
/// `archived-nightly` and `sabre-compat-only` run types run, is checked.
/// Occasional tests stay out, as they do from every run type's selection;
/// only probes beyond the plans, such as the nightly pressure sweep, add them.
pub fn required_cells(manifests: &ManifestSet) -> Result<Vec<CellId>, String> {
    Ok(manifests
        .select(&Selection {
            population: Some(Population::Required),
            all_run_types: true,
            ..Selection::default()
        })?
        .into_iter()
        .map(|cell| cell.id)
        .collect())
}

/// [`check`] over [`required_cells`] of `manifests`, against the exceptions
/// committed at [`EXCEPTIONS_PATH`] under `root`. Ok with the excepted
/// violations.
pub fn check_checkout(root: &Path, manifests: &ManifestSet) -> Result<Vec<Violation>, String> {
    let found = violations(&required_cells(manifests)?)?;
    let path = root.join(EXCEPTIONS_PATH);
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let exceptions = parse_exceptions(&text)?;
    Ok(check(&found, &exceptions)?.into_iter().cloned().collect())
}

/// The one-line summary `test-harness` prints after [`check_checkout`]
/// passes, naming every excepted cell.
pub fn summary(excepted: &[Violation]) -> String {
    let cells: Vec<String> = excepted
        .iter()
        .map(|violation| {
            format!(
                "{}@{}/{} without {}",
                violation.test, violation.backend, violation.mode, violation.prerequisite
            )
        })
        .collect();
    format!(
        "backend order: every selected cell has its prerequisite cell selected, apart from {} exception(s) in {EXCEPTIONS_PATH}{}{}",
        cells.len(),
        if cells.is_empty() { "" } else { ": " },
        cells.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(test: &str, mode: &str, backend: Option<&str>) -> CellId {
        CellId {
            test: test.into(),
            mode: mode.into(),
            backend: backend.map(Into::into),
        }
    }

    fn verify(test: &str, backend: &str) -> CellId {
        cell(test, "verify", Some(backend))
    }

    fn exception(test: &str, backend: &str, prerequisite: &str) -> Exception {
        Exception {
            test: test.into(),
            mode: "verify".into(),
            backend: backend.into(),
            prerequisite: prerequisite.into(),
            reason: "why".into(),
        }
    }

    fn file(exceptions: &str) -> String {
        format!(r#"{{"schema": 1, "about": "test", "exceptions": [{exceptions}]}}"#)
    }

    fn named(found: &[Violation]) -> Vec<String> {
        found
            .iter()
            .map(|violation| {
                format!(
                    "{}@{}/{} without {}",
                    violation.test, violation.backend, violation.mode, violation.prerequisite
                )
            })
            .collect()
    }

    #[test]
    fn the_order_places_every_backend_after_its_prerequisite() {
        assert_eq!(prerequisite("ptrace"), Ok(None));
        for backend in ["dbt", "kvm", "in-guest-trap"] {
            assert_eq!(
                prerequisite(backend),
                Ok(Some(("ptrace", OrderRule::PtraceFirst))),
                "{backend}"
            );
        }
        for backend in ["liteinst", "sabre"] {
            assert_eq!(
                prerequisite(backend),
                Ok(Some(("in-guest-trap", OrderRule::Subset))),
                "{backend}"
            );
        }
        let error = prerequisite("e9patch").unwrap_err();
        assert!(error.contains("no place in the backend order"), "{error}");
    }

    #[test]
    fn a_cell_without_its_prerequisite_is_a_violation_of_its_rule() {
        let cells = [
            verify("ok", "ptrace"),
            verify("ok", "kvm"),
            verify("ok", "in-guest-trap"),
            verify("ok", "liteinst"),
            verify("kvm-only", "kvm"),
            verify("dbt-only", "dbt"),
            verify("patcher", "ptrace"),
            verify("patcher", "liteinst"),
            verify("patcher", "sabre"),
            verify("trap", "in-guest-trap"),
            // Another mode's ptrace cell is no prerequisite of this one.
            cell("other-mode", "replay", Some("ptrace")),
            verify("other-mode", "kvm"),
            // A naked cell has no backend and no place in the order.
            cell("naked", "naked", None),
        ];
        let found = violations(&cells).unwrap();
        assert_eq!(
            named(&found),
            [
                "dbt-only@dbt/verify without ptrace",
                "kvm-only@kvm/verify without ptrace",
                "other-mode@kvm/verify without ptrace",
                "patcher@liteinst/verify without in-guest-trap",
                "patcher@sabre/verify without in-guest-trap",
                "trap@in-guest-trap/verify without ptrace",
            ]
        );
        assert_eq!(found[0].rule, OrderRule::PtraceFirst);
        assert_eq!(found[3].rule, OrderRule::Subset);
        let shown = found[3].to_string();
        assert!(
            shown.starts_with(
                "patcher@liteinst/verify is selected, but patcher@in-guest-trap/verify is not; the Subset rule"
            ),
            "{shown}"
        );
    }

    #[test]
    fn a_chain_is_checked_one_link_at_a_time() {
        // liteinst has its in-guest-trap cell; that cell lacks ptrace. The
        // missing link is reported once, on the cell that needs it.
        let cells = [verify("t", "in-guest-trap"), verify("t", "liteinst")];
        assert_eq!(
            named(&violations(&cells).unwrap()),
            ["t@in-guest-trap/verify without ptrace"]
        );
    }

    #[test]
    fn an_unplaced_backend_is_refused_not_skipped() {
        let error = violations(&[verify("t", "ptrace"), verify("t", "e9patch")]).unwrap_err();
        assert!(error.contains("\"e9patch\" has no place"), "{error}");
    }

    #[test]
    fn exceptions_must_name_exactly_the_violations() {
        let found = violations(&[
            verify("kvm-only", "kvm"),
            verify("patcher", "ptrace"),
            verify("patcher", "liteinst"),
        ])
        .unwrap();
        let both = [
            exception("kvm-only", "kvm", "ptrace"),
            exception("patcher", "liteinst", "in-guest-trap"),
        ];
        assert_eq!(check(&found, &both).unwrap().len(), 2);

        let error = check(&found, &both[..1]).unwrap_err();
        assert!(
            error.contains(
                "\n  VIOLATION patcher@liteinst/verify is selected, but patcher@in-guest-trap/verify is not"
            ),
            "{error}"
        );
        assert!(!error.contains("kvm-only"), "{error}");

        let stale = [
            both[0].clone(),
            both[1].clone(),
            exception("gone", "kvm", "ptrace"),
        ];
        let error = check(&found, &stale).unwrap_err();
        assert!(
            error.contains("\n  STALE EXCEPTION gone@kvm/verify without ptrace"),
            "{error}"
        );
        assert!(!error.contains("VIOLATION"), "{error}");

        // An exception for the same cell in another mode excuses nothing.
        let mut replay = both[0].clone();
        replay.mode = "replay".into();
        let error = check(&found, &[replay, both[1].clone()]).unwrap_err();
        assert!(error.contains("VIOLATION kvm-only@kvm/verify"), "{error}");
        assert!(
            error.contains("STALE EXCEPTION kvm-only@kvm/replay"),
            "{error}"
        );

        assert!(check(&[], &[]).unwrap().is_empty());
    }

    #[test]
    fn the_exceptions_file_refuses_what_it_cannot_honor() {
        let good = r#"{"test": "t", "mode": "verify", "backend": "kvm", "prerequisite": "ptrace", "reason": "why"}"#;
        assert_eq!(
            parse_exceptions(&file(good)).unwrap(),
            [exception("t", "kvm", "ptrace")]
        );
        let refusals = [
            (
                r#"{"schema": 2, "about": "test", "exceptions": []}"#.to_string(),
                "schema must be 1",
            ),
            (
                r#"{"schema": 1, "about": " ", "exceptions": []}"#.to_string(),
                "`about` is empty",
            ),
            (
                r#"{"schema": 1, "about": "test", "exceptions": [], "extra": 1}"#.to_string(),
                "unknown field",
            ),
            (file(&good.replace("why", " ")), "gives no reason"),
            (
                file(&good.replace("\"ptrace\"", "\"in-guest-trap\"")),
                "gives kvm the prerequisite ptrace",
            ),
            (
                file(&good.replace("\"kvm\"", "\"ptrace\"")),
                "ptrace is the reference",
            ),
            (
                file(&good.replace("\"kvm\"", "\"e9patch\"")),
                "no place in the backend order",
            ),
            (
                file(&format!("{good}, {good}")),
                "repeats an earlier exception",
            ),
            (
                file(&good.replace("\"reason\"", "\"note\": 1, \"reason\"")),
                "unknown field",
            ),
        ];
        for (text, expected) in refusals {
            let error = parse_exceptions(&text).unwrap_err();
            assert!(error.contains(expected), "{text}: {error}");
        }
    }

    #[test]
    fn summary_names_every_excepted_cell() {
        let found = violations(&[
            verify("a", "kvm"),
            verify("b", "ptrace"),
            verify("b", "sabre"),
        ])
        .unwrap();
        assert_eq!(
            summary(&found),
            format!(
                "backend order: every selected cell has its prerequisite cell selected, apart from 2 exception(s) in {EXCEPTIONS_PATH}: a@kvm/verify without ptrace, b@sabre/verify without in-guest-trap"
            )
        );
        assert!(summary(&[]).ends_with(&format!("apart from 0 exception(s) in {EXCEPTIONS_PATH}")));
    }

    /// The checked population is the union of every run type's required
    /// cells: it holds all of full validation's, and beyond them SaBRe cells
    /// (archived-nightly and sabre-compat-only) and ptrace cells that only a
    /// focused run type, such as super, requires.
    #[test]
    fn the_population_is_the_union_of_every_run_type() {
        let root = crate::compiled_manifest_dir!().join("../..");
        let manifests = ManifestSet::load(&root).unwrap();
        let union: BTreeSet<CellId> = required_cells(&manifests).unwrap().into_iter().collect();
        let full: BTreeSet<CellId> = manifests
            .select(&Selection {
                population: Some(Population::Required),
                ..Selection::default()
            })
            .unwrap()
            .into_iter()
            .map(|cell| cell.id)
            .collect();
        assert!(full.is_subset(&union));
        let beyond: Vec<&CellId> = union.difference(&full).collect();
        for backend in ["sabre", "ptrace"] {
            assert!(
                beyond
                    .iter()
                    .any(|cell| cell.backend.as_deref() == Some(backend)),
                "no {backend} cell beyond full validation's: {beyond:?}"
            );
        }
    }

    /// The committed selection obeys the order, apart from exactly the
    /// committed exceptions.
    #[test]
    fn the_committed_selection_obeys_the_order_or_names_an_exception() {
        let root = crate::compiled_manifest_dir!().join("../..");
        let manifests = ManifestSet::load(&root).unwrap();
        if let Err(error) = check_checkout(&root, &manifests) {
            panic!("{error}");
        }
    }
}
