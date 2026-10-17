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
//! the kernel's own link when the exec completes (`tool_global`). The
//! kernel's answer is the file it exec'd, and a backend whose launcher loads
//! the guest itself (SaBRe's loader) has the kernel exec the launcher, so the
//! kernel names the launcher; only there does Detcore answer the link from
//! the record (`Detcore::launched_exe`). Everywhere else the kernel's answer
//! stands.
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
        let mut spelled = cwd.join(path);
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
                Script::Interpreter(interpreter) => spelled = cwd.join(interpreter),
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
}

/// The page size Linux aligns the program break to (`ELF_PAGEALIGN`) on
/// x86_64, where Hermit runs.
const PAGE_SIZE: u64 = 4096;

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
        Self::parse(&header, read_exact)
    }

    /// [`Self::read`] of an image whose 64-byte file header is `header`,
    /// reading its program headers through `read_at(offset, buffer)`.
    fn parse(
        header: &[u8; 64],
        mut read_at: impl FnMut(u64, &mut [u8]) -> Option<()>,
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
            if offset <= phoff && phoff < offset.checked_add(filesz)? {
                phdr = (phoff - offset).checked_add(vaddr)?;
            }
            let segment_end = vaddr.checked_add(memsz)?;
            end = Some(end.map_or(segment_end, |end| end.max(segment_end)));
        }
        Some(Self {
            position_independent,
            interpreter,
            phdr,
            end: end?,
        })
    }

    /// Where Linux starts the program break of a process that exec'd this
    /// image, given the `AT_PHDR` its exec reported, with address
    /// randomization off: the end of the highest loaded segment at the load
    /// bias, page-aligned up (`mm->start_brk = mm->brk =
    /// ELF_PAGEALIGN(elf_brk)`), or why there is no answer.
    pub fn start_brk(&self, at_phdr: u64) -> Result<u64, &'static str> {
        if self.position_independent && !self.interpreter {
            // Recent kernels (6.17's load_elf_binary) move it to
            // ELF_ET_DYN_BASE even without randomization, unless built with
            // CONFIG_COMPAT_BRK.
            return Err(
                "a position-independent image without an interpreter, whose break depends on the kernel's build",
            );
        }
        let bias = at_phdr
            .checked_sub(self.phdr)
            .ok_or("AT_PHDR below the image's program headers")?;
        bias.checked_add(self.end)
            .and_then(|end| end.checked_add(PAGE_SIZE - 1))
            .map(|end| end & !(PAGE_SIZE - 1))
            .ok_or("an image ending past the address space")
    }
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
enum Script {
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
fn script_interpreter(path: &Path) -> Script {
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

    #[test]
    fn an_elf_layout_gives_linuxs_start_of_the_program_break() {
        // A real image: this test binary, linked by the toolchain.
        let layout = ElfLayout::read(&std::env::current_exe().unwrap()).unwrap();
        assert!(layout.end > layout.phdr);
        // A non-ELF file and a 32-bit or big-endian header have none.
        assert_eq!(ElfLayout::read(Path::new("/proc/self/status")), None);
        let mut header = [0_u8; 64];
        header[..4].copy_from_slice(b"\x7fELF");
        header[4] = 1;
        header[5] = 1;
        assert_eq!(ElfLayout::parse(&header, |_, _| None), None);

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
        let layout = ElfLayout::parse(&header, read_at).unwrap();
        assert_eq!(
            layout,
            ElfLayout {
                position_independent: false,
                interpreter: false,
                phdr: 0x400040,
                end: 0x4050b0,
            }
        );
        assert_eq!(layout.start_brk(0x400040), Ok(0x406000));
        assert!(layout.start_brk(0x400000).is_err());
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
            ElfLayout::parse(&header, read_at).unwrap(),
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
        drop(child.stdin.take());
        child.wait().unwrap();
        let layout = ElfLayout::read(&image).unwrap();
        assert_eq!(layout.start_brk(at_phdr), Ok(start_brk), "{image:?}");
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
}
