# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# Open-source stand-in for the internal
# fbcode//hermetic_infra/hermit/fb-only/shell_tests:defs.bzl, which tests/BUCK
# loads as "@fbcode//hermetic_infra/hermit/fb-only/shell_tests:defs.bzl" (the
# fbcode cell is this shim here).
#
# Internally, a few shell tests get their own targets there: chaos runs too
# long for the default test timeout, and standalone scripts that need the
# repository root. Here no test is split out, so tests/BUCK defines every
# shell and standalone test the ordinary way.

LONG_RUNNING_CHAOS = []

REPOSITORY_ROOT_STANDALONE = {}

def long_running_chaos_shell_test(_path):
    fail("long_running_chaos_shell_test: LONG_RUNNING_CHAOS is empty in the open-source build")

def repository_root_standalone_test(_path):
    fail("repository_root_standalone_test: REPOSITORY_ROOT_STANDALONE is empty in the open-source build")
