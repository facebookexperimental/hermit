// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::path::PathBuf;

// Defined in kernel include/linux/binfmts.h
const BINPRM_BUF_SIZE: usize = 256;

/// Unix shebang, see https://en.wikipedia.org/wiki/Shebang_(Unix)
#[derive(Debug, Eq, PartialEq)]
pub struct Shebang {
    program: PathBuf,
    args: Vec<OsString>,
}

impl Shebang {
    // Source of truth: fs/binfmt_script.c, function load_script().
    fn from_buf(buf: &[u8]) -> Option<Self> {
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

        let mut j = 1 + i;
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

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn shebang_invalid_shebang() {
        assert_eq!(Shebang::from_buf(b"! /bin/bash"), None);
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
}
