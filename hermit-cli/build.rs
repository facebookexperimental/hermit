/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Emits build metadata consumed by `hermit --version`.
//!
//! The crate version is the single source of truth in `Cargo.toml`
//! (`CARGO_PKG_VERSION`); this script only augments it with the build date and,
//! for a release build, the source revision so the released binary can be
//! traced back to a commit. Regular builds embed no revision; set
//! `HERMIT_STAMP_GIT_SHA=1` to stamp one (see `build_support::emit_git_revision`).
//! Both values are exposed to the crate through `cargo:rustc-env` and read with
//! `env!` in `src/bin/hermit/version.rs`.
//!
//! Only the Cargo/OSS build runs this script. The fbcode (Buck) build derives
//! its version from `build_info::BuildInfo` instead, so nothing here needs to
//! work under Buck.

#[path = "build_support.rs"]
pub(crate) mod build_support;

#[path = "compatibility_build.rs"]
mod compatibility_build;

#[path = "compatibility_graph.rs"]
mod compatibility_graph;

#[path = "compatibility_rustc.rs"]
mod compatibility_rustc;

#[path = "compatibility_source.rs"]
mod compatibility_source;

use build_support::build_date;
use build_support::emit_git_revision;

/// Read the package identity written by the source or registry producer. Cargo
/// preserves package metadata when it normalizes a manifest for publication.
pub fn registry_build_in(crate_dir: &std::path::Path) -> Result<bool, String> {
    let path = crate_dir.join("Cargo.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let manifest: toml::Value = toml::from_str(&text)
        .map_err(|error| format!("cannot parse {}: {error}", path.display()))?;
    let identity = manifest
        .get("package")
        .and_then(|package| package.get("metadata"))
        .and_then(|metadata| metadata.get("hermit"))
        .and_then(|hermit| hermit.get("build-identity"))
        .and_then(toml::Value::as_str);
    build_support::registry_build(identity)
}

fn main() {
    println!("cargo:rustc-check-cfg=cfg(fbcode_build)");
    println!("cargo:rustc-check-cfg=cfg(hermit_registry_build)");
    // Shared CLI tests still name the external application backends' former
    // features to skip their cases. Core never enables them, so those branches
    // compile out; check-cfg must still accept the names.
    println!("cargo:rustc-check-cfg=cfg(feature, values(\"dbt\", \"sabre\", \"e9patch\"))");

    // Backend features do not identify a package: a core source build may
    // declare none of the third-party backends. Registry projections set their
    // identity explicitly before packaging (see README.md).
    println!("cargo:rerun-if-changed=Cargo.toml");
    let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR")
        .expect("Cargo must provide the current CLI manifest directory");
    let registry_build = registry_build_in(std::path::Path::new(&manifest_dir))
        .unwrap_or_else(|error| panic!("invalid Hermit package identity: {error}"));
    if registry_build {
        println!("cargo:rustc-cfg=hermit_registry_build");
    }

    compatibility_build::emit(std::path::Path::new(&manifest_dir), registry_build);

    let date = build_date();

    emit_git_revision();
    println!("cargo:rustc-env=HERMIT_BUILD_DATE={date}");

    // EMBED THE REVERIE PIN THIS BINARY WAS BUILT AGAINST.
    //
    // The staged LiteInst/DBT runtimes are built separately and can be
    // ARBITRARILY STALE relative to the pin the tree declares, with nothing
    // reporting it. A cell then measures whichever `.so` happens to be on disk
    // and publishes a verdict about the pin it believes it is testing.
    //
    // Embedding the pin here is what lets the loader compare, at the moment it
    // resolves a staged runtime, the revision the runtime was built from against
    // the revision this binary was built from. Provenance recorded beside the
    // artifact is not enough on its own -- `sabre.revision` has been written for
    // some time and is read by nothing, which is provenance rather than
    // authority.
    let reverie_pin = build_support::reverie_pin();
    println!("cargo:rustc-env=HERMIT_REVERIE_PIN={reverie_pin}");
    println!("cargo:rerun-if-changed=../detcore/Cargo.toml");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_NATIVE_EXEC");
    if std::env::var_os("CARGO_FEATURE_NATIVE_EXEC").is_some() {
        build_support::emit_native_exec_bundle(&reverie_pin);
    }
}
