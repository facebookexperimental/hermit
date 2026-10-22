#!/usr/bin/env bash
# CI node entrypoint for check.check_outcome_consumers.
#
# WHY THIS WRAPPER EXISTS. The node's command used to be the two checkers run
# directly:
#     ./scripts/test-check-status-outcome.sh && ./scripts/check-merge-gate-policy.sh
# Those checkers report an unevaluable case by printing a NO-RESULT-CASE: line
# and exiting 0, because that is the only shape ci/lint-checks-node.sh can
# classify. With no interpreter here, exiting 0 meant the DAG recorded PASSED.
#
# ⚠️ SO AN UNREACHABLE AUTHORITY WAS RECORDED AS A PASS. Measured 2026-09-04
# against a `gh` returning HTTP 504: the node command exited 0 with two markers.
# At hermit main the same command exits nonzero, so adding the marker channel
# turned this node's red into a false green -- a regression introduced by the
# very change that removed a false red elsewhere. A false green is the worse of
# the two, because nothing investigates it.
#
# The classification rule is SOURCED, not restated; see
# ci/node-run-classification.sh for why one definition matters here specifically.
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
# shellcheck source=ci/node-run-classification.sh
. "$(dirname -- "${BASH_SOURCE[0]}")/node-run-classification.sh"

node_out=$(mktemp)
trap 'rm -f "$node_out"' EXIT

set +e
{ "$root/scripts/test-check-status-outcome.sh" && "$root/scripts/check-merge-gate-policy.sh"; } 2>&1 | tee "$node_out"
pipeline_status=("${PIPESTATUS[@]}")
set -e
run_rc=${pipeline_status[0]}
tee_rc=${pipeline_status[1]}

# The capture is part of the classification input, not incidental logging. If
# tee cannot write it, an empty or partial file could turn a marker-bearing
# no-result into pass. Preserve the checker failure when there is one; otherwise
# make tee's failure the node failure. Either way, never classify incomplete
# evidence.
if [ "$tee_rc" -ne 0 ]; then
    echo "check-outcome-consumers: output capture failed with exit ${tee_rc}" >&2
    if [ "$run_rc" -eq 0 ]; then
        run_rc=$tee_rc
    fi
fi

verdict="$(classify_run "$run_rc" "$node_out")"
case "$verdict" in
    fail*)
        exit "${verdict#fail }"
        ;;
    no_result)
        echo "check-outcome-consumers: NO RESULT -- both checkers completed, and at least" >&2
        echo '  one case could not be evaluated. The unevaluable cases are listed above,' >&2
        echo '  each on a line beginning with the marker below.' >&2
        grep "^${NO_RESULT_MARKER}" "$node_out" | sed 's/^/    /' >&2
        exit 75
        ;;
esac
exit 0
