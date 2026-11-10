# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# Open-source stand-in for the internal
# fbcode//hermetic_infra/hermit_import:excluded_paths.bzl, which the root BUCK
# loads as "@fbcode//hermetic_infra/hermit_import:excluded_paths.bzl" (the
# fbcode cell is this shim here). Internally excluded_paths_test fails if a
# path the fbsource import never takes (the DBT, SaBRe and e9patch trees, for
# example) is present. This tree is where those paths live, so the test has
# nothing to check here.

def excluded_paths_test(name = "excluded-paths-test"):
    _unused = (name,)  # @unused
