/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::io::Write;
use std::sync::OnceLock;

use clap::Args;
use hermit::Error;
use hermit::ExitStatus;

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

            Self(fbcode_version(
                option_env!("CARGO_PKG_VERSION"),
                revision,
                pkg_version,
            ))
        }

        #[cfg(not(fbcode_build))]
        {
            // Single source of truth: the crate version from `Cargo.toml`,
            // augmented with the build date and, for a release build, the
            // source revision emitted by `build.rs`.
            Self(cargo_version(
                env!("CARGO_PKG_VERSION"),
                env!("HERMIT_BUILD_DATE"),
                env!("HERMIT_BUILD_GIT_SHA"),
            ))
        }
    }
}

/// Formats a Cargo or OSS Buck build's version. A release build names its
/// revision, for example `0.4.0 (2026-10-02, gabc123def456)`. A regular build
/// embeds no revision (`unknown`, see `build_support::UNSTAMPED_GIT_SHA`) and
/// says so: `0.4.0 (2026-10-02, source revision not embedded)`. A registry build
/// has the published package version even though it carries no Git stamp.
#[cfg(any(not(fbcode_build), test))]
fn cargo_version(crate_version: &str, build_date: &str, git_sha: &str) -> String {
    if git_sha == "unknown" {
        format!("{crate_version} ({build_date}, source revision not embedded)")
    } else {
        format!("{crate_version} ({build_date}, g{git_sha})")
    }
}

/// Formats an fbcode build's version, for example
/// `0.4.0 (fbsource: abc123, fbpkg: hermit:1407)`. Buck sets
/// `CARGO_PKG_VERSION` only when the target asks for it.
#[cfg(any(fbcode_build, test))]
fn fbcode_version(
    crate_version: Option<&str>,
    revision: Option<&str>,
    pkg_version: Option<&str>,
) -> String {
    format!(
        "{} (fbsource: {}, fbpkg: hermit:{})",
        crate_version.unwrap_or("unknown"),
        revision.unwrap_or("unknown"),
        pkg_version.unwrap_or("unknown")
    )
}

#[derive(Debug, Args)]
pub struct VersionOpts {
    /// Emit the producer-owned build facts as one JSON object.
    #[clap(long)]
    json: bool,
}

impl VersionOpts {
    pub fn main(&self) -> Result<ExitStatus, Error> {
        let mut stdout = std::io::stdout().lock();
        if self.json {
            serde_json::to_writer(&mut stdout, &hermit::build_info::current())?;
            writeln!(stdout)?;
        } else {
            writeln!(stdout, "hermit {}", Version::get())?;
        }
        Ok(ExitStatus::Exited(0))
    }
}

#[cfg(test)]
mod tests {
    use super::cargo_version;
    use super::fbcode_version;

    #[test]
    fn cargo_version_names_a_stamped_revision() {
        assert_eq!(
            cargo_version("0.4.0", "2026-10-02", "abc123def456"),
            "0.4.0 (2026-10-02, gabc123def456)"
        );
        assert_eq!(
            cargo_version("0.4.0", "2026-10-02", "abc123def456-dirty"),
            "0.4.0 (2026-10-02, gabc123def456-dirty)"
        );
    }

    #[test]
    fn cargo_version_of_an_unstamped_build_discloses_missing_revision() {
        assert_eq!(
            cargo_version("0.4.0", "2026-10-02", "unknown"),
            "0.4.0 (2026-10-02, source revision not embedded)"
        );
    }

    #[test]
    fn fbcode_version_leads_with_the_crate_version() {
        assert_eq!(
            fbcode_version(Some("0.4.0"), Some("abc123"), Some("1407")),
            "0.4.0 (fbsource: abc123, fbpkg: hermit:1407)"
        );
    }

    #[test]
    fn fbcode_version_marks_missing_build_facts_unknown() {
        assert_eq!(
            fbcode_version(None, None, None),
            "unknown (fbsource: unknown, fbpkg: hermit:unknown)"
        );
    }
}
