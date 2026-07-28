#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

APPLICATION_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
readonly APPLICATION_DIR
REPO_ROOT=$(cd -- "$APPLICATION_DIR/../../.." && pwd)
readonly REPO_ROOT
readonly HERMIT_BIN=${HERMIT_BIN:-"$REPO_ROOT/target/debug/hermit"}
readonly HERMIT_APPLICATION_TIMEOUT=${HERMIT_APPLICATION_TIMEOUT:-120}

function require_commands {
    local command

    for command in "$@"; do
        if ! command -v "$command" >/dev/null 2>&1; then
            printf 'required application-test command not found: %s\n' "$command" >&2
            return 1
        fi
    done

    if [[ ! -x $HERMIT_BIN ]]; then
        printf 'Hermit binary not found or not executable: %s\n' "$HERMIT_BIN" >&2
        return 1
    fi
}

function assert_native_nondeterminism {
    local label=$1
    local first=$2
    local second=$3

    if [[ $first == "$second" ]]; then
        printf '%s native probes unexpectedly matched:\n%s\n' "$label" "$first" >&2
        return 1
    fi
}

function run_hermit_verify {
    local label=$1
    shift

    local stdout_file stderr_file status=0
    stdout_file=$(mktemp "${TMPDIR:-/tmp}/hermit-app-stdout.XXXXXX")
    stderr_file=$(mktemp "${TMPDIR:-/tmp}/hermit-app-stderr.XXXXXX")

    # Keep this invocation explicit: every application must exercise strict L2.
    timeout "$HERMIT_APPLICATION_TIMEOUT" \
        "$HERMIT_BIN" --log=info run --no-virtualize-cpuid \
        --max-timeslice=disabled --base-env=minimal --strict --verify -- \
        "$@" >"$stdout_file" 2>"$stderr_file" || status=$?

    if ((status != 0)); then
        printf '%s failed under Hermit (status %s)\nstdout:\n' "$label" "$status" >&2
        cat "$stdout_file" >&2
        printf 'stderr:\n' >&2
        cat "$stderr_file" >&2
        rm -f -- "$stdout_file" "$stderr_file"
        return "$status"
    fi

    if ! grep -Fq 'Success: deterministic. Determinism verified.' "$stderr_file"; then
        printf '%s did not report successful deterministic verification\nstderr:\n' "$label" >&2
        cat "$stderr_file" >&2
        rm -f -- "$stdout_file" "$stderr_file"
        return 1
    fi

    cat "$stdout_file"
    rm -f -- "$stdout_file" "$stderr_file"
}
