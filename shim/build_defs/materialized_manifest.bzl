# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

def _materialized_manifest_impl(ctx):
    output = ctx.actions.copied_dir(
        ctx.attrs.name,
        ctx.attrs.srcs,
        has_content_based_path = False,
    )
    return [DefaultInfo(default_output = output)]

# Build scripts normally receive Prelude's symlinked manifest directory. Some
# native build systems preserve input symlinks during installation, making the
# installed result depend on the source directory's old relative layout. This
# opt-in rule gives such a build script a declared, fully copied source tree.
materialized_manifest = rule(
    impl = _materialized_manifest_impl,
    attrs = {
        "srcs": attrs.dict(key = attrs.string(), value = attrs.source()),
    },
)
