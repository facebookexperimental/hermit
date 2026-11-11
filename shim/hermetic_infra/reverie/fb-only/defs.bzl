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
# is this shim here). Internally these names hold what Reverie's build needs
# from the tree around it: Hermit's gated test runner, the import check,
# exports of the Reverie sources Hermit's integration tests include, and where
# Reverie's sources are. In the open-source build:
# - gated_test, excluded_paths_test and downstream_source_exports define
#   nothing, and run_gated_only adds no label: the original test target is the
#   test here;
# - DOWNSTREAM_TEST_VISIBILITY is public, as every target of this shim is;
# - REVERIE_SOURCE_ROOT is the reverie submodule's path in the Hermit checkout.

DOWNSTREAM_TEST_VISIBILITY = ["PUBLIC"]

# Reverie's source tree, relative to the directory tests run in: buck2 runs
# them from the project root, the Hermit checkout. A test that finds its crate's
# files through a relative CARGO_MANIFEST_DIR (reverie-linuxemu-unittest) builds
# that path from this root.
REVERIE_SOURCE_ROOT = "reverie"

def run_gated_only():
    return []

def gated_test(name, test, env = {}, labels = [], native_abi_probe = False):
    _unused = (name, test, env, labels, native_abi_probe)  # @unused

def excluded_paths_test(name = "excluded-paths-test"):
    _unused = (name,)  # @unused

def downstream_source_exports():
    pass
