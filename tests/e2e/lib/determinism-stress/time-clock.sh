#!/usr/bin/env bash

set -euo pipefail
# shellcheck source=tests/e2e/lib/determinism-stress/common.sh
source "$(dirname -- "${BASH_SOURCE[0]}")/common.sh"

clock_guest=$(compile_c tests/c/clock_determinism.c clock-determinism -lrt)
failures=0
if ! verify_guest "gettimeofday and clock_gettime matrix" "$clock_guest"; then
  failures=$((failures + 1))
fi

show_native_variation "timestamped date output" "$repo_root/examples/date.sh"
if ! verify_guest "timestamped date output" "$repo_root/examples/date.sh"; then
  failures=$((failures + 1))
fi

((failures == 0)) || fail "$failures time/clock target(s) failed strict L2"
stress_success "time, clocks, sleeps, and timestamps"
