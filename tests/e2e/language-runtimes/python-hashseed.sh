#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# CPython randomizes string/bytes hashing per interpreter start (PYTHONHASHSEED),
# seeding _Py_HashSecret from the kernel CSPRNG (getrandom(2), /dev/urandom
# fallback) at init. This is the canonical Python reproducibility pitfall: the
# value of hash("...") AND the iteration order of a set of strings both vary run
# to run natively, yet Hermit determinizes them by virtualizing that entropy
# channel. It is a distinct nondeterminism source from the `random` module
# covered by python-random (Mersenne Twister) -- here it is the interpreter's own
# hash seed. The interleaved arithmetic control (`ctl`) must stay byte-identical
# either way. The program is passed inline with `-c`, so this is a single fast
# process with no file. getrandom entropy is unique per run, so the native
# control never collides the way a 1-second-granularity clock channel would.
set -euo pipefail

prog='s = {"alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf"}'
prog+='
ctl = sum(i * i for i in range(1, 101))
print("order=" + ",".join(s) + " hash=" + str(hash("hermit-determinism-corpus")) + " ctl=" + str(ctl))'

case ${1:-} in
    --prepare) command -v python3 >/dev/null ;;
    --run) exec python3 -c "$prog" ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
