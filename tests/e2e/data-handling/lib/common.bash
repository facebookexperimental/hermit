# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

DATA_HANDLING_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
ROOT_DIR="$(cd -- "$DATA_HANDLING_DIR/../../.." && pwd)"
HERMIT_BIN=${HERMIT_BIN:-$ROOT_DIR/target/debug/hermit}
DATA_HANDLING_TIMEOUT=${DATA_HANDLING_TIMEOUT:-120s}
NATIVE_ATTEMPTS=${NATIVE_ATTEMPTS:-12}
NATIVE_RETRY_DELAY=${NATIVE_RETRY_DELAY:-0}
readonly DATA_HANDLING_DIR ROOT_DIR HERMIT_BIN DATA_HANDLING_TIMEOUT

function require_tools {
    local tool
    for tool in "$@"; do
        if ! command -v "$tool" >/dev/null 2>&1; then
            echo "required tool is unavailable: $tool" >&2
            return 1
        fi
    done
}

require_tools cat cut grep mktemp readlink rm sha256sum sleep timeout

function run_captured {
    local stdout=$1
    local stderr=$2
    shift 2

    timeout --foreground --kill-after=10s "$DATA_HANDLING_TIMEOUT" \
        "$@" >"$stdout" 2>"$stderr"
}

function output_digest {
    local stdout=$1
    local stderr=$2
    {
        printf 'stdout\0'
        cat "$stdout"
        printf '\0stderr\0'
        cat "$stderr"
    } | sha256sum | cut -d' ' -f1
}

function assert_nondeterministic_without_hermit {
    local label=$1
    shift
    local evidence
    local baseline_stdout
    local baseline_stderr
    local candidate_stdout
    local candidate_stderr
    local baseline_digest
    local candidate_digest
    local attempt

    evidence=$(mktemp -d "${TMPDIR:-/tmp}/hermit-data-native.XXXXXX")
    baseline_stdout=$evidence/baseline.stdout
    baseline_stderr=$evidence/baseline.stderr
    candidate_stdout=$evidence/candidate.stdout
    candidate_stderr=$evidence/candidate.stderr

    if ! run_captured "$baseline_stdout" "$baseline_stderr" "$@"; then
        echo "$label: naked baseline failed" >&2
        cat "$baseline_stderr" >&2
        rm -rf -- "$evidence"
        return 1
    fi
    baseline_digest=$(output_digest "$baseline_stdout" "$baseline_stderr")

    for ((attempt = 1; attempt <= NATIVE_ATTEMPTS; ++attempt)); do
        if [[ $NATIVE_RETRY_DELAY != 0 ]]; then
            sleep "$NATIVE_RETRY_DELAY"
        fi
        if ! run_captured "$candidate_stdout" "$candidate_stderr" "$@"; then
            echo "$label: naked candidate $attempt failed" >&2
            cat "$candidate_stderr" >&2
            rm -rf -- "$evidence"
            return 1
        fi
        candidate_digest=$(output_digest "$candidate_stdout" "$candidate_stderr")
        if [[ $candidate_digest != "$baseline_digest" ]]; then
            printf 'PASS naked nondeterminism: %s (%s != %s, attempt %d)\n' \
                "$label" "$baseline_digest" "$candidate_digest" "$attempt"
            rm -rf -- "$evidence"
            return 0
        fi
    done

    echo "$label: no naked output variation in $((NATIVE_ATTEMPTS + 1)) runs" >&2
    echo "digest: $baseline_digest" >&2
    rm -rf -- "$evidence"
    return 1
}

function assert_deterministic_with_hermit {
    local label=$1
    shift
    local evidence
    local stdout
    local stderr

    if [[ ! -x $HERMIT_BIN ]]; then
        echo "Hermit binary is not executable: $HERMIT_BIN" >&2
        return 1
    fi

    evidence=$(mktemp -d "${TMPDIR:-/tmp}/hermit-data-strict.XXXXXX")
    stdout=$evidence/stdout
    stderr=$evidence/stderr

    if ! run_captured "$stdout" "$stderr" \
        "$HERMIT_BIN" --log=info run \
        --strict --verify \
        --no-virtualize-cpuid --max-timeslice=disabled \
        -- "$@"; then
        echo "$label: strict Hermit verification failed" >&2
        cat "$stdout" >&2
        cat "$stderr" >&2
        rm -rf -- "$evidence"
        return 1
    fi
    if ! grep -Fq 'Determinism verified' "$stdout" "$stderr"; then
        echo "$label: Hermit exited without a determinism verdict" >&2
        cat "$stdout" >&2
        cat "$stderr" >&2
        rm -rf -- "$evidence"
        return 1
    fi

    printf 'PASS strict determinism: %s\n' "$label"
    rm -rf -- "$evidence"
}

function assert_nondeterminism_removed {
    local label=$1
    shift
    assert_nondeterministic_without_hermit "$label" "$@"
    assert_deterministic_with_hermit "$label" "$@"
}
