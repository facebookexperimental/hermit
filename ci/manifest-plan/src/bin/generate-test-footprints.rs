// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! Generate `ci/test-footprints.json` from Cargo metadata and the hosted-portable DAG.
//!
//! Cargo owns package paths and dependency edges. The hosted-portable DAG owns test
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
const DAG: &str = "ci/dag/validate.json";
const REGENERATE: &str = "cargo run -p hermit-manifest-plan --bin generate-test-footprints";

fn known_selection_node(nodes: &BTreeSet<String>, node: &str) -> bool {
    nodes.contains(node)
        // Policy uses the public selectors shared with local validation. The
        // hosted driver resolves only an exact, present `_on_host` counterpart.
        || nodes.contains(&format!("{node}_on_host"))
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Args {
    check: bool,
    stdout: bool,
}

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
        let prepared_nextest = tokens[command_index].ends_with("/ci/nextest-binaries.rs")
            && matches!(
                tokens.get(command_index + 1).map(String::as_str),
                Some("run" | "list")
            );
        if tokens[command_index] != "cargo" && !counted_nextest && !prepared_nextest {
            command_index += 1;
            continue;
        }
        let subcommand = tokens
            .get(command_index + 1)
            .map(String::as_str)
            .unwrap_or_default();
        let recognized = counted_nextest
            || prepared_nextest
            || matches!(subcommand, "build" | "test" | "clippy" | "fmt" | "doc")
            || (subcommand == "nextest"
                && matches!(
                    tokens.get(command_index + 2).map(String::as_str),
                    Some("run" | "list")
                ));
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

fn prepared_command_packages(
    command: &str,
    graph: &dagrun::model::DagConfig,
    all: &BTreeSet<String>,
    defaults: &BTreeSet<String>,
) -> Result<BTreeSet<String>, String> {
    let tokens = shell_tokens(command);
    let mut result = BTreeSet::new();
    for (index, token) in tokens.iter().enumerate() {
        if !token.ends_with("/ci/nextest-binaries.rs")
            || tokens.get(index + 1).map(String::as_str) != Some("prepare")
        {
            continue;
        }
        let profile = tokens
            .get(index + 2)
            .ok_or("prepared command has no profile")?;
        let selections = hermit_manifest_plan::nextest_binaries::config_selections(graph, profile)?;
        for args in selections.values() {
            // These are validated Cargo build selectors from the graph. This
            // string is only parsed for package names; it is never executed.
            result.extend(cargo_command_packages(
                &format!("cargo test {}", args.join(" ")),
                all,
                defaults,
            ));
            if args
                .windows(2)
                .any(|args| args == ["--test", "hermit_modes"])
            {
                let guests = "hermetic_infra_hermit_tests";
                if !all.contains(guests) {
                    return Err("prepared hermit_modes profile has no Cargo guest package".into());
                }
                result.insert(guests.into());
            }
        }
    }
    Ok(result)
}

fn load_dag_targets(
    dag: &Value,
    packages: &BTreeMap<String, Package>,
    defaults: &BTreeSet<String>,
) -> (BTreeSet<String>, BTreeMap<String, BTreeSet<String>>) {
    let all_packages: BTreeSet<String> = packages.keys().cloned().collect();
    let graph = dagrun::io::dag_from_json(&dag.to_string())
        .unwrap_or_else(|error| die(format!("{DAG}: {error}")));
    let steps = dag["steps"]
        .as_array()
        .unwrap_or_else(|| die(format!("{DAG}: `steps` must be an array")));
    let mut all_nodes = BTreeSet::new();
    let mut targets = BTreeMap::new();
    for step in steps {
        if !step["labels"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|label| label.as_str() == Some("hosted-portable"))
        {
            continue;
        }
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
        let mut selected_packages = cargo_command_packages(command, &all_packages, defaults);
        selected_packages.extend(
            prepared_command_packages(command, &graph, &all_packages, defaults)
                .unwrap_or_else(|error| die(format!("{DAG}: {node}: {error}"))),
        );
        targets.insert(node, selected_packages);
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
            if !known_selection_node(nodes, node) {
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

/// One coarse backend-affinity rule from the policy. `paths` are Hermit-tree
/// globs; `reverie_paths` are globs relative to the reverie submodule root.
#[derive(Clone)]
struct BackendRule {
    id: String,
    paths: Vec<String>,
    reverie_paths: Vec<String>,
    backends: Vec<String>,
    lanes: Vec<String>,
    extra_nodes: Vec<String>,
    tier: String,
}

fn load_backend_rules(policy: &Value, nodes: &BTreeSet<String>) -> Vec<BackendRule> {
    let raw = policy
        .get("backend_rules")
        .and_then(Value::as_array)
        .unwrap_or_else(|| die(format!("{POLICY}: `backend_rules` must be an array")));
    let mut rules = Vec::new();
    let mut ids = BTreeSet::new();
    for (index, item) in raw.iter().enumerate() {
        let location = format!("{POLICY}: backend_rules[{index}]");
        let id = item["id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| die(format!("{location}: `id` must be a non-empty string")))
            .to_owned();
        if !ids.insert(id.clone()) {
            die(format!("{location}: duplicate backend rule id `{id}`"));
        }
        let tier = item["tier"]
            .as_str()
            .unwrap_or_else(|| die(format!("{location}: `tier` must be a string")))
            .to_owned();
        if tier != "backend" && tier != "core" {
            die(format!("{location}: `tier` must be `backend` or `core`"));
        }
        let paths = strings(item, "paths", &location);
        let reverie_paths = strings(item, "reverie_paths", &location);
        if paths.is_empty() && reverie_paths.is_empty() {
            die(format!("{location}: rule needs `paths` or `reverie_paths`"));
        }
        let backends = strings(item, "backends", &location);
        if backends.is_empty() {
            die(format!("{location}: `backends` must be non-empty"));
        }
        let lanes = strings(item, "lanes", &location);
        let extra_nodes = strings(item, "extra_nodes", &location);
        for node in &extra_nodes {
            if !known_selection_node(nodes, node) {
                die(format!("{location}: unknown portable DAG node `{node}`"));
            }
        }
        rules.push(BackendRule {
            id,
            paths,
            reverie_paths,
            backends,
            lanes,
            extra_nodes,
            tier,
        });
    }
    rules
}

/// Literal directory prefix of a glob: the text before the first `*`.
fn glob_literal_prefix(glob: &str) -> &str {
    match glob.find('*') {
        Some(index) => &glob[..index],
        None => glob,
    }
}

/// Whether a backend rule's Hermit paths can match files under a package root.
fn rule_overlaps_root(rule: &BackendRule, root: &str) -> bool {
    if rule.tier != "backend" {
        return false;
    }
    rule.paths.iter().any(|glob| {
        let prefix = glob_literal_prefix(glob).trim_end_matches('/');
        prefix == root
            || prefix.starts_with(&format!("{root}/"))
            || root.starts_with(&format!("{prefix}/"))
    })
}

/// Whether a reverie rule glob covers crate directory `dir` (relative to
/// the reverie root, e.g. `reverie-kvm` or `experimental/reverie-sabre`).
fn reverie_rule_covers_dir(rule: &BackendRule, dir: &str) -> bool {
    rule.reverie_paths.iter().any(|pat| {
        let base = pat.strip_suffix("/**").unwrap_or(pat);
        match base.find('*') {
            Some(index) => dir.starts_with(&base[..index]),
            None => dir == base || dir.starts_with(&format!("{base}/")),
        }
    })
}

fn normalize_reverie_path(base: &str, rel: &str) -> String {
    let mut parts: Vec<&str> = base.split('/').filter(|p| !p.is_empty()).collect();
    for part in rel.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

/// Design choice (item 4 alternative): full `cargo metadata` derivation
/// for the reverie workspace is disproportionate here (separate workspace,
/// submodule may be absent in some checkouts), so multi-backend crates are
/// tiered `core` in the policy and this check refuses any backend-tier
/// rule whose crate's reverse-dependency closure spans backends outside
/// that rule. Closure is read from the reverie crates' own `path =`
/// dependency edges. Skipped when the submodule is not checked out.
fn check_reverie_backend_closure(root: &Path, rules: &[BackendRule]) {
    let reverie = root.join("reverie");
    if !reverie.join("Cargo.toml").exists() {
        return;
    }
    // crate dir (relative to reverie root) -> dependency crate dirs
    let mut deps: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut dirs: Vec<PathBuf> = Vec::new();
    for parent in [reverie.clone(), reverie.join("experimental")] {
        if let Ok(entries) = fs::read_dir(&parent) {
            for entry in entries.flatten() {
                if entry.path().join("Cargo.toml").exists() {
                    dirs.push(entry.path());
                }
            }
        }
    }
    // The `reverie` crate itself lives at reverie/reverie/.
    for path in dirs {
        let rel = path
            .strip_prefix(&reverie)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let text = fs::read_to_string(path.join("Cargo.toml")).unwrap_or_default();
        let mut crate_deps = BTreeSet::new();
        for needle in text.split("path = \"").skip(1) {
            if let Some(end) = needle.find('"') {
                let target = normalize_reverie_path(&rel, &needle[..end]);
                // Only sibling-crate directories count; file paths do not.
                if target.split('/').count() <= 2 && !target.contains('.') {
                    crate_deps.insert(target);
                }
            }
        }
        deps.insert(rel, crate_deps);
    }
    let backends_of = |dir: &str| -> BTreeSet<String> {
        // Experimental tools (e.g. reverie-sabre-strace) consume many
        // backends without being a backend runtime; only the sabre
        // backend crate itself under experimental/ counts.
        if dir.starts_with("experimental/") && dir != "experimental/reverie-sabre" {
            return BTreeSet::new();
        }
        rules
            .iter()
            .filter(|rule| rule.tier == "backend" && reverie_rule_covers_dir(rule, dir))
            .flat_map(|rule| rule.backends.iter().filter(|b| b.as_str() != "*").cloned())
            .collect()
    };
    for rule in rules.iter().filter(|rule| rule.tier == "backend") {
        for dir in deps.keys() {
            if !reverie_rule_covers_dir(rule, dir) {
                continue;
            }
            // Reverse closure: every crate that transitively depends on dir.
            let mut closure = BTreeSet::from([dir.clone()]);
            loop {
                let mut changed = false;
                for (crate_dir, crate_deps) in &deps {
                    if crate_deps.iter().any(|dep| closure.contains(dep)) {
                        changed |= closure.insert(crate_dir.clone());
                    }
                }
                if !changed {
                    break;
                }
            }
            let mut spanned: BTreeSet<String> = BTreeSet::new();
            for member in &closure {
                spanned.extend(backends_of(member));
            }
            let allowed: BTreeSet<String> = rule.backends.iter().cloned().collect();
            if !spanned.is_subset(&allowed) {
                die(format!(
                    "{POLICY}: backend rule `{}` covers reverie crate `{dir}`, whose \
                     reverse-dependency closure spans backends {spanned:?} outside \
                     {allowed:?}; classify the crate as tier `core` (everything) instead",
                    rule.id
                ));
            }
        }
    }
}

/// E2E bucket nodes that contain at least one DBT cell, derived from
/// `ci/expected-e2e-plan.json` (cells with `backend == "dbt"` grouped by
/// `category`). v1 has no hand-picked bucket list: if a category gains or
/// loses its last DBT cell, the derived set follows. Each bucket is emitted
/// in both its local and hosted spellings when the DAG knows either.
fn dbt_bucket_nodes(root: &Path, nodes: &BTreeSet<String>) -> Vec<String> {
    let plan = read_json(&root.join("ci/expected-e2e-plan.json"));
    let mut categories = BTreeSet::new();
    if let Some(cells) = plan["cells"].as_array() {
        for cell in cells {
            if cell["backend"].as_str() == Some("dbt") {
                if let Some(category) = cell["category"].as_str() {
                    categories.insert(category.replace('-', "_"));
                }
            }
        }
    }
    if categories.is_empty() {
        die("ci/expected-e2e-plan.json: no DBT cells; cannot derive DBT buckets");
    }
    let mut out = Vec::new();
    for category in categories {
        for node in [
            format!("e2e.manifest_{category}"),
            format!("e2e.manifest_{category}_on_host"),
        ] {
            if known_selection_node(nodes, &node) {
                out.push(node);
            }
        }
    }
    out
}

fn backend_rule_json(rule: &BackendRule) -> Value {
    json!({
        "id": rule.id,
        "paths": rule.paths,
        "reverie_paths": rule.reverie_paths,
        "backends": rule.backends,
        "lanes": rule.lanes,
        "extra_nodes": rule.extra_nodes,
        "tier": rule.tier,
    })
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
            if !known_selection_node(nodes, &node) {
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
    let mut backend_rules = load_backend_rules(&policy, &all_nodes);
    // v1: the DBT route's E2E buckets are derived, never hand-picked.
    let derived_dbt_buckets = dbt_bucket_nodes(root, &all_nodes);
    for rule in &mut backend_rules {
        if rule.id == "dbt" && rule.tier == "backend" {
            for node in &derived_dbt_buckets {
                if !rule.extra_nodes.contains(node) {
                    rule.extra_nodes.push(node.clone());
                }
            }
            rule.extra_nodes.sort();
        }
    }
    check_reverie_backend_closure(root, &backend_rules);
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
        // Coarse backend affinity: fold every overlapping backend rule's
        // backends and extra nodes into this package's footprint so a
        // backend-crate change (e.g. hermit-run-kvm) yields that backend's
        // E2E cells. Footprints that already select every backend keep
        // their `e2e_all` and gain no narrower affinity.
        let mut backend_backends: BTreeSet<String> = rule
            .map(|rule| rule.e2e_backends.clone())
            .unwrap_or_default();
        let backend_all = rule.is_some_and(|rule| rule.e2e_all);
        if !backend_all {
            for backend_rule in &backend_rules {
                if rule_overlaps_root(backend_rule, &package.root) {
                    selected_nodes.extend(backend_rule.extra_nodes.iter().cloned());
                    backend_backends.extend(backend_rule.backends.iter().cloned());
                }
            }
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
        if backend_all {
            footprint.insert("e2e_all".into(), Value::Bool(true));
        }
        if !backend_backends.is_empty() {
            footprint.insert(
                "e2e_backends".into(),
                Value::Array(backend_backends.into_iter().map(Value::String).collect()),
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
        "backend_rules": backend_rules.iter().map(backend_rule_json).collect::<Vec<_>>(),
        "footprints": package_footprints
    })
}

fn render(root: &Path) -> String {
    let mut rendered = serde_json::to_string_pretty(&generated_footprints(root))
        .unwrap_or_else(|error| die(format!("cannot serialize generated footprints: {error}")));
    rendered.push('\n');
    rendered
}

fn parse_args(arguments: impl Iterator<Item = String>) -> Option<Args> {
    let mut args = Args::default();
    for argument in arguments {
        match argument.as_str() {
            "--check" => args.check = true,
            "--stdout" => args.stdout = true,
            "-h" | "--help" => return None,
            _ => die(format!("unknown argument `{argument}`")),
        }
    }
    if args.check && args.stdout {
        die("--check and --stdout are mutually exclusive");
    }
    Some(args)
}

fn main() -> ExitCode {
    let Some(args) = parse_args(env::args().skip(1)) else {
        println!(
            "Usage: generate-test-footprints [--check | --stdout]\n\n\
             With no option, writes {OUTPUT}. --check fails if it is stale."
        );
        return ExitCode::SUCCESS;
    };
    let root = repo_root();
    let generated = render(&root);
    if args.stdout {
        print!("{generated}");
        return ExitCode::SUCCESS;
    }
    let output = root.join(OUTPUT);
    if args.check {
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

    fn committed_hosted_nodes() -> BTreeSet<String> {
        let dag: Value = serde_json::from_str(include_str!("../../../dag/validate.json")).unwrap();
        dag["steps"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|step| {
                step["labels"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|label| label == "hosted-portable")
            })
            .map(|step| {
                format!(
                    "{}.{}",
                    step["group"].as_str().unwrap(),
                    step["job"].as_str().unwrap()
                )
            })
            .collect()
    }

    #[test]
    fn committed_policy_public_selectors_resolve_to_present_hosted_nodes() {
        let nodes = committed_hosted_nodes();
        let policy: Value =
            serde_json::from_str(include_str!("../../../test-footprints-policy.json")).unwrap();
        assert!(!nodes.contains("test.applications_e2e"));
        assert!(nodes.contains("test.applications_e2e_on_host"));
        for key in ["package_rules", "path_footprints"] {
            for rule in policy[key].as_array().unwrap() {
                for node in strings(rule, "nodes", key) {
                    assert!(known_selection_node(&nodes, &node), "{key}: {node}");
                }
            }
        }
        let backend_rules = load_backend_rules(&policy, &nodes);
        assert!(backend_rules.iter().any(|rule| rule.id == "dbt"));
        // v1 scope cut: no scoped kvm/ptrace/liteinst/sabre/e9patch routes.
        for dropped in ["kvm", "ptrace", "liteinst", "sabre", "e9patch"] {
            assert!(
                !backend_rules.iter().any(|rule| rule.id == dropped),
                "dropped v1 backend route {dropped} must not reappear"
            );
        }
        assert!(
            backend_rules
                .iter()
                .any(|rule| rule.id == "reverie-core" && rule.tier == "core")
        );
        let shared_runtime = policy["package_rules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|rule| {
                strings(rule, "packages", "package_rules")
                    .contains(&"hermit-test-workdir".to_string())
            })
            .expect("shared workdir must retain the shared-runtime coverage policy");
        assert_eq!(shared_runtime["e2e_all"], true);
        assert!(
            strings(shared_runtime, "nodes", "package_rules")
                .contains(&"test.applications_e2e".to_string())
        );
    }

    #[test]
    fn unknown_or_removed_hosted_policy_nodes_still_refuse() {
        let mut nodes = committed_hosted_nodes();
        assert!(known_selection_node(&nodes, "test.applications_e2e"));
        assert!(nodes.remove("test.applications_e2e_on_host"));
        assert!(!known_selection_node(&nodes, "test.applications_e2e"));
        assert!(!known_selection_node(
            &nodes,
            "test.applications_e2e_on_host"
        ));
        assert!(!known_selection_node(&nodes, "test.applications_e2e_typo"));
        assert!(nodes.insert("test.applications_e2e".into()));
        assert!(known_selection_node(&nodes, "test.applications_e2e"));
        assert!(!known_selection_node(
            &nodes,
            "test.applications_e2e_on_host"
        ));
        // The strict compatibility corpus is one bucket, named like any other.
        assert!(known_selection_node(&nodes, "e2e.manifest_compat"));
        assert!(nodes.remove("e2e.manifest_compat_on_host"));
        assert!(!known_selection_node(&nodes, "e2e.manifest_compat"));
    }

    #[test]
    fn no_arguments_still_select_artifact_write() {
        assert_eq!(parse_args(std::iter::empty()), Some(Args::default()));
    }

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
        assert_eq!(
            cargo_command_packages(
                "cargo nextest list --workspace --exclude b && ./ci/nextest-binaries.rs list -p b",
                &all,
                &defaults
            ),
            BTreeSet::from(["a".into(), "b".into(), "c".into()])
        );
        assert!(cargo_command_packages("cargo nextest show-config", &all, &defaults).is_empty());
    }

    #[test]
    fn prepared_profiles_follow_graph_selections_and_include_cargo_guests() {
        use hermit_manifest_plan::nextest_binaries::REQUIRED_ENV;
        use hermit_manifest_plan::nextest_binaries::SELECTION_ENV;
        let mut graph = dagrun::io::dag_from_json(&json!({"steps": [
            {"group": "test", "job": "modes", "cmd": "true", "labels": ["portable"],
             "env": {REQUIRED_ENV: "1", SELECTION_ENV: "[\"-p\",\"hermit-run\",\"--test\",\"hermit_modes\"]"}},
            {"group": "test", "job": "unit", "cmd": "true", "labels": ["quick"],
             "env": {REQUIRED_ENV: "1", SELECTION_ENV: "[\"-p\",\"hermit-detcore\",\"--lib\"]"}}
        ]}).to_string()).unwrap();
        let all = BTreeSet::from(
            [
                "hermit-run",
                "hermit-detcore",
                "hermetic_infra_hermit_tests",
                "other",
            ]
            .map(String::from),
        );
        let defaults = BTreeSet::from(["other".into()]);
        assert_eq!(
            prepared_command_packages(
                "./ci/nextest-binaries.rs prepare portable",
                &graph,
                &all,
                &defaults
            )
            .unwrap(),
            BTreeSet::from(["hermit-run".into(), "hermetic_infra_hermit_tests".into()])
        );
        assert_eq!(
            prepared_command_packages(
                "./ci/nextest-binaries.rs prepare quick",
                &graph,
                &all,
                &defaults
            )
            .unwrap(),
            BTreeSet::from(["hermit-detcore".into()])
        );
        assert!(
            prepared_command_packages(
                "./ci/nextest-binaries.rs prepare absent",
                &graph,
                &all,
                &defaults
            )
            .is_err()
        );
        let mut no_guests = all.clone();
        no_guests.remove("hermetic_infra_hermit_tests");
        assert!(
            prepared_command_packages(
                "./ci/nextest-binaries.rs prepare portable",
                &graph,
                &no_guests,
                &defaults
            )
            .is_err()
        );
        graph.steps[1]
            .env
            .insert(SELECTION_ENV.into(), "[\"-p\",\"other\",\"--lib\"]".into());
        assert_eq!(
            prepared_command_packages(
                "./ci/nextest-binaries.rs prepare quick",
                &graph,
                &all,
                &defaults
            )
            .unwrap(),
            BTreeSet::from(["other".into()])
        );
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
