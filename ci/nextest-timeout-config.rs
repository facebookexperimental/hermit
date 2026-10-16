#!/usr/bin/env -S rust-script --force
//! Produce a nextest configuration with independent per-test CPU and wall
//! bounds scaled by their machine-specific multipliers.
//!
//! ```cargo
//! [dependencies]
//! toml_edit = "0.22.27"
//! serde = { version = "1", features = ["derive"] }
//! serde_json = "1"
//! sha2 = "0.10"
//! ```

use std::env;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use sha2::Digest;
use sha2::Sha256;
use toml_edit::Array;
use toml_edit::ArrayOfTables;
use toml_edit::DocumentMut;
use toml_edit::InlineTable;
use toml_edit::Item;
use toml_edit::Table;
use toml_edit::Value;
use toml_edit::value;

#[path = "../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude;

#[allow(dead_code)]
#[path = "manifest-plan/src/timeouts.rs"]
mod timeouts;

#[allow(dead_code)]
#[path = "manifest-plan/src/nextest_cpu.rs"]
mod nextest_cpu;

use nextest_cpu::BudgetCalibration;
use nextest_cpu::BudgetContext;
use nextest_cpu::ResolvedBudget;
use nextest_cpu::ResolvedBudgets;

const CPU_WRAPPER_BIN_ENV: &str = "HERMIT_NEXTEST_CPU_WRAPPER_BIN";
const CPU_WRAPPER_NAME: &str = "hermit-per-test-cpu";
const DETCORE_ALLOCATOR_WRAPPER_NAME: &str = "hermit-detcore-allocator-cpu";
const DETCORE_CONSTRUCTOR_WRAPPER_NAME: &str = "hermit-detcore-constructor-cpu";
const STANDALONE_ALLOCATOR_WRAPPER_NAME: &str = "hermit-standalone-allocator-cpu";
// Authorized current-source recalibration, not success under the old 22 CPU seconds:
// b27e26fc/67474066, N=3 CPU seconds: 18.762935, 19.040171, 20.171435.
// The existing scalar formula gives ceil(1.5 * max(samples)) = 31; wall stays 57.
const DETCORE_ALLOCATOR_CPU_SECONDS: u64 = 31;
// d554fa09/6109a170 host constructor N=5: maximum completed CPU 21.619340s;
// ceil(1.5 * max(samples)) = 33. Retained d7efd4cb/da86dfbe diagnostic N=3
// at CPU44/wall57 completed in 18.984049-19.191488s; old CPU22 timeouts remain censored.
const DETCORE_CONSTRUCTOR_CPU_SECONDS: u64 = 33;
// Retained d7efd4cb/da86dfbe standalone diagnostic N=3 at CPU44/wall57:
// 17.718674, 17.632669, 17.573148s. The completed-sample formula would give 27.
// Use 33 conservatively after the concurrent CPU22 timeout at 22.799306s;
// that censored failure has unknown completion cost. Wall and cleanup stay unchanged.
const STANDALONE_ALLOCATOR_CPU_SECONDS: u64 = 33;
const CPU_TERMINATION_GRACE_SECONDS: u64 = 2;

#[derive(Clone, Copy)]
struct MeasuredCpuAllowance {
    wrapper_name: &'static str,
    binary: &'static str,
    test: &'static str,
    cpu_seconds: u64,
}

impl MeasuredCpuAllowance {
    fn identity(self) -> nextest_cpu::AttemptIdentity {
        nextest_cpu::AttemptIdentity {
            package: "hermit".into(),
            binary: self.binary.into(),
            test: self.test.into(),
            attempt: 1,
        }
    }
}

const MEASURED_CPU_ALLOWANCES: [MeasuredCpuAllowance; 3] = [
    MeasuredCpuAllowance {
        wrapper_name: DETCORE_ALLOCATOR_WRAPPER_NAME,
        binary: "hermit::liteinst_allocator",
        test: "genuine_detcore_leaf_keeps_scope_free_work_private",
        cpu_seconds: DETCORE_ALLOCATOR_CPU_SECONDS,
    },
    MeasuredCpuAllowance {
        wrapper_name: DETCORE_CONSTRUCTOR_WRAPPER_NAME,
        binary: "hermit::liteinst_stacks",
        test: "cold_constructor::genuine_detcore_constructor_uses_retained_disjoint_bootstrap_stack",
        cpu_seconds: DETCORE_CONSTRUCTOR_CPU_SECONDS,
    },
    MeasuredCpuAllowance {
        wrapper_name: STANDALONE_ALLOCATOR_WRAPPER_NAME,
        binary: "hermit::liteinst_allocator",
        test: "genuine_standalone_leaf_keeps_scope_free_work_private",
        cpu_seconds: STANDALONE_ALLOCATOR_CPU_SECONDS,
    },
];

fn usage() -> &'static str {
    "usage: nextest-timeout-config.rs SOURCE MULTIPLIER OUTPUT"
}

fn parse_period_seconds(period: &str) -> Result<u64, String> {
    let digits = period.strip_suffix('s').ok_or_else(|| {
        format!("slow-timeout.period must be a positive whole-second duration, got {period:?}")
    })?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!(
            "slow-timeout.period must be a positive whole-second duration, got {period:?}"
        ));
    }
    let seconds = digits
        .parse::<u64>()
        .map_err(|error| format!("invalid slow-timeout.period {period:?}: {error}"))?;
    if seconds == 0 {
        return Err("slow-timeout.period must be greater than zero".into());
    }
    Ok(seconds)
}

fn scale_period_value(period: &mut Value, multiplier: f64) -> Result<u64, String> {
    let source = period
        .as_str()
        .ok_or_else(|| "slow-timeout.period must be a quoted duration string".to_string())?;
    let base_seconds = parse_period_seconds(source)?;
    let scaled = timeouts::scale_timeout_seconds(base_seconds, multiplier, "wall multiplier")?;
    *period = Value::from(format!("{scaled}s"));
    Ok(scaled)
}

fn scale_inline_timeout(table: &mut InlineTable, multiplier: f64) -> Result<Option<u64>, String> {
    let terminate_after = table.get("terminate-after").cloned();
    let period = table
        .get_mut("period")
        .ok_or_else(|| "slow-timeout table is missing period".to_string())?;
    let period = scale_period_value(period, multiplier)?;
    termination_bound(period, terminate_after.as_ref())
}

fn scale_table_timeout(table: &mut Table, multiplier: f64) -> Result<Option<u64>, String> {
    let terminate_after = table
        .get("terminate-after")
        .and_then(Item::as_value)
        .cloned();
    let period = table
        .get_mut("period")
        .ok_or_else(|| "slow-timeout table is missing period".to_string())?
        .as_value_mut()
        .ok_or_else(|| "slow-timeout.period must be a scalar value".to_string())?;
    let period = scale_period_value(period, multiplier)?;
    termination_bound(period, terminate_after.as_ref())
}

// Nextest inherits the complete slow-timeout value. A present table without
// terminate-after warns indefinitely; it does not inherit that field alone.
fn termination_bound(period: u64, terminate_after: Option<&Value>) -> Result<Option<u64>, String> {
    let Some(value) = terminate_after else {
        return Ok(None);
    };
    let count = value
        .as_integer()
        .filter(|value| *value > 0)
        .ok_or("slow-timeout.terminate-after must be a positive integer")? as u64;
    period
        .checked_mul(count)
        .map(Some)
        .ok_or_else(|| "slow-timeout termination bound overflows seconds".into())
}

fn scale_timeout_item(item: &mut Item, multiplier: f64) -> Result<Option<u64>, String> {
    match item {
        Item::Value(Value::InlineTable(table)) => scale_inline_timeout(table, multiplier),
        Item::Table(table) => scale_table_timeout(table, multiplier),
        _ => Err("slow-timeout must be a TOML table".into()),
    }
}

fn visit_table(
    table: &mut Table,
    multiplier: f64,
    wall_deadlines: &mut Vec<Option<u64>>,
) -> Result<(), String> {
    for (key, item) in table.iter_mut() {
        if key == "slow-timeout" {
            wall_deadlines.push(scale_timeout_item(item, multiplier)?);
        } else {
            visit_item(item, multiplier, wall_deadlines)?;
        }
    }
    Ok(())
}

fn visit_array_of_tables(
    tables: &mut ArrayOfTables,
    multiplier: f64,
    wall_deadlines: &mut Vec<Option<u64>>,
) -> Result<(), String> {
    for table in tables.iter_mut() {
        visit_table(table, multiplier, wall_deadlines)?;
    }
    Ok(())
}

fn visit_item(
    item: &mut Item,
    multiplier: f64,
    wall_deadlines: &mut Vec<Option<u64>>,
) -> Result<(), String> {
    match item {
        Item::Table(table) => visit_table(table, multiplier, wall_deadlines),
        Item::ArrayOfTables(tables) => visit_array_of_tables(tables, multiplier, wall_deadlines),
        Item::None | Item::Value(_) => Ok(()),
    }
}

fn run_wrapper_count(document: &DocumentMut, profile: &str) -> Result<usize, String> {
    let Some(profile_table) = document
        .get("profile")
        .and_then(Item::as_table)
        .and_then(|profiles| profiles.get(profile))
        .and_then(Item::as_table)
    else {
        return Ok(0);
    };
    let Some(scripts) = profile_table.get("scripts") else {
        return Ok(0);
    };
    let scripts = scripts
        .as_array_of_tables()
        .ok_or_else(|| format!("profile.{profile}.scripts must be an array of tables"))?;
    Ok(scripts
        .iter()
        .filter(|script| script.contains_key("run-wrapper"))
        .count())
}

fn add_cpu_wrapper(
    document: &mut DocumentMut,
    wrapper_bin: &Path,
    cpu_budget_usec: u64,
    measured_budgets: &[(MeasuredCpuAllowance, u64)],
) -> Result<(), String> {
    if !wrapper_bin.is_absolute() {
        return Err("nextest CPU wrapper executable must be absolute".into());
    }
    for profile in ["default", "ci"] {
        let count = run_wrapper_count(document, profile)?;
        if count != 0 {
            return Err(format!(
                "profile.{profile} already defines {count} run-wrapper rule(s); refusing to make per-test CPU measurement partial or ambiguous"
            ));
        }
    }

    let experimental = document
        .entry("experimental")
        .or_insert_with(|| Item::Value(Value::Array(Array::new())))
        .as_array_mut()
        .ok_or_else(|| "experimental must be an array".to_string())?;
    let wrapper_feature_count = experimental
        .iter()
        .filter(|value| value.as_str() == Some("wrapper-scripts"))
        .count();
    if wrapper_feature_count > 1 {
        return Err("experimental contains wrapper-scripts more than once".into());
    }
    if wrapper_feature_count == 0 {
        experimental.push("wrapper-scripts");
    }

    for name in std::iter::once(CPU_WRAPPER_NAME).chain(
        MEASURED_CPU_ALLOWANCES
            .iter()
            .map(|allowance| allowance.wrapper_name),
    ) {
        if document
            .get("scripts")
            .and_then(Item::as_table)
            .and_then(|scripts| scripts.get("wrapper"))
            .and_then(Item::as_table)
            .is_some_and(|wrappers| wrappers.contains_key(name))
        {
            return Err(format!("scripts.wrapper.{name} is already defined"));
        }
    }
    let wrapper_path = wrapper_bin
        .to_str()
        .ok_or_else(|| "nextest CPU wrapper executable path is not UTF-8".to_string())?;
    // Only a prepared, selected exact identity enables these predicates: Nextest
    // rejects unknown binary/package predicates even inside their complement.
    let mut rules = measured_budgets
        .iter()
        .map(|(allowance, budget)| {
            (
                allowance.wrapper_name,
                exact_filter(&allowance.identity()),
                *budget,
            )
        })
        .collect::<Vec<_>>();
    let default_filter = if rules.is_empty() {
        "all()".into()
    } else {
        format!(
            "!({})",
            rules
                .iter()
                .map(|(_, filter, _)| filter.as_str())
                .collect::<Vec<_>>()
                .join(" | ")
        )
    };
    rules.insert(0, (CPU_WRAPPER_NAME, default_filter, cpu_budget_usec));
    for (name, filter, budget) in &rules {
        let mut command = InlineTable::new();
        // An argv array preserves spaces and shell punctuation in a target path.
        let mut argv = Array::new();
        argv.push(wrapper_path);
        argv.push("--cpu-timeout-usec");
        argv.push(budget.to_string());
        argv.push("--termination-grace-ms");
        argv.push((CPU_TERMINATION_GRACE_SECONDS * 1000).to_string());
        argv.push("--");
        command.insert("command-line", Value::Array(argv));
        command.insert("relative-to", Value::from("none"));
        let scripts = document
            .entry("scripts")
            .or_insert_with(|| Item::Table(Table::new()))
            .as_table_mut()
            .ok_or_else(|| "scripts must be a table".to_string())?;
        let wrappers = scripts
            .entry("wrapper")
            .or_insert_with(|| Item::Table(Table::new()))
            .as_table_mut()
            .ok_or_else(|| "scripts.wrapper must be a table".to_string())?;
        let mut wrapper = Table::new();
        wrapper["command"] = value(command);
        wrapper["target-runner"] = value("within-wrapper");
        wrappers.insert(name, Item::Table(wrapper));

        let scripts = &mut document["profile"]["default"]["scripts"];
        if scripts.is_none() {
            *scripts = Item::ArrayOfTables(ArrayOfTables::new());
        }
        let scripts = scripts
            .as_array_of_tables_mut()
            .ok_or_else(|| "profile.default.scripts must be an array of tables".to_string())?;
        let mut binding = Table::new();
        binding["filter"] = value(filter.as_str());
        binding["run-wrapper"] = value(*name);
        scripts.push(binding);
    }

    if run_wrapper_count(document, "default")? != rules.len()
        || run_wrapper_count(document, "ci")? != 0
    {
        return Err(format!(
            "generated nextest config does not define exactly {} inherited run wrappers for default and ci",
            rules.len()
        ));
    }
    let bindings = document["profile"]["default"]["scripts"]
        .as_array_of_tables()
        .ok_or("generated default scripts are missing")?;
    let actual = bindings
        .iter()
        .filter(|binding| binding.contains_key("run-wrapper"))
        .map(|binding| (binding["run-wrapper"].as_str(), binding["filter"].as_str()))
        .collect::<Vec<_>>();
    let expected = rules
        .iter()
        .map(|(name, filter, _)| (Some(*name), Some(filter.as_str())))
        .collect::<Vec<_>>();
    if actual != expected {
        return Err(
            "generated nextest CPU wrapper filters differ from their exact partition".into(),
        );
    }
    Ok(())
}

fn scaled_config(
    source: &str,
    multiplier: f64,
    cpu_wrapper: Option<(&Path, f64)>,
) -> Result<String, String> {
    scaled_config_with_allowances(source, multiplier, cpu_wrapper, &[])
}

fn scaled_config_with_allowances(
    source: &str,
    multiplier: f64,
    cpu_wrapper: Option<(&Path, f64)>,
    selected_allowances: &[MeasuredCpuAllowance],
) -> Result<String, String> {
    timeouts::validate_timeout_multiplier(multiplier, "wall multiplier")?;
    let mut document = source
        .parse::<DocumentMut>()
        .map_err(|error| format!("cannot parse nextest TOML: {error}"))?;
    let mut wall_deadlines = Vec::new();
    visit_table(document.as_table_mut(), multiplier, &mut wall_deadlines)?;
    if wall_deadlines.is_empty() {
        return Err("nextest config contains no slow-timeout.period values".into());
    }
    if let Some((wrapper_bin, cpu_multiplier)) = cpu_wrapper {
        let cpu_seconds = timeouts::scale_timeout_seconds(
            timeouts::DEFAULT_TEST_CPU_TIMEOUT_SECONDS,
            cpu_multiplier,
            "CPU multiplier",
        )?;
        let cpu_budget_usec = cpu_seconds
            .checked_mul(1_000_000)
            .ok_or("CPU timeout overflows microseconds")?;
        let mut maximum_cpu_seconds = cpu_seconds;
        let measured_budgets = selected_allowances
            .iter()
            .map(|allowance| {
                let seconds = timeouts::scale_timeout_seconds(
                    allowance.cpu_seconds,
                    cpu_multiplier,
                    "CPU multiplier",
                )?;
                maximum_cpu_seconds = maximum_cpu_seconds.max(seconds);
                let budget = seconds
                    .checked_mul(1_000_000)
                    .ok_or("CPU timeout overflows microseconds")?;
                Ok((*allowance, budget))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let cpu_and_cleanup = maximum_cpu_seconds
            .checked_add(CPU_TERMINATION_GRACE_SECONDS)
            .ok_or("CPU timeout plus cleanup grace overflows seconds")?;
        // Check complete profile/override values, including inherited defaults,
        // without lengthening a wall timeout or treating a warning as a stop.
        if wall_deadlines
            .iter()
            .flatten()
            .any(|wall| *wall <= cpu_and_cleanup)
        {
            return Err(format!(
                "nextest CPU timeout ({maximum_cpu_seconds}s) plus cleanup grace \
                 ({CPU_TERMINATION_GRACE_SECONDS}s) must be below every finite scaled wall termination bound"
            ));
        }
        add_cpu_wrapper(
            &mut document,
            wrapper_bin,
            cpu_budget_usec,
            &measured_budgets,
        )?;
    }
    Ok(document.to_string())
}

fn cpu_wrapper_from_env() -> Option<std::path::PathBuf> {
    env::var_os(CPU_WRAPPER_BIN_ENV).map(Into::into)
}

fn exact_filter(identity: &nextest_cpu::AttemptIdentity) -> String {
    // Nextest 0.9.100 equality matcher syntax, not a regex or substring match.
    // Unicode escapes also preserve leading/trailing whitespace and delimiters.
    fn escape(value: &str) -> String {
        value
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || "_:-.".contains(character) {
                    character.to_string()
                } else {
                    format!("\\u{{{:x}}}", u32::from(character))
                }
            })
            .collect()
    }
    format!(
        "package(={}) & binary_id(={}) & test(={})",
        escape(&identity.package),
        escape(&identity.binary),
        escape(&identity.test)
    )
}

fn prepared_selected_cpu_allowances(
    prepared: bool,
    inventory: impl FnOnce() -> Result<Vec<u8>, String>,
) -> Result<Vec<MeasuredCpuAllowance>, String> {
    if !prepared {
        // Legacy/null-context callers did not consume this input. In particular,
        // do not make their previously unused malformed/missing inventory fatal.
        return Ok(Vec::new());
    }
    // selected_inventory also performs the existing binary-map validation.
    let selected = nextest_cpu::selected_inventory(&inventory()?)?;
    Ok(MEASURED_CPU_ALLOWANCES
        .iter()
        .copied()
        .filter(|allowance| selected.contains(&allowance.identity()))
        .collect())
}

/// The only overrides a regular configuration may carry, as (filter,
/// test-group) pairs. Each group only lowers how many of its tests run at once.
const REGULAR_GROUP_OVERRIDES: [(&str, &str); 1] = [(
    "package(=hermit) & binary(=cli) & (test(=record_classifies_a_gdbserver_replay_stage_container_child_failure) | test(=every_record_container_site_classifies_a_child_fault_by_name))",
    "cli-gdbserver-port",
)];

fn regular_config_compatible(document: &DocumentMut) -> bool {
    let Some(default_timeout) = document
        .get("profile")
        .and_then(Item::as_table)
        .and_then(|profiles| profiles.get("default"))
        .and_then(Item::as_table)
        .and_then(|profile| profile.get("slow-timeout"))
        .and_then(Item::as_inline_table)
    else {
        return false;
    };
    if default_timeout.get("period").and_then(Value::as_str) != Some("57s")
        || default_timeout
            .get("terminate-after")
            .and_then(Value::as_integer)
            != Some(1)
        || default_timeout.get("grace-period").and_then(Value::as_str) != Some("2s")
    {
        return false;
    }
    for profile in ["default", "ci"] {
        let Some(table) = document
            .get("profile")
            .and_then(Item::as_table)
            .and_then(|table| table.get(profile))
            .and_then(Item::as_table)
        else {
            continue;
        };
        if table.contains_key("retries")
            || table.contains_key("threads-required")
            || profile == "ci" && table.contains_key("slow-timeout")
        {
            return false;
        }
        if let Some(overrides) = table.get("overrides") {
            let Some(overrides) = overrides.as_array_of_tables() else {
                return false;
            };
            for entry in overrides {
                let pair = (
                    entry.get("filter").and_then(Item::as_str),
                    entry.get("test-group").and_then(Item::as_str),
                );
                if profile != "default"
                    || entry.len() != 2
                    || !REGULAR_GROUP_OVERRIDES
                        .iter()
                        .any(|&(filter, group)| pair == (Some(filter), Some(group)))
                {
                    return false;
                }
            }
        }
    }
    true
}

/// Only certify an emitted finite width and the supported regular profiles.
/// Unknown invocation semantics retain the original configuration unchanged.
fn runtime_settings(args: &[String]) -> Option<(u64, u64)> {
    if env::var("NEXTEST_PROFILE")
        .is_ok_and(|profile| !matches!(profile.as_str(), "default" | "ci"))
    {
        return None;
    }
    let mut threads = env::var("NEXTEST_TEST_THREADS").ok();
    let mut retries = env::var("NEXTEST_RETRIES")
        .ok()
        .unwrap_or_else(|| "0".into());
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "-j" | "--jobs" | "--test-threads" => {
                index += 1;
                threads = Some(args.get(index)?.clone());
            }
            "--retries" => {
                index += 1;
                retries = args.get(index)?.clone();
            }
            "--profile" => {
                index += 1;
                if !matches!(args.get(index)?.as_str(), "default" | "ci") {
                    return None;
                }
            }
            "--exclude" => {
                index += 1;
                args.get(index)?;
            }
            "--workspace" | "--no-fail-fast" => {}
            _ => {
                if let Some(value) = arg
                    .strip_prefix("--test-threads=")
                    .or_else(|| arg.strip_prefix("--jobs="))
                    .or_else(|| arg.strip_prefix("-j").filter(|value| !value.is_empty()))
                {
                    threads = Some(value.into());
                } else if let Some(value) = arg.strip_prefix("--retries=") {
                    retries = value.into();
                } else if !matches!(arg.as_str(), "--profile=default" | "--profile=ci") {
                    return None;
                }
            }
        }
        index += 1;
    }
    let threads = threads?.parse::<u64>().ok().filter(|value| *value > 0)?;
    Some((threads, retries.parse().ok()?))
}

fn resolve_budgets(
    inventory: &[u8],
    context: BudgetContext,
    calibration: Option<&[u8]>,
    threads: u64,
    retries: u64,
    cpu_multiplier: f64,
    wall_multiplier: f64,
) -> Result<ResolvedBudgets, String> {
    let identities = nextest_cpu::selected_inventory(inventory)?;
    context.validate_execution_cohort()?;
    // This calibration never covers the hermit package: its integration tests
    // run in the serialized group or, for the cli binary, at the DAG's width.
    if identities
        .iter()
        .any(|identity| identity.package == "hermit")
    {
        return Err("regular calibration cannot cover the hermit package".into());
    }
    let calibration: Option<BudgetCalibration> = calibration
        .map(serde_json::from_slice)
        .transpose()
        .map_err(|error| format!("invalid regular calibration: {error}"))?;
    let mut rows = std::collections::BTreeMap::new();
    let mut applicability = "absent: unchanged defaults".to_string();
    if let Some(table) = &calibration {
        table.context.validate_execution_cohort()?;
        if table.schema != nextest_cpu::BUDGET_SCHEMA
            || table.test_threads == 0
            || table.evidence_sha256.is_empty()
            || table
                .evidence_sha256
                .iter()
                .any(|digest| digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err("invalid calibration schema, cohort or evidence digest".into());
        }
        for row in &table.rows {
            row.identity.validate()?;
            if row.identity.attempt != 1
                || row.samples < 3
                || rows.insert(&row.identity, row).is_some()
            {
                return Err(
                    "duplicate/attempt-specific calibration or fewer than three observations"
                        .into(),
                );
            }
        }
        applicability =
            if context.matches_calibration(&table.context) && table.test_threads == threads {
                "applicable".into()
            } else {
                rows.clear();
                "stale source/build/resource cohort: unchanged defaults".into()
            };
    }
    let mut entries = Vec::with_capacity(identities.len());
    for identity in identities {
        let mut cpu = timeouts::DEFAULT_TEST_CPU_TIMEOUT_SECONDS;
        let mut wall = timeouts::DEFAULT_TEST_WALL_TIMEOUT_SECONDS;
        let mut reason = "unresolved: unchanged defaults";
        if let Some(row) = rows.get(&identity) {
            let candidate_cpu =
                timeouts::cpu_bound_from_p90_usec(row.p90_cpu_usec.max(row.historical_cpu_usec))
                    .max(1);
            let candidate_wall = timeouts::wall_bound_from_p90_millis(
                row.p90_wall_millis.max(row.historical_wall_millis),
            )
            .max(
                candidate_cpu
                    .checked_add(3)
                    .ok_or("calibrated CPU bound overflows")?,
            );
            if candidate_cpu <= cpu && candidate_wall <= wall {
                cpu = candidate_cpu;
                wall = candidate_wall;
                reason = "calibrated: owner formula with historical floor";
            } else {
                reason = "unresolved: owner formula exceeds unchanged defaults";
            }
        }
        entries.push(ResolvedBudget {
            identity,
            cpu_seconds: timeouts::scale_timeout_seconds(cpu, cpu_multiplier, "CPU multiplier")?,
            wall_seconds: timeouts::scale_timeout_seconds(
                wall,
                wall_multiplier,
                "wall multiplier",
            )?,
            reason: reason.into(),
        });
    }
    let result = ResolvedBudgets {
        schema: nextest_cpu::BUDGET_SCHEMA,
        context,
        calibration_sha256: None,
        applicability,
        population: Some(nextest_cpu::population_budget(&entries, threads, retries)?),
        entries,
    };
    // Independent multipliers can destroy base CPU+grace separation. Refuse;
    // never repair that collision by silently extending the resolved wall cap.
    result.validate()?;
    Ok(result)
}

fn resolve_config(args: Vec<std::ffi::OsString>) -> Result<(), String> {
    if args == [std::ffi::OsString::from("--help")] || args == [std::ffi::OsString::from("-h")] {
        println!(
            "usage: nextest-timeout-config.rs --resolve SOURCE MULTIPLIER OUTPUT INVENTORY CONTEXT MAP CALIBRATION -- NEXTEST_ARGS...\n\nGenerate Nextest config OUTPUT from canonical TOML SOURCE and the wall MULTIPLIER. INVENTORY is typed Nextest list JSON; CONTEXT is verified budget-context JSON (or null); MAP is a new absolute output path; CALIBRATION is the optional reviewed JSON table. NEXTEST_ARGS are the actual counted-run arguments, including concurrency and retries. HERMIT_NEXTEST_CPU_WRAPPER_BIN must name the prepared wrapper; HERMIT_TEST_CPU_TIMEOUT_MULTIPLIER defaults to 1.\n\nAbsent/stale regular calibration retains the regular default bounds. Prepared post-inventory selection admits only the exact Detcore allocator ({DETCORE_ALLOCATOR_CPU_SECONDS}s), Detcore constructor ({DETCORE_CONSTRUCTOR_CPU_SECONDS}s), and standalone allocator ({STANDALONE_ALLOCATOR_CPU_SECONDS}s) base CPU allowances, each scaled once; all other tests keep the 22-second base. Unknown context/profile/concurrency produces no regular map or aggregate certification. Malformed input refuses. No test or preparation command is run."
        );
        return Ok(());
    }
    if args.len() < 8 || args[7] != "--" {
        return Err("usage: --resolve SOURCE MULTIPLIER OUTPUT INVENTORY CONTEXT MAP CALIBRATION -- NEXTEST_ARGS...".into());
    }
    let source = fs::read_to_string(&args[0]).map_err(|e| e.to_string())?;
    let wall_multiplier = args[1]
        .to_str()
        .ok_or("wall multiplier is not UTF-8")?
        .parse::<f64>()
        .map_err(|e| e.to_string())?;
    let cpu_multiplier =
        timeouts::timeout_multiplier_from_env(timeouts::TEST_CPU_TIMEOUT_MULTIPLIER_ENV)?;
    let wrapper = cpu_wrapper_from_env().ok_or("budget resolution requires the CPU wrapper")?;
    let selected_allowances = prepared_selected_cpu_allowances(
        env::var("HERMIT_PREPARED_NEXTEST_REQUIRED").as_deref() == Ok("1"),
        || fs::read(&args[3]).map_err(|error| error.to_string()),
    )?;
    let rendered = scaled_config_with_allowances(
        &source,
        wall_multiplier,
        Some((&wrapper, cpu_multiplier)),
        &selected_allowances,
    )?;
    let mut document = rendered.parse::<DocumentMut>().map_err(|e| e.to_string())?;
    let context: Option<BudgetContext> =
        serde_json::from_slice(&fs::read(&args[4]).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let runtime = args[8..]
        .iter()
        .map(|arg| {
            arg.clone()
                .into_string()
                .map_err(|_| "non-UTF-8 Nextest argument".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let settings = runtime_settings(&runtime);
    if context.is_none()
        || settings.is_none()
        || !regular_config_compatible(&source.parse::<DocumentMut>().map_err(|e| e.to_string())?)
    {
        if !selected_allowances.is_empty() {
            for allowance in &selected_allowances {
                eprintln!(
                    "nextest budgets: prepared exact {} test {} CPU base {}s \
                     (CPU multiplier {cpu_multiplier}); ordinary CPU base {}s; existing wall bounds unchanged \
                     except wall multiplier {wall_multiplier}; regular population/concurrency not certified",
                    allowance.binary,
                    allowance.test,
                    allowance.cpu_seconds,
                    timeouts::DEFAULT_TEST_CPU_TIMEOUT_SECONDS,
                );
            }
        } else {
            eprintln!(
                "nextest budgets: legacy bounds; regular population/concurrency not certified"
            );
        }
        return fs::write(&args[2], rendered).map_err(|e| e.to_string());
    }
    let (threads, retries) = settings.unwrap();
    let calibration = match fs::read(&args[6]) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.to_string()),
    };
    let mut resolved = resolve_budgets(
        &fs::read(&args[3]).map_err(|e| e.to_string())?,
        context.unwrap(),
        calibration.as_deref(),
        threads,
        retries,
        cpu_multiplier,
        wall_multiplier,
    )?;
    resolved.calibration_sha256 = calibration
        .as_ref()
        .map(|bytes| format!("{:x}", Sha256::digest(bytes)));
    let bytes = serde_json::to_vec_pretty(&resolved).map_err(|e| e.to_string())?;
    let digest = format!("{:x}", Sha256::digest(&bytes));
    let path = Path::new(&args[5]);
    if !path.is_absolute() {
        return Err("resolved budget path must be absolute".into());
    }
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    std::io::Write::write_all(&mut output, &bytes).map_err(|e| e.to_string())?;
    output.sync_all().map_err(|e| e.to_string())?;
    let argv = document["scripts"]["wrapper"][CPU_WRAPPER_NAME]["command"]
        .as_inline_table_mut()
        .and_then(|command| command.get_mut("command-line"))
        .and_then(Value::as_array_mut)
        .ok_or("generated wrapper command is missing")?;
    let separator = argv.len() - 1;
    for (offset, value) in [
        "--budget-map",
        path.to_str().ok_or("non-UTF-8 budget path")?,
        "--budget-sha256",
        &digest,
    ]
    .iter()
    .enumerate()
    {
        argv.insert(separator + offset, *value);
    }
    let overrides = &mut document["profile"]["default"]["overrides"];
    if overrides.is_none() {
        *overrides = Item::ArrayOfTables(ArrayOfTables::new());
    }
    let overrides = overrides
        .as_array_of_tables_mut()
        .ok_or("invalid default overrides")?;
    for entry in &resolved.entries {
        let mut override_entry = Table::new();
        override_entry["filter"] = value(exact_filter(&entry.identity));
        let mut timeout = InlineTable::new();
        timeout.insert("period", Value::from(format!("{}s", entry.wall_seconds)));
        timeout.insert("terminate-after", Value::from(1_i64));
        timeout.insert("grace-period", Value::from("2s"));
        override_entry["slow-timeout"] = value(timeout);
        overrides.push(override_entry);
    }
    if let Some(population) = &resolved.population {
        eprintln!(
            "nextest budgets: {}; {} selected, {} attempts, J={}, nominal {} CPU-s/{} wall-s; setup/reporting/cleanup failure separate; regular 7200/900 headroom {}",
            resolved.applicability,
            population.selected,
            population.attempts,
            population.test_threads,
            population.cpu_seconds,
            population.nominal_wall_seconds,
            if population.cpu_seconds < 7200 && population.nominal_wall_seconds < 900 {
                "available, overhead not certified"
            } else {
                "NOT certified"
            }
        );
    }
    fs::write(&args[2], document.to_string()).map_err(|e| e.to_string())
}

fn run() -> Result<(), String> {
    let mut args = env::args_os();
    let _program = args.next();
    let source = args.next().ok_or_else(|| usage().to_string())?;
    if source == "--resolve" {
        return resolve_config(args.collect());
    }
    let multiplier = args
        .next()
        .ok_or_else(|| usage().to_string())?
        .into_string()
        .map_err(|_| "MULTIPLIER must be UTF-8".to_string())?
        .parse::<f64>()
        .map_err(|error| format!("invalid wall multiplier: {error}"))?;
    let output = args.next().ok_or_else(|| usage().to_string())?;
    if args.next().is_some() {
        return Err(usage().into());
    }
    let source_text = fs::read_to_string(Path::new(&source))
        .map_err(|error| format!("cannot read {}: {error}", Path::new(&source).display()))?;
    let cpu_wrapper = cpu_wrapper_from_env();
    let cpu_multiplier = if cpu_wrapper.is_some() {
        timeouts::timeout_multiplier_from_env(timeouts::TEST_CPU_TIMEOUT_MULTIPLIER_ENV)?
    } else {
        1.0
    };
    let rendered = scaled_config(
        &source_text,
        multiplier,
        cpu_wrapper.as_deref().map(|path| (path, cpu_multiplier)),
    )?;
    fs::write(Path::new(&output), rendered)
        .map_err(|error| format!("cannot write {}: {error}", Path::new(&output).display()))
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("nextest-timeout-config: {error}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detcore_allocator_identity() -> nextest_cpu::AttemptIdentity {
        MEASURED_CPU_ALLOWANCES[0].identity()
    }

    fn scaled_config_with_allocator(
        source: &str,
        multiplier: f64,
        cpu_wrapper: Option<(&Path, f64)>,
        selected: bool,
    ) -> Result<String, String> {
        scaled_config_with_allowances(
            source,
            multiplier,
            cpu_wrapper,
            if selected {
                &MEASURED_CPU_ALLOWANCES[..1]
            } else {
                &[]
            },
        )
    }

    #[test]
    fn calibration_requires_the_same_actual_execution_cohort_not_the_same_launch() {
        let (inventory, context, mut table) = budget_fixture();
        let resolve = |current: BudgetContext, table: &BudgetCalibration| {
            resolve_budgets(
                &inventory,
                current,
                Some(&serde_json::to_vec(table).unwrap()),
                8,
                0,
                1.0,
                1.0,
            )
            .unwrap()
        };
        table.context.launch_proof = None;
        assert_eq!(resolve(context.clone(), &table).applicability, "applicable");
        let mut other_launch = context.clone();
        other_launch.launch_proof.as_mut().unwrap().path = "/other-launch/proof.json".into();
        other_launch.launch_proof.as_mut().unwrap().sha256 = "e".repeat(64);
        assert_eq!(resolve(other_launch, &table).applicability, "applicable");

        let mut opponents = Vec::new();
        let mut changed = context.clone();
        changed.execution_cohort.as_mut().unwrap().domain =
            nextest_cpu::ExecutionDomain::PinnedRoot {
                image: format!("localhost/image@sha256:{}", "a".repeat(64)),
                image_id: "b".repeat(64),
            };
        opponents.push(changed.clone());
        let mut pinned_table = table.clone();
        pinned_table.context = changed.clone();
        if let nextest_cpu::ExecutionDomain::PinnedRoot { image, .. } =
            &mut changed.execution_cohort.as_mut().unwrap().domain
        {
            *image = format!("localhost/image@sha256:{}", "c".repeat(64));
        }
        assert_ne!(resolve(changed, &pinned_table).applicability, "applicable");
        assert_ne!(
            resolve(context.clone(), &pinned_table).applicability,
            "applicable"
        );
        let mut changed_config = pinned_table.context.clone();
        if let nextest_cpu::ExecutionDomain::PinnedRoot { image_id, .. } =
            &mut changed_config.execution_cohort.as_mut().unwrap().domain
        {
            *image_id = "d".repeat(64);
        }
        assert_ne!(
            resolve(changed_config, &pinned_table).applicability,
            "applicable"
        );
        let mut same_ratio = context.clone();
        let quota = &mut same_ratio
            .execution_cohort
            .as_mut()
            .unwrap()
            .resources
            .cpu_limits[0];
        quota.quota_usec *= 2;
        quota.period_usec *= 2;
        opponents.push(same_ratio);
        for field in 0..6 {
            let mut changed = context.clone();
            let resources = &mut changed.execution_cohort.as_mut().unwrap().resources;
            match field {
                0 => resources.cpu_limits[0].quota_usec -= 1,
                1 => resources.cpu_limits[0].period_usec += 1,
                2 => {
                    resources.affinity.pop();
                }
                3 => {
                    resources.cpuset.pop();
                }
                4 => resources.memory_bytes = nextest_cpu::ResourceLimit::Max(4 << 30),
                _ => resources.swap_bytes = nextest_cpu::ResourceLimit::Unlimited,
            }
            opponents.push(changed);
        }
        for changed in opponents {
            let result = resolve(changed, &table);
            assert_ne!(result.applicability, "applicable");
            assert!(
                result
                    .entries
                    .iter()
                    .all(|row| row.cpu_seconds == 22 && row.wall_seconds == 57)
            );
        }
        let mut legacy = context.clone();
        legacy.execution_cohort = None;
        legacy.launch_proof = None;
        let mut legacy_table = table.clone();
        legacy_table.context = legacy.clone();
        assert_ne!(resolve(legacy, &legacy_table).applicability, "applicable");
        let mut malformed = context;
        malformed.execution_cohort.as_mut().unwrap().schema = 99;
        assert!(
            resolve_budgets(
                &inventory,
                malformed,
                Some(&serde_json::to_vec(&table).unwrap()),
                8,
                0,
                1.0,
                1.0
            )
            .is_err()
        );
    }

    fn budget_fixture() -> (Vec<u8>, BudgetContext, BudgetCalibration) {
        let inventory = serde_json::json!({"rust-suites": {
            "fixture": {"package-name":"fixture", "binary-id":"fixture", "binary-name":"fixture", "kind":"lib", "binary-path":"/fixture/lib", "testcases": {
                "same name,(special)": {"ignored":false,"filter-match":{"status":"matches"}},
                "unresolved": {"ignored":false,"filter-match":{"status":"matches"}},
                "ignored": {"ignored":true,"filter-match":{"status":"matches"}},
                "filtered": {"ignored":false,"filter-match":{"status":"mismatch"}}
            }},
            "fixture::bin/other": {"package-name":"fixture", "binary-id":"fixture::bin/other", "binary-name":"other", "kind":"bin", "binary-path":"/fixture/bin", "testcases": {
                "same name,(special)": {"ignored":false,"filter-match":{"status":"matches"}}
            }}
        }});
        let context = BudgetContext {
            source_sha256: "a".repeat(64),
            source_clean: true,
            build_sha256: "b".repeat(64),
            machine: "fixture CPU".into(),
            available_cpus: 8,
            execution_cohort: Some(nextest_cpu::ExecutionCohort {
                schema: 1,
                domain: nextest_cpu::ExecutionDomain::Host,
                resources: nextest_cpu::ResourceEnvelope {
                    cpu_limits: vec![nextest_cpu::CpuQuota {
                        quota_usec: 800_000,
                        period_usec: 100_000,
                    }],
                    affinity: (0..8).collect(),
                    cpuset: (0..8).collect(),
                    memory_bytes: nextest_cpu::ResourceLimit::Max(8 << 30),
                    swap_bytes: nextest_cpu::ResourceLimit::Max(0),
                },
            }),
            launch_proof: Some(nextest_cpu::LaunchProofRef {
                path: "/fixture/launch.json".into(),
                bytes: 100,
                sha256: "d".repeat(64),
            }),
        };
        let calibration = BudgetCalibration {
            schema: 1,
            context: context.clone(),
            test_threads: 8,
            evidence_sha256: vec!["c".repeat(64)],
            rows: vec![nextest_cpu::CalibrationRow {
                identity: nextest_cpu::AttemptIdentity {
                    package: "fixture".into(),
                    binary: "fixture".into(),
                    test: "same name,(special)".into(),
                    attempt: 1,
                },
                samples: 3,
                p90_cpu_usec: 500_000,
                p90_wall_millis: 200,
                historical_cpu_usec: 600_000,
                historical_wall_millis: 300,
            }],
        };
        (
            serde_json::to_vec(&inventory).unwrap(),
            context,
            calibration,
        )
    }

    #[test]
    fn exact_identity_calibration_retains_unknowns_and_historical_floor() {
        let (inventory, context, mut calibration) = budget_fixture();
        let resolved = resolve_budgets(
            &inventory,
            context.clone(),
            Some(&serde_json::to_vec(&calibration).unwrap()),
            8,
            0,
            1.0,
            1.0,
        )
        .unwrap();
        assert_eq!(resolved.entries.len(), 3);
        let row = resolved
            .entries
            .iter()
            .find(|row| row.identity == calibration.rows[0].identity)
            .unwrap();
        assert_eq!((row.cpu_seconds, row.wall_seconds), (1, 4));
        assert_eq!(
            resolved
                .entries
                .iter()
                .filter(|row| row.cpu_seconds == 22 && row.wall_seconds == 57)
                .count(),
            2
        );
        calibration.rows[0].historical_wall_millis = 19_084;
        let unresolved = resolve_budgets(
            &inventory,
            context,
            Some(&serde_json::to_vec(&calibration).unwrap()),
            8,
            0,
            1.0,
            1.0,
        )
        .unwrap();
        assert!(
            unresolved
                .entries
                .iter()
                .all(|row| row.cpu_seconds == 22 && row.wall_seconds == 57)
        );
        assert!(
            unresolved
                .entries
                .iter()
                .any(|row| row.reason.contains("formula exceeds"))
        );
    }

    #[test]
    fn stale_or_absent_calibration_bootstraps_all_selected_defaults() {
        let (inventory, context, calibration) = budget_fixture();
        let bytes = serde_json::to_vec(&calibration).unwrap();
        let mut stale = context.clone();
        stale.source_sha256 = "d".repeat(64);
        let mut dirty = context.clone();
        dirty.source_clean = false;
        let mut other_build = context.clone();
        other_build.build_sha256 = "e".repeat(64);
        let mut other_cpu = context.clone();
        other_cpu.available_cpus = 2;
        for changed in [stale, dirty, other_build, other_cpu] {
            let result =
                resolve_budgets(&inventory, changed, Some(&bytes), 8, 0, 1.0, 1.0).unwrap();
            assert_eq!(result.entries.len(), 3);
            assert!(
                result
                    .entries
                    .iter()
                    .all(|row| row.cpu_seconds == 22 && row.wall_seconds == 57)
            );
            assert!(result.applicability.starts_with("stale"));
        }
        let absent = resolve_budgets(&inventory, context, None, 8, 0, 1.0, 1.0).unwrap();
        assert_eq!(absent.entries.len(), 3);
        assert!(absent.applicability.starts_with("absent"));
    }

    #[test]
    fn refuses_ambiguous_inputs_and_post_scaling_collision() {
        let (inventory, context, mut calibration) = budget_fixture();
        let bytes = serde_json::to_vec(&calibration).unwrap();
        assert!(
            resolve_budgets(&inventory, context.clone(), Some(&bytes), 8, 0, 2.0, 1.0)
                .unwrap_err()
                .contains("cleanup")
        );
        let scaled =
            resolve_budgets(&inventory, context.clone(), Some(&bytes), 8, 0, 2.0, 2.0).unwrap();
        assert!(
            scaled
                .entries
                .iter()
                .any(|row| row.cpu_seconds == 2 && row.wall_seconds == 8)
        );
        calibration.rows.push(calibration.rows[0].clone());
        assert!(
            resolve_budgets(
                &inventory,
                context.clone(),
                Some(&serde_json::to_vec(&calibration).unwrap()),
                8,
                0,
                1.0,
                1.0
            )
            .is_err()
        );
        calibration.rows.pop();
        calibration.rows[0].samples = 1;
        assert!(
            resolve_budgets(
                &inventory,
                context.clone(),
                Some(&serde_json::to_vec(&calibration).unwrap()),
                8,
                0,
                1.0,
                1.0
            )
            .is_err()
        );
        assert!(resolve_budgets(&inventory, context, Some(b"{}"), 8, 0, 1.0, 1.0).is_err());
    }

    #[test]
    fn population_accounts_for_all_cases_width_retries_and_overflow() {
        let (inventory, context, _) = budget_fixture();
        let result = resolve_budgets(&inventory, context, None, 8, 0, 1.0, 1.0).unwrap();
        let eight = nextest_cpu::population_budget(&result.entries, 8, 0).unwrap();
        let one = nextest_cpu::population_budget(&result.entries, 1, 0).unwrap();
        assert_eq!(
            (
                one.selected,
                one.attempts,
                one.cpu_seconds,
                one.nominal_wall_seconds
            ),
            (3, 3, 66, 177)
        );
        assert!(eight.nominal_wall_seconds < one.nominal_wall_seconds);
        let retried = nextest_cpu::population_budget(&result.entries[..1], 8, 1).unwrap();
        assert_eq!(
            (
                retried.attempts,
                retried.cpu_seconds,
                retried.nominal_wall_seconds
            ),
            (2, 44, 118)
        );
        assert!(nextest_cpu::population_budget(&result.entries, 0, 0).is_err());
        assert!(nextest_cpu::population_budget(&result.entries, 8, u64::MAX).is_err());
        let population = vec![result.entries[0].clone(); 601];
        let full = nextest_cpu::population_budget(&population, 8, 0).unwrap();
        assert_eq!((full.selected, full.cpu_seconds), (601, 13_222));
        assert!(full.nominal_wall_seconds > 900);
    }

    #[test]
    fn exact_filters_escape_delimiters_without_widening_matches() {
        let (_, _, calibration) = budget_fixture();
        assert_eq!(
            exact_filter(&calibration.rows[0].identity),
            "package(=fixture) & binary_id(=fixture) & test(=same\\u{20}name\\u{2c}\\u{28}special\\u{29})"
        );
        let canonical = include_str!("../.config/nextest.toml")
            .parse::<DocumentMut>()
            .unwrap();
        assert!(regular_config_compatible(&canonical));
        let mut changed = canonical.clone();
        let mut rule = Table::new();
        rule["filter"] = value("all()");
        rule["test-group"] = value("serial");
        changed["profile"]["default"]["overrides"]
            .as_array_of_tables_mut()
            .unwrap()
            .push(rule);
        assert!(!regular_config_compatible(&changed));
        let changed_default =
            include_str!("../.config/nextest.toml").replace("period = \"57s\"", "period = \"58s\"");
        assert!(!regular_config_compatible(
            &changed_default.parse::<DocumentMut>().unwrap()
        ));
        let widened_gdb_group = include_str!("../.config/nextest.toml").replace(
            "binary(=cli) & (test(=record_classifies",
            "binary(=cli) | (test(=record_classifies",
        );
        assert_ne!(widened_gdb_group, include_str!("../.config/nextest.toml"));
        assert!(!regular_config_compatible(
            &widened_gdb_group.parse::<DocumentMut>().unwrap()
        ));
        let swapped_groups = include_str!("../.config/nextest.toml").replace(
            "test-group = \"cli-gdbserver-port\"",
            "test-group = \"serial\"",
        );
        assert_ne!(swapped_groups, include_str!("../.config/nextest.toml"));
        assert!(!regular_config_compatible(
            &swapped_groups.parse::<DocumentMut>().unwrap()
        ));
        assert_eq!(
            runtime_settings(&["-j8".into(), "--retries=2".into()]),
            Some((8, 2))
        );
        assert!(runtime_settings(&["-j0".into()]).is_none());
        assert!(runtime_settings(&["-j8".into(), "--profile=other".into()]).is_none());
    }

    #[test]
    fn scales_every_profile_and_override_with_ceil_rounding() {
        let source = concat!(
            "# retained comment\n",
            "[profile.default]\n",
            "slow-timeout = { period = \"57s\", terminate-after = 1, grace-period = \"2s\" }\n",
            "unrelated = \"kept\"\n",
            "[profile.ci]\n",
            "slow-timeout = { period = \"10s\", terminate-after = 2 }\n",
            "[[profile.ci.overrides]]\n",
            "filter = \"test(example)\"\n",
            "slow-timeout = { period = \"3s\", terminate-after = 1 }\n",
        );
        let rendered = scaled_config(source, 1.25, None).unwrap();
        assert!(rendered.contains("# retained comment"));
        assert!(rendered.contains("unrelated = \"kept\""));
        assert!(rendered.contains("filter = \"test(example)\""));
        assert!(rendered.contains("period = \"72s\""));
        assert!(rendered.contains("period = \"13s\""));
        assert!(rendered.contains("period = \"4s\""));
        assert_eq!(rendered.matches("slow-timeout").count(), 3);
    }

    #[test]
    fn refuses_malformed_or_missing_periods() {
        for period in ["0s", "1.5s", "5m", "-1s", "s"] {
            let source = format!(
                "[profile.default]\nslow-timeout = {{ period = {period:?}, terminate-after = 1 }}\n"
            );
            assert!(
                scaled_config(&source, 1.0, None).is_err(),
                "accepted {period}"
            );
        }
        assert!(
            scaled_config(
                "[profile.default]\nslow-timeout = { terminate-after = 1 }\n",
                1.0,
                None,
            )
            .unwrap_err()
            .contains("missing period")
        );
        assert!(
            scaled_config("[profile.default]\nfail-fast = false\n", 1.0, None)
                .unwrap_err()
                .contains("no slow-timeout.period")
        );
    }

    #[test]
    fn refuses_nonpositive_or_nonfinite_multiplier() {
        let source =
            "[profile.default]\nslow-timeout = { period = \"57s\", terminate-after = 1 }\n";
        for multiplier in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(scaled_config(source, multiplier, None).is_err());
        }
    }

    #[test]
    fn adds_one_default_wrapper_inherited_by_ci_without_changing_wall_scaling() {
        let source = concat!(
            "nextest-version = \"0.9.100\"\n",
            "[profile.default]\n",
            "slow-timeout = { period = \"57s\", terminate-after = 1, grace-period = \"2s\" }\n",
            "[profile.ci]\n",
            "status-level = \"slow\"\n",
        );
        let rendered = scaled_config(source, 1.5, Some((Path::new("/tmp/wrapper"), 1.0))).unwrap();
        let document = rendered.parse::<DocumentMut>().unwrap();
        assert_eq!(run_wrapper_count(&document, "default").unwrap(), 1);
        assert_eq!(run_wrapper_count(&document, "ci").unwrap(), 0);
        assert_eq!(rendered.matches("run-wrapper").count(), 1);
        assert!(rendered.contains("period = \"86s\""));
        assert!(rendered.contains("wrapper-scripts"));
        assert!(rendered.contains("target-runner = \"within-wrapper\""));
    }

    #[test]
    fn refuses_a_second_default_or_ci_run_wrapper() {
        for profile in ["default", "ci"] {
            let source = format!(
                "[profile.default]\nslow-timeout = {{ period = \"57s\" }}\n[[profile.{profile}.scripts]]\nfilter = \"all()\"\nrun-wrapper = \"other\"\n"
            );
            let error =
                scaled_config(&source, 1.0, Some((Path::new("/tmp/wrapper"), 1.0))).unwrap_err();
            assert!(error.contains(&format!("profile.{profile} already defines")));
        }
    }

    #[test]
    fn wrapper_path_is_one_literal_argument() {
        let path = "/tmp/a path/it's-$(literal)-wrapper";
        let rendered = scaled_config(
            "[profile.default]\nslow-timeout = { period = \"57s\" }\n",
            1.0,
            Some((Path::new(path), 1.0)),
        )
        .unwrap();
        let document = rendered.parse::<DocumentMut>().unwrap();
        let argv = document["scripts"]["wrapper"][CPU_WRAPPER_NAME]["command"]["command-line"]
            .as_array()
            .unwrap();
        assert_eq!(argv.len(), 6);
        assert_eq!(argv.get(0).unwrap().as_str(), Some(path));
    }

    #[test]
    fn cpu_budget_is_scaled_independently_and_rounded_up() {
        let source = "[profile.default]\nslow-timeout = { period = \"57s\" }\n";
        for (cpu_multiplier, expected) in [(1.0, "22000000"), (1.25, "28000000"), (0.01, "1000000")]
        {
            let rendered = scaled_config(
                source,
                1.5,
                Some((Path::new("/tmp/wrapper"), cpu_multiplier)),
            )
            .unwrap();
            let document = rendered.parse::<DocumentMut>().unwrap();
            let argv = document["scripts"]["wrapper"][CPU_WRAPPER_NAME]["command"]["command-line"]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| value.as_str().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(
                argv,
                [
                    "/tmp/wrapper",
                    "--cpu-timeout-usec",
                    expected,
                    "--termination-grace-ms",
                    "2000",
                    "--"
                ]
            );
            assert!(rendered.contains("period = \"86s\""));
        }
    }

    #[test]
    fn cpu_budget_refuses_invalid_overflow_and_wall_collision() {
        let source =
            "[profile.default]\nslow-timeout = { period = \"57s\", terminate-after = 1 }\n";
        for multiplier in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::MAX, 1e12, 2.5] {
            assert!(
                scaled_config(source, 1.0, Some((Path::new("/tmp/wrapper"), multiplier))).is_err(),
                "accepted {multiplier}"
            );
        }
        let source = format!(
            "{source}[[profile.ci.overrides]]\nfilter = \"test(short)\"\nslow-timeout = {{ period = \"12s\", terminate-after = 2 }}\n"
        );
        assert!(
            scaled_config(&source, 1.0, Some((Path::new("/tmp/wrapper"), 1.0)))
                .unwrap_err()
                .contains("cleanup grace")
        );
        for raw in ["", "wat", "NaN", "inf", "0", "-1"] {
            assert!(timeouts::parse_timeout_multiplier(Some(raw), "CPU multiplier").is_err());
        }
    }

    #[test]
    fn respects_warning_counts_and_whole_timeout_inheritance() {
        let source = concat!(
            "[profile.default]\nslow-timeout = { period = \"12s\", terminate-after = 3 }\n",
            "[profile.ci]\nstatus-level = \"slow\"\n",
            "[[profile.ci.overrides]]\nfilter = \"test(short)\"\n",
            "slow-timeout = { period = \"1s\" }\n",
        );
        let rendered = scaled_config(source, 1.0, Some((Path::new("/tmp/wrapper"), 1.0))).unwrap();
        assert!(rendered.contains("period = \"12s\", terminate-after = 3"));
        assert!(rendered.contains("period = \"1s\""));
        for invalid in ["0", "-1", "1.5", "\"2\""] {
            let source = format!(
                "[profile.default]\nslow-timeout = {{ period = \"57s\", terminate-after = {invalid} }}\n"
            );
            assert!(scaled_config(&source, 1.0, Some((Path::new("/tmp/wrapper"), 1.0))).is_err());
        }
    }

    fn allocator_inventory(identity: &nextest_cpu::AttemptIdentity) -> serde_json::Value {
        serde_json::json!({"rust-suites": {
            (identity.binary.clone()): {
                "package-name": identity.package,
                "binary-id": identity.binary,
                "binary-name": "liteinst_allocator",
                "kind": "test",
                "binary-path": "/fixture/allocator-tests",
                "testcases": {
                    (identity.test.clone()): {"ignored":false,"filter-match":{"status":"matches"}}
                }
            }
        }})
    }

    fn wrapper_argv(document: &DocumentMut, name: &str) -> Vec<String> {
        document["scripts"]["wrapper"][name]["command"]["command-line"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap().to_string())
            .collect()
    }

    fn measured_inventory(identities: &[nextest_cpu::AttemptIdentity]) -> serde_json::Value {
        let mut inventory = serde_json::json!({"rust-suites": {}});
        for identity in identities {
            let suites = inventory["rust-suites"].as_object_mut().unwrap();
            let suite = suites.entry(identity.binary.clone()).or_insert_with(|| {
                let mut suite =
                    allocator_inventory(identity)["rust-suites"][&identity.binary].clone();
                suite["binary-name"] = identity.binary.rsplit("::").next().unwrap().into();
                suite["binary-path"] = format!("/fixture/{}-tests", identity.binary).into();
                suite["testcases"] = serde_json::json!({});
                suite
            });
            suite["testcases"][&identity.test] =
                serde_json::json!({"ignored":false,"filter-match":{"status":"matches"}});
        }
        inventory
    }

    #[test]
    fn new_measured_cells_require_exact_prepared_selected_identities() {
        for (binary, test, name) in [
            (
                "hermit::liteinst_stacks",
                "cold_constructor::genuine_detcore_constructor_uses_retained_disjoint_bootstrap_stack",
                DETCORE_CONSTRUCTOR_WRAPPER_NAME,
            ),
            (
                "hermit::liteinst_allocator",
                "genuine_standalone_leaf_keeps_scope_free_work_private",
                STANDALONE_ALLOCATOR_WRAPPER_NAME,
            ),
        ] {
            let target = nextest_cpu::AttemptIdentity {
                package: "hermit".into(),
                binary: binary.into(),
                test: test.into(),
                attempt: 1,
            };
            let mut identities = vec![target.clone()];
            for (field, replacement) in [
                (0, "other".to_string()),
                (0, "hermit_extra".to_string()),
                (1, format!("{binary}_extra")),
                (1, format!("prefix{binary}")),
                (1, "other::same_test".to_string()),
                (2, format!("{test}_extra")),
                (2, format!("prefix_{test}")),
                (
                    2,
                    "genuine_detcore_leaf_keeps_scope_free_work_private".into(),
                ),
                (
                    2,
                    "genuine_detcore_reaches_fixed_guarded_callback_and_altstack".into(),
                ),
                (2, "source_copy::gitless_standalone_compiler_sources".into()),
            ] {
                let mut identity = target.clone();
                match field {
                    0 => identity.package = replacement,
                    1 => identity.binary = replacement,
                    _ => identity.test = replacement,
                }
                identities.push(identity);
            }
            for identity in identities {
                let bytes =
                    serde_json::to_vec(&measured_inventory(std::slice::from_ref(&identity)))
                        .unwrap();
                let selected = prepared_selected_cpu_allowances(true, || Ok(bytes)).unwrap();
                let expected = if identity == target {
                    Some((name, "33000000"))
                } else if identity == detcore_allocator_identity() {
                    Some((DETCORE_ALLOCATOR_WRAPPER_NAME, "31000000"))
                } else {
                    None
                };
                assert_eq!(
                    selected.len(),
                    usize::from(expected.is_some()),
                    "{identity:?}"
                );
                let rendered = scaled_config_with_allowances(
                    include_str!("../.config/nextest.toml"),
                    1.0,
                    Some((Path::new("/fixture/wrapper"), 1.0)),
                    &selected,
                )
                .unwrap();
                let document = rendered.parse::<DocumentMut>().unwrap();
                assert_eq!(wrapper_argv(&document, CPU_WRAPPER_NAME)[2], "22000000");
                assert_eq!(
                    run_wrapper_count(&document, "default").unwrap(),
                    1 + selected.len()
                );
                assert_eq!(run_wrapper_count(&document, "ci").unwrap(), 0);
                assert_eq!(
                    document["profile"]["default"]["slow-timeout"]["period"].as_str(),
                    Some("57s")
                );
                for allowance in MEASURED_CPU_ALLOWANCES {
                    if let Some((expected_name, expected_budget)) = expected
                        .filter(|(expected_name, _)| *expected_name == allowance.wrapper_name)
                    {
                        assert_eq!(wrapper_argv(&document, expected_name)[2], expected_budget);
                    } else {
                        assert!(
                            document["scripts"]["wrapper"]
                                .get(allowance.wrapper_name)
                                .is_none(),
                            "{identity:?}"
                        );
                    }
                }
            }
            for state in 0..3 {
                let mut inventory = measured_inventory(std::slice::from_ref(&target));
                let cases = &mut inventory["rust-suites"][binary]["testcases"];
                match state {
                    0 => cases[test]["ignored"] = true.into(),
                    1 => cases[test]["filter-match"]["status"] = "mismatch".into(),
                    _ => cases.as_object_mut().unwrap().clear(),
                }
                let bytes = serde_json::to_vec(&inventory).unwrap();
                assert!(
                    prepared_selected_cpu_allowances(true, || Ok(bytes))
                        .unwrap()
                        .is_empty()
                );
            }
            let bytes = serde_json::to_vec(&measured_inventory(&[target])).unwrap();
            assert!(
                prepared_selected_cpu_allowances(false, || Ok(bytes))
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[test]
    fn every_measured_subset_partitions_once_and_scales_cpu_and_wall_independently() {
        let names = [
            DETCORE_ALLOCATOR_WRAPPER_NAME,
            DETCORE_CONSTRUCTOR_WRAPPER_NAME,
            STANDALONE_ALLOCATOR_WRAPPER_NAME,
        ];
        let filters = [
            "package(=hermit) & binary_id(=hermit::liteinst_allocator) & test(=genuine_detcore_leaf_keeps_scope_free_work_private)",
            "package(=hermit) & binary_id(=hermit::liteinst_stacks) & test(=cold_constructor::genuine_detcore_constructor_uses_retained_disjoint_bootstrap_stack)",
            "package(=hermit) & binary_id(=hermit::liteinst_allocator) & test(=genuine_standalone_leaf_keeps_scope_free_work_private)",
        ];
        for mask in 0..8 {
            let identities = MEASURED_CPU_ALLOWANCES
                .iter()
                .enumerate()
                .rev()
                .filter(|(index, _)| mask & (1 << index) != 0)
                .map(|(_, allowance)| allowance.identity())
                .collect::<Vec<_>>();
            let bytes = serde_json::to_vec(&measured_inventory(&identities)).unwrap();
            let selected = prepared_selected_cpu_allowances(true, || Ok(bytes)).unwrap();
            assert_eq!(selected.len(), identities.len());
            for (cpu, wall, ordinary, allocator, larger, period) in [
                (1.0, 1.0, "22000000", "31000000", "33000000", "57s"),
                (1.25, 1.5, "28000000", "39000000", "42000000", "86s"),
                (0.01, 0.1, "1000000", "1000000", "1000000", "6s"),
            ] {
                let rendered = scaled_config_with_allowances(
                    include_str!("../.config/nextest.toml"),
                    wall,
                    Some((Path::new("/fixture/wrapper"), cpu)),
                    &selected,
                )
                .unwrap();
                let document = rendered.parse::<DocumentMut>().unwrap();
                assert_eq!(wrapper_argv(&document, CPU_WRAPPER_NAME)[2], ordinary);
                assert_eq!(
                    document["profile"]["default"]["slow-timeout"]["period"].as_str(),
                    Some(period)
                );
                let active_filters = filters
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| mask & (1 << index) != 0)
                    .map(|(_, filter)| *filter)
                    .collect::<Vec<_>>();
                let ordinary_filter = if active_filters.is_empty() {
                    "all()".to_string()
                } else {
                    format!("!({})", active_filters.join(" | "))
                };
                let bindings = document["profile"]["default"]["scripts"]
                    .as_array_of_tables()
                    .unwrap();
                let actual = bindings
                    .iter()
                    .map(|binding| {
                        (
                            binding["run-wrapper"].as_str().unwrap(),
                            binding["filter"].as_str().unwrap(),
                        )
                    })
                    .collect::<Vec<_>>();
                let mut expected = vec![(CPU_WRAPPER_NAME, ordinary_filter.as_str())];
                for index in 0..3 {
                    if mask & (1 << index) != 0 {
                        expected.push((names[index], filters[index]));
                        assert_eq!(
                            wrapper_argv(&document, names[index])[2],
                            if index == 0 { allocator } else { larger }
                        );
                    } else {
                        assert!(document["scripts"]["wrapper"].get(names[index]).is_none());
                    }
                }
                assert_eq!(actual, expected);
                assert_eq!(
                    document["scripts"]["wrapper"].as_table().unwrap().len(),
                    expected.len()
                );
                assert_eq!(
                    run_wrapper_count(&document, "default").unwrap(),
                    expected.len()
                );
                assert_eq!(run_wrapper_count(&document, "ci").unwrap(), 0);
            }
        }
    }

    #[test]
    fn new_allowances_keep_strict_cpu_cleanup_separation_from_every_finite_wall() {
        let source =
            "[profile.default]\nslow-timeout = { period = \"57s\", terminate-after = 1 }\n";
        let short = source.replace("57s", "35s");
        assert!(scaled_config(&short, 1.0, Some((Path::new("/fixture/wrapper"), 1.0))).is_ok());
        assert!(
            scaled_config_with_allocator(
                &short,
                1.0,
                Some((Path::new("/fixture/wrapper"), 1.0)),
                true
            )
            .is_ok()
        );
        let mut reversed = MEASURED_CPU_ALLOWANCES.to_vec();
        reversed.reverse();
        for selected in [MEASURED_CPU_ALLOWANCES.as_slice(), reversed.as_slice()] {
            assert!(
                scaled_config_with_allowances(
                    &short,
                    1.0,
                    Some((Path::new("/fixture/wrapper"), 1.0)),
                    selected,
                )
                .unwrap_err()
                .contains("33s")
            );
        }
        for allowance in &MEASURED_CPU_ALLOWANCES[1..] {
            for source in [
                short.clone(),
                format!(
                    "{source}[[profile.ci.overrides]]\nfilter = \"test(short)\"\nslow-timeout = {{ period = \"35s\", terminate-after = 1 }}\n"
                ),
            ] {
                assert!(
                    scaled_config_with_allowances(
                        &source,
                        1.0,
                        Some((Path::new("/fixture/wrapper"), 1.0)),
                        &[*allowance]
                    )
                    .unwrap_err()
                    .contains("33s")
                );
            }
            assert!(
                scaled_config_with_allowances(
                    &short.replace("35s", "36s"),
                    1.0,
                    Some((Path::new("/fixture/wrapper"), 1.0)),
                    &[*allowance]
                )
                .is_ok()
            );
            // 33 * 1.25 rounds once to 42; 57 * .75 rounds to 43 <= 42 + 2.
            assert!(
                scaled_config_with_allowances(
                    source,
                    0.75,
                    Some((Path::new("/fixture/wrapper"), 1.25)),
                    &[*allowance]
                )
                .is_err()
            );
            assert!(
                scaled_config_with_allowances(
                    source,
                    0.8,
                    Some((Path::new("/fixture/wrapper"), 1.25)),
                    &[*allowance]
                )
                .unwrap()
                .contains("period = \"46s\"")
            );
            for cpu in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::MAX, 1e12, 2.0] {
                assert!(
                    scaled_config_with_allowances(
                        source,
                        1.0,
                        Some((Path::new("/fixture/wrapper"), cpu)),
                        &[*allowance]
                    )
                    .is_err()
                );
            }
        }
        assert_eq!(
            DETCORE_CONSTRUCTOR_CPU_SECONDS,
            timeouts::cpu_bound_from_p90_usec(21_619_340)
        );
    }

    #[test]
    fn only_the_prepared_exact_selected_allocator_identity_gets_the_measured_budget() {
        let target = detcore_allocator_identity();
        let mut identities = vec![target.clone()];
        for (field, replacement) in [
            (0, "other"),
            (1, "other::liteinst_allocator"),
            (1, "hermit::liteinst_allocator_extra"),
            (1, "prefixhermit::liteinst_allocator"),
            (2, "genuine_standalone_leaf_keeps_scope_free_work_private"),
            (
                2,
                "genuine_detcore_leaf_keeps_scope_free_work_private_extra",
            ),
            (
                2,
                "prefix_genuine_detcore_leaf_keeps_scope_free_work_private",
            ),
        ] {
            let mut identity = target.clone();
            match field {
                0 => identity.package = replacement.into(),
                1 => identity.binary = replacement.into(),
                _ => identity.test = replacement.into(),
            }
            identities.push(identity);
        }
        for identity in identities {
            let bytes = serde_json::to_vec(&allocator_inventory(&identity)).unwrap();
            let selected = prepared_selected_cpu_allowances(true, || Ok(bytes)).unwrap();
            assert_eq!(
                selected
                    .iter()
                    .any(|allowance| allowance.wrapper_name == DETCORE_ALLOCATOR_WRAPPER_NAME),
                identity == target,
                "{identity:?}"
            );
            let standalone = identity == MEASURED_CPU_ALLOWANCES[2].identity();
            assert_eq!(
                selected.len(),
                usize::from(identity == target || standalone)
            );
            let rendered = scaled_config_with_allowances(
                include_str!("../.config/nextest.toml"),
                1.0,
                Some((Path::new("/fixture/wrapper"), 1.0)),
                &selected,
            )
            .unwrap();
            let document = rendered.parse::<DocumentMut>().unwrap();
            assert_eq!(
                run_wrapper_count(&document, "default").unwrap(),
                1 + selected.len()
            );
            assert_eq!(run_wrapper_count(&document, "ci").unwrap(), 0);
            assert_eq!(wrapper_argv(&document, CPU_WRAPPER_NAME)[2], "22000000");
            if standalone {
                assert_eq!(
                    wrapper_argv(&document, STANDALONE_ALLOCATOR_WRAPPER_NAME)[2],
                    "33000000"
                );
                assert!(
                    document["scripts"]["wrapper"]
                        .get(DETCORE_ALLOCATOR_WRAPPER_NAME)
                        .is_none()
                );
            }
            if selected.is_empty() {
                assert_eq!(
                    document["profile"]["default"]["scripts"]
                        .as_array_of_tables()
                        .unwrap()
                        .iter()
                        .next()
                        .unwrap()["filter"]
                        .as_str(),
                    Some("all()")
                );
                assert!(
                    document["scripts"]["wrapper"]
                        .get(DETCORE_ALLOCATOR_WRAPPER_NAME)
                        .is_none()
                );
            }
        }
    }

    #[test]
    fn ignored_filtered_absent_and_legacy_inventory_keep_the_ordinary_wrapper() {
        let target = detcore_allocator_identity();
        for state in 0..3 {
            let mut inventory = allocator_inventory(&target);
            let cases = &mut inventory["rust-suites"][&target.binary]["testcases"];
            match state {
                0 => cases[&target.test]["ignored"] = true.into(),
                1 => cases[&target.test]["filter-match"]["status"] = "mismatch".into(),
                _ => {
                    cases.as_object_mut().unwrap().clear();
                }
            }
            let bytes = serde_json::to_vec(&inventory).unwrap();
            assert!(
                prepared_selected_cpu_allowances(true, || Ok(bytes))
                    .unwrap()
                    .is_empty()
            );
        }
        assert!(
            prepared_selected_cpu_allowances(true, || Ok(br#"{"rust-suites":{}}"#.to_vec()))
                .unwrap()
                .is_empty()
        );
        // The old null-context caller never consumed its unused inventory. Its
        // malformed or missing input must stay unused when not prepared.
        assert!(
            prepared_selected_cpu_allowances(false, || panic!("legacy inventory read"))
                .unwrap()
                .is_empty()
        );
        assert!(
            prepared_selected_cpu_allowances(false, || Err("missing inventory".into()))
                .unwrap()
                .is_empty()
        );
        assert!(
            prepared_selected_cpu_allowances(false, || Ok(b"malformed".to_vec()))
                .unwrap()
                .is_empty()
        );
        let initial = scaled_config(
            include_str!("../.config/nextest.toml"),
            1.0,
            Some((Path::new("/fixture/wrapper"), 1.0)),
        )
        .unwrap();
        assert_eq!(initial.matches("run-wrapper").count(), 1);
        assert!(!initial.contains(DETCORE_ALLOCATOR_WRAPPER_NAME));
    }

    #[test]
    fn prepared_inventory_refuses_missing_malformed_and_ambiguous_binary_evidence() {
        assert!(
            prepared_selected_cpu_allowances(true, || Err("missing inventory".into())).is_err()
        );
        for bytes in [b"malformed".as_slice(), b"{}".as_slice()] {
            assert!(prepared_selected_cpu_allowances(true, || Ok(bytes.to_vec())).is_err());
        }
        let target = detcore_allocator_identity();
        let original = allocator_inventory(&target);
        for defect in 0..4 {
            let mut inventory = original.clone();
            match defect {
                0 => inventory["rust-suites"][&target.binary]["binary-id"] = "different".into(),
                1 => inventory["rust-suites"][&target.binary]["binary-path"] = "relative".into(),
                2 => {
                    inventory["rust-suites"][&target.binary]["testcases"][&target.test]["filter-match"]
                        ["status"] = "unknown".into()
                }
                _ => {
                    // A second valid key/id with the same actual executable is
                    // ambiguous even though only one identity is the target.
                    let mut duplicate = inventory["rust-suites"][&target.binary].clone();
                    duplicate["binary-id"] = "other::test".into();
                    duplicate["binary-name"] = "other".into();
                    inventory["rust-suites"]["other::test"] = duplicate;
                }
            }
            let bytes = serde_json::to_vec(&inventory).unwrap();
            assert!(
                prepared_selected_cpu_allowances(true, || Ok(bytes)).is_err(),
                "defect {defect}"
            );
        }
    }

    #[test]
    fn admitted_wrappers_are_an_exact_complement_with_one_executable_and_independent_scaling() {
        let source = include_str!("../.config/nextest.toml");
        let exact = "package(=hermit) & binary_id(=hermit::liteinst_allocator) & test(=genuine_detcore_leaf_keeps_scope_free_work_private)";
        let path = "/tmp/a path/it's-$(literal)-wrapper";
        for (multiplier, ordinary, detcore) in [
            (1.0, "22000000", "31000000"),
            (1.25, "28000000", "39000000"),
            (0.01, "1000000", "1000000"),
        ] {
            let rendered = scaled_config_with_allocator(
                source,
                1.0,
                Some((Path::new(path), multiplier)),
                true,
            )
            .unwrap();
            let document = rendered.parse::<DocumentMut>().unwrap();
            assert_eq!(run_wrapper_count(&document, "default").unwrap(), 2);
            assert_eq!(run_wrapper_count(&document, "ci").unwrap(), 0);
            assert_eq!(document["scripts"]["wrapper"].as_table().unwrap().len(), 2);
            let bindings = document["profile"]["default"]["scripts"]
                .as_array_of_tables()
                .unwrap();
            let actual = bindings
                .iter()
                .filter(|rule| rule.contains_key("run-wrapper"))
                .map(|rule| {
                    (
                        rule["filter"].as_str().unwrap(),
                        rule["run-wrapper"].as_str().unwrap(),
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(
                actual,
                [
                    (format!("!({exact})").as_str(), CPU_WRAPPER_NAME),
                    (exact, DETCORE_ALLOCATOR_WRAPPER_NAME)
                ]
            );
            for (name, budget) in [
                (CPU_WRAPPER_NAME, ordinary),
                (DETCORE_ALLOCATOR_WRAPPER_NAME, detcore),
            ] {
                assert_eq!(
                    wrapper_argv(&document, name),
                    [
                        path,
                        "--cpu-timeout-usec",
                        budget,
                        "--termination-grace-ms",
                        "2000",
                        "--"
                    ]
                );
                assert_eq!(
                    document["scripts"]["wrapper"][name]["command"]["relative-to"].as_str(),
                    Some("none")
                );
                assert_eq!(
                    document["scripts"]["wrapper"][name]["target-runner"].as_str(),
                    Some("within-wrapper")
                );
            }
            assert_eq!(
                document["experimental"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|value| value.as_str() == Some("wrapper-scripts"))
                    .count(),
                1
            );
            assert!(rendered.contains("period = \"57s\""));
        }
    }

    #[test]
    fn larger_allocator_cpu_and_grace_must_fit_every_scaled_finite_wall_bound() {
        let source =
            "[profile.default]\nslow-timeout = { period = \"57s\", terminate-after = 1 }\n";
        // The old 22+2 fits 33, but the admitted 31+2 must be strictly below it.
        let short = source.replace("57s", "33s");
        assert!(scaled_config(&short, 1.0, Some((Path::new("/fixture/wrapper"), 1.0))).is_ok());
        assert!(
            scaled_config_with_allocator(
                &short,
                1.0,
                Some((Path::new("/fixture/wrapper"), 1.0)),
                true
            )
            .unwrap_err()
            .contains("31s")
        );
        let shorter_override = format!(
            "{source}[[profile.ci.overrides]]\nfilter = \"test(short)\"\nslow-timeout = {{ period = \"33s\", terminate-after = 1 }}\n"
        );
        assert!(
            scaled_config_with_allocator(
                &shorter_override,
                1.0,
                Some((Path::new("/fixture/wrapper"), 1.0)),
                true
            )
            .is_err()
        );
        for multiplier in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::MAX, 1e12, 2.0] {
            assert!(
                scaled_config_with_allocator(
                    source,
                    1.0,
                    Some((Path::new("/fixture/wrapper"), multiplier)),
                    true
                )
                .is_err()
            );
        }
        // CPU rounding: 31*1.25 -> 39; wall rounds 57*.7 -> 40, below 39+2.
        assert!(
            scaled_config_with_allocator(
                source,
                0.7,
                Some((Path::new("/fixture/wrapper"), 1.25)),
                true
            )
            .is_err()
        );
        assert!(
            scaled_config_with_allocator(
                source,
                1.25,
                Some((Path::new("/fixture/wrapper"), 1.25)),
                true
            )
            .unwrap()
            .contains("period = \"72s\"")
        );
        let warning = "[profile.default]\nslow-timeout = { period = \"1s\" }\n";
        assert!(
            scaled_config_with_allocator(
                warning,
                1.0,
                Some((Path::new("/fixture/wrapper"), 1.0)),
                true
            )
            .is_ok()
        );
    }

    #[test]
    fn both_branches_refuse_existing_bindings_and_either_owned_command_collision() {
        for admitted in [false, true] {
            for profile in ["default", "ci"] {
                let source = format!(
                    "[profile.default]\nslow-timeout = {{ period = \"57s\" }}\n[[profile.{profile}.scripts]]\nfilter = \"all()\"\nrun-wrapper = \"other\"\n"
                );
                assert!(
                    scaled_config_with_allocator(
                        &source,
                        1.0,
                        Some((Path::new("/fixture/wrapper"), 1.0)),
                        admitted
                    )
                    .unwrap_err()
                    .contains("already defines")
                );
            }
            for name in [
                CPU_WRAPPER_NAME,
                DETCORE_ALLOCATOR_WRAPPER_NAME,
                DETCORE_CONSTRUCTOR_WRAPPER_NAME,
                STANDALONE_ALLOCATOR_WRAPPER_NAME,
            ] {
                let source = format!(
                    "[profile.default]\nslow-timeout = {{ period = \"57s\" }}\n[scripts.wrapper.{name}]\ntarget-runner = \"within-wrapper\"\ncommand = {{ command-line = [\"/other\"], relative-to = \"none\" }}\n"
                );
                assert!(
                    scaled_config_with_allocator(
                        &source,
                        1.0,
                        Some((Path::new("/fixture/wrapper"), 1.0)),
                        admitted
                    )
                    .unwrap_err()
                    .contains("already defined")
                );
            }
            let duplicate_feature = "experimental = [\"wrapper-scripts\", \"wrapper-scripts\"]\n[profile.default]\nslow-timeout = { period = \"57s\" }\n";
            assert!(
                scaled_config_with_allocator(
                    duplicate_feature,
                    1.0,
                    Some((Path::new("/fixture/wrapper"), 1.0)),
                    admitted
                )
                .is_err()
            );
        }
    }

    #[test]
    fn exact_allocator_admission_does_not_enroll_hermit_in_regular_calibration() {
        let inventory =
            serde_json::to_vec(&allocator_inventory(&detcore_allocator_identity())).unwrap();
        assert_eq!(
            prepared_selected_cpu_allowances(true, || Ok(inventory.clone()))
                .unwrap()
                .len(),
            1
        );
        let (_, context, _) = budget_fixture();
        assert!(
            resolve_budgets(&inventory, context, None, 1, 0, 1.0, 1.0)
                .unwrap_err()
                .contains("regular calibration cannot cover the hermit package")
        );
        assert_eq!(
            DETCORE_ALLOCATOR_CPU_SECONDS,
            timeouts::cpu_bound_from_p90_usec(20_171_435)
        );
    }
}
