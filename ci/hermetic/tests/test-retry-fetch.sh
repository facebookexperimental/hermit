#!/usr/bin/env bash
# Both-ways proof for the fetch phase's bounded retry.
#
# ⚠️ THIS SOURCES THE REAL HELPER, NOT A COPY. A test that reimplements the
# retry would only prove the test agrees with itself; run-split-validate.sh
# sources exactly this file.
#
# A retry proven only on the happy path is not proven, so this asserts BOTH
# that a transient failure is survived AND that a persistent one still fails
# with the underlying command's own error and its own exit status.
set -uo pipefail

HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
export HERMETIC_FETCH_ATTEMPTS=3
export HERMETIC_FETCH_BACKOFF_SECONDS=0
export HERMETIC_FETCH_RETRY_DEADLINE_SECONDS=300
# shellcheck source-path=SCRIPTDIR/..
# shellcheck source=ci/hermetic/retry-fetch.sh
source "$HERE/../retry-fetch.sh"

failures=0
check() { # check <description> <condition-result>
    if [[ $2 -eq 0 ]]; then echo "ok   - $1"; else echo "FAIL - $1"; failures=$((failures+1)); fi
}

work=$(mktemp -d); trap 'rm -rf "$work"' EXIT

# ---------------------------------------------------------------- transient --
# Fails once with the real observed error text, then succeeds.
cat > "$work/transient" <<'EOF'
#!/usr/bin/env bash
n=$(cat "$STATE" 2>/dev/null || echo 0); n=$((n+1)); echo "$n" > "$STATE"
if (( n < 2 )); then
  echo "[60] SSL peer certificate or SSH remote key was not OK" >&2
  exit 101
fi
echo "fetched"
EOF
chmod +x "$work/transient"
out=$(STATE="$work/state1" retry_fetch "transient probe" "$work/transient" 2>&1); rc=$?
check "a transient failure is SURVIVED (rc=0)" "$([[ $rc -eq 0 ]] && echo 0 || echo 1)"
check "  and it really did retry (2 attempts recorded)" "$([[ $(cat "$work/state1") == 2 ]] && echo 0 || echo 1)"
check "  and the first attempt's own error still reached the log" \
      "$(grep -q '\[60\] SSL peer certificate' <<<"$out" && echo 0 || echo 1)"
check "  and the recovery is stated rather than silent" \
      "$(grep -q 'succeeded on attempt 2' <<<"$out" && echo 0 || echo 1)"

# --------------------------------------------------------------- persistent --
# Always fails. The bound must be reached and the CAUSE must survive.
cat > "$work/persistent" <<'EOF'
#!/usr/bin/env bash
n=$(cat "$STATE" 2>/dev/null || echo 0); echo "$((n+1))" > "$STATE"
echo "[60] SSL peer certificate or SSH remote key was not OK" >&2
exit 101
EOF
chmod +x "$work/persistent"
out=$(STATE="$work/state2" retry_fetch "persistent probe" "$work/persistent" 2>&1); rc=$?
check "a persistent failure STILL FAILS" "$([[ $rc -ne 0 ]] && echo 0 || echo 1)"
check "  and returns the command's OWN exit status (101), not a synthetic one" \
      "$([[ $rc -eq 101 ]] && echo 0 || echo 1)"
check "  and is bounded at exactly $HERMETIC_FETCH_ATTEMPTS attempts" \
      "$([[ $(cat "$work/state2") == 3 ]] && echo 0 || echo 1)"
check "  and curl's own error is SURFACED, not replaced by a retry message" \
      "$(grep -q '\[60\] SSL peer certificate' <<<"$out" && echo 0 || echo 1)"
check "  and the final line says the cause is above rather than claiming to be it" \
      "$(grep -q 'is the cause; this line only says the bound was reached' <<<"$out" && echo 0 || echo 1)"

# ----------------------------------------------------------------- deadline --
# An exhausted per-call cutoff must stop a new retry and retain the cause.
# The unchanged outer node timeout still bounds running commands.
out=$(HERMETIC_FETCH_RETRY_DEADLINE_SECONDS=0 STATE="$work/state3" \
      retry_fetch "deadline probe" "$work/persistent" 2>&1); rc=$?
check "an exhausted retry deadline stops early" \
      "$([[ $(cat "$work/state3") == 1 ]] && echo 0 || echo 1)"
check "  still returning the command's own status" "$([[ $rc -eq 101 ]] && echo 0 || echo 1)"
check "  and still surfacing the cause" \
      "$(grep -q '\[60\] SSL peer certificate' <<<"$out" && echo 0 || echo 1)"

# -------------------------------------------------------------- late wakeup --
# Drive the real helper past its positive cutoff during backoff. The sleep
# replacement and SECONDS adjustment exist only in this captured subshell.
# Without the post-backoff check the fail-once child would falsely recover.
out=$(
    {
        sleep() { printf '%s\n' "$1" >> "$work/sleep-late"; SECONDS=$((SECONDS + 301)); }
        HERMETIC_FETCH_BACKOFF_SECONDS=1 HERMETIC_FETCH_RETRY_DEADLINE_SECONDS=300 \
            STATE="$work/state-late" retry_fetch "late wakeup probe" "$work/transient"
    } 2>&1
); rc=$?
check "an overslept backoff returns the last completed child's status" \
      "$([[ $rc -eq 101 ]] && echo 0 || echo 1)"
check "  and cannot start a second invocation after the deadline" \
      "$([[ $(cat "$work/state-late") == 1 ]] && echo 0 || echo 1)"
check "  and really reached the requested positive backoff" \
      "$([[ $(cat "$work/sleep-late") == 1 ]] && echo 0 || echo 1)"
check "  and retains both the child cause and the deadline refusal" \
      "$(grep -q '\[60\] SSL peer certificate' <<<"$out" && \
         grep -q 'retry deadline expired during backoff' <<<"$out" && echo 0 || echo 1)"

# The opposing control rejects a helper that refuses every post-sleep retry.
out=$(
    {
        sleep() { printf '%s\n' "$1" >> "$work/sleep-within"; SECONDS=$((SECONDS + 1)); }
        HERMETIC_FETCH_BACKOFF_SECONDS=1 HERMETIC_FETCH_RETRY_DEADLINE_SECONDS=300 \
            STATE="$work/state-within" retry_fetch "within deadline probe" "$work/transient"
    } 2>&1
); rc=$?
check "a within-deadline backoff can recover successfully" "$([[ $rc -eq 0 ]] && echo 0 || echo 1)"
check "  and invokes the child exactly twice" \
      "$([[ $(cat "$work/state-within") == 2 ]] && echo 0 || echo 1)"
check "  and requests one positive backoff and reports actual recovery" \
      "$([[ $(cat "$work/sleep-within") == 1 ]] && \
         grep -q 'succeeded on attempt 2' <<<"$out" && echo 0 || echo 1)"

# ------------------------------------------------------ control: no retry on ok --
# So none of the above passes merely because everything retries.
cat > "$work/good" <<'EOF'
#!/usr/bin/env bash
n=$(cat "$STATE" 2>/dev/null || echo 0); echo "$((n+1))" > "$STATE"; echo ok
EOF
chmod +x "$work/good"
out=$(STATE="$work/state4" retry_fetch "control" "$work/good" 2>&1); rc=$?
check "a succeeding command runs EXACTLY once and is not retried" \
      "$([[ $rc -eq 0 && $(cat "$work/state4") == 1 ]] && echo 0 || echo 1)"
check "  and prints no attempt chatter on the happy path" \
      "$([[ $out == ok ]] && echo 0 || echo 1)"

echo
if (( failures )); then echo "test-retry-fetch: $failures FAILED"; exit 1; fi
echo "test-retry-fetch: all checks passed"
