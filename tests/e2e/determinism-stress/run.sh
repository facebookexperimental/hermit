#!/usr/bin/env bash

set -euo pipefail

stress_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
tests=(
  examples.sh
  random.sh
  thread-racing.sh
  time-clock.sh
  pid-tid.sh
  signals.sh
  pipe-chain.sh
  syscalls.sh
)

failures=0
for test_script in "${tests[@]}"; do
  printf '\n==================== %s ====================\n' "$test_script"
  if ! "$stress_dir/$test_script"; then
    printf 'FAIL: %s\n' "$test_script" >&2
    failures=$((failures + 1))
  fi
done

if ((failures > 0)); then
  printf '\nFAIL: %d determinism stress categor%s failed\n' \
    "$failures" "$([[ $failures -eq 1 ]] && printf 'y' || printf 'ies')" >&2
  exit 1
fi

printf '\nPASS: complete targeted determinism stress suite\n'
