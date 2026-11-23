#!/usr/bin/env -S rust-script --force
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Run `cargo fmt` with the pinned formatter toolchain, and refuse to run it
//! with any other formatter.
//!
//! The formatter is pinned in `ci/rustfmt-toolchain.toml`: a nightly channel
//! and the exact `rustfmt --version` line of its rustfmt. rustfmt.toml's import
//! grouping, one-import-per-item and doc-comment options are nightly-only, and
//! stable rustfmt formats as if they were absent, so a check under the stable
//! compiler pin would pass code those options reject.
//!
//! The formatter is found in this order:
//! 1. `$HERMIT_RUSTFMT_TOOLCHAIN`, a toolchain root holding `bin/rustfmt` and
//!    `bin/cargo-fmt`, for an environment without rustup.
//! 2. rustup, running the pinned channel (`rustup run --install <channel>`,
//!    which installs it first when absent, as `RUSTUP_TOOLCHAIN=<channel>` did
//!    before this script).
//!
//! Either way the script runs that toolchain's own `bin/rustfmt` and
//! `bin/cargo-fmt` by absolute path, with `RUSTFMT` set, for the latter, to a
//! wrapper that runs the former: neither PATH, CARGO_HOME nor an inherited
//! `RUSTFMT` picks the formatter, and `--self-test` runs the same two
//! executables as the check.
//!
//! Neither, or a formatter whose `rustfmt --version` is not the pinned line,
//! exits 2 before anything is formatted. The rustfmt on PATH is never used.
//! The pinned validation image has neither yet, so the offline split's
//! lint.rustfmt refuses there rather than check with the image's stable
//! rustfmt, until <https://github.com/rrnewton/hermit/issues/5397> packages
//! the pinned formatter in the image.
//!
//! cargo fmt runs rustfmt once per edition and returns the first nonzero exit
//! code among those runs, so a rustfmt killed by a signal (an abort on a
//! failed allocation, an out-of-memory kill), which leaves no exit code,
//! counts as a pass: the diffs it printed fail nothing, and the file it was
//! formatting goes unchecked. Every cargo fmt run therefore sets `RUSTFMT` to
//! a generated wrapper (`.tmp-rustfmt-gate-PID-N.rustfmt`, written into the
//! repository root like the configuration below and removed afterwards) that
//! runs the pinned rustfmt with its arguments and output unchanged and exits
//! with the shell's status for it: rustfmt's own exit code, or 128 plus the
//! signal number for a death by signal, which it names on stderr. A formatter
//! process this script runs directly and that dies by a signal is reported
//! the same way.
//!
//! Files whose bytes cannot change yet (another file pins their sha256) are
//! listed in `ci/rustfmt-exclusions.txt`, one `PATH REASON` per
//! line (spaces, never a tab), and formatting skips exactly those. rustfmt
//! takes `ignore` only from a configuration file and resolves its entries
//! against that file's directory, so every formatting run writes a
//! configuration into the repository root (`.tmp-rustfmt-gate-PID.toml`, which
//! `.tmp*` keeps out of git, removed afterwards): `rustfmt.toml` unchanged plus
//! an `ignore` list of the listed paths, and passes it as `--config-path`.
//! Before formatting, the list must parse (each entry a repository-relative
//! `.rs` path, listed once, with the reason `sha256-pinned in FILE[ and
//! FILE...]`), every listed path must be a file whose sha256 each FILE
//! contains (an entry nothing pins is refused: format that file instead; no
//! FILE may be the list itself or a listed file, by path or by inode),
//! `rustfmt.toml` must set no `ignore` of its own, and the `ignore` list read
//! back from the written file must be exactly the list; any failure exits 2
//! before anything is formatted.
//!
//! `--workspace-members` also formats, file by file and with the same
//! configuration (`rustfmt --unstable-features --skip-children`, as Meta's
//! internal tree runs it), every tracked `.rs` file (`git ls-files`) the
//! members pass did not reach: rust-scripts, include! targets and sources
//! outside any member's module tree, except the listed exclusions.
//!
//! Usage, from anywhere in the repository:
//!   scripts/cargo-fmt-pinned.rs --workspace-members [-- --check]
//!                                                    format exactly the root
//!                                                    [workspace] members (the
//!                                                    lint.rustfmt scope; plain
//!                                                    `--all` would also descend
//!                                                    into path-dependency crates
//!                                                    such as the reverie submodule)
//!   scripts/cargo-fmt-pinned.rs [CARGO-FMT-ARGS...]  e.g. `--all -- --check`;
//!                                                    exits with cargo fmt's status
//!   scripts/cargo-fmt-pinned.rs --self-test          the selected formatter must
//!                                                    reject a brace import (exit 1) and
//!                                                    accept its fixed form (exit 0)
//!   scripts/cargo-fmt-pinned.rs --show               print the selected formatter
//!   scripts/cargo-fmt-pinned.rs --check-exclusions   check ci/rustfmt-exclusions.txt
//!                                                    and the configuration built
//!                                                    from it, without a formatter

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

/// The pin, relative to the repository root.
const PIN_FILE: &str = "ci/rustfmt-toolchain.toml";
/// A toolchain root holding the pinned formatter.
const TOOLCHAIN_ENV: &str = "HERMIT_RUSTFMT_TOOLCHAIN";
/// The follow-up that packages the pinned formatter in the pinned image.
const IMAGE_FOLLOW_UP: &str = "https://github.com/rrnewton/hermit/issues/5397";
/// The self-test fixtures' package: standalone, so no enclosing workspace claims it.
const SELF_TEST_MANIFEST: &str =
    "[package]\nname = \"fmt-self-test\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[workspace]\n";
/// The files formatting leaves alone, relative to the repository root.
const EXCLUSIONS_FILE: &str = "ci/rustfmt-exclusions.txt";
/// The repository's rustfmt configuration, which the generated one extends.
const ROOT_CONFIG: &str = "rustfmt.toml";
/// The generated configuration's name in the repository root, before the PID.
const GATE_CONFIG_PREFIX: &str = ".tmp-rustfmt-gate-";
/// The one reason an entry may give, followed by `FILE[ and FILE...]`.
const PINNED_IN: &str = "sha256-pinned in ";
/// The variable that names the pinned rustfmt to the rustfmt wrapper.
const WRAPPED_RUSTFMT_ENV: &str = "CARGO_FMT_PINNED_RUSTFMT";

/// One `ci/rustfmt-exclusions.txt` entry.
#[derive(Debug, PartialEq, Eq)]
struct Exclusion {
    /// Its line in the list, from 1.
    line: usize,
    /// Repository-relative path of a `.rs` file.
    path: String,
    /// The repository-relative files its reason names, each of which must
    /// contain the file's sha256: that pin is why its bytes cannot change.
    pinned_in: Vec<String>,
}

/// Reads the exclusion list: `PATH REASON` lines, with `#` comment lines and
/// blank lines skipped. PATH is the first whitespace-delimited field and
/// REASON the rest of the line, trimmed. Each PATH is a repository-relative
/// `.rs` path, listed once, and REASON is exactly `sha256-pinned in FILE[ and
/// FILE...]`; [`load_exclusions`] then requires each FILE to contain PATH's
/// sha256. No FILE may be the list itself or a listed file: the list would
/// then vouch for its own entries (a comment holding a digest is enough). A
/// line with a tab is refused, comment lines included: fbsource's linter
/// refuses tabs in text files, so the list separates its fields with spaces.
fn parse_exclusions(text: &str) -> Result<Vec<Exclusion>, String> {
    let mut list: Vec<Exclusion> = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let at = format!("{EXCLUSIONS_FILE}:{}", number + 1);
        if line.contains('\t') {
            return Err(format!(
                "{at}: has a tab; separate PATH and REASON with spaces (fbsource's linter \
                 refuses tabs in text files)"
            ));
        }
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with(char::is_whitespace) {
            return Err(format!(
                "{at}: starts with whitespace (expected `PATH REASON`)"
            ));
        }
        let (path, reason) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        if reason.trim().is_empty() {
            return Err(format!(
                "{at}: `{path}` has no reason (expected `PATH REASON`)"
            ));
        }
        if let Some(problem) = exclusion_path_problem(path) {
            return Err(format!("{at}: `{path}` {problem}"));
        }
        if list.iter().any(|seen| seen.path == path) {
            return Err(format!("{at}: `{path}` is listed twice"));
        }
        let pinned_in = pinning_files(reason.trim())
            .map_err(|problem| format!("{at}: `{path}`'s reason `{}` {problem}", reason.trim()))?;
        if pinned_in.iter().any(|file| file == EXCLUSIONS_FILE) {
            return Err(format!(
                "{at}: `{path}`'s reason names {EXCLUSIONS_FILE}, the list itself; the list \
                 cannot vouch for its own entries"
            ));
        }
        list.push(Exclusion {
            line: number + 1,
            path: path.to_string(),
            pinned_in,
        });
    }
    for entry in &list {
        if let Some(file) = entry
            .pinned_in
            .iter()
            .find(|file| list.iter().any(|other| other.path == **file))
        {
            return Err(format!(
                "{EXCLUSIONS_FILE}:{}: `{}`'s reason names `{file}`, which is itself listed; a \
                 listed file cannot pin an entry",
                entry.line, entry.path
            ));
        }
    }
    Ok(list)
}

/// The files a reason names: it must read `sha256-pinned in FILE[ and
/// FILE...]`, each FILE a repository-relative path.
fn pinning_files(reason: &str) -> Result<Vec<String>, &'static str> {
    let Some(files) = reason.strip_prefix(PINNED_IN) else {
        return Err("is not `sha256-pinned in FILE[ and FILE...]`");
    };
    files
        .split(" and ")
        .map(|file| {
            let relative = !file.is_empty()
                && !file.starts_with('/')
                && !file.contains(char::is_whitespace)
                && file
                    .split('/')
                    .all(|part| !part.is_empty() && part != "." && part != "..");
            if relative {
                Ok(file.to_string())
            } else {
                Err("names a FILE that is not a repository-relative path")
            }
        })
        .collect()
}

/// Why `path` cannot be an entry, if it cannot. rustfmt reads `ignore`
/// entries as gitignore patterns, so a pattern character, or a path with no
/// `/` (which gitignore matches by name in every directory), would exclude
/// more than the one file.
fn exclusion_path_problem(path: &str) -> Option<&'static str> {
    if !path.ends_with(".rs") {
        Some("is not a .rs path")
    } else if !path.contains('/') {
        Some("has no `/`; rustfmt would read it as a pattern matching that name in every directory")
    } else if path.starts_with('/') {
        Some("is not relative to the repository root")
    } else if path
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        Some("has an empty, `.` or `..` component")
    } else if path
        .chars()
        .any(|c| c.is_whitespace() || "*?[]!\\#\"".contains(c))
    {
        Some("has whitespace or a gitignore pattern character")
    } else {
        None
    }
}

/// The list in the checkout at `root`, every entry naming a regular file (an
/// entry left behind by a move or a deletion excludes nothing, or excludes
/// the next file to take its name) whose sha256, as lowercase hex, every file
/// its reason names contains: an entry nothing pins is refused, since that
/// file can simply be formatted.
fn load_exclusions(root: &Path) -> Result<Vec<Exclusion>, String> {
    let path = root.join(EXCLUSIONS_FILE);
    let text = fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let list = parse_exclusions(&text)?;
    // The list and the listed files by identity, so that a FILE hard-linked to
    // one of them is refused like the path itself (a symbolic link is already
    // refused: every FILE must be a regular file).
    let mut vouchers = vec![(EXCLUSIONS_FILE.to_string(), file_identity(&path))];
    for entry in &list {
        vouchers.push((entry.path.clone(), file_identity(&root.join(&entry.path))));
    }
    for entry in &list {
        let at = format!("{EXCLUSIONS_FILE}:{}", entry.line);
        let bytes = regular_file(root, &entry.path)
            .map_err(|problem| format!("{at}: `{}` {problem}", entry.path))?;
        let digest = sha256_hex(&bytes);
        for file in &entry.pinned_in {
            let pins = regular_file(root, file).map_err(|problem| {
                format!(
                    "{at}: `{}`'s reason names `{file}`, which {problem}",
                    entry.path
                )
            })?;
            let identity = file_identity(&root.join(file));
            if let Some((same, _)) = vouchers
                .iter()
                .find(|(_, other)| identity.is_some() && *other == identity)
            {
                return Err(format!(
                    "{at}: `{}`'s reason names `{file}`, which is the same file as `{same}`; \
                     neither the list nor a listed file can pin an entry",
                    entry.path
                ));
            }
            let found = pins
                .windows(digest.len())
                .any(|window| window == digest.as_bytes());
            if !found {
                return Err(format!(
                    "{at}: `{}`'s sha256 {digest} is not in `{file}`, so its reason does not \
                     hold; format the file and delete its entry instead",
                    entry.path
                ));
            }
        }
    }
    Ok(list)
}

/// The device and inode of the file at `path`, if it exists.
fn file_identity(path: &Path) -> Option<(u64, u64)> {
    fs::symlink_metadata(path)
        .ok()
        .map(|meta| (meta.dev(), meta.ino()))
}

/// The bytes of the regular file at `relative` under `root`, or why not.
fn regular_file(root: &Path, relative: &str) -> Result<Vec<u8>, String> {
    let path = root.join(relative);
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_file() => {
            fs::read(&path).map_err(|error| format!("cannot be read: {error}"))
        }
        Ok(_) => Err("is not a regular file".to_string()),
        Err(error) => Err(format!("is missing: {error}")),
    }
}

/// SHA-256 of `data` as lowercase hex (std only, as scripts/syscall-pages.rs
/// computes it).
fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut message = Vec::with_capacity(data.len() + 72);
    message.extend_from_slice(data);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());
    let mut w = [0u32; 64];
    for chunk in message.chunks_exact(64) {
        for (i, word) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, value) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(value);
        }
    }
    h.iter().map(|word| format!("{word:08x}")).collect()
}

/// The formatting configuration: `root_config` unchanged, plus an `ignore`
/// list of exactly the listed paths. A `root_config` that sets `ignore`
/// itself is refused: formatting would then skip files the list does not name.
fn gate_config(root_config: &str, list: &[Exclusion]) -> Result<String, String> {
    let sets_ignore = root_config.lines().any(|line| {
        line.trim_start()
            .strip_prefix("ignore")
            .is_some_and(|rest| rest.trim_start().starts_with('='))
    });
    if sets_ignore {
        return Err(format!(
            "{ROOT_CONFIG} sets `ignore`; the only files formatting may skip are \
             {EXCLUSIONS_FILE}'s"
        ));
    }
    let mut text = root_config.to_string();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!(
        "\n# Generated by scripts/cargo-fmt-pinned.rs from {EXCLUSIONS_FILE}.\nignore = [\n"
    ));
    for entry in list {
        text.push_str(&format!("    \"{}\",\n", entry.path));
    }
    text.push_str("]\n");
    Ok(text)
}

/// The `ignore` entries of a generated configuration, read back from its text
/// rather than taken from [`gate_config`]'s input.
fn config_ignore_entries(config: &str) -> Result<Vec<String>, String> {
    let mut entries: Option<Vec<String>> = None;
    let mut collecting = false;
    for line in config.lines().map(str::trim) {
        if collecting {
            if line == "]" {
                collecting = false;
                continue;
            }
            let entry = line
                .strip_suffix(',')
                .and_then(|line| line.strip_prefix('"'))
                .and_then(|line| line.strip_suffix('"'))
                .ok_or_else(|| format!("unexpected line in the `ignore` list: `{line}`"))?;
            entries
                .as_mut()
                .expect("collecting")
                .push(entry.to_string());
            continue;
        }
        let Some(rest) = line.strip_prefix("ignore") else {
            continue;
        };
        let Some(value) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        if entries.is_some() {
            return Err("`ignore` is set twice".to_string());
        }
        if value.trim() != "[" {
            return Err(format!("unexpected `ignore` line: `{line}`"));
        }
        entries = Some(Vec::new());
        collecting = true;
    }
    if collecting {
        return Err("the `ignore` list is not closed".to_string());
    }
    entries.ok_or_else(|| "no `ignore` list".to_string())
}

/// The generated configuration file, removed when dropped.
struct GateConfig {
    path: PathBuf,
    excluded: usize,
}

impl Drop for GateConfig {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Writes the formatting configuration into the repository root (see the
/// module documentation for why there) and checks it: its `ignore` list, read
/// back from the written file, must be exactly the exclusion list, and the
/// rest of it must be `rustfmt.toml` unchanged.
fn write_gate_config(root: &Path) -> Result<GateConfig, String> {
    let list = load_exclusions(root)?;
    let root_config_path = root.join(ROOT_CONFIG);
    let root_config = fs::read_to_string(&root_config_path)
        .map_err(|error| format!("{}: {error}", root_config_path.display()))?;
    let text = gate_config(&root_config, &list)?;
    let config = GateConfig {
        path: root.join(format!("{GATE_CONFIG_PREFIX}{}.toml", std::process::id())),
        excluded: list.len(),
    };
    fs::write(&config.path, &text)
        .map_err(|error| format!("{}: {error}", config.path.display()))?;
    let written = fs::read_to_string(&config.path)
        .map_err(|error| format!("{}: {error}", config.path.display()))?;
    let ignored = config_ignore_entries(&written)
        .map_err(|error| format!("{}: {error}", config.path.display()))?;
    let listed: Vec<&str> = list.iter().map(|entry| entry.path.as_str()).collect();
    if ignored != listed {
        return Err(format!(
            "{} ignores {ignored:?}, but {EXCLUSIONS_FILE} lists {listed:?}",
            config.path.display()
        ));
    }
    if !written.starts_with(&root_config) {
        return Err(format!(
            "{} does not start with {ROOT_CONFIG} unchanged",
            config.path.display()
        ));
    }
    Ok(config)
}

/// `args` for cargo fmt with `--config-path config` added to the rustfmt
/// arguments (after `--`). An explicit `--config-path` is refused: it would
/// replace the configuration that carries the exclusions.
fn with_gate_config(args: Vec<OsString>, config: &Path) -> Result<Vec<OsString>, String> {
    let separator = args.iter().position(|arg| arg == "--");
    if let Some(separator) = separator {
        let explicit = args[separator + 1..].iter().any(|arg| {
            arg.to_str()
                .is_some_and(|arg| arg == "--config-path" || arg.starts_with("--config-path="))
        });
        if explicit {
            return Err(format!(
                "--config-path is set by this script (rustfmt.toml plus {EXCLUSIONS_FILE}); \
                 do not pass one"
            ));
        }
    }
    let mut with = args;
    let at = match separator {
        Some(separator) => separator + 1,
        None => {
            with.push("--".into());
            with.len()
        }
    };
    with.splice(
        at..at,
        [
            OsString::from("--config-path"),
            config.as_os_str().to_owned(),
        ],
    );
    Ok(with)
}

/// What `ci/rustfmt-toolchain.toml` pins.
#[derive(Debug, PartialEq, Eq)]
struct Pin {
    /// The rustup channel, `nightly-YYYY-MM-DD`.
    channel: String,
    /// The exact `rustfmt --version` line of that channel's rustfmt.
    identity: String,
}

/// Reads the pin file's two keys, each exactly once, as `key = "value"`.
fn parse_pin(text: &str) -> Result<Pin, String> {
    let mut channel = None;
    let mut identity = None;
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parsed = line.split_once('=').and_then(|(key, value)| {
            let value = value.trim().strip_prefix('"')?.strip_suffix('"')?;
            (!value.contains('"')).then(|| (key.trim(), value.to_string()))
        });
        let Some((key, value)) = parsed else {
            return Err(format!(
                "{PIN_FILE}:{}: expected `key = \"value\"`, found `{line}`",
                number + 1
            ));
        };
        let slot = match key {
            "channel" => &mut channel,
            "identity" => &mut identity,
            other => return Err(format!("{PIN_FILE}:{}: unknown key `{other}`", number + 1)),
        };
        if slot.replace(value).is_some() {
            return Err(format!("{PIN_FILE}:{}: `{key}` is set twice", number + 1));
        }
    }
    match (channel, identity) {
        (Some(channel), Some(identity)) => Ok(Pin { channel, identity }),
        (None, _) => Err(format!("{PIN_FILE}: no `channel`")),
        (_, None) => Err(format!("{PIN_FILE}: no `identity`")),
    }
}

/// Where the pinned formatter comes from.
#[derive(Debug, PartialEq, Eq)]
enum Formatter {
    /// A toolchain root named by `$HERMIT_RUSTFMT_TOOLCHAIN`.
    Toolchain(PathBuf),
    /// rustup, running this channel, installed on first use.
    Rustup(String),
}

impl Formatter {
    fn describe(&self) -> String {
        match self {
            Formatter::Toolchain(root) => format!("{TOOLCHAIN_ENV}={}", root.display()),
            Formatter::Rustup(channel) => format!("rustup run --install {channel}"),
        }
    }

    /// The toolchain root holding the pinned formatter. For rustup it is the
    /// pinned channel's sysroot, installed first when absent.
    fn toolchain_root(&self) -> Result<PathBuf, String> {
        let root = match self {
            Formatter::Toolchain(root) => root.clone(),
            Formatter::Rustup(channel) => {
                let output = Command::new("rustup")
                    .args(["run", "--install"])
                    .arg(channel)
                    .args(["rustc", "--print", "sysroot"])
                    .output()
                    .map_err(|error| format!("cannot run rustup for {channel}: {error}"))?;
                if !output.status.success() {
                    return Err(format!(
                        "`rustup run --install {channel} rustc --print sysroot` failed ({}): {}",
                        output.status,
                        String::from_utf8_lossy(&output.stderr).trim()
                    ));
                }
                PathBuf::from(String::from_utf8_lossy(&output.stdout).trim())
            }
        };
        fs::canonicalize(&root)
            .map_err(|error| format!("{} ({}): {error}", root.display(), self.describe()))
    }

    /// The pinned toolchain's `rustfmt` and `cargo-fmt`, by absolute path.
    fn resolve(&self) -> Result<Resolved, String> {
        let bin = self.toolchain_root()?.join("bin");
        let resolved = Resolved {
            rustfmt: bin.join("rustfmt"),
            cargo_fmt: bin.join("cargo-fmt"),
        };
        for path in [&resolved.rustfmt, &resolved.cargo_fmt] {
            if !path.is_file() {
                return Err(format!("{} has no {}", self.describe(), path.display()));
            }
        }
        Ok(resolved)
    }
}

/// The pinned formatter's executables. They are only ever run by these paths.
#[derive(Debug, PartialEq, Eq)]
struct Resolved {
    rustfmt: PathBuf,
    cargo_fmt: PathBuf,
}

impl Resolved {
    /// The `rustfmt --version` line of the pinned rustfmt.
    fn version(&self) -> Result<String, String> {
        let output = Command::new(&self.rustfmt)
            .arg("--version")
            .output()
            .map_err(|error| format!("cannot run {}: {error}", self.rustfmt.display()))?;
        if !output.status.success() {
            return Err(format!(
                "{} --version failed ({}): {}",
                self.rustfmt.display(),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// `cargo fmt` with the pinned formatter: the pinned cargo-fmt by path,
    /// with RUSTFMT set to `wrapper` and the wrapper's variable to the pinned
    /// rustfmt, both overriding any inherited value. The cargo that cargo-fmt
    /// runs for `cargo metadata` only lists packages.
    fn cargo_fmt(&self, wrapper: &RustfmtWrapper) -> Command {
        let mut command = Command::new(&self.cargo_fmt);
        command
            .env("RUSTFMT", &wrapper.path)
            .env(WRAPPED_RUSTFMT_ENV, &self.rustfmt);
        command
    }
}

/// The rustfmt wrapper (see the module documentation): runs the rustfmt that
/// [`WRAPPED_RUSTFMT_ENV`] names with the wrapper's arguments, and exits with
/// the shell's status for that run, naming a death by signal on stderr.
fn rustfmt_wrapper_script() -> String {
    format!(
        "#!/bin/sh\n\
         \"${{{WRAPPED_RUSTFMT_ENV}:?}}\" \"$@\"\n\
         status=$?\n\
         if [ \"$status\" -gt 128 ]; then\n    \
         echo \"cargo-fmt-pinned: ${WRAPPED_RUSTFMT_ENV} ended with status $status (signal \
         $((status - 128))) before checking every file it was given\" >&2\n\
         fi\n\
         exit \"$status\"\n"
    )
}

/// A generated rustfmt wrapper, removed when dropped.
struct RustfmtWrapper {
    path: PathBuf,
}

impl Drop for RustfmtWrapper {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Writes the rustfmt wrapper into `dir` as `.tmp-rustfmt-gate-PID-N.rustfmt`
/// (`.tmp*` keeps it out of git), executable by its owner. N numbers this
/// process's wrappers: the unit tests run self-tests concurrently.
fn write_rustfmt_wrapper(dir: &Path) -> Result<RustfmtWrapper, String> {
    static WRAPPERS: AtomicUsize = AtomicUsize::new(0);
    let path = dir.join(format!(
        "{GATE_CONFIG_PREFIX}{}-{}.rustfmt",
        std::process::id(),
        WRAPPERS.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(&path)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let wrapper = RustfmtWrapper { path };
    file.write_all(rustfmt_wrapper_script().as_bytes())
        .map_err(|error| format!("{}: {error}", wrapper.path.display()))?;
    Ok(wrapper)
}

/// The exit code to report for a formatter process `program` ended with:
/// its own, or for a death by signal, which leaves none, 128 plus the signal
/// number, as a shell reports it, named on stderr.
fn exit_code(status: ExitStatus, program: &Path) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    match status.signal() {
        Some(signal) => {
            eprintln!(
                "cargo-fmt-pinned: {} was killed by signal {signal}",
                program.display()
            );
            128 + signal
        }
        None => {
            eprintln!(
                "cargo-fmt-pinned: {} ended with neither an exit code nor a signal ({status})",
                program.display()
            );
            1
        }
    }
}

/// `cargo fmt ARGS` in `dir` with the pinned formatter, as the check runs it.
fn cargo_fmt_in(
    resolved: &Resolved,
    wrapper: &RustfmtWrapper,
    args: &[OsString],
    dir: &Path,
) -> Command {
    let mut command = resolved.cargo_fmt(wrapper);
    command.args(args).current_dir(dir);
    command
}

/// Runs [`cargo_fmt_in`] with the caller's output streams, its rustfmt
/// wrapper in `dir`; its exit status.
fn run_cargo_fmt(resolved: &Resolved, args: &[OsString], dir: &Path) -> Result<i32, String> {
    let wrapper = write_rustfmt_wrapper(dir)?;
    let status = cargo_fmt_in(resolved, &wrapper, args, dir)
        .status()
        .map_err(|error| format!("cannot run {}: {error}", resolved.cargo_fmt.display()))?;
    Ok(exit_code(status, &resolved.cargo_fmt))
}

/// The line rustfmt's `-v` prints for each file it formats.
const FORMATTING_PREFIX: &[u8] = b"Formatting ";

/// The `--workspace-members` run: cargo fmt over the members, then rustfmt
/// file by file (`--skip-children`) over every tracked `.rs` file that pass
/// did not reach, such as rust-scripts, include! targets and sources outside
/// any member's module tree; listed exclusions are skipped by both. The
/// members pass runs with rustfmt's `-v`, whose per-file `Formatting` lines
/// name what it reached; those and the other `-v`-only lines are left out of
/// what it prints, so its output reads as without `-v`. Returns the first
/// nonzero status, else 0.
fn run_workspace_members(
    resolved: &Resolved,
    root: &Path,
    scoped: Vec<OsString>,
    config: &GateConfig,
) -> Result<i32, String> {
    let rustfmt_args: Vec<OsString> = match scoped.iter().position(|arg| arg == "--") {
        Some(separator) => scoped[separator + 1..].to_vec(),
        None => Vec::new(),
    };
    let mut cargo_args = with_gate_config(scoped, &config.path)?;
    cargo_args.push("-v".into());
    let wrapper = write_rustfmt_wrapper(root)?;
    let output = cargo_fmt_in(resolved, &wrapper, &cargo_args, root)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|error| format!("cannot run {}: {error}", resolved.cargo_fmt.display()))?;
    let members_code = exit_code(output.status, &resolved.cargo_fmt);
    let reached = reached_files(&output.stdout, root, &mut std::io::stdout())?;
    if reached.is_empty() {
        return Err(
            "the workspace members pass printed no `Formatting` line, so what it reached is \
             unknown; refusing rather than reporting the per-file pass"
                .to_string(),
        );
    }
    let listed: Vec<String> = load_exclusions(root)?
        .into_iter()
        .map(|entry| entry.path)
        .collect();
    let tracked = tracked_rust_files(root)?;
    let remaining = per_file_targets(&tracked, &reached, &listed);
    eprintln!(
        "cargo-fmt-pinned: per-file pass over {} of the {} tracked .rs files; the workspace \
         members pass reached {}, and {} are listed in {EXCLUSIONS_FILE}, left alone",
        remaining.len(),
        tracked.len(),
        tracked.iter().filter(|path| reached.contains(path)).count(),
        tracked.iter().filter(|path| listed.contains(path)).count()
    );
    let mut files_code = 0;
    if !remaining.is_empty() {
        let status = Command::new(&resolved.rustfmt)
            .args(&rustfmt_args)
            .arg("--config-path")
            .arg(&config.path)
            .args(["--unstable-features", "--skip-children"])
            .args(&remaining)
            .current_dir(root)
            .status()
            .map_err(|error| format!("cannot run {}: {error}", resolved.rustfmt.display()))?;
        files_code = exit_code(status, &resolved.rustfmt);
    }
    Ok(if members_code != 0 {
        members_code
    } else {
        files_code
    })
}

/// Writes `stdout` of a `-v` formatting run to `out` without its
/// `-v`-only lines, and returns the repository-relative paths its
/// `Formatting` lines name. `-v` also marks each diff line's end with `⏎`,
/// which is removed.
fn reached_files(
    stdout: &[u8],
    root: &Path,
    out: &mut dyn std::io::Write,
) -> Result<Vec<String>, String> {
    let mut reached = Vec::new();
    for line in stdout.split_inclusive(|byte| *byte == b'\n') {
        let text = line.strip_suffix(b"\n").unwrap_or(line);
        if let Some(path) = text.strip_prefix(FORMATTING_PREFIX) {
            let path = Path::new(std::str::from_utf8(path).map_err(|_| {
                format!(
                    "a non-UTF-8 `Formatting` line: {}",
                    String::from_utf8_lossy(text)
                )
            })?);
            let relative = normalize(&root.join(path))
                .strip_prefix(normalize(root))
                .map(|relative| relative.to_string_lossy().into_owned())
                .unwrap_or_else(|_| path.to_string_lossy().into_owned());
            reached.push(relative);
            continue;
        }
        if text.starts_with(b"Spent ") || text.starts_with(b"Using rustfmt config file ") {
            continue;
        }
        let shown = text.strip_suffix("⏎".as_bytes()).unwrap_or(text);
        let newline: &[u8] = if line.ends_with(b"\n") { b"\n" } else { b"" };
        out.write_all(shown)
            .and_then(|()| out.write_all(newline))
            .map_err(|error| format!("writing the formatter's output: {error}"))?;
    }
    Ok(reached)
}

/// `path` with `.` and `..` components resolved lexically, as git names files.
fn normalize(path: &Path) -> PathBuf {
    let mut normal = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normal.pop();
            }
            other => normal.push(other.as_os_str()),
        }
    }
    normal
}

/// The repository's tracked `.rs` files (`git ls-files`), relative to `root`.
/// No git checkout, or no tracked file, is refused: the per-file pass would
/// check nothing and look like a pass.
fn tracked_rust_files(root: &Path) -> Result<Vec<String>, String> {
    let output = Command::new("git")
        .args(["ls-files", "-z", "--", "*.rs"])
        .current_dir(root)
        .output()
        .map_err(|error| format!("cannot run git ls-files: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "`git ls-files -- '*.rs'` failed in {} ({}): {}; the per-file pass needs a git \
             checkout",
            root.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let files: Vec<String> = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .collect();
    if files.is_empty() {
        return Err(format!(
            "`git ls-files -- '*.rs'` lists no file in {}",
            root.display()
        ));
    }
    Ok(files)
}

/// The tracked files the per-file pass checks: neither reached by the
/// members pass nor listed.
fn per_file_targets(tracked: &[String], reached: &[String], listed: &[String]) -> Vec<String> {
    tracked
        .iter()
        .filter(|path| !reached.contains(path) && !listed.contains(path))
        .cloned()
        .collect()
}

/// Picks the formatter: the toolchain root in `toolchain`, else rustup. With
/// neither it refuses: the pinned validation image is that case today, and its
/// only rustfmt is the stable compiler pin's, which ignores the nightly-only
/// options. `path_rustfmt` is the `rustfmt --version` line on PATH, if any,
/// named in the refusal and never run.
fn select(
    pin: &Pin,
    toolchain: Option<OsString>,
    rustup: bool,
    path_rustfmt: Option<&str>,
) -> Result<Formatter, String> {
    match toolchain {
        Some(root) if !root.is_empty() => Ok(Formatter::Toolchain(PathBuf::from(root))),
        _ if rustup => Ok(Formatter::Rustup(pin.channel.clone())),
        _ => Err(format!(
            "offline image formatter is not the pinned toolchain: {TOOLCHAIN_ENV} is unset and \
             there is no rustup, so the only formatter here is the rustfmt on PATH ({}), not the \
             pinned `{}` (channel {}); refusing rather than checking with it. Follow-up \
             {IMAGE_FOLLOW_UP}. Outside the pinned image, install rustup and toolchain {}, or \
             set {TOOLCHAIN_ENV} to a toolchain root holding bin/rustfmt and bin/cargo-fmt",
            path_rustfmt.map_or_else(|| "none found".to_string(), |line| format!("`{line}`")),
            pin.identity,
            pin.channel,
            pin.channel
        )),
    }
}

/// Refuses a formatter whose version line is not the pinned one.
fn check_identity(found: &str, pin: &Pin, formatter: &Formatter) -> Result<(), String> {
    if found == pin.identity {
        Ok(())
    } else {
        Err(format!(
            "refusing the formatter from {}: its rustfmt --version is `{found}`, \
             but {PIN_FILE} pins `{}` (channel {})",
            formatter.describe(),
            pin.identity,
            pin.channel
        ))
    }
}

/// The resolved formatter, run exactly as the check runs it (`cargo fmt --all
/// -- --check` through [`cargo_fmt_in`], its output kept quiet), must reject a brace import, which
/// the nightly-only `imports_granularity = "Item"` splits, and accept its split
/// form. Stable rustfmt accepts both.
fn self_test(formatter: &Resolved, root: &Path) -> Result<(), String> {
    const BAD: &str = "use std::{fs, path::Path};\n\nfn main() {\n    let _ = fs::metadata(Path::new(\".\"));\n}\n";
    const GOOD: &str = "use std::fs;\nuse std::path::Path;\n\nfn main() {\n    let _ = fs::metadata(Path::new(\".\"));\n}\n";
    let wrapper = write_rustfmt_wrapper(root)?;
    // One directory per call: the unit tests run self-tests concurrently.
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    let scratch = env::temp_dir().join(format!(
        "cargo-fmt-pinned-{}-{}",
        std::process::id(),
        CALLS.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&scratch).map_err(|error| format!("{}: {error}", scratch.display()))?;
    let result = (|| {
        for (name, text, expected) in [("bad", BAD, 1), ("good", GOOD, 0)] {
            let dir = scratch.join(name);
            let src = dir.join("src");
            fs::create_dir_all(&src).map_err(|error| format!("{}: {error}", src.display()))?;
            for (path, contents) in [
                (dir.join("Cargo.toml"), SELF_TEST_MANIFEST),
                (src.join("main.rs"), text),
            ] {
                fs::write(&path, contents)
                    .map_err(|error| format!("{}: {error}", path.display()))?;
            }
            let mut args: Vec<OsString> = ["--all", "--", "--check", "--config-path"]
                .into_iter()
                .map(OsString::from)
                .collect();
            args.push(root.join("rustfmt.toml").into_os_string());
            let status = cargo_fmt_in(formatter, &wrapper, &args, &dir)
                .output()
                .map_err(|error| format!("cannot run {}: {error}", formatter.cargo_fmt.display()))?
                .status;
            // Only the exact exit code counts: a fixture run killed by a
            // signal proves nothing about the formatter, so it fails the
            // self-test rather than passing as the bad fixture's rejection.
            match (status.code(), status.signal()) {
                (Some(code), _) if code == expected => {}
                (Some(code), _) => {
                    return Err(format!(
                        "self-test: cargo fmt --all -- --check on the {name} fixture exited {code}, expected {expected}"
                    ));
                }
                (None, signal) => {
                    return Err(format!(
                        "self-test: cargo fmt --all -- --check on the {name} fixture was killed by signal {}, expected exit {expected}",
                        signal.map_or_else(|| "(unknown)".to_string(), |signal| signal.to_string())
                    ));
                }
            }
        }
        Ok(())
    })();
    let _ = fs::remove_dir_all(&scratch);
    result
}

/// The repository root: the nearest directory, from the working directory up,
/// holding the pin file.
fn repository_root() -> Result<PathBuf, String> {
    let start = env::current_dir().map_err(|error| format!("working directory: {error}"))?;
    start
        .ancestors()
        .find(|dir| dir.join(PIN_FILE).is_file())
        .map(Path::to_path_buf)
        .ok_or_else(|| format!("no {PIN_FILE} in {} or above it", start.display()))
}

/// The `[workspace] members` directories named by a root manifest, in
/// declaration order. Pure text work (no Cargo run) so the scope decision
/// is testable offline. A glob member is refused, never silently dropped:
/// formatting fewer packages than the workspace claims is a blind gate.
fn workspace_member_dirs(manifest: &str) -> Result<Vec<String>, String> {
    let mut in_workspace = false;
    let mut members: Option<Vec<String>> = None;
    let mut collecting = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            if collecting {
                break;
            }
            in_workspace = trimmed == "[workspace]";
            continue;
        }
        if !in_workspace {
            continue;
        }
        if !collecting {
            // Line-anchored so `default-members` is never taken for `members`.
            let Some(rest) = trimmed.strip_prefix("members") else {
                continue;
            };
            let Some(rest) = rest.trim_start().strip_prefix('=') else {
                continue;
            };
            members = Some(Vec::new());
            collecting = true;
            collect_member_strings(rest, members.as_mut().expect("just set"))?;
            if rest.contains(']') {
                break;
            }
            continue;
        }
        collect_member_strings(trimmed, members.as_mut().expect("collecting"))?;
        if trimmed.contains(']') {
            break;
        }
    }
    members.ok_or_else(|| "root Cargo.toml names no [workspace] members".to_string())
}

fn collect_member_strings(text: &str, out: &mut Vec<String>) -> Result<(), String> {
    for quoted in text.split('"').skip(1).step_by(2) {
        if quoted.is_empty() {
            continue;
        }
        if quoted.contains('*') {
            return Err(format!(
                "workspace member {quoted:?} is a glob; --workspace-members refuses to \
                 guess its expansion rather than format a narrower scope than claimed"
            ));
        }
        out.push(quoted.to_string());
    }
    Ok(())
}

/// The `[package] name` of one member manifest.
fn package_name(manifest: &str) -> Option<String> {
    let mut in_package = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_package = trimmed == "[package]";
            continue;
        }
        if in_package {
            if let Some(rest) = trimmed.strip_prefix("name") {
                if let Some(rest) = rest.trim_start().strip_prefix('=') {
                    let name = rest.trim().trim_matches('"');
                    if !name.is_empty() {
                        return Some(name.to_string());
                    }
                }
            }
        }
    }
    None
}

/// Package name of every `[workspace] members` entry of the checkout at
/// `root`. This is the `--workspace-members` formatting scope: exactly the
/// packages this repository owns. Since the Reverie path-dependency
/// migration, plain `cargo fmt --all` also descends into the `reverie`
/// submodule's crates and would gate Hermit on another repository's
/// formatting drift (its own repo formats it, with the same options).
fn workspace_member_packages(root: &Path) -> Result<Vec<String>, String> {
    let manifest_path = root.join("Cargo.toml");
    let text = fs::read_to_string(&manifest_path)
        .map_err(|error| format!("{}: {error}", manifest_path.display()))?;
    let mut packages = Vec::new();
    for dir in workspace_member_dirs(&text)? {
        let member_path = root.join(&dir).join("Cargo.toml");
        let member_text = fs::read_to_string(&member_path)
            .map_err(|error| format!("{}: {error}", member_path.display()))?;
        let name = package_name(&member_text)
            .ok_or_else(|| format!("{} names no [package] name", member_path.display()))?;
        packages.push(name);
    }
    if packages.is_empty() {
        return Err("root Cargo.toml [workspace] members is empty".to_string());
    }
    Ok(packages)
}

fn rustup_available() -> bool {
    Command::new("rustup")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

/// The `rustfmt --version` line of the rustfmt on PATH, for the refusal only.
fn path_rustfmt_version() -> Option<String> {
    let output = Command::new("rustfmt").arg("--version").output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn run() -> Result<ExitCode, String> {
    let root = repository_root()?;
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "--check-exclusions") {
        let config = write_gate_config(&root)?;
        println!(
            "cargo-fmt-pinned: {EXCLUSIONS_FILE} is consistent: {} excluded file(s), each a \
             file with a reason, and exactly those in the generated `ignore` list",
            config.excluded
        );
        return Ok(ExitCode::SUCCESS);
    }
    let pin_path = root.join(PIN_FILE);
    let text = fs::read_to_string(&pin_path)
        .map_err(|error| format!("{}: {error}", pin_path.display()))?;
    let pin = parse_pin(&text)?;
    let toolchain = env::var_os(TOOLCHAIN_ENV);
    let rustup = rustup_available();
    let path_rustfmt = match (&toolchain, rustup) {
        (Some(root), _) if !root.is_empty() => None,
        (_, true) => None,
        _ => path_rustfmt_version(),
    };
    let formatter = select(&pin, toolchain, rustup, path_rustfmt.as_deref())?;
    let resolved = formatter.resolve()?;
    check_identity(&resolved.version()?, &pin, &formatter)?;
    match args.first().and_then(|arg| arg.to_str()) {
        Some("--show") => {
            println!(
                "formatter: {}\nrustfmt: {}\ncargo-fmt: {}\nidentity: {}",
                formatter.describe(),
                resolved.rustfmt.display(),
                resolved.cargo_fmt.display(),
                pin.identity
            );
            Ok(ExitCode::SUCCESS)
        }
        Some("--self-test") => match self_test(&resolved, &root) {
            Ok(()) => {
                println!(
                    "cargo-fmt-pinned: self-test passed with {} ({})",
                    formatter.describe(),
                    pin.identity
                );
                Ok(ExitCode::SUCCESS)
            }
            Err(error) => {
                eprintln!("cargo-fmt-pinned: {error}");
                Ok(ExitCode::from(1))
            }
        },
        Some("--workspace-members") => {
            let mut scoped: Vec<OsString> = Vec::new();
            for package in workspace_member_packages(&root)? {
                scoped.push("-p".into());
                scoped.push(package.into());
            }
            scoped.extend(args.into_iter().skip(1));
            let config = write_gate_config(&root)?;
            let code = run_workspace_members(&resolved, &root, scoped, &config)?;
            Ok(ExitCode::from(code.clamp(0, 255) as u8))
        }
        _ => {
            let config = write_gate_config(&root)?;
            let code = run_cargo_fmt(&resolved, &with_gate_config(args, &config.path)?, &root)?;
            Ok(ExitCode::from(code.clamp(0, 255) as u8))
        }
    }
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("cargo-fmt-pinned: {error}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// Days since 1970-01-01 of a `YYYY-MM-DD` date (proleptic Gregorian).
    fn days(date: &str) -> i64 {
        let mut parts = date.split('-').map(|part| part.parse::<i64>().unwrap());
        let (y, m, d) = (
            parts.next().unwrap(),
            parts.next().unwrap(),
            parts.next().unwrap(),
        );
        let y = if m <= 2 { y - 1 } else { y };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    /// The repository root, from this file's own path: rust-script runs the
    /// tests in its cache directory, outside the repository.
    fn test_root() -> PathBuf {
        Path::new(file!())
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf()
    }

    fn real_pin() -> Pin {
        let root = test_root();
        parse_pin(&fs::read_to_string(root.join(PIN_FILE)).unwrap()).unwrap()
    }

    #[test]
    fn workspace_members_scope_reads_members_not_default_members() {
        let manifest = "[workspace]\nmembers = [\n  \"alpha\",\n  \"beta/gamma\",\n]\nexclude = [\"agent-utils\"]\ndefault-members = [\n  \"alpha\",\n]\nresolver = \"2\"\n";
        assert_eq!(
            workspace_member_dirs(manifest).unwrap(),
            ["alpha", "beta/gamma"]
        );
        // Single-line spelling, and a glob refused rather than dropped.
        assert_eq!(
            workspace_member_dirs("[workspace]\nmembers = [\"only\"]\n").unwrap(),
            ["only"]
        );
        assert!(workspace_member_dirs("[workspace]\nmembers = [\"crates/*\"]\n").is_err());
        assert_eq!(
            package_name("[package]\nname = \"hermit-run\"\nversion = \"0.5.7\"\n").as_deref(),
            Some("hermit-run")
        );
        assert_eq!(package_name("[workspace]\nmembers = []\n"), None);
    }

    /// The pinned identity is a nightly rustfmt built from a commit dated the
    /// channel's date or the day before: bumping the channel without the
    /// identity, or the identity without the channel, fails here.
    #[test]
    fn the_pinned_identity_and_channel_agree() {
        let pin = real_pin();
        let channel_date = pin
            .channel
            .strip_prefix("nightly-")
            .expect("a dated nightly channel");
        let (version, build) = pin
            .identity
            .strip_prefix("rustfmt ")
            .and_then(|rest| rest.split_once(" ("))
            .expect("`rustfmt VERSION (COMMIT DATE)`");
        assert!(
            version.ends_with("-nightly"),
            "not a nightly rustfmt: {}",
            pin.identity
        );
        let (commit, commit_date) = build
            .strip_suffix(')')
            .and_then(|inner| inner.split_once(' '))
            .expect("`(COMMIT DATE)`");
        assert!(
            commit.len() >= 9 && commit.chars().all(|c| c.is_ascii_hexdigit()),
            "not a commit hash: {commit}"
        );
        let behind = days(channel_date) - days(commit_date);
        assert!(
            (0..=1).contains(&behind),
            "{} is from {commit_date}, {behind} days from channel {}",
            pin.identity,
            pin.channel
        );
    }

    #[test]
    fn the_date_arithmetic_is_right() {
        assert_eq!(days("1970-01-01"), 0);
        assert_eq!(days("2026-07-12") - days("2026-07-11"), 1);
        assert_eq!(days("2026-03-01") - days("2026-02-28"), 1);
        assert_eq!(days("2024-03-01") - days("2024-02-28"), 2);
    }

    #[test]
    fn the_pin_file_takes_each_key_exactly_once() {
        let good = "# c\nchannel = \"nightly-2026-07-12\"\nidentity = \"rustfmt x\"\n";
        assert_eq!(
            parse_pin(good),
            Ok(Pin {
                channel: "nightly-2026-07-12".into(),
                identity: "rustfmt x".into(),
            })
        );
        assert!(
            parse_pin("channel = \"a\"\n")
                .unwrap_err()
                .contains("no `identity`")
        );
        assert!(
            parse_pin("identity = \"a\"\n")
                .unwrap_err()
                .contains("no `channel`")
        );
        assert!(
            parse_pin("channel = \"a\"\nchannel = \"b\"\nidentity = \"c\"\n")
                .unwrap_err()
                .contains("set twice")
        );
        assert!(
            parse_pin("channel = \"a\"\nidentity = \"c\"\nother = \"d\"\n")
                .unwrap_err()
                .contains("unknown key")
        );
        assert!(
            parse_pin("channel = a\nidentity = \"c\"\n")
                .unwrap_err()
                .contains("expected")
        );
    }

    #[test]
    fn the_toolchain_root_wins_over_rustup_and_nothing_refuses() {
        let pin = Pin {
            channel: "nightly-2026-07-12".into(),
            identity: "rustfmt x".into(),
        };
        assert_eq!(
            select(&pin, Some("/opt/fmt".into()), true, None),
            Ok(Formatter::Toolchain("/opt/fmt".into()))
        );
        assert_eq!(
            select(&pin, Some("".into()), true, None),
            Ok(Formatter::Rustup("nightly-2026-07-12".into()))
        );
        assert_eq!(
            select(&pin, None, true, None),
            Ok(Formatter::Rustup("nightly-2026-07-12".into()))
        );
        assert!(
            select(&pin, None, false, None)
                .unwrap_err()
                .contains("offline image formatter is not the pinned toolchain")
        );
    }

    /// The pinned image today: no toolchain root, no rustup, and the stable
    /// compiler pin's rustfmt on PATH. The selector refuses by name, pointing
    /// at the follow-up, and never falls back to that rustfmt.
    #[test]
    fn the_pinned_image_without_the_formatter_is_refused_by_name() {
        let pin = real_pin();
        let stable = "rustfmt 1.9.0-stable (8bab26f4f6 2026-07-14)";
        for toolchain in [None, Some(OsString::new())] {
            let error = select(&pin, toolchain, false, Some(stable)).unwrap_err();
            assert!(
                error.starts_with("offline image formatter is not the pinned toolchain"),
                "{error}"
            );
            assert!(error.contains(IMAGE_FOLLOW_UP), "{error}");
            assert!(error.contains(&format!("`{stable}`")), "{error}");
            assert!(error.contains(&pin.identity), "{error}");
        }
        assert_eq!(
            IMAGE_FOLLOW_UP,
            "https://github.com/rrnewton/hermit/issues/5397"
        );
    }

    /// A toolchain root under the system temporary directory whose
    /// `bin/rustfmt` prints `version` and runs `check` (a shell fragment
    /// given the file as $last) for `--check`.
    fn fake_toolchain(label: &str, version: &str, check: &str) -> PathBuf {
        let root = env::temp_dir().join(format!(
            "cargo-fmt-pinned-test-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("bin")).unwrap();
        let rustfmt = format!(
            "#!/bin/sh\nif [ \"$1\" = --version ]; then echo '{version}'; exit 0; fi\nfor last; do :; done\n{check}\n"
        );
        executable(&root.join("bin/rustfmt"), &rustfmt);
        executable(&root.join("bin/cargo-fmt"), FAKE_CARGO_FMT);
        root
    }

    /// A stand-in for cargo-fmt: runs $RUSTFMT --check on each src/*.rs of the
    /// package in the working directory, as the real one does through RUSTFMT.
    const FAKE_CARGO_FMT: &str = "#!/bin/sh\nrc=0\nfor f in src/*.rs; do \"$RUSTFMT\" --check \"$f\" || rc=$?; done\nexit $rc\n";

    /// A fake rustfmt check that rejects a brace import, as the pinned one does.
    const STRICT: &str = "if grep -q 'use std::{' \"$last\"; then exit 1; fi\nexit 0";

    fn executable(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn a_formatter_with_another_identity_is_refused() {
        let pin = real_pin();
        let stable = fake_toolchain(
            "stable",
            "rustfmt 1.9.0-stable (8bab26f4f6 2026-07-14)",
            "exit 0",
        );
        let formatter = Formatter::Toolchain(stable.clone());
        let found = formatter.resolve().unwrap().version().unwrap();
        let error = check_identity(&found, &pin, &formatter).unwrap_err();
        assert!(error.contains("refusing the formatter"), "{error}");
        assert!(error.contains(&pin.identity), "{error}");
        let pinned = fake_toolchain("pinned", &pin.identity, "exit 0");
        let formatter = Formatter::Toolchain(pinned.clone());
        assert_eq!(
            check_identity(
                &formatter.resolve().unwrap().version().unwrap(),
                &pin,
                &formatter
            ),
            Ok(())
        );
        fs::remove_dir_all(stable).unwrap();
        fs::remove_dir_all(pinned).unwrap();
    }

    /// The self-test passes only for a formatter that rejects the brace import
    /// and accepts its split form; one that accepts both, as stable rustfmt
    /// does, fails it.
    #[test]
    fn the_self_test_fails_a_formatter_that_accepts_a_brace_import() {
        let root = test_root();
        let strict = fake_toolchain("strict", "x", STRICT);
        let resolved = Formatter::Toolchain(strict.clone()).resolve().unwrap();
        assert_eq!(self_test(&resolved, &root), Ok(()));
        let lenient = fake_toolchain("lenient", "x", "exit 0");
        let resolved = Formatter::Toolchain(lenient.clone()).resolve().unwrap();
        let error = self_test(&resolved, &root).unwrap_err();
        assert!(error.contains("bad fixture exited 0"), "{error}");
        fs::remove_dir_all(strict).unwrap();
        fs::remove_dir_all(lenient).unwrap();
    }

    /// The command that formats names the pinned cargo-fmt by absolute path,
    /// sets RUSTFMT to the rustfmt wrapper and the wrapper's variable to the
    /// pinned rustfmt, so the child cannot inherit another one; a toolchain
    /// root missing either executable is refused.
    #[test]
    fn the_formatting_command_binds_both_pinned_executables() {
        let root = fake_toolchain("bound", "x", "exit 0");
        let resolved = Formatter::Toolchain(root.clone()).resolve().unwrap();
        let canonical = fs::canonicalize(&root).unwrap();
        assert_eq!(resolved.rustfmt, canonical.join("bin/rustfmt"));
        assert_eq!(resolved.cargo_fmt, canonical.join("bin/cargo-fmt"));
        let wrapper = write_rustfmt_wrapper(&root).unwrap();
        let command = resolved.cargo_fmt(&wrapper);
        assert_eq!(Path::new(command.get_program()), resolved.cargo_fmt);
        assert!(resolved.cargo_fmt.is_absolute());
        let env = |wanted: &str| {
            command
                .get_envs()
                .find(|(name, _)| *name == wanted)
                .and_then(|(_, value)| value)
        };
        assert_eq!(env("RUSTFMT"), Some(wrapper.path.as_os_str()));
        assert_eq!(env(WRAPPED_RUSTFMT_ENV), Some(resolved.rustfmt.as_os_str()));
        drop(wrapper);
        fs::remove_file(root.join("bin/cargo-fmt")).unwrap();
        let error = Formatter::Toolchain(root.clone()).resolve().unwrap_err();
        assert!(error.contains("has no"), "{error}");
        fs::remove_dir_all(root).unwrap();
    }

    const PROBE_CRATE: &str = "CARGO_FMT_PINNED_PROBE_CRATE";
    const PROBE_IDENTITY: &str = "rustfmt 0.0.0-nightly (fake pinned)";

    /// The final formatting path, run in a child test process (see
    /// [`final_path_with`]) whose environment carries the decoy under test.
    #[test]
    #[ignore = "run by the_final_path_ignores_* in a child process"]
    fn final_path_probe() {
        let Some(krate) = env::var_os(PROBE_CRATE) else {
            return;
        };
        let pin = Pin {
            channel: "nightly-2026-07-12".into(),
            identity: PROBE_IDENTITY.into(),
        };
        let formatter = select(&pin, env::var_os(TOOLCHAIN_ENV), rustup_available(), None).unwrap();
        assert_eq!(formatter, Formatter::Rustup(pin.channel.clone()));
        let resolved = formatter.resolve().unwrap();
        check_identity(&resolved.version().unwrap(), &pin, &formatter).unwrap();
        let args: Vec<OsString> = ["--all", "--", "--check"].map(OsString::from).to_vec();
        assert_eq!(
            run_cargo_fmt(&resolved, &args, Path::new(&krate)),
            Ok(1),
            "the brace import passed the final path"
        );
    }

    /// Runs [`final_path_probe`] in a child process: a fake rustup first on
    /// PATH reports a fake pinned toolchain (strict rustfmt with the probe
    /// identity) as the channel's sysroot, and runs anything else from PATH;
    /// `decoys` adds the bypass under test. The package holds a brace import.
    fn final_path_with(label: &str, decoys: &[(&str, &Path)], path_front: Option<&Path>) {
        let base = env::temp_dir().join(format!(
            "cargo-fmt-pinned-probe-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        let pinned = fake_toolchain(&format!("probe-pinned-{label}"), PROBE_IDENTITY, STRICT);
        executable(
            &base.join("rustup/rustup"),
            &format!(
                "#!/bin/sh\n[ \"$1\" = --version ] && {{ echo 'rustup (fake)'; exit 0; }}\n[ \"$1\" = run ] || exit 1\nshift; [ \"$1\" = --install ] && shift; shift\nif [ \"$*\" = 'rustc --print sysroot' ]; then echo '{}'; exit 0; fi\nexec \"$@\"\n",
                pinned.display()
            ),
        );
        let krate = base.join("crate");
        executable(
            &krate.join("src/main.rs"),
            "use std::{fs, path::Path};\n\nfn main() {\n    let _ = fs::metadata(Path::new(\".\"));\n}\n",
        );
        let mut path = vec![base.join("rustup")];
        path.extend(path_front.map(Path::to_path_buf));
        path.extend(env::split_paths(&env::var_os("PATH").unwrap_or_default()));
        let mut child = Command::new(env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "tests::final_path_probe",
                "--ignored",
                "--test-threads=1",
            ])
            .env_remove(TOOLCHAIN_ENV)
            .env("PATH", env::join_paths(path).unwrap())
            .env(PROBE_CRATE, &krate);
        for (name, value) in decoys {
            child.env(name, value);
        }
        let output = child.output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("test tests::final_path_probe ... ok"),
            "{label}: {}\n{stdout}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        fs::remove_dir_all(base).unwrap();
        fs::remove_dir_all(pinned).unwrap();
    }

    /// Bypass (a): an inherited RUSTFMT naming a formatter that accepts the
    /// brace import does not reach the final path.
    #[test]
    fn the_final_path_ignores_an_inherited_rustfmt() {
        let lenient = fake_toolchain("probe-lenient-a", "rustfmt 1.9.0-stable (fake)", "exit 0");
        final_path_with("a", &[("RUSTFMT", &lenient.join("bin/rustfmt"))], None);
        fs::remove_dir_all(lenient).unwrap();
    }

    /// Bypass (b): a CARGO_HOME whose bin holds a cargo-fmt (and cargo and
    /// rustfmt) that accept the brace import, also first on PATH, does not
    /// reach the final path.
    #[test]
    fn the_final_path_ignores_a_cargo_fmt_in_cargo_home() {
        let home = fake_toolchain("probe-home-b", "rustfmt 1.9.0-stable (fake)", "exit 0");
        executable(&home.join("bin/cargo-fmt"), "#!/bin/sh\nexit 0\n");
        executable(
            &home.join("bin/cargo"),
            "#!/bin/sh\n[ \"$1\" = fmt ] && shift\nexec \"$(dirname \"$0\")/cargo-fmt\" \"$@\"\n",
        );
        final_path_with("b", &[("CARGO_HOME", &home)], Some(&home.join("bin")));
        fs::remove_dir_all(home).unwrap();
    }

    /// A fixture run killed by a signal is a failed self-test, never the bad
    /// fixture's expected rejection: here cargo-fmt kills itself with SIGTERM
    /// on the brace-import package only, and the formatter otherwise behaves
    /// as the pinned one does.
    #[test]
    fn a_self_test_fixture_killed_by_a_signal_fails_the_self_test() {
        let root = test_root();
        let toolchain = fake_toolchain("signalled", "x", STRICT);
        executable(
            &toolchain.join("bin/cargo-fmt"),
            &format!(
                "#!/bin/sh\nif grep -q 'use std::{{' src/main.rs; then kill -TERM $$; fi\n{}",
                FAKE_CARGO_FMT.trim_start_matches("#!/bin/sh\n")
            ),
        );
        let resolved = Formatter::Toolchain(toolchain.clone()).resolve().unwrap();
        let error = self_test(&resolved, &root).unwrap_err();
        assert!(
            error.contains("bad fixture was killed by signal 15"),
            "{error}"
        );
        fs::remove_dir_all(toolchain).unwrap();
    }

    /// The rustfmt wrapper passes rustfmt's arguments, output and exit code
    /// through, turns a death by signal into 128 plus the signal number,
    /// named on stderr, refuses to run when no rustfmt is named, and is
    /// removed when dropped.
    #[test]
    fn the_rustfmt_wrapper_fails_a_rustfmt_killed_by_a_signal() {
        let dir = env::temp_dir().join(format!("cargo-fmt-pinned-wrapper-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let wrapper = write_rustfmt_wrapper(&dir).unwrap();
        let run = |label: &str, script: &str| {
            let rustfmt = dir.join(label);
            executable(&rustfmt, script);
            Command::new(&wrapper.path)
                .args(["--check", "a b.rs"])
                .env(WRAPPED_RUSTFMT_ENV, &rustfmt)
                .output()
                .unwrap()
        };
        for code in [0, 1] {
            let output = run(
                &format!("exit-{code}"),
                &format!("#!/bin/sh\nprintf '%s|' \"$@\"\nexit {code}\n"),
            );
            assert_eq!(output.status.code(), Some(code));
            assert_eq!(String::from_utf8_lossy(&output.stdout), "--check|a b.rs|");
            assert!(
                output.stderr.is_empty(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        // SIGKILL, as an out-of-memory kill sends it.
        let output = run("killed", "#!/bin/sh\necho partial\nkill -KILL $$\n");
        assert_eq!(output.status.code(), Some(128 + 9));
        assert_eq!(String::from_utf8_lossy(&output.stdout), "partial\n");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("ended with status 137 (signal 9)"),
            "{stderr}"
        );
        let output = Command::new(&wrapper.path)
            .env_remove(WRAPPED_RUSTFMT_ENV)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{}", output.status);
        let path = wrapper.path.clone();
        assert_eq!(path.parent(), Some(dir.as_path()));
        drop(wrapper);
        assert!(!path.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    /// A formatter process the script runs directly that dies by a signal
    /// reports 128 plus the signal number, never success; an exit code is
    /// reported as it is.
    #[test]
    fn a_formatter_killed_by_a_signal_never_reads_as_success() {
        let status = |script: &str| Command::new("sh").args(["-c", script]).status().unwrap();
        let rustfmt = Path::new("rustfmt");
        assert_eq!(exit_code(status("kill -KILL $$"), rustfmt), 128 + 9);
        assert_eq!(exit_code(status("exit 0"), rustfmt), 0);
        assert_eq!(exit_code(status("exit 3"), rustfmt), 3);
    }

    /// Every malformed entry is refused by line: a tab anywhere, no reason, a
    /// reason not of the one form, a duplicate, and paths that are not one
    /// repository-relative `.rs` file. Any run of spaces separates PATH from
    /// REASON, which is trimmed.
    #[test]
    fn the_exclusion_list_refuses_malformed_entries() {
        let good = "# comment\n\na/b.rs sha256-pinned in c.json\n\
                    c/d.rs     sha256-pinned in e/f.json and g.py   \n";
        assert_eq!(
            parse_exclusions(good),
            Ok(vec![
                Exclusion {
                    line: 3,
                    path: "a/b.rs".into(),
                    pinned_in: vec!["c.json".into()],
                },
                Exclusion {
                    line: 4,
                    path: "c/d.rs".into(),
                    pinned_in: vec!["e/f.json".into(), "g.py".into()],
                },
            ])
        );
        for (text, expected) in [
            ("a/b.rs\tsha256-pinned in c.json\n", ":1: has a tab"),
            (
                "a/b.rs sha256-pinned in c.json\n# a\tcomment\n",
                ":2: has a tab",
            ),
            ("a/b.rs\n", "has no reason (expected `PATH REASON`)"),
            ("a/b.rs \n", "has no reason"),
            ("a/b.rs    \n", "has no reason"),
            (
                " a/b.rs sha256-pinned in c.json\n",
                "starts with whitespace",
            ),
            (
                "a/b.rs embedded by c.rs\n",
                "reason `embedded by c.rs` is not `sha256-pinned in FILE[ and FILE...]`",
            ),
            ("a/b.rs sha256-pinned in\n", "is not `sha256-pinned in"),
            (
                "a/b.rs sha256-pinned in /c.json\n",
                "not a repository-relative path",
            ),
            (
                "a/b.rs sha256-pinned in c/../d.json\n",
                "not a repository-relative path",
            ),
            (
                "a/b.rs sha256-pinned in c.json and\n",
                "not a repository-relative path",
            ),
            (
                "a/b.rs sha256-pinned in c.json, d.json\n",
                "not a repository-relative path",
            ),
            (
                "a/b.rs sha256-pinned in ci/rustfmt-exclusions.txt\n",
                ":1: `a/b.rs`'s reason names ci/rustfmt-exclusions.txt, the list itself",
            ),
            (
                "a/b.rs sha256-pinned in c.json and ci/rustfmt-exclusions.txt\n",
                "the list itself",
            ),
            (
                "a/b.rs sha256-pinned in ./ci/rustfmt-exclusions.txt\n",
                "not a repository-relative path",
            ),
            (
                "a/b.rs sha256-pinned in ci/../ci/rustfmt-exclusions.txt\n",
                "not a repository-relative path",
            ),
            (
                "a/b.rs sha256-pinned in c/d.rs\nc/d.rs sha256-pinned in e.json\n",
                ":1: `a/b.rs`'s reason names `c/d.rs`, which is itself listed",
            ),
            ("a/b.rs sha256-pinned in a/b.rs\n", "which is itself listed"),
            (
                "a/b.rs sha256-pinned in c.json\na/b.rs sha256-pinned in c.json\n",
                "listed twice",
            ),
            (
                "d.rs sha256-pinned in c.json\n",
                "has no `/`; rustfmt would read it as a pattern matching that name in every \
                 directory",
            ),
            ("/a/b.rs sha256-pinned in c.json\n", "not relative"),
            ("a/../b.rs sha256-pinned in c.json\n", "`..` component"),
            ("a//b.rs sha256-pinned in c.json\n", "empty"),
            ("a/*.rs sha256-pinned in c.json\n", "pattern character"),
            ("a/b.txt sha256-pinned in c.json\n", "not a .rs path"),
        ] {
            let error = parse_exclusions(text).unwrap_err();
            assert!(error.contains(expected), "{text:?}: {error}");
            assert!(error.starts_with(EXCLUSIONS_FILE), "{error}");
        }
        assert_eq!(
            exclusion_path_problem("a/b c.rs"),
            Some("has whitespace or a gitignore pattern character")
        );
    }

    /// The generated configuration is the repository's unchanged plus exactly
    /// the listed paths, read back the same; a repository configuration with
    /// its own `ignore` is refused.
    #[test]
    fn the_generated_configuration_ignores_exactly_the_list() {
        let list =
            parse_exclusions("a/b.rs sha256-pinned in p.json\nc/d.rs sha256-pinned in p.json\n")
                .unwrap();
        let root_config = "edition = \"2024\"\nmerge_derives = false\n";
        let text = gate_config(root_config, &list).unwrap();
        assert!(text.starts_with(root_config), "{text}");
        assert_eq!(config_ignore_entries(&text).unwrap(), ["a/b.rs", "c/d.rs"]);
        let empty = gate_config(root_config, &[]).unwrap();
        assert_eq!(config_ignore_entries(&empty).unwrap(), Vec::<String>::new());
        let error = gate_config("ignore = [\"z.rs\"]\n", &list).unwrap_err();
        assert!(error.contains("sets `ignore`"), "{error}");
        assert!(
            config_ignore_entries(&format!("{text}ignore = [\n]\n"))
                .unwrap_err()
                .contains("set twice")
        );
        assert!(
            config_ignore_entries("ignore = [\n    \"a.rs\",\n")
                .unwrap_err()
                .contains("not closed")
        );
        assert!(
            config_ignore_entries(root_config)
                .unwrap_err()
                .contains("no `ignore`")
        );
    }

    /// `--config-path` goes after `--`, which is added when absent, and an
    /// explicit one is refused.
    #[test]
    fn the_configuration_is_passed_to_rustfmt() {
        let config = Path::new("/r/.tmp-rustfmt-gate-1.toml");
        let args = |list: &[&str]| list.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            with_gate_config(args(&["-p", "a", "--", "--check"]), config).unwrap(),
            args(&[
                "-p",
                "a",
                "--",
                "--config-path",
                "/r/.tmp-rustfmt-gate-1.toml",
                "--check"
            ])
        );
        assert_eq!(
            with_gate_config(args(&["-p", "a"]), config).unwrap(),
            args(&[
                "-p",
                "a",
                "--",
                "--config-path",
                "/r/.tmp-rustfmt-gate-1.toml"
            ])
        );
        for explicit in ["--config-path", "--config-path=x.toml"] {
            let error = with_gate_config(args(&["--all", "--", explicit]), config).unwrap_err();
            assert!(error.contains("do not pass one"), "{error}");
        }
    }

    /// In a checkout: a listed path that is missing is refused, and so is an
    /// entry whose reason names a missing file or a file without its sha256;
    /// the written configuration matches the list, and it is removed when
    /// dropped.
    #[test]
    fn the_configuration_is_written_into_the_root_and_removed() {
        let root = env::temp_dir().join(format!("cargo-fmt-pinned-gate-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("ci")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join(ROOT_CONFIG), "edition = \"2024\"\n").unwrap();
        fs::write(root.join("src/pinned.rs"), "fn  f() {}\n").unwrap();
        let digest = sha256_hex(b"fn  f() {}\n");
        fs::write(
            root.join("pins.json"),
            format!("{{\"src/pinned.rs\": \"{digest}\"}}\n"),
        )
        .unwrap();
        fs::write(root.join("other.json"), "{}\n").unwrap();
        for (list, expected) in [
            (
                "src/gone.rs sha256-pinned in pins.json\n",
                "`src/gone.rs` is missing",
            ),
            (
                "src/pinned.rs sha256-pinned in gone.json\n",
                "`src/pinned.rs`'s reason names `gone.json`, which is missing",
            ),
            (
                "src/pinned.rs sha256-pinned in other.json\n",
                "is not in `other.json`, so its reason does not hold",
            ),
            (
                "src/pinned.rs sha256-pinned in pins.json and other.json\n",
                "is not in `other.json`",
            ),
        ] {
            fs::write(root.join(EXCLUSIONS_FILE), list).unwrap();
            let error = write_gate_config(&root).err().unwrap();
            assert!(error.contains(expected), "{list:?}: {error}");
        }
        fs::write(
            root.join(EXCLUSIONS_FILE),
            "src/pinned.rs sha256-pinned in pins.json\n",
        )
        .unwrap();
        let config = write_gate_config(&root).unwrap();
        assert_eq!(config.path.parent(), Some(root.as_path()));
        let written = fs::read_to_string(&config.path).unwrap();
        assert_eq!(config_ignore_entries(&written).unwrap(), ["src/pinned.rs"]);
        let path = config.path.clone();
        drop(config);
        assert!(!path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    /// The list cannot vouch for itself: the review's repro (the listed file's
    /// digest in a list comment, an entry citing the list) is refused, and so
    /// is a FILE that is the list or a listed file under another name (a hard
    /// link) or a symbolic link to the list.
    #[test]
    fn the_list_and_listed_files_cannot_pin_an_entry() {
        use std::os::unix::fs::symlink;
        let root = env::temp_dir().join(format!("cargo-fmt-pinned-vouch-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("ci")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join(ROOT_CONFIG), "edition = \"2024\"\n").unwrap();
        fs::write(root.join("src/a.rs"), "fn  a() {}\n").unwrap();
        fs::write(root.join("src/b.rs"), "fn  b() {}\n").unwrap();
        let digest = sha256_hex(b"fn  a() {}\n");
        let list = |entry: &str| format!("# {digest}\n{entry}\n");
        let cases = [
            (
                list("src/a.rs sha256-pinned in ci/rustfmt-exclusions.txt"),
                None,
                "ci/rustfmt-exclusions.txt:2: `src/a.rs`'s reason names ci/rustfmt-exclusions.txt, \
                 the list itself",
            ),
            (
                list("src/a.rs sha256-pinned in ci/pins.txt"),
                Some("hard"),
                ":2: `src/a.rs`'s reason names `ci/pins.txt`, which is the same file as \
                 `ci/rustfmt-exclusions.txt`",
            ),
            (
                list("src/a.rs sha256-pinned in ci/pins.txt"),
                Some("soft"),
                ":2: `src/a.rs`'s reason names `ci/pins.txt`, which is not a regular file",
            ),
        ];
        for (text, link, expected) in cases {
            fs::write(root.join(EXCLUSIONS_FILE), &text).unwrap();
            let _ = fs::remove_file(root.join("ci/pins.txt"));
            match link {
                Some("hard") => {
                    fs::hard_link(root.join(EXCLUSIONS_FILE), root.join("ci/pins.txt")).unwrap()
                }
                Some(_) => symlink("rustfmt-exclusions.txt", root.join("ci/pins.txt")).unwrap(),
                None => {}
            }
            let error = write_gate_config(&root).err().unwrap();
            assert!(error.contains(expected), "{text:?} {link:?}: {error}");
        }
        // A listed file under another name: src/c.rs hard-linked to listed src/b.rs.
        let _ = fs::remove_file(root.join("ci/pins.txt"));
        fs::write(root.join("src/b.rs"), format!("// {digest}\n")).unwrap();
        fs::hard_link(root.join("src/b.rs"), root.join("src/c.rs")).unwrap();
        let b_digest = sha256_hex(format!("// {digest}\n").as_bytes());
        fs::write(root.join("pins.json"), format!("{b_digest}\n")).unwrap();
        fs::write(
            root.join(EXCLUSIONS_FILE),
            "src/a.rs sha256-pinned in src/c.rs\nsrc/b.rs sha256-pinned in pins.json\n",
        )
        .unwrap();
        let error = write_gate_config(&root).err().unwrap();
        assert!(
            error.contains(
                ":1: `src/a.rs`'s reason names `src/c.rs`, which is the same file as `src/b.rs`"
            ),
            "{error}"
        );
        fs::remove_dir_all(root).unwrap();
    }

    /// The digest is SHA-256: the FIPS 180-2 vectors, including a multi-block
    /// message.
    #[test]
    fn the_digest_is_sha256() {
        for (data, hex) in [
            (
                b"".as_slice(),
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
            (
                b"abc".as_slice(),
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
            (
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq".as_slice(),
                "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
            ),
        ] {
            assert_eq!(sha256_hex(data), hex);
        }
        assert_eq!(
            sha256_hex(&vec![b'a'; 1_000_000]),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    /// The checked-in list is consistent with this checkout.
    #[test]
    fn the_checked_in_exclusion_list_is_consistent() {
        let root = test_root();
        let config = write_gate_config(&root).unwrap();
        let written = fs::read_to_string(&config.path).unwrap();
        let listed: Vec<String> = load_exclusions(&root)
            .unwrap()
            .into_iter()
            .map(|entry| entry.path)
            .collect();
        assert_eq!(config_ignore_entries(&written).unwrap(), listed);
    }

    /// The members pass's `-v` output: `Formatting` lines become the reached
    /// set (normalized, relative to the root) and are not printed, nor are
    /// the other `-v`-only lines; diff lines print without `-v`'s `⏎`.
    #[test]
    fn the_members_pass_output_names_what_it_reached() {
        let stdout = "Using rustfmt config file /r/.tmp-rustfmt-gate-1.toml\n\
                      Formatting /r/a/src/lib.rs\n\
                      Formatting /r/a/src/../../scripts/x.rs\n\
                      Diff in /r/a/src/lib.rs:1:\n\
                      -fn f( ) {}⏎\n\
                      +fn f() {}⏎\n\
                      Spent 0.001 secs in the parsing phase, and 0.000 secs in the formatting phase\n";
        let mut shown = Vec::new();
        let reached = reached_files(stdout.as_bytes(), Path::new("/r"), &mut shown).unwrap();
        assert_eq!(reached, ["a/src/lib.rs", "scripts/x.rs"]);
        assert_eq!(
            String::from_utf8(shown).unwrap(),
            "Diff in /r/a/src/lib.rs:1:\n-fn f( ) {}\n+fn f() {}\n"
        );
    }

    /// The per-file pass takes every tracked file neither reached nor listed.
    #[test]
    fn the_per_file_pass_takes_what_the_members_pass_left() {
        let strings = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let tracked = strings(&["a/src/lib.rs", "scripts/x.rs", "scripts/y.rs", "pinned.rs"]);
        assert_eq!(
            per_file_targets(
                &tracked,
                &strings(&["a/src/lib.rs"]),
                &strings(&["pinned.rs"])
            ),
            ["scripts/x.rs", "scripts/y.rs"]
        );
        assert_eq!(
            normalize(Path::new("/r/a/./b/../c.rs")),
            PathBuf::from("/r/a/c.rs")
        );
    }

    /// The per-file pass reads the tracked files from git and refuses a
    /// directory that is not a checkout.
    #[test]
    fn the_per_file_pass_needs_a_git_checkout() {
        let dir = env::temp_dir().join(format!("cargo-fmt-pinned-nogit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let error = tracked_rust_files(&dir).unwrap_err();
        assert!(error.contains("needs a git checkout"), "{error}");
        fs::remove_dir_all(dir).unwrap();
        let tracked = tracked_rust_files(&test_root()).unwrap();
        assert!(
            tracked
                .iter()
                .any(|path| path == "scripts/cargo-fmt-pinned.rs")
        );
    }
}
