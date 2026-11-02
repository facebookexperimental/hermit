#!/usr/bin/env -S rust-script --force
//! Copyright (c) Meta Platforms, Inc. and affiliates.
//! All rights reserved.
//!
//! This source code is licensed under the BSD-style license found in the
//! LICENSE file in the root directory of this source tree.
//!
//! ```cargo
//! [dependencies]
//! serde_json = "1"
//! chrono = "0.4"
//! ```
//!
//! Per-day count of retries by failure class and error-kind signature,
//! from validate run records (ledger rows or run-record JSON files given
//! as file or directory arguments). Owner follow-up 2026-10-31: judge how
//! trustworthy the understood-infrastructure classifier is before any
//! rule lets infra-only retries count as clean green. This report only
//! counts; it changes no verdict (every retry still counts GREEN_FLAKY).
//!
//! A "retry" here is one failed attempt of a retried test, counted from
//! the run record's `retry_rollup_v1` (fallback: `retries_by_class` for
//! its total). Denominator line per day gives runs seen and total
//! retries, so each count reads as n / total-retries-that-day.
//!
//! Usage: retry-class-report.rs <run-record.json|ledger.jsonl|dir>...

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn day_of(record: &serde_json::Value) -> String {
    for key in ["finished_at", "recorded_at", "started_at"] {
        if let Some(text) = record.get(key).and_then(|v| v.as_str()) {
            if text.len() >= 10 {
                return text[..10].to_string();
            }
        }
    }
    "unknown-day".into()
}

fn signature_of(reason: Option<&str>, error_kind: Option<&str>) -> String {
    let reason = reason.unwrap_or_default();
    if reason.contains("HERMIT_SKID_OVERSHOOT") {
        "skid_overshoot".to_string()
    } else if reason.contains("host_input_changed") || reason.contains("outside the guest") {
        "host_input_changed".to_string()
    } else {
        error_kind.unwrap_or("unclassified").to_string()
    }
}

/// The four failure classes stay as they are; within
/// understood_infrastructure_failure the typed signatures are stable
/// subcategories (`pmu_skid`, `host_input_changed`, `sandbox_denied`).
fn class_of(class: &str) -> String {
    if class == "skid_overshoot" {
        "understood_infrastructure_failure".to_string()
    } else {
        class.to_string()
    }
}

fn subcategory_of(signature: &str) -> String {
    match signature {
        "skid_overshoot" => "pmu_skid".to_string(),
        "host_input_changed" => "host_input_changed".to_string(),
        "sandbox-denied" | "sandbox_denied" => "sandbox_denied".to_string(),
        other => other.to_string(),
    }
}

#[derive(Default)]
struct Daily {
    counts: BTreeMap<(String, String), u64>,
    cells: std::collections::BTreeSet<String>,
    cap_reached: u64,
    run_ids: std::collections::BTreeSet<String>,
}

fn count_record(
    record: &serde_json::Value,
    by_day: &mut BTreeMap<String, Daily>,
    runs: &mut BTreeMap<String, u64>,
    totals: &mut BTreeMap<String, u64>,
) {
    let day = day_of(record);
    let Some(rollup) = record
        .get("retry_rollup_v1")
        .or_else(|| record.get("retry_rollup"))
    else {
        return;
    };
    *runs.entry(day.clone()).or_default() += 1;
    *totals.entry(day.clone()).or_default() +=
        rollup.get("total_retries").and_then(|v| v.as_u64()).unwrap_or(0);
    let daily = by_day.entry(day).or_default();
    let counts = &mut daily.counts;
    if let Some(tests) = rollup.get("tests").and_then(|v| v.as_array()) {
        for test in tests {
            if test.get("retries").and_then(|v| v.as_u64()).unwrap_or(0) > 0 {
                if let Some(name) = test.get("test").and_then(|v| v.as_str()) {
                    daily.cells.insert(name.to_string());
                }
            }
            let outcomes = test
                .get("attempt_outcomes")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let classes = test
                .get("attempt_classes")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let kinds = test
                .get("attempt_error_kinds")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let signatures = test
                .get("attempt_signatures")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            for (i, outcome) in outcomes.iter().enumerate() {
                if outcome.as_str() == Some("PASS") {
                    continue;
                }
                let class = classes
                    .get(i)
                    .and_then(|v| v.as_str())
                    .unwrap_or("unclassified")
                    .to_string();
                let signature = signatures
                    .get(i)
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        kinds
                            .get(i)
                            .and_then(|v| v.as_str())
                            .unwrap_or("unclassified")
                            .to_string()
                    });
                let bucket = class_of(&class);
                let signature = subcategory_of(&signature);
                *counts.entry((bucket, signature)).or_default() += 1;
            }
        }
    }
}

/// Count one `results.jsonl` (e2e harness rows) into `daily`: each failed
/// attempt of a retried cell, bucketed like the rollup. A skid attempt at
/// the attempt cap with no later attempt counts as cap-reached.
fn count_results(text: &str, daily: &mut Daily, totals: &mut u64) {
    let mut histories: BTreeMap<String, Vec<serde_json::Value>> = BTreeMap::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(row) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let key = format!(
            "{}/{}/{}/{}",
            row.get("test").and_then(|v| v.as_str()).unwrap_or("?"),
            row.get("mode").and_then(|v| v.as_str()).unwrap_or("?"),
            row.get("backend").and_then(|v| v.as_str()).unwrap_or("?"),
            row.get("category").and_then(|v| v.as_str()).unwrap_or("?")
        );
        histories.entry(key).or_default().push(row);
    }
    for (cell, mut rows) in histories {
        if rows.len() < 2 {
            continue;
        }
        rows.sort_by_key(|r| r.get("attempt").and_then(|v| v.as_u64()).unwrap_or(0));
        daily.cells.insert(cell);
        for row in &rows {
            if row.get("outcome").and_then(|v| v.as_str()) == Some("PASS") {
                continue;
            }
            *totals += 1;
            let class = row
                .get("failure_class")
                .and_then(|v| v.as_str())
                .unwrap_or("unclassified");
            let signature = signature_of(
                row.get("reason").and_then(|v| v.as_str()),
                row.get("error_kind").and_then(|v| v.as_str()),
            );
            let attempt = row.get("attempt").and_then(|v| v.as_u64()).unwrap_or(0);
            let is_last = rows
                .iter()
                .map(|r| r.get("attempt").and_then(|v| v.as_u64()).unwrap_or(0))
                .max()
                .unwrap_or(0)
                == attempt;
            if signature == "skid_overshoot" && attempt >= 2 && is_last {
                daily.cap_reached += 1;
            }
            let bucket = class_of(class);
            let signature = subcategory_of(&signature);
            *daily.counts.entry((bucket, signature)).or_default() += 1;
        }
    }
}

fn collect(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_dir() {
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                collect(&entry.path(), out);
            }
        }
    } else if path.extension().is_some_and(|e| e == "json" || e == "jsonl") {
        out.push(path.to_path_buf());
    }
}

/// One durable row per day in the published test ledger series
/// (`retry-classes/v1`): date, validate runs, retries by bucket and by
/// signature, cap-reached count, and the cells/tests involved. Rewriting
/// a day replaces its row, so a backfill and later live writes converge.
fn write_daily_rows(
    dir: &Path,
    by_day: &BTreeMap<String, Daily>,
    runs: &BTreeMap<String, u64>,
    totals: &BTreeMap<String, u64>,
) {
    std::fs::create_dir_all(dir).expect("create daily ledger dir");
    for (day, daily) in by_day {
        let by_bucket: BTreeMap<&str, u64> = {
            let mut map: BTreeMap<&str, u64> = BTreeMap::new();
            for ((bucket, _), n) in &daily.counts {
                *map.entry(bucket.as_str()).or_default() += n;
            }
            map
        };
        let by_subcategory: BTreeMap<&str, BTreeMap<&str, u64>> = {
            let mut map: BTreeMap<&str, BTreeMap<&str, u64>> = BTreeMap::new();
            for ((class, subcategory), n) in &daily.counts {
                *map.entry(class.as_str())
                    .or_default()
                    .entry(subcategory.as_str())
                    .or_default() += n;
            }
            map
        };
        let summary = format!(
            "{}: {} retries in {} validate runs (product {}, infra {}, no_result {}); cap-reached {}; see retries_by_class/retries_by_subcategory",
            day,
            totals.get(day).copied().unwrap_or(0),
            runs.get(day)
                .copied()
                .unwrap_or(0)
                .max(daily.run_ids.len() as u64),
            by_bucket.get("product_failure").copied().unwrap_or(0),
            by_bucket
                .get("understood_infrastructure_failure")
                .copied()
                .unwrap_or(0),
            by_bucket.get("no_result").copied().unwrap_or(0),
            daily.cap_reached,
        );
        let row = serde_json::json!({
            "schema": "retry-classes/v1",
            "summary": summary,
            "date": day,
            "validate_runs": runs.get(day).copied().unwrap_or(0).max(daily.run_ids.len() as u64),
            "total_retries": totals.get(day).copied().unwrap_or(0),
            "retries_by_class": by_bucket,
            "retries_by_subcategory": by_subcategory,
            "cap_reached": daily.cap_reached,
            "cells_involved": daily.cells.iter().collect::<Vec<_>>(),
        });
        let month = if day.len() >= 7 { &day[..7] } else { "unknown" };
        let path = dir.join(format!("{month}.jsonl"));
        let mut kept: Vec<String> = Vec::new();
        if let Ok(text) = std::fs::read_to_string(&path) {
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                let is_same_day = serde_json::from_str::<serde_json::Value>(line)
                    .ok()
                    .and_then(|v| {
                        v.get("date")
                            .and_then(|d| d.as_str())
                            .map(|d| d == day.as_str())
                    })
                    .unwrap_or(false);
                if !is_same_day {
                    kept.push(line.to_string());
                }
            }
        }
        kept.push(row.to_string());
        kept.sort();
        std::fs::write(&path, kept.join("\n") + "\n").expect("write daily ledger row");
        println!(
            "daily {day}: {} retries ({} runs), cap-reached {}, buckets {:?}",
            totals.get(day).copied().unwrap_or(0),
            runs.get(day)
                .copied()
                .unwrap_or(0)
                .max(daily.run_ids.len() as u64),
            daily.cap_reached,
            by_bucket
        );
    }
}

fn main() {
    rust_script_prelude::init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: retry-class-report.rs <run-record.json|ledger.jsonl|dir>...");
        std::process::exit(2);
    }
    let mut files = Vec::new();
    for arg in &args {
        collect(Path::new(arg), &mut files);
    }
    let mut by_day: BTreeMap<String, Daily> = BTreeMap::new();
    let mut runs: BTreeMap<String, u64> = BTreeMap::new();
    let mut totals: BTreeMap<String, u64> = BTreeMap::new();
    let mut rows = 0u64;
    // `--daily-ledger DIR`: also write one durable row per day into
    // DIR/YYYY-MM.jsonl (the day's row is replaced, never duplicated).
    let daily_dir = args
        .windows(2)
        .find(|w| w[0] == "--daily-ledger")
        .map(|w| PathBuf::from(&w[1]));
    for file in &files {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        if file.file_name().is_some_and(|n| n == "results.jsonl") {
            let day = std::fs::metadata(file)
                .and_then(|m| m.modified())
                .map(|t| {
                    chrono::DateTime::<chrono::Local>::from(t)
                        .format("%Y-%m-%d")
                        .to_string()
                })
                .unwrap_or_else(|_| "unknown-day".to_string());
            let daily = by_day.entry(day.clone()).or_default();
            if let Some(run) = file
                .ancestors()
                .find(|a| a.file_name().is_some_and(|n| n == "artifacts"))
                .and_then(|a| file.strip_prefix(a).ok())
                .and_then(|r| r.components().next())
            {
                daily
                    .run_ids
                    .insert(run.as_os_str().to_string_lossy().into_owned());
            }
            let total = totals.entry(day).or_default();
            count_results(&text, daily, total);
            continue;
        }
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            // Ledger rows wrap the run record; run records are the row.
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let record = value.get("legacy_row").unwrap_or(&value);
            if record.get("retry_rollup_v1").is_some() || record.get("retry_rollup").is_some() {
                rows += 1;
                count_record(record, &mut by_day, &mut runs, &mut totals);
            }
        }
    }
    println!("day,runs,total_retries,failure_class,subcategory,retries");
    for (day, daily) in &by_day {
        let run_count = runs.get(day).copied().unwrap_or(0);
        let total = totals.get(day).copied().unwrap_or(0);
        if daily.counts.is_empty() {
            println!("{day},{run_count},{total},(none),(none),0");
        }
        for ((bucket, signature), n) in &daily.counts {
            println!("{day},{run_count},{total},{bucket},{signature},{n}");
        }
    }
    if let Some(dir) = &daily_dir {
        write_daily_rows(dir, &by_day, &runs, &totals);
    }
    eprintln!("counted {rows} run record(s) from {} file(s)", files.len());
}
