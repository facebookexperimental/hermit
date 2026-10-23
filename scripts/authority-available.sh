#!/usr/bin/env bash
# Obtain the pinned authority ONCE and print a directory to use as
# DEV_HERMIT_PARENT, so every later adapter invocation in this checker reads it
# locally instead of fetching it again.
#
# Usage: authority-available.sh [ADAPTER...]     (default: check_outcome_adapter.py)
# Exit 0  prints the directory on stdout; the caller exports DEV_HERMIT_PARENT.
# Exit 3  the authority could not be obtained. The caller has learned nothing
#         about any check and must declare its cases unevaluable.
#
# ⚠️ PROBING WAS NOT ENOUGH, AND THAT WAS A REAL DEFECT IN THE FIRST VERSION OF
# THIS FIX. It fetched once to answer "reachable?" and then let each checker
# launch fresh adapter processes that fetched again, many times. A 504 arriving
# after the probe still made the checker exit nonzero, and classify_run in
# ci/lint-checks-node.sh correctly ranks a real failure above any marker -- so
# the node went red anyway. The independent codex lane reproduced exactly that
# ordering at head 327f6713 with a substitute for `with-proxy` returning the
# authority on call 1 and HTTP 504 on call 2, and the change's own tests could
# not see it because both directions held availability constant.
#
# Obtaining once removes the window rather than narrowing it: after this
# succeeds there are no further fetches to fail.
#
# ⚠️ ONLY EXIT 3 MEANS UNAVAILABLE. Both adapters reserve 3 for
# AuthorityUnavailable alone: 0 is success, 1 an ordinary error, 2 argparse
# usage, and a digest mismatch raises AuthorityIntegrityError instead. So a
# caller that skips on 3 cannot be skipping a refusal, a bug, or a tampered
# authority -- and materialising cannot launder one, because the digest is
# verified before anything is written.
set -uo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
if [ "$#" -eq 0 ]; then
    set -- "$root/scripts/check_outcome_adapter.py"
fi

dir=$(mktemp -d) || exit 3
for adapter in "$@"; do
    # ⚠️ CAPTURE THE STATUS BEFORE ANYTHING ELSE RUNS. An earlier version put
    # `rm -rf` first and then read $?, which is rm's status, not the adapter's --
    # so a genuine 504 was reported as "failed with exit 0" and the caller
    # proceeded with an empty directory. Same family as reading $? after a pipe.
    "$adapter" --materialize-authority "$dir" >/dev/null 2>"$dir.err"
    rc=$?
    if [ "$rc" -ne 0 ]; then
        cat "$dir.err" >&2
        rm -rf "$dir" "$dir.err"
        # Anything other than 3 is a real problem and must not be reported as
        # an outage; surface it rather than letting the caller skip.
        if [ "$rc" -ne 3 ]; then
            echo "authority-available.sh: $adapter failed with exit $rc" >&2
            exit "$rc"
        fi
        exit 3
    fi
    rm -f "$dir.err"
done
printf '%s\n' "$dir"
