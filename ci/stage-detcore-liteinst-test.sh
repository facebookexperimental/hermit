#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Self-test for ci/stage-detcore-liteinst.sh with the real Cargo, against
# minimal one-crate workspaces it writes under mktemp (no dependencies, so
# every build runs --offline and reads nothing from the network).
#
# WHY THIS EXISTS. The script's claim is that it stages exactly the cdylib
# Cargo just built, at the path Cargo reports, beside the test executables.
# Two ways to get that wrong were found in review, and each has a case here:
#   (stale)  a libdetcore_liteinst.so left in the target directory by an
#            earlier build must not be reported as staged once this tree's
#            detcore-liteinst builds no cdylib: exit 2, by name, no "staged";
#   (triple) with CARGO_BUILD_TARGET set, Cargo writes under
#            TARGET_DIR/<triple>/; the script must stage what Cargo reports
#            there, not refuse it for a wrong reason.
# Also: the dev and a custom profile stage into their own deps directories,
# Cargo's failure keeps Cargo's status and names the whole command (with
# --offline), and usage errors exit 2 by name.
#
# Run by the lint-check-stage-detcore-liteinst Makefile target, which
# check.lint_checks runs. Writes only under its own mktemp directory, which it
# removes on exit.
set -uo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
script="$here/stage-detcore-liteinst.sh"

failures=0
work=$(mktemp -d) || exit 1
trap 'rm -rf -- "$work"' EXIT

check() {
    if [[ $2 -eq 0 ]]; then
        echo "ok   - $1"
    else
        echo "FAIL - $1"
        failures=$((failures + 1))
    fi
}
yes_if() { if "$@"; then echo 0; else echo 1; fi; }

# A workspace whose only member is detcore-liteinst, built as CRATE_TYPES.
workspace() {
    local root=$1 crate_types=$2
    mkdir -p "$root/detcore-liteinst/src"
    printf '[workspace]\nmembers = ["detcore-liteinst"]\nresolver = "2"\n\n[profile.ci]\ninherits = "dev"\n' \
        >"$root/Cargo.toml"
    printf '[package]\nname = "detcore-liteinst"\nversion = "0.1.0"\nedition = "2021"\n\n[lib]\ncrate-type = [%s]\n' \
        "$crate_types" >"$root/detcore-liteinst/Cargo.toml"
    printf 'pub fn runtime() -> u32 { 7 }\n' >"$root/detcore-liteinst/src/lib.rs"
    env -C "$root" cargo generate-lockfile --offline -q
}

# stage OUT_PREFIX ARGS...: run the script; leave status, stdout and stderr in OUT_PREFIX.*
stage() {
    local out=$1 rc=0
    shift
    "$script" "$@" >"$out.stdout" 2>"$out.stderr" || rc=$?
    echo "$rc" >"$out.rc"
}
rc() { cat "$1.rc"; }
staged_line() { grep -F 'stage-detcore-liteinst: staged ' "$1.stdout"; }

ws=$work/ws
workspace "$ws" '"cdylib", "rlib"' || { echo "FAIL - cannot create the fixture workspace"; exit 1; }

# dev profile: the runtime lands in TARGET/debug/deps, as Cargo reports.
stage "$work/dev" --source "$ws" --target-dir "$work/target" --profile dev --offline
runtime=$work/target/debug/deps/libdetcore_liteinst.so
check "dev: exit 0" "$(yes_if test "$(rc "$work/dev")" = 0)"
check "dev: stages TARGET/debug/deps/libdetcore_liteinst.so with its sha256" \
    "$(yes_if grep -qF "staged $runtime (sha256 $(sha256sum "$runtime" 2>/dev/null | cut -d' ' -f1))" "$work/dev.stdout")"

# custom profile: its own output directory, never predicted from the name.
stage "$work/ci" --source "$ws" --target-dir "$work/target" --profile ci --offline
check "custom profile: stages TARGET/ci/deps/libdetcore_liteinst.so" \
    "$(yes_if grep -qF "staged $work/target/ci/deps/libdetcore_liteinst.so " "$work/ci.stdout")"

# (stale) the same target directory, after the crate stops building a cdylib.
workspace "$ws" '"rlib"' || { echo "FAIL - cannot rewrite the fixture workspace"; exit 1; }
check "stale: the earlier build's runtime is still on disk" "$(yes_if test -s "$runtime")"
stage "$work/stale" --source "$ws" --target-dir "$work/target" --profile dev --offline
check "stale: exit 2" "$(yes_if test "$(rc "$work/stale")" = 2)"
check "stale: refuses by name (no cdylib reported)" \
    "$(yes_if grep -qF 'reported 0 cdylib file(s) for it' "$work/stale.stderr")"
check "stale: nothing is reported as staged" "$(yes_if test ! -s "$work/stale.stdout")"

# (triple) CARGO_BUILD_TARGET moves Cargo's output under TARGET/<triple>/.
workspace "$ws" '"cdylib", "rlib"' || { echo "FAIL - cannot rewrite the fixture workspace"; exit 1; }
triple=$(rustc -vV | sed -n 's/^host: //p')
check "triple: the host triple is known" "$(yes_if test -n "$triple")"
CARGO_BUILD_TARGET=$triple stage "$work/triple" --source "$ws" --target-dir "$work/target-triple" --profile dev --offline
check "triple: exit 0" "$(yes_if test "$(rc "$work/triple")" = 0)"
check "triple: stages what Cargo reports, under TARGET/<triple>/debug/deps" \
    "$(yes_if grep -qF "staged $work/target-triple/$triple/debug/deps/libdetcore_liteinst.so " "$work/triple.stdout")"

# Cargo's own failure keeps Cargo's status and names the whole command.
stage "$work/undefined" --source "$ws" --target-dir "$work/target" --profile no-such-profile --offline
check "undefined profile: Cargo's status (101)" "$(yes_if test "$(rc "$work/undefined")" = 101)"
check "undefined profile: the failed command is named, --offline included" \
    "$(yes_if grep -qF 'stage-detcore-liteinst: cargo build --locked --offline --message-format=json-render-diagnostics' "$work/undefined.stderr")"

# Usage errors exit 2 by name before any Cargo call.
stage "$work/nosource" --target-dir "$work/target" --profile dev
check "usage: a missing --source exits 2 by name" \
    "$(yes_if test "$(rc "$work/nosource")" = 2 -a -n "$(grep -F -- '--source is required' "$work/nosource.stderr")")"
stage "$work/unknown" --source "$ws" --target-dir "$work/target" --profile dev --offline=1
check "usage: an unknown flag exits 2 by name" \
    "$(yes_if test "$(rc "$work/unknown")" = 2 -a -n "$(grep -F "unexpected argument '--offline=1'" "$work/unknown.stderr")")"

if ((failures)); then
    echo "stage-detcore-liteinst-test: $failures FAILED"
    exit 1
fi
echo "stage-detcore-liteinst-test: all checks passed"
