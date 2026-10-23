# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# Link groups are an fbcode link-time arrangement. Open-source builds link
# normally, so the registry does nothing here.

def _use_link_groups(**_kwargs):
    pass

link_group_registry = struct(
    use_link_groups = _use_link_groups,
)
