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
init_system_utility_test df "$@"
require_command df

fixture=$SYSTEM_UTIL_GUEST_WORKDIR/df-fixture
mkdir -p "$fixture"

observe_native may-vary /usr/bin/df -Pk -- "$fixture"
run_strict_verify /usr/bin/df -Pk -- "$fixture"
assert_stdout_contains 'Filesystem     1024-blocks'
assert_stdout_matches '^[^[:space:]]+[[:space:]]+[1-9][0-9]*[[:space:]]+[0-9]+[[:space:]]+4000000[[:space:]]+[0-9]{1,3}%[[:space:]]+'
pass_test
