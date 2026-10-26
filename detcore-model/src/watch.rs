/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! `hermit run --watch`: the guest bytes whose accesses Detcore reports
//! through hardware watchpoints (see `detcore::watch`).

use std::fmt;
use std::str::FromStr;

use serde::Deserialize;
use serde::Serialize;

/// The length of a watch when `--watch` names none.
pub const DEFAULT_WATCH_LEN: u8 = 8;

/// Which accesses a watch reports. x86-64 debug registers have no read-only
/// watch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WatchAccess {
    /// Writes (`w`, the default).
    Write,
    /// Reads and writes (`rw`).
    ReadWrite,
}

/// One `--watch=ADDR[:LEN][:w|rw]`: `len` bytes at `addr`, aligned to `len`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WatchArg {
    /// The first watched byte, a guest virtual address.
    pub addr: u64,
    /// 1, 2, 4 or 8 bytes.
    pub len: u8,
    /// The accesses reported.
    pub access: WatchAccess,
}

impl fmt::Display for WatchArg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let access = match self.access {
            WatchAccess::Write => "w",
            WatchAccess::ReadWrite => "rw",
        };
        write!(f, "{:#x}:{}:{}", self.addr, self.len, access)
    }
}

impl FromStr for WatchArg {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.split(':');
        let addr = parts.next().unwrap_or_default();
        let addr = match addr.strip_prefix("0x").or_else(|| addr.strip_prefix("0X")) {
            Some(hex) => u64::from_str_radix(hex, 16),
            None => addr.parse::<u64>(),
        }
        .map_err(|_| {
            format!(
                "--watch={s}: `{addr}` is not an address; give it in hex (0x...) or decimal, as \
                 ADDR[:LEN][:w|rw]"
            )
        })?;
        let mut len = DEFAULT_WATCH_LEN;
        let mut access = WatchAccess::Write;
        let rest: Vec<&str> = parts.collect();
        if rest.len() > 2 {
            return Err(format!("--watch={s}: expected ADDR[:LEN][:w|rw]"));
        }
        for (i, part) in rest.iter().enumerate() {
            match *part {
                "w" if i + 1 == rest.len() => access = WatchAccess::Write,
                "rw" if i + 1 == rest.len() => access = WatchAccess::ReadWrite,
                n if i == 0 => {
                    len = n.parse().map_err(|_| {
                        format!(
                            "--watch={s}: `{n}` is neither a length (1, 2, 4 or 8) nor an \
                             access (w or rw)"
                        )
                    })?
                }
                other => {
                    return Err(format!(
                        "--watch={s}: `{other}` is not an access; use w (writes) or rw (reads and \
                         writes)"
                    ));
                }
            }
        }
        if ![1, 2, 4, 8].contains(&len) {
            return Err(format!(
                "--watch={s}: a hardware watchpoint watches 1, 2, 4 or 8 bytes, not {len}"
            ));
        }
        if addr % u64::from(len) != 0 {
            return Err(format!(
                "--watch={s}: a watch of {len} bytes needs an address aligned to {len}; use \
                 {:#x}:{len} or a shorter length",
                addr - addr % u64::from(len)
            ));
        }
        Ok(WatchArg { addr, len, access })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_watch_takes_an_address_a_length_and_an_access() {
        let parse = |s: &str| s.parse::<WatchArg>();
        let watch = |addr, len, access| Ok(WatchArg { addr, len, access });
        assert_eq!(parse("0x404010"), watch(0x404010, 8, WatchAccess::Write));
        assert_eq!(parse("4210704:4"), watch(0x404010, 4, WatchAccess::Write));
        assert_eq!(
            parse("0x404011:1:rw"),
            watch(0x404011, 1, WatchAccess::ReadWrite)
        );
        assert_eq!(
            parse("0x404010:rw"),
            watch(0x404010, 8, WatchAccess::ReadWrite)
        );
        let arg = WatchArg {
            addr: 0x404012,
            len: 2,
            access: WatchAccess::ReadWrite,
        };
        assert_eq!(arg.to_string(), "0x404012:2:rw");
        assert_eq!(parse(&arg.to_string()), Ok(arg));

        for (bad, says) in [
            ("main", "is not an address"),
            ("0x404010:3", "1, 2, 4 or 8 bytes, not 3"),
            ("0x404014:8", "use 0x404010:8 or a shorter length"),
            ("0x404010:8:r", "`r` is not an access"),
            ("0x404010:w:8", "`w` is neither a length"),
            ("0x404010:8:w:x", "expected ADDR[:LEN][:w|rw]"),
        ] {
            let error = parse(bad).unwrap_err();
            assert!(error.contains(says), "{bad}: {error}");
        }
    }
}
