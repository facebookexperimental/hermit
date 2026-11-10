# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# Open-source stand-in for the internal
# fbcode//hermetic_infra/common:wrap_test_suite.bzl, which tests/BUCK loads as
# "@fbcode//hermetic_infra/common:wrap_test_suite.bzl" (the fbcode cell is this
# shim here).
#
# Internally, wrap_test_suite wraps the upstream rr syscall test programs
# (RR_TEST_TARGETS) as Tpx tests under `hermit run`. Open-source builds do
# not carry that runner: the same rr programs are covered here by the Cargo
# harness in hermit-cli/tests/rr_suite.rs (see docs/rr-test-suite.md). So no
# targets are defined here, mirroring the shim's lit_unittest.

RR_TEST_TARGETS = {}

def wrap_test_suite(**_kwargs):
    pass
