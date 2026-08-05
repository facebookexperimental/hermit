#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Re-derive the Reverie DBI elapsed budget inside the safe-ci child. Under
# cgroup boxing the runner exports its cap-derived CARGO_BUILD_JOBS immediately
# before this wrapper; on an unboxed hosted runner the launch-time
# CI_DAG_BUILD_JOBS value remains the fallback. Keeping this wrapper immediately
# around Cargo prevents a launcher-side width from standing in for NUM_JOBS.

set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

if (($# == 0)); then
    echo "usage: ci/run-with-reverie-dbi-budget.sh COMMAND [ARG...]" >&2
    exit 2
fi

# shellcheck source=ci/configure-build-jobs.sh
REVERIE_DBI_BUDGET_CHILD=1
export REVERIE_DBI_BUDGET_CHILD
source "$ROOT_DIR/ci/configure-build-jobs.sh"

echo "run-with-reverie-dbi-budget.sh: reverie-dbi-budget={source:$REVERIE_DBI_BUILD_JOBS_SOURCE,raw-build-jobs:$REVERIE_DBI_RAW_BUILD_JOBS,effective-cpus:$CI_DAG_EFFECTIVE_CPUS,reverie-max-jobs:$CI_DAG_REVERIE_DBI_MAX_PARALLEL_JOBS,effective-native-jobs:$REVERIE_DBI_EFFECTIVE_BUILD_JOBS,effective-job-seconds:$CI_DAG_REVERIE_DBI_MAX_BUILD_EFFECTIVE_JOB_SECONDS,max-elapsed-seconds:$REVERIE_DBI_MAX_BUILD_SECONDS,basis:github-portable-cold-miss-n3-affinity4}" >&2

exec "$@"
