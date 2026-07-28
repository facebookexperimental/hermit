#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
# HERMIT_E2E_META_BEGIN
# {
#   "schema": 1,
#   "id": "system-utils/date-nanoseconds",
#   "category": "system-utils",
#   "description": "A valid wall-clock timestamp varies natively and repeats under Hermit",
#   "lane": "portable",
#   "requires": ["linux", "x86_64", "userns", "ptrace"],
#   "timeout_seconds": 60,
#   "observation": {"status": true, "stdout": true, "stderr": false, "artifacts": []},
#   "modes": {
#     "naked": {"runs": 3, "assert": {"min_distinct": 2}},
#     "verify": {"backends": ["ptrace"]}
#   },
#   "disabled_modes": {
#     "replay": "The date utility currently diverges while replaying timezone-file reads",
#     "chaos": "Single-threaded wall-clock probe has no scheduling oracle"
#   },
#   "occasional": false
# }
# HERMIT_E2E_META_END
set -euo pipefail

readonly TIMESTAMP_PATTERN='^[0-9]{4}-(0[1-9]|1[0-2])-(0[1-9]|[12][0-9]|3[01])_([01][0-9]|2[0-3]):[0-5][0-9]:[0-5][0-9]_[0-9]{9}$'

case ${1:-} in
    --prepare) exit 0 ;;
    --run)
        output=$("${BASH_SOURCE[0]%/*}/../../../examples/date.sh")
        if [[ ! $output =~ $TIMESTAMP_PATTERN ]]; then
            echo "invalid date example output: $output" >&2
            exit 1
        fi
        printf '%s\n' "$output"
        ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
