#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
set -euo pipefail

readonly ROOT="${BASH_SOURCE[0]%/*}/../../.."

case ${1:-} in
    --prepare) test -x /usr/bin/python3 ;;
    --run)
        # Progress markers go to stderr, which this cell does not observe (see
        # the manifest's observation.stderr = false), so they never affect the
        # verified stdout/status. Their sole purpose is to name the subcase that
        # was executing if the cell times out, instead of only the aggregate row.
        echo "kvm-python-examples: running examples/rand.py" >&2
        "$ROOT/examples/rand.py"
        # timed-progress-bar.py busy-waits on the deterministic clock, issuing
        # one guest clock_gettime per spin iteration (~585k for the default
        # 50-dot, 20ms bar). Under KVM every such read is a VM exit, so the full
        # bar cannot finish inside the cell's timeout. Render a much smaller bar
        # (5 dots x 5ms) here -- this cuts the syscall count ~40x while keeping
        # the clock fully fine-grained; it still verifies that a busy-wait on
        # virtual time is deterministic under KVM.
        echo "kvm-python-examples: running examples/timed-progress-bar.py 5 5" >&2
        exec "$ROOT/examples/timed-progress-bar.py" 5 5
        ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
