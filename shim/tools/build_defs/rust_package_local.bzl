# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# Applies rustc flags to every target in an fbcode package. Open-source builds
# take rustc flags from the toolchain and .buckconfig instead.

def _set_rustc_flags(_flags):
    pass

rust_package_local = struct(
    set_rustc_flags = _set_rustc_flags,
)
