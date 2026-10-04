#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail
# shellcheck source=common.sh source-path=SCRIPTDIR
source "$(dirname -- "${BASH_SOURCE[0]}")/common.sh"

order_guest=$(compile_c \
  tests/e2e/determinism-stress/guests/signal_order.c signal-order)
show_native_variation "concurrent signal delivery order" "$order_guest"
failures=0
if ! verify_guest "concurrent signal delivery order" "$order_guest"; then
  failures=$((failures + 1))
fi

guest=$(compile_c tests/c/signal_determinism.c signal-determinism -lrt)
scenarios=(
  itimer-delivery
  itimer-exit
  blocking-sigsuspend
  masks-fork-clone
  blocking-read-interrupted
  blocking-read-restarted
  poll-sa-restart
  epoll-wait-sa-restart
  sigtimedwait-sa-restart
  handler-reentrance
  altstack-preservation
  pending-exec
)
for scenario in "${scenarios[@]}"; do
  if ! verify_guest "signal scenario: $scenario" "$guest" "$scenario"; then
    failures=$((failures + 1))
  fi
done

((failures == 0)) || fail "$failures signal target(s) failed verification"
stress_success "signal delivery ordering"
