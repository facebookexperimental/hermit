/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The image a process executes, which its `/proc/<pid>/exe` link names.
//!
//! Detcore records it at each exec, on every backend, and checks it against
//! the kernel's own link when the exec completes (`tool_global`). Where the
//! backend's own procfs is not the guest's kernel procfs (KVM), the caller's
//! own link (`/proc/self/exe` and its other spellings) is answered from the
//! record while it still names exactly the image the process executes
//! ([`ExecRecord::names_path_exactly`]; `Detcore::readlink_own_task_link`,
//! package K slice E1); otherwise it is answered as before. Where the
//! kernel's link names a launcher in the guest's place (SaBRe's loader,
//! DynamoRIO's library; `Detcore::launched_exe`), the record answers every
//! process's link while the image is at its path, as before. The kernel's
//! link stands elsewhere.
//!
//! Only the path is recorded. Where the image ends, which a program-break
//! model would need, is known only once the image is loaded, and nothing
//! reads it yet.

use std::fs;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

/// The bytes of a file the kernel reads to find its format
/// (`BINPRM_BUF_SIZE`, include/linux/binfmts.h).
const BINPRM_BUF_SIZE: usize = 256;

/// The interpreter levels the kernel follows before it refuses an exec with
/// `ELOOP` (`exec_binprm`'s `depth > 5`, fs/exec.c).
const MAX_INTERPRETER_DEPTH: usize = 5;

/// The image a process executes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecRecord {
    /// The image's canonical path, as the process's root directory shows it:
    /// what `readlink("/proc/self/exe")` returns while the file is there.
    pub exe: PathBuf,
    /// The image file's device and inode when it was exec'd, which tell
    /// whether the file is still at `exe` ([`Self::is_present`]).
    dev: u64,
    ino: u64,
    /// Its link count and inode change time then: a rename, link or unlink
    /// of the file since changes both, even where the same inode is put back
    /// at `exe`, which Linux's link would no longer name.
    nlink: u64,
    ctime: (i64, i64),
    /// The device and inode of each directory above the image, from its
    /// parent up to the process's root directory, when it was exec'd
    /// ([`Self::names_path_exactly`]).
    directories: Vec<(u64, u64)>,
}

/// The device and inode of each directory above `path`, a canonical path
/// below `root`, from its parent up to `root`, not following a symbolic link
/// at any of them, read with raw syscalls (in-guest Detcore must not call
/// libc's wrappers: `crate::util::raw_syscall`). `None` where one cannot be
/// read or is no directory ([`directory_sample`]).
fn directory_chain(root: &Path, path: &Path) -> Option<Vec<(u64, u64)>> {
    path.ancestors()
        .skip(1)
        .take_while(|directory| directory.starts_with(root))
        .map(|directory| directory_sample(&crate::util::raw_lstat(directory).ok()?))
        .collect()
}

/// The device and inode of a directory, from its unfollowed status: `None`
/// for anything but a directory, so that another kind of file put in a
/// directory's place never matches it, even where it reuses the freed
/// directory's inode number (a symbolic link to where the directory went).
fn directory_sample(stat: &libc::stat) -> Option<(u64, u64)> {
    (stat.st_mode & libc::S_IFMT == libc::S_IFDIR).then_some((stat.st_dev, stat.st_ino))
}

impl ExecRecord {
    /// The image an `execve` of `path` runs, as the kernel finds it: `path`
    /// taken from the working directory `cwd`, every symbolic link followed,
    /// and for a script the interpreter its `#!` line names, level by level,
    /// as `/proc/<pid>/exe` names the interpreter. `root` is the process's
    /// root directory as the caller sees it, and `cwd` is the working
    /// directory below it, absolute.
    ///
    /// `None` where the kernel would refuse the exec (no such file, a
    /// malformed `#!` line, too many interpreter levels), where the caller
    /// cannot look the path up, and for a file the caller cannot read, which
    /// the kernel may still run (an execute-only binary, or a script whose
    /// `#!` line the kernel reads without a permission check): the link is
    /// then the kernel's to answer. A resolution can still be wrong where
    /// the kernel runs a file differently (a `binfmt_misc` handler) or the
    /// caller sees another filesystem; `tool_global` checks every record
    /// against the kernel's own link after the exec.
    pub fn resolve(root: &Path, cwd: &Path, path: &Path) -> Option<Self> {
        Self::resolve_from(root, Some(cwd), path)
    }

    /// [`Self::resolve`] for a caller whose working directory may be unknown
    /// (`cwd` is `None`): then `None` wherever the kernel would look a
    /// relative path up from it (a relative `path`, or a script whose `#!`
    /// line names a relative interpreter).
    pub fn resolve_from(root: &Path, cwd: Option<&Path>, path: &Path) -> Option<Self> {
        let from_cwd = |path: &Path| {
            if path.is_absolute() {
                Some(path.to_path_buf())
            } else {
                Some(cwd?.join(path))
            }
        };
        let mut spelled = from_cwd(path)?;
        for _ in 0..=MAX_INTERPRETER_DEPTH {
            let found = crate::util::canonicalize(&below(root, &spelled)?).ok()?;
            // Linux execs only a regular file, and refuses anything else
            // (EACCES) before it opens it.
            if !fs::metadata(&found).ok()?.is_file() {
                return None;
            }
            match script_interpreter(&found) {
                Script::Not => return Self::at(root, &found),
                // The kernel opens the interpreter from the caller's working
                // directory, as it opened the script.
                Script::Interpreter(interpreter) => spelled = from_cwd(&interpreter)?,
                Script::Declined | Script::Unread => return None,
            }
        }
        None
    }

    /// The record of the image at `found`, a canonical path the caller sees
    /// below `root`, as that file is now.
    pub fn at(root: &Path, found: &Path) -> Option<Self> {
        let metadata = fs::metadata(found).ok()?;
        Some(Self {
            exe: Path::new("/").join(found.strip_prefix(root).ok()?),
            dev: metadata.dev(),
            ino: metadata.ino(),
            nlink: metadata.nlink(),
            ctime: (metadata.ctime(), metadata.ctime_nsec()),
            directories: directory_chain(root, found).unwrap_or_default(),
        })
    }

    /// The image file's device and inode when it was exec'd.
    pub fn identity(&self) -> (u64, u64) {
        (self.dev, self.ino)
    }

    /// Whether the image's file is still at its path, untouched, for a
    /// process whose root directory the caller sees at `root`: the same
    /// inode, with the link count and change time it had at the exec. A
    /// rename or link of the image since, or a change of its mode, reads as
    /// absent, so the caller refuses rather than name a path Linux's link
    /// might not.
    pub fn is_present(&self, root: &Path) -> bool {
        below(root, &self.exe)
            .and_then(|path| fs::symlink_metadata(path).ok())
            .is_some_and(|now| {
                (
                    now.dev(),
                    now.ino(),
                    now.nlink(),
                    (now.ctime(), now.ctime_nsec()),
                ) == (self.dev, self.ino, self.nlink, self.ctime)
            })
    }

    /// Whether the image's path still names exactly the file the process
    /// executes, as Linux's `exe` link renders it (`d_path` of the image's
    /// dentry), for a process whose root directory the caller sees at
    /// `root`: the file is untouched at its path ([`Self::is_present`]), and
    /// every directory above it is the one that was there at the exec (its
    /// device and inode, not following a symbolic link). A directory has one
    /// dentry, so the same directory inode at a path is that path's dentry,
    /// and the same untouched file in the same directory under the same name
    /// is the same dentry: a rename of the image or of any directory above
    /// it, another directory or a symbolic link put at one of their paths,
    /// and a hard link of the image put back at its path all read as not
    /// exact. (Mounts are refused in the guest.)
    pub fn names_path_exactly(&self, root: &Path) -> bool {
        self.is_present(root)
            && !self.directories.is_empty()
            && below(root, &self.exe)
                .and_then(|path| directory_chain(root, &path))
                .is_some_and(|now| now == self.directories)
    }
}

/// What decides where Linux starts a process's program break after it execs
/// an ELF image (`load_elf_binary`, fs/binfmt_elf.c), from the image's file
/// header and program headers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ElfLayout {
    /// Whether the image is `ET_DYN` (position-independent) rather than
    /// `ET_EXEC`.
    pub position_independent: bool,
    /// Whether the image names a program interpreter (`PT_INTERP`).
    pub interpreter: bool,
    /// The virtual address of the image's program headers before the load
    /// bias, as Linux finds it for `AT_PHDR` (since Linux 5.18): `e_phoff` in
    /// the last `PT_LOAD` segment whose file range holds it, or 0 where none
    /// does (`AT_PHDR` is then the bias itself).
    pub phdr: u64,
    /// The end of the highest `PT_LOAD` segment in memory, `p_vaddr +
    /// p_memsz`, before the load bias: the end of its bss.
    pub end: u64,
    /// The first executable `PT_LOAD` segment's file offset and virtual
    /// address, each page-aligned down: where its text mapping starts in the
    /// file and, before the load bias, in memory. `None` without one.
    pub text: Option<(u64, u64)>,
    /// `mm->end_data - mm->start_data` as Linux sets them: the highest
    /// segment end in the file (`p_vaddr + p_filesz`) less the highest
    /// segment start, which `RLIMIT_DATA` counts with the heap
    /// (`check_data_rlimit`).
    pub data_span: u64,
    /// The page size Linux maps the segments with and aligns the program
    /// break to: `ELF_MIN_ALIGN`, the larger of `ELF_EXEC_PAGESIZE` and
    /// `PAGE_SIZE` (fs/binfmt_elf.c), which is the host's page size, since
    /// `ELF_EXEC_PAGESIZE` is `PAGE_SIZE` on arm64 and 4096 on x86_64.
    pub page: u64,
}

impl ElfLayout {
    /// The layout of the ELF64 image at `path` (`ET_EXEC` or `ET_DYN`), or
    /// `None` for any other file, one without a loadable segment, and one the
    /// caller cannot read.
    pub fn read(path: &Path) -> Option<Self> {
        Self::read_file(&crate::util::RawFd::open(path, libc::O_RDONLY).ok()?)
    }

    /// [`Self::read`] of an open file, read with raw syscalls: in-guest
    /// Detcore must not call libc's wrappers (`crate::util::raw_syscall`).
    pub fn read_file(file: &crate::util::RawFd) -> Option<Self> {
        let read_exact = |offset: u64, buffer: &mut [u8]| {
            (file.read_at(buffer, offset).ok()? == buffer.len()).then_some(())
        };
        let mut header = [0_u8; 64];
        read_exact(0, &mut header)?;
        Self::parse(&header, read_exact, crate::page::page_size())
    }

    /// [`Self::read`] of an image whose 64-byte file header is `header`,
    /// reading its program headers through `read_at(offset, buffer)`, on a
    /// host whose pages are `page` bytes.
    fn parse(
        header: &[u8; 64],
        mut read_at: impl FnMut(u64, &mut [u8]) -> Option<()>,
        page: u64,
    ) -> Option<Self> {
        let u16_at = |at: usize| u16::from_le_bytes([header[at], header[at + 1]]);
        let u64_at = |at: usize| u64::from_le_bytes(header[at..at + 8].try_into().unwrap());
        // ELF magic, 64-bit class, little-endian data, ET_EXEC or ET_DYN.
        if header[..4] != *b"\x7fELF" || header[4] != 2 || header[5] != 1 {
            return None;
        }
        let position_independent = match u16_at(16) {
            2 => false, // ET_EXEC
            3 => true,  // ET_DYN
            _ => return None,
        };
        let (phoff, phentsize, phnum) = (u64_at(32), usize::from(u16_at(54)), u16_at(56));
        if phentsize < 56 {
            return None;
        }
        let mut entry = vec![0_u8; phentsize];
        let (mut phdr, mut end, mut interpreter) = (0, None::<u64>, false);
        let (mut text, mut start_data, mut end_data) = (None::<(u64, u64)>, 0_u64, 0_u64);
        for index in 0..u64::from(phnum) {
            read_at(phoff.checked_add(index * phentsize as u64)?, &mut entry)?;
            let field = |at: usize| u64::from_le_bytes(entry[at..at + 8].try_into().unwrap());
            match u32::from_le_bytes(entry[..4].try_into().unwrap()) {
                1 => {} // PT_LOAD
                3 => {
                    // PT_INTERP
                    interpreter = true;
                    continue;
                }
                _ => continue,
            }
            let (offset, vaddr, filesz, memsz) = (field(8), field(16), field(32), field(40));
            // PF_X.
            if text.is_none() && u32::from_le_bytes(entry[4..8].try_into().unwrap()) & 1 != 0 {
                text = Some((offset & !(page - 1), vaddr & !(page - 1)));
            }
            if offset <= phoff && phoff < offset.checked_add(filesz)? {
                phdr = (phoff - offset).checked_add(vaddr)?;
            }
            start_data = start_data.max(vaddr);
            end_data = end_data.max(vaddr.checked_add(filesz)?);
            let segment_end = vaddr.checked_add(memsz)?;
            end = Some(end.map_or(segment_end, |end| end.max(segment_end)));
        }
        Some(Self {
            position_independent,
            interpreter,
            phdr,
            end: end?,
            text,
            data_span: end_data.saturating_sub(start_data),
            page,
        })
    }

    /// Where Linux starts the program break of a process that exec'd this
    /// image, given the `AT_PHDR` its exec reported, with address
    /// randomization off: the end of the highest loaded segment at the load
    /// bias, page-aligned up (`mm->start_brk = mm->brk =
    /// ELF_PAGEALIGN(elf_brk)`), or why there is no answer.
    pub fn start_brk(&self, at_phdr: u64) -> Result<u64, &'static str> {
        let bias = at_phdr
            .checked_sub(self.phdr)
            .ok_or("AT_PHDR below the image's program headers")?;
        self.start_brk_at_bias(bias)
    }

    /// [`Self::start_brk`] for an image loaded at load bias `bias`.
    pub fn start_brk_at_bias(&self, bias: u64) -> Result<u64, &'static str> {
        if self.position_independent && !self.interpreter {
            // Recent kernels (6.17's load_elf_binary) move it to
            // ELF_ET_DYN_BASE even without randomization, unless built with
            // CONFIG_COMPAT_BRK.
            return Err(
                "a position-independent image without an interpreter, whose break depends on the kernel's build",
            );
        }
        bias.checked_add(self.end)
            .and_then(|end| end.checked_add(self.page - 1))
            .map(|end| end & !(self.page - 1))
            .ok_or("an image ending past the address space")
    }
}

/// What an x86-64 ELF image declares it needs of the CPU: the
/// `GNU_PROPERTY_X86_ISA_1_NEEDED` property of its `NT_GNU_PROPERTY_TYPE_0`
/// note (elf.h; the x86-64 psABI's GNU property section), which glibc's
/// loader checks against the CPUID it sees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IsaNeeds {
    /// No such property. The image declares nothing, which does not prove it
    /// runs on any CPU: a library built for a higher level need not say so.
    Unmarked,
    /// The highest x86-64 microarchitecture level its needed bits name:
    /// 1 baseline, 2 x86-64-v2, 3 x86-64-v3, 4 x86-64-v4; 0 for an empty set.
    /// A level above 4 is a bit this Hermit does not know, a level above
    /// x86-64-v4.
    Level(u8),
    /// A declaration that cannot be read as written: a truncated or
    /// misaligned note, or a repeated property.
    Malformed(&'static str),
}

/// `PT_GNU_PROPERTY` (elf.h), which holds the image's
/// `NT_GNU_PROPERTY_TYPE_0` note.
const PT_GNU_PROPERTY: u32 = 0x6474_e553;
/// `PT_NOTE`, read for that note where no `PT_GNU_PROPERTY` segment exists.
const PT_NOTE: u32 = 4;
/// `NT_GNU_PROPERTY_TYPE_0`.
const NT_GNU_PROPERTY_TYPE_0: u32 = 5;
/// `GNU_PROPERTY_X86_ISA_1_NEEDED`.
const GNU_PROPERTY_X86_ISA_1_NEEDED: u32 = 0xc000_8002;
/// `EM_X86_64`.
const EM_X86_64: u16 = 62;
/// The largest `NT_GNU_PROPERTY_TYPE_0` description read; a larger one
/// leaves the image's declaration unknown rather than read without bound.
/// Other notes' descriptions are skipped, not read, whatever their size.
const MAX_PROPERTY_DESCRIPTION: usize = 64 * 1024;
/// The most notes walked in one segment, each at least 12 bytes; past it
/// the rest of the segment is unknown.
const MAX_NOTES: usize = 4096;

/// What one image's note segments showed: the declaration found, with the
/// file offset of its note; whether two different notes declared one; and
/// the first reason some other part could not be read.
#[derive(Default)]
struct NoteWalk {
    found: Option<(u64, IsaNeeds)>,
    conflict: bool,
    unknown: Option<&'static str>,
}

impl NoteWalk {
    fn declare(&mut self, at: u64, level: IsaNeeds) {
        match self.found {
            // One note seen through two segments is one declaration.
            Some((seen, _)) if seen == at => {}
            Some(_) => self.conflict = true,
            None => self.found = Some((at, level)),
        }
    }

    fn unknown(&mut self, why: &'static str) {
        self.unknown.get_or_insert(why);
    }

    /// A declaration read is kept whatever else could not be read: an
    /// unrelated note's failure says nothing about it. Only where none was
    /// read does a failure make the whole image's declaration unknown.
    fn result(self) -> IsaNeeds {
        match (self.found, self.unknown) {
            _ if self.conflict => IsaNeeds::Malformed("more than one x86 ISA needed property"),
            (Some((_, level)), _) => level,
            (None, Some(why)) => IsaNeeds::Malformed(why),
            (None, None) => IsaNeeds::Unmarked,
        }
    }

    /// [`Self::result`] for a reader that must refuse incomplete evidence:
    /// a declaration found beside a later scan state the reader could not
    /// decide (a truncated or unreadable property) is itself malformed
    /// there, because the later state could hide a higher need. The known
    /// declaration stays the lower bound [`Self::result`] reports for a
    /// refusal; completeness is kept separately, here.
    fn result_strict(self) -> IsaNeeds {
        match (self.found, self.unknown) {
            _ if self.conflict => IsaNeeds::Malformed("more than one x86 ISA needed property"),
            (Some(_), Some(why)) => IsaNeeds::Malformed(why),
            (Some((_, level)), None) => level,
            (None, Some(why)) => IsaNeeds::Malformed(why),
            (None, None) => IsaNeeds::Unmarked,
        }
    }
}

impl IsaNeeds {
    /// What the x86-64 ELF64 image open as `file` declares, read with raw
    /// syscalls as [`ElfLayout::read_file`] reads. `None` for a file that is
    /// not a little-endian ELF64 `EM_X86_64` image (nothing for this policy
    /// to read) and for one the caller cannot read.
    pub fn read_file(file: &crate::util::RawFd) -> Option<Self> {
        let read_exact = |offset: u64, buffer: &mut [u8]| {
            (file.read_at(buffer, offset).ok()? == buffer.len()).then_some(())
        };
        let mut header = [0_u8; 64];
        read_exact(0, &mut header)?;
        Self::parse(&header, None, read_exact)
    }

    /// [`Self::read_file`] of the file at `path`.
    pub fn read_path(path: &Path) -> Option<Self> {
        Self::read_file(&crate::util::RawFd::open(path, libc::O_RDONLY).ok()?)
    }

    /// [`Self::read_file`], refusing incomplete scan evidence: a found
    /// declaration beside a scan state the reader could not decide (a
    /// truncated or unreadable later property) reads as
    /// [`Self::Malformed`], so an automatic consumer cannot select a
    /// profile from a lower bound alone. An explicit consumer that only
    /// needs the lower bound for its refusal keeps [`Self::read_file`].
    pub fn read_file_strict(file: &crate::util::RawFd) -> Option<Self> {
        let read_exact = |offset: u64, buffer: &mut [u8]| {
            (file.read_at(buffer, offset).ok()? == buffer.len()).then_some(())
        };
        let mut header = [0_u8; 64];
        read_exact(0, &mut header)?;
        Self::parse_inner(&header, None, read_exact, true)
    }

    /// [`Self::read_file_strict`] of the file at `path`.
    pub fn read_path_strict(path: &Path) -> Option<Self> {
        Self::read_file_strict(&crate::util::RawFd::open(path, libc::O_RDONLY).ok()?)
    }

    /// What the image a process has mapped at load bias `bias`, with its
    /// file header at `header` (where its segment holding file offset 0 is
    /// mapped: `bias` plus that segment's address), declares, read through
    /// `read_memory(address, buffer)` from the process's memory: the bytes
    /// the loader itself reads its properties from. The program headers must
    /// show a `PT_LOAD` mapping file offset 0 at `header - bias` and holding
    /// them, so that what was read is known to be this image's. `None` for
    /// an image mapped otherwise, one that is not x86-64 ELF64, and one whose
    /// header or program headers are unreadable.
    pub fn read_mapped(
        bias: u64,
        header: u64,
        mut read_memory: impl FnMut(u64, &mut [u8]) -> Option<()>,
    ) -> Option<Self> {
        let header_vaddr = header.checked_sub(bias)?;
        let mut bytes = [0_u8; 64];
        read_memory(header, &mut bytes)?;
        Self::parse(&bytes, Some((bias, header_vaddr)), read_memory)
    }

    /// [`Self::read_file`] of an image whose file header is `header`, reading
    /// the rest through `read_at(address, buffer)`: at file offsets, or where
    /// `mapped` gives the load bias and the address its file header is
    /// mapped at before the bias, at the addresses the mapped image has them
    /// ([`Self::read_mapped`]).
    fn parse(
        header: &[u8; 64],
        mapped: Option<(u64, u64)>,
        read_at: impl FnMut(u64, &mut [u8]) -> Option<()>,
    ) -> Option<Self> {
        Self::parse_inner(header, mapped, read_at, false)
    }

    fn parse_inner(
        header: &[u8; 64],
        mapped: Option<(u64, u64)>,
        mut read_at: impl FnMut(u64, &mut [u8]) -> Option<()>,
        strict: bool,
    ) -> Option<Self> {
        let u16_at = |at: usize| u16::from_le_bytes([header[at], header[at + 1]]);
        let u64_at = |at: usize| u64::from_le_bytes(header[at..at + 8].try_into().unwrap());
        if header[..4] != *b"\x7fELF" || header[4] != 2 || header[5] != 1 || u16_at(18) != EM_X86_64
        {
            return None;
        }
        let (phoff, phentsize, phnum) = (u64_at(32), usize::from(u16_at(54)), u16_at(56));
        if phentsize < 56 {
            return None;
        }
        let headers_end = (phentsize as u64)
            .checked_mul(u64::from(phnum))
            .and_then(|size| size.checked_add(phoff))?;
        // Where file offset `offset` of the image (in its first segment) and
        // address `vaddr` are read.
        let at_offset = |offset: u64| match mapped {
            None => Some(offset),
            Some((bias, header_vaddr)) => bias.checked_add(header_vaddr)?.checked_add(offset),
        };
        let at_vaddr = |offset: u64, vaddr: u64| match mapped {
            None => Some(offset),
            Some((bias, _)) => bias.checked_add(vaddr),
        };
        // Every property and note segment, each `(offset, vaddr, size,
        // align)`, and whether the file header is mapped where it was read.
        let (mut segments, mut properties, mut header_mapped) = (Vec::new(), 0, false);
        let mut entry = vec![0_u8; phentsize];
        for index in 0..u64::from(phnum) {
            let offset = phoff.checked_add(index * phentsize as u64)?;
            read_at(at_offset(offset)?, &mut entry)?;
            let field = |at: usize| u64::from_le_bytes(entry[at..at + 8].try_into().unwrap());
            let (offset, vaddr, filesz) = (field(8), field(16), field(32));
            match u32::from_le_bytes(entry[..4].try_into().unwrap()) {
                // PT_LOAD: the program headers are where they were read only
                // if a segment maps them, from file offset 0, at the address
                // the header was read at.
                1 => {
                    header_mapped |= offset == 0
                        && mapped.is_some_and(|(_, header_vaddr)| vaddr == header_vaddr)
                        && headers_end <= filesz
                }
                // Offset, address, size and alignment (p_align), which pads
                // every note's name and description in the segment (4, or 8
                // for the ELF64 property notes).
                kind @ (PT_GNU_PROPERTY | PT_NOTE) => {
                    properties += usize::from(kind == PT_GNU_PROPERTY);
                    segments.push((offset, vaddr, filesz, field(48)));
                }
                _ => {}
            }
        }
        if mapped.is_some() && !header_mapped {
            return None;
        }
        if properties > 1 {
            return Some(Self::Malformed("more than one PT_GNU_PROPERTY segment"));
        }
        // A linker gives the property note both a PT_GNU_PROPERTY segment
        // and a PT_NOTE one (or one PT_NOTE with the image's other notes):
        // every segment is read, so that neither hides a declaration from
        // the other, and a note is told apart by its place in the file, so
        // that one declaration seen twice is not two.
        let mut walk = NoteWalk::default();
        for (offset, vaddr, size, align) in segments {
            let align = match align {
                // An unaligned note segment is laid out as a 4-aligned one.
                0..=4 => 4,
                8 => 8,
                _ => {
                    walk.unknown("a note segment aligned to neither 4 nor 8");
                    continue;
                }
            };
            match at_vaddr(offset, vaddr) {
                Some(address) => {
                    Self::walk_notes(&mut walk, address, offset, size, align, &mut read_at)
                }
                None => walk.unknown("a note segment past the address space"),
            }
        }
        Some(if strict {
            walk.result_strict()
        } else {
            walk.result()
        })
    }

    /// Walk the notes of one segment of `size` bytes, read at `address` and
    /// at file offset `offset`, their names and descriptions each padded to
    /// `align` (as glibc's ELF_NOTE_DESC_OFFSET and ELF_NOTE_NEXT_OFFSET pad
    /// them: the description starts, and the next note follows it, at the
    /// next multiple of `align` from the note's start). Only a GNU property
    /// note's description is read; every other note is skipped by its sizes,
    /// whatever they are, so an unrelated note cannot hide a declaration.
    fn walk_notes(
        walk: &mut NoteWalk,
        address: u64,
        offset: u64,
        size: u64,
        align: u64,
        read_at: &mut impl FnMut(u64, &mut [u8]) -> Option<()>,
    ) {
        let mut cursor = 0_u64;
        for _ in 0..MAX_NOTES {
            if cursor == size {
                return;
            }
            if size - cursor < 12 {
                return walk.unknown("a truncated note header");
            }
            let mut head = [0_u8; 12];
            if address
                .checked_add(cursor)
                .and_then(|at| read_at(at, &mut head))
                .is_none()
            {
                return walk.unknown("a note past the end of the file or unmapped");
            }
            let word =
                |at: usize| u64::from(u32::from_le_bytes(head[at..at + 4].try_into().unwrap()));
            let (namesz, descsz, kind) = (word(0), word(4), word(8) as u32);
            let ends = namesz
                .checked_add(12)
                .and_then(|name_end| name_end.checked_next_multiple_of(align))
                .and_then(|desc_start| {
                    let desc_end = desc_start.checked_add(descsz)?;
                    Some((desc_start, desc_end.checked_next_multiple_of(align)?))
                });
            let Some((desc_start, next)) = ends.filter(|&(_, next)| next <= size - cursor) else {
                return walk.unknown("a truncated note");
            };
            if kind == NT_GNU_PROPERTY_TYPE_0 && namesz == 4 {
                let mut name = [0_u8; 4];
                let read = address
                    .checked_add(cursor + 12)
                    .and_then(|at| read_at(at, &mut name));
                if read.is_none() {
                    return walk.unknown("a note past the end of the file or unmapped");
                }
                if name == *b"GNU\0" {
                    match usize::try_from(descsz) {
                        Ok(length) if length <= MAX_PROPERTY_DESCRIPTION => {
                            let mut description = vec![0_u8; length];
                            let read = address
                                .checked_add(cursor + desc_start)
                                .and_then(|at| read_at(at, &mut description));
                            if read.is_none() {
                                return walk.unknown("a note past the end of the file or unmapped");
                            }
                            let (level, unknown) = Self::from_properties(&description);
                            if let Some(level) = level {
                                walk.declare(offset + cursor, level);
                            }
                            if let Some(why) = unknown {
                                walk.unknown(why);
                            }
                        }
                        _ => walk.unknown("a GNU property note larger than 64 KiB"),
                    }
                }
            }
            cursor += next;
        }
        if cursor < size {
            walk.unknown("a note segment of more than 4096 notes");
        }
    }

    /// The declaration in one `NT_GNU_PROPERTY_TYPE_0` description, if it
    /// has one, and why some of it could not be read, if so. A declaration
    /// read before an unreadable property is kept.
    fn from_properties(mut properties: &[u8]) -> (Option<Self>, Option<&'static str>) {
        let mut found = None;
        while !properties.is_empty() {
            if properties.len() < 8 {
                return (found, Some("a truncated GNU property header"));
            }
            let word = |at: usize| u32::from_le_bytes(properties[at..at + 4].try_into().unwrap());
            let (kind, size) = (word(0), word(4) as usize);
            let end = size
                .checked_next_multiple_of(8)
                .and_then(|data| data.checked_add(8));
            let Some(end) = end.filter(|&end| end <= properties.len()) else {
                return (found, Some("a truncated GNU property"));
            };
            if kind == GNU_PROPERTY_X86_ISA_1_NEEDED {
                if found.is_some() {
                    return (
                        Some(Self::Malformed("more than one x86 ISA needed property")),
                        None,
                    );
                }
                if size != 4 {
                    return (None, Some("an x86 ISA needed property that is not 4 bytes"));
                }
                // The highest set bit: 1 baseline, 2 v2, 4 v3, 8 v4, and
                // above that, levels this Hermit does not know.
                found = Some(Self::Level(32 - word(8).leading_zeros() as u8));
            }
            properties = &properties[end..];
        }
        (found, None)
    }
}

/// The start and pathname of the lowest file mapping at or above `bias` in
/// `maps` (a process's `/proc/<pid>/maps`) that maps its file from offset 0:
/// where the kernel put the first segment of an image it loaded at load
/// bias `bias`, and with it the image's file header. The kernel maps that
/// segment at the bias plus its address, which need not be 0.
pub fn first_image_mapping(maps: &[u8], bias: u64) -> Option<(u64, Vec<u8>)> {
    maps.split(|&byte| byte == b'\n')
        .filter_map(|line| {
            let mut fields = line.splitn(6, |&byte| byte == b' ');
            let range = std::str::from_utf8(fields.next()?).ok()?;
            let (start, _) = range.split_once('-')?;
            let start = u64::from_str_radix(start, 16).ok()?;
            let _permissions = fields.next()?;
            let offset = u64::from_str_radix(std::str::from_utf8(fields.next()?).ok()?, 16).ok()?;
            let _device = fields.next()?;
            let inode: u64 = std::str::from_utf8(fields.next()?).ok()?.parse().ok()?;
            let path = fields
                .next()
                .unwrap_or_default()
                .trim_ascii_start()
                .to_vec();
            (start >= bias && offset == 0 && inode != 0).then_some((start, path))
        })
        .min_by_key(|&(start, _)| start)
}

/// The objects an exec ran, each `(what, declaration)` for
/// [`first_needing_above`]: the executable, and its program interpreter
/// wherever the kernel loaded one (`loader` is `Some` where `AT_BASE` is
/// set), whether or not the executable itself could be read. A declaration
/// read from the mapped interpreter is evidence of its own; it does not
/// wait on the executable's.
pub fn exec_objects(
    image: Option<IsaNeeds>,
    loader: Option<Option<IsaNeeds>>,
) -> Vec<(&'static str, Option<IsaNeeds>)> {
    let mut objects = vec![("the executable", image)];
    if let Some(loader) = loader {
        objects.push(("its program interpreter", loader));
    }
    objects
}

/// The first of an exec's `objects`, each `(what, declaration)`, that
/// declares it needs a higher x86-64 level than `level`, with the level it
/// needs. An unmarked, malformed or unread object declares nothing, so it
/// is never one: an absent declaration does not prove the image runs, and
/// the caller reports it as unknown rather than as compatible.
pub fn first_needing_above<'a>(
    level: u8,
    objects: &[(&'a str, Option<IsaNeeds>)],
) -> Option<(&'a str, u8)> {
    objects.iter().find_map(|&(what, needs)| match needs {
        Some(IsaNeeds::Level(needed)) if needed > level => Some((what, needed)),
        _ => None,
    })
}

/// The kernel's page size on x86-64, the most a `PT_INTERP` name can be.
const PAGE_SIZE: u64 = 4096;

/// The program interpreter (`PT_INTERP`) the x86-64 ELF64 image at `path`
/// names: `Ok(None)` for an image naming none, an error for a file that is
/// not one or whose `PT_INTERP` is unreadable. Read as the kernel reads it
/// (`load_elf_binary`): at most a page, at least two bytes, the last of
/// them NUL, and the name is the string up to its first NUL.
pub fn program_interpreter(path: &Path) -> Result<Option<PathBuf>, &'static str> {
    program_interpreter_of(path, EM_X86_64)
}

/// [`program_interpreter`] of an ELF64 image built for ELF machine
/// `machine` (`e_machine`) instead of x86-64: the `PT_INTERP` it names is
/// read the same way on every little-endian ELF64 machine.
pub fn program_interpreter_of(path: &Path, machine: u16) -> Result<Option<PathBuf>, &'static str> {
    use std::os::unix::ffi::OsStrExt;
    let file = crate::util::RawFd::open(path, libc::O_RDONLY).map_err(|_| "unreadable")?;
    let read_exact = |offset: u64, buffer: &mut [u8]| {
        (file.read_at(buffer, offset).ok()? == buffer.len()).then_some(())
    };
    let mut header = [0_u8; 64];
    read_exact(0, &mut header).ok_or("shorter than an ELF header")?;
    let u16_at = |at: usize| u16::from_le_bytes([header[at], header[at + 1]]);
    let u64_at = |at: usize| u64::from_le_bytes(header[at..at + 8].try_into().unwrap());
    if header[..4] != *b"\x7fELF" || header[4] != 2 || header[5] != 1 || u16_at(18) != machine {
        return Err(if machine == EM_X86_64 {
            "not an x86-64 ELF64 image"
        } else {
            "not an ELF64 image for this machine"
        });
    }
    let (phoff, phentsize, phnum) = (u64_at(32), usize::from(u16_at(54)), u16_at(56));
    if phentsize < 56 {
        return Err("program headers too small");
    }
    let mut entry = vec![0_u8; phentsize];
    for index in 0..u64::from(phnum) {
        let offset = (index * phentsize as u64)
            .checked_add(phoff)
            .ok_or("program headers past the address space")?;
        read_exact(offset, &mut entry).ok_or("program headers past the end of the file")?;
        if u32::from_le_bytes(entry[..4].try_into().unwrap()) != 3 {
            continue; // Not PT_INTERP.
        }
        let field = |at: usize| u64::from_le_bytes(entry[at..at + 8].try_into().unwrap());
        let size = field(32);
        if !(2..=PAGE_SIZE).contains(&size) {
            return Err("a PT_INTERP shorter than two bytes or larger than a page");
        }
        let mut name = vec![0_u8; size as usize];
        read_exact(field(8), &mut name).ok_or("a PT_INTERP past the end of the file")?;
        if name.last() != Some(&0) {
            return Err("a PT_INTERP without a terminating NUL");
        }
        let end = name
            .iter()
            .position(|&byte| byte == 0)
            .expect("the last byte is NUL");
        return Ok(Some(PathBuf::from(std::ffi::OsStr::from_bytes(
            &name[..end],
        ))));
    }
    Ok(None)
}

/// The named profile `--cpu-profile auto` resolves to for an exec of
/// `objects`, each `(what, declaration)`: the lowest presenting the highest
/// level any declares, x86-64-v2 where none declares one, with why. An
/// object unread or malformed, or a level no profile presents, refuses with
/// why: auto never guesses.
pub fn auto_profile(
    objects: &[(String, Option<IsaNeeds>)],
) -> Result<(detcore_model::cpu_profile::CpuProfile, String), String> {
    use detcore_model::cpu_profile::CpuProfile;
    let mut highest: Option<(&str, u8)> = None;
    for (what, needs) in objects {
        match needs {
            None => return Err(format!("{what} is unreadable or not an x86-64 ELF64 image")),
            Some(IsaNeeds::Malformed(why)) => {
                return Err(format!(
                    "{what} declares its x86 ISA level unreadably ({why})"
                ));
            }
            Some(IsaNeeds::Unmarked) => {}
            Some(IsaNeeds::Level(level)) => {
                if highest.is_none_or(|(_, highest)| *level > highest) {
                    highest = Some((what, *level));
                }
            }
        }
    }
    let level = highest.map_or(0, |(_, level)| level);
    let Some(profile) = CpuProfile::lowest_presenting(level) else {
        let (what, _) = highest.expect("a level above 3 was declared");
        return Err(format!(
            "{what} declares that it needs {}, which no CPU profile presents",
            x86_64_level_name(level)
        ));
    };
    let why = match highest {
        Some((what, level)) if level > 2 => {
            format!("{what} declares that it needs {}", x86_64_level_name(level))
        }
        _ => "nothing it execs first declares more than x86-64-v2".to_owned(),
    };
    Ok((profile, why))
}

/// The name of x86-64 level `level` as an ISA needed property gives it.
pub fn x86_64_level_name(level: u8) -> String {
    match level {
        0 | 1 => "baseline x86-64".to_owned(),
        2..=4 => format!("x86-64-v{level}"),
        _ => format!("an x86-64 level above v4 (needed bit {})", level - 1),
    }
}

/// The load bias of the image file at `path` with inode `ino`, in a
/// process whose `/proc/<pid>/maps` reads `maps`: the start of the file's
/// executable mapping at the text segment's offset less the text segment's
/// address ([`ElfLayout::text`]), or why there is none. The executable
/// mapping, because a launcher may also map the file to read it (SaBRe's
/// rewriter maps it read-only at offset 0) and may not map the image's first
/// page at all. The file is matched by inode and its whole pathname as the
/// reader sees it, not by device: maps shows the superblock's device, which
/// on btrfs and overlay is not the `st_dev` a `stat` of the file reports.
/// Where several such mappings give different biases, there is no answer.
/// This is
/// how Detcore finds an image's bias where the kernel exec'd a launcher,
/// whose `AT_PHDR` the process's auxiliary vector holds.
pub fn load_bias(
    maps: &[u8],
    path: &Path,
    ino: u64,
    (text_offset, text_page): (u64, u64),
) -> Result<u64, &'static str> {
    // Bytes, not text: a pathname in the mappings is whatever bytes the file
    // was named with.
    fn text(field: &[u8]) -> Option<&str> {
        std::str::from_utf8(field).ok()
    }
    // As the mappings show it: Linux writes a newline in a pathname as
    // `\012` (`show_map_vma`, `seq_file_path(m, file, "\n")`).
    let mut displayed = Vec::new();
    for &byte in path.as_os_str().as_bytes() {
        if byte == b'\n' {
            displayed.extend_from_slice(b"\\012");
        } else {
            displayed.push(byte);
        }
    }
    let mut biases = maps.split(|&byte| byte == b'\n').filter_map(|line| {
        let mut fields = line.splitn(6, |byte| byte.is_ascii_whitespace());
        let range = text(fields.next()?)?;
        let permissions = fields.next()?;
        let offset = u64::from_str_radix(text(fields.next()?)?, 16).ok()?;
        let _device = fields.next()?;
        let map_ino: u64 = text(fields.next()?)?.parse().ok()?;
        let pathname = fields.next()?.trim_ascii_start();
        if !permissions.contains(&b'x')
            || offset != text_offset
            || map_ino != ino
            || pathname != displayed
        {
            return None;
        }
        let start = u64::from_str_radix(range.split_once('-')?.0, 16).ok()?;
        start.checked_sub(text_page)
    });
    let bias = biases
        .next()
        .ok_or("no executable mapping of the image's text")?;
    if biases.any(|other| other != bias) {
        return Err("executable mappings of the image's text at different biases");
    }
    Ok(bias)
}

/// The kernel's `start_brk` in a `/proc/<pid>/stat` line: field 47, counted
/// past the command, which may hold spaces and parentheses.
pub fn stat_start_brk(stat: &str) -> Option<u64> {
    stat[stat.rfind(')')? + 1..]
        .split_ascii_whitespace()
        .nth(47 - 3)?
        .parse()
        .ok()
}

/// What Detcore found when it checked its model of a process's program break
/// ([`ElfLayout::start_brk`]) against the kernel's `start_brk`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrkCheck {
    /// The model equals the kernel's start.
    Equal(u64),
    /// The model differs from the kernel's start: a Detcore defect.
    Differs {
        /// Detcore's start.
        model: u64,
        /// The kernel's start.
        kernel: u64,
    },
    /// No model, for the reason given.
    Unmodeled(&'static str),
    /// Not compared: the kernel randomized the break, which Linux's rule then
    /// does not give.
    Randomized(u64),
    /// Not compared: whether the kernel randomized the break is unknown.
    RandomizationUnknown(u64),
    /// Not compared: the kernel's start cannot be read.
    KernelUnread(u64),
}

impl BrkCheck {
    /// The outcome for a model (or why there is none), whether the kernel
    /// randomized the break (`None`: unknown), and the kernel's start (`None`:
    /// unread).
    pub fn of(
        model: Result<u64, &'static str>,
        randomized: Option<bool>,
        kernel: Option<u64>,
    ) -> Self {
        let model = match model {
            Ok(model) => model,
            Err(reason) => return Self::Unmodeled(reason),
        };
        match (randomized, kernel) {
            (Some(true), _) => Self::Randomized(model),
            (None, _) => Self::RandomizationUnknown(model),
            (Some(false), None) => Self::KernelUnread(model),
            (Some(false), Some(kernel)) if kernel == model => Self::Equal(model),
            (Some(false), Some(kernel)) => Self::Differs { model, kernel },
        }
    }
}

impl std::fmt::Display for BrkCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Equal(model) => write!(f, "model {model:#x} equals the kernel's start_brk"),
            Self::Differs { model, kernel } => write!(
                f,
                "model {model:#x} differs from the kernel's start_brk {kernel:#x}"
            ),
            Self::Unmodeled(reason) => write!(f, "not modeled: {reason}"),
            Self::Randomized(model) => write!(
                f,
                "model {model:#x} not compared: the kernel randomized the break"
            ),
            Self::RandomizationUnknown(model) => write!(
                f,
                "model {model:#x} not compared: whether the kernel randomized the break is unknown"
            ),
            Self::KernelUnread(model) => write!(
                f,
                "model {model:#x} not compared: the kernel's start_brk is unreadable"
            ),
        }
    }
}

/// The value of entry `key` in an auxiliary vector read from
/// `/proc/<pid>/auxv` as `auxv`, native-endian pairs ended by `AT_NULL`.
pub fn auxv_entry(auxv: &[u8], key: u64) -> Option<u64> {
    auxv.as_chunks::<16>()
        .0
        .iter()
        .map(|pair| {
            let word = |at: usize| u64::from_ne_bytes(pair[at..at + 8].try_into().unwrap());
            (word(0), word(8))
        })
        .take_while(|&(entry, _)| entry != 0)
        .find_map(|(entry, value)| (entry == key).then_some(value))
}

/// `path`, absolute in a root directory the caller sees at `root`, as the
/// caller spells it.
fn below(root: &Path, path: &Path) -> Option<PathBuf> {
    Some(root.join(path.strip_prefix("/").ok()?))
}

/// What the kernel's script handler makes of a file.
#[derive(Debug, PartialEq, Eq)]
pub enum Script {
    /// Not a script.
    Not,
    /// A file the caller may not read, which the kernel may still execute.
    Unread,
    /// A script; the kernel executes this interpreter path.
    Interpreter(PathBuf),
    /// A `#!` line that names no interpreter the kernel would run.
    Declined,
}

/// What the kernel's script handler makes of the file at `path`.
pub fn script_interpreter(path: &Path) -> Script {
    let mut header = Vec::with_capacity(BINPRM_BUF_SIZE);
    // Nonblocking and without a controlling terminal, so that a file
    // replaced by a FIFO or device since it was checked cannot wait.
    let read = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)
        .and_then(|file| file.take(BINPRM_BUF_SIZE as u64).read_to_end(&mut header));
    match read {
        Ok(_) => parse_script(&header),
        Err(_) => Script::Unread,
    }
}

/// Source of truth: `load_script()` in fs/binfmt_script.c. The kernel reads
/// the first [`BINPRM_BUF_SIZE`] bytes into a zeroed buffer. The line ends at
/// the first newline, and the interpreter path at a space, tab or NUL (so a
/// short file with no newline ends at the zeroes). A line with no newline
/// whose interpreter path runs to the end of the buffer may be truncated, and
/// the kernel declines it.
fn parse_script(header: &[u8]) -> Script {
    let mut buffer = [0_u8; BINPRM_BUF_SIZE];
    let read = header.len().min(BINPRM_BUF_SIZE);
    buffer[..read].copy_from_slice(&header[..read]);
    let Some(line) = buffer.strip_prefix(b"#!") else {
        return Script::Not;
    };
    let (line, ended) = match line.iter().position(|&byte| byte == b'\n') {
        Some(end) => (&line[..end], true),
        None => (line, false),
    };
    let start = line
        .iter()
        .position(|&byte| byte != b' ' && byte != b'\t')
        .unwrap_or(line.len());
    let rest = &line[start..];
    let end = rest
        .iter()
        .position(|&byte| matches!(byte, b' ' | b'\t' | 0));
    if end.is_none() && !ended {
        return Script::Declined;
    }
    let interpreter = &rest[..end.unwrap_or(rest.len())];
    if interpreter.is_empty() {
        return Script::Declined;
    }
    Script::Interpreter(PathBuf::from(std::ffi::OsStr::from_bytes(interpreter)))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// An ELF64 x86-64 image whose program headers (at file offset 64) name
    /// `segments`, each `(p_type, contents)` laid out after them at the
    /// address equal to its file offset. A `PT_LOAD` segment maps the whole
    /// file at address 0, whatever its contents.
    fn image_with(machine: u16, segments: &[(u32, Vec<u8>)]) -> Vec<u8> {
        let aligned: Vec<_> = segments
            .iter()
            .map(|(kind, contents)| (*kind, contents.clone(), 8))
            .collect();
        image_aligned(machine, &aligned)
    }

    /// [`image_with`] with each segment's p_align given.
    fn image_aligned(machine: u16, segments: &[(u32, Vec<u8>, u64)]) -> Vec<u8> {
        let mut header = [0_u8; 64];
        header[..4].copy_from_slice(b"\x7fELF");
        header[4] = 2;
        header[5] = 1;
        header[16..18].copy_from_slice(&3_u16.to_le_bytes());
        header[18..20].copy_from_slice(&machine.to_le_bytes());
        header[32..40].copy_from_slice(&64_u64.to_le_bytes());
        header[54..56].copy_from_slice(&56_u16.to_le_bytes());
        header[56..58].copy_from_slice(&(segments.len() as u16).to_le_bytes());
        let mut file = header.to_vec();
        let mut offset = 64 + 56 * segments.len() as u64;
        let mut data = Vec::new();
        for (kind, contents, align) in segments {
            let mut entry = [0_u8; 56];
            entry[..4].copy_from_slice(&kind.to_le_bytes());
            entry[8..16].copy_from_slice(&offset.to_le_bytes());
            entry[16..24].copy_from_slice(&offset.to_le_bytes());
            entry[32..40].copy_from_slice(&(contents.len() as u64).to_le_bytes());
            entry[48..56].copy_from_slice(&align.to_le_bytes());
            file.extend_from_slice(&entry);
            data.extend_from_slice(contents);
            offset += contents.len() as u64;
        }
        file.extend_from_slice(&data);
        let size = (file.len() as u64).to_le_bytes();
        for (index, (kind, _, _)) in segments.iter().enumerate() {
            if *kind == 1 {
                let entry = 64 + 56 * index;
                file[entry + 8..entry + 24].fill(0);
                file[entry + 32..entry + 40].copy_from_slice(&size);
            }
        }
        file
    }

    fn isa_needs(file: &[u8]) -> Option<IsaNeeds> {
        let header: [u8; 64] = file[..64].try_into().unwrap();
        IsaNeeds::parse(&header, None, |offset, buffer| {
            let start = usize::try_from(offset).ok()?;
            buffer.copy_from_slice(file.get(start..start.checked_add(buffer.len())?)?);
            Some(())
        })
    }

    /// One `NT_GNU_PROPERTY_TYPE_0` note holding `properties`, each
    /// `(pr_type, pr_data)`, padded as ELF64 pads them.
    fn property_note(properties: &[(u32, Vec<u8>)]) -> Vec<u8> {
        let mut description = Vec::new();
        for (kind, data) in properties {
            description.extend_from_slice(&kind.to_le_bytes());
            description.extend_from_slice(&(data.len() as u32).to_le_bytes());
            description.extend_from_slice(data);
            description.resize(description.len().next_multiple_of(8), 0);
        }
        let mut note = Vec::new();
        note.extend_from_slice(&4_u32.to_le_bytes());
        note.extend_from_slice(&(description.len() as u32).to_le_bytes());
        note.extend_from_slice(&NT_GNU_PROPERTY_TYPE_0.to_le_bytes());
        note.extend_from_slice(b"GNU\0");
        note.extend_from_slice(&description);
        note
    }

    fn isa_needed(bits: u32) -> (u32, Vec<u8>) {
        (GNU_PROPERTY_X86_ISA_1_NEEDED, bits.to_le_bytes().to_vec())
    }

    #[test]
    fn an_x86_isa_needed_property_names_its_highest_level() {
        for (bits, level) in [
            (0, 0),
            (1, 1),
            (0b11, 2),
            (0b111, 3),
            (0b100, 3),
            (0b1111, 4),
            // A bit above x86-64-v4's, for a level this Hermit does not know:
            // well formed, and above every level it does.
            (0b1_0111, 5),
            (1 << 31, 32),
        ] {
            let file = image_with(
                EM_X86_64,
                &[(PT_GNU_PROPERTY, property_note(&[isa_needed(bits)]))],
            );
            assert_eq!(isa_needs(&file), Some(IsaNeeds::Level(level)), "{bits:#b}");
        }
        // Beside another property (X86_FEATURE_1_AND, IBT|SHSTK), as linkers
        // write them.
        let file = image_with(
            EM_X86_64,
            &[(
                PT_GNU_PROPERTY,
                property_note(&[
                    (0xc000_0002, 3_u32.to_le_bytes().to_vec()),
                    isa_needed(0b111),
                ]),
            )],
        );
        assert_eq!(isa_needs(&file), Some(IsaNeeds::Level(3)));
        // Found in a PT_NOTE segment where no PT_GNU_PROPERTY segment exists.
        let build_id = {
            let mut note = Vec::new();
            note.extend_from_slice(&4_u32.to_le_bytes());
            note.extend_from_slice(&4_u32.to_le_bytes());
            note.extend_from_slice(&3_u32.to_le_bytes()); // NT_GNU_BUILD_ID
            note.extend_from_slice(b"GNU\0\xde\xad\xbe\xef");
            note
        };
        // As linkers lay them out: the 4-aligned build ID and ABI notes in
        // one PT_NOTE segment, the 8-aligned property note in another.
        let mut two_build_ids = build_id.clone();
        two_build_ids.extend_from_slice(&build_id);
        assert_eq!(
            isa_needs(&image_aligned(
                EM_X86_64,
                &[
                    (PT_NOTE, two_build_ids.clone(), 4),
                    (PT_NOTE, property_note(&[isa_needed(0b11)]), 8),
                ]
            )),
            Some(IsaNeeds::Level(2))
        );
        // No property at all: unmarked, which proves nothing about the CPU
        // the image needs.
        assert_eq!(
            isa_needs(&image_aligned(EM_X86_64, &[(PT_NOTE, two_build_ids, 4)])),
            Some(IsaNeeds::Unmarked)
        );
        assert_eq!(
            isa_needs(&image_with(EM_X86_64, &[])),
            Some(IsaNeeds::Unmarked)
        );
        // Not an x86-64 image: nothing for this policy to read.
        let aarch64 = image_with(
            183,
            &[(PT_GNU_PROPERTY, property_note(&[isa_needed(0b111)]))],
        );
        assert_eq!(isa_needs(&aarch64), None);
        // A real image, this test binary: readable, whatever it declares.
        let file =
            crate::util::RawFd::open(&std::env::current_exe().unwrap(), libc::O_RDONLY).unwrap();
        assert!(!matches!(
            IsaNeeds::read_file(&file),
            None | Some(IsaNeeds::Malformed(_))
        ));
    }

    /// A found declaration beside a later property whose size the reader
    /// cannot decide is a lower bound for an explicit refusal, and
    /// incomplete evidence for an automatic one: the strict read refuses
    /// it as malformed, the plain read keeps the level.
    #[test]
    fn a_declaration_beside_an_undecidable_property_is_incomplete_evidence() {
        // A valid declaration in one note segment, and a second note
        // segment whose description runs past its end, in another.
        let truncated = {
            let mut note = Vec::new();
            note.extend_from_slice(&4_u32.to_le_bytes());
            note.extend_from_slice(&0x1000_u32.to_le_bytes());
            note.extend_from_slice(&NT_GNU_PROPERTY_TYPE_0.to_le_bytes());
            note.extend_from_slice(b"GNU\0");
            note
        };
        let file = image_aligned(
            EM_X86_64,
            &[
                (PT_NOTE, property_note(&[isa_needed(0b11)]), 8),
                (PT_NOTE, truncated, 4),
            ],
        );
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("image");
        fs::write(&path, &file).unwrap();
        assert_eq!(
            IsaNeeds::read_path(&path),
            Some(IsaNeeds::Level(2)),
            "the plain read keeps the found lower bound"
        );
        assert!(
            matches!(
                IsaNeeds::read_path_strict(&path),
                Some(IsaNeeds::Malformed(_))
            ),
            "the strict read refuses incomplete evidence: {:?}",
            IsaNeeds::read_path_strict(&path)
        );
        // A complete scan reads the same under both.
        let complete = image_with(
            EM_X86_64,
            &[(PT_GNU_PROPERTY, property_note(&[isa_needed(0b11)]))],
        );
        let path = directory.path().join("complete");
        fs::write(&path, &complete).unwrap();
        assert_eq!(IsaNeeds::read_path_strict(&path), Some(IsaNeeds::Level(2)));
    }

    #[test]
    fn only_a_declared_level_above_the_profiles_refuses() {
        let objects = |image, loader| [("the image", image), ("its loader", loader)];
        let v3 = Some(IsaNeeds::Level(3));
        // The loader's declaration counts as the image's does.
        assert_eq!(
            first_needing_above(2, &objects(Some(IsaNeeds::Unmarked), v3)),
            Some(("its loader", 3))
        );
        assert_eq!(
            first_needing_above(2, &objects(v3, None)),
            Some(("the image", 3))
        );
        assert_eq!(first_needing_above(3, &objects(v3, v3)), None);
        assert_eq!(
            first_needing_above(3, &objects(Some(IsaNeeds::Level(5)), None)),
            Some(("the image", 5))
        );
        // Nothing declared, or nothing readable: no refusal.
        for unknown in [
            None,
            Some(IsaNeeds::Unmarked),
            Some(IsaNeeds::Malformed("a truncated note")),
            Some(IsaNeeds::Level(2)),
        ] {
            assert_eq!(first_needing_above(2, &objects(unknown, unknown)), None);
        }
        assert_eq!(x86_64_level_name(3), "x86-64-v3");
        assert_eq!(x86_64_level_name(1), "baseline x86-64");
        assert!(x86_64_level_name(5).contains("above v4"));
    }

    #[test]
    fn auto_resolves_the_lowest_profile_satisfying_every_declaration() {
        use detcore_model::cpu_profile::CpuProfile;
        let objects = |image, loader| {
            [
                ("the executable".to_owned(), image),
                ("its program interpreter".to_owned(), loader),
            ]
        };
        let (v3, v2, unmarked) = (
            Some(IsaNeeds::Level(3)),
            Some(IsaNeeds::Level(2)),
            Some(IsaNeeds::Unmarked),
        );
        assert_eq!(
            auto_profile(&objects(unmarked, unmarked)).unwrap().0,
            CpuProfile::X86_64V2
        );
        assert_eq!(
            auto_profile(&objects(v2, unmarked)).unwrap().0,
            CpuProfile::X86_64V2
        );
        let (profile, why) = auto_profile(&objects(unmarked, v3)).unwrap();
        assert_eq!(profile, CpuProfile::X86_64V3);
        assert!(why.contains("its program interpreter declares that it needs x86-64-v3"));
        assert_eq!(
            auto_profile(&objects(v3, v2)).unwrap().0,
            CpuProfile::X86_64V3
        );
        // Refusals: above every profile, unreadable, malformed.
        assert!(
            auto_profile(&objects(Some(IsaNeeds::Level(4)), v3))
                .unwrap_err()
                .contains("needs x86-64-v4, which no CPU profile presents")
        );
        assert!(
            auto_profile(&objects(unmarked, None))
                .unwrap_err()
                .contains("its program interpreter is unreadable")
        );
        assert!(
            auto_profile(&objects(Some(IsaNeeds::Malformed("a truncated note")), v2))
                .unwrap_err()
                .contains("unreadably (a truncated note)")
        );
    }

    #[test]
    fn a_program_interpreter_is_read_from_pt_interp() {
        // This test binary: whether or not it names one, readable.
        assert!(program_interpreter(&std::env::current_exe().unwrap()).is_ok());
        assert!(program_interpreter(Path::new("/proc/self/status")).is_err());
        let directory = tempfile::tempdir().unwrap();
        let image = directory.path().join("image");
        let interp = b"/lib64/ld-linux-x86-64.so.2\0".to_vec();
        fs::write(&image, image_with(EM_X86_64, &[(3, interp)])).unwrap();
        assert_eq!(
            program_interpreter(&image),
            Ok(Some(PathBuf::from("/lib64/ld-linux-x86-64.so.2")))
        );
        fs::write(&image, image_with(EM_X86_64, &[])).unwrap();
        assert_eq!(program_interpreter(&image), Ok(None));
        // As the kernel refuses them: a one-byte name (a lone NUL names
        // nothing) and a name the segment does not NUL-terminate.
        fs::write(&image, image_with(EM_X86_64, &[(3, vec![0])])).unwrap();
        assert_eq!(
            program_interpreter(&image),
            Err("a PT_INTERP shorter than two bytes or larger than a page")
        );
        fs::write(
            &image,
            image_with(EM_X86_64, &[(3, b"/lib64/ld-linux".to_vec())]),
        )
        .unwrap();
        assert_eq!(
            program_interpreter(&image),
            Err("a PT_INTERP without a terminating NUL")
        );
    }

    #[test]
    fn one_isa_declaration_in_two_segments_is_read_once() {
        // The property note in a PT_GNU_PROPERTY segment and a PT_NOTE one,
        // as linkers write it: one declaration, not two.
        let mut file = image_with(
            EM_X86_64,
            &[
                (PT_GNU_PROPERTY, property_note(&[isa_needed(0b111)])),
                (PT_NOTE, Vec::new()),
            ],
        );
        let first = file[64 + 8..64 + 56].to_vec();
        file[64 + 56 + 8..64 + 112].copy_from_slice(&first);
        assert_eq!(isa_needs(&file), Some(IsaNeeds::Level(3)));
    }

    /// `file` with every program header's address moved up by `shift`, as
    /// a linker lays out an image whose first segment is not at address 0.
    fn shifted(mut file: Vec<u8>, shift: u64) -> Vec<u8> {
        let count = usize::from(u16::from_le_bytes([file[56], file[57]]));
        for index in 0..count {
            let at = 64 + 56 * index + 16;
            let vaddr = u64::from_le_bytes(file[at..at + 8].try_into().unwrap());
            file[at..at + 8].copy_from_slice(&(vaddr + shift).to_le_bytes());
        }
        file
    }

    #[test]
    fn a_mapped_image_is_read_at_its_load_bias() {
        const BIAS: u64 = 0x7f00_0000_0000;
        // `file` mapped with its first segment (file offset 0) at address
        // `first`, so its header is at BIAS + first.
        let mapped = |file: &[u8], first: u64| {
            IsaNeeds::read_mapped(BIAS, BIAS + first, |address, buffer| {
                let start = usize::try_from(address.checked_sub(BIAS + first)?).ok()?;
                buffer.copy_from_slice(file.get(start..start.checked_add(buffer.len())?)?);
                Some(())
            })
        };
        let declared = (PT_GNU_PROPERTY, property_note(&[isa_needed(0b111)]));
        let file = image_with(EM_X86_64, &[(1, Vec::new()), declared.clone()]);
        assert_eq!(mapped(&file, 0), Some(IsaNeeds::Level(3)));
        // A first segment at 0x10000, its header at AT_BASE + 0x10000 and
        // AT_BASE itself unmapped: read where the header is.
        let high = shifted(file.clone(), 0x10000);
        assert_eq!(mapped(&high, 0x10000), Some(IsaNeeds::Level(3)));
        // Read at the bias, it is not there.
        assert_eq!(mapped(&high, 0), None);
        // Program headers no segment maps from file offset 0 at the header's
        // address: what was read there is not known to be them.
        assert_eq!(mapped(&image_with(EM_X86_64, &[declared]), 0), None);
        // A segment past the mapped bytes.
        let mut file = file;
        file.truncate(file.len() - 8);
        assert!(matches!(mapped(&file, 0), Some(IsaNeeds::Malformed(_))));
        // This test process's own program interpreter, found by its first
        // file mapping at or above AT_BASE and read from memory as Detcore
        // reads a guest's: whatever it declares, readable.
        let auxv = fs::read("/proc/self/auxv").unwrap();
        if let Some(base) = auxv_entry(&auxv, libc::AT_BASE).filter(|&base| base != 0) {
            let maps = fs::read("/proc/self/maps").unwrap();
            let (header, path) = first_image_mapping(&maps, base).unwrap();
            assert!(path.starts_with(b"/"), "{}", String::from_utf8_lossy(&path));
            let loader = IsaNeeds::read_mapped(base, header, |address, buffer| {
                // SAFETY: the loader's header, program headers and notes lie
                // in its mapped, readable first segment.
                let source =
                    unsafe { std::slice::from_raw_parts(address as *const u8, buffer.len()) };
                buffer.copy_from_slice(source);
                Some(())
            });
            assert!(
                matches!(loader, Some(IsaNeeds::Unmarked | IsaNeeds::Level(_))),
                "{loader:?}"
            );
        }
    }

    #[test]
    fn the_first_image_mapping_is_found_at_or_above_the_bias() {
        let maps = b"7f0000000000-7f0000001000 r--p 00001000 fd:01 99 /lib/ld.so\n\
                     7f0000010000-7f0000011000 r--p 00000000 fd:01 99 /lib/ld so\n\
                     7f0000020000-7f0000021000 rw-p 00000000 00:00 0 \n\
                     7e0000000000-7e0000001000 r--p 00000000 fd:01 7 /bin/sh\n";
        assert_eq!(
            first_image_mapping(maps, 0x7f00_0000_0000),
            Some((0x7f00_0001_0000, b"/lib/ld so".to_vec()))
        );
        assert_eq!(first_image_mapping(maps, 0x7f00_0002_0000), None);
    }

    #[test]
    fn a_mapped_interpreters_declaration_does_not_wait_on_the_executable() {
        // The executable unreadable (say, its permissions changed after the
        // exec) while the mapped interpreter declares v3: refused.
        let objects = exec_objects(None, Some(Some(IsaNeeds::Level(3))));
        assert_eq!(
            first_needing_above(2, &objects),
            Some(("its program interpreter", 3))
        );
        // No interpreter loaded: only the executable.
        assert_eq!(exec_objects(Some(IsaNeeds::Unmarked), None).len(), 1);
        // Neither known: nothing to refuse on.
        assert_eq!(
            first_needing_above(2, &exec_objects(None, Some(None))),
            None
        );
    }

    #[test]
    fn an_unrelated_note_cannot_erase_a_declaration() {
        // A well-formed 65552-byte note of another kind (namesz 4, descsz
        // 65536, name "REV"), in its own 4-aligned PT_NOTE.
        let mut large = Vec::new();
        large.extend_from_slice(&4_u32.to_le_bytes());
        large.extend_from_slice(&65536_u32.to_le_bytes());
        large.extend_from_slice(&0x1234_u32.to_le_bytes());
        large.extend_from_slice(b"REV\0");
        large.resize(large.len() + 65536, 0xa5);
        let declared = (PT_GNU_PROPERTY, property_note(&[isa_needed(0b111)]), 8);
        let unrelated = (PT_NOTE, large, 4);
        for segments in [
            [declared.clone(), unrelated.clone()],
            [unrelated.clone(), declared.clone()],
        ] {
            assert_eq!(
                isa_needs(&image_aligned(EM_X86_64, &segments)),
                Some(IsaNeeds::Level(3))
            );
        }
        // Nor can an unrelated note whose header cannot be read (the file
        // ends 6 bytes into it): the declaration stands, and only without
        // one is the image unknown. (An unrelated description is skipped,
        // never read, so its own truncation is not even seen.)
        let cut = 65552 - 6;
        let mut truncated = image_aligned(EM_X86_64, &[declared.clone(), unrelated.clone()]);
        truncated.truncate(truncated.len() - cut);
        assert_eq!(isa_needs(&truncated), Some(IsaNeeds::Level(3)));
        let mut alone = image_aligned(EM_X86_64, &[unrelated]);
        alone.truncate(alone.len() - cut);
        assert!(
            matches!(isa_needs(&alone), Some(IsaNeeds::Malformed(why)) if why.contains("past the end"))
        );
        // A property before an unreadable one in the same note stands too.
        let mut note = property_note(&[isa_needed(0b111), (0xc000_0002, vec![3, 0, 0, 0])]);
        // The second property (at description offset 16) claims 16 data
        // bytes, past the description's end.
        note[16 + 16 + 4..16 + 16 + 8].copy_from_slice(&16_u32.to_le_bytes());
        assert_eq!(
            isa_needs(&image_aligned(EM_X86_64, &[(PT_GNU_PROPERTY, note, 8)])),
            Some(IsaNeeds::Level(3))
        );
    }

    #[test]
    fn a_malformed_isa_declaration_is_not_read_as_unmarked() {
        let malformed =
            |segments: &[(u32, Vec<u8>)]| match isa_needs(&image_with(EM_X86_64, segments)) {
                Some(IsaNeeds::Malformed(why)) => why,
                other => panic!("not malformed: {other:?}"),
            };
        // The property twice, in one note or across segments.
        assert!(
            malformed(&[(
                PT_GNU_PROPERTY,
                property_note(&[isa_needed(0b11), isa_needed(0b111)])
            )])
            .contains("more than one")
        );
        assert!(
            malformed(&[
                (PT_NOTE, property_note(&[isa_needed(0b11)])),
                (PT_NOTE, property_note(&[isa_needed(0b111)])),
            ])
            .contains("more than one")
        );
        // A second declaration beside the one a PT_GNU_PROPERTY segment
        // holds, which a reader of that segment alone would never see.
        assert!(
            malformed(&[
                (PT_GNU_PROPERTY, property_note(&[isa_needed(0b11)])),
                (PT_NOTE, property_note(&[isa_needed(0b111)])),
            ])
            .contains("more than one")
        );
        assert!(
            malformed(&[
                (PT_GNU_PROPERTY, property_note(&[isa_needed(0b11)])),
                (PT_GNU_PROPERTY, property_note(&[isa_needed(0b11)])),
            ])
            .contains("more than one PT_GNU_PROPERTY")
        );
        // A property whose data is not 4 bytes.
        assert!(
            malformed(&[(
                PT_GNU_PROPERTY,
                property_note(&[(GNU_PROPERTY_X86_ISA_1_NEEDED, vec![7; 8])])
            )])
            .contains("not 4 bytes")
        );
        // Truncated notes and properties.
        let mut note = property_note(&[isa_needed(0b111)]);
        note.truncate(note.len() - 4);
        assert!(malformed(&[(PT_GNU_PROPERTY, note)]).contains("truncated"));
        assert!(malformed(&[(PT_GNU_PROPERTY, vec![4, 0, 0])]).contains("truncated"));
        // A segment that runs past the end of the file.
        let mut file = image_with(
            EM_X86_64,
            &[(PT_GNU_PROPERTY, property_note(&[isa_needed(1)]))],
        );
        file.truncate(file.len() - 8);
        assert!(
            matches!(isa_needs(&file), Some(IsaNeeds::Malformed(why)) if why.contains("past the end"))
        );
    }

    #[test]
    fn an_elf_layout_gives_linuxs_start_of_the_program_break() {
        // A real image: this test binary, linked by the toolchain.
        let layout = ElfLayout::read(&std::env::current_exe().unwrap()).unwrap();
        assert!(layout.end > layout.phdr);
        // Its text segment is executable and page-aligned in file and memory.
        let (text_offset, text_page) = layout.text.unwrap();
        assert_eq!((text_offset % layout.page, text_page % layout.page), (0, 0));
        assert_eq!(layout.page, crate::page::page_size());
        // A non-ELF file and a 32-bit or big-endian header have none.
        assert_eq!(ElfLayout::read(Path::new("/proc/self/status")), None);
        let mut header = [0_u8; 64];
        header[..4].copy_from_slice(b"\x7fELF");
        header[4] = 1;
        header[5] = 1;
        assert_eq!(ElfLayout::parse(&header, |_, _| None, 4096), None);

        // A synthetic ET_EXEC image with two loads: text at 0x400000 holding
        // the program headers at file offset 64, and data at 0x403e10 whose
        // bss runs to 0x4050b0.
        let mut header = [0_u8; 64];
        header[..4].copy_from_slice(b"\x7fELF");
        header[4] = 2;
        header[5] = 1;
        header[16..18].copy_from_slice(&2_u16.to_le_bytes());
        header[32..40].copy_from_slice(&64_u64.to_le_bytes());
        header[54..56].copy_from_slice(&56_u16.to_le_bytes());
        header[56..58].copy_from_slice(&3_u16.to_le_bytes());
        let program_header = |kind: u32, offset: u64, vaddr: u64, filesz: u64, memsz: u64| {
            let mut entry = [0_u8; 56];
            entry[..4].copy_from_slice(&kind.to_le_bytes());
            entry[8..16].copy_from_slice(&offset.to_le_bytes());
            entry[16..24].copy_from_slice(&vaddr.to_le_bytes());
            entry[32..40].copy_from_slice(&filesz.to_le_bytes());
            entry[40..48].copy_from_slice(&memsz.to_le_bytes());
            entry
        };
        let entries = [
            program_header(6, 64, 0x400040, 0xa8, 0xa8), // PT_PHDR, not loaded
            program_header(1, 0, 0x400000, 0x1000, 0x1000),
            program_header(1, 0x2e10, 0x403e10, 0x290, 0x12a0),
        ];
        let read_at = |offset: u64, buffer: &mut [u8]| {
            let index = usize::try_from((offset - 64) / 56).ok()?;
            buffer.copy_from_slice(entries.get(index)?);
            Some(())
        };
        let layout = ElfLayout::parse(&header, read_at, 4096).unwrap();
        assert_eq!(
            layout,
            ElfLayout {
                position_independent: false,
                interpreter: false,
                phdr: 0x400040,
                end: 0x4050b0,
                // The synthetic headers carry no flags, so no executable
                // segment.
                text: None,
                data_span: 0x290,
                page: 4096,
            }
        );
        assert_eq!(layout.start_brk(0x400040), Ok(0x406000));
        assert!(layout.start_brk(0x400000).is_err());
        // On a host with 64 KiB pages the same image's break starts at the
        // next 64 KiB boundary.
        let large_pages = ElfLayout::parse(&header, read_at, 0x10000).unwrap();
        assert_eq!(
            large_pages,
            ElfLayout {
                page: 0x10000,
                ..layout
            }
        );
        assert_eq!(large_pages.start_brk(0x400040), Ok(0x410000));
        // An end already on a page boundary stays there, at any bias.
        let aligned = ElfLayout {
            end: 0x406000,
            ..layout
        };
        assert_eq!(aligned.start_brk(0x400040), Ok(0x406000));
        // A position-independent image with an interpreter at its bias.
        let dynamic = ElfLayout {
            position_independent: true,
            interpreter: true,
            ..aligned
        };
        assert_eq!(
            dynamic.start_brk(0x5555_5555_4000 + 0x400040),
            Ok(0x5555_5595_a000)
        );
        // Without an interpreter (a static PIE, or the dynamic loader run
        // directly), the kernel's build decides: no model.
        let static_pie = ElfLayout {
            interpreter: false,
            ..dynamic
        };
        assert!(static_pie.start_brk(0x5555_5555_4000 + 0x400040).is_err());
        // Program headers outside every load: Linux's AT_PHDR is the bias.
        let unloaded = ElfLayout { phdr: 0, ..layout };
        assert_eq!(unloaded.start_brk(0), Ok(0x406000));
        // A PT_INTERP entry and ET_DYN are read from the headers.
        header[16..18].copy_from_slice(&3_u16.to_le_bytes());
        header[56..58].copy_from_slice(&4_u16.to_le_bytes());
        let with_interp = [
            entries[0],
            program_header(3, 0x2a8, 0x4002a8, 0x1c, 0x1c), // PT_INTERP
            entries[1],
            entries[2],
        ];
        let read_at = |offset: u64, buffer: &mut [u8]| {
            let index = usize::try_from((offset - 64) / 56).ok()?;
            buffer.copy_from_slice(with_interp.get(index)?);
            Some(())
        };
        assert_eq!(
            ElfLayout::parse(&header, read_at, 4096).unwrap(),
            ElfLayout {
                position_independent: true,
                interpreter: true,
                ..layout
            }
        );
    }

    /// The kernel's own `start_brk` for a real image, a shell exec'd without
    /// address randomization, is what its layout and `AT_PHDR` give.
    #[test]
    fn the_kernels_start_of_the_break_is_what_the_layout_gives() {
        use std::os::unix::process::CommandExt;
        let mut command = std::process::Command::new("/bin/sh");
        command
            .args(["-c", "read line"])
            .stdin(std::process::Stdio::piped());
        // SAFETY: personality(2) is async-signal-safe.
        unsafe {
            command.pre_exec(|| {
                if libc::personality(libc::ADDR_NO_RANDOMIZE as libc::c_ulong) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        // `spawn` returns once the exec has happened.
        let mut child = command.spawn().unwrap();
        let process = PathBuf::from(format!("/proc/{}", child.id()));
        let image = fs::read_link(process.join("exe")).unwrap();
        let at_phdr = auxv_entry(&fs::read(process.join("auxv")).unwrap(), libc::AT_PHDR).unwrap();
        let start_brk = stat_start_brk(&fs::read_to_string(process.join("stat")).unwrap()).unwrap();
        let maps = fs::read(process.join("maps")).unwrap();
        drop(child.stdin.take());
        child.wait().unwrap();
        let layout = ElfLayout::read(&image).unwrap();
        assert_eq!(layout.start_brk(at_phdr), Ok(start_brk), "{image:?}");
        // The same start from the image's executable mapping, as Detcore
        // finds it where the kernel exec'd a launcher.
        let ino = std::os::unix::fs::MetadataExt::ino(&fs::metadata(&image).unwrap());
        let bias = load_bias(&maps, &image, ino, layout.text.unwrap()).unwrap();
        assert_eq!(layout.start_brk_at_bias(bias), Ok(start_brk), "{image:?}");
    }

    #[test]
    fn the_start_of_the_break_is_field_47_past_the_command() {
        let fields: Vec<String> = (3..=52).map(|field| field.to_string()).collect();
        let stat = format!("12 (a) b) (c) {}\n", fields.join(" "));
        assert_eq!(stat_start_brk(&stat), Some(47));
        assert_eq!(stat_start_brk("12 (a) S 1 2"), None);
        assert_eq!(stat_start_brk("12 a S"), None);
    }

    #[test]
    fn a_break_check_compares_only_where_linuxs_rule_gives_the_kernels_start() {
        assert_eq!(
            BrkCheck::of(Ok(0x406000), Some(false), Some(0x406000)),
            BrkCheck::Equal(0x406000)
        );
        let differs = BrkCheck::of(Ok(0x406000), Some(false), Some(0x407000));
        assert_eq!(
            differs,
            BrkCheck::Differs {
                model: 0x406000,
                kernel: 0x407000
            }
        );
        assert_eq!(
            differs.to_string(),
            "model 0x406000 differs from the kernel's start_brk 0x407000"
        );
        assert_eq!(
            BrkCheck::of(Ok(0x406000), Some(true), Some(0x1234000)),
            BrkCheck::Randomized(0x406000)
        );
        assert_eq!(
            BrkCheck::of(Ok(0x406000), None, Some(0x406000)),
            BrkCheck::RandomizationUnknown(0x406000)
        );
        assert_eq!(
            BrkCheck::of(Ok(0x406000), Some(false), None),
            BrkCheck::KernelUnread(0x406000)
        );
        assert_eq!(
            BrkCheck::of(Err("no exec record"), Some(false), Some(0x406000)),
            BrkCheck::Unmodeled("no exec record")
        );
    }

    #[test]
    fn a_load_bias_comes_from_the_images_executable_mapping() {
        // A non-PIE image's text and data, another file's text, a read-only
        // mapping of the whole image at offset 0 (as SaBRe's rewriter makes),
        // and a position-independent image whose name has a space.
        let listed = "\
00401000-00402000 r-xp 00001000 00:20 1234 /dir/image
00402000-00403000 r--p 00002000 00:20 1234 /dir/image
555555553000-555555554000 r-xp 00001000 00:20 99 /other
7ffff7bac000-7ffff7bb1000 r--p 00000000 00:20 1234 /dir/image
7ffff7fc6000-7ffff7fc7000 r-xp 00001000 00:21 5678 /with spaces
";
        let maps = listed.as_bytes();
        let path = |path: &'static str| Path::new(path);
        let text = (0x1000, 0x401000);
        assert_eq!(load_bias(maps, path("/dir/image"), 1234, text), Ok(0));
        assert!(load_bias(maps, path("/dir/image"), 4321, text).is_err());
        assert!(load_bias(maps, path("/dir/ima"), 1234, text).is_err());
        // The same inode and name under another directory (another
        // filesystem's file) is not the image.
        assert!(load_bias(maps, path("/elsewhere/image"), 1234, text).is_err());
        // Two executable mappings of the text at different biases: no answer.
        let twice =
            format!("{listed}7ffff0001000-7ffff0002000 r-xp 00001000 00:20 1234 /dir/image\n");
        assert!(load_bias(twice.as_bytes(), path("/dir/image"), 1234, text).is_err());
        // The same bias twice is still one answer.
        let again = format!("{listed}00401000-00402000 r-xp 00001000 00:20 1234 /dir/image\n");
        assert_eq!(
            load_bias(again.as_bytes(), path("/dir/image"), 1234, text),
            Ok(0)
        );
        // A text segment at another offset has no executable mapping here.
        assert_eq!(
            load_bias(maps, path("/dir/image"), 1234, (0x2000, 0x402000)).ok(),
            None
        );
        // A position-independent image at its bias.
        assert_eq!(
            load_bias(maps, path("/with spaces"), 5678, (0x1000, 0x1000)),
            Ok(0x7ffff7fc5000)
        );
        // A name that is not UTF-8, anywhere in the mappings, is still bytes
        // to compare.
        let mut named = maps.to_vec();
        named.extend_from_slice(b"7ffff7fd0000-7ffff7fd1000 r-xp 00001000 00:21 42 /n\xffme\n");
        let unnamed = Path::new(std::ffi::OsStr::from_bytes(b"/n\xffme"));
        assert_eq!(
            load_bias(&named, unnamed, 42, (0x1000, 0x1000)),
            Ok(0x7ffff7fcf000)
        );
        assert_eq!(load_bias(&named, path("/dir/image"), 1234, text), Ok(0));
        // A newline in the name, which the mappings show as `\012`.
        named.extend_from_slice(
            b"7ffff7fe0000-7ffff7fe1000 r-xp 00001000 00:21 43 /line\\012break\n",
        );
        let broken = Path::new("/line\nbreak");
        assert_eq!(
            load_bias(&named, broken, 43, (0x1000, 0x1000)),
            Ok(0x7ffff7fdf000)
        );
        // The model at a bias from the mapping: the synthetic image's end
        // page-aligned up from the bias.
        let layout = ElfLayout {
            position_independent: true,
            interpreter: true,
            phdr: 0x40,
            end: 0x50b0,
            text: Some((0x1000, 0x1000)),
            data_span: 0x290,
            page: 4096,
        };
        assert_eq!(layout.start_brk_at_bias(0x555555556000), Ok(0x55555555c000));
    }

    #[test]
    fn an_auxiliary_vector_entry_is_read_up_to_its_end() {
        let auxv: Vec<u8> = [(3_u64, 0x400040_u64), (6, 4096), (0, 0), (9, 1)]
            .iter()
            .flat_map(|(key, value)| [key.to_ne_bytes(), value.to_ne_bytes()].concat())
            .collect();
        assert_eq!(auxv_entry(&auxv, 3), Some(0x400040));
        assert_eq!(auxv_entry(&auxv, 6), Some(4096));
        assert_eq!(auxv_entry(&auxv, 9), None);
        assert_eq!(auxv_entry(&auxv[..20], 6), None);
    }

    #[test]
    fn a_script_line_names_its_interpreter_as_the_kernel_reads_it() {
        let interpreter = |path: &str| Script::Interpreter(PathBuf::from(path));
        assert_eq!(parse_script(b"\x7fELF"), Script::Not);
        assert_eq!(parse_script(b"#!/bin/sh\n"), interpreter("/bin/sh"));
        assert_eq!(parse_script(b"#! \t/bin/sh -e\n"), interpreter("/bin/sh"));
        // A carriage return is part of the path to the kernel.
        assert_eq!(parse_script(b"#!/bin/sh\r\n"), interpreter("/bin/sh\r"));
        assert_eq!(parse_script(b"#!/bin/sh -x"), interpreter("/bin/sh"));
        // A short file with no newline: the path ends at the zeroed buffer.
        assert_eq!(parse_script(b"#!/bin/sh"), interpreter("/bin/sh"));
        // One that fills the buffer may be truncated.
        let long = [b"#!/".as_slice(), &[b'a'; BINPRM_BUF_SIZE]].concat();
        assert_eq!(parse_script(&long), Script::Declined);
        assert_eq!(parse_script(b"#!\n"), Script::Declined);
        assert_eq!(parse_script(b"#!\0/bin/sh\n"), Script::Declined);
    }

    #[test]
    #[allow(clippy::disallowed_methods)] // std::fs::canonicalize is the independent oracle
    fn a_script_resolves_to_its_interpreter_and_a_link_to_its_file() {
        let directory = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        let interpreter = std::env::current_exe().unwrap();
        let interpreter = std::fs::canonicalize(&interpreter).unwrap();
        let script = root.join("script");
        fs::write(&script, format!("#!{}\n", interpreter.display())).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("script", root.join("link")).unwrap();

        let record = ExecRecord::resolve(Path::new("/"), &root, Path::new("link")).unwrap();
        assert_eq!(record.exe, interpreter);
        assert!(record.is_present(Path::new("/")));

        // The same file seen below another root is named from that root.
        let image = root.join("image");
        fs::copy(&interpreter, &image).unwrap();
        let record = ExecRecord::resolve(&root, Path::new("/"), Path::new("/image")).unwrap();
        assert_eq!(record.exe, Path::new("/image"));
        assert!(record.is_present(&root));

        // A file removed or replaced at the path is no longer the image.
        fs::remove_file(&image).unwrap();
        assert!(!record.is_present(&root));
        fs::copy(&interpreter, &image).unwrap();
        assert!(!record.is_present(&root));

        // The same inode put back at the path after a rename is not the
        // image's link any more: Linux's link follows the renamed name.
        let image = root.join("image");
        let record = ExecRecord::resolve(&root, Path::new("/"), Path::new("/image")).unwrap();
        fs::hard_link(&image, root.join("alias")).unwrap();
        fs::rename(&image, root.join("moved")).unwrap();
        fs::hard_link(root.join("alias"), &image).unwrap();
        assert!(!record.is_present(&root));

        // With its link count back where it was, a rename still changed
        // the inode's change time.
        let image = root.join("relinked");
        fs::copy(&interpreter, &image).unwrap();
        fs::hard_link(&image, root.join("second")).unwrap();
        let record = ExecRecord::at(&root, &image).unwrap();
        fs::rename(&image, root.join("away")).unwrap();
        fs::rename(root.join("second"), &image).unwrap();
        assert_eq!(fs::metadata(&image).unwrap().nlink(), 2);
        assert!(!record.is_present(&root));

        // Linux refuses to exec anything but a regular file (EACCES) before
        // it opens it, and so does the resolution: an open of a FIFO with no
        // writer would wait forever.
        let fifo = root.join("fifo");
        let fifo_name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o755) }, 0);
        assert_eq!(
            ExecRecord::resolve(&root, Path::new("/"), Path::new("/fifo")),
            None
        );
        assert_eq!(
            ExecRecord::resolve(Path::new("/"), Path::new("/"), Path::new("/dev/null")),
            None
        );

        assert_eq!(
            ExecRecord::resolve(&root, Path::new("/"), Path::new("/missing")),
            None
        );
        // A file whose header the caller cannot read is the kernel's to
        // resolve (Linux runs an execute-only script's interpreter): its
        // header reads as unread, never as not a script.
        assert_eq!(script_interpreter(&root), Script::Unread);
    }

    #[test]
    fn only_a_directory_samples_as_one() {
        // SAFETY: an all-zero `stat` is a valid value.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        stat.st_dev = 7;
        stat.st_ino = 42;
        stat.st_mode = libc::S_IFDIR | 0o755;
        assert_eq!(directory_sample(&stat), Some((7, 42)));
        // A symbolic link, or any other file, with the recorded directory's
        // device and inode (a freed inode number reused) is not that
        // directory.
        for kind in [
            libc::S_IFLNK,
            libc::S_IFREG,
            libc::S_IFIFO,
            libc::S_IFSOCK,
            libc::S_IFCHR,
            libc::S_IFBLK,
        ] {
            stat.st_mode = kind | 0o777;
            assert_eq!(directory_sample(&stat), None, "{kind:o}");
        }
        // And a chain through such a file is no chain.
        let directory = tempfile::tempdir().unwrap();
        let root = crate::util::canonicalize(directory.path()).unwrap();
        fs::create_dir(root.join("away")).unwrap();
        std::os::unix::fs::symlink("away", root.join("a")).unwrap();
        assert_eq!(directory_chain(&root, &root.join("a/image")), None);
        assert!(directory_chain(&root, &root.join("away/image")).is_some());
    }

    #[test]
    fn a_renamed_directory_above_the_image_is_not_its_path() {
        let directory = tempfile::tempdir().unwrap();
        let root = crate::util::canonicalize(directory.path()).unwrap();
        fs::create_dir(root.join("a")).unwrap();
        fs::create_dir(root.join("b")).unwrap();
        let image = root.join("a/image");
        fs::write(&image, b"image").unwrap();
        fs::hard_link(&image, root.join("b/image")).unwrap();
        let record = ExecRecord::at(&root, &image).unwrap();
        assert_eq!(record.exe, Path::new("/a/image"));
        assert!(record.names_path_exactly(&root));
        // The image's directory renamed away and another directory holding a
        // link of the same file renamed into its place: the file is untouched
        // at the path, but Linux's link names /away/image.
        fs::rename(root.join("a"), root.join("away")).unwrap();
        fs::rename(root.join("b"), root.join("a")).unwrap();
        assert!(record.is_present(&root));
        assert!(!record.names_path_exactly(&root));
        // Renamed back: the same directories at the same paths.
        fs::rename(root.join("a"), root.join("b")).unwrap();
        fs::rename(root.join("away"), root.join("a")).unwrap();
        assert!(record.names_path_exactly(&root));
        // A symbolic link in a directory's place.
        fs::rename(root.join("a"), root.join("away")).unwrap();
        std::os::unix::fs::symlink("away", root.join("a")).unwrap();
        assert!(!record.names_path_exactly(&root));
    }

    /// The test binary, a dynamically linked executable, names its program
    /// interpreter, which exists where the kernel found it; a file that is
    /// not ELF names none.
    #[test]
    fn an_image_names_its_program_interpreter_as_the_kernel_reads_it() {
        let interpreter = program_interpreter(Path::new("/proc/self/exe"))
            .expect("the test binary reads")
            .expect("the test binary is dynamically linked");
        assert!(interpreter.is_absolute(), "{}", interpreter.display());
        assert!(interpreter.exists(), "{}", interpreter.display());
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("script");
        fs::write(&script, "#!/bin/sh\n").unwrap();
        assert!(program_interpreter(&script).is_err());
    }
}
