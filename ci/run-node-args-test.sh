#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# Guard ci/run-node.sh's exact-selection contract. Runtime command replacement
# was removed with the one-DAG cutover; every trailing form must now refuse.

set -uo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR" || exit 2

RUN_NODE="$ROOT_DIR/ci/run-node.sh"
LANE=portable
NODE=check.dagrun_naming
failures=0

fail() {
    printf 'run-node-args-test: FAIL — %s\n' "$1" >&2
    failures=$((failures + 1))
}

run_local() {
    # A nested plan query retains its real outer run state and ancestry.
    # Only the hosted-environment switches are removed for these local controls.
    env -u CI -u GITHUB_ACTIONS "$@"
}

expect_removed_refusal() {
    local what=$1
    shift
    local output status
    output=$(run_local "$@" 2>&1)
    status=$?
    if ((status != 2)); then
        fail "$what: expected exit 2, got $status. Output: $output"
    elif [[ $output != *"trailing command replacement arguments were removed"* ]]; then
        fail "$what: exited 2 for the wrong reason. Output: $output"
    else
        printf 'run-node-args-test: ok — %s refused by the removed-command guard\n' "$what"
    fi
}

expect_removed_refusal "an unmarked trailing argument" \
    "$RUN_NODE" "$LANE" "$NODE" -E 'test(=nothing)'
expect_removed_refusal "a literal -- with nothing after it" \
    "$RUN_NODE" "$LANE" "$NODE" --
expect_removed_refusal "a literal -- with replacement text" \
    "$RUN_NODE" "$LANE" "$NODE" -- --some-flag
expect_removed_refusal "a multi-node replacement" \
    "$RUN_NODE" "$LANE" "$NODE,lint.rustfmt" -- --some-flag

usage_output=$(run_local "$RUN_NODE" "$LANE" "$NODE" -- 2>&1)
if [[ $usage_output != *"usage: ci/run-node.sh <portable|privileged>"* ]]; then
    fail "the replacement refusal did not print the exact-selection usage. Output: $usage_output"
else
    printf 'run-node-args-test: ok — the refusal prints the supported form\n'
fi

# Use the real hosted preflight selection so the positive control covers the
# long comma-separated input used by the workflow without writing a scratch DAG.
long_sel=$(python3 -c '
import json
print(",".join(json.load(open("ci/portable-shards.json"))["preflight_nodes"]))')
if [[ -z $long_sel ]]; then
    fail "could not read preflight_nodes from ci/portable-shards.json"
else
    long_output=$(run_local env RUN_NODE_PRINT_ONLY=1 \
        VALIDATE_SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS=1 \
        "$RUN_NODE" "$LANE" "$long_sel" 2>&1)
    long_status=$?
    if ((long_status != 0)); then
        fail "the real ${#long_sel}-byte preflight selection was refused: exit $long_status. Output: $long_output"
    elif [[ $long_output != *"profile: hosted-portable"* ]]; then
        fail "portable did not select hosted-portable. Output: $long_output"
    elif [[ $long_output != *"scheduler-width=validate-default"* ]]; then
        fail "the selection did not inherit validate's scheduler width. Output: $long_output"
    else
        printf 'run-node-args-test: ok — hosted portable selection reaches the committed graph\n'
    fi
fi

override_output=$(run_local env RUN_NODE_PRINT_ONLY=1 RUN_NODE_JOBS=3 \
    VALIDATE_SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS=1 \
    "$RUN_NODE" "$LANE" "$NODE" 2>&1)
override_status=$?
if ((override_status != 0)); then
    fail "the scheduler-width override was refused: exit $override_status. Output: $override_output"
elif [[ $override_output != *"scheduler-width=-j3"* ]]; then
    fail "RUN_NODE_JOBS was not forwarded. Output: $override_output"
else
    printf 'run-node-args-test: ok — RUN_NODE_JOBS overrides validate default\n'
fi

hosted_test_output=$(run_local env RUN_NODE_PRINT_ONLY=1 \
    VALIDATE_SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS=1 \
    "$RUN_NODE" portable test.regular_crates 2>&1)
hosted_test_status=$?
if ((hosted_test_status != 0)); then
    fail "the shared portable test selector was refused: exit $hosted_test_status. Output: $hosted_test_output"
elif [[ $hosted_test_output != *"test.regular_crates maps to committed node test.regular_crates_on_host"* ]]; then
    fail "the portable public ID did not select its committed host execution. Output: $hosted_test_output"
else
    printf 'run-node-args-test: ok — shared portable test IDs retain hosted execution\n'
fi

compat_output=$(run_local env RUN_NODE_PRINT_ONLY=1 \
    VALIDATE_SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS=1 \
    "$RUN_NODE" portable test.strict_compat 2>&1)
compat_status=$?
if ((compat_status != 0)); then
    fail "the hosted strict compatibility selector was refused: exit $compat_status. Output: $compat_output"
elif [[ $compat_output != *"compatprep.fixtures_on_host"* ]]; then
    fail "strict compatibility omitted its hosted fixture producer. Output: $compat_output"
else
    printf 'run-node-args-test: ok — strict compatibility retains its hosted fixture\n'
fi

privileged_output=$(run_local env RUN_NODE_PRINT_ONLY=1 \
    VALIDATE_SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS=1 \
    "$RUN_NODE" privileged cpuid.faulting 2>&1)
privileged_status=$?
if ((privileged_status != 0)); then
    fail "hosted privileged selection was refused: exit $privileged_status. Output: $privileged_output"
elif [[ $privileged_output != *"profile: hosted-privileged"* ]]; then
    fail "privileged did not select hosted-privileged. Output: $privileged_output"
elif [[ $privileged_output != *"cpuid.faulting maps to committed node privileged-only-cpuid.faulting_on_host"* ]]; then
    fail "privileged public ID was not reported as mapped. Output: $privileged_output"
elif [[ $privileged_output != *"privileged-only-cpuid.faulting_on_host"* ]]; then
    fail "privileged public ID did not select the committed hosted node. Output: $privileged_output"
else
    printf 'run-node-args-test: ok — old privileged IDs select the committed hosted graph\n'
fi

privileged_unknown=$(run_local env RUN_NODE_PRINT_ONLY=1 \
    VALIDATE_SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS=1 \
    "$RUN_NODE" privileged no.such_privileged_node 2>&1)
privileged_unknown_status=$?
if ((privileged_unknown_status == 0)); then
    fail "an unknown privileged ID was accepted. Output: $privileged_unknown"
elif [[ $privileged_unknown != *"unknown step tag"* ]]; then
    fail "unknown privileged ID refused without its cause. Output: $privileged_unknown"
else
    printf 'run-node-args-test: ok — unknown privileged public ID refuses by name\n'
fi

if ((failures > 0)); then
    printf 'run-node-args-test: %d check(s) FAILED\n' "$failures" >&2
    exit 1
fi
printf 'run-node-args-test: OK — replacement arguments refuse; exact hosted selections remain executable\n'
