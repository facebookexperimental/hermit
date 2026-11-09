/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Keep this library cacheable for the rust-scripts that link it.
//!
//! scripts/validate.rs, and dev-hermit's ci-hub/ci-hub.rs and
//! ci-hub/series/check-series-rows.rs, depend on this library by path.
//! dev-hermit's rust-script cache (ci-hub/validate/rust_script_cas.py) shares
//! one compiled binary between checkouts only when no local crate in it reads
//! compile-time environment outside the manifest-derived `CARGO_PKG_*` set,
//! as rustc records each read in the crate's dep-info (`# env-dep:` lines).
//! Such a value can be the path of the checkout the binary was built in, so
//! the cache refuses to store the binary at all.
//!
//! One `env!("CARGO_MANIFEST_DIR")` in library code (hermit 1decde180,
//! 2026-11-08) made it refuse all three scripts. Every validation then
//! compiled them from cold, 2.5 to 8 minutes each behind one host-wide build
//! lock, and setup and checker nodes ran out of time waiting for it. A
//! binary or a test reads such a variable itself, as `compiled_manifest_dir!`
//! does; the library never does.
//!
//! The check reads the dep-info rustc wrote for the library build this test
//! links against, which sits beside the test executable in Cargo's `deps`
//! directory, so it sees exactly what the cache sees and needs no build of
//! its own.

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;

/// Compile-time environment whose value comes from the package manifest,
/// never from the checkout path or the run: the set the cache accepts from a
/// dependency crate (`SAFE_DEPENDENCY_ENV_DEPS` in rust_script_cas.py).
const MANIFEST_DERIVED: [&str; 14] = [
    "CARGO_PKG_NAME",
    "CARGO_PKG_VERSION",
    "CARGO_PKG_VERSION_MAJOR",
    "CARGO_PKG_VERSION_MINOR",
    "CARGO_PKG_VERSION_PATCH",
    "CARGO_PKG_VERSION_PRE",
    "CARGO_PKG_AUTHORS",
    "CARGO_PKG_DESCRIPTION",
    "CARGO_PKG_HOMEPAGE",
    "CARGO_PKG_REPOSITORY",
    "CARGO_PKG_LICENSE",
    "CARGO_PKG_LICENSE_FILE",
    "CARGO_PKG_RUST_VERSION",
    "CARGO_PKG_README",
];

/// Whether a dep-info describes a library build: one of its rules makes the
/// `.rlib` or `.rmeta`. The package's `hermit-manifest-plan` binary and the
/// library's own unit-test harness share the crate name and file prefix, and
/// both may read `CARGO_MANIFEST_DIR`, but each makes an executable.
fn is_library_build(dep_info: &str) -> bool {
    dep_info.lines().any(|line| {
        line.split_once(": ")
            .or_else(|| line.strip_suffix(':').map(|target| (target, "")))
            .is_some_and(|(target, _)| target.ends_with(".rlib") || target.ends_with(".rmeta"))
    })
}

/// Every `# env-dep:` record outside [`MANIFEST_DERIVED`], as `NAME=value`.
fn path_capable_environment(dep_info: &str) -> Vec<String> {
    dep_info
        .lines()
        .filter_map(|line| line.strip_prefix("# env-dep:"))
        .filter(|record| {
            let name = record.split_once('=').map_or(*record, |(name, _)| name);
            !MANIFEST_DERIVED.contains(&name)
        })
        .map(str::to_owned)
        .collect()
}

/// The newest library dep-info in `deps`. Cargo rebuilt or confirmed the
/// library this test links against before running it, so an older one can
/// only be a build of other features or of an older tree.
fn newest_library_dep_info(deps: &Path) -> Option<(PathBuf, String)> {
    let mut newest: Option<(SystemTime, PathBuf, String)> = None;
    for entry in fs::read_dir(deps).expect("reading Cargo's deps directory") {
        let path = entry.expect("listing Cargo's deps directory").path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if !(name.starts_with("hermit_manifest_plan-") && name.ends_with(".d")) {
            continue;
        }
        let text = fs::read_to_string(&path).expect("reading a dep-info file");
        if !is_library_build(&text) {
            continue;
        }
        let modified = fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .expect("reading a dep-info file's modification time");
        if newest.as_ref().is_none_or(|(time, _, _)| modified > *time) {
            newest = Some((modified, path, text));
        }
    }
    newest.map(|(_, path, text)| (path, text))
}

#[test]
fn the_library_build_reads_no_path_capable_compile_time_environment() {
    let exe = std::env::current_exe().expect("locating this test executable");
    let deps = exe
        .parent()
        .expect("the test executable sits in Cargo's deps directory");
    let (dep_info, text) = newest_library_dep_info(deps).unwrap_or_else(|| {
        panic!(
            "no dep-info of the hermit-manifest-plan library build beside {}; \
             run this test from Cargo's target directory, where Cargo builds \
             the library it links",
            exe.display()
        )
    });
    let found = path_capable_environment(&text);
    assert!(
        found.is_empty(),
        "the hermit-manifest-plan library build reads compile-time environment \
         {found:?} ({}). dev-hermit's rust-script cache will not store \
         scripts/validate.rs, ci-hub.rs or check-series-rows.rs while it \
         does, so every validation compiles them from cold. Read the \
         variable in the calling binary or test instead, as \
         compiled_manifest_dir! does.",
        dep_info.display()
    );
}

#[test]
fn a_path_read_in_a_library_build_is_found() {
    // The shapes rustc writes: the library's rules, a phony source rule, and
    // the env-dep records after a blank line.
    let library = "/t/deps/hermit_manifest_plan-0a.d: src/lib.rs\n\
                   /t/deps/libhermit_manifest_plan-0a.rlib: src/lib.rs\n\
                   /t/deps/libhermit_manifest_plan-0a.rmeta: src/lib.rs\n\
                   \n\
                   src/lib.rs:\n\
                   \n\
                   # env-dep:CARGO_PKG_VERSION=0.5.4\n\
                   # env-dep:CARGO_MANIFEST_DIR=/home/someone/hermit/ci/manifest-plan\n";
    assert!(is_library_build(library));
    assert_eq!(
        path_capable_environment(library),
        ["CARGO_MANIFEST_DIR=/home/someone/hermit/ci/manifest-plan"]
    );
    // A value-less record (a variable unset at compile time) still counts.
    assert_eq!(
        path_capable_environment("# env-dep:HOME\n# env-dep:CARGO_PKG_NAME=x\n"),
        ["HOME"]
    );
    // The package binary and the unit-test harness make executables.
    let binary = "/t/deps/hermit_manifest_plan-0b.d: src/main.rs\n\
                  /t/deps/hermit_manifest_plan-0b: src/main.rs\n\
                  \n\
                  # env-dep:CARGO_MANIFEST_DIR=/home/someone/hermit/ci/manifest-plan\n";
    assert!(!is_library_build(binary));
}
