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
init_system_utility_test du "$@"
require_command du

fixture=$SYSTEM_UTIL_GUEST_WORKDIR/du-fixture
mkdir -p "$fixture/subdir"
printf 'alpha\nbeta\ngamma\n' >"$fixture/input.txt"
printf 'delta\nepsilon\n' >"$fixture/subdir/nested.txt"

observe_native host-specific /usr/bin/du -sk -- "$fixture"
run_strict_verify /usr/bin/du -sk -- "$fixture"
assert_stdout_matches "^[1-9][0-9]*[[:space:]]+$fixture$"
pass_test
