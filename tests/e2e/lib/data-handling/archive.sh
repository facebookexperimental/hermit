#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

if [[ ${1:-} == --guest ]]; then
    work=$(mktemp -d "${TMPDIR:-/tmp}/hermit-archive.XXXXXX")
    trap 'rm -rf -- "$work"' EXIT
    mkdir -p "$work/source" "$work/output"
    printf 'Hermit tar timestamp fixture\n' >"$work/source/payload.txt"
    touch "$work/source/payload.txt"

    tar --format=ustar -cf "$work/payload.tar" -C "$work/source" payload.txt
    tar -xf "$work/payload.tar" -C "$work/output"
    cmp "$work/source/payload.txt" "$work/output/payload.txt"

    sha256sum "$work/payload.tar" | awk '{ print "tar=" $1 }'
    exit 0
fi

# shellcheck source=tests/e2e/lib/data-handling/common.bash
source "$(dirname -- "$0")/common.bash"
require_tools tar touch sha256sum awk cmp
export NATIVE_ATTEMPTS=3
export NATIVE_RETRY_DELAY=1.1
assert_nondeterminism_removed tar-timestamp "$(readlink -f "$0")" --guest
