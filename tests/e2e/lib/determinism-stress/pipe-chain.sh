#!/usr/bin/env bash

set -euo pipefail
# shellcheck source=tests/e2e/lib/determinism-stress/common.sh
source "$(dirname -- "${BASH_SOURCE[0]}")/common.sh"

# Expansion is intentionally deferred to the guest Bash process.
# shellcheck disable=SC2016
pipe_program='(
  for value in $(seq 1 80); do printf "left:%03d\n" "$value"; done &
  for value in $(seq 1 80); do printf "right:%03d\n" "$value"; done &
  wait
) | awk '\''{ print NR ":" $0 }'\'' | sed '\''s/^/stage3:/'\'' | sha256sum'

show_native_variation "four-stage Bash pipeline" /bin/bash -c "$pipe_program"
verify_guest "four-stage Bash pipeline" /bin/bash -c "$pipe_program"

stress_success "Bash A | B | C | D pipeline scheduling"
