#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

case ${1:-} in
    --prepare)
        for command in bash awk date paste sha256sum sort wc; do
            command -v "$command" >/dev/null
        done
        ;;
    --run)
        root=${E2E_TMPDIR:-/tmp}/hermit-bash-interpreter-batch
        rm -rf -- "$root"
        mkdir -p -- "$root"

        input=$root/input.txt
        output=$root/output.txt
        for index in {1..12}; do
            printf '%02d:item-%02d\n' "$index" "$((index * index))"
        done >"$input"

        transformed=$(
            awk -F: '{ print $2 }' "$input" |
                sort -r |
                paste -sd, -
        )
        printf '%s\n' "$transformed" >"$output"

        bytes=$(wc -c <"$output")
        digest=$(sha256sum "$output" | awk '{ print $1 }')
        wall_ns=$(date -u +%s%N)
        printf 'BASH version=%s bytes=%s sha256=%s wall_ns=%s values=%s\n' \
            "$BASH_VERSION" "$bytes" "$digest" "$wall_ns" "$transformed"
        ;;
    *)
        echo "usage: $0 --prepare|--run" >&2
        exit 2
        ;;
esac
