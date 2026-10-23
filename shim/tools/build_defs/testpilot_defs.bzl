# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# Labels consumed by Meta's test runner. Open-source builds run tests directly,
# so the labels are empty strings.

tpx_labels = struct(
    local_only = "",
    disabled = "",
    run_as_bundle = "",
    test_is_invisible_to_testpilot = "",
)
