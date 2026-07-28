#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

here="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly -a tests=(
    compression.sh
    archive.sh
    text-processing.sh
    reproducible-build.sh
    numerical.sh
)

for test in "${tests[@]}"; do
    printf '\n==> data handling: %s\n' "$test"
    "$here/$test"
done

printf '\nPASS: %s/%s data-handling tests proved naked variation and strict determinism\n' \
    "${#tests[@]}" "${#tests[@]}"
