#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# DAG-node wrapper for test.liteinst_strict: report a MISSING STAGED RUNTIME as a
# no_result, not as a product failure.
#
# WHY THIS EXISTS. `hermit-cli/tests/common/liteinst.rs` asserts on the staged
# runtime from inside a `#[test]`, and a panic in a test is a FAILURE. So "the
# LiteInst runtime is not staged" (a SETUP condition -- fix your checkout) and a
# real product defect -- the thing these tests exist to catch -- produce the SAME
# red on this node. That second case used to be spelled "the clone boundary is
# not enforced". Since Reverie began following threads and child processes it is
# the opposite: a boundary REAPPEARING, which is what
# `liteinst_thread_clone_runs_without_sigsys` and `liteinst_fork_runs_without_hanging`
# now catch by requiring the guest to run. Measured on the owner's run
# at 4e168f2aa5b9: 23 tests ran, 1 passed, 22 FAILED, and every one of the 22
# carried the same setup refusal -- "the staged LiteInst runtime ... records no
# Reverie revision". The node was counted as one of nine blocking product
# failures. It had not formed a product opinion at all.
#
# WHY 75. scripts/validate.rs reserves exactly one nonzero code that is not a
# product failure: NO_RESULT_EXIT_CODE = 75, matched by outcome_is_no_result()
# and excluded by outcome_is_failure(). Any other value is classified a FAILURE.
# ci/lint-checks-node.sh already uses 75 for precisely this shape; this follows
# that spelling rather than inventing a second one.
#
# ⚠️ WHY THIS ONE PRE-FLIGHTS WHERE lint-checks-node.sh DELIBERATELY DOES NOT.
# That node learned the hard way not to exit 75 before running its target: its
# precondition affected one arm of one case in one of seventeen checkers, so
# skipping the target threw away sixteen checkers' worth of signal. THE RATIO IS
# INVERTED HERE. Every test in this target that touches LiteInst cannot run
# without the runtime -- measured, 22 of 23 -- so running the target establishes
# nothing about the product and produces 22 reds that mean "not staged". Checking
# first is what makes the node's red mean one thing.
#
# ⚠️ AND A NO_RESULT MUST NEVER SWALLOW A RED. That is why this checks BEFORE the
# target rather than reclassifying its exit code afterwards: if the target runs,
# its verdict is passed through untouched, so there is no path by which a real
# failure becomes a no_result.

set -uo pipefail

usage() {
    cat >&2 <<'USAGE'
Usage: ci/liteinst-strict-node.sh [--self-test] -- COMMAND [ARGS...]

Verifies the staged LiteInst runtime is present and records the Reverie revision
it was built from, then execs COMMAND. Exits 75 (no_result) when the runtime
cannot be shown to be staged, so a setup condition is not reported as a product
failure.
USAGE
}

# The same two locations `hermit::liteinst_runtime_library` searches, in the same
# order, plus the same env override. Kept in step with hermit-cli/src/lib.rs; the
# self-test below pins the override precedence.
runtime_path() {
    if [[ -n ${HERMIT_LITEINST_RUNTIME:-} ]]; then
        printf '%s' "$HERMIT_LITEINST_RUNTIME"
        return 0
    fi
    local dir=${HERMIT_LITEINST_STAGE_DIR:-target/release}
    if [[ -f $dir/libreverie_liteinst.so ]]; then
        printf '%s' "$dir/libreverie_liteinst.so"
        return 0
    fi
    if [[ -f $dir/deps/libreverie_liteinst.so ]]; then
        printf '%s' "$dir/deps/libreverie_liteinst.so"
        return 0
    fi
    printf '%s' "$dir/libreverie_liteinst.so"
    return 1
}

# ⚠️ DELIBERATELY DOES NOT COMPARE THE REVISION TO THE PIN, and that is not an
# oversight. The pin rule is `hermit-cli/reverie_pin.rs::parse_reverie_pin`:
# EVERY Reverie revision named in the manifest must agree, commented lines are
# skipped, and ambiguity yields "unknown" on which the product guard SKIPS rather
# than refuses. Re-expressing that in shell would be a second, unvalidated
# implementation of a rule whose whole point is that first-match-wins gets it
# wrong -- the exact hazard its own comment documents. So this checks only the
# two facts that need no pin knowledge:
#
#   * is the runtime staged at all?
#   * did whatever staged it record a revision?
#
# A runtime that records a revision which MISMATCHES the pin is also a setup
# condition and is NOT covered here; it still surfaces as a test failure. Closing
# that needs a machine-readable availability probe from the product, which is
# filed rather than guessed at.
classify() {
    local path
    if ! path=$(runtime_path); then
        printf 'missing %s' "$path"
        return 0
    fi
    if [[ ! -f $path ]]; then
        printf 'missing %s' "$path"
        return 0
    fi
    if [[ ! -f "$path.revision" ]]; then
        printf 'unrecorded %s' "$path"
        return 0
    fi
    if [[ ! -s "$path.revision" ]]; then
        printf 'unrecorded %s' "$path"
        return 0
    fi
    printf 'staged %s' "$path"
    return 0
}

self_test() {
    local failures=0 scratch
    scratch=$(mktemp -d) || return 1
    trap 'rm -rf "$scratch"' RETURN

    check() {
        local name=$1 want=$2 got=$3
        if [[ $got == "$want" ]]; then
            printf 'ok   %s\n' "$name"
        else
            printf 'FAIL %s: expected %q, got %q\n' "$name" "$want" "$got" >&2
            failures=$((failures + 1))
        fi
    }

    # ⚠️ A CONTROL THAT MUST NOT BE A NO_RESULT. Without it this whole file could
    # answer "no_result" unconditionally and every case above would still pass.
    mkdir -p "$scratch/good"
    : >"$scratch/good/libreverie_liteinst.so"
    printf 'ad598995c8018bf17414a92119acfac6c9fd58ee\n' >"$scratch/good/libreverie_liteinst.so.revision"
    check 'a staged runtime that records a revision is NOT a no_result' \
        "staged $scratch/good/libreverie_liteinst.so" \
        "$(HERMIT_LITEINST_STAGE_DIR=$scratch/good classify)"

    mkdir -p "$scratch/absent"
    check 'no runtime at all is a no_result' \
        "missing $scratch/absent/libreverie_liteinst.so" \
        "$(HERMIT_LITEINST_STAGE_DIR=$scratch/absent classify)"

    # The exact condition measured at 4e168f2aa5b9.
    mkdir -p "$scratch/unrecorded"
    : >"$scratch/unrecorded/libreverie_liteinst.so"
    check 'a runtime staged before revision recording is a no_result' \
        "unrecorded $scratch/unrecorded/libreverie_liteinst.so" \
        "$(HERMIT_LITEINST_STAGE_DIR=$scratch/unrecorded classify)"

    # An empty marker records nothing, so it must not read as recorded.
    mkdir -p "$scratch/empty"
    : >"$scratch/empty/libreverie_liteinst.so"
    : >"$scratch/empty/libreverie_liteinst.so.revision"
    check 'an empty revision marker records nothing' \
        "unrecorded $scratch/empty/libreverie_liteinst.so" \
        "$(HERMIT_LITEINST_STAGE_DIR=$scratch/empty classify)"

    # The deps/ fallback the product searches second.
    mkdir -p "$scratch/deps-only/deps"
    : >"$scratch/deps-only/deps/libreverie_liteinst.so"
    printf 'ad598995c8018bf17414a92119acfac6c9fd58ee\n' \
        >"$scratch/deps-only/deps/libreverie_liteinst.so.revision"
    check 'the deps/ fallback is searched, as the product searches it' \
        "staged $scratch/deps-only/deps/libreverie_liteinst.so" \
        "$(HERMIT_LITEINST_STAGE_DIR=$scratch/deps-only classify)"

    # HERMIT_LITEINST_RUNTIME wins over the staged directory, as in the product.
    check 'HERMIT_LITEINST_RUNTIME takes precedence' \
        "staged $scratch/good/libreverie_liteinst.so" \
        "$(HERMIT_LITEINST_RUNTIME=$scratch/good/libreverie_liteinst.so \
           HERMIT_LITEINST_STAGE_DIR=$scratch/absent classify)"

    # The standalone producer is the documented way to stage this runtime. Use
    # a fake Cargo command so this self-test checks its file contract without
    # compiling Reverie.
    cat >"$scratch/fake-cargo" <<'FAKE_CARGO'
#!/usr/bin/env bash
set -euo pipefail
: "${HERMIT_LITEINST_STAGE:?missing HERMIT_LITEINST_STAGE}"
printf 'self-test runtime\n' >"$HERMIT_LITEINST_STAGE"
FAKE_CARGO
    chmod +x "$scratch/fake-cargo"
    mkdir -p "$scratch/producer"
    local produced="$scratch/producer/libreverie_liteinst.so"
    printf 'stale-revision\n' >"$produced.revision"
    if CARGO="$scratch/fake-cargo" ./scripts/stage-liteinst-runtime.sh \
        dev "$produced" "$scratch/runtime-target" \
        >"$scratch/producer.out" 2>"$scratch/producer.err"; then
        local expected_pin recorded_pin
        expected_pin=$(./ci/run-reverie-pin-check.sh --repo "$PWD" --print-pin \
            2>"$scratch/pin.err")
        recorded_pin=$(cat "$produced.revision" 2>/dev/null || true)
        check 'the standalone producer replaces a stale marker with the current Reverie pin' \
            "$expected_pin" "$recorded_pin"
        check 'the standalone runtime is accepted by the consumer preflight' \
            "staged $produced" \
            "$(HERMIT_LITEINST_RUNTIME=$produced classify)"
    else
        printf 'FAIL the standalone producer command failed\n' >&2
        cat "$scratch/producer.out" "$scratch/producer.err" >&2
        failures=$((failures + 1))
    fi

    if ((failures)); then
        printf 'liteinst-strict-node --self-test: %d case(s) failed\n' "$failures" >&2
        return 1
    fi
    printf 'liteinst-strict-node --self-test: all cases pass\n'
    return 0
}

cd "$(dirname "$0")/.." || exit 1

if [[ ${1:-} == --self-test ]]; then
    self_test
    exit $?
fi

if [[ ${1:-} == -- ]]; then
    shift
fi
if (($# == 0)); then
    usage
    exit 2
fi

verdict="$(classify)"
case "$verdict" in
    missing\ *)
        echo "liteinst-strict: NO RESULT -- no staged LiteInst runtime at ${verdict#missing }." >&2
        echo '  This is a SETUP condition, not a product failure: every test in this' >&2
        echo '  target loads that runtime, so nothing about LiteInst was measured.' >&2
        echo '  Stage it with `cargo build --release -p hermit-install` (release-only).' >&2
        exit 75
        ;;
    unrecorded\ *)
        echo "liteinst-strict: NO RESULT -- ${verdict#unrecorded } records no Reverie revision." >&2
        echo '  This is a SETUP condition, not a product failure: the runtime cannot be' >&2
        echo '  shown to match the pin this binary was built from, so hermit refuses the' >&2
        echo '  backend and no test in this target reaches the product.' >&2
        echo '  Restage it with `cargo build --release -p hermit-install` (release-only).' >&2
        exit 75
        ;;
esac

exec "$@"
