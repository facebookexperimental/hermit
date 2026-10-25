#!/usr/bin/env -S rust-script --force
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
//! Give reverie-dbt's build script a declared, materialized manifest directory.

#[path = "lib/reverie_dbt_inventory.rs"]
mod reverie_dbt_inventory;
#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeSet;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;

use reverie_dbt_inventory::REVERIE_DBT_FILES;

const CRATE_RELATIVE: &str = "reverie-dbt";
const VENDORED_CRATE: &str = "shim/third-party/rust/vendor/reverie-dbt-0.5.3";
const TARGET_NAME: &str = "reverie-dbt-0.5-materialized-manifest";
const PRELUDE_LOAD: &str = "load(\"@prelude//rust:cargo_buildscript.bzl\", \"buildscript_run\")\n";
const MATERIALIZED_LOAD: &str =
    "load(\"@shim//build_defs:materialized_manifest.bzl\", \"materialized_manifest\")\n";
const BUILDSCRIPT_START: &str =
    "buildscript_run(\n    name = \"reverie-dbt-0.5-build-script-run\",\n";
const MANIFEST_ARGUMENT: &str = "    manifest_dir = \":reverie-dbt-0.5-materialized-manifest\",\n";
/// A file only a Hermit checkout's root has; check-buck-reindeer-features.rs
/// reads it first too.
const CHECKOUT_MARKER: &str = "shim/third-party/rust/reindeer.toml";
/// The remedy for a run whose Git repository is not a Hermit checkout's.
const CHECKOUT_REMEDY: &str = "run this inside a Hermit checkout, outside its submodules";
/// The remedy for a run that finds no pinned reverie-dbt to inventory.
const SUBMODULE_REMEDY: &str = "run this inside a Hermit checkout whose reverie submodule is \
     checked out (`git submodule update --init reverie`)";
/// The remedy for a Reverie pin that adds or removes a reverie-dbt file.
const INVENTORY_REMEDY: &str = "check which reverie-dbt files the Reverie pin added or removed, \
     set REVERIE_DBT_FILES in scripts/lib/reverie_dbt_inventory.rs to the new count, and rerun \
     bootstrap/regenerate-rust-deps";
/// The remedy for a BUCK file that is missing or is not the generated one. A
/// checkout that has not been regenerated has no generated BUCK yet, so it
/// says what generates one.
const BUCK_REMEDY: &str = "pass the BUCK file Reindeer generated; bootstrap/regenerate-rust-deps \
     generates shim/third-party/rust/BUCK and runs this script on it";
/// The remedy for a vendored reverie-dbt that is missing or cannot be walked.
const VENDOR_REMEDY: &str =
    "run bootstrap/regenerate-rust-deps, which vendors reverie-dbt and then runs this script";

/// The root of the Hermit checkout containing `cwd`. Any other Git repository
/// is refused before anything is listed, because its `reverie` is not this
/// checkout's pin: from the reverie submodule it is Reverie's own crate
/// directory, and from the dev-hermit parent it is the parent's Reverie.
fn checkout_root(cwd: &Path) -> Result<PathBuf, String> {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|error| format!("failed to locate repository root: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git rev-parse failed with {}: {}; {CHECKOUT_REMEDY}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let root = String::from_utf8(output.stdout)
        .map(|root| PathBuf::from(root.trim()))
        .map_err(|error| format!("repository root is not UTF-8: {error}"))?;
    if !root.join(CHECKOUT_MARKER).is_file() {
        return Err(format!(
            "{} is not a Hermit checkout: it has no {CHECKOUT_MARKER}; {CHECKOUT_REMEDY}",
            root.display()
        ));
    }
    Ok(root)
}

fn tracked_reverie_files(root: &Path) -> Result<Vec<String>, String> {
    let directory = root.join("reverie");
    let output = Command::new("git")
        .current_dir(&directory)
        .args(["ls-files", "-z", "--", CRATE_RELATIVE])
        .output()
        .map_err(|error| {
            format!(
                "failed to inventory pinned reverie-dbt in {}: {error}; {SUBMODULE_REMEDY}",
                directory.display()
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "git ls-files for pinned reverie-dbt failed with {}: {}; {SUBMODULE_REMEDY}",
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
    // No file at all means no checked-out pin to count, not a pin with none.
    if files.is_empty() {
        return Err(format!(
            "git ls-files lists no file under reverie/{CRATE_RELATIVE}; {SUBMODULE_REMEDY}"
        ));
    }
    if files.len() != REVERIE_DBT_FILES {
        return Err(format!(
            "pinned reverie-dbt inventory changed: expected {REVERIE_DBT_FILES} files, found {}; \
             {INVENTORY_REMEDY}",
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
    walk_regular_files(&vendor_root, &vendor_root, &mut vendored)
        .map_err(|error| format!("{error}; {VENDOR_REMEDY}"))?;
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
        // against Cargo.lock and .cargo-checksum.json; every other pinned
        // source file must remain byte-for-byte identical.
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
            "vendor/reverie-dbt-0.5.3/{relative}"
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
        return Err(format!(
            "generated BUCK must contain exactly one cargo_buildscript load; {BUCK_REMEDY}"
        ));
    }
    if occurrence_count(input, BUILDSCRIPT_START) != 1 {
        return Err(format!(
            "generated BUCK must contain exactly one reverie-dbt buildscript_run; {BUCK_REMEDY}"
        ));
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
            .ok_or_else(|| {
                format!("reverie-dbt buildscript_run has no closing delimiter; {BUCK_REMEDY}")
            })?;
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
        .ok_or_else(|| {
            format!("reverie-dbt buildscript_run has no closing delimiter; {BUCK_REMEDY}")
        })?;
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

const USAGE: &str =
    "usage: patch-reverie-dbt-buck.rs <generated-BUCK> | --check-inventory | -h | --help";
const HELP: &str = "Give reverie-dbt's build script, in the Buck graph Reindeer generated, a \
     declared manifest directory that materializes the vendored reverie-dbt crate, after \
     checking that it matches the pinned Reverie checkout file for file; \
     bootstrap/regenerate-rust-deps runs it on shim/third-party/rust/BUCK. With \
     --check-inventory it only checks that the pinned reverie-dbt has the number of files \
     scripts/lib/reverie_dbt_inventory.rs records, which make lint-checks runs. Run it inside a \
     Hermit checkout whose reverie submodule is checked out. Exit status: 0 on success, 2 on \
     any error.";

/// The run with these arguments in the checkout containing `cwd`: the lines
/// it prints on standard output, or the error it fails with.
fn run(arguments: &[OsString], cwd: &Path) -> Result<Vec<String>, String> {
    let [argument] = arguments else {
        return Err(format!(
            "takes exactly one argument; run it with --help for what it does\n{USAGE}"
        ));
    };
    if argument == "--help" || argument == "-h" {
        return Ok(vec![USAGE.to_owned(), HELP.to_owned()]);
    }
    let text = argument.to_string_lossy();
    if text.starts_with('-') && text != "--check-inventory" {
        return Err(format!(
            "unknown option {text}; run it with --help for what it does\n{USAGE}"
        ));
    }
    let root = checkout_root(cwd)?;
    // A Reverie pin that adds or removes a reverie-dbt file must update
    // REVERIE_DBT_FILES in scripts/lib/reverie_dbt_inventory.rs, which
    // build-buck-release.rs checks too. Without this mode the stale count
    // surfaces only when someone regenerates the Buck graph; make lint-checks
    // runs it instead.
    if argument == "--check-inventory" {
        let files = tracked_reverie_files(&root)?;
        return Ok(vec![format!(
            "patch-reverie-dbt-buck.rs: pinned reverie-dbt has the expected {} files",
            files.len()
        )]);
    }
    let buck = cwd.join(argument);
    // Read before the vendored crate is checked, so a wrong path gets its
    // remedy in a checkout that has not been regenerated yet.
    let input = fs::read_to_string(&buck)
        .map_err(|error| format!("failed to read {}: {error}; {BUCK_REMEDY}", buck.display()))?;
    let files = verified_files(&root)?;
    let patched = patch_buck(&input, &files)?;
    if patched != input {
        atomic_write(&buck, &patched)?;
    }
    Ok(vec![])
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    let arguments = env::args_os().skip(1).collect::<Vec<_>>();
    match run(&arguments, Path::new(".")) {
        Ok(lines) => {
            for line in lines {
                println!("{line}");
            }
            ExitCode::SUCCESS
        }
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
        // With the load once, the buildscript_run count is what refuses it.
        let duplicated = format!(
            "{}{BUILDSCRIPT_START}    version = \"0.4.0\",\n)\n",
            fixture()
        );
        assert_eq!(
            patch_buck(&duplicated, &files()),
            Err(format!(
                "generated BUCK must contain exactly one reverie-dbt buildscript_run; \
                 {BUCK_REMEDY}"
            ))
        );
    }

    // A buildscript_run cut off before its closing parenthesis is refused,
    // whether or not the BUCK is already patched.
    #[test]
    fn an_unclosed_buildscript_run_is_refused() {
        let unclosed = Err(format!(
            "reverie-dbt buildscript_run has no closing delimiter; {BUCK_REMEDY}"
        ));
        let fresh = fixture();
        let fresh = fresh.strip_suffix(")\n").unwrap();
        assert_eq!(patch_buck(fresh, &files()), unclosed);
        let patched = patch_buck(&fixture(), &files()).unwrap();
        let patched = patched.strip_suffix(")\n").unwrap();
        assert_eq!(patch_buck(patched, &files()), unclosed);
    }

    fn git(cwd: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
    }

    /// A Git repository in a fresh temporary directory standing for a Hermit
    /// checkout, with its CHECKOUT_MARKER, whose `reverie` is a Git repository
    /// tracking `files` files under reverie-dbt, or an empty directory, as an
    /// uninitialized submodule leaves it, when `files` is `None`.
    fn checkout(name: &str, files: Option<usize>) -> PathBuf {
        let root = env::temp_dir().join(format!(
            "patch-reverie-dbt-buck-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let reverie = root.join("reverie");
        fs::create_dir_all(reverie.join(CRATE_RELATIVE)).unwrap();
        let marker = root.join(CHECKOUT_MARKER);
        fs::create_dir_all(marker.parent().unwrap()).unwrap();
        fs::write(marker, "").unwrap();
        git(&root, &["init", "-q"]);
        if let Some(files) = files {
            for index in 0..files {
                fs::write(reverie.join(format!("{CRATE_RELATIVE}/{index}.rs")), "").unwrap();
            }
            git(&reverie, &["init", "-q"]);
            git(&reverie, &["add", "--", CRATE_RELATIVE]);
        }
        root
    }

    fn check_inventory(cwd: &Path) -> Result<Vec<String>, String> {
        run(&[OsString::from("--check-inventory")], cwd)
    }

    #[test]
    fn check_inventory_passes_only_the_recorded_count() {
        let root = checkout("pinned", Some(REVERIE_DBT_FILES));
        assert_eq!(
            check_inventory(&root),
            Ok(vec![format!(
                "patch-reverie-dbt-buck.rs: pinned reverie-dbt has the expected \
                 {REVERIE_DBT_FILES} files"
            )])
        );
        fs::remove_dir_all(&root).unwrap();
        for found in [REVERIE_DBT_FILES - 1, REVERIE_DBT_FILES + 1] {
            let root = checkout("changed", Some(found));
            assert_eq!(
                check_inventory(&root),
                Err(format!(
                    "pinned reverie-dbt inventory changed: expected {REVERIE_DBT_FILES} files, \
                     found {found}; {INVENTORY_REMEDY}"
                ))
            );
            fs::remove_dir_all(&root).unwrap();
        }
    }

    // An uninitialized submodule lists no file; the remedy is to check it out,
    // not to record a count of zero.
    #[test]
    fn check_inventory_without_the_submodule_says_to_check_it_out() {
        let root = checkout("uninitialized", None);
        assert_eq!(
            check_inventory(&root),
            Err(format!(
                "git ls-files lists no file under reverie/reverie-dbt; {SUBMODULE_REMEDY}"
            ))
        );
        fs::remove_dir_all(&root).unwrap();
        let root = checkout("no-reverie", None);
        fs::remove_dir_all(root.join("reverie")).unwrap();
        assert_eq!(
            check_inventory(&root),
            Err(format!(
                "failed to inventory pinned reverie-dbt in {}: No such file or directory \
                 (os error 2); {SUBMODULE_REMEDY}",
                root.join("reverie").display()
            ))
        );
        fs::remove_dir_all(&root).unwrap();
        // A submodule whose Git directory is gone fails `git ls-files`.
        let root = checkout("broken", None);
        fs::write(root.join("reverie/.git"), "gitdir: /nonexistent\n").unwrap();
        let error = check_inventory(&root).unwrap_err();
        assert!(
            error.starts_with("git ls-files for pinned reverie-dbt failed with "),
            "{error}"
        );
        assert!(error.ends_with(SUBMODULE_REMEDY), "{error}");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_run_outside_a_checkout_says_where_to_run() {
        let error = check_inventory(Path::new("/")).unwrap_err();
        assert!(error.starts_with("git rev-parse failed with "), "{error}");
        assert!(error.ends_with(CHECKOUT_REMEDY), "{error}");
    }

    // A Git repository that is not a Hermit checkout is refused before its
    // `reverie` is listed. From the reverie submodule, `reverie` is Reverie's
    // own crate directory and lists nothing, which would otherwise say to check
    // out a submodule that is checked out. From the dev-hermit parent it is the
    // parent's Reverie, which would otherwise be counted: here it has the
    // recorded count and would pass. Run from a subdirectory, as main()'s "."
    // can be, the refusal still names the repository root.
    #[test]
    fn a_run_from_another_repository_says_where_to_run() {
        for (name, files) in [("submodule", None), ("parent", Some(REVERIE_DBT_FILES))] {
            let root = checkout(name, files);
            fs::remove_file(root.join(CHECKOUT_MARKER)).unwrap();
            let refusal = Err(format!(
                "{} is not a Hermit checkout: it has no {CHECKOUT_MARKER}; {CHECKOUT_REMEDY}",
                root.display()
            ));
            assert_eq!(check_inventory(&root), refusal, "{name}");
            assert_eq!(run(&[OsString::from("BUCK")], &root), refusal, "{name}");
            let subdirectory = root.join(CHECKOUT_MARKER);
            let subdirectory = subdirectory.parent().unwrap();
            assert_eq!(check_inventory(subdirectory), refusal, "{name}");
            fs::remove_dir_all(&root).unwrap();
        }
    }

    // The arguments are read before the checkout, which `/` would fail to
    // load.
    #[test]
    fn help_prints_the_usage_and_a_wrong_argument_count_is_a_usage_error() {
        for argument in ["--help", "-h"] {
            assert_eq!(
                run(&[OsString::from(argument)], Path::new("/")),
                Ok(vec![USAGE.to_owned(), HELP.to_owned()])
            );
        }
        for arguments in [&[][..], &["--check-inventory", "BUCK"], &["-h", "-h"]] {
            let arguments = arguments.iter().map(OsString::from).collect::<Vec<_>>();
            let error = run(&arguments, Path::new("/")).unwrap_err();
            assert!(error.starts_with("takes exactly one argument; "), "{error}");
            assert!(error.ends_with(&format!("\n{USAGE}")), "{error}");
        }
        for option in ["--bogus", "-x"] {
            assert_eq!(
                run(&[OsString::from(option)], Path::new("/")),
                Err(format!(
                    "unknown option {option}; run it with --help for what it does\n{USAGE}"
                ))
            );
        }
    }

    // A BUCK path is read relative to the run's directory, patched once, and
    // left alone by a second run. A missing BUCK, a missing vendored crate and
    // a BUCK that is not the generated one each say what to do, the first two
    // in a checkout that has not been regenerated yet. Such a checkout has no
    // generated BUCK, so the remedy for a missing one, spelled out here, says
    // what generates it.
    #[test]
    fn a_generated_buck_is_patched_in_place() {
        let root = checkout("patch", Some(REVERIE_DBT_FILES));
        assert_eq!(
            run(&[OsString::from("missing")], &root),
            Err(format!(
                "failed to read {}: No such file or directory (os error 2); pass the BUCK file \
                 Reindeer generated; bootstrap/regenerate-rust-deps generates \
                 shim/third-party/rust/BUCK and runs this script on it",
                root.join("missing").display()
            ))
        );
        fs::write(root.join("BUCK"), fixture()).unwrap();
        assert_eq!(
            run(&[OsString::from("BUCK")], &root),
            Err(format!(
                "failed to read {}: No such file or directory (os error 2); {VENDOR_REMEDY}",
                root.join(VENDORED_CRATE).display()
            ))
        );
        fs::create_dir_all(root.join(VENDORED_CRATE)).unwrap();
        let mut files = (0..REVERIE_DBT_FILES)
            .map(|index| format!("{index}.rs"))
            .collect::<Vec<_>>();
        files.sort();
        for file in &files {
            fs::write(root.join(VENDORED_CRATE).join(file), "").unwrap();
        }
        let handwritten = "rust_library(name = \"hermit\")\n";
        fs::write(root.join("hermit-BUCK"), handwritten).unwrap();
        assert_eq!(
            run(&[OsString::from("hermit-BUCK")], &root),
            Err(format!(
                "generated BUCK must contain exactly one cargo_buildscript load; {BUCK_REMEDY}"
            ))
        );
        assert_eq!(
            fs::read_to_string(root.join("hermit-BUCK")).unwrap(),
            handwritten
        );
        let patched = patch_buck(&fixture(), &files).unwrap();
        for _ in 0..2 {
            assert_eq!(run(&[OsString::from("BUCK")], &root), Ok(vec![]));
            assert_eq!(fs::read_to_string(root.join("BUCK")).unwrap(), patched);
        }
        fs::remove_dir_all(&root).unwrap();
    }
}
