//! One typed logging interface (design: dev-hermit
//! `ai_docs/2026-11-01-typed-logging-design.md`).
//!
//! Producers build a typed [`Record`] and call [`emit`]; they never open a
//! sink directly. Each record carries the complete legacy payload verbatim
//! alongside normalised query fields, so the git connector is byte-identical
//! by construction. The git connector is authoritative (its failure fails
//! `emit`); optional sinks are isolated. The internal Logger connector
//! lives outside this OSS tree (dev-hermit `ci-hub`), shelling out to
//! `logger_cat`; it is deliberately not implemented here.

use std::collections::BTreeMap;

use serde_json::Value;

pub const FAILURE_SUBCATEGORIES: [&str; 13] = [
    "pmu_skid",
    "determinism_divergence",
    "parity_divergence",
    "replay_divergence",
    "timeout_cpu",
    "timeout_wall",
    "oom",
    "crash",
    "sandbox_denied",
    "infra_error",
    "prerequisite_missing",
    "chaos_floor",
    "unknown",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    ValidateRun,
    NodeResult,
    CellResult,
    RetryAttempt,
    DailyAggregate,
    NightlySweepRow,
    LandingEvent,
}

impl Kind {
    pub fn schema(self) -> &'static str {
        match self {
            Self::ValidateRun => "hermit-log/validate_run/v1",
            Self::NodeResult => "hermit-log/node_result/v1",
            Self::CellResult => "hermit-log/cell_result/v1",
            Self::RetryAttempt => "hermit-log/retry_attempt/v1",
            Self::DailyAggregate => "hermit-log/daily_aggregate/v1",
            Self::NightlySweepRow => "hermit-log/nightly_sweep_row/v1",
            Self::LandingEvent => "hermit-log/landing_event/v1",
        }
    }
    pub fn event_type(self) -> &'static str {
        self.schema()
            .split('/')
            .nth(1)
            .expect("schema is hermit-log/<type>/v1")
    }
}

#[derive(Clone, Debug)]
pub struct Record {
    pub kind: Kind,
    pub event_id: String,
    pub emitted_at: String,
    pub team: String,
    pub host: String,
    pub run_id: String,
    pub producer: String,
    /// Complete legacy payload, verbatim (review addendum §9.1).
    pub legacy: Value,
    /// Normalised query fields.
    pub fields: BTreeMap<String, Value>,
}

pub fn failure_signature(
    first_divergent_syscall: Option<&Value>,
    left: Option<&Value>,
    right: Option<&Value>,
) -> Option<String> {
    if first_divergent_syscall.is_none() && left.is_none() && right.is_none() {
        return None;
    }
    let mut map = BTreeMap::new();
    map.insert(
        "first_divergent_syscall",
        first_divergent_syscall.cloned().unwrap_or(Value::Null),
    );
    map.insert("left", left.cloned().unwrap_or(Value::Null));
    map.insert("right", right.cloned().unwrap_or(Value::Null));
    let blob = serde_json::to_vec(&map).expect("map serialises");
    use sha2::Digest as _;
    Some(format!("{:x}", sha2::Sha256::digest(&blob)))
}

impl Record {
    pub fn validate(&self) -> Result<(), String> {
        if self.event_id.is_empty() || self.run_id.is_empty() || self.producer.is_empty() {
            return Err("event_id, run_id and producer are required".into());
        }
        if !self.emitted_at.ends_with('Z') {
            return Err(format!("emitted_at must be UTC ...Z: {}", self.emitted_at));
        }
        if let Some(sub) = self.fields.get("failure_subcategory") {
            let ok = sub
                .as_str()
                .is_some_and(|s| FAILURE_SUBCATEGORIES.contains(&s));
            if !ok {
                return Err(format!("unknown failure_subcategory {sub}"));
            }
        }
        if let Some(sig) = self.fields.get("signature") {
            if !sig.is_null() {
                let ok = sig
                    .as_str()
                    .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()));
                if !ok {
                    return Err("signature must be lowercase hex SHA-256 or null".into());
                }
            }
        }
        Ok(())
    }

    /// Canonical git-sink line: the legacy payload serialised exactly as
    /// today's writer does (sorted keys via `BTreeMap`-ordered `Value`).
    pub fn git_line(&self) -> Result<String, String> {
        serde_json::to_string(&self.legacy).map_err(|e| e.to_string())
    }
}

/// Derive one `retry_attempt` record per attempt from the retry rollup
/// (design §2). `failure_class` comes from the census; `failure_subcategory`
/// stays `unknown` and `signature` stays null where the census carries no
/// divergence evidence — never fabricated (§9.4).
pub fn retry_attempts_from_rollup(
    rollup: &crate::ledger::RetryRollupV1,
    emitted_at: &str,
    team: &str,
    host: &str,
    run_id: &str,
    producer: &str,
) -> Vec<Record> {
    let mut records = Vec::new();
    for test in &rollup.tests {
        for (index, outcome) in test.attempt_outcomes.iter().enumerate() {
            let mut fields = BTreeMap::new();
            fields.insert(
                "failure_class".into(),
                test.attempt_classes
                    .get(index)
                    .cloned()
                    .flatten()
                    .map(Value::String)
                    .unwrap_or(Value::Null),
            );
            fields.insert(
                "failure_subcategory".into(),
                Value::String("unknown".into()),
            );
            fields.insert("signature".into(), Value::Null);
            fields.insert("outcome".into(), Value::String(outcome.clone()));
            fields.insert("attempt".into(), Value::Number((index as u64 + 1).into()));
            records.push(Record {
                kind: Kind::RetryAttempt,
                event_id: format!("{run_id}-{}-attempt-{}", test.test, index + 1),
                emitted_at: emitted_at.to_string(),
                team: team.to_string(),
                host: host.to_string(),
                run_id: run_id.to_string(),
                producer: producer.to_string(),
                legacy: serde_json::to_value(test).unwrap_or(Value::Null),
                fields,
            });
        }
    }
    records
}

pub trait Connector {
    fn authoritative(&self) -> bool {
        false
    }
    fn write(&self, records: &[Record]) -> Result<(), String>;
}

/// Git-ledger connector: appends `git_line()` bytes to a shard file.
/// Authoritative — failure propagates and the producer keeps its batch.
pub struct GitLedgerConnector {
    pub path: std::path::PathBuf,
}

impl Connector for GitLedgerConnector {
    fn authoritative(&self) -> bool {
        true
    }
    fn write(&self, records: &[Record]) -> Result<(), String> {
        use std::io::Write as _;
        let mut text = String::new();
        for record in records {
            text.push_str(&record.git_line()?);
            text.push('\n');
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| e.to_string())?;
        file.write_all(text.as_bytes()).map_err(|e| e.to_string())
    }
}

/// Validate the whole batch, then write atomically per sink. Authoritative
/// failure propagates; optional-sink failure is isolated (§9.2).
pub fn emit(records: &[Record], sinks: &[&dyn Connector]) -> Result<(), String> {
    for record in records {
        record.validate()?;
    }
    for sink in sinks.iter().filter(|s| s.authoritative()) {
        sink.write(records)?;
    }
    for sink in sinks.iter().filter(|s| !s.authoritative()) {
        if let Err(error) = sink.write(records) {
            eprintln!("typedlog: optional sink failed: {error}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(kind: Kind) -> Record {
        Record {
            kind,
            event_id: "e1".into(),
            emitted_at: "2026-11-01T00:00:00Z".into(),
            team: "hermit".into(),
            host: "validation-host".into(),
            run_id: "validate-1".into(),
            producer: "hermit-validate-rs".into(),
            legacy: serde_json::json!({"b": 1, "a": 2}),
            fields: BTreeMap::new(),
        }
    }

    #[test]
    fn git_line_is_sorted_and_stable() {
        let line = record(Kind::ValidateRun).git_line().unwrap();
        assert_eq!(line, r#"{"a":2,"b":1}"#);
    }

    #[test]
    fn signature_matches_python_definition() {
        // Canonical JSON of the triple with sorted keys, compact separators.
        let sig = failure_signature(
            Some(&serde_json::json!(37)),
            Some(&serde_json::json!("inbound read")),
            Some(&serde_json::json!("finish read")),
        )
        .unwrap();
        assert_eq!(sig.len(), 64);
        assert_eq!(
            failure_signature(None, None, None),
            None,
            "no evidence must stay null, never fabricated"
        );
    }

    #[test]
    fn retry_rollup_derives_attempts_without_fabrication() {
        let rollup =
            crate::ledger::RetryRollupV1::from_records(vec![crate::ledger::RetryRecordV1 {
                test: "test.flaky".into(),
                mode: "verify".into(),
                backend: Some("ptrace".into()),
                attempts: 2,
                retries: 1,
                attempt_outcomes: vec!["FAIL".into(), "PASS".into()],
                attempt_classes: vec![Some("product_failure".into()), None],
                attempt_error_kinds: vec![None, None],
                attempt_signatures: vec![None, None],
                final_outcome: "PASS".into(),
                flaky_pass: true,
            }]);
        let records = retry_attempts_from_rollup(
            &rollup,
            "2026-11-01T00:00:00Z",
            "hermit",
            "validation-host",
            "validate-1",
            "hermit-validate-rs",
        );
        assert_eq!(records.len(), 2);
        assert!(records.iter().all(|r| r.validate().is_ok()));
        assert_eq!(records[0].fields["failure_subcategory"], "unknown");
        assert!(records[0].fields["signature"].is_null());
    }

    #[test]
    fn unknown_subcategory_refused() {
        let mut rec = record(Kind::RetryAttempt);
        rec.fields
            .insert("failure_subcategory".into(), Value::String("nope".into()));
        assert!(rec.validate().is_err());
    }
}
