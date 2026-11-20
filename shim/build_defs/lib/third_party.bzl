# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# Selects an fbcode third-party toolchain version. Open-source builds take the
# toolchain from the prelude, so no version is selected here.

def _versions(_versions_dict):
    return []

third_party = struct(
    versions = _versions,
)
