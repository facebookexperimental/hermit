#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# Touches one Detcore-sanitized leaf under each of the eight sysfs prefixes
# that previously had no guest-level coverage. The assertions live here, in
# the guest: a verify comparison cannot defend a control whose purpose is to
# make both runs agree, because it succeeds identically when both runs see the
# same unsanitized host value.
#
# The preparation self-test proves that these assertions fail for an
# unreadable leaf and for a value that changes between reads. It deliberately
# uses a synthetic sysfs root. A native run also fails when a live node vmstat
# changes, but native execution removes all eight sanitizers at once. Together
# those controls demonstrate the guest program's failure path; neither isolates
# one sanitizer or proves every sanitizer is individually necessary.

set -euo pipefail
shopt -s nullglob

read_sysfs_text() {
    local path=$1
    local output_name=$2
    local value=''
    local status=0

    # Sysfs attributes are text and contain no NUL bytes. `read -d ""` retains
    # every newline, unlike command substitution, and reports 1 for normal EOF.
    IFS= read -r -d '' value <"$path" || status=$?
    if [[ $status -ne 0 && $status -ne 1 ]] || [[ -z $value ]]; then
        return 1
    fi
    printf -v "$output_name" '%s' "$value"
}

probe_prefix() {
    local label=$1
    local root=$2
    local candidate
    local first
    local second
    local -a candidates=()

    case "$label" in
        block) candidates=("$root"/sys/block/*/inflight) ;;
        hwmon) candidates=("$root"/sys/class/hwmon/hwmon*/*_input) ;;
        rtc) candidates=("$root"/sys/class/rtc/rtc*/since_epoch) ;;
        node) candidates=("$root"/sys/devices/system/node/node*/vmstat) ;;
        btrfs) candidates=("$root"/sys/fs/btrfs/*/commit_stats) ;;
        irq) candidates=("$root"/sys/kernel/irq/*/per_cpu_count) ;;
        uevent) candidates=("$root"/sys/kernel/uevent_seqnum) ;;
        module) candidates=("$root"/sys/module/*/refcnt) ;;
        *) printf 'sysfs-sanitized-prefixes FAIL: unknown prefix %s\n' "$label" >&2; return 2 ;;
    esac

    for candidate in "${candidates[@]}"; do
        [[ -r $candidate ]] || continue
        if read_sysfs_text "$candidate" first; then
            if [[ ${SYSFS_PROBE_MUTATE_LABEL:-} == "$label" ]]; then
                printf 'changed\n' >"$candidate"
            fi
            if ! read_sysfs_text "$candidate" second; then
                printf 'sysfs-sanitized-prefixes FAIL: %s second read failed at %s\n' "$label" "$candidate" >&2
                return 1
            fi
            if [[ $first != "$second" ]]; then
                printf 'sysfs-sanitized-prefixes FAIL: %s changed between reads at %s\n' "$label" "$candidate" >&2
                return 1
            fi
            printf '%s=readable-and-stable\n' "$label"
            return 0
        fi
    done

    printf 'sysfs-sanitized-prefixes FAIL: %s has no readable sanitized leaf\n' "$label" >&2
    return 1
}

check_all_prefixes() {
    local root=${1:-}
    local label
    for label in block hwmon rtc node btrfs irq uevent module; do
        probe_prefix "$label" "$root" || return
    done
}

self_test() {
    local fixture
    local output
    fixture=$(mktemp -d)
    mkdir -p \
        "$fixture/sys/block/vda" \
        "$fixture/sys/class/hwmon/hwmon0" \
        "$fixture/sys/class/rtc/rtc0" \
        "$fixture/sys/devices/system/node/node0" \
        "$fixture/sys/fs/btrfs/00000000-0000-0000-0000-000000000000" \
        "$fixture/sys/kernel/irq/1" \
        "$fixture/sys/module/example"
    printf '0 0\n' >"$fixture/sys/block/vda/inflight"
    printf '0\n' >"$fixture/sys/class/hwmon/hwmon0/temp1_input"
    printf '1767225600\n' >"$fixture/sys/class/rtc/rtc0/since_epoch"
    printf 'nr_free_pages 0\n' >"$fixture/sys/devices/system/node/node0/vmstat"
    printf 'commits 0\n' >"$fixture/sys/fs/btrfs/00000000-0000-0000-0000-000000000000/commit_stats"
    printf '0,0\n' >"$fixture/sys/kernel/irq/1/per_cpu_count"
    printf '0\n' >"$fixture/sys/kernel/uevent_seqnum"
    printf '0\n' >"$fixture/sys/module/example/refcnt"

    check_all_prefixes "$fixture" >/dev/null || {
        echo 'sysfs-sanitized-prefixes self-test rejected stable readable fixtures' >&2
        rm -rf "$fixture"
        return 1
    }

    rm -f "$fixture/sys/class/rtc/rtc0/since_epoch"
    mkdir "$fixture/sys/class/rtc/rtc0/since_epoch"
    if output=$(check_all_prefixes "$fixture" 2>&1); then
        echo 'sysfs-sanitized-prefixes self-test accepted an unreadable RTC leaf' >&2
        rm -rf "$fixture"
        return 1
    fi
    [[ $output == *'rtc has no readable sanitized leaf'* ]] || {
        printf 'sysfs-sanitized-prefixes self-test got the wrong unreadable failure: %s\n' "$output" >&2
        rm -rf "$fixture"
        return 1
    }

    rmdir "$fixture/sys/class/rtc/rtc0/since_epoch"
    printf '1767225600\n' >"$fixture/sys/class/rtc/rtc0/since_epoch"
    SYSFS_PROBE_MUTATE_LABEL=module
    if output=$(check_all_prefixes "$fixture" 2>&1); then
        echo 'sysfs-sanitized-prefixes self-test accepted an unstable module leaf' >&2
        rm -rf "$fixture"
        return 1
    fi
    unset SYSFS_PROBE_MUTATE_LABEL
    [[ $output == *'module changed between reads'* ]] || {
        printf 'sysfs-sanitized-prefixes self-test got the wrong unstable failure: %s\n' "$output" >&2
        rm -rf "$fixture"
        return 1
    }

    rm -rf "$fixture"
}

case ${1:-} in
    --prepare | --self-test) self_test ;;
    --run) check_all_prefixes ;;
    *) echo "usage: $0 --prepare|--run|--self-test" >&2; exit 2 ;;
esac
