#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd)"
SOURCE="$ROOT_DIR/tests/e2e/system-utils/procfs_sanitized_paths.c"

compile_probe() {
    local output=$1
    cc -std=c11 -O2 -g -Wall -Wextra -Werror "$SOURCE" -o "$output"
}

populate_fixture() {
    local root=$1
    mkdir -p "$root/proc/self"
    printf '400000-401000 r-xp 00000000 00:00 0 /guest\nRss:\t0 kB\nPss:\t0 kB\n' >"$root/proc/self/smaps"
    printf '0 0 0 0 0 0 0\n' >"$root/proc/self/statm"
    printf 'rchar: 0\nwchar: 0\nsyscr: 0\nsyscw: 0\nread_bytes: 0\nwrite_bytes: 0\ncancelled_write_bytes: 0\n' >"$root/proc/self/io"
    printf '1 0 0:1 /tmpvol/.hermit / rw - tmpfs tmpfs rw\n' >"$root/proc/self/mountinfo"
    printf '400000 default file=/guest kernelpagesize_kB=4\n' >"$root/proc/self/numa_maps"
    printf 'AVX512_elapsed_ms:\t0\n' >"$root/proc/self/arch_status"
    printf 'cpu 0 0 0\nintr 0\nctxt 0\nprocesses 0\nprocs_running 0\nprocs_blocked 0\nsoftirq 0\nbtime 0\n' >"$root/proc/stat"
    printf 'nr_free_pages 0\n' >"$root/proc/vmstat"
    printf 'Node 0, zone Normal\n  pages free 0\n        min 0\n        low 0\n        high 0\n        managed 0\n' >"$root/proc/zoneinfo"
    printf '0.00 0.00 0.00 1/1 1\n' >"$root/proc/loadavg"
    printf '1 0 vda 1 0 8 0 1 0 8 0\n' >"$root/proc/diskstats"
    printf 'example 4096 0 - Live 0x0\n' >"$root/proc/modules"
    printf 'Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n' >"$root/proc/swaps"
    printf '  0: 0 0 timer\n' >"$root/proc/interrupts"
    printf 'TIMER: 0 0\n' >"$root/proc/softirqs"
    printf 'Node 0, zone Normal 0 0 0\n' >"$root/proc/buddyinfo"
    printf 'version 15\ntimestamp 0\ncpu0 0 0\ndomain0 ff 0 0\n' >"$root/proc/schedstat"
    printf '0: 0 0/0 0/200 0/20000\n' >"$root/proc/key-users"
}

self_test() {
    local work
    local output
    work=$(mktemp -d)
    trap 'rm -rf "$work"' RETURN
    compile_probe "$work/probe"
    populate_fixture "$work/root"
    "$work/probe" --fixture-root "$work/root" >/dev/null

    rm "$work/root/proc/self/statm"
    mkdir "$work/root/proc/self/statm"
    if output=$("$work/probe" --fixture-root "$work/root" 2>&1); then
        echo 'procfs-sanitized-paths self-test accepted an unreadable statm' >&2
        return 1
    fi
    [[ $output == *'self-statm read failed'* ]] || {
        printf 'procfs-sanitized-paths self-test got the wrong unreadable failure: %s\n' "$output" >&2
        return 1
    }

    rmdir "$work/root/proc/self/statm"
    printf '0 0 0 0 0 0 0\n' >"$work/root/proc/self/statm"
    printf 'rchar: 1\nwchar: 0\nsyscr: 0\nsyscw: 0\nread_bytes: 0\nwrite_bytes: 0\ncancelled_write_bytes: 0\n' >"$work/root/proc/self/io"
    if output=$("$work/probe" --fixture-root "$work/root" 2>&1); then
        echo 'procfs-sanitized-paths self-test accepted an unnormalized process counter' >&2
        return 1
    fi
    [[ $output == *'self-io rchar retained a nonzero host counter'* ]] || {
        printf 'procfs-sanitized-paths self-test got the wrong invariant failure: %s\n' "$output" >&2
        return 1
    }

    populate_fixture "$work/root"
    if output=$(PROCFS_PROBE_MUTATE_LABEL=loadavg "$work/probe" --fixture-root "$work/root" 2>&1); then
        echo 'procfs-sanitized-paths self-test accepted an unstable fixed snapshot' >&2
        return 1
    fi
    [[ $output == *'loadavg changed between adjacent reads'* ]] || {
        printf 'procfs-sanitized-paths self-test got the wrong stability failure: %s\n' "$output" >&2
        return 1
    }
}

case ${1:-} in
    --prepare)
        compile_probe "$E2E_FIXTURE_DIR/procfs-sanitized-paths"
        ;;
    --run)
        exec "$E2E_FIXTURE_DIR/procfs-sanitized-paths" --run
        ;;
    --self-test)
        self_test
        ;;
    *) echo "usage: $0 --prepare|--run|--self-test" >&2; exit 2 ;;
esac
