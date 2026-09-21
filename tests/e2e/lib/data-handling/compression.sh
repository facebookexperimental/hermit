#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

if [[ ${1:-} == --guest ]]; then
    work=$(mktemp -d "${TMPDIR:-/tmp}/hermit-compression.XXXXXX")
    trap 'rm -rf -- "$work"' EXIT
    cd "$work"

    printf 'Hermit deterministic compression fixture\n' >payload
    head -c 64 /dev/urandom >>payload

    gzip -c payload >payload.gz
    bzip2 -c payload >payload.bz2
    xz -c payload >payload.xz
    zstd -q -c payload >payload.zst

    gzip -dc payload.gz | cmp - payload
    bzip2 -dc payload.bz2 | cmp - payload
    xz -dc payload.xz | cmp - payload
    zstd -q -d -c payload.zst | cmp - payload

    sha256sum payload.gz payload.bz2 payload.xz payload.zst
    exit 0
fi

# shellcheck source=tests/e2e/lib/data-handling/common.bash
source "$(dirname -- "$0")/common.bash"
require_tools gzip bzip2 xz zstd sha256sum cmp head
assert_nondeterminism_removed compression "$(readlink -f "$0")" --guest
