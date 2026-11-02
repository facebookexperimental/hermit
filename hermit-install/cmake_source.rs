/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::io;
use std::path::Path;
use std::path::PathBuf;

/// CMake caches its absolute source directory, even when Cargo's target is shared.
pub fn source_and_build_dir(source: &Path, build_root: &Path) -> io::Result<(PathBuf, PathBuf)> {
    let source = source.canonicalize()?;
    let relative = source
        .strip_prefix(Path::new("/"))
        .expect("canonical CMake source must be absolute");
    // Equal-depth source paths cannot contain one another. Different depths
    // get separate namespaces, so even nested sources have disjoint caches.
    let build = build_root
        .join("dbt-client-sources")
        .join(relative.components().count().to_string())
        .join(relative);
    Ok((source, build))
}
