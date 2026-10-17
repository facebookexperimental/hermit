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
            let found = fs::canonicalize(below(root, &spelled)?).ok()?;
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
    fn a_script_resolves_to_its_interpreter_and_a_link_to_its_file() {
        let directory = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        let interpreter = std::env::current_exe().unwrap();
        let interpreter = fs::canonicalize(interpreter).unwrap();
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
