//! Exact source-node transfer contract. Counts describe source policy only;
//! they do not stand in for a test inventory or an executed result.
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
    let source = contract();
    unique_tags(cfg)?;
    for step in &mut cfg.steps {
        let tag = step.tag();
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
    let source = contract();
    let actual = unique_tags(cfg)?;
    let expected = core_tags();
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
            if !source
                .nodes
                .get(dependency)
                .is_some_and(|node| node.owner == "core")
                || !actual.contains(dependency)
            {
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
}
