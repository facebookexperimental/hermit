#!/usr/bin/env bash

set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
readonly script_dir
readonly TEST_RESULTS_WRITER="$script_dir/structured-test-results.rs"

function usage {
    echo "usage: $0 <executed-tests> <filtered-tests> [<id> <pass|fail> <attempts>]..." >&2
    exit 2
}

function write_structured_test_results {
    (( $# >= 2 )) || usage
    local executed=$1 filtered=$2 path=${DAGRUN_TEST_COUNTS_PATH:-}
    shift 2
    [[ $executed =~ ^[0-9]+$ && $filtered =~ ^[0-9]+$ ]] || usage
    (( $# % 3 == 0 && $# / 3 == executed )) || usage

    local id index
    local -a rows=("$@")
    declare -A seen=()
    for ((index = 0; index < ${#rows[@]}; index += 3)); do
        id=${rows[index]}
        [[ -n $id ]] || continue
        [[ -z ${seen["$id"]+present} ]] || usage
        seen["$id"]=1
    done
    [[ -n $path ]] || return 0

    "$TEST_RESULTS_WRITER" write "$path" "$executed" "$filtered" "${rows[@]}"
}

function self_test {
    local scratch output no_channel_status=0 status=0
    scratch=$(mktemp -d)
    # shellcheck disable=SC2016 # `$` is part of the stable test identity.
    DAGRUN_TEST_COUNTS_PATH="$scratch/counts.json" \
        write_structured_test_results 2 2 \
            'suite$passes' pass 1 'suite$fails' fail 2
    # shellcheck disable=SC2016 # `$` is part of the expected JSON string.
    [[ $(<"$scratch/counts.json") == \
        '{"executed_tests":2,"filtered_tests":2,"results":[{"attempts":1,"id":"suite$passes","result":"pass"},{"attempts":2,"id":"suite$fails","result":"fail"}],"schema":2}' ]] || status=1
    output=$(unset DAGRUN_TEST_COUNTS_PATH; PATH="$scratch/no-rust-script" \
        write_structured_test_results 1 0 anything pass 1) \
        || status=1
    [[ -z $output ]] || status=1
    (unset DAGRUN_TEST_COUNTS_PATH; write_structured_test_results invalid 0) \
        >/dev/null 2>&1 || no_channel_status=$?
    [[ $no_channel_status == 2 ]] || status=1
    no_channel_status=0
    (unset DAGRUN_TEST_COUNTS_PATH; write_structured_test_results 2 0 only-one pass 1) \
        >/dev/null 2>&1 || no_channel_status=$?
    [[ $no_channel_status == 2 ]] || status=1
    no_channel_status=0
    (unset DAGRUN_TEST_COUNTS_PATH; write_structured_test_results \
        2 0 duplicate pass 1 duplicate fail 1) \
        >/dev/null 2>&1 || no_channel_status=$?
    [[ $no_channel_status == 2 ]] || status=1
    (DAGRUN_TEST_COUNTS_PATH="$scratch/invalid.json" \
        write_structured_test_results invalid 0) >/dev/null 2>&1 && status=1
    (DAGRUN_TEST_COUNTS_PATH="$scratch/invalid.json" \
        write_structured_test_results 2 0 only-one pass 1) >/dev/null 2>&1 && status=1
    (DAGRUN_TEST_COUNTS_PATH="$scratch/invalid.json" \
        write_structured_test_results 1 0 ' leading' pass 1) \
        >/dev/null 2>&1 && status=1
    (DAGRUN_TEST_COUNTS_PATH="$scratch/invalid.json" \
        write_structured_test_results 1 0 'trailing ' pass 1) \
        >/dev/null 2>&1 && status=1
    (DAGRUN_TEST_COUNTS_PATH="$scratch/invalid.json" \
        write_structured_test_results 2 0 duplicate pass 1 duplicate fail 1) \
        >/dev/null 2>&1 && status=1
    rm -rf -- "$scratch"
    if ((status != 0)); then
        echo "write-structured-test-counts: self-test failed" >&2
        return "$status"
    fi
    echo "write-structured-test-counts: self-test passed"
}

if [[ ${1:-} == --self-test && $# -eq 1 ]]; then
    self_test
    exit
fi

write_structured_test_results "$@"
