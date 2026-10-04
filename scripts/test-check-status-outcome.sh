#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
SHELL_CLASSIFIER="$ROOT_DIR/scripts/classify-required-check.sh"
PYTHON_CLASSIFIER="$ROOT_DIR/scripts/check_outcome_adapter.py"

# ⚠️ THE AUTHORITY MUST BE REACHABLE BEFORE ANY CASE HERE MEANS ANYTHING. If it
# is not, every case below is unevaluable, so this declares that and exits 0
# rather than failing. `make lint-checks` then still passes and
# ci/lint-checks-node.sh classifies the run no_result (exit 75), which the
# ledger records as no_result rather than fail.
#
# ⚠️ EXIT 0 IS REQUIRED HERE, NOT MERELY TIDY. classify_run in
# ci/lint-checks-node.sh deliberately lets a real failure outrank any marker --
# "A REAL FAILURE ALWAYS WINS" -- so a nonzero exit could never be reported as
# no_result no matter what this printed. The marker channel only works on a
# passing target.
#
# Only exit 3 from the probe takes this path; see scripts/authority-available.sh
# for why 3 cannot be a refusal, a bug, or a tampered authority.
_auth_rc=0
_authority_dir=$("$ROOT_DIR/scripts/authority-available.sh") || _auth_rc=$?
# ⚠️ ONLY EXIT 3 MAY SKIP, AND AN EARLIER VERSION OF THIS GUARD GOT IT WRONG.
# It treated ANY nonzero from the helper as "unavailable", so a TAMPERED
# authority -- fetched successfully, wrong bytes, AuthorityIntegrityError, exit
# 1 -- was laundered into a no-result skip. That is the most dangerous possible
# reading: the one failure mode the content pin exists to catch, silently
# reported as "could not evaluate". Measured 2026-09-04: checker exited 0 with a
# marker against a deliberately corrupted authority.
if [ "$_auth_rc" -eq 3 ]; then
    echo "NO-RESULT-CASE: test-check-status-outcome.sh: the pinned check-status authority could not be consulted; no case in this checker was evaluated"
    exit 0
elif [ "$_auth_rc" -ne 0 ]; then
    echo "test-check-status-outcome.sh: obtaining the pinned check-status authority failed with exit $_auth_rc; that is not an outage and is not being skipped" >&2
    exit "$_auth_rc"
fi
# ⚠️ EXPORTING THIS IS THE POINT, not tidiness. Every later adapter process in
# this checker now reads the authority from disk instead of fetching it again,
# so a 504 arriving after the check above cannot turn a case into a nonzero
# exit. Probing alone left exactly that window open; see
# scripts/authority-available.sh.
export DEV_HERMIT_PARENT="$_authority_dir"
tmp=""
cleanup() {
    local paths=()
    [[ -n ${_authority_dir:-} ]] && paths+=("$_authority_dir")
    [[ -n ${tmp:-} ]] && paths+=("$tmp")
    [[ ${#paths[@]} -eq 0 ]] || rm -rf -- "${paths[@]}"
}
trap cleanup EXIT

check() {
    local expected=$1 status=$2 conclusion=$3 python_result shell_result
    python_result=$("$PYTHON_CLASSIFIER" --status "$status" --conclusion "$conclusion")
    shell_result=$("$SHELL_CLASSIFIER" "$status" "$conclusion")
    [[ $python_result == "$expected" && $shell_result == "$expected" ]] || {
        echo "mismatch: $status/$conclusion expected=$expected python=$python_result shell=$shell_result" >&2
        exit 1
    }
}

check PASSED completed success
check PASSED "" success
for conclusion in failure timed_out error startup_failure; do
    check FAILED completed "$conclusion"
done
while IFS=: read -r status conclusion; do
    check NO_RESULT "$status" "$conclusion"
done <<'EOF'
completed:cancelled
completed:skipped
completed:neutral
completed:stale
completed:action_required
queued:
in_progress:
waiting:
requested:
pending:
missing:
completed:future_state
EOF

fixture='[{"statusCheckRollup":[{"status":"COMPLETED","conclusion":"CANCELLED"},{"state":"SUCCESS"}]}]'
annotated=$(printf '%s' "$fixture" | "$PYTHON_CLASSIFIER" --annotate-rollups)
[[ $(jq -r '.[0].statusCheckRollup[0]._checkOutcome' <<<"$annotated") == NO_RESULT ]]
[[ $(jq -r '.[0].statusCheckRollup[1]._checkOutcome' <<<"$annotated") == PASSED ]]

# Plant the #1597 shape: two opposite gate conclusions at one exact head.
# Both input orders must select the later run, and a different-head run must
# never enter the verdict.
head_sha=01e5653f2a59fdf5ce090c12aa45e944f7237c3f
older='{"name":"merge-gate","headSha":"'$head_sha'","status":"COMPLETED","conclusion":"FAILURE","startedAt":"2026-08-04T15:12:05Z","detailsUrl":"https://github.com/o/r/actions/runs/30922888575/job/1"}'
newer='{"name":"merge-gate","headSha":"'$head_sha'","status":"COMPLETED","conclusion":"SUCCESS","startedAt":"2026-08-04T15:24:36Z","detailsUrl":"https://github.com/o/r/actions/runs/30923975433/job/2"}'
wrong_head='{"name":"merge-gate","headSha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","status":"COMPLETED","conclusion":"FAILURE","startedAt":"2026-08-04T15:25:00Z","detailsUrl":"https://github.com/o/r/actions/runs/30924000000/job/3"}'
for rollup in "[$older,$newer,$wrong_head]" "[$wrong_head,$newer,$older]"; do
    selected=$(printf '%s' "$rollup" | "$PYTHON_CLASSIFIER" \
        --select-latest-rollup --head-sha "$head_sha")
    [[ $(jq 'length' <<<"$selected") -eq 1 ]]
    [[ $(jq -r '.[0].conclusion' <<<"$selected") == SUCCESS ]]
done
mkdir -p "$ROOT_DIR/ignored"
tmp=$(mktemp -d "$ROOT_DIR/ignored/check-status-outcome.XXXXXX")

# Prove that local and fetched authority bytes must match the reviewed digest.
# The source fixture lives under the checkout because Hermit replaces /tmp in
# guests; tests must not hide their inputs there.
ROOT_DIR="$ROOT_DIR" TEST_TMP="$tmp" python3 - <<'PY'
import hashlib
import os
from pathlib import Path
import sys

root = Path(os.environ["ROOT_DIR"])
test_tmp = Path(os.environ["TEST_TMP"])
sys.path.insert(0, str(root / "scripts"))
import check_outcome_adapter as adapter

source = adapter._verified_source()
assert hashlib.sha256(source).hexdigest() == adapter.AUTHORITY_SHA256
(test_tmp / "pinned-authority.py").write_bytes(source)

parent = test_tmp / "changed-parent"
authority = parent / adapter.AUTHORITY_RELATIVE_PATH
authority.parent.mkdir(parents=True)
authority.write_text("raise RuntimeError('unreviewed local authority executed')\n")
os.environ["DEV_HERMIT_PARENT"] = str(parent)
adapter._fetch_pinned_source = lambda: source
assert adapter._verified_source() == source

adapter._fetch_pinned_source = lambda: source + b"# changed\n"
try:
    adapter._verified_source()
except RuntimeError as error:
    assert "digest mismatch" in str(error)
else:
    raise AssertionError("changed fetched authority passed its content pin")
PY

# Execute every real consumer with deterministic GitHub responses. The fixture
# also provides the authenticated gh-api fallback used when the explicit parent
# does not contain the authority.
mkdir -p "$tmp/bin"
PINNED_AUTHORITY="$tmp/pinned-authority.py"
NETWORK_MARKER="$tmp/network-called"
export PINNED_AUTHORITY NETWORK_MARKER
tee "$tmp/bin/with-proxy" >/dev/null <<'EOF_PROXY'
#!/usr/bin/env bash
set -euo pipefail
: >"$NETWORK_MARKER"
exec "$@"
EOF_PROXY
tee "$tmp/bin/gh" >/dev/null <<'EOF_GH'
#!/usr/bin/env bash
set -euo pipefail

main_sha=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
case "$1:$2" in
    api:repos/rrnewton/dev-hermit/contents/ci-hub/check_outcome.py\?ref=4b78d727f35bc8612ac460a6e270dda5f5df304c)
        cat "$PINNED_AUTHORITY"
        ;;
    pr:list)
        printf '%s\n' '[{"number":2363,"title":"fixture","url":"https://github.com/rrnewton/hermit/pull/2363","headRefName":"fixture","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","baseRefName":"main","labels":[],"isDraft":false,"author":{"login":"fixture"},"mergeable":"MERGEABLE","mergeStateStatus":"CLEAN","statusCheckRollup":[{"name":"merge-gate-v4","headSha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"https://github.com/o/r/actions/runs/2/job/1"}]}]'
        ;;
    api:repos/rrnewton/hermit/commits/main)
        printf '%s\n' "$main_sha"
        ;;
    api:repos/rrnewton/hermit/commits/main/check-runs)
        printf '%s\n' '{"check_runs":[{"name":"Regular tests (GitHub-managed portable)","head_sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","status":"completed","conclusion":"success","details_url":"https://github.com/o/r/actions/runs/3/job/1"}]}'
        ;;
    *)
        printf 'unexpected gh fixture command:' >&2
        printf ' %q' "$@" >&2
        printf '\n' >&2
        exit 2
        ;;
esac
EOF_GH
chmod +x "$tmp/bin/with-proxy" "$tmp/bin/gh"
PATH="$tmp/bin:$PATH"
export PATH

# pr_status imports the adapter before parsing arguments. --help must succeed
# without touching either the changed parent authority or the network fallback.
rm -f "$NETWORK_MARKER"
DEV_HERMIT_PARENT="$tmp/changed-parent" python3 "$ROOT_DIR/scripts/pr_status.py" --help >/dev/null
[[ ! -e $NETWORK_MARKER ]] || {
    echo "pr_status.py loaded the authority during import" >&2
    exit 1
}

# A real shell consumer must use the authenticated immutable fallback when its
# explicitly selected parent does not contain reviewed bytes.
rm -f "$NETWORK_MARKER"
fallback=$(
    DEV_HERMIT_PARENT="$tmp/changed-parent" "$SHELL_CLASSIFIER" completed success
)
[[ $fallback == PASSED && -e $NETWORK_MARKER ]] || {
    echo "classify-required-check.sh did not use the pinned gh-api fallback" >&2
    exit 1
}

pr_status=$(
    DEV_HERMIT_PARENT="$tmp/changed-parent" python3 "$ROOT_DIR/scripts/pr_status.py" --repo rrnewton/hermit --no-main-ci
)
grep -Fq 'rrnewton/hermit#2363' <<<"$pr_status"
grep -Fq 'ci=green' <<<"$pr_status"

dag_health=$(
    DEV_HERMIT_PARENT="$tmp/changed-parent" PR_DAG_PROXY="" "$ROOT_DIR/scripts/pr-dag-health.sh" --repo rrnewton/hermit --format json --no-commute
)
jq -e '
    .prs[0].number == 2363 and
    .prs[0].ci.overall == "PASSED" and
    .main.head == "bbbbbbbbbbbb" and
    .main.outcome == "PASSED" and
    .main.green == true
' <<<"$dag_health" >/dev/null

# Exercise the same cleanup used on every early exit, then prove neither
# temporary tree survived. Installing a second EXIT trap used to replace the
# first one and leak the materialized authority directory on every passing run.
authority_dir_for_cleanup_check=$_authority_dir
fixture_dir_for_cleanup_check=$tmp
cleanup
[[ ! -e $authority_dir_for_cleanup_check && ! -e $fixture_dir_for_cleanup_check ]] || {
    echo "test-check-status-outcome.sh: cleanup left a temporary directory behind" >&2
    exit 1
}
trap - EXIT

echo "PASS: lazy content pin and real classify-required-check, pr_status, and pr-dag-health consumers"
