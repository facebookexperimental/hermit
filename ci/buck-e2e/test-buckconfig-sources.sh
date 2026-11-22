#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Tests ci/buck-e2e/buckconfig-sources.sh with a stub buck2 that prints a canned
# `audit config --all-cells --location direct` dump, in a throwaway git checkout. Each case
# runs in a subshell, because a refusal exits 2. No real buck2 and no daemon.
# Run by hand: ci/buck-e2e/test-buckconfig-sources.sh
# Each case sets root, buck2 and HOME inside its own subshell on purpose, so that no case
# leaks into the next.
# shellcheck disable=SC2030,SC2031
set -euo pipefail
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
pass=0
fail() { echo "test-buckconfig-sources.sh: FAIL: $*" >&2; exit 1; }

# A checkout: a tracked .buckconfig with a bundled prelude cell and a shim cell, a tracked
# shim/.buckconfig, and the tracked stock settings file.
co=$work/checkout
mkdir -p "$co/shim" "$co/ci/buck-e2e"
printf '[cells]\nroot = .\nshim = shim\nprelude = prelude\n\n[external_cells]\nprelude = bundled\n' >"$co/.buckconfig"
printf '[buildfile]\nname = BUCK\n' >"$co/shim/.buckconfig"
printf '[buck2]\nsqlite_dep_file_state = false\n\n[buck2_resource_control]\nstatus = if_available\n' >"$co/ci/buck-e2e/stock-runner.buckconfig"
git -C "$co" init -q
git -C "$co" add .buckconfig shim/.buckconfig ci/buck-e2e/stock-runner.buckconfig
git -C "$co" -c user.name=t -c user.email=t@t commit -q -m base
stock=$co/ci/buck-e2e/stock-runner.buckconfig

# The stub buck2 prints $STUB_DUMP for `audit config`.
cat >"$work/buck2" <<'EOF'
#!/bin/bash
[[ ${1:-} == audit ]] && cat "$STUB_DUMP"
EOF
chmod +x "$work/buck2"

clean_dump() {
    cat <<EOF
# Cell: root
[buck2]
    sqlite_dep_file_state = false
  (defined at $stock:2)
[buck2_resource_control]
    status = if_available
  (defined at $stock:5)
[cells]
    shim = shim
  (defined at .buckconfig:3)
[buildfile]
    name = BUCK
  (defined at shim/.buckconfig:2)
[prelude_section]
    key = 1
  (defined at prelude/.buckconfig:7)
EOF
}

# expect WANT NAME DUMP_FILE [setup command...]: run the check and the pre-daemon refusal
# in a subshell on the checkout, and require exit WANT.
expect() {
    local want=$1 name=$2 dump=$3 got
    shift 3
    set +e
    (
        set -euo pipefail
        root=$co buck2=$work/buck2 STUB_DUMP=$dump HOME=$work/home
        mkdir -p "$HOME"
        step() { :; }
        # shellcheck source-path=SCRIPTDIR source=buckconfig-sources.sh
        source "$here/buckconfig-sources.sh"
        export STUB_DUMP
        "$@"
        refuse_untracked_buck_config
        check_buckconfig_sources "$work/out.txt" "$work/records.tsv"
    ) >"$work/case.log" 2>&1
    got=$?
    set -e
    [[ $got == "$want" ]] || { cat "$work/case.log" >&2; fail "$name: exit $got, want $want"; }
    # A refusal says why; exit 2 from a failing command under set -e (grep's status, say) does not.
    if [[ $want == 2 ]] && ! grep -q '^validate-node: ' "$work/case.log"; then
        cat "$work/case.log" >&2
        fail "$name: exit 2 without a refusal message"
    fi
    pass=$((pass + 1))
}

clean_dump >"$work/clean"
expect 0 "clean dump" "$work/clean" true

: >"$work/empty"
expect 2 "empty dump" "$work/empty" true

{ clean_dump; printf '[x]\n    y = 1\n  (defined at /etc/buckconfig.d/host.ini:3)\n'; } >"$work/abs"
expect 2 "absolute (machine-local) location" "$work/abs" true

{ clean_dump; printf '[build]\n    execution_platforms = cache\n  (defined at .buckconfig.local:2)\n'; } >"$work/local"
printf '[build]\nexecution_platforms = cache\n' >"$work/outside.buckconfig"
expect 2 "linked .buckconfig.local" "$work/local" ln -s "$work/outside.buckconfig" "$co/.buckconfig.local"
rm -f -- "$co/.buckconfig.local"
expect 2 "untracked .buckconfig.local" "$work/local" cp "$work/outside.buckconfig" "$co/.buckconfig.local"
rm -f -- "$co/.buckconfig.local"

{ clean_dump; printf '[y]\n    z = 1\n  (defined at shim/.buckconfig.local:1)\n'; } >"$work/sublocal"
expect 2 "untracked .buckconfig.local in another cell" "$work/sublocal" cp "$work/outside.buckconfig" "$co/shim/.buckconfig.local"
rm -f -- "$co/shim/.buckconfig.local"

cp "$co/shim/.buckconfig" "$work/shim.bak"
expect 2 "tracked file edited" "$work/clean" sh -c "echo '# edited' >>'$co/shim/.buckconfig'"
cp "$work/shim.bak" "$co/shim/.buckconfig"

expect 2 "a prelude/ directory in the checkout" "$work/clean" mkdir "$co/prelude"
rmdir -- "$co/prelude"

sed "s|  (defined at $stock:2)|  (defined at .buckconfig:9)|" "$work/clean" >"$work/movedkey"
expect 2 "sqlite_dep_file_state not from the stock file" "$work/movedkey" true

sed 's/status = if_available/status = off/' "$work/clean" >"$work/offkey"
expect 2 "status not if_available" "$work/offkey" true

expect 2 "a home settings file" "$work/clean" sh -c "mkdir -p '$work/home' && : >'$work/home/.bucksettings.local.toml'"
rm -f -- "$work/home/.bucksettings.local.toml"

expect 0 "clean again after the cases" "$work/clean" true

# The environment refusal.
set +e
(
    root=$co
    # shellcheck source-path=SCRIPTDIR source=buckconfig-sources.sh
    source "$here/buckconfig-sources.sh"
    export BUCK2_TEST_RESOURCE_CONTROL_CONFIG='{}'
    export_stock_buckconfig_env
) >"$work/case.log" 2>&1
got=$?
set -e
[[ $got == 2 ]] || fail "BUCK2_TEST_RESOURCE_CONTROL_CONFIG: exit $got, want 2"
pass=$((pass + 1))
(
    root=$co
    # shellcheck source-path=SCRIPTDIR source=buckconfig-sources.sh
    source "$here/buckconfig-sources.sh"
    export_stock_buckconfig_env
    [[ $BUCK2_TEST_SKIP_DEFAULT_EXTERNAL_CONFIG == 1 && $BUCK2_TEST_EXTRA_EXTERNAL_CONFIG == "$co/ci/buck-e2e/stock-runner.buckconfig" ]]
) || fail "export_stock_buckconfig_env did not export both knobs"
pass=$((pass + 1))

# The records of the machine-local files: a present one with its sha256, a missing one absent.
mkdir -p "$work/home/.buckconfig.d"
printf '[x]\ny = 1\n' >"$work/home/.buckconfig.d/rollout"
(
    root=$co HOME=$work/home
    # shellcheck source-path=SCRIPTDIR source=buckconfig-sources.sh
    source "$here/buckconfig-sources.sh"
    record_external_buckconfigs
) >"$work/external.tsv"
tab=$'\t'
grep -q "^$work/home/.buckconfig.d/rollout$tab$(sha256sum <"$work/home/.buckconfig.d/rollout" | cut -c1-64)$tab$(stat -c %s "$work/home/.buckconfig.d/rollout")$" "$work/external.tsv" ||
    fail "record_external_buckconfigs: no sha256 line for a present file: $(cat "$work/external.tsv")"
grep -q "^$work/home/.buckconfig.local${tab}absent$" "$work/external.tsv" ||
    fail "record_external_buckconfigs: no absent line for a missing file"
pass=$((pass + 1))

echo "test-buckconfig-sources.sh: $pass checks passed"
