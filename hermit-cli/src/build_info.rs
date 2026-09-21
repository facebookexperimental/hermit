/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Producer values for the shared machine-readable build record.

#![allow(
    unexpected_cfgs,
    reason = "`fbcode_build` is supplied by the internal Buck build"
)]

pub use detcore_model::build_info::BuildFeatures;
pub use detcore_model::build_info::BuildInfo;

/// Construct the record from values embedded in this binary.
///
/// `hermit --version` remains presentation text and must not be parsed for
/// provenance or feature decisions.
pub fn current() -> BuildInfo {
    #[cfg(fbcode_build)]
    let (version, build_date, git_sha) = {
        use build_info::BuildInfo as FbBuildInfo;

        let revision = Some(FbBuildInfo::get_revision().to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "unknown".to_owned());
        let package = Some(FbBuildInfo::get_package_version().to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "unknown".to_owned());
        (package, None, revision)
    };

    #[cfg(not(fbcode_build))]
    let (version, build_date, git_sha) = (
        env!("CARGO_PKG_VERSION").to_owned(),
        Some(env!("HERMIT_BUILD_DATE").to_owned()),
        env!("HERMIT_BUILD_GIT_SHA").to_owned(),
    );

    BuildInfo {
        schema: BuildInfo::SCHEMA,
        version,
        build_date,
        git_sha,
        features: BuildFeatures {
            dbt: cfg!(feature = "dbt"),
            e9patch: cfg!(feature = "e9patch"),
            sabre: cfg!(feature = "sabre"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_build_info_round_trips_through_the_closed_schema() {
        let expected = current();
        let bytes = serde_json::to_vec(&expected).unwrap();
        let observed: BuildInfo = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(observed, expected);
        assert_eq!(observed.schema, BuildInfo::SCHEMA);
    }
}
