#!/usr/bin/env bash

set -euo pipefail
# shellcheck source=tests/e2e/determinism-stress/common.sh
source "$(dirname -- "${BASH_SOURCE[0]}")/common.sh"

expected=(date.sh devrand.sh race.sh rand.py timed-progress-bar.py)
mapfile -t actual < <(
  find "$repo_root/examples" -maxdepth 1 -type f ! -name README.md -printf '%f\n' | sort
)
if [[ ${actual[*]} != "${expected[*]}" ]]; then
  printf 'expected example programs: %s\n' "${expected[*]}" >&2
  printf 'actual example programs:   %s\n' "${actual[*]}" >&2
  fail "examples manifest changed; classify every program in examples.sh"
fi

failures=0
for example in "${expected[@]}"; do
  program=$repo_root/examples/$example
  show_native_variation "example/$example" "$program"
  if ! verify_guest "example/$example" "$program"; then
    failures=$((failures + 1))
  fi
done

((failures == 0)) || fail "$failures example program(s) failed strict L2"
stress_success "all examples/ programs"
