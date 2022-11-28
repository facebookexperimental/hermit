#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# The full hermit testing setup is Buck-based, and is not working in the OSS release yet.
# This extremely basic test harness is a stop-gap.

set -eEuo pipefail

cd "$(dirname $0)"
cd "../"

rootdir="$(pwd)"
hermit="$rootdir/target/debug/hermit"
hverify="$rootdir/target/debug/hermit-verify"

function hermit_verify {
    # Github Actions VMs don't expose the perf counters we need to use RCBs:
    "$hverify" --hermit-bin="$hermit" run --isolate-workdir \
               --hermit-arg="--base-env=empty" --hermit-arg="--env=HERMIT_MODE=strict" \
               --hermit-arg="--no-rcb-time" --hermit-arg="--preemption-timeout=disabled" \
               "$1"
}

for script in $(ls "$rootdir"/tests/shell/*.sh); do
    echo; echo "Running shell test: $script"
    hermit_verify "$script"
done

rust_tests=(
    rustbin_bind_connect_race
    rustbin_clock_gettime
    rustbin_futex_timeout
    rustbin_futex_wait_child
    rustbin_heap_ptrs
    rustbin_interrogate_tty
    rustbin_nanosleep
    rustbin_network_hello_world
    rustbin_poll
    rustbin_print_clock_nanosleep_monotonic_abs_race
    rustbin_print_clock_nanosleep_monotonic_race
    rustbin_print_clock_nanosleep_realtime_abs_race
    rustbin_print_nanosleep_race
    rustbin_rdtsc
    rustbin_sched_yield
    rustbin_socketpair
    rustbin_stack_ptr
    rustbin_thread_random
)
# Some tests don't work without RCB-timed based preemptions.
# rustbin_clock_total_order
# rustbin_exit_group
# rustbin_futex_and_print
# rustbin_futex_wake_some
# rustbin_mem_race
# rustbin_pipe_basics
# rustbin_poll_spin

for test in "${rust_tests[@]}";
do
    echo; echo "Running rust test: $test"
    file="$rootdir/target/debug/$test"
    hermit_verify "$file"
done
