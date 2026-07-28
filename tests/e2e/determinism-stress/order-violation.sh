#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
# HERMIT_E2E_META_BEGIN
# {
#   "schema": 1,
#   "id": "determinism-stress/order-violation",
#   "category": "determinism-stress",
#   "description": "Different chaos seeds expose distinct reproducible thread schedules",
#   "lane": "portable",
#   "requires": ["linux", "x86_64", "userns", "ptrace", "cc"],
#   "timeout_seconds": 90,
#   "observation": {"status": true, "stdout": true, "stderr": false, "artifacts": []},
#   "modes": {
#     "verify": {"backends": ["ptrace"]},
#     "chaos": {
#       "backends": ["ptrace"],
#       "seeds": [0, 1],
#       "assert": {"min_distinct": 2, "min_passes": 1, "min_failures": 1}
#     }
#   },
#   "disabled_modes": {
#     "naked": "Native scheduling does not guarantee both outcomes within a fixed budget",
#     "replay": "Chaos witness reproduction is the required relation for this deliberately racy guest"
#   },
#   "occasional": false
# }
# HERMIT_E2E_META_END
set -euo pipefail

case ${1:-} in
    --prepare)
        ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd)"
        cc -std=c11 -O2 -pthread "$ROOT_DIR/tests/chaos/order_violation.c" \
            -o "$E2E_FIXTURE_DIR/order-violation"
        ;;
    --run) exec "$E2E_FIXTURE_DIR/order-violation" ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
