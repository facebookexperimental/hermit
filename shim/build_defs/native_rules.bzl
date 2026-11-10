# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

load("@prelude//utils:type_defs.bzl", "is_dict", "is_select")
load("@shim//build_defs/lib:oss.bzl", "translate_target")

def buck_genrule(visibility = ["PUBLIC"], **kwargs):
    # @lint-ignore BUCKLINT: avoid "native is forbidden in fbcode"
    native.genrule(visibility = visibility, **kwargs)

def buck_command_alias(**_):
    pass

def _fix_filegroup_src(src):
    # A label names a target, possibly by its internal path; anything else
    # is a file in this package.
    if "//" in src or src.startswith(":"):
        return translate_target(src)
    return src

def _fix_filegroup_srcs(srcs):
    if is_select(srcs):
        return select_map(srcs, _fix_filegroup_srcs)
    if is_dict(srcs):
        return {k: _fix_filegroup_src(v) for (k, v) in srcs.items()}
    return [_fix_filegroup_src(src) for src in srcs]

def buck_filegroup(visibility = ["PUBLIC"], srcs = [], **kwargs):
    # Internal visibility patterns name internal package paths, so translate
    # them as labels are; PUBLIC has no "//" and passes through unchanged.
    visibility = [translate_target(pattern) for pattern in visibility]

    # @lint-ignore BUCKLINT: avoid "native is forbidden in fbcode"
    native.filegroup(visibility = visibility, srcs = _fix_filegroup_srcs(srcs), **kwargs)

def alias(actual, visibility = ["PUBLIC"], **kwargs):
    if actual.startswith("//buck2/"):
        actual = "root//" + actual.removeprefix("//buck2/")
    native.alias(actual = actual, visibility = visibility, **kwargs)

def buck_sh_binary(visibility = ["PUBLIC"], **kwargs):
    # @lint-ignore BUCKLINT: avoid "native is forbidden in fbcode"
    native.sh_binary(visibility = visibility, **kwargs)

# Runs a shell script as a test. Maps onto the prelude's sh_test.
def buck_sh_test(**kwargs):
    native.sh_test(**kwargs)
