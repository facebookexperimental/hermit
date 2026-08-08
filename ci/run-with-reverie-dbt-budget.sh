#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Re-derive the Reverie DBT elapsed budget inside the safe-ci child. Under
# cgroup boxing the runner exports its cap-derived CARGO_BUILD_JOBS immediately
# before this wrapper; on an unboxed hosted runner the launch-time
# CI_DAG_BUILD_JOBS value remains the fallback. Keeping this wrapper immediately
# around Cargo prevents a launcher-side width from standing in for NUM_JOBS.

set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

if (($# == 0)); then
    echo "usage: ci/run-with-reverie-dbt-budget.sh COMMAND [ARG...]" >&2
    exit 2
fi

# Bind the calibration to the exact local Reverie revision before applying it.
# --print-pin is deliberately offline: the separate latest-main gate owns the
# network authority, while this check prevents a pin bump from silently reusing
# an earlier revision's clamp and measured threshold.
expected_pin=fb963d90dc6c5a136cfff23d3e898ab06f8cb265
recorded_pin=$(
    "$ROOT_DIR/ci/run-reverie-pin-check.sh" --repo "$ROOT_DIR" --print-pin
)
if [[ $recorded_pin != "$expected_pin" ]]; then
    echo "run-with-reverie-dbt-budget.sh: no calibrated budget for Reverie pin $recorded_pin (expected $expected_pin)" >&2
    exit 2
fi
REVERIE_DBT_BUDGET_BOUND_PIN=$recorded_pin
export REVERIE_DBT_BUDGET_BOUND_PIN

# shellcheck source=ci/configure-build-jobs.sh
source "$ROOT_DIR/ci/configure-build-jobs.sh" reverie-dbt-budget-child

echo "run-with-reverie-dbt-budget.sh: reverie-dbt-budget={pin:$REVERIE_DBT_BUDGET_BOUND_PIN,source:$REVERIE_DBT_BUILD_JOBS_SOURCE,raw-build-jobs:$REVERIE_DBT_RAW_BUILD_JOBS,effective-cpus-source:$REVERIE_DBT_EFFECTIVE_CPUS_SOURCE,effective-cpus:$REVERIE_DBT_EFFECTIVE_CPUS,reverie-max-jobs:$REVERIE_DBT_MAX_PARALLEL_JOBS,effective-native-jobs:$REVERIE_DBT_EFFECTIVE_BUILD_JOBS,effective-job-seconds:$REVERIE_DBT_MAX_BUILD_EFFECTIVE_JOB_SECONDS,max-elapsed-seconds:$REVERIE_DBT_MAX_BUILD_SECONDS,basis:github-portable-cold-miss-n3-affinity4,carried-to-pin-on-dynamorio-recipe-key:019b79670b3572c1afc2690932dd3fbbf70bbc9d0d96b5086ea121422de4bbb9}" >&2

exec "$@"
