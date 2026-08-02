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
//! (`CARGO_PKG_VERSION`); this script only augments it with the build date and
//! the source revision so a released binary can be traced back to a commit.
//! Both values are exposed to the crate through `cargo:rustc-env` and read with
//! `env!` in `src/bin/hermit/version.rs`.
//!
//! Only the Cargo/OSS build runs this script. The fbcode (Buck) build derives
//! its version from `build_info::BuildInfo` instead, so nothing here needs to
//! work under Buck.

#[path = "build_support.rs"]
mod build_support;

use build_support::build_date;
use build_support::git_short_sha;
use build_support::git_watch_paths;

fn main() {
    let sha = git_short_sha();
    let date = build_date();

    println!("cargo:rustc-env=HERMIT_BUILD_GIT_SHA={sha}");
    println!("cargo:rustc-env=HERMIT_BUILD_DATE={date}");

    // Re-run when the revision, index, or a tracked worktree file changes so
    // the embedded provenance stays accurate. Untracked generated output is
    // deliberately excluded by build_support.
    for path in git_watch_paths() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
}
