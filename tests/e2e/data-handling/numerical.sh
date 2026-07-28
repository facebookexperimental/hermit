#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Reproduce the numerical determinism scenario associated with DetTrace
# (ASPLOS 2020): OpenMP dynamic scheduling changes a floating-point reduction's
# rounding order on native Linux, while Hermit's deterministic thread schedule
# fixes the reduction order.

set -euo pipefail

# shellcheck source=tests/e2e/data-handling/lib/common.bash
source "$(dirname -- "$0")/lib/common.bash"
require_tools cc

target_dir=${CARGO_TARGET_DIR:-$ROOT_DIR/target}
if [[ $target_dir != /* ]]; then
    target_dir=$ROOT_DIR/$target_dir
fi
build_dir=$target_dir/e2e-data-handling
guest=$build_dir/fp-reduction
mkdir -p "$build_dir"

cc -std=c11 -O2 -g -fopenmp -fno-fast-math -fno-tree-vectorize \
    -Wall -Wextra -Werror \
    "$ROOT_DIR/tests/c/fp_reduction_nondeterminism.c" \
    -o "$guest"

export NATIVE_ATTEMPTS=24
assert_nondeterminism_removed openmp-fp-reduction "$guest"
