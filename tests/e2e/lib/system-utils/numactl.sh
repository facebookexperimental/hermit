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

# KVM passes through changing host NUMA free-memory counters.
# shellcheck disable=SC2034 # Consumed by the sourced common harness.
readonly -a BACKEND_ALLOWLIST=(ptrace)
init_system_utility_test numactl "$@"
require_command numactl
utility=$(command -v numactl)

observe_native host-specific "$utility" --hardware
run_strict_verify "$utility" --hardware
assert_stdout_contains 'available: 1 nodes (0)'
assert_stdout_contains 'node 0 size: 1024 MB'
assert_stdout_contains 'node 0 free: 1024 MB'
assert_stdout_matches '^[[:space:]]*0:[[:space:]]+10[[:space:]]*$'
pass_test
