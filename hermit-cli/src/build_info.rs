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

#[cfg(feature = "buck-release-provenance")]
const fn is_ascii_hex(byte: u8) -> bool {
    byte.is_ascii_hexdigit()
}

#[cfg(feature = "buck-release-provenance")]
const fn valid_release_sha(value: &str) -> bool {
    let bytes = value.as_bytes();
    let valid_length = bytes.len() == 12
        || (bytes.len() == 18
            && bytes[12] == b'-'
            && bytes[13] == b'd'
            && bytes[14] == b'i'
            && bytes[15] == b'r'
            && bytes[16] == b't'
            && bytes[17] == b'y');
    if !valid_length {
        return false;
    }
    let mut index = 0;
    while index < 12 {
        if !is_ascii_hex(bytes[index]) {
            return false;
        }
        index += 1;
    }
    true
}

#[cfg(feature = "buck-release-provenance")]
const fn valid_release_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    let mut index = 0;
    while index < bytes.len() {
        if index != 4 && index != 7 && !bytes[index].is_ascii_digit() {
            return false;
        }
        index += 1;
    }
    true
}

#[cfg(feature = "buck-release-provenance")]
const fn valid_release_version(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty()
        || (bytes.len() == 7
            && bytes[0] == b'u'
            && bytes[1] == b'n'
            && bytes[2] == b'k'
            && bytes[3] == b'n'
            && bytes[4] == b'o'
            && bytes[5] == b'w'
            && bytes[6] == b'n')
    {
        return false;
    }
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if !(byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'-' || byte == b'+') {
            return false;
        }
        index += 1;
    }
    true
}

#[cfg(feature = "buck-release-provenance")]
const fn valid_reverie_pin(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 40 {
        return false;
    }
    let mut index = 0;
    while index < bytes.len() {
        if !is_ascii_hex(bytes[index]) {
            return false;
        }
        index += 1;
    }
    true
}

// A release target is unsafe when invoked without the provenance wrapper. Keep
// this assertion in the Rust compilation itself so a direct Buck invocation
// cannot silently emit an empty/unknown release identity.
#[cfg(feature = "buck-release-provenance")]
const _: () = {
    assert!(valid_release_version(env!("CARGO_PKG_VERSION")));
    assert!(valid_release_date(env!("HERMIT_BUILD_DATE")));
    assert!(valid_release_sha(env!("HERMIT_BUILD_GIT_SHA")));
    assert!(valid_reverie_pin(env!("HERMIT_REVERIE_PIN")));
};

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
