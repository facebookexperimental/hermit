#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# Single preparation step for rr manifest guests: generate rr's
# syscall-enum headers into $1, then exec the compiler command in $@.
# One process keeps the cell to one Preparation CPU observation.
set -euo pipefail

generated_dir=$1
shift
mkdir -p -- "$generated_dir"
for header in SyscallEnumsForTestsX64.generated \
    SyscallEnumsForTestsX86.generated \
    SyscallEnumsForTestsGeneric.generated; do
    python3 third-party/rr/src/generate_syscalls.py "$generated_dir/$header"
done
exec "$@"
