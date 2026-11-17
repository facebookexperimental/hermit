#!/usr/bin/env -S rust-script --force
//! Copyright (c) Meta Platforms, Inc. and affiliates.
//! All rights reserved.
//! This source code is licensed under the BSD-style license found in the
//! LICENSE file in the root directory of this source tree.
//! ```cargo
//! [dependencies]
//! proc-macro2 = "1"
//! syn = { version = "2", features = ["full", "visit"] }
//! ```
//!
//! LinuxEmu unsafe-census ratchet (lead plan U-gate / L1).
//!
//! Counts `unsafe` syntax sites in `reverie-linuxemu/src` outside test
//! scopes with a real parser (syn), never by text matching: unsafe blocks,
//! unsafe fns (free, impl and trait items), unsafe impls, unsafe traits,
//! unsafe extern blocks, plus `unsafe` tokens inside macro bodies (which
//! the AST cannot expand). Test scopes are files under a `tests/`
//! directory, `*_tests.rs` files, and items carrying `#[test]` or a
//! test-only cfg: exactly `cfg(test)` or `cfg(all(test, ...))`.
//! Production-reachable shapes (`cfg(any(test, ...))`, `cfg(not(test))`)
//! are counted. All other cfg branches stay in, so this is a source
//! census, not a compiled-artifact count.
//!
//! The census must match `scripts/linuxemu-unsafe-baseline.txt`
//! exactly, per file — in both directions. An increase refuses as a
//! census overrun; a decrease refuses as unlocked until
//! `--write-baseline` locks it in. Regenerating with an increase
//! requires a recorded justification per raised file
//! (`--justify <path>=<reason>`, persisted as a `justify` line),
//! which the diff's review seat judges alongside the moved sites.
//! A stale justification refuses. Creating a baseline where none
//! exists requires `--init`, so a deleted baseline cannot silently
//! re-baseline the tree. In git scope the audit also compares
//! against the committed (HEAD) baseline, which catches only
//! UNCOMMITTED hand edits: wherever this checker is the gate (CI,
//! validation, export) HEAD already contains the edited baseline,
//! so a committed raise is enforced by the seat's review of its
//! justify lines. Every run prints which comparison ran
//! (`BASE: HEAD baseline compared` or `BASE: skipped: <reason>`);
//! Sapling scope always skips. The real base comparison (explicit
//! merge-base revision) is F4 follow-up dh-l4tx3z.
//! A syntax count is not a safety proof; it is a mechanical record
//! that cannot drift silently.
//!
//! Method note: the 2026-11-16 formal-verification census reported 613
//! sites outside tests at fbsource b9092dc0a180. This checker counts
//! 612 at that same head (per-file identical at fbsource 8c5befcd, so
//! there is no head drift): the one differing site is
//! `executor/read_write.rs:1845`, the `mprotect` block in
//! `#[cfg(test)] fn protect_after_data` — an impl-item fn, which the
//! report scanner's test-range registration (Item::Fn/Mod/Impl only)
//! missed and therefore counted as production. This checker scopes
//! impl-item fns correctly, so its series starts at 612.
//!
//! Exit codes: 0 PASS, 1 REFUSE (census differs from baseline),
//! 2 CHECKER ERROR (fail closed: bad args, missing tree, parse error,
//! negative-control failure).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use syn::visit::Visit;

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

const PASS: u8 = 0;
const REFUSE: u8 = 1;
const CHECKER_ERROR: u8 = 2;

#[derive(Default, Clone, Copy, PartialEq, Eq)]
struct Census {
    blocks: usize,
    fns: usize,
    impls: usize,
    traits: usize,
    externs: usize,
    macro_tokens: usize,
}

impl Census {
    fn total(&self) -> usize {
        self.blocks + self.fns + self.impls + self.traits + self.externs + self.macro_tokens
    }
    fn add(&mut self, other: &Census) {
        self.blocks += other.blocks;
        self.fns += other.fns;
        self.impls += other.impls;
        self.traits += other.traits;
        self.externs += other.externs;
        self.macro_tokens += other.macro_tokens;
    }
}

fn is_test_scoped(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        if attr.path().is_ident("test") {
            return true;
        }
        attr.path().is_ident("cfg")
            && attr
                .meta
                .require_list()
                .map(|list| cfg_is_test_only(&list.tokens))
                .unwrap_or(false)
    })
}

/// Whether a `cfg(...)` predicate can only hold in test builds: exactly
/// `test`, or `all(...)` with a test-only conjunct (recursively).
/// `any(test, ...)` and `not(test)` can hold in production builds
/// (notably `feature = "test-support"`), so they never hide a site.
fn cfg_is_test_only(tokens: &proc_macro2::TokenStream) -> bool {
    let trees: Vec<proc_macro2::TokenTree> = tokens.clone().into_iter().collect();
    match trees.as_slice() {
        [proc_macro2::TokenTree::Ident(ident)] => ident == "test",
        [proc_macro2::TokenTree::Ident(ident), proc_macro2::TokenTree::Group(group)]
            if ident == "all" =>
        {
            split_commas(&group.stream()).iter().any(cfg_is_test_only)
        }
        _ => false,
    }
}

/// Split a token stream at top-level commas.
fn split_commas(tokens: &proc_macro2::TokenStream) -> Vec<proc_macro2::TokenStream> {
    let mut parts = Vec::new();
    let mut current = proc_macro2::TokenStream::new();
    for tree in tokens.clone() {
        match &tree {
            proc_macro2::TokenTree::Punct(punct) if punct.as_char() == ',' => {
                parts.push(std::mem::take(&mut current));
            }
            _ => current.extend([tree]),
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

fn count_macro_unsafe(tokens: &proc_macro2::TokenStream) -> usize {
    tokens
        .clone()
        .into_iter()
        .map(|tree| match tree {
            proc_macro2::TokenTree::Ident(ident) => usize::from(ident == "unsafe"),
            proc_macro2::TokenTree::Group(group) => count_macro_unsafe(&group.stream()),
            _ => 0,
        })
        .sum()
}

#[derive(Default)]
struct Counter {
    census: Census,
}

impl<'ast> Visit<'ast> for Counter {
    fn visit_expr_unsafe(&mut self, node: &'ast syn::ExprUnsafe) {
        self.census.blocks += 1;
        syn::visit::visit_expr_unsafe(self, node);
    }
    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        if is_test_scoped(&node.attrs) {
            return;
        }
        if node.sig.unsafety.is_some() {
            self.census.fns += 1;
        }
        syn::visit::visit_item_fn(self, node);
    }
    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        if is_test_scoped(&node.attrs) {
            return;
        }
        if node.sig.unsafety.is_some() {
            self.census.fns += 1;
        }
        syn::visit::visit_impl_item_fn(self, node);
    }
    fn visit_trait_item_fn(&mut self, node: &'ast syn::TraitItemFn) {
        if is_test_scoped(&node.attrs) {
            return;
        }
        if node.sig.unsafety.is_some() {
            self.census.fns += 1;
        }
        syn::visit::visit_trait_item_fn(self, node);
    }
    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        if is_test_scoped(&node.attrs) {
            return;
        }
        if node.unsafety.is_some() {
            self.census.impls += 1;
        }
        syn::visit::visit_item_impl(self, node);
    }
    fn visit_item_trait(&mut self, node: &'ast syn::ItemTrait) {
        if is_test_scoped(&node.attrs) {
            return;
        }
        if node.unsafety.is_some() {
            self.census.traits += 1;
        }
        syn::visit::visit_item_trait(self, node);
    }
    fn visit_item_foreign_mod(&mut self, node: &'ast syn::ItemForeignMod) {
        if is_test_scoped(&node.attrs) {
            return;
        }
        if node.unsafety.is_some() {
            self.census.externs += 1;
        }
        syn::visit::visit_item_foreign_mod(self, node);
    }
    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        if is_test_scoped(&node.attrs) {
            return;
        }
        syn::visit::visit_item_mod(self, node);
    }
    fn visit_item_macro(&mut self, node: &'ast syn::ItemMacro) {
        if is_test_scoped(&node.attrs) {
            return;
        }
        self.census.macro_tokens += count_macro_unsafe(&node.mac.tokens);
    }
    fn visit_expr_macro(&mut self, node: &'ast syn::ExprMacro) {
        if is_test_scoped(&node.attrs) {
            return;
        }
        self.census.macro_tokens += count_macro_unsafe(&node.mac.tokens);
    }
    fn visit_stmt_macro(&mut self, node: &'ast syn::StmtMacro) {
        if is_test_scoped(&node.attrs) {
            return;
        }
        self.census.macro_tokens += count_macro_unsafe(&node.mac.tokens);
    }
}

fn census_source(source: &str) -> Result<Census, String> {
    let file = syn::parse_file(source).map_err(|err| err.to_string())?;
    let mut counter = Counter::default();
    if !is_test_scoped(&file.attrs) {
        counter.visit_file(&file);
    }
    Ok(counter.census)
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir)
        .map_err(|err| format!("cannot read {}: {err}", dir.display()))?;
    let mut paths: Vec<PathBuf> = entries
        .map(|entry| entry.map(|e| e.path()))
        .collect::<Result<_, _>>()
        .map_err(|err| format!("cannot list {}: {err}", dir.display()))?;
    paths.sort();
    for path in paths {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if path.is_dir() {
            if name != "tests" {
                collect_rs_files(&path, out)?;
            }
        } else if name.ends_with(".rs") && !name.ends_with("_tests.rs") {
            out.push(path);
        }
    }
    Ok(())
}

fn census_tree(src: &Path) -> Result<BTreeMap<String, Census>, String> {
    let mut files = Vec::new();
    collect_rs_files(src, &mut files)?;
    if files.is_empty() {
        return Err(format!("no Rust sources under {}", src.display()));
    }
    let mut per_file = BTreeMap::new();
    for path in files {
        let rel = path
            .strip_prefix(src)
            .map_err(|_| format!("{} escapes {}", path.display(), src.display()))?
            .to_string_lossy()
            .replace('\\', "/");
        let source = std::fs::read_to_string(&path)
            .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
        let census = census_source(&source)
            .map_err(|err| format!("cannot parse {}: {err}", path.display()))?;
        if census.total() > 0 {
            per_file.insert(rel, census);
        }
    }
    Ok(per_file)
}

fn totals(per_file: &BTreeMap<String, Census>) -> Census {
    per_file.values().fold(Census::default(), |mut acc, c| {
        acc.add(c);
        acc
    })
}

struct Baseline {
    counts: BTreeMap<String, usize>,
    /// path -> (justified count, reason). Written only by
    /// `--write-baseline --justify`; the count must equal the baseline
    /// count for that path or the justification is stale and refuses.
    justifies: BTreeMap<String, (usize, String)>,
}

fn render_baseline(per_file: &BTreeMap<String, Census>, justifies: &BTreeMap<String, (usize, String)>) -> String {
    let total = totals(per_file);
    let mut out = String::new();
    out.push_str("# LinuxEmu unsafe census baseline. Regenerated only by\n");
    out.push_str("# scripts/check-linuxemu-unsafe.rs --write-baseline in the same diff\n");
    out.push_str("# that moves unsafe; see that file for the counting method.\n");
    out.push_str("# An increase for a file requires a justify line below,\n");
    out.push_str("# written with --justify <path>=<reason> and judged by the seat.\n");
    out.push_str(&format!("total {}\n", total.total()));
    for (path, census) in per_file {
        out.push_str(&format!("{path} {}\n", census.total()));
    }
    for (path, (count, reason)) in justifies {
        out.push_str(&format!("justify {path} {count} {reason}\n"));
    }
    out
}

fn parse_baseline(text: &str) -> Result<Baseline, String> {
    let mut per_file = BTreeMap::new();
    let mut justifies: BTreeMap<String, (usize, String)> = BTreeMap::new();
    let mut declared_total: Option<usize> = None;
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("justify ") {
            let mut fields = rest.splitn(3, ' ');
            let path = fields.next().filter(|p| !p.is_empty());
            let count: Option<usize> = fields.next().and_then(|n| n.parse().ok());
            let reason = fields.next().filter(|r| !r.trim().is_empty());
            let (Some(path), Some(count), Some(reason)) = (path, count, reason) else {
                return Err(format!(
                    "baseline line {}: justify needs <path> <count> <reason>",
                    index + 1
                ));
            };
            if justifies
                .insert(path.to_string(), (count, reason.trim().to_string()))
                .is_some()
            {
                return Err(format!("baseline line {}: duplicate justify {path}", index + 1));
            }
            continue;
        }
        let mut parts = line.rsplitn(2, ' ');
        let count: usize = parts
            .next()
            .and_then(|n| n.parse().ok())
            .ok_or_else(|| format!("baseline line {}: bad count", index + 1))?;
        let key = parts
            .next()
            .filter(|k| !k.is_empty())
            .ok_or_else(|| format!("baseline line {}: missing path", index + 1))?;
        if key == "total" {
            declared_total = Some(count);
        } else if per_file.insert(key.to_string(), count).is_some() {
            return Err(format!("baseline line {}: duplicate {key}", index + 1));
        }
    }
    for (path, (count, _)) in &justifies {
        match per_file.get(path) {
            Some(current) if current == count => {}
            _ => {
                return Err(format!(
                    "stale justify for {path}: justified {count}, baseline {}",
                    per_file.get(path).copied().unwrap_or(0)
                ));
            }
        }
    }
    let summed: usize = per_file.values().sum();
    match declared_total {
        Some(total) if total == summed => Ok(Baseline { counts: per_file, justifies }),
        Some(total) => Err(format!("baseline total {total} != summed {summed}")),
        None => Err("baseline has no total line".to_string()),
    }
}

fn negative_controls() -> Result<(), String> {
    let cases: &[(&str, &str, Census)] = &[
        (
            "plain block",
            "fn f() { unsafe {} }",
            Census { blocks: 1, ..Census::default() },
        ),
        (
            "nested blocks count twice",
            "fn f() { unsafe { unsafe {} } }",
            Census { blocks: 2, ..Census::default() },
        ),
        (
            "unsafe fn and impl",
            "unsafe fn f() {} struct S; unsafe impl Send for S {}",
            Census { fns: 1, impls: 1, ..Census::default() },
        ),
        (
            "cfg(test) excluded",
            "#[cfg(test)] mod tests { fn f() { unsafe {} } }",
            Census::default(),
        ),
        (
            "test fn excluded",
            "#[test] fn t() { unsafe {} }",
            Census::default(),
        ),
        (
            "cfg(all(test, ...)) excluded",
            "#[cfg(all(test, feature = \"x\"))] fn t() { unsafe {} }",
            Census::default(),
        ),
        (
            "cfg(not(test)) counted",
            "#[cfg(not(test))] fn t() { unsafe {} }",
            Census { blocks: 1, ..Census::default() },
        ),
        (
            "cfg(any(test, ...)) counted",
            "#[cfg(any(test, feature = \"test-support\"))] fn t() { unsafe {} }",
            Census { blocks: 1, ..Census::default() },
        ),
        (
            "cfg(test) impl-item fn excluded",
            "struct S; impl S { #[cfg(test)] fn t(&self) { unsafe {} } }",
            Census::default(),
        ),
        (
            "macro token counted",
            "macro_rules! m { () => { unsafe {} } } fn f() { m!() }",
            Census { macro_tokens: 1, ..Census::default() },
        ),
        (
            "comment and string not counted",
            "// unsafe\nfn f() { let _ = \"unsafe\"; }",
            Census::default(),
        ),
    ];
    for (name, source, want) in cases {
        let got = census_source(source)?;
        if got != *want {
            return Err(format!(
                "negative control {name}: got total {}, want {}",
                got.total(),
                want.total()
            ));
        }
    }
    Ok(())
}

struct Args {
    repo_root: PathBuf,
    linuxemu_src: Option<PathBuf>,
    baseline: Option<PathBuf>,
    write_baseline: bool,
    init: bool,
    justifies: Vec<(String, String)>,
    skip_negative_control: bool,
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut args = Args {
        repo_root: PathBuf::from("."),
        linuxemu_src: None,
        baseline: None,
        write_baseline: false,
        init: false,
        justifies: Vec::new(),
        skip_negative_control: false,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!(
                    "usage: check-linuxemu-unsafe.rs [--repo-root PATH] \
                     [--linuxemu-src PATH] [--baseline PATH] [--write-baseline [--init]] \
                     [--justify <path>=<reason>]... [--skip-negative-control]"
                );
                return Ok(None);
            }
            "--justify" => {
                let spec = iter.next().ok_or("--justify needs <path>=<reason>")?;
                let (path, reason) = spec
                    .split_once('=')
                    .filter(|(p, r)| !p.is_empty() && !r.trim().is_empty())
                    .ok_or_else(|| format!("--justify needs <path>=<reason>, got {spec:?}"))?;
                args.justifies.push((path.to_string(), reason.trim().to_string()));
            }
            "--repo-root" => {
                args.repo_root = PathBuf::from(iter.next().ok_or("--repo-root needs a path")?);
            }
            "--linuxemu-src" => {
                args.linuxemu_src =
                    Some(PathBuf::from(iter.next().ok_or("--linuxemu-src needs a path")?));
            }
            "--baseline" => {
                args.baseline =
                    Some(PathBuf::from(iter.next().ok_or("--baseline needs a path")?));
            }
            "--write-baseline" => args.write_baseline = true,
            "--init" => args.init = true,
            "--skip-negative-control" => args.skip_negative_control = true,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(Some(args))
}

fn locate_src(args: &Args) -> Result<PathBuf, String> {
    if let Some(src) = &args.linuxemu_src {
        return Ok(src.clone());
    }
    let candidates = [
        args.repo_root.join("reverie/reverie-linuxemu/src"),
        args.repo_root.join("../reverie/reverie-linuxemu/src"),
    ];
    candidates
        .into_iter()
        .find(|path| path.is_dir())
        .ok_or_else(|| {
            format!(
                "cannot find reverie-linuxemu/src under {} (pass --linuxemu-src)",
                args.repo_root.display()
            )
        })
}

/// What happened to the base-revision (HEAD) comparison. It only
/// catches UNCOMMITTED hand edits: wherever this checker itself is
/// the gate (CI, validation, export) HEAD already contains the
/// edited baseline, so a committed raise is enforced by the seat's
/// review of its justify lines, not here. The real base comparison
/// (explicit merge-base revision) is F4 follow-up dh-l4tx3z.
enum BaseComparison {
    Compared(Baseline),
    Skipped(&'static str),
}

fn base_baseline(repo_root: &Path, baseline_path: &Path) -> Result<BaseComparison, String> {
    let root = repo_root
        .canonicalize()
        .map_err(|err| format!("cannot resolve repo root {}: {err}", repo_root.display()))?;
    let top = std::process::Command::new("git")
        .args(["-C"])
        .arg(&root)
        .args(["rev-parse", "--show-toplevel"])
        .output();
    let Ok(top) = top else {
        return Ok(BaseComparison::Skipped("git unavailable"));
    };
    if !top.status.success() {
        return Ok(BaseComparison::Skipped(
            "repo root is not a git top-level (Sapling scope)",
        ));
    }
    let top_path = PathBuf::from(String::from_utf8_lossy(&top.stdout).trim());
    let Ok(top_path) = top_path.canonicalize() else {
        return Ok(BaseComparison::Skipped(
            "repo root is not a git top-level (Sapling scope)",
        ));
    };
    if top_path != root {
        return Ok(BaseComparison::Skipped(
            "repo root is not a git top-level (Sapling scope)",
        ));
    }
    let absolute_baseline = if baseline_path.is_absolute() {
        baseline_path.to_path_buf()
    } else {
        root.join(baseline_path)
    };
    let Ok(relative) = absolute_baseline.strip_prefix(&root) else {
        return Ok(BaseComparison::Skipped("baseline outside the git top-level"));
    };
    let shown = std::process::Command::new("git")
        .args(["-C"])
        .arg(&root)
        .arg("show")
        .arg(format!("HEAD:{}", relative.to_string_lossy().replace('\\', "/")))
        .output()
        .map_err(|err| format!("cannot run git show: {err}"))?;
    if !shown.status.success() {
        return Ok(BaseComparison::Skipped("baseline not at HEAD (new file)"));
    }
    let text = String::from_utf8(shown.stdout)
        .map_err(|err| format!("HEAD baseline is not UTF-8: {err}"))?;
    parse_baseline(&text).map(BaseComparison::Compared)
}

fn run() -> u8 {
    let args = match parse_args() {
        Ok(Some(args)) => args,
        Ok(None) => return PASS,
        Err(err) => {
            eprintln!("CHECKER ERROR: {err}");
            return CHECKER_ERROR;
        }
    };
    if !args.skip_negative_control {
        if let Err(err) = negative_controls() {
            eprintln!("CHECKER ERROR: {err}");
            return CHECKER_ERROR;
        }
    }
    let src = match locate_src(&args) {
        Ok(src) => src,
        Err(err) => {
            eprintln!("CHECKER ERROR: {err}");
            return CHECKER_ERROR;
        }
    };
    let per_file = match census_tree(&src) {
        Ok(map) => map,
        Err(err) => {
            eprintln!("CHECKER ERROR: {err}");
            return CHECKER_ERROR;
        }
    };
    let baseline_path = args
        .baseline
        .clone()
        .unwrap_or_else(|| args.repo_root.join("scripts/linuxemu-unsafe-baseline.txt"));
    let actual: BTreeMap<String, usize> = per_file
        .iter()
        .map(|(path, census)| (path.clone(), census.total()))
        .collect();
    let old: Option<Baseline> = match std::fs::read_to_string(&baseline_path) {
        Ok(text) => match parse_baseline(&text) {
            Ok(parsed) => Some(parsed),
            Err(err) => {
                eprintln!("CHECKER ERROR: {err}");
                return CHECKER_ERROR;
            }
        },
        Err(_) if args.write_baseline => None,
        Err(err) => {
            eprintln!(
                "CHECKER ERROR: cannot read {}: {err}",
                baseline_path.display()
            );
            return CHECKER_ERROR;
        }
    };
    if args.init && !args.write_baseline {
        eprintln!("CHECKER ERROR: --init is only valid with --write-baseline");
        return CHECKER_ERROR;
    }
    // Base-revision (HEAD) baseline in git scope; see base_baseline.
    // Always print which comparison ran: a skip must never read as a
    // compared pass.
    let base = match base_baseline(&args.repo_root, &baseline_path) {
        Ok(BaseComparison::Compared(base)) => {
            println!("BASE: HEAD baseline compared (uncommitted edits only)");
            Some(base)
        }
        Ok(BaseComparison::Skipped(reason)) => {
            println!("BASE: skipped: {reason}");
            None
        }
        Err(err) => {
            eprintln!("CHECKER ERROR: {err}");
            return CHECKER_ERROR;
        }
    };
    if args.write_baseline {
        if old.is_none() && !args.init {
            eprintln!(
                "CHECKER ERROR: no baseline at {}; refusing to create one \
                 without --init (a deleted baseline must not silently \
                 re-baseline the tree)",
                baseline_path.display()
            );
            return CHECKER_ERROR;
        }
        if old.is_some() && args.init {
            eprintln!(
                "CHECKER ERROR: --init with an existing baseline at {}",
                baseline_path.display()
            );
            return CHECKER_ERROR;
        }
        // Ratchet gate: raising any file's count over the stricter of
        // the working and base baselines (a new file counts as raising
        // from 0) requires --justify for that file. Decreases and equal
        // counts write freely (locking a decrease in needs no reason);
        // justifications whose count no longer matches are dropped.
        let mut justifies: BTreeMap<String, (usize, String)> = old
            .as_ref()
            .map(|old| old.justifies.clone())
            .unwrap_or_default();
        for (path, reason) in &args.justifies {
            let Some(count) = actual.get(path).copied() else {
                eprintln!("CHECKER ERROR: --justify {path}: no such file in census");
                return CHECKER_ERROR;
            };
            justifies.insert(path.clone(), (count, reason.clone()));
        }
        if !args.init {
            let mut unjustified = Vec::new();
            for (path, count) in &actual {
                let before = [
                    old.as_ref().and_then(|old| old.counts.get(path).copied()),
                    base.as_ref().and_then(|base| base.counts.get(path).copied()),
                ]
                .into_iter()
                .flatten()
                .max()
                .unwrap_or(0);
                if *count > before && justifies.get(path).map(|(c, _)| *c) != Some(*count) {
                    unjustified.push(format!("{path}: {before} -> {count}"));
                }
            }
            if !unjustified.is_empty() {
                eprintln!(
                    "CHECKER ERROR: refusing to raise the baseline without \
                     --justify <path>=<reason> for: {}",
                    unjustified.join(", ")
                );
                return CHECKER_ERROR;
            }
        }
        justifies.retain(|path, (count, _)| actual.get(path).copied() == Some(*count));
        let rendered = render_baseline(&per_file, &justifies);
        if let Err(err) = std::fs::write(&baseline_path, rendered) {
            eprintln!("CHECKER ERROR: cannot write {}: {err}", baseline_path.display());
            return CHECKER_ERROR;
        }
        println!(
            "WROTE {} ({} sites in {} files)",
            baseline_path.display(),
            totals(&per_file).total(),
            per_file.len()
        );
        return PASS;
    }
    let Some(baseline) = old else {
        eprintln!("CHECKER ERROR: no baseline at {}", baseline_path.display());
        return CHECKER_ERROR;
    };
    let paths: std::collections::BTreeSet<&String> =
        actual.keys().chain(baseline.counts.keys()).collect();
    let mut refused = false;
    for path in paths {
        let got = actual.get(path).copied().unwrap_or(0);
        let want = baseline.counts.get(path).copied().unwrap_or(0);
        if got > want {
            eprintln!("REFUSE: {path}: census {got} exceeds baseline {want}");
            refused = true;
        } else if got < want {
            eprintln!(
                "REFUSE: {path}: census {got} is below baseline {want}; \
                 run --write-baseline to lock in the decrease"
            );
            refused = true;
        }
    }
    // Base-revision gate (git scope, uncommitted edits only — see
    // BaseComparison): a working-baseline count raised over HEAD
    // without a matching justification refuses here even though
    // census == working baseline.
    if let Some(base) = &base {
        for (path, count) in &baseline.counts {
            let before = base.counts.get(path).copied().unwrap_or(0);
            if *count > before
                && baseline.justifies.get(path).map(|(c, _)| *c) != Some(*count)
            {
                eprintln!(
                    "REFUSE: {path}: baseline {count} raised over HEAD {before} \
                     without a justification"
                );
                refused = true;
            }
        }
    }
    if refused {
        eprintln!(
            "REFUSE: unsafe census differs from {}; regenerate it in the same \
             diff (--write-baseline, with --justify <path>=<reason> per raised \
             file) and state the delta for the seat",
            baseline_path.display()
        );
        return REFUSE;
    }
    let total = totals(&per_file);
    println!(
        "PASS: LinuxEmu unsafe census matches baseline ({} sites outside tests \
         in {} files; blocks {}, fns {}, impls {}, traits {}, externs {}, \
         macro-tokens {})",
        total.total(),
        per_file.len(),
        total.blocks,
        total.fns,
        total.impls,
        total.traits,
        total.externs,
        total.macro_tokens
    );
    PASS
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    ExitCode::from(run())
}
