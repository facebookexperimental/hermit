/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Git commands that act on a repository this crate names explicitly.
//!
//! Git exports its repository-location variables to every hook and to every
//! `git rebase --exec` step. An inherited `GIT_DIR` takes precedence over both
//! `git -C` and the child's working directory, so a command meant for a named
//! repository acts on the caller's repository instead. In a scratch fixture
//! that means `git init` rewrites the caller's `core.bare` and a fixture commit
//! moves the caller's HEAD. That is what happened in
//! <https://github.com/rrnewton/hermit/issues/3362>, when a rebase step ran this
//! crate's tests.
//!
//! Only the location variables are removed. `GIT_CONFIG_COUNT` and its
//! companions carry this host's proxy rewrites and must survive.

use std::process::Command;

/// Variables that redirect git away from the directory it was pointed at.
pub(crate) const REPOSITORY_LOCATION_VARIABLES: [&str; 8] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
];

/// A `git` command that finds its repository from its own `-C` or working
/// directory, never from an inherited repository-location variable.
pub(crate) fn git_command() -> Command {
    let mut command = Command::new("git");
    for name in REPOSITORY_LOCATION_VARIABLES {
        command.env_remove(name);
    }
    command
}

/// The work tree holding this crate, resolved by git from the crate's own
/// directory.
///
/// Tests use this in place of [`crate::validation_dag::repo_root`], which
/// deliberately follows the caller's working directory and environment. Under
/// an inherited `GIT_DIR`, that answer is the test's working directory rather
/// than the checkout.
#[cfg(test)]
pub(crate) fn checkout_root() -> std::path::PathBuf {
    let output = git_command()
        .arg("-C")
        .arg(env!("CARGO_MANIFEST_DIR"))
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .expect("asking git for the checkout holding this crate");
    assert!(
        output.status.success(),
        "git could not resolve the checkout holding this crate: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    std::path::PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
}
