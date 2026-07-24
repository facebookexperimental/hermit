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
}
