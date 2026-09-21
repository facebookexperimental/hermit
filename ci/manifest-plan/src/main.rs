//! Copyright (c) Meta Platforms, Inc. and affiliates.
//! All rights reserved.
//!
//! This source code is licensed under the BSD-style license found in the
//! LICENSE file in the root directory of this source tree.
//!
//! Validate the centralized e2e manifests and expand their execution plan.
//!
//! Usage:
//!   cargo run -p hermit-manifest-plan -- --format text
//!   cargo run -p hermit-manifest-plan -- --format json
//!   cargo run -p hermit-manifest-plan -- --format matrix-json
//!   cargo run -p hermit-manifest-plan -- --format harness-json

use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
#[cfg(not(test))]
use std::process::exit;

use hermit_manifest_plan::ci_selection::CiDisabledReasonData;
use hermit_manifest_plan::ci_selection::CiDisabledReasonSpec;
use hermit_manifest_plan::ci_selection::CiSelection;
use hermit_manifest_plan::ci_selection::CiSelectionSpec;
use hermit_manifest_plan::cli_help::is_help_flag;
#[cfg(test)]
use hermit_manifest_plan::runner::REQUIRES_VOCABULARY;
use hermit_manifest_plan::runner::requires_capability;
use hermit_manifest_plan::runner::validate_mode_workdir;
#[cfg(test)]
use hermit_manifest_plan::timeouts::DEFAULT_TEST_CPU_TIMEOUT_SECONDS;
use hermit_manifest_plan::timeouts::DEFAULTS_FILE;
use hermit_manifest_plan::timeouts::MANIFEST_SCHEMA;
use hermit_manifest_plan::timeouts::MAX_TIMEOUT_SECONDS;
use hermit_manifest_plan::timeouts::MIN_TIMEOUT_SECONDS;
use hermit_manifest_plan::timeouts::resolve_timeout_seconds;
use serde::de::DeserializeOwned;
use serde_json::Value as JsonValue;
use serde_json::json;

mod manifest_value;
use manifest_value::Value;

const KNOWN_BACKENDS: [&str; 5] = ["ptrace", "dbt", "kvm", "sabre", "liteinst"];
const MODES: [&str; 5] = ["verify", "chaos", "replay", "naked", "custom"];
const MATRIX_SYMMETRY_BASELINE: &str = "ci/matrix-symmetry-baseline.json";
const CI_REASON_BASELINE: &str = "ci/ci-reason-baseline.json";
const TEST_INVENTORY: &str = "tests/e2e/manifests/inventory/test-files.json";

const HELP: &str = "\
Usage: hermit-manifest-plan [--format <FORMAT>]

Validate the centralized end-to-end manifests and print their expanded plan.

Options:
  --format <FORMAT>  Output format: text, json, matrix-json, harness-json, or
                     host-requirements (default: text)
  -h, --help         Print this help";

/// Every `(capability, test-id)` pair whose manifest token has a reviewed
/// absence proof. This reads declarations only; the machine probe is separate.
fn host_requirement_pairs(documents: &[Value]) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for document in documents {
        let Some(tests) = document.get("test").and_then(Value::as_array) else {
            continue;
        };
        for test in tests {
            let Some(id) = test.get("id").and_then(Value::as_str) else {
                continue;
            };
            let Some(tokens) = test.get("requires").and_then(Value::as_array) else {
                continue;
            };
            for token in tokens {
                let Some(token) = token.as_str() else {
                    return Err(format!("{id}.requires: array values must be strings"));
                };
                if let Some(capability) = requires_capability(token)? {
                    out.push((capability.value().to_string(), id.to_string()));
                }
            }
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

#[derive(Debug)]
struct PlanRow {
    bucket: String,
    id: String,
    lane: String,
    mode: String,
    backend: String,
    ci: bool,
    ci_disabled_reason: Option<CiDisabledReasonData>,
    applicable: bool,
    /// Why this backend is not applicable for this mode, verbatim from the
    /// manifest's `modes.<mode>.backends_disabled.<backend>`.
    ///
    /// ⚠️ THIS IS A DIFFERENT FACT FROM `ci_disabled_reason` AND THE TWO MUST NOT
    /// BE MERGED. `ci_disabled_reason` explains why an ENABLED cell is left out
    /// of ordinary CI; this explains why the cell is NOT APPLICABLE AT ALL. A
    /// cell that was never asked to run cannot have failed, and until this was
    /// carried the scorecard had nowhere to put that distinction, so it rendered
    /// 4,940 never-applicable cells as red.
    ///
    /// `Some` exactly when `applicable` is false; the manifest already requires a
    /// non-empty reason for every disabled backend, so this is never invented.
    not_applicable_reason: Option<String>,
    timeout_seconds: i64,
    cpu_timeout_seconds: i64,
    attempts: Option<i64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Format {
    Text,
    Json,
    MatrixJson,
    HarnessJson,
    /// Which cells declare a `requires` token that HAS an absence proof.
    ///
    /// One line per cell: `<capability-name>\t<test-id>`. Empty output means no
    /// cell in the corpus can be withheld by any probe, whatever the machine
    /// looks like. This emits the DECLARATIONS only and never probes; the
    /// machine question is asked separately, so the two halves of the decision
    /// cannot be conflated.
    HostRequirements,
}

#[cfg(not(test))]
fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("manifest-plan: {msg}");
    exit(1);
}

#[cfg(test)]
fn die(msg: impl std::fmt::Display) -> ! {
    panic!("manifest-plan: {msg}");
}

fn parse_format() -> Option<Format> {
    let mut format = Format::Text;
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if matches!(arguments.as_slice(), [flag] if is_help_flag(flag)) {
        println!("{HELP}");
        return None;
    }
    let mut args = arguments.into_iter();
    while let Some(arg) = args.next() {
        let value = if arg == "--format" {
            args.next()
                .unwrap_or_else(|| die("--format requires a value"))
        } else if let Some(value) = arg.strip_prefix("--format=") {
            value.to_string()
        } else {
            die(format!("unknown argument: {arg}"));
        };
        format = match value.as_str() {
            "text" => Format::Text,
            "json" => Format::Json,
            "matrix-json" => Format::MatrixJson,
            "harness-json" => Format::HarnessJson,
            "host-requirements" => Format::HostRequirements,
            _ => die(format!("unknown format: {value}")),
        };
    }
    Some(format)
}

fn main() {
    let Some(format) = parse_format() else {
        return;
    };
    let script_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/e2e/manifests");
    let repo_root = script_dir.join("../../..");

    let defaults_path = script_dir.join(DEFAULTS_FILE);
    let defaults_text = std::fs::read_to_string(&defaults_path)
        .unwrap_or_else(|error| die(format!("cannot read {}: {error}", defaults_path.display())));
    let defaults: Value = defaults_text.parse().unwrap_or_else(|error| {
        die(format!(
            "{}: invalid YAML: {error}",
            defaults_path.display()
        ))
    });
    ensure_keys(
        &defaults,
        &[
            "schema",
            "timeout_seconds",
            "cpu_timeout_seconds",
            "nextest",
        ],
        DEFAULTS_FILE,
    );
    if defaults.get("schema").and_then(Value::as_integer) != Some(MANIFEST_SCHEMA as i64) {
        die(format!("{DEFAULTS_FILE}: schema must be {MANIFEST_SCHEMA}"));
    }
    let global_timeout_seconds = required_timeout_seconds(
        defaults.get("timeout_seconds"),
        "global default.timeout_seconds",
    );
    let global_cpu_timeout_seconds = required_timeout_seconds(
        defaults.get("cpu_timeout_seconds"),
        "global default.cpu_timeout_seconds",
    );
    if let Some(nextest) = defaults.get("nextest") {
        let entries = nextest
            .as_array()
            .unwrap_or_else(|| die("defaults.nextest must be an array"));
        let mut filters = BTreeSet::new();
        for (index, entry) in entries.iter().enumerate() {
            let context = format!("defaults.nextest[{index}]");
            ensure_keys(
                entry,
                &["filter", "timeout_seconds", "slow_reason"],
                &context,
            );
            let filter = required_string(entry, "filter", &context);
            let timeout = required_timeout_seconds(entry.get("timeout_seconds"), &context);
            if timeout == global_timeout_seconds {
                die(format!(
                    "{context}: timeout_seconds redundantly repeats the global default"
                ));
            }
            required_string(entry, "slow_reason", &context);
            if !filters.insert(filter) {
                die(format!(
                    "{context}: duplicate nextest timeout filter `{filter}`"
                ));
            }
        }
    }

    let mut manifests: Vec<PathBuf> = std::fs::read_dir(&script_dir)
        .unwrap_or_else(|error| die(format!("cannot read {}: {error}", script_dir.display())))
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "yaml")
                && path.file_name().is_some_and(|name| name != DEFAULTS_FILE)
        })
        .collect();
    manifests.sort();
    if manifests.is_empty() {
        die(format!(
            "no *.yaml manifests found in {}",
            script_dir.display()
        ));
    }

    let mut rows = Vec::new();
    let mut seen_ids = BTreeSet::new();
    let mut seen_programs = BTreeSet::new();
    let mut documents = Vec::new();

    for path in &manifests {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|error| die(format!("cannot read {}: {error}", path.display())));
        let document: Value = text
            .parse()
            .unwrap_or_else(|error| die(format!("{}: invalid YAML: {error}", path.display())));
        let location = path.file_name().unwrap().to_string_lossy().to_string();
        ensure_keys(
            &document,
            &["schema", "bucket", "timeout_seconds", "slow_reason", "test"],
            &location,
        );

        if document.get("schema").and_then(Value::as_integer) != Some(MANIFEST_SCHEMA as i64) {
            die(format!("{location}: schema must be {MANIFEST_SCHEMA}"));
        }
        let bucket = required_string(&document, "bucket", &location);
        let stem = path.file_stem().unwrap().to_string_lossy();
        if bucket != stem {
            die(format!(
                "{location}: bucket `{bucket}` must equal file stem `{stem}`"
            ));
        }
        let bucket_timeout_seconds = document.get("timeout_seconds").map(|value| {
            required_timeout_seconds(Some(value), &format!("{bucket}.timeout_seconds"))
        });
        let bucket_reason = document
            .get("slow_reason")
            .map(|_| required_string(&document, "slow_reason", bucket));
        match (bucket_timeout_seconds, bucket_reason) {
            (Some(timeout), Some(_)) if timeout != global_timeout_seconds => {}
            (Some(_), Some(_)) => die(format!(
                "{bucket}: bucket timeout_seconds redundantly repeats the global default"
            )),
            (Some(_), _) => die(format!(
                "{bucket}: bucket timeout_seconds requires a non-empty slow_reason"
            )),
            (None, Some(_)) => die(format!(
                "{bucket}: bucket slow_reason has no timeout_seconds"
            )),
            (None, None) => {}
        }
        let inherited_timeout_seconds = resolve_timeout_seconds(
            global_timeout_seconds as u64,
            bucket_timeout_seconds.map(|value| value as u64),
            None,
        ) as i64;
        let tests = document
            .get("test")
            .and_then(Value::as_array)
            .filter(|tests| !tests.is_empty())
            .unwrap_or_else(|| die(format!("{location}: missing non-empty [[test]] array")));
        for test in tests {
            validate_and_expand(
                test,
                bucket,
                inherited_timeout_seconds,
                global_cpu_timeout_seconds,
                &location,
                &repo_root,
                &mut seen_ids,
                &mut seen_programs,
                &mut rows,
            );
        }
        documents.push(document);
    }

    let inventory = load_test_inventory(&repo_root);
    validate_test_inventory(&repo_root, &inventory, &seen_programs);
    validate_front_door(&repo_root, &documents, &inventory);
    validate_ci_reason_baseline(&repo_root, &documents);

    rows.sort_by(|left, right| {
        (&left.bucket, &left.id, &left.mode, &left.backend).cmp(&(
            &right.bucket,
            &right.id,
            &right.mode,
            &right.backend,
        ))
    });

    match format {
        Format::HostRequirements => {
            let pairs = host_requirement_pairs(&documents)
                .unwrap_or_else(|error| die(format!("cannot resolve `requires`: {error}")));
            for (capability, id) in pairs {
                println!("{capability}\t{id}");
            }
        }
        Format::HarnessJson => {
            println!(
                "{}",
                serde_json::to_string(&documents)
                    .unwrap_or_else(|error| die(format!("cannot encode manifests: {error}")))
            );
        }
        Format::Json => {
            let output: Vec<_> = rows
                .iter()
                .filter(|row| row.applicable)
                .map(|row| {
                    json!({
                        "bucket": row.bucket,
                        "test": row.id,
                        "lane": row.lane,
                        "mode": row.mode,
                        "backend": row.backend,
                        "ci": row.ci,
                        "ci_disabled_reason": row.ci_disabled_reason,
                    })
                })
                .collect();
            println!(
                "{}",
                serde_json::to_string(&output)
                    .unwrap_or_else(|error| die(format!("cannot encode plan: {error}")))
            );
        }
        Format::MatrixJson => {
            let output: Vec<_> = rows
                .iter()
                .map(|row| {
                    json!({
                        "bucket": row.bucket,
                        "test": row.id,
                        "lane": row.lane,
                        "mode": row.mode,
                        "backend": row.backend,
                        "ci": row.ci,
                        "ci_disabled_reason": row.ci_disabled_reason,
                        "not_applicable_reason": row.not_applicable_reason,
                        "timeout_seconds": row.timeout_seconds,
                        "cpu_timeout_seconds": row.cpu_timeout_seconds,
                        "attempts": row.attempts,
                    })
                })
                .collect();
            println!(
                "{}",
                serde_json::to_string(&output)
                    .unwrap_or_else(|error| die(format!("cannot encode matrix: {error}")))
            );
        }
        Format::Text => {
            println!(
                "{:<10}\t{:<38}\t{:<10}\t{:<8}\t{:<5}\tBUCKET",
                "LANE", "TEST", "MODE", "BACKEND", "CI"
            );
            for row in rows.iter().filter(|row| row.applicable) {
                println!(
                    "{:<10}\t{:<38}\t{:<10}\t{:<8}\t{:<5}\t{}",
                    row.lane, row.id, row.mode, row.backend, row.ci, row.bucket
                );
            }
            eprintln!(
                "\nPASS: {} manifest(s), {} test(s), {} applicable manifest cells validated",
                manifests.len(),
                seen_ids.len(),
                rows.iter().filter(|row| row.applicable).count()
            );
        }
    }
}

fn json_string_set(value: &JsonValue, key: &str, location: &str) -> BTreeSet<String> {
    let values = value
        .get(key)
        .and_then(JsonValue::as_array)
        .unwrap_or_else(|| die(format!("{location}: `{key}` must be an array")));
    let result: BTreeSet<String> = values
        .iter()
        .map(|item| {
            item.as_str()
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    die(format!(
                        "{location}: `{key}` entries must be non-empty strings"
                    ))
                })
        })
        .collect();
    if result.len() != values.len() {
        die(format!("{location}: `{key}` contains duplicate entries"));
    }
    result
}

fn names_backend(value: &str) -> bool {
    value
        .split(|character: char| !character.is_ascii_alphanumeric())
        .any(|token| {
            let token = token.to_ascii_lowercase();
            matches!(
                token.as_str(),
                "ptrace" | "dbt" | "dynamorio" | "kvm" | "sabre" | "e9patch"
            ) || token.starts_with("liteinst")
        })
}

fn backend_private_guest_files(inventory: &JsonValue) -> BTreeSet<String> {
    inventory
        .get("files")
        .and_then(JsonValue::as_array)
        .unwrap_or_else(|| die(format!("{TEST_INVENTORY}: `files` must be an array")))
        .iter()
        .filter(|entry| {
            entry.get("disposition").and_then(JsonValue::as_str) == Some("guest-fixture")
        })
        .filter_map(|entry| {
            let path = entry.get("path").and_then(JsonValue::as_str)?;
            let runner = entry
                .get("runner")
                .and_then(JsonValue::as_str)
                .unwrap_or("");
            let parity_private = path.starts_with("tests/backend-parity/")
                || runner.contains("tests/backend-parity/");
            (parity_private || names_backend(path) || names_backend(runner))
                .then(|| path.to_string())
        })
        .collect()
}

fn asymmetric_manifest_tests(documents: &[Value]) -> BTreeSet<String> {
    let mut asymmetric = BTreeSet::new();
    for document in documents {
        for test in document
            .get("test")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(id) = test.get("id").and_then(Value::as_str) else {
                continue;
            };
            let Some(modes) = test.get("modes").and_then(Value::as_table) else {
                continue;
            };
            let mut has_ptrace_front_door = false;
            let mut has_backend_without_ptrace = false;
            for mode in MODES.into_iter().filter(|mode| *mode != "naked") {
                let enabled = modes
                    .get(mode)
                    .and_then(Value::as_table)
                    .and_then(|spec| spec.get("backends_enabled"))
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>();
                let has_ptrace = enabled.contains(&"ptrace");
                has_ptrace_front_door |= has_ptrace;
                has_backend_without_ptrace |= !enabled.is_empty() && !has_ptrace;
            }
            if !has_ptrace_front_door || has_backend_without_ptrace {
                asymmetric.insert(id.to_string());
            }
        }
    }
    asymmetric
}

fn enforce_exact_ratchet(label: &str, actual: &BTreeSet<String>, baseline: &BTreeSet<String>) {
    let unexpected: Vec<_> = actual.difference(baseline).cloned().collect();
    let stale: Vec<_> = baseline.difference(actual).cloned().collect();
    if !unexpected.is_empty() || !stale.is_empty() {
        die(format!(
            "matrix symmetry {label} changed; unexpected={unexpected:?}, stale_baseline={stale:?}. New compatibility coverage must enter a shared schema-v3 YAML manifest, establish ptrace first, and declare every backend/mode cell; remove migrated debt from {MATRIX_SYMMETRY_BASELINE}"
        ));
    }
}

/// Every `ci = false` mode that states no reason and enables no backend, named
/// `<test id>::<mode>`.
///
/// WHY THIS IS NOT ALREADY COVERED. `ci_selection.rs` refuses a reasonless
/// `ci = false` mode -- but only when the mode still enables a backend. Its
/// match arm `(false, true, None)` returns an empty map, so a mode that is off
/// for every backend needs no reason at all. Measured on this corpus, 932 of
/// 1467 `ci = false` modes are in exactly that state: switched off, and silent
/// about why. Those are the ones nobody can audit, because there is nothing
/// written down to audit.
///
/// A COUNT WOULD NOT DO. Recording how many exist per bucket lets a change add
/// one unreasoned cell and write a reason for a different cell in the same
/// bucket: the count is unchanged and validation passes. Naming each cell is
/// what makes that swap visible, which is why the baseline is a set.
fn unreasoned_ci_false_cells(documents: &[Value]) -> BTreeSet<String> {
    let mut silent = BTreeSet::new();
    for document in documents {
        for test in document
            .get("test")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(id) = test.get("id").and_then(Value::as_str) else {
                continue;
            };
            let Some(modes) = test.get("modes").and_then(Value::as_table) else {
                continue;
            };
            for mode in MODES {
                let Some(spec) = modes.get(mode).and_then(Value::as_table) else {
                    continue;
                };
                if spec.get("ci").and_then(Value::as_bool) != Some(false) {
                    continue;
                }
                let enables_a_backend = spec
                    .get("backends_enabled")
                    .and_then(Value::as_array)
                    .is_some_and(|backends| !backends.is_empty());
                if enables_a_backend {
                    // ci_selection.rs already refuses this one without a reason.
                    continue;
                }
                let states_a_reason = match spec.get("ci_disabled_reason") {
                    None => false,
                    // A structured per-backend reason counts as stated; a plain
                    // string only counts when it actually says something.
                    Some(reason) => match reason.as_str() {
                        Some(text) => !text.trim().is_empty(),
                        None => true,
                    },
                };
                if !states_a_reason {
                    silent.insert(format!("{id}::{mode}"));
                }
            }
        }
    }
    silent
}

/// Ratchet the silent default-off cells: the recorded set and the observed set
/// must match exactly.
///
/// Exact match in BOTH directions is deliberate. A new silent cell is refused
/// by name, and writing a reason for a recorded one is refused until the
/// baseline is updated too -- which is what stops a compensating swap from
/// passing unnoticed.
fn validate_ci_reason_baseline(repo_root: &Path, documents: &[Value]) {
    let baseline_path = repo_root.join(CI_REASON_BASELINE);
    let baseline_text = std::fs::read_to_string(&baseline_path)
        .unwrap_or_else(|error| die(format!("cannot read {}: {error}", baseline_path.display())));
    let baseline: JsonValue = serde_json::from_str(&baseline_text).unwrap_or_else(|error| {
        die(format!(
            "{}: invalid JSON: {error}",
            baseline_path.display()
        ))
    });
    let baseline_keys: BTreeSet<_> = baseline
        .as_object()
        .unwrap_or_else(|| die(format!("{CI_REASON_BASELINE}: expected an object")))
        .keys()
        .map(String::as_str)
        .collect();
    let expected_keys: BTreeSet<_> = ["schema", "unreasoned_ci_false_cells"]
        .into_iter()
        .collect();
    if baseline_keys != expected_keys {
        die(format!(
            "{CI_REASON_BASELINE}: keys must be exactly {expected_keys:?}, got {baseline_keys:?}"
        ));
    }
    if baseline.get("schema").and_then(JsonValue::as_u64) != Some(1) {
        die(format!("{CI_REASON_BASELINE}: schema must be 1"));
    }
    let recorded = json_string_set(&baseline, "unreasoned_ci_false_cells", CI_REASON_BASELINE);
    let observed = unreasoned_ci_false_cells(documents);
    let added: Vec<_> = observed.difference(&recorded).cloned().collect();
    let removed: Vec<_> = recorded.difference(&observed).cloned().collect();
    if !added.is_empty() {
        die(format!(
            "a `ci = false` mode states no ci_disabled_reason and enables no backend: {added:?}. \
             Every default-off cell must say why it is off, in its own entry. If this is \
             deliberate and temporary, say so in ci_disabled_reason; do not add it to \
             {CI_REASON_BASELINE}, which records existing debt only."
        ));
    }
    if !removed.is_empty() {
        die(format!(
            "these cells now state a reason and must be removed from {CI_REASON_BASELINE}: \
             {removed:?}. The baseline is an exact set so that adding one silent cell while \
             fixing another cannot pass unnoticed."
        ));
    }
}

fn load_test_inventory(repo_root: &Path) -> JsonValue {
    let inventory_path = repo_root.join(TEST_INVENTORY);
    let inventory_text = std::fs::read_to_string(&inventory_path)
        .unwrap_or_else(|error| die(format!("cannot read {}: {error}", inventory_path.display())));
    serde_json::from_str(&inventory_text).unwrap_or_else(|error| {
        die(format!(
            "{}: invalid JSON: {error}",
            inventory_path.display()
        ))
    })
}

fn inventory_string<'a>(entry: &'a JsonValue, key: &str, location: &str) -> &'a str {
    entry
        .get(key)
        .and_then(JsonValue::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| die(format!("{location}: `{key}` must be a non-empty string")))
}

/// Require the inventory to be an exact partition of the test tree.
///
/// Git is the population authority on purpose: tracked files and genuinely new
/// untracked files qualify, while ignored compiler/test output does not. This is
/// the same population used by `ci/merge-test-inventory.py` when it prunes stale
/// entries, so the writer and checker cannot disagree about what must be owned.
fn validate_test_inventory(
    repo_root: &Path,
    inventory: &JsonValue,
    manifest_programs: &BTreeSet<String>,
) {
    let object = inventory
        .as_object()
        .unwrap_or_else(|| die(format!("{TEST_INVENTORY}: expected an object")));
    let actual_keys: BTreeSet<_> = object.keys().map(String::as_str).collect();
    let expected_keys: BTreeSet<_> = ["files", "schema"].into_iter().collect();
    if actual_keys != expected_keys {
        die(format!(
            "{TEST_INVENTORY}: keys must be exactly {expected_keys:?}, got {actual_keys:?}"
        ));
    }
    if inventory.get("schema").and_then(JsonValue::as_u64) != Some(2) {
        die(format!("{TEST_INVENTORY}: schema must be 2"));
    }
    let files = inventory
        .get("files")
        .and_then(JsonValue::as_array)
        .filter(|files| !files.is_empty())
        .unwrap_or_else(|| {
            die(format!(
                "{TEST_INVENTORY}: `files` must be a non-empty array"
            ))
        });

    let mut paths = Vec::with_capacity(files.len());
    let mut inventory_manifest_tests = BTreeSet::new();
    for (index, entry) in files.iter().enumerate() {
        let location = format!("{TEST_INVENTORY}: files[{index}]");
        let entry_object = entry
            .as_object()
            .unwrap_or_else(|| die(format!("{location}: expected an object")));
        let actual_keys: BTreeSet<_> = entry_object.keys().map(String::as_str).collect();
        let expected_keys: BTreeSet<_> = ["disposition", "path", "runner", "why"]
            .into_iter()
            .collect();
        if actual_keys != expected_keys {
            die(format!(
                "{location}: keys must be exactly {expected_keys:?}, got {actual_keys:?}"
            ));
        }
        let path = inventory_string(entry, "path", &location);
        let disposition = inventory_string(entry, "disposition", &location);
        let _runner = inventory_string(entry, "runner", &location);
        let _why = inventory_string(entry, "why", &location);
        if !path.starts_with("tests/") || path.contains("..") {
            die(format!(
                "{location}: `path` must stay below tests/ without `..`: {path}"
            ));
        }
        if disposition == "manifest-test" {
            inventory_manifest_tests.insert(path.to_string());
        }
        paths.push(path.to_string());
    }

    if paths.windows(2).any(|pair| pair[0] > pair[1]) {
        die(format!(
            "{TEST_INVENTORY}: files must be sorted by path; run ci/merge-test-inventory.py to canonicalize"
        ));
    }
    let registered: BTreeSet<_> = paths.iter().cloned().collect();
    if registered.len() != paths.len() {
        let mut seen = BTreeSet::new();
        let duplicates: Vec<_> = paths
            .iter()
            .filter(|path| !seen.insert((*path).clone()))
            .cloned()
            .collect();
        die(format!(
            "{TEST_INVENTORY}: duplicate paths: {}",
            duplicates.join(", ")
        ));
    }

    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
            "tests",
        ])
        .output()
        .unwrap_or_else(|error| die(format!("cannot enumerate tests/ with git: {error}")));
    if !output.status.success() {
        die(format!(
            "cannot enumerate tests/ with git: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let present_text = String::from_utf8(output.stdout)
        .unwrap_or_else(|_| die("git ls-files returned a non-UTF-8 test path"));
    let present: BTreeSet<_> = present_text.lines().map(str::to_string).collect();
    let unregistered: Vec<_> = present.difference(&registered).cloned().collect();
    let phantoms: Vec<_> = registered.difference(&present).cloned().collect();
    if !unregistered.is_empty() || !phantoms.is_empty() {
        die(format!(
            "test inventory is stale; every file in tests/ must have an explicit disposition; unregistered={unregistered:?}, phantom={phantoms:?}"
        ));
    }

    if &inventory_manifest_tests != manifest_programs {
        let missing: Vec<_> = manifest_programs
            .difference(&inventory_manifest_tests)
            .cloned()
            .collect();
        let stale: Vec<_> = inventory_manifest_tests
            .difference(manifest_programs)
            .cloned()
            .collect();
        die(format!(
            "manifest programs and disposition=manifest-test inventory entries differ; missing={missing:?}, stale={stale:?}"
        ));
    }
}

fn validate_front_door(repo_root: &Path, documents: &[Value], inventory: &JsonValue) {
    let baseline_path = repo_root.join(MATRIX_SYMMETRY_BASELINE);
    let baseline_text = std::fs::read_to_string(&baseline_path)
        .unwrap_or_else(|error| die(format!("cannot read {}: {error}", baseline_path.display())));
    let baseline: JsonValue = serde_json::from_str(&baseline_text).unwrap_or_else(|error| {
        die(format!(
            "{}: invalid JSON: {error}",
            baseline_path.display()
        ))
    });
    let baseline_keys: BTreeSet<_> = baseline
        .as_object()
        .unwrap_or_else(|| die(format!("{MATRIX_SYMMETRY_BASELINE}: expected an object")))
        .keys()
        .map(String::as_str)
        .collect();
    let expected_keys: BTreeSet<_> = [
        "schema",
        "asymmetric_manifest_tests",
        "backend_private_guest_files",
    ]
    .into_iter()
    .collect();
    if baseline_keys != expected_keys {
        die(format!(
            "{MATRIX_SYMMETRY_BASELINE}: keys must be exactly {expected_keys:?}, got {baseline_keys:?}"
        ));
    }
    if baseline.get("schema").and_then(JsonValue::as_u64) != Some(1) {
        die(format!("{MATRIX_SYMMETRY_BASELINE}: schema must be 1"));
    }
    let expected_asymmetric = json_string_set(
        &baseline,
        "asymmetric_manifest_tests",
        MATRIX_SYMMETRY_BASELINE,
    );
    let expected_private = json_string_set(
        &baseline,
        "backend_private_guest_files",
        MATRIX_SYMMETRY_BASELINE,
    );

    enforce_exact_ratchet(
        "manifest ptrace-front-door debt",
        &asymmetric_manifest_tests(documents),
        &expected_asymmetric,
    );
    enforce_exact_ratchet(
        "backend-private guest debt",
        &backend_private_guest_files(inventory),
        &expected_private,
    );
}

fn required_string<'a>(value: &'a Value, key: &str, location: &str) -> &'a str {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| die(format!("{location}: missing non-empty string `{key}`")))
}

fn string_array(value: Option<&Value>, location: &str) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .unwrap_or_else(|| die(format!("{location}: expected an array")))
        .iter()
        .map(|item| {
            item.as_str()
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| die(format!("{location}: array values must be strings")))
        })
        .collect()
}

fn parse_schema_value<T: DeserializeOwned>(value: &Value, location: &str) -> T {
    let value = serde_json::to_value(value)
        .unwrap_or_else(|error| die(format!("{location} cannot be encoded: {error}")));
    serde_json::from_value(value)
        .unwrap_or_else(|error| die(format!("{location} has invalid shape: {error}")))
}

fn validate_direct(value: &Value, id: &str) {
    match value {
        Value::String(command) if !command.trim().is_empty() => {}
        Value::String(_) => die(format!("{id}: direct command must not be empty")),
        Value::Array(_) => {
            if string_array(Some(value), &format!("{id}.direct")).is_empty() {
                die(format!("{id}: direct argv must not be empty"));
            }
        }
        _ => die(format!(
            "{id}: direct must be a shell command string or an argv array"
        )),
    }
}

fn ensure_keys(value: &Value, allowed: &[&str], location: &str) {
    let table = value
        .as_table()
        .unwrap_or_else(|| die(format!("{location}: expected a table")));
    let allowed: BTreeSet<_> = allowed.iter().copied().collect();
    let actual: BTreeSet<_> = table.keys().map(String::as_str).collect();
    let unknown: Vec<_> = actual.difference(&allowed).copied().collect();
    if !unknown.is_empty() {
        die(format!("{location}: unknown keys: {unknown:?}"));
    }
}

fn required_timeout_seconds(value: Option<&Value>, context: &str) -> i64 {
    match value.and_then(Value::as_integer) {
        Some(timeout)
            if (MIN_TIMEOUT_SECONDS as i64..=MAX_TIMEOUT_SECONDS as i64).contains(&timeout) =>
        {
            timeout
        }
        other => die(format!(
            "{context} must be {MIN_TIMEOUT_SECONDS}..={MAX_TIMEOUT_SECONDS}, got {other:?}"
        )),
    }
}

fn cell_timeout_overrides(
    spec: &Value,
    id: &str,
    mode: &str,
    enabled: &BTreeSet<String>,
    _inherited_timeout_seconds: i64,
    field: &str,
) -> std::collections::BTreeMap<String, i64> {
    let timeouts = spec
        .get(field)
        .map(|value| {
            value.as_table().unwrap_or_else(|| {
                die(format!("{id}.modes.{mode}.{field} must be a backend table"))
            })
        })
        .cloned()
        .unwrap_or_default();
    let reasons = spec
        .get("slow_reason")
        .map(|value| {
            value.as_table().unwrap_or_else(|| {
                die(format!(
                    "{id}.modes.{mode}.slow_reason must be a backend table"
                ))
            })
        })
        .cloned()
        .unwrap_or_default();
    let mut resolved = std::collections::BTreeMap::new();
    for (backend, value) in timeouts {
        if !enabled.contains(&backend) {
            die(format!(
                "{id}.modes.{mode}.{field} names disabled backend `{backend}`"
            ));
        }
        let timeout = required_timeout_seconds(
            Some(&value),
            &format!("{id}.modes.{mode}.{field}.{backend}"),
        );
        let reason = reasons
            .get(&backend)
            .unwrap_or_else(|| {
                die(format!(
                    "{id}.modes.{mode}.{field}.{backend} requires slow_reason"
                ))
            })
            .as_str()
            .unwrap_or_else(|| {
                die(format!(
                    "{id}.modes.{mode}.slow_reason.{backend} must be a string"
                ))
            });
        if reason.trim().is_empty() {
            die(format!(
                "{id}.modes.{mode}.slow_reason.{backend} must be non-empty"
            ));
        }
        resolved.insert(backend, timeout);
    }
    resolved
}

fn is_file_or_symlink(path: &Path) -> bool {
    path.is_file()
        || std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
}

#[allow(clippy::too_many_arguments)]
fn validate_and_expand(
    test: &Value,
    bucket: &str,
    inherited_timeout_seconds: i64,
    inherited_cpu_timeout_seconds: i64,
    location: &str,
    repo_root: &Path,
    seen_ids: &mut BTreeSet<String>,
    seen_programs: &mut BTreeSet<String>,
    rows: &mut Vec<PlanRow>,
) {
    let id = required_string(test, "id", location).to_string();
    ensure_keys(
        test,
        &[
            "id",
            "description",
            "lane",
            "requires",
            "occasional",
            "program",
            "direct",
            "observation",
            "build",
            "modes",
            "preprocessors",
        ],
        &id,
    );
    if !id.starts_with(&format!("{bucket}/"))
        || !id.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || "-/".contains(character)
        })
        || id
            .strip_prefix(&format!("{bucket}/"))
            .is_none_or(|suffix| suffix.is_empty() || suffix.starts_with('-'))
    {
        die(format!(
            "{location}: id `{id}` must be lowercase and start with `{bucket}/`"
        ));
    }
    if !seen_ids.insert(id.clone()) {
        die(format!("duplicate test id across manifests: {id}"));
    }
    required_string(test, "description", &id);

    let lane = required_string(test, "lane", &id);
    if lane != "portable" && lane != "privileged" {
        die(format!(
            "{id}: lane must be portable|privileged, got `{lane}`"
        ));
    }
    if test.get("occasional").and_then(Value::as_bool).is_none() {
        die(format!("{id}: occasional must be a boolean"));
    }
    // `requires` is a GATE, not documentation. Every token must be in the closed
    // vocabulary; an unrecognized one refuses the whole run here, before any
    // plan is emitted and long before any cell could be withheld.
    for token in string_array(test.get("requires"), &format!("{id}.requires")) {
        if let Err(why) = requires_capability(&token) {
            die(format!("{id}.requires: {why}"));
        }
    }

    let program = test.get("program").and_then(Value::as_str);
    let direct = test.get("direct");
    let mut program_path = None;
    match (program, direct) {
        (Some(_), Some(_)) => die(format!("{id}: set only one of `program`/`direct`")),
        (None, None) => die(format!("{id}: must set `program` or `direct`")),
        (Some(program), None) => {
            let extension = Path::new(program)
                .extension()
                .and_then(|extension| extension.to_str())
                .unwrap_or("");
            if !["sh", "c", "rs"].contains(&extension) {
                die(format!("{id}: program `{program}` must end in .sh/.c/.rs"));
            }
            if !program.starts_with("tests/") || program.split('/').any(|part| part == "..") {
                die(format!(
                    "{id}: program must be a repo-relative path below tests/: {program}"
                ));
            }
            let path = repo_root.join(program);
            if !is_file_or_symlink(&path) {
                die(format!("{id}: program path does not exist: {program}"));
            }
            program_path = Some(path);
            if !seen_programs.insert(program.to_string()) {
                die(format!(
                    "program appears in multiple manifest tests: {program}"
                ));
            }
        }
        (None, Some(direct)) => validate_direct(direct, &id),
    }

    if let Some(build) = test.get("build") {
        ensure_keys(build, &["cflags", "rustflags"], &format!("{id}.build"));
        for key in ["cflags", "rustflags"] {
            if build.get(key).is_some() {
                let _flags = string_array(build.get(key), &format!("{id}.build.{key}"));
            }
        }
    }
    if let Some(preprocessors) = test.get("preprocessors") {
        let preprocessors = string_array(Some(preprocessors), &format!("{id}.preprocessors"));
        if preprocessors.iter().any(|value| value != "e9patch") {
            die(format!("{id}: the only supported preprocessor is e9patch"));
        }
    }

    validate_observation(test, &id);

    let modes = test
        .get("modes")
        .and_then(Value::as_table)
        .unwrap_or_else(|| die(format!("{id}: missing [test.modes]")));
    let actual_modes: BTreeSet<_> = modes.keys().map(String::as_str).collect();
    let expected_modes: BTreeSet<_> = MODES.into_iter().collect();
    if actual_modes != expected_modes {
        die(format!(
            "{id}: modes must be exactly {:?}, got {:?}",
            expected_modes, actual_modes
        ));
    }

    let row_start = rows.len();
    for mode in MODES {
        validate_mode_with_cpu(
            &id,
            bucket,
            lane,
            mode,
            (inherited_timeout_seconds, inherited_cpu_timeout_seconds),
            modes.get(mode).unwrap(),
            rows,
        );
    }
    if rows[row_start..].iter().any(|row| row.applicable && row.ci)
        && program_path.as_ref().is_some_and(|path| !path.is_file())
    {
        die(format!(
            "{id}: program selected by full has an unavailable symlink target: {}",
            program.unwrap()
        ));
    }
}

fn validate_observation(test: &Value, id: &str) {
    let observation_value = test
        .get("observation")
        .unwrap_or_else(|| die(format!("{id}: observation must be a table")));
    ensure_keys(
        observation_value,
        &["status", "stdout", "stderr", "artifacts"],
        &format!("{id}.observation"),
    );
    let observation = observation_value.as_table().unwrap();
    for key in ["status", "stdout", "stderr"] {
        if observation.get(key).and_then(Value::as_bool).is_none() {
            die(format!("{id}: observation.{key} must be a boolean"));
        }
    }
    for artifact in string_array(
        observation.get("artifacts"),
        &format!("{id}.observation.artifacts"),
    ) {
        if artifact.starts_with('/') || artifact.split('/').any(|part| part == "..") {
            die(format!(
                "{id}: observation artifact must stay below E2E_TMPDIR: {artifact}"
            ));
        }
    }
}

#[cfg(test)]
fn validate_mode(
    id: &str,
    bucket: &str,
    lane: &str,
    mode: &str,
    inherited_timeout_seconds: i64,
    spec: &Value,
    rows: &mut Vec<PlanRow>,
) {
    validate_mode_with_cpu(
        id,
        bucket,
        lane,
        mode,
        (
            inherited_timeout_seconds,
            DEFAULT_TEST_CPU_TIMEOUT_SECONDS as i64,
        ),
        spec,
        rows,
    );
}

fn validate_mode_with_cpu(
    id: &str,
    bucket: &str,
    lane: &str,
    mode: &str,
    inherited_timeouts: (i64, i64),
    spec: &Value,
    rows: &mut Vec<PlanRow>,
) {
    let (inherited_timeout_seconds, inherited_cpu_timeout_seconds) = inherited_timeouts;
    let spec_value = spec;
    let spec = spec_value
        .as_table()
        .unwrap_or_else(|| die(format!("{id}: modes.{mode} must be a table")));
    let mut allowed = vec![
        "ci",
        "ci_disabled_reason",
        "backends_enabled",
        "backends_disabled",
        "guest_args",
        "workdir",
        "timeout_seconds",
        "cpu_timeout_seconds",
        "slow_reason",
    ];
    match mode {
        "naked" => allowed.extend(["runs", "assert"]),
        "chaos" => allowed.extend(["seeds", "assert", "outcome_classes"]),
        "custom" => allowed.extend(["args", "assert"]),
        // `verify` accepts one assertion: `bitwise_parity`, which upgrades the
        // cell from the lossy default comparator to the L2 parity comparator and
        // requires the run's own verdict JSON to report parity. Without it a
        // `verify` cell runs `--strict --verify` only, which per
        // AGENTS.md "cannot establish L2" -- so a cell justified by a
        // hand-measured `bitwise_parity: true` does not actually ratchet it.
        "verify" => allowed.extend([
            "assert",
            "compare_io_buffers",
            "compare_io_buffers_disabled_reason",
            "rcb_time",
            "rcb_time_disabled_reason",
        ]),
        _ => {}
    }
    ensure_keys(spec_value, &allowed, &format!("{id}.modes.{mode}"));
    let workdir = spec.get("workdir").map(|workdir| {
        workdir
            .as_str()
            .unwrap_or_else(|| die(format!("{id}: modes.{mode}.workdir must be a string")))
    });
    if mode == "verify" {
        if let Some(assert) = spec.get("assert") {
            ensure_keys(
                assert,
                &["bitwise_parity"],
                &format!("{id}.modes.verify.assert"),
            );
            if let Some(value) = assert.get("bitwise_parity") {
                if value.as_bool().is_none() {
                    die(format!(
                        "{id}: modes.verify.assert.bitwise_parity must be a boolean"
                    ));
                }
            }
        }
        let compare_io_buffers = spec.get("compare_io_buffers").map(|value| {
            value.as_bool().unwrap_or_else(|| {
                die(format!(
                    "{id}: modes.verify.compare_io_buffers must be a boolean"
                ))
            })
        });
        let disabled_reason = spec.get("compare_io_buffers_disabled_reason").map(|value| {
            value.as_str().unwrap_or_else(|| {
                die(format!(
                    "{id}: modes.verify.compare_io_buffers_disabled_reason must be a string"
                ))
            })
        });
        match (compare_io_buffers, disabled_reason) {
            (Some(false), Some(reason)) if !reason.trim().is_empty() => {}
            (Some(false), _) => die(format!(
                "{id}: modes.verify.compare_io_buffers=false requires a substantive compare_io_buffers_disabled_reason"
            )),
            (None | Some(true), Some(_)) => die(format!(
                "{id}: modes.verify comparison reason is stale while I/O-buffer comparison is enabled"
            )),
            (None | Some(true), None) => {}
        }
        let rcb_time = spec.get("rcb_time").map(|value| {
            value
                .as_bool()
                .unwrap_or_else(|| die(format!("{id}: modes.verify.rcb_time must be a boolean")))
        });
        let rcb_time_disabled_reason = spec.get("rcb_time_disabled_reason").map(|value| {
            value.as_str().unwrap_or_else(|| {
                die(format!(
                    "{id}: modes.verify.rcb_time_disabled_reason must be a string"
                ))
            })
        });
        match (rcb_time, rcb_time_disabled_reason) {
            (Some(false), Some(reason)) if !reason.trim().is_empty() => {}
            (Some(false), _) => die(format!(
                "{id}: modes.verify.rcb_time=false requires a substantive rcb_time_disabled_reason"
            )),
            (None | Some(true), Some(_)) => die(format!(
                "{id}: modes.verify RCB-time reason is stale while RCB time is enabled"
            )),
            (None | Some(true), None) => {}
        }
    }
    let ci_spec: CiSelectionSpec = parse_schema_value(
        spec.get("ci")
            .unwrap_or_else(|| die(format!("{id}: modes.{mode}.ci is required"))),
        &format!("{id}: modes.{mode}.ci"),
    );
    let ci_disabled_reason: Option<CiDisabledReasonSpec> = spec
        .get("ci_disabled_reason")
        .map(|value| parse_schema_value(value, &format!("{id}: modes.{mode}.ci_disabled_reason")));
    let enabled = string_array(
        spec.get("backends_enabled"),
        &format!("{id}.modes.{mode}.backends_enabled"),
    );
    let enabled_set = enabled.iter().cloned().collect::<BTreeSet<_>>();
    let timeout_overrides = cell_timeout_overrides(
        spec_value,
        id,
        mode,
        &enabled_set,
        inherited_timeout_seconds,
        "timeout_seconds",
    );
    let cpu_timeout_overrides = cell_timeout_overrides(
        spec_value,
        id,
        mode,
        &enabled_set,
        inherited_cpu_timeout_seconds,
        "cpu_timeout_seconds",
    );
    let wall_override_keys = timeout_overrides.keys().cloned().collect::<BTreeSet<_>>();
    let cpu_override_keys = cpu_timeout_overrides
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();
    let reason_keys = spec
        .get("slow_reason")
        .and_then(Value::as_table)
        .map(|reasons| reasons.keys().cloned().collect::<BTreeSet<_>>())
        .unwrap_or_default();
    if wall_override_keys != cpu_override_keys || wall_override_keys != reason_keys {
        die(format!(
            "{id}.modes.{mode}: timeout_seconds, cpu_timeout_seconds, and slow_reason must name the same backends"
        ));
    }
    for backend in &wall_override_keys {
        if timeout_overrides[backend] == inherited_timeout_seconds
            && cpu_timeout_overrides[backend] == inherited_cpu_timeout_seconds
        {
            die(format!(
                "{id}.modes.{mode}.{backend} timeout pair redundantly repeats its inherited values"
            ));
        }
    }
    validate_mode_workdir(id, mode, workdir, &enabled).unwrap_or_else(|error| die(error));
    let disabled = spec
        .get("backends_disabled")
        .and_then(Value::as_table)
        .unwrap_or_else(|| {
            die(format!(
                "{id}: modes.{mode}.backends_disabled must be a table"
            ))
        });
    for (backend, reason) in disabled {
        if reason.as_str().is_none_or(str::is_empty) {
            die(format!(
                "{id}: modes.{mode}.backends_disabled.{backend} needs a reason"
            ));
        }
    }

    let expected: BTreeSet<&str> = if mode == "naked" {
        ["native"].into_iter().collect()
    } else {
        KNOWN_BACKENDS.into_iter().collect()
    };
    let enabled_names: BTreeSet<&str> = enabled.iter().map(String::as_str).collect();
    let disabled_set: BTreeSet<&str> = disabled.keys().map(String::as_str).collect();
    if enabled_names.len() != enabled.len()
        || !enabled_names.is_disjoint(&disabled_set)
        || enabled_names
            .union(&disabled_set)
            .copied()
            .collect::<BTreeSet<_>>()
            != expected
    {
        die(format!(
            "{id}: modes.{mode} must partition {:?}; enabled={enabled_set:?}, disabled={disabled_set:?}",
            expected
        ));
    }
    let ci = CiSelection::validate(
        &enabled.iter().cloned().collect(),
        &disabled.keys().cloned().collect(),
        &ci_spec,
        ci_disabled_reason.as_ref(),
    )
    .unwrap_or_else(|error| die(format!("{id}: modes.{mode} {error}")));
    if let Some(guest_args) = spec.get("guest_args") {
        let guest_args = guest_args
            .as_table()
            .unwrap_or_else(|| die(format!("{id}: modes.{mode}.guest_args must be a table")));
        for (backend, args) in guest_args {
            if !enabled_names.contains(backend.as_str()) && !disabled_set.contains(backend.as_str())
            {
                die(format!(
                    "{id}: modes.{mode}.guest_args.{backend} names a backend outside backends_enabled/backends_disabled"
                ));
            }
            let args = string_array(
                Some(args),
                &format!("{id}.modes.{mode}.guest_args.{backend}"),
            );
            if args.iter().any(|argument| argument.contains('\0')) {
                die(format!(
                    "{id}: modes.{mode}.guest_args.{backend} contains a NUL byte, which Linux argv cannot represent"
                ));
            }
        }
    }
    if mode == "naked" && ci.any_selected() {
        die(format!(
            "{id}: naked is opt-in meta-CI and must set ci=false"
        ));
    }
    if mode == "replay" && enabled.iter().any(|backend| backend != "ptrace") {
        die(format!("{id}: replay is ptrace-only, got {enabled:?}"));
    }

    if mode == "naked" && !enabled.is_empty() {
        let runs = spec
            .get("runs")
            .and_then(Value::as_integer)
            .unwrap_or_else(|| die(format!("{id}: enabled naked mode requires runs")));
        if !(3..=5).contains(&runs) {
            die(format!("{id}: naked.runs must be 3..=5, got {runs}"));
        }
        let assertions = spec
            .get("assert")
            .and_then(Value::as_table)
            .unwrap_or_else(|| die(format!("{id}: naked.assert must be a table")));
        ensure_keys(
            spec.get("assert").unwrap(),
            &["min_distinct"],
            &format!("{id}.modes.naked.assert"),
        );
        let min_distinct = assertions
            .get("min_distinct")
            .and_then(Value::as_integer)
            .unwrap_or_else(|| die(format!("{id}: naked.assert.min_distinct is required")));
        if !(2..=runs).contains(&min_distinct) {
            die(format!(
                "{id}: naked.assert.min_distinct must be 2..={runs}, got {min_distinct}"
            ));
        }
    }
    if mode == "chaos" && !enabled.is_empty() {
        let seeds = spec
            .get("seeds")
            .and_then(Value::as_array)
            .unwrap_or_else(|| die(format!("{id}: enabled chaos mode requires seeds")));
        let unique: BTreeSet<_> = seeds.iter().filter_map(Value::as_integer).collect();
        if seeds.len() < 2 || unique.len() != seeds.len() {
            die(format!(
                "{id}: chaos seeds must contain at least two unique integers"
            ));
        }
        // How many outcome classes the GUEST can produce at all. This is a
        // property of the program, not of the sweep, and declaring it is what
        // makes a saturated oracle visible AS saturated: when
        // `min_distinct >= outcome_classes` the `distinct >= N` check sits on
        // the guest's ceiling, so it can only ever catch a TOTAL collapse to one
        // class and is structurally blind to a PARTIAL narrowing of schedule
        // diversity. The harness records the count on every chaos row so a
        // reader can tell "diverse" from "saturated and therefore uninformative"
        // instead of reading a pinned `distinct=2` as strength.
        let outcome_classes = spec
            .get("outcome_classes")
            .and_then(Value::as_integer)
            .unwrap_or_else(|| {
                die(format!(
                    "{id}: enabled chaos mode requires outcome_classes (the guest's \
                     observable outcome-class ceiling)"
                ))
            });
        if outcome_classes < 2 {
            die(format!(
                "{id}: chaos.outcome_classes must be >= 2, got {outcome_classes}"
            ));
        }
        let assertions = spec
            .get("assert")
            .and_then(Value::as_table)
            .unwrap_or_else(|| die(format!("{id}: enabled chaos mode requires assert")));
        ensure_keys(
            spec.get("assert").unwrap(),
            &[
                "min_distinct",
                "min_passes",
                "min_failures",
                "min_normalized_entropy",
            ],
            &format!("{id}.modes.chaos.assert"),
        );
        for key in ["min_distinct", "min_passes", "min_failures"] {
            match assertions.get(key).and_then(Value::as_integer) {
                Some(value) if value >= 0 && (key != "min_distinct" || value >= 2) => {}
                other => die(format!(
                    "{id}: chaos.assert.{key} has invalid value {other:?}"
                )),
            }
        }
        let min_distinct = assertions
            .get("min_distinct")
            .and_then(Value::as_integer)
            .expect("min_distinct validated above");
        if min_distinct > outcome_classes {
            die(format!(
                "{id}: chaos.assert.min_distinct {min_distinct} exceeds outcome_classes \
                 {outcome_classes}; the guest cannot produce that many classes"
            ));
        }
        // OPTIONAL degree floor on the outcome-class DISTRIBUTION, expressed as
        // normalized Shannon entropy in 0.0..=1.0. Absent means not enforced,
        // which is the correct state for a guest whose seed sweep is not yet wide
        // enough to populate its classes representatively -- a floor that the
        // current sweep cannot meet would be a new false red, not a better
        // oracle. Unlike `min_distinct`, this does NOT saturate on a two-class
        // guest: the class BALANCE keeps moving as diversity narrows.
        if let Some(value) = assertions.get("min_normalized_entropy") {
            let entropy = value
                .as_float()
                .or_else(|| value.as_integer().map(|integer| integer as f64))
                .unwrap_or_else(|| {
                    die(format!(
                        "{id}: chaos.assert.min_normalized_entropy must be a number, got {value:?}"
                    ))
                });
            if !(0.0..=1.0).contains(&entropy) {
                die(format!(
                    "{id}: chaos.assert.min_normalized_entropy must be 0.0..=1.0, got {entropy}"
                ));
            }
        }
    }
    if mode == "custom" && !enabled.is_empty() {
        let args = string_array(spec.get("args"), &format!("{id}.modes.custom.args"));
        if args.is_empty() {
            die(format!("{id}: enabled custom mode requires args"));
        }
        let assertions = spec
            .get("assert")
            .and_then(Value::as_table)
            .unwrap_or_else(|| die(format!("{id}: enabled custom mode requires assert")));
        ensure_keys(
            spec.get("assert").unwrap(),
            &["runs", "repeat_identical"],
            &format!("{id}.modes.custom.assert"),
        );
        let runs = assertions
            .get("runs")
            .and_then(Value::as_integer)
            .unwrap_or_else(|| die(format!("{id}: custom.assert.runs is required")));
        if !(3..=5).contains(&runs)
            || assertions.get("repeat_identical").and_then(Value::as_bool) != Some(true)
        {
            die(format!(
                "{id}: custom must require 3..=5 runs with repeat_identical=true"
            ));
        }
    }

    let attempts = mode_attempts(id, mode, spec_value);
    for backend in enabled {
        let timeout_seconds = timeout_overrides
            .get(&backend)
            .copied()
            .unwrap_or(inherited_timeout_seconds);
        let cpu_timeout_seconds = cpu_timeout_overrides
            .get(&backend)
            .copied()
            .unwrap_or(inherited_cpu_timeout_seconds);
        let selected = ci.selected(&backend);
        let ci_disabled_reason = ci.reason(&backend).cloned();
        rows.push(PlanRow {
            bucket: bucket.to_string(),
            id: id.to_string(),
            lane: lane.to_string(),
            mode: mode.to_string(),
            backend,
            ci: selected,
            ci_disabled_reason,
            applicable: true,
            not_applicable_reason: None,
            timeout_seconds,
            cpu_timeout_seconds,
            attempts,
        });
    }
    for (backend, reason) in disabled {
        // The reason is already validated non-empty above, so this carries a
        // fact the manifest states rather than synthesising one.
        let not_applicable_reason = reason.as_str().map(str::to_string);
        rows.push(PlanRow {
            bucket: bucket.to_string(),
            id: id.to_string(),
            lane: lane.to_string(),
            mode: mode.to_string(),
            backend: backend.to_string(),
            ci: false,
            ci_disabled_reason: None,
            applicable: false,
            not_applicable_reason,
            timeout_seconds: inherited_timeout_seconds,
            cpu_timeout_seconds: inherited_cpu_timeout_seconds,
            attempts,
        });
    }
}

fn optional_positive_integer(
    table: &std::collections::BTreeMap<String, Value>,
    key: &str,
    context: &str,
) -> Option<i64> {
    let value = table.get(key)?;
    match value.as_integer() {
        Some(value) if value > 0 => Some(value),
        other => die(format!(
            "{context}.{key} must be a positive integer, got {other:?}"
        )),
    }
}

/// Number of `execute_attempt` calls made by the existing harness for one mode.
/// Verify and chaos calls still perform Hermit's internal two-run comparison,
/// and a replay call still performs record and replay. `None` means the
/// manifest does not declare an executable chaos recipe.
fn mode_attempts(id: &str, mode: &str, spec_value: &Value) -> Option<i64> {
    let spec = spec_value
        .as_table()
        .unwrap_or_else(|| die(format!("{id}: modes.{mode} must be a table")));
    match mode {
        "verify" | "replay" => Some(1),
        "naked" => match optional_positive_integer(spec, "runs", &format!("{id}.modes.naked")) {
            Some(runs @ 3..=5) => Some(runs),
            Some(runs) => die(format!("{id}: naked.runs must be 3..=5, got {runs}")),
            None => Some(3),
        },
        "custom" => {
            let Some(assert_value) = spec.get("assert") else {
                return Some(1);
            };
            let assertions = assert_value
                .as_table()
                .unwrap_or_else(|| die(format!("{id}: modes.custom.assert must be a table")));
            match optional_positive_integer(
                assertions,
                "runs",
                &format!("{id}.modes.custom.assert"),
            ) {
                Some(runs @ 3..=5) => Some(runs),
                Some(runs) => die(format!(
                    "{id}: custom.assert.runs must be 3..=5, got {runs}"
                )),
                None => Some(1),
            }
        }
        "chaos" => {
            let seed_value = spec.get("seeds")?;
            let seeds = seed_value
                .as_array()
                .unwrap_or_else(|| die(format!("{id}: modes.chaos.seeds must be an array")));
            if seeds.is_empty() {
                return None;
            }
            let seeds: Vec<_> = seeds
                .iter()
                .map(|seed| {
                    seed.as_integer().unwrap_or_else(|| {
                        die(format!("{id}: modes.chaos.seeds entries must be integers"))
                    })
                })
                .collect();
            if seeds.iter().any(|seed| *seed < 0) {
                die(format!("{id}: chaos seeds must be nonnegative integers"));
            }
            let unique: BTreeSet<_> = seeds.iter().copied().collect();
            if seeds.len() < 2 || unique.len() != seeds.len() {
                die(format!(
                    "{id}: chaos seeds must contain at least two unique integers"
                ));
            }
            Some(
                i64::try_from(seeds.len())
                    .unwrap_or_else(|_| die(format!("{id}: chaos seed count is too large"))),
            )
        }
        other => die(format!("{id}: unknown mode `{other}`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_mode(text: &str) -> Value {
        manifest_value::from_toml(
            text.parse::<toml::Value>()
                .expect("test mode must be valid TOML"),
        )
    }

    // ------------------------------------------- `requires` vocabulary brackets
    //
    // Both directions, on planted manifests, so neither depends on the machine
    // running the test. The load-bearing one is
    // `a_cell_without_a_probeable_token_is_never_selected`: without it the
    // mechanism could be a blanket omission rather than a predicate, and could
    // be used to excuse a cell that is merely broken.

    #[test]
    fn the_one_probeable_token_maps_to_the_shared_capability_name() {
        // The name must be exactly what
        // `host_capability::HostCapability::value` emits and what
        // `scripts/validate.rs --probe-host-capability` accepts. A typo here
        // would make the probe silently unreachable, so the cell would run and
        // fail — safe, but the mechanism would be inert, which this catches.
        assert_eq!(
            requires_capability("cpuid").unwrap(),
            Some(hermit_manifest_plan::stress_series::HostCapability::CpuidFaulting)
        );
    }

    #[test]
    fn every_other_shipped_token_has_no_absence_proof() {
        // Exactly ONE token may withhold anything. If a future edit gives a
        // second token an absence proof, this fails and forces the reviewer to
        // look at it, which is the point.
        let probeable: Vec<&str> = REQUIRES_VOCABULARY
            .iter()
            .filter(|(_, capability)| capability.is_some())
            .map(|(name, _)| *name)
            .collect();
        assert_eq!(probeable, vec!["cpuid"]);
    }

    #[test]
    fn an_unknown_token_is_refused_not_ignored() {
        let error = requires_capability("cpuid-faulting-please").unwrap_err();
        assert!(error.contains("vocabulary is closed"), "{error}");
    }

    #[test]
    fn a_cell_declaring_a_probeable_token_is_selected() {
        let document = parse_mode(
            "[[test]]\nid = \"b/needs-cpuid\"\nrequires = [\"linux\", \"cpuid\", \"cc\"]\n",
        );
        assert_eq!(
            host_requirement_pairs(&[document]).unwrap(),
            vec![("cpuid-faulting".to_string(), "b/needs-cpuid".to_string())]
        );
    }

    #[test]
    fn a_cell_without_a_probeable_token_is_never_selected() {
        // THE ONE THAT MATTERS. A cell that declares nothing probeable can never
        // be withheld, whatever the machine lacks — so a broken cell still runs,
        // still fails, and is still refused.
        let document = parse_mode(
            "[[test]]\nid = \"b/ordinary\"\nrequires = [\"linux\", \"x86_64\", \"ptrace\", \"cc\"]\n",
        );
        assert!(host_requirement_pairs(&[document]).unwrap().is_empty());
    }

    #[test]
    fn an_empty_requires_list_selects_nothing() {
        let document = parse_mode("[[test]]\nid = \"b/bare\"\nrequires = []\n");
        assert!(host_requirement_pairs(&[document]).unwrap().is_empty());
    }

    #[test]
    fn an_unknown_token_refuses_the_whole_extraction() {
        let document =
            parse_mode("[[test]]\nid = \"b/exotic\"\nrequires = [\"linux\", \"quantum\"]\n");
        let error = host_requirement_pairs(&[document]).unwrap_err();
        assert!(
            error.contains("unknown `requires` token `quantum`"),
            "{error}"
        );
    }

    #[test]
    fn the_shipped_corpus_declares_exactly_one_withholdable_cell() {
        // NON-VACUITY. A bracket that passed against an empty corpus would prove
        // nothing, and this also pins the blast radius: if a future manifest edit
        // makes a second cell withholdable, a reviewer has to see it.
        let manifests = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/e2e/manifests");
        let mut documents = Vec::new();
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&manifests)
            .expect("shipped manifests must be readable")
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "yaml")
            })
            .collect();
        paths.sort();
        assert!(
            !paths.is_empty(),
            "shipped manifest corpus must not be empty"
        );
        for path in paths {
            let text = std::fs::read_to_string(&path).expect("manifest must be readable");
            documents.push(text.parse::<Value>().expect("manifest must be valid YAML"));
        }
        assert_eq!(
            host_requirement_pairs(&documents).unwrap(),
            vec![(
                "cpuid-faulting".to_string(),
                "backend-parity-c/cpuid-probe".to_string()
            )]
        );
    }

    #[test]
    #[should_panic(expected = "unknown keys")]
    fn rejects_unknown_schema_keys() {
        let value = parse_mode("known = true\nunknown = false\n");
        ensure_keys(&value, &["known"], "test");
    }

    #[test]
    #[should_panic(expected = "compare_io_buffers=false requires a substantive")]
    fn rejects_io_buffer_comparison_relaxation_without_reason() {
        let spec = parse_mode(
            r#"
ci = true
compare_io_buffers = false
backends_enabled = ["ptrace"]

[backends_disabled]
dbt = "unsupported"
kvm = "unsupported"
sabre = "unsupported"
liteinst = "unsupported"
"#,
        );
        validate_mode(
            "bucket/test",
            "bucket",
            "portable",
            "verify",
            90,
            &spec,
            &mut Vec::new(),
        );
    }

    #[test]
    #[should_panic(expected = "rcb_time=false requires a substantive")]
    fn rejects_rcb_time_relaxation_without_reason() {
        let spec = parse_mode(
            r#"
ci = true
rcb_time = false
backends_enabled = ["ptrace"]

[backends_disabled]
dbt = "unsupported"
kvm = "unsupported"
sabre = "unsupported"
liteinst = "unsupported"
"#,
        );
        validate_mode(
            "bucket/test",
            "bucket",
            "portable",
            "verify",
            90,
            &spec,
            &mut Vec::new(),
        );
    }

    #[test]
    fn accepts_structured_direct_argv() {
        let value = parse_mode("direct = [\"./example\", \"argument with spaces\"]\n");
        validate_direct(value.get("direct").unwrap(), "bucket/test");
    }

    #[test]
    #[should_panic(expected = "direct argv must not be empty")]
    fn rejects_empty_direct_argv() {
        let value = parse_mode("direct = []\n");
        validate_direct(value.get("direct").unwrap(), "bucket/test");
    }

    #[test]
    #[should_panic(expected = "must partition")]
    fn rejects_incomplete_backend_partition() {
        let spec = parse_mode(
            r#"
ci = false
ci_disabled_reason = "not selected"
backends_enabled = ["ptrace"]

[backends_disabled]
dbt = "unsupported"
kvm = "unsupported"
sabre = "unsupported"
"#,
        );
        validate_mode(
            "bucket/test",
            "bucket",
            "portable",
            "verify",
            90,
            &spec,
            &mut Vec::new(),
        );
    }

    #[test]
    #[should_panic(expected = "naked is opt-in meta-CI")]
    fn rejects_naked_mode_in_regular_ci() {
        let spec = parse_mode(
            r#"
ci = true
backends_enabled = ["native"]
runs = 3

[backends_disabled]

[assert]
min_distinct = 2
"#,
        );
        validate_mode(
            "bucket/test",
            "bucket",
            "portable",
            "naked",
            90,
            &spec,
            &mut Vec::new(),
        );
    }

    #[test]
    fn accepts_complete_verify_partition() {
        let spec = parse_mode(
            r#"
ci = true
backends_enabled = ["ptrace"]
guest_args = { ptrace = ["multi"] }
workdir = "/tmp"

[backends_disabled]
dbt = "unsupported"
kvm = "unsupported"
sabre = "unsupported"
liteinst = "unsupported"
"#,
        );
        let mut rows = Vec::new();
        validate_mode(
            "bucket/test",
            "bucket",
            "portable",
            "verify",
            90,
            &spec,
            &mut rows,
        );
        assert_eq!(rows.len(), 5);
        let applicable: Vec<_> = rows.iter().filter(|row| row.applicable).collect();
        assert_eq!(applicable.len(), 1);
        assert_eq!(applicable[0].backend, "ptrace");
        assert!(applicable[0].ci);
        assert_eq!(applicable[0].timeout_seconds, 90);
        assert_eq!(applicable[0].attempts, Some(1));
        assert!(
            rows.iter()
                .all(|row| row.timeout_seconds == 90 && row.attempts == Some(1))
        );
        assert_eq!(rows.iter().filter(|row| !row.applicable).count(), 4);
    }

    #[test]
    fn exact_cell_timeout_overrides_the_inherited_value() {
        let spec = parse_mode(
            r#"
ci = true
backends_enabled = ["ptrace"]
timeout_seconds = { ptrace = 30 }
cpu_timeout_seconds = { ptrace = 25 }
slow_reason = { ptrace = "three measured runs exceeded the inherited limit" }

[backends_disabled]
dbt = "unsupported"
kvm = "unsupported"
sabre = "unsupported"
liteinst = "unsupported"
"#,
        );
        let mut rows = Vec::new();
        validate_mode(
            "bucket/test",
            "bucket",
            "portable",
            "verify",
            15,
            &spec,
            &mut rows,
        );
        let ptrace = rows.iter().find(|row| row.backend == "ptrace").unwrap();
        assert_eq!(ptrace.timeout_seconds, 30);
        assert_eq!(ptrace.cpu_timeout_seconds, 25);
        assert!(
            rows.iter()
                .filter(|row| row.backend != "ptrace")
                .all(|row| row.timeout_seconds == 15)
        );
    }

    #[test]
    #[should_panic(expected = "timeout_seconds.ptrace requires slow_reason")]
    fn rejects_a_cell_timeout_without_its_named_reason() {
        let spec = parse_mode(
            r#"
ci = true
backends_enabled = ["ptrace"]
timeout_seconds = { ptrace = 30 }
cpu_timeout_seconds = { ptrace = 25 }

[backends_disabled]
dbt = "unsupported"
kvm = "unsupported"
sabre = "unsupported"
liteinst = "unsupported"
"#,
        );
        validate_mode(
            "bucket/test",
            "bucket",
            "portable",
            "verify",
            15,
            &spec,
            &mut Vec::new(),
        );
    }

    #[test]
    #[should_panic(expected = "workdir must be an absolute path")]
    fn rejects_relative_run_workdir() {
        let spec = parse_mode(
            r#"
ci = true
backends_enabled = ["ptrace"]
workdir = "tmp"

[backends_disabled]
dbt = "unsupported"
kvm = "unsupported"
sabre = "unsupported"
liteinst = "unsupported"
"#,
        );
        validate_mode(
            "bucket/test",
            "bucket",
            "portable",
            "verify",
            90,
            &spec,
            &mut Vec::new(),
        );
    }

    #[test]
    fn accepts_workdir_with_mixed_ptrace_and_dbt_backends() {
        let spec = parse_mode(
            r#"
ci = true
backends_enabled = ["ptrace", "dbt"]
workdir = "/tmp"

[backends_disabled]
kvm = "unsupported"
sabre = "unsupported"
liteinst = "unsupported"
"#,
        );
        validate_mode(
            "bucket/test",
            "bucket",
            "portable",
            "verify",
            90,
            &spec,
            &mut Vec::new(),
        );
    }

    #[test]
    fn expands_per_backend_ci_and_emits_the_unselected_backend_reason() {
        let spec = parse_mode(
            r#"
ci = { ptrace = true, liteinst = false }
backends_enabled = ["ptrace", "liteinst"]

[ci_disabled_reason.liteinst]
result = "determinism-failure"
evidence = "ignored/results/liteinst.jsonl"
reason = "canonical comparison diverged at scheduler turn 10"

[backends_disabled]
dbt = "unsupported"
kvm = "unsupported"
sabre = "unsupported"
"#,
        );
        let mut rows = Vec::new();
        validate_mode(
            "bucket/test",
            "bucket",
            "portable",
            "verify",
            90,
            &spec,
            &mut rows,
        );
        let ptrace = rows.iter().find(|row| row.backend == "ptrace").unwrap();
        assert!(ptrace.ci);
        assert!(ptrace.ci_disabled_reason.is_none());
        let liteinst = rows.iter().find(|row| row.backend == "liteinst").unwrap();
        assert!(!liteinst.ci);
        assert_eq!(
            liteinst.ci_disabled_reason.as_ref().unwrap().reason,
            "canonical comparison diverged at scheduler turn 10"
        );
        assert!(
            rows.iter()
                .filter(|row| !row.applicable)
                .all(|row| !row.ci && row.ci_disabled_reason.is_none())
        );
    }

    #[test]
    #[should_panic(expected = "must explain the result in at least three words")]
    fn rejects_placeholder_per_backend_reason() {
        let spec = parse_mode(
            r#"
ci = { ptrace = true, liteinst = false }
backends_enabled = ["ptrace", "liteinst"]

[ci_disabled_reason.liteinst]
result = "determinism-failure"
evidence = "ignored/results/liteinst.jsonl"
reason = "x"

[backends_disabled]
dbt = "unsupported"
kvm = "unsupported"
sabre = "unsupported"
"#,
        );
        validate_mode(
            "bucket/test",
            "bucket",
            "portable",
            "verify",
            90,
            &spec,
            &mut Vec::new(),
        );
    }

    #[test]
    fn derives_existing_harness_attempt_counts() {
        let absent = parse_mode("");
        assert_eq!(mode_attempts("bucket/test", "verify", &absent), Some(1));
        assert_eq!(mode_attempts("bucket/test", "replay", &absent), Some(1));
        assert_eq!(mode_attempts("bucket/test", "naked", &absent), Some(3));
        assert_eq!(mode_attempts("bucket/test", "custom", &absent), Some(1));
        assert_eq!(mode_attempts("bucket/test", "chaos", &absent), None);

        let naked = parse_mode("runs = 5\n");
        assert_eq!(mode_attempts("bucket/test", "naked", &naked), Some(5));
        let custom = parse_mode("[assert]\nruns = 4\n");
        assert_eq!(mode_attempts("bucket/test", "custom", &custom), Some(4));
        let chaos = parse_mode("seeds = [0, 3, 9]\n");
        assert_eq!(mode_attempts("bucket/test", "chaos", &chaos), Some(3));
        let empty_chaos = parse_mode("seeds = []\n");
        assert_eq!(mode_attempts("bucket/test", "chaos", &empty_chaos), None);
    }

    #[test]
    #[should_panic(expected = "naked.runs must be 3..=5")]
    fn rejects_invalid_explicit_naked_attempt_count_when_disabled() {
        let spec = parse_mode("runs = 2\n");
        mode_attempts("bucket/test", "naked", &spec);
    }

    #[test]
    #[should_panic(expected = "at least two unique integers")]
    fn rejects_duplicate_chaos_recipe_when_disabled() {
        let spec = parse_mode("seeds = [7, 7]\n");
        mode_attempts("bucket/test", "chaos", &spec);
    }

    #[test]
    #[should_panic(expected = "chaos seeds must be nonnegative integers")]
    fn rejects_negative_chaos_seed() {
        let spec = parse_mode("seeds = [-1, 7]\n");
        mode_attempts("bucket/test", "chaos", &spec);
    }

    #[test]
    fn identifies_backend_private_guest_fixtures() {
        let inventory = json!({
            "files": [
                {
                    "path": "tests/backend-parity/fixtures/new_contract.c",
                    "disposition": "guest-fixture",
                    "runner": "tests/backend-parity/run_matrix.py"
                },
                {
                    "path": "tests/c/liteinst_only.c",
                    "disposition": "guest-fixture",
                    "runner": "hermit-cli/tests/liteinst.rs"
                },
                {
                    "path": "tests/c/shared.c",
                    "disposition": "manifest-test",
                    "runner": "target/debug/test-harness"
                },
                {
                    "path": "tests/c/cargo_fixture.c",
                    "disposition": "guest-fixture",
                    "runner": "detcore integration tests"
                }
            ]
        });
        assert_eq!(
            backend_private_guest_files(&inventory),
            BTreeSet::from([
                "tests/backend-parity/fixtures/new_contract.c".to_string(),
                "tests/c/liteinst_only.c".to_string(),
            ])
        );
    }

    #[test]
    fn identifies_manifest_mode_without_ptrace_front_door() {
        let document = r#"
[[test]]
id = "applications/kvm-only"

[test.modes.verify]
backends_enabled = ["kvm"]

[test.modes.chaos]
backends_enabled = []

[test.modes.replay]
backends_enabled = []

[test.modes.naked]
backends_enabled = []

[test.modes.custom]
backends_enabled = []
"#
        .parse::<toml::Value>()
        .map(manifest_value::from_toml)
        .expect("test manifest must be valid YAML");
        assert_eq!(
            asymmetric_manifest_tests(&[document]),
            BTreeSet::from(["applications/kvm-only".to_string()])
        );
    }

    #[test]
    fn accepts_ptrace_established_shared_manifest_row() {
        let document = r#"
[[test]]
id = "applications/shared"

[test.modes.verify]
backends_enabled = ["ptrace", "kvm"]

[test.modes.chaos]
backends_enabled = []

[test.modes.replay]
backends_enabled = ["ptrace"]

[test.modes.naked]
backends_enabled = []

[test.modes.custom]
backends_enabled = []
"#
        .parse::<toml::Value>()
        .map(manifest_value::from_toml)
        .expect("test manifest must be valid YAML");
        assert!(asymmetric_manifest_tests(&[document]).is_empty());
    }

    #[test]
    #[should_panic(expected = "unexpected=[\"tests/backend-parity/private.c\"]")]
    fn rejects_backend_private_guest_growth() {
        enforce_exact_ratchet(
            "backend-private guest debt",
            &BTreeSet::from(["tests/backend-parity/private.c".to_string()]),
            &BTreeSet::new(),
        );
    }

    #[test]
    fn accepts_guest_args_for_disabled_backend() {
        let spec = parse_mode(
            r#"
ci = false
ci_disabled_reason = "fixture cell: ptrace only, other backends unmeasured here"
backends_enabled = ["ptrace"]
guest_args = { ptrace = [], kvm = ["--kvm"] }

[backends_disabled]
dbt = "unsupported"
kvm = "unsupported"
sabre = "unsupported"
liteinst = "unsupported"
"#,
        );
        validate_mode(
            "bucket/test",
            "bucket",
            "portable",
            "verify",
            90,
            &spec,
            &mut Vec::new(),
        );
    }

    #[test]
    #[should_panic(expected = "names a backend outside backends_enabled/backends_disabled")]
    fn rejects_guest_args_for_unknown_backend() {
        let spec = parse_mode(
            r#"
ci = false
ci_disabled_reason = "fixture cell: ptrace only, other backends unmeasured here"
backends_enabled = ["ptrace"]
guest_args = { unknown = ["--unknown"] }

[backends_disabled]
dbt = "unsupported"
kvm = "unsupported"
sabre = "unsupported"
liteinst = "unsupported"
"#,
        );
        validate_mode(
            "bucket/test",
            "bucket",
            "portable",
            "verify",
            90,
            &spec,
            &mut Vec::new(),
        );
    }

    #[test]
    #[should_panic(expected = "ci=false with enabled backends requires ci_disabled_reason")]
    fn rejects_enabled_cell_silently_disabled_from_ci() {
        let spec = parse_mode(
            r#"
ci = false
backends_enabled = ["ptrace"]

[backends_disabled]
dbt = "unsupported"
kvm = "unsupported"
sabre = "unsupported"
liteinst = "unsupported"
"#,
        );
        validate_mode(
            "bucket/test",
            "bucket",
            "portable",
            "verify",
            90,
            &spec,
            &mut Vec::new(),
        );
    }

    #[test]
    #[should_panic(expected = "ci=true must not carry ci_disabled_reason")]
    fn rejects_stale_ci_disabled_reason_on_selected_cell() {
        let spec = parse_mode(
            r#"
ci = true
ci_disabled_reason = "stale"
backends_enabled = ["ptrace"]

[backends_disabled]
dbt = "unsupported"
kvm = "unsupported"
sabre = "unsupported"
liteinst = "unsupported"
"#,
        );
        validate_mode(
            "bucket/test",
            "bucket",
            "portable",
            "verify",
            90,
            &spec,
            &mut Vec::new(),
        );
    }

    #[cfg(unix)]
    #[test]
    fn recognizes_broken_symlink_as_manual_program_entry() {
        use std::os::unix::fs::symlink;

        let directory = std::env::temp_dir().join(format!(
            "hermit-manifest-plan-symlink-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).expect("create test directory");
        let link = directory.join("external.c");
        symlink("missing-external-target.c", &link).expect("create broken symlink");
        assert!(is_file_or_symlink(&link));
        assert!(!link.is_file());
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    /// A chaos spec that differs from the accepted one only in the clause under
    /// test, so each refusal below is attributable to that clause and not to
    /// unrelated invalidity.
    fn chaos_spec(outcome_classes: &str, assert_body: &str) -> Value {
        parse_mode(&format!(
            r#"
ci = true
backends_enabled = ["ptrace"]
seeds = [0, 9]
{outcome_classes}

[backends_disabled]
dbt = "unsupported"
kvm = "unsupported"
sabre = "unsupported"
liteinst = "unsupported"

[assert]
{assert_body}
"#
        ))
    }

    fn validate_chaos(spec: &Value, rows: &mut Vec<PlanRow>) {
        validate_mode("bucket/test", "bucket", "portable", "chaos", 90, spec, rows);
    }

    // POSITIVE side of the bracket: the qualifying spec is accepted and produces
    // a plan row, so the refusals below are a real discriminator rather than a
    // clause that rejects everything.
    #[test]
    fn accepts_chaos_mode_declaring_its_outcome_class_ceiling() {
        let spec = chaos_spec(
            "outcome_classes = 2",
            "min_distinct = 2\nmin_passes = 1\nmin_failures = 1\n",
        );
        let mut rows = Vec::new();
        validate_chaos(&spec, &mut rows);
        assert_eq!(rows.len(), 5);
        assert_eq!(rows.iter().filter(|row| row.applicable).count(), 1);
        assert!(rows.iter().all(|row| row.mode == "chaos"));
    }

    #[test]
    fn accepts_chaos_mode_with_a_normalized_entropy_floor() {
        let spec = chaos_spec(
            "outcome_classes = 4",
            "min_distinct = 2\nmin_passes = 1\nmin_failures = 1\nmin_normalized_entropy = 0.5\n",
        );
        let mut rows = Vec::new();
        validate_chaos(&spec, &mut rows);
        assert_eq!(rows.len(), 5);
        assert_eq!(rows.iter().filter(|row| row.applicable).count(), 1);
    }

    // NEGATIVE side: an undeclared ceiling is what makes a saturated oracle
    // invisible, so it must be refused rather than defaulted.
    #[test]
    #[should_panic(expected = "requires outcome_classes")]
    fn rejects_chaos_mode_without_an_outcome_class_ceiling() {
        let spec = chaos_spec("", "min_distinct = 2\nmin_passes = 1\nmin_failures = 1\n");
        validate_chaos(&spec, &mut Vec::new());
    }

    #[test]
    #[should_panic(expected = "outcome_classes must be >= 2")]
    fn rejects_single_class_guest_as_a_chaos_guest() {
        let spec = chaos_spec(
            "outcome_classes = 1",
            "min_distinct = 2\nmin_passes = 1\nmin_failures = 1\n",
        );
        validate_chaos(&spec, &mut Vec::new());
    }

    #[test]
    #[should_panic(expected = "exceeds outcome_classes")]
    fn rejects_unsatisfiable_min_distinct_above_the_guest_ceiling() {
        let spec = chaos_spec(
            "outcome_classes = 2",
            "min_distinct = 3\nmin_passes = 1\nmin_failures = 1\n",
        );
        validate_chaos(&spec, &mut Vec::new());
    }

    #[test]
    #[should_panic(expected = "min_normalized_entropy must be 0.0..=1.0")]
    fn rejects_out_of_range_normalized_entropy_floor() {
        let spec = chaos_spec(
            "outcome_classes = 2",
            "min_distinct = 2\nmin_passes = 1\nmin_failures = 1\nmin_normalized_entropy = 1.5\n",
        );
        validate_chaos(&spec, &mut Vec::new());
    }

    #[test]
    #[should_panic(expected = "min_normalized_entropy must be a number")]
    fn rejects_non_numeric_normalized_entropy_floor() {
        let spec = chaos_spec(
            "outcome_classes = 2",
            "min_distinct = 2\nmin_passes = 1\nmin_failures = 1\nmin_normalized_entropy = \"high\"\n",
        );
        validate_chaos(&spec, &mut Vec::new());
    }
}
