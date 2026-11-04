/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Telling code from data in an aarch64 ELF file's executable sections, by
//! the file's mapping symbols.
//!
//! An executable section can hold data as well as instructions: OpenSSL's
//! generated assembly, for one, keeps its constant tables in `.text`. Those
//! words decode as instructions as readily as code does, so a scan that reads
//! the whole section finds load/store-exclusive "loops" that never execute,
//! and a rewrite that took one would corrupt the table. The AArch64 ELF ABI
//! marks the boundaries with mapping symbols in the symbol table: `$x` (or
//! `$x.<any>`) starts A64 code and `$d` (or `$d.<any>`) starts data, each
//! running to the next mapping symbol of the same section ("ELF for the Arm
//! 64-bit Architecture", section 5.5.4, Mapping symbols). A stripped file has
//! no symbol table and so no mapping symbols, and a section can lack them too.
//!
//! Where a section has no mapping symbols, the file's unwind table
//! (`.eh_frame`) is the evidence instead: each frame description entry (FDE)
//! covers one function's code, stripped files keep the table for unwinding,
//! and a constant table has no FDE. An FDE covers its whole function, so a
//! table or literal pool placed inside the function is inside it too: under
//! that evidence alone, code is what control reaches from the functions'
//! entries ([`reachable_code`]), less what the code addresses as data, up
//! to the next instruction a branch reaches ([`data_extents`]), since a
//! call is taken to return; and a sequence a literal load reaches is
//! refused in any case ([`literal_targets`]). Bytes neither marks are not
//! known to be code.
//!
//! These are pure functions of the file's bytes, which the rewriter
//! ([`super::elf`]) consults before it plans a single sequence.

use super::insn::Insn;
use super::insn::decode;

/// `SHT_SYMTAB`: the full symbol table, which keeps local symbols.
const SHT_SYMTAB: u32 = 2;
/// `SHT_STRTAB`: a string table.
const SHT_STRTAB: u32 = 3;
/// The size of an ELF64 section header and of an ELF64 symbol.
const SECTION_HEADER_SIZE: usize = 64;
const SYMBOL_SIZE: usize = 24;
/// `STT_NOTYPE` and `STB_LOCAL`, the type and binding every mapping symbol
/// has: the info byte of a mapping symbol is exactly zero.
const STT_NOTYPE: u8 = 0;
const STB_LOCAL: u8 = 0;
/// `SHN_LORESERVE`: section indices from here up are not sections.
const SHN_LORESERVE: u16 = 0xff00;

/// What tells an ELF file's code from its data: its mapping symbols, and the
/// code ranges its unwind table's FDEs cover.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CodeEvidence {
    /// The mapping symbols, sorted as [`mapping_symbols`] returns them.
    pub symbols: Vec<MappingSymbol>,
    /// The local, untyped symbols named like mapping symbols that are neither
    /// `$x` nor `$d` ([`unsupported_mapping_symbols`]), as (section index,
    /// name). A section that has one is not proved by its mapping symbols.
    pub unsupported: Vec<(usize, String)>,
    /// The address range of each FDE, sorted; `None` without an unwind
    /// table, or with one this reader cannot decode.
    pub unwind: Option<Vec<(u64, u64)>>,
}

/// What [`CodeEvidence::classify`] found a section's code by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Evidence {
    /// The section's mapping symbols.
    MappingSymbols,
    /// The unwind table's FDEs.
    UnwindTable,
}

impl CodeEvidence {
    /// The evidence of an ELF64 little-endian file whose section header
    /// table is `table` and whose section-name string table is section
    /// `shstrndx`. `read` returns the file bytes at (offset, size), or
    /// `None` when it will not (out of the file, or larger than the caller
    /// reads); a table it does not return is evidence the file lacks.
    ///
    /// The unwind table is read only for a loaded image (`e_type` `ET_EXEC`
    /// or `ET_DYN`): in a relocatable object each FDE's start is a
    /// relocation's to fill, so the bytes alone would place every function
    /// near address 0.
    pub fn read(
        table: &[u8],
        shstrndx: usize,
        e_type: u16,
        mut read: impl FnMut((u64, u64)) -> Option<Vec<u8>>,
    ) -> Self {
        const ET_EXEC: u16 = 2;
        const ET_DYN: u16 = 3;
        let (symbols, unsupported) = symbol_table(table)
            .and_then(|(symtab, strtab)| {
                let (symtab, strtab) = (read(symtab)?, read(strtab)?);
                Some((
                    mapping_symbols(&symtab, &strtab),
                    unsupported_mapping_symbols(&symtab, &strtab),
                ))
            })
            .unwrap_or_default();
        let unwind = matches!(e_type, ET_EXEC | ET_DYN)
            .then(|| section_range(table, shstrndx))
            .flatten()
            .and_then(&mut read)
            .and_then(|names| section_named(table, &names, b".eh_frame"))
            .and_then(|(range, address)| fde_ranges(&read(range)?, address));
        CodeEvidence {
            symbols,
            unsupported,
            unwind,
        }
    }

    /// The first mapping symbol section `section` holds that is neither `$x`
    /// nor `$d`, if any.
    pub fn unsupported_in(&self, section: usize) -> Option<&str> {
        self.unsupported
            .iter()
            .find(|(marked, _)| *marked == section)
            .map(|(_, name)| name.as_str())
    }

    /// What section `section` (at `address`, `size` bytes long) holds: by its
    /// mapping symbols when it has any ([`code_spans`]), or else by the
    /// FDEs' ranges within it.
    pub fn classify(&self, section: usize, address: u64, size: u64) -> SectionCode {
        let end = address.saturating_add(size);
        let complement = |covered: &[(u64, u64)]| {
            let mut gaps = Vec::new();
            let mut at = address;
            for (start, stop) in covered {
                if at < *start {
                    gaps.push((at, *start));
                }
                at = at.max(*stop);
            }
            if at < end {
                gaps.push((at, end));
            }
            gaps
        };
        if let Some(spans) = code_spans(&self.symbols, section, address, size) {
            // `$d` ranges are data; only the bytes before the first mark are
            // unknown.
            let first = self
                .symbols
                .iter()
                .filter(|symbol| symbol.section == section)
                .map(|symbol| symbol.address)
                .find(|mark| (address..end).contains(mark))
                .unwrap_or(end);
            return SectionCode {
                code: spans,
                unknown: complement(&[(first, end)]),
                evidence: Some(Evidence::MappingSymbols),
            };
        }
        let Some(unwind) = &self.unwind else {
            return SectionCode {
                code: Vec::new(),
                unknown: complement(&[]),
                evidence: None,
            };
        };
        let mut code: Vec<(u64, u64)> = Vec::new();
        for (start, stop) in unwind
            .iter()
            .map(|(start, stop)| ((*start).max(address), (*stop).min(end)))
            .filter(|(start, stop)| start < stop)
        {
            match code.last_mut() {
                Some(last) if start <= last.1 => last.1 = last.1.max(stop),
                _ => code.push((start, stop)),
            }
        }
        // No FDE is not evidence of data: what no FDE covers is unknown.
        SectionCode {
            unknown: complement(&code),
            code,
            evidence: Some(Evidence::UnwindTable),
        }
    }
}

impl CodeEvidence {
    /// The entries of the functions the unwind table describes within
    /// `address..address + size`: each FDE's start.
    pub fn unwind_entries(&self, address: u64, size: u64) -> Vec<u64> {
        let end = address.saturating_add(size);
        self.unwind
            .iter()
            .flatten()
            .map(|(start, _)| *start)
            .filter(|start| (address..end).contains(start))
            .collect()
    }
}

/// Which of `functions` (sorted by entry, as the FDEs give them) can
/// return, by [`can_return`] over and over until the answer stops
/// growing: a function returns if a `RET` is reachable in it, and a call
/// it makes comes back only if that callee is already known to return and
/// `never` does not name it as one that cannot.
/// Each round can only add functions, so a few settle it; the bound keeps
/// a deep chain from costing more than it is worth, and stopping early
/// leaves a function unproven, which is the safe side.
pub fn returning_functions(
    words: &[u32],
    base: u64,
    functions: &[(u64, u64)],
    never: &dyn Fn(u64) -> bool,
) -> std::collections::HashSet<u64> {
    // Chains deeper than this stay unproven.
    const ROUNDS: usize = 8;
    let mut returning: std::collections::HashSet<u64> = std::collections::HashSet::new();
    for _ in 0..ROUNDS {
        let known = returning.clone();
        // A call out of this section is taken to return, the ordinary
        // case; one to a function here only where it is already proven.
        let returns = |target: u64| {
            !never(target)
                && (known.contains(&target)
                    || functions
                        .binary_search_by_key(&target, |(entry, _)| *entry)
                        .is_err())
        };
        let grown: Vec<u64> = functions
            .iter()
            .filter(|(entry, _)| !returning.contains(entry))
            .filter(|function| can_return(words, base, **function, &returns))
            .map(|(entry, _)| *entry)
            .collect();
        if grown.is_empty() {
            break;
        }
        returning.extend(grown);
    }
    returning
}

/// Whether the function `function` (its entry and end, as an FDE gives
/// them) can return: walking its own words from its entry, some
/// instruction it reaches is a `RET X30`, or a direct branch out of it (a
/// tail call) goes to a function that `returns` says comes back. A `RET`
/// through another register is not a return to this function's caller, and
/// an `SVC` proves nothing past it, whatever its immediate, since what the
/// kernel does with it is not in the file. What this does not check is a
/// write to `X30` before a `RET X30`, so a function that returns somewhere
/// else through a changed link register is taken to return to its caller,
/// and an unresolved `BLR`, whose callee is unknown, is taken to come
/// back. A function
/// whose entry is outside `words` is taken to return, which is the
/// ordinary case for a call out of the section. A function whose every path ends in
/// a trap or in a syscall that does not come back (an exit or abort
/// helper) cannot, and a call to it does not fall through.
pub fn can_return(
    words: &[u32],
    base: u64,
    function: (u64, u64),
    returns: &dyn Fn(u64) -> bool,
) -> bool {
    // `RET X30`, and its authenticated forms `RETAA` and `RETAB`: a
    // return through another register is not a return to this function's
    // caller.
    const RET_X30: [u32; 3] = [0xD65F_03C0, 0xD65F_0BFF, 0xD65F_0FFF];
    // `SVC #imm`: Linux ignores the immediate, and what the kernel does
    // with the call is not in the file, so nothing past it is proof.
    let is_svc = |word: u32| word & 0xFFE0_001F == 0xD400_0001;
    let (entry, end) = function;
    let index = |addr: u64| -> Option<usize> {
        (entry..end)
            .contains(&addr)
            .then(|| usize::try_from((addr - base) / 4).ok())
            .flatten()
            .filter(|index| *index < words.len())
    };
    let Some(first) = index(entry) else {
        return true;
    };
    let last = index(end.saturating_sub(4)).unwrap_or(first);
    let mut seen = vec![false; last - first + 1];
    let mut pending = vec![first];
    while let Some(mut at) = pending.pop() {
        while !seen[at - first] {
            seen[at - first] = true;
            if RET_X30.contains(&words[at]) {
                return true;
            }
            if is_svc(words[at]) {
                break;
            }
            let pc = base + 4 * at as u64;
            let (falls_through, target) = flow(words[at], pc);
            // A call comes back only where `returns` already says so, so
            // the answer only grows as more functions are known to return
            // ([`returning_functions`]).
            let call = matches!(decode(words[at]), Insn::Branch { link: true, .. });
            let comes_back = !call || target.is_some_and(returns);
            // A tail call, a direct branch out of this function, returns
            // to this function's caller when the function it jumps to
            // returns at all.
            if !call
                && let Some(target) = target
                && index(target).is_none()
                && returns(target)
            {
                return true;
            }
            if let Some(target) = target.filter(|_| !call).and_then(index) {
                pending.push(target);
            }
            match (falls_through && comes_back)
                .then(|| index(pc + 4))
                .flatten()
            {
                Some(next) => at = next,
                None => break,
            }
        }
    }
    false
}

/// The code among `words` (at `base`) that control can reach from
/// `entries` without leaving `covered` (sorted, disjoint): following each
/// instruction to the next unless it is an unconditional branch, a return,
/// an indirect jump or a trap, and each direct branch to its target. Under
/// the unwind table alone this is what is taken for code: an FDE covers its
/// whole function, including a constant table the function branches over or
/// keeps after its return, and those bytes are not reached. Code reached
/// only through a jump table or an exception landing pad is not either, and
/// is left alone with them. A call falls through only where `returns`
/// proves the callee comes back; [`data_extents`] takes back what the code
/// addresses as data.
pub fn reachable_code(
    words: &[u32],
    base: u64,
    covered: &[(u64, u64)],
    entries: &[u64],
    returns: &dyn Fn(u64) -> bool,
) -> Vec<(u64, u64)> {
    let index_of = |addr: u64| -> Option<usize> {
        if addr < base || !addr.is_multiple_of(4) {
            return None;
        }
        let index = usize::try_from((addr - base) / 4).ok()?;
        let after = covered.partition_point(|(start, _)| *start <= addr);
        let inside = after.checked_sub(1).is_some_and(|i| addr < covered[i].1);
        (index < words.len() && inside).then_some(index)
    };
    let mut seen = vec![false; words.len()];
    let mut pending: Vec<usize> = entries
        .iter()
        .filter_map(|entry| index_of(*entry))
        .collect();
    while let Some(mut index) = pending.pop() {
        while !seen[index] {
            seen[index] = true;
            let pc = base + 4 * index as u64;
            let (mut falls_through, target) = flow(words[index], pc);
            // A call comes back only if the function it calls can.
            if matches!(decode(words[index]), Insn::Branch { link: true, .. })
                && target.is_some_and(|target| !returns(target))
            {
                falls_through = false;
            }
            if let Some(target) = target.and_then(index_of) {
                pending.push(target);
            }
            match falls_through.then(|| index_of(pc + 4)).flatten() {
                Some(next) => index = next,
                None => break,
            }
        }
    }
    let mut spans: Vec<(u64, u64)> = Vec::new();
    for (index, _) in seen.iter().enumerate().filter(|(_, seen)| **seen) {
        let addr = base + 4 * index as u64;
        match spans.last_mut() {
            Some(last) if last.1 == addr => last.1 = addr + 4,
            _ => spans.push((addr, addr + 4)),
        }
    }
    spans
}

/// Where control goes after `word` at `pc`: whether it can fall through to
/// the next instruction, and the target of a direct branch it can take. A
/// conditional branch whose outcome the encoding fixes counts as what it
/// does: `B.AL` and `B.NV` always branch, and the zero register makes `CBZ`
/// and `TBZ` always branch and `CBNZ` and `TBNZ` never.
fn flow(word: u32, pc: u64) -> (bool, Option<u64>) {
    const ZR: super::insn::Reg = 31;
    let target = |offset: i64| Some(pc.wrapping_add(offset as u64));
    match decode(word) {
        Insn::Branch {
            link: false,
            offset,
        } => (false, target(offset)),
        Insn::CondBranch {
            cond: 0b1110 | 0b1111,
            offset,
        } => (false, target(offset)),
        Insn::CompareBranch {
            rt: ZR,
            nonzero,
            offset,
            ..
        } => {
            if nonzero {
                (true, None)
            } else {
                (false, target(offset))
            }
        }
        // TBZ/TBNZ: op (24), Rt (4:0).
        Insn::TestBranch { offset } if word & 0x1f == u32::from(ZR) => {
            if (word >> 24) & 1 == 1 {
                (true, None)
            } else {
                (false, target(offset))
            }
        }
        Insn::Branch { link: true, offset }
        | Insn::CondBranch { offset, .. }
        | Insn::CompareBranch { offset, .. }
        | Insn::TestBranch { offset } => (true, target(offset)),
        // Unconditional branch (register): BLR and its authenticated forms
        // return; BR, RET, ERET and theirs do not.
        _ if word & 0xFE00_0000 == 0xD600_0000 => {
            (matches!((word >> 21) & 0xF, 0b0001 | 0b1001), None)
        }
        // UDF, BRK and HLT trap.
        _ if word & 0xFFFF_0000 == 0
            || word & 0xFFE0_001F == 0xD420_0000
            || word & 0xFFE0_001F == 0xD440_0000 =>
        {
            (false, None)
        }
        _ => (true, None),
    }
}

/// What one executable section holds, as address ranges, by
/// [`CodeEvidence::classify`]. What is in neither list is data (`$d`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SectionCode {
    /// Known to be code.
    pub code: Vec<(u64, u64)>,
    /// Neither known code nor known data: bytes before a section's first
    /// mapping symbol, bytes no FDE covers, or the whole section when
    /// nothing speaks for it.
    pub unknown: Vec<(u64, u64)>,
    /// What `code` was found by; `None` when nothing speaks for the section.
    pub evidence: Option<Evidence>,
}

/// The file range (offset, size) of section `index` of a section header
/// table.
fn section_range(table: &[u8], index: usize) -> Option<(u64, u64)> {
    let entry = table.as_chunks::<SECTION_HEADER_SIZE>().0.get(index)?;
    Some((
        u64::from_le_bytes(entry[24..32].try_into().unwrap()),
        u64::from_le_bytes(entry[32..40].try_into().unwrap()),
    ))
}

/// The file range and address of the section named `name`, given the
/// section-name string table's bytes.
fn section_named(table: &[u8], names: &[u8], name: &[u8]) -> Option<((u64, u64), u64)> {
    table
        .as_chunks::<SECTION_HEADER_SIZE>()
        .0
        .iter()
        .find(|entry| {
            let at = u32::from_le_bytes(entry[0..4].try_into().unwrap()) as usize;
            names
                .get(at..)
                .and_then(|rest| rest.get(..name.len() + 1))
                .is_some_and(|candidate| {
                    candidate[..name.len()] == *name && candidate[name.len()] == 0
                })
        })
        .map(|entry| {
            (
                (
                    u64::from_le_bytes(entry[24..32].try_into().unwrap()),
                    u64::from_le_bytes(entry[32..40].try_into().unwrap()),
                ),
                u64::from_le_bytes(entry[16..24].try_into().unwrap()),
            )
        })
}

/// A cursor over the bytes of an unwind table.
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Cursor<'_> {
    fn take(&mut self, len: usize) -> Option<&[u8]> {
        let bytes = self.bytes.get(self.at..self.at.checked_add(len)?)?;
        self.at += len;
        Some(bytes)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn uleb(&mut self) -> Option<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = self.u8()?;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }

    fn sleb(&mut self) -> Option<i64> {
        let mut value = 0i64;
        let mut shift = 0;
        loop {
            let byte = self.u8()?;
            value |= i64::from(byte & 0x7f) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                if shift < 64 && byte & 0x40 != 0 {
                    value |= -1i64 << shift;
                }
                return Some(value);
            }
            if shift >= 64 {
                return None;
            }
        }
    }

    /// A pointer in `encoding` (DW_EH_PE_*), at address `here` of the
    /// cursor's position. Only absolute and pc-relative values are read;
    /// any other application, or an indirect one, is `None`.
    fn pointer(&mut self, encoding: u8, here: u64) -> Option<u64> {
        let value = self.raw_pointer(encoding)?;
        match encoding & 0xf0 {
            0x00 => Some(value),
            0x10 => Some(here.wrapping_add(value)),
            _ => None,
        }
    }

    /// A pointer's value as stored in `encoding`'s format, before any
    /// application: how a pointer this reader does not need (a
    /// personality routine's, usually indirect) is stepped over.
    fn raw_pointer(&mut self, encoding: u8) -> Option<u64> {
        Some(match encoding & 0x0f {
            0x00 | 0x04 => u64::from_le_bytes(self.take(8)?.try_into().unwrap()),
            0x01 => self.uleb()?,
            0x02 => u64::from(u16::from_le_bytes(self.take(2)?.try_into().unwrap())),
            0x03 => u64::from(u32::from_le_bytes(self.take(4)?.try_into().unwrap())),
            0x09 => self.sleb()? as u64,
            0x0a => i16::from_le_bytes(self.take(2)?.try_into().unwrap()) as u64,
            0x0b => i32::from_le_bytes(self.take(4)?.try_into().unwrap()) as u64,
            0x0c => u64::from_le_bytes(self.take(8)?.try_into().unwrap()),
            _ => return None,
        })
    }
}

/// The FDE pointer encoding of a CIE, given its body after the CIE ID:
/// `DW_EH_PE_absptr` unless its augmentation names one with `R`. `None` for
/// a version other than 1 or 3 (the two `.eh_frame` uses), an augmentation
/// this reader does not know, or a body that ends early.
fn cie_pointer_encoding(body: &[u8]) -> Option<u8> {
    let mut cie = Cursor { bytes: body, at: 0 };
    let version = cie.u8()?;
    if !matches!(version, 1 | 3) {
        return None;
    }
    let augmentation_start = cie.at;
    while cie.u8()? != 0 {}
    let augmentation = body[augmentation_start..cie.at - 1].to_vec();
    if augmentation.windows(2).any(|pair| pair == b"eh") {
        cie.take(8)?;
    }
    cie.uleb()?; // code alignment
    cie.sleb()?; // data alignment
    if version == 1 {
        cie.u8()?;
    } else {
        cie.uleb()?;
    }
    if augmentation.first() != Some(&b'z') {
        return Some(0);
    }
    cie.uleb()?; // augmentation data length
    for letter in &augmentation[1..] {
        match letter {
            b'R' => return cie.u8(),
            b'L' => {
                cie.u8()?;
            }
            b'P' => {
                let encoding = cie.u8()?;
                // An aligned pointer would need padding this reader does
                // not track.
                if encoding & 0x70 == 0x50 {
                    return None;
                }
                cie.raw_pointer(encoding)?;
            }
            b'S' | b'B' => {}
            _ => return None,
        }
    }
    Some(0)
}

/// The address range of each FDE of an unwind table (`.eh_frame`, at
/// `address`), sorted. `None` when the table cannot be read to its end: a
/// reader that skipped an FDE it could not decode would take that
/// function's code for data, which is safe, but one that misread a length
/// could take data for code.
pub fn fde_ranges(bytes: &[u8], address: u64) -> Option<Vec<(u64, u64)>> {
    let mut ranges = Vec::new();
    // The FDE pointer encoding of each CIE walked so far, by the offset of
    // its length field: an FDE must name one of these.
    let mut encodings = std::collections::HashMap::new();
    let mut at = 0usize;
    while at + 4 <= bytes.len() {
        let mut header = Cursor { bytes, at };
        let length = u32::from_le_bytes(header.take(4)?.try_into().unwrap());
        if length == 0 {
            break;
        }
        let length = if length == u32::MAX {
            u64::from_le_bytes(header.take(8)?.try_into().unwrap())
        } else {
            u64::from(length)
        };
        let end = header.at.checked_add(usize::try_from(length).ok()?)?;
        if end > bytes.len() {
            return None;
        }
        // Each entry is read within its own length.
        let mut entry = Cursor {
            bytes: &bytes[..end],
            at: header.at,
        };
        let id_at = entry.at;
        let id = u32::from_le_bytes(entry.take(4)?.try_into().unwrap());
        if id == 0 {
            encodings.insert(at, cie_pointer_encoding(&bytes[entry.at..end])?);
        } else {
            // An FDE: its CIE is `id` bytes before the field.
            let cie = id_at.checked_sub(usize::try_from(id).ok()?)?;
            let encoding = *encodings.get(&cie)?;
            let start = entry.pointer(encoding, address.wrapping_add(entry.at as u64))?;
            let size = entry.pointer(encoding & 0x0f, 0)?;
            if size != 0 {
                ranges.push((start, start.checked_add(size)?));
            }
        }
        at = end;
    }
    ranges.sort_unstable();
    Some(ranges)
}

/// The byte ranges `words` (at `base`) load as literals or take the address
/// of: each `LDR` (literal) target with its load's size, and each `ADR`
/// target as one word, since what follows it is not known. A literal pool
/// inside a function is read this way, so a candidate sequence any of these
/// reach is data the unwind table took for code.
pub fn literal_targets(words: &[u32], base: u64) -> Vec<(u64, u64)> {
    words
        .iter()
        .enumerate()
        .filter_map(|(index, word)| {
            let pc = base.wrapping_add(4 * index as u64);
            if word & 0x3B00_0000 == 0x1800_0000 {
                // LDR (literal): opc (31:30), V (26), imm19 (23:5).
                let size = match (word >> 30, (word >> 26) & 1) {
                    (0b00, _) | (0b10, 0) => 4,
                    (0b01, _) => 8,
                    (0b10, _) => 16,
                    _ => return None, // PRFM (literal) loads nothing
                };
                let offset = ((((word >> 5) & 0x7_FFFF) << 13) as i32 >> 11) as i64;
                let target = pc.wrapping_add(offset as u64);
                Some((target, target.wrapping_add(size)))
            } else if word & 0x9F00_0000 == 0x1000_0000 {
                // ADR: immlo (30:29), immhi (23:5).
                let imm = ((word >> 5) & 0x7_FFFF) << 2 | ((word >> 29) & 0b11);
                let offset = ((imm << 11) as i32 >> 11) as i64;
                let target = pc.wrapping_add(offset as u64);
                Some((target, target.wrapping_add(4)))
            } else {
                None
            }
        })
        .collect()
}

/// Where `words` (at `base`) address bytes as data, or may: the target of
/// each `LDR` (literal) and `ADR`, and of each `ADRP` with every `ADD`
/// (immediate) or unsigned-offset load or store that completes it among the
/// eight instructions after it, taking the `ADRP`'s register as its base,
/// up to the instruction that writes that register without completing it,
/// since the page is gone from there. One `ADRP` can address several tables.
/// An `ADRP` nothing completes gives its page, the lowest address it can be
/// completing. Each is where a table may start: how far it runs is not in
/// the instruction ([`data_extents`]).
pub fn addressed_starts(words: &[u32], base: u64) -> Addressed {
    // How far after an `ADRP` its completing instruction is looked for.
    const PAIRING_WINDOW: usize = 8;
    let mut starts = Vec::new();
    let mut unresolved = Vec::new();
    for (index, word) in words.iter().enumerate() {
        let pc = base.wrapping_add(4 * index as u64);
        if word & 0x3B00_0000 == 0x1800_0000 && word >> 30 != 0b11 {
            // LDR (literal), not PRFM: its target is its literal's start.
            let offset = ((((word >> 5) & 0x7_FFFF) << 13) as i32 >> 11) as i64;
            starts.push(pc.wrapping_add(offset as u64));
        } else if word & 0x9F00_0000 == 0x1000_0000 {
            // ADR: immlo (30:29), immhi (23:5).
            let imm = ((word >> 5) & 0x7_FFFF) << 2 | ((word >> 29) & 0b11);
            let offset = ((imm << 11) as i32 >> 11) as i64;
            starts.push(pc.wrapping_add(offset as u64));
        } else if word & 0x9F00_0000 == 0x9000_0000 {
            // ADRP: the 4 KiB page of `pc`, plus immhi:immlo pages.
            let imm = ((word >> 5) & 0x7_FFFF) << 2 | ((word >> 29) & 0b11);
            let pages = ((imm << 11) as i32 >> 11) as i64;
            let page = (pc & !0xFFF).wrapping_add((pages << 12) as u64);
            let register = word & 0x1F;
            // Every completion, following the register: an `ADD` into the
            // register itself moves the base the later ones are relative
            // to, and any other write loses it, so the pairing ends there.
            let mut base = page;
            let mut completions = 0usize;
            for next in words[index + 1..].iter().take(PAIRING_WINDOW) {
                let offset = page_offset(*next, register);
                if let Some(offset) = offset {
                    starts.push(base.wrapping_add(offset));
                    completions += 1;
                    // `ADD Xn, Xn, #imm`: from here the register holds
                    // that address, so a later access is relative to it.
                    if *next & 0x1F == register && *next & 0xFF80_0000 == 0x9100_0000 {
                        base = base.wrapping_add(offset);
                        continue;
                    }
                }
                // A write that does not carry the base ends the
                // pairing. What the code addresses through the register
                // afterwards is then unknown; taking the whole page for
                // data there costs about half of a stripped library's
                // rewrites (measured), so it is left as a limit this
                // module's documentation states.
                if writes(*next, register) {
                    break;
                }
            }
            if completions == 0 {
                // Nothing in the window completes it, so all that is known
                // is the page: the caller takes the functions it overlaps
                // for data.
                unresolved.push(page);
            }
        }
    }
    Addressed { starts, unresolved }
}

/// What [`addressed_starts`] found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Addressed {
    /// Where data may start, as addresses.
    pub starts: Vec<u64>,
    /// The page each `ADRP` nothing in the window completes names: its
    /// target is somewhere from there (an `ADD` can place it up to 16 MiB
    /// further, which no range bounds). A caller that must not take data
    /// for code takes the whole page, and says so.
    pub unresolved: Vec<u64>,
}

/// The offset into an `ADRP`'s page that `word` adds, if it is an `ADD`
/// (immediate, 64-bit) or an unsigned-offset load or store whose base is
/// `register`.
fn page_offset(word: u32, register: u32) -> Option<u64> {
    if (word >> 5) & 0x1F != register {
        return None;
    }
    if word & 0xFF80_0000 == 0x9100_0000 {
        // ADD Xd, Xn, #imm12 {, LSL #12}: sh (22), imm12 (21:10).
        let imm = u64::from((word >> 10) & 0xFFF);
        return Some(if (word >> 22) & 1 == 1 {
            imm << 12
        } else {
            imm
        });
    }
    if word & 0x3B00_0000 == 0x3900_0000 {
        // LDR/STR (unsigned immediate): size (31:30), V (26), opc (23:22),
        // imm12 (21:10), scaled by the access size; a 128-bit SIMD access
        // is size 00 with opc 1x.
        let size = word >> 30;
        let scale = if (word >> 26) & 1 == 1 && size == 0 && (word >> 23) & 1 == 1 {
            4
        } else {
            size
        };
        return Some(u64::from((word >> 10) & 0xFFF) << scale);
    }
    None
}

/// Whether `word` writes `register`, as far as the shapes an `ADRP` pairing
/// must not look past: a `MOV`, an `ADD`/`SUB` or another `ADRP` or `ADR`
/// into it, or a load into it. An instruction this does not recognise is
/// taken not to write it, which only widens the pairing, and the pairing's
/// result is a data address, so a mistaken one costs coverage, never
/// safety.
fn writes(word: u32, register: u32) -> bool {
    if word & 0x1F != register {
        return false;
    }
    // ADR and ADRP; ADD/SUB (immediate and shifted register); MOV (wide
    // immediate) and ORR (shifted register), which `MOV` register is; and
    // the loads, whose destination is also bits 4:0. A store in those two
    // classes reads that register instead (`STR Xt, [SP]` spilling the
    // page), and `PRFM` loads nothing, so neither ends a pairing.
    let loads = |opc: u32| opc != 0b00 && opc != 0b11;
    word & 0x1F00_0000 == 0x1000_0000
        || word & 0x1F00_0000 == 0x1100_0000
        || word & 0x1F00_0000 == 0x0B00_0000
        || word & 0x1F80_0000 == 0x1280_0000
        || word & 0x1F00_0000 == 0x0A00_0000
        || (word & 0x3B00_0000 == 0x3900_0000 && (word >> 22) & 1 == 1)
        || (word & 0x3B00_0000 == 0x1800_0000 && loads(word >> 30))
}

/// Every direct branch of `words` (at `base`), as (its address, its target).
pub fn direct_branches(words: &[u32], base: u64) -> Vec<(u64, u64)> {
    words
        .iter()
        .enumerate()
        .filter_map(|(index, word)| {
            let pc = base.wrapping_add(4 * index as u64);
            Some((pc, flow(*word, pc).1?))
        })
        .collect()
}

/// The bytes a function may hold as data, where only the unwind table tells
/// code from data: from each address the code addresses as data
/// (`starts`, [`addressed_starts`]) inside a function (`functions`, the
/// FDEs' ranges, sorted) to the next instruction a branch reaches
/// (`branches`, as [`direct_branches`] gives them), or the function's end.
/// Control does not run into a table, so the code after one is reached by a
/// branch, and only a branch that is not itself inside a table counts: a
/// table word can decode as one. An address at a function's entry is a
/// pointer to the function, not data. The ranges come back sorted and merged.
pub fn data_extents(
    starts: &[u64],
    functions: &[(u64, u64)],
    branches: &[(u64, u64)],
) -> Vec<(u64, u64)> {
    // FDE ranges may nest: a short range inside a longer one does not end
    // the data at its own end, so every containing range contributes an
    // extent and the merge below keeps the longest.
    let mut extents: Vec<(u64, u64)> = starts
        .iter()
        .flat_map(|start| {
            let start = *start;
            let after = functions.partition_point(|(entry, _)| *entry <= start);
            functions[..after]
                .iter()
                .filter(move |(entry, end)| *entry < start && start < *end)
                .map(move |(_, end)| (start, *end))
        })
        .collect();
    extents.sort_unstable();
    extents.dedup();
    // Only a branch whose target lies in an extent can end one, and an
    // extent only shrinks, so the rest are dropped once: on a large
    // stripped library this is thousands of branches instead of millions,
    // round after round, for the same result.
    let initial = merge(extents.clone());
    let reaches = |addr: u64| {
        let after = initial.partition_point(|(start, _)| *start <= addr);
        after.checked_sub(1).is_some_and(|i| addr < initial[i].1)
    };
    let branches: Vec<(u64, u64)> = branches
        .iter()
        .filter(|(_, target)| reaches(*target))
        .copied()
        .collect();
    // Each extent ends at the first target past its start of a branch
    // outside every extent. Extents only shrink, and a branch they no
    // longer hold can only shrink them further, so this ends.
    loop {
        let merged = merge(extents.clone());
        let inside = |addr: u64| {
            let after = merged.partition_point(|(start, _)| *start <= addr);
            after.checked_sub(1).is_some_and(|i| addr < merged[i].1)
        };
        let mut targets: Vec<u64> = branches
            .iter()
            .filter(|(at, _)| !inside(*at))
            .map(|(_, target)| *target)
            .collect();
        targets.sort_unstable();
        let mut changed = false;
        for (start, end) in &mut extents {
            let next = targets.partition_point(|target| *target <= *start);
            if let Some(target) = targets.get(next).filter(|target| **target < *end) {
                *end = *target;
                changed = true;
            }
        }
        if !changed {
            return merged;
        }
    }
}

/// `spans` sorted and with overlapping or touching ones joined.
fn merge(mut spans: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    spans.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::new();
    for (start, end) in spans {
        match out.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => out.push((start, end)),
        }
    }
    out
}

/// One mapping symbol: where in which section code or data starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MappingSymbol {
    /// The index of the section it marks.
    pub section: usize,
    /// The address it marks: the symbol's value, which in an executable or
    /// shared object is a virtual address.
    pub address: u64,
    /// `$x`, code; `$d`, data.
    pub code: bool,
}

/// What a mapping symbol's name says: `Some(true)` for `$x` or `$x.<any>`,
/// `Some(false)` for `$d` or `$d.<any>`, `None` for any other name.
fn mapping_kind(name: &[u8]) -> Option<bool> {
    let code = match name.get(..2)? {
        b"$x" => true,
        b"$d" => false,
        _ => return None,
    };
    matches!(name.get(2), None | Some(b'.')).then_some(code)
}

/// Where an ELF64 little-endian file's symbol table and the string table it
/// names are, as file ranges (offset, size), from its section header table.
/// `None` when it has no symbol table (a stripped file) or the table's
/// string table is not one.
pub fn symbol_table(table: &[u8]) -> Option<((u64, u64), (u64, u64))> {
    let entries = table.as_chunks::<SECTION_HEADER_SIZE>().0;
    let kind =
        |entry: &[u8; SECTION_HEADER_SIZE]| u32::from_le_bytes(entry[4..8].try_into().unwrap());
    let range = |entry: &[u8; SECTION_HEADER_SIZE]| {
        (
            u64::from_le_bytes(entry[24..32].try_into().unwrap()),
            u64::from_le_bytes(entry[32..40].try_into().unwrap()),
        )
    };
    let symtab = entries.iter().find(|entry| kind(entry) == SHT_SYMTAB)?;
    let link = u32::from_le_bytes(symtab[40..44].try_into().unwrap());
    let strtab = entries
        .get(usize::try_from(link).ok()?)
        .filter(|entry| kind(entry) == SHT_STRTAB)?;
    Some((range(symtab), range(strtab)))
}

/// The local, untyped symbols of a symbol table that mark a section, given
/// the string table it names, as (section index, address, name). A mapping
/// symbol is one of them ("ELF for the Arm 64-bit Architecture", 5.5.4: they
/// are `STT_NOTYPE` and `STB_LOCAL`, and the `$` names are reserved for local
/// symbols). A global symbol whose name begins `$x` is an ordinary label:
/// taking it for a mapping symbol would let one inside a `$d` table start a
/// code range there.
fn local_untyped_symbols<'a>(
    symtab: &'a [u8],
    strtab: &'a [u8],
) -> impl Iterator<Item = (usize, u64, &'a [u8])> + 'a {
    symtab
        .as_chunks::<SYMBOL_SIZE>()
        .0
        .iter()
        .filter_map(move |symbol| {
            let name =
                usize::try_from(u32::from_le_bytes(symbol[0..4].try_into().unwrap())).ok()?;
            let section = u16::from_le_bytes(symbol[6..8].try_into().unwrap());
            if symbol[4] & 0xf != STT_NOTYPE
                || symbol[4] >> 4 != STB_LOCAL
                || section == 0
                || section >= SHN_LORESERVE
            {
                return None;
            }
            let name = strtab.get(name..)?;
            let name = &name[..name.iter().position(|byte| *byte == 0)?];
            Some((
                usize::from(section),
                u64::from_le_bytes(symbol[8..16].try_into().unwrap()),
                name,
            ))
        })
}

/// The mapping symbols of a symbol table, given the string table it names:
/// the local, untyped symbols whose names are `$x` or `$d`, sorted by
/// section and address. Where a `$x` and a `$d` mark the same
/// address, the `$d` sorts last and so wins: a word that may be data is not
/// taken for code.
pub fn mapping_symbols(symtab: &[u8], strtab: &[u8]) -> Vec<MappingSymbol> {
    let mut symbols: Vec<MappingSymbol> = local_untyped_symbols(symtab, strtab)
        .filter_map(|(section, address, name)| {
            Some(MappingSymbol {
                section,
                address,
                code: mapping_kind(name)?,
            })
        })
        .collect();
    symbols.sort_by_key(|symbol| (symbol.section, symbol.address, !symbol.code));
    symbols
}

/// The local, untyped symbols named like mapping symbols (a `$` name) that
/// are neither `$x` nor `$d`, with or without a `.suffix`, as (section index,
/// name), in table order: AArch32's `$a` and `$t`, and any other `$` name,
/// such as `$xyz` or `$dx`. They are reserved for mapping symbols, so this
/// reader cannot say where they put code or data, and a section that has one
/// is not proved by its mapping symbols: a `$a` after a `$x` would otherwise
/// leave the code range running through it.
pub fn unsupported_mapping_symbols(symtab: &[u8], strtab: &[u8]) -> Vec<(usize, String)> {
    local_untyped_symbols(symtab, strtab)
        .filter(|(_, _, name)| name.first() == Some(&b'$') && mapping_kind(name).is_none())
        .map(|(section, _, name)| (section, String::from_utf8_lossy(name).into_owned()))
        .collect()
}

/// The code of section `section`, which starts at `address` and is `size`
/// bytes long, as address ranges, given `symbols` sorted as
/// [`mapping_symbols`] returns them: each `$x` to the next mapping symbol of
/// the section, or to its end. Bytes before the section's first mapping
/// symbol are not code, and a mark outside the section is ignored. `None`
/// when the section has no mapping symbol, so its code cannot be told from
/// its data.
pub fn code_spans(
    symbols: &[MappingSymbol],
    section: usize,
    address: u64,
    size: u64,
) -> Option<Vec<(u64, u64)>> {
    let end = address.saturating_add(size);
    // A mark outside its own section's bytes says nothing of them.
    let marks: Vec<&MappingSymbol> = symbols
        .iter()
        .filter(|symbol| symbol.section == section && (address..end).contains(&symbol.address))
        .collect();
    if marks.is_empty() {
        return None;
    }
    let mut spans: Vec<(u64, u64)> = Vec::new();
    for (i, mark) in marks.iter().enumerate() {
        if !mark.code {
            continue;
        }
        let start = mark.address;
        let stop = marks.get(i + 1).map_or(end, |next| next.address);
        if start == stop {
            continue;
        }
        // Adjacent `$x` marks continue one span.
        match spans.last_mut() {
            Some(last) if last.1 == start => last.1 = stop,
            _ => spans.push((start, stop)),
        }
    }
    Some(spans)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// An ELF64 symbol table entry.
    pub(crate) fn symbol(name: u32, info: u8, section: u16, value: u64) -> [u8; SYMBOL_SIZE] {
        let mut entry = [0u8; SYMBOL_SIZE];
        entry[0..4].copy_from_slice(&name.to_le_bytes());
        entry[4] = info;
        entry[6..8].copy_from_slice(&section.to_le_bytes());
        entry[8..16].copy_from_slice(&value.to_le_bytes());
        entry
    }

    /// An unwind table at `address` with one CIE (augmentation "zR",
    /// pc-relative signed 4-byte pointers) and an FDE for each function
    /// (start, end), ended by a zero terminator.
    pub(crate) fn eh_frame(address: u64, functions: &[(u64, u64)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        // CIE: length 16; ID 0; version 1; "zR"; code alignment 4; data
        // alignment -8; return register 30; augmentation data: 0x1b; nops.
        bytes.extend(16u32.to_le_bytes());
        bytes.extend([
            0, 0, 0, 0, 1, b'z', b'R', 0, 0x04, 0x78, 0x1e, 0x01, 0x1b, 0, 0, 0,
        ]);
        for (start, end) in functions {
            let at = bytes.len();
            bytes.extend(16u32.to_le_bytes());
            // The CIE pointer: from this field back to the CIE at 0.
            bytes.extend(((at + 4) as u32).to_le_bytes());
            let here = address + (at + 8) as u64;
            bytes.extend((start.wrapping_sub(here) as u32).to_le_bytes());
            bytes.extend(((end - start) as u32).to_le_bytes());
            bytes.extend([0, 0, 0, 0]); // no augmentation data; nops
        }
        bytes.extend(0u32.to_le_bytes());
        bytes
    }

    #[test]
    fn the_unwind_tables_fdes_give_the_code_ranges() {
        let table = eh_frame(
            0x2000,
            &[(0x1400, 0x1480), (0x1000, 0x1100), (0x1100, 0x1180)],
        );
        assert_eq!(
            fde_ranges(&table, 0x2000),
            Some(vec![(0x1000, 0x1100), (0x1100, 0x1180), (0x1400, 0x1480)])
        );
        // Read at another address, the pc-relative starts move with it.
        assert_eq!(
            fde_ranges(&table, 0x3000),
            Some(vec![(0x2000, 0x2100), (0x2100, 0x2180), (0x2400, 0x2480)])
        );
        assert_eq!(fde_ranges(&eh_frame(0x2000, &[]), 0x2000), Some(vec![]));
        // A record that runs past the table, and an encoding this reader
        // does not take (data-relative), give no evidence.
        let mut truncated = table.clone();
        truncated.truncate(30);
        assert_eq!(fde_ranges(&truncated, 0x2000), None);
        let mut datarel = table.clone();
        datarel[16] = 0x3b;
        assert_eq!(fde_ranges(&datarel, 0x2000), None);
        // A C++ CIE ("zPLR") whose personality pointer is indirect: stepped
        // over, as only the FDE pointer encoding matters.
        let mut cxx = Vec::new();
        cxx.extend(24u32.to_le_bytes());
        cxx.extend([
            0, 0, 0, 0, 1, b'z', b'P', b'L', b'R', 0, 0x04, 0x78, 0x1e, 0x07,
        ]);
        cxx.extend([0x9b, 0, 0, 0, 0, 0x1b, 0x1b, 0, 0, 0]);
        let at = cxx.len();
        cxx.extend(20u32.to_le_bytes());
        cxx.extend(((at + 4) as u32).to_le_bytes());
        let here = 0x2000 + (at + 8) as u64;
        cxx.extend((0x1000u64.wrapping_sub(here) as u32).to_le_bytes());
        cxx.extend(0x40u32.to_le_bytes());
        cxx.extend([4, 0, 0, 0, 0, 0, 0, 0]); // LSDA pointer, nops
        cxx.extend(0u32.to_le_bytes());
        assert_eq!(fde_ranges(&cxx, 0x2000), Some(vec![(0x1000, 0x1040)]));
        // A CIE of a version `.eh_frame` does not use.
        for unused in [2, 4] {
            // Version 4 adds address and segment sizes this reader does not
            // read, so it is refused rather than misread.
            let mut version = table.clone();
            version[8] = unused;
            assert_eq!(fde_ranges(&version, 0x2000), None, "version {unused}");
        }
        // An FDE whose CIE pointer leads to something that is not a CIE.
        let mut astray = table;
        astray[24..28].copy_from_slice(&4u32.to_le_bytes());
        assert_eq!(fde_ranges(&astray, 0x2000), None);
    }

    #[test]
    fn code_is_what_control_reaches_from_the_entries() {
        let words = [
            0x1400_0002, // 0x100: b 0x108
            0x1122_3344, // 0x104: data, branched over
            0x9400_0010, // 0x108: bl elsewhere (returns)
            0xd63f_0000, // 0x10c: blr x0 (returns)
            0x3400_0040, // 0x110: cbz w0, 0x118
            0xd61f_0000, // 0x114: br x0 (does not)
            0xd65f_03c0, // 0x118: ret
            0x5566_7788, // 0x11c: data after the return
            0xd503_201f, // 0x120: a function no branch reaches
            0x0000_0000, // 0x124: udf
            0xd503_201f, // 0x128: after the trap
        ];
        let covered = [(0x100, 0x12c)];
        assert_eq!(
            reachable_code(&words, 0x100, &covered, &[0x100], &|_| true),
            vec![(0x100, 0x104), (0x108, 0x11c)]
        );
        // Its own FDE's entry makes the last function code.
        assert_eq!(
            reachable_code(&words, 0x100, &covered, &[0x100, 0x120], &|_| true),
            vec![(0x100, 0x104), (0x108, 0x11c), (0x120, 0x128)]
        );
        // Nothing outside the covered ranges is reached, entries included.
        assert_eq!(
            reachable_code(&words, 0x100, &[(0x100, 0x110)], &[0x100, 0x120], &|_| true),
            vec![(0x100, 0x104), (0x108, 0x110)]
        );
    }

    /// A branch whose outcome the encoding fixes has only the edge it takes.
    #[test]
    fn always_and_never_taken_branches_have_one_edge() {
        for (branch, falls_through, targets) in [
            (0x5400_004e_u32, false, true), // b.al +8
            (0x5400_004f, false, true),     // b.nv +8
            (0x5400_0040, true, true),      // b.eq +8
            (0x3400_005f, false, true),     // cbz wzr, +8
            (0x3500_005f, true, false),     // cbnz wzr, +8
            (0x3400_0040, true, true),      // cbz w0, +8
            (0x3600_005f, false, true),     // tbz wzr, #0, +8
            (0x3700_005f, true, false),     // tbnz wzr, #0, +8
            (0x3600_0040, true, true),      // tbz w0, #0, +8
        ] {
            assert_eq!(
                flow(branch, 0x100),
                (falls_through, targets.then_some(0x108)),
                "{branch:#010x}"
            );
        }
    }

    #[test]
    fn literal_loads_and_adr_name_the_bytes_they_reach() {
        let words = [
            0x5800_0101, // ldr x1, #+0x20: 8 bytes
            0x1800_0101, // ldr w1, #+0x20: 4 bytes
            0x9c00_0101, // ldr q1, #+0x20: 16 bytes
            0x9800_0101, // ldrsw x1, #+0x20: 4 bytes
            0xd800_0101, // prfm, #+0x20: loads nothing
            0x58ff_ffc1, // ldr x1, #-0x8
            0x1000_0080, // adr x0, #+0x10
            0x3000_0020, // adr x0, #+0x5
            0x9000_0000, // adrp x0, 0: a page, not a literal
            0xd503_201f, // nop
        ];
        assert_eq!(
            literal_targets(&words, 0x1000),
            vec![
                (0x1020, 0x1028),
                (0x1024, 0x1028),
                (0x1028, 0x1038),
                (0x102c, 0x1030),
                (0x100c, 0x1014),
                (0x1028, 0x102c),
                (0x1021, 0x1025),
            ]
        );
    }

    #[test]
    fn an_adr_or_an_adrp_pair_names_where_data_may_start() {
        let words = [
            0x1000_0080, // adr x0, #+0x10
            0x5800_0101, // ldr x1, #+0x20
            0x9000_0001, // adrp x1, 0x1000 (the page of 0x1008)
            0x9105_d021, // add x1, x1, #0x174
            0x9000_0002, // adrp x2, 0x1000, completed by a scaled load
            0xf944_0c43, // ldr x3, [x2, #0x818]
            0x9000_0004, // adrp x4, 0x1000, never completed
            0xd503_201f, // nop
        ];
        let addressed = addressed_starts(&words, 0x1000);
        assert_eq!(addressed.starts, vec![0x1010, 0x1024, 0x1174, 0x1818]);
        // The last `ADRP` is completed by nothing, so its target is
        // unknown and the caller is told where it is.
        assert_eq!(addressed.unresolved, vec![0x1000], "the page it names");
        // The completing instruction must take the ADRP's own register, and
        // is looked for in the eight instructions after it.
        let other = [0x9000_0001, 0x9105_d042]; // adrp x1; add x2, x2, #0x174
        assert_eq!(addressed_starts(&other, 0x1000).unresolved, vec![0x1000]);
        assert!(addressed_starts(&other, 0x1000).starts.is_empty());
        let far: Vec<u32> = std::iter::once(0x9000_0001)
            .chain(std::iter::repeat_n(0xd503_201f, 8))
            .chain(std::iter::once(0x9105_d021))
            .collect();
        assert_eq!(addressed_starts(&far, 0x1000).unresolved, vec![0x1000]);
    }

    #[test]
    fn an_adrp_pairing_ends_where_its_register_is_written() {
        // adrp x1, 0x1000; mov x1, #0 (the page is gone); add x1, x1, #0x40
        // completes nothing of the ADRP.
        let overwritten = [0x9000_0001, 0xd280_0001, 0x9101_0021];
        assert_eq!(
            addressed_starts(&overwritten, 0x1000).unresolved,
            vec![0x1000]
        );
        // The same without the write: the ADD completes it.
        let paired = [0x9000_0001, 0xd503_201f, 0x9101_0021];
        assert_eq!(addressed_starts(&paired, 0x1000).starts, vec![0x1040]);
        // A write to another register says nothing.
        let other = [0x9000_0001, 0xd280_0002, 0x9101_0021];
        assert_eq!(addressed_starts(&other, 0x1000).starts, vec![0x1040]);
        // The completing instruction may be the one that writes the
        // register, as `ADD Xn, Xn, #imm` and a load through it are.
        let load = [0x9000_0001, 0xf940_0421];
        assert_eq!(addressed_starts(&load, 0x1000).starts, vec![0x1008]);
    }

    /// A store through the `ADRP`'s register, or of it, reads the register
    /// rather than writing it, so the pairing runs on: spilling the page to
    /// the stack between the `ADRP` and its `ADD` must not lose the table.
    #[test]
    fn a_store_does_not_end_an_adrp_pairing() {
        for store in [
            0xf900_03e1_u32, // str x1, [sp]: the page spilled, x1 unchanged
            0xd503_201f,     // nop, as the control
        ] {
            let words = [0x9000_0001, store, 0x9101_0021];
            assert_eq!(
                addressed_starts(&words, 0x1000).starts,
                vec![0x1040],
                "{store:#010x}"
            );
        }
        // A store through the register addresses those bytes as well, and
        // the pairing still reaches the `ADD` after it.
        let through = [0x9000_0001, 0xf900_0021, 0x9101_0021];
        assert_eq!(
            addressed_starts(&through, 0x1000).starts,
            vec![0x1000, 0x1040]
        );
        // A load into the register ends the pairing: the page is gone.
        let load = [0x9000_0001, 0xf940_03e1, 0x9101_0021];
        assert_eq!(addressed_starts(&load, 0x1000).unresolved, vec![0x1000]);
    }

    /// An unresolved `ADRP` is not a data start: its page is handed to
    /// the caller, which takes the functions the page overlaps.
    #[test]
    fn an_uncompleted_adrp_is_not_a_start() {
        let words = [0x9000_0001, 0xd503_201f];
        let addressed = addressed_starts(&words, 0x1000);
        assert!(addressed.starts.is_empty());
        assert_eq!(addressed.unresolved, vec![0x1000]);
        assert_eq!(
            data_extents(&addressed.starts, &[(0x1100, 0x1180)], &[]),
            vec![]
        );
    }

    #[test]
    fn data_runs_from_where_it_is_addressed_to_the_next_branch_target() {
        let functions = [(0x100, 0x180), (0x200, 0x240)];
        // 0x140 is addressed inside the first function: data from there to
        // the branch target 0x160, which is code again.
        let branches = [(0x108, 0x160)];
        assert_eq!(
            data_extents(&[0x140], &functions, &branches),
            vec![(0x140, 0x160)]
        );
        // With no branch past it, the data runs to the function's end.
        assert_eq!(
            data_extents(&[0x140], &functions, &[]),
            vec![(0x140, 0x180)]
        );
        // A branch from inside an extent is not evidence: a table word can
        // decode as one. Here the only branch to 0x160 is at 0x150, in the
        // data, so the extent still runs to the end.
        assert_eq!(
            data_extents(&[0x140], &functions, &[(0x150, 0x160)]),
            vec![(0x140, 0x180)]
        );
        // A function's entry is a pointer to it, not data; an address in no
        // function (a global) says nothing.
        assert_eq!(data_extents(&[0x100, 0x2000], &functions, &[]), vec![]);
        // Overlapping extents are merged, and each ends at its own next
        // target.
        assert_eq!(
            data_extents(&[0x140, 0x148, 0x220], &functions, &[(0x108, 0x150)]),
            vec![(0x140, 0x150), (0x220, 0x240)]
        );
        // Nested FDEs: 0x134 is inside both the enclosing range and a
        // short nested one. The data runs to the enclosing range's end,
        // not the nested one's, so a candidate at 0x140 stays excluded.
        let nested = [(0x100, 0x180), (0x130, 0x138)];
        assert_eq!(data_extents(&[0x134], &nested, &[]), vec![(0x134, 0x180)]);
    }

    #[test]
    fn code_is_what_the_mapping_symbols_mark_or_else_what_the_fdes_cover() {
        let mark = |section, address, code| MappingSymbol {
            section,
            address,
            code,
        };
        let evidence = CodeEvidence {
            symbols: vec![mark(1, 0x140, true), mark(1, 0x180, false)],
            unsupported: Vec::new(),
            unwind: Some(vec![(0x80, 0x180), (0x400, 0x440)]),
        };
        // Mapping symbols: the bytes before the first mark are unknown, a
        // `$d` range is data, and the FDEs are not consulted.
        assert_eq!(
            evidence.classify(1, 0x100, 0x100),
            SectionCode {
                code: vec![(0x140, 0x180)],
                unknown: vec![(0x100, 0x140)],
                evidence: Some(Evidence::MappingSymbols),
            }
        );
        // No mapping symbols: the FDEs' ranges, and what they leave unknown.
        assert_eq!(
            evidence.classify(2, 0x3c0, 0x100),
            SectionCode {
                code: vec![(0x400, 0x440)],
                unknown: vec![(0x3c0, 0x400), (0x440, 0x4c0)],
                evidence: Some(Evidence::UnwindTable),
            }
        );
        // Neither: all unknown.
        let symbols_only = CodeEvidence {
            unwind: None,
            ..evidence
        };
        assert_eq!(
            symbols_only.classify(2, 0x400, 0x100),
            SectionCode {
                code: vec![],
                unknown: vec![(0x400, 0x500)],
                evidence: None,
            }
        );
    }

    #[test]
    fn only_mapping_symbol_names_are_mapping_symbols() {
        assert_eq!(mapping_kind(b"$x"), Some(true));
        assert_eq!(mapping_kind(b"$x.42"), Some(true));
        assert_eq!(mapping_kind(b"$d"), Some(false));
        assert_eq!(mapping_kind(b"$d.realdata"), Some(false));
        for other in [&b"$a"[..], b"$t", b"$xyz", b"$", b"x", b"", b"main", b"$dx"] {
            assert_eq!(mapping_kind(other), None, "{other:?}");
        }
    }

    #[test]
    fn the_symbol_table_is_found_through_its_linked_string_table() {
        let header = |kind: u32, offset: u64, size: u64, link: u32| {
            let mut entry = [0u8; SECTION_HEADER_SIZE];
            entry[4..8].copy_from_slice(&kind.to_le_bytes());
            entry[24..32].copy_from_slice(&offset.to_le_bytes());
            entry[32..40].copy_from_slice(&size.to_le_bytes());
            entry[40..44].copy_from_slice(&link.to_le_bytes());
            entry
        };
        let table = |entries: &[[u8; SECTION_HEADER_SIZE]]| entries.concat();
        // null, .text, .symtab -> 3, .strtab
        let good = table(&[
            header(0, 0, 0, 0),
            header(1, 0x100, 0x40, 0),
            header(SHT_SYMTAB, 0x200, 0x48, 3),
            header(SHT_STRTAB, 0x300, 0x10, 0),
        ]);
        assert_eq!(symbol_table(&good), Some(((0x200, 0x48), (0x300, 0x10))));
        // Stripped: no SHT_SYMTAB, only the dynamic symbols (SHT_DYNSYM = 11).
        let stripped = table(&[
            header(0, 0, 0, 0),
            header(11, 0x200, 0x48, 2),
            header(3, 0, 0, 0),
        ]);
        assert_eq!(symbol_table(&stripped), None);
        // A link past the table, or to a section that is not a string table.
        let dangling = table(&[header(0, 0, 0, 0), header(SHT_SYMTAB, 0x200, 0x48, 9)]);
        assert_eq!(symbol_table(&dangling), None);
        let not_strings = table(&[header(0, 0, 0, 0), header(SHT_SYMTAB, 0x200, 0x48, 0)]);
        assert_eq!(symbol_table(&not_strings), None);
    }

    #[test]
    fn mapping_symbols_are_read_sorted_with_data_winning_a_tie() {
        let strtab = b"\0$x\0$d\0main\0$x.1\0$d.q\0";
        let (x, d, main, x1, dq) = (1, 4, 7, 12, 17);
        let symtab = [
            symbol(0, 0, 0, 0),
            symbol(d, 0, 2, 0x180),
            symbol(main, 0x12, 2, 0x100), // STT_FUNC, global: not a mapping symbol
            symbol(x, 0, 2, 0x100),
            symbol(x1, 0, 2, 0x200),
            symbol(dq, 0, 2, 0x200),     // ties with $x.1 at 0x200 and wins
            symbol(x, 0, 0, 0x300),      // SHN_UNDEF
            symbol(x, 0, 0xfff1, 0x300), // SHN_ABS
            symbol(x, 0x10, 1, 0x40),    // STB_GLOBAL: an ordinary label, not a mark
            symbol(x, 0x10, 2, 0x190),   // the same inside section 2's $d range
            symbol(99, 0, 2, 0x100),     // name past the string table
        ]
        .concat();
        let symbols = mapping_symbols(&symtab, strtab);
        let at = |section, address, code| MappingSymbol {
            section,
            address,
            code,
        };
        assert_eq!(
            symbols,
            vec![
                at(2, 0x100, true),
                at(2, 0x180, false),
                at(2, 0x200, true),
                at(2, 0x200, false),
            ]
        );
        // The global `$x` inside the `$d` range does not split it.
        assert_eq!(
            code_spans(&symbols, 2, 0x100, 0x100),
            Some(vec![(0x100, 0x180)])
        );
    }

    /// A local, untyped `$` symbol that is neither `$x` nor `$d` (with or
    /// without a `.suffix`) is collected as unsupported, by section: AArch32's
    /// `$a` and `$t`, an unknown `$q`, and `$xyz` or `$dx`, which only look
    /// like `$x` and `$d`. `$x.foo` and `$d.1` are mapping symbols, and a
    /// global `$a` is an ordinary label.
    #[test]
    fn mapping_symbols_this_reader_does_not_know_are_collected() {
        let strtab = b"\0$x\0$a\0$t\0$q\0$xyz\0$dx\0$x.foo\0$d.1\0main\0";
        let (x, a, t, q, xyz, dx, xfoo, d1, main) = (1, 4, 7, 10, 13, 18, 22, 29, 34);
        let symtab = [
            symbol(0, 0, 0, 0),
            symbol(x, 0, 2, 0x100),
            symbol(a, 0, 2, 0x110),
            symbol(t, 0, 3, 0x200),
            symbol(q, 0, 4, 0x300),
            symbol(xyz, 0, 5, 0x400),
            symbol(dx, 0, 6, 0x500),
            symbol(xfoo, 0, 7, 0x600),
            symbol(d1, 0, 7, 0x610),
            symbol(main, 0, 8, 0x700),
            symbol(a, 0x10, 9, 0x800), // STB_GLOBAL
        ]
        .concat();
        assert_eq!(
            unsupported_mapping_symbols(&symtab, strtab),
            vec![
                (2, "$a".to_owned()),
                (3, "$t".to_owned()),
                (4, "$q".to_owned()),
                (5, "$xyz".to_owned()),
                (6, "$dx".to_owned()),
            ]
        );
        assert_eq!(
            mapping_symbols(&symtab, strtab)
                .iter()
                .map(|symbol| symbol.section)
                .collect::<Vec<_>>(),
            vec![2, 7, 7]
        );
    }

    #[test]
    fn code_runs_from_each_x_to_the_next_mapping_symbol() {
        let at = |section, address, code| MappingSymbol {
            section,
            address,
            code,
        };
        let symbols = [
            at(1, 0x40, true),
            at(2, 0x110, true),
            at(2, 0x120, true),
            at(2, 0x140, false),
            at(2, 0x180, true),
            at(2, 0x1c0, true),
            at(2, 0x1c0, false),
            at(3, 0x400, false),
        ];
        // Section 2 spans 0x100..0x200: the bytes before its first mark are
        // not code, adjacent $x marks continue a span, a $d ends one, and a
        // $d at the same address as a $x wins.
        assert_eq!(
            code_spans(&symbols, 2, 0x100, 0x100),
            Some(vec![(0x110, 0x140), (0x180, 0x1c0)])
        );
        // A section that is all data has no code; one with no mark is unknown.
        assert_eq!(code_spans(&symbols, 3, 0x400, 0x40), Some(vec![]));
        assert_eq!(code_spans(&symbols, 4, 0x500, 0x40), None);
        // A mark outside its section says nothing of it.
        assert_eq!(code_spans(&symbols, 1, 0x80, 0x20), None);
    }
}
