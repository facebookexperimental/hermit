#!/usr/bin/env rust-script
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
//! Fail fast when a *nested* Cargo workspace's `Cargo.lock` is stale relative to
//! its own manifest, before the opaque `--locked` build failure downstream.
//!
//! `liteinst-runtime-build/` is a SEPARATE nested Cargo workspace
//! (`[workspace] members = ["runtime"]` in its own `Cargo.toml`); it is NOT a
//! member of the root Hermit workspace. Its inner `runtime` crate has a git
//! dependency on Reverie pinned by `rev`. Because it is a distinct workspace, a
//! root-level `cargo update` — or a Reverie-pin bump — refreshes the root
//! `Cargo.lock` but NOT `liteinst-runtime-build/Cargo.lock`. When the pin moves
//! and the nested lock is left behind, the staged build
//! (`scripts/stage-liteinst-runtime.sh` via `hermit-install/build.rs`) runs
//! `cargo build --locked` and fails ~78s in with the cryptic:
//!
//! ```text
//! error: cannot update the lock file liteinst-runtime-build/Cargo.lock
//!        because --locked was passed
//! ```
//!
//! `scripts/check-reverie-pin.rs` scans tracked Cargo metadata for a consistent
//! Reverie `rev` string but does NOT verify that each nested lockfile is fresh
//! versus its manifest. This checker closes that gap: for each known nested
//! workspace it runs `cargo metadata --locked` — the SAME probe that reproduces
//! the bug (rc=0 fresh / non-zero stale) — and, on failure, prints the exact
//! regenerate command and fails BEFORE the slow, opaque build.rs panic.
//!
//! `cargo metadata --locked` resolves the git dependency, so it needs network:
//! run it under the proxy on Meta hosts.
//!
//! Local use on Meta hosts:
//!
//! ```text
//! with-proxy ./scripts/check-nested-lockfiles.rs
//! ```

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude; // rust-script cache-key: 088ae17fa4a1 (regen: scripts/lib/prelude-cache-key.sh --write)

use std::env;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;

/// Nested Cargo workspaces (repo-relative directory holding a `Cargo.toml` with
/// its own `[workspace]` table and a sibling `Cargo.lock`) that the root
/// workspace does NOT include as members. Add one line per future nested
/// workspace.
const NESTED_WORKSPACES: &[&str] = &["liteinst-runtime-build"];

#[derive(Default)]
struct Config {
    repo: Option<PathBuf>,
}

fn usage() -> &'static str {
    "Usage: check-nested-lockfiles.rs [OPTIONS]\n\
     \n\
     Verify that each nested Cargo workspace's Cargo.lock is fresh versus its\n\
     manifest, using `cargo metadata --locked` (needs network for git deps; run\n\
     under with-proxy on Meta hosts).\n\
     \n\
     Options:\n\
       --repo PATH    Hermit checkout (default: git root)\n\
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

/// The actionable one-liner shown when a nested lockfile is stale. `workspace`
/// is the repo-relative nested-workspace directory (e.g. `liteinst-runtime-build`).
fn stale_message(workspace: &str) -> String {
    format!(
        "{workspace}/Cargo.lock is STALE vs its manifest. \
         Regenerate: (cd {workspace} && with-proxy cargo metadata --format-version 1 >/dev/null) \
         then commit {workspace}/Cargo.lock"
    )
}

/// Run `cargo metadata --locked` for one nested workspace. Returns Ok(()) when
/// the lockfile is fresh (rc=0), Err(captured stderr) when stale or otherwise
/// unresolvable (non-zero rc). A spawn failure (no cargo on PATH) is a hard
/// checker error.
fn probe_workspace(root: &Path, workspace: &str) -> Result<Result<(), String>, String> {
    let manifest = root.join(workspace).join("Cargo.toml");
    if !manifest.is_file() {
        return Err(format!(
            "nested workspace manifest not found: {}",
            manifest.display()
        ));
    }
    let output = Command::new("cargo")
        .args(["metadata", "--locked", "--format-version", "1"])
        .arg("--manifest-path")
        .arg(&manifest)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| {
            format!("could not run cargo metadata for {workspace}: {error} (is cargo on PATH?)")
        })?;
    if output.status.success() {
        Ok(Ok(()))
    } else {
        Ok(Err(String::from_utf8_lossy(&output.stderr).trim().to_string()))
    }
}

fn loud_header(title: &str) {
    eprintln!("======================================================================");
    eprintln!("NESTED LOCKFILE LINT: {title}");
    eprintln!("======================================================================");
}

fn run_with_config(config: Config) -> Result<i32, String> {
    let root = config.repo.clone().map_or_else(git_root, Ok)?;
    eprintln!(
        "Scope: checking {} nested Cargo workspace(s) for lockfile freshness ({}).",
        NESTED_WORKSPACES.len(),
        NESTED_WORKSPACES.join(", ")
    );

    let mut stale = false;
    for workspace in NESTED_WORKSPACES {
        match probe_workspace(&root, workspace)? {
            Ok(()) => {
                println!("OK: {workspace}/Cargo.lock is fresh versus its manifest.");
            }
            Err(cargo_stderr) => {
                stale = true;
                loud_header("NESTED Cargo.lock IS STALE - BLOCKED");
                eprintln!("{}", stale_message(workspace));
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
            "A nested workspace is NOT a member of the root Cargo workspace, so a root-level"
        );
        eprintln!(
            "`cargo update` or Reverie-pin bump does NOT refresh its Cargo.lock. Regenerate and"
        );
        eprintln!("commit the nested lockfile (see the per-file command above) before landing.");
        Ok(1)
    } else {
        println!(
            "All {} nested workspace lockfile(s) are fresh.",
            NESTED_WORKSPACES.len()
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
    use super::*;

    #[test]
    fn nested_workspace_list_is_non_empty() {
        assert!(
            !NESTED_WORKSPACES.is_empty(),
            "at least one nested workspace must be checked"
        );
    }

    #[test]
    fn liteinst_runtime_build_is_covered() {
        assert!(
            NESTED_WORKSPACES.contains(&"liteinst-runtime-build"),
            "the known nested workspace must be checked"
        );
    }

    #[test]
    fn stale_message_names_the_file_and_regenerate_command() {
        let message = stale_message("liteinst-runtime-build");
        assert!(message.contains("liteinst-runtime-build/Cargo.lock is STALE"));
        assert!(message.contains("cargo metadata --format-version 1"));
        assert!(message.contains("then commit liteinst-runtime-build/Cargo.lock"));
    }

    #[test]
    fn stale_message_uses_repo_relative_paths_only() {
        // Portability: no absolute/owner-specific paths in the guidance.
        let message = stale_message("liteinst-runtime-build");
        assert!(!message.contains("/home/"));
        assert!(!message.contains('\t'));
    }

    #[test]
    fn missing_manifest_is_a_hard_checker_error() {
        let root = env::temp_dir().join(format!(
            "check-nested-lockfiles-missing-{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&root);
        let result = probe_workspace(&root, "does-not-exist");
        let _ = std::fs::remove_dir_all(&root);
        assert!(
            result.is_err(),
            "a missing nested manifest must be a hard checker error, not a silent pass"
        );
    }

    #[test]
    fn help_states_the_checker_scope() {
        let help = usage();
        assert!(help.contains("nested Cargo workspace"));
        assert!(help.contains("--locked"));
    }
}
