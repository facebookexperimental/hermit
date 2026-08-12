#!/usr/bin/env rust-script
//! Generate and run a safe-ci DAG that attempts every red compatibility cell.
//!
//! ```cargo
//! [dependencies]
//! serde = { version = "1", features = ["derive"] }
//! serde_json = "1"
//! toml = "0.8"
//! ```

#[path = "../../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use serde_json::json;
use toml::Value as TomlValue;

const TRACKED_CELLS: &str = "ci/compat-envelope/cells.json";
const PORTABLE_DAG: &str = "ci/dag/portable.json";
const SCHEMA: u64 = 2;
/// The shipped portable DAG gives a whole manifest bucket 600 seconds. A red
/// cell gets that complete existing allowance to itself; cells whose repeated
/// mode could theoretically consume longer remain red when this pressure
/// boundary cuts them. This bounds a known-bad cell without redefining green.
const PRESSURE_CELL_TIMEOUT_SECONDS: i64 = 600;
/// The prior 432-cell measurement completed in nine minutes on this host. Two
/// hours is an operational stop for the periodic experiment, not a pass
/// threshold: breach makes the run incomplete and publishes no promotion.
const PRESSURE_RUN_TIMEOUT_SECONDS: i64 = 2 * 60 * 60;

const USAGE: &str = r#"Usage: ci/compat-envelope/pressure-test.rs COMMAND [OPTIONS]

Commands:
  plan --results DIR [--output FILE]
      Generate a safe-ci DAG that attempts every currently red cell. The
      default output is DIR/dag.json.
  run [--results DIR]
      Require a clean committed checkout, generate the DAG, and execute it.
      The default result directory is ignored/compat-envelope/pressure-<SHA>-<time>.
  summarize --results DIR
      Re-read a completed run, print the per-backend table, and rewrite
      DIR/summary.json. This does not edit the checked-in scorecard.
  self-test
      Check the timeout derivation and shell quoting without running a guest.
  --help
      Show this text.

The generated graph reuses the canonical Hermit/resource build commands from
ci/dag/portable.json. Fixture preparation is serialized. Every red cell then
runs in its own safe-ci cgroup with its manifest timeout nested inside a larger
derived node timeout. A red outcome is data and does not stop the remaining
cells. A pressure run never promotes a cell automatically.
"#;

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct CellId {
    lane: String,
    category: String,
    test: String,
    mode: String,
    backend: String,
}

#[derive(Debug, Deserialize)]
struct TrackedCells {
    schema: u64,
    cells: Vec<TrackedCell>,
}

#[derive(Clone, Debug, Deserialize)]
struct TrackedCell {
    #[serde(flatten)]
    id: CellId,
    enabled: bool,
    status: String,
}

struct PressureCells {
    red: Vec<TrackedCell>,
    preparation_by_test: BTreeMap<String, CellId>,
}

#[derive(Clone, Debug)]
struct CellBudget {
    timeout_seconds: i64,
    attempts: i64,
}

#[derive(Debug, Deserialize)]
struct ResultRow {
    schema: u64,
    hermit_sha: String,
    source_tree_dirty: bool,
    test: String,
    category: String,
    lane: String,
    mode: String,
    backend: Option<String>,
    outcome: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct RunMetadata {
    schema: u64,
    hermit_sha: String,
    detcore_tree: String,
    source_tree_dirty: bool,
    run_timeout_seconds: i64,
    cells: Vec<CellId>,
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("compatibility pressure test: {message}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1);
    let Some(command) = args.next() else {
        return Err(format!("missing command\n\n{USAGE}"));
    };
    if matches!(command.as_str(), "-h" | "--help" | "help") {
        print!("{USAGE}");
        return Ok(());
    }
    let root = repo_root()?;
    match command.as_str() {
        "plan" => {
            let (results, output) = result_options(&root, &mut args, false)?;
            let output = output.unwrap_or_else(|| results.join("dag.json"));
            let metadata = write_plan(&root, &results, &output)?;
            println!("DAG: {}", output.display());
            println!("Results: {}", results.display());
            println!("Cells: {}", metadata.cells.len());
            println!("Whole-run bound: {}s", metadata.run_timeout_seconds);
            println!(
                "Run: RUN_DAG_FILE_OVERRIDE={} ./ci/run-dag.sh portable -k --profile --run-timeout {}",
                shell_quote(&output.to_string_lossy()),
                metadata.run_timeout_seconds
            );
        }
        "run" => {
            let (results, output) = result_options(&root, &mut args, true)?;
            if worktree_dirty(&root)? {
                return Err("run refuses a dirty checkout; commit first so every row binds to reproducible source".into());
            }
            let output = output.unwrap_or_else(|| results.join("dag.json"));
            let metadata = write_plan(&root, &results, &output)?;
            let status = Command::new(root.join("ci/run-dag.sh"))
                .args([
                    "portable",
                    "-k",
                    "--profile",
                    "--run-timeout",
                    &metadata.run_timeout_seconds.to_string(),
                ])
                .env("RUN_DAG_FILE_OVERRIDE", &output)
                .current_dir(&root)
                .status()
                .map_err(|e| format!("cannot start safe-ci-dag-runner: {e}"))?;
            if !status.success() {
                return Err(format!(
                    "safe-ci-dag-runner failed with {}; retained artifacts are in {}",
                    status,
                    results.display()
                ));
            }
            summarize(&root, &results)?;
        }
        "summarize" => {
            let (results, output) = result_options(&root, &mut args, false)?;
            if output.is_some() {
                return Err("summarize does not accept --output".into());
            }
            summarize(&root, &results)?;
        }
        "self-test" => {
            if args.next().is_some() {
                return Err("self-test accepts no options".into());
            }
            self_test()?;
        }
        _ => return Err(format!("unknown command `{command}`\n\n{USAGE}")),
    }
    Ok(())
}

fn result_options(
    root: &Path,
    args: &mut impl Iterator<Item = String>,
    default_results: bool,
) -> Result<(PathBuf, Option<PathBuf>), String> {
    let mut results = None;
    let mut output = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--results" => {
                results = Some(PathBuf::from(
                    args.next().ok_or("--results requires a directory")?,
                ));
            }
            "--output" => {
                output = Some(PathBuf::from(
                    args.next().ok_or("--output requires a file")?,
                ));
            }
            _ => return Err(format!("unknown option `{arg}`\n\n{USAGE}")),
        }
    }
    let results = match (results, default_results) {
        (Some(path), _) => absolute_from(root, path),
        (None, true) => default_result_root(root)?,
        (None, false) => return Err("command requires --results DIR".into()),
    };
    let output = output.map(|path| absolute_from(root, path));
    Ok((results, output))
}

fn absolute_from(root: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

fn default_result_root(root: &Path) -> Result<PathBuf, String> {
    let sha = git_output(root, &["rev-parse", "--short=12", "HEAD"])?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("system clock is before the Unix epoch: {e}"))?
        .as_secs();
    Ok(root
        .join("ignored/compat-envelope")
        .join(format!("pressure-{sha}-{now}")))
}

fn repo_root() -> Result<PathBuf, String> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|e| format!("cannot run git rev-parse: {e}"))?;
    if !output.status.success() {
        return Err("not inside a Git checkout".into());
    }
    let root = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    if !root.join(TRACKED_CELLS).is_file() {
        return Err(format!("{} is not the Hermit checkout", root.display()));
    }
    Ok(root)
}

fn worktree_dirty(root: &Path) -> Result<bool, String> {
    let output = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cannot inspect worktree: {e}"))?;
    if !output.status.success() {
        return Err("git status failed".into());
    }
    Ok(!output.stdout.is_empty())
}

fn git_output(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|e| format!("cannot run git {}: {e}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!("git {} failed", args.join(" ")));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn check_scorecard(root: &Path) -> Result<(), String> {
    let status = Command::new(root.join("ci/compat-envelope/scorecard.rs"))
        .arg("check")
        .current_dir(root)
        .status()
        .map_err(|e| format!("cannot run scorecard check: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("tracked scorecard is stale; update it before generating a pressure run".into())
    }
}

fn pressure_cells(root: &Path) -> Result<PressureCells, String> {
    let path = root.join(TRACKED_CELLS);
    let text =
        fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let tracked: TrackedCells = serde_json::from_str(&text)
        .map_err(|e| format!("invalid JSON in {}: {e}", path.display()))?;
    if tracked.schema != SCHEMA {
        return Err(format!(
            "unsupported tracked cell schema {}",
            tracked.schema
        ));
    }
    let mut seen = BTreeSet::new();
    let mut red = Vec::new();
    let mut preparation_by_test = BTreeMap::new();
    for cell in tracked.cells {
        if !seen.insert(cell.id.clone()) {
            return Err("tracked cells contain a duplicate identity".into());
        }
        if cell.enabled {
            preparation_by_test
                .entry(cell.id.test.clone())
                .or_insert_with(|| cell.id.clone());
        }
        match cell.status.as_str() {
            "red" => red.push(cell),
            "green" => {}
            other => return Err(format!("unknown cell status `{other}`")),
        }
    }
    red.sort_by(|left, right| left.id.cmp(&right.id));
    if red.is_empty() {
        return Err("tracked scorecard has no red cells".into());
    }
    for cell in &red {
        if !preparation_by_test.contains_key(&cell.id.test) {
            return Err(format!(
                "{} has no manifest-enabled mode available to build its fixture",
                cell.id.test
            ));
        }
    }
    Ok(PressureCells {
        red,
        preparation_by_test,
    })
}

fn load_budgets(root: &Path) -> Result<BTreeMap<(String, String), CellBudget>, String> {
    let manifest_dir = root.join("tests/e2e/manifests");
    let mut paths = Vec::new();
    for entry in fs::read_dir(&manifest_dir)
        .map_err(|e| format!("cannot list {}: {e}", manifest_dir.display()))?
    {
        let path = entry
            .map_err(|e| format!("cannot read manifest entry: {e}"))?
            .path();
        if path.extension().and_then(|value| value.to_str()) == Some("toml") {
            paths.push(path);
        }
    }
    paths.sort();
    let mut out = BTreeMap::new();
    for path in paths {
        let text = fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let document: TomlValue = text
            .parse()
            .map_err(|e| format!("invalid TOML in {}: {e}", path.display()))?;
        let tests = document
            .get("test")
            .and_then(TomlValue::as_array)
            .ok_or_else(|| format!("{} has no [[test]] array", path.display()))?;
        for test in tests {
            let id = test
                .get("id")
                .and_then(TomlValue::as_str)
                .ok_or_else(|| format!("{} has a test without id", path.display()))?;
            let timeout_seconds = test
                .get("timeout_seconds")
                .and_then(TomlValue::as_integer)
                .ok_or_else(|| format!("{id} has no timeout_seconds"))?;
            let modes = test
                .get("modes")
                .and_then(TomlValue::as_table)
                .ok_or_else(|| format!("{id} has no modes table"))?;
            for (mode, spec) in modes {
                let attempts = match mode.as_str() {
                    "verify" | "replay" => 1,
                    "naked" => spec
                        .get("runs")
                        .and_then(TomlValue::as_integer)
                        .unwrap_or(1),
                    "custom" => spec
                        .get("assert")
                        .and_then(TomlValue::as_table)
                        .and_then(|assert| assert.get("runs"))
                        .and_then(TomlValue::as_integer)
                        .unwrap_or(1),
                    "chaos" => spec
                        .get("seeds")
                        .and_then(TomlValue::as_array)
                        .map(|seeds| seeds.len() as i64 * 2)
                        .unwrap_or(1),
                    other => return Err(format!("{id} has unknown mode `{other}`")),
                };
                out.insert(
                    (id.to_string(), mode.to_string()),
                    CellBudget {
                        timeout_seconds,
                        attempts,
                    },
                );
            }
        }
    }
    Ok(out)
}

/// The harness may spend one manifest timeout preparing the fixture, then one
/// timeout per attempt. Every timeout has a documented 10-second TERM/KILL
/// grace. The final 30 seconds is the existing nextest/reporting grace used by
/// this repository, not a backend multiplier or a guessed speed ratio.
fn outer_timeout(budget: &CellBudget) -> i64 {
    let phases = budget.attempts + 1;
    phases * (budget.timeout_seconds + 10) + 30
}

fn pressure_timeout(budget: &CellBudget) -> i64 {
    outer_timeout(budget).min(PRESSURE_CELL_TIMEOUT_SECONDS)
}

fn pressure_node_timeout(budget: &CellBudget) -> i64 {
    pressure_timeout(budget) + 20
}

fn write_plan(root: &Path, results: &Path, output: &Path) -> Result<RunMetadata, String> {
    check_scorecard(root)?;
    let PressureCells {
        red: cells,
        preparation_by_test,
    } = pressure_cells(root)?;
    let budgets = load_budgets(root)?;
    fs::create_dir_all(results).map_err(|e| format!("cannot create {}: {e}", results.display()))?;
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }

    let canonical_text = fs::read_to_string(root.join(PORTABLE_DAG))
        .map_err(|e| format!("cannot read {PORTABLE_DAG}: {e}"))?;
    let canonical: JsonValue = serde_json::from_str(&canonical_text)
        .map_err(|e| format!("invalid {PORTABLE_DAG}: {e}"))?;
    let required_builds = BTreeSet::from([
        "setup.manifest_plan",
        "e2e.metadata",
        "build.workspace",
        "build.runtime_release",
        "build.e2e_artifact",
        "build.liteinst_runtime_release",
    ]);
    let mut steps = Vec::new();
    for mut step in canonical["steps"]
        .as_array()
        .ok_or("portable DAG has no steps array")?
        .iter()
        .cloned()
    {
        let tag = format!(
            "{}.{}",
            step["group"].as_str().unwrap_or(""),
            step["job"].as_str().unwrap_or("")
        );
        if required_builds.contains(tag.as_str()) {
            if step
                .get("cpu_timeout")
                .and_then(JsonValue::as_i64)
                .unwrap_or(0)
                <= 0
            {
                let wall = step["timeout"].as_i64().unwrap_or(120);
                step["cpu_timeout"] = json!(wall * 2);
            }
            steps.push(step);
        }
    }
    if steps.len() != required_builds.len() {
        return Err(format!(
            "canonical build extraction found {} of {} required nodes",
            steps.len(),
            required_builds.len()
        ));
    }

    let sha = git_output(root, &["rev-parse", "HEAD"])?;
    let detcore_tree = git_output(root, &["rev-parse", "HEAD:detcore"])?;
    let build_root = results.join("build").join(&sha);
    let mut preparation_tags = BTreeMap::new();
    for (test, cell) in preparation_by_test {
        let budget = budgets
            .get(&(test.clone(), cell.mode.clone()))
            .ok_or_else(|| format!("no manifest budget for {test}/{}", cell.mode))?;
        let job = sanitize(&test);
        let tag = format!("prepare.{job}");
        let status_path = results.join("prepare").join(&job).join("status");
        let backend = if cell.backend == "native" {
            String::new()
        } else {
            format!(" --backend {}", shell_quote(&cell.backend))
        };
        let pressure_seconds = pressure_timeout(budget);
        let cmd = format!(
            "mkdir -p {status_dir}; status=0; \
             timeout --kill-after=10s {pressure_seconds}s env \
             E2E_RESULT_ROOT={results} E2E_BUILD_ROOT={build_root} \
             ./ci/test_harness.sh build --include-manual --include-occasional \
             --test {test} --mode {mode}{backend} || status=$?; \
             printf '%s\\n' \"$status\" > {status}; exit 0",
            status_dir = shell_quote(&status_path.parent().unwrap().to_string_lossy()),
            results = shell_quote(&results.to_string_lossy()),
            build_root = shell_quote(&build_root.to_string_lossy()),
            test = shell_quote(&test),
            mode = shell_quote(&cell.mode),
            backend = backend,
            status = shell_quote(&status_path.to_string_lossy()),
        );
        let wall = pressure_node_timeout(budget);
        steps.push(json!({
            "group": "prepare",
            "job": job,
            "desc": format!("Prepare red-cell fixture {test}"),
            "cmd": cmd,
            "deps": ["build.e2e_artifact", "build.liteinst_runtime_release"],
            "timeout": wall,
            "cpu_timeout": wall * 2,
            "hint": {
                "resources": {"cargo_writer": 1},
                "rss_baseline_bytes": 1073741824_i64,
                "hard_mem_max_bytes": 3221225472_i64,
                "classification": "cpu-bound"
            }
        }));
        preparation_tags.insert(test, tag);
    }

    let mut cell_tags = Vec::new();
    for tracked in &cells {
        let cell = &tracked.id;
        let budget = budgets
            .get(&(cell.test.clone(), cell.mode.clone()))
            .ok_or_else(|| format!("no manifest budget for {}/{}", cell.test, cell.mode))?;
        let slug = sanitize(&format!(
            "{}-{}-{}-{}-{}",
            cell.lane, cell.category, cell.test, cell.mode, cell.backend
        ));
        let tag = format!("cell.{slug}");
        let cell_dir = results.join("cells").join(&slug);
        let result_file = cell_dir.join("results.jsonl");
        let junit = cell_dir.join("junit.xml");
        let status_file = cell_dir.join("harness-status");
        let (selector, backend) = if tracked.enabled {
            let backend = if cell.backend == "native" {
                String::new()
            } else {
                format!(" --backend {}", shell_quote(&cell.backend))
            };
            ("--include-manual", backend)
        } else {
            (
                "--probe-disabled",
                format!(" --backend {}", shell_quote(&cell.backend)),
            )
        };
        let pressure_seconds = pressure_timeout(budget);
        let cmd = format!(
            "mkdir -p {cell_dir}; status=0; \
             timeout --kill-after=10s {pressure_seconds}s env \
             E2E_RESULT_ROOT={results} E2E_BUILD_ROOT={build_root} E2E_RUN_ID={run_id} \
             ./ci/run-with-hermit-e2e-artifact.sh --require-install \
             ./ci/test_harness.sh run {selector} --include-occasional --prebuilt \
             --test {test} --mode {mode}{backend} --results {result_file} --junit {junit} \
             || status=$?; printf '%s\\n' \"$status\" > {status_file}; exit 0",
            cell_dir = shell_quote(&cell_dir.to_string_lossy()),
            results = shell_quote(&results.to_string_lossy()),
            build_root = shell_quote(&build_root.to_string_lossy()),
            run_id = shell_quote(&slug),
            test = shell_quote(&cell.test),
            mode = shell_quote(&cell.mode),
            selector = selector,
            backend = backend,
            result_file = shell_quote(&result_file.to_string_lossy()),
            junit = shell_quote(&junit.to_string_lossy()),
            status_file = shell_quote(&status_file.to_string_lossy()),
        );
        let wall = pressure_node_timeout(budget);
        let memory = if cell.lane == "privileged" {
            16_i64 * 1024 * 1024 * 1024
        } else {
            3_i64 * 1024 * 1024 * 1024
        };
        let mut resources = serde_json::Map::new();
        resources.insert("manifest_guest".into(), json!(1));
        if cell.backend == "kvm" {
            resources.insert("kvm".into(), json!(1));
        }
        steps.push(json!({
            "group": "cell",
            "job": slug,
            "desc": format!("Attempt red cell {}/{}/{}@{}", cell.test, cell.mode, cell.backend, cell.lane),
            "cmd": cmd,
            "deps": [
                preparation_tags.get(&cell.test).expect("preparation tag exists"),
                "build.e2e_artifact",
                "build.liteinst_runtime_release"
            ],
            "timeout": wall,
            "cpu_timeout": wall * 2,
            "hint": {
                "resources": resources,
                "rss_baseline_bytes": memory / 3,
                "hard_mem_max_bytes": memory,
                "classification": "latency-bound"
            }
        }));
        cell_tags.push(tag);
    }

    steps.push(json!({
        "group": "pressure",
        "job": "summarize",
        "desc": "Require every red cell to have been attempted and print outcomes",
        "cmd": format!(
            "./ci/compat-envelope/pressure-test.rs summarize --results {}",
            shell_quote(&results.to_string_lossy())
        ),
        "deps": cell_tags,
        "timeout": 120,
        "cpu_timeout": 120,
        "hint": {
            "rss_baseline_bytes": 268435456_i64,
            "hard_mem_max_bytes": 1073741824_i64,
            "classification": "light"
        }
    }));

    let max_timeout = steps
        .iter()
        .filter_map(|step| step["timeout"].as_i64())
        .max()
        .unwrap_or(120);
    let run_timeout_seconds = PRESSURE_RUN_TIMEOUT_SECONDS;
    let dag = json!({
        "resource_caps": {"cargo_writer": 1, "manifest_guest": 4, "kvm": 1},
        "mem_cap_factor": canonical.get("mem_cap_factor").cloned().unwrap_or(json!(1.25)),
        "mem_cap_floor_bytes": canonical.get("mem_cap_floor_bytes").cloned().unwrap_or(json!(8589934592_i64)),
        "outer_mem_safety_factor": canonical.get("outer_mem_safety_factor").cloned().unwrap_or(json!(1.0)),
        "default_step_timeout": max_timeout,
        "default_step_cpu_timeout": max_timeout * 2,
        "steps": steps,
    });
    audit_dag(&dag, cells.len(), run_timeout_seconds)?;
    let mut dag_text = serde_json::to_string_pretty(&dag)
        .map_err(|e| format!("cannot serialize pressure DAG: {e}"))?;
    dag_text.push('\n');
    fs::write(output, dag_text).map_err(|e| format!("cannot write {}: {e}", output.display()))?;

    let metadata = RunMetadata {
        schema: SCHEMA,
        hermit_sha: sha,
        detcore_tree,
        source_tree_dirty: worktree_dirty(root)?,
        run_timeout_seconds,
        cells: cells.into_iter().map(|cell| cell.id).collect(),
    };
    let mut metadata_text = serde_json::to_string_pretty(&metadata)
        .map_err(|e| format!("cannot serialize run metadata: {e}"))?;
    metadata_text.push('\n');
    fs::write(results.join("run.json"), metadata_text)
        .map_err(|e| format!("cannot write run metadata: {e}"))?;
    Ok(metadata)
}

fn audit_dag(dag: &JsonValue, expected_cells: usize, run_timeout: i64) -> Result<(), String> {
    let steps = dag["steps"]
        .as_array()
        .ok_or("generated DAG has no steps array")?;
    let mut tags = BTreeSet::new();
    let mut deps = Vec::new();
    let mut cells = 0usize;
    let mut summaries = 0usize;
    for step in steps {
        let group = step["group"]
            .as_str()
            .ok_or("generated step has no group")?;
        let job = step["job"].as_str().ok_or("generated step has no job")?;
        let tag = format!("{group}.{job}");
        if !tags.insert(tag.clone()) {
            return Err(format!("generated DAG has duplicate tag {tag}"));
        }
        let timeout = step["timeout"]
            .as_i64()
            .ok_or_else(|| format!("{tag} has no wall timeout"))?;
        let cpu_timeout = step["cpu_timeout"]
            .as_i64()
            .ok_or_else(|| format!("{tag} has no CPU timeout"))?;
        if timeout <= 0 || cpu_timeout <= 0 || timeout >= run_timeout {
            return Err(format!(
                "{tag} has invalid timeout ladder wall={timeout} cpu={cpu_timeout} run={run_timeout}"
            ));
        }
        if step["hint"]["hard_mem_max_bytes"].as_i64().unwrap_or(0) <= 0 {
            return Err(format!("{tag} has no hard memory cap"));
        }
        for dep in step["deps"].as_array().into_iter().flatten() {
            deps.push((tag.clone(), dep.as_str().unwrap_or("").to_string()));
        }
        if group == "cell" {
            cells += 1;
            let cmd = step["cmd"].as_str().unwrap_or("");
            let enabled_selector = cmd.contains("--include-manual");
            let disabled_selector = cmd.contains("--probe-disabled");
            if !cmd.contains("timeout --kill-after=10s")
                || enabled_selector == disabled_selector
                || !cmd.contains("--prebuilt")
                || !cmd.contains("--test")
                || !cmd.contains("--mode")
                || !cmd.contains("--results")
                || !cmd.contains("--junit")
            {
                return Err(format!("{tag} lost its bounded exact-cell harness command"));
            }
        }
        if tag == "pressure.summarize" {
            summaries += 1;
        }
    }
    for (tag, dep) in deps {
        if !tags.contains(&dep) {
            return Err(format!("{tag} depends on absent step {dep}"));
        }
    }
    if cells != expected_cells || summaries != 1 {
        return Err(format!(
            "generated DAG shape mismatch: cells={cells}/{expected_cells}, summaries={summaries}/1"
        ));
    }
    Ok(())
}

fn summarize(root: &Path, results: &Path) -> Result<(), String> {
    let metadata_path = results.join("run.json");
    let metadata: RunMetadata = serde_json::from_str(
        &fs::read_to_string(&metadata_path)
            .map_err(|e| format!("cannot read {}: {e}", metadata_path.display()))?,
    )
    .map_err(|e| format!("invalid {}: {e}", metadata_path.display()))?;
    if metadata.schema != SCHEMA {
        return Err(format!("unsupported run schema {}", metadata.schema));
    }
    let current = git_output(root, &["rev-parse", "HEAD"])?;
    if current != metadata.hermit_sha {
        return Err(format!(
            "run belongs to {}, but checkout HEAD is {}",
            metadata.hermit_sha, current
        ));
    }

    let mut by_backend: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    let mut missing_attempt = Vec::new();
    let mut passing = Vec::new();
    let mut rows = Vec::new();
    for cell in &metadata.cells {
        let slug = sanitize(&format!(
            "{}-{}-{}-{}-{}",
            cell.lane, cell.category, cell.test, cell.mode, cell.backend
        ));
        let cell_dir = results.join("cells").join(&slug);
        let status_file = cell_dir.join("harness-status");
        if !status_file.is_file() {
            missing_attempt.push(display_id(cell));
            continue;
        }
        let harness_status_text = fs::read_to_string(&status_file)
            .map_err(|e| format!("cannot read {}: {e}", status_file.display()))?
            .trim()
            .to_string();
        let harness_status: i32 = harness_status_text.parse().map_err(|_| {
            format!(
                "{} contains nonnumeric harness exit `{harness_status_text}`",
                status_file.display()
            )
        })?;
        let result_file = cell_dir.join("results.jsonl");
        let (outcome, row_valid) = if result_file.is_file() {
            let text = fs::read_to_string(&result_file)
                .map_err(|e| format!("cannot read {}: {e}", result_file.display()))?;
            let lines: Vec<_> = text
                .lines()
                .filter(|line| !line.trim().is_empty())
                .collect();
            if lines.len() != 1 {
                ("NO_RESULT".to_string(), false)
            } else {
                let row: ResultRow = serde_json::from_str(lines[0])
                    .map_err(|e| format!("invalid {}: {e}", result_file.display()))?;
                let observed_backend = row.backend.as_deref().or_else(|| {
                    if row.mode == "naked" {
                        Some("native")
                    } else {
                        None
                    }
                });
                let id_matches = row.schema == 3
                    && row.hermit_sha == metadata.hermit_sha
                    && !row.source_tree_dirty
                    && row.test == cell.test
                    && row.category == cell.category
                    && row.lane == cell.lane
                    && row.mode == cell.mode
                    && observed_backend == Some(cell.backend.as_str());
                let exit_matches = match row.outcome.as_str() {
                    "PASS" => harness_status == 0,
                    "FAIL" | "ERROR" => harness_status != 0,
                    _ => false,
                };
                if id_matches && exit_matches {
                    (row.outcome, true)
                } else {
                    ("NO_RESULT".to_string(), false)
                }
            }
        } else {
            ("NO_RESULT".to_string(), false)
        };
        *by_backend
            .entry(cell.backend.clone())
            .or_default()
            .entry(outcome.clone())
            .or_default() += 1;
        if outcome == "PASS" && row_valid {
            passing.push(display_id(cell));
        }
        rows.push(json!({
            "cell": cell,
            "harness_exit": harness_status,
            "outcome": outcome,
            "result_row_valid": row_valid,
        }));
    }
    if !missing_attempt.is_empty() {
        return Err(format!(
            "{} red cell node(s) never wrote an attempt marker; first is {}",
            missing_attempt.len(),
            missing_attempt[0]
        ));
    }

    println!("# Red-cell pressure-test results");
    println!();
    println!("| Backend | PASS | FAIL | ERROR | No result | Total |");
    println!("| --- | ---: | ---: | ---: | ---: | ---: |");
    let mut totals = [0usize; 5];
    for backend in ["ptrace", "dbt", "kvm", "sabre", "liteinst", "native"] {
        let counts = by_backend.get(backend).cloned().unwrap_or_default();
        let pass = counts.get("PASS").copied().unwrap_or(0);
        let fail = counts.get("FAIL").copied().unwrap_or(0);
        let error = counts.get("ERROR").copied().unwrap_or(0);
        let no_result = counts.get("NO_RESULT").copied().unwrap_or(0);
        let total = pass + fail + error + no_result;
        totals[0] += pass;
        totals[1] += fail;
        totals[2] += error;
        totals[3] += no_result;
        totals[4] += total;
        println!("| `{backend}` | {pass} | {fail} | {error} | {no_result} | {total} |");
    }
    println!(
        "| **Total** | **{}** | **{}** | **{}** | **{}** | **{}** |",
        totals[0], totals[1], totals[2], totals[3], totals[4]
    );
    println!();
    println!(
        "{} red cell(s) passed once; they are candidates for repeated confirmation, not automatic promotion.",
        passing.len()
    );
    for id in passing.iter().take(20) {
        println!("  PASS {id}");
    }

    let summary = json!({
        "schema": SCHEMA,
        "hermit_sha": metadata.hermit_sha,
        "detcore_tree": metadata.detcore_tree,
        "attempted": metadata.cells.len(),
        "pass_candidates": passing,
        "rows": rows,
    });
    let mut text = serde_json::to_string_pretty(&summary)
        .map_err(|e| format!("cannot serialize summary: {e}"))?;
    text.push('\n');
    fs::write(results.join("summary.json"), text)
        .map_err(|e| format!("cannot write summary.json: {e}"))?;
    println!("Summary: {}", results.join("summary.json").display());
    Ok(())
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn display_id(cell: &CellId) -> String {
    format!(
        "{}/{}/{}/{}@{}",
        cell.lane, cell.category, cell.test, cell.mode, cell.backend
    )
}

fn self_test() -> Result<(), String> {
    let budget = CellBudget {
        timeout_seconds: 7,
        attempts: 3,
    };
    if outer_timeout(&budget) != 98 {
        return Err(format!(
            "timeout derivation changed: expected 98, got {}",
            outer_timeout(&budget)
        ));
    }
    if pressure_timeout(&CellBudget {
        timeout_seconds: 1800,
        attempts: 64,
    }) != PRESSURE_CELL_TIMEOUT_SECONDS
    {
        return Err("pressure timeout did not cap a long repeated red cell".into());
    }
    let probe = "space ' quote";
    let quoted = shell_quote(probe);
    let output = Command::new("bash")
        .args(["-c", &format!("printf '%s' {quoted}")])
        .output()
        .map_err(|e| format!("cannot run quoting bracket: {e}"))?;
    if output.stdout != probe.as_bytes() {
        return Err("shell quoting did not round-trip".into());
    }

    let exact_cell_command = "timeout --kill-after=10s 20s ./ci/test_harness.sh run \
        --include-manual --prebuilt --test fixture --mode verify \
        --results result.jsonl --junit result.xml";
    let fixture = json!({
        "steps": [
            {
                "group": "cell",
                "job": "fixture",
                "cmd": exact_cell_command,
                "deps": [],
                "timeout": 40,
                "cpu_timeout": 80,
                "hint": {"hard_mem_max_bytes": 1024}
            },
            {
                "group": "pressure",
                "job": "summarize",
                "cmd": "true",
                "deps": ["cell.fixture"],
                "timeout": 10,
                "cpu_timeout": 10,
                "hint": {"hard_mem_max_bytes": 1024}
            }
        ]
    });
    audit_dag(&fixture, 1, 100)
        .map_err(|e| format!("positive generated-DAG bracket failed: {e}"))?;
    let mut disabled_fixture = fixture.clone();
    disabled_fixture["steps"][0]["cmd"] = json!(
        exact_cell_command
            .replace("--include-manual", "--probe-disabled")
            .replace("--test fixture", "--test fixture --backend kvm")
    );
    audit_dag(&disabled_fixture, 1, 100)
        .map_err(|e| format!("positive disabled-cell bracket failed: {e}"))?;
    let mut missing_exact_selector = fixture;
    missing_exact_selector["steps"][0]["cmd"] =
        json!(exact_cell_command.replace("--mode verify", ""));
    if audit_dag(&missing_exact_selector, 1, 100).is_ok() {
        return Err("negative generated-DAG bracket accepted a cell without an exact mode".into());
    }
    println!(
        "compatibility pressure-test self-test: timeout, quoting, and generated-DAG brackets pass"
    );
    Ok(())
}
