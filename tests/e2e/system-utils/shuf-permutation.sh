#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# End-to-end coreutils `shuf` determinism fixture.
#
# `shuf` produces a uniformly random permutation of its inputs. GNU coreutils
# seeds that permutation from kernel entropy: it draws bytes via getrandom(2)
# (falling back to /dev/urandom), so a fresh permutation is emitted on every
# native run. Both entropy channels are intercepted and determinized by Hermit,
# so under --strict the chosen permutation becomes a pure function of the
# deterministic guest state and repeats bitwise across runs. `shuf -e` takes
# the items directly as arguments, keeping this a single, fast process with no
# filesystem dependency (Hermit isolates the guest /tmp per repeat).
set -euo pipefail

case ${1:-} in
    --prepare)
        command -v shuf >/dev/null 2>&1 || {
            echo "shuf not found" >&2
            exit 1
        }
        exit 0
        ;;
    --run)
        exec shuf -e \
            alpha bravo charlie delta echo foxtrot golf hotel \
            india juliet kilo lima mike november oscar papa \
            quebec romeo sierra tango uniform victor whiskey xray \
            yankee zulu
        ;;
    *)
        echo "usage: $0 --prepare|--run" >&2
        exit 2
        ;;
esac
