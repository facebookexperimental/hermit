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
//!   cargo run -p hermit-manifest-plan -- --format harness-json

use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
#[cfg(not(test))]
use std::process::exit;

use serde_json::Value as JsonValue;
use serde_json::json;
use toml::Value;

const KNOWN_BACKENDS: [&str; 5] = ["ptrace", "dbt", "kvm", "sabre", "liteinst"];
const MODES: [&str; 5] = ["verify", "chaos", "replay", "naked", "custom"];
/// Buckets where `ci = false` is not allowed to be a silent default.
///
/// `backends_disabled` has always required a per-backend reason, but `ci =
/// false` switches a whole mode-cell off with no justification at all — so a
/// generated cell is born not-running and nothing records that it is. In the
/// buckets listed here, every `ci = false` mode must carry a non-empty
/// `ci_disabled_reason`, and every `ci = true` mode must omit it (a leftover
/// reason on a running cell is stale documentation).
///
/// This is a ratchet: add a bucket once its cells all carry reasons. Cells
/// outside these buckets may still carry `ci_disabled_reason` voluntarily.
const CI_REASON_REQUIRED_BUCKETS: [&str; 1] = ["backend-parity-c"];
const MATRIX_SYMMETRY_BASELINE: &str = "ci/matrix-symmetry-baseline.json";
const TEST_INVENTORY: &str = "tests/e2e/manifests/inventory/test-files.json";

#[derive(Debug)]
struct PlanRow {
    bucket: String,
    id: String,
    lane: String,
    mode: String,
    backend: String,
    ci: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Format {
    Text,
    Json,
    HarnessJson,
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

fn parse_format() -> Format {
    let mut format = Format::Text;
    let mut args = std::env::args().skip(1);
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
            "harness-json" => Format::HarnessJson,
            _ => die(format!("unknown format: {value}")),
        };
    }
    format
}

fn main() {
    let format = parse_format();
    let script_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/e2e/manifests");
    let repo_root = script_dir.join("../../..");

    let mut manifests: Vec<PathBuf> = std::fs::read_dir(&script_dir)
        .unwrap_or_else(|error| die(format!("cannot read {}: {error}", script_dir.display())))
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "toml")
        })
        .collect();
    manifests.sort();
    if manifests.is_empty() {
        die(format!(
            "no *.toml manifests found in {}",
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
            .unwrap_or_else(|error| die(format!("{}: invalid TOML: {error}", path.display())));
        let location = path.file_name().unwrap().to_string_lossy().to_string();
        ensure_keys(&document, &["schema", "bucket", "test"], &location);

        if document.get("schema").and_then(Value::as_integer) != Some(2) {
            die(format!("{location}: schema must be 2"));
        }
        let bucket = required_string(&document, "bucket", &location);
        let stem = path.file_stem().unwrap().to_string_lossy();
        if bucket != stem {
            die(format!(
                "{location}: bucket `{bucket}` must equal file stem `{stem}`"
            ));
        }
        let tests = document
            .get("test")
            .and_then(Value::as_array)
            .filter(|tests| !tests.is_empty())
            .unwrap_or_else(|| die(format!("{location}: missing non-empty [[test]] array")));
        for test in tests {
            validate_and_expand(
                test,
                bucket,
                &location,
                &repo_root,
                &mut seen_ids,
                &mut seen_programs,
                &mut rows,
            );
        }
        documents.push(document);
    }

    validate_front_door(&repo_root, &documents);

    rows.sort_by(|left, right| {
        (&left.bucket, &left.id, &left.mode, &left.backend).cmp(&(
            &right.bucket,
            &right.id,
            &right.mode,
            &right.backend,
        ))
    });

    match format {
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
                .map(|row| {
                    json!({
                        "bucket": row.bucket,
                        "test": row.id,
                        "lane": row.lane,
                        "mode": row.mode,
                        "backend": row.backend,
                        "ci": row.ci,
                    })
                })
                .collect();
            println!(
                "{}",
                serde_json::to_string(&output)
                    .unwrap_or_else(|error| die(format!("cannot encode plan: {error}")))
            );
        }
        Format::Text => {
            println!(
                "{:<10}\t{:<38}\t{:<10}\t{:<8}\t{:<5}\tBUCKET",
                "LANE", "TEST", "MODE", "BACKEND", "CI"
            );
            for row in &rows {
                println!(
                    "{:<10}\t{:<38}\t{:<10}\t{:<8}\t{:<5}\t{}",
                    row.lane, row.id, row.mode, row.backend, row.ci, row.bucket
                );
            }
            eprintln!(
                "\nPASS: {} manifest(s), {} test(s), {} enabled plan cells validated",
                manifests.len(),
                seen_ids.len(),
                rows.len()
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
            "matrix symmetry {label} changed; unexpected={unexpected:?}, stale_baseline={stale:?}. New compatibility coverage must enter a shared schema-v2 TOML manifest, establish ptrace first, and declare every backend/mode cell; remove migrated debt from {MATRIX_SYMMETRY_BASELINE}"
        ));
    }
}

fn validate_front_door(repo_root: &Path, documents: &[Value]) {
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

    let inventory_path = repo_root.join(TEST_INVENTORY);
    let inventory_text = std::fs::read_to_string(&inventory_path)
        .unwrap_or_else(|error| die(format!("cannot read {}: {error}", inventory_path.display())));
    let inventory: JsonValue = serde_json::from_str(&inventory_text).unwrap_or_else(|error| {
        die(format!(
            "{}: invalid JSON: {error}",
            inventory_path.display()
        ))
    });

    enforce_exact_ratchet(
        "manifest ptrace-front-door debt",
        &asymmetric_manifest_tests(documents),
        &expected_asymmetric,
    );
    enforce_exact_ratchet(
        "backend-private guest debt",
        &backend_private_guest_files(&inventory),
        &expected_private,
    );
}

fn required_string<'a>(value: &'a Value, key: &str, location: &str) -> &'a str {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
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

fn is_file_or_symlink(path: &Path) -> bool {
    path.is_file()
        || std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
}

#[allow(clippy::too_many_arguments)]
fn validate_and_expand(
    test: &Value,
    bucket: &str,
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
            "timeout_seconds",
            "occasional",
            "program",
            "direct",
            "observation",
            "build",
            "modes",
            "slow_reason",
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
    match test.get("timeout_seconds").and_then(Value::as_integer) {
        Some(timeout) if (1..=1800).contains(&timeout) => {}
        other => die(format!(
            "{id}: timeout_seconds must be 1..=1800, got {other:?}"
        )),
    }
    if test.get("occasional").and_then(Value::as_bool).is_none() {
        die(format!("{id}: occasional must be a boolean"));
    }
    let _requires = string_array(test.get("requires"), &format!("{id}.requires"));

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
    if let Some(reason) = test.get("slow_reason") {
        if reason.as_str().is_none_or(str::is_empty) {
            die(format!("{id}: slow_reason must be a non-empty string"));
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
        validate_mode(&id, bucket, lane, mode, modes.get(mode).unwrap(), rows);
    }
    if rows[row_start..].iter().any(|row| row.ci)
        && program_path.as_ref().is_some_and(|path| !path.is_file())
    {
        die(format!(
            "{id}: CI-enabled program symlink target is unavailable: {}",
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

/// Enforce that a switched-off mode-cell says why it is switched off.
///
/// Only applies to [`CI_REASON_REQUIRED_BUCKETS`]. The rule is symmetric on
/// purpose: `ci = false` must explain itself, and `ci = true` must not carry a
/// reason, so flipping a cell on forces the stale justification to be deleted
/// in the same edit rather than left behind to mislead the next reader.
fn validate_ci_disabled_reason(
    id: &str,
    bucket: &str,
    mode: &str,
    spec: &toml::map::Map<String, Value>,
    ci: bool,
) {
    if !CI_REASON_REQUIRED_BUCKETS.contains(&bucket) {
        return;
    }
    let reason = spec.get("ci_disabled_reason");
    if ci {
        if reason.is_some() {
            die(format!(
                "{id}: modes.{mode} is CI-enabled, so it must not carry ci_disabled_reason"
            ));
        }
        return;
    }
    match reason.and_then(Value::as_str) {
        Some(text) if !text.trim().is_empty() => {}
        _ => die(format!(
            "{id}: modes.{mode} sets ci = false and must state why in a non-empty \
             ci_disabled_reason (bucket `{bucket}` forbids a silent default-off cell)"
        )),
    }
}

fn validate_mode(
    id: &str,
    bucket: &str,
    lane: &str,
    mode: &str,
    spec: &Value,
    rows: &mut Vec<PlanRow>,
) {
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
        "verify" => allowed.push("assert"),
        _ => {}
    }
    ensure_keys(spec_value, &allowed, &format!("{id}.modes.{mode}"));
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
    }
    let ci = spec
        .get("ci")
        .and_then(Value::as_bool)
        .unwrap_or_else(|| die(format!("{id}: modes.{mode}.ci must be a boolean")));
    validate_ci_disabled_reason(id, bucket, mode, spec, ci);
    let enabled = string_array(
        spec.get("backends_enabled"),
        &format!("{id}.modes.{mode}.backends_enabled"),
    );
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
    let enabled_set: BTreeSet<&str> = enabled.iter().map(String::as_str).collect();
    let disabled_set: BTreeSet<&str> = disabled.keys().map(String::as_str).collect();
    if enabled_set.len() != enabled.len()
        || !enabled_set.is_disjoint(&disabled_set)
        || enabled_set
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
    if let Some(guest_args) = spec.get("guest_args") {
        let guest_args = guest_args
            .as_table()
            .unwrap_or_else(|| die(format!("{id}: modes.{mode}.guest_args must be a table")));
        for (backend, args) in guest_args {
            if !enabled_set.contains(backend.as_str()) {
                die(format!(
                    "{id}: modes.{mode}.guest_args.{backend} names a backend that is not enabled"
                ));
            }
            if string_array(
                Some(args),
                &format!("{id}.modes.{mode}.guest_args.{backend}"),
            )
            .is_empty()
            {
                die(format!(
                    "{id}: modes.{mode}.guest_args.{backend} must contain at least one argument"
                ));
            }
        }
    }
    if ci && enabled.is_empty() {
        die(format!(
            "{id}: modes.{mode} is CI-enabled but has no backend"
        ));
    }
    if mode == "naked" && ci {
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

    for backend in enabled {
        rows.push(PlanRow {
            bucket: bucket.to_string(),
            id: id.to_string(),
            lane: lane.to_string(),
            mode: mode.to_string(),
            backend,
            ci,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_mode(text: &str) -> Value {
        text.parse::<Value>().expect("test mode must be valid TOML")
    }

    #[test]
    #[should_panic(expected = "unknown keys")]
    fn rejects_unknown_schema_keys() {
        let value = parse_mode("known = true\nunknown = false\n");
        ensure_keys(&value, &["known"], "test");
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
            &spec,
            &mut rows,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].backend, "ptrace");
        assert!(rows[0].ci);
    }

    const REASON_BUCKET: &str = CI_REASON_REQUIRED_BUCKETS[0];

    fn disabled_verify_spec(extra: &str) -> Value {
        parse_mode(&format!(
            r#"
ci = false
backends_enabled = ["ptrace"]
{extra}

[backends_disabled]
dbt = "unsupported"
kvm = "unsupported"
sabre = "unsupported"
liteinst = "unsupported"
"#
        ))
    }

    #[test]
    #[should_panic(expected = "must state why in a non-empty ci_disabled_reason")]
    fn rejects_silent_default_off_cell_in_ratcheted_bucket() {
        validate_mode(
            "bucket/test",
            REASON_BUCKET,
            "portable",
            "verify",
            &disabled_verify_spec(""),
            &mut Vec::new(),
        );
    }

    #[test]
    #[should_panic(expected = "must state why in a non-empty ci_disabled_reason")]
    fn rejects_blank_ci_disabled_reason() {
        validate_mode(
            "bucket/test",
            REASON_BUCKET,
            "portable",
            "verify",
            &disabled_verify_spec(r#"ci_disabled_reason = "   ""#),
            &mut Vec::new(),
        );
    }

    #[test]
    fn accepts_default_off_cell_that_states_a_reason() {
        let mut rows = Vec::new();
        validate_mode(
            "bucket/test",
            REASON_BUCKET,
            "portable",
            "verify",
            &disabled_verify_spec(r#"ci_disabled_reason = "fixture does not compile""#),
            &mut rows,
        );
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].ci);
    }

    #[test]
    #[should_panic(expected = "must not carry ci_disabled_reason")]
    fn rejects_stale_reason_on_enabled_cell() {
        let spec = parse_mode(
            r#"
ci = true
backends_enabled = ["ptrace"]
ci_disabled_reason = "left over from when this was off"

[backends_disabled]
dbt = "unsupported"
kvm = "unsupported"
sabre = "unsupported"
liteinst = "unsupported"
"#,
        );
        validate_mode(
            "bucket/test",
            REASON_BUCKET,
            "portable",
            "verify",
            &spec,
            &mut Vec::new(),
        );
    }

    #[test]
    fn leaves_unratcheted_buckets_alone() {
        // The ratchet must not silently impose the rule on every other bucket;
        // those still carry bare `ci = false` and must keep validating.
        let mut rows = Vec::new();
        validate_mode(
            "bucket/test",
            "c-programs",
            "portable",
            "verify",
            &disabled_verify_spec(""),
            &mut rows,
        );
        assert_eq!(rows.len(), 1);
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
                    "runner": "ci/test_harness.sh"
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
        .parse::<Value>()
        .expect("test manifest must be valid TOML");
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
        .parse::<Value>()
        .expect("test manifest must be valid TOML");
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
    #[should_panic(expected = "names a backend that is not enabled")]
    fn rejects_guest_args_for_disabled_backend() {
        let spec = parse_mode(
            r#"
ci = false
backends_enabled = ["ptrace"]
guest_args = { kvm = ["--kvm"] }

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
        validate_mode("bucket/test", "bucket", "portable", "chaos", spec, rows);
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
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].mode, "chaos");
    }

    #[test]
    fn accepts_chaos_mode_with_a_normalized_entropy_floor() {
        let spec = chaos_spec(
            "outcome_classes = 4",
            "min_distinct = 2\nmin_passes = 1\nmin_failures = 1\nmin_normalized_entropy = 0.5\n",
        );
        let mut rows = Vec::new();
        validate_chaos(&spec, &mut rows);
        assert_eq!(rows.len(), 1);
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
