#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Regression test for the POSIX timer family under --strict.
#
# After the rseq + syscall-cascade fixes, CPython advanced through startup and
# then aborted under --strict (panic_on_unsupported_syscalls) on timer_create:
# it arms a long CLOCK_MONOTONIC watchdog via timer_create/timer_settime. A
# second blocker, membarrier, appeared right after.
#
# Detcore now emulates timer_create/timer_settime/timer_gettime/timer_getoverrun/
# timer_delete (arming tracked against the virtual clock; expiration signals are
# not delivered) and treats membarrier as a no-op (guest threads are serialized
# onto one logical CPU, so barriers are trivially satisfied). This test guards
# that a program using the timer family runs to completion AND verifies as
# deterministic under --strict, with no GLIBC_TUNABLES workaround.
#
# It compiles a tiny self-contained C guest so it does not depend on a system
# Python being present. If a C compiler is unavailable it is skipped.

set -euo pipefail

if [ "$*" == "" ]; then
    hermit="hermit"
else
    hermit="$1"
fi

unset GLIBC_TUNABLES || true

cc_bin="${CC:-cc}"
if ! command -v "$cc_bin" > /dev/null 2>&1; then
    echo "skip: no C compiler ($cc_bin) available to build the timer guest"
    exit 0
fi

here="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
src="$here/../c/timer_create_determinism.c"

work=$(mktemp -d strict_timer_test_XXXXXXX)
function on_exit {
    rm -rf -- "$work"
}
trap on_exit EXIT

guest="$work/timer_guest"
# -lrt is a no-op on glibc >= 2.34 (timer_* live in libc) but keeps older glibc
# happy.
"$cc_bin" -O2 -o "$guest" "$src" -lrt

if "$hermit" run --strict --verify -- "$guest" < /dev/null 2>&1 \
    | grep -q "Determinism verified"; then
    echo "ok: timer family verified deterministic under --strict"
else
    echo "FAIL: timer guest did not verify deterministic under --strict"
    "$hermit" run --strict --verify -- "$guest" < /dev/null 2>&1 | tail -20
    exit 1
fi

echo "Test succeeded."
