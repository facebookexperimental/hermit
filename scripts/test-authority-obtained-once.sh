#!/usr/bin/env bash
# Prove no guarded checker fetches the pinned authority AFTER obtaining it.
#
# ⚠️ THIS ASKS WHETHER THE RACE CAN OCCUR, NOT WHETHER IT DID. The first version
# of this fix was verified by running a flaky substitute and observing a good
# outcome, which only samples. An intermittent defect that does not reproduce is
# not a defect that is gone.
#
# The method removes the sampling: the authority is obtained up front, and every
# checker then runs with a `gh` that returns HTTP 504 on EVERY call and counts
# its invocations. If any path still reaches the real authority it is counted and
# the checker fails. A zero count is evidence about the path, not about a run.
#
# The defect this guards, reported by the independent codex lane at head
# 327f6713: a preflight that fetched once and returned success, while each
# checker then launched fresh adapter processes that fetched again -- so a 504
# arriving after the probe still exited the checker nonzero, and
# ci/lint-checks-node.sh::classify_run correctly ranks that above any marker.
#
# BASELINE, AND ITS LIMIT. This test cannot be run against the refused head
# 327f6713 -- it depends on --materialize-authority, which that head lacks, so it
# honestly reports itself unevaluable there rather than passing vacuously. The
# property was therefore baselined directly instead, by reverting the five
# checker/helper files to 327f6713 and keeping the new adapters so a payload
# could be staged: the refused head exits the checker 1 with TWO fetches, the
# fixed head exits 0 with exactly ONE.
set -uo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
work=$(mktemp -d) || exit 1
authority=""
cleanup() {
    rm -rf -- "$work"
    if [ -n "$authority" ]; then
        rm -rf -- "$authority"
    fi
}
trap cleanup EXIT

# A `gh` that serves the pinned authority on its FIRST call and returns HTTP 504
# on every call after, counting each. This is the ordering the codex lane
# reported, and it is what makes the test discriminating: the checker must be
# able to obtain once, and must then need no further fetch.
#
# ⚠️ AN EARLIER DRAFT OF THIS TEST PRE-SET DEV_HERMIT_PARENT FOR THE CHECKER AND
# WAS THEREFORE VACUOUS -- the adapter read the authority locally whether or not
# the checker obtained and exported anything, so the unfixed code would have
# passed it too. The checker must do its own obtain for this to test the fix.
mkdir -p "$work/bin"
cat > "$work/bin/gh" <<'STUB'
#!/usr/bin/env bash
n=$(wc -c < "$OBTAINED_ONCE_GH_CALLS")
printf 'x' >> "$OBTAINED_ONCE_GH_CALLS"
if [ "$n" -eq 0 ]; then
    exec cat "$OBTAINED_ONCE_PAYLOAD"
fi
echo "gh: HTTP 504 Gateway Timeout" >&2
exit 1
STUB
chmod +x "$work/bin/gh"

# Obtain both authorities first, with the real environment. Use the same helper
# as the guarded checkers so only its reserved exit 3 means unavailable. A
# reachable refusal, a digest mismatch, or an ordinary adapter error must stay
# nonzero; otherwise this test itself would turn a real lint failure into the
# no-result marker that check.lint_checks trusts.
authority_rc=0
authority=$("$ROOT_DIR/scripts/authority-available.sh" \
    "$ROOT_DIR/scripts/check_outcome_adapter.py" \
    "$ROOT_DIR/scripts/review_contract_adapter.py") || authority_rc=$?
if [ "$authority_rc" -eq 3 ]; then
    echo "NO-RESULT-CASE: test-authority-obtained-once.sh: the pinned authorities could not be obtained, so the no-later-fetch property could not be tested"
    exit 0
elif [ "$authority_rc" -ne 0 ]; then
    echo "test-authority-obtained-once.sh: obtaining the pinned authorities failed with exit $authority_rc; that is not an outage and is not being skipped" >&2
    exit "$authority_rc"
fi

failures=0
# ⚠️ THE LIST IS CHECKED AGAINST THE TARGET, NOT JUST WRITTEN DOWN. A hard-coded
# list is exactly where a newly added authority consumer goes untested: the new
# checker fetches, nobody notices, and the race is reachable again through it.
# So every script named in the lint-checks recipe that references an authority
# adapter must appear below, and this fails if one does not.
GUARDED="test-required-check-outcomes.sh test-check-status-outcome.sh check-merge-gate-policy.sh core-review-protocol-lint-test.sh test_pr_status.py"
# Two scripts reference an adapter without being consumers of its verdict: they
# are tests OF the adapter and deliberately drive it into states, including
# unreachable ones, that this property does not apply to. Exempting them is
# stated here rather than achieved by leaving them off the list silently.
EXEMPT="test-authority-obtained-once.sh test_check_outcome_adapter_authority.py"
consumers=$(sed -n '/^lint-checks:/,/^$/p' "$ROOT_DIR/Makefile" \
    | grep -oE 'scripts/[A-Za-z0-9_.-]+' | sort -u)
missing=0
for consumer in $consumers; do
    file="$ROOT_DIR/$consumer"
    [ -f "$file" ] || continue
    grep -qE 'check_outcome_adapter|review_contract_adapter|classify-required-check' "$file" || continue
    base=$(basename "$consumer")
    case " $EXEMPT " in
        *" $base "*) continue ;;
    esac
    case " $GUARDED " in
        *" $base "*) ;;
        *)
            echo "FAIL: ${base} is in lint-checks and consults an authority adapter, but is not covered here" >&2
            missing=$((missing + 1))
            ;;
    esac
done
if [ "$missing" -ne 0 ]; then
    exit 1
fi

for checker in $GUARDED; do
    calls="$work/calls"
    checker_runner=()
    case "$checker" in *.py) checker_runner=(python3) ;; esac
    : > "$calls"
    # DEV_HERMIT_PARENT is deliberately pointed at an EMPTY directory: the
    # checker must obtain the authority itself, which is the behaviour under
    # test. Pointing it at the materialised copy would test the adapter instead.
    empty="$work/empty"
    rm -rf "$empty"; mkdir -p "$empty"
    payload="$authority/ci-hub/check_outcome.py"
    case "$checker" in core-review-protocol-lint-test.sh)
        payload="$authority/ci-hub/review_contract.py" ;;
    esac
    if ! env PATH="$work/bin:$PATH" DEV_HERMIT_PARENT="$empty" \
            OBTAINED_ONCE_PAYLOAD="$payload" \
            OBTAINED_ONCE_GH_CALLS="$calls" \
            "${checker_runner[@]}" \
            "$ROOT_DIR/scripts/$checker" >"$work/out" 2>&1; then
        echo "FAIL: ${checker} did not complete with the authority already obtained" >&2
        tail -5 "$work/out" >&2
        failures=$((failures + 1))
        continue
    fi
    # ⚠️ THE ORACLE MUST SEE A CHECKER THAT EVALUATED NOTHING. rc=0 and a low
    # fetch count are also what a checker that skipped every case looks like,
    # and a skip is CHEAPER than the behaviour under test -- it makes zero
    # fetches. Without this arm the test passes hardest exactly when the checker
    # did least. The authority is obtainable here, so a marker means the checker
    # declined to evaluate and the property was never exercised.
    if grep -q '^NO-RESULT-CASE:' "$work/out"; then
        echo "FAIL: ${checker} skipped its cases while the authority WAS obtainable; this test proved nothing about it" >&2
        failures=$((failures + 1))
        continue
    fi
    n=$(wc -c < "$calls")
    if [ "$n" -ne 1 ]; then
        echo "FAIL: ${checker} made ${n} authority fetches, expected exactly 1; zero bypasses the planted first-success/later-504 ordering, and more than one reopens the race" >&2
        failures=$((failures + 1))
    fi
done

# ⚠️ EXIT ZERO ALONE CANNOT DISTINGUISH AN OUTAGE FROM A CHECKER THAT DID
# NOTHING. Exercise each pinned-authority adapter through a real guarded
# checker and assert both parts of the result: the checker's exit status and
# the exact number of no-result markers. `gh` exits 1 for every response below;
# only a positively identified transport failure may become an unevaluable
# checker. A 404 or 403 reached the authority and received an answer, so each
# must stay a loud refusal with no marker.
response_bin="$work/response/bin"
mkdir -p "$response_bin"
# Preserve the normal toolset while removing both real network entrypoints.
# An empty PATH would also hide bash, python3 and the utilities used before a
# checker reaches its authority guard, so it would exercise nothing.
for source_dir in /usr/bin /bin /usr/local/bin "${HOME:-}/.cargo/bin"; do
    [ -d "$source_dir" ] || continue
    for source_path in "$source_dir"/*; do
        [ -e "$source_path" ] || [ -L "$source_path" ] || continue
        name=${source_path##*/}
        [ "$name" = gh ] && continue
        [ "$name" = with-proxy ] && continue
        [ -e "$response_bin/$name" ] || [ -L "$response_bin/$name" ] ||
            ln -s "$source_path" "$response_bin/$name"
    done
done
cat > "$response_bin/gh" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$AUTHORITY_RESPONSE" >&2
exit 1
STUB
chmod +x "$response_bin/gh"

check_response() {
    local checker="$1" name="$2" response="$3" want_rc="$4" want_markers="$5"
    local empty="$work/response/empty-${checker}-${name}"
    local out="$work/response/${checker}-${name}.out"
    local rc marker_count
    local -a runner=()
    case "$checker" in *.py) runner=(python3) ;; esac
    mkdir -p "$empty"
    env PATH="$response_bin" DEV_HERMIT_PARENT="$empty" \
        AUTHORITY_RESPONSE="$response" \
        "${runner[@]}" "$ROOT_DIR/scripts/$checker" >"$out" 2>&1
    rc=$?
    marker_count=$(grep -c '^NO-RESULT-CASE:' "$out" || true)
    if [ "$rc" -ne "$want_rc" ]; then
        echo "FAIL: ${checker} under ${name} exited ${rc}, expected ${want_rc}" >&2
        tail -5 "$out" >&2
        failures=$((failures + 1))
    fi
    if [ "$marker_count" -ne "$want_markers" ]; then
        echo "FAIL: ${checker} under ${name} emitted ${marker_count} no-result markers, expected ${want_markers}" >&2
        tail -5 "$out" >&2
        failures=$((failures + 1))
    fi
}

for checker in $GUARDED; do
    check_response "$checker" 'HTTP-504' \
        'gh: HTTP 504 Gateway Timeout' 0 1
    check_response "$checker" 'dial-tcp-socket' \
        'Get "https://127.0.0.1/api/v3/": dial tcp 127.0.0.1:443: socket: operation not permitted' 0 1
    check_response "$checker" 'gh-connect-hint' \
        'error connecting to api.github.com; check your internet connection or https://www.githubstatus.com' 0 1
    check_response "$checker" 'dial-tcp-dns' \
        'Get "https://api.github.com/": dial tcp: lookup api.github.com: no such host' 0 1
    check_response "$checker" 'HTTP-404' \
        'gh: Not Found (HTTP 404)' 1 0
    check_response "$checker" 'HTTP-403' \
        'gh: HTTP 403: Resource not accessible by integration' 1 0
    check_response "$checker" 'unknown-error' \
        'gh: unexpected command failure' 1 0
done

# The validation DAG does not invoke the two checkers above directly. Its node
# wrapper must translate their successful marker-bearing outage result to 75,
# while leaving refusals nonzero. Otherwise the same 504 that is no longer red
# would be recorded as passed instead.
check_node_response() {
    local name="$1" response="$2" want_rc="$3" want_markers="$4"
    local empty="$work/response/node-empty-${name}"
    local out="$work/response/node-${name}.out"
    local rc marker_count
    mkdir -p "$empty"
    env PATH="$response_bin" DEV_HERMIT_PARENT="$empty" \
        AUTHORITY_RESPONSE="$response" \
        "$ROOT_DIR/ci/check-outcome-consumers-node.sh" >"$out" 2>&1
    rc=$?
    marker_count=$(grep -c '^NO-RESULT-CASE:' "$out" || true)
    if [ "$rc" -ne "$want_rc" ]; then
        echo "FAIL: check-outcome-consumers-node under ${name} exited ${rc}, expected ${want_rc}" >&2
        tail -5 "$out" >&2
        failures=$((failures + 1))
    fi
    if [ "$marker_count" -ne "$want_markers" ]; then
        echo "FAIL: check-outcome-consumers-node under ${name} emitted ${marker_count} no-result markers, expected ${want_markers}" >&2
        tail -5 "$out" >&2
        failures=$((failures + 1))
    fi
}

check_node_response 'HTTP-504' 'gh: HTTP 504 Gateway Timeout' 75 2
check_node_response 'dial-tcp-socket' \
    'Get "https://127.0.0.1/api/v3/": dial tcp 127.0.0.1:443: socket: operation not permitted' 75 2
check_node_response 'gh-connect-hint' \
    'error connecting to api.github.com; check your internet connection or https://www.githubstatus.com' 75 2
check_node_response 'dial-tcp-dns' \
    'Get "https://api.github.com/": dial tcp: lookup api.github.com: no such host' 75 2
check_node_response 'HTTP-404' 'gh: Not Found (HTTP 404)' 1 0
check_node_response 'HTTP-403' \
    'gh: HTTP 403: Resource not accessible by integration' 1 0
check_node_response 'unknown-error' 'gh: unexpected command failure' 1 0

# Negative arms are insufficient if the wrapper can pass them by failing every
# invocation. With both verified authorities local and no fetch needed, both
# real checkers must complete and the wrapper must return pass with no marker.
correct_node_out="$work/response/node-correct-authority.out"
env PATH="$response_bin" DEV_HERMIT_PARENT="$authority" \
    "$ROOT_DIR/ci/check-outcome-consumers-node.sh" >"$correct_node_out" 2>&1
correct_node_rc=$?
correct_node_markers=$(grep -c '^NO-RESULT-CASE:' "$correct_node_out" || true)
if [ "$correct_node_rc" -ne 0 ]; then
    echo "FAIL: check-outcome-consumers-node with correct authorities exited ${correct_node_rc}, expected 0" >&2
    tail -5 "$correct_node_out" >&2
    failures=$((failures + 1))
fi
if [ "$correct_node_markers" -ne 0 ]; then
    echo "FAIL: check-outcome-consumers-node with correct authorities emitted ${correct_node_markers} no-result markers, expected 0" >&2
    tail -5 "$correct_node_out" >&2
    failures=$((failures + 1))
fi

# A marker is useful only if the node can inspect the file tee is supposed to
# write. Plant a tee that forwards output but fails without writing that file:
# the markers remain visible to a human, but the node must reject its incomplete
# classification input rather than reporting pass.
rm -f "$response_bin/tee"
cat > "$response_bin/tee" <<'STUB'
#!/usr/bin/env bash
cat
exit 1
STUB
chmod +x "$response_bin/tee"
check_node_response 'tee-write-error' 'gh: HTTP 504 Gateway Timeout' 1 2

# ci/lint-checks-node.sh is the other consumer of the same classification
# helper. Exercise the actual wrapper with clean submodule inventory, a make
# target that succeeds after emitting a marker, and the same failing tee. The
# marker is visible on stdout but absent from the capture file, so ignoring
# tee's status would incorrectly return pass.
rm -f "$response_bin/git" "$response_bin/make"
cat > "$response_bin/git" <<'STUB'
#!/usr/bin/env bash
if [ "${1:-}" = submodule ] && [ "${2:-}" = status ]; then
    echo ' abc123 agent-utils'
    exit 0
fi
echo "unexpected git invocation: $*" >&2
exit 2
STUB
cat > "$response_bin/make" <<'STUB'
#!/usr/bin/env bash
if [ "${1:-}" = lint-checks ]; then
    echo 'NO-RESULT-CASE: planted lint-checks marker'
    exit 0
fi
echo "unexpected make invocation: $*" >&2
exit 2
STUB
chmod +x "$response_bin/git" "$response_bin/make"
lint_node_out="$work/response/lint-node-tee-write-error.out"
env PATH="$response_bin" "$ROOT_DIR/ci/lint-checks-node.sh" >"$lint_node_out" 2>&1
lint_node_rc=$?
lint_node_markers=$(grep -c '^NO-RESULT-CASE:' "$lint_node_out" || true)
if [ "$lint_node_rc" -ne 1 ]; then
    echo "FAIL: lint-checks-node with failed output capture exited ${lint_node_rc}, expected 1" >&2
    tail -5 "$lint_node_out" >&2
    failures=$((failures + 1))
fi
if [ "$lint_node_markers" -ne 1 ]; then
    echo "FAIL: lint-checks-node with failed output capture emitted ${lint_node_markers} visible no-result markers, expected 1" >&2
    tail -5 "$lint_node_out" >&2
    failures=$((failures + 1))
fi

# ⚠️ AN INTEGRITY FAILURE MUST NOT BECOME A SKIP. Serving a CORRECTLY REACHABLE
# but WRONG authority is the one failure the content pin exists to catch. An
# earlier version of the guard treated any nonzero from the helper as
# "unavailable", so a tampered authority exited the checker 0 with a no-result
# marker -- measured 2026-09-04. If this ever passes with a marker again, the
# pin has been turned into a suggestion.
tamper="$work/tamper"
mkdir -p "$tamper/bin"
cat "$authority/ci-hub/check_outcome.py" > "$tamper/payload"
printf '# tampered\n' >> "$tamper/payload"
cat > "$tamper/bin/gh" <<STUB
#!/usr/bin/env bash
exec cat "$tamper/payload"
STUB
chmod +x "$tamper/bin/gh"
mkdir -p "$tamper/empty"
tamper_out="$work/tamper.out"
if env PATH="$tamper/bin:$PATH" DEV_HERMIT_PARENT="$tamper/empty" \
        "$ROOT_DIR/scripts/test-required-check-outcomes.sh" >"$tamper_out" 2>&1; then
    echo "FAIL: a tampered authority did not fail the checker" >&2
    failures=$((failures + 1))
elif grep -q '^NO-RESULT-CASE:' "$tamper_out"; then
    echo "FAIL: a tampered authority was reported as could-not-determine; an integrity refusal is not an outage" >&2
    failures=$((failures + 1))
fi

if [ "$failures" -ne 0 ]; then
    exit 1
fi
echo "PASS: guarded checkers classify transport failures as no-result, fail HTTP 404/403 and unknown errors, and both DAG nodes reject missing output capture"
