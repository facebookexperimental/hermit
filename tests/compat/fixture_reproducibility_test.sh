#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Issue 4997 regression test: the compat fixtures are a pure function of the
# builder. Prepare them N times (a second preparation stands in for the
# host/pinned twin writers of one validation), require the SHA256SUMS
# manifest to be identical every time, and, when a test-harness binary is
# available, run the gcov and cargo replay cells after each preparation and
# require PASS. A cell that sees different inputs on a later run fails here
# before it can flake a validation.

set -euo pipefail

N=${1:-3}
ROOT="$PWD/ignored/fixture-repro-test"
rm -rf "$ROOT"
mkdir -p "$ROOT"
trap 'if [[ -f "$ROOT/PASS" ]]; then rm -rf "$ROOT"; else echo "kept failing run at $ROOT" >&2; fi' EXIT
STATE="$ROOT/state"
FIXTURES="$STATE/strict-compat/real-compat-fixtures"
# Deliberately no pre-created strict-compat directory: preparation must
# work on a fresh run state, lock file first.

bash "$PWD/tests/compat/prepare_real_compat_fixtures.sh" "$FIXTURES" >/dev/null
cp "$FIXTURES/SHA256SUMS" "$ROOT/expected.sums"

for ((i = 1; i <= N; i++)); do
    bash "$PWD/tests/compat/prepare_real_compat_fixtures.sh" "$FIXTURES" >/dev/null
    if ! diff -u "$ROOT/expected.sums" "$FIXTURES/SHA256SUMS" >"$ROOT/diff.txt"; then
        echo "FAIL: fixture inputs changed on preparation $i" >&2
        cat "$ROOT/diff.txt" >&2
        exit 1
    fi
    if [[ -x "$PWD/target/debug/test-harness" && -x "$PWD/target/debug/hermit" ]]; then
        for t in gcov cargo; do
            mkdir -p "$ROOT/cell-$t-$i"
            VALIDATE_RUN_STATE="$STATE" HERMIT_BIN="$PWD/target/debug/hermit" \
            E2E_RESULT_ROOT="$ROOT/cell-$t-$i" E2E_RUN_ID="fixture-repro-$t-$i" \
                "$PWD/target/debug/test-harness" run --lane portable \
                --category compat --ci-only --test "compat/$t" --mode replay \
                --backend ptrace --results "$ROOT/cell-$t-$i/results.jsonl" \
                >"$ROOT/cell-$t-$i.log" 2>&1
            grep -q "^PASS compat/$t (replay/ptrace)$" "$ROOT/cell-$t-$i.log" || {
                echo "FAIL: compat/$t replay/ptrace not PASS on run $i" >&2
                tail -n 20 "$ROOT/cell-$t-$i.log" >&2
                exit 1
            }
        done
    fi
done
touch "$ROOT/PASS"
echo "PASS: $N preparations produced identical fixture inputs"
