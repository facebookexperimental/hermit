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
//! Neither, or a formatter whose `rustfmt --version` is not the pinned line,
//! exits 2 before anything is formatted. The rustfmt on PATH is never used.
//! The pinned validation image has neither yet, so the offline split's
//! lint.rustfmt refuses there rather than check with the image's stable
//! rustfmt, until <https://github.com/rrnewton/hermit/issues/5397> packages
//! the pinned formatter in the image.
//!
//! Usage, from anywhere in the repository:
//!   scripts/cargo-fmt-pinned.rs [CARGO-FMT-ARGS...]  e.g. `--all -- --check`;
//!                                                    exits with cargo fmt's status
//!   scripts/cargo-fmt-pinned.rs --self-test          the selected formatter must
//!                                                    reject a brace import (exit 1) and
//!                                                    accept its fixed form (exit 0)
//!   scripts/cargo-fmt-pinned.rs --show               print the selected formatter

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;

/// The pin, relative to the repository root.
const PIN_FILE: &str = "ci/rustfmt-toolchain.toml";
/// A toolchain root holding the pinned formatter.
const TOOLCHAIN_ENV: &str = "HERMIT_RUSTFMT_TOOLCHAIN";
/// The follow-up that packages the pinned formatter in the pinned image.
const IMAGE_FOLLOW_UP: &str = "https://github.com/rrnewton/hermit/issues/5397";

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

    /// A command running this toolchain's `program` (`rustfmt` or `cargo`).
    fn command(&self, program: &str) -> Command {
        match self {
            Formatter::Toolchain(root) => {
                let bin = root.join("bin");
                if program == "cargo" {
                    // cargo-fmt runs the rustfmt that RUSTFMT names; the cargo
                    // it calls for metadata is whichever cargo is on PATH.
                    let mut command = Command::new(bin.join("cargo-fmt"));
                    command.env("RUSTFMT", bin.join("rustfmt"));
                    command
                } else {
                    Command::new(bin.join(program))
                }
            }
            Formatter::Rustup(channel) => {
                let mut command = Command::new("rustup");
                command
                    .arg("run")
                    .arg("--install")
                    .arg(channel)
                    .arg(program);
                if program == "cargo" {
                    command.arg("fmt");
                }
                command
            }
        }
    }

    /// The `rustfmt --version` line of this formatter.
    fn version(&self) -> Result<String, String> {
        let output = self
            .command("rustfmt")
            .arg("--version")
            .output()
            .map_err(|error| format!("cannot run rustfmt from {}: {error}", self.describe()))?;
        if !output.status.success() {
            return Err(format!(
                "rustfmt --version from {} failed ({}): {}",
                self.describe(),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
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

/// The selected formatter must reject a brace import, which the nightly-only
/// `imports_granularity = "Item"` splits, and accept its split form. Stable
/// rustfmt accepts both.
fn self_test(formatter: &Formatter, root: &Path) -> Result<(), String> {
    const BAD: &str = "use std::{fs, path::Path};\n\nfn main() {\n    let _ = fs::metadata(Path::new(\".\"));\n}\n";
    const GOOD: &str = "use std::fs;\nuse std::path::Path;\n\nfn main() {\n    let _ = fs::metadata(Path::new(\".\"));\n}\n";
    let scratch = env::temp_dir().join(format!("cargo-fmt-pinned-{}", std::process::id()));
    fs::create_dir_all(&scratch).map_err(|error| format!("{}: {error}", scratch.display()))?;
    let result = (|| {
        for (name, text, expected) in [("bad.rs", BAD, 1), ("good.rs", GOOD, 0)] {
            let path = scratch.join(name);
            fs::write(&path, text).map_err(|error| format!("{}: {error}", path.display()))?;
            let status = formatter
                .command("rustfmt")
                .arg("--check")
                .arg("--config-path")
                .arg(root.join("rustfmt.toml"))
                .arg(&path)
                .output()
                .map_err(|error| format!("cannot run rustfmt: {error}"))?
                .status;
            if status.code() != Some(expected) {
                return Err(format!(
                    "self-test: rustfmt --check on the {name} fixture exited {status}, expected {expected}"
                ));
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
    check_identity(&formatter.version()?, &pin, &formatter)?;
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    match args.first().and_then(|arg| arg.to_str()) {
        Some("--show") => {
            println!(
                "formatter: {}\nidentity: {}",
                formatter.describe(),
                pin.identity
            );
            Ok(ExitCode::SUCCESS)
        }
        Some("--self-test") => match self_test(&formatter, &root) {
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
        _ => {
            let status = formatter
                .command("cargo")
                .args(&args)
                .current_dir(&root)
                .status()
                .map_err(|error| {
                    format!(
                        "cannot run cargo fmt from {}: {error}",
                        formatter.describe()
                    )
                })?;
            Ok(ExitCode::from(
                status.code().map_or(1, |code| code.clamp(0, 255) as u8),
            ))
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
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = --version ]; then echo '{version}'; exit 0; fi\nfor last; do :; done\n{check}\n"
        );
        let rustfmt = root.join("bin/rustfmt");
        fs::write(&rustfmt, script).unwrap();
        fs::set_permissions(&rustfmt, fs::Permissions::from_mode(0o755)).unwrap();
        root
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
        let found = formatter.version().unwrap();
        let error = check_identity(&found, &pin, &formatter).unwrap_err();
        assert!(error.contains("refusing the formatter"), "{error}");
        assert!(error.contains(&pin.identity), "{error}");
        let pinned = fake_toolchain("pinned", &pin.identity, "exit 0");
        let formatter = Formatter::Toolchain(pinned.clone());
        assert_eq!(
            check_identity(&formatter.version().unwrap(), &pin, &formatter),
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
        let strict = fake_toolchain(
            "strict",
            "x",
            "if grep -q 'use std::{' \"$last\"; then exit 1; fi\nexit 0",
        );
        assert_eq!(
            self_test(&Formatter::Toolchain(strict.clone()), &root),
            Ok(())
        );
        let lenient = fake_toolchain("lenient", "x", "exit 0");
        let error = self_test(&Formatter::Toolchain(lenient.clone()), &root).unwrap_err();
        assert!(error.contains("bad.rs fixture exited"), "{error}");
        fs::remove_dir_all(strict).unwrap();
        fs::remove_dir_all(lenient).unwrap();
    }
}
