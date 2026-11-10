#!/usr/bin/env bash
# First-party build entry: local source coherence and runner-admitted width.
# The existing pre.reverie_pin gate owns remote ancestry and monotonicity.
# DAG/safe-ci own the unchanged wall, aggregate CPU, memory and task bounds.
set -euo pipefail
ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
if (($# == 0)); then
    echo "usage: ci/run-with-core-build.sh COMMAND [ARG...]" >&2
    exit 2
fi
if ! recorded_pin=$(
    "$ROOT_DIR/ci/run-reverie-pin-check.sh" --repo "$ROOT_DIR" --print-pin
); then
    echo "run-with-core-build.sh: DECLINED (no_result, exit 75): source pin check failed; wrapped command did not run" >&2
    exit 75
fi
if [[ ! $recorded_pin =~ ^[0-9a-f]{40}$ ]]; then
    echo "run-with-core-build.sh: DECLINED (no_result, exit 75): unidentified Reverie pin; wrapped command did not run" >&2
    exit 75
fi
source "$ROOT_DIR/ci/configure-build-jobs.sh" core-build-child
echo "run-with-core-build.sh: core-build={pin:$recorded_pin,source:$CORE_BUILD_JOBS_SOURCE,raw-build-jobs:$CORE_BUILD_RAW_BUILD_JOBS,child-nproc:$CORE_BUILD_EFFECTIVE_CPUS,basis:runner-admission,node-budgets:unchanged}" >&2
exec "$ROOT_DIR/ci/run-build-command.sh" "$@"
