#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
# Run ONE gVisor gtest case in its own process (gVisor's runner model).
#
#   run-case.sh <test-binary> <Suite.Case>
#
# Exit codes: gtest's own code for pass/fail; 77 when the case was
# skipped; 78 when zero cases ran (a filter typo must never read as a
# pass). Same convention as tests/gvisor/main.cc in fbsource
# (D128212278). TEST_TMPDIR is a fresh per-case directory unless the
# caller already set one. Helper binaries are found through
# GVT_RUNFILES_DIR when set (see support/runfiles_shim.cc).
set -uo pipefail

if [ $# -ne 2 ]; then
    echo "usage: run-case.sh <test-binary> <Suite.Case>" >&2
    exit 2
fi
bin=$1
case_name=$2

cleanup=""
if [ -z "${TEST_TMPDIR:-}" ]; then
    TEST_TMPDIR=$(mktemp -d)
    cleanup=$TEST_TMPDIR
    export TEST_TMPDIR
fi
trap '[ -n "$cleanup" ] && rm -rf "$cleanup"' EXIT

out=$(mktemp)
"$bin" --gtest_filter="$case_name" --gtest_color=no --gtest_print_time=0 \
    >"$out" 2>&1
rc=$?
cat "$out"
if grep -q "Running 0 tests" "$out"; then
    echo "run-case: zero cases matched filter: $case_name" >&2
    rm -f "$out"
    exit 78
fi
if grep -Eq "\[  SKIPPED \] .*${case_name##*.}" "$out" && [ "$rc" -eq 0 ]; then
    rm -f "$out"
    exit 77
fi
rm -f "$out"
exit "$rc"
