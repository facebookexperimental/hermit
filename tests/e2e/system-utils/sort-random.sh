#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# coreutils `sort -R` hashes each input line with a random key seeded from the
# kernel CSPRNG (getrandom(2), falling back to /dev/urandom), so the emitted
# permutation varies natively run to run but is determinized by Hermit. This is
# a distinct real tool from shuf-permutation, exercising sort's random-key
# ordering path. The fixed word list is written under $E2E_TMPDIR (Hermit
# isolates /tmp per repeat) and sort runs as a single process reading it.
set -euo pipefail

case ${1:-} in
    --prepare) command -v sort >/dev/null ;;
    --run)
        work="${E2E_TMPDIR:-/tmp}/hermit-sort-random"
        rm -rf "$work"
        mkdir -p "$work"
        printf '%s\n' alpha bravo charlie delta echo foxtrot golf hotel \
            india juliett kilo lima mike november oscar papa quebec romeo \
            sierra tango uniform victor whiskey xray yankee zulu >"$work/words.txt"
        exec sort -R "$work/words.txt"
        ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
