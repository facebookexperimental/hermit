/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Offline discovery of nondeterministic x86 instructions in ELF binaries.

use std::fs;
use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;

use goblin::elf::Elf;
use goblin::elf::header;
use goblin::elf::program_header;
use goblin::elf::section_header;
use iced_x86::Decoder;
use iced_x86::DecoderOptions;
use iced_x86::Instruction;
use iced_x86::Mnemonic;
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

use crate::Context;
use crate::Error;

const CACHE_SCHEMA_VERSION: u32 = 2;

/// One instruction that can expose host nondeterminism or enter the kernel.
#[derive(Debug, Clone, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct InstructionSite {
    /// Byte offset from the start of the ELF file.
    pub offset: u64,
    /// Lowercase x86 mnemonic.
    pub instruction: String,
    /// Encoded instruction length in bytes.
    pub length: u8,
}

/// Nanosecond-resolution file modification time used to validate cache entries.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModificationTime {
    pub seconds: i64,
    pub nanoseconds: i64,
}

/// A self-describing map of nondeterministic instruction sites in one ELF file.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct InstructionMap {
    pub schema_version: u32,
    pub binary: PathBuf,
    pub file_length: u64,
    pub modified: ModificationTime,
    pub sites: Vec<InstructionSite>,
}

/// Whether a map was loaded without reading and decoding the binary.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum CacheStatus {
    Hit,
    Miss,
}

/// Result of a cached instruction-map lookup.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct InstructionMapResult {
    pub map: InstructionMap,
    pub cache_status: CacheStatus,
    pub cache_path: PathBuf,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct FileIdentity {
    binary: PathBuf,
    file_length: u64,
    modified: ModificationTime,
}

#[derive(Debug, Clone, Copy)]
struct ExecutableRange {
    file_offset: u64,
    address: u64,
    size: u64,
}

/// Default directory for cached instruction maps.
pub fn default_cache_dir() -> PathBuf {
    dirs::cache_dir()
        .map_or_else(|| PathBuf::from("/tmp/hermit"), |dir| dir.join("hermit"))
        .join("instruction-maps")
}

/// Load an instruction map from the cache or generate and atomically cache it.
pub fn load_or_generate(
    binary: impl AsRef<Path>,
    cache_dir: impl AsRef<Path>,
) -> Result<InstructionMapResult, Error> {
    let requested = binary.as_ref();
    let canonical = fs::canonicalize(requested)
        .with_context(|| format!("failed to resolve binary {}", requested.display()))?;
    let mut file = File::open(&canonical)
        .with_context(|| format!("failed to open binary {}", canonical.display()))?;
    let before = file
        .metadata()
        .with_context(|| format!("failed to stat binary {}", canonical.display()))?;
    if !before.is_file() {
        return Err(Error::msg(format!(
            "instruction map input is not a regular file: {}",
            canonical.display()
        )));
    }

    let identity = FileIdentity::new(canonical, &before);
    let cache_dir = cache_dir.as_ref();
    let cache_path = cache_dir.join(format!("{}.json", cache_key(&identity)));
    if let Some(map) = read_valid_cache(&cache_path, &identity) {
        return Ok(InstructionMapResult {
            map,
            cache_status: CacheStatus::Hit,
            cache_path,
        });
    }

    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .with_context(|| format!("failed to read binary {}", identity.binary.display()))?;
    let after = file
        .metadata()
        .with_context(|| format!("failed to restat binary {}", identity.binary.display()))?;
    if before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
    {
        return Err(Error::msg(format!(
            "binary changed while generating instruction map: {}",
            identity.binary.display()
        )));
    }

    let sites = scan_elf(&bytes)
        .with_context(|| format!("failed to scan binary {}", identity.binary.display()))?;
    let map = InstructionMap {
        schema_version: CACHE_SCHEMA_VERSION,
        binary: identity.binary.clone(),
        file_length: identity.file_length,
        modified: identity.modified.clone(),
        sites,
    };
    write_cache(&cache_path, &map)?;

    Ok(InstructionMapResult {
        map,
        cache_status: CacheStatus::Miss,
        cache_path,
    })
}

impl FileIdentity {
    fn new(binary: PathBuf, metadata: &fs::Metadata) -> Self {
        Self {
            binary,
            file_length: metadata.len(),
            modified: ModificationTime {
                seconds: metadata.mtime(),
                nanoseconds: metadata.mtime_nsec(),
            },
        }
    }
}

fn cache_key(identity: &FileIdentity) -> String {
    let mut input = Vec::new();
    input.extend_from_slice(&CACHE_SCHEMA_VERSION.to_le_bytes());
    input.extend_from_slice(identity.binary.as_os_str().as_bytes());
    input.push(0);
    input.extend_from_slice(&identity.file_length.to_le_bytes());
    input.extend_from_slice(&identity.modified.seconds.to_le_bytes());
    input.extend_from_slice(&identity.modified.nanoseconds.to_le_bytes());
    Uuid::new_v5(&Uuid::NAMESPACE_URL, &input)
        .simple()
        .to_string()
}

fn read_valid_cache(cache_path: &Path, identity: &FileIdentity) -> Option<InstructionMap> {
    let map: InstructionMap = serde_json::from_reader(File::open(cache_path).ok()?).ok()?;
    (map.schema_version == CACHE_SCHEMA_VERSION
        && map.binary == identity.binary
        && map.file_length == identity.file_length
        && map.modified == identity.modified)
        .then_some(map)
}

fn write_cache(cache_path: &Path, map: &InstructionMap) -> Result<(), Error> {
    let cache_dir = cache_path
        .parent()
        .ok_or_else(|| Error::msg("instruction map cache path has no parent"))?;
    fs::create_dir_all(cache_dir).with_context(|| {
        format!(
            "failed to create instruction map cache directory {}",
            cache_dir.display()
        )
    })?;

    let mut temporary = tempfile::Builder::new()
        .prefix(".instruction-map-")
        .tempfile_in(cache_dir)
        .with_context(|| {
            format!(
                "failed to create temporary instruction map in {}",
                cache_dir.display()
            )
        })?;
    serde_json::to_writer(&mut temporary, map)?;
    temporary.write_all(b"\n")?;
    temporary.as_file().sync_all()?;
    temporary.persist(cache_path).with_context(|| {
        format!(
            "failed to persist instruction map cache {}",
            cache_path.display()
        )
    })?;
    Ok(())
}

fn scan_elf(bytes: &[u8]) -> Result<Vec<InstructionSite>, Error> {
    let elf = Elf::parse(bytes).context("input is not a valid ELF file")?;
    if !elf.little_endian {
        return Err(Error::msg("big-endian x86 ELF files are not supported"));
    }
    let bitness = match elf.header.e_machine {
        header::EM_386 => 32,
        header::EM_X86_64 => 64,
        machine => {
            return Err(Error::msg(format!(
                "unsupported ELF machine {machine}; expected x86 or x86-64"
            )));
        }
    };

    let mut sites = Vec::new();
    for range in executable_ranges(&elf) {
        let start = usize::try_from(range.file_offset)
            .context("executable range offset does not fit in memory")?;
        let size =
            usize::try_from(range.size).context("executable range size does not fit in memory")?;
        let end = start
            .checked_add(size)
            .ok_or_else(|| Error::msg("executable range overflows the file offset space"))?;
        let code = bytes.get(start..end).ok_or_else(|| {
            Error::msg(format!(
                "executable range {:#x}..{:#x} lies outside the ELF file",
                range.file_offset,
                range.file_offset.saturating_add(range.size)
            ))
        })?;
        scan_range(code, range, bitness, &mut sites)?;
    }
    sites.sort_unstable();
    sites.dedup();
    Ok(sites)
}

fn executable_ranges(elf: &Elf<'_>) -> Vec<ExecutableRange> {
    let mut ranges = elf
        .section_headers
        .iter()
        .filter(|section| {
            section.sh_flags & u64::from(section_header::SHF_EXECINSTR) != 0
                && section.sh_type != section_header::SHT_NOBITS
                && section.sh_size != 0
        })
        .map(|section| ExecutableRange {
            file_offset: section.sh_offset,
            address: section.sh_addr,
            size: section.sh_size,
        })
        .collect::<Vec<_>>();

    if ranges.is_empty() {
        ranges.extend(
            elf.program_headers
                .iter()
                .filter(|segment| {
                    segment.p_type == program_header::PT_LOAD
                        && segment.p_flags & program_header::PF_X != 0
                        && segment.p_filesz != 0
                })
                .map(|segment| ExecutableRange {
                    file_offset: segment.p_offset,
                    address: segment.p_vaddr,
                    size: segment.p_filesz,
                }),
        );
    }

    ranges.sort_unstable_by_key(|range| (range.file_offset, range.address, range.size));
    ranges
}

fn scan_range(
    bytes: &[u8],
    range: ExecutableRange,
    bitness: u32,
    sites: &mut Vec<InstructionSite>,
) -> Result<(), Error> {
    let mut decoder = Decoder::with_ip(bitness, bytes, range.address, DecoderOptions::NONE);
    while decoder.can_decode() {
        let instruction = decoder.decode();
        let Some(name) = nondeterministic_instruction(&instruction) else {
            continue;
        };
        let relative_offset = instruction
            .ip()
            .checked_sub(range.address)
            .ok_or_else(|| Error::msg("decoded instruction precedes its executable range"))?;
        let offset = range
            .file_offset
            .checked_add(relative_offset)
            .ok_or_else(|| Error::msg("decoded instruction file offset overflowed"))?;
        sites.push(InstructionSite {
            offset,
            instruction: name.to_string(),
            length: u8::try_from(instruction.len())
                .context("decoded x86 instruction length does not fit in u8")?,
        });
    }
    Ok(())
}

// TODO-HUMAN-REVIEW(PR-594): Review the expanded public instruction-map coverage.
fn nondeterministic_instruction(instruction: &Instruction) -> Option<&'static str> {
    match instruction.mnemonic() {
        Mnemonic::Syscall => Some("syscall"),
        Mnemonic::Cpuid => Some("cpuid"),
        Mnemonic::Rdrand => Some("rdrand"),
        Mnemonic::Rdtsc => Some("rdtsc"),
        Mnemonic::Rdtscp => Some("rdtscp"),
        Mnemonic::Rdseed => Some("rdseed"),
        Mnemonic::Sysenter => Some("sysenter"),
        Mnemonic::Xbegin => Some("xbegin"),
        Mnemonic::Xend => Some("xend"),
        Mnemonic::Int if instruction.immediate8() == 0x80 => Some("int80"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CODE_OFFSET: usize = 64;
    const CODE_ADDRESS: u64 = 0x0040_0000;
    const SECTION_HEADER_SIZE: usize = 64;

    fn write_u16(bytes: &mut [u8], offset: usize, value: u16) {
        bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn write_u64(bytes: &mut [u8], offset: usize, value: u64) {
        bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn elf_with_executable_section(code: &[u8]) -> Vec<u8> {
        let section_table = (CODE_OFFSET + code.len() + 7) & !7;
        let mut bytes = vec![0; section_table + 2 * SECTION_HEADER_SIZE];
        bytes[..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[6] = 1;
        write_u16(&mut bytes, 16, header::ET_EXEC);
        write_u16(&mut bytes, 18, header::EM_X86_64);
        write_u32(&mut bytes, 20, 1);
        write_u64(&mut bytes, 24, CODE_ADDRESS);
        write_u64(&mut bytes, 40, section_table as u64);
        write_u16(&mut bytes, 52, 64);
        write_u16(&mut bytes, 58, SECTION_HEADER_SIZE as u16);
        write_u16(&mut bytes, 60, 2);
        bytes[CODE_OFFSET..CODE_OFFSET + code.len()].copy_from_slice(code);

        let text = section_table + SECTION_HEADER_SIZE;
        write_u32(&mut bytes, text + 4, section_header::SHT_PROGBITS);
        write_u64(
            &mut bytes,
            text + 8,
            u64::from(section_header::SHF_ALLOC | section_header::SHF_EXECINSTR),
        );
        write_u64(&mut bytes, text + 16, CODE_ADDRESS);
        write_u64(&mut bytes, text + 24, CODE_OFFSET as u64);
        write_u64(&mut bytes, text + 32, code.len() as u64);
        write_u64(&mut bytes, text + 48, 1);
        bytes
    }

    fn all_target_instructions() -> Vec<u8> {
        vec![
            0x0f, 0x05, 0x0f, 0xa2, 0x48, 0x0f, 0xc7, 0xf0, 0x0f, 0x31, 0x48, 0x0f, 0xc7, 0xf8,
            0x0f, 0x01, 0xf9, 0x0f, 0x34, 0xcd, 0x80, 0xc7, 0xf8, 0, 0, 0, 0, 0x0f, 0x01, 0xd5,
        ]
    }

    #[test]
    fn scans_every_requested_instruction() {
        let sites = scan_elf(&elf_with_executable_section(&all_target_instructions())).unwrap();
        assert_eq!(
            sites,
            vec![
                InstructionSite {
                    offset: 64,
                    instruction: "syscall".into(),
                    length: 2
                },
                InstructionSite {
                    offset: 66,
                    instruction: "cpuid".into(),
                    length: 2
                },
                InstructionSite {
                    offset: 68,
                    instruction: "rdrand".into(),
                    length: 4
                },
                InstructionSite {
                    offset: 72,
                    instruction: "rdtsc".into(),
                    length: 2
                },
                InstructionSite {
                    offset: 74,
                    instruction: "rdseed".into(),
                    length: 4
                },
                InstructionSite {
                    offset: 78,
                    instruction: "rdtscp".into(),
                    length: 3
                },
                InstructionSite {
                    offset: 81,
                    instruction: "sysenter".into(),
                    length: 2
                },
                InstructionSite {
                    offset: 83,
                    instruction: "int80".into(),
                    length: 2
                },
                InstructionSite {
                    offset: 85,
                    instruction: "xbegin".into(),
                    length: 6
                },
                InstructionSite {
                    offset: 91,
                    instruction: "xend".into(),
                    length: 3
                },
            ]
        );
    }

    #[test]
    fn cache_hit_skips_regeneration_and_identity_changes_miss() {
        let temp = tempfile::tempdir().unwrap();
        let binary = temp.path().join("fixture");
        let cache = temp.path().join("cache");
        fs::write(
            &binary,
            elf_with_executable_section(&all_target_instructions()),
        )
        .unwrap();

        let first = load_or_generate(&binary, &cache).unwrap();
        assert_eq!(first.cache_status, CacheStatus::Miss);
        assert!(first.cache_path.is_file());

        let second = load_or_generate(&binary, &cache).unwrap();
        assert_eq!(second.cache_status, CacheStatus::Hit);
        assert_eq!(second.map, first.map);
        assert_eq!(second.cache_path, first.cache_path);

        let mut changed = fs::read(&binary).unwrap();
        changed.push(0);
        fs::write(&binary, changed).unwrap();
        let third = load_or_generate(&binary, &cache).unwrap();
        assert_eq!(third.cache_status, CacheStatus::Miss);
        assert_ne!(third.cache_path, first.cache_path);
    }

    #[test]
    fn stale_schema_cache_is_regenerated() {
        let temp = tempfile::tempdir().unwrap();
        let binary = temp.path().join("fixture");
        let cache = temp.path().join("cache");
        fs::write(
            &binary,
            elf_with_executable_section(&all_target_instructions()),
        )
        .unwrap();

        let canonical = fs::canonicalize(&binary).unwrap();
        let metadata = fs::metadata(&canonical).unwrap();
        let identity = FileIdentity::new(canonical.clone(), &metadata);
        let cache_path = cache.join(format!("{}.json", cache_key(&identity)));
        let stale = InstructionMap {
            schema_version: CACHE_SCHEMA_VERSION - 1,
            binary: canonical,
            file_length: identity.file_length,
            modified: identity.modified,
            sites: Vec::new(),
        };
        write_cache(&cache_path, &stale).unwrap();

        let regenerated = load_or_generate(&binary, &cache).unwrap();
        assert_eq!(regenerated.cache_status, CacheStatus::Miss);
        assert_eq!(regenerated.map.schema_version, CACHE_SCHEMA_VERSION);
        assert!(
            regenerated
                .map
                .sites
                .iter()
                .any(|site| site.instruction == "rdtscp")
        );
    }
}
