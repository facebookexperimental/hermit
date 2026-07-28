#!/usr/bin/env bash

set -euo pipefail
# shellcheck source=tests/e2e/lib/determinism-stress/common.sh
source "$(dirname -- "${BASH_SOURCE[0]}")/common.sh"

sync_guest=$(compile_c tests/c/thread_sync_determinism.c thread-sync)
failures=0
for pattern in barrier condvar rwlock semaphore cancellation tls-fork; do
  show_native_variation "thread synchronization: $pattern" "$sync_guest" "$pattern"
  if ! verify_guest "thread synchronization: $pattern" "$sync_guest" "$pattern"; then
    failures=$((failures + 1))
  fi
done

lock_free_guest=$(compile_c \
  tests/e2e/determinism-stress/guests/lock_free.c lock-free)
show_native_variation "lock-free CAS contention" "$lock_free_guest"
if ! verify_guest "lock-free CAS contention" "$lock_free_guest"; then
  failures=$((failures + 1))
fi

((failures == 0)) || fail "$failures thread-race target(s) failed strict L2"
stress_success "mutex, condition-variable, semaphore, rwlock, and lock-free races"
