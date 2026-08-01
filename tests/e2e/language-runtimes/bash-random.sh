#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
set -euo pipefail

case ${1:-} in
    --prepare) command -v bash >/dev/null ;;
    --run)
        # Bash seeds $RANDOM from the process PID combined with the wall-clock
        # time, so the sequence varies between native runs. Hermit virtualizes
        # both the PID and time, so the seed -- and therefore the entire
        # sequence -- is deterministic and repeats bitwise under --strict.
        # This exercises a different determinism dependency than python-random
        # (getrandom/urandom) or random-device (kernel bytes): the bash PRNG's
        # PID+time seed.
        values=""
        for _ in {1..8}; do
            values+="$RANDOM "
        done
        printf 'RANDOM %s\n' "${values% }"
        ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
