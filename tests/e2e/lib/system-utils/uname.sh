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
init_system_utility_test uname "$@"
require_command uname
utility=$(command -v uname)

observe_native host-specific "$utility" -a
run_strict_verify "$utility" -a
assert_stdout_exact \
    'Linux hermetic-container.local 5.2.0 #1 SMP Thu Jan 01 00:00:00 UTC 2026 x86_64 x86_64 x86_64 GNU/Linux'
pass_test
