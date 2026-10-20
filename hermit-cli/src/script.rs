/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::path::PathBuf;

// Defined in kernel include/linux/binfmts.h
const BINPRM_BUF_SIZE: usize = 256;

/// Unix shebang, see [Wikipedia](https://en.wikipedia.org/wiki/Shebang_(Unix)).
#[derive(Debug, Eq, PartialEq)]
pub struct Shebang {
    program: PathBuf,
    args: Vec<OsString>,
}

impl Shebang {
    // Source of truth: fs/binfmt_script.c, function load_script().
    pub(crate) fn from_buf(buf: &[u8]) -> Option<Self> {
        if !buf.starts_with(b"#!") {
            return None;
        }

        let mut i = 2;
        while i < buf.len() {
            if buf[i] == b' ' || buf[i] == b'\t' {
                i += 1;
            } else {
                break;
            }
        }

        let mut j = i;
        while j < buf.len() {
            if b" \t\r\n".contains(&buf[j]) {
                break;
            } else {
                j += 1;
            }
        }

        let program = PathBuf::from(OsStr::from_bytes(&buf[i..j]));

        i = j;
        while j < buf.len() {
            if b"\n".contains(&buf[j]) {
                break;
            } else {
                j += 1;
            }
        }

        let args = String::from_utf8_lossy(&buf[i..j])
            .split_ascii_whitespace()
            .map(OsString::from)
            .collect::<Vec<_>>();

        Some(Shebang { program, args })
    }

    /// Extract the interpreter path without checking whether it exists.
    ///
    /// Callers must resolve the path in the filesystem namespace that will
    /// execute the script.
    pub fn interpreter_from_buf(buf: &[u8]) -> Option<PathBuf> {
        Self::from_buf(buf).map(|shebang| shebang.program)
    }

    /// Parse exe/interpreter from a script contains a shebang, whenever possible.
    pub fn new<P: AsRef<Path>>(path: P) -> Option<Self> {
        let mut buf = Vec::new();
        let nb = fs::File::open(path)
            .and_then(|f| {
                let mut handle = f.take(BINPRM_BUF_SIZE as u64);
                handle.read_to_end(&mut buf)
            })
            .ok()?;
        // Pass a slice up to the number of bytes read to the lookup function. We don't want to pass
        // the extra zeroes.
        let shebang = Self::from_buf(&buf[..nb])?;
        let metadata = fs::metadata(&shebang.program).ok()?;
        if metadata.is_file() {
            Some(shebang)
        } else {
            None
        }
    }

    /// Get interpreter from shebang
    pub fn interpreter(&self) -> &Path {
        &self.program
    }

    /// Get intepreter arguments from shebang.
    pub fn args(&self) -> impl Iterator<Item = &OsStr> {
        self.args.iter().map(OsStr::new)
    }

    /// Convert shebang into (intepreter, args).
    pub fn into_parts(self) -> (PathBuf, Vec<OsString>) {
        (self.program, self.args)
    }
}

/// How the kernel's script handler reads the start of a file, for checks that
/// must inspect exactly the interpreter the kernel executes.
// Only the in-guest LiteInst program check uses it.
#[cfg(any(test, feature = "liteinst"))]
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum KernelScript {
    /// The file does not start with `#!`; the kernel tries its other handlers.
    NotScript,
    /// The script handler runs no interpreter: the `#!` line names none, or it
    /// has no newline and the interpreter path runs to the end of the
    /// [`BINPRM_BUF_SIZE`] bytes the kernel reads, so it may be truncated.
    /// Measured on Linux 7.1: `#!\n`, `#! \t\n` and a 254-byte path with no
    /// newline fail with `ENOEXEC`, so the kernel tries its other handlers;
    /// `#!` alone and `#!\0/bin/sh\n`, whose name a NUL leaves empty, fail the
    /// `execve` with `EACCES`.
    Declined,
    /// The kernel executes this interpreter path, byte for byte.
    Interpreter(PathBuf),
}

#[cfg(any(test, feature = "liteinst"))]
impl KernelScript {
    /// Source of truth: `load_script()` in `fs/binfmt_script.c`; the errors
    /// noted on [`KernelScript::Declined`] were measured on Linux 7.1. Unlike
    /// [`Shebang::from_buf`], which its existing callers keep,
    /// this ends the interpreter path only at a space, tab or NUL: a carriage
    /// return is part of the path, as it is for the kernel.
    pub(crate) fn from_buf(buf: &[u8]) -> Self {
        // The kernel reads the file's first BINPRM_BUF_SIZE bytes into a buffer
        // that is NUL-padded when the file is shorter.
        let mut header = [0_u8; BINPRM_BUF_SIZE];
        let len = buf.len().min(BINPRM_BUF_SIZE);
        header[..len].copy_from_slice(&buf[..len]);
        if !header.starts_with(b"#!") {
            return Self::NotScript;
        }
        let last = BINPRM_BUF_SIZE - 1;
        // `strnchr` stops at the first NUL.
        let newline = header
            .iter()
            .take_while(|byte| **byte != 0)
            .position(|byte| *byte == b'\n');
        let line_end = match newline {
            Some(newline) => newline,
            None => {
                // Without a newline the interpreter path must be followed by a
                // space, tab or NUL within the buffer, or it may be truncated.
                let Some(start) = (2..=last).find(|&index| !matches!(header[index], b' ' | b'\t'))
                else {
                    return Self::Declined;
                };
                if !header[start..=last]
                    .iter()
                    .any(|byte| matches!(byte, b' ' | b'\t' | 0))
                {
                    return Self::Declined;
                }
                // The kernel overwrites the buffer's last byte with a NUL.
                last
            }
        };
        let line = &header[2..line_end];
        let start = line
            .iter()
            .position(|byte| !matches!(byte, b' ' | b'\t'))
            .unwrap_or(line.len());
        let name = &line[start..];
        let name = &name[..name
            .iter()
            .position(|byte| matches!(byte, b' ' | b'\t' | 0))
            .unwrap_or(name.len())];
        if name.is_empty() {
            Self::Declined
        } else {
            Self::Interpreter(PathBuf::from(OsStr::from_bytes(name)))
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn shebang_invalid_shebang() {
        assert_eq!(Shebang::from_buf(b"! /bin/bash"), None);
    }

    #[test]
    fn shebang_empty_interpreter() {
        for input in [b"#!".as_slice(), b"#!\n", b"#! \t\n"] {
            assert_eq!(
                Shebang::from_buf(input).map(|shebang| shebang.program),
                Some(PathBuf::new()),
                "input: {input:?}"
            );
        }
    }

    #[test]
    fn shebang_bin_bash() {
        assert_eq!(
            Shebang::from_buf(b"#! /bin/bash").map(|s| s.program),
            Some(PathBuf::from("/bin/bash"))
        );
        assert_eq!(
            Shebang::from_buf(b"#!  \t/bin/bash").map(|s| s.program),
            Some(PathBuf::from("/bin/bash"))
        );
        assert_eq!(
            Shebang::from_buf(b"#!/bin/bash\nfoobar").map(|s| s.program),
            Some(PathBuf::from("/bin/bash"))
        );
        assert_eq!(
            Shebang::from_buf(b"#!/bin/bash").map(|s| s.program),
            Some(PathBuf::from("/bin/bash"))
        );
    }

    #[test]
    fn shebang_env_python() {
        assert_eq!(
            Shebang::from_buf(b"#! /usr/bin/env python").map(|s| s.program),
            Some(PathBuf::from("/usr/bin/env"))
        );
    }

    #[test]
    fn shebang_struct_parse() {
        assert_eq!(
            Shebang::from_buf(b"#! /usr/bin/env python3"),
            Some(Shebang {
                program: PathBuf::from("/usr/bin/env"),
                args: vec![OsString::from("python3")]
            })
        );
        assert_eq!(
            Shebang::from_buf(b"#!/bin/bash\nfoobar"),
            Some(Shebang {
                program: PathBuf::from("/bin/bash"),
                args: Vec::new(),
            })
        );
    }

    #[test]
    fn shebang_struct_env_python_get_interpreter() {
        assert_eq!(
            Shebang::from_buf(b"#! /usr/bin/env python3"),
            Some(Shebang {
                program: PathBuf::from("/usr/bin/env"),
                args: vec![OsString::from("python3")],
            })
        );

        assert_eq!(
            Shebang::from_buf(b"#! /usr/bin/env python3 -c  "),
            Some(Shebang {
                program: PathBuf::from("/usr/bin/env"),
                args: vec![OsString::from("python3"), OsString::from("-c")],
            })
        );

        assert_eq!(
            Shebang::from_buf(b"#! /usr/bin/env python3  -c \nfoobar"),
            Some(Shebang {
                program: PathBuf::from("/usr/bin/env"),
                args: vec![OsString::from("python3"), OsString::from("-c")],
            })
        );
    }

    fn kernel_interpreter(path: &[u8]) -> KernelScript {
        KernelScript::Interpreter(PathBuf::from(OsStr::from_bytes(path)))
    }

    #[test]
    fn kernel_script_reads_the_interpreter_as_load_script_does() {
        let cases: [(&[u8], KernelScript); 7] = [
            (b"\x7fELF\x02\x01\x01", KernelScript::NotScript),
            (b"! /bin/bash", KernelScript::NotScript),
            // A carriage return is part of the path, unlike in Shebang.
            (b"#!/tmp/interp\r\n", kernel_interpreter(b"/tmp/interp\r")),
            (
                b"#! \t/usr/bin/env python3 -c\nbody",
                kernel_interpreter(b"/usr/bin/env"),
            ),
            // A NUL ends the path.
            (b"#!/bin/sh\0junk\n", kernel_interpreter(b"/bin/sh")),
            // A short file is NUL-padded, which ends the path.
            (b"#!/bin/bash", kernel_interpreter(b"/bin/bash")),
            (b"#!relative arg\n", kernel_interpreter(b"relative")),
        ];
        for (input, expected) in cases {
            assert_eq!(KernelScript::from_buf(input), expected, "input: {input:?}");
        }
    }

    #[test]
    fn kernel_script_declines_a_missing_or_truncated_interpreter() {
        for input in [b"#!".as_slice(), b"#!\n", b"#! \t\n", b"#!\0/bin/sh\n"] {
            assert_eq!(
                KernelScript::from_buf(input),
                KernelScript::Declined,
                "input: {input:?}"
            );
        }
        // No newline and no space, tab or NUL in the first 256 bytes: the path
        // may be truncated, so the kernel declines it, whatever follows.
        let mut long = b"#!/".to_vec();
        long.resize(BINPRM_BUF_SIZE, b'a');
        assert_eq!(KernelScript::from_buf(&long), KernelScript::Declined);
        long.extend_from_slice(b" arg\n");
        assert_eq!(KernelScript::from_buf(&long), KernelScript::Declined);
        // A space in the last byte ends the path inside the buffer.
        long.truncate(BINPRM_BUF_SIZE);
        long[BINPRM_BUF_SIZE - 1] = b' ';
        let expected = long[2..BINPRM_BUF_SIZE - 1].to_vec();
        assert_eq!(KernelScript::from_buf(&long), kernel_interpreter(&expected));
    }
}
