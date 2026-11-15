#!/usr/bin/env -S rust-script --force
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
//! Tier 1 of the two-tier `Cargo.lock` gate (Reverie pin-flow design,
//! Finding 4): prove every OSS Cargo root's committed lockfile is fresh
//! versus its manifests, in seconds, before any build starts.
//!
//! The three OSS roots are the Hermit workspace root (`Cargo.toml`), the
//! Reverie workspace root (`reverie/Cargo.toml`, the pinned submodule),
//! and the nested `liteinst-runtime-build/` workspace. For each root this
//! runs the FULL `cargo metadata --locked` probe. The flag choice is
//! load-bearing: measured at the design head and re-verified on
//! facebookexperimental/hermit `262fa517` (cargo 1.97.1), a planted stale
//! lock makes `cargo metadata --locked` exit 101 ("cannot update the lock
//! file … because --locked was passed") while
//! `cargo metadata --locked --no-deps` exits 0 on the same stale lock.
//! This checker therefore never passes `--no-deps`; a unit test pins the
//! argument vector so the flag cannot creep back in.
//!
//! Why this exists beside `scripts/check-nested-lockfiles.rs`: that
//! checker covers only the nested `liteinst-runtime-build` root, as a
//! fast-fail for the staged LiteInst build. Nothing checked the Hermit or
//! Reverie root locks, which is how a stale Reverie `Cargo.lock` reached
//! export on 2026-11-14/15 undetected. This checker covers all three
//! roots; the nested checker keeps its build-staging niche.
//!
//! `cargo metadata --locked` resolves git dependencies, so it needs
//! network on a cold cache: run it under the proxy on Meta hosts
//! (`with-proxy ./scripts/check-cargo-lock-freshness.rs`), or pass
//! `--offline` where a warm cache is guaranteed (for example a prepared
//! export-shaped tree). The `hermit_import` convert/rehearsal job
//! (Tier 1b) runs this same script with `--repo <prepared tree>` as a
//! blocking signal on import diffs; no second implementation exists.

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::env;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;

/// One OSS Cargo root: repo-relative directory holding the workspace
/// `Cargo.toml` whose sibling `Cargo.lock` must be fresh.
struct CargoRoot {
    /// Directory relative to the Hermit checkout root; `"."` is the root.
    dir: &'static str,
    /// Name used in OK/STALE lines and the regenerate command.
    label: &'static str,
}

/// Every OSS Cargo root whose lockfile is exported. Adding a root here is
/// the only edit a future root needs; the unit tests pin this list so a
/// root cannot be dropped silently.
const CARGO_ROOTS: &[CargoRoot] = &[
    CargoRoot {
        dir: ".",
        label: "hermit root",
    },
    CargoRoot {
        dir: "reverie",
        label: "reverie root",
    },
    CargoRoot {
        dir: "liteinst-runtime-build",
        label: "liteinst-runtime-build",
    },
];

#[derive(Default)]
struct Config {
    repo: Option<PathBuf>,
    offline: bool,
}

fn usage() -> &'static str {
    "Usage: check-cargo-lock-freshness.rs [OPTIONS]\n\
     \n\
     Verify that every OSS Cargo root's Cargo.lock is fresh versus its\n\
     manifests, using the full `cargo metadata --locked` probe (never\n\
     `--no-deps`, which passes a stale lock). Needs network for git deps\n\
     on a cold cache; run under with-proxy on Meta hosts.\n\
     \n\
     Options:\n\
       --repo PATH    Hermit checkout (default: git root)\n\
       --offline      Pass --offline to cargo metadata (warm cache only)\n\
       -h, --help     Show this help"
}

fn take_value(args: &[String], i: &mut usize, flag: &str) -> Result<String, String> {
    *i += 1;
    args.get(*i)
        .cloned()
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn parse_args() -> Result<Config, String> {
    let args: Vec<String> = env::args().collect();
    let mut config = Config::default();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--repo" => config.repo = Some(PathBuf::from(take_value(&args, &mut i, "--repo")?)),
            "--offline" => config.offline = true,
            "-h" | "--help" => {
                println!("{}", usage());
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other:?}\n{}", usage())),
        }
        i += 1;
    }
    Ok(config)
}

fn git_root() -> Result<PathBuf, String> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|error| format!("could not run git rev-parse: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git rev-parse failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim(),
    ))
}

/// The `cargo metadata` argument vector, minus the manifest path. Full
/// resolution is the gate: `--locked` refuses to update a stale lock,
/// and `--no-deps` must never appear here because it skips the
/// dependency resolution that notices staleness (it exits 0 on a stale
/// lock). Pinned by `metadata_args_are_a_full_locked_probe` below.
fn metadata_args(offline: bool) -> Vec<&'static str> {
    let mut args = vec!["metadata", "--locked", "--format-version", "1"];
    if offline {
        args.push("--offline");
    }
    args
}

/// Repo-relative lockfile path for one root, for messages.
fn lockfile_display(root: &CargoRoot) -> String {
    if root.dir == "." {
        "Cargo.lock".to_string()
    } else {
        format!("{}/Cargo.lock", root.dir)
    }
}

/// The actionable one-liner shown when a root's lockfile is stale.
fn stale_message(root: &CargoRoot) -> String {
    let lock = lockfile_display(root);
    if root.dir == "." {
        format!(
            "{lock} ({}) is STALE vs its manifests. Regenerate: \
             with-proxy cargo metadata --format-version 1 >/dev/null \
             then commit {lock}",
            root.label
        )
    } else {
        format!(
            "{lock} ({}) is STALE vs its manifests. Regenerate: \
             (cd {} && with-proxy cargo metadata --format-version 1 >/dev/null) \
             then commit {lock}",
            root.label, root.dir
        )
    }
}

/// Run the full locked probe for one root. `Ok(Ok(()))` is fresh,
/// `Ok(Err(stderr))` is stale or otherwise unresolvable, and `Err(_)`
/// is a checker error (missing manifest, no cargo): the caller refuses
/// either way, never passes. A missing manifest is an error, not a
/// skip, so a renamed root cannot silently leave the gate's scope.
fn probe_root(
    checkout: &Path,
    root: &CargoRoot,
    offline: bool,
) -> Result<Result<(), String>, String> {
    let manifest = checkout.join(root.dir).join("Cargo.toml");
    if !manifest.is_file() {
        return Err(format!(
            "Cargo root manifest not found for {}: {}",
            root.label,
            manifest.display()
        ));
    }
    let output = Command::new("cargo")
        .args(metadata_args(offline))
        .arg("--manifest-path")
        .arg(&manifest)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| {
            format!(
                "could not run cargo metadata for {}: {error} (is cargo on PATH?)",
                root.label
            )
        })?;
    if output.status.success() {
        Ok(Ok(()))
    } else {
        Ok(Err(String::from_utf8_lossy(&output.stderr)
            .trim()
            .to_string()))
    }
}

fn loud_header(title: &str) {
    eprintln!("======================================================================");
    eprintln!("CARGO LOCK GATE: {title}");
    eprintln!("======================================================================");
}

fn run_with_config(config: Config) -> Result<i32, String> {
    let checkout = config.repo.clone().map_or_else(git_root, Ok)?;
    let labels: Vec<&str> = CARGO_ROOTS.iter().map(|root| root.label).collect();
    eprintln!(
        "Scope: checking {} OSS Cargo root(s) for lockfile freshness with the full \
         `cargo metadata --locked` probe ({}).",
        CARGO_ROOTS.len(),
        labels.join(", ")
    );

    let mut stale = false;
    for root in CARGO_ROOTS {
        match probe_root(&checkout, root, config.offline)? {
            Ok(()) => {
                println!(
                    "OK: {} ({}) is fresh versus its manifests.",
                    lockfile_display(root),
                    root.label
                );
            }
            Err(cargo_stderr) => {
                stale = true;
                loud_header("Cargo.lock IS STALE - BLOCKED");
                eprintln!("{}", stale_message(root));
                if !cargo_stderr.is_empty() {
                    eprintln!("cargo metadata --locked reported:");
                    for line in cargo_stderr.lines() {
                        eprintln!("  {line}");
                    }
                }
            }
        }
    }

    if stale {
        eprintln!();
        eprintln!(
            "A stale lockfile reaches the exported tree red: every later \
             `--locked` build refuses it, but only after minutes of setup. \
             Regenerate and commit the lockfile(s) named above before landing."
        );
        Ok(1)
    } else {
        println!(
            "All {} OSS Cargo root lockfile(s) are fresh.",
            CARGO_ROOTS.len()
        );
        Ok(0)
    }
}

fn run() -> Result<i32, String> {
    run_with_config(parse_args()?)
}

fn main() {
    rust_script_prelude::init();
    match run() {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            loud_header("CHECKER ERROR - BLOCKED");
            eprintln!("{error}");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use super::*;

    #[test]
    fn metadata_args_are_a_full_locked_probe() {
        for offline in [false, true] {
            let args = metadata_args(offline);
            assert!(
                args.contains(&"--locked"),
                "probe must refuse lock updates: {args:?}"
            );
            assert!(
                !args.contains(&"--no-deps"),
                "--no-deps passes a stale lock and must never be used: {args:?}"
            );
            assert_eq!(
                args.contains(&"--offline"),
                offline,
                "--offline must be exactly the requested mode: {args:?}"
            );
        }
    }

    #[test]
    fn gate_covers_every_oss_cargo_root() {
        let dirs: Vec<&str> = CARGO_ROOTS.iter().map(|root| root.dir).collect();
        assert_eq!(
            dirs,
            [".", "reverie", "liteinst-runtime-build"],
            "a root dropped here leaves the exported tree unchecked"
        );
    }

    #[test]
    fn stale_message_names_lockfile_and_regenerate_command() {
        let root = CargoRoot {
            dir: "reverie",
            label: "reverie root",
        };
        let message = stale_message(&root);
        assert!(message.contains("reverie/Cargo.lock"), "{message}");
        assert!(message.contains("cargo metadata"), "{message}");
        assert_eq!(lockfile_display(&CARGO_ROOTS[0]), "Cargo.lock");
    }

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    /// The probe against real cargo, offline with path dependencies only
    /// (no registry or network): a lock generated before a path
    /// dependency was added is stale and must be refused, and the same
    /// lock after regeneration must pass. This is the gate exercised
    /// end to end at fixture scale; the planted-stale-lock red twin on
    /// the real export trees is run by hand and recorded with the change.
    #[test]
    fn probe_refuses_planted_stale_lock_and_accepts_regenerated() {
        let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let base = env::temp_dir().join(format!(
            "cargo-lock-freshness-{}-{sequence}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        let root_dir = base.join("ws");
        fs::create_dir_all(root_dir.join("app/src")).expect("fixture app");
        fs::create_dir_all(root_dir.join("lib/src")).expect("fixture lib");
        fs::write(
            root_dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"app\"]\nresolver = \"2\"\n",
        )
        .expect("workspace manifest");
        fs::write(
            root_dir.join("app/Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("app manifest");
        fs::write(root_dir.join("app/src/main.rs"), "fn main() {}\n").expect("app main");
        fs::write(
            root_dir.join("lib/Cargo.toml"),
            "[package]\nname = \"fixture-lib\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("lib manifest");
        fs::write(root_dir.join("lib/src/lib.rs"), "").expect("lib source");
        let generated = Command::new("cargo")
            .args(["generate-lockfile", "--offline", "--manifest-path"])
            .arg(root_dir.join("Cargo.toml"))
            .output()
            .expect("generate fixture lockfile (cargo on PATH)");
        assert!(
            generated.status.success(),
            "fixture lock generation failed: {}",
            String::from_utf8_lossy(&generated.stderr)
        );

        // Plant staleness: add a dependency the lock does not record.
        fs::write(
            root_dir.join("app/Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
             [dependencies]\nfixture-lib = { path = \"../lib\" }\n",
        )
        .expect("stale app manifest");
        let fixture_root = CargoRoot {
            dir: "ws",
            label: "fixture",
        };
        let stale = probe_root(&base, &fixture_root, true).expect("probe runs");
        assert!(stale.is_err(), "planted stale lock must be refused");

        // Regenerate: the same probe must now pass.
        let regenerated = Command::new("cargo")
            .args(["metadata", "--offline", "--format-version", "1", "--manifest-path"])
            .arg(root_dir.join("Cargo.toml"))
            .output()
            .expect("regenerate fixture lockfile");
        assert!(regenerated.status.success(), "fixture regen failed");
        let fresh = probe_root(&base, &fixture_root, true).expect("probe runs");
        assert!(fresh.is_ok(), "regenerated lock must pass: {fresh:?}");

        // A missing root is a checker error, never a silent skip.
        let missing = CargoRoot {
            dir: "no-such-root",
            label: "missing",
        };
        assert!(probe_root(&base, &missing, true).is_err());
        let _ = fs::remove_dir_all(&base);
    }
}
