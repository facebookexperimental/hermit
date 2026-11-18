#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
set -euo pipefail

# The body of --run.
run_once() {
    work="${E2E_TMPDIR:-/tmp}/shell-pipeline"
    rm -rf "$work"
    mkdir -p "$work"
    input="$work/words.txt"
    # Unsorted input with duplicates. The pipeline below must impose a
    # total order deterministically regardless of this arrangement.
    printf '%s\n' \
        banana apple cherry apple banana apple date cherry banana \
        elderberry apple >"$input"

    # LC_ALL=C pins collation so the sort order is identical on every host
    # (the #1211 host-path/locale portability lesson).
    export LC_ALL=C

    # Multi-stage pipeline: each stage is a separate process, so hermit must
    # deterministically schedule the fork/exec/pipe fan-out and reap every
    # child. sort -> uniq -c -> awk(normalize whitespace) -> sort(count desc,
    # then name asc). uniq -c field width is normalized by awk so output is
    # portable across coreutils versions.
    freq=$(sort "$input" | uniq -c | awk '{ print $2 " " $1 }' \
        | sort -k2,2nr -k1,1)
    printf 'FREQ\n%s\n' "$freq"

    # Second independent pipeline (more fork/exec/pipe): distinct line count.
    distinct=$(sort -u "$input" | wc -l | tr -d '[:space:]')
    printf 'DISTINCT %s\n' "$distinct"

    # Total input lines.
    total=$(wc -l <"$input" | tr -d '[:space:]')
    printf 'TOTAL %s\n' "$total"
}

case ${1:-} in
    --prepare) exit 0 ;;
    --run)
        # E2E_ITERATIONS=N runs this body N times in one guest run, for a
        # benchmark that fits wall time against N (tests/c/e2e_iterations.h);
        # unset, it runs once, as every e2e cell runs it.
        iterations=${E2E_ITERATIONS:-1}
        if [[ ! $iterations =~ ^[1-9][0-9]{0,6}$ ]] || ((iterations > 1000000)); then
            echo "E2E_ITERATIONS must be a decimal integer from 1 to 1000000, got \"$iterations\"" >&2
            exit 2
        fi
        # A while loop, not `for ((...))`: bash's arithmetic for loop blocks
        # SIGCHLD once more when it ends, and an unset count must leave the
        # run's system calls exactly as they were.
        while :; do
            run_once
            ((--iterations > 0)) || break
        done
        ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
