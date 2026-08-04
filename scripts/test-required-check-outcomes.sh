#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
CLASSIFIER="$ROOT_DIR/scripts/classify-required-check.sh"

# N=2 legitimate GitHub pass representations remain PASSED.
[[ $("$CLASSIFIER" completed success) == PASSED ]]
[[ $("$CLASSIFIER" "" success) == PASSED ]]

# N=4 conclusions contain a genuine failed result.
for conclusion in failure timed_out error startup_failure; do
    [[ $("$CLASSIFIER" completed "$conclusion") == FAILED ]] || exit 1
done

# N=12 terminal, active, absent, and unknown representations have NO_RESULT.
for state in \
    completed:cancelled completed:skipped completed:neutral \
    completed:stale completed:action_required queued:none \
    in_progress:none waiting:none requested:none pending:none missing:none \
    completed:future_state; do
    status=${state%%:*}
    conclusion=${state#*:}
    [[ $conclusion != none ]] || conclusion=
    [[ $("$CLASSIFIER" "$status" "$conclusion") == NO_RESULT ]] || exit 1
done

echo "PASS: N=2 passed, N=4 failed, N=12 no-result"
