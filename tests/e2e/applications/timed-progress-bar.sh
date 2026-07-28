#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
# HERMIT_E2E_META_BEGIN
# {
#   "schema": 1,
#   "id": "applications/timed-progress-bar",
#   "category": "applications",
#   "description": "A time-driven Python application completes identically under verification",
#   "lane": "portable",
#   "requires": ["linux", "x86_64", "userns", "ptrace", "python3"],
#   "timeout_seconds": 120,
#   "observation": {"status": true, "stdout": true, "stderr": false, "artifacts": []},
#   "modes": {"verify": {"backends": ["ptrace"]}},
#   "disabled_modes": {
#     "naked": "Elapsed time drives progress but the final application output is intentionally stable",
#     "replay": "The shorter time and entropy cases provide the blocking replay sentinels",
#     "chaos": "The application is single-threaded"
#   },
#   "occasional": false
# }
# HERMIT_E2E_META_END
set -euo pipefail

case ${1:-} in
    --prepare) exit 0 ;;
    --run) exec "${BASH_SOURCE[0]%/*}/../../../examples/timed-progress-bar.py" ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
