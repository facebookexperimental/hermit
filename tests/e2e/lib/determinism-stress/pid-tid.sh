#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail
# shellcheck source=common.sh source-path=SCRIPTDIR
source "$(dirname -- "${BASH_SOURCE[0]}")/common.sh"

guest=$(compile_c tests/e2e/determinism-stress/guests/pid_tid.c pid-tid)
show_native_variation "PID/TID identities across threads and fork" "$guest"
verify_guest "PID/TID virtualization" "$guest"

stress_success "PID, PPID, and TID virtualization"
