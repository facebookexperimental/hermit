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
//! check, report, and wave.
//!
//! The pages in `docs/syscalls/<name>.md` record the code-derived
//! Detcore strategy for each syscall in this tree's Detcore census.
//! A page may also carry checked determinism-strategy prose whose
//! claims are pinned to Detcore code by hash-anchored citations
//! (schema v3; v2 facts-only pages remain valid). Which backends can
//! run a syscall is not tracked here. Test coverage is likewise
//! separate: generated indexes link the coverage audit and never merge
//! its numbers into the strategy table. The normative page format is
//! `docs/syscalls/SCHEMA.md`.
//!
//! Subcommands:
//! - `lint` — schema, sections, checked-prose rules, Status format,
//!   public-text rules, and discussion-link shape. No build, runs in
//!   seconds.
//! - `generate` — (re)generate every page's code facts from this tree's
//!   Detcore sources, optionally overlaying an exporter TSV for exact
//!   numbers and refused/subscription facts; checked prose, discussion
//!   links, and Status logs are preserved.
//! - `facts [--apply]` — export per-syscall policy from Detcore itself
//!   (public API only, through `scripts/syscall-pages-facts.rs`) plus the
//!   dispatch parse. Needs a git checkout with the `reverie/` submodule.
//! - `aggregate [--check]`, `render [--check]` — regenerate or verify
//!   the JSON table, index, and static site, byte for byte.
//! - `check [--quick|--committed]` — lint + derived census + classes +
//!   dispatch + citations (including prose citation hashes) +
//!   generated-output freshness and public-text scan, all from this
//!   tree's sources; full `check` also compares exporter facts (the
//!   git-only part, named as such in SCHEMA.md). `--committed` is the
//!   hard CI tier: lint and freshness only, deriving nothing from
//!   live Detcore, so unrelated Detcore changes cannot fail it.
//! - `anchor` — rewrite the committed citation snapshot from live
//!   Detcore (after a facts refresh; the hard gate reads it).
//! - `report` — per-page prose status for reviewers (claim counts,
//!   negative claims, triage, stamp lag); never gates.
//! - `wave --from A --to B` — update-wave worklist over two revisions
//!   of a git checkout at B: facts-changed, handler-touched, and
//!   citation-stale syscalls. Reports only.
//!
//! What the census or dispatch parse cannot understand is
//! `parser-failed`, never a pass. What `lint` cannot parse is an error,
//! never a skip.
//!
//! Not ported from the predecessor system: its C08 rule resolving a
//! recorded coverage token to a strict-container test. Checked prose
//! records named tests as evidence (existence-checked), and SCHEMA.md
//! states that no page claim is machine-verified coverage until that
//! check exists.

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
const SCHEMA_V3: &str = "syscall-pages/v3";
/// The checked-prose section (v3 pages with a `prose` block only).
const PROSE_SECTION: &str = "Determinism strategy";
/// Rubric part lead-ins, in order (P37). The exact strings open each
/// part of the Determinism strategy section.
const RUBRIC_PARTS: [&str; 6] = [
    "**What is nondeterministic natively.**",
    "**What Hermit does.**",
    "**Interface fidelity.**",
    "**Record & replay.**",
    "**Known gaps.**",
    "**Tests.**",
];
/// Legal `passthrough_risk` values (P36); `none` selects the short form.
const PASSTHROUGH_RISKS: [&str; 4] = ["none", "host-state", "identity", "timing"];
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
/// Page sections, in order, come from `section_order`: the generated
/// strategy, optional checked prose, discussion link, and append-only
/// status log.
const DETCORE_CLASS: [&str; 3] = ["determinized", "passthrough", "unsupported"];
/// GitHub Discussions category used by newly generated discussion
/// links. The links work only once Discussions and this category are
/// enabled on the public repository; generation never contacts GitHub.
const DISCUSSION_CATEGORY: &str = "syscalls";

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

/// Checked-prose stamps (schema v3). Human/agent-authored, in contrast
/// to tool-owned `CodeFacts`: they move only when the Determinism
/// strategy prose is re-verified against Detcore.
#[derive(Debug, Clone, PartialEq)]
struct Prose {
    status: String,
    reviewed_at: String,
    based_on: String,
    passthrough_risk: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
struct Page {
    schema: String,
    syscall: String,
    nr_x86_64: i32,
    related: Vec<String>,
    discussion_kind: String,
    discussion_url: String,
    code_facts: CodeFacts,
    prose: Option<Prose>,
    /// Section heading -> body text (without the heading line).
    sections: BTreeMap<String, String>,
}

/// Level-2 headings a page carries, in order, by prose state (P02).
/// A prose page reads summary-first: In short, the determinism prose,
/// its gaps, the discussion link, then the evidence appendix (code
/// facts, tests, citations) and the status log. Internally the prose
/// is still modelled as one normalized Determinism strategy text plus
/// a Strategy (code-facts) body; `display_sections` performs the split.
fn section_order(has_prose: bool) -> Vec<&'static str> {
    if has_prose {
        vec![
            "In short",
            PROSE_SECTION,
            "Known gaps",
            "Discussion",
            "Appendix",
            "Status",
        ]
    } else {
        vec!["Strategy", "Discussion", "Status"]
    }
}

/// The generated final sentence of `## In short` (P45): a pure
/// function of the page's gap bullets, so the summary cannot state a
/// count the gaps section does not have.
fn in_short_gaps_sentence(gaps: usize) -> String {
    format!("Known gaps: {gaps}.")
}

/// Count of `- G…` bullets in the normalized Known gaps part.
fn known_gap_count(normalized_ds: &str) -> usize {
    let Some(at) = normalized_ds.find(RUBRIC_PARTS[4]) else {
        return 0;
    };
    let tail = &normalized_ds[at..];
    let end = tail.find(RUBRIC_PARTS[5]).unwrap_or(tail.len());
    tail[..end]
        .lines()
        .filter(|l| l.trim_start().starts_with("- G"))
        .count()
}

/// Split the normalized Determinism strategy text into its display
/// pieces: (parts 1-4 body, gaps body, tests body, citations text).
/// Missing markers yield empty pieces; lint reports the shape errors.
fn decompose_prose(ds: &str) -> (String, String, String, String) {
    let gaps_at = ds.find(RUBRIC_PARTS[4]).unwrap_or(ds.len());
    let tests_at = ds.find(RUBRIC_PARTS[5]).unwrap_or(ds.len());
    let main = ds[..gaps_at].trim().to_string();
    let gaps = if gaps_at < tests_at {
        ds[gaps_at + RUBRIC_PARTS[4].len()..tests_at]
            .trim()
            .to_string()
    } else {
        String::new()
    };
    let rest = if tests_at < ds.len() {
        &ds[tests_at + RUBRIC_PARTS[5].len()..]
    } else {
        ""
    };
    // Citations start at the first footnote-definition line.
    let mut defs_at = rest.len();
    let mut offset = 0usize;
    for line in rest.lines() {
        if line.starts_with("[^") && line.contains("]:") {
            defs_at = offset;
            break;
        }
        offset += line.len() + 1;
    }
    let tests = rest[..defs_at].trim().to_string();
    let defs = rest[defs_at.min(rest.len())..].trim().to_string();
    (main, gaps, tests, defs)
}

/// Sections as displayed (and emitted), in order. Prose pages split
/// the normalized model into the reading order; facts-only pages keep
/// the v2 layout.
fn display_sections(page: &Page) -> Vec<(&'static str, String)> {
    let get = |h: &str| page.sections.get(h).cloned().unwrap_or_default();
    if page.prose.is_none() {
        return ["Strategy", "Discussion", "Status"]
            .into_iter()
            .filter(|h| page.sections.contains_key(*h))
            .map(|h| (h, get(h)))
            .collect();
    }
    let (main, gaps, tests, defs) = decompose_prose(&get(PROSE_SECTION));
    let appendix = format!(
        "### Code facts\n\n{}\n\n### Tests\n\n{}\n\n### Citations\n\n{}",
        get("Strategy").trim(),
        tests,
        defs
    );
    vec![
        ("In short", get("In short")),
        (PROSE_SECTION, format!("\n{main}\n")),
        ("Known gaps", format!("\n{gaps}\n")),
        ("Discussion", get("Discussion")),
        ("Appendix", format!("\n{appendix}\n")),
        ("Status", get("Status")),
    ]
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
    /// Whether a `prose:` block header appeared (even an empty one).
    prose_present: bool,
}

fn parse_front_matter(fm: &str) -> Result<FrontMatter, String> {
    let top_keys = [
        "schema",
        "syscall",
        "nr_x86_64",
        "related",
        "discussion",
        "code_facts",
        "prose",
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
    let prose_keys = ["status", "reviewed_at", "based_on", "passthrough_risk"];
    let mut out = FrontMatter {
        scalars: BTreeMap::new(),
        related: Vec::new(),
        unknown: Vec::new(),
        prose_present: false,
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
            if key == "discussion" || key == "code_facts" || key == "prose" {
                if !value.is_empty() {
                    return Err(format!("`{key}` must be a nested block"));
                }
                if key == "prose" {
                    out.prose_present = true;
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
                "prose" => prose_keys.contains(&key),
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
    let schema = get("", "schema", rep);
    if schema != SCHEMA && schema != SCHEMA_V3 {
        rep.err(
            "P05",
            format!("`schema` must be {SCHEMA} or {SCHEMA_V3}, got {schema:?}"),
        );
    }
    let prose = if fm.prose_present {
        Some(Prose {
            status: get("prose", "status", rep),
            reviewed_at: get("prose", "reviewed_at", rep),
            based_on: get("prose", "based_on", rep),
            passthrough_risk: fm
                .scalars
                .get(&("prose".to_string(), "passthrough_risk".to_string()))
                .cloned(),
        })
    } else {
        None
    };
    let b = |block: &str, key: &str, rep: &mut Report| -> bool {
        match parse_bool(&get(block, key, rep)) {
            Ok(v) => v,
            Err(e) => {
                rep.err("P05", e);
                false
            }
        }
    };
    let mut page = Page {
        schema,
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
        prose,
        sections: split_sections(body),
    };
    // Reading-order layout normalization: fold the Appendix and the
    // Known gaps section back into the internal model (Strategy body
    // plus one normalized Determinism strategy text) that every
    // check below works on. `display_sections` performs the inverse.
    if page.sections.contains_key("Appendix") {
        let appendix = page.sections.remove("Appendix").unwrap_or_default();
        let mut subs: BTreeMap<String, String> = BTreeMap::new();
        let mut cur: Option<String> = None;
        let mut buf = String::new();
        let mut preamble = String::new();
        for line in appendix.lines() {
            if let Some(h) = line.strip_prefix("### ") {
                if let Some(prev) = cur.take() {
                    if subs
                        .insert(prev.clone(), std::mem::take(&mut buf))
                        .is_some()
                    {
                        rep.err("P02", format!("duplicate Appendix subsection `### {prev}`"));
                    }
                }
                cur = Some(h.trim().to_string());
                continue;
            }
            if cur.is_some() {
                buf.push_str(line);
                buf.push('\n');
            } else {
                preamble.push_str(line);
                preamble.push('\n');
            }
        }
        if let Some(prev) = cur {
            if subs.insert(prev.clone(), buf).is_some() {
                rep.err("P02", format!("duplicate Appendix subsection `### {prev}`"));
            }
        }
        // Nothing in the Appendix may sit outside the three known
        // subsections: such text would be published in the page
        // without ever reaching a lint rule.
        if !preamble.trim().is_empty() {
            rep.err(
                "P02",
                "text in `## Appendix` before its first `###` subsection is not allowed",
            );
        }
        for name in subs.keys() {
            if !["Code facts", "Tests", "Citations"].contains(&name.as_str()) {
                rep.err("P02", format!("unknown Appendix subsection `### {name}`"));
            }
        }
        if let Some(facts) = subs.remove("Code facts") {
            page.sections.insert("Strategy".into(), facts);
        }
        let gaps = page.sections.remove("Known gaps").unwrap_or_default();
        let tests = subs.remove("Tests").unwrap_or_default();
        let defs = subs.remove("Citations").unwrap_or_default();
        // Never synthesize a missing Determinism strategy section:
        // its absence is the P36 coupling error's job to report.
        if let Some(main) = page.sections.get(PROSE_SECTION).cloned() {
            let normalized = format!(
                "{}\n\n{}\n{}\n\n{}\n{}\n\n{}",
                main.trim(),
                RUBRIC_PARTS[4],
                gaps.trim(),
                RUBRIC_PARTS[5],
                tests.trim(),
                defs.trim()
            );
            page.sections
                .insert(PROSE_SECTION.into(), format!("\n{normalized}\n"));
        }
    }
    Some(page)
}

fn bool_str(v: bool) -> &'static str {
    if v { "true" } else { "false" }
}

fn emit_page(page: &Page) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "---");
    let _ = writeln!(s, "schema: {}", page.schema);
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
    if let Some(p) = &page.prose {
        let _ = writeln!(
            s,
            "# prose: human/agent-authored, machine-checked (syscall-pages.rs check); v3 only"
        );
        let _ = writeln!(s, "prose:");
        let _ = writeln!(s, "  status: {}", p.status);
        let _ = writeln!(s, "  reviewed_at: {}", p.reviewed_at);
        let _ = writeln!(s, "  based_on: {}", p.based_on);
        if let Some(risk) = &p.passthrough_risk {
            let _ = writeln!(s, "  passthrough_risk: {risk}");
        }
    }
    let _ = writeln!(s, "---");
    for (h, body) in display_sections(page) {
        let _ = writeln!(s, "\n## {h}\n");
        s.push_str(body.trim());
        s.push('\n');
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
    if page.prose.is_none() {
        s.push_str(
            "\nThis facts-only page was generated from Detcore code facts. Checked analysis prose has not been added yet.\n",
        );
    }
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
// Checked prose (schema v3, P36-P46). Claims in the Determinism
// strategy section are pinned to Detcore code by citation footnotes
// whose `sha256:` anchors this tool recomputes in the tree being
// checked (P38), held to the page's own code facts (P40-P42), and kept
// free of hand-typed numbers (P39). The SHA-256 below is implemented
// in-file so the tool needs no dependency beyond serde_json in any
// tree it runs in.
// ---------------------------------------------------------------------------

fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg: Vec<u8> = Vec::with_capacity(data.len() + 72);
    msg.extend_from_slice(data);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    let mut w = [0u32; 64];
    for chunk in msg.chunks_exact(64) {
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(v);
        }
    }
    let mut out = [0u8; 32];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

/// First 16 lowercase hex digits of the SHA-256 of `data` — the
/// citation anchor form (P38).
fn sha256_hex16(data: &[u8]) -> String {
    let digest = sha256(data);
    let mut s = String::with_capacity(16);
    for b in digest.iter().take(8) {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Bytes of lines `first..=last` exactly as stored (each line with its
/// trailing newline), or None when the range leaves the file.
fn lines_bytes(content: &[u8], first: usize, last: usize) -> Option<Vec<u8>> {
    if first == 0 || last < first {
        return None;
    }
    let lines: Vec<&[u8]> = content.split_inclusive(|b| *b == b'\n').collect();
    if last > lines.len() {
        return None;
    }
    let mut out = Vec::new();
    for line in &lines[first - 1..last] {
        out.extend_from_slice(line);
    }
    Some(out)
}

/// One parsed code citation: `path:first-last` with its claimed anchor.
#[derive(Debug, Clone, PartialEq)]
struct Cite {
    path: String,
    first: usize,
    last: usize,
    hash: String,
}

/// The committed citation snapshot (`docs/syscalls/citations.json`):
/// what the hard gate checks pages against, so it reads nothing
/// under detcore/. Written by `anchor` from live Detcore after a
/// facts refresh; one entry per prose page.
const CITATIONS_FILE: &str = "docs/syscalls/citations.json";

#[derive(Debug, Clone, Default, PartialEq)]
struct SnapshotEntry {
    based_on: String,
    citations: Vec<(String, Cite)>,
    tests: Vec<(String, Vec<String>)>,
    cited_errnos: BTreeSet<String>,
}

/// (path, test names) pairs from a Tests part, in order.
fn tests_entries(tests: &str) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for (i, span) in tests.split('`').enumerate() {
        if i % 2 == 0 {
            continue;
        }
        let token = span.trim();
        if token.ends_with(".rs") {
            out.push((token.to_string(), Vec::new()));
            continue;
        }
        let is_fn_name = token.len() > 3
            && token.contains('_')
            && token
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if !is_fn_name {
            continue;
        }
        if let Some((_, names)) = out.last_mut() {
            names.push(token.to_string());
        }
    }
    out
}

/// The canonical snapshot document for a page set, verifying every
/// citation against live Detcore on the way (anchor semantics).
fn snapshot_document(pages: &[Page], root: &Path) -> Result<String, String> {
    let mut page_objs = serde_json::Map::new();
    for page in pages {
        if page.prose.is_none() {
            continue;
        }
        let mut cites_json = Vec::new();
        let mut errnos: BTreeSet<String> = BTreeSet::new();
        for (id, cite) in page_cites(page) {
            let text =
                verify_cite(root, &cite).map_err(|e| format!("{}: [^{id}] {e}", page.syscall))?;
            errnos.extend(errno_tokens(&text));
            cites_json.push(serde_json::json!({
                "id": id, "path": cite.path, "first": cite.first,
                "last": cite.last, "hash": cite.hash,
            }));
        }
        let ds = page
            .sections
            .get(PROSE_SECTION)
            .cloned()
            .unwrap_or_default();
        let ranges = rubric_part_ranges(&split_footnotes(&ds).1);
        let tests_text = match ranges[5] {
            Some((s, e)) => split_footnotes(&ds).1[s..e].to_string(),
            None => String::new(),
        };
        let tests_json: Vec<serde_json::Value> = tests_entries(&tests_text)
            .into_iter()
            .map(|(path, names)| serde_json::json!({"path": path, "names": names}))
            .collect();
        page_objs.insert(
            page.syscall.clone(),
            serde_json::json!({
                "based_on": page.prose.as_ref().map(|p| p.based_on.clone()),
                "citations": cites_json,
                "tests": tests_json,
                "cited_errnos": errnos.iter().collect::<Vec<_>>(),
            }),
        );
    }
    let doc = serde_json::json!({
        "schema": "citations.json/v1",
        "generated_by": "scripts/syscall-pages.rs anchor",
        "pages": page_objs,
    });
    Ok(serde_json::to_string_pretty(&doc).unwrap() + "\n")
}

/// Parse the committed snapshot, if present.
fn load_snapshot(root: &Path) -> Option<BTreeMap<String, SnapshotEntry>> {
    let text = fs::read_to_string(root.join(CITATIONS_FILE)).ok()?;
    let doc: serde_json::Value = serde_json::from_str(&text).ok()?;
    let mut out = BTreeMap::new();
    for (name, entry) in doc.get("pages")?.as_object()? {
        let mut snap = SnapshotEntry {
            based_on: entry.get("based_on")?.as_str()?.to_string(),
            ..SnapshotEntry::default()
        };
        for c in entry.get("citations")?.as_array()? {
            snap.citations.push((
                c.get("id")?.as_str()?.to_string(),
                Cite {
                    path: c.get("path")?.as_str()?.to_string(),
                    first: c.get("first")?.as_u64()? as usize,
                    last: c.get("last")?.as_u64()? as usize,
                    hash: c.get("hash")?.as_str()?.to_string(),
                },
            ));
        }
        for t in entry.get("tests")?.as_array()? {
            let names = t
                .get("names")?
                .as_array()?
                .iter()
                .filter_map(|n| n.as_str().map(|s| s.to_string()))
                .collect();
            snap.tests
                .push((t.get("path")?.as_str()?.to_string(), names));
        }
        for e in entry.get("cited_errnos")?.as_array()? {
            if let Some(e) = e.as_str() {
                snap.cited_errnos.insert(e.to_string());
            }
        }
        out.insert(name.clone(), snap);
    }
    Some(out)
}

/// Exact (`--quick` and full) check's snapshot honesty: rebuild the
/// snapshot from live Detcore and compare it byte for byte with the
/// committed file. This catches hand edits the per-page agreement
/// cannot see (an added `cited_errnos` entry, a malformed document,
/// a stale `based_on`). A missing snapshot is a finding only when a
/// prose page exists; a prose-free tree needs none. Committed mode
/// never calls this: it would read live Detcore.
fn exact_snapshot_finding(pages: &[Page], root: &Path) -> Option<String> {
    let path = root.join(CITATIONS_FILE);
    let got = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(_) => {
            if pages.iter().any(|p| p.prose.is_some()) {
                return Some(format!(
                    "{CITATIONS_FILE} is missing; run `scripts/syscall-pages.rs anchor`"
                ));
            }
            return None;
        }
    };
    match snapshot_document(pages, root) {
        Ok(want) if got == want.as_bytes() => None,
        Ok(_) => Some(format!(
            "{CITATIONS_FILE} disagrees byte for byte with the snapshot rebuilt from live Detcore; run `scripts/syscall-pages.rs anchor` after refreshing facts"
        )),
        Err(e) => Some(format!(
            "cannot rebuild {CITATIONS_FILE} from live Detcore: {e}"
        )),
    }
}

/// The hard tier's cross-page snapshot shape: every snapshot entry
/// must belong to a page that exists and carries prose, and its
/// `based_on` must equal that page's `prose.based_on`. Per-page lint
/// cannot see an entry for a missing or prose-less page.
fn committed_snapshot_findings(pages: &[Page], root: &Path) -> Vec<String> {
    let Some(snapshot) = load_snapshot(root) else {
        if pages.iter().any(|p| p.prose.is_some()) {
            return vec![format!(
                "{CITATIONS_FILE} is missing or malformed; run `scripts/syscall-pages.rs anchor`"
            )];
        }
        return Vec::new();
    };
    let mut out = Vec::new();
    for (name, entry) in &snapshot {
        match pages
            .iter()
            .find(|p| &p.syscall == name)
            .and_then(|p| p.prose.as_ref())
        {
            None => out.push(format!(
                "{CITATIONS_FILE} has an entry for {name}, which is not a prose page; run `scripts/syscall-pages.rs anchor`"
            )),
            Some(prose) if entry.based_on != prose.based_on => out.push(format!(
                "{CITATIONS_FILE} entry for {name} has based_on {}, but the page's prose.based_on is {}; run `scripts/syscall-pages.rs anchor`",
                entry.based_on, prose.based_on
            )),
            Some(_) => {}
        }
    }
    out
}

/// Split a Determinism strategy section into (footnote definitions by
/// id, body text with definitions removed). A definition starts at a
/// `[^id]:` line and continues through following indented lines.
fn split_footnotes(section: &str) -> (BTreeMap<String, String>, String) {
    let mut defs: BTreeMap<String, String> = BTreeMap::new();
    let mut body = String::new();
    let mut in_def: Option<String> = None;
    for line in section.lines() {
        let def_id = line
            .strip_prefix("[^")
            .and_then(|rest| rest.split_once("]:"))
            .map(|(id, _)| id.to_string())
            .filter(|id| !id.is_empty() && !id.contains(char::is_whitespace));
        if let Some(id) = def_id {
            let after = line.split_once("]:").map(|x| x.1).unwrap_or("");
            defs.insert(id.clone(), after.trim().to_string());
            in_def = Some(id);
            continue;
        }
        if let Some(id) = &in_def {
            if line.starts_with("  ") && !line.trim().is_empty() {
                let entry = defs.get_mut(id).expect("def just inserted");
                entry.push(' ');
                entry.push_str(line.trim());
                continue;
            }
            in_def = None;
        }
        body.push_str(line);
        body.push('\n');
    }
    (defs, body)
}

/// Footnote references `[^id]` in body text (definitions removed).
fn footnote_refs(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(at) = rest.find("[^") {
        let after = &rest[at + 2..];
        match after.find(']') {
            Some(end) => {
                out.push(after[..end].to_string());
                rest = &after[end + 1..];
            }
            None => break,
        }
    }
    out
}

/// Code citations inside one footnote definition: every
/// `<path>:<a>[-<b>] sha256:<h>` pair, in order.
fn cite_triples(def: &str) -> Vec<Cite> {
    let tokens: Vec<&str> = def
        .split(|c: char| c.is_whitespace() || c == ';')
        .filter(|t| !t.is_empty())
        .collect();
    let mut out = Vec::new();
    for w in tokens.windows(2) {
        let Some(hash) = w[1].strip_prefix("sha256:") else {
            continue;
        };
        let Some((path, range)) = w[0].rsplit_once(':') else {
            continue;
        };
        let (first, last) = match range.split_once('-') {
            Some((a, b)) => match (a.parse::<usize>(), b.parse::<usize>()) {
                (Ok(a), Ok(b)) => (a, b),
                _ => continue,
            },
            None => match range.parse::<usize>() {
                Ok(a) => (a, a),
                Err(_) => continue,
            },
        };
        out.push(Cite {
            path: path.to_string(),
            first,
            last,
            hash: hash.trim_end_matches(',').to_string(),
        });
    }
    out
}

/// GitHub source links inside one footnote definition (P46): every
/// `.../hermit/blob/<rev>/<path>#L<a>[-L<b>]` token, as
/// `(path, first, last)`. The link is a convenience copy of a
/// citation's range; it must repeat that range exactly, so a range
/// moved by `anchor` cannot leave a stale human-facing anchor.
fn github_source_links(def: &str) -> Vec<(String, usize, usize)> {
    const PREFIX: &str = "https://github.com/facebookexperimental/hermit/blob/";
    let mut out = Vec::new();
    for token in def.split_whitespace() {
        let token = token.trim_end_matches(|c| ",.;:)]".contains(c));
        let Some(after_blob) = token.strip_prefix(PREFIX) else {
            continue;
        };
        // Drop the revision component (`main` or a commit).
        let Some((_rev, path_and_anchor)) = after_blob.split_once('/') else {
            continue;
        };
        let Some((path, anchor)) = path_and_anchor.split_once("#L") else {
            continue;
        };
        let (first, last) = match anchor.split_once("-L") {
            Some((a, b)) => match (a.parse::<usize>(), b.parse::<usize>()) {
                (Ok(a), Ok(b)) => (a, b),
                _ => continue,
            },
            None => match anchor.parse::<usize>() {
                Ok(a) => (a, a),
                Err(_) => continue,
            },
        };
        out.push((path.to_string(), first, last));
    }
    out
}

/// Verify one citation against this tree, at its recorded lines. On
/// success returns the cited lines as text (for the errno
/// cross-check); on failure, why not. The hard gate never calls
/// this: it checks pages against the committed snapshot instead.
fn verify_cite(root: &Path, cite: &Cite) -> Result<String, String> {
    // Citations name Detcore code inside this tree (SCHEMA.md); an
    // absolute path or a `..` escape is refused, never followed.
    if cite.path.starts_with('/')
        || !cite.path.starts_with("detcore/")
        || cite.path.split('/').any(|part| part == "..")
    {
        return Err(format!(
            "{}: citation path must stay under detcore/ in this tree",
            cite.path
        ));
    }
    if cite.last < cite.first || cite.first == 0 {
        return Err(format!(
            "{}:{}-{}: malformed citation range",
            cite.path, cite.first, cite.last
        ));
    }
    let full = root.join(&cite.path);
    let content = fs::read(&full).map_err(|_| {
        format!(
            "{}:{}-{}: cited file does not exist in this tree",
            cite.path, cite.first, cite.last
        )
    })?;
    let Some(bytes) = lines_bytes(&content, cite.first, cite.last) else {
        return Err(format!(
            "{}:{}-{}: cited range leaves the file ({} lines)",
            cite.path,
            cite.first,
            cite.last,
            content.split_inclusive(|b| *b == b'\n').count()
        ));
    };
    let got = sha256_hex16(&bytes);
    if got != cite.hash {
        return Err(format!(
            "{}:{}-{}: citation hash {} does not recompute (got {got}); the cited code moved or the claim is stale",
            cite.path, cite.first, cite.last, cite.hash
        ));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Paragraph/bullet units of prose body (footnote definitions already
/// removed): blank lines bound blocks; a `- ` line starts its own unit.
fn prose_units(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let flush = |out: &mut Vec<String>, cur: &mut String| {
        if !cur.trim().is_empty() {
            out.push(std::mem::take(cur));
        }
    };
    for line in body.lines() {
        if line.trim().is_empty() {
            flush(&mut out, &mut cur);
            continue;
        }
        if line.trim_start().starts_with("- ") {
            flush(&mut out, &mut cur);
        }
        cur.push_str(line.trim());
        cur.push(' ');
    }
    flush(&mut out, &mut cur);
    out
}

/// Body text with `[^id]` footnote references removed.
fn strip_footnote_refs(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    loop {
        match rest.find("[^") {
            Some(at) => {
                out.push_str(&rest[..at]);
                match rest[at..].find(']') {
                    Some(end) => rest = &rest[at + end + 1..],
                    None => break,
                }
            }
            None => {
                out.push_str(rest);
                break;
            }
        }
    }
    out
}

/// Sentences of a prose fragment, split coarsely on sentence ends.
fn prose_sentences(text: &str) -> Vec<String> {
    text.replace('\n', " ")
        .split(". ")
        .map(|s| s.trim().trim_end_matches('.').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Uppercase errno-shaped tokens (`EINVAL`, `ENOSYS`, ...) in text.
fn errno_tokens(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut cur = String::new();
    let flush = |cur: &mut String, out: &mut BTreeSet<String>| {
        if cur.len() >= 3
            && cur.starts_with('E')
            && cur.chars().skip(1).any(|c| c.is_ascii_uppercase())
            && cur
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        {
            out.insert(std::mem::take(cur));
        } else {
            cur.clear();
        }
    };
    for c in text.chars() {
        if c.is_ascii_uppercase() || c.is_ascii_digit() {
            cur.push(c);
        } else {
            flush(&mut cur, &mut out);
        }
    }
    flush(&mut cur, &mut out);
    out
}

/// Sentences that claim an absence or a universal — the class most
/// likely to be wrong by omission. Collected for reviewers by
/// `report`; negative errno claims are exempt from P41 (see SCHEMA).
fn negative_claims(section_body: &str) -> Vec<String> {
    // Whole-word markers: the reviewer reads these sentences first.
    // Substring matching missed plain "not"/"no" phrasing in review.
    const WORDS: [&str; 7] = [
        "never", "always", "cannot", "not", "no", "without", "instead",
    ];
    const PHRASES: [&str; 5] = [
        "can not",
        "does not",
        "instead of",
        "not modelled",
        "not modeled",
    ];
    prose_sentences(&strip_footnote_refs(section_body))
        .into_iter()
        .filter(|s| {
            let lower = s.to_lowercase();
            let words: BTreeSet<&str> = lower
                .split(|c: char| !c.is_ascii_alphanumeric())
                .filter(|w| !w.is_empty())
                .collect();
            WORDS.iter().any(|m| words.contains(m)) || PHRASES.iter().any(|m| lower.contains(m))
        })
        .collect()
}

/// Every prose sentence of a page, in order (footnote definitions
/// removed): the claim inventory `report` prints for the reviewer who
/// checks every claim against code.
fn prose_claims(section_body: &str) -> Vec<String> {
    prose_sentences(&strip_footnote_refs(section_body))
}

/// Byte offset of `fn <name>` as a definition (the next character
/// must be `(`, `<`, or whitespace before one of those), so a renamed
/// `fn read_test_v2` does not satisfy a citation of `read_test`.
fn fn_def_at(masked: &str, name: &str) -> Option<usize> {
    let needle = format!("fn {name}");
    let mut from = 0;
    while let Some(pos) = masked[from..].find(&needle) {
        let at = from + pos;
        let after = masked[at + needle.len()..].trim_start();
        if after.starts_with('(') || after.starts_with('<') {
            return Some(at);
        }
        from = at + needle.len();
    }
    None
}

/// The brace-matched extent of `fn <name>` in `content` (signature
/// through the closing brace), using the same comment/string masking
/// as the dispatch parser so braces in literals do not count.
fn fn_extent(content: &str, name: &str) -> Option<String> {
    let masked = mask_rust(content);
    let at = fn_def_at(&masked, name)?;
    let open = at + masked[at..].find('{')?;
    let close = matching(masked.as_bytes(), open)?;
    Some(content[at..=close].to_string())
}

/// Whether `word` occurs in `text` as a whole word (boundaries are
/// non-identifier characters), so `time` does not match inside
/// `successful_time_store_probe`.
fn contains_word_str(text: &str, word: &str) -> bool {
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(pos) = text[from..].find(word) {
        let start = from + pos;
        let end = start + word.len();
        let before_ok = start == 0 || !is_word(bytes[start - 1] as char);
        let after_ok = end == text.len() || !is_word(bytes[end] as char);
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

/// P43 relevance: does this test exercise the syscall? The test's own
/// extent (plus one level of same-file helpers it calls) must contain,
/// as a whole word, `Sysno::<name>`, `SYS_<name>`, `handle_<name>`,
/// the CamelCase type, or `libc::<name>(` — or call a function that
/// the page's own hash-pinned cited ranges define or call. A test
/// that merely exists in the right file is not evidence.
fn test_fn_relevant(
    content: &str,
    fn_name: &str,
    syscall: &str,
    cited_fn_names: &BTreeSet<String>,
) -> bool {
    let Some(extent) = fn_extent(content, fn_name) else {
        return false;
    };
    let mut hay = extent.clone();
    // One level of same-file helpers the test calls. Identifiers are
    // deduplicated first: a long test repeats its locals many times,
    // and the cap must bound distinct helpers looked up, not words.
    let called: BTreeSet<&str> = extent
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| w.len() > 2)
        .collect();
    for helper in called.iter().take(256) {
        if let Some(body) = fn_extent(content, helper) {
            hay.push_str(&body);
        }
        if hay.len() > 256_000 {
            break;
        }
    }
    // Scan the masked text: a token inside a comment or a string
    // literal is not a mention.
    let hay = mask_rust(&hay);
    if contains_word_str(&hay, &format!("Sysno::{syscall}"))
        || contains_word_str(&hay, &format!("SYS_{syscall}"))
        || contains_word_str(&hay, &format!("SYS_{}", syscall.to_uppercase()))
        || contains_word_str(&hay, &format!("handle_{syscall}"))
        || contains_word_str(&hay, &camel(syscall))
        || hay.contains(&format!("libc::{syscall}("))
    {
        return true;
    }
    // Or: it (or a same-file helper above) calls a credited function
    // the page's cited code defines or calls.
    let called: Vec<&str> = hay
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| w.len() > 2 && !RELEVANCE_STOPLIST.contains(w))
        .collect();
    called.iter().any(|w| cited_fn_names.contains(*w))
}

/// Names that never earn P43 relevance credit: prelude and std
/// shapes that appear in almost every cited range and test body.
const RELEVANCE_STOPLIST: [&str; 13] = [
    "Some", "None", "Ok", "Err", "Errno", "Error", "into", "from", "map", "unwrap", "expect",
    "clone", "cfg",
];

/// Function names the page's cited code calls that may earn a test
/// relevance: a name counts only if it is actually defined in the
/// cited file (cited files live under `detcore/`, P38) and is not a
/// stoplisted prelude/std shape. Without the definition requirement,
/// common call shapes (`Some(`, `Errno(`, `map(`) shared between a
/// test and any cited range would credit unrelated tests.
fn cited_fn_names(page: &Page, root: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for (_, cite) in page_cites(page) {
        let Ok(content) = fs::read(root.join(&cite.path)) else {
            continue;
        };
        let Some(bytes) = lines_bytes(&content, cite.first, cite.last) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        let whole_file = String::from_utf8_lossy(&content).into_owned();
        let masked_file = mask_rust(&whole_file);
        // Only words in call or definition shape count: every long
        // identifier would make the relevance check vacuous.
        for word in text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
            if word.len() > 2
                && !RELEVANCE_STOPLIST.contains(&word)
                && (text.contains(&format!("fn {word}")) || text.contains(&format!("{word}(")))
                && fn_def_at(&masked_file, word).is_some()
            {
                out.insert(word.to_string());
            }
        }
    }
    out
}

fn is_ymd(s: &str) -> bool {
    s.len() == 10
        && s.as_bytes().get(4) == Some(&b'-')
        && s.as_bytes().get(7) == Some(&b'-')
        && s.chars().filter(|c| c.is_ascii_digit()).count() == 8
}

fn is_hex40(s: &str) -> bool {
    s.len() == 40
        && s.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// Byte ranges of the rubric parts present in `body` (footnote
/// definitions removed), indexed like RUBRIC_PARTS. A part runs from
/// its lead-in to the next present lead-in or the end of the text.
fn rubric_part_ranges(body: &str) -> [Option<(usize, usize)>; 6] {
    let mut starts: [Option<usize>; 6] = [None; 6];
    for (i, part) in RUBRIC_PARTS.iter().enumerate() {
        starts[i] = body.find(part);
    }
    let mut out = [None; 6];
    for i in 0..6 {
        if let Some(start) = starts[i] {
            let end = (i + 1..6)
                .filter_map(|j| starts[j])
                .filter(|s| *s > start)
                .min()
                .unwrap_or(body.len());
            out[i] = Some((start, end));
        }
    }
    out
}

/// All code citations on a page (for `wave`): (footnote id, cite).
fn page_cites(page: &Page) -> Vec<(String, Cite)> {
    let Some(section) = page.sections.get(PROSE_SECTION) else {
        return Vec::new();
    };
    let (defs, _) = split_footnotes(section);
    let mut out = Vec::new();
    for (id, def) in &defs {
        if id == "linux" {
            continue;
        }
        for cite in cite_triples(def) {
            out.push((id.clone(), cite));
        }
    }
    out
}

/// P45, the In short summary: three or four plain sentences, no
/// citations, no hand-typed numbers, and a final gap-count sentence
/// that is a pure function of the Known gaps bullets. The summary
/// may compress the prose below; whether it adds a claim is a
/// review check the seat performs (SCHEMA, Done).
fn lint_in_short(page: &Page, rep: &mut Report) {
    let body = page.sections.get("In short").cloned().unwrap_or_default();
    if body.contains("[^") {
        rep.err("P45", "`## In short` carries no citations");
    }
    let ds = page
        .sections
        .get(PROSE_SECTION)
        .cloned()
        .unwrap_or_default();
    let want_last = in_short_gaps_sentence(known_gap_count(&ds));
    let sentences = prose_sentences(&body);
    if !(3..=4).contains(&sentences.len()) {
        rep.err(
            "P45",
            format!(
                "`## In short` must be three or four sentences, got {}",
                sentences.len()
            ),
        );
    }
    match sentences.last() {
        Some(last) if format!("{last}.") == want_last => {}
        _ => rep.err(
            "P45",
            format!("`## In short` must end with the generated sentence {want_last:?}"),
        ),
    }
    // The digit ban applies to the authored sentences; the generated
    // count sentence is the single allowed digit bearer.
    let authored = body.replace(&want_last, "");
    lint_prose_digits(page, &authored, rep);
}

/// The P36-P46 checks over one page's checked prose. Called only when
/// the prose block and the section are both present; their coupling
/// itself is checked by the caller.
fn lint_prose(page: &Page, root: &Path, rep: &mut Report, committed: bool) {
    let prose = page.prose.as_ref().expect("lint_prose needs a prose block");
    let section = page
        .sections
        .get(PROSE_SECTION)
        .cloned()
        .unwrap_or_default();
    // P36: stamp shape and triage legality.
    if page.schema != SCHEMA_V3 {
        rep.err("P36", "a `prose` block requires `schema: syscall-pages/v3`");
    }
    if prose.status != "checked" {
        rep.err(
            "P36",
            format!("`prose.status` must be `checked`, got {:?}", prose.status),
        );
    }
    if !is_ymd(&prose.reviewed_at) {
        rep.err("P36", "`prose.reviewed_at` must be YYYY-MM-DD");
    }
    if !is_hex40(&prose.based_on) {
        rep.err("P36", "`prose.based_on` must be a 40-hex source revision");
    }
    let strat = strategy(&page.code_facts);
    if let Some(risk) = &prose.passthrough_risk {
        if !PASSTHROUGH_RISKS.contains(&risk.as_str()) {
            rep.err(
                "P36",
                format!("`passthrough_risk` {risk:?} is not one of {PASSTHROUGH_RISKS:?}"),
            );
        }
        if strat != "passthrough" {
            rep.err("P36", "`passthrough_risk` on a non-passthrough page");
        }
    }
    let short_form = prose.passthrough_risk.as_deref() == Some("none");
    // P37: rubric shape over the definitions-free body.
    let (defs, body) = split_footnotes(&section);
    let ranges = rubric_part_ranges(&body);
    let mut last_start: Option<usize> = None;
    for (i, part) in RUBRIC_PARTS.iter().enumerate() {
        let required = !short_form || i == 1 || i == 4 || i == 5;
        match ranges[i] {
            None => {
                if required {
                    rep.err("P37", format!("rubric part missing: {part}"));
                }
            }
            Some((start, end)) => {
                if matches!(last_start, Some(prev) if start < prev) {
                    rep.err("P37", format!("rubric part out of order: {part}"));
                }
                last_start = Some(start);
                let lo = (start + part.len()).min(end);
                if body[lo..end].trim().is_empty() {
                    rep.err("P37", format!("rubric part is empty: {part}"));
                }
            }
        }
    }
    // P38: references, definitions, hashes, paragraph coverage.
    let refs = footnote_refs(&body);
    for id in &refs {
        if !defs.contains_key(id) {
            rep.err(
                "P38",
                format!("footnote [^{id}] is referenced but not defined"),
            );
        }
    }
    for id in defs.keys() {
        if !refs.contains(id) {
            rep.err(
                "P38",
                format!("footnote [^{id}] is defined but never referenced"),
            );
        }
    }
    // Snapshot state for this page, if a citations snapshot exists.
    let snapshot = load_snapshot(root);
    let snap_entry = snapshot.as_ref().and_then(|s| s.get(&page.syscall));
    if committed && snapshot.is_none() {
        rep.err(
            "P38",
            "docs/syscalls/citations.json is missing; the hard gate checks pages against it (run `anchor`)",
        );
    }
    let mut cited_text = String::new();
    let mut page_triples: Vec<(String, Cite)> = Vec::new();
    for (id, def) in &defs {
        if id == "linux" {
            if !def.contains("https://") {
                rep.err("P38", "the [^linux] footnote must carry a public link");
            }
            // P44: the kernel pointer names a tag, never a moving
            // branch, so the cited Linux behaviour stays checkable.
            let tagged = (def.contains("?h=v") || def.contains("/blob/v"))
                && def
                    .chars()
                    .zip(def.chars().skip(1))
                    .any(|(a, b)| a == 'v' && b.is_ascii_digit());
            if !tagged || def.contains("/master/") || def.contains("h=master") {
                rep.err(
                    "P44",
                    "the [^linux] footnote must pin a kernel tag (for example `?h=v7.1.3`), not a moving branch",
                );
            }
            continue;
        }
        let triples = cite_triples(def);
        if triples.is_empty() {
            rep.err(
                "P38",
                format!("footnote [^{id}] carries no `path:lines sha256:<hash>` citation"),
            );
        }
        // P46: a GitHub source link in this footnote must repeat one
        // of this footnote's own citation ranges exactly. This runs
        // in committed mode too: it reads the page only, so the
        // hard gate catches a stale anchor without reading Detcore.
        for (path, first, last) in github_source_links(def) {
            let matches_citation = triples
                .iter()
                .any(|c| c.path == path && c.first == first && c.last == last);
            if !matches_citation {
                rep.err(
                    "P46",
                    format!(
                        "footnote [^{id}] GitHub link {path}#L{first}-L{last} does not match any citation range in that footnote; update the link to the citation's current lines"
                    ),
                );
            }
        }
        for cite in &triples {
            page_triples.push((id.clone(), cite.clone()));
            if !committed {
                match verify_cite(root, cite) {
                    Ok(text) => {
                        cited_text.push_str(&text);
                        cited_text.push('\n');
                    }
                    Err(e) => rep.err("P38", e),
                }
            }
        }
    }
    // Snapshot agreement: in committed mode this replaces live
    // verification entirely; in exact modes a present snapshot must
    // still agree, so a hand edit without re-anchoring fails.
    if let Some(entry) = snap_entry {
        if entry.citations != page_triples {
            rep.err(
                "P38",
                "citations disagree with docs/syscalls/citations.json; run `anchor` after refreshing facts",
            );
        }
    } else if committed {
        rep.err(
            "P38",
            format!("no citations.json entry for {}; run `anchor`", page.syscall),
        );
    }
    // Coverage runs part by part. The Tests part is exempt only for
    // its evidence entries (units naming an `.rs` path); any other
    // paragraph in or after it still needs a citation, so prose
    // cannot hide behind the Tests heading.
    for (i, range) in ranges.iter().enumerate() {
        let Some((start, end)) = range else { continue };
        let lo = (start + RUBRIC_PARTS[i].len()).min(*end);
        for unit in prose_units(&body[lo..*end]) {
            let trimmed = unit.trim();
            if i == 5 && trimmed.contains(".rs`") {
                continue;
            }
            if !trimmed.contains("[^") {
                rep.err(
                    "P38",
                    format!(
                        "prose paragraph carries no citation: {:?}",
                        trimmed.chars().take(72).collect::<String>()
                    ),
                );
            }
        }
    }
    // P39: no hand-typed numbers outside the allowed forms.
    lint_prose_digits(page, &body, rep);
    // P40: part 2 names the derived strategy.
    if let Some((start, end)) = ranges[1] {
        let lo = (start + RUBRIC_PARTS[1].len()).min(end);
        let text = &body[lo..end];
        let first_para = text.split("\n\n").next().unwrap_or("").to_lowercase();
        let ok = match strat {
            "determinized" => first_para.contains("determiniz"),
            "determinized-refused" => first_para.contains("refus"),
            "passthrough" => first_para.contains("pass") || first_para.contains("forward"),
            _ => {
                first_para.contains("unsupported")
                    || first_para.contains("not supported")
                    || first_para.contains("refus")
            }
        };
        if !ok {
            rep.err(
                "P40",
                format!("`**What Hermit does.**` does not name the derived strategy ({strat}) in its first paragraph"),
            );
        }
    }
    // P41: a positively-claimed errno must appear in cited code.
    let cited_errnos: BTreeSet<String> = if committed {
        snap_entry
            .map(|e| e.cited_errnos.clone())
            .unwrap_or_default()
    } else {
        errno_tokens(&cited_text)
    };
    let mut claim_text = String::new();
    for i in [1usize, 2] {
        if let Some((start, end)) = ranges[i] {
            let lo = (start + RUBRIC_PARTS[i].len()).min(end);
            claim_text.push_str(&body[lo..end]);
            claim_text.push('\n');
        }
    }
    for sentence in prose_sentences(&strip_footnote_refs(&claim_text)) {
        let lower = sentence.to_lowercase().replace('`', "");
        for errno in errno_tokens(&sentence) {
            // The only exemption is an absence claim about this very
            // errno ("there is no `EINTR` path"): the cited range is
            // the whole handler, and the token's absence from it is
            // the claim. Every occurrence must be inside such a
            // phrase, so "no EINTR retry; it returns EINTR" still
            // checks the positive claim.
            let needle = errno.to_lowercase();
            let occurrences: Vec<usize> = lower.match_indices(&needle).map(|(i, _)| i).collect();
            let absence = !occurrences.is_empty()
                && occurrences.iter().all(|&i| {
                    lower[..i].ends_with("no ") || lower[..i].ends_with("never returns ")
                });
            if !absence && !cited_errnos.contains(&errno) {
                rep.err(
                    "P41",
                    format!("errno `{errno}` is claimed but appears in no cited line range"),
                );
            }
        }
    }
    // P42: subscription phrases held to the facts.
    let lower = body.to_lowercase();
    let f = &page.code_facts;
    if lower.contains("not subscribed") && (f.subscribed_run || f.subscribed_record_replay) {
        rep.err(
            "P42",
            "prose says `not subscribed` but a subscription fact is true",
        );
    }
    if lower.contains("subscribed in both") && !(f.subscribed_run && f.subscribed_record_replay) {
        rep.err(
            "P42",
            "prose says `subscribed in both` but a subscription fact is false",
        );
    }
    // P43: named tests exist and exercise this syscall (or, in
    // committed mode, match the snapshot's record of them).
    if let Some((start, end)) = ranges[5] {
        if committed {
            let got = tests_entries(&body[start..end]);
            match snap_entry {
                Some(entry) if entry.tests == got => {}
                Some(_) => rep.err(
                    "P43",
                    "Tests entries disagree with docs/syscalls/citations.json; run `anchor` after refreshing facts",
                ),
                None => rep.err(
                    "P43",
                    format!(
                        "no citations.json entry for {}; run `anchor`",
                        page.syscall
                    ),
                ),
            }
        } else {
            lint_tests_evidence(page, root, &body[start..end], rep);
        }
    }
}

/// P39's digit scan over definitions-free prose body.
fn lint_prose_digits(page: &Page, body: &str, rep: &mut Report) {
    // Remove code spans, footnote references, and link targets; their
    // digits are identifiers, not hand-typed quantities.
    let mut cleaned = String::new();
    let mut rest = body;
    loop {
        match rest.find('`') {
            Some(at) => {
                cleaned.push_str(&rest[..at]);
                cleaned.push(' ');
                rest = &rest[at + 1..];
                match rest.find('`') {
                    Some(end) => rest = &rest[end + 1..],
                    None => break,
                }
            }
            None => {
                cleaned.push_str(rest);
                break;
            }
        }
    }
    let no_refs = strip_footnote_refs(&cleaned);
    for token in no_refs.split_whitespace() {
        if !token.chars().any(|c| c.is_ascii_digit()) || token.starts_with("http") {
            continue;
        }
        let bare = token.trim_matches(|c: char| ",.;:()[]".contains(c));
        let allowed = is_ymd(bare)
            || bare == "x86_64"
            || bare == "x86-64"
            || bare == page.nr_x86_64.to_string()
            || (bare.starts_with('G')
                && bare[1..].chars().all(|c| c.is_ascii_digit())
                && bare.len() > 1);
        if !allowed {
            rep.err(
                "P39",
                format!("hand-typed number in checked prose: {bare:?} (facts belong in the generated sentence and tables)"),
            );
        }
    }
}

/// P43: in the Tests part, every backticked `.rs` path exists in
/// this tree, is followed by at least one backticked test name, each
/// such name is a function in that file, and the test is relevant
/// (`test_fn_relevant`): a test that merely exists is not evidence.
fn lint_tests_evidence(page: &Page, root: &Path, tests: &str, rep: &mut Report) {
    let cited = cited_fn_names(page, root);
    let spans: Vec<&str> = tests.split('`').collect();
    // (path, content, named a test yet)
    let mut last_rs: Option<(String, String, bool)> = None;
    let finish_path = |last: &Option<(String, String, bool)>, rep: &mut Report| {
        if let Some((path, _, false)) = last {
            rep.err(
                "P43",
                format!("Tests cite `{path}` without naming a test in it"),
            );
        }
    };
    for (i, span) in spans.iter().enumerate() {
        if i % 2 == 0 {
            continue;
        }
        let token = span.trim();
        if token.ends_with(".rs") {
            finish_path(&last_rs, rep);
            if token.starts_with('/') || token.split('/').any(|part| part == "..") {
                rep.err(
                    "P43",
                    format!("Tests cite `{token}`, which escapes this tree"),
                );
                last_rs = None;
                continue;
            }
            match fs::read_to_string(root.join(token)) {
                Ok(content) => last_rs = Some((token.to_string(), content, false)),
                Err(_) => {
                    rep.err("P43", format!("Tests cite `{token}`, which does not exist"));
                    last_rs = None;
                }
            }
            continue;
        }
        let is_fn_name = token.len() > 3
            && token.contains('_')
            && token
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if !is_fn_name {
            continue;
        }
        let Some((path, content, named)) = &mut last_rs else {
            continue;
        };
        *named = true;
        if fn_def_at(&mask_rust(content), token).is_none() {
            rep.err(
                "P43",
                format!("Tests name `{token}`, not a function in {path}"),
            );
        } else if !test_fn_relevant(content, token, &page.syscall, &cited) {
            rep.err(
                "P43",
                format!(
                    "Tests name `{token}` in {path}, whose extent never mentions `{}` or its handler and calls nothing the cited code defines or calls",
                    page.syscall
                ),
            );
        }
    }
    finish_path(&last_rs, rep);
}

// ---------------------------------------------------------------------------
// Public-text findings (P30). Pages and generated outputs live in a
// public repository: no host names, home or scratch paths, internal diff
// or task ids, private-workspace references, or internal URLs. Lint
// refuses every recognised form, so a new form is a visible failure to
// extend here, not a quiet leak.
// ---------------------------------------------------------------------------

/// The name prefix of an internal development host. A needle, not a host:
/// it is what lets P30 refuse a page that names such a host, so it is
/// spelled once, here, with its reason in scripts/portable-paths-allowlist.txt.
const INTERNAL_DEV_HOST_PREFIX: &str = "devbig";

fn is_internal_dev_host(s: &str) -> bool {
    let s = s.strip_suffix("'s").unwrap_or(s);
    if let Some(rest) = s.strip_prefix(INTERNAL_DEV_HOST_PREFIX) {
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
    while let Some(i) = text[at..].find(INTERNAL_DEV_HOST_PREFIX) {
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
        if is_internal_dev_host(core)
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

#[cfg(test)]
fn lint_page(name_hint: &str, text: &str, root: &Path) -> (Report, Option<Page>) {
    lint_page_committed(name_hint, text, root, false)
}

fn lint_page_committed(
    name_hint: &str,
    text: &str,
    root: &Path,
    committed: bool,
) -> (Report, Option<Page>) {
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
    // P36 coupling, then P02: sections exactly once, in order.
    let has_prose_section = page.sections.contains_key(PROSE_SECTION);
    let has_prose_block = page.prose.is_some();
    if has_prose_block != has_prose_section {
        rep.err(
            "P36",
            "the `prose` block and the `## Determinism strategy` section must be present together",
        );
    }
    if page.schema == SCHEMA && (has_prose_block || has_prose_section) {
        rep.err("P36", "checked prose requires `schema: syscall-pages/v3`");
    }
    if page.schema == SCHEMA_V3 && !has_prose_block {
        rep.err(
            "P36",
            "`schema: syscall-pages/v3` requires checked prose; a facts-only page stays `syscall-pages/v2`",
        );
    }
    if let Ok((_, body)) = split_front_matter(text) {
        let got = headings_in_order(body);
        let want: Vec<String> = section_order(has_prose_section)
            .iter()
            .map(|s| s.to_string())
            .collect();
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
    for h in ["Strategy", PROSE_SECTION]
        .into_iter()
        .filter(|h| *h == "Strategy" || has_prose_section)
    {
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
    if has_prose_block && has_prose_section {
        lint_prose(&page, root, &mut rep, committed);
        lint_in_short(&page, &mut rep);
    }
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
        "discussion/existing" => {
            if !page
                .discussion_url
                .starts_with("https://github.com/facebookexperimental/hermit/discussions/")
            {
                rep.err(
                    "P34",
                    "a `discussion/existing` kind must link a facebookexperimental/hermit discussion",
                );
            }
        }
        "discussion/new" => {
            if !page.discussion_url.starts_with(
                "https://github.com/facebookexperimental/hermit/discussions/new?",
            ) || !page
                .discussion_url
                .contains(&format!("category={DISCUSSION_CATEGORY}"))
            {
                rep.err(
                    "P34",
                    "a `discussion/new` kind must be a prefilled facebookexperimental/hermit new-discussion URL in the syscalls category",
                );
            }
        }
        other => rep.err(
            "P34",
            format!("discussion kind must be existing, new, discussion/existing, or discussion/new, got {other:?}"),
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
                if is_syscall_name(name)
                    && !skip_next
                    && classes
                        .insert(name.to_string(), (class.to_string(), off + 1))
                        .is_some()
                {
                    return Err(format!("census: {name} listed twice"));
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
    load_pages_committed(root, false)
}

fn load_pages_committed(
    root: &Path,
    committed: bool,
) -> Vec<(String, String, Report, Option<Page>)> {
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
        let (rep, page) = lint_page_committed(&name, &text, root, committed);
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
    // Staging rule (schema v3 roll-out): a generation with no checked
    // prose anywhere emits the exact v2 document, so the tool change
    // can land alone with generated outputs untouched. The first
    // prose batch flips the document to v3 in its own diff.
    let any_prose = pages.iter().any(|p| p.prose.is_some());
    let rows: Vec<serde_json::Value> = pages
        .iter()
        .map(|p| {
            let f = &p.code_facts;
            let mut row = serde_json::json!({
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
            });
            if any_prose {
                let obj = row.as_object_mut().expect("row is an object");
                let prose_status: serde_json::Value =
                    if p.prose.is_some() { "checked" } else { "none" }.into();
                obj.insert("prose_status".into(), prose_status);
                obj.insert(
                    "prose_based_on".into(),
                    p.prose.as_ref().map(|x| x.based_on.clone()).into(),
                );
                obj.insert(
                    "prose_reviewed_at".into(),
                    p.prose.as_ref().map(|x| x.reviewed_at.clone()).into(),
                );
                obj.insert(
                    "passthrough_risk".into(),
                    p.prose
                        .as_ref()
                        .and_then(|x| x.passthrough_risk.clone())
                        .into(),
                );
            }
            row
        })
        .collect();
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut prose_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for p in &pages {
        *counts.entry(strategy(&p.code_facts)).or_default() += 1;
        *prose_counts
            .entry(if p.prose.is_some() { "checked" } else { "none" })
            .or_default() += 1;
    }
    let mut doc = serde_json::json!({
        "schema": if any_prose { "syscalls.json/v3" } else { "syscalls.json/v2" },
        "generated_by": "scripts/syscall-pages.rs aggregate",
        "code_facts_as_of": sha,
        "total": pages.len(),
        "counts": counts,
        "syscalls": rows,
    });
    if any_prose {
        doc.as_object_mut()
            .expect("document is an object")
            .insert("prose_counts".into(), serde_json::json!(prose_counts));
    }
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
    // Same staging rule as syscalls_json: with no checked prose
    // anywhere, the index is byte-identical to the v2 generation.
    let any_prose = pages.iter().any(|p| p.prose.is_some());
    let intro = if any_prose {
        "One page per Linux syscall in this tree's pinned x86-64 Detcore census: the code-derived strategy and a discussion link. Pages with checked determinism-strategy prose say so in the prose column; the rest are facts only for now. The machine-readable form is [syscalls.json](syscalls.json); the page format is [SCHEMA.md](SCHEMA.md). Regenerate with `scripts/syscall-pages.rs generate` and `aggregate`; verify with `scripts/syscall-pages.rs check`."
    } else {
        "One facts-only page per Linux syscall in this tree's pinned x86-64 Detcore census: the code-derived strategy and a discussion link. Checked analysis prose returns in later batches. The machine-readable form is [syscalls.json](syscalls.json); the page format is [SCHEMA.md](SCHEMA.md). Regenerate with `scripts/syscall-pages.rs generate` and `aggregate`; verify with `scripts/syscall-pages.rs check`."
    };
    let _ = writeln!(s, "{intro}");
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
    if any_prose {
        let _ = writeln!(
            s,
            "| syscall | nr | strategy | dispatch | prose | discussion |"
        );
        let _ = writeln!(s, "|---|---:|---|---|---|---|");
    } else {
        let _ = writeln!(s, "| syscall | nr | strategy | dispatch | discussion |");
        let _ = writeln!(s, "|---|---:|---|---|---|");
    }
    for p in &pages {
        if any_prose {
            let _ = writeln!(
                s,
                "| [{}]({}.md) | {} | {} | {} | {} | [{}]({}) |",
                p.syscall,
                p.syscall,
                p.nr_x86_64,
                strategy(&p.code_facts),
                p.code_facts.dispatch,
                if p.prose.is_some() {
                    "checked"
                } else {
                    "facts only"
                },
                p.discussion_kind,
                p.discussion_url
            );
        } else {
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

/// Legacy prefilled-issue URL builder. Retained for the test
/// fixtures that model shipped v2 pages; new pages use
/// `github_discussion_new_url`.
#[cfg(test)]
fn discussion_url(name: &str) -> String {
    let page =
        format!("https://github.com/facebookexperimental/hermit/blob/main/docs/syscalls/{name}.md");
    let title = url_encode(&format!("Syscall discussion: {name}"));
    let body = url_encode(&format!(
        "Discussion for the `{name}` syscall page: {page}\n\nWhat should change in how Detcore handles `{name}`, or in what the page says?\n"
    ));
    format!("https://github.com/facebookexperimental/hermit/issues/new?title={title}&body={body}")
}

/// Prefilled GitHub Discussions form for a newly generated page. The
/// placeholder points back to the canonical page instead of copying
/// its contents, and records which facts revision it was generated
/// from. It creates nothing until a human submits the form.
fn github_discussion_new_url(name: &str, facts_revision: &str) -> String {
    let page =
        format!("https://github.com/facebookexperimental/hermit/blob/main/docs/syscalls/{name}.md");
    let title = url_encode(&format!("Syscall discussion: {name}"));
    let body = url_encode(&format!(
        "Discussion for the `{name}` syscall page: {page}\n\nPage facts revision when this link was generated: `{facts_revision}`\n\nWhat would you like to discuss?\n\n- Is the page's description of Detcore's behaviour wrong or missing a case?\n- Did a program hit a gap or refusal described there?\n- Should Hermit's handling of `{name}` change?\n"
    ));
    format!(
        "https://github.com/facebookexperimental/hermit/discussions/new?category={DISCUSSION_CATEGORY}&title={title}&body={body}"
    )
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

/// `committed` is the hard CI tier: pages, generator, and committed
/// facts only (lint including prose citation hashes, as_of/Status
/// agreement, generated-output freshness). It derives nothing from
/// live Detcore, so an unrelated Detcore change cannot fail it;
/// comparing committed facts against live Detcore is the soft drift
/// gate's job (`check --quick` in a target that always exits 0).
fn cmd_check(root: &Path, quick: bool, committed: bool) -> i32 {
    let mut rc = 0;
    let loaded = load_pages_committed(root, committed);
    if loaded.is_empty() {
        eprintln!("syscall-pages: no pages under {PAGES_DIR}");
        return 1;
    }
    let errors = print_reports(&loaded);
    if errors > 0 {
        rc = 1;
    }
    let pages = good_pages(&loaded);
    if committed {
        for finding in committed_snapshot_findings(&pages, root) {
            eprintln!("check: {finding}");
            rc = 1;
        }
    } else if let Some(finding) = exact_snapshot_finding(&pages, root) {
        eprintln!("check: {finding}");
        rc = 1;
    }
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
    if quick || committed {
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
        // Committed mode skips this derivation entirely: comparing the
        // committed facts against live Detcore is the soft gate's job.
        if !committed {
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
                        let want_cite =
                            format!("detcore/src/syscall_classification.rs:{class_line}");
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
        }
        if committed {
            println!(
                "check --committed: {} pages (detcore derivation skipped)",
                pages.len()
            );
        } else {
            println!("check --quick: {} pages (facts build skipped)", pages.len());
        }
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
    let status = if text.contains(&format!("schema: {SCHEMA}"))
        || text.contains(&format!("schema: {SCHEMA_V3}"))
    {
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

/// Checked prose a regeneration must preserve: (schema, prose stamps,
/// Determinism strategy section) parsed from the existing page, when it
/// has them. Best-effort by design, like `existing_page_bits`.
fn existing_prose(root: &Path, name: &str) -> Option<(String, Prose, String, String)> {
    let text = fs::read_to_string(root.join(PAGES_DIR).join(format!("{name}.md"))).ok()?;
    let mut rep = Report::new();
    let page = parse_page(&text, &mut rep)?;
    let prose = page.prose?;
    let section = page.sections.get(PROSE_SECTION)?.clone();
    let in_short = page.sections.get("In short").cloned().unwrap_or_default();
    Some((page.schema, prose, section, in_short))
}

/// (Re)generate every page's code facts from this tree's Detcore
/// sources, preserving checked prose. Numbers come from `--facts`
/// (an exporter TSV), then `--nr-map` (`name<TAB>nr` lines), then an
/// existing page; a census name with none fails closed rather than
/// guessing a number.
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
        let (discussion_kind, discussion_url) = existing_discussion.unwrap_or_else(|| {
            (
                "discussion/new".into(),
                github_discussion_new_url(name, &sha),
            )
        });
        let mut page = Page {
            schema: SCHEMA.to_string(),
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
            prose: None,
            sections: BTreeMap::new(),
        };
        if let Some((schema, prose, section, in_short)) = existing_prose(root, name) {
            page.schema = schema;
            page.prose = Some(prose);
            page.sections.insert(PROSE_SECTION.into(), section);
            if !in_short.trim().is_empty() {
                page.sections.insert("In short".into(), in_short);
            }
        }
        let class_line = census.classes.get(name).map(|(_, l)| *l).unwrap_or(0);
        page.sections.insert(
            "Strategy".into(),
            strategy_body(&page, class_line, dispatch.lines.get(name).copied()),
        );
        let discussion_text = match discussion_kind.as_str() {
            "discussion/new" => format!(
                "\n[Start a discussion about `{name}`]({discussion_url}) (opens a prefilled GitHub Discussion form; nothing is posted until you submit it).\n"
            ),
            "discussion/existing" | "existing" => {
                format!("\n[Discuss `{name}` on GitHub]({discussion_url}).\n")
            }
            _ => format!(
                "\n[Start a discussion about `{name}`]({discussion_url}) (opens a prefilled issue form; nothing is filed until you submit it).\n"
            ),
        };
        page.sections.insert("Discussion".into(), discussion_text);
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
        "generate: {written} pages at {sha} (census {}..={})",
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
        } else if let Some(stripped) = rest.strip_prefix("[^") {
            // Footnote reference: render as a plain superscript marker.
            // The definition (with its links) renders as body text below.
            match stripped.find(']') {
                Some(end) => {
                    out.push_str(&format!("<sup>[{}]</sup>", &stripped[..end]));
                    rest = &stripped[end + 1..];
                }
                None => {
                    out.push_str("[^");
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
    // Staging rule: prose chrome appears only once some page has
    // checked prose, keeping a prose-free render byte-stable.
    let prose_checked = pages.iter().filter(|p| p.prose.is_some()).count();
    if prose_checked > 0 {
        let _ = writeln!(
            s,
            "total: {} &middot; checked prose: {} &middot; facts only: {}</p>",
            pages.len(),
            prose_checked,
            pages.len() - prose_checked
        );
    } else {
        let _ = writeln!(s, "total: {}</p>", pages.len());
    }
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
    if prose_checked > 0 {
        let _ = writeln!(
            s,
            "<table><thead><tr><th>syscall</th><th>nr</th><th>strategy</th><th>dispatch</th><th>prose</th><th>discussion</th></tr></thead><tbody id=\"rows\">"
        );
    } else {
        let _ = writeln!(
            s,
            "<table><thead><tr><th>syscall</th><th>nr</th><th>strategy</th><th>dispatch</th><th>discussion</th></tr></thead><tbody id=\"rows\">"
        );
    }
    for p in &pages {
        if prose_checked > 0 {
            let _ = writeln!(
                s,
                "<tr data-strategy=\"{}\"><td><a href=\"{}.html\">{}</a></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td><a href=\"{}\">{}</a></td></tr>",
                html_escape(strategy(&p.code_facts)),
                html_escape(&p.syscall),
                html_escape(&p.syscall),
                p.nr_x86_64,
                html_escape(strategy(&p.code_facts)),
                html_escape(&p.code_facts.dispatch),
                if p.prose.is_some() {
                    "checked"
                } else {
                    "facts only"
                },
                html_escape(&p.discussion_url),
                html_escape(&p.discussion_kind),
            );
        } else {
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
    let short = |rev: &str| html_escape(&rev[..12.min(rev.len())]);
    if let Some(p) = &page.prose {
        let _ = writeln!(
            s,
            "<p>Facts as of <code>{}</code> &middot; prose checked {} at <code>{}</code></p>",
            short(&page.code_facts.as_of),
            html_escape(&p.reviewed_at),
            short(&p.based_on),
        );
    }
    for (h, body) in display_sections(page) {
        let _ = writeln!(s, "<h2>{}</h2>", html_escape(h));
        s.push_str(&md_to_html(&body));
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

/// Reviewer-facing prose status: stamps, the full claim inventory,
/// negative claims verbatim, passthrough triage, and stamp lag.
/// Never gates; batch descriptions quote it (see SCHEMA.md, Done).
/// Write `docs/syscalls/citations.json` from live Detcore: every
/// prose citation verified at its recorded lines, every Tests entry
/// recorded. Run after a facts refresh; this is the only writer.
fn cmd_anchor(root: &Path) -> i32 {
    let loaded = load_pages(root);
    if loaded.is_empty() {
        eprintln!("syscall-pages: no pages under {PAGES_DIR}");
        return 1;
    }
    if print_reports(&loaded) > 0 {
        eprintln!("anchor: refusing to snapshot while lint has errors");
        return 1;
    }
    let pages = good_pages(&loaded);
    match snapshot_document(&pages, root) {
        Ok(doc) => {
            if fs::write(root.join(CITATIONS_FILE), doc).is_err() {
                eprintln!("anchor: cannot write {CITATIONS_FILE}");
                return 1;
            }
            let n = pages.iter().filter(|p| p.prose.is_some()).count();
            println!("anchor: {n} prose pages -> {CITATIONS_FILE}");
            0
        }
        Err(e) => {
            eprintln!("anchor: {e}");
            1
        }
    }
}

fn cmd_report(root: &Path) -> i32 {
    let loaded = load_pages(root);
    if loaded.is_empty() {
        eprintln!("syscall-pages: no pages under {PAGES_DIR}");
        return 1;
    }
    let _ = print_reports(&loaded);
    let pages = sorted_pages(&good_pages(&loaded));
    let checked: Vec<&Page> = pages.iter().filter(|p| p.prose.is_some()).collect();
    println!("# Syscall prose report\n");
    println!(
        "checked prose: {} · facts only: {} · total: {}\n",
        checked.len(),
        pages.len() - checked.len(),
        pages.len()
    );
    let lagging: Vec<&Page> = checked
        .iter()
        .filter(|p| p.prose.as_ref().expect("checked page").based_on != p.code_facts.as_of)
        .copied()
        .collect();
    if !lagging.is_empty() {
        println!("## Prose stamp lag (based_on != code_facts.as_of)\n");
        for p in &lagging {
            let prose = p.prose.as_ref().expect("checked page");
            println!(
                "- {}: prose {} @{} · facts @{}",
                p.syscall,
                prose.reviewed_at,
                &prose.based_on[..12.min(prose.based_on.len())],
                &p.code_facts.as_of[..12.min(p.code_facts.as_of.len())]
            );
        }
        println!();
    }
    for p in &checked {
        let prose = p.prose.as_ref().expect("checked page");
        let section = p.sections.get(PROSE_SECTION).cloned().unwrap_or_default();
        let (_, body) = split_footnotes(&section);
        let refs = footnote_refs(&body).len();
        let negatives = negative_claims(&body);
        println!(
            "## {} ({}, {})",
            p.syscall,
            p.nr_x86_64,
            strategy(&p.code_facts)
        );
        println!(
            "- prose checked {} at {}{} · citation references: {} · negative claims: {}",
            prose.reviewed_at,
            &prose.based_on[..12.min(prose.based_on.len())],
            prose
                .passthrough_risk
                .as_ref()
                .map(|r| format!(" · passthrough_risk: {r}"))
                .unwrap_or_default(),
            refs,
            negatives.len()
        );
        for claim in &negatives {
            println!("  - negative: {claim}");
        }
        println!("  - claims (every one is checked against code in review):");
        for (i, claim) in prose_claims(&body).into_iter().enumerate() {
            println!("    {}. {claim}", i + 1);
        }
        println!();
    }
    let _ = root;
    0
}

/// Word-boundary containment for syscall names (letters, digits, `_`).
fn contains_word(line: &str, word: &str) -> bool {
    let is_word = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_';
    let bytes = line.as_bytes();
    let mut from = 0;
    while let Some(pos) = line[from..].find(word) {
        let start = from + pos;
        let end = start + word.len();
        let before_ok = start == 0 || !is_word(bytes[start - 1] as char);
        let after_ok = end == line.len() || !is_word(bytes[end] as char);
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

/// Update-wave worklist over two revisions of a git checkout that is
/// currently at `--to`. F (facts changed) reuses the exporter diff;
/// T (handler touched) over-approximates by watched files, cited
/// paths, and name mentions in the Detcore diff; S (citation-stale)
/// is exact: a prose citation hash that no longer recomputes here.
/// Reports only; apply with `facts --apply` / `generate`.
fn cmd_wave(root: &Path, args: &[String]) -> i32 {
    let (Some(from), Some(to)) = (arg_value(args, "--from"), arg_value(args, "--to")) else {
        fail("wave: need --from <rev> --to <rev>");
    };
    let to_spec = format!("{to}^{{commit}}");
    let resolved_to = match run_cmd(
        Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["rev-parse", to_spec.as_str()]),
    ) {
        Ok(s) => s.trim().to_string(),
        Err(e) => fail(&format!("wave: {e}")),
    };
    let head = git_head(root);
    if head != resolved_to {
        fail(&format!(
            "wave: this checkout is at {head}, not at --to ({resolved_to}); citation hashes are verified against the working tree"
        ));
    }
    let loaded = load_pages(root);
    let _ = print_reports(&loaded);
    let pages = sorted_pages(&good_pages(&loaded));
    let changed_files: Vec<String> = match run_cmd(Command::new("git").arg("-C").arg(root).args([
        "diff",
        "--name-only",
        from.as_str(),
        to.as_str(),
    ])) {
        Ok(s) => s
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
        Err(e) => fail(&format!("wave: {e}")),
    };
    let changed: BTreeSet<&str> = changed_files.iter().map(|s| s.as_str()).collect();
    let mut cited_paths_all: BTreeSet<String> = BTreeSet::new();
    cited_paths_all.insert("detcore/src/syscall_classification.rs".into());
    cited_paths_all.insert("detcore/src/lib.rs".into());
    for p in &pages {
        for (_, cite) in page_cites(p) {
            cited_paths_all.insert(cite.path);
        }
    }
    let watched = |f: &str| cited_paths_all.contains(f) || f.starts_with("detcore/src/syscalls/");
    let outside: Vec<&str> = changed_files
        .iter()
        .map(|s| s.as_str())
        .filter(|f| f.starts_with("detcore/src/") && !watched(f))
        .collect();
    // Name mentions in the Detcore diff (over-approximate on purpose).
    // A failed diff is fatal, exactly like the name-only call above:
    // degrading to an empty diff would silently drop candidates.
    let diff_text = match run_cmd(Command::new("git").arg("-C").arg(root).args([
        "diff",
        "--unified=0",
        from.as_str(),
        to.as_str(),
        "--",
        "detcore/src",
    ])) {
        Ok(s) => s,
        Err(e) => fail(&format!("wave: {e}")),
    };
    let diff_lines: Vec<&str> = diff_text
        .lines()
        .filter(|l| {
            (l.starts_with('+') || l.starts_with('-'))
                && !l.starts_with("+++")
                && !l.starts_with("---")
        })
        .collect();
    let mentioned: BTreeSet<String> = pages
        .iter()
        .filter(|p| diff_lines.iter().any(|l| contains_word(l, &p.syscall)))
        .map(|p| p.syscall.clone())
        .collect();
    println!("# Wave {from}..{to}\n");
    let mut rc = 0;
    println!("## F — facts changed\n");
    match facts_context(root, &pages) {
        Ok((sha, rows, dispatch)) => {
            let diffs = facts_diff(&pages, &sha, &rows, &dispatch.dispatch);
            if diffs.is_empty() {
                println!("(none)");
            }
            for d in &diffs {
                println!("- {d}");
            }
        }
        Err(e) => {
            println!("facts unavailable in this tree: {e}");
            rc = 1;
        }
    }
    println!("\n## S — citation-stale prose (re-read these claims)\n");
    let mut stale: BTreeSet<String> = BTreeSet::new();
    let mut stale_rows = Vec::new();
    for p in pages.iter().filter(|p| p.prose.is_some()) {
        for (id, cite) in page_cites(p) {
            if let Err(e) = verify_cite(root, &cite) {
                stale.insert(p.syscall.clone());
                stale_rows.push(format!("- {} [^{id}]: {e}", p.syscall));
            }
        }
    }
    if stale_rows.is_empty() {
        println!("(none)");
    }
    for row in &stale_rows {
        println!("{row}");
    }
    println!("\n## T — handler touched\n");
    let mut any_t = false;
    for p in &pages {
        let mut reasons = Vec::new();
        for (_, cite) in page_cites(p) {
            if changed.contains(cite.path.as_str()) {
                reasons.push(format!("cited file changed: {}", cite.path));
            }
        }
        if mentioned.contains(&p.syscall) {
            reasons.push("named in the detcore/src diff".to_string());
        }
        if changed.contains("detcore/src/syscall_classification.rs") {
            reasons.push("classification file changed".to_string());
        }
        reasons.sort();
        reasons.dedup();
        if reasons.is_empty() || stale.contains(&p.syscall) {
            continue;
        }
        any_t = true;
        let tag = if p.prose.is_some() {
            "confirm-unchanged candidate (citations still match)"
        } else {
            "facts only"
        };
        println!("- {} ({tag}): {}", p.syscall, reasons.join("; "));
    }
    if !any_t {
        println!("(none)");
    }
    if !outside.is_empty() {
        println!("\n## Changed under detcore/src but outside the watched surface\n");
        for f in &outside {
            println!("- {f}");
        }
    }
    rc
}

fn usage() -> ! {
    eprintln!(
        "usage: syscall-pages.rs <lint|facts [--apply] [--date YYYY-MM-DD]|generate [--facts TSV|--as-of SHA] [--nr-map TSV] [--mapping JSON] [--date YYYY-MM-DD]|aggregate [--check]|check [--quick|--committed]|render [--check]|anchor|report|wave --from REV --to REV>"
    );
    exit(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_page_for(name: &str, nr: i32) -> Page {
        let mut page = Page {
            schema: SCHEMA.into(),
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
            prose: None,
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
        // A host-shaped name built from the needle, so that no real host is
        // spelled in this public file.
        let bad = sample_page().replace(
            "facts-only page generated",
            &format!("Measured on {INTERNAL_DEV_HOST_PREFIX}000 today; facts-only page generated"),
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
            "/home/user/file",
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
    fn sha256_known_vectors() {
        assert_eq!(sha256_hex16(b""), "e3b0c44298fc1c14");
        assert_eq!(sha256_hex16(b"abc"), "ba7816bf8f01cfea");
        assert_eq!(
            sha256_hex16(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8"
        );
        assert_eq!(sha256_hex16(&vec![b'a'; 1_000_000]), "cdc76e5c9914fb92");
    }

    /// A temp tree holding one fake cited file, plus a v3 page whose
    /// prose cites it with the correct anchor hash.
    fn prose_fixture(tag: &str) -> (PathBuf, Page) {
        let root =
            std::env::temp_dir().join(format!("syscall-pages-test-{}-{tag}", std::process::id()));
        let dir = root.join("detcore/src/syscalls");
        fs::create_dir_all(&dir).expect("temp tree");
        let code = "pub fn handle_read() -> Result<(), Errno> {\n    Err(Errno::EINVAL.into())\n}\n#[test]\nfn read_einval_test() { let _ = handle_read(); }\n#[test]\nfn unrelated_probe_test() { let _ = Some(Err(Errno::EINVAL)); }\n";
        fs::write(dir.join("fake.rs"), code).expect("fake source");
        let hash = sha256_hex16(&lines_bytes(code.as_bytes(), 1, 3).expect("lines"));
        let mut page = sample_page_for("read", 0);
        page.schema = SCHEMA_V3.into();
        page.prose = Some(Prose {
            status: "checked".into(),
            reviewed_at: "2026-11-16".into(),
            based_on: "0123456789abcdef0123456789abcdef01234567".into(),
            passthrough_risk: None,
        });
        let strat = page
            .sections
            .get("Strategy")
            .cloned()
            .unwrap_or_default()
            .replace(
                "\nThis facts-only page was generated from Detcore code facts. Checked analysis prose has not been added yet.\n",
                "\n",
            );
        page.sections.insert("Strategy".into(), strat);
        page.sections.insert(
            "In short".into(),
            "\nNative reads return host file contents. Detcore determinizes the call from its file model. Known gaps: 1.\n".into(),
        );
        page.sections.insert(
            PROSE_SECTION.into(),
            format!(
                "\n**What is nondeterministic natively.** The bytes a native call returns depend on host file contents.[^linux]\n\n**What Hermit does.** Detcore determinizes the call and answers from its file model.[^c1]\n\n**Interface fidelity.** A bad flag combination is rejected with `EINVAL`.[^c1] There is no `EINTR` path.[^c1]\n\n**Record & replay.** The call is subscribed in both modes and replay regenerates the result from model state.[^c1]\n\n**Known gaps.**\n- G1 (by design): host file contents are an input, not made deterministic.[^c1]\n\n**Tests.** `detcore/src/syscalls/fake.rs` (`read_einval_test`) — part on interface fidelity.\n\n[^linux]: Linux `read(2)`: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/fs/read_write.c?h=v7.1.3\n[^c1]: detcore/src/syscalls/fake.rs:1-3 sha256:{hash}\n  https://github.com/facebookexperimental/hermit/blob/main/detcore/src/syscalls/fake.rs#L1-L3\n"
            ),
        );
        let docs = root.join("docs/syscalls");
        fs::create_dir_all(&docs).expect("fixture docs dir");
        let doc = snapshot_document(std::slice::from_ref(&page), &root).expect("fixture snapshot");
        fs::write(docs.join("citations.json"), doc).expect("write fixture snapshot");
        (root, page)
    }

    #[test]
    fn v3_prose_page_lints_clean_and_round_trips() {
        let (root, page) = prose_fixture("clean");
        let text = emit_page(&page);
        assert!(text.contains("prose:\n  status: checked"), "{text}");
        assert!(text.contains("## In short\n"), "{text}");
        assert!(text.contains("## Known gaps\n"), "{text}");
        assert!(text.contains("## Appendix\n"), "{text}");
        assert!(text.contains("### Code facts\n"), "{text}");
        let (rep, parsed) = lint_page("read", &text, &root);
        assert!(rep.errors.is_empty(), "errors: {:?}", rep.errors);
        assert!(parsed.expect("parses").prose.is_some());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn stale_citation_hash_fails_p38() {
        let (root, page) = prose_fixture("stale");
        let text = emit_page(&page);
        fs::write(
            root.join("detcore/src/syscalls/fake.rs"),
            "pub fn handle_read() -> Result<(), Errno> {\n    Ok(())\n}\n",
        )
        .expect("mutate cited file");
        let (rep, _) = lint_page("read", &text, &root);
        assert!(
            rep.errors.iter().any(|e| e.starts_with("P38")),
            "moved code must fail the anchor: {:?}",
            rep.errors
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn prose_red_twins_each_rule_bites() {
        let (root, page) = prose_fixture("twins");
        let text = emit_page(&page);
        // P39: a fossilizing hand-typed count.
        let bad = text.replace(
            "answers from its file model.",
            "answers from its file model across 268 cases.",
        );
        let (rep, _) = lint_page("read", &bad, &root);
        assert!(
            rep.errors.iter().any(|e| e.starts_with("P39")),
            "{:?}",
            rep.errors
        );
        // P41: an absence phrase early in the sentence does not
        // shelter a positive claim later in the same sentence.
        let bad = text.replace(
            "There is no `EINTR` path.[^c1]",
            "There is no `EINTR` retry; it returns `EINTR` on a signal.[^c1]",
        );
        let (rep, _) = lint_page("read", &bad, &root);
        assert!(
            rep.errors.iter().any(|e| e.starts_with("P41")),
            "{:?}",
            rep.errors
        );
        // P41: a positively-claimed errno absent from the cited code.
        let bad = text.replace("rejected with `EINVAL`", "rejected with `EACCES`");
        let (rep, _) = lint_page("read", &bad, &root);
        assert!(
            rep.errors.iter().any(|e| e.starts_with("P41")),
            "{:?}",
            rep.errors
        );
        // P37: a rubric part renamed away.
        let bad = text.replace("**Interface fidelity.**", "**Fidelity.**");
        let (rep, _) = lint_page("read", &bad, &root);
        assert!(
            rep.errors.iter().any(|e| e.starts_with("P37")),
            "{:?}",
            rep.errors
        );
        // P36: prose under the v2 schema line.
        let bad = text.replace("schema: syscall-pages/v3", "schema: syscall-pages/v2");
        let (rep, _) = lint_page("read", &bad, &root);
        assert!(
            rep.errors.iter().any(|e| e.starts_with("P36")),
            "{:?}",
            rep.errors
        );
        // P40: part 2 no longer names the derived strategy.
        let bad = text.replace("Detcore determinizes the call", "Detcore handles the call");
        let (rep, _) = lint_page("read", &bad, &root);
        assert!(
            rep.errors.iter().any(|e| e.starts_with("P40")),
            "{:?}",
            rep.errors
        );
        // P43: a named test that does not exist in the cited file.
        let bad = text.replace("`read_einval_test`", "`read_missing_test`");
        let (rep, _) = lint_page("read", &bad, &root);
        assert!(
            rep.errors.iter().any(|e| e.starts_with("P43")),
            "{:?}",
            rep.errors
        );
        // P46: the GitHub link is a copy of the citation range; a
        // stale anchor fails in exact mode and in committed (hard)
        // mode, where no Detcore file is read. The citation triple
        // itself is unchanged, so P38 alone cannot catch this.
        let bad = text.replace(
            "detcore/src/syscalls/fake.rs#L1-L3",
            "detcore/src/syscalls/fake.rs#L2-L3",
        );
        let (rep, _) = lint_page("read", &bad, &root);
        assert!(
            rep.errors.iter().any(|e| e.starts_with("P46")),
            "stale GitHub anchor must fail P46: {:?}",
            rep.errors
        );
        let (rep, _) = lint_page_committed("read", &bad, &root, true);
        assert!(
            rep.errors.iter().any(|e| e.starts_with("P46")),
            "stale GitHub anchor must fail P46 in committed mode: {:?}",
            rep.errors
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn discussion_links_accept_github_discussions_and_legacy_issues() {
        let revision = "0123456789abcdef0123456789abcdef01234567";
        let url = github_discussion_new_url("futex", revision);
        assert!(
            url.starts_with(
                "https://github.com/facebookexperimental/hermit/discussions/new?category=syscalls&title="
            ),
            "{url}"
        );
        assert!(url.contains("Syscall+discussion%3A+futex"), "{url}");
        assert!(url.contains(revision), "{url}");

        let mut page = sample_page_for("read", 0);
        page.discussion_kind = "discussion/new".into();
        page.discussion_url = github_discussion_new_url("read", revision);
        page.sections.insert(
            "Discussion".into(),
            format!(
                "\n[Start a discussion]({}) (prefilled form).\n",
                page.discussion_url
            ),
        );
        let (rep, _) = lint_page("read", &emit_page(&page), Path::new("."));
        assert!(
            !rep.errors.iter().any(|e| e.starts_with("P34")),
            "discussion/new must lint: {:?}",
            rep.errors
        );

        page.discussion_kind = "discussion/existing".into();
        page.discussion_url =
            "https://github.com/facebookexperimental/hermit/discussions/123".into();
        page.sections.insert(
            "Discussion".into(),
            format!("\n[Discuss]({}).\n", page.discussion_url),
        );
        let (rep, _) = lint_page("read", &emit_page(&page), Path::new("."));
        assert!(
            !rep.errors.iter().any(|e| e.starts_with("P34")),
            "discussion/existing must lint: {:?}",
            rep.errors
        );
    }

    #[test]
    fn prose_free_generation_keeps_v2_output_shape() {
        // The staging guard: with no checked prose anywhere, the v3
        // tool must emit the v2 generated shape so it lands green
        // alone. If this fails, the tool diff stops being landable
        // without a mass regeneration.
        let pages = vec![sample_page_for("read", 0), sample_page_for("write", 1)];
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let json = syscalls_json(&pages, sha);
        assert!(json.contains("\"syscalls.json/v2\""), "{json}");
        assert!(!json.contains("prose_status"), "{json}");
        assert!(!json.contains("prose_counts"), "{json}");
        let index = index_md(&pages, sha);
        assert!(index.contains("facts-only page"), "{index}");
        assert!(!index.contains("| prose |"), "{index}");
        let out = render_outputs(&pages, sha);
        let site = &out[&PathBuf::from("index.html")];
        assert!(!site.contains("<th>prose</th>"), "{site}");
        assert!(!site.contains("checked prose:"), "{site}");
        let page_html = &out[&PathBuf::from("read.html")];
        assert!(!page_html.contains("Determinism strategy"), "{page_html}");
        assert!(!page_html.contains("prose checked"), "{page_html}");
    }

    #[test]
    fn prose_subrules_each_bite() {
        let (root, page) = prose_fixture("subs");
        let text = emit_page(&page);
        let based_on = "0123456789abcdef0123456789abcdef01234567";
        let has = |bad: &str, rule: &str, what: &str| {
            let (rep, _) = lint_page("read", bad, &root);
            assert!(
                rep.errors.iter().any(|e| e.starts_with(rule)),
                "{what} must fail {rule}: {:?}",
                rep.errors
            );
        };
        // Rule-specific variant: the seat requires these to bite on
        // their own check, not on any error carrying the same id.
        let has_msg = |bad: &str, needle: &str, what: &str| {
            let (rep, _) = lint_page("read", bad, &root);
            assert!(
                rep.errors.iter().any(|e| e.contains(needle)),
                "{what} must fail with {needle:?}: {:?}",
                rep.errors
            );
        };
        // P42, both phrases.
        has(
            &text.replace(
                "subscribed in both modes and",
                "subscribed in both modes. It is not subscribed anywhere, and",
            ),
            "P42",
            "not-subscribed claim against true facts",
        );
        has(
            &text.replacen("subscribed_run: true", "subscribed_run: false", 1),
            "P42",
            "subscribed-in-both claim against a false fact",
        );
        // P36 stamp and triage sub-rules.
        has(
            &text.replace("status: checked", "status: draft"),
            "P36",
            "status",
        );
        has(
            &text.replace(&format!("based_on: {based_on}"), "based_on: xyz"),
            "P36",
            "based_on shape",
        );
        has(
            &text.replace(
                &format!("based_on: {based_on}"),
                &format!("based_on: {based_on}\n  passthrough_risk: none"),
            ),
            "P36",
            "risk on a determinized page",
        );
        // P36 coupling, both directions, and v3 without prose. The
        // block-without-section direction asserts on message text:
        // by rule id alone it would also pass via the v3 check below.
        let no_section = text.replace("## Determinism strategy\n", "## Strategy notes\n");
        let (rep, _) = lint_page("read", &no_section, &root);
        assert!(
            rep.errors
                .iter()
                .any(|e| e.contains("must be present together")),
            "prose block without its section must fail coupling by message: {:?}",
            rep.errors
        );
        has(
            &text.replace(&format!(
                "# prose: human/agent-authored, machine-checked (syscall-pages.rs check); v3 only\nprose:\n  status: checked\n  reviewed_at: 2026-11-16\n  based_on: {based_on}\n"
            ), ""),
            "P36",
            "section without its prose block",
        );
        has(
            &sample_page().replace("schema: syscall-pages/v2", "schema: syscall-pages/v3"),
            "P36",
            "v3 without prose",
        );
        // P37 order and emptiness.
        let swapped = text
            .replace("**Interface fidelity.**", "**ZZZ.**")
            .replace("**Record & replay.**", "**Interface fidelity.**")
            .replace("**ZZZ.**", "**Record & replay.**");
        has(&swapped, "P37", "out-of-order parts");
        has(
            &text.replace(
                "A bad flag combination is rejected with `EINVAL`.[^c1] There is no `EINTR` path.[^c1]",
                "",
            ),
            "P37",
            "empty part",
        );
        // P38 reference integrity and range handling.
        has(
            &text.replace("file model.[^c1]", "file model.[^c1][^c9]"),
            "P38",
            "undefined ref",
        );
        has(
            &text.replace(
                "file model.[^c1]",
                "file model.[^c1]\n\nAn uncited extra claim stands here.",
            ),
            "P38",
            "uncited paragraph",
        );
        let post_tests = text.replace(
            "— part on interface fidelity.\n",
            "— part on interface fidelity.\n\nA further uncited claim hides after the tests.\n",
        );
        assert!(
            post_tests.contains("A further uncited claim"),
            "twin setup must actually insert the paragraph"
        );
        has(&post_tests, "P38", "uncited paragraph after the Tests part");
        has_msg(
            &text.replace(
                "`detcore/src/syscalls/fake.rs` (`read_einval_test`)",
                "`/etc/passwd.rs` (`read_einval_test`)",
            ),
            "escapes this tree",
            "tests path escapes the tree",
        );
        has(
            &text.replacen(
                "detcore/src/syscalls/fake.rs:1-3",
                "detcore/src/syscalls/nope.rs:1-3",
                1,
            ),
            "P38",
            "missing cited file",
        );
        has(
            &text.replacen(
                "detcore/src/syscalls/fake.rs:1-3",
                "detcore/src/syscalls/fake.rs:1-99",
                1,
            ),
            "P38",
            "range past end of file",
        );
        // P44: the linux pointer must pin a tag.
        has(
            &text.replace("?h=v7.1.3", "?h=master"),
            "P44",
            "untagged linux link",
        );
        // P40 arms beyond determinized.
        has(
            &text.replacen("refused: false", "refused: true", 1),
            "P40",
            "refused arm",
        );
        has(
            &text.replacen(
                "detcore_class: determinized",
                "detcore_class: passthrough",
                1,
            ),
            "P40",
            "passthrough arm",
        );
        has(
            &text.replacen(
                "detcore_class: determinized",
                "detcore_class: unsupported",
                1,
            ),
            "P40",
            "unsupported arm",
        );
        // P43: missing file, and a path naming no test.
        has(
            &text.replace(
                "`detcore/src/syscalls/fake.rs` (`read_einval_test`)",
                "`detcore/src/syscalls/nope.rs` (`read_einval_test`)",
            ),
            "P43",
            "missing tests file",
        );
        has(
            &text.replace(" (`read_einval_test`)", ""),
            "P43",
            "path without a named test",
        );
        // P05 inside the prose block.
        has(
            &text.replace("  status: checked\n", "  status: checked\n  bogus: x\n"),
            "P05",
            "unknown prose key",
        );
        has(
            &text.replace("  status: checked\n", ""),
            "P05",
            "missing prose status",
        );
        // P36 stamp shapes not covered above.
        has(
            &text.replace("reviewed_at: 2026-11-16", "reviewed_at: 16 Nov 2026"),
            "P36",
            "reviewed_at shape",
        );
        has_msg(
            &text.replace(
                &format!("based_on: {based_on}"),
                &format!("based_on: {based_on}\n  passthrough_risk: bogus"),
            ),
            "is not one of",
            "illegal passthrough_risk value",
        );
        // P38: unreferenced definition, linkless linux note, triple-free note.
        let code = "pub fn handle_read() -> Result<(), Errno> {\n    Err(Errno::EINVAL.into())\n}\n#[test]\nfn read_einval_test() {}\n";
        let hash = sha256_hex16(&lines_bytes(code.as_bytes(), 1, 3).expect("lines"));
        has(
            &text.replace(
                "[^c1]: detcore/src/syscalls/fake.rs:1-3",
                &format!(
                    "[^c7]: detcore/src/syscalls/fake.rs:1-3 sha256:{hash}\n[^c1]: detcore/src/syscalls/fake.rs:1-3"
                ),
            ),
            "P38",
            "defined but unreferenced footnote",
        );
        has(
            &text.replace("https://git.kernel.org", "http://example.com"),
            "P38",
            "linux footnote without https",
        );
        has(
            &text
                .replace("file model.[^c1]", "file model.[^c1][^c8]")
                .replace("[^linux]:", "[^c8]: just some words\n[^linux]:"),
            "P38",
            "footnote with no citation triple",
        );
        // Citation path containment (absolute, escape, outside detcore/).
        for bad_path in [
            "/etc/passwd:1-1",
            "detcore/../outside.rs:1-1",
            "scripts/syscall-pages.rs:1-3",
        ] {
            has_msg(
                &text.replacen("detcore/src/syscalls/fake.rs:1-3", bad_path, 1),
                "must stay under detcore/",
                "citation path containment",
            );
        }
        // P41 narrowed exemption: a merely negative sentence no longer
        // shelters an unrelated errno claim.
        has(
            &text.replace(
                "rejected with `EINVAL`.[^c1]",
                "not served from the stream and rejected with `EACCES`.[^c1]",
            ),
            "P41",
            "negative phrasing does not shelter another errno",
        );
        // P34 wrong shapes for the discussion kinds.
        let mut p = sample_page_for("read", 0);
        p.discussion_kind = "discussion/new".into();
        let (rep, _) = lint_page("read", &emit_page(&p), Path::new("."));
        assert!(
            rep.errors.iter().any(|e| e.starts_with("P34")),
            "discussion/new with an issue URL must fail: {:?}",
            rep.errors
        );
        p.discussion_kind = "discussion/new".into();
        p.discussion_url =
            "https://github.com/facebookexperimental/hermit/discussions/new?title=x".into();
        p.sections.insert(
            "Discussion".into(),
            format!("\n[Start a discussion]({}).\n", p.discussion_url),
        );
        let (rep, _) = lint_page("read", &emit_page(&p), Path::new("."));
        assert!(
            rep.errors.iter().any(|e| e.starts_with("P34")),
            "discussion/new without the category must fail: {:?}",
            rep.errors
        );
        p.discussion_kind = "discussion/existing".into();
        p.discussion_url = "https://example.com/discussions/1".into();
        p.sections.insert(
            "Discussion".into(),
            format!("\n[Discuss]({}).\n", p.discussion_url),
        );
        let (rep, _) = lint_page("read", &emit_page(&p), Path::new("."));
        assert!(
            rep.errors.iter().any(|e| e.starts_with("P34")),
            "discussion/existing on the wrong host must fail: {:?}",
            rep.errors
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn p43_common_call_shapes_do_not_earn_relevance() {
        // The timer_create-as-gettimeofday shape: the test shares
        // only prelude call shapes (`Some(`, `Err(`, `Errno::`) with
        // the cited code. Those earn no credit.
        let (root, page) = prose_fixture("commonshapes");
        let text = emit_page(&page);
        let bad = text.replace("`read_einval_test`", "`unrelated_probe_test`");
        let (rep, _) = lint_page("read", &bad, &root);
        assert!(
            rep.errors.iter().any(|e| e.starts_with("P43")),
            "a test sharing only prelude call shapes must fail P43: {:?}",
            rep.errors
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn p45_in_short_shape_and_count() {
        let (root, page) = prose_fixture("inshort");
        let text = emit_page(&page);
        let has45 = |bad: &str, what: &str| {
            let (rep, _) = lint_page("read", bad, &root);
            assert!(
                rep.errors.iter().any(|e| e.starts_with("P45")),
                "{what} must fail P45: {:?}",
                rep.errors
            );
        };
        has45(
            &text.replace("Known gaps: 1.", "Known gaps: 2."),
            "stated gap count disagrees with the bullets",
        );
        has45(
            &text.replace(
                "Native reads return host file contents.",
                "Native reads return host file contents.[^c1]",
            ),
            "citation inside In short",
        );
        has45(
            &text.replace("Detcore determinizes the call from its file model. ", ""),
            "two-sentence summary",
        );
        has45(
            &text.replace(
                "Known gaps: 1.",
                "Extra words here. More words here. Known gaps: 1.",
            ),
            "five-sentence summary",
        );
        // Mutate a phrase unique to In short: "host file contents."
        // also occurs in prose part 1, where P39 would fire anyway
        // and the twin could not tell the two scans apart.
        assert_eq!(
            text.matches("Native reads return host file contents.")
                .count(),
            1,
            "twin mutation target must be unique to In short"
        );
        let (rep, _) = lint_page(
            "read",
            &text.replace(
                "Native reads return host file contents.",
                "Native reads return 268 host file contents.",
            ),
            &root,
        );
        assert!(
            rep.errors.iter().any(|e| e.starts_with("P39")),
            "hand-typed digit in In short must fail: {:?}",
            rep.errors
        );
        // Appendix strictness: nothing may sit outside the three
        // known subsections, and none may repeat.
        for (bad, what) in [
            (
                text.replace("## Appendix\n", "## Appendix\n\nStray unlinted text.\n"),
                "Appendix preamble text",
            ),
            (
                text.replace(
                    "### Tests\n",
                    "### Notes\n\nStray unlinted text.\n\n### Tests\n",
                ),
                "unknown Appendix subsection",
            ),
            (
                text.replace(
                    "### Citations\n",
                    "### Tests\n\nDuplicate body.\n\n### Citations\n",
                ),
                "duplicate Appendix subsection",
            ),
        ] {
            let (rep, _) = lint_page("read", &bad, &root);
            assert!(
                rep.errors.iter().any(|e| e.starts_with("P02")),
                "{what} must fail P02: {:?}",
                rep.errors
            );
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn committed_mode_ignores_detcore_only_edits_but_not_snapshot_drift() {
        // The two-tier ruling's proofs, on the fixture tree:
        // (i) Detcore-only edits (a 30-line move, a content edit
        // inside the cited range, a cited-test rename) leave the
        // hard committed check green while exact mode reports drift;
        // (ii) a page hash that disagrees with the committed
        // snapshot fails the hard check.
        let (root, page) = prose_fixture("committedproof");
        let text = emit_page(&page);
        let (rep, _) = lint_page_committed("read", &text, &root, true);
        assert!(rep.errors.is_empty(), "clean committed: {:?}", rep.errors);
        let fake = root.join("detcore/src/syscalls/fake.rs");
        let code = fs::read_to_string(&fake).expect("fixture file");
        let moved = format!(
            "{}{}",
            "// shift\n".repeat(30),
            code.replace("read_einval_test", "read_einval_test_v2")
                .replace("Err(Errno::EINVAL.into())", "Ok(())")
        );
        fs::write(&fake, moved).expect("mutate detcore only");
        let (rep, _) = lint_page_committed("read", &text, &root, true);
        assert!(
            rep.errors.is_empty(),
            "detcore-only edits must not fail committed mode: {:?}",
            rep.errors
        );
        let (rep, _) = lint_page("read", &text, &root);
        assert!(
            rep.errors
                .iter()
                .any(|e| e.starts_with("P38") || e.starts_with("P43")),
            "exact mode must report the same edits as drift: {:?}",
            rep.errors
        );
        // Stale snapshot agreement fails committed mode: corrupt the
        // page's hash so page and snapshot disagree.
        let fake_orig = "pub fn handle_read() -> Result<(), Errno> {\n    Err(Errno::EINVAL.into())\n}\n#[test]\nfn read_einval_test() { let _ = handle_read(); }\n#[test]\nfn unrelated_probe_test() { let _ = Some(Err(Errno::EINVAL)); }\n";
        fs::write(&fake, fake_orig).expect("restore fixture file");
        let hash = sha256_hex16(&lines_bytes(fake_orig.as_bytes(), 1, 3).expect("lines"));
        let bad = text.replace(&hash, "0000000000000000");
        let (rep, _) = lint_page_committed("read", &bad, &root, true);
        assert!(
            rep.errors.iter().any(|e| e.starts_with("P38")),
            "snapshot disagreement must fail committed mode: {:?}",
            rep.errors
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn snapshot_honesty_catches_hand_edits_and_bad_shape() {
        // C1: exact modes rebuild the snapshot and compare bytes, so
        // a hand-added errno (which the hard tier would trust for
        // P41) and a malformed snapshot both fail. C2: the hard
        // tier refuses entries for missing or prose-less pages and
        // a based_on that differs from the page's own stamp.
        let (root, page) = prose_fixture("snaphonesty");
        let pages = std::slice::from_ref(&page);
        let snap_path = root.join(CITATIONS_FILE);
        let clean = fs::read_to_string(&snap_path).expect("fixture snapshot");
        assert_eq!(exact_snapshot_finding(pages, &root), None);
        assert!(committed_snapshot_findings(pages, &root).is_empty());

        let mut doc: serde_json::Value =
            serde_json::from_str(&clean).expect("parse fixture snapshot");
        doc["pages"]["read"]["cited_errnos"]
            .as_array_mut()
            .expect("cited_errnos array")
            .push(serde_json::json!("EACCES"));
        let tampered = serde_json::to_string_pretty(&doc).expect("serialize") + "\n";
        fs::write(&snap_path, &tampered).expect("tamper snapshot");
        let finding = exact_snapshot_finding(pages, &root).expect("tamper must be found");
        assert!(
            finding.contains("disagrees byte for byte"),
            "hand-added errno must fail the exact byte compare: {finding}"
        );
        fs::write(&snap_path, "{\n").expect("malform snapshot");
        assert!(
            exact_snapshot_finding(pages, &root).is_some(),
            "a malformed citations.json must not pass exact check silently"
        );

        let mut doc: serde_json::Value =
            serde_json::from_str(&clean).expect("reparse clean snapshot");
        doc["pages"]["read"]["based_on"] =
            serde_json::json!("0000000000000000000000000000000000000000");
        fs::write(
            &snap_path,
            serde_json::to_string_pretty(&doc).expect("serialize") + "\n",
        )
        .expect("write based_on mismatch");
        let findings = committed_snapshot_findings(pages, &root);
        assert!(
            findings.iter().any(|f| f.contains("based_on")),
            "based_on mismatch must fail the hard tier: {findings:?}"
        );

        let mut doc: serde_json::Value =
            serde_json::from_str(&clean).expect("reparse clean snapshot");
        let read_entry = doc["pages"]["read"].clone();
        doc["pages"]
            .as_object_mut()
            .expect("pages object")
            .insert("write".to_string(), read_entry);
        fs::write(
            &snap_path,
            serde_json::to_string_pretty(&doc).expect("serialize") + "\n",
        )
        .expect("write extra entry");
        let findings = committed_snapshot_findings(pages, &root);
        assert!(
            findings.iter().any(|f| f.contains("write")),
            "entry for a missing page must fail the hard tier: {findings:?}"
        );
        let mut prose_less = page.clone();
        prose_less.syscall = "write".to_string();
        prose_less.prose = None;
        let with_prose_less = vec![page.clone(), prose_less];
        let findings = committed_snapshot_findings(&with_prose_less, &root);
        assert!(
            findings.iter().any(|f| f.contains("write")),
            "entry for a prose-less page must fail the hard tier: {findings:?}"
        );
        // Pin the cmd_check wiring itself: with the orphan entry in
        // place, committed check over the written page must refuse.
        fs::write(root.join("docs/syscalls/read.md"), emit_page(&page))
            .expect("write fixture page");
        assert_eq!(cmd_aggregate(&root, false), 0);
        assert_eq!(cmd_check(&root, false, true), 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn p43_renamed_test_no_longer_satisfies_the_page() {
        let (root, page) = prose_fixture("renamed");
        let text = emit_page(&page);
        // Rename the test in the cited file only: the page still
        // names the old one, which must stop counting as existing.
        let fake = root.join("detcore/src/syscalls/fake.rs");
        let code = fs::read_to_string(&fake).expect("fixture file");
        fs::write(
            &fake,
            code.replace("read_einval_test", "read_einval_test_v2"),
        )
        .expect("rename fixture test");
        let (rep, _) = lint_page("read", &text, &root);
        assert!(
            rep.errors.iter().any(|e| e.starts_with("P43")),
            "renamed-away test must fail P43: {:?}",
            rep.errors
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn p43_relevance_extent_and_tokens() {
        // Round-2 shape one: the token sits in the NEXT function,
        // inside the old fixed window but outside this test's extent.
        let spill =
            "fn wrong_test() { let x = 1; }\nfn helper_after() { unsafe { libc::mprotect(p) } }\n";
        assert!(!test_fn_relevant(
            spill,
            "wrong_test",
            "mprotect",
            &BTreeSet::new()
        ));
        // Round-2 shape two: the syscall name inside the test's own
        // name is not a whole-word mention.
        let named = "fn successful_time_store_probe() { assert!(true); }";
        assert!(!test_fn_relevant(
            named,
            "successful_time_store_probe",
            "time",
            &BTreeSet::new()
        ));
        // The passing component case: the test calls a helper the
        // page's cited code defines (the adjtimex shape).
        let comp = "fn timex_query_test() { assert!(timex_mode_is_query(0)); }";
        let mut cited = BTreeSet::new();
        cited.insert("timex_mode_is_query".to_string());
        assert!(test_fn_relevant(
            comp,
            "timex_query_test",
            "adjtimex",
            &cited
        ));
        // One same-file helper level reaches the token (the fixed
        // mprotect citation's shape).
        let two = "fn real_test() { page_before_a_hole(0); }\nfn page_before_a_hole(p: i32) { unsafe { libc::mprotect(p) } }";
        assert!(test_fn_relevant(
            two,
            "real_test",
            "mprotect",
            &BTreeSet::new()
        ));
    }

    #[test]
    fn prose_helpers_negative_claims_and_words() {
        let claims =
            negative_claims("It never blocks. It sometimes waits. There is no EINTR path.");
        assert_eq!(claims.len(), 2, "{claims:?}");
        // The markers a seat read found missing: each must be caught.
        let marked = negative_claims(
            "It does not read the host clock. The record is not consumed here. \
             No handler runs. It falls back instead of resuming. A plain positive claim stands.",
        );
        assert_eq!(marked.len(), 4, "{marked:?}");
        assert!(contains_word("Sysno::getrandom,", "getrandom"));
        assert!(!contains_word("already_read", "read"));
        assert!(errno_tokens("Err(Errno::EINVAL)").contains("EINVAL"));
    }

    #[test]
    fn aggregate_and_render_carry_prose_stamps() {
        let (root, page) = prose_fixture("stamps");
        let pages = vec![page.clone(), sample_page_for("write", 1)];
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let json = syscalls_json(&pages, sha);
        assert!(json.contains("\"syscalls.json/v3\""), "{json}");
        assert!(json.contains("\"prose_status\": \"checked\""), "{json}");
        assert!(json.contains("\"prose_status\": \"none\""), "{json}");
        assert!(
            index_md(&pages, sha).contains("| checked |"),
            "index prose column"
        );
        let html = site_page_html(&page);
        assert!(html.contains("<h2>Determinism strategy</h2>"), "{html}");
        assert!(html.contains("prose checked 2026-11-16"), "{html}");
        assert!(html.contains("<sup>[c1]</sup>"), "{html}");
        let _ = fs::remove_dir_all(&root);
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
        Some("check") => cmd_check(
            &root,
            args.iter().any(|a| a == "--quick"),
            args.iter().any(|a| a == "--committed"),
        ),
        Some("generate") => cmd_generate(&root, &args[1..]),
        Some("render") => cmd_render(&root, args.iter().any(|a| a == "--check")),
        Some("report") => cmd_report(&root),
        Some("anchor") => cmd_anchor(&root),
        Some("wave") => cmd_wave(&root, &args[1..]),
        _ => usage(),
    };
    exit(rc);
}
