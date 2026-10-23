#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# `/proc/uptime` exposes two host time quantities as procfs file content: seconds
# since boot and cumulative idle-CPU seconds (summed across cores). Both advance
# in real time, so reading the file natively yields a different line on every
# run -- the idle field in particular climbs by roughly cores x wall-clock and is
# reliably distinct across back-to-back invocations. This is a DISTINCT
# nondeterminism channel from the clock syscalls covered elsewhere
# (date-nanoseconds / clock-determinism use gettimeofday / clock_gettime) and
# from the random channels (random-device, uuid): here the entropy is
# boot-relative virtual time delivered through the CONTENT of a /proc file read,
# not a syscall return value and not a CSPRNG draw. Hermit virtualizes its
# logical time and services the procfs read from that deterministic clock, so the
# line repeats bitwise under `--strict --verify` (observed virtualized value:
# a fixed uptime with zero idle). This mirrors how proc-random-uuid confirms
# procfs random-uuid virtualization -- here it confirms procfs uptime
# virtualization. Single `cat` process, no temp file.
set -euo pipefail

case ${1:-} in
    --prepare) command -v cat >/dev/null && test -r /proc/uptime ;;
    --run) exec cat /proc/uptime ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
