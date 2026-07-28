#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
# HERMIT_E2E_META_BEGIN
# {
#   "schema": 1,
#   "id": "applications/kvm-python-examples",
#   "category": "applications",
#   "description": "The user-facing Python examples run identically with the system interpreter under KVM",
#   "lane": "privileged",
#   "requires": ["linux", "x86_64", "kvm", "python3"],
#   "timeout_seconds": 120,
#   "observation": {"status": true, "stdout": true, "stderr": false, "artifacts": []},
#   "modes": {"verify": {"backends": ["kvm"]}},
#   "disabled_modes": {
#     "naked": "The portable Python random case already provides the native nondeterminism control",
#     "replay": "Record/replay is a ptrace-only mode",
#     "chaos": "Both examples are single-threaded"
#   },
#   "occasional": false
# }
# HERMIT_E2E_META_END
set -euo pipefail

readonly ROOT="${BASH_SOURCE[0]%/*}/../../.."

case ${1:-} in
    --prepare) test -x /usr/bin/python3 ;;
    --run)
        "$ROOT/examples/rand.py"
        exec "$ROOT/examples/timed-progress-bar.py"
        ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
