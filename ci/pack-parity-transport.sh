#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Pack one hosted manifest node's results into the deterministic parity-v1
# transport archive that the `regular` job's reducer reads
# (.github/workflows/ci-portable.yml). Every hosted job that runs a manifest
# bucket with portable cells calls this: the e2e matrix jobs and the
# strict-compat job. ci/check-shard-coverage.sh requires each such node to sit
# in one of those jobs.
#
# Usage: ci/pack-parity-transport.sh SLUG NODE
# Environment: E2E_RESULT_ROOT, E2E_RUN_ID, GITHUB_SHA, RUN_KEY
#   (RUN_KEY is "<run id>-<run attempt>"; the archive is
#   parity-v1-<RUN_KEY>-portable-<SLUG>.tar.zst in the working directory).
set -euo pipefail

if (($# != 2)); then
    echo "usage: $0 SLUG NODE" >&2
    exit 2
fi
slug=$1
node=$2
: "${E2E_RESULT_ROOT:?}" "${E2E_RUN_ID:?}" "${GITHUB_SHA:?}" "${RUN_KEY:?}"

job=${node#e2e.}
job=${job%_on_host}
out="$E2E_RESULT_ROOT/portable/$job"
captures="$E2E_RESULT_ROOT/runs/$E2E_RUN_ID"
stage="ignored/parity/$slug/parity-v1"
artifact="parity-v1-$RUN_KEY-portable-$slug.tar.zst"

# A constructed --allow-empty bucket writes an empty results file;
# the final scorecard still verifies the complete selected population.
test -f "$out/results.jsonl"
test -s "$out/junit.xml"
mkdir -p "$stage/captures"
cp "$out/results.jsonl" "$stage/results.jsonl"
cp "$out/junit.xml" "$stage/junit.xml"
cp "$out/summary.json" "$stage/summary.json"
if [[ -d "$captures" ]]; then
    cp -a "$captures/." "$stage/captures/"
fi
jq -n \
    --arg repository_sha "$GITHUB_SHA" \
    --arg lane portable \
    --arg step "$node" \
    --arg run_key "$RUN_KEY" \
    '{schema:1, repository_sha:$repository_sha, run_key:$run_key,
      lane:$lane, step:$step}' \
    >"$stage/index.json"
touch "$stage/complete"
tar --sort=name --mtime='UTC 1970-01-01' \
    --owner=0 --group=0 --numeric-owner \
    --zstd -cf "$artifact" -C "$(dirname "$stage")" parity-v1
sha256sum "$artifact" >"$artifact.sha256"
