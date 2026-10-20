/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::ffi::OsStr;
use std::fs;
use std::io::Read;
use std::io::Seek;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::path::PathBuf;

use goblin::container::Ctx;
use goblin::elf::Elf;
use goblin::elf::ProgramHeader;
use goblin::elf::program_header;

const ELF_HEADER_SIZE: usize = 64;
const MAX_INTERP_SIZE: usize = libc::PATH_MAX as usize;

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#552)
/// Get the right ld.so from elf's interp section.
pub fn elf_get_interp<P: AsRef<Path>>(elf: P) -> Option<PathBuf> {
    let mut file = fs::File::open(elf).ok()?;
    let mut header_bytes = [0; ELF_HEADER_SIZE];
    file.read_exact(&mut header_bytes).ok()?;
    let header = Elf::parse_header(&header_bytes).ok()?;
    let mut elf = Elf::lazy_parse(header).ok()?;
    let ctx = Ctx {
        le: header.endianness().ok()?,
        container: header.container().ok()?,
    };

    // parse and assemble the program headers
    let program_header_size = ProgramHeader::size(ctx);
    if usize::from(header.e_phentsize) != program_header_size {
        return None;
    }
    let program_header_count = usize::from(header.e_phnum);
    let table_size = program_header_size.checked_mul(program_header_count)?;
    let mut table = vec![0; table_size];
    file.seek(std::io::SeekFrom::Start(header.e_phoff)).ok()?;
    file.read_exact(&mut table).ok()?;
    elf.program_headers = ProgramHeader::parse(&table, 0, program_header_count, ctx).ok()?;

    for ph in &elf.program_headers {
        if ph.p_type == program_header::PT_INTERP {
            let size = usize::try_from(ph.p_filesz).ok()?;
            if !(2..=MAX_INTERP_SIZE).contains(&size) {
                return None;
            }

            let mut interp = vec![0; size];
            file.seek(std::io::SeekFrom::Start(ph.p_offset)).ok()?;
            file.read_exact(&mut interp).ok()?;
            let path = interp.strip_suffix(b"\0")?;
            if path.is_empty() || path.contains(&0) {
                return None;
            }
            return Some(PathBuf::from(OsStr::from_bytes(path)));
        }
    }

    None
}

/// What an executable's ELF headers say about the code that runs before an
/// `LD_PRELOAD` library's constructor.
#[cfg(feature = "liteinst")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElfStartup {
    /// A 64-bit x86-64 image, the only kind a 64-bit preload can be loaded into.
    pub x86_64: bool,
    /// The image names a dynamic loader (`PT_INTERP`). Without one, nothing
    /// reads `LD_PRELOAD`.
    pub has_interp: bool,
    /// The image's dynamic section has a non-empty `DT_PREINIT_ARRAY`, whose
    /// functions the loader runs before any library constructor.
    pub has_preinit_array: bool,
}

#[cfg(feature = "liteinst")]
const DT_NULL: u64 = 0;
#[cfg(feature = "liteinst")]
const DT_PREINIT_ARRAYSZ: u64 = 33;
#[cfg(feature = "liteinst")]
const MAX_DYNAMIC_SIZE: usize = 1 << 20;

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-3635): Review the ELF startup facts that gate in-guest LiteInst.
/// Reads [`ElfStartup`] from `file`, starting at its beginning whatever its
/// current offset. Returns `Ok(None)` when the file is not an ELF image, and an
/// error when it is one whose headers cannot be read.
#[cfg(feature = "liteinst")]
pub fn elf_startup(file: &mut fs::File) -> std::io::Result<Option<ElfStartup>> {
    let invalid =
        |what: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, what.to_owned());
    file.seek(std::io::SeekFrom::Start(0))?;
    let mut header_bytes = [0; ELF_HEADER_SIZE];
    let mut filled = 0;
    while filled < header_bytes.len() {
        match file.read(&mut header_bytes[filled..])? {
            0 => break,
            read => filled += read,
        }
    }
    if filled < 4 || header_bytes[..4] != *b"\x7fELF" {
        return Ok(None);
    }
    let header =
        Elf::parse_header(&header_bytes[..filled]).map_err(|_| invalid("unreadable ELF header"))?;
    let container = header
        .container()
        .map_err(|_| invalid("unknown ELF class"))?;
    let endianness = header
        .endianness()
        .map_err(|_| invalid("unknown ELF byte order"))?;
    let ctx = Ctx {
        le: endianness,
        container,
    };
    let program_header_size = ProgramHeader::size(ctx);
    if usize::from(header.e_phentsize) != program_header_size {
        return Err(invalid("unexpected ELF program header size"));
    }
    let table_size = program_header_size
        .checked_mul(usize::from(header.e_phnum))
        .ok_or_else(|| invalid("ELF program header table is too large"))?;
    let mut table = vec![0; table_size];
    file.seek(std::io::SeekFrom::Start(header.e_phoff))?;
    file.read_exact(&mut table)?;
    let program_headers = ProgramHeader::parse(&table, 0, usize::from(header.e_phnum), ctx)
        .map_err(|_| invalid("unreadable ELF program headers"))?;

    let mut startup = ElfStartup {
        x86_64: container.is_big()
            && endianness.is_little()
            && header.e_machine == goblin::elf::header::EM_X86_64,
        has_interp: false,
        has_preinit_array: false,
    };
    for ph in &program_headers {
        match ph.p_type {
            program_header::PT_INTERP => startup.has_interp = true,
            program_header::PT_DYNAMIC => {
                let size = usize::try_from(ph.p_filesz)
                    .ok()
                    .filter(|size| *size <= MAX_DYNAMIC_SIZE)
                    .ok_or_else(|| invalid("ELF dynamic section is too large"))?;
                let mut dynamic = vec![0; size];
                file.seek(std::io::SeekFrom::Start(ph.p_offset))?;
                file.read_exact(&mut dynamic)?;
                let word = if container.is_big() { 8 } else { 4 };
                let read_word = |bytes: &[u8]| -> u64 {
                    let mut buffer = [0; 8];
                    if endianness.is_little() {
                        buffer[..word].copy_from_slice(bytes);
                        u64::from_le_bytes(buffer)
                    } else {
                        buffer[8 - word..].copy_from_slice(bytes);
                        u64::from_be_bytes(buffer)
                    }
                };
                for entry in dynamic.chunks_exact(2 * word) {
                    let tag = read_word(&entry[..word]);
                    if tag == DT_NULL {
                        break;
                    }
                    if tag == DT_PREINIT_ARRAYSZ && read_word(&entry[word..]) != 0 {
                        startup.has_preinit_array = true;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(Some(startup))
}

/// Synthetic ELF images for tests of [`elf_startup`] and its callers.
#[cfg(all(test, feature = "liteinst"))]
pub(crate) mod startup_fixtures {
    use super::ELF_HEADER_SIZE;

    pub const EM_X86_64: u16 = 62;
    pub const EM_AARCH64: u16 = 183;
    pub const PT_LOAD: u32 = 1;
    pub const PT_DYNAMIC: u32 = 2;
    pub const PT_INTERP: u32 = 3;
    pub const DT_NULL: u64 = 0;
    pub const DT_NEEDED: u64 = 1;
    pub const DT_PREINIT_ARRAYSZ: u64 = 33;
    pub const INTERP: &[u8] = b"/lib64/ld-linux-x86-64.so.2\0";

    /// A 64-bit little-endian executable for `machine` with one program header
    /// per `(type, contents)` segment, each segment's contents placed after the
    /// header table. Nothing else in it is valid enough to run.
    pub fn elf(machine: u16, segments: &[(u32, &[u8])]) -> Vec<u8> {
        const PROGRAM_HEADER_SIZE: usize = 56;
        let mut bytes = vec![0; ELF_HEADER_SIZE + PROGRAM_HEADER_SIZE * segments.len()];
        bytes[..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2; // ELFCLASS64
        bytes[5] = 1; // ELFDATA2LSB
        bytes[6] = 1; // EV_CURRENT
        put(&mut bytes, 16, &3u16.to_le_bytes()); // ET_DYN
        put(&mut bytes, 18, &machine.to_le_bytes());
        put(&mut bytes, 20, &1u32.to_le_bytes()); // EV_CURRENT
        put(&mut bytes, 32, &(ELF_HEADER_SIZE as u64).to_le_bytes());
        put(&mut bytes, 52, &(ELF_HEADER_SIZE as u16).to_le_bytes());
        put(&mut bytes, 54, &(PROGRAM_HEADER_SIZE as u16).to_le_bytes());
        put(&mut bytes, 56, &(segments.len() as u16).to_le_bytes());
        for (index, (p_type, contents)) in segments.iter().enumerate() {
            let offset = bytes.len() as u64;
            let size = contents.len() as u64;
            bytes.extend_from_slice(contents);
            let header = ELF_HEADER_SIZE + index * PROGRAM_HEADER_SIZE;
            put(&mut bytes, header, &p_type.to_le_bytes());
            put(&mut bytes, header + 4, &4u32.to_le_bytes()); // PF_R
            put(&mut bytes, header + 8, &offset.to_le_bytes());
            put(&mut bytes, header + 32, &size.to_le_bytes());
            put(&mut bytes, header + 40, &size.to_le_bytes());
            put(&mut bytes, header + 48, &1u64.to_le_bytes());
        }
        bytes
    }

    /// The contents of a 64-bit little-endian `PT_DYNAMIC` segment.
    pub fn dynamic(entries: &[(u64, u64)]) -> Vec<u8> {
        entries
            .iter()
            .flat_map(|(tag, value)| tag.to_le_bytes().into_iter().chain(value.to_le_bytes()))
            .collect()
    }

    /// A dynamically linked x86-64 executable, optionally with a non-empty
    /// `DT_PREINIT_ARRAY`.
    pub fn dynamic_executable(preinit_array: bool) -> Vec<u8> {
        let mut entries = vec![(DT_NEEDED, 1)];
        if preinit_array {
            entries.push((DT_PREINIT_ARRAYSZ, 8));
        }
        entries.push((DT_NULL, 0));
        elf(
            EM_X86_64,
            &[(PT_INTERP, INTERP), (PT_DYNAMIC, &dynamic(&entries))],
        )
    }

    fn put(bytes: &mut [u8], offset: usize, value: &[u8]) {
        bytes[offset..offset + value.len()].copy_from_slice(value);
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    const QEMU_INTERP_OFFSET: usize = 0x7c7000;
    const INTERP: &[u8] = b"/lib64/ld-linux-x86-64.so.2\0";

    fn write_u16(bytes: &mut [u8], offset: usize, value: u16) {
        bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn write_u64(bytes: &mut [u8], offset: usize, value: u64) {
        bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn elf_with_late_interp(
        interp_offset: usize,
        declared_size: usize,
        contents: Option<&[u8]>,
    ) -> Vec<u8> {
        const PROGRAM_HEADER_SIZE: usize = 56;

        let len = if let Some(contents) = contents {
            interp_offset + contents.len()
        } else {
            ELF_HEADER_SIZE + PROGRAM_HEADER_SIZE
        };
        let mut bytes = vec![0; len];
        bytes[..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2; // ELFCLASS64
        bytes[5] = 1; // ELFDATA2LSB
        bytes[6] = 1; // EV_CURRENT
        write_u16(&mut bytes, 16, 3); // ET_DYN
        write_u16(&mut bytes, 18, 62); // EM_X86_64
        write_u32(&mut bytes, 20, 1); // EV_CURRENT
        write_u64(&mut bytes, 32, ELF_HEADER_SIZE as u64);
        write_u16(&mut bytes, 52, ELF_HEADER_SIZE as u16);
        write_u16(&mut bytes, 54, PROGRAM_HEADER_SIZE as u16);
        write_u16(&mut bytes, 56, 1);

        let ph = ELF_HEADER_SIZE;
        write_u32(&mut bytes, ph, program_header::PT_INTERP);
        write_u32(&mut bytes, ph + 4, 4); // PF_R
        write_u64(&mut bytes, ph + 8, interp_offset as u64);
        write_u64(&mut bytes, ph + 32, declared_size as u64);
        write_u64(&mut bytes, ph + 40, declared_size as u64);
        write_u64(&mut bytes, ph + 48, 1);

        if let Some(contents) = contents {
            bytes[interp_offset..interp_offset + contents.len()].copy_from_slice(contents);
        }
        bytes
    }

    #[test]
    fn reads_qemu_sized_late_interp_segment() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&elf_with_late_interp(
            QEMU_INTERP_OFFSET,
            INTERP.len(),
            Some(INTERP),
        ))
        .unwrap();

        assert_eq!(
            elf_get_interp(file.path()),
            Some(PathBuf::from("/lib64/ld-linux-x86-64.so.2"))
        );
    }

    #[test]
    fn truncated_interp_segment_returns_none() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&elf_with_late_interp(
            QEMU_INTERP_OFFSET,
            INTERP.len(),
            None,
        ))
        .unwrap();

        assert_eq!(elf_get_interp(file.path()), None);
    }

    #[test]
    fn reads_interp_segment_beyond_previous_buffer_limit() {
        const LARGE_INTERP_OFFSET: usize = 17 * 1024 * 1024;
        const NON_DEFAULT_INTERP: &[u8] = b"/opt/replay/ld.so\0";
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&elf_with_late_interp(
            LARGE_INTERP_OFFSET,
            NON_DEFAULT_INTERP.len(),
            Some(NON_DEFAULT_INTERP),
        ))
        .unwrap();

        assert_eq!(
            elf_get_interp(file.path()),
            Some(PathBuf::from("/opt/replay/ld.so"))
        );
    }

    #[test]
    fn non_terminated_interp_segment_returns_none() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let path = &INTERP[..INTERP.len() - 1];
        file.write_all(&elf_with_late_interp(
            QEMU_INTERP_OFFSET,
            path.len(),
            Some(path),
        ))
        .unwrap();

        assert_eq!(elf_get_interp(file.path()), None);
    }

    #[test]
    fn truncated_interp_terminator_returns_none() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let path = &INTERP[..INTERP.len() - 1];
        file.write_all(&elf_with_late_interp(
            QEMU_INTERP_OFFSET,
            INTERP.len(),
            Some(path),
        ))
        .unwrap();

        assert_eq!(elf_get_interp(file.path()), None);
    }

    #[cfg(feature = "liteinst")]
    fn startup_of(bytes: &[u8]) -> Option<ElfStartup> {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(bytes).unwrap();
        elf_startup(file.as_file_mut()).unwrap()
    }

    #[test]
    #[cfg(feature = "liteinst")]
    fn startup_of_a_dynamic_x86_64_executable() {
        use startup_fixtures::*;

        assert_eq!(
            startup_of(&dynamic_executable(false)),
            Some(ElfStartup {
                x86_64: true,
                has_interp: true,
                has_preinit_array: false,
            })
        );
        assert_eq!(
            startup_of(&dynamic_executable(true)),
            Some(ElfStartup {
                x86_64: true,
                has_interp: true,
                has_preinit_array: true,
            })
        );
    }

    #[test]
    #[cfg(feature = "liteinst")]
    fn startup_of_a_static_executable_has_no_interpreter() {
        use startup_fixtures::*;

        assert_eq!(
            startup_of(&elf(EM_X86_64, &[(PT_LOAD, b"code")])),
            Some(ElfStartup {
                x86_64: true,
                has_interp: false,
                has_preinit_array: false,
            })
        );
    }

    #[test]
    #[cfg(feature = "liteinst")]
    fn startup_reads_the_dynamic_section_only_up_to_dt_null() {
        use startup_fixtures::*;

        let read = |entries: &[(u64, u64)]| {
            startup_of(&elf(
                EM_X86_64,
                &[(PT_INTERP, INTERP), (PT_DYNAMIC, &dynamic(entries))],
            ))
            .unwrap()
            .has_preinit_array
        };
        assert!(read(&[(DT_PREINIT_ARRAYSZ, 8), (DT_NULL, 0)]));
        // An empty array runs nothing.
        assert!(!read(&[(DT_PREINIT_ARRAYSZ, 0), (DT_NULL, 0)]));
        // The loader stops at DT_NULL, so a later entry is not part of the image.
        assert!(!read(&[(DT_NULL, 0), (DT_PREINIT_ARRAYSZ, 8)]));
    }

    #[test]
    #[cfg(feature = "liteinst")]
    fn startup_of_other_machines_is_not_x86_64() {
        use startup_fixtures::*;

        let startup = startup_of(&elf(EM_AARCH64, &[(PT_INTERP, INTERP)])).unwrap();
        assert!(!startup.x86_64);
        assert!(startup.has_interp);

        // A 32-bit i386 header (ELFCLASS32, EM_386) with no program headers.
        let mut i386 = vec![0; ELF_HEADER_SIZE];
        i386[..4].copy_from_slice(b"\x7fELF");
        i386[4] = 1; // ELFCLASS32
        i386[5] = 1; // ELFDATA2LSB
        i386[6] = 1; // EV_CURRENT
        write_u16(&mut i386, 16, 2); // ET_EXEC
        write_u16(&mut i386, 18, 3); // EM_386
        write_u32(&mut i386, 20, 1); // EV_CURRENT
        write_u16(&mut i386, 40, 52); // e_ehsize
        write_u16(&mut i386, 42, 32); // e_phentsize
        assert!(!startup_of(&i386).unwrap().x86_64);
    }

    #[test]
    #[cfg(feature = "liteinst")]
    fn startup_of_a_non_elf_file_is_none() {
        assert_eq!(startup_of(b"#!/bin/sh\necho script\n"), None);
        assert_eq!(startup_of(b""), None);
        assert_eq!(startup_of(b"\x7fEL"), None);
    }
}
