#!/usr/bin/env -S rust-script --force
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Refuse an x86-64 binary that contains instructions above x86-64-v2.
//!
//! Hermit presents an x86-64-v2 CPU to its guests by default and must
//! itself still start on a real v2 host. A build tuned for its own host
//! (`-C target-cpu=native`, an x86-64-v3 `target-cpu`, or a C compiler's
//! `-march=native` in a build script — the default on CentOS 10
//! toolchains) can emit v3 instructions, which a v2 host answers with
//! SIGILL. This is the x86 counterpart of `scripts/check-no-sve.rs`,
//! with the same exit-status contract.
//!
//! The check reads `objdump -d` and classifies every instruction by
//! its encoded bytes, not by trusting the mnemonic, so an older
//! objdump printing an unknown form as `.byte` soup cannot hide a VEX
//! or EVEX instruction. Two classes:
//!
//! - **Above v2** (a failure): after any legacy prefixes and a REX
//!   byte, a first opcode byte of `C5`/`C4` is VEX (AVX, AVX2, FMA,
//!   BMI1/2, F16C forms) and `62` is EVEX (AVX-512); `0F 38 F0`/`F1`
//!   without an F2 prefix is MOVBE. As a backstop, a mnemonic
//!   beginning with `v` also counts: every VEX/EVEX mnemonic does,
//!   and the only legacy v-initial mnemonics, `verr`/`verw`, are
//!   exempted. (`F2 0F 38 F1` is CRC32 — SSE4.2, v2 — and is not
//!   flagged.)
//! - **BSF-alias encodings** (counted, not a failure): only
//!   `F3 0F BC` (TZCNT). On a CPU without BMI1 it decodes as
//!   `REP BSF`, which returns the same value as TZCNT for every
//!   nonzero input; only the zero-input result differs, and the
//!   compiler idioms that emit it (Rust core's `trailing_zeros`,
//!   glibc's string functions) compensate for exactly that case.
//!   It cannot SIGILL, so it is reported as a count for visibility
//!   and never fails the check. `F3 0F BD` (LZCNT) is *not* in this
//!   class: without LZCNT it executes as BSR, which returns the
//!   highest set bit's index rather than the leading-zero count on
//!   every nonzero input, so an LZCNT-only creep would silently
//!   miscompute on a v2 host instead of faulting. LZCNT is above
//!   v2. `F3 0F B8` (POPCNT) is v2 proper and is not counted
//!   either.
//!
//! A hit in code that only runs after a runtime feature check — the
//! AVX2 variants inside `memchr` or `aho-corasick`, reached through
//! `is_x86_feature_detected!("avx2")` — is still an above-v2 hit: a
//! scan cannot tell it from code that runs everywhere, and silently
//! passing a binary whose *unconditional* code crept to v3 is the
//! failure this script exists to prevent. Disposition known dispatch
//! code explicitly with `--allow-symbol <exact demangled symbol>`
//! (repeatable) or `--allow-symbol-file <path>` (one exact symbol
//! per line, `#` comments): hits inside an exactly named symbol
//! are tallied as allowlisted and do not fail. Matching is exact
//! on purpose — a substring entry would silently allowlist the
//! next symbol that happens to share it. The allowlist lives
//! where the check is wired in, reviewed like any other
//! exception; growing it is a visible act. `--hit-symbols` prints
//! the per-symbol counts the allowlist is maintained from.
//! Symbols are named as a Rust-v0-aware `objdump --demangle`
//! (binutils 2.36 or later) prints them: an older objdump prints
//! v0 names (`_R...`) still mangled, so the script demangles those
//! itself, and an allowlist entry matches the same symbol on every
//! host. A v0 name it cannot decode stays as objdump printed it,
//! and the count of such names goes to stderr. AMD
//! XOP/3DNow! encodings are not classified; no supported
//! toolchain emits them.
//!
//! Scope (owner rulings 6488/6489, 2026-11-15): this check is
//! NIGHTLY ONLY, never land-blocking. The zero-hit gate binds the
//! generic (OSS Cargo) build — what a user on an old machine
//! runs. **The fbsource/fbpkg build is exempt from the zero-hit
//! requirement by name**: fbcode deliberately targets v3-class
//! CPUs, Meta's fleet has no v2 hosts, and changing fbcode's arch
//! flags for one binary is the wrong trade. The Buck-built binary
//! is scanned nightly instead against a counted baseline
//! (`--baseline-count`, its current recorded count in
//! `fb-only/contract_checks/check_no_v3.sh`): growth past the
//! baseline fails the nightly, so new v3 creep is visible even
//! while the existing whole-program v3 codegen is dispositioned.
//! A count is a growth alarm only: it CANNOT detect swaps —
//! one v3 instruction added and one removed, at equal count,
//! passes it. The precise parts of this gate are the
//! exact-symbol allowlist (in the zero-hit scan a new dispatched
//! symbol must be named to pass; in the Buck baseline scan an
//! unnamed symbol merely adds to the count) and, for the guest
//! CPUID table, the probe's named LEAK lines; the count exists
//! to catch bulk growth between allowlist reviews, nothing more.
//! The day that scan reaches zero, the exemption is dropped.
//! The exact-symbol allowlist shared by both scans is
//! `scripts/check-no-v3-allowlist.txt`; in words it covers
//! aho-corasick's Teddy AVX2 searchers, memchr's and chacha20's
//! AVX2 kernels, `core::arch` AVX intrinsic wrappers at
//! dispatched call sites (all reached only after runtime
//! feature detection), and
//! `hermit_xinuse_*` — the x86-64-v3 startup qualification probe
//! (`hermit-cli/src/xstate_probe/`), whose EVEX/AVX-512 arms are
//! reachable only after `cpu_profile_admission::check_host_cpuid`
//!   has verified every advertised v3 bit including OSXSAVE and
//!   `xstate_probe::check_xcr0` has required AVX (and, for the
//!   AVX-512 cases, every AVX-512 XCR0 component); the assembly
//!   branches on the same admission flags before any EVEX
//!   instruction. On a v2 host an explicit v3 run is refused by
//!   name before the probe starts, and default v2 runs never probe.
//!
//! Scheduling, stated plainly: the Buck-built scan is wired as a
//! nightly-scheduled target in fb-only/contract_checks. The
//! Cargo-built scan has NO validate-DAG node yet: adding the
//! super-profile (nightly super validate) node that runs this
//! script over the Cargo-built hermit is a follow-up, recorded
//! here so the claim is not mistaken for coverage that exists.
//!
//! Usage: `scripts/check-no-v3.rs [--allow-symbol SYMBOL]...
//! [--allow-symbol-file PATH] [--baseline-count N] [--hit-symbols]
//! BINARY...`
//!
//! Exit status:
//! - 0: no binary has a non-allowlisted above-v2 instruction, or
//!   its count is within `--baseline-count` when one is given;
//! - 1: a binary exceeds that (its first hits are printed, with symbols);
//! - 2: objdump could not disassemble a binary, which says nothing either way.

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::process::Command;
use std::process::ExitCode;

/// How many hits to print per binary.
const SHOWN: usize = 5;

fn usage() -> ExitCode {
    eprintln!(
        "usage: scripts/check-no-v3.rs [--allow-symbol SYMBOL]... \
         [--allow-symbol-file PATH] [--baseline-count N] [--hit-symbols] BINARY..."
    );
    ExitCode::from(2)
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    let mut allow: Vec<String> = Vec::new();
    let mut binaries: Vec<String> = Vec::new();
    let mut baseline: Option<usize> = None;
    let mut hit_symbols = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--allow-symbol" {
            match value_arg(&mut args, &arg) {
                Some(symbol) => allow.push(symbol),
                None => return ExitCode::from(2),
            }
        } else if let Some(symbol) = arg.strip_prefix("--allow-symbol=") {
            allow.push(symbol.to_string());
        } else if arg == "--allow-symbol-file" {
            let Some(path) = value_arg(&mut args, &arg) else {
                return ExitCode::from(2);
            };
            match read_allowlist_file(&path) {
                Ok(symbols) => allow.extend(symbols),
                Err(message) => {
                    eprintln!("check-no-v3: {message}");
                    return ExitCode::from(2);
                }
            }
        } else if arg == "--baseline-count" {
            let Some(value) = value_arg(&mut args, &arg) else {
                return ExitCode::from(2);
            };
            let Some(parsed) = parse_baseline(&value) else {
                eprintln!("check-no-v3: --baseline-count needs a count, got `{value}`");
                return ExitCode::from(2);
            };
            baseline = Some(parsed);
        } else if let Some(value) = arg.strip_prefix("--baseline-count=") {
            let Some(parsed) = parse_baseline(value) else {
                eprintln!("check-no-v3: --baseline-count needs a count, got `{value}`");
                return ExitCode::from(2);
            };
            baseline = Some(parsed);
        } else if arg == "--hit-symbols" {
            hit_symbols = true;
        } else if arg.starts_with("--") {
            eprintln!("check-no-v3: unknown flag `{arg}`");
            return usage();
        } else {
            binaries.push(arg);
        }
    }
    if binaries.is_empty() {
        return usage();
    }
    let allow: std::collections::BTreeSet<&str> = allow.iter().map(String::as_str).collect();
    let mut status = 0;
    for binary in &binaries {
        match scan(binary) {
            Ok(report) => {
                if report.undecoded > 0 {
                    eprintln!(
                        "check-no-v3: {binary}: {} Rust v0 symbol names stayed mangled; \
                         no allowlist entry can match them",
                        report.undecoded
                    );
                }
                if hit_symbols {
                    for (count, symbol) in hit_symbol_counts(&report) {
                        println!("{count} {symbol}");
                    }
                    continue;
                }
                let (hits, allowed) = partition_hits(&report, &allow);
                if within_baseline(hits.len(), baseline) {
                    if baseline.is_none() {
                        println!(
                            "check-no-v3: {binary}: no instructions above x86-64-v2 \
                             ({} allowlisted, {} tzcnt alias encodings)",
                            allowed.len(),
                            report.alias
                        );
                    } else {
                        println!(
                            "check-no-v3: {binary}: {} instructions above x86-64-v2, \
                             within baseline {} \
                             ({} allowlisted, {} tzcnt alias encodings)",
                            hits.len(),
                            baseline.unwrap_or(0),
                            allowed.len(),
                            report.alias
                        );
                    }
                } else {
                    println!(
                        "check-no-v3: {binary}: FAILED -- {} instructions above x86-64-v2{} \
                         ({} allowlisted, {} tzcnt alias encodings), first:",
                        hits.len(),
                        baseline.map_or(String::new(), |b| format!(", baseline is {b}")),
                        allowed.len(),
                        report.alias
                    );
                    for line in hits.iter().take(SHOWN) {
                        println!("  {}", line.text);
                    }
                    status = status.max(1);
                }
            }
            Err(message) => {
                eprintln!("check-no-v3: {binary}: COULD NOT DETERMINE -- {message}");
                status = 2;
            }
        }
    }
    ExitCode::from(status)
}

fn value_arg<I: Iterator<Item = String>>(args: &mut I, flag: &str) -> Option<String> {
    let value = args.next();
    if value.is_none() {
        eprintln!("check-no-v3: {flag} needs a value");
    }
    value
}

fn parse_baseline(value: &str) -> Option<usize> {
    value.replace('_', "").parse().ok()
}

/// Whether `count` passes under `baseline` (no baseline: only zero).
fn within_baseline(count: usize, baseline: Option<usize>) -> bool {
    count <= baseline.unwrap_or(0)
}

/// The exact symbols of an allowlist file: one per line, blank
/// lines and `#` comments skipped, surrounding space trimmed.
fn read_allowlist_file(path: &str) -> Result<Vec<String>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read allowlist file {path}: {error}"))?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect())
}

/// Above-v2 hits per symbol, most first, for allowlist upkeep.
fn hit_symbol_counts(report: &Report) -> Vec<(usize, String)> {
    let mut by_symbol: std::collections::BTreeMap<&str, usize> = Default::default();
    for hit in &report.hits {
        *by_symbol.entry(hit.symbol.as_str()).or_default() += 1;
    }
    let mut counts: Vec<(usize, String)> = by_symbol
        .into_iter()
        .map(|(symbol, count)| {
            (
                count,
                if symbol.is_empty() {
                    "<no symbol>".to_string()
                } else {
                    symbol.to_string()
                },
            )
        })
        .collect();
    counts.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    counts
}

/// One above-v2 hit: its disassembly line and the symbol it sits in
/// (empty when objdump shows none, e.g. a stripped binary).
struct Hit {
    symbol: String,
    text: String,
}

struct Report {
    hits: Vec<Hit>,
    alias: usize,
    /// Rust v0 symbol names left mangled (see `header_symbol`).
    undecoded: usize,
}

/// The hits outside the allowlist, then the allowlisted ones.
fn partition_hits<'r>(
    report: &'r Report,
    allow: &std::collections::BTreeSet<&str>,
) -> (Vec<&'r Hit>, Vec<&'r Hit>) {
    report
        .hits
        .iter()
        .partition(|hit| !allow.contains(hit.symbol.as_str()))
}

fn scan(binary: &str) -> Result<Report, String> {
    let output = Command::new("objdump")
        .args(["-d", "--demangle", binary])
        .output()
        .map_err(|error| format!("objdump could not run: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "objdump exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(report_from_disassembly(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

/// Classify every instruction of `objdump -d --demangle` output.
fn report_from_disassembly(text: &str) -> Report {
    let mut symbol = String::new();
    let mut report = Report {
        hits: Vec::new(),
        alias: 0,
        undecoded: 0,
    };
    for line in text.lines() {
        if let Some(name) = symbol_header(line) {
            symbol = match header_symbol(name) {
                Some(symbol) => symbol,
                None => {
                    report.undecoded += 1;
                    name.to_string()
                }
            };
            continue;
        }
        match classify(line) {
            Class::AboveV2 => report.hits.push(Hit {
                symbol: symbol.clone(),
                text: format!("{symbol} {}", line.trim()),
            }),
            Class::Alias => report.alias += 1,
            Class::Clean => {}
        }
    }
    report
}

/// The symbol a header names, as a v0-aware objdump prints it:
/// `None` for a Rust v0 name (`_R...`) that `demangle_v0` cannot
/// decode. Every other name is kept as objdump printed it.
fn header_symbol(name: &str) -> Option<String> {
    if !name.starts_with("_R") {
        return Some(name.to_string());
    }
    demangle_v0(name)
}

/// Demangle a Rust v0 symbol name (`_R...`) the way binutils 2.36
/// and later print it with `objdump --demangle`: no crate
/// disambiguator hashes, no type suffix on const generic values,
/// and any `.suffix` (`.llvm.N`, `.cold`) dropped. `None` for a
/// name that is not v0 or uses a form decoded here only partly:
/// punycode identifiers, consts other than integers, bool, char and
/// the placeholder `_`, integer consts of more than 16 hex digits and
/// char consts of more than 8 (binutils leaves the second mangled
/// too), more than 200 levels of nesting (binutils allows about
/// 1024), and names whose demangling exceeds `MAX_DEMANGLED_BYTES` or
/// `MAX_DEMANGLE_STEPS`.
fn demangle_v0(symbol: &str) -> Option<String> {
    let mangled = symbol.strip_prefix("_R")?;
    let mangled = mangled.split('.').next().unwrap_or(mangled);
    let sym = mangled.as_bytes();
    // A leading decimal is an encoding version other than v0.
    if sym.first()?.is_ascii_digit()
        || !sym
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
    {
        return None;
    }
    let mut demangler = V0 {
        sym,
        at: 0,
        out: String::new(),
        depth: 0,
        steps: 0,
        bound: 0,
    };
    demangler.path(true)?;
    if demangler.at < sym.len() {
        // The instantiating crate, which objdump does not print.
        demangler.skip(|d| d.path(false))?;
    }
    (demangler.at == sym.len() && demangler.out.len() <= MAX_DEMANGLED_BYTES)
        .then_some(demangler.out)
}

/// Demangling gives `None` when the output passes this many bytes:
/// checked at each nested parse step and once more at the end.
/// Backrefs let a short name expand exponentially: a chain of
/// tuples whose two elements both refer back to the previous tuple
/// doubles the output at each link. Real symbols stay far below.
const MAX_DEMANGLED_BYTES: usize = 1 << 20;

/// Demangling gives `None` after this many nested parse steps, which
/// bounds the number of steps even where a step prints nothing (as in
/// a skipped impl path, which is parsed and then discarded). Each
/// step does work proportional to at most the symbol's length.
const MAX_DEMANGLE_STEPS: u32 = 1 << 20;

/// A v0 demangler over the bytes after `_R`, following the v0
/// grammar of the Rust symbol-mangling RFC (2603). Every method
/// returns `None` on input it does not decode.
struct V0<'a> {
    sym: &'a [u8],
    at: usize,
    out: String,
    /// Nesting depth, bounded so that recursion stays shallow. The
    /// depth bound does not bound the work: see `steps`.
    depth: u32,
    /// Nested parse steps so far, bounded by `MAX_DEMANGLE_STEPS`;
    /// every recursion, backrefs included, goes through `nest`.
    steps: u32,
    /// Lifetimes bound by the enclosing `for<...>` binders.
    bound: u64,
}

impl<'a> V0<'a> {
    fn peek(&self) -> Option<u8> {
        self.sym.get(self.at).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.at += 1;
        Some(byte)
    }

    fn eat(&mut self, byte: u8) -> bool {
        let found = self.peek() == Some(byte);
        if found {
            self.at += 1;
        }
        found
    }

    fn nest(&mut self, f: impl FnOnce(&mut Self) -> Option<()>) -> Option<()> {
        if self.depth >= 200
            || self.steps >= MAX_DEMANGLE_STEPS
            || self.out.len() > MAX_DEMANGLED_BYTES
        {
            return None;
        }
        self.steps += 1;
        self.depth += 1;
        let result = f(self);
        self.depth -= 1;
        result
    }

    /// Parse with `f` but discard what it prints.
    fn skip(&mut self, f: impl FnOnce(&mut Self) -> Option<()>) -> Option<()> {
        let printed = self.out.len();
        f(self)?;
        self.out.truncate(printed);
        Some(())
    }

    /// `_` is 0; otherwise base-62 digits then `_`, one more than
    /// their value.
    fn base62(&mut self) -> Option<u64> {
        if self.eat(b'_') {
            return Some(0);
        }
        let mut value: u64 = 0;
        loop {
            let digit = match self.bump()? {
                byte @ b'0'..=b'9' => byte - b'0',
                byte @ b'a'..=b'z' => byte - b'a' + 10,
                byte @ b'A'..=b'Z' => byte - b'A' + 36,
                b'_' => return value.checked_add(1),
                _ => return None,
            };
            value = value.checked_mul(62)?.checked_add(u64::from(digit))?;
        }
    }

    /// An optional `tag` base-62 number: 0 when absent, else one
    /// more than its value.
    fn tagged_base62(&mut self, tag: u8) -> Option<u64> {
        if self.eat(tag) {
            self.base62()?.checked_add(1)
        } else {
            Some(0)
        }
    }

    fn decimal(&mut self) -> Option<usize> {
        let start = self.at;
        if self.eat(b'0') {
            return Some(0);
        }
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.at += 1;
        }
        let sym = self.sym;
        std::str::from_utf8(&sym[start..self.at]).ok()?.parse().ok()
    }

    /// An undisambiguated identifier: length, an optional `_`
    /// separator, then the bytes. Punycode (`u`) is not decoded.
    fn ident(&mut self) -> Option<&'a str> {
        if self.peek() == Some(b'u') {
            return None;
        }
        let len = self.decimal()?;
        self.eat(b'_');
        let end = self.at.checked_add(len)?;
        let sym = self.sym;
        let bytes = sym.get(self.at..end)?;
        self.at = end;
        std::str::from_utf8(bytes).ok()
    }

    /// Run `f` at an earlier position (`B` already eaten), then
    /// resume after the backref.
    fn backref(&mut self, f: impl FnOnce(&mut Self) -> Option<()>) -> Option<()> {
        let start = self.at - 1;
        let target = usize::try_from(self.base62()?).ok()?;
        if target >= start {
            return None;
        }
        let resume = self.at;
        self.at = target;
        self.nest(f)?;
        self.at = resume;
        Some(())
    }

    /// Print `item`s until `E`, separated by `separator`.
    fn list(
        &mut self,
        separator: &str,
        mut item: impl FnMut(&mut Self) -> Option<()>,
    ) -> Option<usize> {
        let mut count = 0;
        while !self.eat(b'E') {
            if count > 0 {
                self.out.push_str(separator);
            }
            item(self)?;
            count += 1;
        }
        Some(count)
    }

    /// A path; `in_value` paths spell generic arguments `::<...>`.
    fn path(&mut self, in_value: bool) -> Option<()> {
        self.nest(|d| {
            let tag = d.bump()?;
            match tag {
                b'C' => {
                    d.tagged_base62(b's')?;
                    let name = d.ident()?;
                    d.out.push_str(name);
                }
                b'N' => {
                    let namespace = d.bump()?;
                    if !namespace.is_ascii_alphabetic() {
                        return None;
                    }
                    d.path(in_value)?;
                    let disambiguator = d.tagged_base62(b's')?;
                    let name = d.ident()?;
                    if namespace.is_ascii_uppercase() {
                        d.out.push_str("::{");
                        match namespace {
                            b'C' => d.out.push_str("closure"),
                            b'S' => d.out.push_str("shim"),
                            other => d.out.push(char::from(other)),
                        }
                        if !name.is_empty() {
                            d.out.push(':');
                            d.out.push_str(name);
                        }
                        d.out.push_str(&format!("#{disambiguator}}}"));
                    } else if !name.is_empty() {
                        d.out.push_str("::");
                        d.out.push_str(name);
                    }
                }
                b'M' | b'X' | b'Y' => {
                    if tag != b'Y' {
                        // The impl's own path, which is not printed.
                        d.tagged_base62(b's')?;
                        d.skip(|d| d.path(false))?;
                    }
                    d.out.push('<');
                    d.ty()?;
                    if tag != b'M' {
                        d.out.push_str(" as ");
                        d.path(false)?;
                    }
                    d.out.push('>');
                }
                b'I' => {
                    d.path(in_value)?;
                    if in_value {
                        d.out.push_str("::");
                    }
                    d.out.push('<');
                    d.list(", ", |d| d.generic_arg())?;
                    d.out.push('>');
                }
                b'B' => d.backref(|d| d.path(in_value))?,
                _ => return None,
            }
            Some(())
        })
    }

    fn generic_arg(&mut self) -> Option<()> {
        if self.eat(b'L') {
            let lifetime = self.base62()?;
            self.lifetime(lifetime)
        } else if self.eat(b'K') {
            self.konst()
        } else {
            self.ty()
        }
    }

    /// Lifetime 0 is erased (`'_`); the rest count back through the
    /// enclosing binders, named `'a`, `'b`, ...
    fn lifetime(&mut self, index: u64) -> Option<()> {
        if index == 0 {
            self.out.push_str("'_");
            return Some(());
        }
        let depth = self.bound.checked_sub(index)?;
        match u8::try_from(depth) {
            Ok(depth) if depth < 26 => {
                self.out.push('\'');
                self.out.push(char::from(b'a' + depth));
            }
            _ => self.out.push_str(&format!("'_{depth}")),
        }
        Some(())
    }

    /// An optional `for<...>` binder (`G`), then `f` inside it.
    fn binder(&mut self, f: impl FnOnce(&mut Self) -> Option<()>) -> Option<()> {
        let count = self.tagged_base62(b'G')?;
        if count > 64 {
            return None;
        }
        if count > 0 {
            self.out.push_str("for<");
            for i in 0..count {
                if i > 0 {
                    self.out.push_str(", ");
                }
                self.bound += 1;
                self.lifetime(1)?;
            }
            self.out.push_str("> ");
        }
        let result = f(self);
        self.bound -= count;
        result
    }

    fn ty(&mut self) -> Option<()> {
        self.nest(|d| {
            let tag = d.bump()?;
            if let Some(name) = basic_type(tag) {
                d.out.push_str(name);
                return Some(());
            }
            match tag {
                b'R' | b'Q' => {
                    d.out.push('&');
                    if d.eat(b'L') {
                        let lifetime = d.base62()?;
                        if lifetime != 0 {
                            d.lifetime(lifetime)?;
                            d.out.push(' ');
                        }
                    }
                    if tag == b'Q' {
                        d.out.push_str("mut ");
                    }
                    d.ty()?;
                }
                b'P' => {
                    d.out.push_str("*const ");
                    d.ty()?;
                }
                b'O' => {
                    d.out.push_str("*mut ");
                    d.ty()?;
                }
                b'A' => {
                    d.out.push('[');
                    d.ty()?;
                    d.out.push_str("; ");
                    d.konst()?;
                    d.out.push(']');
                }
                b'S' => {
                    d.out.push('[');
                    d.ty()?;
                    d.out.push(']');
                }
                b'T' => {
                    d.out.push('(');
                    if d.list(", ", |d| d.ty())? == 1 {
                        d.out.push(',');
                    }
                    d.out.push(')');
                }
                b'F' => d.binder(|d| d.fn_sig())?,
                b'D' => {
                    d.out.push_str("dyn ");
                    d.binder(|d| d.list(" + ", |d| d.dyn_trait()).map(|_| ()))?;
                    if !d.eat(b'L') {
                        return None;
                    }
                    let lifetime = d.base62()?;
                    if lifetime != 0 {
                        d.out.push_str(" + ");
                        d.lifetime(lifetime)?;
                    }
                }
                b'B' => d.backref(|d| d.ty())?,
                _ => {
                    d.at -= 1;
                    d.path(false)?;
                }
            }
            Some(())
        })
    }

    fn fn_sig(&mut self) -> Option<()> {
        let is_unsafe = self.eat(b'U');
        let abi = if !self.eat(b'K') {
            None
        } else if self.eat(b'C') {
            Some("C".to_string())
        } else {
            Some(self.ident()?.replace('_', "-"))
        };
        if is_unsafe {
            self.out.push_str("unsafe ");
        }
        if let Some(abi) = abi {
            self.out.push_str(&format!("extern \"{abi}\" "));
        }
        self.out.push_str("fn(");
        self.list(", ", |d| d.ty())?;
        self.out.push(')');
        if !self.eat(b'u') {
            self.out.push_str(" -> ");
            self.ty()?;
        }
        Some(())
    }

    /// One trait of a `dyn` type, with its associated-type bindings.
    fn dyn_trait(&mut self) -> Option<()> {
        let mut open = self.path_maybe_open_generics()?;
        while self.eat(b'p') {
            self.out.push_str(if open { ", " } else { "<" });
            open = true;
            let name = self.ident()?;
            self.out.push_str(name);
            self.out.push_str(" = ");
            self.ty()?;
        }
        if open {
            self.out.push('>');
        }
        Some(())
    }

    /// A path whose generic arguments stay open for bindings: true
    /// when a `<` was printed and is still unclosed.
    fn path_maybe_open_generics(&mut self) -> Option<bool> {
        if self.eat(b'B') {
            let mut open = false;
            self.backref(|d| {
                open = d.path_maybe_open_generics()?;
                Some(())
            })?;
            Some(open)
        } else if self.eat(b'I') {
            self.path(false)?;
            self.out.push('<');
            self.list(", ", |d| d.generic_arg())?;
            Some(true)
        } else {
            self.path(false)?;
            Some(false)
        }
    }

    /// A const generic value: integers in decimal, bool, char escaped
    /// as binutils escapes it, and the placeholder `_`. Other const
    /// kinds, integers of more than 16 hex digits and chars of more
    /// than 8 are not decoded.
    fn konst(&mut self) -> Option<()> {
        if self.eat(b'B') {
            return self.backref(|d| d.konst());
        }
        match self.bump()? {
            b'p' => self.out.push('_'),
            b'h' | b't' | b'm' | b'y' | b'o' | b'j' => self.const_uint()?,
            b'a' | b's' | b'l' | b'x' | b'n' | b'i' => {
                if self.eat(b'n') {
                    self.out.push('-');
                }
                self.const_uint()?;
            }
            b'b' => match self.hex_nibbles()? {
                "0" => self.out.push_str("false"),
                "1" => self.out.push_str("true"),
                _ => return None,
            },
            b'c' => {
                // binutils refuses a char of more than 8 hex digits,
                // leading zeros included.
                let nibbles = self.hex_nibbles()?;
                if nibbles.len() > 8 {
                    return None;
                }
                let code = u32::from_str_radix(nibbles, 16).ok()?;
                self.out.push('\'');
                push_escaped_char(&mut self.out, code);
                self.out.push('\'');
            }
            _ => return None,
        }
        Some(())
    }

    /// Lowercase hex nibbles up to `_`; at least one.
    fn hex_nibbles(&mut self) -> Option<&'a str> {
        let start = self.at;
        while self
            .peek()
            .is_some_and(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        {
            self.at += 1;
        }
        let sym = self.sym;
        let nibbles = std::str::from_utf8(&sym[start..self.at]).ok()?;
        (!nibbles.is_empty() && self.eat(b'_')).then_some(nibbles)
    }

    /// The value of at most 16 hex nibbles. binutils prints a longer
    /// value in an altered hex form, which is not reproduced.
    fn hex_value(&mut self) -> Option<u64> {
        let nibbles = self.hex_nibbles()?;
        if nibbles.len() > 16 {
            return None;
        }
        u64::from_str_radix(nibbles, 16).ok()
    }

    fn const_uint(&mut self) -> Option<()> {
        let value = self.hex_value()?;
        self.out.push_str(&value.to_string());
        Some(())
    }
}

/// A char const's code point as binutils 2.37 prints it between the
/// quotes: `\t`, `\r` and `\n` escaped, `!` through `}` as
/// themselves (the quote and backslash included), and every other
/// value, invalid ones included, as `\u{hex}`.
fn push_escaped_char(out: &mut String, code: u32) {
    match code {
        0x09 => out.push_str("\\t"),
        0x0d => out.push_str("\\r"),
        0x0a => out.push_str("\\n"),
        0x21..=0x7d => out.extend(char::from_u32(code)),
        _ => out.push_str(&format!("\\u{{{code:x}}}")),
    }
}

/// The basic types of the v0 grammar.
fn basic_type(tag: u8) -> Option<&'static str> {
    Some(match tag {
        b'a' => "i8",
        b'b' => "bool",
        b'c' => "char",
        b'd' => "f64",
        b'e' => "str",
        b'f' => "f32",
        b'h' => "u8",
        b'i' => "isize",
        b'j' => "usize",
        b'l' => "i32",
        b'm' => "u32",
        b'n' => "i128",
        b'o' => "u128",
        b'p' => "_",
        b's' => "i16",
        b't' => "u16",
        b'u' => "()",
        b'v' => "...",
        b'x' => "i64",
        b'y' => "u64",
        b'z' => "!",
        _ => return None,
    })
}

/// `0000000000401000 <main>:` → `main`.
fn symbol_header(line: &str) -> Option<&str> {
    let line = line.trim_end();
    let (_, rest) = line.split_once(" <")?;
    let name = rest.strip_suffix(">:")?;
    if line
        .split_whitespace()
        .next()
        .is_some_and(|addr| !addr.is_empty() && addr.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        Some(name)
    } else {
        None
    }
}

#[derive(PartialEq, Debug)]
enum Class {
    AboveV2,
    Alias,
    Clean,
}

/// Classify one objdump line (`  401000:\tc5 f8 77   \tvzeroupper`).
fn classify(line: &str) -> Class {
    let Some((bytes, text)) = instruction(line) else {
        return Class::Clean;
    };
    // The alias encodings never carry a VEX/EVEX form, so checking
    // them first cannot mask an above-v2 instruction.
    if is_alias_encoding(&bytes) || is_alias_mnemonic(text) {
        return Class::Alias;
    }
    if encoding_above_v2(&bytes) || mnemonic_above_v2(text) {
        return Class::AboveV2;
    }
    Class::Clean
}

/// One objdump instruction line: the encoded bytes and the mnemonic
/// text; `None` for any other line. Bytes of an instruction too long
/// for one line continue on following lines with no mnemonic; those
/// are dropped, as the first line already classified the instruction.
fn instruction(line: &str) -> Option<(Vec<u8>, &str)> {
    let (address, rest) = line.split_once(':')?;
    let address = address.trim();
    if address.is_empty() || !address.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut bytes = Vec::new();
    let mut text = rest;
    for token in rest.split_whitespace() {
        if token.len() == 2 && token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bytes.push(u8::from_str_radix(token, 16).ok()?);
            text = &text[text.find(token)? + token.len()..];
        } else {
            break;
        }
    }
    if bytes.is_empty() {
        return None;
    }
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some((bytes, text))
}

/// Split encoded bytes into (saw F2, saw F3, opcode start), skipping
/// legacy prefixes and at most one REX byte.
fn opcode_start(bytes: &[u8]) -> (bool, bool, usize) {
    let mut at = 0;
    let (mut f2, mut f3) = (false, false);
    while at < bytes.len() {
        match bytes[at] {
            0xf0 | 0x2e | 0x36 | 0x3e | 0x26 | 0x64 | 0x65 | 0x66 | 0x67 => at += 1,
            0xf2 => {
                f2 = true;
                at += 1;
            }
            0xf3 => {
                f3 = true;
                at += 1;
            }
            0x40..=0x4f => {
                at += 1; // REX: opcode proper follows.
                break;
            }
            _ => break,
        }
    }
    (f2, f3, at)
}

/// `F3 0F BC` only (TZCNT, executing as BSF without BMI1; equal on
/// every nonzero input). `F3 0F BD` (LZCNT) is not an alias: as BSR
/// it returns a different value on every nonzero input.
fn is_alias_encoding(bytes: &[u8]) -> bool {
    let (_, f3, at) = opcode_start(bytes);
    f3 && bytes.get(at) == Some(&0x0f) && bytes.get(at + 1) == Some(&0xbc)
}

fn is_alias_mnemonic(text: &str) -> bool {
    let Some(mnemonic) = text.split_whitespace().next() else {
        return false;
    };
    mnemonic.starts_with("tzcnt")
}

/// Whether the encoded instruction exceeds x86-64-v2, by the byte
/// rules in the module documentation.
fn encoding_above_v2(bytes: &[u8]) -> bool {
    let (f2, f3, at) = opcode_start(bytes);
    let opcode = &bytes[at.min(bytes.len())..];
    match opcode.first() {
        Some(0xc4) | Some(0xc5) => true, // VEX
        Some(0x62) => true,              // EVEX
        Some(0x0f) => {
            // MOVBE 0F 38 F0/F1; CRC32 carries F2 and is v2.
            // LZCNT F3 0F BD; without F3 it is BSR, and F3 0F BC
            // (TZCNT) is the alias class above.
            (opcode.get(1) == Some(&0x38)
                && !f2
                && matches!(opcode.get(2), Some(0xf0) | Some(0xf1)))
                || (f3 && opcode.get(1) == Some(&0xbd))
        }
        _ => false,
    }
}

/// Whether objdump's mnemonic alone marks an above-v2 instruction.
fn mnemonic_above_v2(text: &str) -> bool {
    let Some(mnemonic) = text.split_whitespace().next() else {
        return false;
    };
    let mnemonic = mnemonic.trim_end_matches(|c: char| !c.is_ascii_alphabetic());
    if mnemonic.starts_with("movbe") || mnemonic.starts_with("lzcnt") {
        return true;
    }
    mnemonic.starts_with('v') && !mnemonic.starts_with("verr") && !mnemonic.starts_with("verw")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn above_v2_instructions_are_found() {
        for line in [
            "  401000:\tc5 f8 77             \tvzeroupper",
            "  401003:\tc5 fc 58 c0          \tvaddps %ymm0,%ymm0,%ymm0",
            "  401007:\tc4 e2 6d f5 c0       \tpdep %rax,%rax,%rax",
            "  40100c:\t62 f1 7c 48 58 c0    \tvaddps %zmm0,%zmm0,%zmm0",
            "  401012:\t0f 38 f0 00          \tmovbe (%rax),%eax",
            // LZCNT is not a BSR alias: BSR returns the index, not the count.
            "  401016:\tf3 48 0f bd c0       \tlzcnt %rax,%rax",
            // An objdump too old for the mnemonic: bytes still classify.
            "  40101f:\tc5 fc 58 c0          \t.byte 0xc5,0xfc,0x58,0xc0",
        ] {
            assert_eq!(classify(line), Class::AboveV2, "missed: {line}");
        }
    }

    #[test]
    fn alias_encodings_are_counted_not_failed() {
        for line in ["  40101b:\tf3 0f bc c0          \ttzcnt %eax,%eax"] {
            assert_eq!(classify(line), Class::Alias, "misclassified: {line}");
        }
    }

    #[test]
    fn baseline_counts_bound_the_verdict() {
        assert!(within_baseline(0, None));
        assert!(!within_baseline(1, None));
        assert!(within_baseline(1_000, Some(1_000)));
        assert!(!within_baseline(1_001, Some(1_000)));
        assert_eq!(parse_baseline("1_000"), Some(1_000));
        assert_eq!(parse_baseline("many"), None);
    }

    #[test]
    fn v2_and_baseline_instructions_are_not() {
        for line in [
            "  401000:\tf3 0f b8 c0          \tpopcnt %eax,%eax",
            "  401004:\tf2 0f 38 f1 c0       \tcrc32 %eax,%eax",
            "  401009:\t0f bd c0             \tbsr %eax,%eax",
            "  40100c:\t0f 28 c1             \tmovaps %xmm1,%xmm0",
            "  40100f:\t66 0f 38 41 c0       \tphminposuw %xmm0,%xmm0",
            "  401014:\t0f 00 e0             \tverr %ax",
            "  401017:\t48 89 d8             \tmov %rbx,%rax",
            "  40101a:\tc3                   \tret",
            "0000000000401000 <main>:",
            "",
        ] {
            assert_eq!(classify(line), Class::Clean, "false hit: {line}");
        }
    }

    #[test]
    fn symbol_headers_parse() {
        assert_eq!(symbol_header("0000000000401000 <main>:"), Some("main"));
        assert_eq!(symbol_header("  401000:\tc3\tret"), None);
    }

    /// An objdump without v0 demangling (binutils before 2.36)
    /// prints Rust names mangled; the allowlist names them
    /// demangled, and must still match.
    #[test]
    fn allowlist_matches_rust_symbols_objdump_left_mangled() {
        let text = "\
0000000000401000 <_RINvNtNtNtCs8o9Cfoj3Yfu_4core9core_arch3x864avx217__mm256_slli_epi64Kl2a_E>:
  401000:\tc5 fd 73 f0 2a       \tvpsllq $0x2a,%ymm0,%ymm0
0000000000401010 <_RNvNtNtNtNtNtCs8o9Cfoj3Yfu_4core3num3imp7flt2dec8strategy6dragon12format_exact>:
  401010:\tc5 f8 57 c0          \tvxorps %xmm0,%xmm0,%xmm0
";
        let report = report_from_disassembly(text);
        let symbols: Vec<&str> = report.hits.iter().map(|hit| hit.symbol.as_str()).collect();
        assert_eq!(
            symbols,
            [
                "core::core_arch::x86::avx2::_mm256_slli_epi64::<42>",
                "core::num::imp::flt2dec::strategy::dragon::format_exact",
            ],
            "each hit is named as a v0-aware objdump names it"
        );
        assert_eq!(report.undecoded, 0);
        let allow = std::collections::BTreeSet::from([
            "core::core_arch::x86::avx2::_mm256_slli_epi64::<42>",
        ]);
        let (hits, allowed) = partition_hits(&report, &allow);
        assert_eq!(
            (hits.len(), allowed.len()),
            (1, 1),
            "the allowlisted intrinsic is allowlisted; the formatting code still counts"
        );
    }

    /// Each expected string is byte-identical to what binutils 2.37
    /// `c++filt -i` prints for the mangled name; it demangles as
    /// `objdump --demangle` does. LLVM's `llvm-cxxfilt` agrees except
    /// that it keeps a `.llvm.N` suffix, escapes chars differently,
    /// rejects leading zeros in a const value and leaves some large
    /// char values (such as 0xffffffff) mangled.
    #[test]
    fn v0_names_demangle_as_objdump_prints_them() {
        for (mangled, expected) in [
            (
                "_RNvNtNtNtNtNtCs8o9Cfoj3Yfu_4core3num3imp7flt2dec8strategy6dragon12format_exact",
                "core::num::imp::flt2dec::strategy::dragon::format_exact",
            ),
            (
                "_RINvNtNtNtCs8o9Cfoj3Yfu_4core9core_arch3x864avx217__mm256_slli_epi64Kl2a_E",
                "core::core_arch::x86::avx2::_mm256_slli_epi64::<42>",
            ),
            (
                "_RNvMNtNtNtNtCsaB12_6memchr4arch6x86_644avx26memchrNtNtNtNtNtCsaB12_6memchr4arch6x86_644avx26memchr3One13find_raw_avx2",
                "<memchr::arch::x86_64::avx2::memchr::One>::find_raw_avx2",
            ),
            // The same impl path, reached through a backref.
            (
                "_RNvMNtNtNtNtCsaB12_6memchr4arch6x86_644avx26memchrNtB2_3One13new_unchecked",
                "<memchr::arch::x86_64::avx2::memchr::One>::new_unchecked",
            ),
            (
                "_RNvXNtNtNtNtCs5x_12aho_corasick6packed5teddy7builder6x86_64INtNtNtNtNtCs5x_12aho_corasick6packed5teddy7builder6x86_647FatAVX2Kj1_ENtNtNtNtCs5x_12aho_corasick6packed5teddy7builder9SearcherT4find",
                "<aho_corasick::packed::teddy::builder::x86_64::FatAVX2<1> as aho_corasick::packed::teddy::builder::SearcherT>::find",
            ),
            (
                "_RINvNtNtCs7_8chacha208backends4avx29rng_innerNtCs7_8chacha203R12NtNtCs7_8chacha208variants6LegacyE",
                "chacha20::backends::avx2::rng_inner::<chacha20::R12, chacha20::variants::Legacy>",
            ),
            ("_RNCNvCs1_6hermit3foo0", "hermit::foo::{closure#0}"),
            ("_RNCNvCs1_6hermit3foos_0", "hermit::foo::{closure#1}"),
            // binutils drops a `.suffix`.
            (
                "_RNvNtNtNtNtNtCs8o9Cfoj3Yfu_4core3num3imp7flt2dec8strategy6dragon12format_exact.llvm.12345",
                "core::num::imp::flt2dec::strategy::dragon::format_exact",
            ),
            ("_RINvCs1_6hermit3fooRShE", "hermit::foo::<&[u8]>"),
            ("_RINvCs1_6hermit3fooQReE", "hermit::foo::<&mut &str>"),
            ("_RINvCs1_6hermit3fooTlmEE", "hermit::foo::<(i32, u32)>"),
            ("_RINvCs1_6hermit3fooKb1_E", "hermit::foo::<true>"),
            ("_RINvCs1_6hermit3fooKln2a_E", "hermit::foo::<-42>"),
            ("_RINvCs1_6hermit3fooKc61_E", "hermit::foo::<'a'>"),
            // binutils escapes a char const its own way.
            ("_RINvCs1_6hermit3fooKc20_E", "hermit::foo::<'\\u{20}'>"),
            ("_RINvCs1_6hermit3fooKc7e_E", "hermit::foo::<'\\u{7e}'>"),
            ("_RINvCs1_6hermit3fooKc27_E", "hermit::foo::<'''>"),
            ("_RINvCs1_6hermit3fooKc5c_E", "hermit::foo::<'\\'>"),
            ("_RINvCs1_6hermit3fooKca_E", "hermit::foo::<'\\n'>"),
            ("_RINvCs1_6hermit3fooKc0_E", "hermit::foo::<'\\u{0}'>"),
            ("_RINvCs1_6hermit3fooKce9_E", "hermit::foo::<'\\u{e9}'>"),
            ("_RINvCs1_6hermit3fooKcd800_E", "hermit::foo::<'\\u{d800}'>"),
            ("_RINvCs1_6hermit3fooKc0061_E", "hermit::foo::<'a'>"),
            ("_RINvCs1_6hermit3fooKc00000061_E", "hermit::foo::<'a'>"),
            (
                "_RINvCs1_6hermit3fooKcffffffff_E",
                "hermit::foo::<'\\u{ffffffff}'>",
            ),
            ("_RINvCs1_6hermit3fooKj00_E", "hermit::foo::<0>"),
            ("_RINvCs1_6hermit3fooKln0_E", "hermit::foo::<-0>"),
            (
                "_RINvCs1_6hermit3fooKjffffffffffffffff_E",
                "hermit::foo::<18446744073709551615>",
            ),
            ("_RINvCs1_6hermit3fooKb0_E", "hermit::foo::<false>"),
            (
                "_RINvCs1_6hermit3fooPAhj10_E",
                "hermit::foo::<*const [u8; 16]>",
            ),
            ("_RINvCs1_6hermit3fooOShE", "hermit::foo::<*mut [u8]>"),
            ("_RINvCs1_6hermit3fooFmEuE", "hermit::foo::<fn(u32)>"),
            (
                "_RINvCs1_6hermit3fooFUKCmEhE",
                "hermit::foo::<unsafe extern \"C\" fn(u32) -> u8>",
            ),
            (
                "_RINvCs1_6hermit3fooDNtCs1_6hermit5TraitEL_E",
                "hermit::foo::<dyn hermit::Trait>",
            ),
        ] {
            assert_eq!(demangle_v0(mangled).as_deref(), Some(expected), "{mangled}");
        }
    }

    /// A name this cannot decode is left alone (and counted), never
    /// renamed to something it might wrongly match.
    #[test]
    fn names_that_do_not_decode_stay_as_printed() {
        for name in [
            "_RNvC",                      // truncated
            "_RNvCs1_6hermit3fooX",       // trailing junk
            "_RNvCs1_6hermitu3foo",       // punycode: objdump decodes it, this does not
            "_R1NvCs1_6hermit3foo",       // a later encoding version
            "_RNvB1_3foo",                // a backref to itself
            "_RINvCs1_6hermit3fooKeE",    // a str const, not decoded here
            "_RINvCs1_6hermit3fooKj_E",   // an integer const with no digits
            "_RINvCs1_6hermit3fooKb00_E", // a bool const other than 0 or 1
            // 9 hex digits: binutils leaves a char of more than 8 mangled
            "_RINvCs1_6hermit3fooKc100000000_E",
            "_RINvCs1_6hermit3fooKc000000061_E",
            // 17 hex digits: binutils prints an altered hex form, not reproduced
            "_RINvCs1_6hermit3fooKj10000000000000000_E",
        ] {
            assert_eq!(demangle_v0(name), None, "{name}");
            assert_eq!(header_symbol(name), None, "{name}");
        }
        assert_eq!(
            header_symbol("hermit_xinuse_arm").as_deref(),
            Some("hermit_xinuse_arm"),
            "a C symbol is kept as printed"
        );
        let report = report_from_disassembly(
            "0000000000401000 <_RNvC>:\n  401000:\tc5 f8 77             \tvzeroupper\n",
        );
        assert_eq!(report.undecoded, 1);
        assert_eq!(report.hits[0].symbol, "_RNvC");
    }

    /// `value` as a v0 backref target: `_` for 0, else the base-62
    /// digits of one less, then `_`.
    fn base62(value: usize) -> String {
        const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
        let Some(mut rest) = value.checked_sub(1) else {
            return "_".to_string();
        };
        let mut digits = Vec::new();
        loop {
            digits.push(DIGITS[rest % 62]);
            rest /= 62;
            if rest == 0 {
                break;
            }
        }
        digits.reverse();
        format!(
            "{}_",
            String::from_utf8(digits).expect("base-62 digits are ASCII")
        )
    }

    /// `hermit::foo::<LEAF, (LEAF, LEAF), ...>` with `links` tuples,
    /// each of whose two elements is a backref to the previous
    /// argument, so the demangled output doubles at each link.
    fn tuple_chain(leaf: &str, links: usize) -> String {
        let mut mangled = String::from("INvCs1_6hermit3foo");
        let mut previous = mangled.len();
        mangled.push_str(leaf);
        for _ in 0..links {
            let backref = format!("B{}", base62(previous));
            previous = mangled.len();
            mangled.push_str(&format!("T{backref}{backref}E"));
        }
        format!("_R{mangled}E")
    }

    /// A backref chain decodes in full while it is short, and is
    /// stopped by the output cap or the step cap, without expanding,
    /// when it is not.
    #[test]
    fn backref_chains_stop_at_the_work_caps() {
        let types = (0..8).fold(vec!["u8".to_string()], |mut types, _| {
            let last = types.last().expect("the chain starts with u8");
            let next = format!("({last}, {last})");
            types.push(next);
            types
        });
        assert_eq!(
            demangle_v0(&tuple_chain("h", 8)),
            Some(format!("hermit::foo::<{}>", types.join(", "))),
            "8 links: the last argument has 256 elements, as binutils 2.37 prints it"
        );
        assert_eq!(
            demangle_v0(&tuple_chain("h", 40)),
            None,
            "40 links would print 2^40 elements"
        );

        // A 200-byte identifier as the leaf: 12 links print about
        // 1.7 MB in about 60,000 steps, so only the output cap stops it.
        let wide = format!("C200{}", "a".repeat(200));
        assert!(
            demangle_v0(&tuple_chain(&wide, 4)).is_some(),
            "4 links decode in full"
        );
        assert_eq!(
            demangle_v0(&tuple_chain(&wide, 12)),
            None,
            "12 links: more than MAX_DEMANGLED_BYTES of output"
        );

        // A leaf of 50 nested paths that print nothing: 14 links take
        // about 1.9 million steps but print only about 130 kB, so only
        // the step cap stops it.
        let silent = format!("{}C0{}", "Nv".repeat(50), "0".repeat(50));
        assert!(
            demangle_v0(&tuple_chain(&silent, 4)).is_some(),
            "4 links decode in full"
        );
        assert_eq!(
            demangle_v0(&tuple_chain(&silent, 14)),
            None,
            "14 links: more than MAX_DEMANGLE_STEPS parse steps"
        );
    }
}
