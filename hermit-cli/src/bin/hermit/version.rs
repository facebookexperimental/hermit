/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::sync::OnceLock;

pub struct Version(String);

impl Version {
    /// Gets a static string of the version. Useful for integration with
    /// clap.
    pub fn get() -> &'static str {
        static VERSION: OnceLock<Version> = OnceLock::new();
        VERSION.get_or_init(Self::new).version()
    }

    /// Returns the version string.
    pub fn version(&self) -> &str {
        &self.0
    }

    /// Computes the version string from the build info.
    pub fn new() -> Self {
        #[cfg(fbcode_build)]
        {
            use build_info::BuildInfo;

            let revision = Some(BuildInfo::get_revision()).filter(|s| !s.is_empty());
            let pkg_version = Some(BuildInfo::get_package_version()).filter(|s| !s.is_empty());

            Self(format!(
                "fbsource: {}, fbpkg: hermit:{}",
                revision.unwrap_or("unknown"),
                pkg_version.unwrap_or("unknown")
            ))
        }

        #[cfg(not(fbcode_build))]
        {
            // Single source of truth: the crate version from `Cargo.toml`,
            // augmented with the build date and source revision emitted by
            // `build.rs`. Produces, for example:
            //   0.2.0 (2026-07-31, gabc123def456)
            Self(format!(
                "{} ({}, g{})",
                env!("CARGO_PKG_VERSION"),
                env!("HERMIT_BUILD_DATE"),
                env!("HERMIT_BUILD_GIT_SHA"),
            ))
        }
    }
}
