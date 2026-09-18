//! Validation ledger schema emitted by Hermit's validation driver and consumed
//! by ci-hub.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

use dagrun::model::Step;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;

use crate::runner::FailureClass;

mod schema10;
pub use schema10::*;

mod admission;
pub use admission::*;

/// The stable fields emitted by `validate/aggregate.py --json` and JSONL stores.
/// Optional fields reflect honest reconstructed rows where a measurement was not
/// available; unrecognized fields are retained for forward compatibility.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HistoryRow {
    #[serde(default)]
    pub schema_version: Option<u32>,
    /// Stable validation-run identity. Rows whose schema supports cell results
    /// require this separately from `record_id`, because finalization mints a
    /// correction record while preserving the run and its per-cell evidence.
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub finished_at: Option<String>,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub slot: Option<String>,
    /// Product the run validated: "hermit" or "reverie". Absent on pre-`repo`
    /// hermit ledger rows (aggregate.py defaults those to "hermit").
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub selection_mode: Option<String>,
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default)]
    pub commit_anchored: Option<bool>,
    #[serde(default)]
    pub tree_dirty: Option<bool>,
    #[serde(default)]
    pub result: Option<String>,
    /// Process exit from the validation driver. A signal/cancellation exit can
    /// truncate a run after only passing gates; it is not a product failure.
    #[serde(default)]
    pub exit_code: Option<i32>,
    /// Tests EXECUTED per the run's structured producer records. `None` =
    /// unknown (no usable record), distinct from `Some(0)` = a demonstrably
    /// inert green (a `--features`-gated build that compiled the tests out). A
    /// green carries this so a reader — and the landing predicate — can tell a
    /// real pass from a no-result wearing a success badge.
    #[serde(default)]
    pub executed_tests: Option<i64>,
    /// Tests that passed according to the runner-owned structured result.
    /// This field is omitted when absent so retained receipt bytes and digests
    /// stay unchanged. A row without the field remains readable as `None` and
    /// gains no value reconstructed from presentation text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passed_tests: Option<i64>,
    /// Tests FILTERED OUT by the run's selection. `Some(0)` alongside
    /// `executed_tests == Some(0)` is an empty target; `Some(n>0)` is a filtered
    /// subset (the `1 passed; 154 filtered out` narrowed-scope trap).
    #[serde(default)]
    pub filtered_tests: Option<i64>,
    /// Per-DAG-node test-coverage obligation outcome, written by the producer
    /// (Hermit's Rust driver) from structured per-node counts and terminal outcomes. Carries
    /// the CONDITION with the value (Proxy Binding): the NAMES of any inert or
    /// absent nodes travel in the receipt, so the landing predicate decides from
    /// receipt fields alone and never re-reads a log. `None` on a pre-coverage
    /// receipt; a count-capable receipt (schema >= COUNTS_SCHEMA) that omits it is
    /// a writer defect. Supersedes the blunt `filtered_tests == 0` predicate.
    #[serde(default)]
    pub coverage: Option<CoverageRow>,
    /// Whether the run covered the FULL profile (`level == "full"`), not a
    /// partial `*-only` profile whose pass reads identically to a full green.
    #[serde(default)]
    pub full_coverage: Option<bool>,
    /// Historical gate-record count. A scheduler record may carry
    /// `execution = "unknown"`; this field therefore must not be presented as
    /// a count of nodes whose child process executed. Use [`Self::executed_nodes`]
    /// for that measurement on current rows.
    #[serde(default)]
    pub checks: Option<u64>,
    /// Explicit completed/expected outer-gate counts. `checks` is retained for
    /// old rows; `gates_run` counts entries in `gates`, including an explicit
    /// unknown-execution row, so completeness is observable rather than inferred
    /// from a profile name. It is not an executed-node count.
    #[serde(default)]
    pub gates_run: Option<u64>,
    #[serde(default)]
    pub gates_expected: Option<u64>,
    #[serde(default)]
    pub failures: Option<u64>,
    /// `-j` passed to the CI DAG lane, and the number of other validates
    /// observed concurrently. A failure without these conditions cannot be
    /// promoted to a durable known-bad verdict.
    #[serde(default)]
    pub dag_jobs: Option<u64>,
    #[serde(default)]
    pub concurrent_validates: Option<u64>,
    /// The failed cell was in the measured-flaky registry at run time. Such a
    /// red needs a solo `-j 4` confirmation before it is durable.
    #[serde(default)]
    pub known_flaky_failure: Option<bool>,
    /// This row is the required solo `-j 4` confirmation of an earlier
    /// contended or known-flaky red.
    #[serde(default)]
    pub solo_rerun_confirmation: Option<bool>,
    #[serde(default)]
    pub real_seconds: Option<f64>,
    #[serde(default)]
    pub user_seconds: Option<f64>,
    #[serde(default)]
    pub sys_seconds: Option<f64>,
    #[serde(default)]
    pub log_file: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    /// Receipt-bound per-cell comparison evidence. Historical rows predate this
    /// field and remain readable as `None`. Keep an unrecognized newer value as
    /// JSON so the outer `schema_version` can be consulted before deciding which
    /// typed shape applies; supported schemas still require a complete value.
    #[serde(default)]
    pub cell_results: Option<CellResultsValue>,
    /// Schema-9 producer-owned terminal test results. Keep the raw value until
    /// the enclosing row version is known so schemas 6, 7, 8, and future rows
    /// retain their established serialization and receive no accidental
    /// authority from a shape that happens to parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_results: Option<TestResultsValue>,
    #[serde(default)]
    pub gates: Vec<GateHistoryRow>,
    /// `tree` deliberately remains in this map even though it has a shared
    /// accessor below. Receipt identity uses
    /// `serde_json::to_vec(HistoryRow)-v1`, whose bytes depend on struct-field
    /// order followed by this `BTreeMap`'s key order. Moving `tree` into an
    /// ordinary struct field would change existing receipt digests without
    /// changing their meaning.
    #[serde(flatten, deserialize_with = "admission::deserialize_extensions")]
    pub extra: BTreeMap<String, Value>,
}

impl HistoryRow {
    /// Return the Git tree recorded for this validation row.
    ///
    /// Absence is valid for historical rows. A present value is either one
    /// forty-hex Git object ID or a malformed row; callers must not turn the
    /// malformed case into absence.
    pub fn tree(&self) -> Result<Option<&str>, &'static str> {
        let Some(value) = self.extra.get("tree") else {
            return Ok(None);
        };
        let Some(tree) = value.as_str() else {
            return Err("malformed HistoryRow tree: expected a string");
        };
        if tree.len() != 40 || !tree.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("malformed HistoryRow tree: expected exactly 40 hexadecimal characters");
        }
        Ok(Some(tree))
    }

    /// Return how many retry rounds the validation runner executed.
    ///
    /// The field deliberately remains in [`Self::extra`], like `tree`, because
    /// moving an extension into the ordered struct fields would change the
    /// canonical bytes and receipt digest of every retained row. Historical
    /// `env_block_retries` rows are not treated as this value: that older name
    /// described a narrower population, while current retries include every
    /// retry-eligible failure.
    pub fn retry_rounds(&self) -> Result<Option<u64>, &'static str> {
        let Some(value) = self.extra.get("retry_rounds") else {
            return Ok(None);
        };
        value
            .as_u64()
            .map(Some)
            .ok_or("malformed HistoryRow retry_rounds: expected a nonnegative integer")
    }

    /// Decode cell-results evidence only after the enclosing schema version is
    /// known. Schema 6 and 7 retain their historical type; schema 8 introduced
    /// the exact versioned shape that schema 9 carries forward unchanged while
    /// adding separate test-results evidence. Newer schemas remain readable but
    /// receive no typed evidence authority.
    pub fn cell_results_evidence(&self) -> Option<Cow<'_, CellResultsEvidence>> {
        let value = self.cell_results.as_ref()?;
        match self.schema_version? {
            6 | 7 => value.typed().map(Cow::Borrowed),
            8 | 9 => value
                .schema8()
                .map(|evidence| Cow::Owned(evidence.into_evidence())),
            _ => None,
        }
    }

    /// Return the recorded validation path for schema 8 or its schema-9
    /// extension, both of which use the same cell-results shape.
    pub fn cell_results_validate_path(&self) -> Option<ValidatePath> {
        if !matches!(self.schema_version, Some(8) | Some(9)) {
            return None;
        }
        self.cell_results
            .as_ref()?
            .schema8()
            .map(|evidence| evidence.path)
    }

    /// Decode and validate producer-owned per-test evidence only for schema 9.
    ///
    /// Artifact bytes are verified by the receipt consumer. This reader proves
    /// everything available from the ledger row itself: exact row identity,
    /// validation path, selected producer population, canonical summaries,
    /// checked totals, and the content-addressed artifact reference.
    pub fn test_results_evidence(&self) -> Result<Option<TestResultsEvidenceV9>, String> {
        if self.schema_version != Some(9) {
            return Ok(None);
        }
        let value = self
            .test_results
            .as_ref()
            .ok_or_else(|| "schema 9 row omitted test_results".to_string())?;
        let evidence = value.schema9()?;
        evidence.validate_for_row(self)?;
        Ok(Some(evidence))
    }

    /// Verify the exact canonical JSONL bytes bound by schema-9 test evidence.
    ///
    /// The producer population is derived here from the constructed validation
    /// plan's declared result manifests (plus its explicit compatibility
    /// producer), never from the artifact rows. Requiring the plan here
    /// prevents a missing producer from shrinking both the artifact and its
    /// self-reported denominator.
    ///
    /// Historical and future schemas remain readable but receive no schema-9
    /// artifact authority.
    pub fn verify_test_results_artifact_bytes(
        &self,
        constructed_plan_steps: &[Step],
        compatibility_selected: bool,
        artifact_bytes: &[u8],
    ) -> Result<Option<VerifiedTestResultsArtifactV9>, String> {
        let Some(evidence) = self.test_results_evidence()? else {
            return Ok(None);
        };
        let planned_selected = TestResultsSelectedPopulation::from_constructed_plan_steps(
            constructed_plan_steps,
            compatibility_selected,
        )?;
        evidence
            .verify_artifact_bytes(&planned_selected, artifact_bytes)
            .map(Some)
    }

    /// Return the number of nodes for which at least one child execution
    /// produced an exit status.
    ///
    /// This stays in [`Self::extra`] to preserve existing receipt bytes. Missing
    /// means older evidence did not carry the measurement; it is not zero.
    pub fn executed_nodes(&self) -> Result<Option<u64>, &'static str> {
        let Some(value) = self.extra.get("executed_nodes") else {
            return Ok(None);
        };
        value
            .as_u64()
            .map(Some)
            .ok_or("malformed HistoryRow executed_nodes: expected a nonnegative integer")
    }
}

/// Per-DAG-node test-coverage obligation outcome. Current producers compute this
/// from dagrun's structured per-step test-count files and terminal outcomes,
/// crossed against the PLANNED set of `test.*` DAG nodes in the lane manifests.
/// Historical rows were reconstructed from human-readable banners. The
/// obligation is SATISFIED iff
/// `planned_test_nodes > 0` and both failure lists were reported and empty.
/// This is the per-node replacement for the blunt aggregate `filtered_tests == 0`
/// predicate, which could not distinguish a full run's legitimate cross-shard
/// filtering (~693 tests) from a narrowed-subset masquerade.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct CoverageRow {
    /// Decode-only identity retained for admission row binding. Historical
    /// receipt serialization intentionally omitted this newer extension;
    /// keeping it out of serialization preserves those exact canonical bytes.
    /// Duplicate occurrences are retained as a non-string array so admission
    /// validation refuses them, while historical rows remain readable.
    #[serde(skip_serializing)]
    pub admission_run_id: Option<Value>,
    /// Test-bearing DAG nodes the run PLANNED (manifest `test.*` steps for the
    /// lanes actually run). `0` = the producer could not determine a planned set;
    /// never treated as a satisfied obligation.
    #[serde(default)]
    pub planned_test_nodes: u64,
    /// Planned test nodes that supplied a positive structured executed-test
    /// count. Diagnostic.
    #[serde(default)]
    pub executed_test_nodes: u64,
    /// NAMES of planned test nodes that supplied a structured zero executed-test
    /// count — an inert green (every crate filtered-to-empty or compiled-out).
    ///
    /// `None` = THE PRODUCER DID NOT REPORT THIS LIST, which is not the same as
    /// reporting an empty one. It must never satisfy the obligation: an absent
    /// list is unknown, and unknown is refused. This was `Vec<String>` with
    /// `#[serde(default)]`, so a receipt that simply omitted the field
    /// deserialized to `[]` and read as "no inert nodes" — a pass. Python's
    /// `cov.get("zero_executed_nodes") == []` is `False` for a missing key and
    /// therefore already refused it, so the two verifiers disagreed on exactly
    /// this input, with Rust the permissive one.
    #[serde(default)]
    pub zero_executed_nodes: Option<Vec<String>>,
    /// NAMES of planned test nodes that produced no usable structured count —
    /// never ran, were aborted, or ran without the required producer record.
    ///
    /// `None` = not reported; see [`Self::zero_executed_nodes`]. Refused, never
    /// treated as "no absent nodes".
    #[serde(default)]
    pub absent_nodes: Option<Vec<String>>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct GateHistoryRow {
    pub name: String,
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub real_seconds: Option<f64>,
    /// For a test function extracted from a retained DAG log, the manifest node
    /// which emitted it. Ledger-native outer gates leave this unset.
    #[serde(default)]
    pub source_node: Option<String>,
    /// `outer_gate` when the named gate itself failed; `lane_substep` when the
    /// outer gate merely carried a failing DAG node.
    #[serde(default)]
    pub failure_origin: Option<String>,
    /// Canonical DAG node names for a lane-carried failure.
    ///
    /// Missing means the producer did not bind substep evidence. `Some([])` is
    /// a positive assertion that an `outer_gate` failure has no failing child
    /// nodes, while `Some(nonempty)` is required for `lane_substep`. A present
    /// JSON `null` is malformed rather than another spelling of missing.
    #[serde(
        default,
        deserialize_with = "deserialize_present_failed_substeps",
        skip_serializing_if = "Option::is_none"
    )]
    pub failed_substeps: Option<Vec<String>>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl GateHistoryRow {
    /// Return the producer's terminal failure attribution for this gate.
    ///
    /// The value stays in `extra` so retained receipt bytes do not change merely
    /// because the shared reader learned its meaning. Missing is valid for
    /// historical rows. An unknown present value is malformed, not absence; a
    /// future schema therefore remains deserializable and earns no authority
    /// until its failure class is understood.
    pub fn failure_class(&self) -> Result<Option<FailureClass>, String> {
        let Some(value) = self.extra.get("failure_class") else {
            return Ok(None);
        };
        let Some(value) = value.as_str() else {
            return Err("malformed GateHistoryRow failure_class: expected a string".into());
        };
        FailureClass::parse(value)
            .map(Some)
            .map_err(|error| format!("malformed GateHistoryRow failure_class: {error}"))
    }
}

fn deserialize_present_failed_substeps<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    Vec::<String>::deserialize(deserializer).map(Some)
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ComparisonStrictness {
    Stripped,
    Canonical,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ComparedLogScope {
    Deterministic,
    Info,
    FullTrace,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ComparisonTier {
    CanonicalBitwise,
    ExitAndStreamEquality,
    ExecutionOnlySelfConsistent,
    DeclaredButUnverifiable,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ComparisonSpec {
    pub strictness: ComparisonStrictness,
    // Added by the current Hermit verifier; absent on older schema-6 rows.
    #[serde(default)]
    pub display_name: Option<String>,
    pub compare_logs: bool,
    // Added by the current Hermit verifier; absent on older schema-6 rows.
    #[serde(default)]
    pub compare_io_buffers: Option<bool>,
    // Current canonical reports bind the complete record envelope.
    #[serde(default)]
    pub record_envelope: Option<String>,
    // A match without virtual time is replay evidence, not a determinism result.
    #[serde(default)]
    pub virtualize_time: Option<bool>,
    pub log_scope: ComparedLogScope,
    pub strip_lines: bool,
    pub canonicalize_addresses: bool,
    pub full_trace: bool,
    pub exact_remainder: bool,
    pub stripped_prefixes: Vec<String>,
    pub canonicalizations: Vec<String>,
    pub ignore_lines: bool,
    pub skip_commit: bool,
    pub skip_detlog: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ComparedLogCounts {
    pub left: u64,
    pub right: u64,
}

/// A required key whose JSON value may be null. A plain `Option<T>` would make
/// an omitted key deserialize exactly like an explicit null, recreating the
/// evidence collapse this schema exists to prevent.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum RequiredNullable<T> {
    Null,
    Value(T),
}

impl ComparisonSpec {
    /// Whether this is the complete canonical BitwiseInfoV1 comparison used by
    /// a `canonical-bitwise` cell verdict.
    ///
    /// Keep this check with the shared wire type. The validation producer and
    /// receipt readers must not maintain separate lists of comparison fields
    /// and then disagree about whether a cell carries usable evidence.
    pub fn is_canonical_bitwise_info_v1(
        &self,
        compared_log_messages: &RequiredNullable<ComparedLogCounts>,
    ) -> bool {
        self.is_canonical_bitwise_info_v1_for_time_policy(true, compared_log_messages)
    }

    /// Whether this is the complete canonical BitwiseInfoV1 comparison for the
    /// producer's declared virtual-time policy.
    ///
    /// Independent verify and chaos runs require virtual time. Record/replay
    /// deliberately compares one recording with its replay without virtual
    /// time, so receipt readers must bind the expected value to the cell mode
    /// rather than either deleting this field or accepting both values.
    pub fn is_canonical_bitwise_info_v1_for_time_policy(
        &self,
        expected_virtualize_time: bool,
        compared_log_messages: &RequiredNullable<ComparedLogCounts>,
    ) -> bool {
        let RequiredNullable::Value(counts) = compared_log_messages else {
            return false;
        };
        self.strictness == ComparisonStrictness::Canonical
            && self.display_name.as_deref() == Some("BitwiseInfoV1")
            && self.compare_logs
            && self.compare_io_buffers == Some(true)
            && self.record_envelope.as_deref() == Some("all_records_v1")
            && self.virtualize_time == Some(expected_virtualize_time)
            && self.log_scope == ComparedLogScope::Info
            && !self.strip_lines
            && self.canonicalize_addresses
            && self.full_trace
            && self.exact_remainder
            && self.stripped_prefixes == ["real-wall-clock-prefix/v1"]
            && self.canonicalizations == ["host-address-to-first-appearance-ordinal/v1"]
            && !self.ignore_lines
            && !self.skip_commit
            && !self.skip_detlog
            && counts.left > 0
            && counts.right > 0
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "kebab-case", deny_unknown_fields)]
pub enum CellVerdict {
    ComparedAndMatched {
        comparison_tier: ComparisonTier,
        comparison: ComparisonSpec,
        bitwise_parity: bool,
        compared_log_messages: RequiredNullable<ComparedLogCounts>,
    },
    ComparedAndDiverged {
        comparison_tier: ComparisonTier,
        comparison: ComparisonSpec,
        bitwise_parity: bool,
        compared_log_messages: RequiredNullable<ComparedLogCounts>,
    },
    PerformsNoComparisonByDesign {
        comparison_tier: ComparisonTier,
        reason: String,
    },
    UnavailableWithReason {
        comparison_tier: ComparisonTier,
        reason: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct CellIdentity {
    pub lane: String,
    pub category: String,
    pub test: String,
    pub mode: String,
    pub backend: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CellResult {
    pub lane: String,
    pub category: String,
    pub test: String,
    pub mode: String,
    pub backend: String,
    pub cell_verdict: CellVerdict,
    /// The exact series event this verdict's evidence was selected from.
    ///
    /// FORWARD-ONLY AND DELIBERATELY ABSENT ON OLD ROWS. Every row written
    /// before this field existed is UNBOUND, and `None` is how it says so --
    /// there is no inference from timestamps, ordering, result equality or a
    /// guessed retry that could recover the identity, and every one of those
    /// would manufacture a foreign key rather than record one. A reader must
    /// render "unbound" and must not count such a row as resolved.
    ///
    /// A NEW row omitting it would read exactly like an old one, so absence is
    /// fail-open by itself; `require_bound_compared_cells` is the guard that
    /// closes it, and the producer is required to pass that guard.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_binding: Option<CellEvidenceBinding>,
}

impl CellResult {
    pub fn identity(&self) -> CellIdentity {
        CellIdentity {
            lane: self.lane.clone(),
            category: self.category.clone(),
            test: self.test.clone(),
            mode: self.mode.clone(),
            backend: self.backend.clone(),
        }
    }

    /// Whether this verdict states a comparison, which is the population the
    /// binding must cover. A cell that performs no comparison by design, or
    /// that was unavailable, read no attempt and has nothing to bind.
    pub fn is_compared(&self) -> bool {
        matches!(
            self.cell_verdict,
            CellVerdict::ComparedAndMatched { .. } | CellVerdict::ComparedAndDiverged { .. }
        )
    }
}

/// The exact source ATTEMPT a terminal cell verdict was computed from.
///
/// ⚠️ THIS DELIBERATELY DOES NOT CARRY A PUBLISHED `event_id`, AND AN EARLIER
/// VERSION OF IT DID. That version predicted the identity by digesting these
/// coordinates the way the series producer does. The prediction is unsound for
/// exactly the population this type covers, and the correction is worth stating
/// rather than quietly dropping:
///
/// * the series producer derives `event_id` AFTER collapsing equal rows;
/// * collapse folds a row if and only if its payload carries no `attempt` key;
/// * a validate row gets that key if and only if it has no-verdict evidence.
///
/// A COMPARED verdict has a verdict, so it never carries the key, so it is
/// always collapsible. Two attempts with equal payloads publish ONE event at
/// `run_index: 1, last_run_index: 2` and there is no attempt-2 event. MEASURED
/// over the tracked shards: 170 published validate events fold attempts 1 and
/// 2, 152 of them compared, and **152 of 152 of the predicted `run_index: 2`
/// identities are absent from the corpus**. The earlier design named a
/// nonexistent event for every retried compared cell, self-consistently, which
/// is why self-consistency was not the safety property it looked like.
///
/// So this records what the producer actually knows -- which attempt it read --
/// and leaves resolution to a reader that holds the published rows. THE
/// RESOLUTION RULE, because it is not obvious and getting it wrong reintroduces
/// the same defect: a published event carries the half-open-free interval
/// `[run_index, last_run_index]` of the attempts it represents, so the bound
/// attempt resolves to the event whose interval CONTAINS it, not to the event
/// whose `run_index` equals it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CellEvidenceBinding {
    pub run_id: String,
    pub producer: String,
    /// `test/mode/backend`. The series cell key deliberately omits lane and
    /// category, so this is NOT the enclosing `CellIdentity` rendered.
    pub series_cell: String,
    /// The Hermit tree the bound attempt executed at.
    pub tree: String,
    /// The attempt ordinal the verdict was computed from. For the validation
    /// driver this is also the pre-collapse `run_index` the series producer
    /// would assign, but it is NOT the published `run_index` when the row was
    /// folded -- see the resolution rule above.
    pub selected_attempt: u64,
}

/// The series producer name the validation driver emits under.
pub const VALIDATE_SERIES_PRODUCER: &str = "validate";

impl CellEvidenceBinding {
    /// The series cell key for a five-part cell identity.
    ///
    /// `naked` cells with no backend are published as `native`, matching the
    /// series producer. Every other mode requires the backend it was given.
    pub fn series_cell_key(identity: &CellIdentity) -> String {
        let backend = if identity.backend.is_empty() && identity.mode == "naked" {
            "native"
        } else {
            identity.backend.as_str()
        };
        format!("{}/{}/{}", identity.test, identity.mode, backend)
    }

    /// Bind a COMPARED terminal verdict produced by the validation driver to
    /// the attempt it read.
    pub fn for_validate_compared(
        run_id: &str,
        identity: &CellIdentity,
        tree: &str,
        selected_attempt: u64,
    ) -> Self {
        Self {
            run_id: run_id.to_string(),
            producer: VALIDATE_SERIES_PRODUCER.to_string(),
            series_cell: Self::series_cell_key(identity),
            tree: tree.to_string(),
            selected_attempt,
        }
    }

    /// Reproduce the series producer's event identity for one PUBLISHED row's
    /// coordinates.
    ///
    /// ⚠️ READER-SIDE ONLY. Verified against the whole published corpus --
    /// 71,607 of 71,607 rows recompute from their own payload, 0 differ -- so
    /// this is exact for coordinates taken FROM a published row. It is NOT a
    /// way to predict the identity a future row will get: `run_index` after
    /// collapse is the fold's FIRST attempt, which a producer holding one
    /// attempt cannot know. Callers resolving a binding must select the
    /// published row by interval and may then digest THAT row's coordinates.
    pub fn derive_event_id(
        run_id: &str,
        producer: &str,
        series_cell: &str,
        tree: &str,
        run_index: u64,
        attempt: Option<u64>,
    ) -> String {
        // Built as an ordered list rather than a serde struct so the key order
        // is the sorted order the producer emits and stays visible here.
        let mut fields: Vec<(&str, String)> = Vec::with_capacity(6);
        if let Some(attempt) = attempt {
            fields.push(("attempt", attempt.to_string()));
        }
        fields.push(("cell", json_string(series_cell)));
        fields.push(("producer", json_string(producer)));
        fields.push(("run_id", json_string(run_id)));
        fields.push(("run_index", run_index.to_string()));
        fields.push(("tree", json_string(tree)));
        let body = fields
            .iter()
            .map(|(key, value)| format!("{}:{value}", json_string(key)))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "series-{:x}",
            Sha256::digest(format!("{{{body}}}").as_bytes())
        )
    }

    /// Whether a published row's fold interval represents this bound attempt.
    ///
    /// `last_run_index` is absent on an unfolded row, where the interval is the
    /// single `run_index`.
    pub fn is_represented_by(&self, run_index: u64, last_run_index: Option<u64>) -> bool {
        let last = last_run_index.unwrap_or(run_index);
        run_index <= self.selected_attempt && self.selected_attempt <= last
    }

    /// Refuse a binding that does not belong to the run, tree and cell the
    /// enclosing evidence claims.
    ///
    /// ⚠️ THE ATTEMPT AXIS IS NOT CHECKED HERE AND MUST NOT BE CLAIMED HERE.
    /// An earlier version took the expected attempt as a parameter and every
    /// live caller passed the binding's own value, so the comparison was
    /// between one expression and itself and could never fail -- five distinct
    /// ordinals were all accepted. The attempt is checked by
    /// `CellResultV10`'s own `selected_attempt` at decode, and independently
    /// re-derived from the attempt history for parity cells only. Ordinary
    /// cells carry a producer assertion that nothing in the ledger can refute;
    /// the end-to-end resolution against published rows is what would.
    pub fn verify_against(
        &self,
        run_id: &str,
        tree: &str,
        identity: &CellIdentity,
    ) -> Result<(), String> {
        if self.producer != VALIDATE_SERIES_PRODUCER {
            return Err(format!(
                "cell evidence binding names producer {}, not {VALIDATE_SERIES_PRODUCER}",
                self.producer
            ));
        }
        if self.run_id != run_id {
            return Err(format!(
                "cell evidence binding is for run {}, not {run_id}",
                self.run_id
            ));
        }
        if self.tree != tree {
            return Err(format!(
                "cell evidence binding is for tree {}, not {tree}",
                self.tree
            ));
        }
        let expected_cell = Self::series_cell_key(identity);
        if self.series_cell != expected_cell {
            return Err(format!(
                "cell evidence binding is for cell {}, not {expected_cell}",
                self.series_cell
            ));
        }
        if self.selected_attempt == 0 {
            return Err("cell evidence binding names attempt 0; attempts are one-based".into());
        }
        Ok(())
    }
}

/// Match the series producer's Python `json.dumps` default `ensure_ascii=True`.
/// Serde supplies JSON quote and control escapes; Python additionally escapes
/// DEL and every non-ASCII scalar as lowercase UTF-16 code units. Supplementary
/// scalars therefore need two surrogate escapes, not literal UTF-8 bytes.
fn json_string(value: &str) -> String {
    use std::fmt::Write;

    let encoded = serde_json::to_string(value).expect("a string always encodes as JSON");
    let mut ascii = String::with_capacity(encoded.len());
    for scalar in encoded.chars() {
        if scalar < '\u{7f}' {
            ascii.push(scalar);
        } else {
            let mut units = [0; 2];
            for unit in scalar.encode_utf16(&mut units) {
                write!(ascii, "\\u{unit:04x}").expect("writing into a String cannot fail");
            }
        }
    }
    ascii
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CellResultsArtifact {
    /// Parent-root-relative JSONL path below `ignored/validate/artifacts/`.
    /// The validation checkout is disposable; binding there would leave a
    /// receipt whose evidence vanishes when the unit is removed.
    pub path: String,
    pub sha256: String,
    pub row_count: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CellResultsEvidence {
    pub run_id: String,
    pub hermit_sha: String,
    pub source_tree_dirty: bool,
    pub selected_count: u64,
    pub recorded_count: u64,
    /// SHA-256 over canonical JSON of the sorted `selected` identities.
    pub population_sha256: String,
    pub artifact: CellResultsArtifact,
    pub selected: Vec<CellIdentity>,
    pub cells: Vec<CellResult>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ValidatePath {
    Quick,
    Full,
    Super,
    /// Evidence for the selected cell owner and dependencies, not the full suite.
    #[serde(rename = "cell-requalification")]
    CellRequalification,
}

impl ValidatePath {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Quick => "quick",
            Self::Full => "full",
            Self::Super => "super",
            Self::CellRequalification => "cell-requalification",
        }
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CellResultsEvidenceV8 {
    pub path: ValidatePath,
    pub run_id: String,
    pub hermit_sha: String,
    pub source_tree_dirty: bool,
    pub selected_count: u64,
    pub recorded_count: u64,
    /// SHA-256 over canonical JSON of the sorted `selected` identities.
    pub population_sha256: String,
    pub artifact: CellResultsArtifact,
    pub selected: Vec<CellIdentity>,
    pub cells: Vec<CellResult>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ComparisonSpecV8 {
    strictness: ComparisonStrictness,
    display_name: RequiredNullable<String>,
    compare_logs: bool,
    compare_io_buffers: RequiredNullable<bool>,
    record_envelope: RequiredNullable<String>,
    virtualize_time: RequiredNullable<bool>,
    log_scope: ComparedLogScope,
    strip_lines: bool,
    canonicalize_addresses: bool,
    full_trace: bool,
    exact_remainder: bool,
    stripped_prefixes: Vec<String>,
    canonicalizations: Vec<String>,
    ignore_lines: bool,
    skip_commit: bool,
    skip_detlog: bool,
}

fn required_nullable_into_option<T>(value: RequiredNullable<T>) -> Option<T> {
    match value {
        RequiredNullable::Null => None,
        RequiredNullable::Value(value) => Some(value),
    }
}

impl From<ComparisonSpecV8> for ComparisonSpec {
    fn from(value: ComparisonSpecV8) -> Self {
        Self {
            strictness: value.strictness,
            display_name: required_nullable_into_option(value.display_name),
            compare_logs: value.compare_logs,
            compare_io_buffers: required_nullable_into_option(value.compare_io_buffers),
            record_envelope: required_nullable_into_option(value.record_envelope),
            virtualize_time: required_nullable_into_option(value.virtualize_time),
            log_scope: value.log_scope,
            strip_lines: value.strip_lines,
            canonicalize_addresses: value.canonicalize_addresses,
            full_trace: value.full_trace,
            exact_remainder: value.exact_remainder,
            stripped_prefixes: value.stripped_prefixes,
            canonicalizations: value.canonicalizations,
            ignore_lines: value.ignore_lines,
            skip_commit: value.skip_commit,
            skip_detlog: value.skip_detlog,
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case", deny_unknown_fields)]
enum CellVerdictV8 {
    ComparedAndMatched {
        comparison_tier: ComparisonTier,
        comparison: ComparisonSpecV8,
        bitwise_parity: bool,
        compared_log_messages: RequiredNullable<ComparedLogCounts>,
    },
    ComparedAndDiverged {
        comparison_tier: ComparisonTier,
        comparison: ComparisonSpecV8,
        bitwise_parity: bool,
        compared_log_messages: RequiredNullable<ComparedLogCounts>,
    },
    PerformsNoComparisonByDesign {
        comparison_tier: ComparisonTier,
        reason: String,
    },
    UnavailableWithReason {
        comparison_tier: ComparisonTier,
        reason: String,
    },
}

impl From<CellVerdictV8> for CellVerdict {
    fn from(value: CellVerdictV8) -> Self {
        match value {
            CellVerdictV8::ComparedAndMatched {
                comparison_tier,
                comparison,
                bitwise_parity,
                compared_log_messages,
            } => Self::ComparedAndMatched {
                comparison_tier,
                comparison: comparison.into(),
                bitwise_parity,
                compared_log_messages,
            },
            CellVerdictV8::ComparedAndDiverged {
                comparison_tier,
                comparison,
                bitwise_parity,
                compared_log_messages,
            } => Self::ComparedAndDiverged {
                comparison_tier,
                comparison: comparison.into(),
                bitwise_parity,
                compared_log_messages,
            },
            CellVerdictV8::PerformsNoComparisonByDesign {
                comparison_tier,
                reason,
            } => Self::PerformsNoComparisonByDesign {
                comparison_tier,
                reason,
            },
            CellVerdictV8::UnavailableWithReason {
                comparison_tier,
                reason,
            } => Self::UnavailableWithReason {
                comparison_tier,
                reason,
            },
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CellResultV8 {
    lane: String,
    category: String,
    test: String,
    mode: String,
    backend: String,
    cell_verdict: CellVerdictV8,
}

impl From<CellResultV8> for CellResult {
    fn from(value: CellResultV8) -> Self {
        Self {
            lane: value.lane,
            category: value.category,
            test: value.test,
            mode: value.mode,
            backend: value.backend,
            cell_verdict: value.cell_verdict.into(),
            // Schema 8 predates the binding. Lifting an old row leaves it
            // UNBOUND rather than reconstructing an identity from the
            // coordinates around it, which is the historical inference the
            // requirement forbids.
            evidence_binding: None,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CellResultsEvidenceV8Wire {
    path: ValidatePath,
    run_id: String,
    hermit_sha: String,
    source_tree_dirty: bool,
    selected_count: u64,
    recorded_count: u64,
    population_sha256: String,
    artifact: CellResultsArtifact,
    selected: Vec<CellIdentity>,
    cells: Vec<CellResultV8>,
}

impl<'de> Deserialize<'de> for CellResultsEvidenceV8 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = CellResultsEvidenceV8Wire::deserialize(deserializer)?;
        Ok(Self {
            path: value.path,
            run_id: value.run_id,
            hermit_sha: value.hermit_sha,
            source_tree_dirty: value.source_tree_dirty,
            selected_count: value.selected_count,
            recorded_count: value.recorded_count,
            population_sha256: value.population_sha256,
            artifact: value.artifact,
            selected: value.selected,
            cells: value.cells.into_iter().map(CellResult::from).collect(),
        })
    }
}

impl CellResultsEvidenceV8 {
    fn into_evidence(self) -> CellResultsEvidence {
        CellResultsEvidence {
            run_id: self.run_id,
            hermit_sha: self.hermit_sha,
            source_tree_dirty: self.source_tree_dirty,
            selected_count: self.selected_count,
            recorded_count: self.recorded_count,
            population_sha256: self.population_sha256,
            artifact: self.artifact,
            selected: self.selected,
            cells: self.cells,
        }
    }
}

/// A supported cell-results shape, or the exact JSON from a newer shape that
/// this reader does not understand yet. Exact versioned shapes precede the raw
/// fallback so supported rows retain typed access while unknown extensions stay
/// readable.
#[derive(Clone, Debug)]
pub enum CellResultsValue {
    Typed(CellResultsEvidence),
    Other(Value),
    /// Decode-only duplicate identity state. Historical serialization and typed
    /// access delegate to `value`; admission checks must inspect the original IDs.
    #[doc(hidden)]
    WithDuplicateRunIds {
        value: Box<Self>,
        run_ids: Value,
    },
}

#[derive(Clone, Debug)]
struct RetainedResultValue {
    value: Value,
    duplicate_run_ids: Option<Value>,
}

impl RetainedResultValue {
    fn plain(value: Value) -> Self {
        Self {
            value,
            duplicate_run_ids: None,
        }
    }
}

// Retain top-level nested run-ID occurrences before Value overwrites duplicate
// keys. Only CellResultsValue had the existing binding_contract refusal; keep
// that policy separate from the newly retained, decode-only identity metadata.
fn deserialize_result_value<'de, D: Deserializer<'de>>(
    deserializer: D,
    cell_contract: bool,
) -> Result<RetainedResultValue, D::Error> {
    struct Visitor {
        cell_contract: bool,
    }
    impl<'de> serde::de::Visitor<'de> for Visitor {
        type Value = RetainedResultValue;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("result evidence retaining original run-ID occurrences")
        }

        fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(RetainedResultValue::plain(Value::Null))
        }
        fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Self::Value, E> {
            Ok(RetainedResultValue::plain(Value::Bool(value)))
        }
        fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Self::Value, E> {
            Ok(RetainedResultValue::plain(Value::from(value)))
        }
        fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Self::Value, E> {
            Ok(RetainedResultValue::plain(Value::from(value)))
        }
        fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Self::Value, E> {
            serde_json::Number::from_f64(value)
                .map(Value::Number)
                .map(RetainedResultValue::plain)
                .ok_or_else(|| E::custom("non-finite result evidence number"))
        }
        fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
            Ok(RetainedResultValue::plain(Value::String(value.into())))
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            let mut values = Vec::new();
            while let Some(value) = seq.next_element::<Value>()? {
                values.push(value);
            }
            Ok(RetainedResultValue::plain(Value::Array(values)))
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> Result<Self::Value, A::Error> {
            let mut values = serde_json::Map::new();
            let mut ids = Vec::new();
            while let Some(key) = map.next_key::<String>()? {
                if self.cell_contract && key == "binding_contract" && values.contains_key(&key) {
                    return Err(serde::de::Error::custom(
                        "duplicate field `binding_contract` in cell evidence",
                    ));
                }
                let value = map.next_value::<Value>()?;
                if key == "run_id" {
                    ids.push(value.clone());
                }
                values.insert(key, value);
            }
            Ok(RetainedResultValue {
                value: Value::Object(values),
                duplicate_run_ids: (ids.len() > 1).then_some(Value::Array(ids)),
            })
        }
    }
    deserializer.deserialize_any(Visitor { cell_contract })
}

impl<'de> Deserialize<'de> for CellResultsValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = deserialize_result_value(deserializer, true)?;
        let value = match serde_json::from_value(raw.value.clone()) {
            Ok(value) => Self::Typed(value),
            Err(_) => Self::Other(raw.value),
        };
        Ok(match raw.duplicate_run_ids {
            Some(run_ids) => Self::WithDuplicateRunIds {
                value: Box::new(value),
                run_ids,
            },
            None => value,
        })
    }
}

impl Serialize for CellResultsValue {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Typed(value) => value.serialize(serializer),
            Self::Other(value) => value.serialize(serializer),
            Self::WithDuplicateRunIds { value, .. } => value.serialize(serializer),
        }
    }
}

// Equality retains the historical typed/raw value semantics. Optional admission
// integrity is a separate check on the original row, never inferred from equality.
impl PartialEq for CellResultsValue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::WithDuplicateRunIds { value, .. }, other) => value.as_ref() == other,
            (value, Self::WithDuplicateRunIds { value: other, .. }) => value == other.as_ref(),
            (Self::Typed(value), Self::Typed(other)) => value == other,
            (Self::Other(value), Self::Other(other)) => value == other,
            _ => false,
        }
    }
}

impl CellResultsValue {
    pub fn typed(&self) -> Option<&CellResultsEvidence> {
        match self {
            Self::Typed(evidence) => Some(evidence),
            Self::Other(_) => None,
            Self::WithDuplicateRunIds { value, .. } => value.typed(),
        }
    }

    pub fn typed_mut(&mut self) -> Option<&mut CellResultsEvidence> {
        match self {
            Self::Typed(evidence) => Some(evidence),
            Self::Other(_) => None,
            Self::WithDuplicateRunIds { value, .. } => value.typed_mut(),
        }
    }

    fn schema8(&self) -> Option<CellResultsEvidenceV8> {
        match self {
            Self::Other(value) => serde_json::from_value(value.clone()).ok(),
            Self::Typed(_) => None,
            Self::WithDuplicateRunIds { value, .. } => value.schema8(),
        }
    }

    /// Original nested identity for admission checks and transient delegation.
    /// Duplicate occurrences remain an invalid array even when every ID matches.
    /// This metadata is deliberately excluded from historical serialization.
    pub fn admission_run_id(&self) -> Option<Value> {
        match self {
            Self::Typed(value) => Some(Value::String(value.run_id.clone())),
            Self::Other(value) => value.get("run_id").cloned(),
            Self::WithDuplicateRunIds { run_ids, .. } => Some(run_ids.clone()),
        }
    }
}

/// Raw test-results evidence. The enclosing [`HistoryRow::schema_version`]
/// decides whether these bytes have a supported typed interpretation.
#[derive(Clone, Debug)]
pub struct TestResultsValue(RetainedResultValue);

impl<'de> Deserialize<'de> for TestResultsValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_result_value(deserializer, false).map(Self)
    }
}

impl Serialize for TestResultsValue {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.value.serialize(serializer)
    }
}

impl PartialEq for TestResultsValue {
    fn eq(&self, other: &Self) -> bool {
        self.0.value == other.0.value
    }
}

impl TestResultsValue {
    fn schema9(&self) -> Result<TestResultsEvidenceV9, String> {
        serde_json::from_value(self.0.value.clone())
            .map_err(|error| format!("schema 9 test_results is malformed: {error}"))
    }

    /// Original nested identity, including duplicate/non-string refusal state.
    /// Callers may restore this only in a transient verifier request, not history.
    pub fn admission_run_id(&self) -> Option<Value> {
        self.0
            .duplicate_run_ids
            .clone()
            .or_else(|| self.0.value.get("run_id").cloned())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TestResultVerdict {
    Pass,
    Fail,
}

/// One producer-owned terminal test row in the retained JSONL artifact.
///
/// Canonical artifacts contain one compact serialization of this struct per
/// line, with a trailing newline. Node rows sort by `(node, id)` and precede
/// compatibility rows, which sort by `id`.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TestResultArtifactRow {
    pub run_id: String,
    pub hermit_sha: String,
    pub path: ValidatePath,
    pub producer: TestResultProducer,
    pub id: String,
    pub result: TestResultVerdict,
    pub attempts: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TestResultArtifactRowWire {
    run_id: String,
    hermit_sha: String,
    path: ValidatePath,
    producer: TestResultProducer,
    id: String,
    result: TestResultVerdict,
    attempts: u64,
}

impl<'de> Deserialize<'de> for TestResultArtifactRow {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::Error;

        let value = TestResultArtifactRowWire::deserialize(deserializer)?;
        if !nonblank_component(&value.run_id)
            || !is_lower_hex(&value.hermit_sha, 40)
            || value.id.is_empty()
            || value.id.trim() != value.id
            || value.attempts == 0
        {
            return Err(D::Error::custom(
                "schema 9 test-result artifact row has malformed identity or attempts",
            ));
        }
        if let TestResultProducer::Node {
            node,
            outer_attempt,
        } = &value.producer
        {
            if !nonblank_component(node) || *outer_attempt == 0 {
                return Err(D::Error::custom(
                    "schema 9 test-result artifact row has malformed node or outer_attempt",
                ));
            }
        }
        Ok(Self {
            run_id: value.run_id,
            hermit_sha: value.hermit_sha,
            path: value.path,
            producer: value.producer,
            id: value.id,
            result: value.result,
            attempts: value.attempts,
        })
    }
}

/// The existing two sources of structured test results remain distinct.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum TestResultProducer {
    Node { node: String, outer_attempt: u64 },
    Compatibility,
}

/// Exact count-bearing population selected before outcomes are observed.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TestResultsSelectedPopulation {
    pub nodes: Vec<String>,
    pub compatibility: bool,
}

impl TestResultsSelectedPopulation {
    /// Derive the exact count-bearing population from one already-constructed
    /// validation plan. Dependencies without a structured-result declaration
    /// are intentionally absent; every declared producer is included before
    /// any outcome or artifact row is observed.
    pub fn from_constructed_plan_steps(
        steps: &[Step],
        compatibility: bool,
    ) -> Result<Self, String> {
        let mut nodes = BTreeSet::new();
        for step in steps {
            if step.structured_test_results_manifest()?.is_some() {
                let tag = step.tag();
                if !nodes.insert(tag.clone()) {
                    return Err(format!(
                        "constructed validation plan repeats test-result producer {tag}"
                    ));
                }
            }
        }
        if nodes.is_empty() && !compatibility {
            return Err("constructed validation plan selects no test-result producers".into());
        }
        Ok(Self {
            nodes: nodes.into_iter().collect(),
            compatibility,
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TestResultTotals {
    pub executed_tests: u64,
    pub passed_tests: u64,
    pub failed_tests: u64,
    pub filtered_tests: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NodeTestResultSummary {
    pub node: String,
    pub outer_attempt: u64,
    pub totals: TestResultTotals,
    pub row_count: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CompatibilityTestResultSummary {
    pub totals: TestResultTotals,
    pub row_count: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TestResultsArtifact {
    /// Parent-root-relative producer path. Receipt publication replaces this
    /// local location with an immutable receipt-repository artifact binding.
    pub path: String,
    pub sha256: String,
    pub row_count: u64,
}

/// Schema-9 reader contract. This PR deliberately has no writer.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TestResultsEvidenceV9 {
    pub path: ValidatePath,
    pub run_id: String,
    pub hermit_sha: String,
    pub source_tree_dirty: bool,
    pub selected_count: u64,
    pub recorded_count: u64,
    pub population_sha256: String,
    pub selected: TestResultsSelectedPopulation,
    pub nodes: Vec<NodeTestResultSummary>,
    pub compatibility: Option<CompatibilityTestResultSummary>,
    pub totals: TestResultTotals,
    pub artifact: TestResultsArtifact,
}

/// Values independently reconstructed from one verified schema-9 artifact.
///
/// The filtered counts come from the ledger's per-producer summaries because
/// filtered tests do not have terminal artifact rows. Executed, passed, failed,
/// row-count, producer-population, and identity values are all recomputed from
/// the artifact bytes and checked against those summaries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct VerifiedTestResultsArtifactV9 {
    pub selected: TestResultsSelectedPopulation,
    pub nodes: Vec<NodeTestResultSummary>,
    pub compatibility: Option<CompatibilityTestResultSummary>,
    pub totals: TestResultTotals,
    pub row_count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum TestResultProducerIdentity {
    Node(String),
    Compatibility,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct TestResultArtifactRowKey {
    producer: TestResultProducerIdentity,
    id: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RecomputedTestResultCounts {
    executed_tests: u64,
    passed_tests: u64,
    failed_tests: u64,
}

impl RecomputedTestResultCounts {
    fn add(&mut self, verdict: TestResultVerdict) -> Result<(), String> {
        self.executed_tests = self
            .executed_tests
            .checked_add(1)
            .ok_or("schema 9 artifact executed_tests overflowed u64")?;
        let count = match verdict {
            TestResultVerdict::Pass => &mut self.passed_tests,
            TestResultVerdict::Fail => &mut self.failed_tests,
        };
        *count = count
            .checked_add(1)
            .ok_or("schema 9 artifact verdict count overflowed u64")?;
        Ok(())
    }
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn nonblank_component(value: &str) -> bool {
    !value.is_empty()
        && value.trim() == value
        && value != "."
        && value != ".."
        && !value.contains('/')
        && !value.contains('\\')
}

fn checked_summary(summary: &TestResultTotals, row_count: u64) -> Result<(), String> {
    if summary
        .passed_tests
        .checked_add(summary.failed_tests)
        .ok_or_else(|| "test-results pass/fail total overflowed u64".to_string())?
        != summary.executed_tests
    {
        return Err("test-results pass/fail total differs from executed_tests".into());
    }
    if row_count != summary.executed_tests {
        return Err("test-results row_count differs from executed_tests".into());
    }
    Ok(())
}

impl TestResultsEvidenceV9 {
    fn validate_for_row(&self, row: &HistoryRow) -> Result<(), String> {
        if row.profile.as_deref() != Some(self.path.as_str()) {
            return Err("schema 9 test_results path differs from row profile".into());
        }
        if !nonblank_component(&self.run_id) || row.run_id.as_deref() != Some(self.run_id.as_str())
        {
            return Err("schema 9 test_results run_id differs from row run_id".into());
        }
        if !is_lower_hex(&self.hermit_sha, 40)
            || row.commit.as_deref() != Some(self.hermit_sha.as_str())
        {
            return Err("schema 9 test_results hermit_sha differs from row commit".into());
        }
        if self.source_tree_dirty || row.tree_dirty != Some(false) {
            return Err("schema 9 test_results source is not the exact clean row tree".into());
        }
        if !self
            .selected
            .nodes
            .iter()
            .all(|node| nonblank_component(node))
            || !self.selected.nodes.windows(2).all(|pair| pair[0] < pair[1])
        {
            return Err(
                "schema 9 test_results selected nodes are empty, untrimmed, duplicate, or unsorted"
                    .into(),
            );
        }
        let selected_count = u64::try_from(self.selected.nodes.len())
            .map_err(|_| "schema 9 selected node count does not fit u64")?
            .checked_add(u64::from(self.selected.compatibility))
            .ok_or("schema 9 selected producer count overflowed u64")?;
        if self.selected_count != selected_count {
            return Err("schema 9 test_results selected_count mismatch".into());
        }
        let selected_nodes = self
            .selected
            .nodes
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        let summary_nodes = self
            .nodes
            .iter()
            .map(|summary| summary.node.as_str())
            .collect::<BTreeSet<_>>();
        if self.nodes.len() != summary_nodes.len()
            || !self
                .nodes
                .windows(2)
                .all(|pair| pair[0].node < pair[1].node)
            || selected_nodes != summary_nodes
        {
            return Err("schema 9 test_results node summaries differ from selected nodes".into());
        }
        if self.selected.compatibility != self.compatibility.is_some() {
            return Err(
                "schema 9 test_results compatibility summary differs from selection".into(),
            );
        }

        let mut totals = TestResultTotals {
            executed_tests: 0,
            passed_tests: 0,
            failed_tests: 0,
            filtered_tests: 0,
        };
        let mut recorded_count = 0u64;
        let mut add = |summary: &TestResultTotals, row_count: u64| -> Result<(), String> {
            checked_summary(summary, row_count)?;
            totals.executed_tests = totals
                .executed_tests
                .checked_add(summary.executed_tests)
                .ok_or("schema 9 executed_tests overflowed u64")?;
            totals.passed_tests = totals
                .passed_tests
                .checked_add(summary.passed_tests)
                .ok_or("schema 9 passed_tests overflowed u64")?;
            totals.failed_tests = totals
                .failed_tests
                .checked_add(summary.failed_tests)
                .ok_or("schema 9 failed_tests overflowed u64")?;
            totals.filtered_tests = totals
                .filtered_tests
                .checked_add(summary.filtered_tests)
                .ok_or("schema 9 filtered_tests overflowed u64")?;
            recorded_count = recorded_count
                .checked_add(row_count)
                .ok_or("schema 9 recorded_count overflowed u64")?;
            Ok(())
        };
        for summary in &self.nodes {
            if summary.outer_attempt == 0 {
                return Err(format!(
                    "schema 9 test_results node {} has zero outer_attempt",
                    summary.node
                ));
            }
            add(&summary.totals, summary.row_count)?;
        }
        if let Some(summary) = &self.compatibility {
            add(&summary.totals, summary.row_count)?;
        }
        if totals != self.totals
            || self.recorded_count != recorded_count
            || self.artifact.row_count != recorded_count
        {
            return Err("schema 9 test_results checked totals or row counts mismatch".into());
        }
        let executed = i64::try_from(totals.executed_tests)
            .map_err(|_| "schema 9 executed_tests does not fit ledger i64")?;
        let passed = i64::try_from(totals.passed_tests)
            .map_err(|_| "schema 9 passed_tests does not fit ledger i64")?;
        let filtered = i64::try_from(totals.filtered_tests)
            .map_err(|_| "schema 9 filtered_tests does not fit ledger i64")?;
        if row.executed_tests != Some(executed)
            || row.passed_tests != Some(passed)
            || row.filtered_tests != Some(filtered)
        {
            return Err("schema 9 test_results totals differ from HistoryRow totals".into());
        }

        let population_bytes = serde_json::to_vec(&self.selected)
            .map_err(|error| format!("cannot encode schema 9 selected population: {error}"))?;
        let population_sha256 = format!("{:x}", Sha256::digest(population_bytes));
        if !is_lower_hex(&self.population_sha256, 64) || self.population_sha256 != population_sha256
        {
            return Err("schema 9 test_results population_sha256 mismatch".into());
        }
        let expected_path = format!(
            "ignored/validate/artifacts/{}/test-results.jsonl",
            self.run_id
        );
        if self.artifact.path != expected_path || !is_lower_hex(&self.artifact.sha256, 64) {
            return Err("schema 9 test_results artifact binding is malformed".into());
        }
        Ok(())
    }

    fn verify_artifact_bytes(
        &self,
        planned_selected: &TestResultsSelectedPopulation,
        artifact_bytes: &[u8],
    ) -> Result<VerifiedTestResultsArtifactV9, String> {
        if planned_selected.nodes.is_empty() && !planned_selected.compatibility {
            return Err("schema 9 planned test-result producer population is empty".into());
        }
        if !planned_selected
            .nodes
            .iter()
            .all(|node| nonblank_component(node))
            || !planned_selected
                .nodes
                .windows(2)
                .all(|pair| pair[0] < pair[1])
        {
            return Err(
                "schema 9 planned test-result nodes are malformed, duplicate, or unsorted".into(),
            );
        }
        if &self.selected != planned_selected {
            return Err(
                "schema 9 test_results selected population differs from constructed plan".into(),
            );
        }
        if artifact_bytes.is_empty() || !artifact_bytes.ends_with(b"\n") {
            return Err(
                "schema 9 test-results artifact is empty or lacks its final newline".into(),
            );
        }

        let mut counts = BTreeMap::<TestResultProducerIdentity, RecomputedTestResultCounts>::new();
        let mut seen = BTreeSet::<TestResultArtifactRowKey>::new();
        let mut previous = None::<TestResultArtifactRowKey>;
        let mut parsed_row_count = 0u64;

        for (index, line) in artifact_bytes
            .split_inclusive(|byte| *byte == b'\n')
            .enumerate()
        {
            let row_bytes = &line[..line.len() - 1];
            if row_bytes.is_empty() {
                return Err(format!(
                    "schema 9 test-results artifact row {} is empty",
                    index + 1
                ));
            }
            let row: TestResultArtifactRow =
                serde_json::from_slice(row_bytes).map_err(|error| {
                    format!(
                        "schema 9 test-results artifact row {} is malformed: {error}",
                        index + 1
                    )
                })?;
            let mut canonical = serde_json::to_vec(&row).map_err(|error| {
                format!(
                    "cannot encode schema 9 test-results artifact row {}: {error}",
                    index + 1
                )
            })?;
            canonical.push(b'\n');
            if canonical != line {
                return Err(format!(
                    "schema 9 test-results artifact row {} is not canonical",
                    index + 1
                ));
            }
            if row.run_id != self.run_id {
                return Err(format!(
                    "schema 9 test-results artifact row {} has wrong run_id",
                    index + 1
                ));
            }
            if row.hermit_sha != self.hermit_sha {
                return Err(format!(
                    "schema 9 test-results artifact row {} has wrong hermit_sha",
                    index + 1
                ));
            }
            if row.path != self.path {
                return Err(format!(
                    "schema 9 test-results artifact row {} has wrong validation path",
                    index + 1
                ));
            }

            let producer = match &row.producer {
                TestResultProducer::Node {
                    node,
                    outer_attempt,
                } => {
                    let summary = self
                        .nodes
                        .binary_search_by(|summary| summary.node.as_str().cmp(node.as_str()))
                        .ok()
                        .map(|position| &self.nodes[position])
                        .ok_or_else(|| {
                            format!(
                                "schema 9 test-results artifact row {} names unselected node {}",
                                index + 1,
                                node
                            )
                        })?;
                    if *outer_attempt != summary.outer_attempt {
                        return Err(format!(
                            "schema 9 test-results artifact row {} has wrong outer_attempt for {}",
                            index + 1,
                            node
                        ));
                    }
                    TestResultProducerIdentity::Node(node.clone())
                }
                TestResultProducer::Compatibility => {
                    if self.compatibility.is_none() {
                        return Err(format!(
                            "schema 9 test-results artifact row {} names unselected compatibility producer",
                            index + 1
                        ));
                    }
                    TestResultProducerIdentity::Compatibility
                }
            };
            let key = TestResultArtifactRowKey {
                producer: producer.clone(),
                id: row.id.clone(),
            };
            if !seen.insert(key.clone()) {
                return Err(format!(
                    "schema 9 test-results artifact has duplicate producer/test row at row {}",
                    index + 1
                ));
            }
            if previous.as_ref().is_some_and(|previous| previous >= &key) {
                return Err(format!(
                    "schema 9 test-results artifact row {} is not in canonical order",
                    index + 1
                ));
            }
            previous = Some(key);
            counts.entry(producer).or_default().add(row.result)?;
            parsed_row_count = parsed_row_count
                .checked_add(1)
                .ok_or("schema 9 artifact row count overflowed u64")?;
        }

        let digest = format!("{:x}", Sha256::digest(artifact_bytes));
        if digest != self.artifact.sha256 {
            return Err("schema 9 test-results artifact sha256 mismatch".into());
        }
        if parsed_row_count != self.artifact.row_count || parsed_row_count != self.recorded_count {
            return Err("schema 9 test-results artifact row_count mismatch".into());
        }

        let mut verified_nodes = Vec::with_capacity(self.nodes.len());
        let mut verified_totals = TestResultTotals {
            executed_tests: 0,
            passed_tests: 0,
            failed_tests: 0,
            filtered_tests: 0,
        };
        let mut add_verified = |counts: RecomputedTestResultCounts,
                                filtered_tests: u64|
         -> Result<TestResultTotals, String> {
            let totals = TestResultTotals {
                executed_tests: counts.executed_tests,
                passed_tests: counts.passed_tests,
                failed_tests: counts.failed_tests,
                filtered_tests,
            };
            verified_totals.executed_tests = verified_totals
                .executed_tests
                .checked_add(totals.executed_tests)
                .ok_or("schema 9 verified executed_tests overflowed u64")?;
            verified_totals.passed_tests = verified_totals
                .passed_tests
                .checked_add(totals.passed_tests)
                .ok_or("schema 9 verified passed_tests overflowed u64")?;
            verified_totals.failed_tests = verified_totals
                .failed_tests
                .checked_add(totals.failed_tests)
                .ok_or("schema 9 verified failed_tests overflowed u64")?;
            verified_totals.filtered_tests = verified_totals
                .filtered_tests
                .checked_add(totals.filtered_tests)
                .ok_or("schema 9 verified filtered_tests overflowed u64")?;
            Ok(totals)
        };

        for summary in &self.nodes {
            let producer = TestResultProducerIdentity::Node(summary.node.clone());
            let recomputed = counts.remove(&producer).ok_or_else(|| {
                format!(
                    "schema 9 test-results artifact omits selected producer {}",
                    summary.node
                )
            })?;
            if recomputed.executed_tests != summary.row_count
                || recomputed.executed_tests != summary.totals.executed_tests
                || recomputed.passed_tests != summary.totals.passed_tests
                || recomputed.failed_tests != summary.totals.failed_tests
            {
                return Err(format!(
                    "schema 9 test-results artifact totals differ for producer {}",
                    summary.node
                ));
            }
            let totals = add_verified(recomputed, summary.totals.filtered_tests)?;
            verified_nodes.push(NodeTestResultSummary {
                node: summary.node.clone(),
                outer_attempt: summary.outer_attempt,
                totals,
                row_count: recomputed.executed_tests,
            });
        }

        let verified_compatibility = match &self.compatibility {
            Some(summary) => {
                let recomputed = counts
                    .remove(&TestResultProducerIdentity::Compatibility)
                    .ok_or_else(|| {
                        "schema 9 test-results artifact omits selected compatibility producer"
                            .to_string()
                    })?;
                if recomputed.executed_tests != summary.row_count
                    || recomputed.executed_tests != summary.totals.executed_tests
                    || recomputed.passed_tests != summary.totals.passed_tests
                    || recomputed.failed_tests != summary.totals.failed_tests
                {
                    return Err(
                        "schema 9 test-results artifact totals differ for compatibility producer"
                            .into(),
                    );
                }
                let totals = add_verified(recomputed, summary.totals.filtered_tests)?;
                Some(CompatibilityTestResultSummary {
                    totals,
                    row_count: recomputed.executed_tests,
                })
            }
            None => None,
        };
        if !counts.is_empty() {
            return Err("schema 9 test-results artifact contains an extra producer".into());
        }
        if verified_totals != self.totals {
            return Err("schema 9 test-results artifact checked totals mismatch".into());
        }

        Ok(VerifiedTestResultsArtifactV9 {
            selected: planned_selected.clone(),
            nodes: verified_nodes,
            compatibility: verified_compatibility,
            totals: verified_totals,
            row_count: parsed_row_count,
        })
    }
}

#[cfg(test)]
mod tests {
    use dagrun::io::dag_from_json;
    use sha2::Digest;
    use sha2::Sha256;

    use super::*;

    #[test]
    fn history_schema_accepts_unmeasured_cpu() {
        let row: HistoryRow = serde_json::from_str(
            r#"{
                "schema_version":1,
                "commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "result":"pass",
                "checks":36,
                "real_seconds":123,
                "user_seconds":null,
                "sys_seconds":null
            }"#,
        )
        .unwrap();
        assert_eq!(row.checks, Some(36));
        assert_eq!(row.real_seconds, Some(123.0));
        assert_eq!(row.user_seconds, None);
    }

    #[test]
    fn history_tree_distinguishes_absent_valid_and_malformed() {
        let absent: HistoryRow = serde_json::from_str(r#"{"schema_version":1}"#).unwrap();
        assert_eq!(absent.tree(), Ok(None));

        let valid: HistoryRow = serde_json::from_str(
            r#"{"schema_version":1,"tree":"0123456789abcdef0123456789ABCDEF01234567"}"#,
        )
        .unwrap();
        assert_eq!(
            valid.tree(),
            Ok(Some("0123456789abcdef0123456789ABCDEF01234567"))
        );

        let malformed_string: HistoryRow =
            serde_json::from_str(r#"{"schema_version":1,"tree":"unknown"}"#).unwrap();
        assert_eq!(
            malformed_string.tree(),
            Err("malformed HistoryRow tree: expected exactly 40 hexadecimal characters")
        );

        let malformed_type: HistoryRow =
            serde_json::from_str(r#"{"schema_version":1,"tree":7}"#).unwrap();
        assert_eq!(
            malformed_type.tree(),
            Err("malformed HistoryRow tree: expected a string")
        );
    }

    #[test]
    fn history_retry_rounds_distinguishes_absent_valid_and_malformed() {
        let absent: HistoryRow = serde_json::from_str(r#"{"schema_version":5}"#).unwrap();
        assert_eq!(absent.retry_rounds(), Ok(None));

        let valid: HistoryRow =
            serde_json::from_str(r#"{"schema_version":7,"retry_rounds":2}"#).unwrap();
        assert_eq!(valid.retry_rounds(), Ok(Some(2)));

        for malformed in [
            r#"{"schema_version":7,"retry_rounds":"2"}"#,
            r#"{"schema_version":7,"retry_rounds":-1}"#,
            r#"{"schema_version":7,"retry_rounds":1.5}"#,
        ] {
            let row: HistoryRow = serde_json::from_str(malformed).unwrap();
            assert_eq!(
                row.retry_rounds(),
                Err("malformed HistoryRow retry_rounds: expected a nonnegative integer")
            );
        }

        let historical: HistoryRow =
            serde_json::from_str(r#"{"schema_version":5,"env_block_retries":2}"#).unwrap();
        assert_eq!(historical.retry_rounds(), Ok(None));
    }

    #[test]
    fn history_executed_nodes_distinguishes_absent_valid_and_malformed() {
        let absent: HistoryRow = serde_json::from_str(r#"{"schema_version":5}"#).unwrap();
        assert_eq!(absent.executed_nodes(), Ok(None));

        let valid: HistoryRow =
            serde_json::from_str(r#"{"schema_version":7,"executed_nodes":12}"#).unwrap();
        assert_eq!(valid.executed_nodes(), Ok(Some(12)));

        for malformed in [
            r#"{"schema_version":7,"executed_nodes":"12"}"#,
            r#"{"schema_version":7,"executed_nodes":-1}"#,
            r#"{"schema_version":7,"executed_nodes":1.5}"#,
        ] {
            let row: HistoryRow = serde_json::from_str(malformed).unwrap();
            assert_eq!(
                row.executed_nodes(),
                Err("malformed HistoryRow executed_nodes: expected a nonnegative integer")
            );
        }
    }

    #[test]
    fn gate_failure_class_distinguishes_absent_valid_and_malformed() {
        let absent: GateHistoryRow = serde_json::from_str(r#"{"name":"test.fixture"}"#).unwrap();
        assert_eq!(absent.failure_class(), Ok(None));

        let valid: GateHistoryRow =
            serde_json::from_str(r#"{"name":"test.fixture","failure_class":"product_failure"}"#)
                .unwrap();
        assert_eq!(
            valid.failure_class(),
            Ok(Some(FailureClass::ProductFailure))
        );

        let malformed_type: GateHistoryRow =
            serde_json::from_str(r#"{"name":"test.fixture","failure_class":7}"#).unwrap();
        assert_eq!(
            malformed_type.failure_class(),
            Err("malformed GateHistoryRow failure_class: expected a string".into())
        );

        let unknown: GateHistoryRow =
            serde_json::from_str(r#"{"name":"test.fixture","failure_class":"future_failure"}"#)
                .unwrap();
        assert_eq!(
            unknown.failure_class(),
            Err(
                "malformed GateHistoryRow failure_class: unknown failure_class `future_failure`"
                    .into()
            )
        );
    }

    #[test]
    fn history_tree_accessor_preserves_receipt_canonicalization_v1() {
        let row: HistoryRow = serde_json::from_str(
            r#"{
                "schema_version":5,
                "commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "tree":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "tree_dirty":false,
                "result":"pass",
                "z_extension":2,
                "a_extension":1
            }"#,
        )
        .unwrap();

        let canonical = serde_json::to_vec(&row).unwrap();
        assert_eq!(
            canonical,
            br#"{"schema_version":5,"run_id":null,"started_at":null,"finished_at":null,"host":null,"slot":null,"repo":null,"cwd":null,"profile":null,"selection_mode":null,"commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","commit_anchored":null,"tree_dirty":false,"result":"pass","exit_code":null,"executed_tests":null,"filtered_tests":null,"coverage":null,"full_coverage":null,"checks":null,"gates_run":null,"gates_expected":null,"failures":null,"dag_jobs":null,"concurrent_validates":null,"known_flaky_failure":null,"solo_rerun_confirmation":null,"real_seconds":null,"user_seconds":null,"sys_seconds":null,"log_file":null,"source":null,"cell_results":null,"gates":[],"a_extension":1,"tree":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","z_extension":2}"#.as_slice()
        );
        let digest = format!("{:x}", Sha256::digest(&canonical));
        assert_eq!(
            digest,
            "e909dc57a7de484727d13d556895ad1afd547c956700987ab1945d2d5fb7bb5d"
        );
    }

    #[test]
    fn passed_tests_is_typed_without_changing_retained_receipt_bytes() {
        let retained: HistoryRow =
            serde_json::from_str(r#"{"schema_version":5,"executed_tests":7,"filtered_tests":2}"#)
                .unwrap();
        assert_eq!(retained.passed_tests, None);
        assert!(
            !serde_json::to_string(&retained)
                .unwrap()
                .contains("passed_tests")
        );

        let current: HistoryRow = serde_json::from_str(
            r#"{"schema_version":7,"executed_tests":7,"passed_tests":5,"filtered_tests":2}"#,
        )
        .unwrap();
        assert_eq!(current.passed_tests, Some(5));
        assert_eq!(
            serde_json::to_value(&current).unwrap()["passed_tests"],
            serde_json::json!(5)
        );
    }

    #[test]
    fn cell_verdict_schema_keeps_all_four_states_distinct() {
        let compared = r#"{
            "state":"compared-and-matched",
            "comparison_tier":"canonical-bitwise",
            "comparison":{
                "strictness":"canonical","compare_logs":true,"log_scope":"info",
                "strip_lines":false,"canonicalize_addresses":true,"full_trace":true,
                "exact_remainder":true,"stripped_prefixes":["real-wall-clock-prefix/v1"],
                "canonicalizations":["host-address-to-first-appearance-ordinal/v1"],
                "ignore_lines":false,"skip_commit":false,"skip_detlog":false
            },
            "bitwise_parity":true,
            "compared_log_messages":{"left":7,"right":7}
        }"#;
        assert!(matches!(
            serde_json::from_str::<CellVerdict>(compared).unwrap(),
            CellVerdict::ComparedAndMatched { .. }
        ));
        let current = compared.replace(
            "\"compare_logs\":true,",
            "\"display_name\":\"BitwiseInfoV1\",\"compare_logs\":true,\"compare_io_buffers\":true,\"record_envelope\":\"all_records_v1\",\"virtualize_time\":true,",
        );
        let CellVerdict::ComparedAndMatched {
            comparison,
            compared_log_messages,
            ..
        } = serde_json::from_str::<CellVerdict>(&current).unwrap()
        else {
            panic!("current verifier report changed verdict state");
        };
        assert_eq!(comparison.compare_io_buffers, Some(true));
        assert_eq!(comparison.display_name.as_deref(), Some("BitwiseInfoV1"));
        assert_eq!(
            comparison.record_envelope.as_deref(),
            Some("all_records_v1")
        );
        assert_eq!(comparison.virtualize_time, Some(true));
        assert!(comparison.is_canonical_bitwise_info_v1(&compared_log_messages));
        let mut replay_comparison = comparison.clone();
        replay_comparison.virtualize_time = Some(false);
        assert!(
            replay_comparison
                .is_canonical_bitwise_info_v1_for_time_policy(false, &compared_log_messages)
        );
        assert!(!replay_comparison.is_canonical_bitwise_info_v1(&compared_log_messages));
        assert!(
            !comparison.is_canonical_bitwise_info_v1_for_time_policy(false, &compared_log_messages)
        );
        let CellVerdict::ComparedAndMatched {
            comparison: older_comparison,
            compared_log_messages: older_counts,
            ..
        } = serde_json::from_str::<CellVerdict>(compared).unwrap()
        else {
            panic!("older verifier report changed verdict state");
        };
        assert!(!older_comparison.is_canonical_bitwise_info_v1(&older_counts));
        for (state, expected) in [
            ("compared-and-diverged", "diverged"),
            ("performs-no-comparison-by-design", "by-design"),
            ("unavailable-with-reason", "unavailable"),
        ] {
            let value = if state == "compared-and-diverged" {
                compared
                    .replace("compared-and-matched", state)
                    .replace("\"bitwise_parity\":true", "\"bitwise_parity\":false")
            } else {
                format!(
                    r#"{{"state":"{state}","comparison_tier":"declared-but-unverifiable","reason":"fixture reason"}}"#
                )
            };
            let verdict: CellVerdict = serde_json::from_str(&value).unwrap();
            assert_eq!(
                match verdict {
                    CellVerdict::ComparedAndDiverged { .. } => "diverged",
                    CellVerdict::PerformsNoComparisonByDesign { .. } => "by-design",
                    CellVerdict::UnavailableWithReason { .. } => "unavailable",
                    CellVerdict::ComparedAndMatched { .. } => "matched",
                },
                expected
            );
        }
    }

    #[test]
    fn compared_log_messages_key_is_required_but_explicit_null_is_retained() {
        let base = r#"{
            "state":"compared-and-matched",
            "comparison_tier":"execution-only-self-consistent",
            "comparison":{
                "strictness":"stripped","compare_logs":false,"log_scope":"deterministic",
                "strip_lines":true,"canonicalize_addresses":false,"full_trace":false,
                "exact_remainder":false,"stripped_prefixes":[],"canonicalizations":[],
                "ignore_lines":false,"skip_commit":false,"skip_detlog":false
            },
            "bitwise_parity":false,
            "compared_log_messages":null
        }"#;
        assert!(serde_json::from_str::<CellVerdict>(base).is_ok());
        let mut missing: serde_json::Value = serde_json::from_str(base).unwrap();
        missing
            .as_object_mut()
            .unwrap()
            .remove("compared_log_messages");
        assert!(serde_json::from_value::<CellVerdict>(missing).is_err());

        for retired in [
            "full-stdout-info-stack-heap",
            "stdout-info-stack-heap-spot-check",
            "legacy-unqualified",
            "unqualified-no-comparison",
            "unqualified-stdout-only",
            "unqualified-self-verify-only",
            "unqualified-tool-count-only",
        ] {
            let old_tier = base.replace("execution-only-self-consistent", retired);
            assert!(serde_json::from_str::<CellVerdict>(&old_tier).is_err());
        }
    }

    #[test]
    fn cell_results_reads_supported_shape_first_and_preserves_its_serialization() {
        let json = serde_json::json!({
            "run_id": "run-1",
            "hermit_sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "source_tree_dirty": false,
            "selected_count": 1,
            "recorded_count": 1,
            "population_sha256": "b".repeat(64),
            "artifact": {
                "path": "ignored/validate/artifacts/run-1/cell-results.jsonl",
                "sha256": "c".repeat(64),
                "row_count": 1
            },
            "selected": [{
                "lane": "portable",
                "category": "c-programs",
                "test": "hello",
                "mode": "verify",
                "backend": "ptrace"
            }],
            "cells": [{
                "lane": "portable",
                "category": "c-programs",
                "test": "hello",
                "mode": "verify",
                "backend": "ptrace",
                "cell_verdict": {
                    "state": "unavailable-with-reason",
                    "comparison_tier": "declared-but-unverifiable",
                    "reason": "fixture"
                }
            }]
        });
        let evidence: CellResultsEvidence = serde_json::from_value(json.clone()).unwrap();
        let value: CellResultsValue = serde_json::from_value(json.clone()).unwrap();
        assert!(matches!(value, CellResultsValue::Typed(_)));
        assert_eq!(
            serde_json::to_vec(&value).unwrap(),
            serde_json::to_vec(&evidence).unwrap(),
            "the wrapper must not change canonical receipt bytes for supported rows"
        );

        let mut newer = json;
        newer["future_field"] = serde_json::json!(true);
        let value: CellResultsValue = serde_json::from_value(newer.clone()).unwrap();
        assert!(matches!(value, CellResultsValue::Other(_)));
        assert_eq!(serde_json::to_value(value).unwrap(), newer);
    }

    #[test]
    fn schema8_cell_results_has_an_exact_validate_path_shape() {
        let json = serde_json::json!({
            "path": "quick",
            "run_id": "run-8",
            "hermit_sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "source_tree_dirty": false,
            "selected_count": 0,
            "recorded_count": 0,
            "population_sha256": "b".repeat(64),
            "artifact": {
                "path": "ignored/validate/artifacts/run-8/cell-results.jsonl",
                "sha256": "c".repeat(64),
                "row_count": 0
            },
            "selected": [],
            "cells": []
        });
        let evidence: CellResultsEvidenceV8 = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(evidence.path, ValidatePath::Quick);
        let value: CellResultsValue = serde_json::from_value(json.clone()).unwrap();
        assert!(matches!(value, CellResultsValue::Other(_)));
        assert_eq!(serde_json::to_value(value).unwrap(), json);

        let row: HistoryRow = serde_json::from_value(serde_json::json!({
            "schema_version": 8,
            "cell_results": json,
        }))
        .unwrap();
        assert_eq!(row.cell_results_validate_path(), Some(ValidatePath::Quick));
        assert_eq!(row.cell_results_evidence().unwrap().selected_count, 0);

        let mut wrong_outer_version = serde_json::to_value(&row).unwrap();
        wrong_outer_version["schema_version"] = serde_json::json!(7);
        let wrong_outer_version: HistoryRow = serde_json::from_value(wrong_outer_version).unwrap();
        assert!(wrong_outer_version.cell_results_evidence().is_none());

        let mut schema9_outer_version = serde_json::to_value(&row).unwrap();
        schema9_outer_version["schema_version"] = serde_json::json!(9);
        let schema9_outer_version: HistoryRow =
            serde_json::from_value(schema9_outer_version).unwrap();
        assert_eq!(
            schema9_outer_version.cell_results_validate_path(),
            Some(ValidatePath::Quick)
        );
        assert!(schema9_outer_version.cell_results_evidence().is_some());

        let mut future_outer_version = serde_json::to_value(&row).unwrap();
        future_outer_version["schema_version"] = serde_json::json!(10);
        let future_outer_version: HistoryRow =
            serde_json::from_value(future_outer_version).unwrap();
        assert!(future_outer_version.cell_results_evidence().is_none());

        let mut wrong_path = serde_json::to_value(&row).unwrap();
        wrong_path["schema_version"] = serde_json::json!(8);
        wrong_path["cell_results"]["path"] = serde_json::json!("selective");
        let wrong_path: HistoryRow = serde_json::from_value(wrong_path).unwrap();
        assert!(wrong_path.cell_results_evidence().is_none());

        let mut extension = serde_json::to_value(&row).unwrap();
        extension["cell_results"]["future_field"] = serde_json::json!(true);
        let extension: HistoryRow = serde_json::from_value(extension).unwrap();
        assert!(extension.cell_results_evidence().is_none());
    }

    #[test]
    fn schema8_compared_verdict_requires_every_current_comparison_key() {
        let comparison = serde_json::json!({
            "strictness": "canonical",
            "display_name": "BitwiseInfoV1",
            "compare_logs": true,
            "compare_io_buffers": true,
            "record_envelope": "all_records_v1",
            "virtualize_time": true,
            "log_scope": "info",
            "strip_lines": false,
            "canonicalize_addresses": true,
            "full_trace": true,
            "exact_remainder": true,
            "stripped_prefixes": ["real-wall-clock-prefix/v1"],
            "canonicalizations": ["host-address-to-first-appearance-ordinal/v1"],
            "ignore_lines": false,
            "skip_commit": false,
            "skip_detlog": false
        });
        let identity = serde_json::json!({
            "lane": "portable",
            "category": "c-programs",
            "test": "example",
            "mode": "verify",
            "backend": "ptrace"
        });
        let mut json = serde_json::json!({
            "path": "full",
            "run_id": "run-8",
            "hermit_sha": "a".repeat(40),
            "source_tree_dirty": false,
            "selected_count": 1,
            "recorded_count": 1,
            "population_sha256": "b".repeat(64),
            "artifact": {
                "path": "ignored/validate/artifacts/run-8/cell-results.jsonl",
                "sha256": "c".repeat(64),
                "row_count": 1
            },
            "selected": [identity.clone()],
            "cells": [{
                "lane": "portable",
                "category": "c-programs",
                "test": "example",
                "mode": "verify",
                "backend": "ptrace",
                "cell_verdict": {
                    "state": "compared-and-matched",
                    "comparison_tier": "canonical-bitwise",
                    "comparison": comparison,
                    "bitwise_parity": true,
                    "compared_log_messages": {"left": 1, "right": 1}
                }
            }]
        });
        assert!(serde_json::from_value::<CellResultsEvidenceV8>(json.clone()).is_ok());

        for required in [
            "display_name",
            "compare_io_buffers",
            "record_envelope",
            "virtualize_time",
        ] {
            let mut missing = json.clone();
            missing["cells"][0]["cell_verdict"]["comparison"]
                .as_object_mut()
                .unwrap()
                .remove(required);
            assert!(
                serde_json::from_value::<CellResultsEvidenceV8>(missing).is_err(),
                "schema 8 must require comparison.{required}"
            );
        }

        json["cells"][0]["cell_verdict"]["comparison"]["display_name"] = serde_json::Value::Null;
        assert!(serde_json::from_value::<CellResultsEvidenceV8>(json).is_ok());
    }

    fn schema9_row() -> Value {
        let selected = TestResultsSelectedPopulation {
            nodes: vec!["test.alpha".into(), "test.beta".into()],
            compatibility: true,
        };
        let population_sha256 = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&selected).unwrap())
        );
        serde_json::json!({
            "schema_version": 9,
            "run_id": "run-9",
            "profile": "full",
            "commit": "a".repeat(40),
            "tree_dirty": false,
            "executed_tests": 4,
            "passed_tests": 3,
            "filtered_tests": 7,
            "test_results": {
                "path": "full",
                "run_id": "run-9",
                "hermit_sha": "a".repeat(40),
                "source_tree_dirty": false,
                "selected_count": 3,
                "recorded_count": 4,
                "population_sha256": population_sha256,
                "selected": {
                    "nodes": ["test.alpha", "test.beta"],
                    "compatibility": true
                },
                "nodes": [{
                    "node": "test.alpha",
                    "outer_attempt": 2,
                    "totals": {
                        "executed_tests": 2,
                        "passed_tests": 1,
                        "failed_tests": 1,
                        "filtered_tests": 3
                    },
                    "row_count": 2
                }, {
                    "node": "test.beta",
                    "outer_attempt": 1,
                    "totals": {
                        "executed_tests": 1,
                        "passed_tests": 1,
                        "failed_tests": 0,
                        "filtered_tests": 4
                    },
                    "row_count": 1
                }],
                "compatibility": {
                    "totals": {
                        "executed_tests": 1,
                        "passed_tests": 1,
                        "failed_tests": 0,
                        "filtered_tests": 0
                    },
                    "row_count": 1
                },
                "totals": {
                    "executed_tests": 4,
                    "passed_tests": 3,
                    "failed_tests": 1,
                    "filtered_tests": 7
                },
                "artifact": {
                    "path": "ignored/validate/artifacts/run-9/test-results.jsonl",
                    "sha256": "b".repeat(64),
                    "row_count": 4
                }
            }
        })
    }

    fn schema9_artifact_rows() -> Vec<TestResultArtifactRow> {
        vec![
            TestResultArtifactRow {
                run_id: "run-9".into(),
                hermit_sha: "a".repeat(40),
                path: ValidatePath::Full,
                producer: TestResultProducer::Node {
                    node: "test.alpha".into(),
                    outer_attempt: 2,
                },
                id: "a".into(),
                result: TestResultVerdict::Pass,
                attempts: 1,
            },
            TestResultArtifactRow {
                run_id: "run-9".into(),
                hermit_sha: "a".repeat(40),
                path: ValidatePath::Full,
                producer: TestResultProducer::Node {
                    node: "test.alpha".into(),
                    outer_attempt: 2,
                },
                id: "z".into(),
                result: TestResultVerdict::Fail,
                attempts: 2,
            },
            TestResultArtifactRow {
                run_id: "run-9".into(),
                hermit_sha: "a".repeat(40),
                path: ValidatePath::Full,
                producer: TestResultProducer::Node {
                    node: "test.beta".into(),
                    outer_attempt: 1,
                },
                id: "same-id".into(),
                result: TestResultVerdict::Pass,
                attempts: 1,
            },
            TestResultArtifactRow {
                run_id: "run-9".into(),
                hermit_sha: "a".repeat(40),
                path: ValidatePath::Full,
                producer: TestResultProducer::Compatibility,
                id: "same-id".into(),
                result: TestResultVerdict::Pass,
                attempts: 1,
            },
        ]
    }

    fn schema9_artifact_bytes(rows: &[TestResultArtifactRow]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for row in rows {
            bytes.extend(serde_json::to_vec(row).unwrap());
            bytes.push(b'\n');
        }
        bytes
    }

    fn bind_schema9_artifact(value: &mut Value, bytes: &[u8], row_count: u64) {
        value["test_results"]["artifact"]["sha256"] =
            serde_json::json!(format!("{:x}", Sha256::digest(bytes)));
        value["test_results"]["artifact"]["row_count"] = serde_json::json!(row_count);
    }

    fn schema9_constructed_plan_steps() -> Vec<Step> {
        dag_from_json(
            r#"{
                "steps": [{
                    "group": "test",
                    "job": "beta",
                    "cmd": "true",
                    "result_manifests": [{
                        "kind": "structured-test-results",
                        "schema": 2,
                        "path_env": "DAGRUN_TEST_COUNTS_PATH",
                        "owner": "test.beta"
                    }]
                }, {
                    "group": "setup",
                    "job": "dependency",
                    "cmd": "true",
                    "result_manifests": []
                }, {
                    "group": "test",
                    "job": "alpha",
                    "cmd": "true",
                    "result_manifests": [{
                        "kind": "structured-test-results",
                        "schema": 2,
                        "path_env": "DAGRUN_TEST_COUNTS_PATH",
                        "owner": "test.alpha"
                    }]
                }]
            }"#,
        )
        .unwrap()
        .steps
    }

    fn schema9_row_with_artifact() -> (HistoryRow, Vec<Step>, Vec<u8>) {
        let bytes = schema9_artifact_bytes(&schema9_artifact_rows());
        let mut value = schema9_row();
        bind_schema9_artifact(&mut value, &bytes, 4);
        (
            serde_json::from_value(value).unwrap(),
            schema9_constructed_plan_steps(),
            bytes,
        )
    }

    fn assert_schema9_artifact_refused(
        value: Value,
        constructed_plan_steps: &[Step],
        compatibility_selected: bool,
        bytes: &[u8],
        expected: &str,
    ) {
        let row: HistoryRow = serde_json::from_value(value).unwrap();
        let error = row
            .verify_test_results_artifact_bytes(
                constructed_plan_steps,
                compatibility_selected,
                bytes,
            )
            .expect_err("mutated schema 9 artifact must refuse");
        assert!(
            error.contains(expected),
            "schema 9 artifact refusal {error:?} did not name {expected:?}"
        );
    }

    fn assert_schema9_refused(value: Value, expected: &str) {
        let row: HistoryRow = serde_json::from_value(value).unwrap();
        let error = row
            .test_results_evidence()
            .expect_err("mutated schema 9 evidence must refuse");
        assert!(
            error.contains(expected),
            "schema 9 refusal {error:?} did not name {expected:?}"
        );
    }

    #[test]
    fn schema9_test_results_accept_exact_identity_population_path_and_totals() {
        let value = schema9_row();
        let row: HistoryRow = serde_json::from_value(value.clone()).unwrap();
        let evidence = row
            .test_results_evidence()
            .unwrap()
            .expect("schema 9 should expose typed evidence");
        assert_eq!(evidence.path, ValidatePath::Full);
        assert_eq!(evidence.selected_count, 3);
        assert_eq!(evidence.recorded_count, 4);
        assert_eq!(evidence.totals.executed_tests, 4);
        assert_eq!(evidence.totals.passed_tests, 3);
        assert_eq!(
            serde_json::to_value(&row).unwrap()["test_results"],
            value["test_results"]
        );
    }

    #[test]
    fn schema9_artifact_bytes_recompute_exact_population_and_totals() {
        let (row, constructed_plan_steps, bytes) = schema9_row_with_artifact();
        let verified = row
            .verify_test_results_artifact_bytes(&constructed_plan_steps, true, &bytes)
            .unwrap()
            .expect("schema 9 should verify its exact artifact bytes");
        assert_eq!(
            verified.selected,
            TestResultsSelectedPopulation {
                nodes: vec!["test.alpha".into(), "test.beta".into()],
                compatibility: true,
            }
        );
        assert_eq!(verified.row_count, 4);
        assert_eq!(verified.nodes.len(), 2);
        assert_eq!(verified.nodes[0].node, "test.alpha");
        assert_eq!(verified.nodes[0].outer_attempt, 2);
        assert_eq!(verified.nodes[0].totals.executed_tests, 2);
        assert_eq!(verified.nodes[0].totals.passed_tests, 1);
        assert_eq!(verified.nodes[0].totals.failed_tests, 1);
        assert_eq!(verified.nodes[0].totals.filtered_tests, 3);
        assert_eq!(verified.compatibility.unwrap().totals.executed_tests, 1);
        assert_eq!(verified.totals.executed_tests, 4);
        assert_eq!(verified.totals.passed_tests, 3);
        assert_eq!(verified.totals.failed_tests, 1);
        assert_eq!(verified.totals.filtered_tests, 7);

        let mut wrong_owner = constructed_plan_steps.clone();
        let declaration = wrong_owner[0]
            .result_manifests
            .as_mut()
            .unwrap()
            .iter_mut()
            .find_map(|manifest| match manifest {
                dagrun::model::ResultManifest::StructuredTestResults(declaration) => {
                    Some(declaration)
                }
                dagrun::model::ResultManifest::ManifestCell(_) => None,
            })
            .unwrap();
        declaration.owner = "test.someone-else".into();
        let error = row
            .verify_test_results_artifact_bytes(&wrong_owner, true, &bytes)
            .expect_err("a constructed plan with a false result owner must refuse");
        assert!(error.contains("owner"), "unexpected owner refusal: {error}");

        for schema in [6, 7, 8, 10] {
            let mut value = schema9_row();
            value["schema_version"] = serde_json::json!(schema);
            let historical: HistoryRow = serde_json::from_value(value).unwrap();
            assert!(
                historical
                    .verify_test_results_artifact_bytes(&constructed_plan_steps, true, &bytes)
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn schema9_artifact_refuses_row_identity_producer_and_attempt_mutations() {
        let (_, constructed_plan_steps, _) = schema9_row_with_artifact();
        for (field, replacement, expected) in [
            ("run_id", serde_json::json!("other-run"), "wrong run_id"),
            (
                "hermit_sha",
                serde_json::json!("c".repeat(40)),
                "wrong hermit_sha",
            ),
            ("path", serde_json::json!("quick"), "wrong validation path"),
        ] {
            let mut rows = schema9_artifact_rows();
            let mut row = serde_json::to_value(&rows[0]).unwrap();
            row[field] = replacement;
            rows[0] = serde_json::from_value(row).unwrap();
            let bytes = schema9_artifact_bytes(&rows);
            let mut value = schema9_row();
            bind_schema9_artifact(&mut value, &bytes, 4);
            assert_schema9_artifact_refused(value, &constructed_plan_steps, true, &bytes, expected);
        }

        let mut rows = schema9_artifact_rows();
        rows[0].producer = TestResultProducer::Node {
            node: "test.gamma".into(),
            outer_attempt: 2,
        };
        let bytes = schema9_artifact_bytes(&rows);
        let mut value = schema9_row();
        bind_schema9_artifact(&mut value, &bytes, 4);
        assert_schema9_artifact_refused(
            value,
            &constructed_plan_steps,
            true,
            &bytes,
            "unselected node",
        );

        let mut rows = schema9_artifact_rows();
        rows[0].producer = TestResultProducer::Node {
            node: "test.alpha".into(),
            outer_attempt: 3,
        };
        let bytes = schema9_artifact_bytes(&rows);
        let mut value = schema9_row();
        bind_schema9_artifact(&mut value, &bytes, 4);
        assert_schema9_artifact_refused(
            value,
            &constructed_plan_steps,
            true,
            &bytes,
            "wrong outer_attempt",
        );
    }

    #[test]
    fn schema9_artifact_refuses_missing_extra_duplicate_and_shrunk_populations() {
        let (_, constructed_plan_steps, _) = schema9_row_with_artifact();

        let mut missing_row = schema9_artifact_rows();
        missing_row.remove(1);
        let bytes = schema9_artifact_bytes(&missing_row);
        let mut value = schema9_row();
        bind_schema9_artifact(&mut value, &bytes, 4);
        assert_schema9_artifact_refused(
            value,
            &constructed_plan_steps,
            true,
            &bytes,
            "row_count mismatch",
        );

        let mut extra = schema9_artifact_rows();
        extra.insert(
            3,
            TestResultArtifactRow {
                run_id: "run-9".into(),
                hermit_sha: "a".repeat(40),
                path: ValidatePath::Full,
                producer: TestResultProducer::Node {
                    node: "test.gamma".into(),
                    outer_attempt: 1,
                },
                id: "extra".into(),
                result: TestResultVerdict::Pass,
                attempts: 1,
            },
        );
        let bytes = schema9_artifact_bytes(&extra);
        let mut value = schema9_row();
        bind_schema9_artifact(&mut value, &bytes, 4);
        assert_schema9_artifact_refused(
            value,
            &constructed_plan_steps,
            true,
            &bytes,
            "unselected node",
        );

        let mut duplicate = schema9_artifact_rows();
        duplicate.insert(1, duplicate[0].clone());
        let bytes = schema9_artifact_bytes(&duplicate);
        let mut value = schema9_row();
        bind_schema9_artifact(&mut value, &bytes, 4);
        assert_schema9_artifact_refused(
            value,
            &constructed_plan_steps,
            true,
            &bytes,
            "duplicate producer/test row",
        );

        let rows = schema9_artifact_rows()
            .into_iter()
            .filter(|row| {
                !matches!(
                    &row.producer,
                    TestResultProducer::Node { node, .. } if node == "test.beta"
                )
            })
            .collect::<Vec<_>>();
        let bytes = schema9_artifact_bytes(&rows);
        let mut omitted = schema9_row();
        omitted["test_results"]["nodes"][1]["totals"] = serde_json::json!({
            "executed_tests": 0,
            "passed_tests": 0,
            "failed_tests": 0,
            "filtered_tests": 4
        });
        omitted["test_results"]["nodes"][1]["row_count"] = serde_json::json!(0);
        omitted["test_results"]["totals"]["executed_tests"] = serde_json::json!(3);
        omitted["test_results"]["totals"]["passed_tests"] = serde_json::json!(2);
        omitted["test_results"]["recorded_count"] = serde_json::json!(3);
        omitted["executed_tests"] = serde_json::json!(3);
        omitted["passed_tests"] = serde_json::json!(2);
        bind_schema9_artifact(&mut omitted, &bytes, 3);
        assert_schema9_artifact_refused(
            omitted,
            &constructed_plan_steps,
            true,
            &bytes,
            "omits selected producer",
        );

        let mut compatibility_omitted_rows = schema9_artifact_rows();
        compatibility_omitted_rows.pop();
        let compatibility_omitted_bytes = schema9_artifact_bytes(&compatibility_omitted_rows);
        let mut compatibility_omitted = schema9_row();
        compatibility_omitted["test_results"]["compatibility"]["totals"] = serde_json::json!({
            "executed_tests": 0,
            "passed_tests": 0,
            "failed_tests": 0,
            "filtered_tests": 0
        });
        compatibility_omitted["test_results"]["compatibility"]["row_count"] = serde_json::json!(0);
        compatibility_omitted["test_results"]["totals"]["executed_tests"] = serde_json::json!(3);
        compatibility_omitted["test_results"]["totals"]["passed_tests"] = serde_json::json!(2);
        compatibility_omitted["test_results"]["recorded_count"] = serde_json::json!(3);
        compatibility_omitted["executed_tests"] = serde_json::json!(3);
        compatibility_omitted["passed_tests"] = serde_json::json!(2);
        bind_schema9_artifact(&mut compatibility_omitted, &compatibility_omitted_bytes, 3);
        assert_schema9_artifact_refused(
            compatibility_omitted,
            &constructed_plan_steps,
            true,
            &compatibility_omitted_bytes,
            "omits selected compatibility producer",
        );

        let mut compatibility_unselected = schema9_row();
        compatibility_unselected["test_results"]["selected"]["compatibility"] =
            serde_json::json!(false);
        compatibility_unselected["test_results"]["selected_count"] = serde_json::json!(2);
        compatibility_unselected["test_results"]["compatibility"] = serde_json::Value::Null;
        compatibility_unselected["test_results"]["totals"] = serde_json::json!({
            "executed_tests": 3,
            "passed_tests": 2,
            "failed_tests": 1,
            "filtered_tests": 7
        });
        compatibility_unselected["test_results"]["recorded_count"] = serde_json::json!(3);
        compatibility_unselected["executed_tests"] = serde_json::json!(3);
        compatibility_unselected["passed_tests"] = serde_json::json!(2);
        let unselected_population = TestResultsSelectedPopulation {
            nodes: vec!["test.alpha".into(), "test.beta".into()],
            compatibility: false,
        };
        compatibility_unselected["test_results"]["population_sha256"] = serde_json::json!(format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&unselected_population).unwrap())
        ));
        bind_schema9_artifact(
            &mut compatibility_unselected,
            &schema9_artifact_bytes(&schema9_artifact_rows()),
            3,
        );
        assert_schema9_artifact_refused(
            compatibility_unselected,
            &constructed_plan_steps,
            false,
            &schema9_artifact_bytes(&schema9_artifact_rows()),
            "unselected compatibility producer",
        );

        let shrunk_selected = TestResultsSelectedPopulation {
            nodes: vec!["test.alpha".into()],
            compatibility: true,
        };
        let mut shrunk = schema9_row();
        shrunk["test_results"]["selected"] = serde_json::to_value(&shrunk_selected).unwrap();
        shrunk["test_results"]["selected_count"] = serde_json::json!(2);
        shrunk["test_results"]["nodes"]
            .as_array_mut()
            .unwrap()
            .pop();
        shrunk["test_results"]["totals"] = serde_json::json!({
            "executed_tests": 3,
            "passed_tests": 2,
            "failed_tests": 1,
            "filtered_tests": 3
        });
        shrunk["test_results"]["recorded_count"] = serde_json::json!(3);
        shrunk["executed_tests"] = serde_json::json!(3);
        shrunk["passed_tests"] = serde_json::json!(2);
        shrunk["filtered_tests"] = serde_json::json!(3);
        shrunk["test_results"]["population_sha256"] = serde_json::json!(format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&shrunk_selected).unwrap())
        ));
        bind_schema9_artifact(&mut shrunk, &bytes, 3);
        assert_schema9_artifact_refused(
            shrunk,
            &constructed_plan_steps,
            true,
            &bytes,
            "differs from constructed plan",
        );
    }

    #[test]
    fn schema9_artifact_refuses_malformed_noncanonical_digest_and_total_mutations() {
        let (_, constructed_plan_steps, valid_bytes) = schema9_row_with_artifact();

        let mut value = schema9_row();
        bind_schema9_artifact(&mut value, &valid_bytes, 4);
        assert_schema9_artifact_refused(
            value.clone(),
            &constructed_plan_steps,
            true,
            &valid_bytes[..valid_bytes.len() - 1],
            "final newline",
        );
        assert_schema9_artifact_refused(
            value.clone(),
            &constructed_plan_steps,
            true,
            b"{\n",
            "malformed",
        );

        let mut noncanonical = valid_bytes.clone();
        noncanonical.insert(1, b' ');
        assert_schema9_artifact_refused(
            value.clone(),
            &constructed_plan_steps,
            true,
            &noncanonical,
            "not canonical",
        );

        value["test_results"]["artifact"]["sha256"] = serde_json::json!("c".repeat(64));
        assert_schema9_artifact_refused(
            value,
            &constructed_plan_steps,
            true,
            &valid_bytes,
            "sha256 mismatch",
        );

        let mut rows = schema9_artifact_rows();
        rows[0].result = TestResultVerdict::Fail;
        let bytes = schema9_artifact_bytes(&rows);
        let mut totals = schema9_row();
        bind_schema9_artifact(&mut totals, &bytes, 4);
        assert_schema9_artifact_refused(
            totals,
            &constructed_plan_steps,
            true,
            &bytes,
            "totals differ for producer test.alpha",
        );

        let mut wrong_path = schema9_row();
        wrong_path["test_results"]["artifact"]["path"] =
            serde_json::json!("ignored/validate/artifacts/run-9/other.jsonl");
        bind_schema9_artifact(&mut wrong_path, &valid_bytes, 4);
        assert_schema9_artifact_refused(
            wrong_path,
            &constructed_plan_steps,
            true,
            &valid_bytes,
            "artifact binding is malformed",
        );
    }

    #[test]
    fn schema9_test_results_authority_is_version_first_and_old_bytes_omit_the_new_field() {
        for schema in [6, 7, 8] {
            let row: HistoryRow = serde_json::from_value(serde_json::json!({
                "schema_version": schema,
                "commit": "a".repeat(40)
            }))
            .unwrap();
            let bytes = serde_json::to_vec(&row).unwrap();
            assert!(
                !bytes
                    .windows(b"test_results".len())
                    .any(|window| window == b"test_results")
            );
            assert!(row.test_results_evidence().unwrap().is_none());
        }

        let mut older = schema9_row();
        older["schema_version"] = serde_json::json!(8);
        let older: HistoryRow = serde_json::from_value(older.clone()).unwrap();
        assert!(older.test_results_evidence().unwrap().is_none());
        assert!(serde_json::to_value(older).unwrap()["test_results"].is_object());

        let mut future = schema9_row();
        future["schema_version"] = serde_json::json!(10);
        future["test_results"]["future_field"] = serde_json::json!(true);
        let expected_test_results = future["test_results"].clone();
        let future: HistoryRow = serde_json::from_value(future).unwrap();
        assert!(future.test_results_evidence().unwrap().is_none());
        assert_eq!(
            serde_json::to_value(future).unwrap()["test_results"],
            expected_test_results
        );

        let mut missing = schema9_row();
        missing.as_object_mut().unwrap().remove("test_results");
        assert_schema9_refused(missing, "omitted test_results");
    }

    #[test]
    fn schema9_test_results_refuse_identity_population_and_path_mutations() {
        let mut wrong_path = schema9_row();
        wrong_path["test_results"]["path"] = serde_json::json!("quick");
        assert_schema9_refused(wrong_path, "path differs");

        let mut wrong_run = schema9_row();
        wrong_run["test_results"]["run_id"] = serde_json::json!("other-run");
        assert_schema9_refused(wrong_run, "run_id differs");

        let mut wrong_sha = schema9_row();
        wrong_sha["test_results"]["hermit_sha"] = serde_json::json!("c".repeat(40));
        assert_schema9_refused(wrong_sha, "hermit_sha differs");

        let mut dirty = schema9_row();
        dirty["test_results"]["source_tree_dirty"] = serde_json::json!(true);
        assert_schema9_refused(dirty, "exact clean row tree");

        let mut selected_count = schema9_row();
        selected_count["test_results"]["selected_count"] = serde_json::json!(2);
        assert_schema9_refused(selected_count, "selected_count mismatch");

        let mut unsorted = schema9_row();
        unsorted["test_results"]["selected"]["nodes"] =
            serde_json::json!(["test.beta", "test.alpha"]);
        assert_schema9_refused(unsorted, "duplicate, or unsorted");

        let mut missing_summary = schema9_row();
        missing_summary["test_results"]["nodes"]
            .as_array_mut()
            .unwrap()
            .pop();
        assert_schema9_refused(missing_summary, "summaries differ");

        let mut compatibility = schema9_row();
        compatibility["test_results"]["selected"]["compatibility"] = serde_json::json!(false);
        compatibility["test_results"]["selected_count"] = serde_json::json!(2);
        assert_schema9_refused(compatibility, "compatibility summary differs");

        let mut population = schema9_row();
        population["test_results"]["population_sha256"] = serde_json::json!("d".repeat(64));
        assert_schema9_refused(population, "population_sha256 mismatch");

        let mut traversal = schema9_row();
        traversal["test_results"]["artifact"]["path"] =
            serde_json::json!("ignored/validate/artifacts/../test-results.jsonl");
        assert_schema9_refused(traversal, "artifact binding is malformed");

        let mut digest = schema9_row();
        digest["test_results"]["artifact"]["sha256"] = serde_json::json!("NOT-A-DIGEST");
        assert_schema9_refused(digest, "artifact binding is malformed");
    }

    #[test]
    fn schema9_test_results_refuse_attempt_count_and_total_mutations() {
        let mut attempt = schema9_row();
        attempt["test_results"]["nodes"][0]["outer_attempt"] = serde_json::json!(0);
        assert_schema9_refused(attempt, "zero outer_attempt");

        let mut summary = schema9_row();
        summary["test_results"]["nodes"][0]["totals"]["passed_tests"] = serde_json::json!(2);
        assert_schema9_refused(summary, "pass/fail total differs");

        let mut row_count = schema9_row();
        row_count["test_results"]["nodes"][0]["row_count"] = serde_json::json!(3);
        assert_schema9_refused(row_count, "row_count differs");

        let mut aggregate = schema9_row();
        aggregate["test_results"]["totals"]["filtered_tests"] = serde_json::json!(8);
        assert_schema9_refused(aggregate, "checked totals");

        let mut recorded = schema9_row();
        recorded["test_results"]["recorded_count"] = serde_json::json!(5);
        assert_schema9_refused(recorded, "checked totals");

        let mut artifact_count = schema9_row();
        artifact_count["test_results"]["artifact"]["row_count"] = serde_json::json!(5);
        assert_schema9_refused(artifact_count, "checked totals");

        let mut row_total = schema9_row();
        row_total["passed_tests"] = serde_json::json!(4);
        assert_schema9_refused(row_total, "HistoryRow totals");

        let mut overflow = schema9_row();
        overflow["test_results"]["nodes"][0]["totals"] = serde_json::json!({
            "executed_tests": u64::MAX,
            "passed_tests": u64::MAX,
            "failed_tests": 0,
            "filtered_tests": 3
        });
        overflow["test_results"]["nodes"][0]["row_count"] = serde_json::json!(u64::MAX);
        assert_schema9_refused(overflow, "executed_tests overflowed");
    }

    #[test]
    fn schema9_exact_shapes_refuse_missing_unknown_and_invalid_artifact_rows() {
        let mut unknown = schema9_row();
        unknown["test_results"]["unknown"] = serde_json::json!(true);
        assert_schema9_refused(unknown, "unknown field");

        let mut missing = schema9_row();
        missing["test_results"]
            .as_object_mut()
            .unwrap()
            .remove("totals");
        assert_schema9_refused(missing, "missing field");

        let row = serde_json::json!({
            "run_id": "run-9",
            "hermit_sha": "a".repeat(40),
            "path": "full",
            "producer": {
                "source": "node",
                "node": "test.alpha",
                "outer_attempt": 2
            },
            "id": "suite$case",
            "result": "pass",
            "attempts": 1
        });
        assert!(serde_json::from_value::<TestResultArtifactRow>(row.clone()).is_ok());
        for (field, value) in [
            ("attempts", serde_json::json!(0)),
            ("id", serde_json::json!("")),
        ] {
            let mut invalid = row.clone();
            invalid[field] = value;
            assert!(serde_json::from_value::<TestResultArtifactRow>(invalid).is_err());
        }
        let mut zero_outer = row.clone();
        zero_outer["producer"]["outer_attempt"] = serde_json::json!(0);
        assert!(serde_json::from_value::<TestResultArtifactRow>(zero_outer).is_err());
        let mut extra = row;
        extra["extra"] = serde_json::json!(true);
        assert!(serde_json::from_value::<TestResultArtifactRow>(extra).is_err());
    }
}

#[cfg(test)]
mod cell_evidence_binding_tests {
    use super::*;

    fn identity(test: &str, mode: &str, backend: &str) -> CellIdentity {
        CellIdentity {
            lane: "portable".into(),
            category: "c-programs".into(),
            test: test.into(),
            mode: mode.into(),
            backend: backend.into(),
        }
    }

    /// The derivation must be the series producer's, not merely a digest of its
    /// own. These vectors are REAL published rows taken verbatim from the
    /// tracked shards, three carrying an attempt and three not, so both
    /// encodings are pinned against data rather than against this code.
    #[test]
    fn the_derivation_reproduces_real_published_event_identities() {
        /// (run_id, producer, cell, tree, run_index, attempt, event_id).
        type Vector = (
            &'static str,
            &'static str,
            &'static str,
            &'static str,
            u64,
            Option<u64>,
            &'static str,
        );
        let vectors: [Vector; 6] = [
            (
                "pressure-e12c472d22d2-red-verify-3x-20260830T151049Z",
                "pressure-test",
                "shared-futex-c/qemu-init/verify/ptrace",
                "e12c472d22d2725aa1e58b7169c4a44bf82fc890",
                3,
                Some(1),
                "series-00b21d40ff83452b02f8c4f7e12d26f08b45a9e95d34f1748f570004f99b5092",
            ),
            (
                "pressure-e12c472d22d2-red-verify-3x-20260830T151049Z",
                "pressure-test",
                "backend-parity-c/fd-duplication/verify/dbt",
                "e12c472d22d2725aa1e58b7169c4a44bf82fc890",
                1,
                Some(2),
                "series-00b28bf1cb59532e6a6729b6e3e26844b5d28e92dfb28ecdab28ef5553a87c64",
            ),
            (
                "pressure-e12c472d22d2-red-verify-3x-20260830T151049Z",
                "pressure-test",
                "chaos-c/lock-granularity/verify/sabre",
                "e12c472d22d2725aa1e58b7169c4a44bf82fc890",
                1,
                Some(1),
                "series-00c667343cb4402d4e4009c4cd2e6b0322c0f3d282af6171e07d6cf5023a9a19",
            ),
            (
                "git-pipe-baseline-cycle-v2",
                "pressure-test",
                "applications/git-repository-workflow/verify/ptrace",
                "dee8cf49ce6a7e3e9e2c87c930f3aa55ae83da18",
                18,
                None,
                "series-34f5c99e64c04f2efd989350d48d254915492ea805da537a8bd156078be9ce1c",
            ),
            (
                "git-pipe-baseline-cycle-v2",
                "pressure-test",
                "applications/git-repository-workflow/verify/ptrace",
                "dee8cf49ce6a7e3e9e2c87c930f3aa55ae83da18",
                8,
                None,
                "series-4dd18413e91e3570edc563e1ccb66be9638257002fe1bbdd946dde119c71838d",
            ),
            (
                "git-pipe-baseline-cycle-v2",
                "pressure-test",
                "applications/git-repository-workflow/verify/ptrace",
                "dee8cf49ce6a7e3e9e2c87c930f3aa55ae83da18",
                16,
                None,
                "series-5fb123dddfab57d4a5a87c90df843bb72cfa991a2447cad3ff17ca7d8e6242ff",
            ),
        ];
        for (run_id, producer, cell, tree, run_index, attempt, expected) in vectors {
            assert_eq!(
                CellEvidenceBinding::derive_event_id(
                    run_id, producer, cell, tree, run_index, attempt
                ),
                expected,
                "derivation drifted from the published identity for {cell} run_index {run_index}"
            );
        }

        // Synthetic golden controls from the actual Python identity expression
        // at parent 45fa84a89d773af4816afc02354adbb62952553f,
        // ci-hub/series/series.py:3653. They are separate from the six published
        // ASCII events above. Cover BMP, supplementary surrogate pairs (at both
        // range endpoints), DEL, and JSON quote/backslash/control escaping.
        let unicode_vectors: [Vector; 3] = [
            (
                "validate-caf\u{e9}-\u{4e2d}",
                "validate",
                "backend-parity-c/identity/verify/kvm",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                1,
                Some(1),
                "series-f3f6ebb451fc0b5ee6feb8fcea9749c7598133b12f430b77b90654abe1396d78",
            ),
            (
                "validate-\u{1f680}-\u{10000}-\u{10ffff}",
                "validate",
                "backend-parity-c/identity/verify/kvm",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                2,
                None,
                "series-79fe7f02b6fe0b917fa1ee883ba616593e6e652b445c7b672e0361e46288d1ad",
            ),
            (
                "validate-\u{7f}-\u{0}\u{8}\u{c}\u{a}\u{d}\u{9}\"\\",
                "validate",
                "backend-parity-c/identity/verify/kvm",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                3,
                Some(3),
                "series-1e748ca8c190012ee45a76e973760feeb4054a2e047d1b2550217c814238bed1",
            ),
        ];
        for (run_id, producer, cell, tree, run_index, attempt, expected) in unicode_vectors {
            assert_eq!(
                CellEvidenceBinding::derive_event_id(
                    run_id, producer, cell, tree, run_index, attempt
                ),
                expected,
                "derivation differs from the Python producer's Unicode encoding"
            );
        }
    }

    /// THE REGRESSION FOR THE COLLAPSE DEFECT, on a real folded row.
    ///
    /// `validate-hermit-147-green-bar-baseline-26f5bc2de3d3` published ONE
    /// event for attempts 1 and 2 of this cell, at `run_index: 1,
    /// last_run_index: 2`. An earlier version of this type predicted the
    /// identity from the SELECTED attempt, so a verdict computed from attempt 2
    /// named `run_index: 2` -- an identity that is absent from the corpus, and
    /// absent for 152 of 152 such rows.
    #[test]
    fn a_folded_row_is_resolved_by_interval_and_not_by_the_selected_attempt() {
        const RUN: &str = "validate-hermit-147-green-bar-baseline-26f5bc2de3d3";
        const CELL: &str = "system-utils/clock-determinism/custom/ptrace";
        const TREE: &str = "26f5bc2de3d328b64c41c42ef093ecb9d89e98ab";
        const PUBLISHED: &str =
            "series-041bf9284137fb073e8d0e154a4b6ce64e8a32cf889dc33c9db3ddf4e1b8e9d5";

        // The published event is the FOLD, at run_index 1.
        assert_eq!(
            CellEvidenceBinding::derive_event_id(RUN, "validate", CELL, TREE, 1, None),
            PUBLISHED
        );
        // Predicting from the selected attempt 2 yields a DIFFERENT identity,
        // and that identity is the one measured absent from the corpus. This
        // assertion is the defect, pinned so it cannot come back.
        assert_ne!(
            CellEvidenceBinding::derive_event_id(RUN, "validate", CELL, TREE, 2, None),
            PUBLISHED
        );

        // What the binding records instead, and how it resolves.
        let binding = CellEvidenceBinding::for_validate_compared(
            RUN,
            &identity("system-utils/clock-determinism", "custom", "ptrace"),
            TREE,
            2,
        );
        assert_eq!(binding.series_cell, CELL);
        assert!(
            binding.is_represented_by(1, Some(2)),
            "the fold [1,2] represents attempt 2"
        );
        // Controls, so the interval is not accepting everything: a fold that
        // stops short, and an unfolded row at the wrong index.
        assert!(!binding.is_represented_by(1, Some(1)));
        assert!(!binding.is_represented_by(3, Some(4)));
        assert!(!binding.is_represented_by(1, None));
        assert!(
            CellEvidenceBinding::for_validate_compared(
                RUN,
                &identity("system-utils/clock-determinism", "custom", "ptrace"),
                TREE,
                1,
            )
            .is_represented_by(1, None),
            "an unfolded attempt-1 row represents attempt 1"
        );
    }

    #[test]
    fn a_naked_cell_without_a_backend_takes_the_published_native_key() {
        assert_eq!(
            CellEvidenceBinding::series_cell_key(&identity("prog", "naked", "")),
            "prog/naked/native"
        );
        assert_eq!(
            CellEvidenceBinding::series_cell_key(&identity("prog", "verify", "ptrace")),
            "prog/verify/ptrace"
        );
    }

    fn bound() -> CellEvidenceBinding {
        CellEvidenceBinding::for_validate_compared(
            "run-1",
            &identity("prog", "verify", "ptrace"),
            "a".repeat(40).as_str(),
            3,
        )
    }

    #[test]
    fn verify_against_refuses_each_axis_it_claims_and_accepts_a_correct_binding() {
        let tree = "a".repeat(40);
        let cell = identity("prog", "verify", "ptrace");

        bound()
            .verify_against("run-1", &tree, &cell)
            .expect("a correct binding is accepted");

        let error = bound().verify_against("run-2", &tree, &cell).unwrap_err();
        assert!(error.contains("is for run run-1"), "{error}");

        let error = bound()
            .verify_against("run-1", &"b".repeat(40), &cell)
            .unwrap_err();
        assert!(error.contains("is for tree"), "{error}");

        let error = bound()
            .verify_against("run-1", &tree, &identity("prog", "verify", "dbt"))
            .unwrap_err();
        assert!(error.contains("is for cell prog/verify/ptrace"), "{error}");

        let mut foreign_producer = bound();
        foreign_producer.producer = "pressure-test".into();
        let error = foreign_producer
            .verify_against("run-1", &tree, &cell)
            .unwrap_err();
        assert!(error.contains("names producer pressure-test"), "{error}");

        let mut zero = bound();
        zero.selected_attempt = 0;
        let error = zero.verify_against("run-1", &tree, &cell).unwrap_err();
        assert!(error.contains("attempts are one-based"), "{error}");

        // The control in the other direction: a correctly rebuilt binding for
        // each perturbed axis is ACCEPTED, so the refusals above are not
        // passing because everything is refused.
        for (run_id, cell, attempt) in [
            ("run-2", identity("prog", "verify", "ptrace"), 3),
            ("run-1", identity("prog", "verify", "dbt"), 3),
            ("run-1", identity("prog", "verify", "ptrace"), 1),
        ] {
            CellEvidenceBinding::for_validate_compared(run_id, &cell, &tree, attempt)
                .verify_against(run_id, &tree, &cell)
                .expect("a correctly built binding must be accepted");
        }
    }

    #[test]
    fn the_binding_round_trips_and_rejects_an_unknown_field() {
        let binding = bound();
        let encoded = serde_json::to_value(&binding).unwrap();
        // It must not carry a predicted published identity at all.
        assert!(encoded.get("event_id").is_none(), "{encoded}");
        assert_eq!(
            serde_json::from_value::<CellEvidenceBinding>(encoded.clone()).unwrap(),
            binding
        );
        let mut extra = encoded;
        extra["surprise"] = serde_json::json!(true);
        assert!(serde_json::from_value::<CellEvidenceBinding>(extra).is_err());
    }
}
