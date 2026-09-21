#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# Assert that ci/run-with-reverie-dbt-budget.sh REACHES ITS WRAPPED COMMAND at
# the pin this tree actually records.
#
# WHY THIS EXISTS
#
# The wrapper fails closed when its calibrated pin does not match the recorded
# one, which is correct -- a measured budget must not be reused across a recipe
# it was not measured against. The failure mode is that NOTHING NOTICES. The
# wrapper gates roughly twenty portable DAG nodes (the fat build, all-features
# Clippy, doc, and most nextest nodes). After a Reverie pin bump every one of
# them exits 2 in about a second, having never invoked Cargo, and a truncated
# node reads a great deal like a fast one.
#
# `check-reverie-pin.rs` reads the calibration site, but historically only on the
# pin-UPDATE path, and it deliberately refuses to rewrite it
# (`BUDGET_CALIBRATION_SITE`) because doing so would assert that a measurement
# still applies, which that tool cannot establish. That reasoning is unchanged.
#
# ⚠️ "SO THE DRIFT HAS NO GATE AT ALL -- THIS IS IT" WAS TRUE AND IS NO LONGER.
# `check_dbt_budget_bindings` in check-reverie-pin.rs now REFUSES on the ordinary
# path -- the one the pre-commit hook and scripts/validate.rs run -- when either
# binding disagrees with the recorded pin, and it covers
# ci/configure-build-jobs.sh too, which nothing checked before. It refuses; it
# still does not rewrite, for exactly the reason above. This file remains the
# END-TO-END check: it asserts the wrapped command actually RAN, which no
# string comparison can establish.
#
# This has failed for two DIFFERENT reasons already, which is why the assertion
# is "the wrapped command ran" rather than "expected_pin looks right":
#   1. a pin bump left expected_pin behind;
#   2. --print-pin gained a uniformity report on stdout, so the captured value
#      became 941 characters over 8 lines and the comparison could never succeed
#      for ANY pin, including a correctly calibrated one.
# Only an end-to-end check catches both. A string comparison against
# expected_pin would have passed cleanly through case 2.

set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

WRAPPER=ci/run-with-reverie-dbt-budget.sh
failures=0

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# ---------------------------------------------------------------- positive
# The sentinel is a FILE the wrapped command creates, not the wrapper's exit
# status. `exec "$@"` means a wrapper that exits 0 without ever reaching the
# command is indistinguishable by status alone; the artifact is not.
marker="$work/reached"
# Capture the status in its own statement. Inside `if ! cmd; then`, `$?` is the
# status of the NEGATION, so it reads 0 and the diagnostic reports the opposite
# of what happened -- the same class of mistake as reading `$?` after a pipe.
rc=0
"./$WRAPPER" /bin/sh -c "printf ok > '$marker'" >/dev/null 2>"$work/stderr" || rc=$?
if ((rc != 0)); then
    echo "FAIL: $WRAPPER exited $rc at the recorded pin; it must reach its wrapped command" >&2
    sed 's/^/    /' "$work/stderr" >&2
    failures=$((failures + 1))
elif [[ ! -s $marker ]]; then
    echo "FAIL: $WRAPPER exited 0 but the wrapped command never ran" >&2
    failures=$((failures + 1))
else
    echo "PASS: $WRAPPER reaches its wrapped command at the recorded pin"
fi

# The budget line is the evidence a reader needs when a node is slow; assert it
# is still emitted, on stderr, so it cannot be silently dropped.
if ! grep -q 'reverie-dbt-budget={pin:' "$work/stderr"; then
    echo "FAIL: $WRAPPER did not emit its reverie-dbt-budget= line on stderr" >&2
    failures=$((failures + 1))
fi

# ---------------------------------------------------------------- negative
# A test that only asserts the happy path cannot distinguish a working wrapper
# from one that ignores the pin entirely. Mutate the calibration in a COPY of
# the tree and require a refusal, so this file is demonstrably able to fail.
# A SYMLINK FARM, not a partial copy. `ci/` is a real copy so the wrapper can be
# mutated; every other entry is a symlink to the real tree so the pin checker
# still finds the Cargo metadata and reports the genuine revision. `.git` is the
# exception: a linked checkout records a RELATIVE gitdir in that file, and a
# symlink would make Git resolve it relative to this temporary tree. Write the
# same gitdir as an absolute pointer so the copied worktree remains readable.
#
# ⚠️ Copying only ci/ makes this control pass for the WRONG REASON: the pin
# checker finds no Cargo metadata, the wrapper refuses with "did not yield a
# 40-hex revision", and a test asserting only "exit != 0" reads that as the
# pin-mismatch refusal it never saw. That is why the assertion below is on the
# refusal MESSAGE, not on the status alone.
copy="$work/tree"
mkdir -p "$copy"
shopt -s dotglob
for entry in "$ROOT_DIR"/*; do
    name=$(basename "$entry")
    # dotglob already covers .git; ci/ is replaced by a real copy below.
    [[ $name == ci ]] && continue
    if [[ $name == .git ]]; then
        git_dir=$(git rev-parse --absolute-git-dir)
        printf 'gitdir: %s\n' "$git_dir" >"$copy/.git"
        continue
    fi
    ln -s "$entry" "$copy/$name"
done
shopt -u dotglob
cp -a ci "$copy/ci"
sed -i 's/^expected_pin=.*/expected_pin=0000000000000000000000000000000000000000/' \
    "$copy/$WRAPPER"

marker2="$work/must-not-exist"
rc2=0
"$copy/$WRAPPER" /bin/sh -c "printf bad > '$marker2'" >/dev/null 2>"$work/stderr2" || rc2=$?
if ((rc2 == 0)); then
    echo "FAIL: $WRAPPER accepted a deliberately wrong expected_pin" >&2
    failures=$((failures + 1))
elif [[ -e $marker2 ]]; then
    echo "FAIL: $WRAPPER refused but still reached the wrapped command" >&2
    failures=$((failures + 1))
elif ! grep -q 'no calibrated budget for Reverie pin' "$work/stderr2"; then
    echo "FAIL: the negative control refused for the wrong reason -- expected the" >&2
    echo "      pin-mismatch refusal, got:" >&2
    sed 's/^/    /' "$work/stderr2" >&2
    failures=$((failures + 1))
else
    echo "PASS: $WRAPPER refuses a mismatched pin, by pin mismatch, without running the command"
fi

# A machine-readable producer must emit exactly one value. The wrapper used to
# pipe through `head -n 1`, which silently accepted small trailing output and
# turned large trailing output into SIGPIPE instead of naming malformed stdout.
expect_unparseable_pin_refusal() {
    local label=$1
    local marker=$2
    local stderr_file=$3
    local rc=0

    "$copy/$WRAPPER" /bin/sh -c "printf bad > '$marker'" \
        >/dev/null 2>"$stderr_file" || rc=$?
    if ((rc == 0)); then
        echo "FAIL: $label extra stdout was accepted" >&2
        failures=$((failures + 1))
    elif [[ -e $marker ]]; then
        echo "FAIL: $label extra stdout reached the wrapped command" >&2
        failures=$((failures + 1))
    elif ! grep -q 'did not yield a 40-hex revision' "$stderr_file"; then
        echo "FAIL: $label extra stdout refused without naming malformed pin output" >&2
        sed 's/^/    /' "$stderr_file" >&2
        failures=$((failures + 1))
    else
        echo "PASS: $label extra stdout is rejected explicitly without running the command"
    fi
}

cp -p "$ROOT_DIR/$WRAPPER" "$copy/$WRAPPER"
cp -p "$ROOT_DIR/ci/run-reverie-pin-check.sh" "$copy/ci/run-reverie-pin-check.sh"
printf '\nprintf "%%s\\n" "unexpected extra stdout"\n' \
    >>"$copy/ci/run-reverie-pin-check.sh"
expect_unparseable_pin_refusal small "$work/small-must-not-exist" "$work/stderr3"

cp -p "$ROOT_DIR/ci/run-reverie-pin-check.sh" "$copy/ci/run-reverie-pin-check.sh"
printf '\nfor _ in {1..2048}; do printf "%%064d" 0; done\nprintf "\\n"\n' \
    >>"$copy/ci/run-reverie-pin-check.sh"
expect_unparseable_pin_refusal large "$work/large-must-not-exist" "$work/stderr4"

# ---------------------------------------------------------------------------
# THE EXIT CODE IS THE VERDICT, AND BOTH DIRECTIONS ARE ASSERTED.
#
# A declining wrapper and a genuinely broken build were both recorded as gate
# result "fail" with a bare `exit N` reason, so a node that compiled NOTHING was
# indistinguishable in the ledger from one that compiled and broke. 75 is
# EX_TEMPFAIL, which `scripts/validate.rs` defines as the only nonzero code that
# is not a product failure, and which it maps to "no_result".
#
# ⚠️ ASSERTING ONLY THE REFUSAL WOULD BE HALF A TEST. If a real breakage started
# reporting no_result, one silent failure would have been traded for another --
# a build failure that reads as "could not determine" is WORSE than one that
# reads as a fast fail. So the pass-through direction is asserted too, with the
# code cargo actually returns for a compile error, which is 101 and not 2.

expect_exit() {
    local label=$1 want=$2 marker=$3
    shift 3
    local rc=0
    "./$WRAPPER" "$@" >/dev/null 2>&1 || rc=$?
    if ((rc != want)); then
        echo "FAIL: $label exited $rc, expected $want" >&2
        failures=$((failures + 1))
        return
    fi
    if [ -n "$marker" ] && [ ! -f "$marker" ]; then
        echo "FAIL: $label exited $want but the wrapped command never ran" >&2
        failures=$((failures + 1))
        return
    fi
    echo "PASS: $label exits $want"
}

# Direction 1 -- a refusal declines with 75 and runs nothing. Covered above for
# the message; here for the CODE, which is what every consumer branches on.
rc=0
sed -i 's/^expected_pin=.*/expected_pin=0000000000000000000000000000000000000000/' \
    "$copy/$WRAPPER"
(cd "$copy" && "./$WRAPPER" /bin/sh -c "printf ok > '$work/declined-must-not-exist'") \
    >/dev/null 2>&1 || rc=$?
if ((rc != 75)); then
    echo "FAIL: a pin-mismatch refusal exited $rc, expected 75 (EX_TEMPFAIL/no_result)" >&2
    failures=$((failures + 1))
elif [ -e "$work/declined-must-not-exist" ]; then
    echo "FAIL: the refusal exited 75 but still ran the command" >&2
    failures=$((failures + 1))
else
    echo "PASS: a pin-mismatch refusal exits 75 and runs nothing"
fi
cp -p "$ROOT_DIR/$WRAPPER" "$copy/$WRAPPER"

# Direction 2 -- at the recorded pin the wrapper `exec`s, so the wrapped
# command's status is the node's status, UNCHANGED. 101 is cargo's compile-error
# code; 2 is what the old refusal used and must no longer be produced by a
# refusal; 0 must survive.
expect_exit "a wrapped compile-failure status (101)" 101 "$work/ran-101" \
    /bin/sh -c "printf ok > '$work/ran-101'; exit 101"
expect_exit "a wrapped generic failure (1)" 1 "" /bin/sh -c "exit 1"
expect_exit "a wrapped success" 0 "$work/ran-0" \
    /bin/sh -c "printf ok > '$work/ran-0'"

# The caller-bug path stays LOUD. Supplying no command is not a node declining.
rc=0
"./$WRAPPER" >/dev/null 2>&1 || rc=$?
if ((rc != 2)); then
    echo "FAIL: the usage error exited $rc, expected 2 -- a caller bug is not a no_result" >&2
    failures=$((failures + 1))
else
    echo "PASS: the usage error still exits 2"
fi

if ((failures != 0)); then
    echo "run-with-reverie-dbt-budget-test.sh: $failures check(s) failed" >&2
    exit 1
fi
echo "run-with-reverie-dbt-budget-test.sh: OK"
