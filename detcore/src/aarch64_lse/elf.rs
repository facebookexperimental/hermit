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
//! `.init` and the like) are scanned for rewrites. Bytes an executable
//! segment carries that no such section covers are never rewritten, but
//! their literal and base readers are collected for protection, under
//! their own refusal name (`gap-reader-inside-the-sequence`): gap
//! bytes are not proved code, so data there (ordinary `.rodata` in a
//! single R+X load, the GNU ld default) can decode as a reader it is
//! not and refuse sequences only it would refuse. Unaligned gap ends
//! are rounded outward and decoded; only a gap whose bytes run past
//! the end of the file is refused by name. An executable `PT_LOAD` segment also
//! carries the ELF header, the dynamic symbol and relocation tables and
//! read-only data, and those bytes decode as instructions as readily as code
//! does: measured on one Android test binary, scanning whole segments found
//! 69 "exclusive instructions" of which 66 were in data before `.text` even
//! began. A file with no section headers is left alone and reported, because
//! nothing in it can be told apart from data.
//!
//! Within those sections, only code is rewritten ([`super::mapping`]). An
//! executable section can hold data too (a literal pool, or OpenSSL's
//! constant tables in `.text`), and a table whose words happened to form a
//! canonical loop would otherwise be rewritten, corrupting it. What is taken
//! for code depends on what the file says, section by section:
//!
//! - Mapping symbols: exactly the ranges they mark `$x`. The AArch64 ELF ABI
//!   has an assembler mark data `$d`, so data marked as the ABI says is never
//!   taken for code. This is the only evidence the rewrite accepts, because
//!   it is the only one that proves where code is. A section that also holds
//!   a mapping symbol of another kind (AArch32's `$a` or `$t`, or a `$` name
//!   this reader does not know, such as `$xyz`) is not proved by them: its
//!   sequences are left alone as `unsupported-mapping-symbol`, naming the
//!   symbol.
//! - No mapping symbols: nothing, whatever the unwind table covers. Every
//!   exclusive sequence in such a section, as in every stripped file, is
//!   left alone as `no-mapping-symbols`, so the file is left unchanged and
//!   named, never silently. An FDE covers its whole function, including a
//!   table the function branches over or keeps after a call that does not
//!   return, so the unwind table cannot prove which of its bytes are code.
//!
//! Under any evidence, a sequence an `LDR` (literal) or `ADR` in the code
//! reaches is refused (`literal-inside-the-sequence`): an object linked
//! without mapping symbols after one with them inherits the earlier `$x`
//! range. The branch analysis still reads every word of every executable
//! section, so a word that might branch into a sequence still refuses it.
//! Gap words contribute decoded direct-branch targets the same way, but
//! they are exempt from the unknown-direct-branch rule section words
//! trip: data in a gap can look like an unallocated branch, and applying
//! that rule to it would refuse nearly every image with R+X data.
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

use super::insn;
use super::mapping;
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
    /// Every sequence left alone, by address, with the reason, in the order
    /// the sections were planned: all of them, so that a listing of them
    /// (`hermit rewrite-exclusives --list-refusals`) is complete. A run logs
    /// only the first few.
    pub refused_sites: Vec<(u64, String)>,
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
    /// The sequences left alone because no mapping symbol marks their
    /// section (`no-mapping-symbols`), which is left unchanged.
    pub fn unmarked(&self) -> usize {
        self.refused
            .get(refusal_name(&Refusal::NoMappingSymbols))
            .copied()
            .unwrap_or(0)
    }

    /// The sequences left alone because their section holds a mapping symbol
    /// that is neither `$x` nor `$d` (`unsupported-mapping-symbol`).
    pub fn unsupported(&self) -> usize {
        self.refused
            .get(refusal_name(&Refusal::UnsupportedMappingSymbol(
                String::new(),
            )))
            .copied()
            .unwrap_or(0)
    }

    /// The sequences left alone because no mapping symbol this reader accepts
    /// proves their section's code: [`Self::unmarked`] and
    /// [`Self::unsupported`] together.
    pub fn unproven(&self) -> usize {
        self.unmarked() + self.unsupported()
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
        Refusal::LiteralInsideSequence => "literal-inside-the-sequence",
        Refusal::GapReaderInsideSequence => "gap-reader-inside-the-sequence",
        Refusal::AddressedData => "addressed-data-inside-the-sequence",
        Refusal::NoMappingSymbols => "no-mapping-symbols",
        Refusal::UnsupportedMappingSymbol(_) => "unsupported-mapping-symbol",
        Refusal::CompareExit => "compare-exit",
        Refusal::UnknownDirectBranch => "unknown-direct-branch",
    }
}

fn refusal_detail(reason: &Refusal) -> String {
    match reason {
        Refusal::Registers(why) => format!("register-constraint: {why}"),
        Refusal::NoMappingSymbols => "no-mapping-symbols: nothing marks this section's code \
                                      ($x, $d), so it is left unchanged"
            .to_string(),
        Refusal::UnsupportedMappingSymbol(name) => format!(
            "unsupported-mapping-symbol: `{name}` marks this section, and only `$x` and `$d` \
             prove where code is"
        ),
        other => refusal_name(other).to_string(),
    }
}

/// One rewritten sequence of a file: where it is, and its bytes before and
/// after. Same length, so applying it moves nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePatch {
    /// File offset of the sequence's first instruction.
    pub offset: u64,
    /// The sequence as the file holds it.
    pub original: Vec<u8>,
    /// The replacement.
    pub rewritten: Vec<u8>,
}

/// What [`plan_bytes`] decided for one file: the report, and every
/// replacement it would make.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FilePlan {
    /// The counts [`rewrite_bytes`] reports.
    pub report: FileReport,
    /// The replacements, in file order within each section.
    pub patches: Vec<FilePatch>,
}

/// Rewrite the executable segments of `bytes` in place.
///
/// Returns `Ok(None)` when the file is not an aarch64 ELF, which is not an
/// error: a caller that was handed the wrong file should say so and run the
/// original.
pub fn rewrite_bytes(bytes: &mut [u8]) -> Result<Option<FileReport>, goblin::error::Error> {
    let Some(plan) = plan_bytes(bytes)? else {
        return Ok(None);
    };
    for patch in &plan.patches {
        let start = patch.offset as usize;
        bytes[start..start + patch.rewritten.len()].copy_from_slice(&patch.rewritten);
    }
    Ok(Some(plan.report))
}

/// Decide the rewrite of `bytes` without applying it: the report
/// [`rewrite_bytes`] gives, and each replacement as a [`FilePatch`]. A
/// caller that applies the replacements somewhere other than the file (a
/// process's mapped pages) gets exactly the bytes a rewritten file would
/// hold. Like [`rewrite_bytes`], it rewrites only what mapping symbols
/// prove is code.
pub fn plan_bytes(bytes: &[u8]) -> Result<Option<FilePlan>, goblin::error::Error> {
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
    // Addresses control is known to start at without executing what precedes
    // them: the loader entry, defined function symbols and FDE entries.
    let mut known_entries: std::collections::HashSet<u64>;
    let (sections, functions, gap_spans, gap_unreadable) = {
        let elf = Elf::parse(bytes)?;
        if elf.header.e_machine != header::EM_AARCH64 {
            return Ok(None);
        }
        // STT_FUNC = 2, SHN_UNDEF = 0: a defined function symbol names an
        // entry a caller can reach (for example through ADR/BLR) without
        // executing the instruction before it.
        const STT_FUNC: u8 = 2;
        known_entries = std::iter::once(elf.header.e_entry)
            .filter(|entry| *entry != 0)
            .chain(
                elf.syms
                    .iter()
                    .chain(elf.dynsyms.iter())
                    .filter(|symbol| {
                        symbol.st_info & 0xf == STT_FUNC
                            && symbol.st_shndx != 0
                            && symbol.st_value != 0
                    })
                    .map(|symbol| symbol.st_value),
            )
            .collect();
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
        let skip_file = |why: &str| {
            let mut report = FileReport::default();
            report.skipped_segments.push(why.into());
            Ok(Some(FilePlan {
                report,
                patches: Vec::new(),
            }))
        };
        if elf.section_headers.is_empty() {
            return skip_file("the file has no section headers, so code cannot be told from data");
        }
        let evidence = file_code_evidence(bytes, &elf);
        let sections = elf
            .section_headers
            .iter()
            .enumerate()
            .filter(|(_, sh)| {
                sh.sh_type == section_header::SHT_PROGBITS
                    && sh.sh_flags & u64::from(section_header::SHF_EXECINSTR) != 0
                    && sh.sh_size != 0
            })
            .map(|(index, sh)| {
                let writable = writable_executable
                    .iter()
                    .any(|(start, end)| sh.sh_addr >= *start && sh.sh_addr < *end);
                let start = usize::try_from(sh.sh_offset).ok();
                let words: Option<Vec<u32>> = start
                    .zip(usize::try_from(sh.sh_size).ok())
                    .and_then(|(start, size)| bytes.get(start..start.checked_add(size)?))
                    .filter(|_| sh.sh_offset.is_multiple_of(4) && sh.sh_addr.is_multiple_of(4))
                    .map(|section| {
                        section
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .map(|word| u32::from_le_bytes(*word))
                            .collect()
                    });
                let held = evidence.classify(index, sh.sh_addr, sh.sh_size);
                // Only `$x` and `$d` prove where code is. A section that also
                // holds a mapping symbol of another kind is not read for code,
                // and neither is a section no mapping symbol marks.
                let unproven = match evidence.unsupported_in(index) {
                    Some(name) => Some(Refusal::UnsupportedMappingSymbol(name.to_owned())),
                    None => (held.evidence != Some(mapping::Evidence::MappingSymbols))
                        .then_some(Refusal::NoMappingSymbols),
                };
                // Under the unwind table alone, code is what control can
                // reach from the functions' entries: a table a function
                // branches over, or keeps after its return, is not.
                let (code, unknown) = match (&words, held.evidence) {
                    _ if unproven.is_some() => {
                        (Vec::new(), vec![(sh.sh_addr, sh.sh_addr + sh.sh_size)])
                    }
                    _ => (held.code, held.unknown),
                };
                Section {
                    offset: sh.sh_offset,
                    vaddr: sh.sh_addr,
                    size: sh.sh_size,
                    writable,
                    words,
                    evidence: held.evidence,
                    unproven,
                    code,
                    unknown,
                }
            })
            .collect::<Vec<_>>();
        // Bytes an executable segment carries that no executable
        // section covers (segment gaps, or bytes in sections
        // without SHF_EXECINSTR) are never rewritten, but a reader
        // there can still read rewritten bytes, so collect its
        // words for protection only. Only a gap whose nominal
        // bytes run past the end of the file is refused.
        let mut gap_spans: Vec<(Vec<u32>, u64)> = Vec::new();
        let mut gap_unreadable = false;
        // A scan domain: a VA interval, the translation that maps
        // its addresses to file offsets (`base_off + (va -
        // base_va)`, signed, never saturated), and the nominal
        // load ranges whose file backing must be complete.
        type Domain = (u64, u64, u64, u64, Vec<(u64, u64)>);
        let translate = |base_va: u64, base_off: u64, va: u64| -> Option<u64> {
            if va >= base_va {
                base_off.checked_add(va - base_va)
            } else {
                base_off.checked_sub(base_va - va)
            }
        };
        let load_domain = |ph: &goblin::elf::program_header::ProgramHeader| -> Domain {
            // The largest aarch64 page, 64 KiB: the kernel's page
            // size is not in the ELF, so every load is scanned
            // over its 64 KiB-rounded filesz domain, extended
            // through the file-backed part of its memsz (a musl
            // loader leaves a non-writable load's memsz tail as
            // the file bytes it mapped).
            let extent = ph.p_filesz.max(ph.p_memsz);
            (
                ph.p_vaddr & !0xffff,
                ph.p_vaddr.saturating_add(extent).saturating_add(0xffff) & !0xffff,
                ph.p_vaddr,
                ph.p_offset,
                vec![(ph.p_vaddr, ph.p_vaddr.saturating_add(ph.p_filesz))],
            )
        };
        let loads: Vec<&goblin::elf::program_header::ProgramHeader> = elf
            .program_headers
            .iter()
            .filter(|ph| ph.p_type == program_header::PT_LOAD)
            .collect();
        let mut domains: Vec<Domain> = loads
            .iter()
            .filter(|ph| ph.is_executable())
            .map(|ph| load_domain(ph))
            .collect();
        // Loader-created executable domains, for dynamic images
        // only (a static image's holes are mapped by nobody).
        let is_dynamic = elf.header.e_type == 3
            || elf
                .program_headers
                .iter()
                .any(|ph| ph.p_type == 2 || ph.p_type == 3);
        if is_dynamic && !loads.is_empty() {
            // DT_TEXTREL, read from the dynamic segment's entries.
            let mut textrel = false;
            for ph in elf.program_headers.iter().filter(|ph| ph.p_type == 2) {
                let start = ph.p_offset as usize;
                let count = (ph.p_filesz as usize) / 16;
                for i in 0..count {
                    let Some(entry) = bytes.get(start + 16 * i..start + 16 * i + 16) else {
                        break;
                    };
                    let tag = i64::from_le_bytes(entry[..8].try_into().unwrap());
                    let val = u64::from_le_bytes(entry[8..].try_into().unwrap());
                    if tag == 22 || (tag == 30 && val & 0x4 != 0) {
                        textrel = true;
                    }
                    if tag == 0 {
                        break;
                    }
                }
            }
            let lowest = loads
                .iter()
                .min_by_key(|ph| ph.p_vaddr)
                .expect("loads is non-empty");
            if textrel {
                // Every load is remapped through its own
                // translation before the image becomes executable
                // everywhere, so every load has a domain.
                for ph in loads.iter().filter(|ph| !ph.is_executable()) {
                    domains.push(load_domain(ph));
                }
            }
            if lowest.is_executable() || textrel {
                // The musl-style initial map: the whole image span
                // with the lowest load's permissions, holes left
                // executable. Its span is exact only at the host
                // page size, which is not monotone in that size,
                // so take the union at 4, 16 and 64 KiB, each span
                // decoded through its own translation and kept
                // distinct from the others.
                let min_vaddr = lowest.p_vaddr;
                let max_mem_end = loads
                    .iter()
                    .map(|ph| ph.p_vaddr.saturating_add(ph.p_memsz))
                    .max()
                    .expect("loads is non-empty");
                // (initial spans carry no nominal range: they are
                // hypothetical mappings, clipped to the file, and
                // only a real load's domain refuses as truncated)
                for page in [0x1000u64, 0x4000, 0x10000] {
                    let a = min_vaddr & !(page - 1);
                    let o = lowest.p_offset & !(page - 1);
                    let c = max_mem_end.saturating_add(page - 1) & !(page - 1);
                    let span_end = c.saturating_add(o);
                    domains.push((a, span_end, a, o, Vec::new()));
                }
            }
        }
        for (seg_start, seg_end, base_va, base_off, nominal) in &domains {
            let (seg_start, seg_end) = (*seg_start, *seg_end);
            let file_offset = |va: u64| translate(*base_va, *base_off, va);
            // A section interval covers domain bytes only where
            // the section scanner reads the same file bytes at the
            // same addresses under this translation: its interval
            // must intersect the domain, and its file offset for
            // the intersection must agree. Sections in other
            // domains, and sections whose backing disagrees, cover
            // nothing here.
            let mut covered: Vec<(u64, u64)> = sections
                .iter()
                .filter_map(|section| {
                    // An unreadable section covers nothing: its
                    // bytes join the gap scan instead.
                    section.words.as_ref()?;
                    let start = section.vaddr.max(seg_start);
                    let end = section.vaddr.saturating_add(section.size).min(seg_end);
                    if start >= end {
                        return None;
                    }
                    let via_section = section.offset.checked_add(start - section.vaddr)?;
                    (file_offset(start) == Some(via_section)).then_some((start, end))
                })
                .collect();
            covered.sort_unstable();
            let mut cursor = seg_start;
            let mut gaps: Vec<(u64, u64)> = Vec::new();
            for &(start, end) in &covered {
                if start > cursor {
                    gaps.push((cursor, start));
                }
                cursor = cursor.max(end);
            }
            if cursor < seg_end {
                gaps.push((cursor, seg_end));
            }
            for (start, end) in gaps {
                if start >= end {
                    continue;
                }
                let first = start & !3;
                let last = end.saturating_add(3) & !3;
                // Clip a partly file-backed prefix, preserving
                // PCs: the first VA whose file offset is 0
                // (`base_va - base_off` in signed arithmetic)
                // advances the span by exactly that displacement.
                let file_start_va = *base_va as i128 - *base_off as i128;
                let first = if (first as i128) < file_start_va {
                    u64::try_from(file_start_va).unwrap_or(u64::MAX)
                } else {
                    first
                };
                if first >= last {
                    continue;
                }
                // Clip to actual file backing, preserving PCs: a
                // hypothetical interval wholly outside the file is
                // empty, and a rounded tail past the file holds no
                // bytes. Only a gap whose NOMINAL load bytes run
                // past the file is truncated, refused below.
                let nominal_ok =
                    nominal
                        .iter()
                        .filter(|(s, e)| *s < last && first < *e)
                        .all(|(_, e)| {
                            file_offset(*e).is_some_and(|limit| (limit as usize) <= bytes.len())
                        });
                let Some(offset) = file_offset(first)
                    .and_then(|offset| usize::try_from(offset).ok())
                    .zip(usize::try_from(last - first).ok())
                    .and_then(|(offset, size)| {
                        let end = offset.checked_add(size)?;
                        if end <= bytes.len() {
                            bytes.get(offset..end)
                        } else if nominal_ok {
                            bytes.get(offset..bytes.len())
                        } else {
                            None
                        }
                    })
                else {
                    // No file byte at all under this translation:
                    // empty unless a nominal range is truncated.
                    let intersects = nominal.iter().any(|(s, e)| *s < last && first < *e);
                    if intersects && !nominal_ok {
                        gap_unreadable = true;
                    }
                    continue;
                };
                if offset.is_empty() {
                    continue;
                }
                gap_spans.push((
                    offset
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|word| u32::from_le_bytes(*word))
                        .collect(),
                    first,
                ));
            }
        }
        let unwind = evidence.unwind.unwrap_or_default();
        (sections, unwind, gap_spans, gap_unreadable)
    };
    // Every FDE start is likewise an entry control can begin at.
    known_entries.extend(functions.iter().map(|(entry, _)| *entry));
    // A direct branch in one executable section can target a sequence in
    // another, so collect each section's targets before planning any. A
    // section's OWN branches are not external to it: the planner already
    // knows them, and its loops' own backward branches are among them.
    let section_targets: Vec<std::collections::HashSet<u64>> = sections
        .iter()
        .map(|section| match &section.words {
            Some(words) => rewrite::direct_branch_targets(words, section.vaddr),
            None => std::collections::HashSet::new(),
        })
        .collect();
    // Collect `LDR` (literal) and `ADR` references, and what each `ADRP`
    // lets the code after it read, from code and unknown executable ranges.
    // An unproven section stays unchanged, but its readers can still observe
    // rewritten bytes in a different, marked section. Proven data
    // contributes no references. Sort the targets below.
    // An `ADR` that names a function's own entry takes a pointer to the
    // function. A literal load reads its instruction bytes even there,
    // so only the address-taking can allow that function's first sequence.
    let entry_of = |target: u64| {
        functions
            .binary_search_by_key(&target, |(entry, _)| *entry)
            .is_ok()
    };
    // Each possibly executable range of each readable section, as words
    // and the PC of the first. Include every word intersecting a range, even
    // at a partial-word boundary; decode at that word's actual PC, not the
    // possibly unaligned span start.
    let spans: Vec<(&[u32], u64)> = sections
        .iter()
        .filter_map(|section| Some((section, section.words.as_ref()?)))
        .flat_map(|(section, words)| {
            section
                .code
                .iter()
                .chain(&section.unknown)
                .map(move |(start, end)| {
                    let first = (((start - section.vaddr) / 4) as usize).min(words.len());
                    let last = (((end - section.vaddr).div_ceil(4)) as usize).min(words.len());
                    (&words[first..last], section.vaddr + 4 * first as u64)
                })
        })
        .collect();
    // Gap words are protection-only readers: they are scanned for
    // literal and base references like section code, never rewritten,
    // and their refusals are named apart from proven code's.
    let gap_span_refs: Vec<(&[u32], u64)> = gap_spans
        .iter()
        .map(|(words, pc)| (words.as_slice(), *pc))
        .collect();
    // Direct branches out of gap bytes target sequences as surely
    // as branches out of sections do.
    let gap_targets: std::collections::HashSet<u64> = gap_span_refs
        .iter()
        .flat_map(|(words, pc)| rewrite::direct_branch_targets(words, *pc))
        .collect();
    let literals_of = |spans: &[(&[u32], u64)]| -> Vec<(u64, u64)> {
        spans
            .iter()
            .flat_map(|(words, pc)| mapping::literal_references(words, *pc))
            .filter(|reference| {
                reference.kind != mapping::LiteralKind::Address || !entry_of(reference.range.0)
            })
            .map(|reference| reference.range)
            .collect()
    };
    let mut literals: Vec<(u64, u64)> = literals_of(&spans);
    let mut gap_literals: Vec<(u64, u64)> = literals_of(&gap_span_refs);
    // What the code reads through an `ADRP`'s page or an `ADR`'s address
    // ([`mapping::base_references`]): an access of known address and width
    // reads those bytes, and an `ADD` from it takes an address, as `ADR`
    // does, which is not a read of the word there where it is an FDE start:
    // it takes a function pointer. Any use whose extent is not known may
    // read any executable byte of the image, a function's entry included:
    // a function pointer passed on, or still held, can be read as well as
    // called.
    let executable: Vec<(u64, u64)> = sections
        .iter()
        .map(|section| (section.vaddr, section.vaddr.saturating_add(section.size)))
        .collect();
    let mut unbounded: Vec<(u64, u64)> = Vec::new();
    let mut gap_unbounded: Vec<(u64, u64)> = Vec::new();
    for (span_set, is_gap) in [(&spans, false), (&gap_span_refs, true)] {
        for reference in span_set
            .iter()
            .flat_map(|(words, pc)| mapping::base_references(words, *pc))
        {
            let (ranges, targets) = if is_gap {
                (&mut gap_unbounded, &mut gap_literals)
            } else {
                (&mut unbounded, &mut literals)
            };
            match reference {
                mapping::BaseReference::Access(start, end) => ranges.push((start, end)),
                mapping::BaseReference::Address(at) if !entry_of(at) => {
                    targets.push((at, at.wrapping_add(4)))
                }
                mapping::BaseReference::Address(_) => {}
                mapping::BaseReference::Unknown { .. } => ranges.extend(&executable),
            }
        }
    }
    literals.sort_unstable();
    gap_literals.sort_unstable();
    unbounded.sort_unstable();
    unbounded.dedup();
    gap_unbounded.sort_unstable();
    gap_unbounded.dedup();
    // No addressed-data extents are computed: without mapping symbols a
    // section is unproven and left alone, so no data exclusion is needed.
    let extents: Vec<(u64, u64)> = Vec::new();
    // Compare idioms are left alone as compare-exit whether or not any
    // other section holds a store-exclusive, so no external-store scan is
    // needed here.
    //
    // The fail-closed entry rule across the image: a word that might be a
    // direct branch with an unknown target, anywhere but in bytes marked
    // data, could be aimed into any executable section, so no section's
    // sequences are rewritten. Bytes marked data are not executed; a
    // section nothing proves is scanned whole, and one that cannot be read
    // could hold anything.
    let unknown_direct_branch = sections.iter().any(|section| {
        let Some(words) = &section.words else {
            return true;
        };
        words.iter().enumerate().any(|(index, word)| {
            let addr = section.vaddr + 4 * index as u64;
            (in_spans(&section.code, addr) || in_spans(&section.unknown, addr))
                && rewrite::unknown_direct_branch_possible(*word)
        })
    });
    let mut report = FileReport::default();
    let mut patches = Vec::new();
    if gap_unreadable {
        report.skipped_segments.push(
            "executable segment bytes not covered by an executable section are truncated, \
             so their readers cannot be checked"
                .into(),
        );
        return Ok(Some(FilePlan { report, patches }));
    }
    if relr_unreadable {
        report
            .skipped_segments
            .push("a packed relative relocation section is truncated".into());
        return Ok(Some(FilePlan { report, patches }));
    }
    for (index, section) in sections.into_iter().enumerate() {
        let Section {
            offset,
            vaddr,
            size: _,
            writable,
            words,
            evidence,
            unproven,
            code,
            unknown,
        } = section;
        let mut external_targets: std::collections::HashSet<u64> = section_targets
            .iter()
            .enumerate()
            .filter(|(other, _)| *other != index)
            .flat_map(|(_, targets)| targets.iter().copied())
            .collect();
        external_targets.extend(&gap_targets);
        let Some(words) = words else {
            report.skipped_segments.push(format!(
                "executable section at file offset {offset:#x} is truncated or unaligned"
            ));
            continue;
        };
        if writable {
            report.skipped_segments.push(format!(
                "executable section at {vaddr:#x} is in a writable mapping"
            ));
            continue;
        }
        // Nothing proves this section's code, so nothing in it is rewritten:
        // each sequence the planner finds is named as left alone for that
        // reason, whatever else it would have said of it.
        if let Some(reason) = unproven {
            let plan = rewrite::plan_section(&words, vaddr, &external_targets, &known_entries);
            report.exclusive_instructions += plan.exclusive_instructions;
            let mut sites: Vec<u64> = plan
                .patched
                .iter()
                .map(|site| site.addr)
                .chain(plan.refused.iter().map(|refused| refused.addr))
                .collect();
            sites.sort_unstable();
            for addr in sites {
                *report.refused.entry(refusal_name(&reason)).or_default() += 1;
                report.refused_sites.push((addr, refusal_detail(&reason)));
            }
            continue;
        }
        // Exclusive instructions in bytes neither known code nor marked
        // data are left alone, and said to be.
        let unknown = words
            .iter()
            .enumerate()
            .filter(|(index, word)| {
                is_exclusive(**word) && in_spans(&unknown, vaddr + 4 * *index as u64)
            })
            .count();
        if evidence.is_none() {
            report.skipped_segments.push(format!(
                "executable section at {vaddr:#x} has no mapping symbols and the file no unwind \
                 table, so its code cannot be told from data ({unknown} exclusive instructions \
                 left alone)"
            ));
            continue;
        }
        let mut plan = rewrite::plan_section(&words, vaddr, &external_targets, &known_entries);
        restrict_to_code(&mut plan, &words, vaddr, &code);
        // Section readers before gap readers, so a refusal is
        // named for gap words only when no proven reader causes it.
        refuse_literals(&mut plan, &literals, Refusal::LiteralInsideSequence);
        refuse_overlapping(&mut plan, &unbounded, Refusal::LiteralInsideSequence);
        refuse_literals(&mut plan, &gap_literals, Refusal::GapReaderInsideSequence);
        refuse_overlapping(&mut plan, &gap_unbounded, Refusal::GapReaderInsideSequence);
        refuse_overlapping(&mut plan, &extents, Refusal::AddressedData);
        if unknown_direct_branch {
            rewrite::refuse_patched(&mut plan, Refusal::UnknownDirectBranch);
        }
        if unknown != 0 {
            report.skipped_segments.push(format!(
                "executable section at {vaddr:#x}: {unknown} exclusive instructions in bytes not \
                 known to be code were left alone"
            ));
        }
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
            report
                .refused_sites
                .push((refused.addr, refusal_detail(&refused.reason)));
        }
        // Each site replaces exactly its own words (`rewrite::apply`), so
        // the patch is those words before and after.
        let start = offset as usize;
        for site in &plan.patched {
            let index = ((site.addr - vaddr) / 4) as usize;
            let to_bytes = |words: &[u32]| -> Vec<u8> {
                words.iter().flat_map(|word| word.to_le_bytes()).collect()
            };
            patches.push(FilePatch {
                offset: (start + 4 * index) as u64,
                original: to_bytes(&words[index..index + site.words.len()]),
                rewritten: to_bytes(&site.words),
            });
        }
    }
    Ok(Some(FilePlan { report, patches }))
}

/// One executable section, as [`rewrite_bytes`] plans it.
struct Section {
    /// Its file offset and address.
    offset: u64,
    vaddr: u64,
    /// Its size in bytes, as its header gives it.
    size: u64,
    /// Whether a writable mapping holds it.
    writable: bool,
    /// Its instruction words; `None` when it is truncated or unaligned.
    words: Option<Vec<u32>>,
    /// What its code was found by; `None` when nothing speaks for it.
    evidence: Option<mapping::Evidence>,
    /// Why it is left alone without being read for code, if it is: no
    /// mapping symbol marks it, or it holds a mapping symbol that is neither
    /// `$x` nor `$d`.
    unproven: Option<Refusal>,
    /// Its code, as sorted, disjoint address ranges.
    code: Vec<(u64, u64)>,
    /// Its bytes neither known code nor marked data, likewise.
    unknown: Vec<(u64, u64)>,
}

/// Whether `addr` lies in one of `spans` (sorted, disjoint).
fn in_spans(spans: &[(u64, u64)], addr: u64) -> bool {
    let after = spans.partition_point(|(start, _)| *start <= addr);
    after.checked_sub(1).is_some_and(|i| addr < spans[i].1)
}

/// Whether `word` is a load-exclusive, a store-exclusive or an exclusive
/// pair instruction.
fn is_exclusive(word: u32) -> bool {
    matches!(
        insn::decode(word),
        insn::Insn::LoadExcl { .. } | insn::Insn::StoreExcl { .. } | insn::Insn::ExclPair
    )
}

/// What tells an ELF image's code from its data ([`mapping::CodeEvidence`]):
/// nothing for a file whose section header table or tables lie outside it.
fn file_code_evidence(bytes: &[u8], elf: &Elf<'_>) -> mapping::CodeEvidence {
    let slice = |(offset, size): (u64, u64)| {
        let start = usize::try_from(offset).ok()?;
        bytes.get(start..start.checked_add(usize::try_from(size).ok()?)?)
    };
    let Some(table) = slice((
        elf.header.e_shoff,
        (elf.section_headers.len() as u64).saturating_mul(64),
    )) else {
        return mapping::CodeEvidence::default();
    };
    mapping::CodeEvidence::read(
        table,
        usize::from(elf.header.e_shstrndx),
        elf.header.e_type,
        |range| slice(range).map(<[u8]>::to_vec),
    )
}

/// Refuse each rewrite whose sequence's bytes an `LDR` (literal) or `ADR`
/// in the image's code reaches (`targets`, sorted): those bytes may be a
/// literal pool. Whatever the evidence: an object linked without mapping
/// symbols after one with them inherits the earlier `$x` range, and a real
/// loop is never a literal's target.
fn refuse_literals(plan: &mut rewrite::Report, targets: &[(u64, u64)], reason: Refusal) {
    // No target is longer than 16 bytes (`LDR Qt`).
    const LONGEST: u64 = 16;
    let patched = std::mem::take(&mut plan.patched);
    for site in patched {
        let end = site.addr + 4 * site.words.len() as u64;
        let from = targets.partition_point(|(start, _)| start.saturating_add(LONGEST) <= site.addr);
        let reached = targets[from..]
            .iter()
            .take_while(|(start, _)| *start < end)
            .any(|(start, stop)| *start < end && site.addr < *stop);
        if reached {
            plan.refused.push(rewrite::RefusedSite {
                addr: site.addr,
                reason: reason.clone(),
            });
        } else {
            plan.patched.push(site);
        }
    }
}

/// Refuse, for `reason`, each rewrite whose sequence overlaps one of
/// `ranges` (sorted by start; overlapping or nested ranges are handled, so
/// a short range cannot shadow a longer one).
fn refuse_overlapping(plan: &mut rewrite::Report, ranges: &[(u64, u64)], reason: Refusal) {
    let patched = std::mem::take(&mut plan.patched);
    for site in patched {
        let end = site.addr + 4 * site.words.len() as u64;
        let after = ranges.partition_point(|(start, _)| *start < end);
        if ranges[..after]
            .iter()
            .any(|(start, stop)| *start < end && site.addr < *stop)
        {
            plan.refused.push(rewrite::RefusedSite {
                addr: site.addr,
                reason: reason.clone(),
            });
        } else {
            plan.patched.push(site);
        }
    }
}

/// Keep of a section's plan only what is in its code, the address ranges
/// `code` (sorted, disjoint): a site is placed by its store-exclusive, as
/// the planner reports a refusal. A site whose store-exclusive is in data
/// is not an instruction and goes, a rewrite whose sequence runs out of the
/// code becomes a refusal, and only exclusive instructions in the code are
/// counted.
fn restrict_to_code(plan: &mut rewrite::Report, words: &[u32], base: u64, code: &[(u64, u64)]) {
    let span_of = |addr: u64| {
        let after = code.partition_point(|(start, _)| *start <= addr);
        after
            .checked_sub(1)
            .map(|i| code[i])
            .filter(|(_, end)| addr < *end)
    };
    let at = |index: usize| base + 4 * index as u64;
    plan.exclusive_instructions = words
        .iter()
        .enumerate()
        .filter(|(index, word)| span_of(at(*index)).is_some() && is_exclusive(**word))
        .count();
    plan.refused.retain(|site| span_of(site.addr).is_some());
    let patched = std::mem::take(&mut plan.patched);
    for site in patched {
        let first = ((site.addr - base) / 4) as usize;
        let store = (first..first + site.words.len())
            .find(|index| matches!(insn::decode(words[*index]), insn::Insn::StoreExcl { .. }))
            .map_or(site.addr, at);
        let end = site.addr + 4 * site.words.len() as u64;
        // A sub-word compare against a register is rewritten only because
        // the instruction before the loop zero-extends that register
        // (`rewrite::cas_compare`), which must then be code too.
        let needs_before = site.idiom == Idiom::CasCompare
            && matches!(
                insn::decode(words[first]),
                insn::Insn::LoadExcl {
                    size: insn::Size::B | insn::Size::H,
                    ..
                }
            )
            && matches!(
                words.get(first + 1).map(|word| insn::decode(*word)),
                Some(insn::Insn::Cmp {
                    operand: insn::Operand::Reg(_),
                    ..
                })
            );
        match span_of(store) {
            None => {}
            Some((start, stop))
                if start <= site.addr
                    && end <= stop
                    && (!needs_before || start + 4 <= site.addr) =>
            {
                plan.patched.push(site)
            }
            Some((start, stop)) if start <= site.addr && end <= stop => {
                plan.refused.push(rewrite::RefusedSite {
                    addr: store,
                    reason: Refusal::Registers("a sub-word compare against a register"),
                })
            }
            Some(_) => plan.refused.push(rewrite::RefusedSite {
                addr: store,
                reason: Refusal::NotCanonical,
            }),
        }
    }
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
pub(crate) mod tests {
    use super::*;

    /// A minimal aarch64 ELF image: one read-and-execute `PT_LOAD` covering
    /// the file, a `.text` section (index 1) holding `words` at file offset
    /// and address 0x100, and, unless `marks` is `None`, a symbol table
    /// whose mapping symbols are `marks`: (section index, word index, code).
    pub(crate) fn elf_image(words: &[u32], marks: Option<&[(u16, usize, bool)]>) -> Vec<u8> {
        elf_image_with(words, marks, None)
    }

    /// [`elf_image`] with, unless `functions` is `None`, an unwind table
    /// whose FDEs cover `functions`: (first word, end word) of `.text`.
    fn elf_image_with(
        words: &[u32],
        marks: Option<&[(u16, usize, bool)]>,
        functions: Option<&[(usize, usize)]>,
    ) -> Vec<u8> {
        elf_image_full(words, marks, functions, &[])
    }

    /// [`elf_image_with`] plus, when `pointers` is not empty, a loaded,
    /// non-executable `.data` section holding the address of each named
    /// word of `.text`, as a program stores a pointer to a table.
    fn elf_image_full(
        words: &[u32],
        marks: Option<&[(u16, usize, bool)]>,
        functions: Option<&[(usize, usize)]>,
        pointers: &[usize],
    ) -> Vec<u8> {
        const TEXT: usize = 0x100;
        let text_len = 4 * words.len();
        let shstrtab = b"\0.text\0.shstrtab\0.symtab\0.strtab\0.eh_frame\0";
        let strtab = b"\0$x\0$d\0abort\0";
        let symbols: Vec<[u8; 24]> = marks
            .unwrap_or_default()
            .iter()
            .map(|(section, word, code)| {
                let name = if *code { 1 } else { 4 };
                mapping::tests::symbol(name, 0, *section, (TEXT + 4 * word) as u64)
            })
            .collect();
        let shstrtab_at = TEXT + text_len;
        let strtab_at = shstrtab_at + shstrtab.len();
        let symtab_at = (strtab_at + strtab.len()).next_multiple_of(8);
        let symtab_len = 24 * (symbols.len() + 1);
        let eh_frame_at = (symtab_at + symtab_len).next_multiple_of(8);
        let eh_frame = functions.map(|functions| {
            let functions: Vec<(u64, u64)> = functions
                .iter()
                .map(|(start, end)| ((TEXT + 4 * start) as u64, (TEXT + 4 * end) as u64))
                .collect();
            mapping::tests::eh_frame(eh_frame_at as u64, &functions)
        });
        let eh_frame_len = eh_frame.as_ref().map_or(0, Vec::len);
        let data_at = (eh_frame_at + eh_frame_len).next_multiple_of(8);
        let data_len = 8 * pointers.len();
        let sections = 3
            + if marks.is_some() { 2 } else { 0 }
            + usize::from(eh_frame.is_some())
            + usize::from(!pointers.is_empty());
        let shoff = (data_at + data_len).next_multiple_of(8);
        let total = shoff + sections * 64;
        let mut bytes = vec![0u8; total];
        let put = |bytes: &mut Vec<u8>, at: usize, value: &[u8]| {
            bytes[at..at + value.len()].copy_from_slice(value)
        };
        // ELF header: ELFCLASS64, little-endian, ET_DYN, EM_AARCH64.
        put(&mut bytes, 0, b"\x7fELF\x02\x01\x01");
        put(&mut bytes, 16, &3u16.to_le_bytes());
        put(&mut bytes, 18, &183u16.to_le_bytes());
        put(&mut bytes, 20, &1u32.to_le_bytes());
        put(&mut bytes, 32, &64u64.to_le_bytes()); // e_phoff
        put(&mut bytes, 40, &(shoff as u64).to_le_bytes());
        put(&mut bytes, 52, &64u16.to_le_bytes()); // e_ehsize
        put(&mut bytes, 54, &56u16.to_le_bytes()); // e_phentsize
        put(&mut bytes, 56, &1u16.to_le_bytes()); // e_phnum
        put(&mut bytes, 58, &64u16.to_le_bytes()); // e_shentsize
        put(&mut bytes, 60, &(sections as u16).to_le_bytes()); // e_shnum
        put(&mut bytes, 62, &2u16.to_le_bytes()); // e_shstrndx
        // PT_LOAD, PF_R | PF_X, the whole file at address 0.
        put(&mut bytes, 64, &1u32.to_le_bytes());
        put(&mut bytes, 68, &5u32.to_le_bytes());
        put(&mut bytes, 64 + 32, &(total as u64).to_le_bytes()); // p_filesz
        put(&mut bytes, 64 + 40, &(total as u64).to_le_bytes()); // p_memsz
        put(&mut bytes, 64 + 48, &0x1000u64.to_le_bytes());
        for (i, word) in words.iter().enumerate() {
            put(&mut bytes, TEXT + 4 * i, &word.to_le_bytes());
        }
        put(&mut bytes, shstrtab_at, shstrtab);
        put(&mut bytes, strtab_at, strtab);
        for (i, symbol) in symbols.iter().enumerate() {
            put(&mut bytes, symtab_at + 24 * (i + 1), symbol);
        }
        if let Some(eh_frame) = &eh_frame {
            put(&mut bytes, eh_frame_at, eh_frame);
        }
        for (i, word) in pointers.iter().enumerate() {
            put(
                &mut bytes,
                data_at + 8 * i,
                &((TEXT + 4 * word) as u64).to_le_bytes(),
            );
        }
        // Section headers: null, .text (PROGBITS, ALLOC | EXECINSTR),
        // .shstrtab, and with marks .symtab (linked to 4) and .strtab.
        let header = |bytes: &mut Vec<u8>, index: usize, fields: [u64; 8]| {
            let at = shoff + 64 * index;
            let [name, kind, flags, addr, offset, size, link, entsize] = fields;
            put(bytes, at, &(name as u32).to_le_bytes());
            put(bytes, at + 4, &(kind as u32).to_le_bytes());
            put(bytes, at + 8, &flags.to_le_bytes());
            put(bytes, at + 16, &addr.to_le_bytes());
            put(bytes, at + 24, &offset.to_le_bytes());
            put(bytes, at + 32, &size.to_le_bytes());
            put(bytes, at + 40, &(link as u32).to_le_bytes());
            put(bytes, at + 48, &4u64.to_le_bytes());
            put(bytes, at + 56, &entsize.to_le_bytes());
        };
        let (text, len) = (TEXT as u64, text_len as u64);
        header(&mut bytes, 1, [1, 1, 6, text, text, len, 0, 0]);
        header(
            &mut bytes,
            2,
            [7, 3, 0, 0, shstrtab_at as u64, shstrtab.len() as u64, 0, 0],
        );
        if marks.is_some() {
            let (at, len) = (symtab_at as u64, symtab_len as u64);
            header(&mut bytes, 3, [17, 2, 0, 0, at, len, 4, 24]);
            header(
                &mut bytes,
                4,
                [25, 3, 0, 0, strtab_at as u64, strtab.len() as u64, 0, 0],
            );
        }
        if eh_frame.is_some() {
            // .eh_frame: PROGBITS, ALLOC, mapped at its file offset.
            let (at, len) = (eh_frame_at as u64, eh_frame_len as u64);
            let index = sections - 1 - usize::from(!pointers.is_empty());
            header(&mut bytes, index, [33, 1, 2, at, at, len, 0, 0]);
        }
        if !pointers.is_empty() {
            // The pointers' section: PROGBITS, ALLOC, not executable,
            // mapped at its file offset. It borrows `.eh_frame`'s name,
            // which nothing here reads.
            let (at, len) = (data_at as u64, data_len as u64);
            header(&mut bytes, sections - 1, [33, 1, 2, at, at, len, 0, 0]);
        }
        bytes
    }

    /// A minimal two-executable-section image at 0x400000: `.text.loop`
    /// at file offset/address offset 0x100 and `.text.out` at 0x200, each
    /// marked `$x`, in one read-and-execute `PT_LOAD`, entered at the
    /// first section.
    fn elf_image_two_sections(first: &[u32], second: &[u32]) -> Vec<u8> {
        const BASE: u64 = 0x400000;
        let offsets = [0x100usize, 0x200];
        let names = b"\0.text.loop\0.text.out\0.shstrtab\0.symtab\0.strtab\0";
        let strings = b"\0$x\0";
        let mut syms = vec![0u8; 24];
        for (i, offset) in offsets.iter().enumerate() {
            syms.extend_from_slice(&mapping::tests::symbol(
                1,
                0,
                (i + 1) as u16,
                BASE + *offset as u64,
            ));
        }
        let names_at = offsets[1] + 4 * second.len();
        let str_at = names_at + names.len();
        let sym_at = (str_at + strings.len()).next_multiple_of(8);
        let shoff = (sym_at + syms.len()).next_multiple_of(8);
        let sections = 6usize;
        let total = shoff + sections * 64;
        let mut bytes = vec![0u8; total];
        let put = |bytes: &mut Vec<u8>, at: usize, value: &[u8]| {
            bytes[at..at + value.len()].copy_from_slice(value)
        };
        put(&mut bytes, 0, b"\x7fELF\x02\x01\x01");
        put(&mut bytes, 16, &2u16.to_le_bytes());
        put(&mut bytes, 18, &183u16.to_le_bytes());
        put(&mut bytes, 20, &1u32.to_le_bytes());
        put(&mut bytes, 24, &(BASE + 0x100).to_le_bytes());
        put(&mut bytes, 32, &64u64.to_le_bytes());
        put(&mut bytes, 40, &(shoff as u64).to_le_bytes());
        put(&mut bytes, 52, &64u16.to_le_bytes());
        put(&mut bytes, 54, &56u16.to_le_bytes());
        put(&mut bytes, 56, &1u16.to_le_bytes());
        put(&mut bytes, 58, &64u16.to_le_bytes());
        put(&mut bytes, 60, &(sections as u16).to_le_bytes());
        put(&mut bytes, 62, &3u16.to_le_bytes());
        put(&mut bytes, 64, &1u32.to_le_bytes());
        put(&mut bytes, 68, &5u32.to_le_bytes());
        put(&mut bytes, 72, &0u64.to_le_bytes());
        put(&mut bytes, 80, &BASE.to_le_bytes());
        put(&mut bytes, 88, &BASE.to_le_bytes());
        put(&mut bytes, 96, &(total as u64).to_le_bytes());
        put(&mut bytes, 104, &(total as u64).to_le_bytes());
        put(&mut bytes, 112, &0x1000u64.to_le_bytes());
        for (i, word) in first.iter().enumerate() {
            put(&mut bytes, offsets[0] + 4 * i, &word.to_le_bytes());
        }
        for (i, word) in second.iter().enumerate() {
            put(&mut bytes, offsets[1] + 4 * i, &word.to_le_bytes());
        }
        put(&mut bytes, names_at, names);
        put(&mut bytes, str_at, strings);
        put(&mut bytes, sym_at, &syms);
        let header = |bytes: &mut Vec<u8>, index: usize, fields: [u64; 8]| {
            let at = shoff + 64 * index;
            let [name, kind, flags, addr, offset, size, link, entsize] = fields;
            put(bytes, at, &(name as u32).to_le_bytes());
            put(bytes, at + 4, &(kind as u32).to_le_bytes());
            put(bytes, at + 8, &flags.to_le_bytes());
            put(bytes, at + 16, &addr.to_le_bytes());
            put(bytes, at + 24, &offset.to_le_bytes());
            put(bytes, at + 32, &size.to_le_bytes());
            put(bytes, at + 40, &(link as u32).to_le_bytes());
            put(bytes, at + 48, &4u64.to_le_bytes());
            put(bytes, at + 56, &entsize.to_le_bytes());
        };
        header(
            &mut bytes,
            1,
            [1, 1, 6, BASE + 0x100, 0x100, (4 * first.len()) as u64, 0, 0],
        );
        header(
            &mut bytes,
            2,
            [
                12,
                1,
                6,
                BASE + 0x200,
                0x200,
                (4 * second.len()) as u64,
                0,
                0,
            ],
        );
        header(
            &mut bytes,
            3,
            [22, 3, 0, 0, names_at as u64, names.len() as u64, 0, 0],
        );
        header(
            &mut bytes,
            4,
            [32, 2, 0, 0, sym_at as u64, syms.len() as u64, 5, 24],
        );
        // Fix the symtab link/info fields the generic header writes at
        // fixed offsets: link is the strtab index, info the first
        // non-local symbol.
        let symtab_at = shoff + 64 * 4;
        bytes[symtab_at + 40..symtab_at + 44].copy_from_slice(&5u32.to_le_bytes());
        bytes[symtab_at + 44..symtab_at + 48].copy_from_slice(&3u32.to_le_bytes());
        header(
            &mut bytes,
            5,
            [40, 3, 0, 0, str_at as u64, strings.len() as u64, 0, 0],
        );
        bytes
    }

    /// nop; ldaxr w8, [x0]; add w8, w8, #1; stlxr w9, w8, [x0];
    /// cbnz w9, <ldaxr>; ret
    pub(crate) const LOOP: [u32; 6] = [
        0xd503_201f,
        0x885f_fc08,
        0x1100_0508,
        0x8809_fc08,
        0x35ff_ffa9,
        0xd65f_03c0,
    ];

    /// [`elf_image`] whose `.text` is all code. Shared with the in-memory
    /// rewrite's runtime tests, which map such an image.
    pub(crate) fn elf_with_text(words: &[u32]) -> Vec<u8> {
        elf_image(words, Some(&[(1, 0, true)]))
    }

    /// [`elf_image`] whose `.text` is marked `$x` at its start and holds an
    /// AArch32 `$a` mapping symbol at word `at`: a section this reader does
    /// not take as proved by its marks. Shared with the in-memory rewrite's
    /// runtime tests, which are aarch64-only, as it is.
    #[cfg(target_arch = "aarch64")]
    pub(crate) fn elf_with_unsupported_mark(words: &[u32], at: usize) -> Vec<u8> {
        let mut image = elf_image(words, Some(&[(1, 0, true), (1, at, false)]));
        let names = b"\0$x\0$d\0";
        let found = image
            .windows(names.len())
            .position(|window| window == names)
            .expect("the mapping symbols' names");
        image[found + 4..found + 6].copy_from_slice(b"$a");
        image
    }

    /// The plan names each rewritten sequence by file offset with its bytes
    /// before and after, those bytes are the rewriter's, and applying the
    /// plan is exactly what `rewrite_bytes` does to the file.
    #[test]
    fn the_plan_locates_each_sequence_and_matches_the_rewritten_file() {
        // nop; ldaxr w8, [x0]; add w8, w8, #1; stlxr w9, w8, [x0];
        // cbnz w9, <ldaxr>; ret
        let words = [
            0xd503_201f,
            0x885f_fc08,
            0x1100_0508,
            0x8809_fc08,
            0x35ff_ffa9,
            0xd65f_03c0,
        ];
        let image = elf_with_text(&words);
        let plan = plan_bytes(&image).unwrap().expect("an aarch64 ELF image");
        let expected = rewrite::plan(&words, 0x100);
        assert_eq!(expected.patched.len(), 1, "{expected:?}");
        assert_eq!(plan.report.patched_total(), 1, "{:?}", plan.report);
        assert_eq!(plan.patches.len(), 1);

        let patch = &plan.patches[0];
        assert_eq!(patch.offset, 0x104, "the loop starts after the nop");
        let words_of = |bytes: &[u8]| -> Vec<u32> {
            bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|word| u32::from_le_bytes(*word))
                .collect()
        };
        assert_eq!(words_of(&patch.original), words[1..5].to_vec());
        assert_eq!(words_of(&patch.rewritten), expected.patched[0].words);

        let mut rewritten = image.clone();
        let report = rewrite_bytes(&mut rewritten).unwrap().unwrap();
        assert_eq!(report, plan.report);
        let mut applied = image;
        applied[0x104..0x104 + patch.rewritten.len()].copy_from_slice(&patch.rewritten);
        assert_eq!(applied, rewritten);
    }

    /// The plan the in-memory rewrite writes into a guest's pages holds
    /// nothing for a literal pool: the same file whose bytes
    /// [`rewrite_bytes`] leaves alone yields no patch over them, so the
    /// memory path cannot change what the file path keeps.
    #[test]
    fn the_plan_holds_no_patch_over_a_literal_pool() {
        // The review's pool: a branch over four words that decode as a
        // canonical loop, then an `LDR` of them, as an assembler lays it
        // out, marked `$x`, `$d`, `$x`.
        let words = [
            0x1400_0005, // b <word 5>
            0xc85f_fc64, // the pool
            0x9100_0484,
            0xc805_fc64,
            0x35ff_ffa5,
            0x58ff_ff80, // ldr x0, <word 1>
            0xd65f_03c0, // ret
        ];
        let marked = elf_image(&words, Some(&[(1, 0, true), (1, 1, false), (1, 5, true)]));
        let plan = plan_bytes(&marked).unwrap().expect("an aarch64 ELF image");
        assert!(plan.patches.is_empty(), "{:?}", plan.report);
        assert_eq!(plan.report.exclusive_instructions, 0, "{:?}", plan.report);

        // Stripped, where nothing proves the code and the pool sits after a
        // call that does not return: likewise nothing to write.
        let mut after_a_call = [0u32; 9];
        after_a_call[0] = 0x9400_0008; // bl <word 8>, which traps
        after_a_call[1..5].copy_from_slice(&words[1..5]);
        after_a_call[5] = 0x58ff_ff80;
        after_a_call[6] = 0xd65f_03c0;
        after_a_call[7] = 0xd503_201f;
        after_a_call[8] = 0xd420_0000; // brk
        let stripped = elf_image_with(&after_a_call, None, Some(&[(0, 8), (8, 9)]));
        let plan = plan_bytes(&stripped)
            .unwrap()
            .expect("an aarch64 ELF image");
        assert!(plan.patches.is_empty(), "{:?}", plan.report);

        // The planner by itself takes the pool, which is what the marks
        // keep it from doing.
        let raw = rewrite::plan(&words, 0x100);
        assert_eq!(raw.patched.len(), 1, "{raw:?}");
    }

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

    /// The image's bytes after [`rewrite_bytes`], with its report.
    fn rewritten(image: &[u8]) -> (Vec<u8>, FileReport) {
        let mut bytes = image.to_vec();
        let report = rewrite_bytes(&mut bytes)
            .unwrap()
            .expect("an aarch64 ELF image");
        (bytes, report)
    }

    /// A stripped file is left unchanged and named: every sequence
    /// in a section no mapping symbol marks is left alone as
    /// `no-mapping-symbols`, listed by address, and the report is incomplete.
    #[test]
    fn a_stripped_file_is_left_unchanged_and_named_by_default() {
        let words: Vec<u32> = LOOP.iter().chain(LOOP.iter()).copied().collect();
        let image = elf_image_with(&words, None, Some(&[(0, 6)]));
        let (bytes, report) = rewritten(&image);
        assert_eq!(bytes, image, "nothing is rewritten");
        assert_eq!(report.patched_total(), 0, "{report:?}");
        assert_eq!(report.exclusive_instructions, 4, "{report:?}");
        assert_eq!(report.unmarked(), 2, "{report:?}");
        assert_eq!(
            report.refused,
            BTreeMap::from([("no-mapping-symbols", 2)]),
            "{report:?}"
        );
        assert_eq!(report.refused_sites.len(), 2, "{report:?}");
        assert!(
            report
                .refused_sites
                .iter()
                .all(|(_, why)| why.starts_with("no-mapping-symbols:")),
            "{report:?}"
        );
        assert!(!report.complete());
    }

    #[test]
    fn a_file_without_mapping_symbols_is_refused_even_where_unwind_covers_code() {
        // Unwind coverage alone proves nothing; the file is left unchanged.
        let words: Vec<u32> = LOOP.iter().chain(LOOP.iter()).copied().collect();
        let image = elf_image_with(&words, None, Some(&[(0, 6)]));
        let (bytes, report) = rewritten(&image);
        assert_eq!(bytes, image, "{report:?}");
        assert_eq!(report.patched_total(), 0, "{report:?}");
        assert_eq!(report.unmarked(), 2, "{report:?}");
    }

    /// A mapping symbol that is neither `$x` nor `$d` -- AArch32's `$a` or
    /// `$t`, an unknown `$q`, or `$xyz` and `$dx`, which only look like them
    /// -- after a valid `$x` leaves its section unproven:
    /// every loop in it is left alone as `unsupported-mapping-symbol`, naming
    /// the symbol. A suffixed `$x.f` still proves the code after it, and a
    /// `$d` makes it data.
    #[test]
    fn an_unsupported_mapping_symbol_leaves_its_section_unproven() {
        let words: Vec<u32> = LOOP.iter().chain(LOOP.iter()).copied().collect();
        let named = |name: &[u8]| {
            let mut image = elf_image(&words, Some(&[(1, 0, true), (1, 6, false)]));
            let strtab = b"\0$x\0$d\0abort\0";
            let at = image
                .windows(strtab.len())
                .position(|window| window == strtab)
                .unwrap();
            // The second mark's name is the one at offset 4.
            image[at + 4..at + 4 + name.len()].copy_from_slice(name);
            image
        };
        for name in [&b"$a\0"[..], b"$t\0", b"$q\0", b"$xyz\0", b"$dx\0"] {
            let shown = std::str::from_utf8(&name[..name.len() - 1]).unwrap();
            let image = named(name);
            let (bytes, report) = rewritten(&image);
            assert_eq!(bytes, image, "{shown}");
            assert_eq!(report.unsupported(), 2, "{shown}: {report:?}");
            let why = format!("unsupported-mapping-symbol: `{shown}` marks this section");
            assert!(
                report
                    .refused_sites
                    .iter()
                    .all(|(_, detail)| detail.starts_with(&why)),
                "{report:?}"
            );
        }
        let patched = |name: &[u8]| rewritten(&named(name)).1.patched_total();
        assert_eq!(patched(b"$d\0"), 1);
        assert_eq!(patched(b"$x.f\0"), 2);
    }

    /// A plan counts as unproven every loop no mapping symbol it accepts
    /// proves: those of a section nothing marks, and those of a section
    /// holding a mapping symbol other than `$x` and `$d`. The in-memory
    /// rewrite names a file by this count (`Rewrite::unmarked`), so a file
    /// refused for an unsupported mapping symbol is named as one refused for
    /// having none.
    #[test]
    fn a_plan_counts_every_loop_no_accepted_mapping_symbol_proves() {
        let words: Vec<u32> = LOOP.iter().chain(LOOP.iter()).copied().collect();
        let plan = |image: &[u8]| plan_bytes(image).unwrap().unwrap().report;
        let stripped = elf_image_with(&words, None, Some(&[(0, 6)]));
        assert_eq!(plan(&stripped).unproven(), 2);
        let mut unsupported = elf_image(&words, Some(&[(1, 0, true), (1, 6, false)]));
        let strtab = b"\0$x\0$d\0abort\0";
        let at = unsupported
            .windows(strtab.len())
            .position(|window| window == strtab)
            .unwrap();
        unsupported[at + 4..at + 6].copy_from_slice(b"$a");
        let report = plan(&unsupported);
        assert_eq!(report.unmarked(), 0, "{report:?}");
        assert_eq!(report.unproven(), 2, "{report:?}");
        let marked = elf_image(&words, Some(&[(1, 0, true)]));
        assert_eq!(plan(&marked).unproven(), 0);
    }

    /// A file mapping symbols mark is rewritten, whatever its unwind table
    /// covers: the marks are the evidence read first.
    #[test]
    fn a_marked_file_is_rewritten_by_default() {
        let words: Vec<u32> = LOOP.iter().chain(LOOP.iter()).copied().collect();
        for functions in [None, Some(&[(0, 6)][..])] {
            let marked = elf_image_with(&words, Some(&[(1, 0, true)]), functions);
            let (bytes, report) = rewritten(&marked);
            assert_eq!(report.patched_total(), 2, "{report:?}");
            assert_eq!(report.unmarked(), 0, "{report:?}");
            assert!(report.complete(), "{report:?}");
            let _ = (bytes, report);
        }
    }

    /// A file with neither mapping symbols nor an unwind table is named
    /// sequence by sequence, and so is a section in a file whose only mapping
    /// symbol marks another section.
    #[test]
    fn the_default_names_a_section_nothing_marks_whatever_else_the_file_holds() {
        for marks in [None, Some(&[(2, 0, true)][..])] {
            let image = elf_image(&LOOP, marks);
            let (bytes, report) = rewritten(&image);
            assert_eq!(bytes, image);
            assert_eq!(report.unmarked(), 1, "{report:?}");
            assert_eq!(report.exclusive_instructions, 2, "{report:?}");
            assert!(report.skipped_segments.is_empty(), "{report:?}");
        }
    }

    /// A literal pool in `.text` whose words form a canonical retry loop,
    /// laid out as an assembler and linker emit it (`b` over four `.word`s,
    /// then an `LDR` (literal) that loads them): `$x` at the branch, `$d`
    /// at the pool, `$x` after it. The pool keeps its bytes whatever tells
    /// code from data: its `$d` mark; the unwind table alone, whose FDE
    /// covers the whole function but whose entry never reaches the pool; or a
    /// lone `$x` over everything, where the literal load refuses it. The
    /// planner by itself would take it.
    #[test]
    fn a_literal_pool_shaped_like_a_loop_keeps_its_bytes() {
        let words = [
            0x1400_0005, // b <word 5>
            0xc85f_fc64, // the pool: ldaxr x4, [x3] as a word
            0x9100_0484, // add x4, x4, #1
            0xc805_fc64, // stlxr w5, x4, [x3]
            0x35ff_ffa5, // cbnz w5, <word 1>
            0x58ff_ff80, // ldr x0, <word 1>
            0xd65f_03c0, // ret
        ];
        // What the planner alone does with these words: it rewrites the
        // pool, which is what corrupted it.
        let raw = rewrite::plan(&words, 0x100);
        assert_eq!(raw.patched.len(), 1, "{raw:?}");
        assert_eq!(raw.patched[0].addr, 0x104);

        let marked = elf_image(&words, Some(&[(1, 0, true), (1, 1, false), (1, 5, true)]));
        let (bytes, report) = rewritten(&marked);
        assert_eq!(bytes, marked, "{report:?}");
        assert_eq!(report.exclusive_instructions, 0, "{report:?}");
        assert!(report.complete(), "data is not a refusal: {report:?}");

        let stripped = elf_image_with(&words, None, Some(&[(0, words.len())]));
        let (bytes, report) = rewritten(&stripped);
        assert_eq!(bytes, stripped, "{report:?}");
        assert_eq!(report.patched_total(), 0, "{report:?}");
        assert_eq!(
            report.refused,
            BTreeMap::from([("no-mapping-symbols", 1)]),
            "{report:?}"
        );

        let unmarked = elf_image(&words, Some(&[(1, 0, true)]));
        let (bytes, report) = rewritten(&unmarked);
        assert_eq!(bytes, unmarked, "{report:?}");
        assert_eq!(
            report.refused,
            BTreeMap::from([("literal-inside-the-sequence", 1)]),
            "{report:?}"
        );
    }

    /// Every refused sequence is listed, not only the first sixteen: 17
    /// lone store-exclusives give 17 sites, in address order, which is what
    /// `--list-refusals` prints.
    #[test]
    fn every_refused_loop_is_listed() {
        // stlxr w9, w8, [x0] seventeen times, then ret.
        let mut words = vec![0x8809_fc08_u32; 17];
        words.push(0xd65f_03c0);
        let image = elf_image(&words, Some(&[(1, 0, true)]));
        let (bytes, report) = rewritten(&image);
        assert_eq!(bytes, image);
        assert_eq!(report.refused_total(), 17, "{report:?}");
        assert_eq!(
            report
                .refused_sites
                .iter()
                .map(|(addr, _)| *addr)
                .collect::<Vec<_>>(),
            (0..17).map(|i| 0x100 + 4 * i).collect::<Vec<u64>>(),
            "{report:?}"
        );
    }

    /// A constant table in `.text` whose words form a canonical loop is
    /// left as it is when the mapping symbols mark it data, and only the
    /// code's exclusive instructions are counted. Without the `$d` mark the
    /// same table would be rewritten, which is what the marks prevent.
    #[test]
    fn a_data_table_shaped_like_a_loop_is_not_rewritten() {
        // The loop as code at 0x100, then the same six words as a table at
        // 0x118 (its cbnz targets the table's own ldaxr).
        let words: Vec<u32> = LOOP.iter().chain(LOOP.iter()).copied().collect();
        let marked = elf_image(&words, Some(&[(1, 0, true), (1, 6, false)]));
        let (bytes, report) = rewritten(&marked);
        assert_eq!(report.patched_total(), 1, "{report:?}");
        assert_eq!(report.refused_total(), 0, "{report:?}");
        assert_eq!(report.exclusive_instructions, 2, "{report:?}");
        assert!(report.skipped_segments.is_empty(), "{report:?}");
        assert_eq!(
            bytes[0x118..0x130],
            marked[0x118..0x130],
            "the table is untouched"
        );
        assert_ne!(
            bytes[0x104..0x114],
            marked[0x104..0x114],
            "the code is rewritten"
        );

        let unmarked = elf_image(&words, Some(&[(1, 0, true)]));
        let (_, report) = rewritten(&unmarked);
        assert_eq!(report.patched_total(), 2, "{report:?}");
        assert_eq!(report.exclusive_instructions, 4, "{report:?}");
    }

    /// A loop whose store-exclusive is code but whose branch back is in a
    /// `$d` range is refused, not rewritten; one whose store-exclusive is in
    /// data is not reported at all.
    #[test]
    fn a_loop_that_runs_into_data_is_refused() {
        // Code ends at the cbnz (word 4): the sequence runs into data.
        let image = elf_image(&LOOP, Some(&[(1, 0, true), (1, 4, false)]));
        let (bytes, report) = rewritten(&image);
        assert_eq!(bytes, image);
        assert_eq!(report.patched_total(), 0, "{report:?}");
        assert_eq!(
            report.refused,
            BTreeMap::from([("not-a-canonical-loop", 1)]),
            "{report:?}"
        );
        assert_eq!(
            report.refused_sites,
            vec![(0x10c, "not-a-canonical-loop".into())]
        );
        // Data from the stlxr (word 3) on: the store is not an instruction.
        let image = elf_image(&LOOP, Some(&[(1, 0, true), (1, 3, false)]));
        let (bytes, report) = rewritten(&image);
        assert_eq!(bytes, image);
        assert_eq!(report.patched_total(), 0, "{report:?}");
        assert_eq!(report.refused_total(), 0, "{report:?}");
        assert_eq!(report.exclusive_instructions, 1, "the ldaxr is code");
    }

    /// Bytes not known to be code are never rewritten: a file with neither
    /// mapping symbols nor an unwind table is left alone whole, and so is an
    /// executable section the mapping symbols do not mark in a file without
    /// an unwind table.
    #[test]
    fn code_nothing_marks_is_left_alone() {
        let image = elf_image(&LOOP, None);
        let (bytes, stripped) = rewritten(&image);
        assert_eq!(bytes, image);
        assert_eq!(stripped.patched_total(), 0, "{stripped:?}");
        assert_eq!(stripped.exclusive_instructions, 2, "{stripped:?}");
        assert_eq!(
            stripped.refused,
            BTreeMap::from([("no-mapping-symbols", 1)]),
            "{stripped:?}"
        );
        assert!(!stripped.complete());
        // The only mapping symbol marks another section (.shstrtab, 2).
        let image = elf_image(&LOOP, Some(&[(2, 0, true)]));
        let (bytes, unmarked) = rewritten(&image);
        assert_eq!(bytes, image);
        assert_eq!(unmarked.patched_total(), 0, "{unmarked:?}");
        assert_eq!(
            unmarked.refused,
            BTreeMap::from([("no-mapping-symbols", 1)]),
            "{unmarked:?}"
        );
    }

    /// Without mapping symbols nothing is rewritten: unwind-table coverage
    /// alone proves nothing, so the whole file is left unchanged as
    /// `no-mapping-symbols`. Mapping symbols still prove code.
    #[test]
    fn a_stripped_file_is_not_rewritten_where_its_unwind_table_covers_code() {
        let words: Vec<u32> = LOOP.iter().chain(LOOP.iter()).copied().collect();
        let image = elf_image_with(&words, None, Some(&[(0, 6)]));
        let (bytes, report) = rewritten(&image);
        assert_eq!(bytes, image, "nothing is rewritten: {report:?}");
        assert_eq!(report.patched_total(), 0, "{report:?}");
        assert_eq!(report.exclusive_instructions, 4, "{report:?}");
        assert_eq!(
            report.refused,
            BTreeMap::from([("no-mapping-symbols", 2)]),
            "{report:?}"
        );
        // Mapping symbols, where a section has them, take precedence.
        let marked = elf_image_with(&words, Some(&[(1, 0, true)]), Some(&[(0, 6)]));
        assert_eq!(rewritten(&marked).1.patched_total(), 2);
    }

    /// An `LDR` (literal) that reaches a sequence's bytes makes them a
    /// possible literal pool, and it is refused, whatever the evidence: an
    /// object linked without mapping symbols after one with them inherits
    /// the earlier `$x` range.
    #[test]
    fn a_literal_load_into_a_sequence_refuses_it() {
        // The code loop (words 0..6), `ldr x1, <word 9>`, `ret`, then a
        // loop-shaped pool at words 8..14, all under one `$x`.
        let mut words: Vec<u32> = LOOP.to_vec();
        words.extend([0x5800_0061, 0xd65f_03c0]);
        words.extend(LOOP);
        let image = elf_image(&words, Some(&[(1, 0, true)]));
        let (bytes, report) = rewritten(&image);
        assert_eq!(report.patched_total(), 1, "{report:?}");
        assert_ne!(
            bytes[0x104..0x114],
            image[0x104..0x114],
            "the code is rewritten"
        );
        assert_eq!(bytes[0x120..], image[0x120..], "the pool is untouched");
        assert_eq!(
            report.refused,
            BTreeMap::from([("literal-inside-the-sequence", 1)]),
            "{report:?}"
        );
        assert_eq!(
            report.refused_sites,
            vec![(0x124, "literal-inside-the-sequence".into())]
        );
    }

    /// [`elf_image_two_sections`] with the second section's mapping symbols
    /// replaced by `marks`, (name, offset into the section): names are
    /// `$x`=1, `$d`=4 and `$a`=7. The first section stays marked as code.
    fn two_sections_with_marks(first: &[u32], second: &[u32], marks: &[(u32, u64)]) -> Vec<u8> {
        // Reuse the two-section fixture, replacing only its non-allocated
        // symbol/string tables.
        let mut image = elf_image_two_sections(first, second);
        let shoff = u64::from_le_bytes(image[40..48].try_into().unwrap()) as usize;
        let symtab_header = shoff + 64 * 4;
        let strtab_header = shoff + 64 * 5;
        let symtab_at = image.len().next_multiple_of(8);
        image.resize(symtab_at, 0);
        image.extend_from_slice(&[0u8; 24]);
        image.extend_from_slice(&mapping::tests::symbol(1, 0, 1, 0x400100));
        for &(name, offset) in marks {
            image.extend_from_slice(&mapping::tests::symbol(name, 0, 2, 0x400200 + offset));
        }
        let symtab_len = image.len() - symtab_at;
        let strtab_at = image.len();
        let strings = b"\0$x\0$d\0$a\0";
        image.extend_from_slice(strings);
        image[symtab_header + 24..symtab_header + 32]
            .copy_from_slice(&(symtab_at as u64).to_le_bytes());
        image[symtab_header + 32..symtab_header + 40]
            .copy_from_slice(&(symtab_len as u64).to_le_bytes());
        image[symtab_header + 44..symtab_header + 48]
            .copy_from_slice(&((2 + marks.len()) as u32).to_le_bytes());
        image[strtab_header + 24..strtab_header + 32]
            .copy_from_slice(&(strtab_at as u64).to_le_bytes());
        image[strtab_header + 32..strtab_header + 40]
            .copy_from_slice(&(strings.len() as u64).to_le_bytes());
        image
    }

    #[test]
    fn literal_readers_in_unknown_executable_ranges_protect_other_sections() {
        let loop_words = [
            0x885f_fc08, // 0x400100: ldaxr w8, [x0]
            0x1100_0508, // add w8, w8, #1
            0x8809_fc08, // stlxr w9, w8, [x0]
            0x35ff_ffa9, // cbnz w9, 0x400100
            0xd65f_03c0, // ret
        ];
        const LDR: u32 = 0x18ff_f801; // 0x400200: ldr w1, 0x400100
        const NOP: u32 = 0xd503_201f;

        let image_with_reader_marks = |reader: u32, marks: &[(u32, u64)]| {
            two_sections_with_marks(&loop_words, &[reader, 0xd65f_03c0], marks)
        };

        let layouts: &[(&str, &[(u32, u64)])] = &[
            ("marked code", &[(1, 0)]),
            ("unmarked section", &[]),
            ("unknown prefix before code", &[(1, 4)]),
            ("unsupported section", &[(7, 0)]),
            ("unsupported after code", &[(1, 0), (7, 4)]),
            ("unsupported after data", &[(4, 0), (7, 4)]),
            // A boundary through a word cannot hide the instruction that
            // starts in the unknown prefix; decode it at its actual PC.
            ("partial unknown word before data", &[(4, 2)]),
        ];
        for &(layout, marks) in layouts {
            // Refusing every image with an unknown section is not the fix:
            // without a reader, the marked canonical loop still qualifies.
            let control = image_with_reader_marks(NOP, marks);
            let control_plan = plan_bytes(&control).unwrap().unwrap();
            assert_eq!(control_plan.patches.len(), 1, "{layout}: {control_plan:?}");
            assert_eq!(control_plan.patches[0].offset, 0x100, "{layout}");
            let (changed, report) = rewritten(&control);
            assert_eq!(report.patched_total(), 1, "{layout}: {report:?}");
            assert_ne!(changed, control, "{layout}");

            let image = image_with_reader_marks(LDR, marks);
            let plan = plan_bytes(&image).unwrap().unwrap();
            assert!(plan.patches.is_empty(), "{layout}: {plan:?}");
            let (unchanged, report) = rewritten(&image);
            assert_eq!(unchanged, image, "{layout}: {report:?}");
            assert_eq!(report.patched_total(), 0, "{layout}: {report:?}");
            assert_eq!(
                report.refused,
                BTreeMap::from([("literal-inside-the-sequence", 1)]),
                "{layout}: {report:?}"
            );
            assert_eq!(
                report.refused_sites,
                vec![(0x400100, "literal-inside-the-sequence".into())],
                "{layout}: {report:?}"
            );
        }

        // A wholly proven $d word that looks like an LDR is data, not a
        // reader. Scanning every word in every section would break this.
        let data = image_with_reader_marks(LDR, &[(4, 0)]);
        let plan = plan_bytes(&data).unwrap().unwrap();
        assert_eq!(plan.patches.len(), 1, "{plan:?}");
        let (changed, report) = rewritten(&data);
        assert_eq!(report.patched_total(), 1, "{report:?}");
        assert!(report.refused.is_empty(), "{report:?}");
        assert_eq!(changed[0x200..0x208], data[0x200..0x208]);
        assert_ne!(changed, data);

        // The actual LDR ends immediately before the candidate. Starting a
        // decoder at the unaligned $x address would move its PC by two bytes
        // and invent an overlapping read. Range rounding must not do that.
        let nearby = image_with_reader_marks(0x18ff_f7e1, &[(1, 2)]);
        let plan = plan_bytes(&nearby).unwrap().unwrap();
        assert_eq!(plan.patches.len(), 1, "{plan:?}");
        let (changed, report) = rewritten(&nearby);
        assert_eq!(report.patched_total(), 1, "{report:?}");
        assert!(report.refused.is_empty(), "{report:?}");
        assert_ne!(changed, nearby);
    }

    /// A literal reader in bytes an executable segment carries but no
    /// executable section covers (here the second section keeps its
    /// segment place with only `SHF_ALLOC`) still protects the loop it
    /// reads; with no reader there, or a reader in a covered section,
    /// the loop is rewritten or refused as the covered cases are.
    #[test]
    fn literal_readers_in_segment_gaps_protect_other_sections() {
        let loop_words = [
            0x885f_fc08, // 0x400100: ldaxr w8, [x0]
            0x1100_0508, // add w8, w8, #1
            0x8809_fc08, // stlxr w9, w8, [x0]
            0x35ff_ffa9, // cbnz w9, 0x400100
            0xd65f_03c0, // ret
        ];
        const LDR: u32 = 0x18ff_f801; // 0x400200: ldr w1, 0x400100
        const NOP: u32 = 0xd503_201f;

        // The two-section image with the second section's flags
        // reduced to ALLOC, so its bytes are in the RX segment but
        // in no executable section.
        let gap_image = |reader: u32| {
            let mut image = two_sections_with_marks(&loop_words, &[reader, 0xd65f_03c0], &[(1, 0)]);
            let shoff = u64::from_le_bytes(image[40..48].try_into().unwrap()) as usize;
            let second = shoff + 64 * 2;
            image[second + 8..second + 16].copy_from_slice(&2u64.to_le_bytes());
            image
        };

        // Control: the reader in a covered executable section refuses.
        let covered = two_sections_with_marks(&loop_words, &[LDR, 0xd65f_03c0], &[(1, 0)]);
        let plan = plan_bytes(&covered).unwrap().unwrap();
        assert!(plan.patches.is_empty(), "{plan:?}");

        // Gap reader: an LDR literal in uncovered segment bytes
        // refuses the loop it reads, and nothing is rewritten.
        let image = gap_image(LDR);
        let plan = plan_bytes(&image).unwrap().unwrap();
        assert!(plan.patches.is_empty(), "{plan:?}");
        let (unchanged, report) = rewritten(&image);
        assert_eq!(unchanged, image, "{report:?}");
        assert_eq!(
            report.refused,
            BTreeMap::from([("gap-reader-inside-the-sequence", 1)]),
            "{report:?}"
        );

        // Reader-free gap: NOPs in uncovered segment bytes leave
        // the loop rewritten.
        let image = gap_image(NOP);
        let plan = plan_bytes(&image).unwrap().unwrap();
        assert_eq!(plan.patches.len(), 1, "{plan:?}");
        let (changed, report) = rewritten(&image);
        assert_eq!(report.patched_total(), 1, "{report:?}");
        assert_ne!(changed, image);
    }

    /// `LDR w1, <loop>` at `pc`, reading the loop at 0x400100.
    fn gap_ldr(pc: u64) -> u32 {
        let imm19 = ((0x400100i64 - pc as i64) / 4) as u32 & 0x7_ffff;
        0x1800_0001 | (imm19 << 5)
    }

    /// Patch the only program header's fields of a two-section image:
    /// file size and mem size, virtual address and file offset at
    /// their ELF offsets (the alignment is left as it was), and
    /// return the image.
    fn with_program_header(mut image: Vec<u8>, filesz: u64, vaddr: u64, offset: u64) -> Vec<u8> {
        image[72..80].copy_from_slice(&offset.to_le_bytes());
        image[80..88].copy_from_slice(&vaddr.to_le_bytes());
        image[96..104].copy_from_slice(&filesz.to_le_bytes());
        image[104..112].copy_from_slice(&filesz.to_le_bytes());
        image
    }

    /// A later executable section past the segment end must not
    /// erase this segment's tail gap, and a segment before every
    /// section is itself a gap.
    #[test]
    fn gap_walk_keeps_each_segments_tail() {
        let loop_words = [
            0x885f_fc08,
            0x1100_0508,
            0x8809_fc08,
            0x35ff_ffa9,
            0xd65f_03c0,
        ];
        const NOP: u32 = 0xd503_201f;
        let image = |reader_pc: u64, reader: u32, filesz: u64| {
            let mut image = two_sections_with_marks(&loop_words, &[NOP, 0xd65f_03c0], &[(1, 0)]);
            // Move the second section's address past the segment end;
            // its bytes stay where they were, so the walk must not
            // let it consume the first segment's tail.
            let shoff = u64::from_le_bytes(image[40..48].try_into().unwrap()) as usize;
            let second = shoff + 64 * 2;
            image[second + 16..second + 24].copy_from_slice(&0x401000u64.to_le_bytes());
            let at = (reader_pc - 0x400000) as usize;
            image[at..at + 4].copy_from_slice(&reader.to_le_bytes());
            with_program_header(image, filesz, 0x400000, 0)
        };
        // Reader in the tail gap (0x400180) of a segment ending at
        // 0x400300: refused, nothing rewritten.
        let plan = plan_bytes(&image(0x400180, gap_ldr(0x400180), 0x300))
            .unwrap()
            .unwrap();
        assert!(plan.patches.is_empty(), "{plan:?}");
        assert_eq!(
            plan.report.refused.get("gap-reader-inside-the-sequence"),
            Some(&1),
            "{plan:?}"
        );
        // Reader-free tail: still rewritten.
        let plan = plan_bytes(&image(0x400180, NOP, 0x300)).unwrap().unwrap();
        assert_eq!(plan.patches.len(), 1, "{plan:?}");
        // A segment entirely before the first section (it ends at
        // 0x4000c4, before the loop at 0x400100) with a reader at
        // 0x4000c0: refused as well.
        let plan = plan_bytes(&image(0x4000c0, gap_ldr(0x4000c0), 0xc4))
            .unwrap()
            .unwrap();
        assert!(plan.patches.is_empty(), "{plan:?}");
    }

    /// A legal non-word-aligned, page-congruent segment start
    /// decodes the gap bytes at their real PCs and offsets.
    #[test]
    fn gap_walk_maps_an_unaligned_segment_start() {
        let loop_words = [
            0x885f_fc08,
            0x1100_0508,
            0x8809_fc08,
            0x35ff_ffa9,
            0xd65f_03c0,
        ];
        let mut image =
            two_sections_with_marks(&loop_words, &[0xd503_201f, 0xd65f_03c0], &[(1, 0)]);
        let at = 0x80usize;
        image[at..at + 4].copy_from_slice(&gap_ldr(0x400080).to_le_bytes());
        // vaddr 0x400001 at file offset 1: congruent mod 0x1000.
        let filesz = (image.len() - 1) as u64;
        let image = with_program_header(image, filesz, 0x400001, 1);
        let plan = plan_bytes(&image).unwrap().unwrap();
        assert!(plan.patches.is_empty(), "{plan:?}");
        assert_eq!(
            plan.report.refused.get("gap-reader-inside-the-sequence"),
            Some(&1),
            "{plan:?}"
        );
    }

    /// A reader past nominal `p_filesz` but inside the segment's
    /// last mapped page is still a reader.
    #[test]
    fn gap_walk_covers_the_mapped_page_tail() {
        let loop_words = [
            0x885f_fc08,
            0x1100_0508,
            0x8809_fc08,
            0x35ff_ffa9,
            0xd65f_03c0,
        ];
        let mut image =
            two_sections_with_marks(&loop_words, &[0xd503_201f, 0xd65f_03c0], &[(1, 0)]);
        let at = 0x180usize;
        image[at..at + 4].copy_from_slice(&gap_ldr(0x400180).to_le_bytes());
        // The segment's file size ends just after the loop.
        let image = with_program_header(image, 0x114, 0x400000, 0);
        let plan = plan_bytes(&image).unwrap().unwrap();
        assert!(plan.patches.is_empty(), "{plan:?}");
        assert_eq!(
            plan.report.refused.get("gap-reader-inside-the-sequence"),
            Some(&1),
            "{plan:?}"
        );
    }

    /// A section whose file offset disagrees with the load does
    /// not cover the load's actual reader bytes.
    #[test]
    fn gap_walk_distrusts_a_disagreeing_section_offset() {
        let loop_words = [
            0x885f_fc08,
            0x1100_0508,
            0x8809_fc08,
            0x35ff_ffa9,
            0xd65f_03c0,
        ];
        let second = [gap_ldr(0x400200), 0xd65f_03c0];
        let mut image = two_sections_with_marks(&loop_words, &second, &[(1, 0)]);
        // The second section now claims its bytes live at file
        // offset 0x800 (zeros) instead of 0x200 (the reader).
        let shoff = u64::from_le_bytes(image[40..48].try_into().unwrap()) as usize;
        let header = shoff + 64 * 2;
        let total = 0x808usize;
        image.resize(total.max(image.len()), 0);
        for (i, word) in [0xd503_201fu32, 0xd65f_03c0].iter().enumerate() {
            image[0x800 + 4 * i..0x804 + 4 * i].copy_from_slice(&word.to_le_bytes());
        }
        image[header + 24..header + 32].copy_from_slice(&0x800u64.to_le_bytes());
        let plan = plan_bytes(&image).unwrap().unwrap();
        assert!(plan.patches.is_empty(), "{plan:?}");
        assert_eq!(
            plan.report.refused.get("gap-reader-inside-the-sequence"),
            Some(&1),
            "{plan:?}"
        );
    }

    /// Two executable segments: a reader in the first segment's
    /// trailing gap is collected even though a second segment and
    /// its section follow it.
    #[test]
    fn gap_readers_survive_a_second_executable_segment() {
        let loop_words = [
            0x885f_fc08,
            0x1100_0508,
            0x8809_fc08,
            0x35ff_ffa9,
            0xd65f_03c0,
        ];
        let mut image =
            two_sections_with_marks(&loop_words, &[0xd503_201f, 0xd65f_03c0], &[(1, 0)]);
        // Reader at 0x400180, in the first segment's tail gap.
        image[0x180..0x184].copy_from_slice(&gap_ldr(0x400180).to_le_bytes());
        // Move the second section to 0x500100, backed at file
        // offset 0x2100, and give the image a second, valid
        // PT_LOAD for it at offset 0x2000 (page-congruent, and its
        // filesz 0x108 covers the relocated section).
        let second_bytes: Vec<u8> = image[0x200..0x208].to_vec();
        image.resize(0x2200.max(image.len()), 0);
        image[0x2100..0x2108].copy_from_slice(&second_bytes);
        let shoff = u64::from_le_bytes(image[40..48].try_into().unwrap()) as usize;
        let second = shoff + 64 * 2;
        image[second + 16..second + 24].copy_from_slice(&0x500100u64.to_le_bytes());
        image[second + 24..second + 32].copy_from_slice(&0x2100u64.to_le_bytes());
        image[56..58].copy_from_slice(&2u16.to_le_bytes());
        image[120..124].copy_from_slice(&1u32.to_le_bytes());
        image[124..128].copy_from_slice(&5u32.to_le_bytes());
        image[128..136].copy_from_slice(&0x2000u64.to_le_bytes());
        image[136..144].copy_from_slice(&0x500000u64.to_le_bytes());
        image[144..152].copy_from_slice(&0x500000u64.to_le_bytes());
        image[152..160].copy_from_slice(&0x108u64.to_le_bytes());
        image[160..168].copy_from_slice(&0x108u64.to_le_bytes());
        image[168..176].copy_from_slice(&0x1000u64.to_le_bytes());
        let image = with_program_header(image, 0x300, 0x400000, 0);
        let plan = plan_bytes(&image).unwrap().unwrap();
        assert!(plan.patches.is_empty(), "{plan:?}");
        assert_eq!(
            plan.report.refused.get("gap-reader-inside-the-sequence"),
            Some(&1),
            "{plan:?}"
        );
        // Matched control: same geometry, only the gap reader at
        // 0x180 is left zero, so the loop is rewritten.
        let mut control =
            two_sections_with_marks(&loop_words, &[0xd503_201f, 0xd65f_03c0], &[(1, 0)]);
        control.resize(0x2200.max(control.len()), 0);
        let bytes2: Vec<u8> = control[0x200..0x208].to_vec();
        control[0x2100..0x2108].copy_from_slice(&bytes2);
        let shoff2 = u64::from_le_bytes(control[40..48].try_into().unwrap()) as usize;
        let second2 = shoff2 + 64 * 2;
        control[second2 + 16..second2 + 24].copy_from_slice(&0x500100u64.to_le_bytes());
        control[second2 + 24..second2 + 32].copy_from_slice(&0x2100u64.to_le_bytes());
        control[56..58].copy_from_slice(&2u16.to_le_bytes());
        control[120..124].copy_from_slice(&1u32.to_le_bytes());
        control[124..128].copy_from_slice(&5u32.to_le_bytes());
        control[128..136].copy_from_slice(&0x2000u64.to_le_bytes());
        control[136..144].copy_from_slice(&0x500000u64.to_le_bytes());
        control[144..152].copy_from_slice(&0x500000u64.to_le_bytes());
        control[152..160].copy_from_slice(&0x108u64.to_le_bytes());
        control[160..168].copy_from_slice(&0x108u64.to_le_bytes());
        control[168..176].copy_from_slice(&0x1000u64.to_le_bytes());
        let control = with_program_header(control, 0x300, 0x400000, 0);
        let plan = plan_bytes(&control).unwrap().unwrap();
        assert_eq!(plan.patches.len(), 1, "{plan:?}");
    }

    /// A direct branch out of gap bytes into a sequence refuses it,
    /// like a branch out of a covered section.
    #[test]
    fn gap_branches_into_a_sequence_refuse_it() {
        let loop_words = [
            0x885f_fc08,
            0x1100_0508,
            0x8809_fc08,
            0x35ff_ffa9,
            0xd65f_03c0,
        ];
        let mut image =
            two_sections_with_marks(&loop_words, &[0xd503_201f, 0xd65f_03c0], &[(1, 0)]);
        // `b 0x400104` (into the loop's second word) at 0x400180.
        let imm26 = ((0x400104i64 - 0x400180) / 4) as u32 & 0x3ff_ffff;
        let branch = 0x1400_0000 | imm26;
        image[0x180..0x184].copy_from_slice(&branch.to_le_bytes());
        let shoff = u64::from_le_bytes(image[40..48].try_into().unwrap()) as usize;
        let second = shoff + 64 * 2;
        image[second + 8..second + 16].copy_from_slice(&2u64.to_le_bytes());
        let plan = plan_bytes(&image).unwrap().unwrap();
        assert!(plan.patches.is_empty(), "{plan:?}");
        assert_eq!(
            plan.report.refused.get("branch-into-sequence"),
            Some(&1),
            "{plan:?}"
        );
    }

    /// A segment whose gap bytes run past the end of the file is
    /// refused by name, with no patches.
    #[test]
    fn a_truncated_gap_is_refused_by_name() {
        let loop_words = [
            0x885f_fc08,
            0x1100_0508,
            0x8809_fc08,
            0x35ff_ffa9,
            0xd65f_03c0,
        ];
        let image = two_sections_with_marks(&loop_words, &[0xd503_201f, 0xd65f_03c0], &[(1, 0)]);
        // The segment claims far more file than the image holds,
        // while every section stays inside it.
        let image = with_program_header(image, 0x1_0000, 0x400000, 0);
        let plan = plan_bytes(&image).unwrap().unwrap();
        assert!(plan.patches.is_empty(), "{plan:?}");
        assert_eq!(
            plan.report.skipped_segments,
            vec![
                "executable segment bytes not covered by an executable section are truncated, \
                 so their readers cannot be checked"
                    .to_string()
            ],
            "{plan:?}"
        );
    }

    /// Ordinary read-only data in a single R+X segment (the GNU ld
    /// default layout) can decode as an ADR a branch then reads
    /// through, which refuses every sequence under the gap-reader
    /// name, not the proven-code literal name. This pins that
    /// cost deliberately: the refusal is safe, and the name says
    /// only gap words caused it.
    #[test]
    fn rodata_words_in_a_gap_refuse_under_the_gap_reader_name() {
        let loop_words = [
            0x885f_fc08,
            0x1100_0508,
            0x8809_fc08,
            0x35ff_ffa9,
            0xd65f_03c0,
        ];
        let mut image =
            two_sections_with_marks(&loop_words, &[0xd503_201f, 0xd65f_03c0], &[(1, 0)]);
        // ASCII "P_SE" / "ER_T" bytes at 0x400180, in the gap.
        image[0x180..0x184].copy_from_slice(&0x505f_534eu32.to_le_bytes());
        image[0x184..0x188].copy_from_slice(&0x545f_5245u32.to_le_bytes());
        let shoff = u64::from_le_bytes(image[40..48].try_into().unwrap()) as usize;
        let second = shoff + 64 * 2;
        image[second + 8..second + 16].copy_from_slice(&2u64.to_le_bytes());
        let plan = plan_bytes(&image).unwrap().unwrap();
        assert!(plan.patches.is_empty(), "{plan:?}");
        assert_eq!(
            plan.report.refused.get("gap-reader-inside-the-sequence"),
            Some(&1),
            "{plan:?}"
        );
    }

    /// Write a program header at `index` (56 bytes at 64 + 56*index).
    fn put_ph(
        image: &mut [u8],
        index: usize,
        kind: u32,
        flags: u32,
        offset: u64,
        vaddr: u64,
        filesz: u64,
    ) {
        let at = 64 + 56 * index;
        image[at..at + 4].copy_from_slice(&kind.to_le_bytes());
        image[at + 4..at + 8].copy_from_slice(&flags.to_le_bytes());
        image[at + 8..at + 16].copy_from_slice(&offset.to_le_bytes());
        image[at + 16..at + 24].copy_from_slice(&vaddr.to_le_bytes());
        image[at + 24..at + 32].copy_from_slice(&vaddr.to_le_bytes());
        image[at + 32..at + 40].copy_from_slice(&filesz.to_le_bytes());
        image[at + 40..at + 48].copy_from_slice(&filesz.to_le_bytes());
        image[at + 48..at + 56].copy_from_slice(&0x1000u64.to_le_bytes());
    }

    /// The image of the domain tests: the two-section loop image
    /// grown to `total` bytes, with a reader word at file offset
    /// `reader_at` (VA `reader_pc`) reading the loop. Callers add
    /// the extra program headers (and any dynamic entries) their
    /// case needs.
    fn domain_image(total: usize, reader_at: usize, reader_pc: u64) -> Vec<u8> {
        let loop_words = [
            0x885f_fc08,
            0x1100_0508,
            0x8809_fc08,
            0x35ff_ffa9,
            0xd65f_03c0,
        ];
        let mut image =
            two_sections_with_marks(&loop_words, &[0xd503_201f, 0xd65f_03c0], &[(1, 0)]);
        image.resize(total.max(image.len()), 0);
        image[reader_at..reader_at + 4]
            .copy_from_slice(&gap_ldr_at(reader_pc, 0x400100).to_le_bytes());
        image
    }

    /// `LDR w1, target` at `pc`.
    fn gap_ldr_at(pc: u64, target: u64) -> u32 {
        let displacement = target as i64 - pc as i64;
        assert!(
            (-(1 << 20)..(1 << 20)).contains(&displacement),
            "LDR literal displacement out of range"
        );
        let imm19 = (displacement / 4) as u32 & 0x7_ffff;
        0x1800_0001 | (imm19 << 5)
    }

    #[test]
    fn readers_past_the_4k_page_in_a_load_are_collected() {
        // Filesz ends at 0x114; the reader is in the 64 KiB
        // mapped tail at 0x402000.
        let image = domain_image(0x10000, 0x2000, 0x402000);
        let image = with_program_header(image, 0x114, 0x400000, 0);
        let plan = plan_bytes(&image).unwrap().unwrap();
        assert!(plan.patches.is_empty(), "{plan:?}");
        assert_eq!(
            plan.report.refused.get("gap-reader-inside-the-sequence"),
            Some(&1),
            "{plan:?}"
        );
    }

    #[test]
    fn holes_between_loads_of_a_dynamic_image_are_collected() {
        // An RW load far away; a reader at 0x420000 in the hole.
        let mut image = domain_image(0x30100, 0x20000, 0x420000);
        put_ph(&mut image, 1, 1, 6, 0x30000, 0x430000, 0x100);
        // A PT_DYNAMIC so the image is dynamic even read strictly.
        put_ph(&mut image, 2, 2, 4, 0x30040, 0x430040, 16);
        image[56..58].copy_from_slice(&3u16.to_le_bytes());
        let image = with_program_header(image, 0x114, 0x400000, 0);
        let plan = plan_bytes(&image).unwrap().unwrap();
        assert!(plan.patches.is_empty(), "{plan:?}");
        assert_eq!(
            plan.report.refused.get("gap-reader-inside-the-sequence"),
            Some(&1),
            "{plan:?}"
        );
    }

    #[test]
    fn readers_in_loads_and_holes_are_collected() {
        // TEXTREL is set in this fixture, but the lowest load is
        // executable, so the initial spans reach both readers even
        // without it; the TEXTREL-caused cases are the two named
        // textrel_ tests below.
        // A reader in the RW load's own bytes.
        let mut image = domain_image(0x30100, 0x30000, 0x430000);
        put_ph(&mut image, 1, 1, 6, 0x30000, 0x430000, 0x100);
        put_ph(&mut image, 2, 2, 4, 0x30040, 0x430040, 32);
        image[0x30040..0x30048].copy_from_slice(&22i64.to_le_bytes());
        image[56..58].copy_from_slice(&3u16.to_le_bytes());
        let image = with_program_header(image, 0x114, 0x400000, 0);
        let plan = plan_bytes(&image).unwrap().unwrap();
        assert!(plan.patches.is_empty(), "{plan:?}");
        // The hole shape is the same image with the reader in the hole
        // at 0x420000 instead: covered by the initial-map span.
        let mut image = domain_image(0x30100, 0x20000, 0x420000);
        put_ph(&mut image, 1, 1, 6, 0x30000, 0x430000, 0x100);
        put_ph(&mut image, 2, 2, 4, 0x30040, 0x430040, 32);
        image[0x30040..0x30048].copy_from_slice(&22i64.to_le_bytes());
        image[56..58].copy_from_slice(&3u16.to_le_bytes());
        let image = with_program_header(image, 0x114, 0x400000, 0);
        let plan = plan_bytes(&image).unwrap().unwrap();
        assert!(plan.patches.is_empty(), "{plan:?}");
    }

    #[test]
    fn textrel_covers_holes_even_when_the_lowest_load_is_read_only() {
        // An R load below the RX load, DT_TEXTREL present, and
        // the reader in the hole between them.
        let mut image = domain_image(0x47000, 0x46000, 0x3f8000);
        // A read-only load below the loop's load (0x3e0000, backed
        // at file 0x2e000), so the lowest load is not executable,
        // and a dynamic segment carrying DT_TEXTREL.
        put_ph(&mut image, 1, 1, 4, 0x2e000, 0x3e0000, 0x100);
        put_ph(&mut image, 2, 2, 4, 0x2e040, 0x3e0040, 32);
        image[0x2e040..0x2e048].copy_from_slice(&22i64.to_le_bytes());
        image[56..58].copy_from_slice(&3u16.to_le_bytes());
        let image = with_program_header(image, 0x114, 0x400000, 0);
        let plan = plan_bytes(&image).unwrap().unwrap();
        assert!(plan.patches.is_empty(), "{plan:?}");
        assert_eq!(
            plan.report.refused.get("gap-reader-inside-the-sequence"),
            Some(&1),
            "{plan:?}"
        );
        // Matched control: identical layout, only the reader at
        // file 0x46000 is a NOP; the loop is rewritten.
        let mut control = domain_image(0x47000, 0x46000, 0x3f8000);
        control[0x46000..0x46004].copy_from_slice(&0xd503_201fu32.to_le_bytes());
        put_ph(&mut control, 1, 1, 4, 0x2e000, 0x3e0000, 0x100);
        put_ph(&mut control, 2, 2, 4, 0x2e040, 0x3e0040, 32);
        control[0x2e040..0x2e048].copy_from_slice(&22i64.to_le_bytes());
        control[56..58].copy_from_slice(&3u16.to_le_bytes());
        let control = with_program_header(control, 0x114, 0x400000, 0);
        let plan = plan_bytes(&control).unwrap().unwrap();
        assert_eq!(plan.patches.len(), 1, "{plan:?}");
    }

    #[test]
    fn textrel_readers_decode_through_their_own_load() {
        // An RX load at VA 0x430000 / offset 0x10000 holding bytes
        // the planner must not touch here (they are not the loop's
        // section), and an RW load VA 0x450000 / offset 0x20000
        // whose reader at VA 0x450080 reads the loop in the first
        // load. Only the RW load's own translation (VA 0x450080 is
        // file 0x20080) decodes it correctly.
        let mut image = domain_image(0x51000, 0x20080, 0x450080);
        // Retarget that reader at the loop (0x400100).
        image[0x20080..0x20084].copy_from_slice(&gap_ldr_at(0x450080, 0x400100).to_le_bytes());
        put_ph(&mut image, 1, 1, 6, 0x20000, 0x450000, 0x100);
        put_ph(&mut image, 2, 2, 4, 0x20040, 0x450040, 32);
        image[0x20040..0x20048].copy_from_slice(&22i64.to_le_bytes());
        image[56..58].copy_from_slice(&3u16.to_le_bytes());
        let image = with_program_header(image, 0x114, 0x400000, 0);
        let plan = plan_bytes(&image).unwrap().unwrap();
        assert!(plan.patches.is_empty(), "{plan:?}");
        assert_eq!(
            plan.report.refused.get("gap-reader-inside-the-sequence"),
            Some(&1),
            "{plan:?}"
        );
    }

    #[test]
    fn memsz_tails_of_an_executable_load_are_collected() {
        // As a static ET_EXEC, so no hole domain applies.
        let mut image = domain_image(0x30100, 0x20000, 0x420000);
        image[16..18].copy_from_slice(&2u16.to_le_bytes());
        image[96..104].copy_from_slice(&0x114u64.to_le_bytes());
        image[104..112].copy_from_slice(&0x30000u64.to_le_bytes());
        let plan = plan_bytes(&image).unwrap().unwrap();
        assert!(plan.patches.is_empty(), "{plan:?}");
        // Matched control: same memsz (0x30000), only the reader
        // at file 0x20000 is a NOP; the loop is rewritten.
        let mut control = domain_image(0x30100, 0x20000, 0x420000);
        control[0x20000..0x20004].copy_from_slice(&0xd503_201fu32.to_le_bytes());
        control[16..18].copy_from_slice(&2u16.to_le_bytes());
        control[96..104].copy_from_slice(&0x114u64.to_le_bytes());
        control[104..112].copy_from_slice(&0x30000u64.to_le_bytes());
        let plan = plan_bytes(&control).unwrap().unwrap();
        assert_eq!(plan.patches.len(), 1, "{plan:?}");
        // The same reader in a static image whose segment ends
        // at 0x114 with no memsz tail: not collected, still patched.
        let mut image = domain_image(0x30100, 0x20000, 0x420000);
        image[16..18].copy_from_slice(&2u16.to_le_bytes());
        let image = with_program_header(image, 0x114, 0x400000, 0);
        let plan = plan_bytes(&image).unwrap().unwrap();
        assert_eq!(plan.patches.len(), 1, "{plan:?}");
    }

    #[test]
    fn a_section_reader_names_a_combined_refusal_before_a_gap_reader() {
        // A section ADRP of unknown extent and a gap LDR both reach
        // the loop: the refusal is the proven reader's name, not
        // the gap's, because section guards run first.
        let loop_words = [
            0x885f_fc08,
            0x1100_0508,
            0x8809_fc08,
            0x35ff_ffa9,
            0xd65f_03c0,
        ];
        let reader = [0x9000_0001u32, 0xd503_201f, 0xd65f_03c0];
        let mut image = two_sections_with_marks(&loop_words, &reader, &[(1, 0)]);
        image[0x180..0x184].copy_from_slice(&gap_ldr_at(0x400180, 0x400100).to_le_bytes());
        let plan = plan_bytes(&image).unwrap().unwrap();
        assert!(plan.patches.is_empty(), "{plan:?}");
        assert_eq!(
            plan.report.refused.get("literal-inside-the-sequence"),
            Some(&1),
            "{plan:?}"
        );
        assert_eq!(
            plan.report.refused.get("gap-reader-inside-the-sequence"),
            None,
            "{plan:?}"
        );
    }

    #[test]
    fn bytes_mapped_by_no_load_are_not_scanned() {
        // Precision: a reader-shaped word in file bytes no load
        // maps, in a static two-load layout, so it is rewritten.
        let loop_words = [
            0x885f_fc08,
            0x1100_0508,
            0x8809_fc08,
            0x35ff_ffa9,
            0xd65f_03c0,
        ];
        let mut image =
            two_sections_with_marks(&loop_words, &[0xd503_201f, 0xd65f_03c0], &[(1, 0)]);
        image[16..18].copy_from_slice(&2u16.to_le_bytes());
        image.resize(0x20100, 0);
        image[0x18000..0x18004].copy_from_slice(&gap_ldr_at(0x418000, 0x400100).to_le_bytes());
        put_ph(&mut image, 1, 1, 5, 0x20000, 0x500000, 0x100);
        image[56..58].copy_from_slice(&2u16.to_le_bytes());
        let image = with_program_header(image, 0x300, 0x400000, 0);
        let plan = plan_bytes(&image).unwrap().unwrap();
        assert_eq!(plan.patches.len(), 1, "{plan:?}");
        assert!(plan.report.refused.is_empty(), "{plan:?}");
    }

    #[test]
    fn each_initial_span_decodes_through_its_own_translation() {
        // Lowest offset 0x1000, so the word at file 0x21000 decodes
        // at PC 0x420000 in the 4 KiB initial span and at 0x421000 in
        // the 16/64 KiB spans. Two images pin each direction: in one
        // the word is a loop reader only at the 4 KiB PC, in the
        // other only at the larger-page PC.
        let build = |reader: u32| {
            let mut image = domain_image(0x30100, 0x21000, 0x420000);
            image[0x21000..0x21004].copy_from_slice(&reader.to_le_bytes());
            // Back the loop section at file 0x1100, where this
            // image's first load (offset 0x1000) maps VA 0x400100,
            // and the second section at file 0x1200, where that
            // translation maps VA 0x400200.
            let text: Vec<u8> = image[0x100..0x208].to_vec();
            image[0x1100..0x1100 + text.len()].copy_from_slice(&text);
            let shoff = u64::from_le_bytes(image[40..48].try_into().unwrap()) as usize;
            for (index, off) in [(1usize, 0x1100u64), (2, 0x1200)] {
                let header = shoff + 64 * index;
                image[header + 24..header + 32].copy_from_slice(&off.to_le_bytes());
            }
            put_ph(&mut image, 1, 1, 6, 0x30000, 0x430000, 0x100);
            put_ph(&mut image, 2, 2, 4, 0x30040, 0x430040, 16);
            image[56..58].copy_from_slice(&3u16.to_le_bytes());
            with_program_header(image, 0x114, 0x400000, 0x1000)
        };
        // Reader only at the 4 KiB PC (0x420000 -> loop 0x400100).
        let plan = plan_bytes(&build(gap_ldr_at(0x420000, 0x400100)))
            .unwrap()
            .unwrap();
        assert!(plan.patches.is_empty(), "{plan:?}");
        // Reader only at the larger-page PC (0x421000 -> loop).
        let plan = plan_bytes(&build(gap_ldr_at(0x421000, 0x400100)))
            .unwrap()
            .unwrap();
        assert!(plan.patches.is_empty(), "{plan:?}");
    }

    /// Shift every recorded address of a two-section image by
    /// `delta`: section header addresses and symbol values naming
    /// the old section starts move with them (file bytes stay).
    fn shift_section_addresses(image: &mut [u8], delta: u64) {
        let shoff = u64::from_le_bytes(image[40..48].try_into().unwrap()) as usize;
        for index in [1usize, 2] {
            let header = shoff + 64 * index;
            let addr = u64::from_le_bytes(image[header + 16..header + 24].try_into().unwrap());
            image[header + 16..header + 24].copy_from_slice(&(addr + delta).to_le_bytes());
        }
        // Symbol values naming the old section starts, in the
        // symtab the helper appended (section header index 4).
        let sym_header = shoff + 64 * 4;
        let sym_at = u64::from_le_bytes(image[sym_header + 24..sym_header + 32].try_into().unwrap())
            as usize;
        let sym_len =
            u64::from_le_bytes(image[sym_header + 32..sym_header + 40].try_into().unwrap())
                as usize;
        for entry in (sym_at..sym_at + sym_len).step_by(24) {
            let value = u64::from_le_bytes(image[entry + 8..entry + 16].try_into().unwrap());
            if value == 0x400100 || value == 0x400200 {
                image[entry + 8..entry + 16].copy_from_slice(&(value + delta).to_le_bytes());
            }
        }
    }

    #[test]
    fn a_partly_file_backed_prefix_gap_is_clipped_not_skipped() {
        // Static RX at VA 0x401000, offset 0, filesz 0x300; the loop
        // section is at 0x401100 (file 0x100), the reader at VA
        // 0x401080 / file 0x80, in the part of the 64 KiB scan
        // domain before the segment.
        let loop_words = [
            0x885f_fc08,
            0x1100_0508,
            0x8809_fc08,
            0x35ff_ffa9,
            0xd65f_03c0,
        ];
        let build = |reader: u32| {
            let mut image =
                two_sections_with_marks(&loop_words, &[0xd503_201f, 0xd65f_03c0], &[(1, 0)]);
            shift_section_addresses(&mut image, 0x1000);
            image[16..18].copy_from_slice(&2u16.to_le_bytes());
            image[0x80..0x84].copy_from_slice(&reader.to_le_bytes());
            with_program_header(image, 0x300, 0x401000, 0)
        };
        let plan = plan_bytes(&build(gap_ldr_at(0x401080, 0x401100)))
            .unwrap()
            .unwrap();
        assert!(plan.patches.is_empty(), "{plan:?}");
        assert_eq!(
            plan.report.refused.get("gap-reader-inside-the-sequence"),
            Some(&1),
            "{plan:?}"
        );
        let plan = plan_bytes(&build(0xd503_201f)).unwrap().unwrap();
        assert_eq!(plan.patches.len(), 1, "{plan:?}");
    }

    /// A reader in the first mapped page before `p_vaddr` (the
    /// lld-style shape: RX at VA 0x2a30 / offset 0x1a30 under a
    /// lower read-only load, so the executable bytes VA
    /// [0x2000, 0x2a30) sit before the segment's own start). The
    /// reader at VA 0x2100 / file 0x1100 reads the loop at
    /// 0x2b30.
    #[test]
    fn a_reader_before_p_vaddr_is_scanned() {
        let loop_words = [
            0x885f_fc08,
            0x1100_0508,
            0x8809_fc08,
            0x35ff_ffa9,
            0xd65f_03c0,
        ];
        let build = |reader: u32| {
            let mut image =
                two_sections_with_marks(&loop_words, &[0xd503_201f, 0xd65f_03c0], &[(1, 0)]);
            image.resize(0x4000, 0);
            // Move the sections into the RX load: the loop to VA
            // 0x2b30 / file 0x1b30, the second section to VA
            // 0x2c30 / file 0x1c30.
            let text: Vec<u8> = image[0x100..0x208].to_vec();
            image[0x1b30..0x1b30 + text.len()].copy_from_slice(&text);
            for (from, to) in [(0x400100u64, 0x2b30u64), (0x400200, 0x2c30)] {
                let from = from.to_le_bytes();
                let mut at = 0;
                while let Some(found) = image[at..].windows(8).position(|w| w == from) {
                    at += found;
                    image[at..at + 8].copy_from_slice(&to.to_le_bytes());
                    at += 8;
                }
            }
            let shoff = u64::from_le_bytes(image[40..48].try_into().unwrap()) as usize;
            for (index, off) in [(1usize, 0x1b30u64), (2, 0x1c30)] {
                let header = shoff + 64 * index;
                image[header + 24..header + 32].copy_from_slice(&off.to_le_bytes());
            }
            image[0x1100..0x1104].copy_from_slice(&reader.to_le_bytes());
            let filesz = (image.len() - 0x1a30) as u64;
            put_ph(&mut image, 0, 1, 5, 0x1a30, 0x2a30, filesz);
            put_ph(&mut image, 1, 1, 4, 0, 0, 0x1000);
            put_ph(&mut image, 2, 2, 4, 0x3000, 0x3000, 16);
            image[56..58].copy_from_slice(&3u16.to_le_bytes());
            image
        };
        let plan = plan_bytes(&build(gap_ldr_at(0x2100, 0x2b30)))
            .unwrap()
            .unwrap();
        assert!(plan.patches.is_empty(), "{plan:?}");
        assert_eq!(
            plan.report.refused.get("gap-reader-inside-the-sequence"),
            Some(&1),
            "{plan:?}"
        );
        // Matched control: the word at VA 0x2100 is a NOP.
        let plan = plan_bytes(&build(0xd503_201f)).unwrap().unwrap();
        assert_eq!(plan.patches.len(), 1, "{plan:?}");
    }

    /// A dynamic image whose lowest load is read-only and which
    /// has no TEXTREL: no initial span exists, so a reader in the
    /// hole between the loads is never collected and the loop is
    /// rewritten. This pins the boundary of the hole domain (it
    /// also kills an "always add spans" mutant).
    #[test]
    fn a_dynamic_hole_without_textrel_or_an_executable_lowest_load_is_not_scanned() {
        let mut image = domain_image(0x3f1000, 0x3f0000, 0x3f0000);
        image[0x3f0000..0x3f0004].copy_from_slice(&gap_ldr_at(0x3f0000, 0x400100).to_le_bytes());
        put_ph(&mut image, 1, 1, 4, 0, 0, 0x1000);
        put_ph(&mut image, 2, 2, 4, 0x3000, 0x3000, 16);
        image[56..58].copy_from_slice(&3u16.to_le_bytes());
        let image = with_program_header(image, 0x114, 0x400000, 0);
        let plan = plan_bytes(&image).unwrap().unwrap();
        assert_eq!(plan.patches.len(), 1, "{plan:?}");
        assert!(plan.report.refused.is_empty(), "{plan:?}");
    }

    #[test]
    fn only_a_smaller_page_initial_span_reaches_a_low_offset_reader() {
        // The lowest load's VA/offset shifted by 0xf000, the later
        // load at +0x2f000, and the reader at +0x38000, which lies
        // in the 4 KiB initial span but past the 64 KiB formula's
        // end (its O floors to 0 at 64 KiB).
        let loop_words = [
            0x885f_fc08,
            0x1100_0508,
            0x8809_fc08,
            0x35ff_ffa9,
            0xd65f_03c0,
        ];
        let mut image =
            two_sections_with_marks(&loop_words, &[0xd503_201f, 0xd65f_03c0], &[(1, 0)]);
        image.resize(0x39000, 0);
        // Move the section bytes and addresses up by 0xf000.
        let text: Vec<u8> = image[0x100..0x208].to_vec();
        image[0xf100..0xf100 + text.len()].copy_from_slice(&text);
        let shoff = u64::from_le_bytes(image[40..48].try_into().unwrap()) as usize;
        for index in [1usize, 2] {
            let header = shoff + 64 * index;
            let off = u64::from_le_bytes(image[header + 24..header + 32].try_into().unwrap());
            image[header + 24..header + 32].copy_from_slice(&(off + 0xf000).to_le_bytes());
        }
        shift_section_addresses(&mut image, 0xf000);
        image[0x38000..0x38004].copy_from_slice(&gap_ldr_at(0x438000, 0x40f100).to_le_bytes());
        put_ph(&mut image, 1, 1, 6, 0x2f000, 0x42f000, 0x1000);
        put_ph(&mut image, 2, 2, 4, 0x2f040, 0x42f040, 16);
        image[56..58].copy_from_slice(&3u16.to_le_bytes());
        let image = with_program_header(image, 0x1000, 0x40f000, 0xf000);
        let plan = plan_bytes(&image).unwrap().unwrap();
        assert!(plan.patches.is_empty(), "{plan:?}");
        assert_eq!(
            plan.report.refused.get("gap-reader-inside-the-sequence"),
            Some(&1),
            "{plan:?}"
        );
        // Matched control: identical geometry, only the reader at
        // file 0x38000 is a NOP; the loop is rewritten.
        let mut control =
            two_sections_with_marks(&loop_words, &[0xd503_201f, 0xd65f_03c0], &[(1, 0)]);
        control.resize(0x39000, 0);
        let text: Vec<u8> = control[0x100..0x208].to_vec();
        control[0xf100..0xf100 + text.len()].copy_from_slice(&text);
        let shoff = u64::from_le_bytes(control[40..48].try_into().unwrap()) as usize;
        for index in [1usize, 2] {
            let header = shoff + 64 * index;
            let off = u64::from_le_bytes(control[header + 24..header + 32].try_into().unwrap());
            control[header + 24..header + 32].copy_from_slice(&(off + 0xf000).to_le_bytes());
        }
        shift_section_addresses(&mut control, 0xf000);
        control[0x38000..0x38004].copy_from_slice(&0xd503_201fu32.to_le_bytes());
        put_ph(&mut control, 1, 1, 6, 0x2f000, 0x42f000, 0x1000);
        put_ph(&mut control, 2, 2, 4, 0x2f040, 0x42f040, 16);
        control[56..58].copy_from_slice(&3u16.to_le_bytes());
        let control = with_program_header(control, 0x1000, 0x40f000, 0xf000);
        let plan = plan_bytes(&control).unwrap().unwrap();
        assert_eq!(plan.patches.len(), 1, "{plan:?}");
    }

    #[test]
    fn readers_at_a_64k_location_and_their_reader_free_control() {
        let image = domain_image(0x10000, 0x8000, 0x408000);
        let image = with_program_header(image, 0x114, 0x400000, 0);
        let plan = plan_bytes(&image).unwrap().unwrap();
        assert!(plan.patches.is_empty(), "{plan:?}");
        let mut image = domain_image(0x10000, 0x8000, 0x408000);
        image[0x8000..0x8004].copy_from_slice(&0xd503_201fu32.to_le_bytes());
        let image = with_program_header(image, 0x114, 0x400000, 0);
        let plan = plan_bytes(&image).unwrap().unwrap();
        assert_eq!(plan.patches.len(), 1, "{plan:?}");
    }

    #[test]
    fn widening_covered_intervals_scans_bytes_no_domain_maps() {
        // Precision for the widening mutant: both sections sit
        // outside the first load's 64 KiB domain in their own far
        // load, and the reader-shaped word at VA 0x418000 (under
        // the first load's translation) is mapped by no domain.
        // Correct clipping rewrites it; widening the covered intervals
        // without clipping lets that word refuse.
        let loop_words = [
            0x885f_fc08,
            0x1100_0508,
            0x8809_fc08,
            0x35ff_ffa9,
            0xd65f_03c0,
        ];
        let mut image =
            two_sections_with_marks(&loop_words, &[0xd503_201f, 0xd65f_03c0], &[(1, 0)]);
        image[16..18].copy_from_slice(&2u16.to_le_bytes());
        image.resize(0x20200, 0);
        // The second section goes to VA 0x420100 / file 0x20100 in
        // its own load; its bytes move with it.
        let second_bytes: Vec<u8> = image[0x200..0x208].to_vec();
        image[0x20100..0x20108].copy_from_slice(&second_bytes);
        let shoff = u64::from_le_bytes(image[40..48].try_into().unwrap()) as usize;
        let second = shoff + 64 * 2;
        image[second + 16..second + 24].copy_from_slice(&0x420100u64.to_le_bytes());
        image[second + 24..second + 32].copy_from_slice(&0x20100u64.to_le_bytes());
        image[0x18000..0x18004].copy_from_slice(&gap_ldr_at(0x418000, 0x400100).to_le_bytes());
        put_ph(&mut image, 1, 1, 5, 0x20000, 0x420000, 0x200);
        image[56..58].copy_from_slice(&2u16.to_le_bytes());
        let image = with_program_header(image, 0x300, 0x400000, 0);
        let plan = plan_bytes(&image).unwrap().unwrap();
        assert_eq!(plan.patches.len(), 1, "{plan:?}");
        assert!(plan.report.refused.is_empty(), "{plan:?}");
    }

    #[test]
    fn bytes_past_a_dynamic_images_span_are_not_scanned() {
        // The same precision shape in a dynamic image: the word at
        // VA 0x440000 is past every load domain and past the
        // initial map's span too, so the loop is still rewritten.
        let loop_words = [
            0x885f_fc08,
            0x1100_0508,
            0x8809_fc08,
            0x35ff_ffa9,
            0xd65f_03c0,
        ];
        let mut image =
            two_sections_with_marks(&loop_words, &[0xd503_201f, 0xd65f_03c0], &[(1, 0)]);
        image.resize(0x20200, 0);
        let second_bytes: Vec<u8> = image[0x200..0x208].to_vec();
        image[0x20100..0x20108].copy_from_slice(&second_bytes);
        let shoff = u64::from_le_bytes(image[40..48].try_into().unwrap()) as usize;
        let second = shoff + 64 * 2;
        image[second + 16..second + 24].copy_from_slice(&0x420100u64.to_le_bytes());
        image[second + 24..second + 32].copy_from_slice(&0x20100u64.to_le_bytes());
        image.resize(0x40100, 0);
        image[0x40000..0x40004].copy_from_slice(&gap_ldr_at(0x440000, 0x400100).to_le_bytes());
        put_ph(&mut image, 1, 1, 5, 0x20000, 0x420000, 0x200);
        put_ph(&mut image, 2, 2, 4, 0x20040, 0x420040, 16);
        image[56..58].copy_from_slice(&3u16.to_le_bytes());
        let image = with_program_header(image, 0x300, 0x400000, 0);
        let plan = plan_bytes(&image).unwrap().unwrap();
        assert_eq!(plan.patches.len(), 1, "{plan:?}");
    }

    /// What an `ADRP` lets the code read is protected like a literal load's
    /// target, from the sequence's own section, another marked section and
    /// an unknown range alike: a load through the page its width, and a use
    /// of unknown extent (the page held at a return or across a call, an
    /// index load, a copy) any executable byte. With no reader, a reader of
    /// other bytes, or the page gone before the call, the loop is still
    /// rewritten.
    #[test]
    fn adrp_readers_protect_the_bytes_they_read() {
        let loop_words = [
            0x885f_fc08, // 0x400100: ldaxr w8, [x0]
            0x1100_0508, // add w8, w8, #1
            0x8809_fc08, // stlxr w9, w8, [x0]
            0x35ff_ffa9, // cbnz w9, 0x400100
            0xd65f_03c0, // ret
        ];
        const ADRP: u32 = 0x9000_0001; // adrp x1, 0x400000 (from 0x400200)
        const ADRP19: u32 = 0x9000_0013; // adrp x19, 0x400000
        const MOV: u32 = 0xd280_0001; // mov x1, #0: the page is gone
        const MOV19: u32 = 0xd280_0013; // mov x19, #0
        const NOP: u32 = 0xd503_201f;
        const RET: u32 = 0xd65f_03c0;
        let reads: &[(&str, &[u32])] = &[
            // ldr w2, [x1, #0x100]: the loop's first word.
            ("a load of the loop", &[ADRP, 0xb941_0022, MOV, RET]),
            // Nothing completes the page before the return.
            ("an ADRP nothing completes", &[ADRP, NOP, RET]),
            // add x1, x1, #0x100; ldr w2, [x1, x3]: an index from the loop.
            (
                "an index load from an ADD",
                &[ADRP, 0x9104_0021, 0xb863_6822, MOV, RET],
            ),
            // ldr w0, [x19, #0x200]; bl; ldr w1, [x19, #0x100]: completed,
            // then held across the call and read there.
            (
                "a page reused after a call",
                &[ADRP19, 0xb942_0260, 0x9400_0002, 0xb941_0261, RET],
            ),
            // mov x20, x19; mov x19, #0; ldr w1, [x20, #0x100]
            (
                "a copy read after the page is gone",
                &[ADRP19, 0xaa13_03f4, MOV19, 0xb941_0281, RET],
            ),
        ];
        let misses: &[(&str, &[u32])] = &[
            ("no reader", &[NOP, NOP, NOP, RET]),
            // ldr w2, [x1, #0x200]: the reader's own section.
            ("a load of other bytes", &[ADRP, 0xb942_0022, MOV, RET]),
            // ldr w0, [x19, #0x200]; mov x19, #0; bl: nothing held at the call.
            (
                "the page gone before a call",
                &[ADRP19, 0xb942_0260, MOV19, 0x9400_0002, RET],
            ),
        ];
        let layouts: &[(&str, &[(u32, u64)])] = &[
            ("marked code", &[(1, 0)]),
            ("unmarked section", &[]),
            ("unsupported section", &[(7, 0)]),
        ];
        for &(layout, marks) in layouts {
            for &(read, reader) in reads {
                let image = two_sections_with_marks(&loop_words, reader, marks);
                let (unchanged, report) = rewritten(&image);
                assert_eq!(unchanged, image, "{layout}, {read}: {report:?}");
                assert_eq!(
                    report.refused_sites,
                    vec![(0x400100, "literal-inside-the-sequence".into())],
                    "{layout}, {read}: {report:?}"
                );
            }
            for &(miss, reader) in misses {
                let image = two_sections_with_marks(&loop_words, reader, marks);
                let (changed, report) = rewritten(&image);
                assert_eq!(report.patched_total(), 1, "{layout}, {miss}: {report:?}");
                assert_ne!(changed, image, "{layout}, {miss}");
            }
        }

        // An index load from an address an `ADD` formed before the loop reads
        // on into it: add x1, x1, #0x100 names a `nop` before the loop, so
        // ldr w2, [x1, x3] may read the loop, and ldr w2, [x1] only the `nop`.
        let mut shifted = vec![NOP];
        shifted.extend(loop_words);
        for &(layout, marks) in layouts {
            let index = [ADRP, 0x9104_0021, 0xb863_6822, MOV, RET];
            let image = two_sections_with_marks(&shifted, &index, marks);
            let (unchanged, report) = rewritten(&image);
            assert_eq!(unchanged, image, "{layout}: {report:?}");
            assert_eq!(
                report.refused_sites,
                vec![(0x400104, "literal-inside-the-sequence".into())],
                "{layout}: {report:?}"
            );
            let first = [ADRP, 0x9104_0021, 0xb940_0022, MOV, RET];
            let image = two_sections_with_marks(&shifted, &first, marks);
            assert_eq!(rewritten(&image).1.patched_total(), 1, "{layout}");
        }

        // In the loop's own section: ldr w2, [x1, #0x104] reads the loop's
        // first instruction (the `ldaxr` after `LOOP`'s `nop`), and
        // ldr w2, [x1, #0x100] only the `nop` before it.
        let own = |load: u32| {
            let mut words = LOOP.to_vec();
            words.extend([ADRP, load, MOV, RET]);
            elf_with_text(&words)
        };
        let image = own(0xb941_0422);
        let (unchanged, report) = rewritten(&image);
        assert_eq!(unchanged, image, "{report:?}");
        assert_eq!(
            report.refused,
            BTreeMap::from([("literal-inside-the-sequence", 1)]),
            "{report:?}"
        );
        assert_eq!(rewritten(&own(0xb941_0022)).1.patched_total(), 1);
    }

    #[test]
    fn a_literal_load_of_an_fde_entry_keeps_the_read_bytes() {
        let words = [
            0x885f_fc08, // 0x100: ldaxr w8, [x0]
            0x1100_0508, // add w8, w8, #1
            0x8809_fc08, // stlxr w9, w8, [x0]
            0x35ff_ffa9, // cbnz w9, 0x100
            0xd65f_03c0, // ret
            0x18ff_ff61, // 0x114: ldr w1, 0x100 (reads code bytes)
            0xd65f_03c0, // ret
        ];
        let marks = [(1, 0, true)];
        let functions = [(0, 5), (5, 7)];

        // Without a read of its bytes, the marked canonical loop qualifies.
        let mut control = words;
        control[5] = 0xd503_201f; // nop
        let image = elf_image_with(&control, Some(&marks), Some(&functions));
        let (changed, report) = rewritten(&image);
        assert_eq!(report.patched_total(), 1, "{report:?}");
        assert_ne!(changed, image);

        // An FDE never exempts a memory read of the entry's instruction bytes.
        for unwind in [None, Some(functions.as_slice())] {
            let image = elf_image_with(&words, Some(&marks), unwind);
            let plan = plan_bytes(&image).unwrap().unwrap();
            assert!(plan.patches.is_empty(), "{plan:?}");
            let (unchanged, report) = rewritten(&image);
            assert_eq!(unchanged, image, "{report:?}");
            assert_eq!(report.patched_total(), 0, "{report:?}");
            assert_eq!(
                report.refused.get("literal-inside-the-sequence"),
                Some(&1),
                "{report:?}"
            );
        }

        // ADR takes an address rather than reading bytes, so an FDE start
        // qualifies where nothing reads through the address (`mov x1, #0`
        // drops it); an address inside the sequence does not. Kept to the
        // return, the pointer reaches the caller, which may read it as well
        // as call it.
        let taken = |adr: u32, then: u32| {
            let mut address = words.to_vec();
            address[5] = adr;
            address.insert(6, then);
            elf_image_with(&address, Some(&marks), Some(&[(0, 5), (5, 8)]))
        };
        let image = taken(0x10ff_ff61, 0xd280_0001); // adr x1, 0x100; mov x1, #0
        let (changed, report) = rewritten(&image);
        assert_eq!(report.patched_total(), 1, "{report:?}");
        assert_ne!(changed, image);
        for (adr, then) in [
            (0x10ff_ff81, 0xd280_0001), // adr x1, 0x104; mov x1, #0
            (0x10ff_ff61, 0xd503_201f), // adr x1, 0x100; nop: held at the return
        ] {
            let image = taken(adr, then);
            assert!(plan_bytes(&image).unwrap().unwrap().patches.is_empty());
            let (unchanged, report) = rewritten(&image);
            assert_eq!(unchanged, image, "{report:?}");
            assert_eq!(report.patched_total(), 0, "{report:?}");
            assert_eq!(
                report.refused.get("literal-inside-the-sequence"),
                Some(&1),
                "{report:?}"
            );
        }
    }

    /// A function pointer can be read as well as called: one taken with
    /// `ADR` at an FDE start, kept in a register the callee preserves across
    /// a call and read after it (`adr x19, 0x100; bl; ldr w0, [x19]`), leaves
    /// the loop alone. Dropped before the call, nothing reads it.
    #[test]
    fn a_function_pointer_kept_across_a_call_protects_the_function() {
        let words = vec![
            0x885f_fc08, // 0x100: ldaxr w8, [x0]
            0x1100_0508, // add w8, w8, #1
            0x8809_fc08, // stlxr w9, w8, [x0]
            0x35ff_ffa9, // cbnz w9, 0x100
            0xd65f_03c0, // ret
        ];
        let functions = [(0, 5), (5, 9), (9, 10)];
        let image = |reader: [u32; 4]| {
            let mut words = words.clone();
            words.extend(reader);
            words.push(0xd65f_03c0); // 0x124: the callee, ret
            elf_image_with(&words, Some(&[(1, 0, true)]), Some(&functions))
        };
        const ADR: u32 = 0x10ff_ff73; // 0x114: adr x19, 0x100
        const RET: u32 = 0xd65f_03c0;
        // bl 0x124; ldr w0, [x19]
        let read = image([ADR, 0x9400_0003, 0xb940_0260, RET]);
        let (unchanged, report) = rewritten(&read);
        assert_eq!(unchanged, read, "{report:?}");
        assert_eq!(
            report.refused,
            BTreeMap::from([("literal-inside-the-sequence", 1)]),
            "{report:?}"
        );
        // mov x19, #0; bl 0x124
        let dropped = image([ADR, 0xd280_0013, 0x9400_0002, RET]);
        let (changed, report) = rewritten(&dropped);
        assert_eq!(report.patched_total(), 1, "{report:?}");
        assert_ne!(changed, dropped);
    }

    /// The canonical loop at 0x400100 in the first of
    /// [`two_sections_with_marks`]'s sections, its `ret` at 0x400110.
    const NARROWED_LOOP: [u32; 5] = [
        0x885f_fc08, // 0x400100: ldaxr w8, [x0]
        0x1100_0508, // add w8, w8, #1
        0x8809_fc08, // stlxr w9, w8, [x0]
        0x35ff_ffa9, // cbnz w9, 0x400100
        0xd65f_03c0, // 0x400110: ret
    ];

    /// The forms a base survives, from another marked section: `BFXIL` and
    /// `CCMP` keep it (the first reads the bits it does not insert, the
    /// second writes only flags), `BC.cond` branches with it held, and
    /// `DC ZVA` zeroes a block through it. Each leaves the loop alone; a
    /// `BFXIL` into another register, a `DC CVAC` and the base dropped before
    /// a `BC.cond` do not.
    #[test]
    fn a_base_kept_by_a_partial_write_a_flag_write_or_a_branch_still_protects() {
        const ADRP: u32 = 0x9000_0001; // adrp x1, 0x400000 (from 0x400200)
        const MOV: u32 = 0xd280_0001; // mov x1, #0
        const RET: u32 = 0xd65f_03c0;
        const ADD: u32 = 0x9103_0021; // add x1, x1, #0xc0: before the loop
        let readers: &[(&str, &[u32])] = &[
            // bfxil x1, x2, #8, #8; ldr w0, [x1, #0x100]
            ("BFXIL", &[ADRP, 0xb348_3c41, 0xb941_0020, MOV, RET]),
            // ccmp x1, x2, #1, eq; ldr w0, [x1, #0x100]
            ("CCMP", &[ADRP, 0xfa42_0021, 0xb941_0020, MOV, RET]),
            // bc.eq +8; mov x1, #0; ret
            ("BC.cond", &[ADRP, 0x5400_0050, MOV, RET]),
            // dc zva, x1
            ("DC ZVA", &[ADRP, ADD, 0xd50b_7421, MOV, RET]),
        ];
        let controls: &[(&str, &[u32])] = &[
            // bfxil x3, x2, #8, #8; ldr w0, [x1, #0x200]
            ("BFXIL into x3", &[ADRP, 0xb348_3c43, 0xb942_0020, MOV, RET]),
            // ccmp x1, x2, #1, eq; ldr w0, [x1, #0x200]
            (
                "CCMP, other bytes",
                &[ADRP, 0xfa42_0021, 0xb942_0020, MOV, RET],
            ),
            // mov x1, #0; bc.eq +8
            ("BC.cond after the drop", &[ADRP, MOV, 0x5400_0050, RET]),
            // dc cvac, x1
            ("DC CVAC", &[ADRP, ADD, 0xd50b_7a21, MOV, RET]),
        ];
        for &(form, reader) in readers {
            let image = two_sections_with_marks(&NARROWED_LOOP, reader, &[(1, 0)]);
            let (unchanged, report) = rewritten(&image);
            assert_eq!(unchanged, image, "{form}: {report:?}");
            assert_eq!(
                report.refused_sites,
                vec![(0x400100, "literal-inside-the-sequence".into())],
                "{form}: {report:?}"
            );
        }
        for &(form, reader) in controls {
            let image = two_sections_with_marks(&NARROWED_LOOP, reader, &[(1, 0)]);
            let (changed, report) = rewritten(&image);
            assert_eq!(report.patched_total(), 1, "{form}: {report:?}");
            assert_ne!(changed, image, "{form}");
        }
    }

    /// Under the unwind table alone, code is what control reaches from the
    /// functions' entries: a constant table a function addresses with `ADR`
    /// and branches over is inside its FDE but is not reached, so it is not
    /// rewritten, however its words decode.
    #[test]
    fn a_table_a_function_branches_over_is_not_code_under_the_unwind_table() {
        let words = [
            0x1000_0061, // adr x1, <the table, word 3>
            0xb940_0423, // ldr w3, [x1, #4]
            0x1400_0006, // b <word 8>
            0x1122_3344, // the table: a word, then a loop's shape
            0x885f_fc08,
            0x1100_0508,
            0x8809_fc08,
            0x35ff_ffa9,
            0xd65f_03c0, // ret
        ];
        // The branch over the table as B, and as the conditional forms that
        // always branch: B.AL, CBZ WZR and TBZ WZR.
        for branch in [0x1400_0006, 0x5400_00ce, 0x3400_00df, 0x3600_00df] {
            let mut words = words;
            words[2] = branch;
            let image = elf_image_with(&words, None, Some(&[(0, words.len())]));
            let (bytes, report) = rewritten(&image);
            assert_eq!(report.patched_total(), 0, "{branch:#010x}: {report:?}");
            assert_eq!(report.exclusive_instructions, 2, "{report:?}");
            assert_eq!(
                report.refused,
                BTreeMap::from([("no-mapping-symbols", 1)]),
                "{branch:#010x}: {report:?}"
            );
            assert_eq!(bytes, image, "{branch:#010x}: the table is untouched");
        }
        // Marked as code, the same words would be rewritten: the
        // reachability is what refuses them. The load reads the table's
        // first word into the register, which then holds no address; the one
        // at #4 reads the loop's first instruction, which a rewrite would
        // change under it, and keeps the address across the branch, so that
        // loop is left alone.
        let mut marked_words = words;
        marked_words[1] = 0xb940_0021; // ldr w1, [x1]
        let marked = elf_image(&marked_words, Some(&[(1, 0, true)]));
        assert_eq!(rewritten(&marked).1.patched_total(), 1);
        let reading = rewritten(&elf_image(&words, Some(&[(1, 0, true)]))).1;
        assert_eq!(reading.patched_total(), 0, "{reading:?}");
        assert_eq!(
            reading.refused,
            BTreeMap::from([("literal-inside-the-sequence", 1)]),
            "{reading:?}"
        );
    }

    /// Under the unwind table alone, a call is taken to return, so a table
    /// placed after a call that does not return is reached as code. What the
    /// code addresses as data is taken for data from there to the next
    /// instruction a branch reaches, so a table whose words form a loop is
    /// refused, addressed either by `ADRP`+`ADD` or by `ADR`. Marked `$x`, the
    /// same words are refused too while the table's address is held across
    /// the call, and rewritten when only the header word is loaded.
    #[test]
    fn a_table_after_a_call_that_does_not_return_is_not_rewritten() {
        // <address the table at word 3>; bl <word 9>, which traps; a header
        // word; the loop; ret; brk.
        let table = [
            0x1234_5678, // the header the code loads
            0x885f_fc08, // ldaxr w8, [x0]
            0x1100_0508, // add w8, w8, #1
            0x8809_fc08, // stlxr w9, w8, [x0]
            0x35ff_ffa9, // cbnz w9, <ldaxr>
        ];
        let mut words = [0u32; 10];
        words[2] = 0x9400_0007; // bl <word 9>
        words[3..8].copy_from_slice(&table);
        words[8] = 0xd65f_03c0; // ret
        words[9] = 0xd420_0000; // brk
        let addressings = [
            // adrp x1, 0x0; add x1, x1, #0x10c (the table at word 3).
            ([0x9000_0001, 0x9104_3021], "ADRP+ADD"),
            // adr x1, <word 3>; ldr w3, [x1, #4] (the loop's first word).
            ([0x1000_0061, 0xb940_0423], "ADR"),
        ];
        for (addressing, how) in addressings {
            let mut words = words;
            words[..2].copy_from_slice(&addressing);
            assert_eq!(
                mapping::addressed_starts(&words[..3], 0x100).starts,
                vec![0x10c],
                "{how} addresses the table"
            );
            let image = elf_image_with(&words, None, Some(&[(0, 9), (9, 10)]));
            let (bytes, report) = rewritten(&image);
            assert_eq!(bytes, image, "{how}: the table is untouched");
            assert_eq!(report.patched_total(), 0, "{how}: {report:?}");
            assert_eq!(report.exclusive_instructions, 2, "{how}: {report:?}");
            assert_eq!(
                report.refused,
                BTreeMap::from([("no-mapping-symbols", 1)]),
                "{how}: {report:?}"
            );
            // Marked as code, the loop is still left alone: the address of
            // the table is held across the call, where it may be read (and
            // `ADR`'s load at #4 reads the loop's first instruction).
            let marked = elf_image(&words, Some(&[(1, 0, true)]));
            let report = rewritten(&marked).1;
            assert_eq!(report.patched_total(), 0, "{how}: {report:?}");
            assert_eq!(
                report.refused,
                BTreeMap::from([("literal-inside-the-sequence", 1)]),
                "{how}: {report:?}"
            );
        }
        // Marked as code with only the header word loaded, into the register
        // itself (adrp x1, 0x0; ldr w1, [x1, #0x10c]), nothing is held across
        // the call, and only the unwind table's weaker evidence refused it.
        let mut header = words;
        header[..2].copy_from_slice(&[0x9000_0001, 0xb941_0c21]);
        let marked = elf_image(&header, Some(&[(1, 0, true)]));
        assert_eq!(rewritten(&marked).1.patched_total(), 1);
        // Nor is a table the code does not address, now that the call to
        // a function that cannot return does not fall through.
        let mut unaddressed = words;
        unaddressed[0] = 0xd503_201f;
        unaddressed[1] = 0xd503_201f;
        let image = elf_image_with(&unaddressed, None, Some(&[(0, 9), (9, 10)]));
        assert_eq!(rewritten(&image).1.patched_total(), 0);
        // With the callee returning the file is still left unchanged:
        // without mapping symbols nothing is rewritten.
        let mut returning = unaddressed;
        returning[9] = 0xd65f_03c0; // ret in place of the trap
        let image = elf_image_with(&returning, None, Some(&[(0, 9), (9, 10)]));
        let (bytes, report) = rewritten(&image);
        assert_eq!(bytes, image, "{report:?}");
        assert_eq!(report.patched_total(), 0, "{report:?}");
    }

    /// One `ADRP` whose register completes two accesses: the table after the
    /// second call is addressed only by the second, and must keep its bytes.
    /// (Found reviewing this change against the census's own reader.)
    #[test]
    fn a_table_addressed_by_a_second_use_of_one_adrp_is_not_rewritten() {
        let mut words = [0u32; 14];
        words[0] = 0x9000_0001; // adrp x1, 0x0
        words[1] = 0x9104_5022; // add x2, x1, #0x114 (a header word at word 5)
        words[2] = 0x9104_8023; // add x3, x1, #0x120 (the table at word 8)
        words[3] = 0xb400_0060; // cbz x0, <word 6>
        words[4] = 0x9400_0009; // bl <word 13>, which traps
        words[5] = 0x1234_5678; // the header
        words[6] = 0xd503_201f; // nop: code again, a branch target
        words[7] = 0x9400_0006; // bl <word 13>
        words[8..12].copy_from_slice(&[0x885f_fc08, 0x1100_0508, 0x8809_fc08, 0x35ff_ffa9]);
        words[12] = 0xd65f_03c0; // ret
        words[13] = 0xd420_0000; // brk
        assert_eq!(
            mapping::addressed_starts(&words[..3], 0x100).starts,
            vec![0x114, 0x120],
            "both completions of the one ADRP"
        );
        let image = elf_image_with(&words, None, Some(&[(0, 13), (13, 14)]));
        let (bytes, report) = rewritten(&image);
        assert_eq!(bytes, image, "{report:?}");
        assert_eq!(report.patched_total(), 0, "{report:?}");
    }

    /// Only a local, untyped symbol is a mapping symbol: an ordinary global
    /// label named `$x…` inside a `$d` constant table does not start a code
    /// range there, so the table keeps its bytes. A real local `$x` there
    /// does, which is the control.
    #[test]
    fn a_global_symbol_named_like_a_mapping_symbol_does_not_split_a_table() {
        // Code at 0x100, then the same words as a `$d` table at 0x118,
        // with a third mark at 0x11c, inside the table.
        let words: Vec<u32> = LOOP.iter().chain(LOOP.iter()).copied().collect();
        let inside = 0x100 + 4 * 7;
        let split = elf_image(&words, Some(&[(1, 0, true), (1, 6, false), (1, 7, true)]));
        let (bytes, report) = rewritten(&split);
        assert_eq!(
            report.patched_total(),
            2,
            "a local $x inside the table makes its loop code: {report:?}"
        );
        assert_ne!(bytes[0x11c..0x130], split[0x11c..0x130]);

        // The same file with that symbol's binding changed to STB_GLOBAL:
        // an ordinary label, so the `$d` range runs on.
        let local = mapping::tests::symbol(1, 0, 1, inside);
        let at = split
            .windows(local.len())
            .position(|window| window == local)
            .expect("the mark is in the symbol table");
        let mut global = split.clone();
        global[at..at + local.len()].copy_from_slice(&mapping::tests::symbol(1, 0x10, 1, inside));
        let (bytes, report) = rewritten(&global);
        assert_eq!(
            report.patched_total(),
            1,
            "a global $x does not split the table: {report:?}"
        );
        assert_eq!(
            bytes[0x118..0x130],
            global[0x118..0x130],
            "the table keeps its bytes"
        );
    }

    /// A table the code reaches only through a pointer stored elsewhere: no
    /// instruction of the file addresses it, so only the pointer says where
    /// the data is. In an executable with fixed addresses that pointer is a
    /// plain word of a loaded, non-executable section.
    #[test]
    fn a_table_a_stored_pointer_names_is_not_rewritten() {
        // ldr w1, [x0]; mov w2, #0; cbz w2, <word 8>; the loop-shaped
        // table; ret; brk. Nothing addresses the table.
        let mut words = [0u32; 10];
        words[0] = 0xb940_0001; // ldr w1, [x0]: through the argument
        words[1] = 0x5280_0002; // mov w2, #0
        words[2] = 0x3400_00c2; // cbz w2, <word 8>: always taken, but not
        words[3..7].copy_from_slice(&[0x885f_fc08, 0x1100_0508, 0x8809_fc08, 0x35ff_ffa9]);
        words[7] = 0xd503_201f; // nop
        words[8] = 0xd65f_03c0; // ret
        words[9] = 0xd420_0000; // brk
        let functions = [(0, 9), (9, 10)];
        // Without the pointer the file is still left unchanged:
        // without mapping symbols nothing is rewritten.
        let bare = elf_image_with(&words, None, Some(&functions));
        let (bytes, report) = rewritten(&bare);
        assert_eq!(bytes, bare, "{report:?}");
        assert_eq!(report.patched_total(), 0, "{report:?}");

        // An ET_EXEC image storing the table's address in its data.
        let mut image = elf_image_full(&words, None, Some(&functions), &[3]);
        image[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
        let (bytes, report) = rewritten(&image);
        assert_eq!(bytes, image, "{report:?}");
        assert_eq!(
            report.refused,
            BTreeMap::from([("no-mapping-symbols", 1)]),
            "{report:?}"
        );

        // A stored pointer to a function's entry changes nothing either:
        // the file is left unchanged without mapping symbols.
        let mut entry = elf_image_full(&words, None, Some(&functions), &[0]);
        entry[16..18].copy_from_slice(&2u16.to_le_bytes());
        let (bytes, report) = rewritten(&entry);
        assert_eq!(bytes, entry, "{report:?}");
        assert_eq!(report.patched_total(), 0, "{report:?}");
    }

    /// A call to a function the image names as one that never returns ends
    /// its path, so a table after it is not reached. Here the callee is a
    /// local symbol named `abort`, which is what a stripped library's
    /// imported one is through its procedure linkage table.
    #[test]
    fn a_call_to_a_function_that_never_returns_does_not_fall_through() {
        // bl <word 6>; the loop-shaped table; ret; the callee, which
        // returns, so only its name keeps the table from being code.
        let mut words = [0u32; 8];
        words[0] = 0x9400_0006; // bl <word 6>
        words[1..5].copy_from_slice(&[0x885f_fc08, 0x1100_0508, 0x8809_fc08, 0x35ff_ffa9]);
        words[5] = 0xd65f_03c0; // ret
        words[6] = 0xd65f_03c0; // the callee: ret
        words[7] = 0xd503_201f;
        let functions = [(0, 6), (6, 8)];
        // A mark at the callee, which the lines below turn into the
        // symbol that names it.
        let image = elf_image_with(
            &words,
            Some(&[(1, 0, true), (1, 6, true)]),
            Some(&functions),
        );
        // With mapping symbols the table is code, which is the control
        // for the fixture itself.
        assert_eq!(rewritten(&image).1.patched_total(), 1);

        // The same words with no mapping symbols are left unchanged: the
        // unwind table alone proves nothing.
        let unnamed = elf_image_with(&words, None, Some(&functions));
        let (bytes, report) = rewritten(&unnamed);
        assert_eq!(bytes, unnamed, "{report:?}");
        assert_eq!(report.patched_total(), 0, "{report:?}");

        // Now name the callee `abort`: a global function symbol at its
        // address, in a file with no mapping symbols.
        let mark = mapping::tests::symbol(1, 0, 1, 0x100 + 4 * 6);
        let at = image
            .windows(mark.len())
            .position(|window| window == mark)
            .expect("the mark is in the symbol table");
        let mut named = image.clone();
        // Name index 7 is "abort" in the builder's string table; STT_FUNC
        // with a global binding, as an ordinary symbol is.
        named[at..at + mark.len()].copy_from_slice(&mapping::tests::symbol(
            7,
            0x12,
            1,
            0x100 + 4 * 6,
        ));
        // And drop the section's other mapping symbol, so only the unwind
        // table tells its code from its data.
        let text = mapping::tests::symbol(1, 0, 1, 0x100);
        let at = named
            .windows(text.len())
            .position(|window| window == text)
            .expect("the first mark is in the symbol table");
        named[at..at + text.len()].copy_from_slice(&[0u8; 24]);
        let (bytes, report) = rewritten(&named);
        assert_eq!(bytes, named, "{report:?}");
        assert_eq!(report.patched_total(), 0, "{report:?}");
    }

    /// A literal or `ADR` that names a function's own entry is a pointer
    /// to the function, not to data, so the function's first sequence is
    /// still rewritten. A pointer into its body is data, as before.
    #[test]
    fn a_pointer_to_a_functions_entry_does_not_make_it_data() {
        // worker: the loop; its caller takes its address and calls it.
        let mut words = [0u32; 9];
        words[0..4].copy_from_slice(&[0x885f_fc08, 0x1100_0508, 0x8809_fc08, 0x35ff_ffa9]);
        words[4] = 0xd65f_03c0; // ret
        words[5] = 0x1000_0001; // adr x1, <word 0>: see below
        words[6] = 0xd63f_0020; // blr x1
        words[7] = 0xd65f_03c0; // ret
        words[8] = 0xd503_201f;
        // adr x1, 0x100 from 0x114 is -0x14.
        words[5] = 0x1000_0001 | ((((-0x14i32 >> 2) & 0x7_FFFF) as u32) << 5);
        let functions = [(0, 5), (5, 8)];
        let image = elf_image_with(&words, None, Some(&functions));
        let (bytes, report) = rewritten(&image);
        assert_eq!(bytes, image, "nothing is rewritten: {report:?}");
        assert_eq!(report.patched_total(), 0, "{report:?}");
        assert_eq!(
            report.refused,
            BTreeMap::from([("no-mapping-symbols", 1)]),
            "{report:?}"
        );

        // The same `ADR` one word into the function names data, and the
        // sequence over it is refused.
        let mut inside = words;
        inside[5] = 0x1000_0001 | ((((-0x10i32 >> 2) & 0x7_FFFF) as u32) << 5);
        let image = elf_image_with(&inside, None, Some(&functions));
        let (bytes, report) = rewritten(&image);
        assert_eq!(bytes, image, "{report:?}");
        assert_eq!(report.patched_total(), 0, "{report:?}");
    }

    /// A relocatable object's FDE starts are relocations to fill, so its
    /// unwind table is no evidence.
    #[test]
    fn a_relocatable_objects_unwind_table_is_no_evidence() {
        let mut image = elf_image_with(&LOOP, None, Some(&[(0, 6)]));
        image[16..18].copy_from_slice(&1u16.to_le_bytes()); // ET_REL
        let (bytes, report) = rewritten(&image);
        assert_eq!(bytes, image);
        assert_eq!(report.patched_total(), 0, "{report:?}");
        assert_eq!(
            report.refused,
            BTreeMap::from([("no-mapping-symbols", 1)]),
            "{report:?}"
        );
    }

    /// The loader entry inside a compare sequence refuses it: starting at
    /// the CMP executes a different instruction once the sequence's first
    /// word materialises the expected value and its second is the CAS.
    #[test]
    fn a_loader_entry_inside_a_sequence_refuses_it() {
        // ldaxr x0,[x2]; cmp x0,#1; b.ne out; stlxr w17,x1,[x2]; cbnz w17; ret
        let words = [
            0x885f_fc40,
            0x7100_041f,
            0x5400_0061,
            0x8811_fc41,
            0x35ff_ff91,
            0xd65f_03c0,
        ];
        let mut image = elf_image(&words, Some(&[(1, 0, true)]));
        // e_entry at the CMP (word 1).
        image[24..32].copy_from_slice(&0x104u64.to_le_bytes());
        let (bytes, report) = rewritten(&image);
        assert_eq!(bytes, image, "{report:?}");
        assert_eq!(report.patched_total(), 0, "{report:?}");
        assert_eq!(
            report.refused,
            BTreeMap::from([("branch-into-sequence", 1)]),
            "{report:?}"
        );
        // The same image entered at its first instruction is still a
        // compare idiom, left alone as compare-exit.
        let mut entered_at_head = elf_image(&words, Some(&[(1, 0, true)]));
        entered_at_head[24..32].copy_from_slice(&0x100u64.to_le_bytes());
        let (bytes, report) = rewritten(&entered_at_head);
        assert_eq!(bytes, entered_at_head, "{report:?}");
        assert_eq!(report.patched_total(), 0, "{report:?}");
        assert_eq!(
            report.refused,
            BTreeMap::from([("compare-exit", 1)]),
            "{report:?}"
        );
    }

    /// A function entry at a sub-word compare's head bypasses the
    /// zero-extension before it, whether the entry is named by an FDE or by
    /// a defined STT_FUNC symbol, so the compare stays refused.
    #[test]
    fn a_function_entry_at_a_sub_word_compare_bypasses_its_zero_extension() {
        // uxtb w16,w0; ldaxrb w0,[x2]; cmp w0,w16; b.ne out;
        // stlxrb w17,w1,[x2]; cbnz w17; ret
        let words = [
            0x5300_1c10,
            0x085f_fc40,
            0x6b10_001f,
            0x5400_0061,
            0x0811_fc41,
            0x35ff_ff91,
            0xd65f_03c0,
        ];
        // Named by its FDE alone.
        let image = elf_image_with(&words, Some(&[(1, 0, true)]), Some(&[(1, 7)]));
        let (bytes, report) = rewritten(&image);
        assert_eq!(bytes, image, "{report:?}");
        assert_eq!(report.patched_total(), 0, "{report:?}");
        assert_eq!(
            report.refused,
            BTreeMap::from([("register-constraint", 1)]),
            "{report:?}"
        );
        // Named by a defined STT_FUNC symbol alone: turn the $x mark at the
        // ldaxrb into a function symbol at the same address.
        let mut image = elf_image(&words, Some(&[(1, 0, true), (1, 1, true)]));
        let mark = mapping::tests::symbol(1, 0, 1, 0x104);
        let at = image
            .windows(mark.len())
            .position(|window| window == mark)
            .expect("the mark is in the symbol table");
        image[at..at + mark.len()].copy_from_slice(&mapping::tests::symbol(0, 0x12, 1, 0x104));
        let (bytes, report) = rewritten(&image);
        assert_eq!(bytes, image, "{report:?}");
        assert_eq!(report.patched_total(), 0, "{report:?}");
        assert_eq!(
            report.refused,
            BTreeMap::from([("register-constraint", 1)]),
            "{report:?}"
        );
        // With no entry at the head the extension runs, but a compare
        // idiom is still left alone as compare-exit.
        let image = elf_image(&words, Some(&[(1, 0, true)]));
        let (bytes, report) = rewritten(&image);
        assert_eq!(bytes, image, "{report:?}");
        assert_eq!(report.patched_total(), 0, "{report:?}");
        assert_eq!(
            report.refused,
            BTreeMap::from([("compare-exit", 1)]),
            "{report:?}"
        );
    }

    /// Zero padding in a marked section's unmarked bytes (here, before its
    /// first `$x`) is UDF #0, which traps and never branches, so the loop
    /// after it is still rewritten. A word in the same place that may be a
    /// direct branch with an unknown target refuses every site in the image.
    #[test]
    fn zero_padding_in_unmarked_bytes_does_not_refuse_the_image() {
        for (head, expected) in [(0x0000_0000, 1), (0x0001_0000, 0)] {
            let mut words = vec![head];
            words.extend_from_slice(&LOOP);
            let image = elf_image(&words, Some(&[(1, 1, true)]));
            let (bytes, report) = rewritten(&image);
            assert_eq!(report.patched_total(), expected, "{head:#x} {report:?}");
            if expected == 0 {
                assert_eq!(bytes, image, "{report:?}");
                assert_eq!(
                    report.refused,
                    BTreeMap::from([("unknown-direct-branch", 1)]),
                    "{report:?}"
                );
            } else {
                assert_ne!(bytes, image, "{report:?}");
            }
        }
    }

    /// A short addressed-data exclusion nested inside a whole-function
    /// exclusion must not shadow it: the candidate after the short range
    /// is still inside the function the unresolved ADRP took for data.
    #[test]
    fn a_short_exclusion_nested_in_a_whole_function_one_does_not_shadow_it() {
        // Directly: the disjoint-range lookup handles nested ranges.
        let mut plan = rewrite::Report::default();
        plan.patched.push(rewrite::Site {
            addr: 0x140,
            idiom: Idiom::Rmw,
            words: vec![0; 4],
        });
        refuse_overlapping(
            &mut plan,
            &[(0x100, 0x154), (0x130, 0x138)],
            Refusal::AddressedData,
        );
        assert_eq!(plan.patched, vec![]);
        assert_eq!(plan.refused.len(), 1);

        // End to end, where an unresolved ADRP and an ADR name data inside
        // one function: without mapping symbols the loop after the short
        // table keeps its bytes.
        let mut words = [0xd503_201fu32; 21];
        words[0] = 0x9000_0001; // adrp x1, <this page>, never completed
        words[1] = 0x1000_0162; // adr x2, <word 12>
        words[2] = 0xb940_0443; // ldr w3, [x2, #4]
        words[3] = 0x1400_0005; // b <word 8>
        words[9] = 0x1400_0005; // b <word 14>
        words[16] = 0x885f_fc08; // ldaxr w8, [x0]
        words[17] = 0x1100_0508; // add w8, w8, #1
        words[18] = 0x8809_fc08; // stlxr w9, w8, [x0]
        words[19] = 0x35ff_ffa9; // cbnz w9, <ldaxr>
        words[20] = 0xd65f_03c0; // ret
        let image = elf_image_with(&words, None, Some(&[(0, 21)]));
        let (bytes, report) = rewritten(&image);
        assert_eq!(bytes, image, "{report:?}");
        assert_eq!(report.patched_total(), 0, "{report:?}");
        assert_eq!(
            report.refused,
            BTreeMap::from([("no-mapping-symbols", 1)]),
            "{report:?}"
        );
    }

    /// A compare loop in one executable section whose failing exit
    /// branches to a store-exclusive in another section: a section
    /// boundary does not clear the exclusive monitor, so the compare is
    /// refused and the image is preserved. Without the outside store the
    /// same loop is rewritten.
    #[test]
    fn a_compare_whose_reservation_is_consumed_in_another_section_is_refused() {
        // ldaxr x0,[x2]; cbnz x0, <second section>; stlxr w3,x1,[x2];
        // cbnz w3, <ldaxr>
        let first = [0xc85f_fc40, 0xb500_07e0, 0xc803_fc41, 0x35ff_ffa3];
        // stlxr w4,x1,[x2]
        let second = [0xc804_fc41];
        let image = elf_image_two_sections(&first, &second);
        let (bytes, report) = rewritten(&image);
        assert_eq!(bytes, image, "{report:?}");
        assert_eq!(report.patched_total(), 0, "{report:?}");
        assert_eq!(report.refused.get("compare-exit"), Some(&1), "{report:?}");
        // The same loop with no store in the other section is also left
        // alone: compare idioms are not rewritten at all.
        let image = elf_image_two_sections(&first, &[0xd503_201f]);
        let (bytes, report) = rewritten(&image);
        assert_eq!(bytes, image, "{report:?}");
        assert_eq!(report.patched_total(), 0, "{report:?}");
        assert_eq!(report.refused.get("compare-exit"), Some(&1), "{report:?}");
    }

    /// Nested FDEs, direct data: data addressed inside both an enclosing
    /// and a short nested FDE runs to the enclosing end, so the loop
    /// after the nested range but inside the enclosing one is refused.
    #[test]
    fn nested_fdes_do_not_shorten_a_direct_data_exclusion() {
        let mut words = [0xd503_201fu32; 21];
        words[0] = 0x1000_01a2; // adr x2, <word 13>
        words[1] = 0xb940_0443; // ldr w3, [x2, #4]
        words[16] = 0x885f_fc08; // ldaxr w8, [x0]
        words[17] = 0x1100_0508; // add w8, w8, #1
        words[18] = 0x8809_fc08; // stlxr w9, w8, [x0]
        words[19] = 0x35ff_ffa9; // cbnz w9, <ldaxr>
        words[20] = 0xd65f_03c0; // ret
        let image = elf_image_with(&words, None, Some(&[(0, 21), (12, 14)]));
        let (bytes, report) = rewritten(&image);
        assert_eq!(bytes, image, "{report:?}");
        assert_eq!(report.patched_total(), 0, "{report:?}");
        assert_eq!(
            report.refused,
            BTreeMap::from([("no-mapping-symbols", 1)]),
            "{report:?}"
        );
        // Without the nested FDE the same program is likewise preserved.
        let image = elf_image_with(&words, None, Some(&[(0, 21)]));
        let (bytes, report) = rewritten(&image);
        assert_eq!(bytes, image, "{report:?}");
        assert_eq!(report.patched_total(), 0, "{report:?}");
    }

    /// Nested FDEs, unresolved ADRP page: an enclosing FDE that reaches
    /// into the page an uncompleted ADRP names is taken for data even
    /// when a short nested FDE ending before the page comes first in
    /// entry order.
    #[test]
    fn nested_fdes_do_not_hide_an_enclosing_function_from_an_unresolved_page() {
        let candidate = 1000usize;
        let mut words = vec![0xd503_201fu32; candidate + 5];
        words[0] = 0xb000_0001; // adrp x1, <page 0x1000>, never completed
        words[candidate] = 0x885f_fc08; // ldaxr w8, [x0]
        words[candidate + 1] = 0x1100_0508; // add w8, w8, #1
        words[candidate + 2] = 0x8809_fc08; // stlxr w9, w8, [x0]
        words[candidate + 3] = 0x35ff_ffa9; // cbnz w9, <ldaxr>
        words[candidate + 4] = 0xd65f_03c0; // ret
        let total = words.len();
        let image = elf_image_with(&words, None, Some(&[(0, total), (64, 65)]));
        let (bytes, report) = rewritten(&image);
        assert_eq!(bytes, image, "{report:?}");
        assert_eq!(report.patched_total(), 0, "{report:?}");
        assert_eq!(
            report.refused,
            BTreeMap::from([("no-mapping-symbols", 1)]),
            "{report:?}"
        );
    }

    /// A sub-word compare against a register is rewritten only after the
    /// zero-extension before it, which must be code too.
    #[test]
    fn a_sub_word_compare_needs_its_zero_extension_in_the_code() {
        // uxtb w16, w0; ldaxrb w0, [x2]; cmp w0, w16; b.ne out;
        // stlxrb w17, w1, [x2]; cbnz w17, <ldaxrb>; out: ret
        let words = [
            0x5300_1c10,
            0x085f_fc40,
            0x6b10_001f,
            0x5400_0061,
            0x0811_fc41,
            0x35ff_ff91,
            0xd65f_03c0,
        ];
        // With the extension in the code the compare is still left
        // alone as compare-exit.
        let whole = elf_image(&words, Some(&[(1, 0, true)]));
        let (bytes, report) = rewritten(&whole);
        assert_eq!(bytes, whole, "{report:?}");
        assert_eq!(report.patched_total(), 0, "{report:?}");
        assert_eq!(
            report.refused,
            BTreeMap::from([("compare-exit", 1)]),
            "{report:?}"
        );
        // The zero-extension is data: the compare is not rewritten
        // either; as a compare idiom it is left alone as compare-exit.
        let split = elf_image(&words, Some(&[(1, 0, false), (1, 1, true)]));
        let (bytes, report) = rewritten(&split);
        assert_eq!(bytes, split);
        assert_eq!(report.patched_total(), 0, "{report:?}");
        assert_eq!(
            report.refused,
            BTreeMap::from([("compare-exit", 1)]),
            "{report:?}"
        );
    }
}
