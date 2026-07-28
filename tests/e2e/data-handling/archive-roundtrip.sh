#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
# HERMIT_E2E_META_BEGIN
# {
#   "schema": 1,
#   "id": "data-handling/archive-roundtrip",
#   "category": "data-handling",
#   "description": "Create, extract, and verify a normalized tar archive",
#   "lane": "portable",
#   "requires": ["linux", "x86_64", "userns", "ptrace"],
#   "timeout_seconds": 60,
#   "observation": {"status": true, "stdout": true, "stderr": false, "artifacts": []},
#   "modes": {"verify": {"backends": ["ptrace"]}},
#   "disabled_modes": {
#     "naked": "The archive uses normalized metadata and is deterministic by construction",
#     "replay": "The focused C sentinel provides the blocking replay contract",
#     "chaos": "The workload is single-threaded"
#   },
#   "occasional": false
# }
# HERMIT_E2E_META_END
set -euo pipefail

case ${1:-} in
    --prepare) exit 0 ;;
    --run)
        work="$E2E_TMPDIR/archive"
        rm -rf "$work"
        mkdir -p "$work/input" "$work/output"
        printf 'alpha\nbeta\ngamma\n' >"$work/input/payload.txt"
        touch -t 200001010000 "$work/input/payload.txt"
        tar --sort=name --mtime='2000-01-01 UTC' --owner=0 --group=0 --numeric-owner \
            -cf "$work/payload.tar" -C "$work/input" payload.txt
        tar -xf "$work/payload.tar" -C "$work/output"
        cmp "$work/input/payload.txt" "$work/output/payload.txt"
        sha256sum "$work/output/payload.txt" | cut -d' ' -f1
        ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
