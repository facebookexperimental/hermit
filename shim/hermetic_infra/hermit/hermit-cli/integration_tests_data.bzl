# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# Open-source stand-in for the internal
# fbcode//hermetic_infra/hermit/hermit-cli:integration_tests_data.bzl, which the
# root BUCK loads as
# "@fbcode//hermetic_infra/hermit/hermit-cli:integration_tests_data.bzl" (the
# fbcode cell is this shim here). Internally a generator writes that file from
# ci/dag/validate.json so that the fbsource test runner can run hermit-cli's
# Cargo integration tests as Buck targets. This build runs them through Cargo,
# so there are none here.

# Sources the integration tests include by #[path] from outside hermit-cli.
# The root BUCK exports each one. Internally the list also holds the ci/
# sources that ci/manifest-plan's library maps from those exports; this build
# compiles that library with Buck too, so it lists exactly those.
EXTERNAL_SRCS = [
    "ci/cargo-guest-binaries.rs",
    "ci/lit-guest-binaries.rs",
    "ci/record-replay-workloads.rs",
]

# Reverie sources the integration tests include through the reverie/ submodule
# path; Reverie's BUCK exports them.
REVERIE_SRCS = []
