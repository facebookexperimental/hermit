//! Every cell a validation's harness ran more than once, carried into the run
//! record (<https://github.com/rrnewton/hermit/issues/1845>).
//!
//! A retry that passes makes its cell pass, so without this a divergence the
//! retry hid leaves no trace in the run record. It is an independent versioned
//! extension computed from the same raw `results.jsonl` bytes the raw input
//! census binds, and it revises no verdict.

use super::*;
use crate::runner::outcome_after_retries;

/// The cells of one run that have more than one attempt row, whatever caused
/// the retry.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FrameworkRetriesV1 {
    pub schema: u32,
    /// Cells with more than one attempt.
    pub retried_cells: u64,
    /// Of those, the cells whose history selected PASS: a failure the retry
    /// turned into a pass.
    pub retried_cells_final_pass: u64,
    pub cells: Vec<RetriedCellV1>,
}

/// One retried cell: what its first attempt observed, where it first
/// diverged, and the outcome its whole history selected.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetriedCellV1 {
    /// The `results.jsonl` path, relative to the result root, as in the census.
    pub path: String,
    pub cell: CellIdentity,
    pub attempts: u64,
    pub first_outcome: String,
    pub first_failure_class: Option<String>,
    pub first_error_kind: Option<String>,
    pub first_result: Option<String>,
    pub first_divergent_scheduler_turn: Option<u64>,
    pub first_divergent_record: Option<u64>,
    pub first_divergent_syscall: Option<u64>,
    pub final_outcome: String,
}

impl FrameworkRetriesV1 {
    /// Group each file's rows by cell, in attempt order, and keep every cell
    /// with more than one attempt. A row without an identity, a positive
    /// attempt or an outcome, or a history [`outcome_after_retries`] refuses,
    /// refuses the whole extension rather than undercounting.
    pub fn from_inputs(inputs: &BTreeMap<String, Vec<u8>>) -> Result<Self, String> {
        let mut cells = Vec::new();
        for (path, bytes) in inputs {
            let text = std::str::from_utf8(bytes)
                .map_err(|error| format!("{path}: results are not UTF-8: {error}"))?;
            let mut histories: BTreeMap<CellIdentity, Vec<(u64, Value)>> = BTreeMap::new();
            for line in text.lines().filter(|line| !line.trim().is_empty()) {
                let row: Value = serde_json::from_str(line)
                    .map_err(|error| format!("{path}: malformed result row: {error}"))?;
                let field = |name: &str| {
                    row.get(name)
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .ok_or_else(|| format!("{path}: result row omitted {name}"))
                };
                let cell = CellIdentity {
                    lane: field("lane")?,
                    category: field("category")?,
                    test: field("test")?,
                    mode: field("mode")?,
                    backend: field("backend")?,
                };
                let attempt = row
                    .get("attempt")
                    .and_then(Value::as_u64)
                    .filter(|attempt| *attempt > 0)
                    .ok_or_else(|| format!("{path}: result row omitted a positive attempt"))?;
                field("outcome")?;
                histories.entry(cell).or_default().push((attempt, row));
            }
            for (cell, mut history) in histories {
                if history.len() < 2 {
                    continue;
                }
                history.sort_by_key(|(attempt, _)| *attempt);
                let outcome = |row: &Value| row["outcome"].as_str().unwrap_or_default().to_string();
                let final_outcome = outcome_after_retries(
                    history
                        .iter()
                        .map(|(attempt, row)| (*attempt, row["outcome"].as_str().unwrap_or(""))),
                )
                .map_err(|error| format!("{path}: {} ({}): {error}", cell.test, cell.mode))?;
                let first = &history[0].1;
                let text = |name: &str| first.get(name).and_then(Value::as_str).map(str::to_string);
                let number = |name: &str| first.get(name).and_then(Value::as_u64);
                cells.push(RetriedCellV1 {
                    path: path.clone(),
                    cell,
                    attempts: history.len() as u64,
                    first_outcome: outcome(first),
                    first_failure_class: text("failure_class"),
                    first_error_kind: text("error_kind"),
                    first_result: text("result"),
                    first_divergent_scheduler_turn: number("first_divergent_scheduler_turn"),
                    first_divergent_record: number("first_divergent_record"),
                    first_divergent_syscall: number("first_divergent_syscall"),
                    final_outcome: final_outcome.to_string(),
                });
            }
        }
        Ok(Self {
            schema: 1,
            retried_cells: cells.len() as u64,
            retried_cells_final_pass: cells
                .iter()
                .filter(|cell| cell.final_outcome == "PASS")
                .count() as u64,
            cells,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(test: &str, attempt: u64, outcome: &str, diverged: bool) -> String {
        let mut row = serde_json::json!({
            "lane": "portable", "category": "fixture", "test": test, "mode": "verify",
            "backend": "ptrace", "attempt": attempt, "outcome": outcome,
        });
        if outcome == "FAIL" {
            row["failure_class"] = "product_failure".into();
            row["result"] = "determinism-failure".into();
        }
        if diverged {
            row["first_divergent_scheduler_turn"] = 4.into();
            row["first_divergent_record"] = 9.into();
            row["first_divergent_syscall"] = 2.into();
        }
        format!("{row}\n")
    }

    #[test]
    fn every_retried_cell_is_counted_with_its_first_attempt_and_selected_outcome() {
        let one = [
            row("fixture/once", 1, "PASS", false),
            // Rows of one cell need not be adjacent, nor in attempt order.
            row("fixture/rescued", 2, "PASS", false),
            row("fixture/twice", 1, "FAIL", true),
            row("fixture/rescued", 1, "FAIL", true),
            row("fixture/twice", 2, "FAIL", true),
        ]
        .concat();
        let inputs = BTreeMap::from([
            ("one/results.jsonl".to_string(), one.into_bytes()),
            ("empty/results.jsonl".to_string(), Vec::new()),
        ]);
        let retries = FrameworkRetriesV1::from_inputs(&inputs).unwrap();
        assert_eq!(
            (retries.retried_cells, retries.retried_cells_final_pass),
            (2, 1)
        );
        let rescued = &retries.cells[0];
        assert_eq!(rescued.cell.test, "fixture/rescued");
        assert_eq!(
            (
                rescued.path.as_str(),
                rescued.attempts,
                rescued.first_outcome.as_str(),
                rescued.first_failure_class.as_deref(),
                rescued.first_error_kind.as_deref(),
                rescued.first_result.as_deref(),
                rescued.first_divergent_scheduler_turn,
                rescued.first_divergent_record,
                rescued.first_divergent_syscall,
                rescued.final_outcome.as_str(),
            ),
            (
                "one/results.jsonl",
                2,
                "FAIL",
                Some("product_failure"),
                None,
                Some("determinism-failure"),
                Some(4),
                Some(9),
                Some(2),
                "PASS"
            )
        );
        assert_eq!(retries.cells[1].cell.test, "fixture/twice");
        assert_eq!(retries.cells[1].final_outcome, "FAIL");
        // The record round-trips through its strict reader.
        let value = serde_json::to_value(&retries).unwrap();
        assert_eq!(
            serde_json::from_value::<FrameworkRetriesV1>(value).unwrap(),
            retries
        );
    }

    #[test]
    fn an_unreadable_history_refuses_rather_than_undercounting() {
        for (case, rows) in [
            ("no attempt", "{\"lane\":\"portable\",\"category\":\"fixture\",\"test\":\"fixture/x\",\"mode\":\"verify\",\"backend\":\"ptrace\",\"outcome\":\"FAIL\"}\n".to_string()),
            ("attempt after a pass", [row("fixture/x", 1, "PASS", false), row("fixture/x", 2, "FAIL", false)].concat()),
            ("a gap", [row("fixture/x", 1, "FAIL", false), row("fixture/x", 3, "PASS", false)].concat()),
            ("malformed", "{".to_string()),
        ] {
            let inputs = BTreeMap::from([("one/results.jsonl".to_string(), rows.into_bytes())]);
            assert!(FrameworkRetriesV1::from_inputs(&inputs).is_err(), "{case}");
        }
    }
}
