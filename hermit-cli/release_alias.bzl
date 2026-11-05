# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

def hermit_release_aliases():
    # hermit-release / libhermit-release pin the release binaries to the
    # OSS-only shim platform through configured_alias, a raw Buck rule that
    # fbcode build files may not call. Define the aliases only in the
    # GitHub build, whose root config carries [oss] (internal_cell);
    # fbcode's root config has no such section, so these targets simply do
    # not exist internally, as before the release pipeline.
    if read_root_config("oss", "internal_cell", "") == "":
        return

    native.configured_alias(
        name = "libhermit-release",
        actual = ":_libhermit-release",
        platform = "shim//:hermit_rust_release_platform",
        visibility = ["PUBLIC"],
    )

    native.configured_alias(
        name = "hermit-release",
        actual = ":_hermit-release",
        platform = "shim//:hermit_rust_release_platform",
        visibility = ["PUBLIC"],
    )
