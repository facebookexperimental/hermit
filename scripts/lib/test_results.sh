#!/usr/bin/env bash

# Shell bridge for dagrun::TestResults. The Rust reader validates the complete
# shared record before this library projects its fields for human-facing output.

test_results_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
readonly test_results_root
readonly TEST_RESULTS_READER="$test_results_root/ci/structured-test-results.rs"

# These globals are the sourced library's interface.
# shellcheck disable=SC2034
TEST_RESULTS_EXECUTED=
# shellcheck disable=SC2034
TEST_RESULTS_FILTERED=
# shellcheck disable=SC2034
TEST_RESULTS_PASSED=
# shellcheck disable=SC2034
TEST_RESULTS_FAILED=
# shellcheck disable=SC2034
TEST_RESULTS_FIRST_FAILURE=

load_test_results() { # <path>
    local path=$1 canonical fields
    TEST_RESULTS_EXECUTED=
    TEST_RESULTS_FILTERED=
    TEST_RESULTS_PASSED=
    TEST_RESULTS_FAILED=
    TEST_RESULTS_FIRST_FAILURE=
    [[ -s $path ]] || return 1
    canonical=$("$TEST_RESULTS_READER" summary "$path") || return 1
    fields=$(jq -er '
        [
            .executed_tests,
            .filtered_tests,
            .passed_tests,
            .failed_tests,
            (.first_failed_test // "")
          ]
        | @tsv
    ' <<<"$canonical") || return 1
    # shellcheck disable=SC2034 # The caller consumes these exported shell values.
    IFS=$'\t' read -r \
        TEST_RESULTS_EXECUTED TEST_RESULTS_FILTERED TEST_RESULTS_PASSED \
        TEST_RESULTS_FAILED TEST_RESULTS_FIRST_FAILURE <<<"$fields"
}
