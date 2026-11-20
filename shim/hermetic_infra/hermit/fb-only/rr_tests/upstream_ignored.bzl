# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# Open-source stand-in for the internal
# fbcode//hermetic_infra/hermit/fb-only/rr_tests:upstream_ignored.bzl, which
# tests/BUCK loads as "@fbcode//hermetic_infra/hermit/fb-only/rr_tests:upstream_ignored.bzl"
# (the fbcode cell is this shim here). It names the rr tests upstream's rr suite
# #[ignore]s, for the internal rr wrapper; this build has no such wrapper (see
# ../../../common/wrap_test_suite.bzl).

RR_UPSTREAM_IGNORED = {}
