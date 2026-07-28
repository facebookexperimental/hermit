#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

fail() {
    printf 'FAIL [%s/%s]: %s\n' \
        "${SYSTEM_UTIL_TEST_NAME:-uninitialized}" \
        "${SYSTEM_UTIL_BACKEND:-uninitialized}" "$*" >&2
    exit 1
}

skip() {
    printf 'SKIP [%s/%s]: %s\n' \
        "${SYSTEM_UTIL_TEST_NAME:-uninitialized}" \
        "${SYSTEM_UTIL_BACKEND:-uninitialized}" "$*"
    exit 0
}

cleanup_system_utility_test() {
    if [[ -n ${SYSTEM_UTIL_WORKDIR:-} && -d $SYSTEM_UTIL_WORKDIR ]]; then
        rm -rf -- "$SYSTEM_UTIL_WORKDIR"
    fi
    if [[ -n ${SYSTEM_UTIL_GUEST_WORKDIR:-} && -d $SYSTEM_UTIL_GUEST_WORKDIR ]]; then
        rm -rf -- "$SYSTEM_UTIL_GUEST_WORKDIR"
    fi
}

backend_is_allowed() {
    local candidate
    for candidate in "${BACKEND_ALLOWLIST[@]}"; do
        if [[ $candidate == "$SYSTEM_UTIL_BACKEND" ]]; then
            return 0
        fi
    done
    return 1
}

init_system_utility_test() {
    local test_name=$1
    shift

    if (($# > 2)); then
        fail "usage: $0 [HERMIT_BIN [BACKEND]]"
    fi
    if ((${#BACKEND_ALLOWLIST[@]} == 0)); then
        fail "BACKEND_ALLOWLIST must name at least one backend"
    fi

    local script_dir repo_root hermit_bin requested_backend timeout_seconds
    script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[1]}")" && pwd)
    repo_root=$(cd -- "$script_dir/../../.." && pwd)
    hermit_bin=${1:-$repo_root/target/release/hermit}
    requested_backend=${2:-${SYSTEM_UTIL_BACKEND:-ptrace}}
    timeout_seconds=${SYSTEM_UTIL_TIMEOUT_SECONDS:-120}

    [[ $timeout_seconds =~ ^[1-9][0-9]*$ ]] \
        || fail "SYSTEM_UTIL_TIMEOUT_SECONDS must be a positive integer"

    readonly SYSTEM_UTIL_TEST_NAME=$test_name
    readonly SYSTEM_UTIL_BACKEND=$requested_backend
    readonly SYSTEM_UTIL_TIMEOUT_SECONDS=$timeout_seconds
    readonly HERMIT_BIN=$hermit_bin
    readonly SYSTEM_UTIL_REPO_ROOT=$repo_root

    backend_is_allowed || skip \
        "backend is outside allowlist: ${BACKEND_ALLOWLIST[*]}"
    [[ -x $HERMIT_BIN ]] || fail "Hermit binary is not executable: $HERMIT_BIN"
    command -v timeout >/dev/null || fail "required host command is missing: timeout"
    command -v sha256sum >/dev/null || fail "required host command is missing: sha256sum"

    export LC_ALL=C
    SYSTEM_UTIL_WORKDIR=$(mktemp -d \
        "${TMPDIR:-/tmp}/hermit-system-utils.${SYSTEM_UTIL_TEST_NAME}.XXXXXX")
    readonly SYSTEM_UTIL_WORKDIR
    mkdir -p "$SYSTEM_UTIL_REPO_ROOT/target"
    SYSTEM_UTIL_GUEST_WORKDIR=$(mktemp -d \
        "$SYSTEM_UTIL_REPO_ROOT/target/system-utils-e2e.${SYSTEM_UTIL_TEST_NAME}.XXXXXX")
    readonly SYSTEM_UTIL_GUEST_WORKDIR
    trap cleanup_system_utility_test EXIT
}

require_command() {
    local command_name=$1
    command -v "$command_name" >/dev/null \
        || skip "required utility is not installed: $command_name"
}

# Native observations are diagnostic rather than pass criteria. They show
# whether a value varies immediately or is merely tied to the current host.
observe_native() {
    local mode=$1
    shift
    local first=$SYSTEM_UTIL_WORKDIR/native.1
    local second=$SYSTEM_UTIL_WORKDIR/native.2
    local first_status second_status digest

    set +e
    "$@" >"$first" 2>&1
    first_status=$?
    set -e
    if ((first_status != 0)); then
        printf 'native-observation: command exited %d; Hermit probe continues\n' \
            "$first_status"
        return
    fi

    digest=$(sha256sum "$first" | awk '{print $1}')
    if [[ $mode == host-specific ]]; then
        printf 'native-observation: host-specific sha256=%s\n' "$digest"
        return
    fi

    sleep 0.05
    set +e
    "$@" >"$second" 2>&1
    second_status=$?
    set -e
    if ((second_status != 0)); then
        printf 'native-observation: second command exited %d\n' "$second_status"
        return
    fi

    if cmp -s "$first" "$second"; then
        printf 'native-observation: stable in this sample (sha256=%s)\n' "$digest"
        [[ $mode != must-vary ]] \
            || fail "native probe was expected to expose a changing value"
    else
        printf 'native-observation: output changed across two uncontained runs\n'
    fi
}

run_strict_verify() {
    (($# > 0)) || fail "run_strict_verify requires a guest command"

    STRICT_STDOUT=$SYSTEM_UTIL_WORKDIR/strict.stdout
    STRICT_STDERR=$SYSTEM_UTIL_WORKDIR/strict.stderr
    VERIFY_STDOUT=$SYSTEM_UTIL_WORKDIR/verify.stdout
    VERIFY_STDERR=$SYSTEM_UTIL_WORKDIR/verify.stderr
    readonly STRICT_STDOUT STRICT_STDERR VERIFY_STDOUT VERIFY_STDERR

    local status
    set +e
    timeout -k 5s "${SYSTEM_UTIL_TIMEOUT_SECONDS}s" \
        "$HERMIT_BIN" --log INFO run --backend "$SYSTEM_UTIL_BACKEND" \
        --strict -- "$@" >"$STRICT_STDOUT" 2>"$STRICT_STDERR"
    status=$?
    set -e
    if ((status != 0)); then
        tail -80 "$STRICT_STDERR" >&2
        fail "strict oracle run exited $status"
    fi

    set +e
    timeout -k 5s "${SYSTEM_UTIL_TIMEOUT_SECONDS}s" \
        "$HERMIT_BIN" --log INFO run --backend "$SYSTEM_UTIL_BACKEND" \
        --strict --verify -- "$@" >"$VERIFY_STDOUT" 2>"$VERIFY_STDERR"
    status=$?
    set -e
    if ((status != 0)); then
        tail -80 "$VERIFY_STDERR" >&2
        fail "strict verification exited $status"
    fi

    if ! grep -Fq 'Determinism verified' "$VERIFY_STDERR" \
        && ! grep -Fq 'KVM guest output and exit status matched' "$VERIFY_STDERR"; then
        cat "$VERIFY_STDOUT" >&2
        tail -80 "$VERIFY_STDERR" >&2
        fail "verification exited successfully without a determinism verdict"
    fi

    printf '%s strict stdout:\n' "$SYSTEM_UTIL_TEST_NAME"
    cat "$STRICT_STDOUT"
}

assert_stdout_exact() {
    local expected=$1
    local actual
    actual=$(cat "$STRICT_STDOUT")
    [[ $actual == "$expected" ]] || fail \
        "unexpected strict stdout; expected '$expected', got '$actual'"
}

assert_stdout_contains() {
    local expected=$1
    grep -Fq -- "$expected" "$STRICT_STDOUT" \
        || fail "strict stdout is missing: $expected"
}

assert_stdout_matches() {
    local pattern=$1
    grep -Eq -- "$pattern" "$STRICT_STDOUT" \
        || fail "strict stdout does not match: $pattern"
}

pass_test() {
    if [[ $SYSTEM_UTIL_BACKEND == kvm ]]; then
        printf 'PASS [%s/kvm]: strict --verify output/exit parity with INFO logging; internal trace comparison unavailable; relaxations=none\n' \
            "$SYSTEM_UTIL_TEST_NAME"
    else
        printf 'PASS [%s/%s]: L2 with INFO logging; relaxations=none\n' \
            "$SYSTEM_UTIL_TEST_NAME" "$SYSTEM_UTIL_BACKEND"
    fi
}

if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
    printf '%s is a library for the system utility e2e tests.\n' "$0" >&2
    exit 2
fi
