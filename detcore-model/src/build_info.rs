/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Machine-readable facts about a Hermit binary.

use serde::Deserialize;
use serde::Serialize;

/// Compile-time Cargo features whose presence changes the public CLI surface.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BuildFeatures {
    pub dbt: bool,
    pub e9patch: bool,
    pub sabre: bool,
}

/// The build facts emitted by `hermit version --json`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BuildInfo {
    pub schema: u64,
    pub version: String,
    pub build_date: Option<String>,
    pub git_sha: String,
    pub features: BuildFeatures,
}

impl BuildInfo {
    pub const SCHEMA: u64 = 1;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_fields_are_refused() {
        let value = serde_json::json!({
            "schema": BuildInfo::SCHEMA,
            "version": "0.2.0",
            "build_date": null,
            "git_sha": "0123456789ab",
            "features": {"dbt": false, "e9patch": false, "sabre": false},
            "future": true,
        });
        let error = serde_json::from_value::<BuildInfo>(value).unwrap_err();
        assert!(error.to_string().contains("unknown field `future`"));
    }
}
