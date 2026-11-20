# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# Reports which fbcode sanitizer a build is configured with. Open-source builds
# select their sanitizer through the prelude and toolchain, so nothing is
# reported here and callers see the unsanitized case.

def _get_sanitizer_v2():
    return None

def _get_sanitizer():
    return None

sanitizers = struct(
    get_sanitizer = _get_sanitizer,
    get_sanitizer_v2 = _get_sanitizer_v2,
)
