# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

load("@shim//build_defs/lib:oss.bzl", "translate_target")

def fbcode_configured_alias(name, actual, additional_constraints = [], platform = None, visibility = ["PUBLIC"], **kwargs):
    """The internal macro pins `actual` to an fbcode platform plus extra
    constraints (such as an optimized build mode). This tree has neither that
    platform nor those constraints, so it is a plain alias of `actual`."""
    _unused = (additional_constraints, platform)  # @unused

    # @lint-ignore BUCKLINT: avoid "native is forbidden in fbcode"
    native.alias(name = name, actual = translate_target(actual), visibility = visibility, **kwargs)
