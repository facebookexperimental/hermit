#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
# HERMIT_E2E_META_BEGIN
# {
#   "schema": 1,
#   "id": "applications/kvm-shell-environment",
#   "category": "applications",
#   "description": "KVM executes a shell workload with a deterministic environment",
#   "lane": "privileged",
#   "requires": ["linux", "x86_64", "kvm"],
#   "timeout_seconds": 60,
#   "observation": {"status": true, "stdout": true, "stderr": false, "artifacts": []},
#   "modes": {"verify": {"backends": ["kvm"]}},
#   "disabled_modes": {
#     "naked": "This is a privileged backend integration test",
#     "replay": "KVM has no record/replay runtime",
#     "chaos": "KVM does not provide Detcore schedule exploration"
#   },
#   "occasional": false
# }
# HERMIT_E2E_META_END
set -euo pipefail

case ${1:-} in
    --prepare) exit 0 ;;
    --run)
        test "$LC_ALL" = C
        test "$TZ" = UTC
        printf 'kvm-shell:%s:%s\n' "$LC_ALL" "$TZ"
        ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
