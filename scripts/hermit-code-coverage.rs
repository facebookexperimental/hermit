#!/usr/bin/env rust-script
//! Copyright (c) Meta Platforms, Inc. and affiliates.
//! All rights reserved.
//!
//! This source code is licensed under the BSD-style license found in the
//! LICENSE file in the root directory of this source tree.
//!
//! Collect and compare per-test source coverage of Hermit and Detcore.
//!
//! ```cargo
//! [dependencies]
//! serde = { version = "1", features = ["derive"] }
//! serde_json = "1"
//! ```

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;
use std::process::ExitStatus;
use std::process::Stdio;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

const DEFAULT_OUTPUT_ROOT: &str = "target/hermit-code-coverage";
const SCHEMA: &str = "hermit-code-coverage/v1";
const PRODUCT_ROOTS: &[&str] = &[
    "common",
    "detcore",
    "detcore-dbi",
    "detcore-liteinst",
    "detcore-model",
    "detcore-sabre",
    "hermit-cli",
    "hermit-install",
    "hermit-resources",
    "hermit-verify",
];

fn usage() -> &'static str {
    "Usage: hermit-code-coverage.rs COMMAND [OPTIONS]\n\
\n\
Commands:\n\
  prepare\n\
      Build the continuously-profiled Hermit binary.\n\
\n\
  collect --name NAME [--output-root PATH] [--no-build] [--command] -- ARGS...\n\
      Run one named case and write isolated line/region coverage. By default\n\
      ARGS are passed to the instrumented Hermit binary. With --command, ARGS\n\
      are an arbitrary wrapper command and HERMIT_BIN/HERMIT_COVERAGE_BIN point\n\
      to the instrumented binary.\n\
\n\
  diff --baseline NAME --candidate NAME [--output-root PATH] [--fail-on-loss]\n\
      Compare normalized covered-line and covered-region sets.\n\
\n\
Examples:\n\
  scripts/hermit-code-coverage.rs collect --name original -- \\\n+      --backend ptrace run --strict -- /bin/echo hello\n\
  scripts/hermit-code-coverage.rs collect --name shrunk -- \\\n+      --backend ptrace run --strict -- /bin/true\n\
  scripts/hermit-code-coverage.rs diff --baseline original --candidate shrunk\n\
\n\
Exit status: 0 success, the wrapped case status after writing a report,\n\
1 for --fail-on-loss, and 2 for an operational error."
}

#[derive(Debug)]
struct CollectOptions {
    name: String,
    output_root: PathBuf,
    build: bool,
    wrapper_command: bool,
    args: Vec<String>,
}

#[derive(Debug)]
struct DiffOptions {
    baseline: String,
    candidate: String,
    output_root: PathBuf,
    fail_on_loss: bool,
}

#[derive(Debug)]
enum Action {
    Prepare,
    Collect(CollectOptions),
    Diff(DiffOptions),
    Help,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct Metric {
    count: u64,
    covered: u64,
    percent: f64,
}

impl Metric {
    fn add(&mut self, count: u64, covered: u64) {
        self.count += count;
        self.covered += covered;
        self.percent = percentage(self.covered, self.count);
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct FileCoverage {
    path: String,
    lines: Metric,
    regions: Metric,
    functions: Metric,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct Totals {
    files: u64,
    lines: Metric,
    regions: Metric,
    functions: Metric,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct CoverageReport {
    schema: String,
    name: String,
    mode: String,
    command: Vec<String>,
    exit_code: i32,
    git_head: String,
    git_dirty: bool,
    rustc: String,
    cargo_llvm_cov: String,
    scope_roots: Vec<String>,
    totals: Totals,
    files: Vec<FileCoverage>,
    covered_lines: BTreeMap<String, BTreeSet<u64>>,
    covered_regions: BTreeMap<String, BTreeSet<String>>,
}

#[derive(Debug, Deserialize, Serialize)]
struct CoverageDiff {
    schema: String,
    baseline: String,
    candidate: String,
    baseline_command: Vec<String>,
    candidate_command: Vec<String>,
    baseline_totals: Totals,
    candidate_totals: Totals,
    lost_lines: BTreeMap<String, BTreeSet<u64>>,
    gained_lines: BTreeMap<String, BTreeSet<u64>>,
    lost_regions: BTreeMap<String, BTreeSet<String>>,
    gained_regions: BTreeMap<String, BTreeSet<String>>,
    baseline_covered_line_set: u64,
    candidate_covered_line_set: u64,
    baseline_covered_region_set: u64,
    candidate_covered_region_set: u64,
    preserved_line_percent: f64,
    preserved_region_percent: f64,
    coverage_preserved: bool,
}

fn parse_args() -> Result<Action, String> {
    let mut args = env::args().skip(1);
    let Some(command) = args.next() else {
        return Ok(Action::Help);
    };
    match command.as_str() {
        "-h" | "--help" | "help" => Ok(Action::Help),
        "prepare" => {
            if args.next().is_some() {
                Err("prepare takes no options".to_owned())
            } else {
                Ok(Action::Prepare)
            }
        }
        "collect" => {
            let mut name = None;
            let mut output_root = PathBuf::from(DEFAULT_OUTPUT_ROOT);
            let mut build = true;
            let mut wrapper_command = false;
            let mut run_args = Vec::new();
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--name" => name = Some(next_value(&mut args, "--name")?),
                    "--output-root" => {
                        output_root = PathBuf::from(next_value(&mut args, "--output-root")?)
                    }
                    "--no-build" => build = false,
                    "--command" => wrapper_command = true,
                    "--" => {
                        run_args.extend(args);
                        break;
                    }
                    _ => return Err(format!("unknown collect option: {arg}")),
                }
            }
            let name = name.ok_or_else(|| "collect requires --name NAME".to_owned())?;
            validate_name(&name)?;
            if run_args.is_empty() {
                return Err("collect requires a command after --".to_owned());
            }
            Ok(Action::Collect(CollectOptions {
                name,
                output_root,
                build,
                wrapper_command,
                args: run_args,
            }))
        }
        "diff" => {
            let mut baseline = None;
            let mut candidate = None;
            let mut output_root = PathBuf::from(DEFAULT_OUTPUT_ROOT);
            let mut fail_on_loss = false;
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--baseline" => baseline = Some(next_value(&mut args, "--baseline")?),
                    "--candidate" => candidate = Some(next_value(&mut args, "--candidate")?),
                    "--output-root" => {
                        output_root = PathBuf::from(next_value(&mut args, "--output-root")?)
                    }
                    "--fail-on-loss" => fail_on_loss = true,
                    _ => return Err(format!("unknown diff option: {arg}")),
                }
            }
            let baseline = baseline.ok_or_else(|| "diff requires --baseline NAME".to_owned())?;
            let candidate = candidate.ok_or_else(|| "diff requires --candidate NAME".to_owned())?;
            validate_name(&baseline)?;
            validate_name(&candidate)?;
            Ok(Action::Diff(DiffOptions {
                baseline,
                candidate,
                output_root,
                fail_on_loss,
            }))
        }
        _ => Err(format!("unknown command: {command}")),
    }
}

fn next_value(args: &mut impl Iterator<Item = String>, option: &str) -> Result<String, String> {
    args.next()
        .ok_or_else(|| format!("{option} requires a value"))
}

fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(format!(
            "invalid report name {name:?}; use only ASCII letters, digits, '.', '_', or '-'"
        ));
    }
    Ok(())
}

fn repo_root() -> Result<PathBuf, String> {
    let mut current = env::current_dir().map_err(|error| format!("current directory: {error}"))?;
    loop {
        if current.join("Cargo.toml").is_file()
            && current.join("detcore").is_dir()
            && current.join("hermit-cli").is_dir()
        {
            return current
                .canonicalize()
                .map_err(|error| format!("canonicalize repository root: {error}"));
        }
        if !current.pop() {
            return Err("run from inside the Hermit repository".to_owned());
        }
    }
}

fn command_output(root: &Path, program: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|error| format!("run {program}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{program} {} failed with {}: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn instrumented_binary(root: &Path) -> PathBuf {
    root.join("target/hermit-code-coverage/build/debug/hermit")
}

fn prepare(root: &Path) -> Result<PathBuf, String> {
    command_output(root, "cargo", &["llvm-cov", "--version"])?;
    llvm_tools(root)?;
    let show_env = command_output(root, "cargo", &["llvm-cov", "show-env", "--sh"])?;
    let mut coverage_env = parse_coverage_environment(&show_env)?;
    let build_dir = root.join("target/hermit-code-coverage/build");
    coverage_env.insert(
        "CARGO_TARGET_DIR".to_owned(),
        build_dir.display().to_string(),
    );
    coverage_env.insert(
        "CARGO_LLVM_COV_TARGET_DIR".to_owned(),
        build_dir.display().to_string(),
    );
    coverage_env.insert(
        "CARGO_LLVM_COV_BUILD_DIR".to_owned(),
        build_dir.display().to_string(),
    );
    coverage_env.insert(
        "LLVM_PROFILE_FILE".to_owned(),
        build_dir.join("build-%p-%m.profraw").display().to_string(),
    );
    let rustflags = match env::var("RUSTFLAGS") {
        Ok(existing) if !existing.trim().is_empty() => {
            format!("{existing} -C llvm-args=-runtime-counter-relocation")
        }
        _ => "-C llvm-args=-runtime-counter-relocation".to_owned(),
    };
    let status = Command::new("cargo")
        .args(["build", "--bin", "hermit"])
        .envs(&coverage_env)
        .env("RUSTFLAGS", rustflags)
        .current_dir(root)
        .stdout(Stdio::null())
        .status()
        .map_err(|error| format!("build instrumented Hermit: {error}"))?;
    if !status.success() {
        return Err(format!("instrumented Hermit build failed with {status}"));
    }
    let binary = instrumented_binary(root);
    if !binary.is_file() {
        return Err(format!(
            "cargo-llvm-cov did not produce expected binary {}",
            binary.display()
        ));
    }
    Ok(binary)
}

fn parse_coverage_environment(input: &str) -> Result<BTreeMap<String, String>, String> {
    let mut result = BTreeMap::new();
    for line in input.lines().filter(|line| !line.trim().is_empty()) {
        let assignment = line
            .strip_prefix("export ")
            .ok_or_else(|| format!("unexpected cargo llvm-cov show-env line: {line}"))?;
        let (name, value) = assignment
            .split_once('=')
            .ok_or_else(|| format!("invalid cargo llvm-cov show-env assignment: {line}"))?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return Err(format!("invalid cargo llvm-cov environment name: {line}"));
        }
        let value = match value
            .strip_prefix('\'')
            .and_then(|value| value.strip_suffix('\''))
        {
            Some(value) => value,
            // This value is passed directly to Command::env, not evaluated by a
            // shell, so cargo-llvm-cov's unquoted comma-separated crate list is
            // safe to preserve verbatim.
            None => value,
        };
        if value.contains('\'') {
            return Err(format!(
                "unsupported quote in cargo llvm-cov show-env value: {line}"
            ));
        }
        result.insert(name.to_owned(), value.to_owned());
    }
    Ok(result)
}

fn llvm_tools(root: &Path) -> Result<(PathBuf, PathBuf), String> {
    let sysroot = PathBuf::from(command_output(root, "rustc", &["--print", "sysroot"])?);
    let version = command_output(root, "rustc", &["-vV"])?;
    let host = version
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .ok_or_else(|| "rustc -vV did not report a host triple".to_owned())?;
    let bin = sysroot.join("lib/rustlib").join(host).join("bin");
    let profdata = bin.join("llvm-profdata");
    let cov = bin.join("llvm-cov");
    if !profdata.is_file() || !cov.is_file() {
        return Err(format!(
            "missing llvm-tools-preview under {}; run `rustup component add llvm-tools-preview`",
            bin.display()
        ));
    }
    Ok((profdata, cov))
}

fn collect(root: &Path, options: CollectOptions) -> Result<(CoverageReport, ExitStatus), String> {
    let output_root = absolutize(root, &options.output_root);
    let output_dir = output_root.join(&options.name);
    if output_dir.exists() {
        return Err(format!(
            "report directory already exists: {}; choose a new --name to preserve evidence",
            output_dir.display()
        ));
    }
    let binary = if options.build {
        prepare(root)?
    } else {
        instrumented_binary(root)
    };
    if !binary.is_file() {
        return Err(format!(
            "instrumented Hermit is missing at {}; omit --no-build or run prepare",
            binary.display()
        ));
    }
    let raw_dir = output_dir.join("raw");
    fs::create_dir_all(&raw_dir)
        .map_err(|error| format!("create {}: {error}", raw_dir.display()))?;

    let stdout_path = output_dir.join("run.stdout.log");
    let stderr_path = output_dir.join("run.stderr.log");
    let stdout = File::create(&stdout_path)
        .map_err(|error| format!("create {}: {error}", stdout_path.display()))?;
    let stderr = File::create(&stderr_path)
        .map_err(|error| format!("create {}: {error}", stderr_path.display()))?;
    let profile_pattern = raw_dir.join("%p-%m%c.profraw");

    let (mut command, rendered, mode) = if options.wrapper_command {
        let mut command = Command::new(&options.args[0]);
        command.args(&options.args[1..]);
        (command, options.args.clone(), "wrapper-command".to_owned())
    } else {
        let mut command = Command::new(&binary);
        command.args(&options.args);
        let mut rendered = vec![binary.display().to_string()];
        rendered.extend(options.args.clone());
        (command, rendered, "hermit-args".to_owned())
    };
    let status = command
        .current_dir(root)
        .env("LLVM_PROFILE_FILE", &profile_pattern)
        .env("HERMIT_BIN", &binary)
        .env("HERMIT_COVERAGE_BIN", &binary)
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .status()
        .map_err(|error| format!("run covered command: {error}"))?;

    let raw_profiles = files_with_extension(&raw_dir, "profraw")?;
    if raw_profiles.is_empty() {
        return Err(format!(
            "covered command produced no .profraw files; see {}",
            stderr_path.display()
        ));
    }
    let (profdata, cov) = llvm_tools(root)?;
    let merged = output_dir.join("coverage.profdata");
    let mut merge = Command::new(&profdata);
    merge.arg("merge").arg("-sparse");
    merge.args(&raw_profiles).arg("-o").arg(&merged);
    run_status(&mut merge, "merge LLVM profiles")?;

    let json_path = output_dir.join("coverage.json");
    llvm_export(&cov, &binary, &merged, "text", &json_path)?;
    let lcov_path = output_dir.join("coverage.lcov");
    llvm_export(&cov, &binary, &merged, "lcov", &lcov_path)?;

    let json_value: Value = serde_json::from_reader(
        File::open(&json_path).map_err(|error| format!("open {}: {error}", json_path.display()))?,
    )
    .map_err(|error| format!("parse {}: {error}", json_path.display()))?;
    let covered_lines = parse_lcov(root, &lcov_path)?;
    let covered_regions = covered_regions(root, &json_value)?;
    let (files, totals) = file_summaries(root, &json_value)?;
    let exit_code = status.code().unwrap_or(128);
    let report = CoverageReport {
        schema: SCHEMA.to_owned(),
        name: options.name,
        mode,
        command: rendered,
        exit_code,
        git_head: command_output(root, "git", &["rev-parse", "HEAD"])?,
        git_dirty: !command_output(root, "git", &["status", "--porcelain"])?.is_empty(),
        rustc: command_output(root, "rustc", &["--version"])?,
        cargo_llvm_cov: command_output(root, "cargo", &["llvm-cov", "--version"])?,
        scope_roots: PRODUCT_ROOTS
            .iter()
            .map(|root| (*root).to_owned())
            .collect(),
        totals,
        files,
        covered_lines,
        covered_regions,
    };
    write_json(&output_dir.join("summary.json"), &report)?;
    fs::write(output_dir.join("summary.md"), render_summary(&report))
        .map_err(|error| format!("write summary.md: {error}"))?;
    println!(
        "coverage: name={} exit={} lines={}/{} ({:.2}%) regions={}/{} ({:.2}%)",
        report.name,
        report.exit_code,
        report.totals.lines.covered,
        report.totals.lines.count,
        report.totals.lines.percent,
        report.totals.regions.covered,
        report.totals.regions.count,
        report.totals.regions.percent,
    );
    println!("report: {}", output_dir.join("summary.md").display());
    Ok((report, status))
}

fn absolutize(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn files_with_extension(directory: &Path, extension: &str) -> Result<Vec<PathBuf>, String> {
    let mut paths = fs::read_dir(directory)
        .map_err(|error| format!("read {}: {error}", directory.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some(extension))
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

fn run_status(command: &mut Command, description: &str) -> Result<(), String> {
    let status = command
        .status()
        .map_err(|error| format!("{description}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{description} failed with {status}"))
    }
}

fn llvm_export(
    llvm_cov: &Path,
    binary: &Path,
    profdata: &Path,
    format: &str,
    output: &Path,
) -> Result<(), String> {
    let file =
        File::create(output).map_err(|error| format!("create {}: {error}", output.display()))?;
    let status = Command::new(llvm_cov)
        .arg("export")
        .arg(binary)
        .arg(format!("-instr-profile={}", profdata.display()))
        .arg(format!("-format={format}"))
        .stdout(Stdio::from(file))
        .status()
        .map_err(|error| format!("export LLVM {format}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("export LLVM {format} failed with {status}"))
    }
}

fn normalize_source_path(root: &Path, source: &str) -> Option<String> {
    let source = Path::new(source);
    let relative = if source.is_absolute() {
        source.strip_prefix(root).ok()?
    } else {
        source
    };
    let normalized = relative.to_string_lossy().replace('\\', "/");
    let first = normalized.split('/').next()?;
    PRODUCT_ROOTS.contains(&first).then_some(normalized)
}

fn parse_lcov(root: &Path, path: &Path) -> Result<BTreeMap<String, BTreeSet<u64>>, String> {
    let file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    parse_lcov_reader(root, BufReader::new(file))
}

fn parse_lcov_reader(
    root: &Path,
    reader: impl BufRead,
) -> Result<BTreeMap<String, BTreeSet<u64>>, String> {
    let mut result = BTreeMap::<String, BTreeSet<u64>>::new();
    let mut current = None;
    for line in reader.lines() {
        let line = line.map_err(|error| format!("read LCOV: {error}"))?;
        if let Some(source) = line.strip_prefix("SF:") {
            current = normalize_source_path(root, source);
        } else if let (Some(path), Some(row)) = (current.as_ref(), line.strip_prefix("DA:")) {
            let mut fields = row.split(',');
            let line_number = fields
                .next()
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or_else(|| format!("invalid LCOV DA row: {line}"))?;
            let count = fields
                .next()
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or_else(|| format!("invalid LCOV DA row: {line}"))?;
            if count > 0 {
                result.entry(path.clone()).or_default().insert(line_number);
            }
        } else if line == "end_of_record" {
            current = None;
        }
    }
    Ok(result)
}

fn covered_regions(
    root: &Path,
    document: &Value,
) -> Result<BTreeMap<String, BTreeSet<String>>, String> {
    let functions = document
        .pointer("/data/0/functions")
        .and_then(Value::as_array)
        .ok_or_else(|| "coverage JSON lacks data[0].functions".to_owned())?;
    let mut result = BTreeMap::<String, BTreeSet<String>>::new();
    for function in functions {
        let Some(filenames) = function.get("filenames").and_then(Value::as_array) else {
            continue;
        };
        let Some(regions) = function.get("regions").and_then(Value::as_array) else {
            continue;
        };
        for region in regions {
            let Some(fields) = region.as_array() else {
                continue;
            };
            if fields.len() < 8 || fields[4].as_u64().unwrap_or(0) == 0 {
                continue;
            }
            let file_id = fields[5].as_u64().unwrap_or(0) as usize;
            let Some(filename) = filenames.get(file_id).and_then(Value::as_str) else {
                continue;
            };
            let Some(path) = normalize_source_path(root, filename) else {
                continue;
            };
            let coordinate = format!(
                "{}:{}-{}:{}#{}",
                fields[0].as_u64().unwrap_or(0),
                fields[1].as_u64().unwrap_or(0),
                fields[2].as_u64().unwrap_or(0),
                fields[3].as_u64().unwrap_or(0),
                fields[7].as_u64().unwrap_or(0),
            );
            result.entry(path).or_default().insert(coordinate);
        }
    }
    Ok(result)
}

fn file_summaries(root: &Path, document: &Value) -> Result<(Vec<FileCoverage>, Totals), String> {
    let values = document
        .pointer("/data/0/files")
        .and_then(Value::as_array)
        .ok_or_else(|| "coverage JSON lacks data[0].files".to_owned())?;
    let mut files = Vec::new();
    let mut totals = Totals::default();
    for value in values {
        let Some(filename) = value.get("filename").and_then(Value::as_str) else {
            continue;
        };
        let Some(path) = normalize_source_path(root, filename) else {
            continue;
        };
        let summary = value
            .get("summary")
            .ok_or_else(|| format!("coverage summary missing for {path}"))?;
        let lines = parse_metric(summary, "lines")?;
        let regions = parse_metric(summary, "regions")?;
        let functions = parse_metric(summary, "functions")?;
        totals.files += 1;
        totals.lines.add(lines.count, lines.covered);
        totals.regions.add(regions.count, regions.covered);
        totals.functions.add(functions.count, functions.covered);
        files.push(FileCoverage {
            path,
            lines,
            regions,
            functions,
        });
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok((files, totals))
}

fn parse_metric(summary: &Value, name: &str) -> Result<Metric, String> {
    let value = summary
        .get(name)
        .ok_or_else(|| format!("coverage summary lacks {name}"))?;
    let count = value
        .get("count")
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("coverage {name} lacks count"))?;
    let covered = value
        .get("covered")
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("coverage {name} lacks covered"))?;
    Ok(Metric {
        count,
        covered,
        percent: percentage(covered, count),
    })
}

fn percentage(covered: u64, count: u64) -> f64 {
    if count == 0 {
        100.0
    } else {
        covered as f64 * 100.0 / count as f64
    }
}

fn render_summary(report: &CoverageReport) -> String {
    let mut output = format!(
        "# Hermit code coverage: `{}`\n\n\
         - Exit code: `{}`\n\
         - Git head: `{}`{}\n\
         - Mode: `{}`\n\
         - Command: `{}`\n\n\
         | Metric | Covered | Total | Percent |\n\
         | --- | ---: | ---: | ---: |\n\
         | Lines | {} | {} | {:.2}% |\n\
         | Regions | {} | {} | {:.2}% |\n\
         | Functions | {} | {} | {:.2}% |\n\n\
         ## Files with covered lines\n\n\
         | File | Lines | Regions |\n\
         | --- | ---: | ---: |\n",
        report.name,
        report.exit_code,
        report.git_head,
        if report.git_dirty { " (dirty)" } else { "" },
        report.mode,
        report.command.join(" ").replace('`', "\\`"),
        report.totals.lines.covered,
        report.totals.lines.count,
        report.totals.lines.percent,
        report.totals.regions.covered,
        report.totals.regions.count,
        report.totals.regions.percent,
        report.totals.functions.covered,
        report.totals.functions.count,
        report.totals.functions.percent,
    );
    let mut files = report
        .files
        .iter()
        .filter(|file| file.lines.covered > 0)
        .collect::<Vec<_>>();
    files.sort_by(|left, right| {
        right
            .lines
            .covered
            .cmp(&left.lines.covered)
            .then_with(|| left.path.cmp(&right.path))
    });
    for file in files.into_iter().take(80) {
        output.push_str(&format!(
            "| `{}` | {}/{} | {}/{} |\n",
            file.path,
            file.lines.covered,
            file.lines.count,
            file.regions.covered,
            file.regions.count,
        ));
    }
    output
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let mut serialized = serde_json::to_string_pretty(value)
        .map_err(|error| format!("serialize {}: {error}", path.display()))?;
    serialized.push('\n');
    fs::write(path, serialized).map_err(|error| format!("write {}: {error}", path.display()))
}

fn read_report(path: &Path) -> Result<CoverageReport, String> {
    serde_json::from_reader(
        File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?,
    )
    .map_err(|error| format!("parse {}: {error}", path.display()))
}

fn diff_reports(root: &Path, options: DiffOptions) -> Result<(CoverageDiff, PathBuf), String> {
    let output_root = absolutize(root, &options.output_root);
    let baseline = read_report(&output_root.join(&options.baseline).join("summary.json"))?;
    let candidate = read_report(&output_root.join(&options.candidate).join("summary.json"))?;
    let lost_lines = map_difference(&baseline.covered_lines, &candidate.covered_lines);
    let gained_lines = map_difference(&candidate.covered_lines, &baseline.covered_lines);
    let lost_regions = map_difference(&baseline.covered_regions, &candidate.covered_regions);
    let gained_regions = map_difference(&candidate.covered_regions, &baseline.covered_regions);
    let baseline_lines = set_count(&baseline.covered_lines);
    let candidate_lines = set_count(&candidate.covered_lines);
    let baseline_regions = set_count(&baseline.covered_regions);
    let candidate_regions = set_count(&candidate.covered_regions);
    let lost_line_count = set_count(&lost_lines);
    let lost_region_count = set_count(&lost_regions);
    let coverage_preserved = lost_line_count == 0 && lost_region_count == 0;
    let diff = CoverageDiff {
        schema: SCHEMA.to_owned(),
        baseline: options.baseline.clone(),
        candidate: options.candidate.clone(),
        baseline_command: baseline.command,
        candidate_command: candidate.command,
        baseline_totals: baseline.totals,
        candidate_totals: candidate.totals,
        lost_lines,
        gained_lines,
        lost_regions,
        gained_regions,
        baseline_covered_line_set: baseline_lines,
        candidate_covered_line_set: candidate_lines,
        baseline_covered_region_set: baseline_regions,
        candidate_covered_region_set: candidate_regions,
        preserved_line_percent: percentage(baseline_lines - lost_line_count, baseline_lines),
        preserved_region_percent: percentage(
            baseline_regions - lost_region_count,
            baseline_regions,
        ),
        coverage_preserved,
    };
    let diff_dir = output_root.join("diffs");
    fs::create_dir_all(&diff_dir)
        .map_err(|error| format!("create {}: {error}", diff_dir.display()))?;
    let stem = format!("{}-vs-{}", options.baseline, options.candidate);
    let json_path = diff_dir.join(format!("{stem}.json"));
    let markdown_path = diff_dir.join(format!("{stem}.md"));
    write_json(&json_path, &diff)?;
    fs::write(&markdown_path, render_diff(&diff))
        .map_err(|error| format!("write {}: {error}", markdown_path.display()))?;
    println!(
        "coverage-diff: baseline={} candidate={} preserved={} lost-lines={} lost-regions={}",
        diff.baseline,
        diff.candidate,
        diff.coverage_preserved,
        set_count(&diff.lost_lines),
        set_count(&diff.lost_regions),
    );
    println!("report: {}", markdown_path.display());
    if options.fail_on_loss && !diff.coverage_preserved {
        eprintln!("coverage-diff: --fail-on-loss detected lost baseline coverage");
    }
    Ok((diff, markdown_path))
}

fn map_difference<T: Ord + Clone>(
    left: &BTreeMap<String, BTreeSet<T>>,
    right: &BTreeMap<String, BTreeSet<T>>,
) -> BTreeMap<String, BTreeSet<T>> {
    let mut result = BTreeMap::new();
    for (path, values) in left {
        let empty = BTreeSet::new();
        let other = right.get(path).unwrap_or(&empty);
        let difference = values.difference(other).cloned().collect::<BTreeSet<_>>();
        if !difference.is_empty() {
            result.insert(path.clone(), difference);
        }
    }
    result
}

fn set_count<T>(map: &BTreeMap<String, BTreeSet<T>>) -> u64 {
    map.values().map(|values| values.len() as u64).sum()
}

fn render_diff(diff: &CoverageDiff) -> String {
    let mut output = format!(
        "# Hermit code coverage diff: `{}` vs `{}`\n\n\
         - Coverage preserved: **{}**\n\
         - Covered-line set preserved: {:.2}%\n\
         - Covered-region set preserved: {:.2}%\n\n\
         | Metric | Baseline | Candidate | Lost | Gained |\n\
         | --- | ---: | ---: | ---: | ---: |\n\
         | Covered source lines | {} | {} | {} | {} |\n\
         | Covered source regions | {} | {} | {} | {} |\n\n",
        diff.baseline,
        diff.candidate,
        diff.coverage_preserved,
        diff.preserved_line_percent,
        diff.preserved_region_percent,
        diff.baseline_covered_line_set,
        diff.candidate_covered_line_set,
        set_count(&diff.lost_lines),
        set_count(&diff.gained_lines),
        diff.baseline_covered_region_set,
        diff.candidate_covered_region_set,
        set_count(&diff.lost_regions),
        set_count(&diff.gained_regions),
    );
    render_line_map(&mut output, "Lost covered lines", &diff.lost_lines, 100);
    render_line_map(&mut output, "Gained covered lines", &diff.gained_lines, 100);
    render_region_map(
        &mut output,
        "Lost covered source regions",
        &diff.lost_regions,
        100,
    );
    render_region_map(
        &mut output,
        "Gained covered source regions",
        &diff.gained_regions,
        100,
    );
    output
}

fn render_line_map(
    output: &mut String,
    heading: &str,
    map: &BTreeMap<String, BTreeSet<u64>>,
    limit: usize,
) {
    output.push_str(&format!("## {heading}\n\n"));
    if map.is_empty() {
        output.push_str("None.\n\n");
        return;
    }
    let mut shown = 0;
    for (path, lines) in map {
        if shown >= limit {
            break;
        }
        output.push_str(&format!("- `{path}`: {}\n", compact_lines(lines)));
        shown += 1;
    }
    if map.len() > shown {
        output.push_str(&format!(
            "- … {} more files (see JSON)\n",
            map.len() - shown
        ));
    }
    output.push('\n');
}

fn compact_lines(lines: &BTreeSet<u64>) -> String {
    let mut ranges = Vec::new();
    let mut iter = lines.iter().copied();
    let Some(mut start) = iter.next() else {
        return String::new();
    };
    let mut end = start;
    for line in iter {
        if line == end + 1 {
            end = line;
        } else {
            ranges.push((start, end));
            start = line;
            end = line;
        }
    }
    ranges.push((start, end));
    ranges
        .into_iter()
        .map(|(start, end)| {
            if start == end {
                start.to_string()
            } else {
                format!("{start}-{end}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_region_map(
    output: &mut String,
    heading: &str,
    map: &BTreeMap<String, BTreeSet<String>>,
    limit: usize,
) {
    output.push_str(&format!("## {heading}\n\n"));
    if map.is_empty() {
        output.push_str("None.\n\n");
        return;
    }
    let mut shown = 0;
    let total = set_count(map) as usize;
    for (path, regions) in map {
        for region in regions {
            if shown >= limit {
                break;
            }
            output.push_str(&format!("- `{path}:{region}`\n"));
            shown += 1;
        }
        if shown >= limit {
            break;
        }
    }
    if total > shown {
        output.push_str(&format!("- … {} more regions (see JSON)\n", total - shown));
    }
    output.push('\n');
}

fn exit_code(status: ExitStatus) -> ExitCode {
    if status.success() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(status.code().unwrap_or(1).clamp(1, 125) as u8)
    }
}

fn run() -> Result<ExitCode, String> {
    let action = parse_args()?;
    if matches!(action, Action::Help) {
        println!("{}", usage());
        return Ok(ExitCode::SUCCESS);
    }
    let root = repo_root()?;
    match action {
        Action::Help => unreachable!(),
        Action::Prepare => {
            let binary = prepare(&root)?;
            println!("instrumented-hermit: {}", binary.display());
            Ok(ExitCode::SUCCESS)
        }
        Action::Collect(options) => {
            let (_, status) = collect(&root, options)?;
            Ok(exit_code(status))
        }
        Action::Diff(options) => {
            let fail_on_loss = options.fail_on_loss;
            let (diff, _) = diff_reports(&root, options)?;
            if fail_on_loss && !diff.coverage_preserved {
                Ok(ExitCode::FAILURE)
            } else {
                Ok(ExitCode::SUCCESS)
            }
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("hermit-code-coverage: {error}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn validates_report_names() {
        assert!(validate_name("echo-original_1.0").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("../escape").is_err());
        assert!(validate_name("has space").is_err());
    }

    #[test]
    fn parses_only_covered_product_lines() {
        let root = Path::new("/repo");
        let lcov = "SF:/repo/detcore/src/lib.rs\nDA:10,1\nDA:11,0\nend_of_record\n\
                    SF:/outside/dependency.rs\nDA:2,9\nend_of_record\n";
        let parsed = parse_lcov_reader(root, Cursor::new(lcov)).unwrap();
        assert_eq!(
            parsed.get("detcore/src/lib.rs").unwrap(),
            &BTreeSet::from([10])
        );
        assert_eq!(parsed.len(), 1);
    }

    #[test]
    fn computes_per_file_set_differences() {
        let left = BTreeMap::from([
            ("a.rs".to_owned(), BTreeSet::from([1, 2, 3])),
            ("b.rs".to_owned(), BTreeSet::from([9])),
        ]);
        let right = BTreeMap::from([
            ("a.rs".to_owned(), BTreeSet::from([2, 3, 4])),
            ("b.rs".to_owned(), BTreeSet::from([9])),
        ]);
        assert_eq!(
            map_difference(&left, &right),
            BTreeMap::from([("a.rs".to_owned(), BTreeSet::from([1]))])
        );
        assert_eq!(set_count(&left), 4);
    }

    #[test]
    fn compacts_adjacent_line_numbers() {
        assert_eq!(
            compact_lines(&BTreeSet::from([1, 2, 3, 7, 9, 10])),
            "1-3, 7, 9-10"
        );
    }

    #[test]
    fn parses_cargo_llvm_cov_exports() {
        let parsed = parse_coverage_environment(
            "export LLVM_PROFILE_FILE='/repo/target/hermit-%p.profraw'\n\
             export CARGO_LLVM_COV=1\n\
             export __CARGO_LLVM_COV_RUSTC_WRAPPER_CRATE_NAMES=detcore,hermit\n",
        )
        .unwrap();
        assert_eq!(
            parsed.get("LLVM_PROFILE_FILE").unwrap(),
            "/repo/target/hermit-%p.profraw"
        );
        assert_eq!(parsed.get("CARGO_LLVM_COV").unwrap(), "1");
        assert_eq!(
            parsed
                .get("__CARGO_LLVM_COV_RUSTC_WRAPPER_CRATE_NAMES")
                .unwrap(),
            "detcore,hermit"
        );
    }
}
