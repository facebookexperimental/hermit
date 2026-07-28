#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
# HERMIT_E2E_META_BEGIN
# {
#   "schema": 1,
#   "id": "system-utils/record-getpid",
#   "category": "system-utils",
#   "description": "Process identity is deterministic in run and record/replay modes",
#   "lane": "portable",
#   "requires": ["linux", "x86_64", "userns", "ptrace", "cc"],
#   "timeout_seconds": 60,
#   "observation": {"status": true, "stdout": true, "stderr": false, "artifacts": []},
#   "modes": {
#     "verify": {"backends": ["ptrace"]},
#     "replay": {"backends": ["ptrace"]}
#   },
#   "disabled_modes": {
#     "naked": "Native process IDs vary but this test asserts Hermit's virtualized identity",
#     "chaos": "The guest is single-threaded"
#   },
#   "occasional": false
# }
# HERMIT_E2E_META_END
set -euo pipefail

case ${1:-} in
    --prepare)
        ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd)"
        cc -std=c11 -O2 -Wall -Wextra -Werror "$ROOT_DIR/tests/c/getpid.c" \
            -o "$E2E_FIXTURE_DIR/getpid"
        ;;
    --run) exec "$E2E_FIXTURE_DIR/getpid" ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
