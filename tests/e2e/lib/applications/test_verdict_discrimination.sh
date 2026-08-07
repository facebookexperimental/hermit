#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# Bracket `run_hermit_verify`'s verdict reading from BOTH sides.
#
# The bug this guards against is not "verification fails"; it is verification
# reporting PASS for work it never did. The previous helper decided success by
# grepping stderr for a banner while running bare `--verify` (the lossy Stripped
# policy), so a stripped match, and anything that merely printed the banner,
# read as strict L2.
#
# These cases drive the reader with SYNTHETIC `--verify-json` reports via a fake
# Hermit, so each outcome is exercised deterministically and without needing a
# real guest. The positive case is what keeps the rest honest: a reader that
# rejected everything would satisfy every negative below and be useless.

set -euo pipefail

HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
WORK=$(mktemp -d "${TMPDIR:-/tmp}/hermit-verdict-test.XXXXXX")
trap 'rm -rf -- "$WORK"' EXIT

failures=0

# A fake Hermit that writes $FAKE_REPORT (if set) to the --verify-json path and
# exits $FAKE_STATUS. Nothing here depends on a real guest or a real comparison.
cat >"$WORK/fake-hermit" <<'FAKE'
#!/usr/bin/env bash
set -uo pipefail
verdict_path=""
prev=""
for arg in "$@"; do
    [[ $prev == --verify-json ]] && verdict_path=$arg
    prev=$arg
done
if [[ -n ${FAKE_REPORT:-} && -n $verdict_path ]]; then
    printf '%s' "$FAKE_REPORT" >"$verdict_path"
fi
printf 'fake-guest-stdout\n'
printf 'Success: deterministic. Determinism verified.\n' >&2
exit "${FAKE_STATUS:-0}"
FAKE
chmod +x "$WORK/fake-hermit"

# `common.sh` resolves HERMIT_BIN at source time, so it is exported first.
export HERMIT_BIN="$WORK/fake-hermit"
export HERMIT_APPLICATION_TIMEOUT=30
# shellcheck source=/dev/null
source "$HERE/common.sh"

function expect {
    local name=$1 want=$2 report=$3 fake_status=$4
    local out rc=0
    # Exported, not prefixed: `VAR=x out=$(cmd)` is an assignment statement, so
    # the prefix never reaches the command substitution's environment.
    export FAKE_REPORT="$report" FAKE_STATUS="$fake_status"
    out=$(run_hermit_verify "$name" /bin/true 2>&1) || rc=$?
    unset FAKE_REPORT FAKE_STATUS

    if [[ $want == PASS ]]; then
        if ((rc == 0)); then
            printf '  ok   %-28s PASS as expected\n' "$name"
        else
            printf '  FAIL %-28s expected PASS, got rc=%s:\n%s\n' "$name" "$rc" "$out"
            failures=$((failures + 1))
        fi
        return
    fi

    if ((rc == 0)); then
        printf '  FAIL %-28s expected refusal (%s) but it PASSED\n' "$name" "$want"
        failures=$((failures + 1))
    elif [[ $out != *"$want"* ]]; then
        printf '  FAIL %-28s expected reason %s, got:\n%s\n' "$name" "$want" "$out"
        failures=$((failures + 1))
    else
        printf '  ok   %-28s refused: %s\n' "$name" "$want"
    fi
}

parity_report='{"verified":true,"bitwise_parity":true,"verdict":"matched","comparison":{"strictness":"canonical"},"compared_log_messages":{"left":1200,"right":1200},"guest_exit_code":0,"guest_signal":null}'

printf 'run_hermit_verify verdict discrimination\n'

# POSITIVE. Without this the negatives prove nothing: a reader that always
# refused would pass every one of them.
expect strict-parity PASS "$parity_report" 0

# The original defect, planted exactly. A stripped match sets verified=true and
# the fake still prints the old success banner, so the previous banner-grep
# implementation accepted this. It is NOT L2.
expect stripped-match DIVERGED \
    '{"verified":true,"bitwise_parity":false,"verdict":"matched","comparison":{"strictness":"stripped"},"compared_log_messages":{"left":1200,"right":1200},"guest_exit_code":0,"guest_signal":null}' 0

# A strict CONFIGURATION that compared nothing. Parity over an empty selection
# is vacuous; this is the "ok with zero executed tests" failure.
expect zero-compared NO-RESULT \
    '{"verified":true,"bitwise_parity":true,"verdict":"matched","comparison":{"strictness":"canonical"},"compared_log_messages":{"left":0,"right":0},"guest_exit_code":0,"guest_signal":null}' 0

# Log comparison never ran at all (output-only fallback): null, not zero.
expect null-compared NO-RESULT \
    '{"verified":true,"bitwise_parity":true,"verdict":"matched","comparison":null,"compared_log_messages":null,"guest_exit_code":0,"guest_signal":null}' 0

# Hermit's own pre-run stamp, left behind by an early abort.
expect no-result-stamp NO-RESULT \
    '{"verified":false,"bitwise_parity":false,"verdict":"no_result","comparison":null,"compared_log_messages":null,"guest_exit_code":null,"guest_signal":null}' 1

# A real comparison that failed.
expect diverged DIVERGED \
    '{"verified":false,"bitwise_parity":false,"verdict":"diverged","comparison":{"strictness":"canonical"},"compared_log_messages":{"left":1200,"right":1199},"guest_exit_code":0,"guest_signal":null}' 1

# Launch refusal: no report written at all. Distinct from a no-result report.
expect launch-refusal REFUSED '' 1

# Malformed report must not be read as anything.
expect malformed-json NO-RESULT 'not json at all' 0

printf '\n'
if ((failures != 0)); then
    printf '%s case(s) FAILED\n' "$failures" >&2
    exit 1
fi
printf 'all verdict-discrimination cases passed\n'
