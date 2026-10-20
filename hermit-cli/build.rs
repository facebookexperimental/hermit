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
mod build_support;

use build_support::build_date;
use build_support::emit_git_revision;

fn main() {
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
}
