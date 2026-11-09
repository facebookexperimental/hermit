#!/usr/bin/env -S rust-script --force
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
//! Refuse an aarch64 binary that contains SVE or SME instructions.
//!
//! aarch64 Hermit is often built on a server for Android devices. A server
//! build tuned for its own core (`-C target-cpu=native`, a Neoverse
//! `target-cpu`, or a C compiler's `-march=native` in a build script) can emit
//! SVE, which Cortex-A78-class devices do not implement: there the binary dies
//! with SIGILL. The default `aarch64-unknown-linux-gnu` target is generic
//! Armv8-A and emits none, so a hit means a build or a linked library opted in.
//!
//! The check reads `objdump -d` and classifies every instruction word by its
//! A64 encoding group (bits 28:25 of the word), so it does not depend on
//! objdump knowing the instruction: an older objdump prints an SVE2 or SME2
//! word as `.inst 0x... ; undefined`, which is still classified.
//! - bits 28:25 = `0010` is the SVE group, which includes SVE2 and the
//!   streaming-vector-length forms `rdsvl`, `addsvl` and `addspl`;
//! - bit 31 = 1 with bits 28:25 = `0000` is the SME group.
//!
//! SME also reaches into the system-instruction space: the streaming-mode
//! switches `smstart`/`smstop` (`msr svcr..., #imm`) and reads or writes of
//! the `SVCR` and `TPIDR2_EL0` registers. Those are matched by encoding too,
//! as an older objdump names them only as `s0_3_c4_c7_3` or `s3_3_c4_c2_2`.
//!
//! Lines objdump prints as `.word` are data, not instructions, and are
//! skipped.
//!
//! A hit in code that only runs after a runtime check for SVE (an ifunc
//! variant, or a path guarded by `is_aarch64_feature_detected!("sve")`) is
//! still reported: a scan cannot tell it from code that runs everywhere.
//!
//! Usage: `scripts/check-no-sve.rs BINARY...`
//!
//! Exit status:
//! - 0: no binary has such an instruction;
//! - 1: a binary does (its first hits are printed);
//! - 2: objdump could not disassemble a binary, which says nothing either way.

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::process::Command;
use std::process::ExitCode;

/// How many hits to print per binary.
const SHOWN: usize = 5;

fn main() -> ExitCode {
    rust_script_prelude::init();
    let binaries: Vec<String> = std::env::args().skip(1).collect();
    if binaries.is_empty() {
        eprintln!("usage: scripts/check-no-sve.rs BINARY...");
        return ExitCode::from(2);
    }
    let mut status = 0;
    for binary in &binaries {
        match sve_lines(binary) {
            Ok(hits) if hits.is_empty() => println!("check-no-sve: {binary}: no SVE or SME"),
            Ok(hits) => {
                println!(
                    "check-no-sve: {binary}: FAILED -- {} SVE or SME instructions, first:",
                    hits.len()
                );
                for line in hits.iter().take(SHOWN) {
                    println!("  {line}");
                }
                status = status.max(1);
            }
            Err(message) => {
                eprintln!("check-no-sve: {binary}: COULD NOT DETERMINE -- {message}");
                status = 2;
            }
        }
    }
    ExitCode::from(status)
}

/// The disassembly lines of `binary` with an SVE or SME instruction.
fn sve_lines(binary: &str) -> Result<Vec<String>, String> {
    let output = Command::new("objdump")
        .args(["-d", binary])
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
    Ok(text
        .lines()
        .filter(|line| is_sve_or_sme(line))
        .map(|line| line.trim().to_string())
        .collect())
}

/// One objdump instruction line (`  4005a8:\ta540a000 \tld1w\t{z0.s}, ...`):
/// the instruction word and the mnemonic with its operands; `None` for any
/// other line.
fn instruction(line: &str) -> Option<(u32, &str)> {
    let (address, rest) = line.split_once(':')?;
    let address = address.trim();
    if address.is_empty() || !address.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let rest = rest.trim_start();
    let (word, text) = rest.split_once(char::is_whitespace)?;
    if word.len() != 8 {
        return None;
    }
    let word = u32::from_str_radix(word, 16).ok()?;
    Some((word, text.trim()))
}

/// Whether bits 28:25 of `word` select the SVE group, or bit 31 and bits
/// 28:25 the SME group.
fn sve_or_sme_encoding(word: u32) -> bool {
    let op0 = (word >> 25) & 0xf;
    op0 == 0b0010 || (word >> 31 == 1 && op0 == 0b0000)
}

/// Whether `word` is one of SME's system instructions: `msr svcr..., #imm`
/// (`smstart`/`smstop`, op1 = 3, CRn = 4, CRm = 2-7, op2 = 3), or `mrs`/`msr`
/// of `SVCR` (S3_3_C4_C2_2) or `TPIDR2_EL0` (S3_3_C13_C0_5).
fn sme_system_instruction(word: u32) -> bool {
    let svcr_immediate =
        word & 0xffff_f0ff == 0xd503_407f && (2..=7).contains(&((word >> 8) & 0xf));
    // The L bit (21) and Rt (4:0) vary between mrs and msr and by register.
    let sysreg = word & 0xffdf_ffe0;
    svcr_immediate || sysreg == 0xd51b_4240 || sysreg == 0xd51b_d0a0
}

/// Whether an objdump line is an SVE or SME instruction.
fn is_sve_or_sme(line: &str) -> bool {
    let Some((word, text)) = instruction(line) else {
        return false;
    };
    if text.split_whitespace().next() == Some(".word") {
        return false;
    }
    sve_or_sme_encoding(word) || sme_system_instruction(word)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sve_and_sme_instructions_are_found() {
        for line in [
            "  400540:\ta540a020 \tld1w\t{z0.s}, p0/z, [x1]",
            "  400534:\t25a30400 \twhilelt\tp0.s, w0, w3",
            "  400538:\t2598e3e0 \tptrue\tp0.s",
            "  40053c:\t04bf5020 \trdvl\tx0, #1",
            "  400544:\t04215020 \taddvl\tx0, x1, #1",
            "  400548:\t0420e3e0 \tcntb\tx0",
            "  40054c:\t25a07810 \tptrue\tpn8.s",
            "  400550:\t85800000 \tldr\tp0, [x0]",
            "  400554:\t04bf5820 \trdsvl\tx0, #1",
            "  400558:\tc00800ff \tzero\t{za}",
            "  40055c:\tc0480001 \tzero\t{zt0}",
            "  400560:\td503477f \tsmstart",
            "  400564:\td503467f \tsmstop",
            "  400568:\td53b4240 \tmrs\tx0, svcr",
            "  400574:\td503477f \tmsr\ts0_3_c4_c7_3, xzr",
            "  400578:\td53b4240 \tmrs\tx0, s3_3_c4_c2_2",
            "  40057c:\td51b4240 \tmsr\tsvcr, x0",
            "  400580:\td53bd0a0 \tmrs\tx0, tpidr2_el0",
            "  40056c:\t04bf5020 \t.inst\t0x04bf5020 ; undefined",
            "  400570:\tc00800ff \t.inst\t0xc00800ff ; undefined",
        ] {
            assert!(is_sve_or_sme(line), "missed: {line}");
        }
    }

    #[test]
    fn base_instructions_symbols_and_data_are_not() {
        for line in [
            "  400500:\tf9400be0 \tldr\tx0, [sp, #16]",
            "  400504:\teb0103ff \tcmp\txzr, x1",
            "  400508:\t4c407820 \tld1\t{v0.4s}, [x1]",
            "  40050c:\t9400003d \tbl\t400600 <z0>",
            "  400510:\ta9be7bfd \tstp\tx29, x30, [sp, #-32]!",
            "  400514:\tc85ffc40 \tldaxr\tx0, [x2]",
            "  400518:\td2896803 \tmov\tx3, #0x9680",
            "  40051c:\tf9800020 \tprfm\tpldl1keep, [x1]",
            "  400520:\td50b7420 \tdc\tzva, x0",
            "  400524:\t04bf5020 \t.word\t0x04bf5020",
            "  400528:\td53b4200 \tmrs\tx0, nzcv",
            "  40052c:\td53bd040 \tmrs\tx0, tpidr_el0",
            "  400530:\td503201f \tnop",
            "  400534:\td50342ff \tmsr\tdaifclr, #0x2",
            "0000000000400500 <main>:",
            "",
        ] {
            assert!(!is_sve_or_sme(line), "false hit: {line}");
        }
    }
}
