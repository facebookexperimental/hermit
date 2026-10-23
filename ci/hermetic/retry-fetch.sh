#!/usr/bin/env bash
# Bounded retry for the FETCH phase's network operations.
#
# WHY THIS EXISTS. The fetch phase is the validate run's ONLY network phase --
# everything after it builds offline from what it fetched -- so every node in the
# run depends on it. A single transient failure there therefore discards the
# whole run: measured on run 1843, one node failing in 4.86s left 30 passed,
# 1 failed and 239 skipped against a 270-node plan.
#
# The observed failure was `curl [60] SSL peer certificate ... unable to get
# local issuer certificate` on one crates.io CDN download, in a run whose DNS
# resolved, whose network negative-control passed, and whose two git remotes
# both succeeded. It is transient: the identical `cargo fetch --locked`
# reproduces successfully with a fresh CARGO_HOME, and other runs pass this node
# in ~95s. Cargo will not retry it itself, because cargo retries only errors it
# classifies as SPURIOUS and a certificate-verification failure is not one.
#
# ⚠️ WHAT THIS DELIBERATELY DOES NOT DO.
#   * It does NOT relax TLS verification. A retry converts a visible transient
#     into a second attempt; disabling verification would convert it into a
#     silent supply-chain hole.
#   * It does NOT raise any timeout. The failure takes ~5 seconds against a
#     600-second node budget, so a timeout was never the constraint.
#   * It does NOT capture the child's output. The streams are INHERITED so the
#     failing command's own words reach the run log exactly as before -- that is
#     the whole diagnostic value, and swallowing it to print a tidy
#     "retries exhausted" line would recreate the defect this repairs.

# Attempts INCLUDING the first, so 3 means at most two retries.
: "${HERMETIC_FETCH_ATTEMPTS:=3}"
# Linear backoff: attempt N waits N * this many seconds before attempt N+1.
: "${HERMETIC_FETCH_BACKOFF_SECONDS:=5}"
# Per-call wall-clock cutoff for starting another attempt, rechecked after
# backoff. This does not bound a running child or the complete fetch phase:
# the existing validation-node timeout remains the unchanged outer backstop.
: "${HERMETIC_FETCH_RETRY_DEADLINE_SECONDS:=300}"

# retry_fetch <label> <command> [args...]
#
# Returns the child's OWN exit status on final failure, never a synthetic one,
# so `exit 101` still reaches the ledger as `exit 101`.
retry_fetch() {
    local label=$1
    shift
    local attempt=1 rc=0 started elapsed sleep_for
    started=$SECONDS
    while :; do
        # NOT `if "$@"; then ... fi; rc=$?`. After an `if` statement completes,
        # `$?` is the STATUS OF THE IF, which is 0 when the condition was false
        # and there is no else -- so that spelling silently turns every failure
        # into rc=0. The first version of this file had exactly that bug and the
        # both-ways test below caught it; keep the `||` form.
        rc=0
        "$@" || rc=$?
        if (( rc == 0 )); then
            if (( attempt > 1 )); then
                echo ":: $label succeeded on attempt $attempt/$HERMETIC_FETCH_ATTEMPTS"
            fi
            return 0
        fi
        elapsed=$(( SECONDS - started ))
        if (( attempt >= HERMETIC_FETCH_ATTEMPTS )); then
            echo "run-split-validate: $label FAILED on attempt $attempt of $HERMETIC_FETCH_ATTEMPTS (exit $rc)." >&2
            echo "  The failing command's own output is ABOVE and is the cause; this line only says the bound was reached." >&2
            return "$rc"
        fi
        sleep_for=$(( HERMETIC_FETCH_BACKOFF_SECONDS * attempt ))
        if (( elapsed + sleep_for >= HERMETIC_FETCH_RETRY_DEADLINE_SECONDS )); then
            echo "run-split-validate: $label FAILED on attempt $attempt (exit $rc); not retrying -- ${elapsed}s already spent against a ${HERMETIC_FETCH_RETRY_DEADLINE_SECONDS}s retry deadline." >&2
            echo "  Stopping here deliberately: a further attempt risks a timeout kill, which would hide the cause printed above." >&2
            return "$rc"
        fi
        echo "run-split-validate: $label attempt $attempt of $HERMETIC_FETCH_ATTEMPTS failed (exit $rc); retrying in ${sleep_for}s. Its output is above." >&2
        sleep "$sleep_for"
        elapsed=$(( SECONDS - started ))
        if (( elapsed >= HERMETIC_FETCH_RETRY_DEADLINE_SECONDS )); then
            echo "run-split-validate: $label FAILED on attempt $attempt (exit $rc); not retrying -- ${elapsed}s spent against a ${HERMETIC_FETCH_RETRY_DEADLINE_SECONDS}s retry deadline." >&2
            echo "  The last completed command's own output is ABOVE; the retry deadline expired during backoff." >&2
            return "$rc"
        fi
        attempt=$(( attempt + 1 ))
    done
}
