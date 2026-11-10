#!/usr/bin/env -S rust-script --force
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
//! Enforce the repository's GitHub Actions trigger policy.
//!
//! Local exact-head validation is the landing authority. The portable workflow
//! is supplemental evidence and may run automatically only after a commit is
//! pushed to `integration`, and for pull requests not opened by the project bot
//! (owner decision, 2026-10-27): its `pull_request` trigger must use exactly
//! PORTABLE_PR_TYPES, and every job that can start without a predecessor or
//! runs under `always()` must carry PORTABLE_PR_GATE, so a bot or draft pull
//! request runs nothing. Two owner-approved maintenance workflows also run
//! at their exact reviewed schedules; every workflow may remain manually
//! dispatchable. This checker deliberately accepts only the small YAML shape
//! used here. An unfamiliar or ambiguous trigger block is an error, never a
//! silent pass.

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

const WORKFLOW_DIR: &str = ".github/workflows";
const PORTABLE: &str = "ci-portable.yml";
const VALIDATION_LEVELS: &str = "validation-levels.yml";
const BUCK2_OSS_NIGHTLY: &str = "buck2-oss-nightly.yml";
const RUST_SCRIPT_VERSION: &str = "0.36.0";
/// The only `pull_request` activity types ci-portable.yml may list.
const PORTABLE_PR_TYPES: [&str; 4] = ["opened", "synchronize", "reopened", "ready_for_review"];
/// The gate every ci-portable.yml entry job carries: a pull_request run whose
/// author is hermit-bot3, or whose pull request is a draft, does nothing.
const PORTABLE_PR_GATE: &str = "github.event_name != 'pull_request' || (github.event.pull_request.user.login != 'hermit-bot3' && !github.event.pull_request.draft)";
/// The cleanup job deletes artifacts (actions: write), which a fork's token lacks.
const PORTABLE_FORK_GUARD: &str = "github.event_name != 'pull_request' || github.event.pull_request.head.repo.full_name == github.repository";

#[derive(Debug, PartialEq, Eq)]
struct Triggers {
    events: BTreeSet<String>,
    push_branches: Option<Vec<String>>,
    schedule_crons: Option<Vec<String>>,
    pull_request_types: Option<Vec<String>>,
}

fn indentation(line: &str) -> Result<usize, String> {
    let prefix = line.len() - line.trim_start_matches([' ', '\t']).len();
    if line[..prefix].contains('\t') {
        return Err("tabs are not accepted in YAML indentation".to_string());
    }
    Ok(prefix)
}

fn code(line: &str) -> &str {
    line.split_once('#')
        .map_or(line, |(before, _)| before)
        .trim_end()
}

fn key(line: &str) -> Option<&str> {
    let trimmed = code(line).trim();
    let (name, value) = trimmed.split_once(':')?;
    if !value.trim().is_empty()
        || name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    Some(name)
}

fn inline_list(value: &str) -> Option<Vec<String>> {
    let value = value.trim();
    let inner = value.strip_prefix('[')?.strip_suffix(']')?;
    let entries = inner
        .split(',')
        .map(|entry| entry.trim().trim_matches(['\'', '"']).to_string())
        .collect::<Vec<_>>();
    if entries.is_empty() || entries.iter().any(String::is_empty) {
        return None;
    }
    Some(entries)
}

fn quoted_scalar(value: &str) -> Option<String> {
    let value = value.trim();
    let inner = value.strip_prefix('"')?.strip_suffix('"')?;
    if inner.is_empty() || inner.contains('"') {
        return None;
    }
    Some(inner.to_string())
}

fn parse_triggers(source: &str) -> Result<Triggers, String> {
    let lines = source.lines().collect::<Vec<_>>();
    let mut on_entries = Vec::new();
    for (index, raw) in lines.iter().enumerate() {
        let line = code(raw);
        if line.trim().is_empty() || indentation(line)? != 0 {
            continue;
        }
        let trimmed = line.trim();
        if matches!(trimmed, "---" | "...") {
            continue;
        }
        let (spelling, value) = trimmed
            .split_once(':')
            .ok_or_else(|| format!("unsupported top-level YAML structure: `{trimmed}`"))?;
        let spelling = spelling.trim();
        if spelling.is_empty()
            || !spelling
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(format!(
                "top-level YAML keys must use an unquoted simple spelling: `{spelling}`"
            ));
        }
        if spelling == "on" {
            on_entries.push((index, value.trim()));
        }
    }
    if on_entries.len() != 1 {
        return Err(format!(
            "expected exactly one top-level `on` key, found {}",
            on_entries.len()
        ));
    }
    let (on_index, value) = on_entries[0];
    if !value.is_empty() {
        return Err("the top-level `on` value must be a mapping".to_string());
    }

    let mut events = BTreeSet::new();
    let mut event_indent = None;
    let mut push_indent = None;
    let mut push_child_indent = None;
    let mut push_branches = None;
    let mut schedule_indent = None;
    let mut schedule_child_indent = None;
    let mut schedule_crons = None;
    let mut pull_request_indent = None;
    let mut pull_request_child_indent = None;
    let mut pull_request_types = None;
    for raw in &lines[on_index + 1..] {
        let line = code(raw);
        if line.trim().is_empty() {
            continue;
        }
        let indent = indentation(line)?;
        if indent == 0 {
            break;
        }
        let direct_indent = *event_indent.get_or_insert(indent);
        if indent < direct_indent {
            return Err("inconsistent indentation in top-level `on:` mapping".to_string());
        }
        if indent == direct_indent {
            let event = key(line).ok_or_else(|| {
                format!(
                    "event entries must use a mapping key on its own line; found `{}`",
                    line.trim()
                )
            })?;
            if !events.insert(event.to_string()) {
                return Err(format!("duplicate `{event}` trigger"));
            }
            push_indent = (event == "push").then_some(indent);
            push_child_indent = None;
            schedule_indent = (event == "schedule").then_some(indent);
            schedule_child_indent = None;
            pull_request_indent = (event == "pull_request").then_some(indent);
            pull_request_child_indent = None;
            if event == "schedule" {
                schedule_crons = Some(Vec::new());
            }
            continue;
        }

        if let Some(indent_of_push) = push_indent {
            if indent > indent_of_push {
                let direct_push_indent = *push_child_indent.get_or_insert(indent);
                if indent != direct_push_indent {
                    return Err(format!(
                        "ambiguous nested structure in `push`: `{}`",
                        line.trim()
                    ));
                }
                let trimmed = line.trim();
                if let Some(value) = trimmed.strip_prefix("branches:") {
                    if push_branches.is_some() {
                        return Err("duplicate `push.branches` entry".to_string());
                    }
                    push_branches = Some(inline_list(value).ok_or_else(|| {
                        "`push.branches` must be an explicit inline list".to_string()
                    })?);
                } else {
                    return Err(format!(
                        "unsupported direct `push` key; only `branches` is permitted: `{trimmed}`"
                    ));
                }
            }
        } else if let Some(indent_of_pull_request) = pull_request_indent {
            if indent > indent_of_pull_request {
                let direct = *pull_request_child_indent.get_or_insert(indent);
                if indent != direct {
                    return Err(format!(
                        "ambiguous nested structure in `pull_request`: `{}`",
                        line.trim()
                    ));
                }
                let trimmed = line.trim();
                if let Some(value) = trimmed.strip_prefix("types:") {
                    if pull_request_types.is_some() {
                        return Err("duplicate `pull_request.types` entry".to_string());
                    }
                    pull_request_types = Some(inline_list(value).ok_or_else(|| {
                        "`pull_request.types` must be an explicit inline list".to_string()
                    })?);
                } else {
                    return Err(format!(
                        "unsupported direct `pull_request` key; only `types` is permitted: `{trimmed}`"
                    ));
                }
            }
        } else if let Some(indent_of_schedule) = schedule_indent {
            if indent > indent_of_schedule {
                let direct_schedule_indent = *schedule_child_indent.get_or_insert(indent);
                if indent != direct_schedule_indent {
                    return Err(format!(
                        "ambiguous nested structure in `schedule`: `{}`",
                        line.trim()
                    ));
                }
                let trimmed = line.trim();
                let value = trimmed.strip_prefix("- cron:").ok_or_else(|| {
                    format!(
                        "unsupported direct `schedule` entry; only `- cron: \"...\"` is permitted: `{trimmed}`"
                    )
                })?;
                let cron = quoted_scalar(value).ok_or_else(|| {
                    "`schedule` cron must be an explicit nonempty double-quoted scalar".to_string()
                })?;
                schedule_crons
                    .as_mut()
                    .expect("schedule trigger initializes cron storage")
                    .push(cron);
            }
        }
    }

    if events.is_empty() {
        return Err("the top-level `on:` mapping has no events".to_string());
    }
    Ok(Triggers {
        events,
        push_branches,
        schedule_crons,
        pull_request_types,
    })
}

fn approved_schedule(name: &str) -> Option<&'static [&'static str]> {
    match name {
        "buck2-oss-nightly.yml" => Some(&["17 10 * * *"]),
        // Daily at 08:23 UTC, as before; "0-6" instead of "*" so the commit
        // that wrote it re-attributed the schedule to its pusher (see the
        // comment on the cron line in docs.yml).
        "docs.yml" => Some(&["23 8 * * 0-6"]),
        _ => None,
    }
}

fn validate(name: &str, triggers: &Triggers) -> Vec<String> {
    let mut recognized = BTreeSet::from([
        "push".to_string(),
        "schedule".to_string(),
        "workflow_dispatch".to_string(),
    ]);
    if name == PORTABLE {
        recognized.insert("pull_request".to_string());
    }
    let mut errors = Vec::new();
    for event in triggers.events.difference(&recognized) {
        errors.push(format!("automatic `{event}` trigger is not permitted"));
    }
    if !triggers.events.contains("workflow_dispatch") {
        errors.push("missing `workflow_dispatch` trigger".to_string());
    }
    if triggers.events.contains("push") {
        if triggers.push_branches.as_deref() != Some(&["integration".to_string()]) {
            errors.push("`push.branches` must contain exactly `integration`".to_string());
        }
    } else if triggers.push_branches.is_some() {
        errors.push("found `push.branches` without a `push` trigger".to_string());
    }

    if let Some(expected_crons) = approved_schedule(name) {
        let required_events =
            BTreeSet::from(["schedule".to_string(), "workflow_dispatch".to_string()]);
        if triggers.events != required_events {
            errors.push(format!(
                "{name} must have exactly `workflow_dispatch` and `schedule` triggers"
            ));
        }
        let actual_crons = triggers.schedule_crons.as_deref().unwrap_or_default();
        if !actual_crons
            .iter()
            .map(String::as_str)
            .eq(expected_crons.iter().copied())
        {
            errors.push(format!(
                "{name} must use exactly the approved cron schedule: {}",
                expected_crons.join(", ")
            ));
        }
    } else if triggers.events.contains("schedule") {
        errors.push("automatic `schedule` trigger is not permitted".to_string());
    }

    if name == PORTABLE {
        let allowed = BTreeSet::from([
            "pull_request".to_string(),
            "push".to_string(),
            "workflow_dispatch".to_string(),
        ]);
        if triggers.events != allowed {
            errors.push(
                "ci-portable.yml must have exactly `workflow_dispatch`, `push` and `pull_request` triggers"
                    .to_string(),
            );
        }
        if triggers.events.contains("pull_request")
            && !triggers
                .pull_request_types
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(String::as_str)
                .eq(PORTABLE_PR_TYPES.iter().copied())
        {
            errors.push(format!(
                "ci-portable.yml `pull_request.types` must be exactly [{}]",
                PORTABLE_PR_TYPES.join(", ")
            ));
        }
        if triggers.push_branches.as_deref() != Some(&["integration".to_string()]) {
            errors.push("ci-portable.yml must push-trigger exactly on `integration`".to_string());
        }
    }
    errors
}

/// GitHub-hosted runner labels ci-portable.yml may use. It is the one workflow
/// that runs a pull request's code automatically, so no job may run on a
/// self-hosted runner.
const PORTABLE_HOSTED_RUNNERS: [&str; 3] = ["ubuntu-latest", "ubuntu-24.04", "ubuntu-22.04"];

#[derive(Debug, Default)]
struct PortableJob {
    name: String,
    needs: Vec<String>,
    condition: Option<String>,
    runs_on: Vec<String>,
}

/// An `if:` expression without its optional `${{ }}` wrapper, whitespace collapsed.
fn normalized_condition(expr: &str) -> String {
    let trimmed = expr.trim();
    // A YAML double-quoted scalar: the expression is its content.
    let unquoted = trimmed
        .strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .unwrap_or(trimmed);
    let mut text = unquoted.split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some(inner) = text.strip_prefix("${{").and_then(|t| t.strip_suffix("}}")) {
        text = inner.split_whitespace().collect::<Vec<_>>().join(" ");
    }
    text
}

/// The `&&` conjuncts of an expression at parenthesis depth 0, outside quotes.
fn top_level_conjuncts(expr: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let (mut depth, mut quoted, mut current) = (0usize, false, String::new());
    let chars = expr.chars().collect::<Vec<_>>();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            quoted = !quoted;
        } else if !quoted && c == '(' {
            depth += 1;
        } else if !quoted && c == ')' {
            depth = depth.saturating_sub(1);
        } else if !quoted && depth == 0 && c == '&' && chars.get(i + 1) == Some(&'&') {
            parts.push(current.trim().to_string());
            current.clear();
            i += 2;
            continue;
        }
        current.push(c);
        i += 1;
    }
    parts.push(current.trim().to_string());
    parts
}

/// Whether an expression uses a status function, which replaces the implicit
/// `success() &&` GitHub adds otherwise. `success()` counts too: written
/// explicitly it can be negated or or-ed away (`!success()`).
fn has_status_function(expr: &str) -> bool {
    // Expression function names match regardless of case (`Always()`) and of
    // whitespace before the parenthesis (`always ( )`). Over-matching only refuses.
    let squeezed = expr
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    ["always(", "cancelled(", "failure(", "success("]
        .iter()
        .any(|function| squeezed.contains(function))
}

/// Whether an expression has a `||` at parenthesis depth 0, outside quotes.
/// `&&` binds tighter, so `x || y && anchor` does not require the anchor.
fn has_top_level_or(expr: &str) -> bool {
    let (mut depth, mut quoted) = (0usize, false);
    let chars = expr.chars().collect::<Vec<_>>();
    for (i, &c) in chars.iter().enumerate() {
        if c == '\'' {
            quoted = !quoted;
        } else if !quoted && c == '(' {
            depth += 1;
        } else if !quoted && c == ')' {
            depth = depth.saturating_sub(1);
        } else if !quoted && depth == 0 && c == '|' && chars.get(i + 1) == Some(&'|') {
            return true;
        }
    }
    false
}

/// A line without its YAML comment: `#` starts a comment only at the start of
/// the line or after whitespace, and outside quotes.
fn yaml_code(line: &str) -> &str {
    let (mut single, mut double, mut escaped) = (false, false, false);
    let mut previous = ' ';
    for (i, c) in line.char_indices() {
        if escaped {
            // A double-quoted `\"` or `\\` does not end the string.
            escaped = false;
            previous = c;
            continue;
        }
        match c {
            '\\' if double => escaped = true,
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            '#' if !single && !double && previous.is_whitespace() => {
                return line[..i].trim_end();
            }
            _ => {}
        }
        previous = c;
    }
    line.trim_end()
}

fn list_value(value: &str) -> Option<Vec<String>> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if value.starts_with('[') {
        inline_list(value)
    } else {
        Some(vec![
            value.trim_matches(|c| c == '"' || c == '\'').to_string(),
        ])
    }
}

/// The jobs of ci-portable.yml with their needs, job-level condition and runners.
/// Any line at job-key indentation that is not a plain `name:` key is an error.
fn portable_jobs(source: &str) -> Result<Vec<PortableJob>, String> {
    let Some(jobs_start) = source.find("\njobs:\n") else {
        return Err("ci-portable.yml has no top-level `jobs:` mapping".to_string());
    };
    let mut jobs: Vec<PortableJob> = Vec::new();
    // The key whose block-scalar or block-list continuation is being read.
    let mut open: Option<(&'static str, usize)> = None;
    // A guarded key whose value was given on its own line: nothing may continue it.
    let mut closed: Option<&'static str> = None;
    for raw in source[jobs_start + "\njobs:\n".len()..].lines() {
        let line = yaml_code(raw);
        if line.trim().is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if indent == 0 {
            break;
        }
        if let Some((what, at)) = open {
            if indent > at {
                let job = jobs.last_mut().expect("a key belongs to a job");
                let text = line.trim();
                match what {
                    "if" => {
                        let current = job.condition.get_or_insert_with(String::new);
                        current.push(' ');
                        current.push_str(text);
                    }
                    "needs" | "runs-on" => {
                        let item = text.strip_prefix("- ").ok_or_else(|| {
                            format!(
                                "unsupported `{what}` continuation in job `{}`: `{text}`",
                                job.name
                            )
                        })?;
                        let target = if what == "needs" {
                            &mut job.needs
                        } else {
                            &mut job.runs_on
                        };
                        target.push(
                            item.trim()
                                .trim_matches(|c| c == '"' || c == '\'')
                                .to_string(),
                        );
                    }
                    _ => {}
                }
                continue;
            }
            open = None;
        }
        if let Some(key) = closed {
            if indent > 4 {
                let job = jobs.last().map_or("", |job| job.name.as_str());
                return Err(format!(
                    "job `{job}` continues its single-line `{key}:` value on a further line; write the whole value on one line or as a block: `{}`",
                    line.trim()
                ));
            }
            closed = None;
        }
        if indent == 2 {
            let name = line
                .trim()
                .strip_suffix(':')
                .filter(|name| {
                    !name.is_empty()
                        && name
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                })
                .ok_or_else(|| {
                    format!("unrecognised line at job indentation: `{}`", line.trim())
                })?;
            jobs.push(PortableJob {
                name: name.to_string(),
                ..PortableJob::default()
            });
            continue;
        }
        let Some(job) = jobs.last_mut() else {
            return Err(format!("line before any job: `{}`", line.trim()));
        };
        if indent != 4 {
            continue;
        }
        let text = line.trim();
        // Only a plain `key:` is read below. A quoted key (`"if":`), a space
        // before the colon (`if :`), an explicit `? key` or a `<<` merge key is
        // still a job key to YAML, so it is refused rather than skipped.
        let plain_key = text.split_once(':').is_some_and(|(key, _)| {
            !key.is_empty()
                && key
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        });
        if !plain_key {
            return Err(format!(
                "job `{}` has a job-level line that is not a plain `key:`: `{text}`",
                job.name
            ));
        }
        let guarded = ["if", "needs", "runs-on"].into_iter().find_map(|key| {
            text.strip_prefix(key)
                .and_then(|rest| rest.strip_prefix(':'))
                .map(|value| (key, value.trim()))
        });
        let Some((key, value)) = guarded else {
            continue;
        };
        let seen = match key {
            "if" => job.condition.is_some(),
            "needs" => !job.needs.is_empty(),
            _ => !job.runs_on.is_empty(),
        };
        if seen {
            return Err(format!("job `{}` repeats `{key}:`", job.name));
        }
        if value.starts_with('*') || value.starts_with('&') {
            return Err(format!(
                "job `{}` uses a YAML alias or anchor in `{key}:`; write the value out",
                job.name
            ));
        }
        if (value.starts_with('>') || value.starts_with('|'))
            && !matches!(value, ">" | ">-" | "|" | "|-")
        {
            return Err(format!(
                "job `{}` uses an unsupported block-scalar header `{value}` in `{key}:`",
                job.name
            ));
        }
        let block = value.is_empty() || matches!(value, ">" | ">-" | "|" | "|-");
        match key {
            "if" if block => {
                job.condition = Some(String::new());
                open = Some(("if", 4));
            }
            "if" => {
                job.condition = Some(value.to_string());
                closed = Some("if");
            }
            "needs" | "runs-on" if value.is_empty() => {
                open = Some((if key == "needs" { "needs" } else { "runs-on" }, 4));
            }
            _ => {
                let items = list_value(value).ok_or_else(|| {
                    format!(
                        "job `{}` has an unreadable `{key}:` value `{value}`",
                        job.name
                    )
                })?;
                if key == "needs" {
                    job.needs = items;
                    closed = Some("needs");
                } else {
                    job.runs_on = items;
                    closed = Some("runs-on");
                }
            }
        }
    }
    if jobs.is_empty() {
        return Err("ci-portable.yml declares no jobs".to_string());
    }
    Ok(jobs)
}

/// ci-portable.yml must run nothing for a bot or draft pull request, and only
/// on GitHub-hosted runners:
/// - a job with no `needs:` must have exactly PORTABLE_PR_GATE as its `if:`;
/// - `regular` must be exactly `always() && (gate)` and `cleanup` exactly
///   `always() && (gate) && (fork guard)`;
/// - any other job whose `if:` uses a status function (always(), cancelled(),
///   failure()), which drops the default success() over its needs, must carry a
///   top-level `&& needs.<J>.result == 'success'` for a job J it needs that is
///   itself gated; a job without one is gated when any job it needs is gated;
/// - every `runs-on` must be a PORTABLE_HOSTED_RUNNERS label.
fn validate_portable_pr_gate(source: &str) -> Vec<String> {
    let jobs = match portable_jobs(source) {
        Ok(jobs) => jobs,
        Err(error) => return vec![error],
    };
    let mut errors = Vec::new();
    let gate = normalized_condition(PORTABLE_PR_GATE);
    let regular = format!("always() && ({gate})");
    let cleanup = format!("always() && ({gate}) && ({PORTABLE_FORK_GUARD})");
    for job in &jobs {
        if job.runs_on.is_empty() {
            errors.push(format!(
                "ci-portable.yml job `{}` has no `runs-on`",
                job.name
            ));
        }
        for label in &job.runs_on {
            if !PORTABLE_HOSTED_RUNNERS.contains(&label.as_str()) {
                errors.push(format!(
                    "ci-portable.yml job `{}` must run on a GitHub-hosted runner ({}), not `{label}`",
                    job.name,
                    PORTABLE_HOSTED_RUNNERS.join(", ")
                ));
            }
        }
    }
    let mut gated = BTreeSet::new();
    let mut verdict: std::collections::BTreeMap<String, Result<(), String>> = Default::default();
    // Fixed point over the needs graph: a job's verdict can depend on its needs.
    for _ in 0..=jobs.len() {
        for job in &jobs {
            if verdict.get(&job.name).is_some_and(Result::is_ok) {
                continue;
            }
            let condition = job
                .condition
                .as_deref()
                .map(normalized_condition)
                .unwrap_or_default();
            let result = if job.needs.is_empty() {
                if condition == gate {
                    Ok(())
                } else {
                    Err(format!(
                        "ci-portable.yml job `{}` has no `needs:` and its `if:` must be exactly: {PORTABLE_PR_GATE}",
                        job.name
                    ))
                }
            } else if job.name == "regular" {
                if condition == regular {
                    Ok(())
                } else {
                    Err(format!(
                        "ci-portable.yml job `regular` must have `if:` exactly: {regular}"
                    ))
                }
            } else if job.name == "cleanup" {
                if condition == cleanup {
                    Ok(())
                } else {
                    Err(format!(
                        "ci-portable.yml job `cleanup` deletes artifacts and must have `if:` exactly: {cleanup}"
                    ))
                }
            } else if has_status_function(&condition) && has_top_level_or(&condition) {
                Err(format!(
                    "ci-portable.yml job `{}` combines a status function with a top-level `||`, which makes its success requirement optional",
                    job.name
                ))
            } else if has_status_function(&condition) {
                let anchored = top_level_conjuncts(&condition).iter().any(|part| {
                    part.strip_prefix("needs.")
                        .and_then(|rest| rest.strip_suffix(".result == 'success'"))
                        .is_some_and(|need| {
                            job.needs.iter().any(|n| n == need) && gated.contains(need)
                        })
                });
                if anchored {
                    Ok(())
                } else {
                    Err(format!(
                        "ci-portable.yml job `{}` uses a status function in `if:` and must also require a gated job it needs to succeed (`&& needs.<job>.result == 'success'`)",
                        job.name
                    ))
                }
            } else if job.needs.iter().any(|need| gated.contains(need)) {
                Ok(())
            } else {
                Err(format!(
                    "ci-portable.yml job `{}` needs no gated job",
                    job.name
                ))
            };
            if result.is_ok() {
                gated.insert(job.name.clone());
            }
            verdict.insert(job.name.clone(), result);
        }
    }
    for job in &jobs {
        if let Some(Err(error)) = verdict.get(&job.name) {
            errors.push(error.clone());
        }
    }
    errors
}

fn named_step<'a>(job: &'a str, name: &str) -> Result<(usize, &'a str), String> {
    let marker = format!("      - name: {name}");
    if job.matches(&marker).count() != 1 {
        return Err(format!("expected exactly one `{name}` step"));
    }
    let start = job
        .find(&marker)
        .expect("unique marker must have an offset");
    let rest = &job[start..];
    let end = rest[1..]
        .find("\n      - ")
        .map_or(rest.len(), |offset| offset + 1);
    Ok((start, rest[..end].trim_end()))
}

fn validate_validation_levels_bootstrap(source: &str) -> Vec<String> {
    let mut errors = Vec::new();
    let version_declaration = format!("  RUST_SCRIPT_VERSION: \"{RUST_SCRIPT_VERSION}\"");
    if source.matches("RUST_SCRIPT_VERSION:").count() != 1
        || !source.lines().any(|line| line == version_declaration)
    {
        errors.push(format!(
            "validation-levels.yml must declare exactly one global RUST_SCRIPT_VERSION pinned to {RUST_SCRIPT_VERSION}"
        ));
    }

    let Some(quick_start) = source.find("\n  quick:\n") else {
        errors.push("validation-levels.yml is missing its quick job".to_string());
        return errors;
    };
    let Some(relative_quick_end) = source[quick_start + 1..].find("\n  full:\n") else {
        errors.push("validation-levels.yml quick job has no full-job boundary".to_string());
        return errors;
    };
    let quick_end = quick_start + 1 + relative_quick_end;
    let quick = &source[quick_start..quick_end];

    let expected_install = "      - name: Install rust-script\n        env:\n          RUSTFLAGS: \"\"\n        run: cargo install rust-script --version \"${RUST_SCRIPT_VERSION}\" --locked";
    let expected_verification = r#"      - name: Verify rust-script resolves at the pinned version
        run: |
          rust_script_path=$(command -v rust-script) || {
            echo "::error::rust-script is not on PATH; scripts/validate.rs would die at exec with status 127."
            exit 1
          }
          actual_version=$(rust-script --version)
          expected_version="rust-script ${RUST_SCRIPT_VERSION}"
          if [[ "$actual_version" != "$expected_version" ]]; then
            echo "::error::expected ${expected_version}, found ${actual_version} at ${rust_script_path}"
            exit 1
          fi
          echo "rust-script resolved: ${rust_script_path} ${actual_version}""#;
    let expected_validation = "      - name: GitHub-managed portable test lane\n        run: ./scripts/validate.rs --portable-only --no-label-pr";

    let install = named_step(quick, "Install rust-script");
    let verification = named_step(quick, "Verify rust-script resolves at the pinned version");
    let validation = named_step(quick, "GitHub-managed portable test lane");
    if install.as_ref().map(|(_, step)| *step) != Ok(expected_install) {
        errors.push(
            "validation-levels.yml quick job must install the pinned rust-script with tool-only RUSTFLAGS"
                .to_string(),
        );
    }
    if verification.as_ref().map(|(_, step)| *step) != Ok(expected_verification) {
        errors.push(
            "validation-levels.yml quick job must fail closed unless rust-script resolves at the pinned version"
                .to_string(),
        );
    }
    if validation.as_ref().map(|(_, step)| *step) != Ok(expected_validation) {
        errors.push(
            "validation-levels.yml quick job must run the canonical portable validation command"
                .to_string(),
        );
    }

    let validation_command = "./scripts/validate.rs --portable-only --no-label-pr";
    let actual_validation_offset = quick.find(validation_command);
    if quick.matches(validation_command).count() != 1
        || !matches!(
            (&install, &verification, &validation, actual_validation_offset),
            (Ok((install, _)), Ok((verify, _)), Ok((validation, _)), Some(actual))
                if install < verify && verify < validation && validation < &actual
        )
    {
        errors.push(
            "validation-levels.yml must install and verify rust-script before invoking scripts/validate.rs"
                .to_string(),
        );
    }
    errors
}

fn buck2_nightly_required_markers() -> Vec<(&'static str, &'static str)> {
    let path_export = r#"echo "$RUNNER_TEMP/cargo-home/bin" >>"$GITHUB_PATH""#;
    let install = r#"cargo install rust-script --version 0.36.0 --locked --root "$CARGO_HOME""#;
    let path_binding = r#"test "$(realpath "$resolved_rust_script")" = "$(realpath "$CARGO_HOME/bin/rust-script")""#;
    let version_binding = r#"test "$("$resolved_rust_script" --version)" = "rust-script 0.36.0""#;
    vec![
        (
            "180-minute supplemental release-evidence job",
            "  buck-release-evidence:\n    name: Supplemental feature-complete Buck release evidence\n    runs-on: ubuntu-24.04\n    timeout-minutes: 180",
        ),
        (
            "restored 120-minute legacy OSS Buck job",
            "  oss-buck2:\n    name: Regenerate and build the OSS Buck2 graph\n    runs-on: ubuntu-24.04\n    timeout-minutes: 120",
        ),
        (
            "read-only workflow permissions",
            "permissions:\n  contents: read",
        ),
        ("checkout credential refusal", "persist-credentials: false"),
        ("isolated rust-script PATH export", path_export),
        ("isolated rust-script install root", install),
        (
            "isolated rust-script executable check",
            r#"test -x "$CARGO_HOME/bin/rust-script""#,
        ),
        (
            "resolved rust-script path lookup",
            r#"resolved_rust_script="$(command -v rust-script)""#,
        ),
        ("resolved rust-script path binding", path_binding),
        ("pinned rust-script version binding", version_binding),
        (
            "Buck release driver refusal tests",
            "rust-script --test scripts/build-buck-release.rs",
        ),
        (
            "release capture step",
            "      - name: Capture the feature-complete Hermit release build\n        id: release-build",
        ),
        (
            "inner process-tree build deadline",
            "          timeout --foreground --kill-after=30s 40m \\\n            \"$PUBLIC_DOTSLASH\" ./bootstrap/buck2 build --show-output --no-remote-cache \\",
        ),
        (
            "capture EXIT receipt trap",
            "          trap retain_shell_exit EXIT",
        ),
        (
            "recognized release event-log filename",
            "          event_log=\"$evidence/hermit-release.json-lines.gz\"",
        ),
        (
            "Cargo package version derivation",
            "          version=\"$(sed -n '/^\\[package\\]/,/^\\[/s/^version = \"\\([^\"]*\\)\"/\\1/p' hermit-cli/Cargo.toml | head -n 1)\"",
        ),
        (
            "UTC build-date derivation",
            "          build_date=\"$(date -u +%F)\"",
        ),
        (
            "12-character Hermit SHA derivation",
            "          hermit_sha=\"$(git rev-parse --short=12 HEAD)\"",
        ),
        (
            "40-character Reverie gitlink derivation",
            "          reverie_sha=\"$(git rev-parse HEAD:reverie)\"",
        ),
        (
            "Hermit SHA length assertion",
            "          test \"${#hermit_sha}\" -eq 12",
        ),
        (
            "Reverie SHA length assertion",
            "          test \"${#reverie_sha}\" -eq 40",
        ),
        (
            "explicit public DotSlash release build",
            "            \"$PUBLIC_DOTSLASH\" ./bootstrap/buck2 build --show-output --no-remote-cache \\",
        ),
        (
            "recognized release event-log argument",
            "            --event-log \"$event_log\" \\",
        ),
        (
            "release version metadata argument",
            "            -c \"hermit_release.version=$version\" \\",
        ),
        (
            "release date metadata argument",
            "            -c \"hermit_release.build_date=$build_date\" \\",
        ),
        (
            "release Hermit SHA metadata argument",
            "            -c \"hermit_release.hermit_sha=$hermit_sha\" \\",
        ),
        (
            "release Reverie SHA metadata argument",
            "            -c \"hermit_release.reverie_sha=$reverie_sha\" \\",
        ),
        (
            "feature-complete release target argument",
            "            //hermit-cli:hermit-release \\",
        ),
        (
            "captured shell exit",
            "printf 'shell_exit\\t%s\\n' \"$capture_rc\" >\"$evidence/shell-exit.tsv\"",
        ),
        ("capture status propagation", "          exit \"$shell_rc\""),
        (
            "always-run reconciliation step",
            "      - name: Reconcile and scan release evidence\n        id: release-evidence\n        if: always()",
        ),
        (
            "explicit public DotSlash event summary",
            "          \"$PUBLIC_DOTSLASH\" ./bootstrap/buck2 log summary \\",
        ),
        (
            "typed release evidence reconciler",
            "          ./scripts/build-buck-release.rs --reconcile-release-evidence \\",
        ),
        (
            "reconciler show-output binding",
            "            --show-output \"$evidence/build.stdout\" \\",
        ),
        (
            "reconciler shell-exit binding",
            "            --shell-exit \"$shell_rc\" \\",
        ),
        (
            "combined captured-byte evidence packager",
            "          ./scripts/build-buck-release.rs --scan-and-package-public-evidence \\",
        ),
        (
            "combined scanner receipt output",
            "            --receipt-output \"$scan_receipt\" \\",
        ),
        (
            "scanner failure refusal",
            "          ./scripts/build-buck-release.rs --release-evidence-verdict \\",
        ),
        (
            "zero-marker assertion",
            "          test \"$marker_count\" = 0",
        ),
        (
            "reconciliation failure refusal",
            "            --reconciliation-exit \"$reconciliation_rc\"",
        ),
        (
            "external immutable archive binding",
            "            --archive \"$archive\"",
        ),
        (
            "archive canonical-path equality",
            "          test \"$artifact_path\" = \"$archive\"",
        ),
        (
            "canonical immutable archive path",
            "          artifact_path=\"$(realpath -e \"$archive\")\"",
        ),
        (
            "canonical artifact output",
            "          echo \"artifact-path=$artifact_path\" >>\"$GITHUB_OUTPUT\"",
        ),
        (
            "conditional safe upload output",
            "          echo \"safe-to-upload=true\" >>\"$GITHUB_OUTPUT\"",
        ),
        (
            "safe public upload condition",
            "if: always() && steps.release-evidence.outputs.safe-to-upload == 'true'",
        ),
        (
            "release evidence artifact path",
            "path: ${{ steps.release-evidence.outputs.artifact-path }}",
        ),
        (
            "upload missing-file refusal",
            "          if-no-files-found: error",
        ),
        (
            "diagnostic after evidence upload",
            "      - name: Run the full third-party diagnostic",
        ),
        (
            "diagnostic non-upload disclosure",
            "      - name: Scan and package the full third-party diagnostic",
        ),
        (
            "guarded legacy diagnostic upload",
            "if: always() && steps.third-party-evidence.outputs.safe-to-upload == 'true'",
        ),
        (
            "preserved legacy diagnostic artifact name",
            "name: buck2-third-party-diagnostic-${{ github.run_id }}",
        ),
        (
            "immutable legacy diagnostic staging path",
            "path: ${{ steps.third-party-evidence.outputs.artifact-path }}",
        ),
        (
            "combined captured-byte legacy staging",
            "          ./scripts/build-buck-release.rs --scan-and-stage-public-evidence \\",
        ),
        ("legacy Hermit target", "//hermit-cli:hermit'"),
    ]
}

// These non-cryptographic hashes are drift tripwires for reviewed workflow
// blocks. Semantic validators and mutation tests remain the security boundary.
const RELEASE_CAPTURE_BLOCK_FNV1A64: u64 = 0xd47269f7822be906;
const RELEASE_FINALIZE_BLOCK_FNV1A64: u64 = 0x2391b53a25c3213a;
const RELEASE_UPLOAD_BLOCK_FNV1A64: u64 = 0x729e01b3370e7f6f;
const LEGACY_REVERIE_BLOCK_FNV1A64: u64 = 0x560c7d2fcc02b62f;
const LEGACY_DIAGNOSTIC_CAPTURE_BLOCK_FNV1A64: u64 = 0x0ce72a075541cfdf;
const LEGACY_DIAGNOSTIC_FINALIZE_BLOCK_FNV1A64: u64 = 0x6c037b9cd4dabcaf;
const LEGACY_DIAGNOSTIC_UPLOAD_BLOCK_FNV1A64: u64 = 0x48723ec958bff433;
const LEGACY_FINAL_TARGET_BLOCK_FNV1A64: u64 = 0x2a320ec4e59c3edc;

fn named_step_block<'a>(source: &'a str, name: &str) -> Option<&'a str> {
    let marker = format!("      - name: {name}\n");
    let start = source.find(&marker)?;
    let rest = &source[start + marker.len()..];
    let end = rest
        .find("\n      - name: ")
        .map(|offset| start + marker.len() + offset + 1)
        .or_else(|| {
            rest.find("\n  oss-buck2:")
                .map(|offset| start + marker.len() + offset + 1)
        })
        .unwrap_or(source.len());
    source.get(start..end)
}

fn fnv1a64(text: &str) -> u64 {
    text.replace("\r\n", "\n")
        .trim_end()
        .bytes()
        .fold(0xcbf29ce484222325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
        })
}

fn validate_bound_release_blocks(source: &str) -> Vec<String> {
    let mut errors = Vec::new();
    for (name, expected) in [
        (
            "Capture the feature-complete Hermit release build",
            RELEASE_CAPTURE_BLOCK_FNV1A64,
        ),
        (
            "Reconcile and scan release evidence",
            RELEASE_FINALIZE_BLOCK_FNV1A64,
        ),
        (
            "Upload feature-complete release evidence",
            RELEASE_UPLOAD_BLOCK_FNV1A64,
        ),
        (
            "Build the four Reverie targets",
            LEGACY_REVERIE_BLOCK_FNV1A64,
        ),
        (
            "Run the full third-party diagnostic",
            LEGACY_DIAGNOSTIC_CAPTURE_BLOCK_FNV1A64,
        ),
        (
            "Scan and package the full third-party diagnostic",
            LEGACY_DIAGNOSTIC_FINALIZE_BLOCK_FNV1A64,
        ),
        (
            "Upload the full third-party diagnostic log",
            LEGACY_DIAGNOSTIC_UPLOAD_BLOCK_FNV1A64,
        ),
        (
            "Build the Hermit green gate",
            LEGACY_FINAL_TARGET_BLOCK_FNV1A64,
        ),
    ] {
        match named_step_block(source, name) {
            Some(block) if fnv1a64(block) == expected => {}
            Some(block) => errors.push(format!(
                "buck2-oss-nightly.yml bound step {name:?} changed: expected fnv1a64={expected:016x}, got {:016x}",
                fnv1a64(block)
            )),
            None => errors.push(format!(
                "buck2-oss-nightly.yml lacks bound step {name:?}"
            )),
        }
    }
    errors
}

fn validate_buck2_nightly_contract(source: &str) -> Vec<String> {
    let mut errors = Vec::new();
    errors.extend(validate_bound_release_blocks(source));
    for (description, marker) in buck2_nightly_required_markers() {
        let expected = match description {
            "checkout credential refusal"
            | "isolated rust-script PATH export"
            | "isolated rust-script install root"
            | "isolated rust-script executable check"
            | "resolved rust-script path lookup"
            | "resolved rust-script path binding"
            | "pinned rust-script version binding"
            | "reconciler show-output binding"
            | "canonical artifact output"
            | "conditional safe upload output"
            | "upload missing-file refusal" => 2,
            _ => 1,
        };
        if source.matches(marker).count() != expected {
            errors.push(format!(
                "buck2-oss-nightly.yml must contain exactly {expected} occurrence(s) of {description}"
            ));
        }
    }

    let path_export = r#"echo "$RUNNER_TEMP/cargo-home/bin" >>"$GITHUB_PATH""#;
    let install = r#"cargo install rust-script --version 0.36.0 --locked --root "$CARGO_HOME""#;
    let path_binding = r#"test "$(realpath "$resolved_rust_script")" = "$(realpath "$CARGO_HOME/bin/rust-script")""#;
    let version_binding = r#"test "$("$resolved_rust_script" --version)" = "rust-script 0.36.0""#;
    let path_offset = source.find(path_export);
    let install_offset = source.find(install);
    let binding_offset = source.find(path_binding);
    let version_offset = source.find(version_binding);
    let regeneration_offset = source.find("./bootstrap/regenerate-rust-deps");
    let capture_offset =
        source.find("      - name: Capture the feature-complete Hermit release build");
    let reconcile_offset = source.find("      - name: Reconcile and scan release evidence");
    let scan_offset =
        source.find("./scripts/build-buck-release.rs --scan-and-package-public-evidence");
    let zero_marker_offset = source.find("test \"$marker_count\" = 0");
    let artifact_output_offset =
        source.find("echo \"artifact-path=$artifact_path\" >>\"$GITHUB_OUTPUT\"");
    let safe_output_offset = source.find("echo \"safe-to-upload=true\" >>\"$GITHUB_OUTPUT\"");
    let verdict_offset = source.find("./scripts/build-buck-release.rs --release-evidence-verdict");
    let upload_offset = source.find("      - name: Upload feature-complete release evidence");
    let diagnostic_offset = source.find("      - name: Run the full third-party diagnostic");
    let legacy_offset = source.find("      - name: Build the Hermit green gate");
    if !matches!(
        (
            path_offset,
            install_offset,
            binding_offset,
            version_offset,
            regeneration_offset,
            capture_offset,
            reconcile_offset,
            scan_offset,
            zero_marker_offset,
            artifact_output_offset,
            safe_output_offset,
            verdict_offset,
            upload_offset,
            diagnostic_offset,
            legacy_offset,
        ),
        (
            Some(path),
            Some(install),
            Some(binding),
            Some(version),
            Some(regeneration),
            Some(capture),
            Some(reconcile),
            Some(scan),
            Some(zero_marker),
            Some(artifact_output),
            Some(safe_output),
            Some(verdict),
            Some(upload),
            Some(diagnostic),
            Some(legacy),
        )
            if path < install
                && install < binding
                && binding < version
                && version < regeneration
                && regeneration < capture
                && capture < reconcile
                && reconcile < scan
                && scan < zero_marker
                && zero_marker < artifact_output
                && artifact_output < safe_output
                && safe_output < verdict
                && verdict < upload
                && upload < diagnostic
                && diagnostic < legacy
    ) {
        errors.push(
            "buck2-oss-nightly.yml load-bearing tool/build/reconcile/scan/upload/diagnostic ordering drifted"
                .to_string(),
        );
    }
    let global = source
        .split_once("\njobs:")
        .map(|(global, _)| global)
        .unwrap_or_default();
    let carriers = [
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "ACTIONS_RUNTIME_TOKEN",
        "ACTIONS_ID_TOKEN_REQUEST_TOKEN",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
    ];
    for carrier in carriers {
        if global.contains(&format!("  {carrier}:")) {
            errors.push(format!(
                "buck2-oss-nightly.yml must not globally blank token carrier {carrier}; upload actions need their runtime credentials"
            ));
        }
    }
    let mut starts = source
        .match_indices("      - name: ")
        .map(|(offset, _)| offset)
        .collect::<Vec<_>>();
    starts.push(source.len());
    for pair in starts.windows(2) {
        let block = &source[pair[0]..pair[1]];
        let name = block.lines().next().unwrap_or("unnamed step");
        if block.contains("        shell: bash\n") {
            for carrier in carriers {
                let marker = format!("          {carrier}: \"\"");
                if block.matches(&marker).count() != 1 {
                    errors.push(format!(
                        "buck2-oss-nightly.yml shell step {name:?} must explicitly clear {carrier} exactly once"
                    ));
                }
            }
        }
        if block.contains("uses: actions/upload-artifact@")
            && ["ACTIONS_RUNTIME_TOKEN", "ACTIONS_ID_TOKEN_REQUEST_TOKEN"]
                .into_iter()
                .any(|carrier| block.contains(&format!("{carrier}: \"\"")))
        {
            errors.push(format!(
                "buck2-oss-nightly.yml upload step {name:?} must inherit Actions runtime/id tokens"
            ));
        }
    }
    if upload_offset.is_some()
        && reconcile_offset.is_some()
        && source[..upload_offset.unwrap()]
            .rfind("\n      - name:")
            .is_some_and(|offset| offset + 1 != reconcile_offset.unwrap())
    {
        errors.push(
            "buck2-oss-nightly.yml must upload the immutable archive immediately after reconciliation"
                .to_owned(),
        );
    }
    let diagnostic_finalize =
        source.find("      - name: Scan and package the full third-party diagnostic");
    let diagnostic_upload = source.find("      - name: Upload the full third-party diagnostic log");
    if diagnostic_upload.is_none()
        || diagnostic_finalize.is_none()
        || source[..diagnostic_upload.unwrap()]
            .rfind("\n      - name:")
            .is_none_or(|offset| offset + 1 != diagnostic_finalize.unwrap())
    {
        errors.push(
            "buck2-oss-nightly.yml must immediately upload the guarded legacy diagnostic staging directory"
                .to_owned(),
        );
    }
    for forbidden in [
        "command_success == true",
        "release_output_count",
        "reconciliation=pass",
        "marker_count=0",
        "scan_rc=0",
        "shell_rc=0",
        "summary_rc=0",
        "reconciliation_rc=0",
        "|| true",
        "path: ${{ runner.temp }}/buck2-phase1-release",
        "continue-on-error:",
        "    needs:",
        "${{ github.token }}",
        " --scan-public-evidence ",
        " --package-public-evidence ",
        " --stage-public-evidence ",
        " --verify-staged-public-evidence ",
    ] {
        if source.contains(forbidden) {
            errors.push(format!(
                "buck2-oss-nightly.yml contains forbidden evidence shortcut {forbidden:?}"
            ));
        }
    }
    errors
}

fn workflow_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let entries =
        fs::read_dir(dir).map_err(|error| format!("cannot read {}: {error}", dir.display()))?;
    let mut files = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|error| format!("cannot read workflow directory entry: {error}"))?
            .path();
        if matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("yml" | "yaml")
        ) {
            files.push(path);
        }
    }
    files.sort();
    if files.is_empty() {
        return Err(format!("no YAML workflows found under {}", dir.display()));
    }
    Ok(files)
}

fn run(dir: &Path) -> Result<usize, Vec<String>> {
    let files = workflow_files(dir).map_err(|error| vec![error])?;
    let mut errors = Vec::new();
    let mut saw_portable = false;
    for path in &files {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        saw_portable |= name == PORTABLE;
        let source = match fs::read_to_string(path) {
            Ok(source) => source,
            Err(error) => {
                errors.push(format!("{}: cannot read: {error}", path.display()));
                continue;
            }
        };
        match parse_triggers(&source) {
            Ok(triggers) => {
                errors.extend(
                    validate(name, &triggers)
                        .into_iter()
                        .map(|error| format!("{}: {error}", path.display())),
                );
            }
            Err(error) => errors.push(format!("{}: {error}", path.display())),
        }
        if name == PORTABLE {
            errors.extend(
                validate_portable_pr_gate(&source)
                    .into_iter()
                    .map(|error| format!("{}: {error}", path.display())),
            );
        }
        if name == VALIDATION_LEVELS {
            errors.extend(
                validate_validation_levels_bootstrap(&source)
                    .into_iter()
                    .map(|error| format!("{}: {error}", path.display())),
            );
        }
        if name == BUCK2_OSS_NIGHTLY {
            errors.extend(
                validate_buck2_nightly_contract(&source)
                    .into_iter()
                    .map(|error| format!("{}: {error}", path.display())),
            );
        }
    }
    if !saw_portable {
        errors.push(format!("{PORTABLE} is missing"));
    }
    if errors.is_empty() {
        Ok(files.len())
    } else {
        Err(errors)
    }
}

fn main() {
    rust_script_prelude::init();
    match run(Path::new(WORKFLOW_DIR)) {
        Ok(count) => println!(
            "GitHub Actions triggers OK: {count} workflows; automatic runs are limited to integration, non-bot pull requests and exact approved schedules"
        ),
        Err(errors) => {
            for error in errors {
                eprintln!("check-github-actions-triggers: {error}");
            }
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(source: &str) -> Triggers {
        parse_triggers(source).expect("fixture should parse")
    }

    const PORTABLE_ON: &str = "name: portable\non:\n  workflow_dispatch:\n  push:\n    branches: [integration]\n  pull_request:\n    types: [opened, synchronize, reopened, ready_for_review]\njobs:\n";

    #[test]
    fn portable_policy_accepts_integration_manual_and_gated_pull_requests() {
        assert!(validate(PORTABLE, &parsed(PORTABLE_ON)).is_empty());
    }

    #[test]
    fn portable_now_requires_its_pull_request_trigger() {
        let triggers = parsed(
            "name: portable\non:\n  workflow_dispatch:\n  push:\n    branches: [integration]\njobs:\n",
        );
        assert!(!validate(PORTABLE, &triggers).is_empty());
    }

    #[test]
    fn portable_pull_request_types_must_be_exact() {
        for types in [
            "[opened]",
            "[opened, synchronize, reopened, ready_for_review, labeled]",
            "[opened, synchronize, reopened]",
        ] {
            let source =
                PORTABLE_ON.replace("[opened, synchronize, reopened, ready_for_review]", types);
            assert!(!validate(PORTABLE, &parsed(&source)).is_empty(), "{types}");
        }
        let missing = PORTABLE_ON.replace(
            "    types: [opened, synchronize, reopened, ready_for_review]\n",
            "",
        );
        assert!(!validate(PORTABLE, &parsed(&missing)).is_empty());
    }

    #[test]
    fn pull_request_accepts_only_a_types_list() {
        for extra in [
            "    branches: [main]\n",
            "    paths: [src]\n",
            "    types:\n      - opened\n",
        ] {
            let source = PORTABLE_ON.replace(
                "    types: [opened, synchronize, reopened, ready_for_review]\n",
                extra,
            );
            assert!(
                parse_triggers(&source).is_err()
                    || !validate(PORTABLE, &parsed(&source)).is_empty(),
                "{extra:?}"
            );
        }
    }

    #[test]
    fn checked_in_portable_workflow_gates_every_entry_job() {
        let source = include_str!("../.github/workflows/ci-portable.yml");
        assert!(validate(PORTABLE, &parsed(source)).is_empty());
        let errors = validate_portable_pr_gate(source);
        assert!(errors.is_empty(), "{errors:?}");
    }

    /// A miniature of ci-portable.yml's job graph: two gated roots, plain jobs that
    /// need them, a status-function job anchored on a gated job, and the two
    /// always() jobs.
    fn portable_fixture() -> String {
        format!(
            "{PORTABLE_ON}  select:\n    name: Select\n    if: {PORTABLE_PR_GATE}\n    runs-on: ubuntu-latest\n  plan:\n    if: >-\n      {PORTABLE_PR_GATE}\n    runs-on: ubuntu-latest\n  preflight:\n    needs: select\n    if: needs.select.outputs.decision != 'skip'\n    runs-on: ubuntu-latest\n  build-debug:\n    needs: [select, preflight]\n    runs-on: ubuntu-latest\n  test-debug:\n    needs: [select, build-debug]\n    if: \"!cancelled() && needs.select.outputs.run_debug_shards != 'false' && needs.build-debug.result == 'success'\"\n    runs-on: ubuntu-latest\n  regular:\n    needs: [select, plan, test-debug]\n    if: always() && ({PORTABLE_PR_GATE})\n    runs-on: ubuntu-latest\n  cleanup:\n    needs:\n      - test-debug\n      - regular\n    if: always() && ({PORTABLE_PR_GATE}) && ({PORTABLE_FORK_GUARD})\n    runs-on: ubuntu-latest\n"
        )
    }

    #[test]
    fn the_portable_fixture_is_accepted() {
        let ok = portable_fixture();
        assert!(
            validate_portable_pr_gate(&ok).is_empty(),
            "{:?}",
            validate_portable_pr_gate(&ok)
        );
    }

    fn rejected_with(source: &str, needle: &str) {
        let errors = validate_portable_pr_gate(source);
        assert!(
            errors.iter().any(|error| error.contains(needle)),
            "expected an error containing {needle:?}, got {errors:?}"
        );
    }

    #[test]
    fn a_single_line_condition_cannot_continue_on_the_next_line() {
        // Review round 2, P1: `if: x` then `      || always()` was ignored.
        let source = format!(
            "{}  wrapped:\n    needs: select\n    if: needs.select.outputs.decision != 'skip'\n      || always()\n    runs-on: ubuntu-latest\n",
            portable_fixture()
        );
        rejected_with(&source, "continues its single-line `if:` value");
    }

    #[test]
    fn a_top_level_or_cannot_make_the_success_anchor_optional() {
        let ok = portable_fixture();
        // Review round 2, P4 and P14.
        for prefix in ["always() || true && ", "!cancelled() || (true) && "] {
            let source = ok.replace("\"!cancelled() && ", &format!("\"{prefix}"));
            assert_ne!(source, ok);
            rejected_with(&source, "top-level `||`");
        }
    }

    #[test]
    fn a_hash_inside_quotes_is_not_a_comment() {
        // Review round 2, P5: `contains(github.ref, '#') || always()` was cut at `#`.
        let source = format!(
            "{}  hashed:\n    needs: select\n    if: contains(github.ref, '#') || always()\n    runs-on: ubuntu-latest\n",
            portable_fixture()
        );
        rejected_with(&source, "`hashed`");
    }

    #[test]
    fn unsupported_block_scalar_headers_are_refused() {
        // Review round 2, P6 and P6b.
        for header in [">+", "|2", ">2-"] {
            let source = format!(
                "{}  blocky:\n    needs: select\n    if: {header}\n      always()\n    runs-on: ubuntu-latest\n",
                portable_fixture()
            );
            rejected_with(&source, "unsupported block-scalar header");
        }
    }

    #[test]
    fn yaml_aliases_are_refused() {
        // Review round 2, P7: `if: *st`.
        let source = format!(
            "{}  aliased:\n    needs: select\n    if: *st\n    runs-on: ubuntu-latest\n",
            portable_fixture()
        );
        rejected_with(&source, "YAML alias or anchor");
    }

    #[test]
    fn status_functions_are_found_in_any_case() {
        // Review round 2, P2: `${{ Always() }}`.
        let source = format!(
            "{}  shouty:\n    needs: select\n    if: ${{{{ Always() }}}}\n    runs-on: ubuntu-latest\n",
            portable_fixture()
        );
        rejected_with(&source, "uses a status function");
    }

    #[test]
    fn an_explicit_success_is_a_status_function() {
        // Review round 3, R1 and R2: written out, success() can be or-ed away
        // or negated, and then the job runs when the gated job was skipped.
        for condition in [
            "success() || github.event_name == 'pull_request'",
            "${{ !success() }}",
        ] {
            let source = format!(
                "{}  probe:\n    needs: select\n    if: {condition}\n    runs-on: ubuntu-latest\n",
                portable_fixture()
            );
            assert!(
                !validate_portable_pr_gate(&source).is_empty(),
                "{condition}"
            );
        }
    }

    #[test]
    fn status_functions_are_found_with_spaces_before_the_parenthesis() {
        // Review round 3, R3: `always ( )`.
        let source = format!(
            "{}  spaced:\n    needs: select\n    if: ${{{{ always ( ) }}}}\n    runs-on: ubuntu-latest\n",
            portable_fixture()
        );
        rejected_with(&source, "uses a status function");
    }

    #[test]
    fn a_job_key_must_be_written_plainly() {
        // Review round 3, R5-R7: YAML reads each of these as the key `if`.
        for line in [
            "\"if\": always()",
            "'if': always()",
            "if : always()",
            "? if",
            "<<: *base",
        ] {
            let source = format!(
                "{}  probe:\n    needs: select\n    {line}\n    runs-on: ubuntu-latest\n",
                portable_fixture()
            );
            rejected_with(&source, "not a plain `key:`");
        }
    }

    #[test]
    fn an_escaped_quote_does_not_end_a_double_quoted_string() {
        // Review round 3, R4: the `\"` must not reopen a comment before always().
        let source = format!(
            "{}  probe:\n    needs: select\n    if: \"contains(github.ref, '\\\" #') || always()\"\n    runs-on: ubuntu-latest\n",
            portable_fixture()
        );
        rejected_with(&source, "top-level `||`");
    }

    #[test]
    fn a_guarded_key_may_appear_once() {
        // Review round 3, R13: two `if:` keys, whichever GitHub keeps.
        let source = format!(
            "{}  twice:\n    needs: select\n    if: always()\n    if: needs.select.result == 'success'\n    runs-on: ubuntu-latest\n",
            portable_fixture()
        );
        rejected_with(&source, "repeats `if:`");
    }

    #[test]
    fn earlier_rejections_name_their_cause() {
        let ok = portable_fixture();
        rejected_with(
            &format!("{ok}  newroot:\n    runs-on: ubuntu-latest\n"),
            "has no `needs:`",
        );
        rejected_with(
            &ok.replace(&format!(" && ({PORTABLE_FORK_GUARD})"), ""),
            "job `cleanup`",
        );
        rejected_with(
            &ok.replace(" && needs.build-debug.result == 'success'", ""),
            "uses a status function",
        );
        rejected_with(
            &format!("{ok}  orphan:\n    needs: select\n"),
            "has no `runs-on`",
        );
    }

    fn rejected(source: &str, why: &str) {
        let errors = validate_portable_pr_gate(source);
        assert!(!errors.is_empty(), "accepted although {why}");
    }

    #[test]
    fn a_new_root_job_without_the_gate_is_rejected() {
        rejected(
            &format!(
                "{}  newroot:\n    runs-on: ubuntu-latest\n",
                portable_fixture()
            ),
            "a root job has no gate",
        );
    }

    #[test]
    fn a_job_header_with_a_trailing_comment_is_still_a_job() {
        // Review probe A: `evil: # note` used to be merged into the job above.
        rejected(
            &format!(
                "{}  evil: # note\n    runs-on: ubuntu-latest\n",
                portable_fixture()
            ),
            "a commented header hid an ungated job",
        );
    }

    #[test]
    fn an_optional_gate_is_rejected() {
        let ok = portable_fixture();
        // Review probe B: regular with `always() || (gate)`.
        rejected(
            &ok.replace(
                &format!("always() && ({PORTABLE_PR_GATE})\n    runs-on: ubuntu-latest\n  cleanup"),
                &format!("always() || ({PORTABLE_PR_GATE})\n    runs-on: ubuntu-latest\n  cleanup"),
            ),
            "regular's gate was made optional",
        );
        // Review probe E: a root with `gate || true`.
        rejected(
            &ok.replacen(
                &format!("    if: {PORTABLE_PR_GATE}\n"),
                &format!("    if: {PORTABLE_PR_GATE} || true\n"),
                1,
            ),
            "a root's gate was made optional",
        );
    }

    #[test]
    fn a_status_function_job_must_require_a_gated_job_to_succeed() {
        let ok = portable_fixture();
        // Review probe C: a new job with `needs: select` and `if: ${{ !cancelled() }}`.
        rejected(
            &format!(
                "{ok}  sneaky:\n    needs: select\n    if: ${{{{ !cancelled() }}}}\n    runs-on: ubuntu-latest\n"
            ),
            "a !cancelled() job runs without its gated need succeeding",
        );
        // Review probe D: test-debug without its needs.build-debug.result conjunct.
        rejected(
            &ok.replace(" && needs.build-debug.result == 'success'", ""),
            "test-debug lost its success anchor",
        );
        // The anchor must be a top-level conjunct, not hidden behind ||.
        rejected(
            &ok.replace(
                " && needs.build-debug.result == 'success'",
                " || needs.build-debug.result == 'success'",
            ),
            "the anchor is optional",
        );
        // And it must name a job the job actually needs.
        rejected(
            &ok.replace(
                "needs.build-debug.result == 'success'",
                "needs.other.result == 'success'",
            ),
            "the anchor names a job it does not need",
        );
    }

    #[test]
    fn cleanup_must_keep_the_fork_guard() {
        rejected(
            &portable_fixture().replace(&format!(" && ({PORTABLE_FORK_GUARD})"), ""),
            "cleanup lost the fork guard",
        );
    }

    #[test]
    fn every_portable_job_must_run_on_a_github_hosted_runner() {
        let ok = portable_fixture();
        // Review probe G: preflight moved to the self-hosted PMU pool.
        rejected(&ok.replacen("    needs: select\n    if: needs.select.outputs.decision != 'skip'\n    runs-on: ubuntu-latest\n", "    needs: select\n    if: needs.select.outputs.decision != 'skip'\n    runs-on: [self-hosted, hermit, pmu]\n", 1), "a job runs on a self-hosted runner");
        rejected(
            &ok.replacen(
                "    runs-on: ubuntu-latest\n  plan:",
                "    runs-on: self-hosted\n  plan:",
                1,
            ),
            "a root runs on a self-hosted runner",
        );
        rejected(
            &format!("{ok}  orphan:\n    needs: select\n"),
            "a job has no runs-on",
        );
    }

    #[test]
    fn manual_only_workflow_is_allowed() {
        let triggers = parsed("on:\n  workflow_dispatch:\njobs:\n");
        assert!(validate("other.yml", &triggers).is_empty());
    }

    #[test]
    fn exact_approved_schedules_are_allowed() {
        for (name, cron) in [
            ("buck2-oss-nightly.yml", "17 10 * * *"),
            ("docs.yml", "23 8 * * 0-6"),
        ] {
            let triggers = parsed(&format!(
                "on:\n  schedule:\n    - cron: \"{cron}\"\n  workflow_dispatch:\njobs:\n"
            ));
            assert!(validate(name, &triggers).is_empty(), "{name}");
        }
    }

    #[test]
    fn pull_request_and_schedule_are_rejected() {
        for event in [
            "pull_request",
            "pull_request_target",
            "schedule",
            "workflow_run",
        ] {
            let triggers = parsed(&format!("on:\n  workflow_dispatch:\n  {event}:\njobs:\n"));
            assert!(
                !validate("other.yml", &triggers).is_empty(),
                "{event} unexpectedly passed"
            );
        }
    }

    #[test]
    fn copied_or_modified_schedules_are_rejected() {
        for (name, cron) in [
            ("other.yml", "17 10 * * *"),
            ("buck2-oss-nightly.yml", "18 10 * * *"),
            ("docs.yml", "23 8 * * 1"),
            ("docs.yml", "23 8 * * *"),
        ] {
            let triggers = parsed(&format!(
                "on:\n  schedule:\n    - cron: \"{cron}\"\n  workflow_dispatch:\njobs:\n"
            ));
            assert!(!validate(name, &triggers).is_empty(), "{name} {cron}");
        }
    }

    #[test]
    fn approved_schedule_rejects_extra_automatic_trigger() {
        let triggers = parsed(
            "on:\n  schedule:\n    - cron: \"17 10 * * *\"\n  workflow_dispatch:\n  push:\n    branches: [integration]\njobs:\n",
        );
        assert!(!validate("buck2-oss-nightly.yml", &triggers).is_empty());
    }

    #[test]
    fn ambiguous_schedule_shapes_fail_to_parse() {
        for source in [
            "on:\n  schedule:\n    cron: \"17 10 * * *\"\n  workflow_dispatch:\njobs:\n",
            "on:\n  schedule:\n    - cron: 17 10 * * *\n  workflow_dispatch:\njobs:\n",
            "on:\n  schedule:\n    - cron: \"17 10 * * *\"\n      timezone: UTC\n  workflow_dispatch:\njobs:\n",
        ] {
            assert!(
                parse_triggers(source).is_err(),
                "schedule fixture unexpectedly parsed"
            );
        }
    }

    #[test]
    fn main_and_unbounded_pushes_are_rejected() {
        for push in ["  push:\n    branches: [main]\n", "  push:\n"] {
            let triggers = parsed(&format!("on:\n  workflow_dispatch:\n{push}jobs:\n"));
            assert!(!validate("other.yml", &triggers).is_empty());
        }
    }

    #[test]
    fn tag_pushes_are_rejected() {
        let source = "on:\n  workflow_dispatch:\n  push:\n    branches: [integration]\n    tags: [v1]\njobs:\n";
        let error = parse_triggers(source).expect_err("tag trigger unexpectedly parsed");
        assert!(error.contains("only `branches` is permitted"), "{error}");
    }

    #[test]
    fn deeper_push_structures_are_rejected() {
        let source = "on:\n  workflow_dispatch:\n  push:\n    branches: [integration]\n      unexpected: value\njobs:\n";
        let error = parse_triggers(source).expect_err("deeper push structure unexpectedly parsed");
        assert!(error.contains("ambiguous nested structure"), "{error}");
    }

    #[test]
    fn portable_requires_both_allowed_events() {
        let triggers = parsed("on:\n  workflow_dispatch:\njobs:\n");
        assert!(!validate(PORTABLE, &triggers).is_empty());
    }

    #[test]
    fn validation_levels_quick_job_bootstraps_the_pinned_rust_script() {
        let source = include_str!("../.github/workflows/validation-levels.yml");
        assert!(
            validate_validation_levels_bootstrap(source).is_empty(),
            "checked-in workflow lost its rust-script bootstrap"
        );
    }

    #[test]
    fn validation_levels_bootstrap_rejects_missing_or_unpinned_tools() {
        let source = include_str!("../.github/workflows/validation-levels.yml");
        let premature_validation = source
            .replacen(
                "      - name: GitHub-managed portable test lane\n        run: ./scripts/validate.rs --portable-only --no-label-pr",
                "      - name: GitHub-managed portable test lane\n        run: echo skipped",
                1,
            )
            .replacen(
                "      - name: Install rust-script",
                "      - name: Premature portable validation\n        run: ./scripts/validate.rs --portable-only --no-label-pr\n      - name: Install rust-script",
                1,
            );
        for broken in [
            source.replacen("  RUST_SCRIPT_VERSION: \"0.36.0\"\n", "", 1),
            source.replacen(
                "cargo install rust-script --version \"${RUST_SCRIPT_VERSION}\" --locked",
                "cargo install rust-script",
                1,
            ),
            source.replacen(
                "rust_script_path=$(command -v rust-script)",
                "rust_script_path=rust-script",
                1,
            ),
            source.replacen(
                "actual_version=$(rust-script --version)",
                "actual_version=unknown",
                1,
            ),
            source.replacen(
                "rust_script_path=$(command -v rust-script) || {\n            echo \"::error::rust-script is not on PATH; scripts/validate.rs would die at exec with status 127.\"\n            exit 1",
                "rust_script_path=$(command -v rust-script) || {\n            echo \"::error::rust-script is not on PATH; scripts/validate.rs would die at exec with status 127.\"\n            true",
                1,
            ),
            source.replacen(
                "echo \"::error::expected ${expected_version}, found ${actual_version} at ${rust_script_path}\"\n            exit 1",
                "echo \"::error::expected ${expected_version}, found ${actual_version} at ${rust_script_path}\"\n            true",
                1,
            ),
            source.replacen(
                "[[ \"$actual_version\" != \"$expected_version\" ]]",
                "[[ \"$actual_version\" == \"$expected_version\" ]]",
                1,
            ),
            premature_validation,
        ] {
            assert!(
                !validate_validation_levels_bootstrap(&broken).is_empty(),
                "broken bootstrap unexpectedly passed"
            );
        }
    }

    #[test]
    fn buck2_nightly_binds_tools_release_event_and_public_artifact_evidence() {
        let source = include_str!("../.github/workflows/buck2-oss-nightly.yml");
        assert!(
            validate_buck2_nightly_contract(source).is_empty(),
            "checked-in Buck2 nightly lost its fail-closed release contract"
        );
    }

    #[test]
    fn buck2_nightly_contract_refuses_path_and_evidence_gate_regressions() {
        let source = include_str!("../.github/workflows/buck2-oss-nightly.yml");
        for (description, marker) in buck2_nightly_required_markers() {
            let broken = source.replacen(marker, "removed-load-bearing-contract", 1);
            assert!(
                !validate_buck2_nightly_contract(&broken).is_empty(),
                "removing {description} unexpectedly passed"
            );
        }
        let path_export = r#"echo "$RUNNER_TEMP/cargo-home/bin" >>"$GITHUB_PATH""#;
        let install = r#"cargo install rust-script --version 0.36.0 --locked --root "$CARGO_HOME""#;
        let late_path = source.replacen(path_export, "", 1).replacen(
            install,
            &format!("{install}\n          {path_export}"),
            1,
        );
        let late_upload = source
            .replacen(
                "      - name: Upload feature-complete release evidence",
                "      - name: Misplaced release evidence upload",
                1,
            )
            .replacen(
                "      - name: Build the Hermit green gate",
                "      - name: Upload feature-complete release evidence\n        run: true\n      - name: Build the Hermit green gate",
                1,
            );
        for broken in [
            source.replacen(
                path_export,
                r#"echo "$HOME/.cargo/bin" >>"$GITHUB_PATH""#,
                1,
            ),
            late_path,
            late_upload,
            source.replacen(
                "          GH_TOKEN: \"\"",
                "          GH_TOKEN: inherited",
                1,
            ),
            source.replacen(
                "          test \"$marker_count\" = 0",
                "          marker_count=0\n          test \"$marker_count\" = 0",
                1,
            ),
            source.replacen(
                "          [[ $shell_rc =~ ^[0-9]+$ ]]",
                "          shell_rc=0\n          [[ $shell_rc =~ ^[0-9]+$ ]]",
                1,
            ),
            source.replacen(
                "          summary_rc=$?",
                "          summary_rc=$?\n          summary_rc=0",
                1,
            ),
            source.replacen(
                "          reconciliation_rc=$?",
                "          reconciliation_rc=$?\n          reconciliation_rc=0",
                1,
            ),
            source.replacen(
                "          summary_rc=$?",
                "          summary_rc=$?\n          summary_rc=$((summary_rc * 0))",
                1,
            ),
            source.replacen(
                "          ./scripts/build-buck-release.rs --release-evidence-verdict \\",
                "          if false; then\n            ./scripts/build-buck-release.rs --release-evidence-verdict \\",
                1,
            ),
            source.replacen(
                "          exit \"$shell_rc\"",
                "          : \"$shell_rc\"",
                1,
            ),
            source.replacen("          set +e\n", "", 1),
            source.replacen(
                "          candidate_rc=$?\n          set -e",
                "          set -e\n          candidate_rc=$?",
                1,
            ),
            source.replacen(
                "          ./scripts/build-buck-release.rs --scan-and-package-public-evidence \\",
                "          ./scripts/build-buck-release.rs --scan-and-package-public-evidence \\ || true",
                1,
            ),
            source.replacen(
                "        if: always() && steps.release-evidence.outputs.safe-to-upload == 'true'",
                "        if: always()",
                1,
            ),
            source.replacen(
                "          echo \"safe-to-upload=true\" >>\"$GITHUB_OUTPUT\"",
                "          echo \"safe-to-upload=true\" >>\"$GITHUB_OUTPUT\"\n          echo \"safe-to-upload=true\" >>\"$GITHUB_OUTPUT\"",
                1,
            ),
            source.replacen(
                "path: ${{ steps.release-evidence.outputs.artifact-path }}",
                "path: ${{ runner.temp }}/buck2-phase1-release",
                1,
            ),
            source.replacen(
                "      - name: Upload feature-complete release evidence",
                "      - name: Interposed mutable diagnostic\n        run: true\n\n      - name: Upload feature-complete release evidence",
                1,
            ),
        ] {
            assert!(
                !validate_buck2_nightly_contract(&broken).is_empty(),
                "broken Buck2 nightly contract unexpectedly passed"
            );
        }
        for carrier in [
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "ACTIONS_RUNTIME_TOKEN",
            "ACTIONS_ID_TOKEN_REQUEST_TOKEN",
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
        ] {
            let cleared = format!("          {carrier}: \"\"");
            let broken = source.replacen(&cleared, &format!("          {carrier}: inherited"), 1);
            assert_ne!(source, broken, "missing token fixture for {carrier}");
            assert!(
                !validate_buck2_nightly_contract(&broken).is_empty(),
                "inheriting {carrier} in a load-bearing step unexpectedly passed"
            );
        }
        let github_token_reintroduced = source.replacen(
            "          GITHUB_TOKEN: \"\"",
            "          GITHUB_TOKEN: ${{ github.token }}",
            1,
        );
        assert!(
            !validate_buck2_nightly_contract(&github_token_reintroduced).is_empty(),
            "reintroducing github.token into a shell step unexpectedly passed"
        );
    }

    #[test]
    fn every_single_byte_mutation_of_bound_release_blocks_is_refused() {
        let source = include_str!("../.github/workflows/buck2-oss-nightly.yml");
        for name in [
            "Capture the feature-complete Hermit release build",
            "Reconcile and scan release evidence",
            "Upload feature-complete release evidence",
            "Build the four Reverie targets",
            "Run the full third-party diagnostic",
            "Scan and package the full third-party diagnostic",
            "Upload the full third-party diagnostic log",
            "Build the Hermit green gate",
        ] {
            let block = named_step_block(source, name).unwrap();
            let start = block.as_ptr() as usize - source.as_ptr() as usize;
            for index in 0..block.len() {
                let mut bytes = block.as_bytes().to_vec();
                bytes[index] = if bytes[index] == b'x' { b'y' } else { b'x' };
                let changed = String::from_utf8(bytes).unwrap();
                let mut mutated = source.to_owned();
                mutated.replace_range(start..start + block.len(), &changed);
                assert!(
                    !validate_bound_release_blocks(&mutated).is_empty(),
                    "single-byte mutation {index} of {name} escaped the bound-block contract"
                );
            }
        }
    }

    #[test]
    fn ambiguous_trigger_shapes_fail_to_parse() {
        for source in [
            "on: [push, workflow_dispatch]\njobs:\n",
            "on:\n  workflow_dispatch: {}\njobs:\n",
            "on:\n  push:\n    branches:\n      - integration\njobs:\n",
        ] {
            assert!(
                parse_triggers(source).is_err(),
                "fixture unexpectedly parsed"
            );
        }
    }

    #[test]
    fn duplicate_alternate_on_form_is_rejected() {
        for alternate in [
            "on: [pull_request]",
            "on : [pull_request]",
            "\"on\": [pull_request]",
            "'on': [pull_request]",
            "!!str on: [pull_request]",
            "? on\n: [pull_request]",
        ] {
            let source = format!("on:\n  workflow_dispatch:\n{alternate}\njobs:\n  test:\n");
            assert!(
                parse_triggers(&source).is_err(),
                "alternate top-level on unexpectedly parsed: {alternate}"
            );
        }
    }

    #[test]
    fn quoted_top_level_on_is_rejected() {
        for spelling in ["\"on\"", "'on'"] {
            let source = format!("{spelling}:\n  workflow_dispatch:\njobs:\n");
            let error =
                parse_triggers(&source).expect_err("quoted top-level on unexpectedly parsed");
            assert!(error.contains("unquoted simple spelling"), "{error}");
        }
    }
}
