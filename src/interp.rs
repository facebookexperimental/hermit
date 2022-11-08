// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

use std::ffi::OsStr;
use std::fs;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::path::PathBuf;

use goblin::container::Ctx;
use goblin::elf::program_header;
use goblin::elf::Elf;
use goblin::elf::ProgramHeader;

// minimal size required to parse elf's .interp section.
const ELF_MIN_SIZE: usize = 8192;

/// Get the right ld.so from elf's interp section.
pub fn elf_get_interp<P: AsRef<Path>>(elf: P) -> Option<PathBuf> {
    let mut buffer = Vec::new();
    let nb = fs::File::open(elf)
        .and_then(|f| {
            let mut handle = f.take(ELF_MIN_SIZE as u64);
            handle.read_to_end(&mut buffer)
        })
        .ok()?;

    let header = Elf::parse_header(&buffer[..nb]).ok()?;
    let mut elf = Elf::lazy_parse(header).ok()?;
    let ctx = Ctx {
        le: header.endianness().ok()?,
        container: header.container().ok()?,
    };

    // parse and assemble the program headers
    elf.program_headers = ProgramHeader::parse(
        &buffer,
        header.e_phoff as usize,
        header.e_phnum as usize,
        ctx,
    )
    .ok()?;

    let mut interp: Option<&OsStr> = None;
    for ph in &elf.program_headers {
        // read in interpreter segment
        if ph.p_type == program_header::PT_INTERP && ph.p_filesz != 0 {
            let size = ph.p_filesz as usize;
            let offset = ph.p_offset as usize;
            // skip trailing '\0'.
            interp = Some(OsStr::from_bytes(&buffer[offset..offset + size - 1]));
            break;
        }
    }

    interp.map(PathBuf::from)
}
