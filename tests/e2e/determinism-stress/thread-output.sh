#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
# HERMIT_E2E_META_BEGIN
# {
#   "schema": 1,
#   "id": "determinism-stress/thread-output",
#   "category": "determinism-stress",
#   "description": "Concurrent shell writers produce repeatable output under strict verification",
#   "lane": "portable",
#   "requires": ["linux", "x86_64", "userns", "ptrace"],
#   "timeout_seconds": 90,
#   "observation": {"status": true, "stdout": true, "stderr": false, "artifacts": []},
#   "modes": {"verify": {"backends": ["ptrace"]}},
#   "disabled_modes": {
#     "naked": "Native ordering can vary but is not guaranteed to diverge in a bounded run",
#     "replay": "Strict verification already covers this example in the required lane",
#     "chaos": "The focused order-violation test provides a stronger seeded oracle"
#   },
#   "occasional": false
# }
# HERMIT_E2E_META_END
set -euo pipefail

case ${1:-} in
    --prepare) exit 0 ;;
    --run) exec "${BASH_SOURCE[0]%/*}/../../../examples/race.sh" ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
