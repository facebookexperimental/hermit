// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// This source code is dual-licensed under either the MIT license found in the
// LICENSE-MIT file in the root directory of this source tree or the Apache
// License, Version 2.0 found in the LICENSE-APACHE file in the root directory
// of this source tree. You may select, at your option, one of the
// above-listed licenses.

//! Reports the fbsource revision and fbpkg version a binary was built from.
//! Callers reach for it under `#[cfg(fbcode_build)]`, which is not set outside
//! Meta, so the values here are never read in an open-source build. The type
//! exists so the dependency edge in the BUCK files resolves.

pub struct BuildInfo;

impl BuildInfo {
    pub fn get_revision() -> &'static str {
        ""
    }

    pub fn get_package_version() -> &'static str {
        ""
    }
}
