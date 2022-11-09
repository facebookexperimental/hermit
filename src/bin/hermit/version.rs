/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use build_info::BuildInfo;
use once_cell::sync::OnceCell;

pub struct Version(String);

impl Version {
    /// Gets a static string of the version. Useful for integration with
    /// clap.
    pub fn get() -> &'static str {
        static VERSION: OnceCell<Version> = OnceCell::new();
        VERSION.get_or_init(Self::new).version()
    }

    /// Returns the version string.
    pub fn version(&self) -> &str {
        &self.0
    }

    /// Computes the version string from the build info.
    pub fn new() -> Self {
        let revision = Some(BuildInfo::get_revision()).filter(|s| !s.is_empty());
        let pkg_version = Some(BuildInfo::get_package_version()).filter(|s| !s.is_empty());

        Self(format!(
            "fbsource: {}, fbpkg: hermit:{}",
            revision.unwrap_or("unknown"),
            pkg_version.unwrap_or("unknown")
        ))
    }
}
