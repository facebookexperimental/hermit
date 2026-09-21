# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# Declares how much network a test package may use. Open-source builds do not
# run under that test infrastructure, so the declaration is recorded and unused.

def _set_package_default(_access):
    pass

network_access_utils = struct(
    NetworkAccess = struct(
        none = "none",
        public = "public",
    ),
    set_package_default = _set_package_default,
)
