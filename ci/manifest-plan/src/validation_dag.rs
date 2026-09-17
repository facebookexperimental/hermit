// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! Refresh and audit the generated partition of the committed validation DAG.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use dagrun::io::dag_from_json;
use dagrun::io::dag_to_json;
use dagrun::model::DagConfig;
use dagrun::model::DagManifest;
use dagrun::model::ResultManifest;
use dagrun::model::Step;
use dagrun::model::result_manifest_owner;
use dagrun::select_steps_by_labels;
use serde::Deserialize;

use crate::runner::E2E_KERNEL_VERSION_ENV;
use crate::runner::E2E_MACHINE_SHORTNAME_ENV;
use crate::validation_dag_static::NEXTEST_EXPECTED_COUNTS;
use crate::validation_dag_static::StructuredResultProducerKind;

pub const OUTPUT: &str = "ci/dag/validate.json";
const EXPECTED_PLAN: &str = "ci/expected-e2e-plan.json";
const SUPER_REPETITIONS: &str = "20";
const PINNED_ROOT_FETCH_TAG: &str = "setup.pinned_root_fetch";
const PINNED_ROOT_FETCH_COMMAND: &str = "seed=(); if [ -n \"${CARGO_HOME:-}\" ]; then seed=(--seed-cargo \"$CARGO_HOME\"); fi; ./ci/hermetic/run-split-validate.sh --fetch-only \"${seed[@]}\"";
const PIN_GATE_COMMAND: &str = r#"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; with-proxy ./ci/run-reverie-pin-check.sh --repo "$PWD""#;
const OUTCOME_CONSUMERS_COMMAND: &str = r#"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/check-outcome-consumers-node.sh"#;
const PINNED_ROOT_TWIN_SUFFIX: &str = "_in_pinned_root";
pub const HOSTED_PORTABLE_LABEL: &str = "hosted-portable";
const HOSTED_PRIVILEGED_LABEL: &str = "hosted-privileged";
const HOSTED_VARIANT_SUFFIX: &str = "_on_host";
const HOSTED_RESOURCE_TUPLES: [(&str, &str, i64, i64); 13] = [
    ("e2e.manifest_applications", "manifest_guest", 1, 8),
    ("e2e.manifest_backend_parity_c", "manifest_guest", 8, 8),
    ("e2e.manifest_bin_c", "manifest_guest", 1, 8),
    ("e2e.manifest_c_programs", "manifest_guest", 8, 8),
    ("e2e.manifest_chaos_c", "manifest_guest", 1, 8),
    ("e2e.manifest_data_handling", "manifest_guest", 1, 8),
    ("e2e.manifest_debugger_c", "manifest_guest", 1, 8),
    ("e2e.manifest_determinism_stress", "manifest_guest", 1, 8),
    ("e2e.manifest_determinism_stress_c", "manifest_guest", 1, 8),
    ("e2e.manifest_language_runtimes", "manifest_guest", 1, 8),
    ("e2e.manifest_shared_futex_c", "manifest_guest", 1, 8),
    ("e2e.manifest_system_utils", "manifest_guest", 1, 8),
    ("e2e.manifest_util_c", "manifest_guest", 1, 8),
];
const PINNED_ROOT_PRODUCER_STEPS: &[&str] = &[
    "build.rust_scripts",
    "setup.manifest_plan",
    "build.workspace",
    "build.runtime_release",
    "build.e2e_artifact",
    "build.manifest_guests",
    "build.liteinst_runtime_release",
    "compatprep.hermit_release",
];
// Explicit execution destinations; hosted variants retain their original host commands.
const PINNED_ROOT_EXECUTION_STEPS: &[&str] = &[
    "test.regular_crates",
    "test.hermit_unit",
    "test.detcore_unit",
    "test.detcore_misc",
    "test.detcore_parallel",
    "test.hermit_integration",
    "test.arbitrary_binaries",
    "test.cli",
    "test.isolated_dbt_workdir",
    "test.isolated_detcore_workdir",
    "test.liteinst_strict",
    "test.sabre_examples",
    "test.hermit_modes",
    "test.app_strict_verify",
    "test.command_strict_verify",
    "test.ignored_syscall_regressions",
    "test.rr_suite_contract",
    "test.dbt_parity",
    "test.envelope_levels",
    "test.applications_e2e",
    "liteinst.strict",
    "liteinst.hermit_release",
    "liteinst.runtime",
    "quick.build",
    "quick.detcore_unit",
    "quick.run_smoke",
    "quick.verify_smoke",
    "quick.record_replay_smoke",
    "privileged-build.privileged_tests",
    "privileged-cpuid.faulting",
    "privileged-pmu.preemption",
    "privileged-test.pmu_buck_chaos_cases",
    "privileged-test.cli_kvm",
    "privileged-only-cpuid.faulting",
    "privileged-only-pmu.preemption",
    "privileged-only-test.pmu_buck_chaos_cases",
    "privileged-only-test.cli_kvm",
];

const PINNED_ROOT_FORWARDED_ENV: &[&str] = &[
    "CI",
    "CARGO_BUILD_JOBS",
    "DAGRUN_STEP_STARTED_MONOTONIC_NS",
    "E2E_BUILD_ROOT",
    E2E_KERNEL_VERSION_ENV,
    E2E_MACHINE_SHORTNAME_ENV,
    "E2E_RESULT_ROOT",
    "E2E_RUN_ID",
    "HERMIT_E2E_EMPTY_WORKDIR",
    crate::timeouts::TEST_CPU_TIMEOUT_MULTIPLIER_ENV,
    crate::timeouts::TEST_WALL_TIMEOUT_MULTIPLIER_ENV,
    "HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT",
    "L4_REPS",
    "NEXTEST_TEST_THREADS",
    "PR_NUMBER",
    "SUPER_REPETITIONS",
    "THIRD_PARTY_BUILD_JOBS",
    "VALIDATE_VERBOSITY",
    "VALIDATE_RUN_STATE",
];
#[derive(Clone, Copy)]
struct Profile {
    label: &'static str,
    direct_steps: usize,
    selected_steps: usize,
}

const PROFILES: [Profile; 7] = [
    Profile {
        label: "full",
        direct_steps: 269,
        selected_steps: 270,
    },
    Profile {
        label: "portable",
        direct_steps: 260,
        selected_steps: 261,
    },
    Profile {
        label: "quick",
        direct_steps: 15,
        selected_steps: 16,
    },
    Profile {
        label: "super",
        direct_steps: 145,
        selected_steps: 146,
    },
    Profile {
        label: "privileged",
        direct_steps: 11,
        selected_steps: 19,
    },
    Profile {
        label: HOSTED_PORTABLE_LABEL,
        direct_steps: 251,
        selected_steps: 251,
    },
    Profile {
        label: HOSTED_PRIVILEGED_LABEL,
        direct_steps: 12,
        selected_steps: 12,
    },
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectedPlan {
    schema: u64,
    cells: Vec<ExpectedCell>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectedCell {
    lane: String,
    category: String,
    test: String,
    mode: String,
    backend: String,
    #[serde(default)]
    requires_host_capabilities: Vec<String>,
}

impl From<ExpectedCell> for DagManifest {
    fn from(cell: ExpectedCell) -> Self {
        Self {
            lane: cell.lane,
            category: cell.category,
            test: Some(cell.test),
            mode: Some(cell.mode),
            backend: Some(cell.backend),
        }
    }
}

struct Scratch(PathBuf);

impl Scratch {
    fn create() -> Result<Self, String> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| format!("clock is before Unix epoch: {error}"))?
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hermit-generate-validation-dag-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&path).map_err(|error| {
            format!(
                "cannot create scratch directory {}: {error}",
                path.display()
            )
        })?;
        Ok(Self(path))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn repo_root() -> Result<PathBuf, String> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|error| format!("cannot run git rev-parse: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git rev-parse failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim(),
    ))
}

fn generated_plan(root: &Path, scratch: &Path) -> Result<DagConfig, String> {
    let path = scratch.join("generated.json");
    let mut command = Command::new(root.join("scripts/validate.rs"));
    command
        .current_dir(root)
        .arg("--write-generated-plan")
        .arg(&path);
    command
        .env(
            "HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT",
            "cpuid-faulting,kvm",
        )
        .env("SUPER_REPETITIONS", SUPER_REPETITIONS)
        .env("VALIDATE_VERBOSITY", "1");
    for name in [
        "VALIDATE_LEVEL",
        "VALIDATE_FORCE_FULL",
        "VALIDATE_GATE_TIMEOUT_SECONDS",
        "VALIDATE_GATE_CPU_TIMEOUT_SECONDS",
        "HERMIT_VALIDATE_RUN_TIMEOUT_SECONDS",
        "DAGRUN_CPU_TIMEOUT_MULTIPLIER",
        "DAGRUN_CPU_TIMEOUT_PLATFORM",
        "VALIDATE_RUN_STATE",
    ] {
        command.env_remove(name);
    }
    let output = command
        .output()
        .map_err(|error| format!("cannot run scripts/validate.rs for generated nodes: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "generated-partition export failed with {}:\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("cannot read generated {}: {error}", path.display()))?;
    dag_from_json(&text).map_err(|error| format!("invalid generated {}: {error}", path.display()))
}

fn expected_cells(root: &Path) -> Result<Vec<DagManifest>, String> {
    let path = root.join(EXPECTED_PLAN);
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    expected_cells_from_json(&text).map_err(|error| format!("invalid {}: {error}", path.display()))
}

/// Decode the same source-owned expected population for generation and retained
/// plan verification. Duplicates are refused before constructing any set.
pub fn expected_cells_from_json(text: &str) -> Result<Vec<DagManifest>, String> {
    let plan: ExpectedPlan = serde_json::from_str(text)
        .map_err(|error| format!("invalid expected E2E plan: {error}"))?;
    if plan.schema != 1 {
        return Err("expected E2E plan schema must be 1".into());
    }
    let mut seen = BTreeSet::new();
    let mut cells = Vec::new();
    for cell in plan.cells {
        if [
            &cell.lane,
            &cell.category,
            &cell.test,
            &cell.mode,
            &cell.backend,
        ]
        .iter()
        .any(|field| field.trim().is_empty())
            || cell
                .requires_host_capabilities
                .iter()
                .any(|field| field.trim().is_empty())
        {
            return Err("expected E2E plan contains an empty identity or capability".into());
        }
        let cell: DagManifest = cell.into();
        if !seen.insert(result_identity(&cell)) {
            return Err("expected E2E plan contains a duplicate cell identity".into());
        }
        cells.push(cell);
    }
    Ok(cells)
}

fn normalize_step(step: &mut Step, root: &Path, run_state: &Path) -> Result<(), String> {
    let root = root
        .to_str()
        .ok_or_else(|| "repository root is not valid UTF-8".to_string())?;
    let run_state = run_state
        .to_str()
        .ok_or_else(|| "generator scratch path is not valid UTF-8".to_string())?;
    step.result_manifests = Some(
        step.result_manifests
            .take()
            .unwrap_or_default()
            .into_iter()
            .filter(|manifest| matches!(manifest, ResultManifest::StructuredTestResults(_)))
            .collect(),
    );
    step.cmd = step
        .cmd
        .replace(run_state, "$VALIDATE_RUN_STATE")
        .replace(root, "$PWD");
    step.desc = step
        .desc
        .replace(run_state, "$VALIDATE_RUN_STATE")
        .replace(root, "$PWD");
    step.description = step
        .description
        .replace(run_state, "$VALIDATE_RUN_STATE")
        .replace(root, "$PWD");
    for value in step.env.values_mut() {
        *value = value
            .replace(run_state, "$VALIDATE_RUN_STATE")
            .replace(root, "$PWD");
    }
    if step.timeout <= 0 || step.cpu_timeout <= 0 {
        return Err(format!(
            "{} does not carry explicit wall/CPU budgets: wall={} cpu={}",
            step.tag(),
            step.timeout,
            step.cpu_timeout
        ));
    }
    if step.hint.rss_baseline_bytes.is_none() && step.hint.hard_mem_max_bytes.is_none() {
        return Err(format!(
            "{} does not carry an explicit memory budget",
            step.tag()
        ));
    }
    Ok(())
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"@%+=:,./-_".contains(&byte))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', r"'\''"))
}

fn pinned_root_twin_tag(tag: &str) -> String {
    format!("{tag}{PINNED_ROOT_TWIN_SUFFIX}")
}

fn is_manifest_run(step: &Step) -> bool {
    step.manifest.is_some() || (step.group == "quick" && step.job == "e2e_verify")
}

fn is_hosted_variant(step: &Step) -> bool {
    step.job.ends_with(HOSTED_VARIANT_SUFFIX)
}

fn hosted_resource_tuples(cfg: &DagConfig) -> Result<Vec<(String, String, i64, i64)>, String> {
    let selected = select_steps_by_labels(cfg, &[HOSTED_PORTABLE_LABEL.to_string()])?;
    let mut tuples = Vec::new();
    for step in &selected.steps {
        let tag = step.tag();
        let tag = tag
            .strip_suffix(HOSTED_VARIANT_SUFFIX)
            .unwrap_or(&tag)
            .to_string();
        for (resource, demand) in &step.hint.resources {
            let capacity = selected
                .resource_caps
                .get(resource)
                .copied()
                .ok_or_else(|| {
                    format!(
                        "{HOSTED_PORTABLE_LABEL} step {} demands undeclared resource {resource}",
                        step.tag()
                    )
                })?;
            tuples.push((tag.clone(), resource.clone(), *demand, capacity));
        }
    }
    tuples.sort();
    Ok(tuples)
}

fn is_pinned_root_producer(step: &Step) -> bool {
    PINNED_ROOT_PRODUCER_STEPS.contains(&step.tag().as_str())
        || step.job == "manifest_guests"
        || (step.job == "privileged_tests"
            && step.cmd.contains("cargo ")
            && step.cmd.contains("publish-hermit-e2e-artifact.sh"))
}

fn runs_in_pinned_root(step: &Step) -> bool {
    !is_hosted_variant(step)
        && (is_manifest_run(step)
            || PINNED_ROOT_EXECUTION_STEPS.contains(&step.tag().as_str())
            || matches!(step.group.as_str(), "portablecompat" | "portablecompatprep"))
}

// Dagrun appends admitted argv after the complete wrapper command. Re-quote
// each resulting argument before appending it to the original shell payload;
// preserve literal bytes and the original command's argument placement.
pub const PINNED_ROOT_COMMAND_GUARD: &str = r#"/src/ci/hermetic/assert-no-network.sh && /src/ci/hermetic/assert-build-dependencies.sh && hermit_payload=$1 && shift && if [ "$#" -gt 0 ]; then printf -v hermit_extra ' %q' "$@"; hermit_payload+=$hermit_extra; fi && exec bash -c "$hermit_payload""#;
pub(super) const LEGACY_PINNED_ROOT_COMMAND_GUARD: &str = r#"/src/ci/hermetic/assert-no-network.sh && /src/ci/hermetic/assert-build-dependencies.sh && exec bash -c "$1""#;

fn pinned_root_command(step: &Step) -> String {
    let mut env_names = PINNED_ROOT_FORWARDED_ENV
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    if is_manifest_run(step)
        || step
            .structured_test_results_manifest()
            .ok()
            .flatten()
            .is_some()
    {
        env_names.insert("DAGRUN_TEST_COUNTS_PATH");
    }
    env_names.extend(step.env.keys().map(String::as_str));
    let mut argv = vec![
        "./ci/hermetic/run-in-pinned-root.sh".to_string(),
        "--src".into(),
        ".".into(),
        "--out".into(),
        "ignored/hermetic/split".into(),
        "--src-rw".into(),
        "--cargo-home".into(),
        "ignored/hermetic/split/cargo".into(),
    ];
    // Proc-locks snapshots include OFD locks from other PID namespaces. Share
    // the native host lease inode, not one file per container or validation.
    if step.tag() == "test.hermit_integration" {
        argv.push("--proc-locks-runtime".into());
    }
    for name in env_names {
        argv.extend(["--env".into(), name.into()]);
    }
    argv.extend([
        "--".into(),
        "bash".into(),
        "-c".into(),
        PINNED_ROOT_COMMAND_GUARD.into(),
        "bash".into(),
        step.cmd.clone(),
    ]);
    argv.iter()
        .map(|argument| shell_quote(argument))
        .collect::<Vec<_>>()
        .join(" ")
}

// The authored source already contains wrapped manifest commands. Keep their
// command payload unchanged while carrying the current environment policy into
// the outer wrapper; otherwise only newly cloned producers see added settings.
pub(crate) fn refresh_pinned_root_environment(tag: &str, command: &str) -> Result<String, String> {
    let (header, payload) = command
        .split_once(" -- bash -c ")
        .ok_or_else(|| format!("{tag} has an unrecognized pinned-root command boundary"))?;
    let words = header.split_whitespace().collect::<Vec<_>>();
    let mut refreshed = header.to_owned();
    let lease_options = words
        .iter()
        .filter(|word| **word == "--proc-locks-runtime")
        .count();
    match (tag == "test.hermit_integration", lease_options) {
        (true, 0) => refreshed.push_str(" --proc-locks-runtime"),
        (true, 1) | (false, 0) => {}
        _ => return Err(format!("{tag} has an unexpected proc-locks runtime option")),
    }
    for name in PINNED_ROOT_FORWARDED_ENV {
        let count = words
            .windows(2)
            .filter(|pair| pair[0] == "--env" && pair[1] == *name)
            .count();
        match count {
            0 => refreshed.push_str(&format!(" --env {name}")),
            1 => {}
            _ => {
                return Err(format!(
                    "{tag} forwards pinned-root environment name {name} more than once"
                ));
            }
        }
    }
    let legacy = format!("{} bash ", shell_quote(LEGACY_PINNED_ROOT_COMMAND_GUARD));
    let current = format!("{} bash ", shell_quote(PINNED_ROOT_COMMAND_GUARD));
    let payload = if let Some(command) = payload.strip_prefix(&legacy) {
        format!("{current}{command}")
    } else if payload.starts_with(&current) {
        payload.to_owned()
    } else {
        return Err(format!("{tag} has an unrecognized pinned-root argv guard"));
    };
    Ok(format!("{refreshed} -- bash -c {payload}"))
}

fn pinned_root_fetch() -> Result<Step, String> {
    let text = format!(
        r#"{{"description":"Pinned-root fetch node","steps":[{{"group":"setup","job":"pinned_root_fetch","desc":"Fetch locked Cargo inputs","description":"Fetch locked Cargo inputs before network-disabled pinned-root commands.","cmd":{},"deps":[],"env":{{"VALIDATE_VERBOSITY":"1"}},"labels":[],"result_manifests":[],"timeout":600,"cpu_timeout":600,"hint":{{"rss_baseline_bytes":1073741824,"hard_mem_max_bytes":1073741824}},"fail_fast_family":"setup.pinned_root_fetch"}}]}}"#,
        serde_json::to_string(PINNED_ROOT_FETCH_COMMAND).expect("constant is serializable")
    );
    let mut step = dag_from_json(&text)
        .map_err(|error| format!("internal pinned-root fetch node is invalid: {error}"))?
        .steps
        .into_iter()
        .next()
        .ok_or_else(|| "internal pinned-root fetch node disappeared".to_string())?;
    step.deps = vec!["pre.reverie_pin".into()];
    Ok(step)
}

/// Add the hosted label to the corpus-derived portable compatibility rows.
///
/// Authored hosted steps and host-only variants come from the independent typed
/// source. The corpus rows are regenerated, so their label is derived here from
/// their typed generated partition rather than copied from the output artifact.
fn materialize_hosted_portable_selection(cfg: &mut DagConfig) {
    for step in &mut cfg.steps {
        if generated_partition(step) == Some(GeneratedPartition::PortableCompat)
            && !step
                .labels
                .iter()
                .any(|label| label == HOSTED_PORTABLE_LABEL)
        {
            step.labels.push(HOSTED_PORTABLE_LABEL.into());
            step.labels.sort();
            step.labels.dedup();
        }
    }
}

/// Separate the hosted dependency closure without dropping fixture success checks.
/// Generated compatibility rows keep their commands and run-state paths; only
/// their hosted identity and dependencies change. Runtime consumes these nodes.
fn materialize_hosted_test_variants(cfg: &mut DagConfig) -> Result<(), String> {
    let mut split = cfg
        .steps
        .iter()
        .filter(|step| {
            runs_in_pinned_root(step)
                && step
                    .labels
                    .iter()
                    .any(|label| label == HOSTED_PORTABLE_LABEL)
        })
        .map(Step::tag)
        .collect::<BTreeSet<_>>();
    if split.len() != 16 {
        return Err(format!(
            "hosted test split has {} roots, expected 16",
            split.len()
        ));
    }
    loop {
        let previous = split.len();
        for step in &cfg.steps {
            if step
                .labels
                .iter()
                .any(|label| label == HOSTED_PORTABLE_LABEL)
                && step
                    .labels
                    .iter()
                    .any(|label| label != HOSTED_PORTABLE_LABEL)
                && step
                    .deps
                    .iter()
                    .any(|dependency| split.contains(dependency))
            {
                split.insert(step.tag());
            }
        }
        if split.len() == previous {
            break;
        }
    }
    if split.len() != 206 {
        return Err(format!(
            "hosted test dependency closure has {} nodes, expected 206",
            split.len()
        ));
    }
    let mut variants = Vec::new();
    for step in &mut cfg.steps {
        if split.contains(&step.tag()) {
            let mut hosted = step.clone();
            hosted.job.push_str(HOSTED_VARIANT_SUFFIX);
            hosted.labels = vec![HOSTED_PORTABLE_LABEL.into()];
            hosted.fail_fast_family = Some(hosted.tag());
            let owner = hosted.tag();
            for result in hosted.result_manifests.iter_mut().flatten() {
                if let ResultManifest::StructuredTestResults(result) = result {
                    result.owner = owner.clone();
                }
            }
            step.labels.retain(|label| label != HOSTED_PORTABLE_LABEL);
            variants.push(hosted);
        }
    }
    cfg.steps.extend(variants);
    for step in &mut cfg.steps {
        if step.labels == [HOSTED_PORTABLE_LABEL] {
            for dependency in &mut step.deps {
                if split.contains(dependency) {
                    dependency.push_str(HOSTED_VARIANT_SUFFIX);
                }
            }
        }
    }
    Ok(())
}

/// Materialize the pinned-root execution split as ordinary committed nodes.
///
/// This transform belongs to maintenance-time generation. Runtime validation
/// sends the selected committed graph to dagrun without cloning producers or
/// rewriting commands/dependencies. Existing twins are replaced from their
/// host-side producers. Already-wrapped manifest commands retain their authored
/// inner commands while their outer environment forwarding follows this policy.
fn materialize_pinned_root(cfg: &mut DagConfig) -> Result<(), String> {
    cfg.steps.retain(|step| {
        step.tag() != PINNED_ROOT_FETCH_TAG && !step.job.ends_with(PINNED_ROOT_TWIN_SUFFIX)
    });

    let producers = cfg
        .steps
        .iter()
        .filter(|step| is_pinned_root_producer(step))
        .cloned()
        .collect::<Vec<_>>();
    let producer_tags = producers.iter().map(Step::tag).collect::<BTreeSet<_>>();
    let has_rust_scripts = producer_tags.contains("build.rust_scripts");

    for step in &mut cfg.steps {
        if is_hosted_variant(step) {
            continue;
        }
        if !runs_in_pinned_root(step) {
            continue;
        }
        if step.tag() == "test.envelope_levels" {
            let previous =
                "ARGS='run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled'";
            if step.cmd.matches(previous).count() != 1 {
                return Err(
                    "working-envelope command lost its exact guest argument boundary".into(),
                );
            }
            step.cmd = step.cmd.replace(previous, "ARGS='run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --mount=type=tmpfs,target=/test --workdir=/test'");
        }
        step.env
            .insert("HERMIT_E2E_EMPTY_WORKDIR".into(), "/test".into());
        step.deps = step
            .deps
            .iter()
            .map(|dependency| {
                if producer_tags.contains(dependency) {
                    pinned_root_twin_tag(dependency)
                } else {
                    dependency.clone()
                }
            })
            .collect();
        // The image supplies Nextest. Test execution consumes the metadata
        // prepared in that same root, not a host installation or Cargo cache.
        step.deps.retain(|dependency| dependency != "setup.nextest");
        if step.group == "privileged-e2e"
            && producer_tags.contains("build.e2e_artifact")
            && !step
                .deps
                .iter()
                .any(|dependency| dependency == "build.e2e_artifact_in_pinned_root")
        {
            step.deps.push("build.e2e_artifact_in_pinned_root".into());
        }
        if has_rust_scripts
            && !step
                .deps
                .iter()
                .any(|dependency| dependency == "build.rust_scripts_in_pinned_root")
        {
            step.deps.push("build.rust_scripts_in_pinned_root".into());
        }
        if !step
            .deps
            .iter()
            .any(|dependency| dependency == PINNED_ROOT_FETCH_TAG)
        {
            step.deps.push(PINNED_ROOT_FETCH_TAG.into());
        }
        step.deps.sort();
        step.deps.dedup();
        if !step.cmd.starts_with("./ci/hermetic/run-in-pinned-root.sh ") {
            step.cmd = pinned_root_command(step);
        } else {
            step.cmd = refresh_pinned_root_environment(&step.tag(), &step.cmd)?;
        }
    }

    let mut twins = Vec::with_capacity(producers.len());
    for producer in &producers {
        let mut twin = producer.clone();
        twin.job.push_str(PINNED_ROOT_TWIN_SUFFIX);
        twin.labels.retain(|label| label != HOSTED_PORTABLE_LABEL);
        if producer.tag() == "compatprep.hermit_release" {
            twin.labels
                .retain(|label| label == "portable-strict-compat-only");
        }
        twin.deps = producer
            .deps
            .iter()
            .filter(|dependency| producer_tags.contains(*dependency))
            .map(|dependency| pinned_root_twin_tag(dependency))
            .collect();
        if producer.tag() != "build.rust_scripts" && has_rust_scripts {
            twin.deps.push("build.rust_scripts_in_pinned_root".into());
        }
        if producer.job == "manifest_guests" && producer_tags.contains("setup.manifest_plan") {
            twin.deps.push("setup.manifest_plan_in_pinned_root".into());
        }
        if producer.tag() == "compatprep.hermit_release" {
            twin.deps.push("gate.manifest".into());
        }
        twin.deps.push(PINNED_ROOT_FETCH_TAG.into());
        twin.deps.sort();
        twin.deps.dedup();
        twin.env
            .insert("HERMIT_E2E_EMPTY_WORKDIR".into(), "/test".into());
        twin.cmd = pinned_root_command(&twin);
        twins.push(twin);
    }

    for step in &mut cfg.steps {
        if step.tag() == "compatprep.hermit_release" {
            step.labels
                .retain(|label| label != "portable-strict-compat-only");
        }
    }
    cfg.steps.push(pinned_root_fetch()?);
    cfg.steps.extend(twins);
    Ok(())
}

// These six shared ancestors need separate immutable IDs because quick/super
// use the measured 1200-second Rust-script CPU budget, while the other profiles
// keep the established 7200-second cold-build budget. This is generation, not
// a runtime rewrite of the selected graph.
const QUICK_SUPER_VARIANTS: &[&str] = &[
    "build.rust_scripts",
    "build.rust_scripts_in_pinned_root",
    "gate.manifest",
    "setup.manifest_plan",
    "setup.manifest_plan_in_pinned_root",
    "setup.nextest",
];

fn quick_super_variant(tag: &str) -> String {
    format!("quick-super-{tag}")
}

fn materialize_quick_super_budgets(cfg: &mut DagConfig) {
    let is_quick_super = |label: &str| matches!(label, "quick" | "super");
    let mut variants = Vec::new();
    for step in &mut cfg.steps {
        if QUICK_SUPER_VARIANTS.contains(&step.tag().as_str()) {
            let mut variant = step.clone();
            variant.group = format!("quick-super-{}", variant.group);
            variant.labels.retain(|label| is_quick_super(label));
            variant.fail_fast_family = Some(variant.tag());
            if step.group == "build" {
                variant.timeout = crate::validation_dag_static::RUST_SCRIPT_PRODUCER_WALL_SECONDS;
                variant.cpu_timeout =
                    crate::validation_dag_static::RUST_SCRIPT_PRODUCER_QUICK_SUPER_CPU_SECONDS;
                variant.hint.rss_baseline_bytes =
                    Some(crate::validation_dag_static::RUST_SCRIPT_PRODUCER_RSS_BASELINE_BYTES);
                variant.hint.hard_mem_max_bytes =
                    Some(crate::validation_dag_static::RUST_SCRIPT_PRODUCER_HARD_MEM_MAX_BYTES);
                variant.hint.est_duration_s = 0.0;
            }
            step.labels.retain(|label| !is_quick_super(label));
            variants.push(variant);
        }
    }
    cfg.steps.extend(variants);
    for step in &mut cfg.steps {
        if !step.labels.is_empty() && step.labels.iter().all(|label| is_quick_super(label)) {
            for dependency in &mut step.deps {
                if QUICK_SUPER_VARIANTS.contains(&dependency.as_str()) {
                    *dependency = quick_super_variant(dependency);
                }
            }
            step.deps.sort();
            step.deps.dedup();
        }
    }
}

/// Keep direct preflight dependencies on host commands and tests so focused
/// selection cannot drop their source/gate ordering. Pinned preparation retains
/// its original pin/fetch prerequisites and can overlap the manifest audit.
fn materialize_focused_preflight(cfg: &mut DagConfig) -> Result<(), String> {
    for (labels, gate, pin, manifest_producer) in [
        (
            vec![
                "full",
                "portable",
                "quick",
                "super",
                "privileged",
                HOSTED_PORTABLE_LABEL,
            ],
            "gate.manifest",
            "pre.reverie_pin",
            "setup.manifest_plan",
        ),
        (
            vec![HOSTED_PRIVILEGED_LABEL],
            "gate.manifest_on_host",
            "pre.reverie_pin_on_host",
            "setup.manifest_plan_on_host",
        ),
    ] {
        let selected = select_steps_by_labels(
            cfg,
            &labels.into_iter().map(str::to_string).collect::<Vec<_>>(),
        )?;
        let executable_tags = selected
            .steps
            .iter()
            .map(Step::tag)
            .collect::<BTreeSet<_>>();
        let preflight = dagrun::select_steps_by_tags(cfg, &[gate.into()], false)?;
        let gate_ancestors = preflight
            .steps
            .iter()
            .map(Step::tag)
            .collect::<BTreeSet<_>>();
        let pin_preflight = dagrun::select_steps_by_tags(cfg, &[pin.into()], false)?;
        let pin_ancestors = pin_preflight
            .steps
            .iter()
            .map(Step::tag)
            .collect::<BTreeSet<_>>();
        for step in &mut cfg.steps {
            let tag = step.tag();
            if !executable_tags.contains(&tag) {
                continue;
            }
            let pinned_preparation =
                step.job.ends_with(PINNED_ROOT_TWIN_SUFFIX) || tag == PINNED_ROOT_FETCH_TAG;
            if !pinned_preparation && !gate_ancestors.contains(&tag) {
                step.deps.push(gate.into());
            }
            if !pin_ancestors.contains(&tag) && !gate_ancestors.contains(&tag) {
                step.deps.push(pin.into());
            }
            // Manifest commands need their canonical test-harness producer
            // even when focused selection uses other already-built artifacts.
            if is_manifest_run(step)
                || step
                    .job
                    .strip_suffix(HOSTED_VARIANT_SUFFIX)
                    .unwrap_or(&step.job)
                    == "manifest_guests"
            {
                step.deps.push(manifest_producer.into());
                if step.cmd.starts_with("./ci/hermetic/run-in-pinned-root.sh ") {
                    step.deps.push("setup.manifest_plan_in_pinned_root".into());
                }
            }
            step.deps.sort();
            step.deps.dedup();
        }
    }
    Ok(())
}

fn materialize_runtime_policy(cfg: &mut DagConfig) {
    for step in &mut cfg.steps {
        if step.fail_fast_family.is_none() {
            step.fail_fast_family = Some(step.tag());
        }
        // The CLI verbosity is scheduler invocation policy. Leaving a fixed
        // value on every node both duplicates that policy and prevents a
        // caller-selected level from reaching child helpers.
        step.env.remove("VALIDATE_VERBOSITY");
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum GeneratedPartition {
    PortableCompat,
    PortableFocusedCompat,
    StrictCompat,
    SabreCompat,
    E9patchCompat,
    RrCompat,
    SuperCompat,
    SuperStress,
}

fn generated_partition(step: &Step) -> Option<GeneratedPartition> {
    match step.group.as_str() {
        "portablecompat" | "portablecompatprep" => {
            return Some(GeneratedPartition::PortableFocusedCompat);
        }
        "strictcompat" | "strictcompatprep" => return Some(GeneratedPartition::StrictCompat),
        "sabrecompat" | "sabrecompatprep" => return Some(GeneratedPartition::SabreCompat),
        "e9patchcompat" | "e9patchcompatprep" => {
            return Some(GeneratedPartition::E9patchCompat);
        }
        "rrcompat" | "rrcompatprep" => return Some(GeneratedPartition::RrCompat),
        _ => {}
    }
    if step.group == "superstress" {
        return Some(GeneratedPartition::SuperStress);
    }
    if step.tag() == "super-compatprep.fixtures"
        || (step.group == "compat" && step.labels.iter().any(|label| label == "super"))
    {
        return Some(GeneratedPartition::SuperCompat);
    }
    if step.tag() == "compatprep.fixtures"
        || (step.group == "compat"
            && step
                .labels
                .iter()
                .any(|label| matches!(label.as_str(), "portable" | "full")))
    {
        return Some(GeneratedPartition::PortableCompat);
    }
    None
}

fn refresh_generated_partitions(
    mut committed: DagConfig,
    generated: DagConfig,
) -> Result<DagConfig, String> {
    let mut replacements = BTreeMap::<GeneratedPartition, Vec<Step>>::new();
    for step in generated.steps {
        let Some(partition) = generated_partition(&step) else {
            if step.labels == ["generator-dependency-anchor"] || step.tag() == "build.rust_scripts"
            {
                continue;
            }
            return Err(format!(
                "generated-plan exporter emitted static-looking node {}",
                step.tag()
            ));
        };
        replacements.entry(partition).or_default().push(step);
    }
    for (partition, expected) in [
        (GeneratedPartition::PortableCompat, 190usize),
        (GeneratedPartition::PortableFocusedCompat, 190usize),
        (GeneratedPartition::StrictCompat, 194usize),
        (GeneratedPartition::SabreCompat, 213usize),
        (GeneratedPartition::E9patchCompat, 174usize),
        (GeneratedPartition::RrCompat, 140usize),
        (GeneratedPartition::SuperCompat, 5usize),
        (GeneratedPartition::SuperStress, 102usize),
    ] {
        let actual = replacements.get(&partition).map_or(0, Vec::len);
        if actual != expected {
            return Err(format!(
                "generated {partition:?} partition has {actual} nodes, expected {expected}"
            ));
        }
    }

    let mut inserted = BTreeSet::new();
    let mut steps = Vec::with_capacity(committed.steps.len());
    for step in committed.steps {
        let Some(partition) = generated_partition(&step) else {
            steps.push(step);
            continue;
        };
        if inserted.insert(partition) {
            steps.extend(
                replacements
                    .remove(&partition)
                    .expect("validated generated partition exists"),
            );
        }
    }
    // The independent typed source intentionally contains no corpus-derived
    // partition anchors. Append every generated partition exactly once in a
    // stable order after replacing any legacy anchors supplied by a focused
    // test fixture.
    for partition in [
        GeneratedPartition::PortableCompat,
        GeneratedPartition::PortableFocusedCompat,
        GeneratedPartition::StrictCompat,
        GeneratedPartition::SabreCompat,
        GeneratedPartition::E9patchCompat,
        GeneratedPartition::RrCompat,
        GeneratedPartition::SuperCompat,
        GeneratedPartition::SuperStress,
    ] {
        if inserted.insert(partition) {
            steps.extend(
                replacements
                    .remove(&partition)
                    .expect("validated generated partition exists"),
            );
        }
    }
    if inserted.len() != 8 {
        return Err(format!(
            "committed DAG has anchors for {} of 8 generated partitions",
            inserted.len()
        ));
    }
    committed.steps = steps;
    Ok(committed)
}

fn attach_result_ownership(cfg: &mut DagConfig, cells: &[DagManifest]) {
    for step in &mut cfg.steps {
        let structured = step
            .result_manifests
            .take()
            .unwrap_or_default()
            .into_iter()
            .filter(|manifest| matches!(manifest, ResultManifest::StructuredTestResults(_)))
            .collect::<Vec<_>>();
        let mut owned = if let Some(selector) = &step.manifest {
            cells
                .iter()
                .filter(|cell| cell.lane == selector.lane && cell.category == selector.category)
                .cloned()
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        if step.tag() == "quick.e2e_verify" {
            owned.extend(
                cells
                    .iter()
                    .filter(|cell| {
                        cell.lane == "portable"
                            && cell.mode.as_deref() == Some("verify")
                            && cell.backend.as_deref() == Some("ptrace")
                    })
                    .cloned(),
            );
        }
        owned.sort_by_key(result_identity);
        let mut manifests = owned
            .into_iter()
            .map(ResultManifest::ManifestCell)
            .collect::<Vec<_>>();
        manifests.extend(structured);
        step.result_manifests = Some(manifests);
    }
}

fn assert_structured_result_producers(cfg: &DagConfig) -> Result<(), String> {
    let mut expected = BTreeMap::<&str, StructuredResultProducerKind>::new();
    for kind in StructuredResultProducerKind::ALL {
        for tag in kind.tags() {
            if expected.insert(tag, kind).is_some() {
                return Err(format!(
                    "structured result producer registry declares {tag} more than once"
                ));
            }
        }
    }
    if expected.len() != 106 {
        return Err(format!(
            "structured result producer registry has {} entries, expected 106",
            expected.len()
        ));
    }

    let mut expected_counts = NEXTEST_EXPECTED_COUNTS
        .iter()
        .copied()
        .collect::<BTreeMap<_, _>>();
    if expected_counts.len() != 38 {
        return Err(format!(
            "Nextest expected-count registry has {} entries, expected 38",
            expected_counts.len()
        ));
    }

    let mut seen_by_kind = BTreeMap::<StructuredResultProducerKind, usize>::new();
    for step in &cfg.steps {
        let tag = step.tag();
        let command_kinds = StructuredResultProducerKind::ALL
            .into_iter()
            .filter_map(|kind| {
                let occurrences = step.cmd.matches(kind.command_marker()).count();
                (occurrences != 0).then_some((kind, occurrences))
            })
            .collect::<Vec<_>>();
        let command_kind = match command_kinds.as_slice() {
            [] => None,
            [(kind, 1)] => Some(*kind),
            [(kind, occurrences)] => {
                return Err(format!(
                    "{tag} invokes the {kind:?} structured result producer {occurrences} times; expected exactly once"
                ));
            }
            _ => {
                return Err(format!(
                    "{tag} invokes more than one structured result producer: {command_kinds:?}"
                ));
            }
        };
        let command = crate::nextest_build_selections::execution_command(step)?;
        if command.contains("NEXTEST_EXPECTED_EXECUTED") {
            return Err(format!(
                "{tag} declares NEXTEST_EXPECTED_EXECUTED in command text instead of typed step environment"
            ));
        }
        if command_kind == Some(StructuredResultProducerKind::Nextest)
            || step.cmd.contains("nextest-binaries.rs executable ")
        {
            crate::nextest_build_selections::assert_command_selection(step)?;
        }
        let declared = step
            .structured_test_results_manifest()
            .map_err(|error| format!("{tag}: {error}"))?;
        match (expected.remove(tag.as_str()), command_kind, declared) {
            (Some(expected_kind), Some(actual_kind), Some(manifest)) => {
                if actual_kind != expected_kind {
                    return Err(format!(
                        "{tag} is registered as {expected_kind:?} but invokes {actual_kind:?}"
                    ));
                }
                if manifest.owner != tag {
                    return Err(format!(
                        "{tag} declares structured result owner {:?}",
                        manifest.owner
                    ));
                }
                *seen_by_kind.entry(expected_kind).or_default() += 1;
            }
            (Some(expected_kind), None, _) => {
                return Err(format!(
                    "{tag} is registered as {expected_kind:?} but no longer invokes that writer"
                ));
            }
            (Some(expected_kind), Some(actual_kind), None) => {
                return Err(format!(
                    "{tag} invokes {actual_kind:?} and is registered as {expected_kind:?}, but omits its structured result declaration"
                ));
            }
            (None, Some(actual_kind), _) => {
                return Err(format!(
                    "{tag} invokes unregistered structured result producer {actual_kind:?}"
                ));
            }
            (None, None, Some(_)) => {
                return Err(format!(
                    "{tag} declares structured results but invokes no registered writer"
                ));
            }
            (None, None, None) => {}
        }

        match (
            expected_counts.remove(tag.as_str()),
            step.env.get("NEXTEST_EXPECTED_EXECUTED"),
        ) {
            (Some(expected_count), Some(actual)) if actual == &expected_count.to_string() => {}
            (Some(expected_count), Some(actual)) => {
                return Err(format!(
                    "{tag} expects {expected_count} Nextest tests but declares {actual:?}"
                ));
            }
            (Some(expected_count), None) => {
                return Err(format!(
                    "{tag} omits NEXTEST_EXPECTED_EXECUTED={expected_count}"
                ));
            }
            (None, Some(actual)) => {
                return Err(format!(
                    "{tag} declares unexpected NEXTEST_EXPECTED_EXECUTED={actual:?}"
                ));
            }
            (None, None) => {}
        }
    }
    if !expected.is_empty() {
        return Err(format!(
            "registered structured result producers are absent from the DAG: {}",
            expected.keys().copied().collect::<Vec<_>>().join(", ")
        ));
    }
    if !expected_counts.is_empty() {
        return Err(format!(
            "Nextest expected-count steps are absent from the DAG: {}",
            expected_counts
                .keys()
                .copied()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let actual_group_counts = StructuredResultProducerKind::ALL
        .into_iter()
        .map(|kind| seen_by_kind.get(&kind).copied().unwrap_or_default())
        .collect::<Vec<_>>();
    if actual_group_counts != [67, 33, 2, 2, 2] {
        return Err(format!(
            "structured result producer group counts changed: {actual_group_counts:?}"
        ));
    }
    Ok(())
}

fn result_identity(result: &DagManifest) -> String {
    format!(
        "{}/{}/{}/{}/{}",
        result.lane,
        result.category,
        result.test.as_deref().unwrap_or(""),
        result.mode.as_deref().unwrap_or(""),
        result.backend.as_deref().unwrap_or("")
    )
}

fn expected_for_label<'a>(label: &str, cells: &'a [DagManifest]) -> Vec<&'a DagManifest> {
    cells
        .iter()
        .filter(|cell| match label {
            "full" => true,
            "portable" => cell.lane == "portable",
            HOSTED_PORTABLE_LABEL => cell.lane == "portable",
            HOSTED_PRIVILEGED_LABEL => cell.lane == "privileged",
            "privileged" => cell.lane == "privileged",
            "quick" => {
                cell.lane == "portable"
                    && cell.mode.as_deref() == Some("verify")
                    && cell.backend.as_deref() == Some("ptrace")
            }
            "super" => false,
            _ => false,
        })
        .collect()
}

fn assert_rust_script_producer_contract(cfg: &DagConfig) -> Result<(), String> {
    type ProducerContract<'a> = (&'a str, &'a [&'a str], &'a [&'a str], i64, f64);
    let expected: &[ProducerContract<'_>] = &[
        (
            "build.rust_scripts",
            &["full", "hosted-portable", "portable"],
            &["pre.reverie_pin"],
            7200,
            190.0,
        ),
        (
            "build.rust_scripts_on_host",
            &["hosted-privileged"],
            &["pre.reverie_pin_on_host"],
            7200,
            190.0,
        ),
        (
            "build.rust_scripts_in_pinned_root",
            &["full", "portable"],
            &["pre.reverie_pin", "setup.pinned_root_fetch"],
            7200,
            190.0,
        ),
        (
            "quick-super-build.rust_scripts",
            &["quick", "super"],
            &["pre.reverie_pin"],
            crate::validation_dag_static::RUST_SCRIPT_PRODUCER_QUICK_SUPER_CPU_SECONDS,
            0.0,
        ),
        (
            "quick-super-build.rust_scripts_in_pinned_root",
            &["quick", "super"],
            &["pre.reverie_pin", "setup.pinned_root_fetch"],
            crate::validation_dag_static::RUST_SCRIPT_PRODUCER_QUICK_SUPER_CPU_SECONDS,
            0.0,
        ),
    ];
    let expected_tags = expected
        .iter()
        .map(|(tag, ..)| (*tag).to_string())
        .collect::<BTreeSet<_>>();
    let actual_producers = cfg
        .steps
        .iter()
        .filter(|step| {
            matches!(
                step.job.as_str(),
                "rust_scripts" | "rust_scripts_on_host" | "rust_scripts_in_pinned_root"
            )
        })
        .collect::<Vec<_>>();
    let actual_tags = actual_producers
        .iter()
        .map(|step| step.tag())
        .collect::<BTreeSet<_>>();
    if actual_producers.len() != expected.len() || actual_tags != expected_tags {
        return Err(format!(
            "rust-script producer identity population changed: expected={} {expected_tags:?}, actual={} {actual_tags:?}",
            expected.len(),
            actual_producers.len(),
        ));
    }

    for (tag, labels, deps, cpu_timeout, est_duration_s) in expected {
        let step = cfg
            .steps
            .iter()
            .find(|step| step.tag() == *tag)
            .ok_or_else(|| format!("committed DAG lost {tag}"))?;
        let execution_command = crate::nextest_build_selections::execution_command(step)?;
        let expected_labels = labels
            .iter()
            .map(|label| (*label).to_string())
            .collect::<Vec<_>>();
        let expected_deps = deps
            .iter()
            .map(|dependency| (*dependency).to_string())
            .collect::<Vec<_>>();
        let has_no_result_ownership =
            step.manifest.is_none() && matches!(step.result_manifests.as_deref(), Some([]));
        if execution_command != crate::validation_dag_static::RUST_SCRIPT_PRODUCER_COMMAND
            || step.labels != expected_labels
            || step.deps != expected_deps
            || !has_no_result_ownership
            || step.timeout != crate::validation_dag_static::RUST_SCRIPT_PRODUCER_WALL_SECONDS
            || step.cpu_timeout != *cpu_timeout
            || step.hint.est_duration_s != *est_duration_s
            || step.hint.rss_baseline_bytes
                != Some(crate::validation_dag_static::RUST_SCRIPT_PRODUCER_RSS_BASELINE_BYTES)
            || step.hint.hard_mem_max_bytes
                != Some(crate::validation_dag_static::RUST_SCRIPT_PRODUCER_HARD_MEM_MAX_BYTES)
            || step.hint.classification != dagrun::model::StepClass::CpuBound
            || step.hint.preferred_inner_jobs
                != Some(crate::validation_dag_static::RUST_SCRIPT_PRODUCER_INNER_JOBS)
            || step.jobs_flag.as_deref() != Some("")
            || step.jobs_env.as_deref() != Some("CARGO_BUILD_JOBS")
            || step.fail_fast_family.as_deref() != Some(*tag)
        {
            return Err(format!(
                "{tag} changed its exact rust-script producer identity or resource contract: {step:?}"
            ));
        }
    }
    Ok(())
}

fn critical_path_wall_seconds(cfg: &DagConfig) -> Result<i64, String> {
    let by_tag = cfg
        .steps
        .iter()
        .map(|step| (step.tag(), step))
        .collect::<BTreeMap<_, _>>();
    let mut longest = BTreeMap::<String, i64>::new();
    while longest.len() < by_tag.len() {
        let mut advanced = false;
        for (tag, step) in &by_tag {
            if longest.contains_key(tag) {
                continue;
            }
            if step
                .deps
                .iter()
                .any(|dependency| !by_tag.contains_key(dependency))
            {
                return Err(format!(
                    "{tag} names a dependency outside its selected graph"
                ));
            }
            if step
                .deps
                .iter()
                .any(|dependency| !longest.contains_key(dependency))
            {
                continue;
            }
            let predecessor = step
                .deps
                .iter()
                .filter_map(|dependency| longest.get(dependency))
                .copied()
                .max()
                .unwrap_or(0);
            longest.insert(tag.clone(), predecessor + step.timeout);
            advanced = true;
        }
        if !advanced {
            return Err("selected graph contains a dependency cycle".into());
        }
    }
    longest
        .values()
        .copied()
        .max()
        .ok_or_else(|| "selected graph is empty".to_string())
}

fn assert_invariants(cfg: &DagConfig, cells: &[DagManifest]) -> Result<(), String> {
    assert_structured_result_producers(cfg)?;
    crate::nextest_build_selections::assert_preparation_dependencies(cfg)?;
    assert_rust_script_producer_contract(cfg)?;
    if cfg.steps.len() != 1598 {
        return Err(format!(
            "superset has {} steps, expected 1598",
            cfg.steps.len()
        ));
    }
    if cfg.default_step_timeout != 600
        || cfg.resource_caps
            != BTreeMap::from([
                ("manifest_guest".into(), 8),
                ("integration_test_binaries.cli".into(), 1),
                ("integration_test_binaries.hermit_modes".into(), 1),
            ])
    {
        return Err(format!(
            "top-level validation policy changed: default_step_timeout={} resource_caps={:?}",
            cfg.default_step_timeout, cfg.resource_caps
        ));
    }
    let step = |tag: &str| {
        cfg.steps
            .iter()
            .find(|step| step.tag() == tag)
            .ok_or_else(|| format!("committed DAG lost {tag}"))
    };
    for tag in crate::validation_dag_static::PMU_MEMORY_FAILURE_FAMILY_MEMBERS {
        if step(tag)?.fail_fast_family.as_deref()
            != Some(crate::validation_dag_static::PMU_MEMORY_FAILURE_FAMILY)
        {
            return Err(format!(
                "{tag} lost the shared pre-cutover PMU failure family"
            ));
        }
    }
    let outcome_consumers = step("check.check_outcome_consumers")?;
    if outcome_consumers.cmd != OUTCOME_CONSUMERS_COMMAND {
        return Err(
            "check.check_outcome_consumers must retain its no-result classification wrapper".into(),
        );
    }
    let builder = step("privileged-build.privileged_tests")?;
    for (binary, portable, privileged) in [
        ("cli", "test.cli", "privileged-test.cli_kvm"),
        (
            "hermit_modes",
            "test.hermit_modes",
            "privileged-test.pmu_buck_chaos_cases",
        ),
    ] {
        if builder.deps.iter().any(|dependency| dependency == portable) {
            return Err(format!(
                "privileged build must not depend on portable test success: {portable}"
            ));
        }
        let resource = format!("integration_test_binaries.{binary}");
        let mut expected = BTreeMap::from([
            (builder.tag(), 1),
            (portable.to_string(), 1),
            (privileged.to_string(), 1),
        ]);
        if binary == "cli" {
            expected.insert("test.isolated_dbt_workdir".into(), 1);
        }
        let actual = cfg
            .steps
            .iter()
            .filter_map(|step| {
                step.hint
                    .resources
                    .get(&resource)
                    .map(|demand| (step.tag(), *demand))
            })
            .collect::<BTreeMap<_, _>>();
        if actual != expected {
            return Err(format!(
                "shared integration resource {resource} demanders changed: expected={expected:?}, actual={actual:?}"
            ));
        }
        if !step(privileged)?
            .deps
            .iter()
            .any(|dependency| dependency == &builder.tag())
        {
            return Err(format!(
                "{privileged} lost its privileged build prerequisite"
            ));
        }
    }
    let focused_release = step("compatprep.hermit_release")?;
    let expected_focused_labels = [
        "strict-compat-only",
        "sabre-compat-only",
        "e9patch-compat-only",
        "rr-compat-only",
    ];
    if focused_release.cmd != "cargo build --release -p hermit --features third-party-backends"
        || focused_release.deps != ["gate.manifest"]
        || focused_release.labels
            != expected_focused_labels
                .iter()
                .map(|value| (*value).to_string())
                .collect::<Vec<_>>()
        || focused_release.timeout != 420
        || focused_release.cpu_timeout != 840
        || focused_release.hint.hard_mem_max_bytes != Some(16 * 1024 * 1024 * 1024)
        || focused_release.hint.preferred_inner_jobs != Some(8)
    {
        return Err("focused compatibility release producer changed command, dependency, labels, or measured resources".into());
    }
    let focused_image = step("compatprep.hermit_release_in_pinned_root")?;
    if crate::nextest_build_selections::execution_command(focused_image)? != focused_release.cmd
        || focused_image.labels != ["portable-strict-compat-only"]
        || focused_image.deps
            != [
                "build.rust_scripts_in_pinned_root",
                "gate.manifest",
                "setup.pinned_root_fetch",
            ]
        || focused_image.timeout != focused_release.timeout
        || focused_image.cpu_timeout != focused_release.cpu_timeout
        || focused_image.hint.resources != focused_release.hint.resources
        || focused_image.hint.est_duration_s != focused_release.hint.est_duration_s
        || focused_image.hint.rss_baseline_bytes != focused_release.hint.rss_baseline_bytes
        || focused_image.hint.rss_baseline_inner_jobs
            != focused_release.hint.rss_baseline_inner_jobs
        || focused_image.hint.hard_mem_max_bytes != focused_release.hint.hard_mem_max_bytes
        || focused_image.hint.classification != focused_release.hint.classification
        || focused_image.hint.preferred_inner_jobs != focused_release.hint.preferred_inner_jobs
        || focused_image.hint.measured_effective_cores
            != focused_release.hint.measured_effective_cores
        || focused_image.hint.measured_cpu_utilization
            != focused_release.hint.measured_cpu_utilization
        || focused_image.jobs_flag != focused_release.jobs_flag
        || focused_image.jobs_env != focused_release.jobs_env
    {
        return Err("portable focused release producer changed the dedicated command or resources, or lost its gate/image prerequisites".into());
    }
    for group in [
        "portablecompatprep",
        "strictcompatprep",
        "sabrecompatprep",
        "e9patchcompatprep",
        "rrcompatprep",
    ] {
        let prep = step(&format!("{group}.fixtures"))?;
        let producer = if group == "portablecompatprep" {
            "compatprep.hermit_release_in_pinned_root"
        } else {
            "compatprep.hermit_release"
        };
        if !prep.deps.iter().any(|dependency| dependency == producer)
            || prep
                .deps
                .iter()
                .any(|dependency| dependency == "build.runtime_release")
        {
            return Err(format!(
                "{group}.fixtures does not use the dedicated focused release producer"
            ));
        }
    }
    let quick_verify = step("quick.e2e_verify")?;
    if cfg
        .steps
        .iter()
        .any(|step| step.tag() == "quick.build_in_pinned_root")
        || quick_verify.deps
            != [
                "pre.reverie_pin".to_string(),
                "quick-super-build.rust_scripts_in_pinned_root".to_string(),
                "quick-super-gate.manifest".to_string(),
                "quick-super-setup.manifest_plan".to_string(),
                "quick-super-setup.manifest_plan_in_pinned_root".to_string(),
                "quick.build".to_string(),
                "setup.pinned_root_fetch".to_string(),
            ]
    {
        return Err(format!(
            "quick selection changed its single workspace-build topology: deps={:?}",
            quick_verify.deps
        ));
    }
    let canonical = dag_to_json(cfg);
    let reparsed = dag_from_json(&canonical)
        .map_err(|error| format!("generated DAG fails strict reload: {error}"))?;
    if dag_to_json(&reparsed) != canonical {
        return Err("generated DAG is not byte-stable across a strict reload".into());
    }
    let fetch = cfg
        .steps
        .iter()
        .find(|step| step.tag() == PINNED_ROOT_FETCH_TAG)
        .ok_or("committed DAG lost setup.pinned_root_fetch")?;
    if fetch.deps != ["pre.reverie_pin".to_string()] {
        return Err(format!(
            "setup.pinned_root_fetch must depend exactly on pre.reverie_pin before networked input is fetched; got {:?}",
            fetch.deps
        ));
    }
    let pin = cfg
        .steps
        .iter()
        .find(|step| step.tag() == "pre.reverie_pin")
        .ok_or("committed DAG lost pre.reverie_pin")?;
    if pin.cmd != PIN_GATE_COMMAND {
        return Err(format!(
            "pre.reverie_pin must use the unconditional with-proxy command; got {:?}",
            pin.cmd
        ));
    }
    let missing_rust_script_dep = cfg
        .steps
        .iter()
        .filter(|step| {
            is_manifest_run(step)
                && !is_hosted_variant(step)
                && !step.deps.iter().any(|dependency| {
                    dependency
                        == if step
                            .labels
                            .iter()
                            .any(|label| label == "quick" || label == "super")
                        {
                            "quick-super-build.rust_scripts_in_pinned_root"
                        } else {
                            "build.rust_scripts_in_pinned_root"
                        }
                })
        })
        .map(Step::tag)
        .collect::<Vec<_>>();
    if !missing_rust_script_dep.is_empty() {
        return Err(format!(
            "local manifest nodes lost their direct build.rust_scripts_in_pinned_root dependency: {}",
            missing_rust_script_dep.join(", ")
        ));
    }
    for profile in PROFILES {
        let direct = cfg
            .steps
            .iter()
            .filter(|step| step.labels.iter().any(|label| label == profile.label))
            .count();
        if direct != profile.direct_steps {
            return Err(format!(
                "{} label has {direct} direct steps, expected {}",
                profile.label, profile.direct_steps
            ));
        }
        let selected = select_steps_by_labels(cfg, &[profile.label.to_string()])
            .map_err(|error| format!("{} label selection failed: {error}", profile.label))?;
        if selected.steps.len() != profile.selected_steps {
            return Err(format!(
                "{} label closes over {} steps, expected {}",
                profile.label,
                selected.steps.len(),
                profile.selected_steps
            ));
        }
        let expected_results = expected_for_label(profile.label, cells);
        for result in &expected_results {
            result_manifest_owner(&selected.steps, result)
                .map_err(|error| format!("{} result ownership failed: {error}", profile.label))?;
        }
        let expected_result_ids = expected_results
            .into_iter()
            .map(result_identity)
            .collect::<BTreeSet<_>>();
        let actual_result_ids = selected
            .steps
            .iter()
            .flat_map(|step| step.effective_result_manifests().into_owned())
            .map(|result| result_identity(&result))
            .collect::<BTreeSet<_>>();
        if actual_result_ids != expected_result_ids {
            return Err(format!(
                "{} selected result population changed: expected={} actual={} missing={:?} extra={:?}",
                profile.label,
                expected_result_ids.len(),
                actual_result_ids.len(),
                expected_result_ids
                    .difference(&actual_result_ids)
                    .collect::<Vec<_>>(),
                actual_result_ids
                    .difference(&expected_result_ids)
                    .collect::<Vec<_>>()
            ));
        }
        // The single quick build consumes Nextest from the pinned image, and
        // the width-8 rust-script producer now has a 900-second wall boundary
        // with the measured quick/super 1200-second CPU budget.
        if profile.label == "quick" && critical_path_wall_seconds(&selected)? != 9180 {
            return Err(format!(
                "quick selected critical path differs from 9180 seconds with image-owned Nextest and the measured rust-script wall bound: {}",
                critical_path_wall_seconds(&selected)?
            ));
        }
        if profile.label == "privileged" && critical_path_wall_seconds(&selected)? != 4500 {
            return Err(format!(
                "local privileged selected critical path differs from 4500 seconds with the measured rust-script wall bound: {}",
                critical_path_wall_seconds(&selected)?
            ));
        }
        if profile.label == HOSTED_PRIVILEGED_LABEL {
            let expected = [
                "build.rust_scripts_on_host",
                "gate.manifest_on_host",
                "pre.reverie_pin_on_host",
                "privileged-build.manifest_guests_on_host",
                "privileged-only-build.privileged_tests_on_host",
                "privileged-only-cpuid.faulting_on_host",
                "privileged-only-e2e.manifest_applications_on_host",
                "privileged-only-e2e.manifest_backend_parity_c_on_host",
                "privileged-only-pmu.preemption_on_host",
                "privileged-only-test.cli_kvm_on_host",
                "privileged-only-test.pmu_buck_chaos_cases_on_host",
                "setup.manifest_plan_on_host",
            ]
            .into_iter()
            .map(str::to_string)
            .collect::<BTreeSet<_>>();
            let actual = selected
                .steps
                .iter()
                .map(Step::tag)
                .collect::<BTreeSet<_>>();
            if actual != expected {
                return Err(format!(
                    "hosted privileged node population changed: expected={expected:?} actual={actual:?}"
                ));
            }
            let expected_cpu = [
                ("pre.reverie_pin_on_host", 300),
                ("build.rust_scripts_on_host", 7200),
                ("setup.manifest_plan_on_host", 7200),
                ("gate.manifest_on_host", 600),
                ("privileged-only-build.privileged_tests_on_host", 7200),
                ("privileged-only-cpuid.faulting_on_host", 7200),
                ("privileged-only-pmu.preemption_on_host", 7200),
                ("privileged-only-test.pmu_buck_chaos_cases_on_host", 7200),
                ("privileged-build.manifest_guests_on_host", 7200),
                ("privileged-only-e2e.manifest_applications_on_host", 7200),
                (
                    "privileged-only-e2e.manifest_backend_parity_c_on_host",
                    7200,
                ),
                ("privileged-only-test.cli_kvm_on_host", 7200),
            ]
            .into_iter()
            .map(|(tag, cpu)| (tag.to_string(), cpu))
            .collect::<BTreeMap<_, _>>();
            let actual_cpu = selected
                .steps
                .iter()
                .map(|step| (step.tag(), step.cpu_timeout))
                .collect::<BTreeMap<_, _>>();
            if actual_cpu != expected_cpu {
                return Err(format!(
                    "hosted privileged CPU budgets changed: expected={expected_cpu:?} actual={actual_cpu:?}; every hosted step must retain an explicit measured/current budget rather than dagrun's stale 10-second fallback"
                ));
            }
            if selected
                .steps
                .iter()
                .any(|step| !step.hint.resources.is_empty())
            {
                return Err("hosted privileged graph gained an unmeasured resource demand".into());
            }
            let critical = critical_path_wall_seconds(&selected)?;
            if critical != 2100 {
                return Err(format!(
                    "hosted privileged critical path differs from 2100 seconds with the measured rust-script wall bound: {critical}"
                ));
            }
        }
        if profile.label == "super"
            && selected
                .steps
                .iter()
                .any(|step| !step.effective_result_manifests().is_empty())
        {
            return Err("super selection unexpectedly owns manifest result rows".into());
        }
        if profile.label == HOSTED_PORTABLE_LABEL {
            let pinned = selected
                .steps
                .iter()
                .filter(|step| {
                    step.tag() == PINNED_ROOT_FETCH_TAG
                        || step.job.ends_with(PINNED_ROOT_TWIN_SUFFIX)
                        || step.cmd.contains("run-in-pinned-root.sh")
                })
                .map(Step::tag)
                .collect::<Vec<_>>();
            if !pinned.is_empty() {
                return Err(format!(
                    "{HOSTED_PORTABLE_LABEL} selection contains local pinned-root step(s): {}",
                    pinned.join(", ")
                ));
            }
            let expected_resources = HOSTED_RESOURCE_TUPLES
                .iter()
                .map(|(tag, resource, demand, capacity)| {
                    ((*tag).into(), (*resource).into(), *demand, *capacity)
                })
                .collect::<Vec<(String, String, i64, i64)>>();
            let actual_resources = hosted_resource_tuples(cfg)?;
            if actual_resources != expected_resources {
                return Err(format!(
                    "{HOSTED_PORTABLE_LABEL} effective resource tuples changed: expected={expected_resources:?}, actual={actual_resources:?}"
                ));
            }
            let rust_scripts = selected
                .steps
                .iter()
                .find(|step| step.tag() == "build.rust_scripts")
                .ok_or_else(|| format!("{HOSTED_PORTABLE_LABEL} lost build.rust_scripts"))?;
            if rust_scripts.cpu_timeout != 7200 {
                return Err(format!(
                    "{HOSTED_PORTABLE_LABEL} build.rust_scripts CPU budget is {}, expected the pre-cutover effective 7200 seconds",
                    rust_scripts.cpu_timeout
                ));
            }
        }
    }
    for profile in ["quick", "super", "portable", "full", HOSTED_PORTABLE_LABEL] {
        let selected = select_steps_by_labels(cfg, &[profile.into()])?;
        let quick_super = matches!(profile, "quick" | "super");
        let producers = selected
            .steps
            .iter()
            .filter(|step| step.job == "rust_scripts" || step.job == "rust_scripts_in_pinned_root")
            .collect::<Vec<_>>();
        let expected_count = if profile == HOSTED_PORTABLE_LABEL {
            1
        } else {
            2
        };
        let expected_cpu = if quick_super {
            crate::validation_dag_static::RUST_SCRIPT_PRODUCER_QUICK_SUPER_CPU_SECONDS
        } else {
            7200
        };
        if producers.len() != expected_count
            || producers.iter().any(|step| {
                step.cpu_timeout != expected_cpu
                    || step.group.starts_with("quick-super-") != quick_super
            })
        {
            return Err(format!(
                "{profile} Rust-script producers must retain {expected_count} distinct producers with CPU budget {expected_cpu}: {:?}",
                producers
                    .iter()
                    .map(|step| (step.tag(), step.cpu_timeout))
                    .collect::<Vec<_>>()
            ));
        }
    }
    let known_results = cells.iter().map(result_identity).collect::<BTreeSet<_>>();
    for step in &cfg.steps {
        if step.result_manifests.is_none() {
            return Err(format!("{} omits explicit result ownership", step.tag()));
        }
        if step.timeout <= 0 || step.cpu_timeout <= 0 {
            return Err(format!("{} omits an explicit wall/CPU budget", step.tag()));
        }
        for result in step.effective_result_manifests().iter() {
            let identity = result_identity(result);
            if !known_results.contains(&identity) {
                return Err(format!("{} owns unknown result {identity}", step.tag()));
            }
        }
        for forbidden in ["dagrun run", "scripts/validate.rs", "pressure-test.rs"] {
            if step.cmd.contains(forbidden) {
                return Err(format!(
                    "{} contains forbidden nested scheduler boundary {forbidden:?}",
                    step.tag()
                ));
            }
        }
    }
    Ok(())
}

pub fn generate(root: &Path) -> Result<DagConfig, String> {
    let static_source = crate::validation_dag_static::config();
    let scratch = Scratch::create()?;
    let mut generated = generated_plan(root, &scratch.0)?;
    for step in &mut generated.steps {
        normalize_step(step, root, &scratch.0.join("run-state"))?;
    }
    let cells = expected_cells(root)?;
    let mut refreshed = refresh_generated_partitions(static_source, generated)?;
    materialize_hosted_portable_selection(&mut refreshed);
    materialize_hosted_test_variants(&mut refreshed)?;
    materialize_pinned_root(&mut refreshed)?;
    materialize_focused_preflight(&mut refreshed)?;
    materialize_quick_super_budgets(&mut refreshed);
    materialize_runtime_policy(&mut refreshed);
    attach_result_ownership(&mut refreshed, &cells);
    assert_invariants(&refreshed, &cells)?;
    Ok(refreshed)
}

pub fn canonical_text(cfg: &DagConfig) -> String {
    format!("{}\n", dag_to_json(cfg))
}

pub fn require_fresh(committed: &str, generated: &str) -> Result<(), String> {
    if committed == generated {
        return Ok(());
    }
    let first = committed
        .lines()
        .zip(generated.lines())
        .position(|(left, right)| left != right)
        .map(|line| line + 1)
        .unwrap_or_else(|| committed.lines().count().min(generated.lines().count()) + 1);
    Err(format!(
        "{OUTPUT} is stale (first differing line {first}); regenerate with: \
         cargo run -p hermit-manifest-plan --bin generate-validation-dag -- --write"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_setup_prepares_tracked_dagrun_before_cargo_with_admitted_width() {
        use std::os::unix::fs::PermissionsExt;

        use dagrun::model::command_with_inner_jobs;
        use dagrun::model::env_with_inner_jobs;

        for tag in ["setup.manifest_plan", "setup.manifest_plan_on_host"] {
            let step = crate::validation_dag_static::config()
                .steps
                .into_iter()
                .find(|step| step.tag() == tag)
                .unwrap();
            for width in [1, 3] {
                for (prepare_status, cargo_status) in [(0, 0), (23, 0), (0, 29)] {
                    let scratch = Scratch::create().unwrap();
                    let root = &scratch.0;
                    fs::create_dir_all(root.join("agent-utils/rs/bin")).unwrap();
                    fs::create_dir_all(root.join("tools")).unwrap();
                    let launcher = root.join("agent-utils/rs/bin/dagrun");
                    fs::write(
                        &launcher,
                        "#!/bin/bash\nprintf 'runner:%s:%s:%s\\n' \"${AGENT_UTILS_RS_ENSURE_ONLY:-}\" \"${CARGO_BUILD_JOBS:-}\" \"$#\" >> \"$CAPTURE\"\nexit \"$PREPARE_STATUS\"\n",
                    )
                    .unwrap();
                    let cargo = root.join("tools/cargo");
                    fs::write(
                        &cargo,
                        "#!/bin/bash\nprintf 'cargo:%s\\n' \"${CARGO_BUILD_JOBS:-}\" >> \"$CAPTURE\"\nprintf '<%s>\\n' \"$@\" >> \"$CAPTURE\"\nexit \"$CARGO_STATUS\"\n",
                    )
                    .unwrap();
                    for path in [&launcher, &cargo] {
                        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
                    }
                    let capture = root.join("capture");
                    let mut command = Command::new("timeout");
                    command
                        .args(["--kill-after=1s", "5s", "bash", "-c"])
                        .arg(command_with_inner_jobs(&step, "-j", Some(width)))
                        .current_dir(root)
                        .env(
                            "PATH",
                            format!("{}:/usr/bin:/bin", root.join("tools").display()),
                        )
                        .env("CAPTURE", &capture)
                        .env("CARGO_BUILD_JOBS", "99")
                        .env("PREPARE_STATUS", prepare_status.to_string())
                        .env("CARGO_STATUS", cargo_status.to_string());
                    if let Some((key, value)) = env_with_inner_jobs(&step, "", Some(width)) {
                        command.env(key, value);
                    }
                    let output = command.output().unwrap();
                    assert_eq!(
                        output.status.code(),
                        Some(if prepare_status == 0 {
                            cargo_status
                        } else {
                            prepare_status
                        }),
                        "{tag}: {output:?}",
                    );
                    let mut expected = format!("runner:1:{width}:0\n");
                    if prepare_status == 0 {
                        expected.push_str(&format!(
                            "cargo:{width}\n<build>\n<-p>\n<hermit-manifest-plan>\n<--bins>\n<-j>\n<{width}>\n"
                        ));
                    }
                    assert_eq!(fs::read_to_string(&capture).unwrap(), expected, "{tag}");
                }
            }
        }
    }

    // The fake Podman below executes no container. It records the real wrapper's
    // argv, reconstructs only its declared environment, and maps exactly the two
    // fixed image assertion paths to inert files before executing the guard.
    fn renderer_wrapper_capture(step: &Step, width: i64, wrapped: bool) -> serde_json::Value {
        use std::os::unix::fs::PermissionsExt;

        use dagrun::model::command_with_inner_jobs;
        use dagrun::model::env_with_inner_jobs;

        let scratch = Scratch::create().unwrap();
        let root = &scratch.0;
        let write_executable = |path: &Path, text: &[u8]| {
            fs::write(path, text).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        };
        fs::create_dir_all(root.join("ci/hermetic")).unwrap();
        fs::create_dir_all(root.join("tools")).unwrap();
        fs::create_dir_all(root.join("ignored/hermetic/split/cargo/registry")).unwrap();
        let actual_wrapper =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../hermetic/run-in-pinned-root.sh");
        write_executable(
            &root.join("ci/hermetic/run-in-pinned-root.sh"),
            &fs::read(actual_wrapper).unwrap(),
        );
        fs::write(
            root.join("ci/hermetic/image.digest"),
            "fixture@sha256:unused\n",
        )
        .unwrap();
        for name in ["assert-no-network.sh", "assert-build-dependencies.sh"] {
            write_executable(&root.join("ci/hermetic").join(name), b"#!/bin/sh\nexit 0\n");
        }
        fs::write(
            root.join("guards.json"),
            serde_json::to_vec(&[PINNED_ROOT_COMMAND_GUARD, LEGACY_PINNED_ROOT_COMMAND_GUARD])
                .unwrap(),
        )
        .unwrap();
        write_executable(
            &root.join("tools/podman"),
            br##"#!/usr/bin/env python3
import json, os, pathlib, shlex, subprocess, sys
root = pathlib.Path(os.environ['WRAPPER_TEST_ROOT'])
args = sys.argv[1:]
with (root / 'podman.jsonl').open('a') as out:
    out.write(json.dumps(args) + '\n')
if args == ['image', 'exists', 'fixture@sha256:unused']:
    sys.exit(0)
assert args[0] == 'run', args
boundary = args.index('fixture@sha256:unused')
command = args[boundary + 1:]
assert command[:2] == ['bash', '-c'] and command[3] == 'bash', command
assert command[2] in json.loads((root / 'guards.json').read_text()), command
# No ambient NEXTEST_TEST_THREADS leakage: emulate only explicitly forwarded
# Podman environment flags, plus the executable lookup needed by the fixture.
env = {'PATH': os.environ['PATH'], 'LC_ALL': 'C'}
for index, arg in enumerate(args[:boundary]):
    if arg in ['--env', '-e']:
        item = args[index + 1]
        if '=' in item:
            name, value = item.split('=', 1)
            env[name] = value
        elif item in os.environ:
            env[item] = os.environ[item]
for name in ['assert-no-network.sh', 'assert-build-dependencies.sh']:
    original = '/src/ci/hermetic/' + name
    assert command[2].count(original) == 1
    command[2] = command[2].replace(original, shlex.quote(str(root / 'ci/hermetic' / name)))
completed = subprocess.run(command, cwd=root, env=env, timeout=5)
sys.exit(completed.returncode)
"##,
        );
        fs::write(
            root.join("capture.py"),
            br#"import json, os, pathlib, sys
pathlib.Path('capture.json').write_text(json.dumps({
    'args': sys.argv[1:], 'width': os.environ.get('NEXTEST_TEST_THREADS')
}))
print('literal-payload-status-37')
sys.exit(37)
"#,
        )
        .unwrap();
        let mut command_step = step.clone();
        command_step.cmd = format!(
            "python3 {} {}",
            shell_quote(root.join("capture.py").to_str().unwrap()),
            step.cmd,
        );
        let unwrapped = command_step.cmd.clone();
        if wrapped {
            command_step.cmd = pinned_root_command(&command_step);
        }
        let rendered = command_with_inner_jobs(&command_step, "-j", Some(width));
        let mut command = Command::new("timeout");
        command
            .args(["-k", "1", "10", "bash", "-c"])
            .arg(&rendered)
            .current_dir(root)
            .env_clear()
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    root.join("tools").display(),
                    std::env::var("PATH").unwrap(),
                ),
            )
            .env("LC_ALL", "C")
            .env("WRAPPER_TEST_ROOT", root)
            .env("NEXTEST_TEST_THREADS", "99");
        if let Some((name, value)) = env_with_inner_jobs(&command_step, "", Some(width)) {
            command.env(name, value);
        }
        let output = command.output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(37),
            "payload failure status must survive: {rendered}\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert_eq!(output.stdout, b"literal-payload-status-37\n");
        assert!(output.stderr.is_empty(), "{:?}", output.stderr);
        if wrapped {
            let calls = fs::read_to_string(root.join("podman.jsonl")).unwrap();
            let calls = calls
                .lines()
                .map(|line| serde_json::from_str::<Vec<String>>(line).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[0], ["image", "exists", "fixture@sha256:unused"]);
            let boundary = calls[1]
                .iter()
                .position(|arg| arg == "fixture@sha256:unused")
                .unwrap();
            assert_eq!(calls[1][boundary + 5], unwrapped);
            if step.jobs_env.as_deref() == Some("NEXTEST_TEST_THREADS") {
                assert_eq!(
                    calls[1][..boundary]
                        .windows(2)
                        .filter(|pair| pair[0] == "--env" && pair[1] == "NEXTEST_TEST_THREADS")
                        .count(),
                    1,
                    "the wrapper must explicitly forward the admitted width",
                );
                assert_eq!(calls[1].len(), boundary + 6, "no trailing jobs argv");
            }
        }
        serde_json::from_slice(&fs::read(root.join("capture.json")).unwrap()).unwrap()
    }

    #[test]
    fn pinned_wrapper_preserves_actual_renderer_literal_arguments_and_status() {
        let mut step = owner(Vec::new());
        let original = ["already present", ""];
        let literal = [
            "space value",
            "",
            "$(printf expanded)",
            "`printf expanded`",
            "semi;value",
            "quote'\"",
            "line\nbreak",
            "*",
        ];
        step.cmd = original.map(shell_quote).join(" ");
        step.jobs_flag = Some(format!("--jobs %d {}", literal.map(shell_quote).join(" ")));
        step.jobs_env = Some(String::new());
        for width in [1, 3] {
            let plain = renderer_wrapper_capture(&step, width, false);
            let wrapped = renderer_wrapper_capture(&step, width, true);
            let expected = original
                .iter()
                .copied()
                .chain(["--jobs", &width.to_string()])
                .chain(literal)
                .map(str::to_owned)
                .collect::<Vec<_>>();
            assert_eq!(plain["args"], serde_json::json!(expected));
            assert_eq!(
                wrapped, plain,
                "literal renderer argv changed at width {width}"
            );
        }
    }

    #[test]
    fn pinned_wrapper_forwards_admitted_nextest_width_without_changing_filter_tail() {
        for tag in ["test.isolated_dbt_workdir", "test.isolated_detcore_workdir"] {
            let mut step = crate::validation_dag_static::config()
                .steps
                .into_iter()
                .find(|step| step.tag() == tag)
                .unwrap();
            assert_eq!(step.jobs_flag.as_deref(), Some(""));
            assert_eq!(step.jobs_env.as_deref(), Some("NEXTEST_TEST_THREADS"));
            let tail = [
                "existing argument",
                "--",
                "--include-ignored",
                "--exact",
                "literal test",
            ];
            step.cmd = tail.map(shell_quote).join(" ");
            for width in [1, 3] {
                let plain = renderer_wrapper_capture(&step, width, false);
                let wrapped = renderer_wrapper_capture(&step, width, true);
                assert_eq!(plain["args"], serde_json::json!(tail));
                assert_eq!(plain["width"], width.to_string());
                assert_eq!(
                    wrapped, plain,
                    "renderer-owned width/filter changed for {tag}"
                );
            }
        }
    }

    #[test]
    fn pinned_root_wrapper_preserves_cache_and_run_state_boundaries() {
        let steps = crate::validation_dag_static::config().steps;
        for step in &steps {
            let command = pinned_root_command(step);
            assert_eq!(
                command.matches(" --proc-locks-runtime ").count(),
                usize::from(step.tag() == "test.hermit_integration")
            );
        }
        let integration = steps
            .iter()
            .find(|step| step.tag() == "test.hermit_integration")
            .unwrap();
        let command = pinned_root_command(integration);
        assert_eq!(
            refresh_pinned_root_environment("test.hermit_integration", &command).unwrap(),
            command
        );
        assert_eq!(
            refresh_pinned_root_environment(
                "test.hermit_integration",
                &command.replace(" --proc-locks-runtime", "")
            )
            .unwrap()
            .matches(" --proc-locks-runtime ")
            .count(),
            1
        );
        assert!(refresh_pinned_root_environment("test.hermit_unit", &command).is_err());
        assert!(
            refresh_pinned_root_environment(
                "test.hermit_integration",
                &command.replace(
                    " --proc-locks-runtime",
                    " --proc-locks-runtime --proc-locks-runtime"
                )
            )
            .is_err()
        );
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../hermetic/run-in-pinned-root-cache-test.py");
        let output = std::process::Command::new("python3")
            .arg(script)
            .output()
            .expect("run the actual wrapper with the recorded Podman fixture");
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn exact(test: &str) -> DagManifest {
        DagManifest {
            lane: "portable".into(),
            category: "applications".into(),
            test: Some(test.into()),
            mode: Some("verify".into()),
            backend: Some("ptrace".into()),
        }
    }

    fn owner(result_manifests: Vec<DagManifest>) -> Step {
        let text = r#"{"description":"","steps":[{"group":"e2e","job":"owner","cmd":"true","timeout":1,"cpu_timeout":1,"hint":{"rss_baseline_bytes":1,"hard_mem_max_bytes":1}}]}"#;
        let mut step = dag_from_json(text).unwrap().steps.remove(0);
        step.result_manifests = Some(
            result_manifests
                .into_iter()
                .map(ResultManifest::ManifestCell)
                .collect(),
        );
        step
    }

    #[test]
    fn freshness_is_exact_and_detects_every_contract_axis() {
        let baseline = r#"{
  "cmd": "run",
  "deps": ["build.x"],
  "labels": ["portable"],
  "cpu_timeout": 30,
  "result_manifests": [{"lane":"portable"}],
  "resource_caps": {"guest": 1}
}
"#;
        assert!(require_fresh(baseline, baseline).is_ok());
        for (from, to) in [
            ("\"run\"", "\"run changed\""),
            ("build.x", "build.y"),
            ("portable", "quick"),
            ("30", "31"),
            ("manifest", "manifest_changed"),
            ("\"guest\": 1", "\"guest\": 2"),
        ] {
            let changed = baseline.replacen(from, to, 1);
            assert!(
                require_fresh(baseline, &changed).is_err(),
                "mutation {from:?} passed"
            );
        }
    }

    #[test]
    fn full_generator_refuses_static_artifact_mutations() {
        let root = repo_root().unwrap();
        let generated = canonical_text(&generate(&root).unwrap());
        let committed = include_str!("../../dag/validate.json");
        assert_eq!(committed, generated);
        for mutate in ["command", "dependency", "cap"] {
            let mut changed = dag_from_json(committed).unwrap();
            match mutate {
                "command" => changed
                    .steps
                    .iter_mut()
                    .find(|step| step.tag() == "quick.run_smoke")
                    .unwrap()
                    .cmd
                    .push_str(" --planted"),
                "dependency" => changed
                    .steps
                    .iter_mut()
                    .find(|step| step.tag() == "quick.run_smoke")
                    .unwrap()
                    .deps
                    .clear(),
                "cap" => changed.default_step_timeout += 1,
                _ => unreachable!(),
            }
            let changed = canonical_text(&changed);
            assert!(
                require_fresh(&changed, &generated).is_err(),
                "{mutate} mutation was accepted as fresh"
            );
        }
    }

    #[test]
    fn result_ownership_accepts_one_owner_and_refuses_zero_or_two() {
        let result = exact("applications/echo");
        let first = owner(vec![result.clone()]);
        assert_eq!(
            result_manifest_owner(std::slice::from_ref(&first), &result)
                .unwrap()
                .tag(),
            "e2e.owner"
        );
        assert!(
            result_manifest_owner(&[owner(Vec::new())], &result)
                .unwrap_err()
                .contains("no owning step")
        );
        let mut second = first.clone();
        second.job = "duplicate".into();
        assert!(
            result_manifest_owner(&[first, second], &result)
                .unwrap_err()
                .contains("multiple owning steps")
        );
    }

    #[test]
    fn refresh_replaces_generated_mutations_but_preserves_static_edits() {
        let committed = dag_from_json(include_str!("../../dag/validate.json")).unwrap();
        let generated = committed.with_steps(
            committed
                .steps
                .iter()
                .filter(|step| generated_partition(step).is_some())
                .cloned()
                .collect(),
        );

        let mut generated_mutation = committed.clone();
        generated_mutation
            .steps
            .iter_mut()
            .find(|step| step.tag() == "compat.echo")
            .unwrap()
            .cmd
            .push_str(" --planted-generated-mutation");
        let refreshed =
            refresh_generated_partitions(generated_mutation, generated.clone()).unwrap();
        assert!(
            !refreshed
                .steps
                .iter()
                .find(|step| step.tag() == "compat.echo")
                .unwrap()
                .cmd
                .contains("planted-generated-mutation")
        );

        let mut static_edit = committed;
        static_edit
            .steps
            .iter_mut()
            .find(|step| step.tag() == "quick.run_smoke")
            .unwrap()
            .description = "intentional static edit".into();
        let refreshed = refresh_generated_partitions(static_edit, generated).unwrap();
        assert_eq!(
            refreshed
                .steps
                .iter()
                .find(|step| step.tag() == "quick.run_smoke")
                .unwrap()
                .description,
            "intentional static edit"
        );
    }

    #[test]
    fn parity_activation_preserves_the_two_existing_mixed_bucket_selectors() {
        let dag = generate(&repo_root().unwrap()).unwrap();
        let parity = dag
            .steps
            .iter()
            .filter(|step| step.cmd.contains("--parity-reference"))
            .collect::<Vec<_>>();
        assert_eq!(
            parity
                .iter()
                .map(|step| format!("{}.{}", step.group, step.job))
                .collect::<Vec<_>>(),
            [
                "e2e.manifest_backend_parity_c",
                "e2e.manifest_backend_parity_c_on_host"
            ]
        );
        for step in parity {
            assert_eq!(step.cmd.matches("--parity-reference ptrace").count(), 1);
            assert!(step.cmd.contains("--category backend-parity-c --ci-only --allow-empty --prebuilt --parity-reference ptrace --jobs 8"));
            let selector = step.manifest.as_ref().unwrap();
            assert_eq!(selector.lane, "portable");
            assert_eq!(selector.category, "backend-parity-c");
            assert_eq!(selector.test, None);
            assert_eq!(selector.mode, None);
            assert_eq!(selector.backend, None);
            assert_eq!(step.hint.resources.get("manifest_guest"), Some(&8));
            assert_eq!(step.hint.preferred_inner_jobs, Some(8));
            assert!(!step.cmd.contains("--probe-disabled"));
        }
    }

    #[test]
    fn hosted_selection_is_complete_and_excludes_local_pinned_root_steps() {
        let committed = dag_from_json(include_str!("../../dag/validate.json")).unwrap();
        let selected =
            select_steps_by_labels(&committed, &[HOSTED_PORTABLE_LABEL.to_string()]).unwrap();
        assert_eq!(selected.steps.len(), 251);
        let legacy_variants = [
            "test.cli_on_host",
            "test.hermit_modes_on_host",
            "e2e.manifest_applications_on_host",
            "e2e.manifest_backend_parity_c_on_host",
            "e2e.manifest_bin_c_on_host",
            "e2e.manifest_c_programs_on_host",
            "e2e.manifest_chaos_c_on_host",
            "e2e.manifest_data_handling_on_host",
            "e2e.manifest_debugger_c_on_host",
            "e2e.manifest_determinism_stress_c_on_host",
            "e2e.manifest_determinism_stress_on_host",
            "e2e.manifest_language_runtimes_on_host",
            "e2e.manifest_shared_futex_c_on_host",
            "e2e.manifest_system_utils_on_host",
            "e2e.manifest_util_c_on_host",
            "scorecard.compatibility_on_host",
        ];
        let shared_tests = [
            "app_strict_verify",
            "applications_e2e",
            "arbitrary_binaries",
            "command_strict_verify",
            "dbt_parity",
            "detcore_misc",
            "detcore_parallel",
            "detcore_unit",
            "envelope_levels",
            "hermit_integration",
            "hermit_unit",
            "ignored_syscall_regressions",
            "liteinst_strict",
            "regular_crates",
            "rr_suite_contract",
            "sabre_examples",
        ];
        let mut new_variants = committed
            .steps
            .iter()
            .filter(|step| {
                step.group == "compat"
                    && !is_hosted_variant(step)
                    && step.labels.iter().any(|label| label == "portable")
            })
            .map(|step| format!("{}_on_host", step.tag()))
            .collect::<BTreeSet<_>>();
        assert_eq!(new_variants.len(), 189);
        new_variants.extend(shared_tests.map(|job| format!("test.{job}_on_host")));
        new_variants.insert("compatprep.fixtures_on_host".into());
        assert_eq!(new_variants.len(), 206);
        let mut expected = legacy_variants
            .map(str::to_string)
            .into_iter()
            .collect::<BTreeSet<_>>();
        assert_eq!(expected.len(), 16);
        assert!(expected.is_disjoint(&new_variants));
        expected.extend(new_variants);
        assert_eq!(
            selected
                .steps
                .iter()
                .filter(|step| is_hosted_variant(step))
                .map(Step::tag)
                .collect::<BTreeSet<_>>(),
            expected
        );
        assert!(selected.steps.iter().all(|step| {
            step.tag() != PINNED_ROOT_FETCH_TAG
                && !step.job.ends_with(PINNED_ROOT_TWIN_SUFFIX)
                && !step.cmd.contains("run-in-pinned-root.sh")
        }));
        assert_eq!(
            hosted_resource_tuples(&committed).unwrap(),
            HOSTED_RESOURCE_TUPLES
                .iter()
                .map(|(tag, resource, demand, capacity)| {
                    ((*tag).into(), (*resource).into(), *demand, *capacity)
                })
                .collect::<Vec<(String, String, i64, i64)>>(),
        );
        let cells = expected_cells(&repo_root().unwrap()).unwrap();
        for result in expected_for_label(HOSTED_PORTABLE_LABEL, &cells) {
            result_manifest_owner(&selected.steps, result).unwrap();
        }

        let mut planted_pinned_command = committed.clone();
        planted_pinned_command
            .steps
            .iter_mut()
            .find(|step| step.tag() == "e2e.manifest_applications_on_host")
            .unwrap()
            .cmd
            .push_str(" && ./ci/hermetic/run-in-pinned-root.sh");
        let error = assert_invariants(&planted_pinned_command, &cells).unwrap_err();
        assert!(error.contains("local pinned-root step"), "{error}");

        let mut planted_early_fetch = committed.clone();
        planted_early_fetch
            .steps
            .iter_mut()
            .find(|step| step.tag() == PINNED_ROOT_FETCH_TAG)
            .unwrap()
            .deps
            .clear();
        let error = assert_invariants(&planted_early_fetch, &cells).unwrap_err();
        assert!(
            error.contains("must depend exactly on pre.reverie_pin"),
            "{error}"
        );

        let mut planted_pin_fallback = committed.clone();
        planted_pin_fallback
            .steps
            .iter_mut()
            .find(|step| step.tag() == "pre.reverie_pin")
            .unwrap()
            .cmd = "if command -v with-proxy; then with-proxy true; else true; fi".into();
        let error = assert_invariants(&planted_pin_fallback, &cells).unwrap_err();
        assert!(error.contains("unconditional with-proxy"), "{error}");

        let mut planted_missing_rust_script_dep = committed.clone();
        planted_missing_rust_script_dep
            .steps
            .iter_mut()
            .find(|step| step.tag() == "e2e.manifest_applications")
            .unwrap()
            .deps
            .retain(|dependency| dependency != "build.rust_scripts_in_pinned_root");
        let error = assert_invariants(&planted_missing_rust_script_dep, &cells).unwrap_err();
        assert!(
            error.contains("lost their direct build.rust_scripts_in_pinned_root dependency"),
            "{error}"
        );

        let mut planted_coverage_loss = committed;
        planted_coverage_loss
            .steps
            .iter_mut()
            .find(|step| step.tag() == "check.dagrun_naming")
            .unwrap()
            .labels
            .retain(|label| label != HOSTED_PORTABLE_LABEL);
        let error = assert_invariants(&planted_coverage_loss, &cells).unwrap_err();
        assert!(
            error.contains("hosted-portable label has 250 direct steps"),
            "{error}"
        );
    }

    #[test]
    fn profile_producers_retain_distinct_measured_cpu_limits() {
        let committed = dag_from_json(include_str!("../../dag/validate.json")).unwrap();
        let cells = expected_cells(&repo_root().unwrap()).unwrap();
        assert_invariants(&committed, &cells).unwrap();
        for (tag, wrong_cpu) in [
            ("quick-super-build.rust_scripts", 900),
            ("quick-super-build.rust_scripts_in_pinned_root", 900),
            ("build.rust_scripts", 1200),
            ("build.rust_scripts_in_pinned_root", 1200),
        ] {
            let mut changed = committed.clone();
            changed
                .steps
                .iter_mut()
                .find(|step| step.tag() == tag)
                .unwrap()
                .cpu_timeout = wrong_cpu;
            let error = assert_invariants(&changed, &cells).unwrap_err();
            assert!(
                error.contains("rust-script producer identity or resource contract"),
                "{tag}: {error}"
            );
        }
    }

    #[test]
    fn rust_script_producers_retain_exact_identity_and_measured_resources() {
        let committed = dag_from_json(include_str!("../../dag/validate.json")).unwrap();
        assert_rust_script_producer_contract(&committed).unwrap();
        let tags = [
            "build.rust_scripts",
            "build.rust_scripts_on_host",
            "build.rust_scripts_in_pinned_root",
            "quick-super-build.rust_scripts",
            "quick-super-build.rust_scripts_in_pinned_root",
        ];
        type ResourceMutation = fn(&mut Step, &str);
        let old_resource_mutations: [(&str, ResourceMutation); 3] = [
            ("wall", |step, _| step.timeout = 300),
            ("baseline", |step, tag| {
                step.hint.rss_baseline_bytes = Some(if tag.starts_with("quick-super-") {
                    2 * 1024 * 1024 * 1024
                } else {
                    1024 * 1024 * 1024
                })
            }),
            ("hard cap", |step, _| {
                step.hint.hard_mem_max_bytes = Some(2 * 1024 * 1024 * 1024)
            }),
        ];
        for tag in tags {
            for (name, mutate) in old_resource_mutations {
                let mut changed = committed.clone();
                let step = changed
                    .steps
                    .iter_mut()
                    .find(|step| step.tag() == tag)
                    .unwrap();
                mutate(step, tag);
                let error = assert_rust_script_producer_contract(&changed).unwrap_err();
                assert!(error.contains(tag), "{name} mutation: {error}");
            }
        }

        let mut changed_command = committed.clone();
        changed_command
            .steps
            .iter_mut()
            .find(|step| step.tag() == "build.rust_scripts")
            .unwrap()
            .cmd
            .push_str(" --planted");
        assert!(
            assert_rust_script_producer_contract(&changed_command)
                .unwrap_err()
                .contains("build.rust_scripts")
        );

        let mut changed_deps = committed.clone();
        changed_deps
            .steps
            .iter_mut()
            .find(|step| step.tag() == "build.rust_scripts_in_pinned_root")
            .unwrap()
            .deps
            .clear();
        assert!(
            assert_rust_script_producer_contract(&changed_deps)
                .unwrap_err()
                .contains("build.rust_scripts_in_pinned_root")
        );

        let mut changed_ownership = committed.clone();
        changed_ownership
            .steps
            .iter_mut()
            .find(|step| step.tag() == "build.rust_scripts_on_host")
            .unwrap()
            .result_manifests = None;
        assert!(
            assert_rust_script_producer_contract(&changed_ownership)
                .unwrap_err()
                .contains("build.rust_scripts_on_host")
        );

        let mut changed_population = committed;
        changed_population
            .steps
            .retain(|step| step.tag() != "quick-super-build.rust_scripts");
        assert!(
            assert_rust_script_producer_contract(&changed_population)
                .unwrap_err()
                .contains("identity population")
        );
    }

    #[test]
    fn result_classification_and_failure_families_retain_their_pre_cutover_policy() {
        let committed = dag_from_json(include_str!("../../dag/validate.json")).unwrap();
        let cells = expected_cells(&repo_root().unwrap()).unwrap();
        assert_invariants(&committed, &cells).unwrap();

        let mut changed_family = committed.clone();
        changed_family
            .steps
            .iter_mut()
            .find(|step| {
                step.tag() == crate::validation_dag_static::PMU_MEMORY_FAILURE_FAMILY_MEMBERS[0]
            })
            .unwrap()
            .fail_fast_family = Some("independent family".into());
        let error = assert_invariants(&changed_family, &cells).unwrap_err();
        assert!(
            error.contains("shared pre-cutover PMU failure family"),
            "{error}"
        );

        let mut bypassed = committed;
        bypassed
            .steps
            .iter_mut()
            .find(|step| step.tag() == "check.check_outcome_consumers")
            .unwrap()
            .cmd = OUTCOME_CONSUMERS_COMMAND.replace(
            "./ci/check-outcome-consumers-node.sh",
            "./scripts/test-check-status-outcome.sh && ./scripts/check-merge-gate-policy.sh",
        );
        let error = assert_invariants(&bypassed, &cells).unwrap_err();
        assert!(
            error.contains("no-result classification wrapper"),
            "{error}"
        );
    }

    #[test]
    fn structured_result_registry_is_exact_and_bijective() {
        let committed = dag_from_json(include_str!("../../dag/validate.json")).unwrap();
        assert_structured_result_producers(&committed).unwrap();

        for kind in StructuredResultProducerKind::ALL {
            let tag = kind.tags()[0];
            let mut missing = committed.clone();
            missing
                .steps
                .iter_mut()
                .find(|step| step.tag() == tag)
                .unwrap()
                .result_manifests
                .as_mut()
                .unwrap()
                .retain(|manifest| !matches!(manifest, ResultManifest::StructuredTestResults(_)));
            let error = assert_structured_result_producers(&missing).unwrap_err();
            assert!(error.contains(tag) && error.contains("omits"), "{error}");

            let mut changed_writer = committed.clone();
            let step = changed_writer
                .steps
                .iter_mut()
                .find(|step| step.tag() == tag)
                .unwrap();
            step.cmd = step
                .cmd
                .replace(kind.command_marker(), "removed-structured-result-writer");
            let error = assert_structured_result_producers(&changed_writer).unwrap_err();
            assert!(
                error.contains(tag) && error.contains("no longer invokes"),
                "{error}"
            );
        }

        let template = committed
            .steps
            .iter()
            .find(|step| step.tag() == "test.regular_crates")
            .unwrap()
            .result_manifests
            .as_ref()
            .unwrap()
            .iter()
            .find(|manifest| matches!(manifest, ResultManifest::StructuredTestResults(_)))
            .unwrap()
            .clone();

        let mut extra = committed.clone();
        let extra_step = extra
            .steps
            .iter_mut()
            .find(|step| step.tag() == "quick.run_smoke")
            .unwrap();
        let mut extra_manifest = template.clone();
        let ResultManifest::StructuredTestResults(declaration) = &mut extra_manifest else {
            unreachable!()
        };
        declaration.owner = extra_step.tag();
        extra_step
            .result_manifests
            .as_mut()
            .unwrap()
            .push(extra_manifest);
        let error = assert_structured_result_producers(&extra).unwrap_err();
        assert!(
            error.contains("quick.run_smoke") && error.contains("declares"),
            "{error}"
        );

        let mut duplicate = committed.clone();
        duplicate
            .steps
            .iter_mut()
            .find(|step| step.tag() == "test.regular_crates")
            .unwrap()
            .result_manifests
            .as_mut()
            .unwrap()
            .push(template.clone());
        let error = assert_structured_result_producers(&duplicate).unwrap_err();
        assert!(error.contains("more than once"), "{error}");

        let mut wrong_owner = committed.clone();
        let ResultManifest::StructuredTestResults(declaration) = wrong_owner
            .steps
            .iter_mut()
            .find(|step| step.tag() == "test.regular_crates")
            .unwrap()
            .result_manifests
            .as_mut()
            .unwrap()
            .iter_mut()
            .find(|manifest| matches!(manifest, ResultManifest::StructuredTestResults(_)))
            .unwrap()
        else {
            unreachable!()
        };
        declaration.owner = "other.step".into();
        let error = assert_structured_result_producers(&wrong_owner).unwrap_err();
        assert!(error.contains("owner 'other.step'"), "{error}");
    }

    #[test]
    fn structured_result_wire_contract_refuses_wrong_schema_and_path() {
        let committed =
            canonical_text(&dag_from_json(include_str!("../../dag/validate.json")).unwrap());
        let declaration = |field: &str, value: serde_json::Value| {
            let mut document: serde_json::Value = serde_json::from_str(&committed).unwrap();
            let steps = document["steps"].as_array_mut().unwrap();
            let step = steps
                .iter_mut()
                .find(|step| step["group"] == "test" && step["job"] == "regular_crates")
                .unwrap();
            let manifest = step["result_manifests"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|manifest| manifest["kind"] == "structured-test-results")
                .unwrap();
            manifest[field] = value;
            serde_json::to_string(&document).unwrap()
        };
        let wrong_schema = declaration("schema", serde_json::Value::from(99));
        let error = dag_from_json(&wrong_schema).unwrap_err().to_string();
        assert!(error.contains("schema") && error.contains("99"), "{error}");
        let wrong_path = declaration("path_env", serde_json::Value::from("OTHER"));
        let error = dag_from_json(&wrong_path).unwrap_err().to_string();
        assert!(error.contains("path_env"), "{error}");
    }

    #[test]
    fn structured_result_counts_and_ownership_survive_generation_transforms() {
        let committed = dag_from_json(include_str!("../../dag/validate.json")).unwrap();
        for (tag, mutation) in [
            ("test.regular_crates", None),
            ("test.cli", Some("999")),
            ("quick.run_smoke", Some("1")),
        ] {
            let mut changed = committed.clone();
            let step = changed
                .steps
                .iter_mut()
                .find(|step| step.tag() == tag)
                .unwrap();
            match mutation {
                Some(value) => {
                    step.env
                        .insert("NEXTEST_EXPECTED_EXECUTED".into(), value.into());
                }
                None => {
                    step.env.remove("NEXTEST_EXPECTED_EXECUTED");
                }
            }
            let error = assert_structured_result_producers(&changed).unwrap_err();
            assert!(error.contains(tag), "{error}");
        }

        let mut inline_count = committed.clone();
        inline_count
            .steps
            .iter_mut()
            .find(|step| step.tag() == "test.cli")
            .unwrap()
            .cmd
            .insert_str(0, "NEXTEST_EXPECTED_EXECUTED=71 ");
        let error = assert_structured_result_producers(&inline_count).unwrap_err();
        assert!(
            error.contains("test.cli") && error.contains("command text"),
            "{error}"
        );

        let mut duplicate_writer = committed.clone();
        duplicate_writer
            .steps
            .iter_mut()
            .find(|step| step.tag() == "test.regular_crates")
            .unwrap()
            .cmd
            .push_str("; ./ci/run-nextest-counted.sh -p duplicate");
        let error = assert_structured_result_producers(&duplicate_writer).unwrap_err();
        assert!(
            error.contains("test.regular_crates") && error.contains("2 times"),
            "{error}"
        );
        let mut normalized = committed
            .steps
            .iter()
            .find(|step| step.tag() == "test.regular_crates")
            .unwrap()
            .clone();
        let before_normalize = normalized
            .result_manifests
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|manifest| matches!(manifest, ResultManifest::StructuredTestResults(_)))
            .cloned()
            .collect::<Vec<_>>();
        normalize_step(&mut normalized, Path::new("/repo"), Path::new("/run-state")).unwrap();
        let after_normalize = normalized
            .result_manifests
            .as_deref()
            .unwrap_or_default()
            .to_vec();
        assert_eq!(before_normalize, after_normalize);
        assert!(normalized.effective_result_manifests().is_empty());

        let cells = expected_cells(&repo_root().unwrap()).unwrap();
        let mut reattached = committed.clone();
        let before = reattached
            .steps
            .iter()
            .map(|step| (step.tag(), step.result_manifests.clone()))
            .collect::<BTreeMap<_, _>>();
        attach_result_ownership(&mut reattached, &cells);
        for step in &reattached.steps {
            let before_structured = before[&step.tag()]
                .as_deref()
                .unwrap_or_default()
                .iter()
                .filter(|manifest| matches!(manifest, ResultManifest::StructuredTestResults(_)))
                .collect::<Vec<_>>();
            let after_structured = step
                .result_manifests
                .as_deref()
                .unwrap_or_default()
                .iter()
                .filter(|manifest| matches!(manifest, ResultManifest::StructuredTestResults(_)))
                .collect::<Vec<_>>();
            assert_eq!(before_structured, after_structured, "{}", step.tag());
        }
        assert_structured_result_producers(&reattached).unwrap();
    }
}
