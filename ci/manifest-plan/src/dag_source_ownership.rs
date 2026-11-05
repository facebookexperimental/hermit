//! Exact source-node transfer contract. Counts describe source policy only;
//! they do not stand in for a test inventory or an executed result.
//!
//! # Adding a post-split DAG node (the ONE admission path)
//!
//! `ci/dag-source-ownership.json` is the frozen pre-split source snapshot.
//! Never edit it (or `ORIGIN_SHA256`) to admit a new node, and never fold a
//! new bucket into an existing node's step. Every validation-DAG node created
//! after the first-party core split — rr, gVisor, and future e2e buckets —
//! is admitted by adding one entry to [`POST_SPLIT_CORE_NODES`] below:
//!
//! ```ignore
//! ExtensionNode {
//!     tag: "e2e.manifest_example",
//!     labels: &["full", "portable"],
//!     deps: &["gate.manifest", /* core or extension tags only */],
//! }
//! ```
//!
//! Rules (all fail-closed, each refusal names the node and dependency):
//! the tag must not duplicate a frozen node or another extension node, and
//! every dependency must be a frozen core node or another extension node.
//! Dependencies on patching/dynamorio-owned nodes (crossed deps) are refused;
//! new bucket steps therefore omit `check.dbt_runtime_abi`. Register every
//! generated twin (`_on_host`, `_buck`) as its own entry. The admitted core
//! population is then exactly frozen core tags ∪ extension tags
//! ([`admitted_core_tags`]); anything else is refused as an extra node.
use std::collections::BTreeMap;
use std::collections::BTreeSet;

use serde::Deserialize;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    schema: u64,
    origin_sha256: String,
    nodes: BTreeMap<String, Node>,
    transferred_diagnostic_edges: BTreeMap<String, BTreeMap<String, String>>,
    producer_policies: BTreeMap<String, serde_json::Value>,
    hosted_original_roots: Vec<String>,
    hosted_original_closure: Vec<String>,
    named_populations: BTreeMap<String, Vec<String>>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Node {
    owner: String,
    labels: Vec<String>,
    deps: Vec<String>,
}
const ORIGIN_SHA256: &str = "1eeecab763f13b8b653e0f46ee66e6313f7df1b2fa261b1bb182839a51abf5ef";

/// One post-split core node admitted through [`POST_SPLIT_CORE_NODES`].
pub struct ExtensionNode {
    pub tag: &'static str,
    pub labels: &'static [&'static str],
    pub deps: &'static [&'static str],
}

/// The ONE registry for validation-DAG nodes created after the split.
/// Empty until the first post-split bucket lands; see module docs.
pub const POST_SPLIT_CORE_NODES: &[ExtensionNode] = &[];

/// Tags admitted by `extension`, after validating it against `source`.
fn validate_extension_registry(
    source: &Document,
    extension: &[ExtensionNode],
) -> Result<BTreeSet<String>, String> {
    let mut tags = BTreeSet::new();
    for node in extension {
        if node.tag.is_empty() || node.tag.chars().any(char::is_whitespace) {
            return Err(format!("invalid post-split extension tag {:?}", node.tag));
        }
        if source.nodes.contains_key(node.tag) {
            return Err(format!(
                "post-split extension node {} duplicates a frozen source node",
                node.tag
            ));
        }
        if !tags.insert(node.tag.to_string()) {
            return Err(format!("duplicate post-split extension node {}", node.tag));
        }
        for (name, values) in [("label", node.labels), ("dependency", node.deps)] {
            let unique = values.iter().collect::<BTreeSet<_>>();
            if unique.len() != values.len() || values.iter().any(|value| value.is_empty()) {
                return Err(format!(
                    "post-split extension node {} has empty/duplicate {name}",
                    node.tag
                ));
            }
        }
    }
    for node in extension {
        for dependency in node.deps {
            let frozen_core = source
                .nodes
                .get(*dependency)
                .is_some_and(|n| n.owner == "core");
            if !frozen_core && !tags.contains(*dependency) {
                let owner = source
                    .nodes
                    .get(*dependency)
                    .map(|n| n.owner.as_str())
                    .unwrap_or("unknown");
                return Err(format!(
                    "post-split extension node {} may depend only on core/extension nodes; crossed dependency {dependency} (owner {owner})",
                    node.tag
                ));
            }
        }
    }
    Ok(tags)
}

/// Post-split extension tags in the live registry.
pub fn extension_tags() -> BTreeSet<String> {
    validate_extension_registry(&contract(), POST_SPLIT_CORE_NODES)
        .expect("post-split extension registry")
}

/// Frozen core tags plus post-split extension tags, from `extension`.
fn admitted_core_tags_with(extension: &[ExtensionNode]) -> BTreeSet<String> {
    let source = contract();
    let mut tags = core_tags();
    tags.extend(
        validate_extension_registry(&source, extension).expect("post-split extension registry"),
    );
    tags
}

/// The exact admitted core population: frozen core ∪ extension, no extras.
pub fn admitted_core_tags() -> BTreeSet<String> {
    admitted_core_tags_with(POST_SPLIT_CORE_NODES)
}
fn parse_contract(bytes: &[u8]) -> Result<Document, String> {
    if bytes.len() > 1024 * 1024 {
        return Err("source ownership contract exceeds bound".into());
    }
    crate::execution_owners::reject_duplicate_keys(bytes)?;
    let source: Document = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    if source.schema != 1 || source.origin_sha256 != ORIGIN_SHA256 || source.nodes.is_empty() {
        return Err("source ownership contract schema/original source hash differs".into());
    }
    for (tag, node) in &source.nodes {
        if tag.is_empty()
            || tag.chars().any(char::is_whitespace)
            || !matches!(node.owner.as_str(), "core" | "patching" | "dynamorio")
        {
            return Err(format!("invalid source owner/tag for {tag:?}"));
        }
        for (name, values) in [("label", &node.labels), ("dependency", &node.deps)] {
            let unique = values.iter().collect::<BTreeSet<_>>();
            if unique.len() != values.len() || values.iter().any(|value| value.is_empty()) {
                return Err(format!("{tag} has empty/duplicate {name}"));
            }
        }
        if node
            .deps
            .iter()
            .any(|dependency| !source.nodes.contains_key(dependency))
        {
            return Err(format!("{tag} names an unprovided original dependency"));
        }
    }
    for (tag, edges) in &source.transferred_diagnostic_edges {
        if !source
            .nodes
            .get(tag)
            .is_some_and(|node| node.owner == "core")
            || edges.is_empty()
        {
            return Err(format!("invalid diagnostic source owner {tag}"));
        }
        for (dependency, destination) in edges {
            if !source
                .nodes
                .get(dependency)
                .is_some_and(|node| node.owner != "core")
                || destination.is_empty()
            {
                return Err(format!("invalid diagnostic destination for {tag}"));
            }
        }
    }
    if source
        .producer_policies
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>()
        != BTreeSet::from([
            "compatprep.hermit_release",
            "compatprep.hermit_release_in_pinned_root",
        ])
    {
        return Err("source producer resource owners changed".into());
    }
    for (rows, expected) in [
        (&source.hosted_original_roots, 19usize),
        (&source.hosted_original_closure, 22usize),
    ] {
        if rows.len() != expected
            || rows.iter().collect::<BTreeSet<_>>().len() != expected
            || rows
                .iter()
                .any(|tag| !source.nodes.contains_key(&format!("{tag}_on_host")))
        {
            return Err("original hosted split source policy changed".into());
        }
    }
    if source
        .hosted_original_roots
        .iter()
        .any(|tag| !source.hosted_original_closure.contains(tag))
    {
        return Err("original hosted root lost its closure".into());
    }
    for (tag, node) in &source.nodes {
        if node.owner != "core" {
            continue;
        }
        let crossed = node
            .deps
            .iter()
            .filter(|dependency| source.nodes[*dependency].owner != "core")
            .cloned()
            .collect::<BTreeSet<_>>();
        let declared = source.transferred_diagnostic_edges.get(tag);
        let keys = declared
            .into_iter()
            .flat_map(|edges| edges.keys().cloned())
            .collect::<BTreeSet<_>>();
        if crossed != keys {
            return Err(format!(
                "{tag} original crossed dependencies have no exact maintained owner"
            ));
        }
        for (dependency, destination) in declared.into_iter().flat_map(|edges| edges.iter()) {
            if dependency != "check.dbt_runtime_abi" || destination != &format!("dynamorio-{tag}") {
                return Err(format!("{tag} invalid original diagnostic destination"));
            }
        }
    }
    if source
        .named_populations
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>()
        != BTreeSet::from(["manifest_publishers"])
    {
        return Err("source population owners changed".into());
    }
    for rows in source.named_populations.values() {
        if rows.is_empty()
            || rows.iter().collect::<BTreeSet<_>>().len() != rows.len()
            || rows.iter().any(|tag| !source.nodes.contains_key(tag))
        {
            return Err("source population has duplicate or unknown nodes".into());
        }
    }
    Ok(source)
}
fn contract() -> Document {
    parse_contract(include_bytes!("../../dag-source-ownership.json"))
        .expect("source-owned DAG node contract")
}
pub fn assert_producer_resources(
    step: &dagrun::model::Step,
    source_tag: &str,
) -> Result<(), String> {
    let source = contract();
    let expected = source
        .producer_policies
        .get(source_tag)
        .ok_or_else(|| format!("unprovided immutable producer policy {source_tag}"))?;
    let actual = serde_json::json!({"timeout": step.timeout, "cpu_timeout": step.cpu_timeout,
        "resources": step.hint.resources, "est_duration_s": step.hint.est_duration_s,
        "rss_baseline_bytes": step.hint.rss_baseline_bytes, "rss_baseline_inner_jobs": step.hint.rss_baseline_inner_jobs,
        "hard_mem_max_bytes": step.hint.hard_mem_max_bytes, "classification": step.hint.classification.value(),
        "preferred_inner_jobs": step.hint.preferred_inner_jobs, "measured_effective_cores": step.hint.measured_effective_cores,
        "measured_cpu_utilization": step.hint.measured_cpu_utilization, "jobs_flag": step.jobs_flag, "jobs_env": step.jobs_env});
    if &actual != expected {
        return Err(format!(
            "{} changed the complete immutable producer resource policy: {actual} != {expected}",
            step.tag()
        ));
    }
    Ok(())
}
fn unique_tags(cfg: &dagrun::model::DagConfig) -> Result<BTreeSet<String>, String> {
    let mut tags = BTreeSet::new();
    for step in &cfg.steps {
        let tag = step.tag();
        if !tags.insert(tag.clone()) {
            return Err(format!("duplicate DAG source tag {tag}"));
        }
    }
    Ok(tags)
}
/// Keep the complete original count and compare the actual exact core subset.
/// External members are required in their catalog, never inferred as executed.
pub fn assert_named_population(
    name: &str,
    original_count: usize,
    actual: BTreeSet<String>,
) -> Result<(), String> {
    let source = contract();
    let original = source
        .named_populations
        .get(name)
        .ok_or_else(|| format!("unknown original population {name}"))?;
    if original.len() != original_count {
        return Err(format!("original {name} count changed"));
    }
    let expected = original
        .iter()
        .filter(|tag| source.nodes[*tag].owner == "core")
        .cloned()
        .collect::<BTreeSet<_>>();
    if actual != expected {
        return Err(format!(
            "actual {name} core owner population changed: {actual:?} != {expected:?}"
        ));
    }
    Ok(())
}
pub fn assert_profile_population(
    label: &str,
    original_count: usize,
    actual: BTreeSet<String>,
) -> Result<(), String> {
    let original = source_profile(label, false, true)?;
    if original.len() != original_count {
        return Err(format!("original {label} selected count changed"));
    }
    let expected = source_profile(label, true, true)?;
    if actual != expected {
        return Err(format!("actual {label} core owner population changed"));
    }
    Ok(())
}
pub fn is_core(tag: &str) -> bool {
    contract()
        .nodes
        .get(tag)
        .is_some_and(|node| node.owner == "core")
}
pub fn core_tags() -> BTreeSet<String> {
    contract()
        .nodes
        .into_iter()
        .filter_map(|(tag, node)| (node.owner == "core").then_some(tag))
        .collect()
}
pub fn source_profile(
    label: &str,
    core_only: bool,
    closure: bool,
) -> Result<BTreeSet<String>, String> {
    let source = contract();
    let admitted = |tag: &str| {
        source
            .nodes
            .get(tag)
            .is_some_and(|node| !core_only || node.owner == "core")
    };
    let mut selected = source
        .nodes
        .iter()
        .filter_map(|(tag, node)| {
            (admitted(tag) && node.labels.iter().any(|value| value == label)).then_some(tag.clone())
        })
        .collect::<BTreeSet<_>>();
    if closure {
        let mut pending = selected.iter().cloned().collect::<Vec<_>>();
        while let Some(tag) = pending.pop() {
            let node = source
                .nodes
                .get(&tag)
                .ok_or_else(|| format!("unknown source owner {tag}"))?;
            for dependency in &node.deps {
                if !source.nodes.contains_key(dependency) {
                    return Err(format!("unprovided original dependency {dependency}"));
                }
                if admitted(dependency) && selected.insert(dependency.clone()) {
                    pending.push(dependency.clone());
                }
            }
        }
    }
    Ok(selected)
}
/// The original complete source sets remain asserted (19/22); the real current
/// core split must equal their exact first-party owner subset, not a new count.
pub fn hosted_split_tags(closure: bool) -> BTreeSet<String> {
    let source = contract();
    let rows = if closure {
        &source.hosted_original_closure
    } else {
        &source.hosted_original_roots
    };
    rows.iter()
        .filter(|tag| source.nodes[&format!("{tag}_on_host")].owner == "core")
        .cloned()
        .collect()
}
pub fn source_partition_tags(
    prefixes: &[&str],
    exact: &[&str],
    core_only: bool,
) -> BTreeSet<String> {
    contract()
        .nodes
        .into_iter()
        .filter_map(|(tag, node)| {
            ((!core_only || node.owner == "core")
                && (exact.contains(&tag.as_str())
                    || prefixes.iter().any(|prefix| tag.starts_with(prefix))))
            .then_some(tag)
        })
        .collect()
}
/// This runs before any dependency-closure materializer. Only the explicitly
/// mapped original diagnostic edge may leave a core node; unknown changes
/// refuse before a missing prerequisite can be dropped or rewritten.
pub fn transfer_diagnostic_dependencies(cfg: &mut dagrun::model::DagConfig) -> Result<(), String> {
    transfer_diagnostic_dependencies_with(cfg, POST_SPLIT_CORE_NODES)
}

fn transfer_diagnostic_dependencies_with(
    cfg: &mut dagrun::model::DagConfig,
    extension: &[ExtensionNode],
) -> Result<(), String> {
    let source = contract();
    let extension_tags = validate_extension_registry(&source, extension)?;
    unique_tags(cfg)?;
    for step in &mut cfg.steps {
        let tag = step.tag();
        if extension_tags.contains(&tag) {
            // Extension nodes get no diagnostic transfer. Refuse deps the
            // frozen contract owns to patching/dynamorio here; deps unknown
            // at this phase (pre-materialization names such as
            // build.e2e_artifact) are judged by select_core after renaming.
            for dependency in &step.deps {
                if let Some(node) = source.nodes.get(dependency) {
                    if node.owner != "core" {
                        return Err(format!(
                            "post-split extension node {tag} may depend only on core/extension nodes; crossed dependency {dependency} (owner {})",
                            node.owner
                        ));
                    }
                }
            }
            continue;
        }
        let crossed = step
            .deps
            .iter()
            .filter(|dependency| {
                source
                    .nodes
                    .get(*dependency)
                    .is_some_and(|node| node.owner != "core")
            })
            .cloned()
            .collect::<BTreeSet<_>>();
        if crossed.is_empty() {
            continue;
        }
        let declared = source
            .transferred_diagnostic_edges
            .get(&tag)
            .ok_or_else(|| format!("{tag} has an unprovided diagnostic owner"))?;
        let expected = declared.keys().cloned().collect::<BTreeSet<_>>();
        if crossed != expected {
            return Err(format!(
                "{tag} changed crossed dependencies before owner transfer: {crossed:?} != {expected:?}"
            ));
        }
        step.deps
            .retain(|dependency| !expected.contains(dependency));
    }
    Ok(())
}
/// Final core generation no longer constructs external physical nodes. Its
/// exact population and all dependency references are checked after the common
/// materializers; external recipes live only in their real extension catalogs.
pub fn select_core(cfg: &mut dagrun::model::DagConfig) -> Result<(), String> {
    select_core_with(cfg, POST_SPLIT_CORE_NODES)
}

fn select_core_with(
    cfg: &mut dagrun::model::DagConfig,
    extension: &[ExtensionNode],
) -> Result<(), String> {
    let source = contract();
    let extension_tags = validate_extension_registry(&source, extension)?;
    let actual = unique_tags(cfg)?;
    let expected = admitted_core_tags_with(extension);
    if actual != expected {
        return Err(format!(
            "core DAG source population differs: missing={:?}, extra={:?}",
            expected.difference(&actual).collect::<Vec<_>>(),
            actual.difference(&expected).collect::<Vec<_>>()
        ));
    }
    for step in &cfg.steps {
        let tag = step.tag();
        for dependency in &step.deps {
            let admitted = source
                .nodes
                .get(dependency)
                .is_some_and(|node| node.owner == "core")
                || extension_tags.contains(dependency);
            if !admitted || !actual.contains(dependency) {
                return Err(format!(
                    "{tag} retained an unprovided or foreign prerequisite {dependency}"
                ));
            }
        }
        if step.cmd.contains("third-party-backends")
            || step.cmd.contains("detcore-dbt")
            || step.cmd.contains("detcore-sabre")
            || step.cmd.contains("run-with-reverie-dbt-budget.sh")
        {
            return Err(format!("core command still owns an external recipe: {tag}"));
        }
        for cell in step.effective_result_manifests().iter() {
            if matches!(cell.backend.as_deref(), Some("dbt" | "sabre")) {
                return Err(format!("core source still claims external cell: {tag}"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_owner_front_door_refuses_changed_source_unknown_owner_and_duplicate_actual_tags() {
        let bytes = include_bytes!("../../dag-source-ownership.json");
        parse_contract(bytes).unwrap();
        let valid: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        for field in ["schema", "origin_sha256"] {
            let mut bad = valid.clone();
            bad[field] = if field == "schema" {
                serde_json::json!(2)
            } else {
                serde_json::json!("0".repeat(64))
            };
            assert!(
                parse_contract(&serde_json::to_vec(&bad).unwrap())
                    .err()
                    .unwrap()
                    .contains("schema/original source hash differs")
            );
        }
        let mut bad = valid;
        bad["nodes"]["pre.submodules"]["owner"] = serde_json::json!("unknown");
        assert!(
            parse_contract(&serde_json::to_vec(&bad).unwrap())
                .err()
                .unwrap()
                .contains("invalid source owner/tag")
        );
        let duplicate = String::from_utf8(bytes.to_vec()).unwrap().replacen(
            "\"schema\": 1",
            "\"schema\": 1, \"schema\": 1",
            1,
        );
        assert!(
            parse_contract(duplicate.as_bytes())
                .err()
                .unwrap()
                .contains("duplicate JSON object key")
        );
        let mut cfg = crate::validation_dag_static::config();
        cfg.steps.push(cfg.steps[0].clone());
        assert!(
            select_core(&mut cfg)
                .unwrap_err()
                .contains("duplicate DAG source tag")
        );
    }

    const EXAMPLE: &[ExtensionNode] = &[ExtensionNode {
        tag: "e2e.manifest_example",
        labels: &["full", "portable"],
        deps: &["gate.manifest"],
    }];

    fn one_step_cfg(deps: &[&str]) -> dagrun::model::DagConfig {
        let mut cfg = crate::validation_dag_static::config();
        let mut step = cfg.steps[0].clone();
        step.group = "e2e".into();
        step.job = "manifest_example".into();
        step.deps = deps.iter().map(|dep| dep.to_string()).collect();
        cfg.steps = vec![step];
        cfg
    }

    #[test]
    fn frozen_contract_is_untouched_and_admitted_is_the_exact_union() {
        let source = contract();
        assert_eq!(source.origin_sha256, ORIGIN_SHA256);
        assert_eq!(source.hosted_original_roots.len(), 19);
        assert_eq!(source.hosted_original_closure.len(), 22);
        let frozen = core_tags();
        let admitted = admitted_core_tags_with(EXAMPLE);
        let extension: BTreeSet<_> = ["e2e.manifest_example".to_string()].into();
        assert!(frozen.is_disjoint(&extension));
        assert_eq!(admitted, &frozen | &extension);
        assert_eq!(admitted_core_tags(), {
            let mut live = frozen.clone();
            live.extend(extension_tags());
            live
        });
    }

    #[test]
    fn extension_registry_refuses_collision_crossed_and_unknown_deps() {
        let source = contract();
        let collision = [ExtensionNode {
            tag: "pre.submodules",
            labels: &["full"],
            deps: &[],
        }];
        assert!(
            validate_extension_registry(&source, &collision)
                .unwrap_err()
                .contains("duplicates a frozen source node")
        );
        let crossed = [ExtensionNode {
            tag: "e2e.manifest_example",
            labels: &["full"],
            deps: &["check.dbt_runtime_abi"],
        }];
        assert!(validate_extension_registry(&source, &crossed)
            .unwrap_err()
            .contains("may depend only on core/extension nodes; crossed dependency check.dbt_runtime_abi (owner dynamorio)"));
        let unknown = [ExtensionNode {
            tag: "e2e.manifest_example",
            labels: &["full"],
            deps: &["no.such.node"],
        }];
        assert!(
            validate_extension_registry(&source, &unknown)
                .unwrap_err()
                .contains("crossed dependency no.such.node (owner unknown)")
        );
    }

    #[test]
    fn extension_transfer_admits_core_deps_and_refuses_crossed_by_name() {
        let mut ok = one_step_cfg(&["gate.manifest"]);
        transfer_diagnostic_dependencies_with(&mut ok, EXAMPLE).unwrap();
        // Pre-materialization names unknown to the frozen contract pass
        // transfer; select_core judges their final names after renaming.
        let mut pre_materialized = one_step_cfg(&["build.e2e_artifact"]);
        transfer_diagnostic_dependencies_with(&mut pre_materialized, EXAMPLE).unwrap();
        let mut crossed = one_step_cfg(&["check.dbt_runtime_abi"]);
        let crossed_registry = [ExtensionNode {
            tag: "e2e.manifest_example",
            labels: &["full", "portable"],
            deps: &["gate.manifest"],
        }];
        assert!(transfer_diagnostic_dependencies_with(&mut crossed, &crossed_registry)
            .unwrap_err()
            .contains("post-split extension node e2e.manifest_example may depend only on core/extension nodes; crossed dependency check.dbt_runtime_abi (owner dynamorio)"));
        // Without a registry entry the same step is an unadmitted extra node:
        // select_core refuses it by exact population, never silently.
        let mut unlisted = one_step_cfg(&["gate.manifest"]);
        let err = select_core_with(&mut unlisted, &[]).unwrap_err();
        assert!(err.contains("core DAG source population differs"), "{err}");
    }
}
