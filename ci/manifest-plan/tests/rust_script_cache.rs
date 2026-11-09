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
//! The check reads the dep-info rustc wrote for every library build in the
//! Cargo `deps` directory this test executable sits in that is current, that
//! is, written after each source it lists last changed. Cargo has just
//! confirmed that the build this test links is current, so it is always
//! among them; other current builds of the same sources (other RUSTFLAGS or
//! features, which a rust-script cache keys separately) are checked too. A
//! build from older sources is skipped, as Cargo would rebuild it before
//! linking it. Checking only the newest dep-info passed falsely when a clean
//! alternate build was newer than the violating one this test linked
//! (rev-h5326-astra, 2026-11-09).

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
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

/// The workspace root, which Cargo runs rustc from, so relative paths in a
/// dep-info are relative to it.
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Whether a dep-info describes a library build that can be linked: one of
/// its rules makes the `.rlib`. The package's `hermit-manifest-plan` binary
/// and the library's own unit-test harness share the crate name and file
/// prefix, and both may read `CARGO_MANIFEST_DIR` in test or binary code; a
/// build of either makes an executable, and a `cargo check` or `cargo clippy`
/// of the unit-test harness makes only an `.rmeta`.
fn is_library_build(dep_info: &str) -> bool {
    dep_info.lines().any(|line| {
        line.split_once(": ")
            .or_else(|| line.strip_suffix(':').map(|target| (target, "")))
            .is_some_and(|(target, _)| target.ends_with(".rlib"))
    })
}

/// The sources the first rule of a dep-info lists, with `\ ` unescaped.
fn first_rule_sources(dep_info: &str) -> Vec<String> {
    let first = dep_info.lines().next().unwrap_or("");
    let Some((_, rest)) = first.split_once(": ") else {
        return Vec::new();
    };
    let mut sources = Vec::new();
    let mut current = String::new();
    let mut chars = rest.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek() == Some(&' ') {
            current.push(' ');
            chars.next();
        } else if c == ' ' {
            if !current.is_empty() {
                sources.push(std::mem::take(&mut current));
            }
        } else {
            current.push(c);
        }
    }
    if !current.is_empty() {
        sources.push(current);
    }
    sources
}

/// Whether rustc wrote the dep-info at `path` no earlier than every source
/// it lists last changed, the test Cargo applies before it links a build as
/// it is. A source that is missing makes the build out of date.
fn is_current(path: &Path, dep_info: &str, root: &Path) -> bool {
    let Ok(written) = fs::metadata(path).and_then(|metadata| metadata.modified()) else {
        return false;
    };
    let sources = first_rule_sources(dep_info);
    !sources.is_empty()
        && sources.iter().all(|source| {
            fs::metadata(root.join(source))
                .and_then(|metadata| metadata.modified())
                .is_ok_and(|changed| changed <= written)
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

/// Every current library build in `deps`, with the path-capable
/// compile-time environment its dep-info records.
fn current_library_builds(deps: &Path, root: &Path) -> Vec<(PathBuf, Vec<String>)> {
    let mut builds = Vec::new();
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
        if is_library_build(&text) && is_current(&path, &text, root) {
            builds.push((path, path_capable_environment(&text)));
        }
    }
    builds.sort();
    builds
}

/// The builds among `builds` that read path-capable environment, for an
/// assertion message.
fn violations(builds: &[(PathBuf, Vec<String>)]) -> Vec<String> {
    builds
        .iter()
        .filter(|(_, found)| !found.is_empty())
        .map(|(path, found)| format!("{found:?} ({})", path.display()))
        .collect()
}

#[test]
fn no_current_library_build_reads_path_capable_compile_time_environment() {
    let exe = std::env::current_exe().expect("locating this test executable");
    let deps = exe
        .parent()
        .expect("the test executable sits in Cargo's deps directory");
    let builds = current_library_builds(deps, &workspace_root());
    assert!(
        !builds.is_empty(),
        "no current dep-info of the hermit-manifest-plan library build beside {}; \
         run this test from Cargo's target directory, where Cargo builds the \
         library it links",
        exe.display()
    );
    let found = violations(&builds);
    assert!(
        found.is_empty(),
        "the hermit-manifest-plan library build reads compile-time environment: \
         {found:?}. dev-hermit's rust-script cache will not store \
         scripts/validate.rs, ci-hub.rs or check-series-rows.rs while it \
         does, so every validation compiles them from cold. Read the \
         variable in the calling binary or test instead, as \
         compiled_manifest_dir! does."
    );
}

#[test]
fn a_violating_build_is_found_beside_a_newer_clean_build() {
    // The reviewer's reproduction: the linked default build reads a path
    // under a cfg, and a clean build of the same sources with other RUSTFLAGS
    // is newer. Reading only the newest dep-info passed.
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "rust_script_cache-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let deps = dir.join("deps");
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::create_dir_all(&deps).unwrap();
    let base = SystemTime::now() - Duration::from_secs(3600);
    let write = |path: &Path, text: &str, age: u64| {
        fs::write(path, text).unwrap();
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(base + Duration::from_secs(age))
            .unwrap();
    };
    let library = |hash: &str, env: &str| {
        format!(
            "{d}/hermit_manifest_plan-{hash}.d: src/lib.rs\n\
             {d}/libhermit_manifest_plan-{hash}.rlib: src/lib.rs\n\
             {d}/libhermit_manifest_plan-{hash}.rmeta: src/lib.rs\n\
             \n\
             src/lib.rs:\n\
             \n\
             # env-dep:CARGO_PKG_VERSION=0.5.4\n{env}",
            d = deps.display()
        )
    };
    let path_read = "# env-dep:CARGO_MANIFEST_DIR=/src/hermit/ci/manifest-plan\n";
    write(&dir.join("src/lib.rs"), "", 100);
    // Built from the sources as they are now: the linked default build, which
    // reads the path, and the newer clean alternate.
    write(
        &deps.join("hermit_manifest_plan-0a.d"),
        &library("0a", path_read),
        200,
    );
    write(
        &deps.join("hermit_manifest_plan-0b.d"),
        &library("0b", ""),
        300,
    );
    // Built before the sources last changed, so Cargo would rebuild it.
    write(
        &deps.join("hermit_manifest_plan-0c.d"),
        &library("0c", path_read),
        50,
    );
    // The package binary may read the path.
    write(
        &deps.join("hermit_manifest_plan-0d.d"),
        &format!(
            "{d}/hermit_manifest_plan-0d.d: src/main.rs\n\
             {d}/hermit_manifest_plan-0d: src/main.rs\n\
             \n\
             {path_read}",
            d = deps.display()
        ),
        250,
    );
    write(&dir.join("src/main.rs"), "", 100);

    let builds = current_library_builds(&deps, &dir);
    let names: Vec<_> = builds
        .iter()
        .map(|(path, _)| path.file_name().unwrap().to_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        names,
        ["hermit_manifest_plan-0a.d", "hermit_manifest_plan-0b.d"]
    );
    let found = violations(&builds);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains("hermit_manifest_plan-0a.d"), "{found:?}");
    assert!(found[0].contains("CARGO_MANIFEST_DIR"), "{found:?}");

    // Once a source changes after every build, none is current, which the
    // test above refuses rather than passing.
    write(&dir.join("src/lib.rs"), "changed", 400);
    assert!(current_library_builds(&deps, &dir).is_empty());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_path_read_in_a_library_build_is_found() {
    // The shapes rustc writes: the library's rules, a phony source rule, and
    // the env-dep records after a blank line.
    let library = "/t/deps/hermit_manifest_plan-0a.d: src/lib.rs src/a\\ b.rs\n\
                   /t/deps/libhermit_manifest_plan-0a.rlib: src/lib.rs src/a\\ b.rs\n\
                   /t/deps/libhermit_manifest_plan-0a.rmeta: src/lib.rs src/a\\ b.rs\n\
                   \n\
                   src/lib.rs:\n\
                   \n\
                   # env-dep:CARGO_PKG_VERSION=0.5.4\n\
                   # env-dep:CARGO_MANIFEST_DIR=/src/hermit/ci/manifest-plan\n";
    assert!(is_library_build(library));
    assert_eq!(first_rule_sources(library), ["src/lib.rs", "src/a b.rs"]);
    assert_eq!(
        path_capable_environment(library),
        ["CARGO_MANIFEST_DIR=/src/hermit/ci/manifest-plan"]
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
                  # env-dep:CARGO_MANIFEST_DIR=/src/hermit/ci/manifest-plan\n";
    assert!(!is_library_build(binary));
    // `cargo clippy --tests` checks the unit-test harness: metadata only.
    let checked = "/t/deps/hermit_manifest_plan-0c.d: src/lib.rs\n\
                   /t/deps/libhermit_manifest_plan-0c.rmeta: src/lib.rs\n\
                   \n\
                   # env-dep:CARGO_MANIFEST_DIR=/src/hermit/ci/manifest-plan\n\
                   # env-dep:CLIPPY_CONF_DIR\n";
    assert!(!is_library_build(checked));
}
