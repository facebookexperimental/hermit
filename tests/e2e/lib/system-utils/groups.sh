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
init_system_utility_test groups "$@"
require_command groups
utility=$(command -v groups)

observe_native host-specific "$utility"
run_strict_verify "$utility"
assert_stdout_exact 'root nobody'
pass_test
