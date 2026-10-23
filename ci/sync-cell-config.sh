#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# sync-cell-config.sh — regenerate everything a backend cell flip derives.
#
# A cell flip makes one existing manifest cell (test, mode, backend) required:
# in tests/e2e/manifests/<category>.yaml its backend joins the mode's
# `backends_enabled` and `ci` map (dropping its `backends_disabled` reason), or
# its `ci: false` becomes true. That manifest edit is the only hand edit. This
# script then rewrites, in dependency order,
#
#   test-harness sync-cells      ci/expected-e2e-plan.json, ci/optional-e2e-cells.txt
#   generate-parity-cells        ci/compat-envelope/parity-cells.json (the parity
#                                population, derived from the manifests)
#   scorecard.rs update          SCORECARD.md, ci/compat-envelope/cells.json
#   generate-validation-dag      ci/dag/validate.json
#   generate-test-footprints     checked only; a flip does not move a footprint
#
# The diff of ci/expected-e2e-plan.json is the record of the change to the
# required cells; validation refuses any of these files that drifts from its
# generator (https://github.com/rrnewton/hermit/issues/3606).

set -uo pipefail

usage() {
    cat <<'EOF'
Usage: ci/sync-cell-config.sh [--check]

Regenerate every file derived from the E2E manifests after a cell flip: the
expected plan, the optional-cell inventory, parity-cells.json, the scorecard and cell
table, and the validation DAG. Edit the manifest, run this,
and commit the manifest with the files it rewrites.

Options:
  --check      Write nothing; exit 1 naming each step whose files are stale
  -h, --help   Print this help

Each step runs under its own timeout. Exit status: 0 when every file is
current (--check) or was regenerated, 1 when a step is stale, refused, failed
or timed out, 2 for a usage error.

Environment:
  CARGO_TARGET_DIR   Where the generators are built (default: <checkout>/target)
EOF
}

check=0
case "$#:${1:-}" in
    0:) ;;
    1:--check) check=1 ;;
    1:-h | 1:--help)
        usage
        exit 0
        ;;
    *)
        echo "sync-cell-config: unexpected arguments: $*; see ci/sync-cell-config.sh --help" >&2
        exit 2
        ;;
esac

root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel) || {
    echo "sync-cell-config: $(dirname "$0") is not inside a git checkout; run the script from a Hermit checkout" >&2
    exit 1
}
target=${CARGO_TARGET_DIR:-$root/target}
# Every step runs from the checkout root and scorecard.rs wants an absolute
# helper path, so resolve a relative target directory as cargo would, against
# the caller's directory, and hand cargo the same absolute path.
[[ $target == /* ]] || target=$PWD/$target
export CARGO_TARGET_DIR=$target
bin=$target/debug

# step SECONDS LABEL COMMAND... — run one step from the checkout root under a
# timeout, report its wall time, and say what a failure means.
failed=()
step() {
    local limit=$1 label=$2
    shift 2
    local start=$SECONDS
    env -C "$root" timeout --signal=INT --kill-after=10 "$limit" "$@"
    local rc=$?
    local took=$((SECONDS - start))
    case $rc in
        0) echo "sync-cell-config: ${label}: ok (${took} s)" ;;
        124 | 137)
            echo "sync-cell-config: ${label}: TIMED OUT after ${limit} s; nothing after it ran" >&2
            failed+=("$label")
            ;;
        126 | 127)
            echo "sync-cell-config: ${label}: could not execute $1 (exit $rc)" >&2
            failed+=("$label")
            ;;
        *)
            echo "sync-cell-config: ${label}: exit $rc (${took} s)" >&2
            failed+=("$label")
            ;;
    esac
    # A write step's successors read its output, so stop at its failure.
    if [[ $rc -ne 0 && $check -eq 0 ]]; then
        echo "sync-cell-config: stopped; fix ${label} and rerun. Files earlier steps rewrote are left in place; \`git status\` lists them." >&2
        exit 1
    fi
}

step 1800 build \
    cargo build -q -p hermit-manifest-plan --bin test-harness --bin generate-parity-cells \
    --bin generate-validation-dag --bin generate-test-footprints --bin hermit-manifest-plan
if [[ ${#failed[@]} -ne 0 ]]; then
    exit 1
fi

if [[ $check -eq 1 ]]; then
    mode=--check scorecard=check
else
    mode=--write scorecard=update
fi
step 120 "test-harness sync-cells" "$bin/test-harness" sync-cells "$mode" --repo-root "$root"
step 120 generate-parity-cells "$bin/generate-parity-cells" "$mode"
step 900 "scorecard.rs $scorecard" \
    env HERMIT_MANIFEST_PLAN_BIN="$bin/hermit-manifest-plan" ./ci/compat-envelope/scorecard.rs "$scorecard"
step 300 generate-validation-dag "$bin/generate-validation-dag" "$mode"
step 120 "generate-test-footprints --check" "$bin/generate-test-footprints" --check

if [[ ${#failed[@]} -ne 0 ]]; then
    echo "sync-cell-config: stale or failing: ${failed[*]}; run ci/sync-cell-config.sh to regenerate" >&2
    exit 1
fi
if [[ $check -eq 1 ]]; then
    echo "sync-cell-config: every derived file is current"
else
    changed=$(git -C "$root" status --porcelain | awk '{print $2}' | tr '\n' ' ')
    if [[ -z $changed ]]; then
        echo "sync-cell-config: every derived file already matched the manifests; the checkout is unchanged"
    else
        echo "sync-cell-config: regenerated; commit together: $changed"
    fi
fi
