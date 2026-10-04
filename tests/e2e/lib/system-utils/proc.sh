#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck disable=SC1091 # Resolved relative to this script at runtime.
source "$script_dir/_common.sh"

# shellcheck disable=SC2034 # Consumed by the sourced common harness.
readonly -a BACKEND_ALLOWLIST=(ptrace kvm)
init_system_utility_test proc "$@"
require_command bash
require_command awk

native_probe='cat /proc/cpuinfo /proc/meminfo /proc/uptime'
observe_native must-vary /bin/bash -c "$native_probe"

guest_probe=$(cat <<'EOF'
set -euo pipefail
awk -F ':' '
    /^processor/ { processors++ }
    /^cpu MHz/ {
        gsub(/^[[:space:]]+|[[:space:]]+$/, "", $2)
        if ($2 != "1000.000") bad_mhz = 1
    }
    END {
        printf "processors=%d\n", processors
        print "cpu_mhz=1000.000"
        exit bad_mhz
    }
' /proc/cpuinfo
awk '
    /^MemTotal:/ { print "mem_total_kb=" $2 }
    /^MemFree:/ { print "mem_free_kb=" $2 }
    /^MemAvailable:/ { print "mem_available_kb=" $2 }
    /^Cached:/ { print "cached_kb=" $2 }
    /^SwapTotal:/ { print "swap_total_kb=" $2 }
' /proc/meminfo
awk '{ print "uptime=" $1 " " $2 }' /proc/uptime
EOF
)

run_strict_verify /bin/bash -c "$guest_probe"
assert_stdout_matches '^processors=[1-9][0-9]*$'
assert_stdout_contains 'cpu_mhz=1000.000'
if [[ $SYSTEM_UTIL_BACKEND == kvm ]]; then
    assert_stdout_contains 'mem_total_kb=2097152'
    assert_stdout_contains 'mem_free_kb=1048576'
    assert_stdout_contains 'mem_available_kb=1572864'
    assert_stdout_contains 'uptime=0.00 0.00'
else
    assert_stdout_contains 'mem_total_kb=976562'
    assert_stdout_contains 'mem_free_kb=976562'
    assert_stdout_contains 'mem_available_kb=976562'
    assert_stdout_contains 'uptime=120.00 0.00'
fi
assert_stdout_contains 'cached_kb=0'
assert_stdout_contains 'swap_total_kb=0'
pass_test
