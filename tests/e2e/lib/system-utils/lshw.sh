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
init_system_utility_test lshw "$@"
require_command lshw
utility=$(command -v lshw)

observe_native host-specific "$utility" -short
run_strict_verify "$utility" -short
assert_stdout_contains 'H/W path'
assert_stdout_matches 'system[[:space:]]+Computer$'
assert_stdout_matches 'processor[[:space:]]+'
pass_test
