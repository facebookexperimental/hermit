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
//! the per-symbol counts the allowlist is maintained from. AMD
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
                if hit_symbols {
                    for (count, symbol) in hit_symbol_counts(&report) {
                        println!("{count} {symbol}");
                    }
                    continue;
                }
                let (hits, allowed): (Vec<&Hit>, Vec<&Hit>) = report
                    .hits
                    .iter()
                    .partition(|hit| !allow.contains(hit.symbol.as_str()));
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
    let text = String::from_utf8_lossy(&output.stdout);
    let mut symbol = String::new();
    let mut report = Report {
        hits: Vec::new(),
        alias: 0,
    };
    for line in text.lines() {
        if let Some(name) = symbol_header(line) {
            symbol = name.to_string();
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
    Ok(report)
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
        Some(0x62) => true,             // EVEX
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
}
