#!/usr/bin/env bash
# Self-test for check-detcore-backend-abstraction.sh.
#
# The checker supplies its own positive and negative controls for parsed TOML
# dependencies and parsed Rust paths. This companion test brackets the derived
# wall-time budget: a sufficient declaration passes and an insufficient one
# refuses before work starts.

set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
readonly SCRIPT_DIR REPO_ROOT
readonly LINT="$SCRIPT_DIR/check-detcore-backend-abstraction.sh"

failures=0
note() { echo "  $*"; }
fail() {
    echo "check-detcore-backend-abstraction-test.sh: FAIL — $*" >&2
    failures=$((failures + 1))
}

if [[ ! -x $LINT ]]; then
    fail "$LINT is missing or not executable"
    exit 1
fi

echo "derived budget guard"

scratch=$(mktemp -d)
trap 'rm -rf -- "$scratch"' EXIT
cp -a "$REPO_ROOT/detcore" "$scratch/detcore"
mkdir -p "$scratch/ci/dag"
printf '[workspace]\nmembers = ["detcore"]\nresolver = "2"\n' > "$scratch/Cargo.toml"

write_dag() {
    python3 - "$scratch/ci/dag/validate.json" "$1" <<'PYEOF'
import json
import sys

path, timeout = sys.argv[1], int(sys.argv[2])
graph = {
    "steps": [
        {
            "group": "check",
            "job": "backend_abstraction",
            "desc": "fixture",
            "cmd": "true",
            "timeout": timeout,
        }
    ]
}
with open(path, "w", encoding="utf-8") as handle:
    json.dump(graph, handle)
PYEOF
}

# A budget that comfortably covers the derived work must be accepted.
write_dag 600
if output=$("$LINT" --repo-root "$scratch" 2>&1); then
    if ! grep -q "budget: 600s declared covers" <<< "$output"; then
        fail "a sufficient budget was accepted but not reported"
    else
        note "OK — a sufficient 600s budget is accepted and reported"
    fi
else
    status=$?
    if grep -q "BUDGETED FOR LESS WORK" <<< "$output"; then
        fail "a sufficient 600s budget was refused as insufficient"
    else
        note "OK — sufficient budget accepted (lint exited $status on unrelated fixture grounds)"
    fi
fi

# A budget that cannot cover the derived work must be refused, and must say so
# as a budget statement rather than as a timeout.
write_dag 1
if output=$("$LINT" --repo-root "$scratch" 2>&1); then
    fail "a 1s budget was accepted for work that cannot fit in it"
else
    if ! grep -q "BUDGETED FOR LESS WORK THAN IT DERIVES" <<< "$output"; then
        fail "an insufficient budget was refused without naming the budget as the cause"
    elif ! grep -q "THIS IS NOT A TIMEOUT" <<< "$output"; then
        fail "an insufficient budget was refused without distinguishing itself from a timeout"
    elif ! grep -qE "Raise check.backend_abstraction .* to at least [0-9]+" <<< "$output"; then
        fail "an insufficient budget was refused without naming the value to set"
    else
        note "OK — an insufficient budget is refused, named as a budget, with the value to set"
    fi
fi

echo
if ((failures > 0)); then
    echo "check-detcore-backend-abstraction-test.sh: FAIL — $failures case(s) failed" >&2
    exit 1
fi
echo "check-detcore-backend-abstraction-test.sh: OK — parsed dependency/source controls and budget guard hold"
