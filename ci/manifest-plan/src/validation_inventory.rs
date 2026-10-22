// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! The validation-graph inventory: what each node of `ci/dag/validate.json`
//! runs, as its title and one paragraph. Groups are listed in order of their
//! earliest node's dependency layer (the group graph itself has cycles), and
//! nodes within a group in dependency order.
//!
//! The text is each node's `desc` (title) and `description` (paragraph). Both
//! are written once, where the node is defined (`validation_dag_static.rs`
//! for authored nodes, the generating function for generated ones), and the
//! generator copies them into the committed graph, which is all this module
//! reads. Nothing here restates a node.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt::Write as _;

use dagrun::model::DagConfig;
use dagrun::model::Step;

pub const USAGE: &str = "usage: scripts/validate.rs --inventory [--labels LABEL[,LABEL...]] [--ascii]\n\n\
Print every node of ci/dag/validate.json with its title and paragraph, grouped\n\
by DAG group: groups in order of their earliest node's dependency layer, nodes\n\
within a group in dependency order. --labels keeps the nodes carrying any named\n\
label and every node they depend on, exactly as `dagrun run --labels` selects\n\
them. --ascii first prints the group-level graph (one line per group with its\n\
upstream groups). Nothing is run.";

/// The width paragraphs are wrapped to.
const WIDTH: usize = 88;

#[derive(Debug, Default, Eq, PartialEq)]
pub struct Options {
    pub labels: Vec<String>,
    pub ascii: bool,
}

/// Parse the arguments after `--inventory`.
pub fn parse(args: &[String]) -> Result<Options, String> {
    let mut options = Options::default();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--ascii" => options.ascii = true,
            "--labels" => {
                let value = args
                    .get(index + 1)
                    .filter(|value| !value.is_empty() && !value.starts_with('-'))
                    .ok_or("--labels needs LABEL[,LABEL...]")?;
                let labels = value
                    .split(',')
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(String::from)
                    .collect::<Vec<_>>();
                if labels.is_empty() {
                    return Err("--labels needs at least one label".into());
                }
                options.labels.extend(labels);
                index += 1;
            }
            other => return Err(format!("unknown inventory argument {other}")),
        }
        index += 1;
    }
    Ok(options)
}

/// Each step's dependency layer: 0 for a step with no dependency inside `cfg`,
/// otherwise one more than its deepest dependency.
fn layers(cfg: &DagConfig) -> BTreeMap<String, usize> {
    let by_tag = cfg
        .steps
        .iter()
        .map(|step| (step.tag(), step))
        .collect::<BTreeMap<_, _>>();
    fn layer(
        tag: &str,
        by_tag: &BTreeMap<String, &Step>,
        memo: &mut BTreeMap<String, usize>,
    ) -> usize {
        if let Some(found) = memo.get(tag) {
            return *found;
        }
        let depth = by_tag[tag]
            .deps
            .iter()
            .filter(|dep| by_tag.contains_key(dep.as_str()))
            .map(|dep| layer(dep, by_tag, memo) + 1)
            .max()
            .unwrap_or(0);
        memo.insert(tag.to_string(), depth);
        depth
    }
    let mut memo = BTreeMap::new();
    for tag in by_tag.keys() {
        layer(tag, &by_tag, &mut memo);
    }
    memo
}

fn wrap(text: &str, indent: &str, out: &mut String) {
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && indent.len() + line.len() + 1 + word.len() > WIDTH {
            let _ = writeln!(out, "{indent}{line}");
            line.clear();
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        let _ = writeln!(out, "{indent}{line}");
    }
}

/// Render the inventory of `cfg`, restricted to `options.labels` when given.
pub fn render(cfg: &DagConfig, options: &Options) -> Result<String, String> {
    let selected = if options.labels.is_empty() {
        cfg.clone()
    } else {
        dagrun::select_steps_by_labels(cfg, &options.labels)?
    };
    let layer = layers(&selected);
    let mut groups: BTreeMap<&str, Vec<&Step>> = BTreeMap::new();
    for step in &selected.steps {
        groups.entry(step.group.as_str()).or_default().push(step);
    }
    let mut order = groups
        .iter()
        .map(|(group, steps)| {
            let first = steps.iter().map(|s| layer[&s.tag()]).min().unwrap_or(0);
            (first, *group)
        })
        .collect::<Vec<_>>();
    order.sort();

    let scope = if options.labels.is_empty() {
        "the whole superset".to_string()
    } else {
        format!("labels {}", options.labels.join(","))
    };
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Validation graph ci/dag/validate.json, {scope}: {} nodes in {} groups.",
        selected.steps.len(),
        groups.len()
    );
    if options.ascii {
        out.push('\n');
        out.push_str(&dagrun::viz::to_ascii_groups(&selected, None));
    }
    for (_, group) in order {
        let mut steps = groups[group].clone();
        steps.sort_by_key(|step| (layer[&step.tag()], step.tag()));
        let upstream = steps
            .iter()
            .flat_map(|step| step.deps.iter())
            .filter_map(|dep| dep.split_once('.').map(|(g, _)| g))
            .filter(|g| *g != group && groups.contains_key(g))
            .collect::<BTreeSet<_>>();
        let _ = writeln!(
            out,
            "\n== {group}: {} node{}{}",
            steps.len(),
            if steps.len() == 1 { "" } else { "s" },
            if upstream.is_empty() {
                String::new()
            } else {
                format!(
                    ", after {}",
                    upstream.into_iter().collect::<Vec<_>>().join(", ")
                )
            }
        );
        // A paragraph several nodes share (one per generated node of a
        // family) is printed once, with the first node that carries it.
        let mut printed: BTreeMap<&str, String> = BTreeMap::new();
        for step in steps {
            let tag = step.tag();
            let _ = writeln!(out, "\n{tag} -- {}", step.desc);
            let deps = step
                .deps
                .iter()
                .filter(|dep| layer.contains_key(dep.as_str()))
                .cloned()
                .collect::<Vec<_>>();
            if !deps.is_empty() {
                wrap(&format!("needs: {}", deps.join(", ")), "    ", &mut out);
            }
            if step.description.trim().is_empty() {
                let _ = writeln!(out, "    (no paragraph)");
            } else if let Some(first) = printed.get(step.description.as_str()) {
                let _ = writeln!(out, "    (same paragraph as {first})");
            } else {
                wrap(&step.description, "    ", &mut out);
                printed.insert(step.description.as_str(), tag);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph() -> DagConfig {
        dagrun::io::dag_from_json(
            r#"{"steps": [
                {"group": "build", "job": "a", "cmd": "true", "desc": "Build A",
                 "description": "Builds A."},
                {"group": "test", "job": "x", "cmd": "true", "desc": "Test X",
                 "description": "Runs X against A.", "deps": ["build.a"], "labels": ["full"]},
                {"group": "test", "job": "y", "cmd": "true", "desc": "Test Y",
                 "description": "Runs X against A.", "deps": ["build.a"], "labels": ["super"]},
                {"group": "lint", "job": "z", "cmd": "true", "desc": "Lint",
                 "description": "Checks style.", "labels": ["super"]},
                {"group": "lint", "job": "u", "cmd": "true", "desc": "Undescribed A",
                 "labels": ["super"]},
                {"group": "lint", "job": "v", "cmd": "true", "desc": "Undescribed B",
                 "labels": ["super"]}
            ]}"#,
        )
        .unwrap()
    }

    #[test]
    fn labels_select_the_closure_and_groups_follow_dependency_order() {
        let text = render(
            &graph(),
            &Options {
                labels: vec!["full".into()],
                ascii: false,
            },
        )
        .unwrap();
        assert!(text.starts_with(
            "Validation graph ci/dag/validate.json, labels full: 2 nodes in 2 groups."
        ));
        let build = text.find("== build: 1 node").unwrap();
        let test = text.find("== test: 1 node, after build").unwrap();
        assert!(build < test, "{text}");
        assert!(text.contains("test.x -- Test X\n    needs: build.a\n    Runs X against A.\n"));
        assert!(!text.contains("test.y"), "{text}");
        assert!(!text.contains("lint.z"), "{text}");
    }

    #[test]
    fn a_shared_paragraph_is_printed_once_per_group() {
        let text = render(&graph(), &Options::default()).unwrap();
        assert!(text.contains("6 nodes in 3 groups"), "{text}");
        // An absent paragraph is never "shared".
        assert!(
            text.contains("lint.u -- Undescribed A\n    (no paragraph)\n"),
            "{text}"
        );
        assert!(
            text.contains("lint.v -- Undescribed B\n    (no paragraph)\n"),
            "{text}"
        );
        assert_eq!(text.matches("Runs X against A.").count(), 1, "{text}");
        assert!(
            text.contains("test.y -- Test Y\n    needs: build.a\n    (same paragraph as test.x)\n")
        );
    }

    #[test]
    fn ascii_prepends_the_group_graph() {
        let text = render(
            &graph(),
            &Options {
                labels: vec![],
                ascii: true,
            },
        )
        .unwrap();
        assert!(text.contains(&dagrun::viz::to_ascii_groups(&graph(), None)));
    }

    #[test]
    fn arguments_are_parsed_strictly() {
        assert_eq!(
            parse(&["--labels".into(), "full,super".into(), "--ascii".into()]).unwrap(),
            Options {
                labels: vec!["full".into(), "super".into()],
                ascii: true,
            }
        );
        assert!(parse(&["--labels".into()]).is_err());
        assert!(parse(&["--labels".into(), ",".into()]).is_err());
        assert!(parse(&["--labels".into(), "--ascii".into()]).is_err());
        assert!(parse(&["--json".into()]).is_err());
    }
}
