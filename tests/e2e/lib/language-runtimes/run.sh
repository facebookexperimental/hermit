#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../../.." && pwd)"
cd "$ROOT_DIR"

HERMIT_BIN=${HERMIT_BIN:-target/debug/hermit}
RUNTIME_TIMEOUT=${HERMIT_LANGUAGE_RUNTIME_TIMEOUT:-180s}
BACKEND=${HERMIT_LANGUAGE_RUNTIME_BACKEND:-ptrace}
BUILD_DIR=target/language-runtime-e2e
SENTINEL=language-runtime-e2e
readonly ROOT_DIR HERMIT_BIN RUNTIME_TIMEOUT BACKEND BUILD_DIR SENTINEL

if [[ ! -x $HERMIT_BIN ]]; then
  echo "Hermit binary is not executable: $HERMIT_BIN" >&2
  exit 1
fi

mkdir -p "$BUILD_DIR"

function runtime_path {
  local candidate
  for candidate in "$@"; do
    if command -v "$candidate" >/dev/null 2>&1; then
      command -v "$candidate"
      return 0
    fi
  done
  return 1
}

function run_l2 {
  local label=$1
  local slug=$2
  local program=$3
  shift 3
  local strict_output="$BUILD_DIR/$slug.strict.log"
  local verify_output="$BUILD_DIR/$slug.verify.log"

  printf '==> %s: %s category probe (--strict)\n' "$label" "$BACKEND"
  if ! timeout --foreground --kill-after=10s "$RUNTIME_TIMEOUT" \
      "$HERMIT_BIN" --log=off --backend "$BACKEND" run \
      --strict --no-virtualize-cpuid --max-timeslice=disabled \
      --base-env=minimal --env="HERMIT_RUNTIME_SENTINEL=$SENTINEL" -- \
      "$program" "$@" >"$strict_output" 2>&1; then
    cat "$strict_output" >&2
    echo "FAIL: $label category probe failed under $BACKEND --strict" >&2
    return 1
  fi

  cat "$strict_output"
  local category
  for category in RANDOM TIME THREAD SYSTEM; do
    if ! grep -q "^$category " "$strict_output"; then
      echo "FAIL: $label did not emit the $category category" >&2
      return 1
    fi
  done

  printf '==> %s: %s L2 (--strict --verify)\n' "$label" "$BACKEND"
  if ! timeout --foreground --kill-after=10s "$RUNTIME_TIMEOUT" \
      "$HERMIT_BIN" --log=off --backend "$BACKEND" run \
      --strict --verify --no-virtualize-cpuid --max-timeslice=disabled \
      --base-env=minimal --env="HERMIT_RUNTIME_SENTINEL=$SENTINEL" -- \
      "$program" "$@" >"$verify_output" 2>&1; then
    cat "$verify_output" >&2
    echo "FAIL: $label did not reach $BACKEND L2" >&2
    return 1
  fi

  cat "$verify_output"
  if ! grep -q "Determinism verified" "$verify_output"; then
    echo "FAIL: $label exited zero without Hermit's verification marker" >&2
    return 1
  fi
  printf 'PASS: %s reached %s L2 with all four categories\n' "$label" "$BACKEND"
}

available=0
skipped=0

if python=$(runtime_path python3); then
  run_l2 Python python "$python" -S tests/e2e/language-runtimes/python.py
  available=$((available + 1))
else
  echo "SKIP: Python runtime unavailable"
  skipped=$((skipped + 1))
fi

if ruby=$(runtime_path ruby); then
  run_l2 Ruby ruby "$ruby" --disable-gems tests/e2e/language-runtimes/ruby.rb
  available=$((available + 1))
else
  echo "SKIP: Ruby runtime unavailable"
  skipped=$((skipped + 1))
fi

if node=$(runtime_path node nodejs); then
  run_l2 JavaScript javascript "$node" tests/e2e/language-runtimes/javascript.js
  available=$((available + 1))
else
  echo "SKIP: Node.js runtime unavailable"
  skipped=$((skipped + 1))
fi

if java=$(runtime_path java) && javac=$(runtime_path javac); then
  rm -rf "$BUILD_DIR/java"
  mkdir -p "$BUILD_DIR/java"
  "$javac" -d "$BUILD_DIR/java" tests/e2e/language-runtimes/RuntimeProbe.java
  run_l2 Java java "$java" \
    -Xint -XX:+UseSerialGC -XX:ActiveProcessorCount=1 \
    -cp "$BUILD_DIR/java" RuntimeProbe
  available=$((available + 1))
else
  echo "SKIP: complete Java runtime/toolchain unavailable"
  skipped=$((skipped + 1))
fi

if ((available == 0)); then
  echo "FAIL: no supported language runtime was available" >&2
  exit 1
fi

printf 'PASS: %d language runtime(s), 4 categories each, %d unavailable\n' \
  "$available" "$skipped"
