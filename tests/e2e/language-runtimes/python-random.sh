#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
# HERMIT_E2E_META_BEGIN
# {
#   "schema": 1,
#   "id": "language-runtimes/python-random",
#   "category": "language-runtimes",
#   "description": "Python random output varies natively and repeats under Hermit",
#   "lane": "portable",
#   "requires": ["linux", "x86_64", "userns", "ptrace", "python3"],
#   "timeout_seconds": 90,
#   "observation": {"status": true, "stdout": true, "stderr": false, "artifacts": []},
#   "modes": {
#     "naked": {"runs": 3, "assert": {"min_distinct": 2}},
#     "verify": {"backends": ["ptrace"]}
#   },
#   "disabled_modes": {
#     "replay": "The focused C sentinel provides the blocking replay contract",
#     "chaos": "The Python example is single-threaded"
#   },
#   "occasional": false
# }
# HERMIT_E2E_META_END
set -euo pipefail

case ${1:-} in
    --prepare) exit 0 ;;
    --run) exec "${BASH_SOURCE[0]%/*}/../../../examples/rand.py" ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
