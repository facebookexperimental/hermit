#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "$script_dir/../../.." && pwd)
hermit_bin=${1:-$repo_root/target/release/hermit}
backend=${2:-${SYSTEM_UTIL_BACKEND:-ptrace}}

tests=(
    whoami
    hostname
    lscpu
    lshw
    numactl
    uname
    id
    groups
    proc
    du
    df
)

for test_name in "${tests[@]}"; do
    printf '\n== %s (%s) ==\n' "$test_name" "$backend"
    "$script_dir/$test_name.sh" "$hermit_bin" "$backend"
done

printf '\nSystem utility e2e suite completed for backend %s.\n' "$backend"
