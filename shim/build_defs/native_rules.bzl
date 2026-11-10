# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

load("@prelude//utils:type_defs.bzl", "is_dict", "is_select")
load("@shim//build_defs/lib:oss.bzl", "translate_macros", "translate_target")

def _fix_genrule_kwargs(kwargs):
    # Internal BUCK files name targets by internal path in `srcs` and inside
    # the macros of the command; translate them as labels are.
    if kwargs.get("srcs"):
        kwargs["srcs"] = _fix_filegroup_srcs(kwargs["srcs"])
    for attr in ("cmd", "bash"):
        if kwargs.get(attr) and not is_select(kwargs[attr]):
            kwargs[attr] = translate_macros(kwargs[attr])
    return kwargs

def buck_genrule(visibility = ["PUBLIC"], **kwargs):
    # @lint-ignore BUCKLINT: avoid "native is forbidden in fbcode"
    native.genrule(visibility = visibility, **_fix_genrule_kwargs(kwargs))

# The C compiler of this shim's C/C++ toolchain (`system_cxx_toolchain` in
# shim/BUCK). That toolchain provides no $(cc) placeholder for a cxx_genrule,
# so cxx_genrule below substitutes it.
_CXX_GENRULE_CC = "gcc"

# A genrule whose command may name the C compiler as $(cc), the one macro of
# the internal cxx_genrule the BUCK files here use.
def cxx_genrule(visibility = ["PUBLIC"], **kwargs):
    kwargs = _fix_genrule_kwargs(kwargs)
    for attr in ("cmd", "bash"):
        if kwargs.get(attr) and not is_select(kwargs[attr]):
            kwargs[attr] = kwargs[attr].replace("$(cc)", _CXX_GENRULE_CC)

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

def buck_sh_binary(visibility = ["PUBLIC"], main = None, **kwargs):
    # @lint-ignore BUCKLINT: avoid "native is forbidden in fbcode"
    native.sh_binary(visibility = visibility, main = _fix_label_or_source(main) if main != None else None, **kwargs)

def _fix_label_or_source(src):
    # A label (possibly by its internal path) or a file in this package.
    if "//" in src or src.startswith(":"):
        return translate_target(src)
    return src

# Runs a shell script as a test. Maps onto the prelude's sh_test. Internal
# BUCK files name targets by internal path in `test`, `deps` and `resources`,
# and inside the macros of `args` and `env`; translate them as labels are.
def buck_sh_test(test = None, args = [], env = {}, deps = [], resources = [], **kwargs):
    native.sh_test(
        test = _fix_label_or_source(test) if test != None else None,
        args = [translate_macros(arg) for arg in args],
        env = {k: translate_macros(v) for (k, v) in env.items()},
        deps = [translate_target(dep) for dep in deps],
        resources = [_fix_label_or_source(r) for r in resources],
        **kwargs
    )
