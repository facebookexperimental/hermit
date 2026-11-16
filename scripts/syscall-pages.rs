#!/usr/bin/env -S rust-script --force
//! Copyright (c) Meta Platforms, Inc. and affiliates.
//! All rights reserved.
//!
//! This source code is licensed under the BSD-style license found in the
//! LICENSE file in the root directory of this source tree.
//!
//! ```cargo
//! [dependencies]
//! serde_json = "=1.0.154"
//! ```
//!
//! Per-syscall Detcore pages: lint, generate, facts, aggregate, render,
//! and check.
//!
//! The facts-only pages in `docs/syscalls/<name>.md` record the
//! code-derived Detcore strategy for each syscall in this tree's
//! Detcore census. Checked analysis prose is not in them yet; it returns
//! in batches. Which backends can run a syscall is not tracked here.
//! Test coverage is likewise separate: generated indexes link the
//! coverage audit and never merge its numbers into the strategy table.
//! The normative page format is `docs/syscalls/SCHEMA.md`.
//!
//! Subcommands:
//! - `lint` — schema, sections, Status format, public-text rules, and
//!   discussion-link shape. No build, runs in seconds.
//! - `generate` — (re)generate every facts-only page from this tree's
//!   Detcore sources, optionally overlaying an exporter TSV for exact
//!   numbers and refused/subscription facts.
//! - `facts [--apply]` — export per-syscall policy from Detcore itself
//!   (public API only, through `scripts/syscall-pages-facts.rs`) plus the
//!   dispatch parse. Needs a git checkout with the `reverie/` submodule.
//! - `aggregate [--check]`, `render [--check]` — regenerate or verify
//!   the JSON table, index, and static site, byte for byte.
//! - `check [--quick]` — lint + derived census + classes + dispatch +
//!   citations + generated-output freshness and public-text scan, all
//!   from this tree's sources; full `check` also compares exporter facts
//!   (the git-only part, named as such in SCHEMA.md).
//!
//! What the census or dispatch parse cannot understand is
//! `parser-failed`, never a pass. What `lint` cannot parse is an error,
//! never a skip.
//!
//! Not ported from the predecessor system: its C08 rule resolving a
//! recorded coverage token to a strict-container test. Facts-only pages
//! record no coverage tokens at all, and SCHEMA.md states that no page
//! claim is machine-verified coverage until that check exists.

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::exit;

const SCHEMA: &str = "syscall-pages/v2";
const PAGES_DIR: &str = "docs/syscalls";
const JSON_FILE: &str = "docs/syscalls/syscalls.json";
const INDEX_FILE: &str = "docs/syscalls/README.md";
/// Public test-coverage measurement linked (never merged) from
/// generated indexes: `docs/SYSCALL_TEST_COVERAGE.md`, the exercised
/// count from the published validation series. This is deliberately not
/// `docs/SYSCALL_COVERAGE.md`, which audits trap dispositions, a
/// different measurement.
const COVERAGE_URL: &str =
    "https://github.com/facebookexperimental/hermit/blob/main/docs/SYSCALL_TEST_COVERAGE.md";
/// Facts-only page sections, in order. Analysis prose returns in batches
/// after each paragraph is checked against Detcore; until then a page has
/// the generated strategy, discussion link, and append-only status log.
const SECTIONS: [&str; 3] = ["Strategy", "Discussion", "Status"];
const DETCORE_CLASS: [&str; 3] = ["determinized", "passthrough", "unsupported"];

fn repo_root() -> PathBuf {
    let script = Path::new(file!());
    script
        .parent()
        .and_then(Path::parent)
        .unwrap_or_else(|| Path::new("."))
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from("."))
}

fn fail(msg: &str) -> ! {
    eprintln!("syscall-pages: {msg}");
    exit(2)
}

fn read(path: &Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_else(|e| fail(&format!("cannot read {}: {e}", path.display())))
}

fn is_syscall_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
}

// ---------------------------------------------------------------------------
// Page model. Front matter is a fixed YAML subset: scalar keys, one string
// list, and two nested blocks. The parser understands exactly that subset
// and rejects everything else, so a hand edit outside the subset is a lint
// error rather than a silently different reading.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
struct CodeFacts {
    as_of: String,
    detcore_class: String,
    dispatch: String,
    refused: bool,
    refused_only_when_fail_closed: bool,
    subscribed_run: bool,
    subscribed_record_replay: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct Page {
    syscall: String,
    nr_x86_64: i32,
    related: Vec<String>,
    discussion_kind: String,
    discussion_url: String,
    code_facts: CodeFacts,
    /// Section heading -> body text (without the heading line).
    sections: BTreeMap<String, String>,
}

struct Report {
    errors: Vec<String>,
    warnings: Vec<String>,
}

impl Report {
    fn new() -> Self {
        Report {
            errors: Vec::new(),
            warnings: Vec::new(),
        }
    }
    fn err(&mut self, rule: &str, msg: impl Into<String>) {
        self.errors.push(format!("{rule}: {}", msg.into()));
    }
}

fn split_front_matter(text: &str) -> Result<(&str, &str), String> {
    let rest = text
        .strip_prefix("---\n")
        .ok_or("page does not start with a `---` front-matter fence")?;
    let end = rest
        .find("\n---\n")
        .ok_or("front matter has no closing `---` fence")?;
    Ok((&rest[..end], &rest[end + 5..]))
}

fn split_sections(body: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut cur: Option<String> = None;
    let mut buf = String::new();
    let mut in_fence = false;
    for line in body.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        if !in_fence && line.starts_with("## ") && !line.starts_with("###") {
            if let Some(h) = cur.take() {
                out.insert(h, std::mem::take(&mut buf));
            }
            cur = Some(line[3..].trim().to_string());
            continue;
        }
        if cur.is_some() {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    if let Some(h) = cur {
        out.insert(h, buf);
    }
    out
}

fn headings_in_order(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_fence = false;
    for line in body.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        if !in_fence && line.starts_with("## ") {
            out.push(line[3..].trim().to_string());
        }
    }
    out
}

/// Parse the front-matter subset. `block` names the nested block a key was
/// found in ("" at top level). Unknown keys are collected for P05.
struct FrontMatter {
    scalars: BTreeMap<(String, String), String>,
    related: Vec<String>,
    unknown: Vec<String>,
}

fn parse_front_matter(fm: &str) -> Result<FrontMatter, String> {
    let top_keys = [
        "schema",
        "syscall",
        "nr_x86_64",
        "related",
        "discussion",
        "code_facts",
    ];
    let discussion_keys = ["kind", "url"];
    let facts_keys = [
        "as_of",
        "detcore_class",
        "dispatch",
        "refused",
        "refused_only_when_fail_closed",
        "subscribed_run",
        "subscribed_record_replay",
    ];
    let mut out = FrontMatter {
        scalars: BTreeMap::new(),
        related: Vec::new(),
        unknown: Vec::new(),
    };
    let mut block = String::new();
    for raw in fm.lines() {
        let line = raw.trim_end();
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        if let Some(item) = line.trim_start().strip_prefix("- ") {
            if block != "related" {
                return Err(format!("list item outside `related`: {line:?}"));
            }
            out.related.push(item.trim().to_string());
            continue;
        }
        let indented = line.starts_with("  ");
        let (key, value) = line
            .trim_start()
            .split_once(':')
            .ok_or_else(|| format!("front-matter line is not `key: value`: {line:?}"))?;
        let (key, value) = (key.trim(), value.trim());
        if !indented {
            block = if value.is_empty() {
                key.to_string()
            } else {
                String::new()
            };
            if key == "related" {
                if value == "[]" {
                    block = String::new();
                } else if !value.is_empty() {
                    return Err(format!("`related` must be a list or `[]`, got {value:?}"));
                }
                continue;
            }
            if key == "discussion" || key == "code_facts" {
                if !value.is_empty() {
                    return Err(format!("`{key}` must be a nested block"));
                }
                continue;
            }
            if !top_keys.contains(&key) {
                out.unknown.push(key.to_string());
                continue;
            }
            let value = value.split(" #").next().unwrap_or("").trim();
            if out
                .scalars
                .insert((String::new(), key.to_string()), value.to_string())
                .is_some()
            {
                return Err(format!("duplicate front-matter key `{key}`"));
            }
        } else {
            let known = match block.as_str() {
                "discussion" => discussion_keys.contains(&key),
                "code_facts" => facts_keys.contains(&key),
                _ => false,
            };
            if !known {
                out.unknown.push(format!("{block}.{key}"));
                continue;
            }
            let value = value
                .split(" #")
                .next()
                .unwrap_or("")
                .trim()
                .trim_matches('"');
            if out
                .scalars
                .insert((block.clone(), key.to_string()), value.to_string())
                .is_some()
            {
                return Err(format!("duplicate front-matter key `{block}.{key}`"));
            }
        }
    }
    Ok(out)
}

fn fm_get<'a>(fm: &'a FrontMatter, block: &str, key: &str) -> Result<&'a str, String> {
    fm.scalars
        .get(&(block.to_string(), key.to_string()))
        .map(|s| s.as_str())
        .ok_or_else(|| format!("front matter is missing `{key}`"))
}

fn parse_bool(s: &str) -> Result<bool, String> {
    match s {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(format!("expected true/false, got {other:?}")),
    }
}

fn parse_page(text: &str, rep: &mut Report) -> Option<Page> {
    let (fm_text, body) = match split_front_matter(text) {
        Ok(v) => v,
        Err(e) => {
            rep.err("P01", e);
            return None;
        }
    };
    let fm = match parse_front_matter(fm_text) {
        Ok(v) => v,
        Err(e) => {
            rep.err("P01", e);
            return None;
        }
    };
    for u in &fm.unknown {
        rep.err("P05", format!("unknown front-matter key `{u}`"));
    }
    let get = |block: &str, key: &str, rep: &mut Report| -> String {
        match fm_get(&fm, block, key) {
            Ok(v) => v.to_string(),
            Err(e) => {
                rep.err("P05", e);
                String::new()
            }
        }
    };
    if get("", "schema", rep) != SCHEMA {
        rep.err("P05", format!("`schema` must be {SCHEMA}"));
    }
    let b = |block: &str, key: &str, rep: &mut Report| -> bool {
        match parse_bool(&get(block, key, rep)) {
            Ok(v) => v,
            Err(e) => {
                rep.err("P05", e);
                false
            }
        }
    };
    let page = Page {
        syscall: get("", "syscall", rep),
        nr_x86_64: get("", "nr_x86_64", rep).parse().unwrap_or(-1),
        related: fm.related.clone(),
        discussion_kind: get("discussion", "kind", rep),
        discussion_url: get("discussion", "url", rep),
        code_facts: CodeFacts {
            as_of: get("code_facts", "as_of", rep),
            detcore_class: get("code_facts", "detcore_class", rep),
            dispatch: get("code_facts", "dispatch", rep),
            refused: b("code_facts", "refused", rep),
            refused_only_when_fail_closed: b("code_facts", "refused_only_when_fail_closed", rep),
            subscribed_run: b("code_facts", "subscribed_run", rep),
            subscribed_record_replay: b("code_facts", "subscribed_record_replay", rep),
        },
        sections: split_sections(body),
    };
    Some(page)
}

fn bool_str(v: bool) -> &'static str {
    if v { "true" } else { "false" }
}

fn emit_page(page: &Page) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "---");
    let _ = writeln!(s, "schema: {SCHEMA}");
    let _ = writeln!(s, "syscall: {}", page.syscall);
    let _ = writeln!(s, "nr_x86_64: {}", page.nr_x86_64);
    if page.related.is_empty() {
        let _ = writeln!(s, "related: []");
    } else {
        let _ = writeln!(s, "related:");
        for u in &page.related {
            let _ = writeln!(s, "  - {u}");
        }
    }
    let _ = writeln!(s, "discussion:");
    let _ = writeln!(s, "  kind: {}", page.discussion_kind);
    let _ = writeln!(s, "  url: {}", page.discussion_url);
    let _ = writeln!(
        s,
        "# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit"
    );
    let _ = writeln!(s, "code_facts:");
    let f = &page.code_facts;
    let _ = writeln!(s, "  as_of: {}", f.as_of);
    let _ = writeln!(s, "  detcore_class: {}", f.detcore_class);
    let _ = writeln!(s, "  dispatch: {:?}", f.dispatch);
    let _ = writeln!(s, "  refused: {}", bool_str(f.refused));
    let _ = writeln!(
        s,
        "  refused_only_when_fail_closed: {}",
        bool_str(f.refused_only_when_fail_closed)
    );
    let _ = writeln!(s, "  subscribed_run: {}", bool_str(f.subscribed_run));
    let _ = writeln!(
        s,
        "  subscribed_record_replay: {}",
        bool_str(f.subscribed_record_replay)
    );
    let _ = writeln!(s, "---");
    for h in SECTIONS {
        let _ = writeln!(s, "\n## {h}\n");
        if let Some(body) = page.sections.get(h) {
            s.push_str(body.trim());
            s.push('\n');
        }
    }
    s
}

/// The strategy table column, derived from code facts only. `refused` is
/// split out of `determinized` because the two mean different things to a
/// guest: modelled or emulated behaviour, versus a fixed errno.
fn strategy(f: &CodeFacts) -> &'static str {
    match (f.detcore_class.as_str(), f.refused) {
        ("determinized", true) => "determinized-refused",
        ("determinized", false) => "determinized",
        ("passthrough", _) => "passthrough",
        _ => "unsupported",
    }
}

/// The single prose sentence a facts-only page is allowed to assert about
/// strategy. It is a pure function of `code_facts`, so lint can hold every
/// page to it exactly.
fn strategy_body(page: &Page, classification_line: usize, dispatch_line: Option<usize>) -> String {
    let mut s = format!("\n{}\n", strategy_sentence(page));
    let _ = writeln!(
        s,
        "\n- Reviewed list: `detcore/src/syscall_classification.rs:{classification_line}`"
    );
    if let Some(line) = dispatch_line {
        let _ = writeln!(s, "- Dispatch: `detcore/src/lib.rs:{line}`");
    }
    s.push_str(
        "\nThis facts-only page was generated from Detcore code facts. Checked analysis prose has not been added yet.\n",
    );
    s
}

fn strategy_sentence(page: &Page) -> String {
    let f = &page.code_facts;
    format!(
        "Detcore classifies `{}` ({}) as {} via {} dispatch; deterministic refusal: {}; run subscription: {}; record/replay subscription: {}.",
        page.syscall,
        page.nr_x86_64,
        strategy(f),
        f.dispatch,
        bool_str(f.refused),
        bool_str(f.subscribed_run),
        bool_str(f.subscribed_record_replay)
    )
}

// ---------------------------------------------------------------------------
// Public-text findings (P30). Pages and generated outputs live in a
// public repository: no host names, home or scratch paths, internal diff
// or task ids, private-workspace references, or internal URLs. Lint
// refuses every recognised form, so a new form is a visible failure to
// extend here, not a quiet leak.
// ---------------------------------------------------------------------------

fn is_devbig_host(s: &str) -> bool {
    let s = s.strip_suffix("'s").unwrap_or(s);
    if let Some(rest) = s.strip_prefix("devbig") {
        return rest.chars().take_while(|c| c.is_ascii_digit()).count() >= 3
            && rest[rest.chars().take_while(|c| c.is_ascii_digit()).count()..]
                .chars()
                .all(|c| c == '.' || c.is_alphanumeric());
    }
    false
}

fn scrub_findings(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    // Hostname substrings anywhere, including inside quoted payloads.
    let mut at = 0;
    while let Some(i) = text[at..].find("devbig") {
        let start = at + i;
        let run: String = text[start..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-')
            .collect();
        out.push(run.clone());
        at = start + run.len();
    }
    let lower = text.to_lowercase();
    for needle in [
        "from the checkout at",
        "from our checkout",
        "from my checkout",
        "in our slot",
        "in my slot",
        "in our worktree",
        "in my worktree",
        "in our lane",
        "in my lane",
        "author:",
    ] {
        if lower.contains(needle) {
            out.push(needle.to_string());
        }
    }
    for tok in text.split(|c: char| c.is_whitespace() || c == '`' || c == '(' || c == ')') {
        let core = tok.trim_matches(|c: char| {
            !c.is_alphanumeric() && c != '.' && c != '-' && c != '/' && c != ':'
        });
        let lc = core.to_lowercase();
        let dc_host = [
            "prn", "frc", "ftw", "atn", "cln", "odn", "lla", "ash", "vll", "nao", "rva", "pnb",
            "cco", "snc", "eag",
        ]
        .iter()
        .any(|dc| {
            lc.split('.').any(|part| {
                part.strip_prefix(dc)
                    .map(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
                    .unwrap_or(false)
            })
        });
        if is_devbig_host(core)
            || core.starts_with("/home/")
            || core.starts_with("/data/users/")
            || core.starts_with("/var/tmp")
            || lc.contains(".internalfb.com")
            || lc.contains(".facebook.com")
            || lc.contains(".fb.com")
            || lc.contains("fburl")
            || lc.contains("tfbnw")
            || lc.contains("thefacebook")
            || lc.contains("/intern/")
            || lc.contains("dev-hermit/")
            || lc.contains("work/dev-hermit")
            || lc.contains("worktrees/slots/")
            || lc.contains("ai_docs/")
            || lc.contains("ignored/")
            || lc.contains("github.com/rrnewton/dev-hermit")
            || lc.contains("githubusercontent.com/rrnewton/dev-hermit")
            || (lc.contains("temp/") && lc.contains("-status.md"))
            || dc_host
        {
            out.push(core.to_string());
        }
    }
    // Internal diff, task, tracker, agent/lane and model identifiers, as
    // standalone tokens. The private-workspace script
    // (`scripts/check-private-workspace-refs.sh`) defines the path/link
    // classes above; these are the people-and-tracking classes it does not.
    // Built by concatenation: this file is processed by tools that would
    // otherwise rewrite the lowercase model name in the source text.
    let claude_name = concat!("cla", "ude");
    for tok in text.split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_')) {
        let lc = tok.to_lowercase();
        let tracker = (tok.len() >= 7
            && ['D', 'P', 'S', 'T'].contains(&tok.chars().next().unwrap_or(' '))
            && tok[1..].chars().all(|c| c.is_ascii_digit()))
            || (lc.starts_with("dh-") && tok.len() >= 6)
            || lc.starts_with("tbh-")
            || ["bead", "minibeads", "agentctl", "herdr"].contains(&lc.as_str());
        let agent_or_lane = lc.starts_with("syscall-tracking")
            || lc.starts_with("orc-coord")
            || lc == "orc"
            || lc.starts_with("muse-")
            || lc == "muse"
            || lc.starts_with(claude_name)
            || lc.starts_with("codex")
            || lc.starts_with("coord")
                && lc[5..]
                    .chars()
                    .next()
                    .map(|c| c.is_ascii_digit())
                    .unwrap_or(false)
            || [
                "hermit2",
                "hermit-bot",
                "hermit-bot3",
                "hermit-bots",
                "facts-bot",
                "p3-samples",
            ]
            .contains(&lc.as_str());
        let model_name = lc.starts_with("gpt-")
            || lc.starts_with("tbh-")
            || ["opus", "sonnet", "haiku"].contains(&lc.as_str())
            || (lc.contains(claude_name) && lc.chars().any(|c| c.is_ascii_digit()));
        if tracker || agent_or_lane || model_name {
            out.push(tok.to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

// ---------------------------------------------------------------------------
// Lint. Rule ids are P-prefixed (page form); the SCHEMA.md rule index is the
// normative text for each id.
// ---------------------------------------------------------------------------

fn lint_page(name_hint: &str, text: &str, root: &Path) -> (Report, Option<Page>) {
    let mut rep = Report::new();
    for hit in scrub_findings(text) {
        rep.err(
            "P30",
            format!("internal or host-specific text must not reach a public page: {hit:?}"),
        );
    }
    // P31 is checked per-section below: the dropped `ptrace_verification`
    // key may still appear inside historical Status entries, nowhere else.
    let page = parse_page(text, &mut rep);
    let Some(page) = page else { return (rep, None) };
    // P02: sections exactly once, in order, nothing before the first.
    if let Ok((_, body)) = split_front_matter(text) {
        let got = headings_in_order(body);
        let want: Vec<String> = SECTIONS.iter().map(|s| s.to_string()).collect();
        if got != want {
            rep.err(
                "P02",
                format!("level-2 headings are {got:?}, want {want:?}"),
            );
        }
        if let Some(first) = body.find("\n## ") {
            if !body[..first].trim().is_empty() && !body.trim_start().starts_with("## ") {
                rep.err("P02", "text before the first level-2 heading");
            }
        }
    }
    // P03: facts sections non-empty.
    for h in ["Strategy"] {
        if page
            .sections
            .get(h)
            .map(|b| b.trim().is_empty())
            .unwrap_or(true)
        {
            rep.err("P03", format!("section `## {h}` is empty or missing"));
        }
    }
    // P06-P09: judgement and code-fact values.
    if !is_syscall_name(&page.syscall) || page.syscall != name_hint {
        rep.err(
            "P06",
            format!(
                "`syscall: {}` does not match file name {name_hint}",
                page.syscall
            ),
        );
    }
    if page.nr_x86_64 < 0 {
        rep.err("P07", format!("nr_x86_64 {} is negative", page.nr_x86_64));
    }
    if !DETCORE_CLASS.contains(&page.code_facts.detcore_class.as_str()) {
        rep.err(
            "P09",
            format!(
                "value {:?} is not one of {DETCORE_CLASS:?}",
                page.code_facts.detcore_class
            ),
        );
    }
    let f = &page.code_facts;
    if f.as_of.len() != 40
        || !f
            .as_of
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    {
        rep.err("P09", "`code_facts.as_of` must be a 40-hex source revision");
    }
    let dispatch_ok = [
        "typed",
        "sysno-guard",
        "passthrough",
        "unsupported",
        "fallback",
    ]
    .contains(&f.dispatch.as_str())
        || f.dispatch
            .strip_prefix("pred:is_")
            .map(is_syscall_name)
            .unwrap_or(false);
    if !dispatch_ok {
        rep.err("P09", format!("dispatch {:?} is not typed | sysno-guard | pred:is_<fn> | passthrough | unsupported | fallback", f.dispatch));
    }
    for u in &page.related {
        if !u.starts_with("https://github.com/") {
            rep.err(
                "P08",
                format!("`related` holds full GitHub URLs only: {u:?}"),
            );
        }
    }
    for (h, b) in &page.sections {
        if h != "Status" && b.contains("ptrace_verification") {
            rep.err(
                "P31",
                format!("`ptrace_verification` was dropped from page form (in `## {h}`)"),
            );
        }
    }
    let _ = root;
    let want_strategy = strategy_sentence(&page);
    let got_strategy = page.sections.get("Strategy").cloned().unwrap_or_default();
    if !got_strategy.contains(&want_strategy) {
        rep.err(
            "P35",
            format!("`## Strategy` must contain the code-derived line: {want_strategy:?}"),
        );
    }
    lint_status(&page, &mut rep);
    lint_discussion(&page, &mut rep);
    (rep, Some(page))
}

fn status_entries(page: &Page) -> Vec<String> {
    page.sections
        .get("Status")
        .map(|b| {
            b.lines()
                .filter(|l| l.trim_start().starts_with("- "))
                .map(|l| l.trim().to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn lint_status(page: &Page, rep: &mut Report) {
    let entries = status_entries(page);
    if entries.is_empty() {
        rep.err("P12", "`## Status` needs at least one dated entry");
    }
    let mut last = "";
    for e in &entries {
        let parts: Vec<&str> = e.trim_start_matches("- ").splitn(3, " · ").collect();
        if parts.len() < 3 {
            rep.err(
                "P12",
                format!("Status entry is not `- date · rev <sha> · text`: {e:?}"),
            );
            continue;
        }
        let date = parts[0];
        let ok_date = date.len() == 10
            && date.as_bytes().get(4) == Some(&b'-')
            && date.as_bytes().get(7) == Some(&b'-')
            && date.chars().filter(|c| c.is_ascii_digit()).count() == 8;
        if !ok_date {
            rep.err("P12", format!("Status entry date is not YYYY-MM-DD: {e:?}"));
        }
        if date < last {
            rep.err("P13", format!("Status dates decrease at {e:?}"));
        }
        last = date;
        let sha_ok = parts[1]
            .strip_prefix("rev ")
            .map(|s| {
                (12..=40).contains(&s.len())
                    && s.chars()
                        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
            })
            .unwrap_or(false);
        if !sha_ok {
            rep.err(
                "P12",
                format!("Status entry does not name `rev <12-40 hex>`: {e:?}"),
            );
        }
    }
}

fn lint_discussion(page: &Page, rep: &mut Report) {
    match page.discussion_kind.as_str() {
        "existing" => {
            if !page
                .discussion_url
                .starts_with("https://github.com/facebookexperimental/hermit/issues/")
            {
                rep.err(
                    "P34",
                    "an `existing` discussion must link a facebookexperimental/hermit issue",
                );
            }
        }
        "new" => {
            if !page
                .discussion_url
                .starts_with("https://github.com/facebookexperimental/hermit/issues/new?")
            {
                rep.err("P34", "a `new` discussion must be a prefilled facebookexperimental/hermit new-issue URL");
            }
        }
        other => rep.err(
            "P34",
            format!("discussion kind must be existing or new, got {other:?}"),
        ),
    }
    let sec = page.sections.get("Discussion").cloned().unwrap_or_default();
    if !sec.contains(&page.discussion_url) {
        rep.err(
            "P34",
            "`## Discussion` must link the front-matter discussion URL",
        );
    }
}

// ---------------------------------------------------------------------------
// Dispatch parse (P27). Adapted from the dev-hermit C4 parse and extended
// for today's dispatcher: wildcard pre-arms, #[cfg] attributes on arms and
// inside predicate bodies, and match-form predicates. Anything else shape
// is `parser-failed`, never a pass.
// ---------------------------------------------------------------------------

fn mask_rust(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = b.to_vec();
    let mut i = 0;
    let blank = |out: &mut Vec<u8>, from: usize, to: usize| {
        for x in out.iter_mut().take(to).skip(from) {
            if *x != b'\n' {
                *x = b' ';
            }
        }
    };
    while i < b.len() {
        if b[i] == b'/' && b.get(i + 1) == Some(&b'/') {
            let end = src[i..].find('\n').map(|k| i + k).unwrap_or(b.len());
            blank(&mut out, i, end);
            i = end;
        } else if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
            let (mut depth, mut j) = (1, i + 2);
            while j < b.len() && depth > 0 {
                if b[j] == b'/' && b.get(j + 1) == Some(&b'*') {
                    depth += 1;
                    j += 2;
                } else if b[j] == b'*' && b.get(j + 1) == Some(&b'/') {
                    depth -= 1;
                    j += 2;
                } else {
                    j += 1;
                }
            }
            blank(&mut out, i, j);
            i = j;
        } else if b[i] == b'"' {
            let mut j = i + 1;
            while j < b.len() && b[j] != b'"' {
                j += if b[j] == b'\\' { 2 } else { 1 };
            }
            blank(&mut out, i + 1, j.min(b.len()));
            i = j + 1;
        } else {
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_default()
}

fn matching(m: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (k, &c) in m.iter().enumerate().skip(open) {
        match c {
            b'{' | b'(' | b'[' => depth += 1,
            b'}' | b')' | b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(k);
                }
            }
            _ => {}
        }
    }
    None
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[derive(Debug, Clone)]
struct Arm {
    pattern: String,
    body: String,
    start: usize,
    /// Attribute texts (without `#[` / `]`) that preceded the pattern.
    attrs: Vec<String>,
}

/// Split match arms on masked source `m`; attribute text is read from the
/// original source at the same offsets (masking blanks string contents, and
/// the arch in `#[cfg(target_arch = "...")]` is a string).
fn split_arms(m: &str, orig: &str, from: usize, to: usize) -> Result<Vec<Arm>, String> {
    let b = m.as_bytes();
    let mut arms = Vec::new();
    let mut i = from;
    loop {
        while i < to && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= to {
            break;
        }
        // Capture leading attributes (#[cfg(...)]) before an arm pattern;
        // arch-gated arms are handled by the caller.
        let mut start = i;
        let mut attrs = Vec::new();
        while start + 1 < to && b[start] == b'#' && b[start + 1] == b'[' {
            let close = matching(b, start + 1).ok_or("unbalanced arm attribute")?;
            attrs.push(one_line(&orig[start + 2..close]));
            start = close + 1;
            while start < to && b[start].is_ascii_whitespace() {
                start += 1;
            }
        }
        let mut depth = 0i32;
        let mut arrow = None;
        let mut k = start;
        while k + 1 < to {
            match b[k] {
                b'{' | b'(' | b'[' => depth += 1,
                b'}' | b')' | b']' => depth -= 1,
                b'=' if depth == 0 && b[k + 1] == b'>' => {
                    arrow = Some(k);
                    break;
                }
                _ => {}
            }
            k += 1;
        }
        let Some(a) = arrow else {
            return Err(format!("no `=>` after byte {start}"));
        };
        let pattern = one_line(&m[start..a]);
        let mut j = a + 2;
        while j < to && b[j].is_ascii_whitespace() {
            j += 1;
        }
        let body_start = j;
        let body_end;
        if j < to && b[j] == b'{' {
            let close = matching(b, j).ok_or("unbalanced arm body")?;
            body_end = close + 1;
            j = close + 1;
            while j < to && b[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < to && b[j] == b',' {
                j += 1;
            }
        } else {
            let mut depth = 0i32;
            while j < to {
                match b[j] {
                    b'{' | b'(' | b'[' => depth += 1,
                    b'}' | b')' | b']' => depth -= 1,
                    b',' if depth == 0 => break,
                    _ => {}
                }
                j += 1;
            }
            body_end = j;
            if j < to {
                j += 1;
            }
        }
        arms.push(Arm {
            pattern,
            body: m[body_start..body_end].to_string(),
            start,
            attrs,
        });
        i = j;
    }
    Ok(arms)
}

fn split_top(s: &str, sep: &str) -> Vec<String> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let (mut depth, mut last, mut i) = (0i32, 0usize, 0usize);
    while i < b.len() {
        match b[i] {
            b'{' | b'(' | b'[' => depth += 1,
            b'}' | b')' | b']' => depth -= 1,
            _ => {}
        }
        if depth == 0 && s[i..].starts_with(sep) {
            let pipe_pair = sep == "|" && (s[i..].starts_with("||") || (i > 0 && b[i - 1] == b'|'));
            if !pipe_pair {
                out.push(s[last..i].trim().to_string());
                i += sep.len();
                last = i;
                continue;
            }
        }
        i += 1;
    }
    out.push(s[last..].trim().to_string());
    out
}

type PredDef = Result<(BTreeSet<String>, BTreeSet<String>), String>;

fn ident_after(m: &str, at: usize) -> Option<(String, usize)> {
    let rest = &m[at..];
    let len = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .map(|c| c.len_utf8())
        .sum::<usize>();
    if len == 0 {
        None
    } else {
        Some((rest[..len].to_string(), at + len))
    }
}

/// Strip #[...] attributes from masked source (cfg gates on arms). Members
/// named only under a non-x86_64 cfg are not in the pinned set and drop out
/// when membership is intersected with it; a member gated to x86_64 stays.
fn strip_attrs(m: &str) -> String {
    let b = m.as_bytes();
    let mut out = String::with_capacity(m.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'#' && b.get(i + 1) == Some(&b'[') {
            match matching(b, i + 1) {
                Some(close) => {
                    out.push(' ');
                    i = close + 1;
                }
                None => break,
            }
        } else {
            let ch = m[i..].chars().next().unwrap_or(' ');
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn parse_predicates(src: &str, file: &str) -> Vec<(String, Option<PredDef>)> {
    let m = strip_attrs(&mask_rust(src));
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(off) = m[at..].find("fn is_") {
        let start = at + off + 3;
        let Some((name, after)) = ident_after(&m, start) else {
            break;
        };
        at = after;
        let Some(paren) = m[after..].find('(') else {
            continue;
        };
        let sig_ok = m[after..after + paren].trim().is_empty()
            && m[after + paren..]
                .trim_start()
                .starts_with("(sysno: Sysno) -> bool {");
        if !sig_ok {
            continue;
        }
        let open = after + paren;
        let Some(sig_end) = m[open..].find('{') else {
            continue;
        };
        let open = open + sig_end;
        let Some(close) = matching(m.as_bytes(), open) else {
            out.push((
                name.clone(),
                Some(Err(format!("{file}: unbalanced body for {name}"))),
            ));
            continue;
        };
        let body = &m[open + 1..close];
        if body.trim() == format!("crate::syscall_classification::{name}(sysno)")
            || body.trim() == format!("syscall_classification::{name}(sysno)")
        {
            out.push((name, None));
            continue;
        }
        let mut members = BTreeSet::new();
        let mut subs = BTreeSet::new();
        let mut rest = body.to_string();
        while let Some(i) = rest.find("Sysno::") {
            match ident_after(&rest, i + 7) {
                Some((id, end)) => {
                    members.insert(id);
                    rest.replace_range(i..end, " ");
                }
                None => break,
            }
        }
        while let Some(i) = rest.find("is_") {
            match ident_after(&rest, i) {
                Some((id, end)) => {
                    if rest[end..].trim_start().starts_with("(sysno)") {
                        subs.insert(id);
                    }
                    rest.replace_range(i..end, " ");
                }
                None => break,
            }
        }
        for word in ["matches!", "match", "sysno", "true", "false", "_"] {
            rest = rest.replace(word, " ");
        }
        let leftover: String = rest
            .chars()
            .filter(|ch| !ch.is_whitespace() && !"()|,{}=>".contains(*ch))
            .collect();
        if !leftover.is_empty() || body.contains("&&") {
            out.push((name.clone(), Some(Err(format!(
                "{file}: predicate {name} has a body the parser does not understand: {leftover:?}"
            )))));
            continue;
        }
        out.push((name, Some(Ok((members, subs)))));
    }
    out
}

fn merge_predicates(files: &[(&str, &str)]) -> BTreeMap<String, PredDef> {
    let mut all: BTreeMap<String, Vec<PredDef>> = BTreeMap::new();
    let mut delegates: BTreeSet<String> = BTreeSet::new();
    for (file, src) in files {
        for (name, def) in parse_predicates(src, file) {
            match def {
                Some(d) => all.entry(name).or_default().push(d),
                None => {
                    delegates.insert(name);
                }
            }
        }
    }
    let mut out: BTreeMap<String, PredDef> = BTreeMap::new();
    for name in delegates.iter().filter(|n| !all.contains_key(*n)) {
        out.insert(
            name.clone(),
            Err(format!(
                "predicate {name} only delegates; definition not found"
            )),
        );
    }
    for (name, mut defs) in all {
        let d = if defs.len() == 1 {
            defs.remove(0)
        } else {
            Err(format!("predicate {name} is defined {} times", defs.len()))
        };
        out.insert(name, d);
    }
    out
}

fn expand_pred(
    name: &str,
    preds: &BTreeMap<String, PredDef>,
    seen: &mut Vec<String>,
    errors: &mut Vec<String>,
) -> BTreeSet<String> {
    let Some(def) = preds.get(name) else {
        errors.push(format!(
            "predicate {name} is used but its definition was not found"
        ));
        return BTreeSet::new();
    };
    let (members, subs) = match def {
        Ok(d) => d,
        Err(why) => {
            errors.push(format!(
                "predicate {name} is used by the dispatch but was not understood: {why}"
            ));
            return BTreeSet::new();
        }
    };
    if seen.iter().any(|s| s == name) {
        errors.push(format!("predicate cycle through {name}"));
        return BTreeSet::new();
    }
    seen.push(name.to_string());
    let mut out = members.clone();
    for s in subs {
        out.extend(expand_pred(s, preds, seen, errors));
    }
    seen.pop();
    out
}

/// Pages describe x86_64. An arm gated to aarch64 does not run there and
/// assigns nothing; an arm gated to x86_64 (or not-aarch64) does. Any other
/// cfg shape is not understood.
fn arm_arch(attrs: &[String]) -> Result<bool, String> {
    let mut applies = true;
    for a in attrs {
        let a = a.replace(' ', "");
        match a.as_str() {
            "cfg(target_arch=\"aarch64\")" => applies = false,
            "cfg(target_arch=\"x86_64\")" | "cfg(not(target_arch=\"aarch64\"))" => {}
            other => return Err(format!("arm attribute #[{other}] is not understood")),
        }
    }
    Ok(applies)
}

fn camel(name: &str) -> String {
    name.split('_')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_ascii_uppercase().to_string() + c.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

struct DispatchReport {
    status: String,
    errors: Vec<String>,
    dispatch: BTreeMap<String, String>,
    /// 1-based line in `detcore/src/lib.rs` of the arm that assigned each
    /// dispatch label, where an explicit arm exists.
    lines: BTreeMap<String, usize>,
    missing_arm: Vec<String>,
}

fn line_at(src: &str, byte: usize) -> usize {
    src[..byte.min(src.len())].matches('\n').count() + 1
}

/// Assign each pinned syscall the dispatch arm that handles it. `classes`
/// maps name -> detcore_class. Determinized syscalls with no explicit arm
/// get `fallback`; any shape the parser does not understand makes the whole
/// report `parser-failed`.
fn parse_dispatch(
    lib: &str,
    preds_src: &[(&str, &str)],
    classes: &BTreeMap<String, String>,
) -> DispatchReport {
    let mut errors: Vec<String> = Vec::new();
    let fail = |errors: Vec<String>, classes: &BTreeMap<String, String>| DispatchReport {
        status: "parser-failed".into(),
        errors,
        dispatch: classes
            .keys()
            .map(|k| (k.clone(), "parser-failed".into()))
            .collect(),
        lines: BTreeMap::new(),
        missing_arm: Vec::new(),
    };
    let m = mask_rust(lib);
    let needle = "match classify_syscall(call.number()) {";
    let hits: Vec<usize> = m.match_indices(needle).map(|(i, _)| i).collect();
    if hits.len() != 1 {
        errors.push(format!(
            "expected exactly one `{needle}` in detcore/src/lib.rs, found {}",
            hits.len()
        ));
        return fail(errors, classes);
    }
    let open = hits[0] + needle.len() - 1;
    let Some(close) = matching(m.as_bytes(), open) else {
        errors.push("unbalanced classify_syscall match".into());
        return fail(errors, classes);
    };
    let arms = match split_arms(&m, lib, open + 1, close) {
        Ok(a) => a,
        Err(e) => {
            errors.push(format!("cannot split classify_syscall arms: {e}"));
            return fail(errors, classes);
        }
    };
    let preds = merge_predicates(preds_src);
    let mut dispatch: BTreeMap<String, String> = BTreeMap::new();
    let mut dispatch_lines: BTreeMap<String, usize> = BTreeMap::new();
    let mut typed_arm: Option<Arm> = None;
    for a in &arms {
        match arm_arch(&a.attrs) {
            Ok(false) => continue,
            Ok(true) => {}
            Err(e) => {
                errors.push(e);
                continue;
            }
        }
        let pat = a.pattern.as_str();
        // Wildcard pre-arms intercept conditional subsets and fall through
        // to the classifying arms when their guard is false; they assign no
        // syscall. An unguarded wildcard would swallow the match: refuse.
        if pat == "_" || pat.starts_with("_ if ") {
            if pat == "_" {
                errors.push(format!("unguarded wildcard outer arm at byte {}", a.start));
            }
            continue;
        }
        let Some(cls) = ["Determinized", "PassThrough", "Unsupported"]
            .iter()
            .find(|c| pat.starts_with(&format!("SyscallClassification::{c}")))
        else {
            errors.push(format!(
                "unrecognised outer arm pattern {pat:?} at byte {}",
                a.start
            ));
            continue;
        };
        let rest = pat["SyscallClassification::".len() + cls.len()..].trim();
        if rest.is_empty() {
            if *cls == "Determinized" {
                typed_arm = Some(a.clone());
            }
            continue;
        }
        let Some(guard) = rest.strip_prefix("if ") else {
            errors.push(format!("outer arm pattern {pat:?} has trailing {rest:?}"));
            continue;
        };
        // A guard with an extra `&&` condition (today only the PassThrough
        // brk launcher arm) assigns nothing: the syscall still needs its
        // unconditional arm below. On a Determinized arm that shape is not
        // understood and fails the parse rather than reading as handled.
        if guard.contains("&&") {
            if *cls == "Determinized" {
                errors.push(format!(
                    "Determinized outer guard with && is not understood: {guard:?}"
                ));
            }
            continue;
        }
        if guard.starts_with("matches!(") {
            let mut rest = guard.to_string();
            while let Some(i) = rest.find("Sysno::") {
                match ident_after(&rest, i + 7) {
                    Some((name, end)) => {
                        if classes.contains_key(&name) {
                            // Rust takes the first matching arm; a later
                            // arm naming the same syscall must not relabel it.
                            dispatch_lines
                                .entry(name.clone())
                                .or_insert_with(|| line_at(&m, a.start));
                            dispatch.entry(name).or_insert("sysno-guard".into());
                        }
                        rest.replace_range(i..end, " ");
                    }
                    None => break,
                }
            }
            continue;
        }
        for term in split_top(guard, "||") {
            if let Some(name) = term.strip_prefix("call.number() == Sysno::") {
                if is_syscall_name(name) {
                    dispatch_lines
                        .entry(name.to_string())
                        .or_insert_with(|| line_at(&m, a.start));
                    dispatch
                        .entry(name.to_string())
                        .or_insert("sysno-guard".into());
                } else {
                    errors.push(format!("bad sysno guard term {term:?}"));
                }
            } else if term.starts_with("is_") && term.ends_with("(call.number())") {
                let pred = &term[..term.len() - "(call.number())".len()];
                let pred_ok = pred.len() > 3
                    && pred[3..]
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
                if pred_ok {
                    for member in expand_pred(pred, &preds, &mut Vec::new(), &mut errors) {
                        if classes.contains_key(&member) {
                            dispatch_lines
                                .entry(member.clone())
                                .or_insert_with(|| line_at(&m, a.start));
                            dispatch.entry(member).or_insert(format!("pred:{pred}"));
                        }
                    }
                } else {
                    errors.push(format!("bad predicate guard term {term:?}"));
                }
            } else {
                errors.push(format!(
                    "guard term {term:?} is neither a Sysno equality nor a predicate call"
                ));
            }
        }
    }
    // Typed inner match.
    let variants: BTreeMap<String, String> =
        classes.keys().map(|n| (camel(n), n.clone())).collect();
    let mut has_fallback = false;
    match &typed_arm {
        None => errors
            .push("no unguarded `SyscallClassification::Determinized => match call` arm".into()),
        Some(o) => {
            let body = o.body.trim_start();
            if !body.starts_with("match call {") {
                errors.push("the unguarded Determinized arm is not `match call { ... }`".into());
            } else {
                // The `{` of `match call {` in masked source, at/after the arm.
                let rel = m[o.start..].find("match call {").unwrap_or(0);
                let iopen = o.start + rel + "match call ".len();
                if let Some(ic) = matching(m.as_bytes(), iopen) {
                    match split_arms(&m, lib, iopen + 1, ic) {
                        Ok(inner) => {
                            for a in &inner {
                                if let Err(e) = arm_arch(&a.attrs) {
                                    errors.push(e);
                                }
                            }
                            let inner: Vec<&Arm> = inner
                                .iter()
                                .filter(|a| matches!(arm_arch(&a.attrs), Ok(true)))
                                .collect();
                            let n = inner.len();
                            for (k, a) in inner.iter().enumerate() {
                                let parts = split_top(&a.pattern, " if ");
                                let alts = split_top(&parts[0], "|");
                                if alts.len() == 1
                                    && (alts[0] == "_"
                                        || (is_syscall_name(&alts[0])
                                            && !alts[0].starts_with("Syscall")))
                                {
                                    if k + 1 != n || parts.len() > 1 {
                                        errors.push(format!(
                                            "catch-all arm {:?} is not the last unguarded arm",
                                            a.pattern
                                        ));
                                    }
                                    has_fallback = true;
                                    continue;
                                }
                                for alt in alts {
                                    let Some(vrest) = alt.trim().strip_prefix("Syscall::") else {
                                        errors.push(format!("typed arm alternative {alt:?} does not start with Syscall::"));
                                        continue;
                                    };
                                    let Some((variant, _)) = ident_after(vrest, 0) else {
                                        continue;
                                    };
                                    match variants.get(&variant) {
                                        Some(name) => {
                                            dispatch_lines
                                                .entry(name.clone())
                                                .or_insert_with(|| line_at(&m, a.start));
                                            dispatch.entry(name.clone()).or_insert("typed".into());
                                        }
                                        None => {
                                            if variant != "Other" {
                                                errors.push(format!("typed variant Syscall::{variant} maps to no pinned syscall"));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => errors.push(format!("cannot split typed arms: {e}")),
                    }
                } else {
                    errors.push("unbalanced typed `match call`".into());
                }
            }
        }
    }
    if !has_fallback {
        errors.push("typed `match call` has no catch-all fallback arm".into());
    }
    let mut missing = Vec::new();
    for (name, cls) in classes {
        let d = match cls.as_str() {
            "passthrough" => dispatch.get(name).cloned().unwrap_or("passthrough".into()),
            "unsupported" => "unsupported".into(),
            _ => match dispatch.get(name) {
                Some(d) => d.clone(),
                None => {
                    missing.push(name.clone());
                    "fallback".into()
                }
            },
        };
        dispatch.insert(name.clone(), d);
    }
    if !errors.is_empty() {
        return fail(errors, classes);
    }
    DispatchReport {
        status: "ok".into(),
        errors,
        dispatch,
        lines: dispatch_lines,
        missing_arm: missing,
    }
}

/// The x86_64 syscall census Detcore itself asserts in
/// `detcore/src/syscall_classification.rs`: the reviewed name lists plus
/// the compile-time count and last-id assertions. This is the census the
/// in-tree gate uses; it is derived from the tree being checked, so a
/// pinned-table bump changes it without any number changing here.
struct Census {
    expected_count: usize,
    last_id: i32,
    /// name -> (detcore_class, 1-based line of its reviewed-list entry)
    classes: BTreeMap<String, (String, usize)>,
}

fn parse_census(src: &str) -> Result<Census, String> {
    let lines: Vec<&str> = src.lines().collect();
    let mut expected_count = None;
    let mut last_id = None;
    for (i, line) in lines.iter().enumerate() {
        if line.contains("const EXPECTED_SYSNO_COUNT") {
            let prev = lines[..i]
                .iter()
                .rev()
                .find(|l| !l.trim().is_empty())
                .copied()
                .unwrap_or("");
            if prev.contains("x86_64") {
                expected_count = line
                    .split('=')
                    .nth(1)
                    .map(|v| v.trim().trim_end_matches(';').trim().parse::<usize>())
                    .transpose()
                    .map_err(|_| "census: unparsable EXPECTED_SYSNO_COUNT".to_string())?;
            }
        }
        if let Some(rest) = line.split("Sysno::last().id() == ").nth(1) {
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !digits.is_empty() {
                last_id = digits.parse::<i32>().ok();
            }
        }
    }
    let expected_count = expected_count.ok_or("census: x86_64 EXPECTED_SYSNO_COUNT not found")?;
    let last_id = last_id.ok_or("census: Sysno::last().id() assertion not found")?;
    let mut classes: BTreeMap<String, (String, usize)> = BTreeMap::new();
    for (const_name, class) in [
        ("DETERMINIZED_SYSCALLS", "determinized"),
        ("PASSTHROUGH_SYSCALLS", "passthrough"),
        ("UNSUPPORTED_SYSCALLS", "unsupported"),
    ] {
        let Some(start) = lines
            .iter()
            .position(|l| l.contains(&format!("const {const_name}")))
        else {
            return Err(format!("census: {const_name} not found"));
        };
        let mut skip_next = false;
        for (off, line) in lines.iter().enumerate().skip(start) {
            let t = line.trim();
            if off > start && t.starts_with("];") {
                break;
            }
            if t.contains("#[cfg(") {
                skip_next = t.contains("aarch64") && !t.contains("not(");
                continue;
            }
            let mut at = 0;
            while let Some(rel) = line[at..].find('"') {
                let begin = at + rel + 1;
                let Some(end_rel) = line[begin..].find('"') else {
                    break;
                };
                let name = &line[begin..begin + end_rel];
                if is_syscall_name(name) && !skip_next {
                    if classes
                        .insert(name.to_string(), (class.to_string(), off + 1))
                        .is_some()
                    {
                        return Err(format!("census: {name} listed twice"));
                    }
                }
                at = begin + end_rel + 1;
            }
            if line.contains('"') {
                skip_next = false;
            }
            if off == start && t.contains("];") {
                break;
            }
        }
    }
    if classes.len() != expected_count {
        return Err(format!(
            "census parser-failed: reviewed lists hold {} x86_64 names, EXPECTED_SYSNO_COUNT is {expected_count}",
            classes.len()
        ));
    }
    Ok(Census {
        expected_count,
        last_id,
        classes,
    })
}

/// Facts derivable from Detcore source alone (no build): class from the
/// census, deterministic refusal from the refusal predicates, and the
/// record/replay subscription rule. Numbers (`nr`) stay -1 placeholders;
/// the exporter or an existing page must supply them.
/// Every explicitly named syscall in the `passthru_opt` branch of
/// Detcore's subscriptions, evaluating the branch for the generation
/// configuration (sequentialized, virtual time and metadata on, default
/// descriptor-naming backend). Every list counts, including
/// `#[cfg(target_arch = "x86_64")]` lists; aarch64 lists and the
/// backend-conditional memory-lock list are skipped, matching that
/// configuration. Unknown conditional shapes still contribute their
/// names only when their guard is one this function knows is true
/// under the generation configuration.
fn passthru_explicit_names(lib: &str) -> Result<BTreeSet<String>, String> {
    // Scan the original text: masking would blank the architecture
    // strings inside `#[cfg(target_arch = "...")]`, which are exactly
    // what distinguishes the lists that count here.
    let m = lib;
    let Some(branch) = m.find("let mut subscription = Subscription::none();") else {
        return Err("source facts parser-failed: passthru_opt branch not found".into());
    };
    let Some(sweep_rel) = m[branch..].find("crate::all_pinned_syscalls()") else {
        return Err("source facts parser-failed: passthru_opt sweep not found".into());
    };
    let region = &m[branch..branch + sweep_rel];
    let mut names = BTreeSet::new();
    let mut skip_call = false;
    let mut pending_skip = false;
    for line in region.lines() {
        let t = line.trim();
        if t.starts_with("//") {
            continue;
        }
        if t.contains("#[cfg(") {
            pending_skip = t.contains("aarch64") && !t.contains("not(");
            continue;
        }
        if t.contains("if !crate::procfs_inode::tool_can_name_guest_descriptors") {
            skip_call = true;
            continue;
        }
        if t.contains("subscription.syscall") {
            if skip_call {
                skip_call = false;
                pending_skip = false;
                continue;
            }
            if !pending_skip {
                let mut rest = line.to_string();
                while let Some(i) = rest.find("Sysno::") {
                    match ident_after(&rest, i + 7) {
                        Some((name, end)) => {
                            names.insert(name);
                            rest.replace_range(i..end, " ");
                        }
                        None => break,
                    }
                }
            }
        }
        if t.contains("Sysno::") {
            // A list continues over following lines; the pending cfg was
            // consumed by the call line itself or by these member lines.
            if !pending_skip && !t.contains("subscription.syscall") {
                let mut rest = line.to_string();
                while let Some(i) = rest.find("Sysno::") {
                    match ident_after(&rest, i + 7) {
                        Some((name, end)) => {
                            names.insert(name);
                            rest.replace_range(i..end, " ");
                        }
                        None => break,
                    }
                }
            }
            if t.contains("]);") || t.trim_end().ends_with(");") {
                pending_skip = false;
            }
        }
    }
    if names.is_empty() {
        return Err("source facts parser-failed: no passthru_opt subscriptions found".into());
    }
    Ok(names)
}

/// Independent quick-check read of the class the generation parser once
/// missed: names in x86_64-cfg-gated subscription calls, found by a plain
/// line scan rather than the generation parser's list walk.
fn x86_cfg_subscription_names(lib: &str) -> BTreeSet<String> {
    let lines: Vec<&str> = lib.lines().collect();
    let mut names = BTreeSet::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].contains("#[cfg(target_arch = \"x86_64\")]") {
            // Scan the gated statement: up to its closing `]);` or `);`.
            let mut j = i + 1;
            let mut saw_call = false;
            while j < lines.len() && j <= i + 40 {
                if lines[j].contains("subscription.syscall") {
                    saw_call = true;
                }
                if saw_call {
                    let mut rest = lines[j].to_string();
                    while let Some(at) = rest.find("Sysno::") {
                        match ident_after(&rest, at + 7) {
                            Some((name, end)) => {
                                names.insert(name);
                                rest.replace_range(at..end, " ");
                            }
                            None => break,
                        }
                    }
                    if lines[j].contains("]);") || lines[j].trim_end().ends_with(");") {
                        break;
                    }
                }
                j += 1;
            }
            i = j;
        }
        i += 1;
    }
    names
}

fn source_fact_rows(
    lib: &str,
    cls: &str,
    census: &Census,
) -> Result<BTreeMap<String, FactRow>, String> {
    let preds = merge_predicates(&[
        ("detcore/src/lib.rs", lib),
        ("detcore/src/syscall_classification.rs", cls),
    ]);
    let mut errors = Vec::new();
    let refused = expand_pred(
        "is_deterministically_refused_syscall",
        &preds,
        &mut Vec::new(),
        &mut errors,
    );
    let strict_only = expand_pred(
        "is_strict_only_deterministic_refusal_syscall",
        &preds,
        &mut Vec::new(),
        &mut errors,
    );
    if !errors.is_empty() {
        return Err(format!("source facts parser-failed: {}", errors.join("; ")));
    }
    // Run-mode subscription is the non-passthru branch's blanket
    // `all_syscalls` today; if that shape goes, fail rather than keep
    // stamping a constant `true` the source no longer supports.
    if !mask_rust(lib).contains("Subscription::all_syscalls()") {
        return Err(
            "source facts parser-failed: run-mode all_syscalls subscription not found".into(),
        );
    }
    let explicit = passthru_explicit_names(lib)?;
    let mut rows = BTreeMap::new();
    for (name, (class, _line)) in &census.classes {
        rows.insert(
            name.clone(),
            FactRow {
                nr: -1,
                detcore_class: class.clone(),
                refused: refused.contains(name),
                refused_only_when_fail_closed: strict_only.contains(name),
                subscribed_run: true,
                subscribed_record_replay: class != "passthrough" || explicit.contains(name),
            },
        );
    }
    Ok(rows)
}

// ---------------------------------------------------------------------------
// Facts: the TSV the exporter prints and the dispatch parse. Facts are
// bound to an exact HEAD: the exporter binary carries the SHA it was built
// against and refuses others, and this tool refuses to build it from a
// dirty tree. The exporter path needs a git checkout with the `reverie/`
// submodule; the census, dispatch, lint and freshness checks above it do
// not, and run in an fbsource tree as-is.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct FactRow {
    nr: i32,
    detcore_class: String,
    refused: bool,
    refused_only_when_fail_closed: bool,
    subscribed_run: bool,
    subscribed_record_replay: bool,
}

fn parse_facts_tsv(text: &str) -> Result<(String, BTreeMap<String, FactRow>), String> {
    let mut sha = String::new();
    let mut rows = BTreeMap::new();
    for line in text.lines() {
        if let Some(s) = line.strip_prefix("# hermit ") {
            sha = s.trim().to_string();
            continue;
        }
        if line.starts_with("name\t") || line.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() != 7 {
            return Err(format!("facts line does not have 7 columns: {line:?}"));
        }
        let bit = |s: &str| s == "1";
        rows.insert(
            f[0].to_string(),
            FactRow {
                nr: f[1]
                    .parse()
                    .map_err(|_| format!("bad nr in facts line {line:?}"))?,
                detcore_class: f[2].to_string(),
                refused: bit(f[3]),
                refused_only_when_fail_closed: bit(f[4]),
                subscribed_run: bit(f[5]),
                subscribed_record_replay: bit(f[6]),
            },
        );
    }
    if sha.is_empty() {
        return Err("facts TSV does not name its source revision".into());
    }
    Ok((sha, rows))
}

fn run_cmd(cmd: &mut Command) -> Result<String, String> {
    let out = cmd
        .output()
        .map_err(|e| format!("cannot run {cmd:?}: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "{cmd:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
                .chars()
                .take(600)
                .collect::<String>()
        ))
    }
}

fn git_head(root: &Path) -> String {
    run_cmd(
        Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["rev-parse", "HEAD"]),
    )
    .map(|s| s.trim().to_string())
    .unwrap_or_else(|e| fail(&e))
}

fn export_facts(root: &Path) -> Result<(String, BTreeMap<String, FactRow>), String> {
    let sha = git_head(root);
    let dirty = run_cmd(Command::new("git").arg("-C").arg(root).args([
        "status",
        "--porcelain",
        "--",
        "detcore",
        "detcore-model",
        "reverie",
        "Cargo.lock",
    ]))?;
    if !dirty.trim().is_empty() {
        return Err(format!(
            "refusing to export facts from a dirty tree (detcore, detcore-model, reverie, Cargo.lock); commit or stash first:\n{dirty}"
        ));
    }
    let template = read(&root.join("scripts/syscall-pages-facts.rs"));
    let build = root.join("ignored/syscall-pages/facts-build").join(&sha);
    fs::create_dir_all(build.join("src")).map_err(|e| e.to_string())?;
    let root_s = root.to_string_lossy().into_owned();
    let main_rs = template
        .replace("@HERMIT_SHA@", &sha)
        .lines()
        .filter(|l| !l.starts_with("//!"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(build.join("src/main.rs"), main_rs).map_err(|e| e.to_string())?;
    let reverie_ok = root.join("reverie/reverie/Cargo.toml").exists();
    let reverie_rev = run_cmd(
        Command::new("git")
            .arg("-C")
            .arg(root.join("reverie"))
            .args(["rev-parse", "HEAD"]),
    )
    .map(|s| s.trim().to_string())
    .unwrap_or_default();
    let detcore_manifest = read(&root.join("detcore/Cargo.toml"));
    if !reverie_ok || !detcore_manifest.contains(&reverie_rev) || reverie_rev.is_empty() {
        return Err(
            "the checkout's reverie/ tree does not match detcore's pinned reverie rev".into(),
        );
    }
    let manifest = format!(
        "[package]\nname = \"syscall-pages-facts-build\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n\n[dependencies]\ndetcore = {{ package = \"hermit-detcore\", path = \"{root_s}/detcore\" }}\nreverie = {{ package = \"reverie-core\", path = \"{root_s}/reverie/reverie\" }}\n\n[patch.\"https://github.com/rrnewton/reverie.git\"]\nreverie-core = {{ path = \"{root_s}/reverie/reverie\" }}\nreverie-syscalls = {{ path = \"{root_s}/reverie/reverie-syscalls\" }}\nreverie-process = {{ path = \"{root_s}/reverie/reverie-process\" }}\nreverie-memory = {{ path = \"{root_s}/reverie/reverie-memory\" }}\n"
    );
    fs::write(build.join("Cargo.toml"), manifest).map_err(|e| e.to_string())?;
    fs::copy(root.join("Cargo.lock"), build.join("Cargo.lock")).map_err(|e| e.to_string())?;
    // Honour an external CARGO_TARGET_DIR (freeze windows keep build output
    // out of the tree); default beside the generated project under ignored/.
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(|t| PathBuf::from(t).join(format!("syscall-pages-facts-{sha}")))
        .unwrap_or_else(|| build.join("target"));
    run_cmd(
        Command::new("cargo")
            .args(["build", "--offline", "--manifest-path"])
            .arg(build.join("Cargo.toml"))
            .env("CARGO_TARGET_DIR", &target),
    )?;
    let bin = target.join("debug/syscall-pages-facts-build");
    let tsv = run_cmd(Command::new(&bin).args(["--expect-sha", &sha]))?;
    let facts_cache = root
        .join("ignored/syscall-pages/facts")
        .join(format!("{sha}.tsv"));
    if let Some(parent) = facts_cache.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::write(&facts_cache, &tsv).map_err(|e| e.to_string())?;
    parse_facts_tsv(&tsv)
}

// ---------------------------------------------------------------------------
// Loading pages, aggregate outputs, and the facts comparison.
// ---------------------------------------------------------------------------

fn load_pages(root: &Path) -> Vec<(String, String, Report, Option<Page>)> {
    let dir = root.join(PAGES_DIR);
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(&dir) else {
        return out;
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.ends_with(".md") && n != "README.md" && n != "SCHEMA.md")
        .map(|n| n.trim_end_matches(".md").to_string())
        .collect();
    names.sort();
    for name in names {
        let text = read(&dir.join(format!("{name}.md")));
        let (rep, page) = lint_page(&name, &text, root);
        out.push((name, text, rep, page));
    }
    out
}

fn sorted_pages(pages: &[Page]) -> Vec<Page> {
    let mut sorted = pages.to_vec();
    sorted.sort_by(|a, b| {
        a.nr_x86_64
            .cmp(&b.nr_x86_64)
            .then_with(|| a.syscall.cmp(&b.syscall))
    });
    sorted
}

fn syscalls_json(pages: &[Page], sha: &str) -> String {
    let pages = sorted_pages(pages);
    let rows: Vec<serde_json::Value> = pages
        .iter()
        .map(|p| {
            let f = &p.code_facts;
            serde_json::json!({
                "syscall": p.syscall,
                "nr_x86_64": p.nr_x86_64,
                "strategy": strategy(f),
                "detcore_class": f.detcore_class,
                "refused": f.refused,
                "refused_only_when_fail_closed": f.refused_only_when_fail_closed,
                "dispatch": f.dispatch,
                "subscribed_run": f.subscribed_run,
                "subscribed_record_replay": f.subscribed_record_replay,
                "discussion": {"kind": p.discussion_kind, "url": p.discussion_url},
                "page": format!("docs/syscalls/{}.md", p.syscall),
            })
        })
        .collect();
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for p in &pages {
        *counts.entry(strategy(&p.code_facts)).or_default() += 1;
    }
    let doc = serde_json::json!({
        "schema": "syscalls.json/v2",
        "generated_by": "scripts/syscall-pages.rs aggregate",
        "code_facts_as_of": sha,
        "total": pages.len(),
        "counts": counts,
        "syscalls": rows,
    });
    serde_json::to_string_pretty(&doc).unwrap() + "\n"
}

fn index_md(pages: &[Page], sha: &str) -> String {
    let pages = sorted_pages(pages);
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for p in &pages {
        *counts.entry(strategy(&p.code_facts)).or_default() += 1;
    }
    let mut s = String::new();
    let _ = writeln!(s, "# Per-syscall Detcore pages");
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "One facts-only page per Linux syscall in this tree's pinned x86-64 Detcore census: the code-derived strategy and a discussion link. Checked analysis prose returns in later batches. The machine-readable form is [syscalls.json](syscalls.json); the page format is [SCHEMA.md](SCHEMA.md). Regenerate with `scripts/syscall-pages.rs generate` and `aggregate`; verify with `scripts/syscall-pages.rs check`."
    );
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "These pages track Detcore strategy only. Which backends can run a syscall is tracked with the backends, not here. Which syscalls the test matrix exercises is a separate measurement in the [syscall coverage audit]({COVERAGE_URL}), linked here and never merged into the strategy table."
    );
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "Facts on every page were exported or derived at the revision in its `code_facts.as_of`; source citations in generated strategy text are line numbers in that source tree, not permanent addresses."
    );
    let _ = writeln!(s);
    let _ = writeln!(s, "Code facts as of source revision `{sha}`.");
    let _ = writeln!(s);
    let _ = writeln!(s, "| strategy | count |");
    let _ = writeln!(s, "|---|---:|");
    for (k, v) in &counts {
        let _ = writeln!(s, "| {k} | {v} |");
    }
    let _ = writeln!(s, "| **total** | **{}** |", pages.len());
    let _ = writeln!(s);
    let _ = writeln!(s, "| syscall | nr | strategy | dispatch | discussion |");
    let _ = writeln!(s, "|---|---:|---|---|---|");
    for p in &pages {
        let _ = writeln!(
            s,
            "| [{}]({}.md) | {} | {} | {} | [{}]({}) |",
            p.syscall,
            p.syscall,
            p.nr_x86_64,
            strategy(&p.code_facts),
            p.code_facts.dispatch,
            p.discussion_kind,
            p.discussion_url
        );
    }
    s
}

fn facts_diff(
    pages: &[Page],
    sha: &str,
    rows: &BTreeMap<String, FactRow>,
    dispatch: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut out = Vec::new();
    let want = |p: &Page| CodeFacts {
        as_of: sha.to_string(),
        detcore_class: rows[&p.syscall].detcore_class.clone(),
        dispatch: dispatch.get(&p.syscall).cloned().unwrap_or_default(),
        refused: rows[&p.syscall].refused,
        refused_only_when_fail_closed: rows[&p.syscall].refused_only_when_fail_closed,
        subscribed_run: rows[&p.syscall].subscribed_run,
        subscribed_record_replay: rows[&p.syscall].subscribed_record_replay,
    };
    for p in pages {
        if !rows.contains_key(&p.syscall) {
            out.push(format!(
                "{}: page has no facts row (not a pinned Sysno?)",
                p.syscall
            ));
            continue;
        }
        let w = want(p);
        let f = &p.code_facts;
        let mut diffs = Vec::new();
        // `as_of` is the generation revision, not required to equal the
        // exporter HEAD: docs-only commits after generation must not make
        // every page stale. Quick check ties it to the aggregate and to
        // the Status hermit prefix instead (see SCHEMA.md).
        if f.detcore_class != w.detcore_class {
            diffs.push(format!(
                "detcore_class {} -> {}",
                f.detcore_class, w.detcore_class
            ));
        }
        if f.dispatch != w.dispatch {
            diffs.push(format!("dispatch {} -> {}", f.dispatch, w.dispatch));
        }
        if f.refused != w.refused {
            diffs.push(format!("refused {} -> {}", f.refused, w.refused));
        }
        if f.refused_only_when_fail_closed != w.refused_only_when_fail_closed {
            diffs.push("refused_only_when_fail_closed changed".into());
        }
        if f.subscribed_run != w.subscribed_run {
            diffs.push("subscribed_run changed".into());
        }
        if f.subscribed_record_replay != w.subscribed_record_replay {
            diffs.push("subscribed_record_replay changed".into());
        }
        if p.nr_x86_64 != rows[&p.syscall].nr {
            diffs.push(format!(
                "nr_x86_64 {} -> {}",
                p.nr_x86_64, rows[&p.syscall].nr
            ));
        }
        if !diffs.is_empty() {
            out.push(format!("{}: {}", p.syscall, diffs.join(", ")));
        }
    }
    for name in rows.keys() {
        if !pages.iter().any(|p| &p.syscall == name) {
            out.push(format!("{name}: pinned Sysno has no page"));
        }
    }
    out
}

fn today_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    // Howard Hinnant's civil-from-days algorithm.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

fn discussion_url(name: &str) -> String {
    let page =
        format!("https://github.com/facebookexperimental/hermit/blob/main/docs/syscalls/{name}.md");
    let title = url_encode(&format!("Syscall discussion: {name}"));
    let body = url_encode(&format!(
        "Discussion for the `{name}` syscall page: {page}\n\nWhat should change in how Detcore handles `{name}`, or in what the page says?\n"
    ));
    format!("https://github.com/facebookexperimental/hermit/issues/new?title={title}&body={body}")
}

// ---------------------------------------------------------------------------
// Commands.
// ---------------------------------------------------------------------------

fn print_reports(loaded: &[(String, String, Report, Option<Page>)]) -> usize {
    let mut errors = 0;
    for (name, _, rep, _) in loaded {
        for e in &rep.errors {
            eprintln!("{name}: {e}");
            errors += 1;
        }
        for w in &rep.warnings {
            println!("{name}: warning {w}");
        }
    }
    errors
}

fn good_pages(loaded: &[(String, String, Report, Option<Page>)]) -> Vec<Page> {
    loaded.iter().filter_map(|(_, _, _, p)| p.clone()).collect()
}

fn census_at(root: &Path) -> Result<Census, String> {
    parse_census(&read(&root.join("detcore/src/syscall_classification.rs")))
}

fn dispatch_at(root: &Path, classes: &BTreeMap<String, String>) -> DispatchReport {
    let lib = read(&root.join("detcore/src/lib.rs"));
    let cls = read(&root.join("detcore/src/syscall_classification.rs"));
    parse_dispatch(
        &lib,
        &[
            ("detcore/src/lib.rs", &lib),
            ("detcore/src/syscall_classification.rs", &cls),
        ],
        classes,
    )
}

fn cmd_lint(root: &Path) -> i32 {
    let loaded = load_pages(root);
    if loaded.is_empty() {
        eprintln!("syscall-pages: no pages under {PAGES_DIR}");
        return 1;
    }
    let errors = print_reports(&loaded);
    println!("lint: {} pages, {errors} errors", loaded.len());
    if errors > 0 { 1 } else { 0 }
}

fn facts_context(
    root: &Path,
    pages: &[Page],
) -> Result<(String, BTreeMap<String, FactRow>, DispatchReport), String> {
    let (sha, rows) = export_facts(root)?;
    let classes: BTreeMap<String, String> = rows
        .iter()
        .map(|(k, v)| (k.clone(), v.detcore_class.clone()))
        .collect();
    let dispatch = dispatch_at(root, &classes);
    if dispatch.status != "ok" {
        return Err(format!(
            "dispatch parse failed: {}",
            dispatch.errors.join("; ")
        ));
    }
    let _ = pages;
    Ok((sha, rows, dispatch))
}

fn cmd_facts(root: &Path, args: &[String]) -> i32 {
    let apply = args.iter().any(|a| a == "--apply");
    let loaded = load_pages(root);
    let pages = good_pages(&loaded);
    let (sha, rows, dispatch) = match facts_context(root, &pages) {
        Ok(v) => v,
        Err(e) => fail(&e),
    };
    let diffs = facts_diff(&pages, &sha, &rows, &dispatch.dispatch);
    if !apply {
        for d in &diffs {
            println!("{d}");
        }
        println!(
            "facts at {sha}: {} pages differ (dispatch parse {}; missing arms: {})",
            diffs.len(),
            dispatch.status,
            dispatch.missing_arm.len()
        );
        return if diffs.is_empty() { 0 } else { 1 };
    }
    let date = arg_value(args, "--date").unwrap_or_else(today_utc);
    let mut changed = 0;
    for p in &pages {
        let Some(row) = rows.get(&p.syscall) else {
            continue;
        };
        let want = CodeFacts {
            as_of: sha.clone(),
            detcore_class: row.detcore_class.clone(),
            dispatch: dispatch
                .dispatch
                .get(&p.syscall)
                .cloned()
                .unwrap_or_default(),
            refused: row.refused,
            refused_only_when_fail_closed: row.refused_only_when_fail_closed,
            subscribed_run: row.subscribed_run,
            subscribed_record_replay: row.subscribed_record_replay,
        };
        let values_differ = p.code_facts != want;
        if !values_differ {
            continue;
        }
        let facts_changed = p.code_facts.detcore_class != want.detcore_class
            || p.code_facts.dispatch != want.dispatch
            || p.code_facts.refused != want.refused
            || p.code_facts.refused_only_when_fail_closed != want.refused_only_when_fail_closed
            || p.code_facts.subscribed_run != want.subscribed_run
            || p.code_facts.subscribed_record_replay != want.subscribed_record_replay;
        let mut q = p.clone();
        q.code_facts = want;
        let body = match census_at(root) {
            Ok(census) => {
                let class_line = census.classes.get(&p.syscall).map(|(_, l)| *l).unwrap_or(0);
                strategy_body(&q, class_line, dispatch.lines.get(&p.syscall).copied())
            }
            Err(_) => format!("\n{}\n", strategy_sentence(&q)),
        };
        q.sections.insert("Strategy".into(), body);
        if facts_changed {
            let entry = format!(
                "- {date} · rev {} · facts: code_facts refreshed by `facts --apply` at this SHA.",
                &sha[..12]
            );
            let status = q.sections.entry("Status".into()).or_default();
            if !status.trim_end().ends_with(&entry) {
                status.push_str(&entry);
                status.push('\n');
            }
        }
        let path = root.join(PAGES_DIR).join(format!("{}.md", p.syscall));
        if fs::write(&path, emit_page(&q)).is_err() {
            fail(&format!("cannot write {}", path.display()));
        }
        changed += 1;
    }
    println!("facts --apply at {sha}: {changed} pages updated");
    0
}

fn aggregate_sha(pages: &[Page]) -> String {
    let mut set = BTreeSet::new();
    for p in pages {
        set.insert(p.code_facts.as_of.clone());
    }
    if set.len() == 1 {
        set.into_iter().next().unwrap()
    } else {
        "mixed (see pages)".into()
    }
}

fn cmd_aggregate(root: &Path, check_only: bool) -> i32 {
    let loaded = load_pages(root);
    let errors = print_reports(&loaded);
    if errors > 0 {
        eprintln!("syscall-pages: aggregate refuses while lint has {errors} errors");
        return 1;
    }
    let mut pages = good_pages(&loaded);
    pages.sort_by_key(|p| p.nr_x86_64);
    let sha = aggregate_sha(&pages);
    let json = syscalls_json(&pages, &sha);
    let index = index_md(&pages, &sha);
    let json_path = root.join(JSON_FILE);
    let index_path = root.join(INDEX_FILE);
    if check_only {
        let mut bad = 0;
        if read(&json_path) != json {
            eprintln!(
                "syscall-pages: {JSON_FILE} is stale; run `scripts/syscall-pages.rs aggregate`"
            );
            bad += 1;
        }
        if read(&index_path) != index {
            eprintln!(
                "syscall-pages: {INDEX_FILE} is stale; run `scripts/syscall-pages.rs aggregate`"
            );
            bad += 1;
        }
        return if bad > 0 { 1 } else { 0 };
    }
    if fs::write(&json_path, json).is_err() || fs::write(&index_path, index).is_err() {
        fail("cannot write aggregate outputs");
    }
    println!(
        "aggregate: {} pages -> {JSON_FILE}, {INDEX_FILE}",
        pages.len()
    );
    0
}

fn cmd_check(root: &Path, quick: bool) -> i32 {
    let mut rc = 0;
    let loaded = load_pages(root);
    if loaded.is_empty() {
        eprintln!("syscall-pages: no pages under {PAGES_DIR}");
        return 1;
    }
    let errors = print_reports(&loaded);
    if errors > 0 {
        rc = 1;
    }
    let pages = good_pages(&loaded);
    {
        let sorted = sorted_pages(&pages);
        let sha = aggregate_sha(&sorted);
        for (rel, want) in [
            (JSON_FILE, syscalls_json(&sorted, &sha)),
            (INDEX_FILE, index_md(&sorted, &sha)),
        ] {
            let got = read(&root.join(rel));
            if got != want {
                eprintln!("check: {rel} is stale; run `scripts/syscall-pages.rs aggregate`");
                rc = 1;
            }
            for hit in scrub_findings(&got) {
                eprintln!(
                    "check: {rel}: internal or host-specific text in generated output: {hit:?}"
                );
                rc = 1;
            }
        }
        // Site freshness is checked when the rendered tree exists (it is
        // added by the site change on top of the pages change); stale or
        // orphan HTML then fails, byte for byte.
        let site_dir = root.join("docs/site/syscalls");
        if site_dir.exists() {
            let want = render_outputs(&sorted, &sha);
            let mut seen_files: BTreeSet<String> = BTreeSet::new();
            if let Ok(entries) = fs::read_dir(&site_dir) {
                for e in entries.filter_map(|e| e.ok()) {
                    if let Ok(name) = e.file_name().into_string() {
                        if name.ends_with(".html") {
                            seen_files.insert(name);
                        }
                    }
                }
            }
            for (rel, html) in &want {
                let name = rel.to_string_lossy().into_owned();
                if !seen_files.contains(&name) {
                    eprintln!(
                        "check: docs/site/syscalls/{name} is missing; run `scripts/syscall-pages.rs render`"
                    );
                    rc = 1;
                    continue;
                }
                let got = read(&site_dir.join(rel));
                if got != *html {
                    eprintln!(
                        "check: docs/site/syscalls/{name} is stale; run `scripts/syscall-pages.rs render`"
                    );
                    rc = 1;
                }
                for hit in scrub_findings(&got) {
                    eprintln!(
                        "check: docs/site/syscalls/{name}: internal or host-specific text in generated output: {hit:?}"
                    );
                    rc = 1;
                }
            }
            for orphan in seen_files
                .iter()
                .filter(|n| !want.contains_key(&PathBuf::from(n.as_str())))
            {
                eprintln!(
                    "check: docs/site/syscalls/{orphan} is orphaned rendered output; run `scripts/syscall-pages.rs render`"
                );
                rc = 1;
            }
        }
    }
    if quick {
        // One generation revision across the page set, echoed by every
        // Status log: ties `as_of` to the recorded generation instead of
        // leaving it a free per-page claim.
        let mut as_ofs: BTreeSet<&str> = BTreeSet::new();
        for p in &pages {
            as_ofs.insert(&p.code_facts.as_of);
            let prefix = &p.code_facts.as_of[..12.min(p.code_facts.as_of.len())];
            let status = p.sections.get("Status").cloned().unwrap_or_default();
            if !status.contains(&format!("rev {prefix}")) {
                eprintln!(
                    "check: {}: Status has no entry at its code_facts.as_of prefix {prefix}",
                    p.syscall
                );
                rc = 1;
            }
        }
        if as_ofs.len() > 1 {
            eprintln!("check: pages do not share one code_facts.as_of: {as_ofs:?}");
            rc = 1;
        }
        // The in-tree gate: census, classes, dispatch, citations and
        // generated-output freshness are all derived from this tree's
        // Detcore sources, with no build and no VCS access. Exact syscall
        // numbers and refused/subscription facts are proven by the full
        // facts export (see SCHEMA.md); quick mode cannot see those.
        match census_at(root) {
            Err(e) => {
                eprintln!("check: census parser-failed: {e}");
                rc = 1;
            }
            Ok(census) => {
                let page_names: BTreeSet<String> =
                    pages.iter().map(|p| p.syscall.clone()).collect();
                for name in census.classes.keys() {
                    if !page_names.contains(name) {
                        eprintln!("check: {name}: in Detcore census but has no page");
                        rc = 1;
                    }
                }
                for p in &pages {
                    let Some((class, class_line)) = census.classes.get(&p.syscall) else {
                        eprintln!("check: {}: page is not in Detcore census", p.syscall);
                        rc = 1;
                        continue;
                    };
                    if &p.code_facts.detcore_class != class {
                        eprintln!(
                            "check: {}: detcore_class {} disagrees with census {class}",
                            p.syscall, p.code_facts.detcore_class
                        );
                        rc = 1;
                    }
                    if p.nr_x86_64 > census.last_id {
                        eprintln!(
                            "check: {}: nr_x86_64 {} exceeds census last id {}",
                            p.syscall, p.nr_x86_64, census.last_id
                        );
                        rc = 1;
                    }
                    let strategy_text = p.sections.get("Strategy").cloned().unwrap_or_default();
                    let want_cite = format!("detcore/src/syscall_classification.rs:{class_line}");
                    if !strategy_text.contains(&want_cite) {
                        eprintln!(
                            "check: {}: Strategy does not cite its classification at {want_cite}",
                            p.syscall
                        );
                        rc = 1;
                    }
                }
                let mut nrs: BTreeMap<i32, String> = BTreeMap::new();
                for p in &pages {
                    if let Some(other) = nrs.insert(p.nr_x86_64, p.syscall.clone()) {
                        eprintln!(
                            "check: nr_x86_64 {} is claimed by both {other} and {}",
                            p.nr_x86_64, p.syscall
                        );
                        rc = 1;
                    }
                }
                // Source-derived refused/subscription facts are compared
                // too: deriving them at generation time but never
                // re-checking them is how a missed subscription list
                // shipped a wrong `access` fact. The independent cfg-list
                // scan below is a second read of that same class.
                let lib_src = read(&root.join("detcore/src/lib.rs"));
                let cls_src = read(&root.join("detcore/src/syscall_classification.rs"));
                match source_fact_rows(&lib_src, &cls_src, &census) {
                    Ok(rows) => {
                        for p in &pages {
                            let Some(row) = rows.get(&p.syscall) else {
                                continue;
                            };
                            let f = &p.code_facts;
                            if f.refused != row.refused
                                || f.refused_only_when_fail_closed
                                    != row.refused_only_when_fail_closed
                                || f.subscribed_run != row.subscribed_run
                                || f.subscribed_record_replay != row.subscribed_record_replay
                            {
                                eprintln!(
                                    "check: {}: refused/subscription facts disagrees with source derivation (refused {}, strict {}, run {}, record/replay {})",
                                    p.syscall,
                                    row.refused,
                                    row.refused_only_when_fail_closed,
                                    row.subscribed_run,
                                    row.subscribed_record_replay
                                );
                                rc = 1;
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("check: {e}");
                        rc = 1;
                    }
                }
                for name in x86_cfg_subscription_names(&lib_src) {
                    if let Some(p) = pages.iter().find(|p| p.syscall == name) {
                        if p.code_facts.detcore_class == "passthrough"
                            && !p.code_facts.subscribed_record_replay
                        {
                            eprintln!(
                                "check: {name}: x86_64 subscription list subscribes it under passthru_opt but page says subscribed_record_replay: false"
                            );
                            rc = 1;
                        }
                    }
                }
                let classes: BTreeMap<String, String> = census
                    .classes
                    .iter()
                    .map(|(k, (v, _))| (k.clone(), v.clone()))
                    .collect();
                let dispatch = dispatch_at(root, &classes);
                if dispatch.status != "ok" {
                    eprintln!(
                        "check: dispatch parser-failed: {}",
                        dispatch.errors.join("; ")
                    );
                    rc = 1;
                } else {
                    for name in &dispatch.missing_arm {
                        eprintln!("check: {name}: determinized but reaches the fallback arm");
                        rc = 1;
                    }
                    for p in &pages {
                        if let Some(d) = dispatch.dispatch.get(&p.syscall) {
                            if d != &p.code_facts.dispatch {
                                eprintln!(
                                    "check: {}: dispatch {} disagrees with source parse {d}",
                                    p.syscall, p.code_facts.dispatch
                                );
                                rc = 1;
                            }
                        }
                        if let Some(line) = dispatch.lines.get(&p.syscall) {
                            let strategy_text =
                                p.sections.get("Strategy").cloned().unwrap_or_default();
                            let want_cite = format!("detcore/src/lib.rs:{line}");
                            if !strategy_text.contains(&want_cite) {
                                eprintln!(
                                    "check: {}: Strategy does not cite its dispatch at {want_cite}",
                                    p.syscall
                                );
                                rc = 1;
                            }
                        }
                    }
                }
            }
        }
        println!("check --quick: {} pages (facts build skipped)", pages.len());
        return rc;
    }
    match facts_context(root, &pages) {
        Ok((sha, rows, dispatch)) => {
            let diffs = facts_diff(&pages, &sha, &rows, &dispatch.dispatch);
            for d in &diffs {
                eprintln!("check: {d}");
            }
            if !diffs.is_empty() {
                rc = 1;
            }
            println!(
                "check: {} pages at {sha}; {} facts diffs",
                pages.len(),
                diffs.len()
            );
        }
        Err(e) => {
            eprintln!("check: {e}");
            rc = 1;
        }
    }
    rc
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone())
}

/// Bits a facts-only regeneration preserves from a previous page version:
/// its number, public related links, discussion link, and (for v2 pages)
/// its Status log. Read with line grabs so v1 pages migrate too.
fn existing_page_bits(
    root: &Path,
    name: &str,
) -> (
    Option<i32>,
    Vec<String>,
    Option<(String, String)>,
    Option<String>,
) {
    let path = root.join(PAGES_DIR).join(format!("{name}.md"));
    let Ok(text) = fs::read_to_string(&path) else {
        return (None, Vec::new(), None, None);
    };
    let mut nr = None;
    let mut related = Vec::new();
    let mut in_related = false;
    let mut kind = None;
    let mut url = None;
    for line in text.lines() {
        let t = line.trim();
        if let Some(v) = t.strip_prefix("nr_x86_64:") {
            nr = v.trim().parse().ok();
        }
        if t == "related:" {
            in_related = true;
            continue;
        }
        if in_related {
            if let Some(item) = t.strip_prefix("- ") {
                related.push(item.trim().to_string());
                continue;
            }
            in_related = false;
        }
        if let Some(v) = t.strip_prefix("kind:") {
            kind = Some(v.trim().to_string());
        }
        if let Some(v) = t.strip_prefix("url:") {
            url = Some(v.trim().to_string());
        }
    }
    let status = if text.contains(&format!("schema: {SCHEMA}")) {
        split_sections(split_front_matter(&text).map(|x| x.1).unwrap_or(""))
            .get("Status")
            .cloned()
            .map(|s| s.replace(" · hermit ", " · rev "))
    } else {
        None
    };
    (
        nr,
        related,
        match (kind, url) {
            (Some(k), Some(u)) => Some((k, u)),
            _ => None,
        },
        status,
    )
}

/// (Re)generate every facts-only page from this tree's Detcore sources.
/// Numbers come from `--facts` (an exporter TSV), then `--nr-map`
/// (`name<TAB>nr` lines), then an existing page; a census name with none
/// fails closed rather than guessing a number.
fn cmd_generate(root: &Path, args: &[String]) -> i32 {
    let census = match census_at(root) {
        Ok(c) => c,
        Err(e) => fail(&format!("generate: census parser-failed: {e}")),
    };
    let lib = read(&root.join("detcore/src/lib.rs"));
    let cls = read(&root.join("detcore/src/syscall_classification.rs"));
    let classes: BTreeMap<String, String> = census
        .classes
        .iter()
        .map(|(k, (v, _))| (k.clone(), v.clone()))
        .collect();
    let dispatch = dispatch_at(root, &classes);
    if dispatch.status != "ok" {
        fail(&format!(
            "generate: dispatch parser-failed: {}",
            dispatch.errors.join("; ")
        ));
    }
    if !dispatch.missing_arm.is_empty() {
        fail(&format!(
            "generate: determinized syscalls reach the fallback arm: {}",
            dispatch.missing_arm.join(", ")
        ));
    }
    let mut rows = match source_fact_rows(&lib, &cls, &census) {
        Ok(r) => r,
        Err(e) => fail(&format!("generate: {e}")),
    };
    let mut sha = arg_value(args, "--as-of").unwrap_or_default();
    if let Some(facts_path) = arg_value(args, "--facts") {
        let text = read(Path::new(&facts_path));
        let (facts_sha, facts) = match parse_facts_tsv(&text) {
            Ok(v) => v,
            Err(e) => fail(&format!("generate: --facts: {e}")),
        };
        if sha.is_empty() {
            sha = facts_sha;
        } else if sha != facts_sha {
            fail("generate: --as-of disagrees with the --facts TSV revision");
        }
        for (name, row) in facts {
            if let Some(slot) = rows.get_mut(&name) {
                *slot = row;
            }
        }
    }
    if sha.len() != 40 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        fail("generate: need --as-of <40-hex source revision> or --facts TSV with one");
    }
    let mut nr_map: BTreeMap<String, i32> = BTreeMap::new();
    if let Some(map_path) = arg_value(args, "--nr-map") {
        for line in read(Path::new(&map_path)).lines() {
            let Some((name, nr)) = line.split_once('\t') else {
                continue;
            };
            if let Ok(n) = nr.trim().parse::<i32>() {
                nr_map.insert(name.trim().to_string(), n);
            }
        }
    }
    let mut mapping: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    if let Some(mapping_path) = arg_value(args, "--mapping") {
        let v: serde_json::Value = serde_json::from_str(&read(Path::new(&mapping_path)))
            .unwrap_or_else(|e| fail(&format!("generate: bad --mapping JSON: {e}")));
        if let Some(obj) = v.as_object() {
            for (k, val) in obj {
                let nums = val
                    .as_array()
                    .map(|a| a.iter().filter_map(|x| x.as_u64()).collect())
                    .unwrap_or_default();
                mapping.insert(k.clone(), nums);
            }
        }
    }
    let date = arg_value(args, "--date").unwrap_or_else(today_utc);
    let dir = root.join(PAGES_DIR);
    if fs::create_dir_all(&dir).is_err() {
        fail("generate: cannot create docs/syscalls");
    }
    let mut written = 0;
    let mut unresolved = Vec::new();
    for (name, row) in &rows {
        let (existing_nr, mut related, existing_discussion, existing_status) =
            existing_page_bits(root, name);
        let nr = if row.nr >= 0 {
            row.nr
        } else if let Some(n) = nr_map.get(name) {
            *n
        } else if let Some(n) = existing_nr {
            n
        } else {
            unresolved.push(name.clone());
            continue;
        };
        for n in mapping.get(name).cloned().unwrap_or_default() {
            let u = format!("https://github.com/rrnewton/hermit/issues/{n}");
            if !related.contains(&u) {
                related.push(u);
            }
        }
        if name == "ppoll"
            && !related.contains(&"https://github.com/rrnewton/hermit/issues/26".to_string())
        {
            // The one-off ppoll tracker used a different title pattern, so
            // it appears in no per-syscall mapping input.
            related.push("https://github.com/rrnewton/hermit/issues/26".into());
        }
        let (discussion_kind, discussion_url) =
            existing_discussion.unwrap_or_else(|| ("new".into(), discussion_url(name)));
        let mut page = Page {
            syscall: name.clone(),
            nr_x86_64: nr,
            related,
            discussion_kind: discussion_kind.clone(),
            discussion_url: discussion_url.clone(),
            code_facts: CodeFacts {
                as_of: sha.clone(),
                detcore_class: row.detcore_class.clone(),
                dispatch: dispatch.dispatch.get(name).cloned().unwrap_or_default(),
                refused: row.refused,
                refused_only_when_fail_closed: row.refused_only_when_fail_closed,
                subscribed_run: row.subscribed_run,
                subscribed_record_replay: row.subscribed_record_replay,
            },
            sections: BTreeMap::new(),
        };
        let class_line = census.classes.get(name).map(|(_, l)| *l).unwrap_or(0);
        page.sections.insert(
            "Strategy".into(),
            strategy_body(&page, class_line, dispatch.lines.get(name).copied()),
        );
        page.sections.insert(
            "Discussion".into(),
            format!(
                "\n[Start a discussion about `{name}`]({discussion_url}) (opens a prefilled issue form; nothing is filed until you submit it).\n"
            ),
        );
        let status = existing_status.unwrap_or_else(|| {
            format!(
                "\n- {date} · rev {} · facts-only page generated from Detcore code facts at this revision.\n",
                &sha[..12]
            )
        });
        page.sections.insert("Status".into(), status);
        if fs::write(dir.join(format!("{name}.md")), emit_page(&page)).is_err() {
            fail(&format!("generate: cannot write {name}.md"));
        }
        written += 1;
    }
    if !unresolved.is_empty() {
        eprintln!(
            "generate: no syscall number known for: {} (supply --facts or --nr-map)",
            unresolved.join(", ")
        );
        return 1;
    }
    println!(
        "generate: {written} facts-only pages at {sha} (census {}..={})",
        census.expected_count, census.last_id
    );
    0
}

/// Escape text for HTML. Rendered pages are generated, never hand-edited.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Minimal inline Markdown on already-escaped text: `code`, **bold**,
/// [text](url). Anything unrecognised stays literal text.
fn inline_md(escaped: &str) -> String {
    let mut out = String::new();
    let mut rest = escaped;
    loop {
        let code = rest.find('`');
        let bold = rest.find("**");
        let link = rest.find('[');
        let next = [code, bold, link].into_iter().flatten().min();
        let Some(at) = next else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        if let Some(stripped) = rest.strip_prefix('`') {
            match stripped.find('`') {
                Some(end) => {
                    out.push_str(&format!("<code>{}</code>", &stripped[..end]));
                    rest = &stripped[end + 1..];
                }
                None => {
                    out.push('`');
                    rest = stripped;
                }
            }
        } else if let Some(stripped) = rest.strip_prefix("**") {
            match stripped.find("**") {
                Some(end) => {
                    out.push_str(&format!("<strong>{}</strong>", &stripped[..end]));
                    rest = &stripped[end + 2..];
                }
                None => {
                    out.push_str("**");
                    rest = stripped;
                }
            }
        } else if rest.starts_with('[') {
            let parsed = rest.find("](").and_then(|mid| {
                rest[mid..].find(')').map(|close| {
                    (
                        rest[1..mid].to_string(),
                        rest[mid + 2..mid + close].to_string(),
                        mid + close + 1,
                    )
                })
            });
            match parsed {
                Some((label, url, len))
                    if url.starts_with("https://")
                        || url.ends_with(".md")
                        || url.ends_with(".html") =>
                {
                    // `url` is already HTML-escaped (the parser runs on
                    // escaped text), so escaping it again would corrupt
                    // every `&` into `&amp;amp;`. Rewrite only a trailing
                    // `.md` on a relative link, never inside absolute URLs
                    // or query strings.
                    let mut href = url.clone();
                    if !url.contains("://") {
                        if let Some(stripped) = url.strip_suffix(".md") {
                            href = format!("{stripped}.html");
                        }
                    }
                    out.push_str(&format!("<a href=\"{}\">{}</a>", href, label));
                    rest = &rest[len..];
                }
                _ => {
                    out.push('[');
                    rest = &rest[1..];
                }
            }
        }
    }
    out
}

/// Render one section body. Deliberately small: headings, fenced code,
/// bullet lists (indentation becomes nested text), and paragraphs cover the
/// page corpus; anything else degrades to paragraph text rather than
/// breaking the page.
fn md_to_html(body: &str) -> String {
    let mut out = String::new();
    let mut in_fence = false;
    let mut in_list = false;
    let mut para: Vec<String> = Vec::new();
    let flush_para = |out: &mut String, para: &mut Vec<String>| {
        if !para.is_empty() {
            let text = para.join(" ");
            let _ = writeln!(out, "<p>{}</p>", inline_md(&html_escape(&text)));
            para.clear();
        }
    };
    for line in body.lines() {
        if line.trim_start().starts_with("```") {
            flush_para(&mut out, &mut para);
            if in_list {
                out.push_str("</ul>\n");
                in_list = false;
            }
            out.push_str(if in_fence {
                "</code></pre>\n"
            } else {
                "<pre><code>"
            });
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            out.push_str(&html_escape(line));
            out.push('\n');
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            flush_para(&mut out, &mut para);
            if in_list {
                out.push_str("</ul>\n");
                in_list = false;
            }
            continue;
        }
        if let Some(h) = trimmed.strip_prefix("### ") {
            flush_para(&mut out, &mut para);
            if in_list {
                out.push_str("</ul>\n");
                in_list = false;
            }
            let _ = writeln!(out, "<h3>{}</h3>", inline_md(&html_escape(h)));
            continue;
        }
        if trimmed == "---" {
            flush_para(&mut out, &mut para);
            out.push_str("<hr>\n");
            continue;
        }
        if let Some(item) = trimmed.strip_prefix("- ") {
            flush_para(&mut out, &mut para);
            if !in_list {
                out.push_str("<ul>\n");
                in_list = true;
            }
            let _ = writeln!(out, "<li>{}</li>", inline_md(&html_escape(item)));
            continue;
        }
        if in_list && line.starts_with("  ") {
            // Continuation of the previous list item.
            if let Some(pos) = out.rfind("</li>") {
                out.insert_str(pos, &format!(" {}", inline_md(&html_escape(trimmed))));
            }
            continue;
        }
        para.push(trimmed.to_string());
    }
    flush_para(&mut out, &mut para);
    if in_list {
        out.push_str("</ul>\n");
    }
    if in_fence {
        out.push_str("</code></pre>\n");
    }
    out
}

const SITE_STYLE: &str = "body{font-family:system-ui,sans-serif;max-width:70rem;margin:2rem auto;padding:0 1rem;line-height:1.55}table{border-collapse:collapse;width:100%}th,td{border:1px solid #8884;padding:.3rem .5rem;text-align:left}input,select{font:inherit;padding:.4rem;margin:0 .5rem 1rem 0}code{background:#8882;padding:.05rem .25rem;border-radius:.25rem}pre{background:#8881;padding:1rem;overflow:auto}";

fn site_index_html(pages: &[Page], sha: &str) -> String {
    let pages = sorted_pages(pages);
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for p in &pages {
        *counts.entry(strategy(&p.code_facts)).or_default() += 1;
    }
    let mut s = String::new();
    let _ = writeln!(
        s,
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">"
    );
    let _ = writeln!(
        s,
        "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">"
    );
    let _ = writeln!(s, "<title>Per-syscall Detcore pages | Hermit</title>");
    let _ = writeln!(
        s,
        "<link rel=\"stylesheet\" href=\"../site.css\"><style>{SITE_STYLE}</style></head><body>"
    );
    let _ = writeln!(
        s,
        "<header><p><a href=\"../\">Hermetic Infra</a></p><h1>Per-syscall Detcore pages</h1>"
    );
    let _ = writeln!(
        s,
        "<p>The code-derived Detcore strategy for each Linux syscall in this tree's census. Code facts as of source revision <code>{}</code>. The machine-readable table is <a href=\"https://github.com/facebookexperimental/hermit/blob/main/docs/syscalls/syscalls.json\">syscalls.json</a>; test-matrix coverage is a separate measurement in the <a href=\"{COVERAGE_URL}\">syscall coverage audit</a>.</p></header>",
        html_escape(sha)
    );
    let _ = writeln!(s, "<p>");
    for (k, v) in &counts {
        let _ = write!(s, "{}: {} &middot; ", html_escape(k), v);
    }
    let _ = writeln!(s, "total: {}</p>", pages.len());
    let _ = writeln!(
        s,
        "<label>Search <input id=\"q\" type=\"search\" placeholder=\"syscall, strategy, dispatch...\"></label>"
    );
    let _ = writeln!(
        s,
        "<label>Strategy <select id=\"s\"><option value=\"\">all</option>"
    );
    for k in counts.keys() {
        let _ = writeln!(s, "<option>{}</option>", html_escape(k));
    }
    let _ = writeln!(s, "</select></label>");
    let _ = writeln!(
        s,
        "<table><thead><tr><th>syscall</th><th>nr</th><th>strategy</th><th>dispatch</th><th>discussion</th></tr></thead><tbody id=\"rows\">"
    );
    for p in &pages {
        let _ = writeln!(
            s,
            "<tr data-strategy=\"{}\"><td><a href=\"{}.html\">{}</a></td><td>{}</td><td>{}</td><td>{}</td><td><a href=\"{}\">{}</a></td></tr>",
            html_escape(strategy(&p.code_facts)),
            html_escape(&p.syscall),
            html_escape(&p.syscall),
            p.nr_x86_64,
            html_escape(strategy(&p.code_facts)),
            html_escape(&p.code_facts.dispatch),
            html_escape(&p.discussion_url),
            html_escape(&p.discussion_kind),
        );
    }
    let _ = writeln!(s, "</tbody></table>");
    let _ = writeln!(
        s,
        "<script>const q=document.getElementById('q'),s=document.getElementById('s'),rows=[...document.querySelectorAll('#rows tr')];function f(){{const t=q.value.toLowerCase(),v=s.value;for(const r of rows){{r.style.display=(!v||r.dataset.strategy===v)&&r.textContent.toLowerCase().includes(t)?'':'none';}}}}q.addEventListener('input',f);s.addEventListener('change',f);</script>"
    );
    let _ = writeln!(s, "</body></html>");
    s
}

fn site_page_html(page: &Page) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">"
    );
    let _ = writeln!(
        s,
        "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">"
    );
    let _ = writeln!(
        s,
        "<title>{} | Per-syscall Detcore pages</title>",
        html_escape(&page.syscall)
    );
    let _ = writeln!(
        s,
        "<link rel=\"stylesheet\" href=\"../site.css\"><style>{SITE_STYLE}</style></head><body>"
    );
    let _ = writeln!(
        s,
        "<header><p><a href=\"./\">All syscalls</a></p><h1><code>{}</code> ({})</h1>",
        html_escape(&page.syscall),
        page.nr_x86_64
    );
    let _ = writeln!(
        s,
        "<p>Strategy: <strong>{}</strong> &middot; dispatch: <code>{}</code> &middot; <a href=\"{}\">discussion ({})</a></p></header>",
        html_escape(strategy(&page.code_facts)),
        html_escape(&page.code_facts.dispatch),
        html_escape(&page.discussion_url),
        html_escape(&page.discussion_kind),
    );
    for h in SECTIONS {
        let _ = writeln!(s, "<h2>{}</h2>", html_escape(h));
        if let Some(body) = page.sections.get(h) {
            s.push_str(&md_to_html(body));
        }
    }
    let _ = writeln!(s, "</body></html>");
    s
}

/// Every file `render` owns under `docs/site/syscalls/`, as a pure
/// function of the page set. `check` compares this map byte for byte.
fn render_outputs(pages: &[Page], sha: &str) -> BTreeMap<PathBuf, String> {
    let pages = sorted_pages(pages);
    let mut out = BTreeMap::new();
    out.insert(PathBuf::from("index.html"), site_index_html(&pages, sha));
    for p in &pages {
        out.insert(
            PathBuf::from(format!("{}.html", p.syscall)),
            site_page_html(p),
        );
    }
    out
}

fn cmd_render(root: &Path, check_only: bool) -> i32 {
    let loaded = load_pages(root);
    let errors = print_reports(&loaded);
    if errors > 0 {
        eprintln!("syscall-pages: render refuses while lint has {errors} errors");
        return 1;
    }
    let pages = sorted_pages(&good_pages(&loaded));
    let sha = aggregate_sha(&pages);
    let want = render_outputs(&pages, &sha);
    let out = root.join("docs/site/syscalls");
    if check_only {
        let mut bad = 0;
        for (rel, html) in &want {
            let path = out.join(rel);
            if !path.exists() || read(&path) != *html {
                eprintln!(
                    "syscall-pages: docs/site/syscalls/{} is stale; run `scripts/syscall-pages.rs render`",
                    rel.display()
                );
                bad += 1;
            }
        }
        if let Ok(entries) = fs::read_dir(&out) {
            for e in entries.filter_map(|e| e.ok()) {
                if let Ok(name) = e.file_name().into_string() {
                    if name.ends_with(".html") && !want.contains_key(&PathBuf::from(&name)) {
                        eprintln!(
                            "syscall-pages: docs/site/syscalls/{name} is orphaned rendered output"
                        );
                        bad += 1;
                    }
                }
            }
        }
        return if bad > 0 { 1 } else { 0 };
    }
    if fs::create_dir_all(&out).is_err() {
        fail("cannot create docs/site/syscalls");
    }
    for (rel, html) in &want {
        if fs::write(out.join(rel), html).is_err() {
            fail(&format!("cannot write site file {}", rel.display()));
        }
    }
    if let Ok(entries) = fs::read_dir(&out) {
        for e in entries.filter_map(|e| e.ok()) {
            if let Ok(name) = e.file_name().into_string() {
                if name.ends_with(".html") && !want.contains_key(&PathBuf::from(&name)) {
                    let _ = fs::remove_file(e.path());
                }
            }
        }
    }
    println!("render: {} pages -> docs/site/syscalls/", pages.len());
    0
}

fn usage() -> ! {
    eprintln!(
        "usage: syscall-pages.rs <lint|facts [--apply] [--date YYYY-MM-DD]|generate [--facts TSV|--as-of SHA] [--nr-map TSV] [--mapping JSON] [--date YYYY-MM-DD]|aggregate [--check]|check [--quick]|render [--check]>"
    );
    exit(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_page_for(name: &str, nr: i32) -> Page {
        let mut page = Page {
            syscall: name.into(),
            nr_x86_64: nr,
            related: vec![],
            discussion_kind: "new".into(),
            discussion_url: discussion_url(name),
            code_facts: CodeFacts {
                as_of: "0123456789abcdef0123456789abcdef01234567".into(),
                detcore_class: "determinized".into(),
                dispatch: "typed".into(),
                refused: false,
                refused_only_when_fail_closed: false,
                subscribed_run: true,
                subscribed_record_replay: true,
            },
            sections: BTreeMap::new(),
        };
        page.sections
            .insert("Strategy".into(), strategy_body(&page, 11, Some(22)));
        page.sections.insert(
            "Discussion".into(),
            format!(
                "\n[Start a discussion]({}) (prefilled form).\n",
                page.discussion_url
            ),
        );
        page.sections.insert(
            "Status".into(),
            "\n- 2026-11-15 · rev 0123456789ab · facts-only page generated from Detcore code facts at this revision.\n".into(),
        );
        page
    }

    fn sample_page() -> String {
        emit_page(&sample_page_for("read", 0))
    }

    #[test]
    fn emitted_page_lints_clean_and_round_trips() {
        let text = sample_page();
        let (rep, page) = lint_page("read", &text, Path::new("."));
        assert!(rep.errors.is_empty(), "errors: {:?}", rep.errors);
        let page = page.expect("page parses");
        assert_eq!(page.syscall, "read");
        assert_eq!(strategy(&page.code_facts), "determinized");
    }

    #[test]
    fn lint_rejects_internal_hostname_unknown_and_duplicate_keys() {
        let bad = sample_page().replace(
            "facts-only page generated",
            "Measured on devbig030 today; facts-only page generated",
        );
        let (rep, _) = lint_page("read", &bad, Path::new("."));
        assert!(
            rep.errors.iter().any(|e| e.starts_with("P30")),
            "{:?}",
            rep.errors
        );
        let bad = sample_page().replace(
            "nr_x86_64: 0",
            "nr_x86_64: 0\nptrace_verification: untested",
        );
        let (rep, _) = lint_page("read", &bad, Path::new("."));
        assert!(
            rep.errors.iter().any(|e| e.starts_with("P05")),
            "{:?}",
            rep.errors
        );
        let bad = sample_page().replace("nr_x86_64: 0", "nr_x86_64: 0\nnr_x86_64: 1");
        let (rep, _) = lint_page("read", &bad, Path::new("."));
        assert!(
            rep.errors.iter().any(|e| e.starts_with("P01")),
            "duplicate key must fail: {:?}",
            rep.errors
        );
    }

    #[test]
    fn lint_rejects_each_public_text_leak_class() {
        // One red twin per P30 class the seat enumerated.
        for leak in [
            "T78538674",
            "P123456789",
            "S123456789",
            "dh-86rp6",
            "tbh-1005a",
            "orc-coord-014",
            "syscall-tracking-p3",
            "hermit2",
            "gpt-5.6-sol",
            "claude-3-5-sonnet",
            "host.example.facebook.com",
            "host.internalfb.com",
            "host.fb.com",
            "ai_docs/plans/x.md",
            "ignored/syscall-tracking/x.md",
            "dev-hermit/experiments/x",
            "worktrees/slots/lane",
            "/home/someone/file",
            "Author: Someone",
        ] {
            assert!(
                !scrub_findings(&format!("evidence {leak} end")).is_empty(),
                "leak class not refused: {leak}"
            );
        }
        assert!(scrub_findings("model policy text about a model").is_empty());
    }

    #[test]
    fn lint_rejects_bad_status_discussion_and_dropped_key() {
        let bad = sample_page().replace("2026-11-15", "15 Nov 2026");
        let (rep, _) = lint_page("read", &bad, Path::new("."));
        assert!(
            rep.errors.iter().any(|e| e.starts_with("P12")),
            "{:?}",
            rep.errors
        );
        let bad = sample_page().replace(
            "This facts-only page was generated",
            "`ptrace_verification` note. This facts-only page was generated",
        );
        let (rep, _) = lint_page("read", &bad, Path::new("."));
        assert!(
            rep.errors.iter().any(|e| e.starts_with("P31")),
            "P31 must bite outside Status: {:?}",
            rep.errors
        );
        let bad = sample_page().replace(
            "https://github.com/facebookexperimental/hermit/issues/new?",
            "https://example.com/issues/new?",
        );
        let (rep, _) = lint_page("read", &bad, Path::new("."));
        assert!(
            rep.errors.iter().any(|e| e.starts_with("P34")),
            "P34 must bite: {:?}",
            rep.errors
        );
    }

    #[test]
    fn aggregate_and_render_are_order_independent_and_repeatable() {
        // nr order differs from name order (access 21 after read 0), so a
        // sort-by-name or descending regression fails this test too.
        let pages = vec![sample_page_for("access", 21), sample_page_for("read", 0)];
        let json = syscalls_json(&pages, "0123456789abcdef0123456789abcdef01234567");
        assert!(
            json.find("\"read\"").unwrap() < json.find("\"access\"").unwrap(),
            "rows must be nr-ordered: {json}"
        );
        let mut shuffled = pages.clone();
        shuffled.reverse();
        let sha = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(syscalls_json(&pages, sha), syscalls_json(&shuffled, sha));
        assert_eq!(index_md(&pages, sha), index_md(&shuffled, sha));
        // Standing determinism check: build the render twice, compare.
        assert_eq!(render_outputs(&pages, sha), render_outputs(&shuffled, sha));
        assert_eq!(render_outputs(&pages, sha), render_outputs(&pages, sha));
    }

    #[test]
    fn passthru_lists_include_arch_gated_second_list() {
        // Red twin for the access finding: the old reader stopped at the
        // first list and never saw the x86_64-only one.
        let lib = "let mut subscription = Subscription::none();\nsubscription.syscalls([Sysno::read]);\n#[cfg(target_arch = \"x86_64\")]\nsubscription.syscalls([\n    Sysno::access,\n]);\n#[cfg(target_arch = \"aarch64\")]\nsubscription.syscalls([Sysno::fstatat]);\nsubscription.syscalls(crate::all_pinned_syscalls().filter(|_| true));\n";
        let names = passthru_explicit_names(lib).expect("lists parse");
        assert!(names.contains("read"), "{names:?}");
        assert!(
            names.contains("access"),
            "second x86_64 list missed: {names:?}"
        );
        assert!(!names.contains("fstatat"), "aarch64 list leaked: {names:?}");
        assert!(x86_cfg_subscription_names(lib).contains("access"));
    }

    #[test]
    fn census_parse_counts_and_classes() {
        let src = "#[cfg(target_arch = \"x86_64\")]\nconst EXPECTED_SYSNO_COUNT: usize = 3;\nconst _: () = { assert!(Sysno::last().id() == 469); };\nconst DETERMINIZED_SYSCALLS: &[&str] = &[\n    \"read\",\n    #[cfg(target_arch = \"aarch64\")]\n    \"io_pgetevents\",\n];\nconst PASSTHROUGH_SYSCALLS: &[&str] = &[\"mkdir\"];\nconst UNSUPPORTED_SYSCALLS: &[&str] = &[\"restart_syscall\"];\n";
        let census = parse_census(src).expect("census parses");
        assert_eq!(census.expected_count, 3);
        assert_eq!(census.last_id, 469);
        assert_eq!(census.classes["read"].0, "determinized");
        assert!(!census.classes.contains_key("io_pgetevents"));
    }

    #[test]
    fn site_outputs_cover_search_filter_and_discussion() {
        let pages = vec![sample_page_for("read", 0)];
        let out = render_outputs(&pages, "0123456789abcdef0123456789abcdef01234567");
        let index = &out[&PathBuf::from("index.html")];
        assert!(index.contains("<input id=\"q\" type=\"search\""), "{index}");
        assert!(index.contains("<select id=\"s\">"), "{index}");
        assert!(index.contains("addEventListener('input',f)"), "{index}");
        assert!(index.contains("addEventListener('change',f)"), "{index}");
        let page = &out[&PathBuf::from("read.html")];
        assert!(page.contains("discussion (new)</a>"), "{page}");
        assert!(page.contains("&amp;body="), "{page}");
        assert!(!page.contains("&amp;amp;"), "double-escaped: {page}");
        assert!(
            page.contains("docs%2Fsyscalls%2Fread.md"),
            "encoded .md must survive: {page}"
        );
    }

    #[test]
    fn dispatch_parse_assigns_each_arm_kind_and_fails_closed() {
        let lib = r#"
            let res = match classify_syscall(call.number()) {
                _ if something.is_some() => { todo!() }
                SyscallClassification::Determinized if call.number() == Sysno::rseq => { todo!() }
                SyscallClassification::Determinized if is_refused_family(call.number()) => { todo!() }
                SyscallClassification::Determinized => match call {
                    Syscall::Read(s) => { todo!() }
                    unexpected => { todo!() }
                },
                SyscallClassification::PassThrough => { todo!() }
                SyscallClassification::Unsupported => { todo!() }
            };
        "#;
        let preds = "pub(crate) const fn is_refused_family(sysno: Sysno) -> bool { matches!(sysno, Sysno::futex_waitv) }";
        let classes: BTreeMap<String, String> = [
            ("read".to_string(), "determinized".to_string()),
            ("rseq".to_string(), "determinized".to_string()),
            ("futex_waitv".to_string(), "determinized".to_string()),
            ("mkdir".to_string(), "passthrough".to_string()),
            ("restart_syscall".to_string(), "unsupported".to_string()),
        ]
        .into_iter()
        .collect();
        let rep = parse_dispatch(lib, &[("preds", preds)], &classes);
        assert_eq!(rep.status, "ok", "errors: {:?}", rep.errors);
        assert_eq!(rep.dispatch["read"], "typed");
        assert_eq!(rep.dispatch["rseq"], "sysno-guard");
        assert_eq!(rep.dispatch["futex_waitv"], "pred:is_refused_family");
        assert_eq!(rep.dispatch["mkdir"], "passthrough");
        let broken = lib.replace(
            "match classify_syscall(call.number()) {",
            "match something_else {",
        );
        let rep = parse_dispatch(&broken, &[("preds", preds)], &classes);
        assert_eq!(rep.status, "parser-failed");
        assert!(rep.dispatch.values().all(|v| v == "parser-failed"));
    }

    #[test]
    fn render_escapes_and_links() {
        let html = md_to_html(
            "A <b>raw</b> `code` and **bold** [page](read.md) [ext](https://example.com/a.md?x=1&y=2).\n\n- F1: item\n  continuation\n",
        );
        assert!(html.contains("&lt;b&gt;raw&lt;/b&gt;"), "{html}");
        assert!(html.contains("<code>code</code>"), "{html}");
        assert!(html.contains("<strong>bold</strong>"), "{html}");
        assert!(html.contains("href=\"read.html\""), "{html}");
        assert!(
            html.contains("href=\"https://example.com/a.md?x=1&amp;y=2\""),
            "absolute .md and single-escaped &: {html}"
        );
        assert!(!html.contains("&amp;amp;"), "{html}");
        assert!(html.contains("continuation</li>"), "{html}");
    }

    #[test]
    fn facts_tsv_and_strategy_round_trip() {
        let (sha, rows) = parse_facts_tsv("# hermit abc\nname\tnr\tclass\trefused\tstrict_only_refused\tsub_run\tsub_record_replay\nread\t0\tdeterminized\t0\t0\t1\t1\n").unwrap();
        assert_eq!(sha, "abc");
        assert_eq!(rows["read"].nr, 0);
        assert_eq!(today_utc().len(), 10);
    }
}

fn main() {
    rust_script_prelude::init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = repo_root();
    let rc = match args.first().map(|s| s.as_str()) {
        Some("lint") => cmd_lint(&root),
        Some("facts") => cmd_facts(&root, &args[1..]),
        Some("aggregate") => cmd_aggregate(&root, args.iter().any(|a| a == "--check")),
        Some("check") => cmd_check(&root, args.iter().any(|a| a == "--quick")),
        Some("generate") => cmd_generate(&root, &args[1..]),
        Some("render") => cmd_render(&root, args.iter().any(|a| a == "--check")),
        _ => usage(),
    };
    exit(rc);
}
