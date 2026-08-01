#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# GNU awk's rand() is a pseudo-random generator that must be seeded before it
# produces varying output; an unseeded rand() is fixed. The idiomatic entropy
# source is srand() (time in seconds), but that has one-second granularity and
# would collide across the fast back-to-back native runs this corpus uses. So we
# seed from PROCINFO["pid"] instead: the process id is real per-run entropy
# natively, yet Hermit virtualizes pids to a deterministic value, so the seed --
# and therefore the whole rand() sequence -- repeats bitwise under Hermit. This
# is the first awk fixture in the corpus, exercising a distinct real interpreter
# (text-processing language) and its built-in PRNG. The interleaved arithmetic
# control (sumsq) and the toupper() control must stay byte-identical either way,
# isolating the pid-seeded rand() output as the sole field that varies natively.
# The whole program runs in one gawk process via BEGIN, so there is no file and
# no subprocess.
set -euo pipefail

prog='BEGIN {
    srand(PROCINFO["pid"])
    r = ""
    for (i = 0; i < 5; i++) r = r int(rand() * 1000000) ","
    s = 0
    for (i = 1; i <= 100; i++) s += i * i
    printf "rand=%s sumsq=%s ctl=%s\n", r, s, toupper("awk")
}'

case ${1:-} in
    --prepare) command -v gawk >/dev/null ;;
    --run) exec gawk "$prog" ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
