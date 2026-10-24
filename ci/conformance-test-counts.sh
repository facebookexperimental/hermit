#!/usr/bin/env bash
# Run the isolation matrix and publish its per-cell outcomes as the node's
# structured test results (DAGRUN_TEST_COUNTS_PATH), through
# ci/write-structured-test-counts.sh.
#
#   usage: conformance-test-counts.sh ARTIFACTS -- CMD...
#          conformance-test-counts.sh --self-test
#
# WHY: validate plans every test.* and *-test.* node as a test node, and a
# node counts as executed only through the structured result file it writes.
# The isolation matrix wrote none, so from 2026-10-15 every full validation
# listed its nodes as absent and none was canonically VALIDATED.
#
# CMD is tests/isolation/conformance.rs, which writes ARTIFACTS/results.tsv:
# one `id<TAB>pass|fail` row per collected cell and per stale ledger row,
# judged by its own expected-mismatch ledger.
#
# A missing or malformed results.tsv adds the failing row
# `isolation/incomplete`; a checker that exits nonzero with no failing row
# adds `isolation/checker-exit`. Either way the structured results never pass
# a node the checker failed. The exit status is the checker's, or the
# writer's when the checker passed.

set -uo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
readonly script_dir

function usage {
    echo "usage: $0 ARTIFACTS -- CMD... | --self-test" >&2
    exit 2
}

# rows_from_results RESULTS_TSV: prints `id<TAB>result` rows; status 1 if the
# file is missing or malformed.
function rows_from_results {
    local file=$1 line id result extra header_seen=false
    [[ -f $file ]] || return 1
    while IFS= read -r line || [[ -n $line ]]; do
        [[ $line == '#'* ]] && continue
        if ! $header_seen; then
            [[ $line == $'id\tresult' ]] || return 1
            header_seen=true
            continue
        fi
        IFS=$'\t' read -r id result extra <<<"$line"
        [[ -n $id && -z $extra && ( $result == pass || $result == fail ) ]] || return 1
        printf '%s\t%s\n' "$id" "$result"
    done <"$file"
    $header_seen
}

function main {
    local artifacts=${1:-}
    [[ -n $artifacts && ${2:-} == -- ]] || usage
    shift 2
    (($# > 0)) || usage

    local status=0 parsed parse_status=0
    "$@" || status=$?
    parsed=$(rows_from_results "$artifacts/results.tsv") || parse_status=$?

    local -a args=()
    local executed=0 failed=0 id result
    while IFS=$'\t' read -r id result; do
        [[ -n $id ]] || continue
        args+=("$id" "$result" 1)
        executed=$((executed + 1))
        [[ $result == fail ]] && failed=$((failed + 1))
    done <<<"$parsed"
    if ((parse_status != 0)); then
        echo "conformance-test-counts: $artifacts/results.tsv is missing or malformed; recording isolation/incomplete as failed" >&2
        args+=(isolation/incomplete fail 1)
        executed=$((executed + 1))
        failed=$((failed + 1))
        ((status != 0)) || status=1
    fi
    if ((status != 0 && failed == 0)); then
        args+=(isolation/checker-exit fail 1)
        executed=$((executed + 1))
    fi

    local count_status=0
    "$script_dir/write-structured-test-counts.sh" "$executed" 0 "${args[@]}" || count_status=$?
    echo "conformance-test-counts: $executed result rows, $failed failed, checker exit $status" >&2
    if ((status == 0 && count_status != 0)); then
        status=$count_status
    fi
    exit "$status"
}

function self_test {
    local scratch status=0 got rc
    scratch=$(mktemp -d) || return 1
    # One case: expected exit, expected counts file, then the arguments.
    function check_case {
        local name=$1 expected_rc=$2 expected=$3
        shift 3
        rc=0
        DAGRUN_TEST_COUNTS_PATH="$scratch/$name.json" "$0" "$@" >/dev/null 2>&1 || rc=$?
        got=$(cat "$scratch/$name.json" 2>/dev/null)
        if [[ $rc != "$expected_rc" || $got != "$expected" ]]; then
            echo "conformance-test-counts: self-test $name: exit $rc (want $expected_rc), counts $got" >&2
            echo "  want $expected" >&2
            status=1
        fi
    }
    local iso=$scratch/iso
    mkdir -p "$iso"
    printf '# schema=backend-isolation-results-v1\nid\tresult\nisolation/a\tpass\nisolation/b\tpass\n' >"$iso/results.tsv"
    check_case pass 0 \
        '{"executed_tests":2,"filtered_tests":0,"results":[{"attempts":1,"id":"isolation/a","result":"pass"},{"attempts":1,"id":"isolation/b","result":"pass"}],"schema":2}' \
        "$iso" -- true
    printf '# schema=backend-isolation-results-v1\nid\tresult\nisolation/a\tfail\nisolation-ledger/line-3/kvm\tfail\n' >"$iso/results.tsv"
    check_case fail 1 \
        '{"executed_tests":2,"filtered_tests":0,"results":[{"attempts":1,"id":"isolation/a","result":"fail"},{"attempts":1,"id":"isolation-ledger/line-3/kvm","result":"fail"}],"schema":2}' \
        "$iso" -- false
    printf '# schema=backend-isolation-results-v1\nid\tresult\nisolation/a\tpass\n' >"$iso/results.tsv"
    check_case exit-without-failing-row 2 \
        '{"executed_tests":2,"filtered_tests":0,"results":[{"attempts":1,"id":"isolation/a","result":"pass"},{"attempts":1,"id":"isolation/checker-exit","result":"fail"}],"schema":2}' \
        "$iso" -- sh -c 'exit 2'
    rm -f -- "$iso/results.tsv"
    check_case missing 1 \
        '{"executed_tests":1,"filtered_tests":0,"results":[{"attempts":1,"id":"isolation/incomplete","result":"fail"}],"schema":2}' \
        "$iso" -- true
    printf 'id\tresult\nisolation/a\tmaybe\n' >"$iso/results.tsv"
    check_case malformed 1 \
        '{"executed_tests":1,"filtered_tests":0,"results":[{"attempts":1,"id":"isolation/incomplete","result":"fail"}],"schema":2}' \
        "$iso" -- true
    printf 'isolation/a\tpass\n' >"$iso/results.tsv"
    check_case no-header 1 \
        '{"executed_tests":1,"filtered_tests":0,"results":[{"attempts":1,"id":"isolation/incomplete","result":"fail"}],"schema":2}' \
        "$iso" -- true

    rm -rf -- "$scratch"
    if ((status != 0)); then
        echo "conformance-test-counts: self-test FAILED" >&2
        return 1
    fi
    echo "conformance-test-counts: self-test PASS (pass, fail, exit without failing row, missing, malformed, no header)"
}

if [[ ${1:-} == --self-test ]]; then
    self_test
    exit
fi
main "$@"
