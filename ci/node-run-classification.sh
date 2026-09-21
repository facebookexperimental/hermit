#!/usr/bin/env bash
# The pass / no_result / fail rule shared by every CI node whose target can
# report an unevaluable case.
#
# ⚠️ ONE DEFINITION, SOURCED, NOT COPIED. Measured 2026-09-04: this rule lived
# only inside ci/lint-checks-node.sh, and check.check_outcome_consumers ran the
# same two checkers DIRECTLY with no interpreter at all -- so under an
# unreachable authority those checkers exited 0 with markers and that node
# recorded PASSED. A second copy of the rule would have the same failure mode
# one edit later.
#
# Exit 75 (EX_TEMPFAIL) is the established spelling: scripts/validate.rs
# classifies it no_result and excludes it from failures.

NO_RESULT_MARKER='NO-RESULT-CASE:'

# ⚠️ THE MARKER MATCH IS ANCHORED, and the anchor is the whole correctness argument.
#
# `NO-RESULT-CASE:` is now TRACKED TEXT in this repository -- it appears in this
# file and in scripts/test_validate_stop_paths.py. An unanchored `grep -q` matches
# the token ANYWHERE on ANY line of the target's combined output, so any checker
# that SUCCEEDS while echoing a line that merely mentions the marker would convert
# a fully green run into a no_result. A no_result on this node is invisible by
# design, so that failure mode is silent in the reassuring direction.
#
# Latent rather than live today: no current checker prints the token on a success
# path. The reason to anchor now is that the most likely next addition to this
# target is a test OF THIS FEATURE, which would quote the marker while passing.
#
# ⚠️ AND THE ANCHOR MUST NOT MISS A GENUINE EMISSION, which is the trade an anchor
# usually gets wrong. The producer prints `f"{NO_RESULT_MARKER} {item}"` to stderr,
# at column 0, and `make` does not prefix recipe output -- so column 0 is where a
# real marker lands. The one way it could not is interleaving: this node merges
# streams with `2>&1`, so an unterminated stdout line from an earlier writer could
# leave the marker appended mid-line. scripts/test_validate_stop_paths.py therefore
# flushes stdout before emitting, which closes that window at the source rather
# than widening the pattern here to compensate. Both halves are covered by
# --self-test below, including the case that must NOT match.
classify_run() {
    # $1 = make's exit code, $2 = file holding the target's combined output.
    # Echoes exactly one of: "fail <rc>", "no_result", "pass".
    local make_rc="$1" out="$2"
    # A REAL FAILURE ALWAYS WINS. A no_result must never be able to swallow a red,
    # which is the defect this whole marker channel is downstream of.
    if [ "$make_rc" -ne 0 ]; then
        echo "fail ${make_rc}"
        return 0
    fi
    if grep -q "^${NO_RESULT_MARKER}" "$out"; then
        echo 'no_result'
        return 0
    fi
    echo 'pass'
}
