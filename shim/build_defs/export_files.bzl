# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

load("@shim//build_defs/lib:oss.bzl", "translate_target")

def export_file(visibility = ["PUBLIC"], **kwargs):
    # Internal visibility patterns name internal package paths, so translate
    # them as labels are; PUBLIC has no "//" and passes through unchanged. A
    # `src` may name another package's file by label.
    src = kwargs.get("src")
    if src != None and ("//" in src or src.startswith(":")):
        kwargs["src"] = translate_target(src)

    # @lint-ignore BUCKLINT: avoid "native is forbidden in fbcode"
    native.export_file(visibility = [translate_target(pattern) for pattern in visibility], **kwargs)

# The internal build's name for the native rule; the same rule here.
buck_export_file = export_file

def export_files(files, visibility = ["PUBLIC"], **kwargs):
    # @lint-ignore BUCKLINT: avoid "native is forbidden in fbcode"
    for file in files:
        native.export_file(name = file, visibility = visibility, **kwargs)
