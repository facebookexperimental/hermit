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

# KVM currently denies lscpu's /proc/cpuinfo read.
# shellcheck disable=SC2034 # Consumed by the sourced common harness.
readonly -a BACKEND_ALLOWLIST=(ptrace)
init_system_utility_test lscpu "$@"
require_command lscpu
utility=$(command -v lscpu)

observe_native host-specific "$utility"
run_strict_verify "$utility"
assert_stdout_matches '^Architecture:[[:space:]]+x86_64$'
assert_stdout_matches '^Byte Order:[[:space:]]+Little Endian$'
assert_stdout_matches '^CPU\(s\):[[:space:]]+[1-9][0-9]*$'
assert_stdout_matches '^NUMA node\(s\):[[:space:]]+1$'
pass_test
