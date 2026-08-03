#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# Reading /proc/sys/kernel/random/uuid makes the *kernel* format a fresh
# version-4 UUID from its CSPRNG on every read and return it as file content.
# This is a distinct entropy channel from getrandom(2)/libuuid (uuidgen-random)
# and from the /dev/urandom character device (random-device): the randomness
# arrives as the bytes of a procfs read, not from a device or a syscall return
# register. The value therefore varies natively run to run but is determinized
# by Hermit, which virtualizes that read. `cat` of a single procfs file is one
# fast process with no temp file. Kernel-UUID entropy is unique per read, so the
# native control never collides the way a 1-second-granularity clock channel
# would.
set -euo pipefail

uuid=/proc/sys/kernel/random/uuid

case ${1:-} in
    --prepare) test -r "$uuid" ;;
    --run) exec cat "$uuid" ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
