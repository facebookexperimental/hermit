#!/usr/bin/env -S rust-script --force
//! Copyright (c) Meta Platforms, Inc. and affiliates.
//! All rights reserved.
//!
//! This source code is licensed under the BSD-style license found in the
//! LICENSE file in the root directory of this source tree.
//!
//! Print the rust-deps cache key of a Hermit checkout: a SHA-256 over
//! everything pinned Reindeer reads when bootstrap/regenerate-rust-deps runs
//! it, so two checkouts with the same key get the same vendored crates and raw
//! BUCK (bootstrap/rust-deps-cache.sh keeps those under the key).
//!
//! The key covers:
//!
//! - Cargo.lock, which pins every crate (git sources by commit);
//! - the workspace manifests: every Cargo.toml Git lists, tracked or not, and
//!   the manifest of every package `cargo metadata` reports, read from its
//!   decoded JSON, so a member Git ignores is still read;
//! - the toolchain file, the root Cargo.toml and Cargo configuration (.cargo),
//!   the ignore files on the way to the third-party directory, the Reindeer pin
//!   and launcher, the build recipe, this program and its caller;
//! - the Buck package Reindeer names its targets under, and the `.gitignore`
//!   files it applies: it walks up from the third-party directory to the nearest
//!   directory holding `.buckconfig` or `.buckroot`, whether or not that is
//!   inside the checkout, and applies the `.gitignore` of every directory from
//!   there down;
//! - `cargo --version`;
//! - the compiler Reindeer queries, as Reindeer finds it (RUSTC, else rustc
//!   from PATH): its `-vV`, and for each platform target in reindeer.toml the
//!   `target_` lines of `--print=cfg --target T`, which Reindeer copies into the
//!   platform's cfg. The answers are recorded, not the compiler's name, so a
//!   wrapper or a toolchain changed in place is covered;
//! - every file under shim/third-party/rust except vendor, BUCK and .cargo,
//!   which regenerate-rust-deps removes before Reindeer runs and Reindeer
//!   writes, whether or not Git ignores it, read through symbolic links with
//!   each link's target recorded. Nothing is left out for its name alone: a
//!   local package under a directory named `target` is read like any other.
//!
//! reindeer.toml is read with a TOML parser, so keys are compared after
//! decoding (`"fixups_dir"` is `fixups_dir`). A setting that makes
//! Reindeer read something the key does not cover declines the cache:
//! `fixups_dir`, `cargo.cargo`, `cargo.rustc`,
//! `vendor.gitignore_checksum_exclude`, no `[platform]` table (Reindeer would
//! query its built-in platforms), and any key this program does not know,
//! which pinned Reindeer refuses anyway.
//!
//! Anything that cannot be read is an error, never a smaller key: a command
//! that fails, JSON or TOML that does not parse, an unreadable directory or
//! file, a link loop. The caller treats an error as "no cache": it vendors and
//! stores nothing.
//!
//! The key does not cover HERMIT_GIT_DEP_MIRRORS (a mirror changes where the
//! pinned objects come from, never which) or Cargo configuration outside the
//! checkout (Reindeer gives Cargo its own CARGO_HOME, and anything else could
//! change resolution only by rewriting Cargo.lock, which the caller refuses).
//!
//! Usage: rust-deps-cache-key.rs REPO_ROOT. Exit status: 0 with the key on
//! standard output; 3 with the reason the cache cannot serve this checkout on
//! standard output; 1 when the key could not be computed (the error is on
//! standard error); 2 on a usage error.
//!
//! ```cargo
//! [dependencies]
//! serde_json = "1"
//! sha2 = "0.10"
//! toml = "0.8"
//! ```

#[path = "../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeSet;
use std::env;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;
use std::process::ExitStatus;
use std::process::Stdio;

use sha2::Digest;
use sha2::Sha256;

const NAME: &str = "rust-deps-cache-key.rs";
const USAGE: &str = "usage: rust-deps-cache-key.rs REPO_ROOT";
/// Changes whenever what an entry holds, or how the key is formed, changes, so
/// an entry stored under an earlier recipe is never restored.
const RECIPE: &str = "4";
/// Reindeer's third-party directory, which regenerate-rust-deps passes as
/// `--third-party-dir`; reindeer.toml and the default fixups directory are in it.
const THIRD_PARTY_DIR: &str = "shim/third-party/rust";
const REINDEER_TOML: &str = "shim/third-party/rust/reindeer.toml";
/// What regenerate-rust-deps removes from the third-party directory before
/// Reindeer runs, and Reindeer then writes; nothing else there is output.
const GENERATED: [&str; 3] = ["vendor", "BUCK", ".cargo"];
/// The files whose presence makes a directory the root of Reindeer's Buck
/// cell (pinned Reindeer's `buck_package`, src/path.rs).
const BUCK_MARKERS: [&str; 2] = [".buckconfig", ".buckroot"];
/// Inputs at fixed paths, hashed whether or not Git tracks or ignores them.
const FIXED_INPUTS: [&str; 13] = [
    "Cargo.lock",
    "Cargo.toml",
    "rust-toolchain.toml",
    ".gitignore",
    "shim/.gitignore",
    "shim/third-party/.gitignore",
    "bootstrap/regenerate-rust-deps",
    "bootstrap/rust-deps-cache.sh",
    "bootstrap/rust-deps-cache-key.rs",
    "scripts/lib/rust_script_prelude.rs",
    "bootstrap/reindeer",
    "bootstrap/run-pinned-tool",
    "bootstrap/tool-pins.sh",
];
/// Top-level reindeer.toml keys whose effect the key covers: they only shape
/// the generated BUCK from inputs the key already reads. `manifest_path` and
/// `third_party_dir` are overridden by the arguments regenerate-rust-deps
/// passes. `cargo`, `vendor` and `platform` are examined further.
const COVERED_KEYS: [&str; 15] = [
    "manifest_path",
    "third_party_dir",
    "precise_srcs",
    "license_patterns",
    "unresolved_fixup_error",
    "unresolved_fixup_error_message",
    "include_top_level",
    "include_workspace_members",
    "third_party_metadata",
    "cargo_env",
    "cargo",
    "buck",
    "vendor",
    "extern_crates",
    "platform",
];
const DECLINED: u8 = 3;

enum Outcome {
    Key(String),
    Declined(String),
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    let root = match args.as_slice() {
        [arg] if arg == "-h" || arg == "--help" => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        [root] => PathBuf::from(root),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(&root) {
        Ok(Outcome::Key(key)) => {
            println!("{key}");
            ExitCode::SUCCESS
        }
        Ok(Outcome::Declined(reason)) => {
            println!("{reason}");
            ExitCode::from(DECLINED)
        }
        Err(err) => {
            eprintln!("{NAME}: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run(root: &Path) -> Result<Outcome, String> {
    let logical = if root.is_absolute() {
        root.to_path_buf()
    } else {
        env::current_dir()
            .map_err(|err| format!("cannot read the current directory: {err}"))?
            .join(root)
    };
    let physical = fs::canonicalize(root)
        .map_err(|err| format!("cannot resolve {}: {err}", root.display()))?;
    env::set_current_dir(root).map_err(|err| format!("cannot enter {}: {err}", root.display()))?;

    let text = match fs::read_to_string(REINDEER_TOML) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Ok(Outcome::Declined(format!("{REINDEER_TOML} is missing")));
        }
        Err(err) => return Err(format!("cannot read {REINDEER_TOML}: {err}")),
    };
    let config: toml::Table = text
        .parse()
        .map_err(|err| format!("cannot parse {REINDEER_TOML}: {err}"))?;
    if let Some(reason) = decline(&config) {
        return Ok(Outcome::Declined(format!("{REINDEER_TOML} {reason}")));
    }

    let mut manifest = Manifest::default();
    manifest.record(&[b"recipe", RECIPE.as_bytes()]);

    let cargo = output(Command::new("cargo").arg("--version"), "cargo --version")?;
    manifest.record(&[b"cargo --version", &cargo]);

    // Reindeer runs RUSTC (unset: rustc from PATH) here, with this
    // environment, skipping a query that fails; so a failure is recorded too.
    let rustc = env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"));
    match Command::new(&rustc)
        .arg("-vV")
        .stdin(Stdio::null())
        .output()
    {
        Ok(out) => manifest.record(&[
            b"rustc -vV",
            describe(out.status).as_bytes(),
            &out.stdout,
            &out.stderr,
        ]),
        Err(err) => manifest.record(&[b"rustc -vV", b"spawn failed", err.to_string().as_bytes()]),
    }
    for target in platform_targets(&config) {
        let query = Command::new(&rustc)
            .args(["--print=cfg", "--target", &target])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output();
        match query {
            Ok(out) if out.status.success() => match std::str::from_utf8(&out.stdout) {
                Ok(text) => {
                    let lines: Vec<&str> = text
                        .lines()
                        .filter(|line| line.starts_with("target_"))
                        .collect();
                    manifest.record(&[b"cfg", target.as_bytes(), lines.join("\n").as_bytes()]);
                }
                Err(_) => manifest.record(&[b"cfg", target.as_bytes(), b"not UTF-8"]),
            },
            Ok(out) => {
                manifest.record(&[b"cfg", target.as_bytes(), describe(out.status).as_bytes()])
            }
            Err(err) => manifest.record(&[
                b"cfg",
                target.as_bytes(),
                b"spawn failed",
                err.to_string().as_bytes(),
            ]),
        }
    }

    let mut paths: BTreeSet<Vec<u8>> = FIXED_INPUTS.iter().map(|p| p.as_bytes().to_vec()).collect();
    let listed = output(
        Command::new("git").args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
            ":(glob)**/Cargo.toml",
        ]),
        "git ls-files",
    )?;
    paths.extend(
        listed
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(<[u8]>::to_vec),
    );
    let metadata = output(
        Command::new("cargo").args(["metadata", "--locked", "--format-version", "1", "--no-deps"]),
        "cargo metadata",
    )?;
    for path in member_manifests(&metadata)? {
        let path = Path::new(&path);
        let relative = path
            .strip_prefix(&physical)
            .or_else(|_| path.strip_prefix(&logical))
            .unwrap_or(path);
        paths.insert(relative.as_os_str().as_bytes().to_vec());
    }
    record_buck_cell(&mut manifest)?;
    let mut ancestors = Vec::new();
    walk(Path::new(THIRD_PARTY_DIR), &mut ancestors, &mut paths)?;
    if fs::symlink_metadata(".cargo").is_ok() {
        walk(Path::new(".cargo"), &mut ancestors, &mut paths)?;
    }

    let mut lock_hashed = false;
    for bytes in &paths {
        let path = Path::new(OsStr::from_bytes(bytes));
        let kind = manifest.record_path(path)?;
        lock_hashed |= bytes.as_slice() == b"Cargo.lock" && kind == PathKind::File;
    }
    if !lock_hashed {
        return Err("Cargo.lock is not a readable file".to_owned());
    }
    Ok(Outcome::Key(manifest.key()))
}

/// Records the Buck package pinned Reindeer derives and the `.gitignore` files
/// it applies. `buck_package` (src/path.rs) walks up from the canonical
/// third-party directory to the nearest directory holding a marker and names
/// every generated target after the third-party directory's path below it, or
/// after nothing when no directory up to `/` holds one. `load_gitignore`
/// (src/gitignore.rs) applies the `.gitignore` of each directory from that one
/// down to the third-party directory, the third-party directory's alone when
/// there is no marker. Each file is named by its place relative to the
/// third-party directory, so the key does not depend on where the checkout is.
fn record_buck_cell(manifest: &mut Manifest) -> Result<(), String> {
    let third_party = fs::canonicalize(THIRD_PARTY_DIR)
        .map_err(|err| format!("cannot resolve {THIRD_PARTY_DIR}: {err}"))?;
    let mut cell = None;
    for (up, dir) in third_party.ancestors().enumerate() {
        if has_buck_marker(dir)? {
            cell = Some((up, dir));
            break;
        }
    }
    let (levels, package) = match cell {
        Some((up, dir)) => (
            up,
            third_party
                .strip_prefix(dir)
                .expect("an ancestor is a prefix")
                .as_os_str()
                .as_bytes(),
        ),
        None => (0, &b""[..]),
    };
    let marker: &[u8] = if cell.is_some() {
        b"marker"
    } else {
        b"no marker"
    };
    manifest.record(&[b"buck package", marker, package]);
    let dirs: Vec<&Path> = third_party.ancestors().take(levels + 1).collect();
    for (up, dir) in dirs.iter().enumerate().rev() {
        let name = format!("{}.gitignore", "../".repeat(up));
        manifest.record_path_as(name.as_bytes(), &dir.join(".gitignore"))?;
    }
    Ok(())
}

/// Whether `dir` holds a Buck cell marker, as `fs::exists` answers, which is
/// what pinned Reindeer asks; an error is an error here too.
fn has_buck_marker(dir: &Path) -> Result<bool, String> {
    for marker in BUCK_MARKERS {
        let path = dir.join(marker);
        if fs::exists(&path).map_err(|err| format!("cannot read {}: {err}", path.display()))? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Why `config` (a parsed reindeer.toml) makes Reindeer read something the key
/// does not cover, or `None` when the key covers it.
fn decline(config: &toml::Table) -> Option<String> {
    for key in config.keys() {
        if key == "fixups_dir" {
            return Some("sets fixups_dir, a fixups directory the key does not read".to_owned());
        }
        if !COVERED_KEYS.contains(&key.as_str()) {
            return Some(format!("sets `{key}`, which the key does not know"));
        }
    }
    if let Some(cargo) = config.get("cargo") {
        let Some(cargo) = cargo.as_table() else {
            return Some("sets cargo to something other than a table".to_owned());
        };
        for key in cargo.keys() {
            match key.as_str() {
                "cargo" | "rustc" => {
                    return Some(format!(
                        "sets cargo.{key}, a program the key does not query"
                    ));
                }
                "bindeps" => {}
                _ => return Some(format!("sets `cargo.{key}`, which the key does not know")),
            }
        }
    }
    if let Some(vendor) = config.get("vendor").and_then(toml::Value::as_table) {
        if vendor.contains_key("gitignore_checksum_exclude") {
            return Some(
                "sets vendor.gitignore_checksum_exclude, ignore files the key does not read"
                    .to_owned(),
            );
        }
        if let Some(key) = vendor.keys().next() {
            return Some(format!("sets `vendor.{key}`, which the key does not know"));
        }
    }
    let Some(platforms) = config.get("platform") else {
        return Some(
            "defines no platform table, so Reindeer queries its built-in platforms, which the key \
             does not"
                .to_owned(),
        );
    };
    let Some(platforms) = platforms.as_table() else {
        return Some("sets platform to something other than a table".to_owned());
    };
    for (name, platform) in platforms {
        let Some(platform) = platform.as_table() else {
            return Some(format!(
                "sets platform.{name} to something other than a table"
            ));
        };
        if platform
            .get("target")
            .is_some_and(|target| !target.is_str())
        {
            return Some(format!(
                "sets platform.{name}.target to something other than a string"
            ));
        }
    }
    None
}

/// The decoded `target` of every platform; `decline` has checked the shape.
fn platform_targets(config: &toml::Table) -> BTreeSet<String> {
    config
        .get("platform")
        .and_then(toml::Value::as_table)
        .into_iter()
        .flat_map(|platforms| platforms.values())
        .filter_map(|platform| platform.get("target")?.as_str())
        .map(str::to_owned)
        .collect()
}

/// The decoded `manifest_path` of every package in `cargo metadata` output.
fn member_manifests(metadata: &[u8]) -> Result<Vec<String>, String> {
    let metadata: serde_json::Value = serde_json::from_slice(metadata)
        .map_err(|err| format!("cannot parse the output of cargo metadata: {err}"))?;
    let packages = metadata
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .ok_or("cargo metadata reported no packages array")?;
    packages
        .iter()
        .map(|package| {
            package
                .get("manifest_path")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| {
                    "cargo metadata reported a package without a manifest_path".to_owned()
                })
        })
        .collect()
}

/// Adds every path under `path` to `paths`, following symbolic links as
/// Reindeer reads through them and leaving out the generated paths. A
/// directory link is added too, so its target is recorded. An unreadable
/// directory or a link loop is an error rather than a gap in the key.
fn walk(
    path: &Path,
    ancestors: &mut Vec<(u64, u64)>,
    paths: &mut BTreeSet<Vec<u8>>,
) -> Result<(), String> {
    let third_party = Path::new(THIRD_PARTY_DIR);
    if GENERATED
        .iter()
        .any(|generated| path == third_party.join(generated))
    {
        return Ok(());
    }
    let link = fs::symlink_metadata(path)
        .map_err(|err| format!("cannot read {}: {err}", path.display()))?
        .file_type()
        .is_symlink();
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        // A dangling link: recorded as such.
        Err(err) if link && err.kind() == io::ErrorKind::NotFound => {
            paths.insert(path.as_os_str().as_bytes().to_vec());
            return Ok(());
        }
        Err(err) => return Err(format!("cannot read {}: {err}", path.display())),
    };
    if link || !metadata.is_dir() {
        paths.insert(path.as_os_str().as_bytes().to_vec());
    }
    if !metadata.is_dir() {
        return Ok(());
    }
    let id = (metadata.dev(), metadata.ino());
    if ancestors.contains(&id) {
        return Err(format!(
            "{} leads back to a directory above it",
            path.display()
        ));
    }
    let mut names = Vec::new();
    for entry in
        fs::read_dir(path).map_err(|err| format!("cannot list {}: {err}", path.display()))?
    {
        names.push(
            entry
                .map_err(|err| format!("cannot list {}: {err}", path.display()))?
                .file_name(),
        );
    }
    ancestors.push(id);
    for name in names {
        walk(&path.join(name), ancestors, paths)?;
    }
    ancestors.pop();
    Ok(())
}

/// Runs `command` and returns its standard output, or an error naming `what`
/// when it cannot start or fails. Its standard error reaches the caller's.
fn output(command: &mut Command, what: &str) -> Result<Vec<u8>, String> {
    let out = command
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|err| format!("cannot run {what}: {err}"))?;
    if !out.status.success() {
        return Err(format!("{what} failed ({})", describe(out.status)));
    }
    Ok(out.stdout)
}

fn describe(status: ExitStatus) -> String {
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("exit {code}"),
        (None, Some(signal)) => format!("signal {signal}"),
        (None, None) => "unknown status".to_owned(),
    }
}

#[derive(PartialEq)]
enum PathKind {
    File,
    Other,
}

/// The bytes the key is the SHA-256 of. Each field is written as its length,
/// a colon and its bytes, so no two different sequences of records write the
/// same bytes, whatever a path or a command's output contains.
#[derive(Default)]
struct Manifest {
    bytes: Vec<u8>,
}

impl Manifest {
    fn record(&mut self, fields: &[&[u8]]) {
        self.bytes
            .extend_from_slice(format!("{}:", fields.len()).as_bytes());
        for field in fields {
            self.bytes
                .extend_from_slice(format!("{}:", field.len()).as_bytes());
            self.bytes.extend_from_slice(field);
        }
    }

    /// Records what is at `path`: a link's target, then a file's SHA-256, a
    /// dangling link, a directory, something else, or nothing.
    fn record_path(&mut self, path: &Path) -> Result<PathKind, String> {
        self.record_path_as(path.as_os_str().as_bytes(), path)
    }

    /// Records what is at `path`, as `record_path` does, under `name`.
    fn record_path_as(&mut self, name: &[u8], path: &Path) -> Result<PathKind, String> {
        let link = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata.file_type().is_symlink(),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                self.record(&[b"absent", name]);
                return Ok(PathKind::Other);
            }
            Err(err) => return Err(format!("cannot read {}: {err}", path.display())),
        };
        if link {
            let target = fs::read_link(path)
                .map_err(|err| format!("cannot read the link {}: {err}", path.display()))?;
            self.record(&[b"link", name, target.as_os_str().as_bytes()]);
        }
        match fs::metadata(path) {
            Ok(metadata) if metadata.is_file() => {
                let mut file = fs::File::open(path)
                    .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
                let mut hasher = Sha256::new();
                io::copy(&mut file, &mut hasher)
                    .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
                self.record(&[b"file", name, &hasher.finalize()]);
                Ok(PathKind::File)
            }
            Ok(metadata) if metadata.is_dir() => {
                self.record(&[b"directory", name]);
                Ok(PathKind::Other)
            }
            Ok(_) => {
                self.record(&[b"other", name]);
                Ok(PathKind::Other)
            }
            Err(err) if link && err.kind() == io::ErrorKind::NotFound => {
                self.record(&[b"dangling", name]);
                Ok(PathKind::Other)
            }
            Err(err) => Err(format!("cannot read {}: {err}", path.display())),
        }
    }

    fn key(&self) -> String {
        Sha256::digest(&self.bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> toml::Table {
        text.parse().unwrap()
    }

    const PLATFORM: &str = "[platform.linux-x86_64]\ntarget = \"x86_64-unknown-linux-gnu\"\n";

    #[test]
    fn escaped_keys_are_decoded_before_they_are_compared() {
        let reason = decline(&parse(&format!("\"fixups\\u005fdir\" = \"x\"\n{PLATFORM}"))).unwrap();
        assert!(reason.contains("fixups_dir"), "{reason}");
        let config =
            parse("[platform.musl]\n\"targ\\u0065t\" = \"x86_64\\u002dunknown-linux-musl\"\n");
        assert_eq!(decline(&config), None);
        assert_eq!(
            platform_targets(&config).into_iter().collect::<Vec<_>>(),
            ["x86_64-unknown-linux-musl"]
        );
    }

    #[test]
    fn settings_the_key_does_not_cover_decline() {
        for text in [
            "[cargo]\nrustc = \"x\"\n",
            "[cargo]\ncargo = \"x\"\n",
            "[cargo]\nsurprise = 1\n",
            "cargo = 1\n",
            "[vendor]\ngitignore_checksum_exclude = [\"x\"]\n",
            "surprise = 1\n",
        ] {
            assert!(
                decline(&parse(&format!("{text}{PLATFORM}"))).is_some(),
                "{text}"
            );
        }
        assert!(
            decline(&parse("[cargo]\n"))
                .unwrap()
                .contains("no platform table")
        );
        assert!(decline(&parse("[platform.x]\ntarget = 1\n")).is_some());
        let covered = format!(
            "vendor = true\n[cargo]\nbindeps = true\n[buck]\nbuckfile_imports = \"\"\"\nx\n\"\"\"\n{PLATFORM}"
        );
        assert_eq!(decline(&parse(&covered)), None);
    }

    #[test]
    fn member_paths_are_json_decoded() {
        let paths =
            member_manifests(br#"{"packages":[{"manifest_path":"/r/a\\b\"c/Cargo.toml"}]}"#)
                .unwrap();
        assert_eq!(paths, ["/r/a\\b\"c/Cargo.toml"]);
        assert!(member_manifests(br#"{"packages":[{"name":"x"}]}"#).is_err());
        assert!(member_manifests(b"{\"packages\":[").is_err());
    }

    #[test]
    fn records_are_unambiguous() {
        let mut one = Manifest::default();
        one.record(&[b"file", b"a\nb"]);
        let mut two = Manifest::default();
        two.record(&[b"file", b"a"]);
        two.record(&[b"b"]);
        assert_ne!(one.bytes, two.bytes);
    }
}
