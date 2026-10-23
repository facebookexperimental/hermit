/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Minimal Detcore preload discovery, separate from full artifact qualification.

use std::io;
use std::ops::Range;

use goblin::elf::Elf;
use goblin::elf::dynamic;
use goblin::elf::header;
use goblin::elf::program_header;
use goblin::elf::reloc;
use goblin::elf::section_header;
use goblin::elf::sym;

pub(crate) const ROOT: &str = "detcore_liteinst_initialize";

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn file_span(bytes: &[u8], offset: u64, size: u64) -> io::Result<Range<usize>> {
    let start = usize::try_from(offset).map_err(|_| invalid("file extent offset overflow"))?;
    let size = usize::try_from(size).map_err(|_| invalid("file extent size overflow"))?;
    let end = start
        .checked_add(size)
        .ok_or_else(|| invalid("file extent end overflow"))?;
    bytes
        .get(start..end)
        .ok_or_else(|| invalid("file extent is truncated"))?;
    Ok(start..end)
}

fn loaded_file_span(
    bytes: &[u8],
    elf: &Elf<'_>,
    address: u64,
    size: u64,
) -> io::Result<Range<usize>> {
    let end = address
        .checked_add(size)
        .ok_or_else(|| invalid("loaded extent end overflow"))?;
    let mut found = None;
    for segment in &elf.program_headers {
        if segment.p_type != program_header::PT_LOAD {
            continue;
        }
        let top = segment
            .p_vaddr
            .checked_add(segment.p_filesz)
            .ok_or_else(|| invalid("loaded file extent overflow"))?;
        if address < segment.p_vaddr || end > top {
            continue;
        }
        if size == 0 || segment.p_filesz > segment.p_memsz || found.is_some() {
            return Err(invalid("loaded file extent is empty or ambiguous"));
        }
        let offset = segment
            .p_offset
            .checked_add(address - segment.p_vaddr)
            .ok_or_else(|| invalid("loaded file offset overflow"))?;
        found = Some(file_span(bytes, offset, size)?);
    }
    found.ok_or_else(|| invalid("loaded extent has no complete file backing"))
}

fn tag(elf: &Elf<'_>, wanted: u64) -> io::Result<Option<u64>> {
    let dynamic = elf
        .dynamic
        .as_ref()
        .ok_or_else(|| invalid("Detcore has no dynamic table"))?;
    let mut entries = dynamic.dyns.iter().filter(|entry| entry.d_tag == wanted);
    let value = entries.next().map(|entry| entry.d_val);
    if entries.next().is_some() {
        return Err(invalid("duplicate constructor dynamic tag"));
    }
    Ok(value)
}

// x86-64 ABI write widths. COPY and unknown relocation operations cannot prove
// a sole constructor writer and are unsupported in this discovery branch.
fn relocation_width(kind: u32) -> io::Result<u64> {
    match kind {
        0 | 35 => Ok(0), // NONE, TLSDESC_CALL (no data write)
        1 | 6..=8 | 16..=18 | 24..=25 | 27..=31 | 33 | 37..=38 => Ok(8),
        2..=4 | 9..=11 | 19..=23 | 26 | 32 | 34 | 41..=42 => Ok(4),
        12..=13 => Ok(2),
        14..=15 => Ok(1),
        36 => Ok(16), // TLSDESC
        _ => Err(invalid("unsupported constructor relocation writer")),
    }
}

/// This discovers the strict public binding/registration shape. It does not
/// prove the full constructor bytes, native stack transition, loader layouts,
/// allocator or preparation closure; those require the shared qualifier.
pub(crate) fn validate(bytes: &[u8], elf: &Elf<'_>) -> io::Result<()> {
    if !elf.is_64
        || !elf.little_endian
        || elf.header.e_type != header::ET_DYN
        || elf.header.e_machine != header::EM_X86_64
    {
        return Err(invalid("Detcore is not a little-endian x86-64 DSO"));
    }
    let mut roots = elf
        .dynsyms
        .iter()
        .filter(|symbol| elf.dynstrtab.get_at(symbol.st_name) == Some(ROOT));
    let root = roots
        .next()
        .ok_or_else(|| invalid("Detcore constructor root is absent"))?;
    if roots.next().is_some()
        || root.st_bind() != sym::STB_GLOBAL
        || root.st_visibility() != sym::STV_PROTECTED
        || root.st_type() != sym::STT_FUNC
        || root.st_shndx == section_header::SHN_UNDEF as usize
        || root.st_size != 16
        || root.st_value == 0
    {
        return Err(invalid(
            "Detcore constructor root is not unique strong PROTECTED FUNC16",
        ));
    }
    let root_end = root
        .st_value
        .checked_add(root.st_size)
        .ok_or_else(|| invalid("Detcore constructor root extent overflow"))?;
    let section = elf
        .section_headers
        .get(root.st_shndx)
        .ok_or_else(|| invalid("Detcore constructor root section is absent"))?;
    if section.sh_type != section_header::SHT_PROGBITS
        || section.sh_flags & u64::from(section_header::SHF_EXECINSTR) == 0
        || root.st_value < section.sh_addr
        || section
            .sh_addr
            .checked_add(section.sh_size)
            .is_none_or(|end| root_end > end)
    {
        return Err(invalid(
            "Detcore constructor root has no executable complete section",
        ));
    }
    let root_offset = section
        .sh_offset
        .checked_add(root.st_value - section.sh_addr)
        .ok_or_else(|| invalid("Detcore constructor root file offset overflow"))?;
    if loaded_file_span(bytes, elf, root.st_value, root.st_size)?
        != file_span(bytes, root_offset, root.st_size)?
        || !elf.program_headers.iter().any(|segment| {
            segment.p_type == program_header::PT_LOAD
                && segment.p_flags & (program_header::PF_R | program_header::PF_X)
                    == (program_header::PF_R | program_header::PF_X)
                && segment.p_flags & program_header::PF_W == 0
                && root.st_value >= segment.p_vaddr
                && segment
                    .p_vaddr
                    .checked_add(segment.p_filesz)
                    .is_some_and(|end| root_end <= end)
        })
    {
        return Err(invalid(
            "Detcore constructor root has no unique file-backed RX mapping",
        ));
    }
    let mut dynamics = elf
        .program_headers
        .iter()
        .filter(|segment| segment.p_type == program_header::PT_DYNAMIC);
    let dynamic_segment = dynamics
        .next()
        .ok_or_else(|| invalid("Detcore dynamic mapping is absent"))?;
    if dynamics.next().is_some()
        || dynamic_segment.p_filesz == 0
        || !dynamic_segment.p_filesz.is_multiple_of(16)
        || loaded_file_span(
            bytes,
            elf,
            dynamic_segment.p_vaddr,
            dynamic_segment.p_filesz,
        )? != file_span(bytes, dynamic_segment.p_offset, dynamic_segment.p_filesz)?
    {
        return Err(invalid(
            "Detcore dynamic mapping is ambiguous or physically inconsistent",
        ));
    }
    let mut arrays = elf
        .section_headers
        .iter()
        .filter(|section| section.sh_type == section_header::SHT_INIT_ARRAY);
    let array = arrays
        .next()
        .ok_or_else(|| invalid("Detcore has no constructor array"))?;
    if arrays.next().is_some()
        || array.sh_size == 0
        || !array.sh_addr.is_multiple_of(8)
        || !array.sh_offset.is_multiple_of(8)
        || !array.sh_size.is_multiple_of(8)
        || !matches!(array.sh_entsize, 0 | 8)
        || array.sh_flags & u64::from(section_header::SHF_ALLOC) == 0
    {
        return Err(invalid(
            "Detcore constructor array is empty, ambiguous or unaligned",
        ));
    }
    let end = array
        .sh_addr
        .checked_add(array.sh_size)
        .ok_or_else(|| invalid("Detcore constructor array extent overflow"))?;
    let array_file = file_span(bytes, array.sh_offset, array.sh_size)?;
    if tag(elf, dynamic::DT_INIT_ARRAY)? != Some(array.sh_addr)
        || tag(elf, dynamic::DT_INIT_ARRAYSZ)? != Some(array.sh_size)
        || loaded_file_span(bytes, elf, array.sh_addr, array.sh_size)? != array_file
    {
        return Err(invalid(
            "Detcore constructor dynamic/section/file identity differs",
        ));
    }
    for packed in [35, 36, 37] {
        // DT_RELRSZ, DT_RELR, DT_RELRENT
        if tag(elf, packed)?.is_some() {
            return Err(invalid(
                "packed constructor relocation writers are unsupported",
            ));
        }
    }
    if tag(elf, dynamic::DT_PREINIT_ARRAY)?.is_some_and(|value| value != 0)
        || tag(elf, dynamic::DT_PREINIT_ARRAYSZ)?.is_some_and(|value| value != 0)
        || tag(elf, dynamic::DT_INIT)?
            .is_some_and(|entry| (root.st_value..root_end).contains(&entry))
    {
        return Err(invalid("Detcore has a competing constructor registration"));
    }
    let mut slots = vec![None; array_file.len() / 8];
    for relocation in elf
        .dynrelas
        .iter()
        .chain(elf.dynrels.iter())
        .chain(elf.pltrelocs.iter())
    {
        let width = relocation_width(relocation.r_type)?;
        let writer_end = relocation
            .r_offset
            .checked_add(width)
            .ok_or_else(|| invalid("constructor relocation writer overflow"))?;
        if width == 0 || relocation.r_offset >= end || writer_end <= array.sh_addr {
            continue;
        }
        if width != 8
            || relocation.r_offset < array.sh_addr
            || writer_end > end
            || !(relocation.r_offset - array.sh_addr).is_multiple_of(8)
        {
            return Err(invalid(
                "constructor array has a partial or unaligned writer",
            ));
        }
        if relocation.r_type != reloc::R_X86_64_RELATIVE || relocation.r_sym != 0 {
            return Err(invalid("constructor slot is not symbol-zero RELATIVE"));
        }
        let target = relocation
            .r_addend
            .and_then(|addend| u64::try_from(addend).ok())
            .ok_or_else(|| invalid("constructor slot has no nonnegative RELA addend"))?;
        let index = usize::try_from((relocation.r_offset - array.sh_addr) / 8)
            .map_err(|_| invalid("constructor slot index overflow"))?;
        if slots[index].replace(target).is_some() {
            return Err(invalid("constructor slot has duplicate relocation writers"));
        }
        if target != root.st_value && (root.st_value..root_end).contains(&target) {
            return Err(invalid("constructor slot enters the root interior"));
        }
    }
    if slots.iter().any(Option::is_none) {
        return Err(invalid("constructor slot has no sole RELATIVE relocation"));
    }
    if slots
        .iter()
        .filter(|target| **target == Some(root.st_value))
        .count()
        != 1
    {
        return Err(invalid(
            "Detcore has no unique exact-root RELATIVE constructor slot",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    const ROOT_ADDRESS: u64 = 0x1100;
    const ARRAY_ADDRESS: u64 = 0x2000;
    const DYNAMIC: usize = 0x240;
    const SYMBOL: usize = 0x318;
    const RELA: usize = 0x400;
    const SECTIONS: usize = 0x600;

    fn word(bytes: &mut [u8], offset: usize, value: u16) {
        bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn dword(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn qword(bytes: &mut [u8], offset: usize, value: u64) {
        bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn section(bytes: &mut [u8], index: usize, fields: (u32, u64, u64, u64, u64, u64)) {
        let (kind, flags, address, offset, size, entry_size) = fields;
        let at = SECTIONS + index * 64;
        dword(bytes, at + 4, kind);
        qword(bytes, at + 8, flags);
        qword(bytes, at + 16, address);
        qword(bytes, at + 24, offset);
        qword(bytes, at + 32, size);
        qword(bytes, at + 48, 8);
        qword(bytes, at + 56, entry_size);
    }

    fn image(entries: usize) -> Vec<u8> {
        let mut bytes = vec![0; 0x800];
        bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
        word(&mut bytes, 16, header::ET_DYN);
        word(&mut bytes, 18, header::EM_X86_64);
        dword(&mut bytes, 20, 1);
        qword(&mut bytes, 32, 64);
        qword(&mut bytes, 40, SECTIONS as u64);
        word(&mut bytes, 52, 64);
        word(&mut bytes, 54, 56);
        word(&mut bytes, 56, 3);
        word(&mut bytes, 58, 64);
        word(&mut bytes, 60, 6);
        for (index, kind, flags, offset, address, size) in [
            (0, program_header::PT_LOAD, 5, 0, 0x1000, 0x200),
            (1, program_header::PT_LOAD, 6, 0x200, 0x2000, 0x380),
            (
                2,
                program_header::PT_DYNAMIC,
                6,
                DYNAMIC as u64,
                0x2040,
                176,
            ),
        ] {
            let at = 64 + index * 56;
            dword(&mut bytes, at, kind);
            dword(&mut bytes, at + 4, flags);
            qword(&mut bytes, at + 8, offset);
            qword(&mut bytes, at + 16, address);
            qword(&mut bytes, at + 32, size);
            qword(&mut bytes, at + 40, size);
            qword(&mut bytes, at + 48, 1);
        }
        bytes[0x100..0x110].copy_from_slice(&[0x90; 16]);
        let strings = format!("\0{ROOT}\0");
        bytes[0x360..0x360 + strings.len()].copy_from_slice(strings.as_bytes());
        for (index, (kind, value)) in [
            (dynamic::DT_STRTAB, 0x2160),
            (dynamic::DT_SYMTAB, 0x2100),
            (dynamic::DT_STRSZ, strings.len() as u64),
            (dynamic::DT_SYMENT, 24),
            (dynamic::DT_HASH, 0x21a0),
            (dynamic::DT_RELA, 0x2200),
            (dynamic::DT_RELASZ, entries as u64 * 24),
            (dynamic::DT_RELAENT, 24),
            (dynamic::DT_INIT_ARRAY, ARRAY_ADDRESS),
            (dynamic::DT_INIT_ARRAYSZ, entries as u64 * 8),
            (dynamic::DT_NULL, 0),
        ]
        .into_iter()
        .enumerate()
        {
            qword(&mut bytes, DYNAMIC + index * 16, kind);
            qword(&mut bytes, DYNAMIC + index * 16 + 8, value);
        }
        dword(&mut bytes, SYMBOL, 1);
        bytes[SYMBOL + 4] = (sym::STB_GLOBAL << 4) | sym::STT_FUNC;
        bytes[SYMBOL + 5] = sym::STV_PROTECTED;
        word(&mut bytes, SYMBOL + 6, 1);
        qword(&mut bytes, SYMBOL + 8, ROOT_ADDRESS);
        qword(&mut bytes, SYMBOL + 16, 16);
        for (index, value) in [1, 2, 1, 0, 0].into_iter().enumerate() {
            dword(&mut bytes, 0x3a0 + index * 4, value);
        }
        for index in 0..entries {
            qword(
                &mut bytes,
                RELA + index * 24,
                ARRAY_ADDRESS + index as u64 * 8,
            );
            qword(
                &mut bytes,
                RELA + index * 24 + 8,
                u64::from(reloc::R_X86_64_RELATIVE),
            );
            qword(&mut bytes, RELA + index * 24 + 16, ROOT_ADDRESS);
        }
        section(
            &mut bytes,
            1,
            (section_header::SHT_PROGBITS, 6, ROOT_ADDRESS, 0x100, 16, 0),
        );
        section(
            &mut bytes,
            2,
            (
                section_header::SHT_INIT_ARRAY,
                3,
                ARRAY_ADDRESS,
                0x200,
                entries as u64 * 8,
                8,
            ),
        );
        section(
            &mut bytes,
            3,
            (section_header::SHT_DYNSYM, 2, 0x2100, 0x300, 48, 24),
        );
        dword(&mut bytes, SECTIONS + 3 * 64 + 40, 4);
        section(
            &mut bytes,
            4,
            (
                section_header::SHT_STRTAB,
                2,
                0x2160,
                0x360,
                strings.len() as u64,
                0,
            ),
        );
        section(
            &mut bytes,
            5,
            (
                section_header::SHT_RELA,
                2,
                0x2200,
                RELA as u64,
                entries as u64 * 24,
                24,
            ),
        );
        dword(&mut bytes, SECTIONS + 5 * 64 + 40, 3);
        bytes
    }

    fn discover(bytes: &[u8]) -> io::Result<()> {
        let mut file = tempfile::NamedTempFile::new()?;
        file.write_all(bytes)?;
        crate::validate_preload_constructor_library(file.path(), "unit Detcore", &[ROOT], ROOT)
            .map(|_| ())
    }

    fn refused(bytes: &[u8], reason: &str) {
        let error = discover(bytes).expect_err("invalid constructor was discovered");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(
            error.to_string().contains(reason),
            "{error}; expected {reason}"
        );
    }

    #[test]
    fn protected_relative_constructor_with_zero_raw_slot_is_discovered() {
        let bytes = image(1);
        assert_eq!(&bytes[0x200..0x208], &[0; 8]);
        discover(&bytes).expect("local RELATIVE addend must be resolved without raw pointers");
        // LLD may leave this advisory section field absent. The x86-64 ABI,
        // full table extent and sole eight-byte writers still determine width.
        let mut absent_entry_size = bytes.clone();
        qword(&mut absent_entry_size, SECTIONS + 2 * 64 + 56, 0);
        discover(&absent_entry_size)
            .expect("absent section entry size must retain ABI pointer width");
    }

    #[test]
    fn preemptible_and_symbol_relocated_constructor_controls_are_refused() {
        let mut old = image(1);
        old[SYMBOL + 5] = sym::STV_DEFAULT;
        qword(
            &mut old,
            RELA + 8,
            (1_u64 << 32) | u64::from(reloc::R_X86_64_64),
        );
        qword(&mut old, RELA + 16, 0);
        refused(&old, "strong PROTECTED");
        let mut protected_r64 = image(1);
        qword(
            &mut protected_r64,
            RELA + 8,
            (1_u64 << 32) | u64::from(reloc::R_X86_64_64),
        );
        refused(&protected_r64, "symbol-zero RELATIVE");
        let mut weak = image(1);
        weak[SYMBOL + 4] = (sym::STB_WEAK << 4) | sym::STT_FUNC;
        refused(&weak, "strong PROTECTED");
    }

    #[test]
    fn constructor_registration_physical_controls_are_refused() {
        let mut wrong = image(1);
        qword(&mut wrong, RELA + 16, ROOT_ADDRESS + 0x100);
        refused(&wrong, "unique exact-root");
        refused(&image(2), "unique exact-root");
        let mut duplicate_writer = image(2);
        qword(&mut duplicate_writer, RELA + 24, ARRAY_ADDRESS);
        refused(&duplicate_writer, "duplicate relocation");
        let mut unaligned = image(1);
        qword(&mut unaligned, RELA, ARRAY_ADDRESS + 1);
        refused(&unaligned, "partial or unaligned");
        let mut partial = image(1);
        qword(&mut partial, RELA, ARRAY_ADDRESS - 1);
        refused(&partial, "partial or unaligned");
        let mut truncated = image(1);
        qword(&mut truncated, SECTIONS + 2 * 64 + 24, 0x800);
        refused(&truncated, "file extent is truncated");
        let mut missing = image(1);
        qword(&mut missing, DYNAMIC + 6 * 16 + 8, 0);
        refused(&missing, "no sole RELATIVE");
        qword(&mut missing, 0x200, ROOT_ADDRESS);
        refused(&missing, "no sole RELATIVE");
        let mut different_dynamic = image(1);
        qword(&mut different_dynamic, DYNAMIC + 9 * 16 + 8, 16);
        refused(&different_dynamic, "dynamic/section/file identity");
        let mut section_alias = image(1);
        qword(&mut section_alias, SECTIONS + 2 * 64 + 24, 0x208);
        refused(&section_alias, "dynamic/section/file identity");
        let mut incompatible_entry_size = image(1);
        qword(&mut incompatible_entry_size, SECTIONS + 2 * 64 + 56, 4);
        refused(&incompatible_entry_size, "constructor array");
    }
}
