/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The x86-64 level a replay cell's C library declares it needs, and the CPU
//! profile its `record start` names for it.
//!
//! `hermit record start` resolves `--cpu-profile auto` from what the program
//! and its program interpreter declare, and reads no shared library (the
//! `--cpu-profile` help). CentOS 10's glibc declares its need in `libc.so.6`
//! alone: its `GNU_PROPERTY_X86_ISA_1_NEEDED` names x86-64-v3, and
//! `ld-linux-x86-64.so.2` declares no need. So `record start` presents
//! x86-64-v2 there, and the loader refuses the guest before it starts
//! ("Fatal glibc error: CPU does not support x86-64-v3"). For that case the
//! help says to name the profile, and [`replay_profile_arg`] names it.
//!
//! The library read is the `libc.so.6` beside the program's glibc
//! interpreter, after the interpreter's symlinks. glibc's loader and its C
//! library come from one build (they share `GLIBC_PRIVATE`), so that file
//! declares what the C library the loader maps declares, without repeating
//! the loader's search.

use std::ffi::OsStr;
use std::fs::File;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::FileExt as _;
use std::path::Path;
use std::path::PathBuf;

/// The profile a replay cell's `record start` names where its program's C
/// library needs x86-64-v3.
pub const LIBC_V3_PROFILE_ARG: &str = "--cpu-profile=x86-64-v3";

/// The file name of glibc's x86-64 program interpreter.
const GLIBC_X86_64_INTERPRETER: &str = "ld-linux-x86-64.so.2";
/// The C library glibc's interpreter maps, from its own directory.
const GLIBC_C_LIBRARY: &str = "libc.so.6";
/// How many script interpreters are followed, as many as Hermit's auto
/// follows.
const MAX_SCRIPT_HOPS: usize = 4;
/// The bytes of a script the kernel reads its interpreter line from
/// (`BINPRM_BUF_SIZE`).
const SCRIPT_HEAD: usize = 256;

/// `EM_X86_64`.
const EM_X86_64: u16 = 62;
/// `PT_INTERP`.
const PT_INTERP: u32 = 3;
/// `PT_NOTE`, read for the property note where no `PT_GNU_PROPERTY` exists.
const PT_NOTE: u32 = 4;
/// `PT_GNU_PROPERTY`.
const PT_GNU_PROPERTY: u32 = 0x6474_e553;
/// `NT_GNU_PROPERTY_TYPE_0`.
const NT_GNU_PROPERTY_TYPE_0: u32 = 5;
/// `GNU_PROPERTY_X86_ISA_1_NEEDED`.
const GNU_PROPERTY_X86_ISA_1_NEEDED: u32 = 0xc000_8002;
/// The largest segment read, for a note or an interpreter path.
const MAX_SEGMENT: u64 = 64 * 1024;

/// [`LIBC_V3_PROFILE_ARG`] where the C library of the program at host path
/// `program` declares that it needs x86-64-v3 ([`libc_needed_level`]), and
/// `None` otherwise. A library that needs less leaves auto to choose. One
/// that needs more is beyond every profile, so naming one would not help.
pub fn replay_profile_arg(program: &Path) -> Option<&'static str> {
    (libc_needed_level(program)? == 3).then_some(LIBC_V3_PROFILE_ARG)
}

/// The highest x86-64 level (1 baseline, 2 to 4 for x86-64-v2 to v4, 0 for
/// an empty set) that the C library of the program at host path `program`
/// declares it needs.
///
/// The program is followed through the `#!` scripts the kernel would follow
/// to the image it execs. The level is the `GNU_PROPERTY_X86_ISA_1_NEEDED`
/// of the `libc.so.6` beside that image's glibc interpreter, after the
/// interpreter path's symlinks are resolved.
///
/// `None` where there is no such declaration to read: a static image, an
/// interpreter that is not glibc's or is named by a relative path, no C
/// library there, or one that declares nothing or declares it unreadably.
/// The kernel resolves a relative interpreter against the guest's working
/// directory when it execs the image (`load_elf_binary` calls `open_exec`,
/// which opens relative to the current directory), not against the
/// harness's, so this reads none.
pub fn libc_needed_level(program: &Path) -> Option<u8> {
    let image = exec_image(program)?;
    let interpreter = Elf::open(&image)?.interpreter()?;
    if !interpreter.is_absolute()
        || interpreter.file_name()? != OsStr::new(GLIBC_X86_64_INTERPRETER)
    {
        return None;
    }
    let resolved = std::fs::canonicalize(&interpreter).ok()?;
    Elf::open(&resolved.parent()?.join(GLIBC_C_LIBRARY))?.isa_needed_level()
}

/// The image the kernel execs for `program`: `program` itself, or each `#!`
/// script's interpreter in turn, through at most [`MAX_SCRIPT_HOPS`] scripts.
/// `None` for an unreadable file, an interpreter named by a relative path,
/// and a longer chain.
fn exec_image(program: &Path) -> Option<PathBuf> {
    let mut image = program.to_path_buf();
    for _ in 0..=MAX_SCRIPT_HOPS {
        match script_interpreter(&image)? {
            Some(interpreter) => image = interpreter,
            None => return Some(image),
        }
    }
    None
}

/// The interpreter the `#!` line of the script at `path` names
/// (`Some(Some(_))`), `Some(None)` for a file that is not a script, and
/// `None` for an unreadable file, one that is not a regular file
/// ([`open_regular`]), and an interpreter that is not an absolute path. The
/// line and the path end where the kernel ends them (`load_script`): at a
/// newline, and at a space, tab or NUL.
fn script_interpreter(path: &Path) -> Option<Option<PathBuf>> {
    let mut head = [0_u8; SCRIPT_HEAD];
    let read = open_regular(path)?.read_at(&mut head, 0).ok()?;
    let Some(line) = head[..read].strip_prefix(b"#!") else {
        return Some(None);
    };
    let line = line.split(|&byte| byte == b'\n').next().unwrap_or(line);
    let start = line
        .iter()
        .position(|&byte| byte != b' ' && byte != b'\t')
        .unwrap_or(line.len());
    let line = &line[start..];
    let end = line
        .iter()
        .position(|&byte| matches!(byte, b' ' | b'\t' | 0))
        .unwrap_or(line.len());
    let interpreter = &line[..end];
    interpreter
        .starts_with(b"/")
        .then(|| Some(PathBuf::from(OsStr::from_bytes(interpreter))))
}

/// The file at `path`, after its symlinks, open for reading where it is a
/// regular file, and `None` otherwise: opening a FIFO for reading would
/// block the harness until a writer came, and a device need never end.
fn open_regular(path: &Path) -> Option<File> {
    if !std::fs::metadata(path).ok()?.is_file() {
        return None;
    }
    File::open(path).ok()
}

/// One program header: its type, file offset, file size and alignment.
struct ProgramHeader {
    kind: u32,
    offset: u64,
    size: u64,
    align: u64,
}

/// A little-endian ELF64 x86-64 image open for reading, with its program
/// headers.
struct Elf {
    file: File,
    headers: Vec<ProgramHeader>,
}

impl Elf {
    /// The image at `path`, or `None` for an unreadable file, one that is not
    /// a regular file ([`open_regular`]), and one that is not a little-endian
    /// ELF64 x86-64 image.
    fn open(path: &Path) -> Option<Self> {
        let file = open_regular(path)?;
        let mut header = [0_u8; 64];
        file.read_exact_at(&mut header, 0).ok()?;
        // ELFMAG, ELFCLASS64, ELFDATA2LSB and EM_X86_64.
        if header[..4] != *b"\x7fELF"
            || header[4] != 2
            || header[5] != 1
            || u16_at(&header, 18) != EM_X86_64
        {
            return None;
        }
        let table = u64_at(&header, 0x20);
        let entry_size = u64::from(u16_at(&header, 0x36));
        let count = u16_at(&header, 0x38);
        if entry_size < 56 {
            return None;
        }
        let mut headers = Vec::with_capacity(usize::from(count));
        for index in 0..u64::from(count) {
            let mut entry = [0_u8; 56];
            let at = table.checked_add(index.checked_mul(entry_size)?)?;
            file.read_exact_at(&mut entry, at).ok()?;
            headers.push(ProgramHeader {
                kind: u32_at(&entry, 0),
                offset: u64_at(&entry, 8),
                size: u64_at(&entry, 32),
                align: u64_at(&entry, 48),
            });
        }
        Some(Self { file, headers })
    }

    /// The bytes of `header`'s segment, or `None` past [`MAX_SEGMENT`] or the
    /// end of the file.
    fn segment(&self, header: &ProgramHeader) -> Option<Vec<u8>> {
        if header.size > MAX_SEGMENT {
            return None;
        }
        let mut bytes = vec![0_u8; usize::try_from(header.size).ok()?];
        self.file.read_exact_at(&mut bytes, header.offset).ok()?;
        Some(bytes)
    }

    /// The path the image's `PT_INTERP` names, or `None` for an image with
    /// none.
    fn interpreter(&self) -> Option<PathBuf> {
        let header = self
            .headers
            .iter()
            .find(|header| header.kind == PT_INTERP)?;
        let bytes = self.segment(header)?;
        let path = bytes.split(|&byte| byte == 0).next()?;
        (!path.is_empty()).then(|| PathBuf::from(OsStr::from_bytes(path)))
    }

    /// The highest level the image's `GNU_PROPERTY_X86_ISA_1_NEEDED` names,
    /// read from its `PT_GNU_PROPERTY` segment, or from its `PT_NOTE`
    /// segments where it has none. `None` for an image that declares
    /// nothing, a malformed note, and two different declarations.
    fn isa_needed_level(&self) -> Option<u8> {
        let mut segments: Vec<_> = self
            .headers
            .iter()
            .filter(|header| header.kind == PT_GNU_PROPERTY)
            .collect();
        if segments.is_empty() {
            segments = self
                .headers
                .iter()
                .filter(|header| header.kind == PT_NOTE)
                .collect();
        }
        let mut declared = None;
        for header in segments {
            let align = if header.align == 8 { 8 } else { 4 };
            for value in isa_needed_values(&self.segment(header)?, align)? {
                match declared {
                    None => declared = Some(value),
                    Some(seen) if seen == value => {}
                    Some(_) => return None,
                }
            }
        }
        let bits: u32 = declared?;
        Some(match bits.checked_ilog2() {
            Some(highest) => u8::try_from(highest + 1).ok()?,
            None => 0,
        })
    }
}

/// Every `GNU_PROPERTY_X86_ISA_1_NEEDED` value in the notes of `segment`,
/// whose notes are aligned to `align` bytes, or `None` for a malformed note.
fn isa_needed_values(segment: &[u8], align: usize) -> Option<Vec<u32>> {
    let mut values = Vec::new();
    let mut at = 0;
    while at + 12 <= segment.len() {
        let name_size = usize::try_from(u32_at(segment, at)).ok()?;
        let description_size = usize::try_from(u32_at(segment, at + 4)).ok()?;
        let kind = u32_at(segment, at + 8);
        let name_start = at + 12;
        let name_end = name_start.checked_add(name_size)?;
        let description_start = round_up(name_end, align)?;
        let description_end = description_start.checked_add(description_size)?;
        if description_end > segment.len() {
            return None;
        }
        if kind == NT_GNU_PROPERTY_TYPE_0 && segment[name_start..name_end] == *b"GNU\0" {
            values.extend(property_values(
                &segment[description_start..description_end],
            )?);
        }
        at = round_up(description_end, align)?;
    }
    Some(values)
}

/// The `GNU_PROPERTY_X86_ISA_1_NEEDED` values among the properties of one
/// `NT_GNU_PROPERTY_TYPE_0` description, each property padded to 8 bytes as
/// in an ELF64 image, or `None` for a malformed description.
fn property_values(description: &[u8]) -> Option<Vec<u32>> {
    let mut values = Vec::new();
    let mut at = 0;
    while at < description.len() {
        if at + 8 > description.len() {
            return None;
        }
        let kind = u32_at(description, at);
        let size = usize::try_from(u32_at(description, at + 4)).ok()?;
        let data = at + 8;
        let end = data.checked_add(size)?;
        if end > description.len() {
            return None;
        }
        if kind == GNU_PROPERTY_X86_ISA_1_NEEDED {
            if size != 4 {
                return None;
            }
            values.push(u32_at(description, data));
        }
        at = round_up(end, 8)?;
    }
    Some(values)
}

/// `value` rounded up to a multiple of `align`.
fn round_up(value: usize, align: usize) -> Option<usize> {
    Some(value.checked_add(align - 1)? / align * align)
}

/// The little-endian `u16` at `at`; the caller has checked the length.
fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

/// The little-endian `u32` at `at`; the caller has checked the length.
fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// The little-endian `u64` at `at`; the caller has checked the length.
fn u64_at(bytes: &[u8], at: usize) -> u64 {
    let mut word = [0_u8; 8];
    word.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(word)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A minimal little-endian ELF64 x86-64 image: a `PT_INTERP` naming
    /// `interpreter`, where given, and a `PT_GNU_PROPERTY` segment whose one
    /// property is `GNU_PROPERTY_X86_ISA_1_NEEDED` = `needed`, where given.
    pub(crate) fn image(interpreter: Option<&Path>, needed: Option<u32>) -> Vec<u8> {
        let interp = interpreter.map(|path| {
            let mut bytes = path.as_os_str().as_bytes().to_vec();
            bytes.push(0);
            bytes
        });
        let note = needed.map(|value| {
            let mut bytes = Vec::new();
            for word in [4, 16, NT_GNU_PROPERTY_TYPE_0] {
                bytes.extend(u32::to_le_bytes(word));
            }
            bytes.extend(*b"GNU\0");
            for word in [GNU_PROPERTY_X86_ISA_1_NEEDED, 4, value, 0] {
                bytes.extend(u32::to_le_bytes(word));
            }
            bytes
        });
        let count = u16::from(interp.is_some()) + u16::from(note.is_some());
        let mut file = vec![0_u8; 64 + 56 * usize::from(count)];
        file[..4].copy_from_slice(b"\x7fELF");
        file[4] = 2;
        file[5] = 1;
        file[6] = 1;
        file[16..18].copy_from_slice(&3_u16.to_le_bytes());
        file[18..20].copy_from_slice(&EM_X86_64.to_le_bytes());
        file[0x20..0x28].copy_from_slice(&64_u64.to_le_bytes());
        file[0x34..0x36].copy_from_slice(&64_u16.to_le_bytes());
        file[0x36..0x38].copy_from_slice(&56_u16.to_le_bytes());
        file[0x38..0x3a].copy_from_slice(&count.to_le_bytes());
        let mut slot = 0;
        let mut header = |file: &mut Vec<u8>, kind: u32, bytes: &[u8], align: u64| {
            while !file.len().is_multiple_of(8) {
                file.push(0);
            }
            let offset = file.len() as u64;
            file.extend(bytes);
            let at = 64 + 56 * slot;
            file[at..at + 4].copy_from_slice(&kind.to_le_bytes());
            file[at + 8..at + 16].copy_from_slice(&offset.to_le_bytes());
            file[at + 32..at + 40].copy_from_slice(&(bytes.len() as u64).to_le_bytes());
            file[at + 48..at + 56].copy_from_slice(&align.to_le_bytes());
            slot += 1;
        };
        if let Some(bytes) = &interp {
            header(&mut file, PT_INTERP, bytes, 1);
        }
        if let Some(bytes) = &note {
            header(&mut file, PT_GNU_PROPERTY, bytes, 8);
        }
        file
    }

    /// A fresh directory for one test.
    fn scratch(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("libc-isa-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    /// A glibc layout under `root`: `lib/ld-linux-x86-64.so.2`, a
    /// `lib/libc.so.6` declaring `libc_needs` where given, and the program
    /// `program`, whose interpreter is that loader. Returns the program.
    pub(crate) fn glibc_layout(root: &Path, libc_needs: Option<u32>) -> PathBuf {
        let lib = root.join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        let loader = lib.join(GLIBC_X86_64_INTERPRETER);
        std::fs::write(&loader, image(None, None)).unwrap();
        if let Some(needed) = libc_needs {
            std::fs::write(lib.join(GLIBC_C_LIBRARY), image(None, Some(needed))).unwrap();
        }
        let program = root.join("program");
        std::fs::write(&program, image(Some(&loader), None)).unwrap();
        program
    }

    #[test]
    fn a_libc_needing_x86_64_v3_names_the_profile() {
        let root = scratch("v3");
        let program = glibc_layout(&root, Some(0b111));
        assert_eq!(libc_needed_level(&program), Some(3));
        assert_eq!(replay_profile_arg(&program), Some(LIBC_V3_PROFILE_ARG));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_libc_needing_less_or_more_names_none() {
        for (needed, level) in [(0b11, 2), (0b1111, 4), (0b1, 1), (0, 0)] {
            let root = scratch(&format!("level-{needed}"));
            let program = glibc_layout(&root, Some(needed));
            assert_eq!(libc_needed_level(&program), Some(level), "{needed:#b}");
            assert_eq!(replay_profile_arg(&program), None, "{needed:#b}");
            std::fs::remove_dir_all(&root).unwrap();
        }
    }

    #[test]
    fn an_unmarked_or_missing_libc_names_none() {
        let root = scratch("unmarked");
        let program = glibc_layout(&root, None);
        assert_eq!(libc_needed_level(&program), None, "no libc.so.6");
        std::fs::write(root.join("lib").join(GLIBC_C_LIBRARY), image(None, None)).unwrap();
        assert_eq!(libc_needed_level(&program), None, "unmarked libc.so.6");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_malformed_libc_note_names_none() {
        let root = scratch("malformed");
        let program = glibc_layout(&root, Some(0b111));
        let libc = root.join("lib").join(GLIBC_C_LIBRARY);
        let mut bytes = std::fs::read(&libc).unwrap();
        // The note's description size, now past the end of its segment.
        let note = bytes.len() - 32;
        bytes[note + 4..note + 8].copy_from_slice(&64_u32.to_le_bytes());
        std::fs::write(&libc, bytes).unwrap();
        assert_eq!(libc_needed_level(&program), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn only_glibcs_interpreter_and_a_dynamic_image_are_read() {
        let root = scratch("not-glibc");
        let lib = root.join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(lib.join(GLIBC_C_LIBRARY), image(None, Some(0b111))).unwrap();
        let musl = lib.join("ld-musl-x86_64.so.1");
        std::fs::write(&musl, image(None, None)).unwrap();
        let program = root.join("program");
        std::fs::write(&program, image(Some(&musl), None)).unwrap();
        assert_eq!(libc_needed_level(&program), None, "musl's interpreter");
        std::fs::write(&program, image(None, Some(0b111))).unwrap();
        assert_eq!(libc_needed_level(&program), None, "a static image");
        std::fs::write(&program, b"not an image\n").unwrap();
        assert_eq!(libc_needed_level(&program), None, "not an image");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_libc_beside_the_resolved_interpreter_is_read() {
        let root = scratch("resolved");
        // The loader the program names is a symlink into another directory,
        // whose libc.so.6 needs x86-64-v3; the one beside the symlink needs
        // x86-64-v2.
        let real = root.join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("ld-2.39.so"), image(None, None)).unwrap();
        std::fs::write(real.join(GLIBC_C_LIBRARY), image(None, Some(0b111))).unwrap();
        let lib = root.join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        std::os::unix::fs::symlink(real.join("ld-2.39.so"), lib.join(GLIBC_X86_64_INTERPRETER))
            .unwrap();
        std::fs::write(lib.join(GLIBC_C_LIBRARY), image(None, Some(0b11))).unwrap();
        let program = root.join("program");
        std::fs::write(
            &program,
            image(Some(&lib.join(GLIBC_X86_64_INTERPRETER)), None),
        )
        .unwrap();
        assert_eq!(libc_needed_level(&program), Some(3));
        // A symlinked directory, as /lib64 is on CentOS, reaches the same
        // libc.so.6.
        std::os::unix::fs::symlink(&lib, root.join("lib64")).unwrap();
        std::fs::write(
            &program,
            image(
                Some(&root.join("lib64").join(GLIBC_X86_64_INTERPRETER)),
                None,
            ),
        )
        .unwrap();
        assert_eq!(libc_needed_level(&program), Some(3));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn scripts_are_followed_to_the_image_the_kernel_execs() {
        let root = scratch("script");
        let program = glibc_layout(&root, Some(0b111));
        let script = root.join("script");
        std::fs::write(&script, format!("#! {} -x\nbody\n", program.display())).unwrap();
        assert_eq!(libc_needed_level(&script), Some(3));
        // A chain of MAX_SCRIPT_HOPS scripts is followed; one more is not.
        let mut previous = script;
        for hop in 2..=MAX_SCRIPT_HOPS + 1 {
            let next = root.join(format!("script-{hop}"));
            std::fs::write(&next, format!("#!{}\n", previous.display())).unwrap();
            let expected = (hop <= MAX_SCRIPT_HOPS).then_some(3);
            assert_eq!(libc_needed_level(&next), expected, "{hop} scripts");
            previous = next;
        }
        std::fs::write(&previous, "#!relative/interpreter\n").unwrap();
        assert_eq!(libc_needed_level(&previous), None, "a relative interpreter");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A relative `PT_INTERP` names none, even one that resolves, from the
    /// harness's working directory, to a glibc loader beside a C library
    /// needing x86-64-v3: the kernel resolves it against the guest's.
    #[test]
    fn a_relative_interpreter_names_none() {
        let root = scratch("relative");
        let program = glibc_layout(&root, Some(0b111));
        assert_eq!(libc_needed_level(&program), Some(3));
        let loader = root.join("lib").join(GLIBC_X86_64_INTERPRETER);
        // From this process's working directory up to `/`, then down to the
        // loader.
        let cwd = std::env::current_dir().unwrap();
        let mut relative = PathBuf::new();
        for _ in cwd
            .components()
            .filter(|part| matches!(part, std::path::Component::Normal(_)))
        {
            relative.push("..");
        }
        relative.push(loader.strip_prefix("/").unwrap());
        assert!(relative.is_relative(), "{}", relative.display());
        assert_eq!(
            std::fs::canonicalize(&relative).unwrap(),
            std::fs::canonicalize(&loader).unwrap()
        );
        std::fs::write(&program, image(Some(&relative), None)).unwrap();
        assert_eq!(libc_needed_level(&program), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A FIFO is never opened, where the program, a script's interpreter or
    /// the C library would be: opening one for reading would block until a
    /// writer came. Each read returns at once.
    #[test]
    fn a_fifo_is_never_opened() {
        let root = scratch("fifo");
        let program = glibc_layout(&root, None);
        let fifo = |path: &Path| {
            let status = std::process::Command::new("mkfifo")
                .arg(path)
                .status()
                .unwrap();
            assert!(status.success(), "mkfifo {}", path.display());
        };
        fifo(&root.join("lib").join(GLIBC_C_LIBRARY));
        let program_fifo = root.join("program-fifo");
        fifo(&program_fifo);
        let interpreter_fifo = root.join("interpreter-fifo");
        fifo(&interpreter_fifo);
        let script = root.join("script");
        std::fs::write(&script, format!("#!{}\n", interpreter_fifo.display())).unwrap();
        for path in [program, program_fifo, script] {
            let (sender, receiver) = std::sync::mpsc::channel();
            let read = path.clone();
            std::thread::spawn(move || sender.send(libc_needed_level(&read)).unwrap());
            let level = receiver
                .recv_timeout(std::time::Duration::from_secs(30))
                .unwrap_or_else(|_| panic!("reading {} blocked", path.display()));
            assert_eq!(level, None, "{}", path.display());
        }
        std::fs::remove_dir_all(&root).unwrap();
    }
}
