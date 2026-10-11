#!/usr/bin/env -S rust-script --force
//! ```cargo
//! [package]
//! edition = "2024"
//! [dependencies]
//! hermit-manifest-plan = { path = "../../ci/manifest-plan" }
//! detcore-model = { path = "../../detcore-model" }
//! ```

/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

pub mod driver;
#[path = "../../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

fn selection(value: &str) -> Vec<&str> {
    value.split(',').collect()
}

fn validate_selection(
    name: &str,
    selected: &Option<String>,
    allowed: BTreeSet<String>,
) -> Result<(), String> {
    let Some(selected) = selected else {
        return Ok(());
    };
    let mut seen = BTreeSet::new();
    for value in selection(selected) {
        if !allowed.contains(value) {
            return Err(format!(
                "{name} contains an unknown or empty selection {value:?}"
            ));
        }
        if !seen.insert(value) {
            return Err(format!("{name} repeats selection {value:?}"));
        }
    }
    Ok(())
}

fn bytes_hex(value: &[u8]) -> String {
    let mut encoded = String::new();
    for byte in value {
        write!(encoded, "{byte:02x}").expect("String writes succeed");
    }
    encoded
}

fn write_summary(
    prepared: &driver::Prepared,
    required: &[driver::Cell],
    collected: &[driver::Collected],
    accepted_scope: &str,
) -> Result<(), String> {
    use driver::conformance::Verdict;

    let mut summary = format!(
        "# schema=backend-isolation-row-summary-v1\n# accepted_scope={accepted_scope} counts_scope=full_assertion_rows\n# source_inputs=inputs.sha256 installation_inputs=installation.inputs\n# separate tool/refusal entries remain in assertions.tsv and required-cells.tsv, excluded from backend totals\nbackend\texecution_path\tcollection_state\trequired_ids\tattempted_ids\tuncollected_ids\temitted_rows\tmatch\tmismatch\tno_result\n",
    );
    let mut retained = String::from(
        "# schema=backend-isolation-assertion-rows-v1\n# name_hex and detail_hex retain the complete original UTF-8 bytes; multiplicity is preserved\ncell\trow_index\tname_hex\tverdict\tdetail_hex\n",
    );
    for result in collected {
        for (index, row) in result.assertions.iter().enumerate() {
            writeln!(
                retained,
                "{}\t{index}\t{}\t{}\t{}",
                result.cell.name(),
                bytes_hex(row.name.as_bytes()),
                row.verdict.name(),
                bytes_hex(row.detail.as_bytes()),
            )
            .expect("String writes succeed");
        }
    }
    for backend in driver::conformance::BACKENDS {
        let required_ids = required
            .iter()
            .filter(|cell| {
                cell.backend == backend && driver::REQUIRED_PATHS.contains(&cell.path.as_str())
            })
            .count();
        let operands: Vec<_> = collected
            .iter()
            .filter(|result| {
                result.cell.backend == backend
                    && driver::REQUIRED_PATHS.contains(&result.cell.path.as_str())
            })
            .collect();
        let attempted_ids = operands.len();
        let uncollected_ids = required_ids - attempted_ids;
        let execution_path = if backend == "e9patch" {
            "ptrace_with_e9patch_preprocessing"
        } else {
            backend
        };
        if operands.is_empty() {
            writeln!(summary, "{backend}\t{execution_path}\tuncollected\t{required_ids}\t0\t{uncollected_ids}\tNA\tNA\tNA\tNA")
                .expect("String writes succeed");
            println!(
                "backend={backend} required_ids={required_ids} attempted_ids=0 uncollected_ids={uncollected_ids} emitted_rows=unmeasured M=unmeasured X=unmeasured NR=unmeasured"
            );
            continue;
        }
        let mut counts = [0_usize; 3];
        for result in operands {
            for row in &result.assertions {
                counts[match row.verdict {
                    Verdict::Match => 0,
                    Verdict::Mismatch => 1,
                    Verdict::NoResult => 2,
                }] += 1;
            }
        }
        let [matched, mismatched, no_result] = counts;
        let emitted_rows = matched + mismatched + no_result;
        writeln!(summary, "{backend}\t{execution_path}\tattempted\t{required_ids}\t{attempted_ids}\t{uncollected_ids}\t{emitted_rows}\t{matched}\t{mismatched}\t{no_result}")
            .expect("String writes succeed");
        println!(
            "backend={backend} required_ids={required_ids} attempted_ids={attempted_ids} uncollected_ids={uncollected_ids} emitted_rows={emitted_rows} M={matched} X={mismatched} NR={no_result}"
        );
    }
    let ordinary = required
        .iter()
        .filter(|cell| driver::REQUIRED_PATHS.contains(&cell.path.as_str()))
        .count();
    let cohorts: BTreeSet<_> = required
        .iter()
        .filter(|cell| driver::REQUIRED_PATHS.contains(&cell.path.as_str()))
        .map(|cell| {
            format!(
                "{}:{}:{}",
                cell.root,
                cell.path,
                driver::policy_name(cell.policy),
            )
        })
        .collect();
    let backends = driver::conformance::BACKENDS.len();
    let pair_slots = cohorts.len() * backends * (backends - 1) / 2;
    let mut ledger = format!(
        "# schema=backend-isolation-required-ledger-v1\n# required_ids={} ordinary_ids={ordinary} separate_entries={} required_cohorts={} required_pair_slots={pair_slots}\n# cohort_comparison=uncollected pair_comparison=uncollected; no parity or whole-input authority is inferred from row counts\n# uncollected coverage no_result is not an emitted assertion row\ncell\tclass\tcollection_state\tfull_verdict\temitted_rows\treport_path_hex\n",
        required.len(),
        required.len() - ordinary,
        cohorts.len(),
    );
    for cell in required {
        let class = if driver::REQUIRED_PATHS.contains(&cell.path.as_str()) {
            "backend_cell"
        } else {
            "separate_entry"
        };
        if let Some(result) = collected.iter().find(|result| result.cell == *cell) {
            writeln!(
                ledger,
                "{}\t{class}\tattempted\t{}\t{}\t{}",
                cell.name(),
                if result.full_matches() {
                    "match"
                } else {
                    "mismatch_or_no_result"
                },
                result.assertions.len(),
                bytes_hex(
                    result
                        .directory
                        .join("assertions.json")
                        .as_os_str()
                        .as_encoded_bytes(),
                ),
            )
            .expect("String writes succeed");
        } else {
            writeln!(
                ledger,
                "{}\t{class}\tuncollected\tunmeasured\tNA\tNA",
                cell.name()
            )
            .expect("String writes succeed");
        }
    }
    let mut failures = Vec::new();
    for (name, bytes) in [
        ("assertions.tsv", retained),
        ("summary.tsv", summary),
        ("required-cells.tsv", ledger),
    ] {
        if let Err(error) = fs::write(prepared.artifacts.join(name), bytes.as_bytes()) {
            failures.push(format!("{name}: {error}"));
            eprintln!("isolation conformance retained {name} after publication failure:\n{bytes}");
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

fn run() -> Result<bool, String> {
    let mut repository = std::env::current_dir().map_err(|error| format!("repository: {error}"))?;
    let mut hermit = None;
    let mut safehermit = None;
    let mut artifacts = None;
    let mut installation = None;
    let mut descriptor_obligations = None;
    let mut descriptor_obligations_sha256 = None;
    let mut backends = None;
    let mut roots = None;
    let mut paths = None;
    let mut policies = None;
    let mut scope = "full".to_owned();
    let mut list = false;
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        if matches!(argument.as_str(), "--help" | "-h") {
            println!("Usage: conformance.rs --hermit PATH --artifacts DIR [OPTIONS]");
            println!("Defaults to every required backend, root, path, and namespace policy.");
            println!(
                "Options: --repository DIR --safehermit PATH --installation DIR --backends LIST --roots LIST --paths LIST --policies LIST --descriptor-obligations FILE --descriptor-obligations-sha256 HEX --assert-scope full|launch-migration --list"
            );
            println!(
                "Missing resources and incomplete observations fail requested cells. A selected scope makes no broader parity claim."
            );
            println!(
                "Artifacts include lossless assertion rows, per-backend M/X/NR totals, and the complete required-cell ledger; uncollected IDs are excluded from emitted-row totals."
            );
            return Ok(true);
        }
        if argument == "--list" {
            list = true;
            index += 1;
            continue;
        }
        let (name, value, consumed) = if let Some((name, value)) = argument.split_once('=') {
            (name, value, 1)
        } else {
            (
                argument.as_str(),
                arguments
                    .get(index + 1)
                    .ok_or_else(|| format!("{argument} requires a value"))?
                    .as_str(),
                2,
            )
        };
        match name {
            "--repository" => repository = PathBuf::from(value),
            "--hermit" => hermit = Some(PathBuf::from(value)),
            "--safehermit" => safehermit = Some(PathBuf::from(value)),
            "--artifacts" => artifacts = Some(PathBuf::from(value)),
            "--installation" => installation = Some(PathBuf::from(value)),
            "--descriptor-obligations" => {
                if descriptor_obligations
                    .replace(PathBuf::from(value))
                    .is_some()
                {
                    return Err("--descriptor-obligations is repeated".to_owned());
                }
            }
            "--descriptor-obligations-sha256" => {
                if descriptor_obligations_sha256
                    .replace(value.to_owned())
                    .is_some()
                {
                    return Err("--descriptor-obligations-sha256 is repeated".to_owned());
                }
            }
            "--backends" => backends = Some(value.to_owned()),
            "--roots" => roots = Some(value.to_owned()),
            "--paths" => paths = Some(value.to_owned()),
            "--policies" => policies = Some(value.to_owned()),
            "--assert-scope" => scope = value.to_owned(),
            _ => return Err(format!("unknown option {name}")),
        }
        index += consumed;
    }
    if !matches!(scope.as_str(), "full" | "launch-migration") {
        return Err("--assert-scope must be full or launch-migration".to_owned());
    }
    let required = driver::required_cells();
    validate_selection(
        "--backends",
        &backends,
        required.iter().map(|cell| cell.backend.clone()).collect(),
    )?;
    validate_selection(
        "--roots",
        &roots,
        required.iter().map(|cell| cell.root.clone()).collect(),
    )?;
    validate_selection(
        "--paths",
        &paths,
        required.iter().map(|cell| cell.path.clone()).collect(),
    )?;
    validate_selection(
        "--policies",
        &policies,
        required
            .iter()
            .map(|cell| driver::policy_name(cell.policy).to_owned())
            .collect(),
    )?;
    let cells: Vec<_> = required
        .iter()
        .filter(|cell| {
            backends
                .as_ref()
                .is_none_or(|value| selection(value).contains(&cell.backend.as_str()))
                && roots
                    .as_ref()
                    .is_none_or(|value| selection(value).contains(&cell.root.as_str()))
                && paths
                    .as_ref()
                    .is_none_or(|value| selection(value).contains(&cell.path.as_str()))
                && policies.as_ref().is_none_or(|value| {
                    selection(value).contains(&driver::policy_name(cell.policy))
                })
        })
        .cloned()
        .collect();
    if cells.is_empty() {
        return Err("selection names no required cells".to_owned());
    }
    if scope == "launch-migration"
        && (backends.is_none()
            || roots.is_none()
            || paths.is_none()
            || policies.is_none()
            || cells.iter().any(|cell| {
                cell.root != "host"
                    || cell.path != "run-raw"
                    || cell.policy != driver::conformance::NamespacePolicy::Private
            }))
    {
        return Err(
            "launch-migration scope requires an explicit host/run-raw/private selection".to_owned(),
        );
    }
    let descriptor_registry = match (descriptor_obligations, descriptor_obligations_sha256) {
        (None, None) => None,
        (Some(path), Some(expected)) => Some(driver::load_descriptor_registry(&path, &expected, &cells)?),
        _ => return Err("--descriptor-obligations and --descriptor-obligations-sha256 must be supplied together".to_owned()),
    };
    if list {
        for cell in cells {
            println!("{}", cell.name());
        }
        return Ok(true);
    }
    let hermit = hermit.ok_or("--hermit is required")?;
    let artifacts = artifacts
        .ok_or("--artifacts is required and must name a fresh ignored/ or target/ directory")?;
    let safehermit = safehermit.map_or_else(|| driver::find_safehermit(&repository), Ok)?;
    let prepared = driver::Prepared::build_with_installation_and_registry(
        &repository,
        &hermit,
        &safehermit,
        &artifacts,
        installation.as_deref(),
        descriptor_registry,
    )?;
    let mut matches = true;
    let mut collected = Vec::new();
    for cell in cells {
        let result = prepared.collect(cell);
        let accepted = if scope == "launch-migration" {
            result.launch_migration_matches()
        } else {
            result.full_matches()
        };
        let failures = result
            .assertions
            .iter()
            .filter(|row| row.verdict != driver::conformance::Verdict::Match)
            .count();
        println!(
            "{} scope={} verdict={} full_required_failures={} report={}",
            result.cell.name(),
            scope,
            if accepted {
                "match"
            } else {
                "mismatch_or_no_result"
            },
            failures,
            result.directory.join("assertions.json").display()
        );
        matches &= accepted;
        collected.push(result);
    }
    write_summary(&prepared, &required, &collected, &scope).map_err(|error| {
        format!(
            "summary publication: {error}; checker verdict={}; emitted assertion verdicts are unchanged",
            if matches {
                "match"
            } else {
                "mismatch_or_no_result"
            },
        )
    })?;
    Ok(matches)
}

fn main() {
    rust_script_prelude::init();
    match run() {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(error) => {
            eprintln!("isolation conformance: {error}");
            std::process::exit(2);
        }
    }
}
