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

use serde_json::json;
use toml::Value;

const KNOWN_BACKENDS: [&str; 5] = ["ptrace", "dbi", "kvm", "sabre", "liteinst"];
const MODES: [&str; 5] = ["verify", "chaos", "replay", "naked", "custom"];

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
    let direct = test.get("direct").and_then(Value::as_str);
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
        (None, Some(command)) if command.trim().is_empty() => {
            die(format!("{id}: direct command must not be empty"));
        }
        (None, Some(_)) => {}
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
    let mut allowed = vec!["ci", "backends_enabled", "backends_disabled"];
    match mode {
        "naked" => allowed.extend(["runs", "assert"]),
        "chaos" => allowed.extend(["seeds", "assert"]),
        "custom" => allowed.extend(["args", "assert"]),
        _ => {}
    }
    ensure_keys(spec_value, &allowed, &format!("{id}.modes.{mode}"));
    let ci = spec
        .get("ci")
        .and_then(Value::as_bool)
        .unwrap_or_else(|| die(format!("{id}: modes.{mode}.ci must be a boolean")));
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
        let assertions = spec
            .get("assert")
            .and_then(Value::as_table)
            .unwrap_or_else(|| die(format!("{id}: enabled chaos mode requires assert")));
        ensure_keys(
            spec.get("assert").unwrap(),
            &["min_distinct", "min_passes", "min_failures"],
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
    #[should_panic(expected = "must partition")]
    fn rejects_incomplete_backend_partition() {
        let spec = parse_mode(
            r#"
ci = false
backends_enabled = ["ptrace"]

[backends_disabled]
dbi = "unsupported"
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

[backends_disabled]
dbi = "unsupported"
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
}
