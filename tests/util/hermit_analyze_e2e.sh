#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
readonly ROOT_DIR
cd "$ROOT_DIR"

TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT_DIR/target}"
if [[ "$TARGET_DIR" != /* ]]; then
    TARGET_DIR="$ROOT_DIR/$TARGET_DIR"
fi
readonly TARGET_DIR

readonly HERMIT="$TARGET_DIR/debug/hermit"
readonly HELLO_RACE="$TARGET_DIR/debug/hello_race"
readonly RACEWRITE="$TARGET_DIR/debug/racewrite_nostdlib"
readonly ANALYZE_DRIVER="$ROOT_DIR/tests/util/hermit_analyze_test.sh"
readonly ANALYZE_TIMEOUT="${ANALYZE_TIMEOUT:-300s}"

echo ":: [schedule_search] Building Hermit and analyzer fixtures"
cargo build -p hermit --bin hermit
cargo build -p hermetic_infra_hermit_flaky-tests --bin hello_race
mkdir -p "$TARGET_DIR/debug"
"${CC:-cc}" -g -nostdlib \
    "$ROOT_DIR/tests/c/simple/racewrite_nostdlib.c" \
    -o "$RACEWRITE"

echo ":: [schedule_search] Localizing hello_race"
# Neither fixture uses the network. Host mode avoids requiring a network/sysfs
# namespace on otherwise PMU-capable CI runners.
ANALYZE_OPTS="--run-arg=--network=host" \
    EXPECTED_OUTPUT="flaky-tests/hello_race.rs:37" \
    timeout "$ANALYZE_TIMEOUT" \
    "$ANALYZE_DRIVER" "$HERMIT" "$HELLO_RACE"

echo ":: [schedule_search] Localizing racewrite_nostdlib"
ANALYZE_OPTS="--run-arg=--network=host --run-arg=--base-env=empty --target-exit-code=0 --target-stdout=foobar" \
    EXPECTED_OUTPUT="tests/c/simple/racewrite_nostdlib.c:35" \
    timeout "$ANALYZE_TIMEOUT" \
    "$ANALYZE_DRIVER" "$HERMIT" "$RACEWRITE"
