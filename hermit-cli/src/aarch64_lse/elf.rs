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
//! A sequence any relocation points into is refused as well. A relocation is
//! the loader's licence to change those bytes at load time, which would
//! overwrite part of a rewritten sequence with something computed for the
//! original one.

use std::collections::BTreeMap;
use std::path::Path;

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
    let mut relocated: std::collections::HashSet<u64> = std::collections::HashSet::new();
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
        if elf.section_headers.is_empty() {
            let mut report = FileReport::default();
            report
                .skipped_segments
                .push("the file has no section headers, so code cannot be told from data".into());
            return Ok(Some(report));
        }
        relocated = elf
            .shdr_relocs
            .iter()
            .flat_map(|(_, relocs)| relocs.iter().map(|reloc| reloc.r_offset))
            .chain(elf.dynrelas.iter().map(|reloc| reloc.r_offset))
            .chain(elf.dynrels.iter().map(|reloc| reloc.r_offset))
            .chain(elf.pltrelocs.iter().map(|reloc| reloc.r_offset))
            .collect();
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
    let mut report = FileReport::default();
    for (offset, filesz, vaddr, writable) in sections {
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
        let mut plan = rewrite::plan(&words, vaddr);
        // A relocation inside a sequence means the loader will write those
        // bytes itself, for the instruction that used to be there.
        if !relocated.is_empty() {
            let (keep, relocated_sites) = plan.patched.into_iter().partition::<Vec<_>, _>(|site| {
                !(0..site.words.len() as u64).any(|i| relocated.contains(&(site.addr + 4 * i)))
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

/// Read `input`, rewrite it, and write the result to `output` with `input`'s
/// permissions. The output is written whole and renamed into place, so a
/// failed rewrite leaves no partial file behind under that name and a
/// concurrent writer of the same output cannot be seen half-written.
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
    // A unique temporary in the output's own directory: several runs may
    // rewrite the same input into the same cached output at the same time.
    // A name derived from the process id was not enough -- four concurrent
    // runs collided on it and three failed -- so take a name the filesystem
    // guarantees is unused.
    let directory = output.as_ref().parent().unwrap_or(Path::new("."));
    let mut temporary = tempfile::Builder::new()
        .prefix(".rewrite-")
        .tempfile_in(directory)?;
    use std::io::Write;
    temporary.write_all(&bytes)?;
    temporary.flush()?;
    let mode = std::fs::metadata(input)?.permissions();
    std::fs::set_permissions(temporary.path(), mode)?;
    temporary
        .persist(output.as_ref())
        .map_err(|error| error.error)?;
    Ok(Some(report))
}
