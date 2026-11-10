# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# Open-source stand-in for the internal
# fbcode//hermetic_infra/reverie/fb-only:defs.bzl, which Reverie's BUCK files
# load as "@fbcode//hermetic_infra/reverie/fb-only:defs.bzl" (the fbcode cell
# is this shim here). Internally these names hold everything Reverie's build
# needs from its downstream user, Hermit: Hermit's gated test runner, the
# import check, and exports of the Reverie sources Hermit's integration tests
# include. None of that exists in the open-source build, so:
# - gated_test, excluded_paths_test and downstream_source_exports define
#   nothing, and run_gated_only adds no label: the original test target is the
#   test here;
# - DOWNSTREAM_TEST_VISIBILITY is public, as every target of this shim is.

DOWNSTREAM_TEST_VISIBILITY = ["PUBLIC"]

def run_gated_only():
    return []

def gated_test(name, test, env = {}, labels = [], native_abi_probe = False):
    _unused = (name, test, env, labels, native_abi_probe)  # @unused

def excluded_paths_test(name = "excluded-paths-test"):
    _unused = (name,)  # @unused

def downstream_source_exports():
    pass
