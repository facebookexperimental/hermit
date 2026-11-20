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
#   1. today's graph passes with exactly the two named debts flagged:
#      the KVM maps extension in the Buck closure (Sapling scope only;
#      git scope never judges it), and reverie-procfs only on the
#      Cargo surfaces until the pin advance (S2-R4)
#   2. a planted reverie-linuxemu-ext-* path dependency refuses
#   3. features on their own line (spread table) refuses [seat bypass a]
#   4. a multiline renamed key (package = "...") refuses [seat bypass b]
#   5. an expired allow-until refuses: on a planted reverie-procfs
#      fixture, and on today's real graph judged after 2026-12-31
#   6. the same planted extension under a Sapling (sl) scope refuses
#   7. a directory under no VCS errors closed (rc 2), never guesses
#   8. an Eden-shaped Sapling checkout nested in an outer git repo is
#      audited at the inner root even though bare `sl root` escapes
#   9. with sl removed from PATH, git controls still refuse a planted
#      extension and Sapling controls are named, counted skips
#  10. buck2 closure labels are judged (fake buck2 over a Sapling
#      fbcode fixture): kind-derived roots incl. a NEW primary, a
#      denied label as Buck expands it from variables/concatenation/
#      select()/macros/.bzl data refuses; a clean closure (incl.
#      reverie-proc-grammar) passes; the excluded harness root
#      re-joins loudly once its closure evaluates
#  11. buck2 missing / failing / empty closure each fail closed (rc 2)
#  12. a default `dep:<name>` activation does not hide a distinct
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
# Twins 9 and 11 run the checker under a PATH narrowed to a few tools. That
# PATH must narrow what the CHECKER finds, not what runs it: the validation
# graph's rust-script is ci/rust-script-bin/rust-script, a bash shim over a
# prepared binary that itself needs bash, realpath, dirname and awk, and under
# the narrowed PATH it died with "/usr/bin/env: 'bash': No such file or
# directory". Resolve the prepared binary once; a plain rust-script install
# has no --resolve-optional, keeps compiling the source (the narrowed PATH
# carries cargo and rustc for it), and is run as before.
CHECKER_CMD=("$RUST_SCRIPT_BIN" "$CHECKER")
if prepared=$("$RUST_SCRIPT_BIN" --resolve-optional "$CHECKER" 2>/dev/null) \
    && [[ -x $prepared ]]; then
    CHECKER_CMD=("$prepared")
fi

echo "twin 1: today's graph passes with only the two named debts flagged (extension in Buck where judged, procfs on Cargo surfaces until the pin advance)"
OUT=$("${CHECKER_CMD[@]}" --repo-root "$REPO_ROOT" 2>&1); RC=$?
# The Buck closure is judged only in Sapling scope; in git scope the
# checker witnesses it by the Cargo legs and the extension (a Buck-only
# dependency) cannot appear. Scope is read from the checker's own
# output: Sapling runs print flagged "buck2 deps closure" lines and/or
# the PASS line's "Buck closure via buck2 uquery" note.
if ((RC != 0)); then fail "real graph rc=$RC (want 0): $OUT";
elif grep -q ": reverie-procfs at buck2" <<<"$OUT"; then fail "reverie-procfs back in the Buck graph: $OUT";
elif grep "^FLAGGED" <<<"$OUT" | grep -v "reverie-linuxemu-ext-procfs" \
    | grep -qv ": reverie-procfs at Cargo"; then
    fail "a flagged debt other than the KVM maps extension or Cargo-surface procfs: $OUT";
elif ! grep -q ": reverie-linuxemu-ext-procfs at Cargo" <<<"$OUT"; then
    fail "real graph passed without the Cargo-surface procfs extension (reverie-linuxemu-ext-procfs, renamed from reverie-procfs) flagged: $OUT";
elif { grep -q "buck2 deps closure" <<<"$OUT" \
    || grep -q "Buck closure via buck2 uquery" <<<"$OUT"; } \
    && ! grep -q "^FLAGGED.*reverie-linuxemu-ext-procfs" <<<"$OUT"; then
    fail "Buck closure judged without flagging the KVM maps extension: $OUT";
elif grep -q "flagged allow-until debt" <<<"$OUT"; then
    note "ok (rc=0, only the named debts flagged for this scope)";
else fail "real graph passed without the flagged-debt line: $OUT"; fi

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

echo "twin 5: expired allow-until refuses a planted reverie-procfs, naming it EXPIRED"
make_ws 'reverie-procfs = { path = "../stub" }' reverie-procfs ''
OUT=$(rust-script "$CHECKER" --repo-root "$scratch/repo" \
    --deny-file "$scratch/scripts/linuxemu-graph-deny.txt" \
    --today 2027-06-01 --skip-negative-control 2>&1); RC=$?
if ((RC == 1)) && grep -q "reverie-procfs" <<<"$OUT" && grep -q "EXPIRED" <<<"$OUT";
then note "ok (expired allow refuses)";
else fail "expiry twin: rc=$RC (want 1, naming reverie-procfs EXPIRED): $OUT"; fi

echo "twin 5b: today's real graph refuses after the allow-until deadline"
OUT=$("${CHECKER_CMD[@]}" --repo-root "$REPO_ROOT" \
    --today 2027-06-01 --skip-negative-control 2>&1); RC=$?
if ((RC != 1)); then fail "real-graph expiry: rc=$RC (want 1): $OUT";
elif ! grep -q "EXPIRED" <<<"$OUT" || ! grep -q ": reverie-linuxemu-ext-procfs at Cargo" <<<"$OUT"; then
    fail "real-graph expiry does not name the Cargo-surface procfs extension (reverie-linuxemu-ext-procfs) EXPIRED: $OUT";
elif grep -q "buck2 deps closure" <<<"$OUT" \
    && ! grep -q "reverie-linuxemu-ext-procfs .*EXPIRED" <<<"$OUT"; then
    fail "real-graph expiry judged the Buck closure without expiring the extension: $OUT";
else note "ok (real graph refuses past 2026-12-31)"; fi

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
    OUT=$(env PATH="$fakebin" "${CHECKER_CMD[@]}" --repo-root "$scratch/repo" \
        --deny-file "$scratch/scripts/linuxemu-graph-deny.txt" 2>&1); RC=$?
    if ((RC == 1)) && grep -q "REFUSE" <<<"$OUT" \
        && grep -q "SKIP SUMMARY (counted): 2 Sapling negative control(s)" <<<"$OUT"; then
        note "ok (git controls refused ext; Sapling controls counted as skipped)"
    else
        fail "git-only PATH: rc=$RC (want 1 REFUSE with counted Sapling skips): $OUT"
    fi
fi

echo "twin 10: buck2 closure labels judged (fake buck2, Sapling fbcode fixture)"
if command -v sl >/dev/null 2>&1; then
    buckroot="$scratch/buckrepo"
    buckaudit="$buckroot/fbcode/hermetic_infra/hermit"
    rm -rf "$buckroot"
    mkdir -p "$buckaudit/consumer/src" "$buckaudit/stub/src" "$scratch/buckbin"
    printf '[workspace]\nmembers = ["consumer", "stub"]\nresolver = "2"\n' \
        > "$buckaudit/Cargo.toml"
    printf '[package]\nname = "consumer"\nversion = "0.0.0"\nedition = "2021"\n[dependencies]\nstub = { path = "../stub" }\n' \
        > "$buckaudit/consumer/Cargo.toml"
    printf '[package]\nname = "stub"\nversion = "0.0.0"\nedition = "2021"\n' \
        > "$buckaudit/stub/Cargo.toml"
    : > "$buckaudit/consumer/src/lib.rs"
    : > "$buckaudit/stub/src/lib.rs"
    (cd "$buckaudit" && cargo generate-lockfile --offline >/dev/null 2>&1) \
        || { echo "buck fixture generate-lockfile failed" >&2; exit 2; }
    (cd "$buckroot" && sl init . >/dev/null 2>&1 && sl -R . add >/dev/null 2>&1) \
        || { echo "buck fixture sl setup failed" >&2; exit 2; }
    cat > "$scratch/buckbin/buck2" <<'FAKEBUCK'
#!/usr/bin/env bash
# Recorded-output buck2 stand-in. Modes via env:
#   BUCK_FIXTURE_KIND     file printed for the kind() root query
#   BUCK_FIXTURE_LABELS   file printed for deps(set(...))
#   BUCK_FIXTURE_EXCLUDED ok|fail for the excluded-root probe
#   BUCK_FIXTURE_MODE     ok|fail|empty (overall deps behaviour)
expr="${@: -1}"
case "$expr" in
    *validate-test-hermit-integration*)
        [[ ${BUCK_FIXTURE_EXCLUDED:-fail} == ok ]]             && echo 'fbcode//hermetic_infra/hermit/hermit-cli:validate'             || exit 3 ;;
    deps\(set\(*)
        case "${BUCK_FIXTURE_MODE:-ok}" in
            fail) exit 3 ;;
            empty) ;;
            *) cat "$BUCK_FIXTURE_LABELS" ;;
        esac ;;
    kind\(*)
        [[ ${BUCK_FIXTURE_MODE:-ok} == fail ]] && exit 3
        cat "$BUCK_FIXTURE_KIND" ;;
    *) exit 3 ;;
esac
FAKEBUCK
    chmod +x "$scratch/buckbin/buck2"
    printf '%s\n' \
        'fbcode//hermetic_infra/reverie:reverie-linuxemu' \
        'fbcode//hermetic_infra/reverie:reverie-procfs' \
        'fbcode//hermetic_infra/reverie:reverie-ptrace' \
        'fbcode//hermetic_infra/reverie:reverie-proc-grammar' \
        > "$scratch/labels-clean.txt"
    # Kind-derived roots, including a primary that did not exist when
    # the checker was written (hermit-newtool): derivation must pick
    # it up without a checker edit.
    printf '%s\n' \
        'fbcode//hermetic_infra/hermit/hermit-cli:hermit' \
        'fbcode//hermetic_infra/hermit/hermit-cli:libhermit' \
        'fbcode//hermetic_infra/hermit/hermit-cli:hermit-newtool' \
        'fbcode//hermetic_infra/hermit/hermit-cli:integration-cli' \
        > "$scratch/kind-roots.txt"
    run_buck_fixture() { # labels-file excluded-mode buck-mode
        OUT=$(BUCK_FIXTURE_KIND="$scratch/kind-roots.txt" \
            BUCK_FIXTURE_LABELS="$1" BUCK_FIXTURE_EXCLUDED="$2" \
            BUCK_FIXTURE_MODE="$3" \
            env PATH="$scratch/buckbin:$PATH" \
            rust-script "$CHECKER" --repo-root "$buckaudit" \
            --deny-file "$scratch/scripts/linuxemu-graph-deny.txt" \
            --skip-negative-control 2>&1); RC=$?
    }
    run_buck_fixture "$scratch/labels-clean.txt" fail ok
    if ((RC == 0)) && grep -q "Buck closure via buck2 uquery" <<<"$OUT" \
        && grep -q "excluded (closure not evaluatable" <<<"$OUT"; then
        note "ok (clean buck closure passes, procfs flagged, proc-grammar accepted)"
    else
        fail "buck clean closure: rc=$RC (want 0 PASS with buck note): $OUT"
    fi
    for form in variable concatenation select macro bzl-data new-primary; do
        cp "$scratch/labels-clean.txt" "$scratch/labels-denied.txt"
        echo 'fbcode//hermetic_infra/reverie:reverie-linuxemu-ext-demo' \
            >> "$scratch/labels-denied.txt"
        run_buck_fixture "$scratch/labels-denied.txt" fail ok
        if ((RC == 1)) && grep -q "REFUSE" <<<"$OUT"; then
            note "ok (buck closure refuses $form-expanded denied edge)"
        else
            fail "buck closure $form: rc=$RC (want 1 REFUSE): $OUT"
        fi
    done
    run_buck_fixture "$scratch/labels-clean.txt" ok ok
    if ((RC == 0)) && grep -q "RE-JOINED" <<<"$OUT"; then
        note "ok (excluded harness root re-joins loudly once evaluatable)"
    else
        fail "buck rejoin: rc=$RC (want 0 PASS with RE-JOINED): $OUT"
    fi
else
    skip "twin 10 buck2 closure judging requires sl, which is absent"
fi

echo "twin 11: buck2 missing / failing / empty closure fail closed (rc 2)"
if command -v sl >/dev/null 2>&1; then
    fakebin="$scratch/fakebin"
    OUT=$(BUCK_FIXTURE_KIND="$scratch/kind-roots.txt" \
        BUCK_FIXTURE_LABELS="$scratch/labels-clean.txt" \
        env PATH="$fakebin" "${CHECKER_CMD[@]}" \
        --repo-root "$buckaudit" \
        --deny-file "$scratch/scripts/linuxemu-graph-deny.txt" \
        --skip-negative-control 2>&1); RC=$?
    if ((RC == 2)) && grep -q "CHECKER ERROR" <<<"$OUT"; then
        note "ok (buck2 missing rc=2)"
    else
        fail "buck2 missing: rc=$RC (want 2 CHECKER ERROR): $OUT"
    fi
    run_buck_fixture "$scratch/labels-clean.txt" fail fail
    if ((RC == 2)) && grep -q "CHECKER ERROR" <<<"$OUT"; then
        note "ok (buck2 failing rc=2)"
    else
        fail "buck2 failing: rc=$RC (want 2 CHECKER ERROR): $OUT"
    fi
    run_buck_fixture "$scratch/labels-clean.txt" fail empty
    if ((RC == 2)) && grep -q "CHECKER ERROR" <<<"$OUT"; then
        note "ok (buck2 empty closure rc=2)"
    else
        fail "buck2 empty closure: rc=$RC (want 2 CHECKER ERROR): $OUT"
    fi
else
    skip "twin 11 buck2 fail-closed paths require sl, which is absent"
fi

echo "twin 12: default dep: activation does not hide same-named feature"
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
