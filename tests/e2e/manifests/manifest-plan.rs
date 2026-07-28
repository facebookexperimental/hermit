#!/usr/bin/env rust-script
//! Copyright (c) Meta Platforms, Inc. and affiliates.
//! All rights reserved.
//!
//! This source code is licensed under the BSD-style license found in the
//! LICENSE file in the root directory of this source tree.
//!
//! Prototype loader for the centralized e2e manifests (schema v2).
//!
//! It parses every `*.toml` beside this script, enforces the validation rules
//! documented in `README.md`, and prints the expanded execution plan — one row
//! per `(test, mode, enabled-backend)` cell — exactly the fan-out a harness
//! successor would feed to the runner. It is the machine-consumable proof that
//! the manifest format is real, not just documentation.
//!
//! Usage:
//!   ./manifest-plan.rs              # validate + print the plan (text)
//!   ./manifest-plan.rs --format json
//!
//! ```cargo
//! [dependencies]
//! toml = "0.8"
//! ```

use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::process::exit;

use toml::Value;

const KNOWN_BACKENDS: [&str; 5] = ["ptrace", "dbi", "kvm", "sabre", "liteinst"];
const ACCOUNTED_MODES: [&str; 4] = ["verify", "chaos", "replay", "naked"];

#[derive(Debug)]
struct PlanRow {
    bucket: String,
    id: String,
    lane: String,
    mode: String,
    backend: String, // "native" for naked
}

fn die(msg: String) -> ! {
    eprintln!("manifest-plan: {msg}");
    exit(1);
}

fn main() {
    let json = std::env::args().any(|a| a == "--format=json")
        || std::env::args().collect::<Vec<_>>().windows(2).any(|w| w[0] == "--format" && w[1] == "json");

    let script_dir = Path::new(file!())
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    // repo root = tests/e2e/manifests/../../.. ; used to check program paths exist.
    let repo_root = script_dir.join("../../..");

    let mut manifests: Vec<PathBuf> = std::fs::read_dir(&script_dir)
        .unwrap_or_else(|e| die(format!("cannot read {}: {e}", script_dir.display())))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|x| x == "toml").unwrap_or(false))
        .collect();
    manifests.sort();
    if manifests.is_empty() {
        die(format!("no *.toml manifests found in {}", script_dir.display()));
    }

    let mut rows: Vec<PlanRow> = Vec::new();
    let mut seen_ids: BTreeSet<String> = BTreeSet::new();

    for path in &manifests {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| die(format!("cannot read {}: {e}", path.display())));
        let doc: Value = text
            .parse()
            .unwrap_or_else(|e| die(format!("{}: invalid TOML: {e}", path.display())));
        let where_ = path.file_name().unwrap().to_string_lossy().to_string();

        let schema = doc.get("schema").and_then(Value::as_integer);
        if schema != Some(2) {
            die(format!("{where_}: schema must be 2, got {schema:?}"));
        }
        let bucket = doc
            .get("bucket")
            .and_then(Value::as_str)
            .unwrap_or_else(|| die(format!("{where_}: missing string `bucket`")))
            .to_string();
        let stem = path.file_stem().unwrap().to_string_lossy();
        if bucket != stem {
            die(format!("{where_}: bucket `{bucket}` must equal file stem `{stem}`"));
        }

        let tests = doc
            .get("test")
            .and_then(Value::as_array)
            .unwrap_or_else(|| die(format!("{where_}: missing [[test]] array")));

        for test in tests {
            validate_and_expand(test, &bucket, &where_, &repo_root, &mut seen_ids, &mut rows);
        }
    }

    rows.sort_by(|a, b| {
        (&a.bucket, &a.id, &a.mode, &a.backend).cmp(&(&b.bucket, &b.id, &b.mode, &b.backend))
    });

    if json {
        let items: Vec<String> = rows
            .iter()
            .map(|r| {
                format!(
                    r#"{{"bucket":"{}","test":"{}","lane":"{}","mode":"{}","backend":"{}"}}"#,
                    r.bucket, r.id, r.lane, r.mode, r.backend
                )
            })
            .collect();
        println!("[{}]", items.join(","));
    } else {
        println!("{:<10}\t{:<38}\t{:<10}\t{:<7}\t{}", "LANE", "TEST", "MODE", "BACKEND", "BUCKET");
        for r in &rows {
            println!(
                "{:<10}\t{:<38}\t{:<10}\t{:<7}\t{}",
                r.lane, r.id, r.mode, r.backend, r.bucket
            );
        }
        let tests: BTreeSet<&String> = rows.iter().map(|r| &r.id).collect();
        eprintln!(
            "\nPASS: {} manifest(s), {} test(s), {} plan cells validated",
            manifests.len(),
            tests.len(),
            rows.len()
        );
    }
}

fn validate_and_expand(
    test: &Value,
    bucket: &str,
    where_: &str,
    repo_root: &Path,
    seen_ids: &mut BTreeSet<String>,
    rows: &mut Vec<PlanRow>,
) {
    let id = test
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_else(|| die(format!("{where_}: a [[test]] is missing string `id`")))
        .to_string();
    if !id.starts_with(&format!("{bucket}/")) {
        die(format!("{where_}: id `{id}` must start with `{bucket}/`"));
    }
    if !seen_ids.insert(id.clone()) {
        die(format!("duplicate test id across manifests: {id}"));
    }

    let lane = test.get("lane").and_then(Value::as_str).unwrap_or("");
    if lane != "portable" && lane != "privileged" {
        die(format!("{id}: lane must be portable|privileged, got `{lane}`"));
    }
    match test.get("timeout_seconds").and_then(Value::as_integer) {
        Some(t) if (1..=1800).contains(&t) => {}
        other => die(format!("{id}: timeout_seconds must be 1..=1800, got {other:?}")),
    }

    // Exactly one of `program` / `direct`; program path must exist + known ext.
    let program = test.get("program").and_then(Value::as_str);
    let direct = test.get("direct").and_then(Value::as_str);
    match (program, direct) {
        (Some(_), Some(_)) => die(format!("{id}: set only one of `program`/`direct`")),
        (None, None) => die(format!("{id}: must set `program` or `direct`")),
        (Some(p), None) => {
            let ext = Path::new(p).extension().and_then(|x| x.to_str()).unwrap_or("");
            if !["sh", "c", "rs"].contains(&ext) {
                die(format!("{id}: program `{p}` must end in .sh/.c/.rs"));
            }
            if !repo_root.join(p).exists() {
                die(format!("{id}: program path does not exist: {p}"));
            }
        }
        (None, Some(_)) => {}
    }

    let modes = test.get("modes").and_then(Value::as_table);
    let disabled = test.get("disabled_modes").and_then(Value::as_table);

    // Every accounted mode is present in exactly one of modes/disabled_modes.
    for m in ACCOUNTED_MODES {
        let in_modes = modes.map(|t| t.contains_key(m)).unwrap_or(false);
        let in_disabled = disabled.map(|t| t.contains_key(m)).unwrap_or(false);
        match (in_modes, in_disabled) {
            (true, true) => die(format!("{id}: mode `{m}` is both enabled and disabled")),
            (false, false) => die(format!(
                "{id}: mode `{m}` is neither enabled nor given a disabled reason"
            )),
            _ => {}
        }
    }
    if let Some(dt) = disabled {
        for (m, reason) in dt {
            if reason.as_str().map(str::is_empty).unwrap_or(true) {
                die(format!("{id}: disabled_modes.{m} needs a non-empty reason"));
            }
        }
    }

    let Some(modes) = modes else {
        die(format!("{id}: missing [test.modes]"))
    };
    for (mode, spec) in modes {
        if mode == "naked" {
            // Native sanity check: no backend axis.
            let runs = spec.get("runs").and_then(Value::as_integer).unwrap_or(3);
            if !(2..=20).contains(&runs) {
                die(format!("{id}: naked.runs must be 2..=20, got {runs}"));
            }
            rows.push(PlanRow {
                bucket: bucket.to_string(),
                id: id.clone(),
                lane: lane.to_string(),
                mode: mode.clone(),
                backend: "native".to_string(),
            });
            continue;
        }

        let enabled: Vec<String> = spec
            .get("backends_enabled")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        if enabled.is_empty() {
            die(format!("{id}: mode `{mode}` has empty backends_enabled"));
        }
        for b in &enabled {
            if !KNOWN_BACKENDS.contains(&b.as_str()) {
                die(format!("{id}: mode `{mode}` unknown backend `{b}`"));
            }
        }
        if mode == "replay" && enabled.iter().any(|b| b != "ptrace") {
            die(format!("{id}: replay is ptrace-only, got {enabled:?}"));
        }
        // backends_disabled keys must be disjoint from enabled.
        if let Some(dis) = spec.get("backends_disabled").and_then(Value::as_table) {
            for (b, reason) in dis {
                if enabled.contains(b) {
                    die(format!("{id}: mode `{mode}` backend `{b}` both enabled and disabled"));
                }
                if reason.as_str().map(str::is_empty).unwrap_or(true) {
                    die(format!("{id}: mode `{mode}` backends_disabled.{b} needs a reason"));
                }
            }
        }
        for b in enabled {
            rows.push(PlanRow {
                bucket: bucket.to_string(),
                id: id.clone(),
                lane: lane.to_string(),
                mode: mode.clone(),
                backend: b,
            });
        }
    }
}
