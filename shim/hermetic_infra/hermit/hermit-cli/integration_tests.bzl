# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# Open-source stand-in for the internal
# fbcode//hermetic_infra/hermit/hermit-cli:integration_tests.bzl, which
# hermit-cli/BUCK loads as
# "@fbcode//hermetic_infra/hermit/hermit-cli:integration_tests.bzl" (the
# fbcode cell is this shim here). Internally hermit_integration_tests defines
# the validation DAG's hermit-cli integration nodes as Buck tests for the
# fbsource test runner. This build runs them through Cargo and the validation
# DAG, so it defines no targets here.

def hermit_integration_tests():
    pass
