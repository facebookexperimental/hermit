#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
# HERMIT_E2E_META_BEGIN
# {
#   "schema": 1,
#   "id": "determinism-stress/process-chains",
#   "category": "determinism-stress",
#   "description": "A 13-process fork tree and five-stage C pipe chain repeat under strict verification",
#   "lane": "portable",
#   "requires": ["linux", "x86_64", "userns", "ptrace", "cc"],
#   "timeout_seconds": 120,
#   "observation": {"status": true, "stdout": true, "stderr": false, "artifacts": []},
#   "modes": {"verify": {"backends": ["ptrace"]}},
#   "disabled_modes": {
#     "naked": "Both programs self-check deterministic invariants rather than requiring native output variation",
#     "replay": "The focused C replay sentinel owns the blocking record/replay contract",
#     "chaos": "The order-violation test provides the seeded schedule-diversity oracle"
#   },
#   "occasional": false
# }
# HERMIT_E2E_META_END
set -euo pipefail

case ${1:-} in
    --prepare)
        ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd)"
        cc -std=c11 -O2 -g -Wall -Wextra -Werror -pthread \
            "$ROOT_DIR/tests/e2e/determinism-stress/fork_tree.c" \
            -o "$E2E_FIXTURE_DIR/fork-tree"
        cc -std=c11 -O2 -g -Wall -Wextra -Werror -pthread \
            "$ROOT_DIR/tests/e2e/determinism-stress/pipe_chain.c" \
            -o "$E2E_FIXTURE_DIR/pipe-chain"
        ;;
    --run)
        "$E2E_FIXTURE_DIR/fork-tree"
        exec "$E2E_FIXTURE_DIR/pipe-chain"
        ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
