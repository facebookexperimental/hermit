#!/usr/bin/env bash

set -uo pipefail

function emit_libtest_count {
    local log=$1 line runs='' passed='' skipped=0 matches=0
    while IFS= read -r line; do
        if [[ $line =~ Summary.*\][[:space:]]+([0-9]+)[[:space:]]+tests?[[:space:]]+run:[[:space:]]+([0-9]+)[[:space:]]+passed(,[[:space:]]+([0-9]+)[[:space:]]+skipped)?$ ]]; then
            runs=${BASH_REMATCH[1]}
            passed=${BASH_REMATCH[2]}
            skipped=${BASH_REMATCH[4]:-0}
            ((matches += 1))
        fi
    done <"$log"

    if ((matches != 1)); then
        printf 'run-nextest-counted: expected exactly one final nextest Summary, found %s\n' "$matches" >&2
        return 2
    fi
    if ((runs != passed)); then
        printf 'run-nextest-counted: successful nextest summary disagrees: %s run, %s passed\n' \
            "$runs" "$passed" >&2
        return 2
    fi

    # safe-ci-dag-runner consumes canonical libtest counts from complete step
    # output. Nextest's human summary is equally authoritative but has a
    # different spelling, so restate that one parsed summary without guessing.
    printf 'running %s tests\n' "$runs"
    printf 'test result: ok. %s passed; 0 failed; 0 ignored; %s filtered out\n' \
        "$passed" "$skipped"
}

function self_test {
    local scratch got expected status=0
    scratch=$(mktemp -d)
    trap 'rm -rf "$scratch"' RETURN

    printf 'Summary [  20.921s] 8 tests run: 8 passed, 7 skipped\n' >"$scratch/with-skips"
    got=$(emit_libtest_count "$scratch/with-skips")
    expected=$'running 8 tests\ntest result: ok. 8 passed; 0 failed; 0 ignored; 7 filtered out'
    [[ $got == "$expected" ]] || return 1

    printf 'Summary [   0.003s] 1 test run: 1 passed\n' >"$scratch/no-skips"
    got=$(emit_libtest_count "$scratch/no-skips")
    expected=$'running 1 tests\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out'
    [[ $got == "$expected" ]] || return 1

    printf 'not a nextest summary\n' >"$scratch/missing"
    emit_libtest_count "$scratch/missing" >/dev/null 2>&1 || status=$?
    [[ $status == 2 ]] || return 1
    status=0
    printf 'Summary [   0.003s] 2 tests run: 1 passed\n' >"$scratch/mismatch"
    emit_libtest_count "$scratch/mismatch" >/dev/null 2>&1 || status=$?
    [[ $status == 2 ]] || return 1

    printf 'run-nextest-counted: self-test PASS (2 positive, 2 refusal)\n'
}

if [[ ${1:-} == --self-test ]]; then
    self_test
    exit
fi

summary_log=$(mktemp)
trap 'rm -f "$summary_log"' EXIT

set +e
cargo nextest run --color never "$@" 2>&1 | tee "$summary_log"
status=${PIPESTATUS[0]}
set -e

if ((status == 0)); then
    emit_libtest_count "$summary_log" || exit $?
fi
exit "$status"
