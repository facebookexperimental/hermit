// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! Generate `ci/test-footprints.json` from Cargo metadata and the portable DAG.
//!
//! Cargo owns package paths and dependency edges. The portable DAG owns test
//! nodes and commands. `ci/test-footprints-policy.json` contains only the
//! fail-safe path policy and semantic edges for non-Cargo harnesses.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;

use serde_json::Map;
use serde_json::Value;
use serde_json::json;

const POLICY: &str = "ci/test-footprints-policy.json";
const OUTPUT: &str = "ci/test-footprints.json";
const DAG: &str = "ci/dag/portable.json";
const REGENERATE: &str = "cargo run -p hermit-manifest-plan --bin generate-test-footprints";

#[derive(Debug)]
struct Package {
    name: String,
    root: String,
    dependencies: BTreeSet<String>,
}

#[derive(Default)]
struct Rule {
    nodes: BTreeSet<String>,
    e2e_all: bool,
    e2e_backends: BTreeSet<String>,
}

fn die(message: impl AsRef<str>) -> ! {
    eprintln!("generate-test-footprints: {}", message.as_ref());
    std::process::exit(2);
}

fn read_json(path: &Path) -> Value {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|error| die(format!("cannot read {}: {error}", path.display())));
    serde_json::from_str(&text)
        .unwrap_or_else(|error| die(format!("invalid JSON in {}: {error}", path.display())))
}

fn strings(value: &Value, key: &str, location: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .unwrap_or_else(|| die(format!("{location}: `{key}` must be an array")))
        .iter()
        .map(|item| {
            item.as_str()
                .filter(|item| !item.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    die(format!(
                        "{location}: `{key}` entries must be non-empty strings"
                    ))
                })
        })
        .collect()
}

fn repo_root() -> PathBuf {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .unwrap_or_else(|error| die(format!("failed to run git: {error}")));
    if !output.status.success() {
        die(format!(
            "git rev-parse failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    PathBuf::from(String::from_utf8_lossy(&output.stdout).trim())
}

fn cargo_metadata(root: &Path) -> Value {
    let output = Command::new("cargo")
        .current_dir(root)
        .args(["metadata", "--format-version", "1", "--no-deps", "--locked"])
        .output()
        .unwrap_or_else(|error| die(format!("failed to run cargo metadata: {error}")));
    if !output.status.success() {
        die(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| die(format!("cargo metadata returned invalid JSON: {error}")))
}

fn relative_dir(root: &Path, manifest: &str) -> String {
    let manifest = Path::new(manifest);
    let directory = manifest
        .parent()
        .unwrap_or_else(|| die(format!("manifest has no parent: {manifest:?}")));
    directory
        .strip_prefix(root)
        .unwrap_or_else(|_| {
            die(format!(
                "workspace manifest {} is outside repository {}",
                manifest.display(),
                root.display()
            ))
        })
        .to_string_lossy()
        .replace('\\', "/")
}

fn load_packages(root: &Path, metadata: &Value) -> (BTreeMap<String, Package>, BTreeSet<String>) {
    let members: BTreeSet<String> = strings(metadata, "workspace_members", "cargo metadata")
        .into_iter()
        .collect();
    let defaults: BTreeSet<String> =
        strings(metadata, "workspace_default_members", "cargo metadata")
            .into_iter()
            .collect();

    let raw_packages = metadata["packages"]
        .as_array()
        .unwrap_or_else(|| die("cargo metadata: `packages` must be an array"));
    let mut id_to_name = BTreeMap::new();
    let mut path_to_name = BTreeMap::new();
    for package in raw_packages {
        let id = package["id"]
            .as_str()
            .unwrap_or_else(|| die("cargo metadata package is missing `id`"));
        if !members.contains(id) {
            continue;
        }
        let name = package["name"]
            .as_str()
            .unwrap_or_else(|| die("cargo metadata package is missing `name`"));
        let manifest = package["manifest_path"]
            .as_str()
            .unwrap_or_else(|| die(format!("package {name} is missing `manifest_path`")));
        let package_root = Path::new(manifest)
            .parent()
            .unwrap_or_else(|| die(format!("package {name} manifest has no parent")))
            .to_path_buf();
        if path_to_name.insert(package_root, name.to_owned()).is_some() {
            die(format!(
                "multiple workspace packages named or rooted at {name}"
            ));
        }
        id_to_name.insert(id.to_owned(), name.to_owned());
    }

    let mut packages = BTreeMap::new();
    for package in raw_packages {
        let id = package["id"]
            .as_str()
            .unwrap_or_else(|| die("cargo metadata package is missing `id`"));
        let Some(name) = id_to_name.get(id) else {
            continue;
        };
        if packages.contains_key(name) {
            die(format!("workspace package name is not unique: {name}"));
        }
        let manifest = package["manifest_path"]
            .as_str()
            .unwrap_or_else(|| die(format!("package {name} is missing `manifest_path`")));
        let mut dependencies = BTreeSet::new();
        for dependency in package["dependencies"].as_array().into_iter().flatten() {
            let Some(path) = dependency["path"].as_str() else {
                continue;
            };
            if let Some(dependency_name) = path_to_name.get(Path::new(path)) {
                dependencies.insert(dependency_name.clone());
            }
        }
        packages.insert(
            name.clone(),
            Package {
                name: name.clone(),
                root: relative_dir(root, manifest),
                dependencies,
            },
        );
    }

    let default_names = defaults
        .iter()
        .map(|id| {
            id_to_name
                .get(id)
                .cloned()
                .unwrap_or_else(|| die(format!("default workspace member is unknown: {id}")))
        })
        .collect();
    (packages, default_names)
}

fn reverse_dependency_closure(
    owner: &str,
    packages: &BTreeMap<String, Package>,
) -> BTreeSet<String> {
    let mut closure = BTreeSet::from([owner.to_owned()]);
    loop {
        let mut changed = false;
        for package in packages.values() {
            if package
                .dependencies
                .iter()
                .any(|dependency| closure.contains(dependency))
            {
                changed |= closure.insert(package.name.clone());
            }
        }
        if !changed {
            return closure;
        }
    }
}

fn shell_tokens(command: &str) -> Vec<String> {
    command
        .split_whitespace()
        .map(|token| token.trim_matches(|ch| ch == '\'' || ch == '"').to_owned())
        .collect()
}

fn cargo_command_packages(
    command: &str,
    all: &BTreeSet<String>,
    defaults: &BTreeSet<String>,
) -> BTreeSet<String> {
    let tokens = shell_tokens(command);
    let mut result = BTreeSet::new();
    let mut command_index = 0;
    while command_index < tokens.len() {
        let counted_nextest = tokens[command_index].ends_with("/ci/run-nextest-counted.sh");
        if tokens[command_index] != "cargo" && !counted_nextest {
            command_index += 1;
            continue;
        }
        let subcommand = tokens
            .get(command_index + 1)
            .map(String::as_str)
            .unwrap_or_default();
        let recognized = counted_nextest
            || matches!(subcommand, "build" | "test" | "clippy" | "fmt" | "doc")
            || (subcommand == "nextest"
                && tokens.get(command_index + 2).map(String::as_str) == Some("run"));
        if !recognized {
            command_index += 1;
            continue;
        }

        let mut workspace = subcommand == "fmt";
        let mut explicit = BTreeSet::new();
        let mut excluded = BTreeSet::new();
        let mut cursor = command_index + if counted_nextest { 1 } else { 2 };
        while cursor < tokens.len() && !matches!(tokens[cursor].as_str(), "&&" | "||" | ";") {
            let token = tokens[cursor].trim_end_matches(';');
            match token {
                "--workspace" => workspace = true,
                "-p" | "--package" => {
                    cursor += 1;
                    if let Some(package) = tokens.get(cursor) {
                        explicit.insert(package.trim_end_matches(';').to_owned());
                    }
                }
                "--exclude" => {
                    cursor += 1;
                    if let Some(package) = tokens.get(cursor) {
                        excluded.insert(package.trim_end_matches(';').to_owned());
                    }
                }
                _ => {
                    if let Some(package) = token.strip_prefix("--package=") {
                        explicit.insert(package.to_owned());
                    } else if let Some(package) = token.strip_prefix("--exclude=") {
                        excluded.insert(package.to_owned());
                    } else if token == "--all" && subcommand == "fmt" {
                        workspace = true;
                    }
                }
            }
            cursor += 1;
        }
        let mut targets = if !explicit.is_empty() {
            explicit
        } else if workspace {
            all.clone()
        } else {
            defaults.clone()
        };
        targets.retain(|package| !excluded.contains(package));
        for package in &targets {
            if !all.contains(package) {
                die(format!(
                    "portable DAG command names non-workspace package `{package}`: {command}"
                ));
            }
        }
        result.extend(targets);
        command_index = cursor.max(command_index + 1);
    }
    result
}

fn load_dag_targets(
    dag: &Value,
    packages: &BTreeMap<String, Package>,
    defaults: &BTreeSet<String>,
) -> (BTreeSet<String>, BTreeMap<String, BTreeSet<String>>) {
    let all_packages: BTreeSet<String> = packages.keys().cloned().collect();
    let steps = dag["steps"]
        .as_array()
        .unwrap_or_else(|| die(format!("{DAG}: `steps` must be an array")));
    let mut all_nodes = BTreeSet::new();
    let mut targets = BTreeMap::new();
    for step in steps {
        let group = step["group"]
            .as_str()
            .unwrap_or_else(|| die(format!("{DAG}: step is missing `group`")));
        let job = step["job"]
            .as_str()
            .unwrap_or_else(|| die(format!("{DAG}: step is missing `job`")));
        let command = step["cmd"]
            .as_str()
            .unwrap_or_else(|| die(format!("{DAG}: {group}.{job} is missing `cmd`")));
        let node = format!("{group}.{job}");
        if !all_nodes.insert(node.clone()) {
            die(format!("{DAG}: duplicate node {node}"));
        }
        targets.insert(
            node,
            cargo_command_packages(command, &all_packages, defaults),
        );
    }
    (all_nodes, targets)
}

fn load_rules(
    policy: &Value,
    packages: &BTreeMap<String, Package>,
    nodes: &BTreeSet<String>,
) -> BTreeMap<String, Rule> {
    let mut rules: BTreeMap<String, Rule> = BTreeMap::new();
    for (index, raw_rule) in policy["package_rules"]
        .as_array()
        .unwrap_or_else(|| die(format!("{POLICY}: `package_rules` must be an array")))
        .iter()
        .enumerate()
    {
        let location = format!("{POLICY}: package_rules[{index}]");
        let rule_nodes = strings(raw_rule, "nodes", &location);
        for node in &rule_nodes {
            if !nodes.contains(node) {
                die(format!("{location}: unknown portable DAG node `{node}`"));
            }
        }
        for package in strings(raw_rule, "packages", &location) {
            if !packages.contains_key(&package) {
                die(format!("{location}: unknown workspace package `{package}`"));
            }
            let rule = rules.entry(package).or_default();
            rule.nodes.extend(rule_nodes.iter().cloned());
            rule.e2e_all |= raw_rule["e2e_all"].as_bool().unwrap_or(false);
            rule.e2e_backends.extend(
                raw_rule["e2e_backends"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|value| {
                        value
                            .as_str()
                            .unwrap_or_else(|| die(format!("{location}: invalid e2e backend")))
                            .to_owned()
                    }),
            );
        }
    }
    rules
}

fn validate_path_footprints(policy: &Value, nodes: &BTreeSet<String>) {
    for (index, footprint) in policy["path_footprints"]
        .as_array()
        .unwrap_or_else(|| die(format!("{POLICY}: `path_footprints` must be an array")))
        .iter()
        .enumerate()
    {
        let location = format!("{POLICY}: path_footprints[{index}]");
        let _ = strings(footprint, "paths", &location);
        for node in strings(footprint, "nodes", &location) {
            if !nodes.contains(&node) {
                die(format!("{location}: unknown portable DAG node `{node}`"));
            }
        }
    }
}

fn load_package_paths(
    policy: &Value,
    packages: &BTreeMap<String, Package>,
) -> BTreeMap<String, BTreeSet<String>> {
    let raw = policy["package_paths"]
        .as_object()
        .unwrap_or_else(|| die(format!("{POLICY}: `package_paths` must be an object")));
    let mut result = BTreeMap::new();
    for (package_name, value) in raw {
        let package = packages.get(package_name).unwrap_or_else(|| {
            die(format!(
                "{POLICY}: package_paths names unknown workspace package `{package_name}`"
            ))
        });
        let location = format!("{POLICY}: package_paths.{package_name}");
        let paths: BTreeSet<String> = value
            .as_array()
            .unwrap_or_else(|| die(format!("{location} must be an array")))
            .iter()
            .map(|path| {
                path.as_str()
                    .filter(|path| !path.is_empty())
                    .unwrap_or_else(|| die(format!("{location} entries must be non-empty strings")))
                    .to_owned()
            })
            .collect();
        let root_prefix = format!("{}/", package.root);
        for path in &paths {
            if path != &package.root && !path.starts_with(&root_prefix) {
                die(format!(
                    "{location}: `{path}` must stay below package root `{}`",
                    package.root
                ));
            }
        }
        result.insert(package_name.clone(), paths);
    }
    result
}

fn generated_footprints(root: &Path) -> Value {
    let metadata = cargo_metadata(root);
    let policy = read_json(&root.join(POLICY));
    if policy["version"].as_u64() != Some(1) {
        die(format!("{POLICY}: expected version 1"));
    }
    let dag = read_json(&root.join(DAG));
    let (packages, defaults) = load_packages(root, &metadata);
    let (all_nodes, dag_targets) = load_dag_targets(&dag, &packages, &defaults);
    let rules = load_rules(&policy, &packages, &all_nodes);
    let package_paths = load_package_paths(&policy, &packages);
    validate_path_footprints(&policy, &all_nodes);

    let mut package_footprints = Vec::new();
    let mut by_root: Vec<&Package> = packages.values().collect();
    by_root.sort_by(|left, right| left.root.cmp(&right.root).then(left.name.cmp(&right.name)));
    for package in by_root {
        let reverse = reverse_dependency_closure(&package.name, &packages);
        let mut selected_nodes = BTreeSet::new();
        for (node, targets) in &dag_targets {
            if !targets.is_disjoint(&reverse) {
                selected_nodes.insert(node.clone());
            }
        }
        let rule = rules.get(&package.name);
        if let Some(rule) = rule {
            selected_nodes.extend(rule.nodes.iter().cloned());
        }

        let mut footprint = Map::new();
        footprint.insert(
            "_why".into(),
            Value::String(format!(
                "@generated: Cargo package `{}`; reverse-dependent closure: {}.",
                package.name,
                reverse.iter().cloned().collect::<Vec<_>>().join(", ")
            )),
        );
        let mut paths = BTreeSet::from([
            format!("{}/**/*.rs", package.root),
            format!("{}/Cargo.toml", package.root),
        ]);
        if let Some(extra_paths) = package_paths.get(&package.name) {
            paths.extend(extra_paths.iter().cloned());
        }
        footprint.insert(
            "paths".into(),
            Value::Array(paths.into_iter().map(Value::String).collect()),
        );
        footprint.insert(
            "nodes".into(),
            Value::Array(selected_nodes.into_iter().map(Value::String).collect()),
        );
        if rule.is_some_and(|rule| rule.e2e_all) {
            footprint.insert("e2e_all".into(), Value::Bool(true));
        }
        if let Some(rule) = rule.filter(|rule| !rule.e2e_backends.is_empty()) {
            footprint.insert(
                "e2e_backends".into(),
                Value::Array(
                    rule.e2e_backends
                        .iter()
                        .cloned()
                        .map(Value::String)
                        .collect(),
                ),
            );
        }
        package_footprints.push(Value::Object(footprint));
    }

    package_footprints.extend(
        policy["path_footprints"]
            .as_array()
            .expect("validated path_footprints")
            .iter()
            .cloned(),
    );

    json!({
        "_comment": [
            "@generated by ci/manifest-plan/src/bin/generate-test-footprints.rs; do not edit.",
            format!("Regenerate: {REGENERATE}"),
            "",
            "Cargo metadata supplies package roots and local dependency edges. For each owning",
            "package, the generator computes its reverse-dependency closure, then selects every",
            "portable DAG Cargo command whose package set intersects that closure. Non-Cargo",
            "harness edges and fail-safe path policy come from ci/test-footprints-policy.json.",
            "",
            "Safety is unchanged: force_full and unknown paths run everything. CI is skipped",
            "only when every changed path is explicitly ci_irrelevant and no footprint matches."
        ],
        "version": 2,
        "groups": {},
        "force_full": policy["force_full"].clone(),
        "ci_irrelevant": policy["ci_irrelevant"].clone(),
        "footprints": package_footprints
    })
}

fn render(root: &Path) -> String {
    let mut rendered = serde_json::to_string_pretty(&generated_footprints(root))
        .unwrap_or_else(|error| die(format!("cannot serialize generated footprints: {error}")));
    rendered.push('\n');
    rendered
}

fn main() -> ExitCode {
    let root = repo_root();
    let generated = render(&root);
    let mut check = false;
    let mut stdout = false;
    for argument in env::args().skip(1) {
        match argument.as_str() {
            "--check" => check = true,
            "--stdout" => stdout = true,
            "-h" | "--help" => {
                println!(
                    "Usage: generate-test-footprints [--check | --stdout]\n\n\
                     With no option, writes {OUTPUT}. --check fails if it is stale."
                );
                return ExitCode::SUCCESS;
            }
            _ => die(format!("unknown argument `{argument}`")),
        }
    }
    if check && stdout {
        die("--check and --stdout are mutually exclusive");
    }
    if stdout {
        print!("{generated}");
        return ExitCode::SUCCESS;
    }
    let output = root.join(OUTPUT);
    if check {
        let current = fs::read_to_string(&output).unwrap_or_default();
        if current != generated {
            eprintln!("generate-test-footprints: {OUTPUT} is stale; run `{REGENERATE}`");
            return ExitCode::FAILURE;
        }
        println!("generate-test-footprints: {OUTPUT} is current");
        return ExitCode::SUCCESS;
    }
    fs::write(&output, generated)
        .unwrap_or_else(|error| die(format!("cannot write {}: {error}", output.display())));
    println!("generate-test-footprints: wrote {OUTPUT}");
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(name: &str, dependencies: &[&str]) -> Package {
        Package {
            name: name.to_owned(),
            root: name.to_owned(),
            dependencies: dependencies.iter().map(|item| (*item).to_owned()).collect(),
        }
    }

    #[test]
    fn computes_transitive_reverse_dependency_closure() {
        let packages = BTreeMap::from([
            ("base".into(), package("base", &[])),
            ("middle".into(), package("middle", &["base"])),
            ("top".into(), package("top", &["middle"])),
            ("other".into(), package("other", &[])),
        ]);
        assert_eq!(
            reverse_dependency_closure("base", &packages),
            BTreeSet::from(["base".into(), "middle".into(), "top".into()])
        );
    }

    #[test]
    fn extracts_workspace_explicit_and_excluded_cargo_targets() {
        let all = BTreeSet::from(["a".into(), "b".into(), "c".into()]);
        let defaults = BTreeSet::from(["a".into(), "b".into()]);
        assert_eq!(
            cargo_command_packages("cargo test --workspace --exclude b", &all, &defaults),
            BTreeSet::from(["a".into(), "c".into()])
        );
        assert_eq!(
            cargo_command_packages(
                "cargo build -p b && cargo test --package c",
                &all,
                &defaults
            ),
            BTreeSet::from(["b".into(), "c".into()])
        );
        assert_eq!(
            cargo_command_packages(
                "./ci/run-with-reverie-dbt-budget.sh ./ci/run-nextest-counted.sh --workspace --exclude b",
                &all,
                &defaults
            ),
            BTreeSet::from(["a".into(), "c".into()])
        );
        assert_eq!(
            cargo_command_packages("/tmp/tree/ci/run-nextest-counted.sh -p b", &all, &defaults),
            BTreeSet::from(["b".into()])
        );
        assert!(cargo_command_packages("cargo nextest show-config", &all, &defaults).is_empty());
    }

    #[test]
    fn accepts_package_local_non_cargo_paths() {
        let packages = BTreeMap::from([("crate-a".into(), package("crate-a", &[]))]);
        let policy = json!({
            "package_paths": {
                "crate-a": ["crate-a/fixtures/**", "crate-a/scripts/*.sh"]
            }
        });
        assert_eq!(
            load_package_paths(&policy, &packages),
            BTreeMap::from([(
                "crate-a".into(),
                BTreeSet::from(["crate-a/fixtures/**".into(), "crate-a/scripts/*.sh".into()])
            )])
        );
    }
}
