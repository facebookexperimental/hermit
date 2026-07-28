#!/usr/bin/env bash

set -euo pipefail
# shellcheck source=tests/e2e/determinism-stress/common.sh
source "$(dirname -- "${BASH_SOURCE[0]}")/common.sh"

guest=$(compile_c tests/e2e/determinism-stress/guests/pid_tid.c pid-tid)
show_native_variation "PID/TID identities across threads and fork" "$guest"
verify_guest "PID/TID virtualization" "$guest"

stress_success "PID, PPID, and TID virtualization"
