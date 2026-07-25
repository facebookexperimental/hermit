#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

ROOT="$(cd -- "$(dirname -- "$0")" && pwd)"
MANIFEST="$ROOT/Cargo.toml"
CRITERION_HOME="${CRITERION_HOME:-$ROOT/target/criterion}"
RESULTS_DIRECTORY="${1:-$ROOT/results/latest}"
export CRITERION_HOME
export SYSCALL_BENCH_CAPABILITIES="${SYSCALL_BENCH_CAPABILITIES:-$CRITERION_HOME/capabilities.tsv}"

if ! command -v with-proxy >/dev/null 2>&1; then
  echo "with-proxy is required for Cargo dependency access" >&2
  exit 2
fi

if [[ -z "${SYSCALL_BENCH_CPU:-}" ]]; then
  echo "warning: set SYSCALL_BENCH_CPU to an idle logical CPU for publishable results" >&2
fi

mkdir -p "$CRITERION_HOME" "$RESULTS_DIRECTORY"

with-proxy cargo bench --locked --manifest-path "$MANIFEST" --bench marginal_syscalls
with-proxy cargo run --locked --release --manifest-path "$MANIFEST" --bin summarize -- \
  "$CRITERION_HOME" "$RESULTS_DIRECTORY"

printf "Criterion HTML: %s\n" "$CRITERION_HOME/report/index.html"
printf "Summary: %s\n" "$RESULTS_DIRECTORY/SUMMARY.md"
