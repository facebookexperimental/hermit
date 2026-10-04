#!/usr/bin/env -S rust-script --force
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Give reverie-dbt's build script a declared, materialized manifest directory.

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;

const EXPECTED_FILES: usize = 931;
const CRATE_RELATIVE: &str = "reverie-dbt";
const VENDORED_CRATE: &str = "shim/third-party/rust/vendor/reverie-dbt-0.4.0";
const TARGET_NAME: &str = "reverie-dbt-0.4-materialized-manifest";
const PRELUDE_LOAD: &str = "load(\"@prelude//rust:cargo_buildscript.bzl\", \"buildscript_run\")\n";
const MATERIALIZED_LOAD: &str =
    "load(\"@shim//build_defs:materialized_manifest.bzl\", \"materialized_manifest\")\n";
const BUILDSCRIPT_START: &str =
    "buildscript_run(\n    name = \"reverie-dbt-0.4-build-script-run\",\n";
const MANIFEST_ARGUMENT: &str = "    manifest_dir = \":reverie-dbt-0.4-materialized-manifest\",\n";

fn repository_root() -> Result<PathBuf, String> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|error| format!("failed to locate repository root: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git rev-parse failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout)
        .map(|root| PathBuf::from(root.trim()))
        .map_err(|error| format!("repository root is not UTF-8: {error}"))
}

fn tracked_reverie_files(root: &Path) -> Result<Vec<String>, String> {
    let output = Command::new("git")
        .current_dir(root.join("reverie"))
        .args(["ls-files", "-z", "--", CRATE_RELATIVE])
        .output()
        .map_err(|error| format!("failed to inventory pinned reverie-dbt: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git ls-files for pinned reverie-dbt failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let prefix = format!("{CRATE_RELATIVE}/");
    let mut files = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| {
            let path = std::str::from_utf8(path)
                .map_err(|error| format!("pinned reverie-dbt path is not UTF-8: {error}"))?;
            path.strip_prefix(&prefix)
                .map(str::to_owned)
                .ok_or_else(|| format!("tracked path escaped reverie-dbt: {path:?}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    files.sort();
    if files.len() != EXPECTED_FILES {
        return Err(format!(
            "pinned reverie-dbt inventory changed: expected {EXPECTED_FILES} files, found {}",
            files.len()
        ));
    }
    Ok(files)
}

fn walk_regular_files(
    root: &Path,
    directory: &Path,
    files: &mut Vec<String>,
) -> Result<(), String> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| format!("failed to read {}: {error}", directory.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("failed to inspect {}: {error}", directory.display()))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("failed to inspect {}: {error}", path.display()))?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "vendored reverie-dbt contains symlink {}",
                path.display()
            ));
        }
        if metadata.is_dir() {
            walk_regular_files(root, &path, files)?;
        } else if metadata.is_file() {
            let relative = path
                .strip_prefix(root)
                .map_err(|error| format!("vendored path escaped crate root: {error}"))?
                .to_str()
                .ok_or_else(|| format!("vendored path is not UTF-8: {}", path.display()))?;
            if relative != ".cargo-checksum.json" {
                files.push(relative.to_owned());
            }
        } else {
            return Err(format!("unsupported vendored entry {}", path.display()));
        }
    }
    Ok(())
}

fn verified_files(root: &Path) -> Result<Vec<String>, String> {
    let tracked = tracked_reverie_files(root)?;
    let vendor_root = root.join(VENDORED_CRATE);
    let mut vendored = Vec::new();
    walk_regular_files(&vendor_root, &vendor_root, &mut vendored)?;
    vendored.sort();
    if tracked != vendored {
        let tracked_set = tracked.iter().collect::<BTreeSet<_>>();
        let vendored_set = vendored.iter().collect::<BTreeSet<_>>();
        let missing = tracked_set
            .difference(&vendored_set)
            .take(5)
            .collect::<Vec<_>>();
        let extra = vendored_set
            .difference(&tracked_set)
            .take(5)
            .collect::<Vec<_>>();
        return Err(format!(
            "vendored reverie-dbt inventory differs from the pinned checkout; missing={missing:?} extra={extra:?}"
        ));
    }
    for relative in &tracked {
        // Cargo's vendoring contract rewrites Cargo.toml into its normalized
        // publish form. Reindeer has already verified that packaged file
        // against Cargo.lock and .cargo-checksum.json; every other one of the
        // 931 pinned source files must remain byte-for-byte identical.
        if relative == "Cargo.toml" {
            continue;
        }
        let pinned = root.join("reverie").join(CRATE_RELATIVE).join(relative);
        let vendored = vendor_root.join(relative);
        let pinned_bytes = fs::read(&pinned)
            .map_err(|error| format!("failed to read {}: {error}", pinned.display()))?;
        let vendored_bytes = fs::read(&vendored)
            .map_err(|error| format!("failed to read {}: {error}", vendored.display()))?;
        if pinned_bytes != vendored_bytes {
            return Err(format!(
                "vendored reverie-dbt file differs from pinned checkout: {relative}"
            ));
        }
    }
    Ok(tracked)
}

fn starlark_string(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            other if other.is_control() => {
                quoted.push_str(&format!("\\u{{{:x}}}", other as u32));
            }
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

fn render_target(files: &[String]) -> String {
    let mut target = format!(
        "# reverie-dbt's CMake install preserves input symlinks. Materialize the\n\
         # complete pinned crate so installed headers remain self-contained.\n\
         materialized_manifest(\n    name = {name},\n    srcs = {{\n",
        name = starlark_string(TARGET_NAME),
    );
    for relative in files {
        target.push_str("        ");
        target.push_str(&starlark_string(relative));
        target.push_str(": ");
        target.push_str(&starlark_string(&format!(
            "vendor/reverie-dbt-0.4.0/{relative}"
        )));
        target.push_str(",\n");
    }
    target.push_str("    },\n    visibility = [],\n)\n");
    target
}

fn occurrence_count(haystack: &str, needle: &str) -> usize {
    haystack.match_indices(needle).count()
}

fn patch_buck(input: &str, files: &[String]) -> Result<String, String> {
    if occurrence_count(input, PRELUDE_LOAD) != 1 {
        return Err("generated BUCK must contain exactly one cargo_buildscript load".to_owned());
    }
    if occurrence_count(input, BUILDSCRIPT_START) != 1 {
        return Err(
            "generated BUCK must contain exactly one reverie-dbt buildscript_run".to_owned(),
        );
    }
    let target = render_target(files);
    let custom_counts = [
        occurrence_count(input, MATERIALIZED_LOAD),
        occurrence_count(input, &format!("    name = \"{TARGET_NAME}\",\n")),
        occurrence_count(input, MANIFEST_ARGUMENT),
    ];
    if custom_counts.iter().any(|count| *count != 0) {
        if custom_counts != [1, 1, 1] || occurrence_count(input, &target) != 1 {
            return Err(format!(
                "generated BUCK contains a partial or duplicate reverie-dbt materialization patch: {custom_counts:?}"
            ));
        }
        let call_start = input
            .find(BUILDSCRIPT_START)
            .expect("buildscript occurrence was checked");
        let call_end = input[call_start..]
            .find("\n)\n")
            .map(|offset| call_start + offset)
            .ok_or_else(|| "reverie-dbt buildscript_run has no closing delimiter".to_owned())?;
        if !input[call_start..call_end].contains(MANIFEST_ARGUMENT.trim_end()) {
            return Err("manifest_dir argument is outside reverie-dbt buildscript_run".to_owned());
        }
        return Ok(input.to_owned());
    }

    let mut patched = input.replacen(
        PRELUDE_LOAD,
        &format!("{PRELUDE_LOAD}{MATERIALIZED_LOAD}"),
        1,
    );
    patched = patched.replacen(
        BUILDSCRIPT_START,
        &format!("{target}\n{BUILDSCRIPT_START}"),
        1,
    );
    let call_start = patched
        .find(BUILDSCRIPT_START)
        .expect("buildscript occurrence was checked");
    let call_end = patched[call_start..]
        .find("\n)\n")
        .map(|offset| call_start + offset)
        .ok_or_else(|| "reverie-dbt buildscript_run has no closing delimiter".to_owned())?;
    patched.insert_str(call_end + 1, MANIFEST_ARGUMENT);
    if occurrence_count(&patched, MATERIALIZED_LOAD) != 1
        || occurrence_count(&patched, &target) != 1
        || occurrence_count(&patched, MANIFEST_ARGUMENT) != 1
    {
        return Err("reverie-dbt materialization patch failed its postcondition".to_owned());
    }
    Ok(patched)
}

fn atomic_write(path: &Path, contents: &str) -> Result<(), String> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("output path is not UTF-8: {}", path.display()))?;
    let temporary = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    fs::write(&temporary, contents)
        .map_err(|error| format!("failed to write {}: {error}", temporary.display()))?;
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(format!("failed to replace {}: {error}", path.display()));
    }
    Ok(())
}

fn run() -> Result<(), String> {
    let mut arguments = env::args_os().skip(1);
    let buck = arguments
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| "usage: patch-reverie-dbt-buck.rs <generated-BUCK>".to_owned())?;
    if arguments.next().is_some() {
        return Err("usage: patch-reverie-dbt-buck.rs <generated-BUCK>".to_owned());
    }
    let root = repository_root()?;
    let files = verified_files(&root)?;
    let input = fs::read_to_string(&buck)
        .map_err(|error| format!("failed to read {}: {error}", buck.display()))?;
    let patched = patch_buck(&input, &files)?;
    if patched != input {
        atomic_write(&buck, &patched)?;
    }
    Ok(())
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("patch-reverie-dbt-buck.rs: {error}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> String {
        format!("# generated\n{PRELUDE_LOAD}\n{BUILDSCRIPT_START}    version = \"0.4.0\",\n)\n")
    }

    fn files() -> Vec<String> {
        vec!["Cargo.toml".to_owned(), "src/lib.rs".to_owned()]
    }

    #[test]
    fn patch_is_idempotent_and_complete() {
        let once = patch_buck(&fixture(), &files()).unwrap();
        let twice = patch_buck(&once, &files()).unwrap();
        assert_eq!(once, twice);
        assert_eq!(occurrence_count(&once, MATERIALIZED_LOAD), 1);
        assert_eq!(occurrence_count(&once, MANIFEST_ARGUMENT), 1);
        assert_eq!(occurrence_count(&once, &render_target(&files())), 1);
    }

    #[test]
    fn partial_patch_is_refused() {
        let partial = fixture().replacen(
            PRELUDE_LOAD,
            &format!("{PRELUDE_LOAD}{MATERIALIZED_LOAD}"),
            1,
        );
        assert!(patch_buck(&partial, &files()).is_err());
    }

    #[test]
    fn duplicate_buildscript_is_refused() {
        let duplicated = format!("{}{}", fixture(), fixture());
        assert!(patch_buck(&duplicated, &files()).is_err());
    }
}
