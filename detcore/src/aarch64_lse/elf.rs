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
//! Only sections marked executable (`SHF_EXECINSTR`, so `.text`, `.plt`,
//! `.init` and the like) are scanned. An executable `PT_LOAD` segment also
//! carries the ELF header, the dynamic symbol and relocation tables and
//! read-only data, and those bytes decode as instructions as readily as code
//! does: measured on one Android test binary, scanning whole segments found
//! 69 "exclusive instructions" of which 66 were in data before `.text` even
//! began. A file with no section headers is left alone and reported, because
//! nothing in it can be told apart from data.
//!
//! A section in a segment that is both writable and executable is refused
//! too: writable text is either a loader quirk or a runtime that rewrites
//! itself, and rewriting it would race whatever writes it.
//!
//! A sequence any relocation points into is refused as well. Relocations are
//! read as 64-bit little-endian, the only aarch64 ELF this rewriter handles. A relocation is
//! the loader's licence to change those bytes at load time, which would
//! overwrite part of a rewritten sequence with something computed for the
//! original one.

use std::collections::BTreeMap;

use goblin::elf::Elf;
use goblin::elf::header;
use goblin::elf::program_header;
use goblin::elf::section_header;

use super::rewrite;
use super::rewrite::Idiom;
use super::rewrite::Refusal;

/// What a rewrite did to one file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileReport {
    /// Exclusive instructions found in executable sections.
    pub exclusive_instructions: usize,
    /// Rewritten sequences, by idiom.
    pub patched: BTreeMap<&'static str, usize>,
    /// Sequences left alone, by reason.
    pub refused: BTreeMap<&'static str, usize>,
    /// Addresses of the first few refusals, for reporting.
    pub refused_examples: Vec<(u64, String)>,
    /// Executable sections skipped entirely, with the reason.
    pub skipped_segments: Vec<String>,
}

impl FileReport {
    /// The rewritten sequences.
    pub fn patched_total(&self) -> usize {
        self.patched.values().sum()
    }

    /// The sequences left alone.
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
        Refusal::Relocated => "relocation-inside-the-sequence",
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
    // A guest that is not an ELF image at all -- a `#!` script, say -- is not
    // an error: it simply has nothing to rewrite. A file that starts like an
    // ELF and then does not parse IS an error, because something is wrong
    // with a file we are about to run.
    if bytes.len() < 4 || bytes[..4] != *goblin::elf::header::ELFMAG {
        return Ok(None);
    }
    // The byte ranges relocations cover, filled inside the block below, which
    // is where the parsed ELF lives.
    let relocated: Vec<(u64, u64)>;
    let mut relr_unreadable = false;
    let sections = {
        let elf = Elf::parse(bytes)?;
        if elf.header.e_machine != header::EM_AARCH64 {
            return Ok(None);
        }
        let writable_executable: Vec<(u64, u64)> = elf
            .program_headers
            .iter()
            .filter(|ph| {
                ph.p_type == program_header::PT_LOAD && ph.is_executable() && ph.is_write()
            })
            .map(|ph| (ph.p_vaddr, ph.p_vaddr.saturating_add(ph.p_memsz)))
            .collect();
        // A relocation's own width is not in the entry, so take the widest
        // aarch64 relocation (8 bytes): any sequence those bytes touch is
        // refused.
        let mut relocations: Vec<(u64, u64)> = elf
            .shdr_relocs
            .iter()
            .flat_map(|(_, relocs)| relocs.iter().map(|reloc| reloc.r_offset))
            .chain(elf.dynrelas.iter().map(|reloc| reloc.r_offset))
            .chain(elf.dynrels.iter().map(|reloc| reloc.r_offset))
            .chain(elf.pltrelocs.iter().map(|reloc| reloc.r_offset))
            .map(|offset| (offset, offset + 8))
            .collect();
        // SHT_RELR = 19: packed relative relocations, which the ELF reader
        // does not report, so they are decoded here and ADDED to that list.
        // The format is its own: an even entry is an address, and each odd
        // entry after it is a bitmap of the 63 words that follow. Decoding
        // them keeps the images that use them (every current aarch64 system
        // binary) rewritable instead of refusing them whole.
        for sh in &elf.section_headers {
            if sh.sh_type != 19 || sh.sh_size == 0 {
                continue;
            }
            let Some(end) = sh
                .sh_offset
                .checked_add(sh.sh_size)
                .and_then(|end| usize::try_from(end).ok())
                .filter(|end| *end <= bytes.len())
            else {
                relr_unreadable = true;
                continue;
            };
            let start = sh.sh_offset as usize;
            relocations.extend(decode_relr(&bytes[start..end]));
        }
        relocated = relocations;
        if elf.section_headers.is_empty() {
            let mut report = FileReport::default();
            report
                .skipped_segments
                .push("the file has no section headers, so code cannot be told from data".into());
            return Ok(Some(report));
        }
        elf.section_headers
            .iter()
            .filter(|sh| {
                sh.sh_type == section_header::SHT_PROGBITS
                    && sh.sh_flags & u64::from(section_header::SHF_EXECINSTR) != 0
                    && sh.sh_size != 0
            })
            .map(|sh| {
                let writable = writable_executable
                    .iter()
                    .any(|(start, end)| sh.sh_addr >= *start && sh.sh_addr < *end);
                (sh.sh_offset, sh.sh_size, sh.sh_addr, writable)
            })
            .collect::<Vec<_>>()
    };
    // A direct branch in one executable section can target a sequence in
    // another, so collect each section's targets before planning any. A
    // section's OWN branches are not external to it: the planner already
    // knows them, and its loops' own backward branches are among them.
    let section_targets: Vec<std::collections::HashSet<u64>> = sections
        .iter()
        .map(|(offset, filesz, vaddr, _)| {
            let start = *offset as usize;
            let Some(end) = offset
                .checked_add(*filesz)
                .and_then(|end| usize::try_from(end).ok())
                .filter(|end| *end <= bytes.len() && start.is_multiple_of(4))
            else {
                return std::collections::HashSet::new();
            };
            let words: Vec<u32> = (0..(end - start) / 4)
                .map(|i| {
                    let at = start + 4 * i;
                    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
                })
                .collect();
            rewrite::direct_branch_targets(&words, *vaddr)
        })
        .collect();
    let mut report = FileReport::default();
    if relr_unreadable {
        report
            .skipped_segments
            .push("a packed relative relocation section is truncated".into());
        return Ok(Some(report));
    }
    for (section, (offset, filesz, vaddr, writable)) in sections.iter().copied().enumerate() {
        let external_targets: std::collections::HashSet<u64> = section_targets
            .iter()
            .enumerate()
            .filter(|(other, _)| *other != section)
            .flat_map(|(_, targets)| targets.iter().copied())
            .collect();
        let start = offset as usize;
        let end = start.saturating_add(filesz as usize);
        if end > bytes.len() || !start.is_multiple_of(4) || !vaddr.is_multiple_of(4) {
            report.skipped_segments.push(format!(
                "executable section at file offset {offset:#x} is truncated or unaligned"
            ));
            continue;
        }
        if writable {
            report.skipped_segments.push(format!(
                "executable section at {vaddr:#x} is in a writable mapping"
            ));
            continue;
        }
        let count = (end - start) / 4;
        let mut words: Vec<u32> = (0..count)
            .map(|i| {
                let at = start + 4 * i;
                u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
            })
            .collect();
        let mut plan = rewrite::plan_with_external_targets(&words, vaddr, &external_targets);
        // A relocation inside a sequence means the loader will write those
        // bytes itself, for the instruction that used to be there.
        if !relocated.is_empty() {
            let (keep, relocated_sites) = plan.patched.into_iter().partition::<Vec<_>, _>(|site| {
                let end = site.addr + 4 * site.words.len() as u64;
                !relocated
                    .iter()
                    .any(|(start, stop)| *start < end && site.addr < *stop)
            });
            plan.patched = keep;
            plan.refused.extend(
                relocated_sites
                    .into_iter()
                    .map(|site| rewrite::RefusedSite {
                        addr: site.addr,
                        reason: rewrite::Refusal::Relocated,
                    }),
            );
        }
        report.exclusive_instructions += plan.exclusive_instructions;
        for site in &plan.patched {
            *report.patched.entry(idiom_name(site.idiom)).or_default() += 1;
        }
        for refused in &plan.refused {
            *report
                .refused
                .entry(refusal_name(&refused.reason))
                .or_default() += 1;
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

/// The byte ranges a packed relative relocation section covers.
///
/// An even entry is an address and covers its own eight bytes; each odd
/// entry after it is a bitmap whose set bits name the words following that
/// address, 63 at a time.
fn decode_relr(section: &[u8]) -> Vec<(u64, u64)> {
    let mut covered = Vec::new();
    let mut next = 0u64;
    for entry in section.as_chunks::<8>().0 {
        let entry = u64::from_le_bytes(*entry);
        if entry & 1 == 0 {
            covered.push((entry, entry.saturating_add(8)));
            next = entry.saturating_add(8);
            continue;
        }
        let mut bits = entry >> 1;
        let mut at = next;
        while bits != 0 {
            if bits & 1 == 1 {
                covered.push((at, at.saturating_add(8)));
            }
            bits >>= 1;
            at = at.saturating_add(8);
        }
        next = next.saturating_add(63 * 8);
    }
    covered
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A relocation section's entries must be seen, including the packed
    /// relative ones the ELF reader does not report. The check here is on
    /// the decoder itself rather than a whole synthetic ELF: an address
    /// entry names its own eight bytes, and the odd entry after it is a
    /// bitmap of the 63 words that follow.
    #[test]
    fn packed_relative_relocations_decode_to_the_bytes_they_cover() {
        let section: Vec<u8> = [0x1000u64, (0b1011 << 1) | 1]
            .iter()
            .flat_map(|entry| entry.to_le_bytes())
            .collect();
        assert_eq!(
            decode_relr(&section),
            vec![
                (0x1000, 0x1008),
                // The bitmap starts at the word after the address, and its
                // set bits are words 0, 1 and 3 from there.
                (0x1008, 0x1010),
                (0x1010, 0x1018),
                (0x1020, 0x1028),
            ]
        );
    }
}
