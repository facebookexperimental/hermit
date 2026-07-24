#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Characterization test for setitimer(ITIMER_REAL) + sigaction(SIGALRM) under
# --strict.
#
# GAP (as of this commit): Detcore has no dedicated setitimer/getitimer handler.
# The syscall is not rejected, but ITIMER_REAL expiration signals (SIGALRM) are
# NOT delivered against the virtual clock -- the same limitation Detcore
# documents for the POSIX timer_create family. Consequences measured with
# tests/c/setitimer_determinism.c:
#
#   native            : SIGALRM deliveries >= 1   (timer fires)
#   hermit --strict   : SIGALRM deliveries == 0   (timer silently never fires)
#   hermit --verify   : "Determinism verified"    (the zero-delivery path is
#                                                   itself deterministic)
#
# So this is a functional-completeness gap, NOT a nondeterminism bug: setitimer
# reproducibly fails to fire rather than firing at an uncontrolled host moment.
#
# This script documents that contract. It is a "known gap" characterization:
#   - It PASSES (exit 0) as long as the --strict run is deterministic
#     (`--verify` succeeds), which is the invariant Hermit must never regress.
#   - It prints EXPECTED-FAIL and the native-vs-strict delivery counts to make
#     the missing-handler gap visible.
#   - When a real virtual-clock setitimer handler lands, flip STRICT_EXPECT_FIRE
#     below to 1: the script will then REQUIRE deterministic, non-zero, matching
#     deliveries and fail loudly if the timer regresses to not firing.
#
# It compiles a tiny self-contained C guest, so it does not depend on any
# system interpreter. If a C compiler is unavailable it is skipped.

set -euo pipefail

# When 0 (today): document the known gap; PASS if the run is deterministic.
# When 1 (after a setitimer handler lands): REQUIRE deterministic firing.
STRICT_EXPECT_FIRE="${STRICT_EXPECT_FIRE:-0}"

if [ "$*" == "" ]; then
    hermit="hermit"
else
    hermit="$1"
fi

unset GLIBC_TUNABLES || true

cc_bin="${CC:-cc}"
if ! command -v "$cc_bin" > /dev/null 2>&1; then
    echo "skip: no C compiler ($cc_bin) available to build the setitimer guest"
    exit 0
fi

here="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
src="$here/../c/setitimer_determinism.c"

work=$(mktemp -d strict_setitimer_test_XXXXXXX)
function on_exit {
    rm -rf -- "$work"
}
trap on_exit EXIT

guest="$work/setitimer_guest"
"$cc_bin" -O2 -o "$guest" "$src"

# Native reference: the timer must actually fire outside Hermit.
native_out="$("$guest" < /dev/null 2>&1 || true)"
native_count="$(printf '%s\n' "$native_out" | sed -n 's/^SIGALRM deliveries: \([0-9]*\)$/\1/p' | tail -1)"
native_count="${native_count:-0}"
echo "native: SIGALRM deliveries = $native_count"
if [ "$native_count" -lt 1 ]; then
    echo "FAIL: native run did not deliver any SIGALRM; test environment broken"
    exit 1
fi

# Plain --strict run to read the guest's delivery count. (In --verify mode the
# guest's stdout is captured for run-to-run comparison and not echoed, so the
# count is read here from a normal --strict run.)
strict_out="$("$hermit" run --strict -- "$guest" < /dev/null 2>&1 || true)"
strict_count="$(printf '%s\n' "$strict_out" | sed -n 's/^SIGALRM deliveries: \([0-9]*\)$/\1/p' | tail -1)"
strict_count="${strict_count:-<none>}"

# --strict --verify for the determinism verdict.
verify_out="$("$hermit" run --strict --verify -- "$guest" < /dev/null 2>&1 || true)"
if printf '%s\n' "$verify_out" | grep -q "Determinism verified"; then
    deterministic=1
else
    deterministic=0
fi

echo "hermit --strict: SIGALRM deliveries = $strict_count"
echo "hermit --strict --verify: deterministic=$deterministic"

if [ "$STRICT_EXPECT_FIRE" = "1" ]; then
    # Post-fix contract: must be deterministic AND actually fire.
    if [ "$deterministic" = "1" ] && [ "$strict_count" != "<none>" ] && [ "$strict_count" -ge 1 ]; then
        echo "ok: setitimer fires deterministically under --strict ($strict_count deliveries)"
        echo "Test succeeded."
        exit 0
    fi
    echo "FAIL: expected deterministic, non-zero setitimer deliveries under --strict"
    printf '%s\n' "$verify_out" | tail -20
    exit 1
fi

# Today's contract: the run must be deterministic; the timer is expected NOT to
# fire (documented gap).
if [ "$deterministic" != "1" ]; then
    echo "FAIL: setitimer guest was NONDETERMINISTIC under --strict (unexpected)"
    printf '%s\n' "$verify_out" | tail -20
    exit 1
fi

if [ "$strict_count" = "0" ]; then
    echo "EXPECTED-FAIL (known gap): setitimer(ITIMER_REAL) is deterministic under"
    echo "  --strict but SIGALRM is never delivered (0 vs native $native_count)."
    echo "  Detcore has no virtual-clock setitimer handler; expiration signals are"
    echo "  not delivered (cf. the timer_create family). Set STRICT_EXPECT_FIRE=1"
    echo "  once a handler lands to convert this into a firing assertion."
    echo "Test succeeded (documented gap)."
    exit 0
fi

# Deterministic AND firing already: the gap has been fixed; nudge the operator.
echo "NOTE: setitimer already fires deterministically under --strict"
echo "  ($strict_count deliveries). The gap appears fixed -- set STRICT_EXPECT_FIRE=1"
echo "  to enforce this going forward."
echo "Test succeeded."
exit 0
