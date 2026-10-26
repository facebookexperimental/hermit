/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Applying the exclusive-loop rewrite to the executable segments of an
//! aarch64 ELF file.
//!
//! The rewrite is in place and the same length, so the output file is the
//! input file with some instruction words replaced: every offset, segment,
//! symbol, relocation and build id stays as it was, and the loader maps it
//! exactly as it mapped the original.
//!
//! Only `PT_LOAD` segments that are executable are scanned, and a segment
//! that is also writable is refused rather than rewritten: writable text is
//! either a loader bug or a runtime that rewrites itself, and rewriting it
//! would race whatever writes it.

use std::collections::BTreeMap;
use std::path::Path;

use goblin::elf::Elf;
use goblin::elf::header;
use goblin::elf::program_header;

use super::rewrite;
use super::rewrite::Idiom;
use super::rewrite::Refusal;

/// What a rewrite did to one file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileReport {
    /// Exclusive instructions found in executable segments.
    pub exclusive_instructions: usize,
    /// Rewritten sequences, by idiom.
    pub patched: BTreeMap<&'static str, usize>,
    /// Sequences left alone, by reason.
    pub refused: BTreeMap<&'static str, usize>,
    /// Addresses of the first few refusals, for reporting.
    pub refused_examples: Vec<(u64, String)>,
    /// Executable segments skipped entirely, with the reason.
    pub skipped_segments: Vec<String>,
}

impl FileReport {
    pub fn patched_total(&self) -> usize {
        self.patched.values().sum()
    }

    pub fn refused_total(&self) -> usize {
        self.refused.values().sum()
    }

    /// Whether every exclusive loop in the file was rewritten. An exclusive
    /// instruction left behind means the program can still retry a store.
    pub fn complete(&self) -> bool {
        self.refused_total() == 0 && self.skipped_segments.is_empty()
    }
}

fn idiom_name(idiom: Idiom) -> &'static str {
    match idiom {
        Idiom::Rmw => "rmw",
        Idiom::Swap => "swap",
        Idiom::CasCompare => "cas-compare",
        Idiom::CasZero => "cas-zero",
    }
}

fn refusal_name(reason: &Refusal) -> &'static str {
    match reason {
        Refusal::ExclusivePair => "exclusive-pair",
        Refusal::NoPairedLoad => "no-paired-load",
        Refusal::Undecoded => "undecoded-instruction",
        Refusal::NotCanonical => "not-a-canonical-loop",
        Refusal::BranchIntoSequence => "branch-into-sequence",
        Refusal::Registers(_) => "register-constraint",
        Refusal::UnsupportedOperation => "unsupported-operation",
        Refusal::BranchOutOfRange => "branch-out-of-range",
    }
}

fn refusal_detail(reason: &Refusal) -> String {
    match reason {
        Refusal::Registers(why) => format!("register-constraint: {why}"),
        other => refusal_name(other).to_string(),
    }
}

/// Rewrite the executable segments of `bytes` in place.
///
/// Returns `Ok(None)` when the file is not an aarch64 ELF, which is not an
/// error: a caller that was handed the wrong file should say so and run the
/// original.
pub fn rewrite_bytes(bytes: &mut [u8]) -> Result<Option<FileReport>, goblin::error::Error> {
    let segments = {
        let elf = Elf::parse(bytes)?;
        if elf.header.e_machine != header::EM_AARCH64 {
            return Ok(None);
        }
        elf.program_headers
            .iter()
            .filter(|ph| ph.p_type == program_header::PT_LOAD && ph.is_executable())
            .map(|ph| {
                (
                    ph.p_offset,
                    ph.p_filesz,
                    ph.p_vaddr,
                    ph.is_write(),
                    ph.p_align,
                )
            })
            .collect::<Vec<_>>()
    };
    let mut report = FileReport::default();
    for (offset, filesz, vaddr, writable, align) in segments {
        let start = offset as usize;
        let end = start.saturating_add(filesz as usize);
        if end > bytes.len() || start % 4 != 0 || vaddr % 4 != 0 {
            report.skipped_segments.push(format!(
                "segment at file offset {offset:#x} is truncated or unaligned"
            ));
            continue;
        }
        if writable {
            report.skipped_segments.push(format!(
                "segment at file offset {offset:#x} is writable and executable"
            ));
            continue;
        }
        let _ = align;
        let count = (end - start) / 4;
        let mut words: Vec<u32> = (0..count)
            .map(|i| {
                let at = start + 4 * i;
                u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
            })
            .collect();
        let plan = rewrite::plan(&words, vaddr);
        report.exclusive_instructions += plan.exclusive_instructions;
        for site in &plan.patched {
            *report.patched.entry(idiom_name(site.idiom)).or_default() += 1;
        }
        for refused in &plan.refused {
            *report.refused.entry(refusal_name(&refused.reason)).or_default() += 1;
            if report.refused_examples.len() < 16 {
                report
                    .refused_examples
                    .push((refused.addr, refusal_detail(&refused.reason)));
            }
        }
        rewrite::apply(&mut words, vaddr, &plan);
        for (i, word) in words.iter().enumerate() {
            bytes[start + 4 * i..start + 4 * i + 4].copy_from_slice(&word.to_le_bytes());
        }
    }
    Ok(Some(report))
}

/// Read `input`, rewrite it, and write the result to `output` with `input`'s
/// permissions. The output is written whole, so a failed rewrite leaves no
/// partial file behind under that name.
pub fn rewrite_file(
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
) -> anyhow::Result<Option<FileReport>> {
    let input = input.as_ref();
    let mut bytes = std::fs::read(input)?;
    let report = match rewrite_bytes(&mut bytes)? {
        Some(report) => report,
        None => return Ok(None),
    };
    let temporary = output.as_ref().with_extension("partial");
    std::fs::write(&temporary, &bytes)?;
    let mode = std::fs::metadata(input)?.permissions();
    std::fs::set_permissions(&temporary, mode)?;
    std::fs::rename(&temporary, output.as_ref())?;
    Ok(Some(report))
}
