#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Red twins for check-linuxemu-graph.rs (Slice 1 of the LinuxEmu
# modularity restart): the gate is worth nothing unless it can be seen
# to refuse. Each case is a real run of the checker, which reads the
# resolved graph (cargo metadata + TOML-parsed lockfiles), so manifest
# formatting games cannot slip it.
#
#   1. today's graph passes, reverie-procfs FLAGGED as expiring debt
#   2. a planted reverie-linuxemu-ext-* path dependency refuses
#   3. features on their own line (spread table) refuses [seat bypass a]
#   4. a multiline renamed key (package = "...") refuses [seat bypass b]
#   5. the real graph judged after the allow-until date refuses,
#      naming reverie-procfs EXPIRED
#   6. the same planted extension under a Sapling (sl) scope refuses
#   7. a directory under no VCS errors closed (rc 2), never guesses
#   8. an Eden-shaped Sapling checkout nested in an outer git repo is
#      audited at the inner root even though bare `sl root` escapes
#   9. with sl removed from PATH, git controls still refuse a planted
#      extension and Sapling controls are named, counted skips
#  10. BUCK mentions that are not dependency edges pass; a real
#      Buck deps label to a denied target refuses
#  11. a default `dep:<name>` activation does not hide a distinct
#      same-named non-default feature
# Sapling twins are skipped only when sl is absent; every skip is
# named and counted in the final summary, never silent.
#
# Exit: 0 all twins behave, 1 a twin misbehaved, 2 environment error.
set -uo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
readonly SCRIPT_DIR REPO_ROOT
readonly CHECKER="$SCRIPT_DIR/check-linuxemu-graph.rs"

failures=0
skips=0
note() { echo "  $*"; }
fail() { echo "check-linuxemu-graph-test.sh: FAIL — $*" >&2; failures=$((failures + 1)); }
skip() { skips=$((skips + 1)); echo "  SKIP (counted): $*"; }
RUST_SCRIPT_BIN=$(command -v rust-script || true)

[[ -f $CHECKER ]] || { echo "missing $CHECKER" >&2; exit 2; }
[[ -n $RUST_SCRIPT_BIN ]] || { echo "rust-script not on PATH" >&2; exit 2; }
command -v cargo >/dev/null 2>&1 || { echo "cargo not on PATH" >&2; exit 2; }

echo "twin 1: today's graph passes; reverie-procfs flagged, not hidden"
OUT=$(rust-script "$CHECKER" --repo-root "$REPO_ROOT" 2>&1); RC=$?
if ((RC != 0)); then fail "real graph rc=$RC (want 0): $OUT";
elif grep -q "FLAGGED.*reverie-procfs" <<<"$OUT"; then note "ok (rc=0, procfs flagged)";
else fail "real graph passed without flagging reverie-procfs: $OUT"; fi

scratch=$(mktemp -d)
trap 'rm -rf -- "$scratch"' EXIT
mkdir -p "$scratch/scripts"
cp "$SCRIPT_DIR/linuxemu-graph-deny.txt" "$scratch/scripts/"

# make_ws <consumer-dep-toml> <stub-name> <stub-extra-toml>
make_ws() {
    rm -rf "$scratch/repo"
    mkdir -p "$scratch/repo/consumer/src" "$scratch/repo/stub/src"
    printf '[workspace]\nmembers = ["consumer", "stub"]\nresolver = "2"\n' \
        > "$scratch/repo/Cargo.toml"
    printf '[package]\nname = "consumer"\nversion = "0.0.0"\nedition = "2021"\n[dependencies]\n%s\n' "$1" \
        > "$scratch/repo/consumer/Cargo.toml"
    printf '[package]\nname = "%s"\nversion = "0.0.0"\nedition = "2021"\n%s\n' "$2" "$3" \
        > "$scratch/repo/stub/Cargo.toml"
    : > "$scratch/repo/consumer/src/lib.rs"
    : > "$scratch/repo/stub/src/lib.rs"
    (cd "$scratch/repo" && cargo generate-lockfile --offline) >/dev/null 2>&1 \
        || { echo "fixture generate-lockfile failed" >&2; exit 2; }
    git -C "$scratch/repo" init -q && git -C "$scratch/repo" add -A
}

expect_refuse() { # label
    OUT=$(rust-script "$CHECKER" --repo-root "$scratch/repo" \
        --deny-file "$scratch/scripts/linuxemu-graph-deny.txt" \
        --skip-negative-control 2>&1); RC=$?
    if ((RC == 1)) && grep -q "REFUSE" <<<"$OUT"; then note "ok ($1 refused)";
    else fail "$1: rc=$RC (want 1 REFUSE): $OUT"; fi
}

echo "twin 2: planted extension package refuses"
make_ws 'reverie-linuxemu-ext-demo = { path = "../stub" }' reverie-linuxemu-ext-demo ''
expect_refuse "ext path dep"

echo "twin 3: features on their own line refuses (seat bypass a)"
make_ws 'reverie-linuxemu = {
    path = "../stub",
    features = [
        "test-support",
    ],
}' reverie-linuxemu '[features]
default = []
test-support = []'
expect_refuse "features own line"

echo "twin 4: multiline renamed key refuses (seat bypass b)"
make_ws 'renamed = {
    package =
        "reverie-linuxemu-ext-demo",
    path = "../stub",
}' reverie-linuxemu-ext-demo ''
expect_refuse "renamed multiline"

echo "twin 5: expired allow-until refuses on the real graph, naming reverie-procfs"
OUT=$(rust-script "$CHECKER" --repo-root "$REPO_ROOT" --today 2027-06-01 \
    --skip-negative-control 2>&1); RC=$?
if ((RC == 1)) && grep -q "reverie-procfs" <<<"$OUT" && grep -q "EXPIRED" <<<"$OUT";
then note "ok (expired allow refuses)";
else fail "expiry twin: rc=$RC (want 1, naming reverie-procfs EXPIRED): $OUT"; fi

echo "twin 6: planted extension under Sapling scope refuses"
make_ws 'reverie-linuxemu-ext-demo = { path = "../stub" }' reverie-linuxemu-ext-demo ''
if command -v sl >/dev/null 2>&1; then
    rm -rf "$scratch/repo/.git"
    (cd "$scratch/repo" && sl init >/dev/null 2>&1 && sl add >/dev/null 2>&1) \
        || { echo "sl fixture setup failed" >&2; exit 2; }
    expect_refuse "ext under sl"
else
    skip "twin 6 planted extension under Sapling scope requires sl, which is absent"
fi

echo "twin 7: no VCS at all errors closed (rc 2)"
rm -rf "$scratch/novcs"; mkdir -p "$scratch/novcs"
printf '[[package]]\nname = "reverie-procfs"\nversion = "0.5.7"\n' > "$scratch/novcs/Cargo.lock"
OUT=$(rust-script "$CHECKER" --repo-root "$scratch/novcs" \
    --deny-file "$scratch/scripts/linuxemu-graph-deny.txt" \
    --skip-negative-control 2>&1); RC=$?
if ((RC == 2)) && grep -q "CHECKER ERROR" <<<"$OUT"; then note "ok (unknown VCS rc=2)";
else fail "no-VCS twin: rc=$RC (want 2 CHECKER ERROR): $OUT"; fi

echo "twin 8: nested Eden-shaped Sapling checkout selects the inner root"
if command -v sl >/dev/null 2>&1; then
    rm -rf "$scratch/nested"
    outer="$scratch/nested/outer"
    inner="$outer/inner"
    audit="$inner/audit"
    mkdir -p "$audit/consumer/src" "$audit/stub/src" "$scratch/nested/eden-marker"
    printf '[[package]]\nname = "reverie-linuxemu-ext-demo"\nversion = "0.0.0"\n' \
        > "$outer/Cargo.lock"
    printf '[workspace]\nmembers = ["consumer", "stub"]\nresolver = "2"\n' \
        > "$audit/Cargo.toml"
    printf '[package]\nname = "consumer"\nversion = "0.0.0"\nedition = "2021"\n[dependencies]\nstub = { path = "../stub" }\n' \
        > "$audit/consumer/Cargo.toml"
    printf '[package]\nname = "stub"\nversion = "0.0.0"\nedition = "2021"\n' \
        > "$audit/stub/Cargo.toml"
    : > "$audit/consumer/src/lib.rs"
    : > "$audit/stub/src/lib.rs"
    ln -s "$scratch/nested/eden-marker" "$audit/.eden"
    if ! git -C "$outer" init -q || ! git -C "$outer" add Cargo.lock; then
        echo "nested fixture outer git setup failed" >&2; exit 2;
    fi
    (cd "$inner" && sl init . >/dev/null 2>&1) \
        || { echo "nested fixture inner sl setup failed" >&2; exit 2; }
    (cd "$audit" && cargo generate-lockfile --offline >/dev/null 2>&1) \
        || { echo "nested fixture generate-lockfile failed" >&2; exit 2; }
    (cd "$inner" && sl -R . add >/dev/null 2>&1) \
        || { echo "nested fixture inner sl add failed" >&2; exit 2; }
    bare_root=$(cd "$audit" && sl root 2>&1) || bare_root="sl-root-failed"
    outer_real=$(cd "$outer" && pwd -P)
    if [[ $bare_root != "$outer_real" ]]; then
        fail "nested fixture did not reproduce Eden escape: bare sl root=$bare_root, want outer $outer_real"
    else
        OUT=$(rust-script "$CHECKER" --repo-root "$audit" \
            --deny-file "$scratch/scripts/linuxemu-graph-deny.txt" \
            --skip-negative-control 2>&1); RC=$?
        if ((RC == 0)) && grep -q "^PASS" <<<"$OUT"; then
            note "ok (bare sl root escaped to outer; checker audited inner clean workspace)"
        else
            fail "nested Eden escape: rc=$RC (want 0 PASS from inner scope): $OUT"
        fi
    fi
else
    skip "twin 8 nested Eden-shaped Sapling checkout requires sl, which is absent"
fi

echo "twin 9: git-only PATH still runs git controls and counts Sapling skips"
make_ws 'reverie-linuxemu-ext-demo = { path = "../stub" }' reverie-linuxemu-ext-demo ''
fakebin="$scratch/fakebin"
mkdir -p "$fakebin"
for tool in git cargo rustc rustup cc gcc ld ar ranlib sh env; do
    tool_path=$(command -v "$tool" || true)
    [[ -n $tool_path ]] && ln -s "$tool_path" "$fakebin/$tool"
done
if [[ -x $fakebin/sl ]]; then
    fail "git-only PATH fixture unexpectedly contains sl"
else
    OUT=$(env PATH="$fakebin" "$RUST_SCRIPT_BIN" "$CHECKER" --repo-root "$scratch/repo" \
        --deny-file "$scratch/scripts/linuxemu-graph-deny.txt" 2>&1); RC=$?
    if ((RC == 1)) && grep -q "REFUSE" <<<"$OUT" \
        && grep -q "SKIP SUMMARY (counted): 2 Sapling negative control(s)" <<<"$OUT"; then
        note "ok (git controls refused ext; Sapling controls counted as skipped)"
    else
        fail "git-only PATH: rc=$RC (want 1 REFUSE with counted Sapling skips): $OUT"
    fi
fi

echo "twin 10: BUCK non-edge mentions pass; a real Buck edge refuses"
make_ws 'stub = { path = "../stub" }' stub ''
cat > "$scratch/repo/BUCK" <<'BUCK'
# reverie-linuxemu-ext-demo reverie-linuxemu-full reverie-procfs
rust_library(
    name = "reverie-linuxemu-ext-demo",
    srcs = ["x.rs"],  # TODO drop reverie-linuxemu-full later
    visibility = ["//hermetic_infra/reverie:reverie-linuxemu-ext-demo"],
    note = "reverie-procfs",
)
BUCK
git -C "$scratch/repo" add BUCK
OUT=$(rust-script "$CHECKER" --repo-root "$scratch/repo" \
    --deny-file "$scratch/scripts/linuxemu-graph-deny.txt" \
    --skip-negative-control 2>&1); RC=$?
if ((RC == 0)) && grep -q "^PASS" <<<"$OUT"; then
    note "ok (BUCK comments/names/visibility are not edges)"
else
    fail "BUCK non-edge: rc=$RC (want 0 PASS): $OUT"
fi
cat > "$scratch/repo/BUCK" <<'BUCK'
rust_library(
    name = "consumer",
    deps = [
        "//hermetic_infra/reverie:reverie-linuxemu-ext-demo",
    ],
)
BUCK
git -C "$scratch/repo" add BUCK
expect_refuse "BUCK real edge"

echo "twin 11: default dep: activation does not hide same-named feature"
rm -rf "$scratch/repo"
mkdir -p "$scratch/repo/consumer/src" "$scratch/repo/stub/src" "$scratch/repo/procfsdep/src"
printf '[workspace]\nmembers = ["consumer", "stub", "procfsdep"]\nresolver = "2"\n' \
    > "$scratch/repo/Cargo.toml"
printf '[package]\nname = "consumer"\nversion = "0.0.0"\nedition = "2021"\n[dependencies]\nreverie-linuxemu = { path = "../stub", features = ["procfs"] }\n' \
    > "$scratch/repo/consumer/Cargo.toml"
printf '[package]\nname = "reverie-linuxemu"\nversion = "0.0.0"\nedition = "2021"\n[dependencies]\nprocfs = { path = "../procfsdep", optional = true }\n[features]\ndefault = ["dep:procfs"]\nprocfs = []\n' \
    > "$scratch/repo/stub/Cargo.toml"
printf '[package]\nname = "procfs"\nversion = "0.0.0"\nedition = "2021"\n' \
    > "$scratch/repo/procfsdep/Cargo.toml"
: > "$scratch/repo/consumer/src/lib.rs"
: > "$scratch/repo/stub/src/lib.rs"
: > "$scratch/repo/procfsdep/src/lib.rs"
(cd "$scratch/repo" && cargo generate-lockfile --offline >/dev/null 2>&1) \
    || { echo "feature-collision fixture generate-lockfile failed" >&2; exit 2; }
git -C "$scratch/repo" init -q && git -C "$scratch/repo" add -A
expect_refuse "dep: collision feature"

if ((failures > 0)); then
    echo "check-linuxemu-graph-test.sh: $failures twin(s) misbehaved, $skips named Sapling skip(s)" >&2
    exit 1
fi
echo "check-linuxemu-graph-test.sh: all twins pass ($skips named Sapling skip(s))"
