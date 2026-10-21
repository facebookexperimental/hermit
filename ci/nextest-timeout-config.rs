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
const DETCORE_STACK_WRAPPER_NAME: &str = "hermit-detcore-stack-cpu";
// Whole-attempt observations and all 24 fixture decisions are retained in
// ci/liteinst-fixture-cpu-budgets.md. Every prepared stacks test has a separate
// 3x-CPU wall backstop; allocator and unrelated wall policies stay unchanged.
// prior CPU timeouts remain censored, not successful old-cap completion samples.
// Detcore allocator completed maximum 20.754169s: ceil(1.5 * max) = 32.
const DETCORE_ALLOCATOR_CPU_SECONDS: u64 = 32;
// Detcore constructor completed maximum 36.113714s: ceil(1.5 * max) = 55.
const DETCORE_CONSTRUCTOR_CPU_SECONDS: u64 = 55;
// Separately approved host-load backstop, not measured wall headroom.
// The earlier f141 wall timeout remains a failed run.
const DETCORE_CONSTRUCTOR_WALL_SECONDS: u64 = 3 * DETCORE_CONSTRUCTOR_CPU_SECONDS;
// Retained d7efd4cb/da86dfbe standalone diagnostic N=3 at CPU44/wall57:
// 17.718674, 17.632669, 17.573148s. The completed-sample formula would give 27.
// Use 33 conservatively after the concurrent CPU22 timeout at 22.799306s;
// that censored failure has unknown completion cost. Wall and cleanup stay unchanged.
const STANDALONE_ALLOCATOR_CPU_SECONDS: u64 = 33;
// Detcore stack completed maximum 27.542858s gave ceil(1.5 * max) = 42 at one test at a time.
// With the node at five tests at once (2026-10-17), its completed maximum is 33.721186s
// (validation of c63f1d21): ceil(1.5 * max) = 51.
// Earlier CPU22 timeouts stay censored, not successful completion samples.
const DETCORE_STACK_CPU_SECONDS: u64 = 51;
const RENDERED_PROCFS_WRAPPER_NAME: &str = "hermit-rendered-procfs-cpu";
// hermit-run::cli detcore_renders_its_procfs_files_on_every_backend runs a
// --verify of one guest on every backend this build has, DBT's among them, plus
// a DBT chroot run. Completed cgroup CPU seconds in the 13 full validations of
// 2026-10-15/16 that ran it: 4.5 to 14.9 (12.8, 12.9 and 14.6 at 2f640c23,
// 90dec733 and d7efd4cb), at 18-24 s wall. dbbbdc86's was stopped by the
// 22-second cap at 23.6 (32.9 s wall), in a run where every DBT test cost more
// (detcore_fixes_the_virtual_kernel_files_on_dbt 17.6 against 14.0-15.6).
// ceil(1.5 * 23.6) = 36 covers that censored run with half again; the
// completed-sample formula would give 23. At that run's 0.72 CPU per wall
// second, 36 CPU seconds take 50 s, inside the unchanged 57-second wall.
const RENDERED_PROCFS_CPU_SECONDS: u64 = 36;
const CPU_PROFILE_V3_LOADER_WRAPPER_NAME: &str = "hermit-cpu-profile-v3-loader-cpu";
// hermit-run::cli cpu_profile_v3_host_loader_sees_exactly_x86_64_v3 runs the host
// glibc loader under `--cpu-profile x86-64-v3` several times
// (--list-diagnostics, --help and more). Recorded cgroup attempts under
// ignored/validate on 2026-10-15/17: 110 completed at 3.01 to 17.22 CPU
// seconds, median 3.91, at most 18.5 s wall (17.22 in a buck-cargo repro
// validation, 13.10 at 2f0d048a, 12.85 at 81cccd55). 9eb80258's full
// validation, at a host load around 330, was stopped by the 22-second cap at
// 23.995 (35.5 s wall); c59f758a's then completed it at 4.86, and six
// --only runs of the node at 9eb80258 at loads of 260 to 300 at 3.11 to 6.07.
// ceil(1.5 * 23.995) = 36 covers that censored run with half again; the
// completed-sample formula would give 26. Its wall backstop is three times
// the cap, as for the liteinst_stacks identities: a host-load policy.
const CPU_PROFILE_V3_LOADER_CPU_SECONDS: u64 = 36;
const CPU_TERMINATION_GRACE_SECONDS: u64 = 2;

#[derive(Clone, Copy)]
struct MeasuredCpuAllowance {
    wrapper_name: &'static str,
    binary: &'static str,
    test: &'static str,
    cpu_seconds: u64,
    wall_seconds: Option<u64>,
}

impl MeasuredCpuAllowance {
    const fn stack(wrapper_name: &'static str, test: &'static str, cpu_seconds: u64) -> Self {
        Self {
            wrapper_name,
            binary: "hermit-run::liteinst_stacks",
            test,
            cpu_seconds,
            wall_seconds: Some(3 * cpu_seconds),
        }
    }

    fn identity(self) -> nextest_cpu::AttemptIdentity {
        nextest_cpu::AttemptIdentity {
            package: "hermit-run".into(),
            binary: self.binary.into(),
            test: self.test.into(),
            attempt: 1,
        }
    }
}

const MEASURED_CPU_ALLOWANCES: [MeasuredCpuAllowance; 19] = [
    MeasuredCpuAllowance {
        wrapper_name: DETCORE_ALLOCATOR_WRAPPER_NAME,
        binary: "hermit-run::liteinst_allocator",
        test: "genuine_detcore_leaf_keeps_scope_free_work_private",
        cpu_seconds: DETCORE_ALLOCATOR_CPU_SECONDS,
        wall_seconds: None,
    },
    MeasuredCpuAllowance {
        wrapper_name: DETCORE_CONSTRUCTOR_WRAPPER_NAME,
        binary: "hermit-run::liteinst_stacks",
        test: "cold_constructor::genuine_detcore_constructor_uses_retained_disjoint_bootstrap_stack",
        cpu_seconds: DETCORE_CONSTRUCTOR_CPU_SECONDS,
        wall_seconds: Some(DETCORE_CONSTRUCTOR_WALL_SECONDS),
    },
    MeasuredCpuAllowance {
        wrapper_name: STANDALONE_ALLOCATOR_WRAPPER_NAME,
        binary: "hermit-run::liteinst_allocator",
        test: "genuine_standalone_leaf_keeps_scope_free_work_private",
        cpu_seconds: STANDALONE_ALLOCATOR_CPU_SECONDS,
        wall_seconds: None,
    },
    MeasuredCpuAllowance {
        wrapper_name: RENDERED_PROCFS_WRAPPER_NAME,
        binary: "hermit-run::cli",
        test: "detcore_renders_its_procfs_files_on_every_backend",
        cpu_seconds: RENDERED_PROCFS_CPU_SECONDS,
        wall_seconds: None,
    },
    MeasuredCpuAllowance {
        wrapper_name: DETCORE_STACK_WRAPPER_NAME,
        binary: "hermit-run::liteinst_stacks",
        test: "genuine_detcore_reaches_fixed_guarded_callback_and_altstack",
        cpu_seconds: DETCORE_STACK_CPU_SECONDS,
        wall_seconds: Some(3 * DETCORE_STACK_CPU_SECONDS),
    },
    MeasuredCpuAllowance::stack(
        "hermit-stack-exact-got-cpu",
        "artifact::tests::credits_exact_local_relative_got_route_and_direct_tailcall",
        22,
    ),
    MeasuredCpuAllowance::stack(
        "hermit-stack-allocator-endpoints-cpu",
        "artifact::tests::inline_endpoints_require_owned_allocator_allocation_methods",
        22,
    ),
    MeasuredCpuAllowance::stack(
        "hermit-stack-build-finished-cpu",
        "artifact::tests::missing_build_finished_is_a_failed_current_artifact_selection",
        22,
    ),
    MeasuredCpuAllowance::stack(
        "hermit-stack-tls-recipes-cpu",
        "artifact::tests::observed_integrated_detcore_e8_tls_whole_recipes",
        22,
    ),
    MeasuredCpuAllowance::stack(
        "hermit-stack-symbol-versions-cpu",
        "artifact::tests::refuses_new_or_unrecognized_guest_symbol_versions",
        22,
    ),
    MeasuredCpuAllowance::stack(
        "hermit-stack-indirect-calls-cpu",
        "artifact::tests::unresolved_or_preemptible_indirect_calls_do_not_earn_a_path",
        22,
    ),
    MeasuredCpuAllowance::stack(
        "hermit-stack-caller-stack-cpu",
        "cold_constructor::caller_stack_samples_never_receive_constructor_placement_credit",
        22,
    ),
    MeasuredCpuAllowance::stack(
        "hermit-stack-guard-alias-cpu",
        "cold_constructor::later_guard_cannot_alias_the_retained_bootstrap_extent",
        22,
    ),
    MeasuredCpuAllowance::stack(
        "hermit-stack-standalone-altstack-cpu",
        "genuine_standalone_registers_fixed_guarded_altstack_without_continuation",
        47,
    ),
    MeasuredCpuAllowance::stack(
        "hermit-stack-inert-preload-cpu",
        "inert_preload_does_not_reserve_or_register_runtime_stacks",
        49,
    ),
    MeasuredCpuAllowance::stack(
        "hermit-stack-foreign-callback-cpu",
        "stack_observation::tests::rejects_foreign_and_unreached_callback_records",
        22,
    ),
    MeasuredCpuAllowance::stack(
        "hermit-stack-readable-guards-cpu",
        "stack_observation::tests::rejects_unprobed_and_readable_guards",
        22,
    ),
    MeasuredCpuAllowance::stack(
        "hermit-stack-disabled-altstack-cpu",
        "standalone_altstack_disabled_receives_no_owned_stack_credit",
        46,
    ),
    MeasuredCpuAllowance {
        wrapper_name: CPU_PROFILE_V3_LOADER_WRAPPER_NAME,
        binary: "hermit-run::cli",
        test: "cpu_profile_v3_host_loader_sees_exactly_x86_64_v3",
        cpu_seconds: CPU_PROFILE_V3_LOADER_CPU_SECONDS,
        wall_seconds: Some(3 * CPU_PROFILE_V3_LOADER_CPU_SECONDS),
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

// Exact selected identities only. Put the wall policy before existing rules in
// default and any profile with its own overrides: Nextest uses the first matching
// override, and a profile's own overrides take precedence over inherited ones.
fn add_wall_allowances(
    document: &mut DocumentMut,
    selected: &[MeasuredCpuAllowance],
    multiplier: f64,
) -> Result<(), String> {
    for allowance in selected {
        let Some(base_seconds) = allowance.wall_seconds else {
            continue;
        };
        let seconds = timeouts::scale_timeout_seconds(base_seconds, multiplier, "wall multiplier")?;
        let mut timeout = InlineTable::new();
        timeout.insert("period", Value::from(format!("{seconds}s")));
        timeout.insert("terminate-after", Value::from(1_i64));
        timeout.insert("grace-period", Value::from("2s"));
        let mut rule = Table::new();
        rule["filter"] = value(exact_filter(&allowance.identity()));
        rule["slow-timeout"] = value(timeout);
        let profiles = document["profile"]
            .as_table_mut()
            .ok_or("profile must be a table")?;
        profiles
            .entry("default")
            .or_insert_with(|| Item::Table(Table::new()));
        for (name, profile) in profiles.iter_mut() {
            let profile = profile.as_table_mut().ok_or("profile must be a table")?;
            if name != "default" && !profile.contains_key("overrides") {
                continue;
            }
            let item = profile
                .entry("overrides")
                .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
            let existing = item
                .as_array_of_tables()
                .ok_or("profile overrides must be an array of tables")?;
            let mut overrides = ArrayOfTables::new();
            overrides.push(rule.clone());
            for entry in existing.iter() {
                overrides.push(entry.clone());
            }
            *item = Item::ArrayOfTables(overrides);
        }
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
        // Original deadlines still bound the ordinary partition and every
        // allowance without an exact wall override. Owned walls are checked
        // against their own CPU budget, not another test's budget.
        let mut maximum_cpu_seconds = cpu_seconds;
        if selected_allowances
            .iter()
            .any(|row| row.wall_seconds.is_some())
        {
            let mut default = document
                .get("profile")
                .and_then(|profiles| profiles.get("default"))
                .and_then(|profile| profile.get("slow-timeout"))
                .cloned()
                .ok_or("owned wall allowances require an original finite default timeout")?;
            if scale_timeout_item(&mut default, 1.0)?.is_none() {
                return Err(
                    "owned wall allowances require an original finite default timeout".into(),
                );
            }
        }
        let measured_budgets = selected_allowances
            .iter()
            .map(|allowance| {
                let seconds = timeouts::scale_timeout_seconds(
                    allowance.cpu_seconds,
                    cpu_multiplier,
                    "CPU multiplier",
                )?;
                if let Some(base_wall) = allowance.wall_seconds {
                    let wall = timeouts::scale_timeout_seconds(base_wall, multiplier, "wall multiplier")?;
                    let cpu_and_cleanup = seconds.checked_add(CPU_TERMINATION_GRACE_SECONDS)
                        .ok_or("CPU timeout plus cleanup grace overflows seconds")?;
                    if wall <= cpu_and_cleanup {
                        return Err(format!(
                            "nextest owned wall ({wall}s) for {} must exceed its CPU timeout ({seconds}s) plus cleanup grace ({CPU_TERMINATION_GRACE_SECONDS}s)",
                            allowance.test,
                        ));
                    }
                } else {
                    maximum_cpu_seconds = maximum_cpu_seconds.max(seconds);
                }
                let budget = seconds
                    .checked_mul(1_000_000)
                    .ok_or("CPU timeout overflows microseconds")?;
                Ok((*allowance, budget))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let cpu_and_cleanup = maximum_cpu_seconds
            .checked_add(CPU_TERMINATION_GRACE_SECONDS)
            .ok_or("CPU timeout plus cleanup grace overflows seconds")?;
        // Every original finite deadline still bounds the unoverridden
        // partition. An exact owned wall deliberately replaces that deadline
        // for its selected test only, with separation checked above.
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
        add_wall_allowances(&mut document, selected_allowances, multiplier)?;
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
    "package(=hermit-run) & binary(=cli) & (test(=record_classifies_a_gdbserver_replay_stage_container_child_failure) | test(=every_record_container_site_classifies_a_child_fault_by_name))",
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
        .any(|identity| identity.package == "hermit-run")
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
            "usage: nextest-timeout-config.rs --resolve SOURCE MULTIPLIER OUTPUT INVENTORY CONTEXT MAP CALIBRATION -- NEXTEST_ARGS...\n\nGenerate Nextest config OUTPUT from canonical TOML SOURCE and the wall MULTIPLIER. INVENTORY is typed Nextest list JSON; CONTEXT is verified budget-context JSON (or null); MAP is a new absolute output path; CALIBRATION is the optional reviewed JSON table. NEXTEST_ARGS are the actual counted-run arguments, including concurrency and retries. HERMIT_NEXTEST_CPU_WRAPPER_BIN must name the prepared wrapper; HERMIT_TEST_CPU_TIMEOUT_MULTIPLIER defaults to 1.\n\nAbsent/stale regular calibration retains the regular default bounds. Prepared post-inventory selection admits only the exact Detcore allocator ({DETCORE_ALLOCATOR_CPU_SECONDS}s), Detcore constructor ({DETCORE_CONSTRUCTOR_CPU_SECONDS}s), standalone allocator ({STANDALONE_ALLOCATOR_CPU_SECONDS}s), Detcore stack callback ({DETCORE_STACK_CPU_SECONDS}s), rendered-procfs ({RENDERED_PROCFS_CPU_SECONDS}s), CPU-profile v3 host loader ({CPU_PROFILE_V3_LOADER_CPU_SECONDS}s), and all 15 measured stacks identities (22 to 55 seconds) base CPU allowances, each scaled once; tests outside those exact identities keep the 22-second base. Every exact prepared selected liteinst_stacks identity, and the CPU-profile v3 host loader, gets a separate base wall backstop of three times its base CPU cap, scaled once by the wall multiplier. This is a host-load policy, not measured wall headroom; other wall bounds and cleanup2 are unchanged. See ci/liteinst-fixture-cpu-budgets.md for the complete fixture decisions and censored-observation limits. Unknown context/profile/concurrency produces no regular map or aggregate certification. Malformed input refuses. No test or preparation command is run."
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
                let wall = match allowance.wall_seconds {
                    Some(base) => format!(
                        "wall backstop base {base}s effective {}s (wall multiplier {wall_multiplier}); cleanup 2s",
                        timeouts::scale_timeout_seconds(base, wall_multiplier, "wall multiplier")?
                    ),
                    None => format!(
                        "existing wall bounds unchanged except wall multiplier {wall_multiplier}"
                    ),
                };
                eprintln!(
                    "nextest budgets: prepared exact {} test {} CPU base {}s \
                     (CPU multiplier {cpu_multiplier}); ordinary CPU base {}s; \
                     {wall}; regular population/concurrency not certified",
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
        for (binary, test, name, budget) in [
            (
                "hermit-run::liteinst_stacks",
                "cold_constructor::genuine_detcore_constructor_uses_retained_disjoint_bootstrap_stack",
                DETCORE_CONSTRUCTOR_WRAPPER_NAME,
                "55000000",
            ),
            (
                "hermit-run::liteinst_allocator",
                "genuine_standalone_leaf_keeps_scope_free_work_private",
                STANDALONE_ALLOCATOR_WRAPPER_NAME,
                "33000000",
            ),
            (
                "hermit-run::liteinst_stacks",
                "genuine_detcore_reaches_fixed_guarded_callback_and_altstack",
                DETCORE_STACK_WRAPPER_NAME,
                "51000000",
            ),
        ] {
            let target = nextest_cpu::AttemptIdentity {
                package: "hermit-run".into(),
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
                (
                    2,
                    "genuine_standalone_registers_fixed_guarded_altstack_without_continuation".into(),
                ),
                (
                    2,
                    "standalone_altstack_disabled_receives_no_owned_stack_credit".into(),
                ),
                (
                    2,
                    "inert_preload_does_not_reserve_or_register_runtime_stacks".into(),
                ),
                (
                    2,
                    "cold_constructor::caller_stack_samples_never_receive_constructor_placement_credit".into(),
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
                    Some((name, budget))
                } else if identity == detcore_allocator_identity() {
                    Some((DETCORE_ALLOCATOR_WRAPPER_NAME, "32000000"))
                } else if identity.package == "hermit-run"
                    && identity.binary == "hermit-run::liteinst_stacks"
                {
                    match identity.test.as_str() {
                        "genuine_detcore_reaches_fixed_guarded_callback_and_altstack" => {
                            Some((DETCORE_STACK_WRAPPER_NAME, "51000000"))
                        }
                        "genuine_standalone_registers_fixed_guarded_altstack_without_continuation" => {
                            Some(("hermit-stack-standalone-altstack-cpu", "47000000"))
                        }
                        "standalone_altstack_disabled_receives_no_owned_stack_credit" => {
                            Some(("hermit-stack-disabled-altstack-cpu", "46000000"))
                        }
                        "inert_preload_does_not_reserve_or_register_runtime_stacks" => {
                            Some(("hermit-stack-inert-preload-cpu", "49000000"))
                        }
                        "cold_constructor::caller_stack_samples_never_receive_constructor_placement_credit" => {
                            Some(("hermit-stack-caller-stack-cpu", "22000000"))
                        }
                        _ => None,
                    }
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
                let wall_overrides = document["profile"]["default"]["overrides"]
                    .as_array_of_tables()
                    .unwrap()
                    .iter()
                    .filter(|entry| entry.contains_key("slow-timeout"))
                    .collect::<Vec<_>>();
                let expected_wall = expected
                    .filter(|_| identity.binary == "hermit-run::liteinst_stacks")
                    .map(|(_, cpu)| format!("{}s", cpu.parse::<u64>().unwrap() / 1_000_000 * 3));
                assert_eq!(wall_overrides.len(), usize::from(expected_wall.is_some()));
                if let Some(entry) = wall_overrides.first() {
                    assert_eq!(
                        entry["filter"].as_str(),
                        Some(exact_filter(&identity).as_str())
                    );
                    assert_eq!(
                        entry["slow-timeout"]["period"].as_str(),
                        expected_wall.as_deref()
                    );
                    assert_eq!(
                        entry["slow-timeout"]["terminate-after"].as_integer(),
                        Some(1)
                    );
                    assert_eq!(entry["slow-timeout"]["grace-period"].as_str(), Some("2s"));
                }
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
    fn cpu_profile_v3_loader_has_its_measured_cap_and_wall_backstop() {
        let identity = nextest_cpu::AttemptIdentity {
            package: "hermit-run".into(),
            binary: "hermit-run::cli".into(),
            test: "cpu_profile_v3_host_loader_sees_exactly_x86_64_v3".into(),
            attempt: 1,
        };
        let inventory = measured_inventory(std::slice::from_ref(&identity));
        let selected =
            prepared_selected_cpu_allowances(true, || Ok(serde_json::to_vec(&inventory).unwrap()))
                .unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(
            (selected[0].cpu_seconds, selected[0].wall_seconds),
            (36, Some(108))
        );
        for (cpu_multiplier, wall_multiplier) in [(1.0, 1.0), (1.25, 1.5)] {
            let rendered = scaled_config_with_allowances(
                include_str!("../.config/nextest.toml"),
                wall_multiplier,
                Some((Path::new("/fixture/wrapper"), cpu_multiplier)),
                &selected,
            )
            .unwrap();
            let doc = rendered.parse::<DocumentMut>().unwrap();
            let cpu_seconds =
                timeouts::scale_timeout_seconds(36, cpu_multiplier, "CPU multiplier").unwrap();
            assert_eq!(
                wrapper_argv(&doc, CPU_PROFILE_V3_LOADER_WRAPPER_NAME)[2],
                (cpu_seconds * 1_000_000).to_string()
            );
            let filter = exact_filter(&identity);
            let walls = doc["profile"]["default"]["overrides"]
                .as_array_of_tables()
                .unwrap()
                .iter()
                .filter(|r| r.contains_key("slow-timeout") && r["filter"].as_str() == Some(&filter))
                .collect::<Vec<_>>();
            assert_eq!(walls.len(), 1);
            let wall_seconds =
                timeouts::scale_timeout_seconds(108, wall_multiplier, "wall multiplier").unwrap();
            assert_eq!(
                walls[0]["slow-timeout"]["period"].as_str(),
                Some(format!("{wall_seconds}s").as_str())
            );
            assert!(wall_seconds > cpu_seconds + CPU_TERMINATION_GRACE_SECONDS);
        }
        // Without prepared admission the test keeps the ordinary 22-second cap.
        assert!(
            prepared_selected_cpu_allowances(false, || Ok(serde_json::to_vec(&inventory).unwrap()))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn original_five_allowance_subsets_partition_once_and_scale_independently() {
        let names = [
            DETCORE_ALLOCATOR_WRAPPER_NAME,
            DETCORE_CONSTRUCTOR_WRAPPER_NAME,
            STANDALONE_ALLOCATOR_WRAPPER_NAME,
            RENDERED_PROCFS_WRAPPER_NAME,
            DETCORE_STACK_WRAPPER_NAME,
        ];
        let filters = [
            "package(=hermit-run) & binary_id(=hermit-run::liteinst_allocator) & test(=genuine_detcore_leaf_keeps_scope_free_work_private)",
            "package(=hermit-run) & binary_id(=hermit-run::liteinst_stacks) & test(=cold_constructor::genuine_detcore_constructor_uses_retained_disjoint_bootstrap_stack)",
            "package(=hermit-run) & binary_id(=hermit-run::liteinst_allocator) & test(=genuine_standalone_leaf_keeps_scope_free_work_private)",
            "package(=hermit-run) & binary_id(=hermit-run::cli) & test(=detcore_renders_its_procfs_files_on_every_backend)",
            "package(=hermit-run) & binary_id(=hermit-run::liteinst_stacks) & test(=genuine_detcore_reaches_fixed_guarded_callback_and_altstack)",
        ];
        for mask in 0..(1 << names.len()) {
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
            for (cpu, wall, ordinary, measured, period, constructor_period, callback_period) in [
                (
                    1.0,
                    1.0,
                    "22000000",
                    ["32000000", "55000000", "33000000", "36000000", "51000000"],
                    "57s",
                    "165s",
                    "153s",
                ),
                (
                    1.25,
                    1.5,
                    "28000000",
                    ["40000000", "69000000", "42000000", "45000000", "64000000"],
                    "86s",
                    "248s",
                    "230s",
                ),
                (
                    0.01,
                    0.1,
                    "1000000",
                    ["1000000", "1000000", "1000000", "1000000", "1000000"],
                    "6s",
                    "17s",
                    "16s",
                ),
                (
                    1.0,
                    1.01,
                    "22000000",
                    ["32000000", "55000000", "33000000", "36000000", "51000000"],
                    "58s",
                    "167s",
                    "155s",
                ),
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
                let wall_overrides = document["profile"]["default"]["overrides"]
                    .as_array_of_tables()
                    .unwrap()
                    .iter()
                    .filter(|entry| entry.contains_key("slow-timeout"))
                    .collect::<Vec<_>>();
                assert_eq!(
                    wall_overrides.len(),
                    usize::from(mask & 2 != 0) + usize::from(mask & 16 != 0)
                );
                for (index, expected_period) in [(1, constructor_period), (4, callback_period)] {
                    let matches = wall_overrides
                        .iter()
                        .filter(|row| row["filter"].as_str() == Some(filters[index]))
                        .collect::<Vec<_>>();
                    assert_eq!(matches.len(), usize::from(mask & (1 << index) != 0));
                    if let Some(entry) = matches.first() {
                        assert_eq!(
                            entry["slow-timeout"]["period"].as_str(),
                            Some(expected_period)
                        );
                        assert_eq!(
                            entry["slow-timeout"]["terminate-after"].as_integer(),
                            Some(1)
                        );
                        assert_eq!(entry["slow-timeout"]["grace-period"].as_str(), Some("2s"));
                    }
                }
                // ci inherits the default exact rule; no extra profile rule is needed.
                assert!(document["profile"]["ci"].get("overrides").is_none());
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
                for index in 0..names.len() {
                    if mask & (1 << index) != 0 {
                        expected.push((names[index], filters[index]));
                        assert_eq!(wrapper_argv(&document, names[index])[2], measured[index]);
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
    fn whole_stacks_inventory_has_exact_cpu_and_wall_rules_without_neighbor_credit() {
        let expected: [(&str, u64, u64); 15] = [
            (
                "artifact::tests::credits_exact_local_relative_got_route_and_direct_tailcall",
                22,
                66,
            ),
            (
                "artifact::tests::inline_endpoints_require_owned_allocator_allocation_methods",
                22,
                66,
            ),
            (
                "artifact::tests::missing_build_finished_is_a_failed_current_artifact_selection",
                22,
                66,
            ),
            (
                "artifact::tests::observed_integrated_detcore_e8_tls_whole_recipes",
                22,
                66,
            ),
            (
                "artifact::tests::refuses_new_or_unrecognized_guest_symbol_versions",
                22,
                66,
            ),
            (
                "artifact::tests::unresolved_or_preemptible_indirect_calls_do_not_earn_a_path",
                22,
                66,
            ),
            (
                "cold_constructor::caller_stack_samples_never_receive_constructor_placement_credit",
                22,
                66,
            ),
            (
                "cold_constructor::genuine_detcore_constructor_uses_retained_disjoint_bootstrap_stack",
                55,
                165,
            ),
            (
                "cold_constructor::later_guard_cannot_alias_the_retained_bootstrap_extent",
                22,
                66,
            ),
            (
                "genuine_detcore_reaches_fixed_guarded_callback_and_altstack",
                51,
                153,
            ),
            (
                "genuine_standalone_registers_fixed_guarded_altstack_without_continuation",
                47,
                141,
            ),
            (
                "inert_preload_does_not_reserve_or_register_runtime_stacks",
                49,
                147,
            ),
            (
                "stack_observation::tests::rejects_foreign_and_unreached_callback_records",
                22,
                66,
            ),
            (
                "stack_observation::tests::rejects_unprobed_and_readable_guards",
                22,
                66,
            ),
            (
                "standalone_altstack_disabled_receives_no_owned_stack_credit",
                46,
                138,
            ),
        ];
        let identities = expected
            .iter()
            .map(|(test, _, _)| nextest_cpu::AttemptIdentity {
                package: "hermit-run".into(),
                binary: "hermit-run::liteinst_stacks".into(),
                test: (*test).into(),
                attempt: 1,
            })
            .collect::<Vec<_>>();
        let inventory = measured_inventory(&identities);
        let selected =
            prepared_selected_cpu_allowances(true, || Ok(serde_json::to_vec(&inventory).unwrap()))
                .unwrap();
        assert_eq!(selected.len(), 15);
        assert_eq!(MEASURED_CPU_ALLOWANCES.len(), 19);
        for (cpu_multiplier, wall_multiplier) in [(1.0, 1.0), (1.25, 1.5), (0.01, 0.1)] {
            let rendered = scaled_config_with_allowances(
                include_str!("../.config/nextest.toml"),
                wall_multiplier,
                Some((Path::new("/fixture/wrapper"), cpu_multiplier)),
                &selected,
            )
            .unwrap();
            let doc = rendered.parse::<DocumentMut>().unwrap();
            let walls = doc["profile"]["default"]["overrides"]
                .as_array_of_tables()
                .unwrap()
                .iter()
                .filter(|r| r.contains_key("slow-timeout"))
                .collect::<Vec<_>>();
            assert_eq!(walls.len(), 15);
            assert_eq!(run_wrapper_count(&doc, "default").unwrap(), 16);
            assert_eq!(run_wrapper_count(&doc, "ci").unwrap(), 0);
            assert!(doc["profile"]["ci"].get("overrides").is_none());
            for ((test, cpu, wall), identity) in expected.iter().zip(&identities) {
                let allowance = selected.iter().find(|a| a.test == *test).unwrap();
                assert_eq!(
                    (allowance.cpu_seconds, allowance.wall_seconds),
                    (*cpu, Some(*wall))
                );
                let cpu_usec =
                    timeouts::scale_timeout_seconds(*cpu, cpu_multiplier, "CPU multiplier")
                        .unwrap()
                        * 1_000_000;
                assert_eq!(
                    wrapper_argv(&doc, allowance.wrapper_name)[2],
                    cpu_usec.to_string()
                );
                let filter = exact_filter(identity);
                let matching = walls
                    .iter()
                    .filter(|r| r["filter"].as_str() == Some(&filter))
                    .collect::<Vec<_>>();
                assert_eq!(matching.len(), 1);
                let wall_seconds =
                    timeouts::scale_timeout_seconds(*wall, wall_multiplier, "wall multiplier")
                        .unwrap();
                assert_eq!(
                    matching[0]["slow-timeout"]["period"].as_str(),
                    Some(format!("{wall_seconds}s").as_str())
                );
                assert_eq!(
                    matching[0]["slow-timeout"]["terminate-after"].as_integer(),
                    Some(1)
                );
                assert_eq!(
                    matching[0]["slow-timeout"]["grace-period"].as_str(),
                    Some("2s")
                );
                assert!(wall_seconds > cpu_usec / 1_000_000 + 2);
            }
        }
        // Every identity retains prepared admission and literal spelling.
        for identity in &identities {
            for field in 0..3 {
                for prefix in [true, false] {
                    let mut neighbor = identity.clone();
                    let target = match field {
                        0 => &mut neighbor.package,
                        1 => &mut neighbor.binary,
                        _ => &mut neighbor.test,
                    };
                    *target = if prefix {
                        format!("prefix_{target}")
                    } else {
                        format!("{target}_extra")
                    };
                    assert!(
                        prepared_selected_cpu_allowances(true, || Ok(serde_json::to_vec(
                            &measured_inventory(&[neighbor])
                        )
                        .unwrap()))
                        .unwrap()
                        .is_empty()
                    );
                }
            }
            for state in 0..3 {
                let mut inventory = measured_inventory(std::slice::from_ref(identity));
                let case =
                    &mut inventory["rust-suites"][&identity.binary]["testcases"][&identity.test];
                match state {
                    0 => case["ignored"] = true.into(),
                    1 => case["filter-match"]["status"] = "mismatch".into(),
                    _ => {}
                }
                assert!(
                    prepared_selected_cpu_allowances(state != 2, || Ok(serde_json::to_vec(
                        &inventory
                    )
                    .unwrap()))
                    .unwrap()
                    .is_empty()
                );
            }
        }
    }

    #[test]
    fn constructor_wall_rule_precedes_matching_overrides_and_keeps_unrelated_rules() {
        let canonical = include_str!("../.config/nextest.toml");
        assert!(regular_config_compatible(
            &canonical.parse::<DocumentMut>().unwrap()
        ));
        let source = format!(
            "{canonical}\n[[profile.default.overrides]]\nfilter = \"all()\"\n\
             slow-timeout = {{ period = \"60s\", terminate-after = 1 }}\n\
             [[profile.ci.overrides]]\nfilter = \"all()\"\n\
             slow-timeout = {{ period = \"90s\", terminate-after = 1 }}\n"
        );
        let rendered = scaled_config_with_allowances(
            &source,
            1.25,
            Some((Path::new("/fixture/wrapper"), 1.0)),
            &[MEASURED_CPU_ALLOWANCES[1]],
        )
        .unwrap();
        let document = rendered.parse::<DocumentMut>().unwrap();
        for (profile, old_period, old_index) in [("default", "75s", 2), ("ci", "113s", 1)] {
            let entries = document["profile"][profile]["overrides"]
                .as_array_of_tables()
                .unwrap();
            assert_eq!(entries.len(), old_index + 1);
            assert_eq!(
                entries.get(0).unwrap()["filter"].as_str(),
                Some(
                    "package(=hermit-run) & binary_id(=hermit-run::liteinst_stacks) & test(=cold_constructor::genuine_detcore_constructor_uses_retained_disjoint_bootstrap_stack)"
                )
            );
            assert_eq!(
                entries.get(0).unwrap()["slow-timeout"]["period"].as_str(),
                Some("207s")
            );
            let previous = entries.get(old_index).unwrap();
            assert_eq!(previous["filter"].as_str(), Some("all()"));
            assert_eq!(
                previous["slow-timeout"]["period"].as_str(),
                Some(old_period)
            );
        }
        let group = document["profile"]["default"]["overrides"]
            .as_array_of_tables()
            .unwrap()
            .get(1)
            .unwrap();
        assert_eq!(group["filter"].as_str(), Some(REGULAR_GROUP_OVERRIDES[0].0));
        assert_eq!(group["test-group"].as_str(), Some("cli-gdbserver-port"));
        assert_eq!(
            wrapper_argv(&document, DETCORE_CONSTRUCTOR_WRAPPER_NAME)[2],
            "55000000"
        );
    }

    #[test]
    fn owned_wall_rules_require_finite_defaults_and_separate_actual_cpu_bounds() {
        let selected = [MEASURED_CPU_ALLOWANCES[1]];
        let wrapper = Some((Path::new("/fixture/wrapper"), 1.0));
        for source in [
            "[profile.default]\n",
            "[profile.default]\nslow-timeout = { period = \"57s\" }\n",
            "[[profile.ci.overrides]]\nfilter = \"all()\"\nslow-timeout = { period = \"200s\", terminate-after = 1 }\n",
        ] {
            assert!(scaled_config_with_allowances(source, 1.0, wrapper, &selected).is_err());
        }
        let source =
            "[profile.default]\nslow-timeout = { period = \"200s\", terminate-after = 1 }\n";
        // The actual constructor wall is 165*.34 -> 57, exactly CPU55+2:
        // equality refuses, even though the original default200*.34 is ample.
        assert!(
            scaled_config_with_allowances(source, 0.34, wrapper, &selected)
                .unwrap_err()
                .contains("(55s)")
        );
        let rendered = scaled_config_with_allowances(source, 0.35, wrapper, &selected).unwrap();
        let document = rendered.parse::<DocumentMut>().unwrap();
        assert_eq!(
            document["profile"]["default"]["overrides"]
                .as_array_of_tables()
                .unwrap()
                .get(0)
                .unwrap()["slow-timeout"]["period"]
                .as_str(),
            Some("58s")
        );
        // An owned wall never hides an ordinary/default collision.
        assert!(
            scaled_config_with_allowances(&source.replace("200s", "24s"), 1.0, wrapper, &selected)
                .is_err()
        );
        assert!(
            scaled_config_with_allowances(&source.replace("200s", "25s"), 1.0, wrapper, &selected)
                .is_ok()
        );
        // Two owned tests do not inherit each other's CPU maxima: a 66s
        // ordinary stack backstop may coexist with constructor CPU55/wall165.
        assert!(
            scaled_config_with_allowances(
                source,
                1.0,
                wrapper,
                &[MEASURED_CPU_ALLOWANCES[1], MEASURED_CPU_ALLOWANCES[5]]
            )
            .is_ok()
        );
        assert_eq!(
            (
                DETCORE_CONSTRUCTOR_CPU_SECONDS,
                DETCORE_CONSTRUCTOR_WALL_SECONDS
            ),
            (55, 165)
        );
    }

    #[test]
    fn unoverridden_allowances_keep_strict_cpu_cleanup_separation_from_every_finite_wall() {
        let source =
            "[profile.default]\nslow-timeout = { period = \"57s\", terminate-after = 1 }\n";
        // These allocator/procfs assertions and boundaries are unchanged.
        for (allowance, seconds, cutoff, first_pass, collision, separated, period) in [
            (MEASURED_CPU_ALLOWANCES[0], 32, 34, 35, 0.7, 0.8, "46s"),
            (MEASURED_CPU_ALLOWANCES[2], 33, 35, 36, 0.75, 0.8, "46s"),
            (MEASURED_CPU_ALLOWANCES[3], 36, 38, 39, 0.8, 0.85, "49s"),
        ] {
            assert_eq!(allowance.cpu_seconds, seconds);
            assert!(allowance.wall_seconds.is_none());
            for source in [
                source.replace("57s", &format!("{cutoff}s")),
                format!(
                    "{source}[[profile.ci.overrides]]\nfilter = \"test(short)\"\nslow-timeout = {{ period = \"{cutoff}s\", terminate-after = 1 }}\n"
                ),
            ] {
                assert!(
                    scaled_config_with_allowances(
                        &source,
                        1.0,
                        Some((Path::new("/fixture/wrapper"), 1.0)),
                        &[allowance]
                    )
                    .unwrap_err()
                    .contains(&format!("({seconds}s)"))
                );
            }
            assert!(
                scaled_config_with_allowances(
                    &source.replace("57s", &format!("{first_pass}s")),
                    1.0,
                    Some((Path::new("/fixture/wrapper"), 1.0)),
                    &[allowance]
                )
                .is_ok()
            );
            assert!(
                scaled_config_with_allowances(
                    source,
                    collision,
                    Some((Path::new("/fixture/wrapper"), 1.25)),
                    &[allowance]
                )
                .is_err()
            );
            assert!(
                scaled_config_with_allowances(
                    source,
                    separated,
                    Some((Path::new("/fixture/wrapper"), 1.25)),
                    &[allowance]
                )
                .unwrap()
                .contains(&format!("period = \"{period}\""))
            );
            for cpu in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::MAX, 1e12, 2.0] {
                assert!(
                    scaled_config_with_allowances(
                        source,
                        1.0,
                        Some((Path::new("/fixture/wrapper"), cpu)),
                        &[allowance]
                    )
                    .is_err()
                );
            }
        }
        // The largest non-owned cap remains36 regardless of the stack rows.
        let mut reversed = MEASURED_CPU_ALLOWANCES.to_vec();
        reversed.reverse();
        for selected in [MEASURED_CPU_ALLOWANCES.as_slice(), reversed.as_slice()] {
            assert!(
                scaled_config_with_allowances(
                    &source.replace("57s", "38s"),
                    1.0,
                    Some((Path::new("/fixture/wrapper"), 1.0)),
                    selected
                )
                .unwrap_err()
                .contains("(36s)")
            );
            assert!(
                scaled_config_with_allowances(
                    &source.replace("57s", "39s"),
                    1.0,
                    Some((Path::new("/fixture/wrapper"), 1.0)),
                    selected
                )
                .is_ok()
            );
        }
        assert_eq!(
            DETCORE_CONSTRUCTOR_CPU_SECONDS,
            timeouts::cpu_bound_from_p90_usec(36_113_714)
        );
        assert_eq!(
            DETCORE_STACK_CPU_SECONDS,
            timeouts::cpu_bound_from_p90_usec(33_721_186)
        );
    }

    #[test]
    fn only_the_prepared_exact_selected_allocator_identity_gets_the_measured_budget() {
        let target = detcore_allocator_identity();
        let mut identities = vec![target.clone()];
        for (field, replacement) in [
            (0, "other"),
            (1, "other::liteinst_allocator"),
            (1, "hermit-run::liteinst_allocator_extra"),
            (1, "prefixhermit-run::liteinst_allocator"),
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
        let exact = "package(=hermit-run) & binary_id(=hermit-run::liteinst_allocator) & test(=genuine_detcore_leaf_keeps_scope_free_work_private)";
        let path = "/tmp/a path/it's-$(literal)-wrapper";
        for (multiplier, ordinary, detcore) in [
            (1.0, "22000000", "32000000"),
            (1.25, "28000000", "40000000"),
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
        // The ordinary 22+2 fits 34, but admitted 32+2 must be strictly below it.
        let short = source.replace("57s", "34s");
        assert!(scaled_config(&short, 1.0, Some((Path::new("/fixture/wrapper"), 1.0))).is_ok());
        assert!(
            scaled_config_with_allocator(
                &short,
                1.0,
                Some((Path::new("/fixture/wrapper"), 1.0)),
                true
            )
            .unwrap_err()
            .contains("32s")
        );
        let shorter_override = format!(
            "{source}[[profile.ci.overrides]]\nfilter = \"test(short)\"\nslow-timeout = {{ period = \"34s\", terminate-after = 1 }}\n"
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
        // CPU scaling: 32*1.25 -> 40; wall rounds 57*.7 -> 40, below 40+2.
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
                RENDERED_PROCFS_WRAPPER_NAME,
                DETCORE_STACK_WRAPPER_NAME,
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
            timeouts::cpu_bound_from_p90_usec(20_754_169)
        );
    }
}
