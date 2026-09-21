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
# timer_delete, including supported SIGEV_SIGNAL delivery against virtual time,
# and periodic ITIMER_REAL delivery. This test guards both timer state and
# signal delivery under --strict, with no GLIBC_TUNABLES workaround.
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
repo_root="$(cd -- "$here/../.." && pwd)"
if [[ $hermit == */* ]]; then
    default_verification_report_bin="$(dirname -- "$hermit")/verification-report"
else
    default_verification_report_bin="$repo_root/target/debug/verification-report"
fi
VERIFICATION_REPORT_BIN=${VERIFICATION_REPORT_BIN:-$default_verification_report_bin}
state_src="$here/../c/timer_create_determinism.c"
signal_src="$here/../bin/posix_timer_test.c"
periodic_src="$here/../c/periodic_setitimer_delivery.c"

if [[ ! -x $VERIFICATION_REPORT_BIN ]]; then
    echo "FAIL: typed verification-report reader is not executable: $VERIFICATION_REPORT_BIN" >&2
    exit 1
fi

work=$(mktemp -d strict_timer_test_XXXXXXX)
function on_exit {
    rm -rf -- "$work"
}
trap on_exit EXIT

state_guest="$work/timer_state_guest"
signal_guest="$work/timer_signal_guest"
periodic_guest="$work/periodic_setitimer_guest"
# -lrt is a no-op on glibc >= 2.34 (timer_* live in libc) but keeps older glibc
# happy.
"$cc_bin" -O2 -o "$state_guest" "$state_src" -lrt
"$cc_bin" -std=c11 -O2 -Wall -Wextra -Werror -o "$signal_guest" "$signal_src" -lrt
"$cc_bin" -std=c11 -O2 -Wall -Wextra -Werror -o "$periodic_guest" "$periodic_src"

for guest in "$state_guest" "$signal_guest" "$periodic_guest"; do
    verify_report="$work/$(basename "$guest").verify.json"
    verify_output="$work/$(basename "$guest").verify.log"
    if ! "$hermit" run --strict --verify --verify-json "$verify_report" -- \
        "$guest" < /dev/null >"$verify_output" 2>&1; then
        echo "FAIL: $(basename "$guest") did not verify deterministic under --strict"
        tail -20 "$verify_output"
        exit 1
    fi
    if ! "$VERIFICATION_REPORT_BIN" matched "$verify_report"; then
        echo "FAIL: $(basename "$guest") typed verification report did not match"
        tail -20 "$verify_output"
        exit 1
    fi
    echo "ok: $(basename "$guest") verified deterministic under --strict"
done

echo "Test succeeded."
