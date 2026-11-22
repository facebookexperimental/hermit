#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Tests ci/buck-e2e/buckconfig-sources.sh with a stub buck2 that prints a canned
# `audit config --all-cells --location direct` dump, in a throwaway git checkout. Each case
# runs in a subshell, because a refusal exits 2. No real buck2 and no daemon.
# Run by `make lint-check-buck-e2e-buckconfig-sources` (part of `make lint-checks`), or by
# hand: ci/buck-e2e/test-buckconfig-sources.sh
# Each case sets root, buck2 and HOME inside its own subshell on purpose, so that no case
# leaks into the next.
# shellcheck disable=SC2030,SC2031
set -euo pipefail
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# git reads only what the cases set up: no inherited GIT_* variable (a hook's GIT_DIR, say),
# and no system or global config (a host clean filter would change what gets hashed).
while read -r name; do unset "$name"; done < <(compgen -e | grep '^GIT_' || true)
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null
# Each signal exits 128+N, and that exit's EXIT trap removes the scratch directory. The trap
# is installed before mktemp, so an interrupt cannot leak the directory, and it keeps the
# script's own exit status.
work=
rc=
for sig in 1 2 3 13 15; do eval "trap 'exit $((sig + 128))' $sig"; done
trap 'rc=$?; set +e; rm -rf -- "$work"; exit $rc' EXIT
work=$(mktemp -d) || exit 1
pass=0
fail() { echo "test-buckconfig-sources.sh: FAIL: $*" >&2; exit 1; }

# A checkout: a tracked .buckconfig with a bundled prelude cell and a shim cell, a tracked
# shim/.buckconfig, and the tracked stock settings file.
co=$work/checkout
# The cases' HOME, inside the scratch directory, so no case reads this machine's files.
fake_home=$work/fake-home
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
        root=$co buck2=$work/buck2 STUB_DUMP=$dump HOME=$fake_home
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

# A file of a real submodule passes: HEAD records the submodule as a gitlink, and the file is
# at the submodule's HEAD.
sub_src=$work/sub-src
mkdir -p "$sub_src"
printf '[sub]\nkey = 1\n' >"$sub_src/.buckconfig"
git -C "$sub_src" init -q
git -C "$sub_src" add .buckconfig
git -C "$sub_src" -c user.name=t -c user.email=t@t commit -q -m sub
git -C "$co" -c protocol.file.allow=always submodule --quiet add "$sub_src" sub
git -C "$co" -c user.name=t -c user.email=t@t commit -q -m submodule
{ clean_dump; printf '[sub]\n    key = 1\n  (defined at sub/.buckconfig:2)\n'; } >"$work/subcell"
expect 0 "a file of a submodule HEAD records" "$work/subcell" true

# A submodule moved off the commit HEAD records, with its own HEAD holding the edited bytes:
# that HEAD is not this commit's, so it does not vouch.
recorded=$(git -C "$co" rev-parse HEAD:sub)
echo '# edited' >>"$co/sub/.buckconfig"
git -C "$co/sub" -c user.name=t -c user.email=t@t commit -q -a -m off-gitlink
[[ $(git -C "$co/sub" ls-tree HEAD -- .buckconfig | awk '{print $3}') == "$(git -C "$co/sub" hash-object -- .buckconfig)" ]] ||
    fail "the moved submodule's own HEAD does not hold the edited bytes"
expect 2 "a file of a submodule moved off its gitlink" "$work/subcell" true
grep -qF "not this commit's ($co/sub/.buckconfig; see" "$work/case.log" ||
    { cat "$work/case.log" >&2; fail "a file of a submodule moved off its gitlink: the refusal does not name exactly sub/.buckconfig"; }
git -C "$co/sub" checkout -q "$recorded"
expect 0 "the submodule back at its gitlink" "$work/subcell" true

# Each case below edits shim/.buckconfig and then makes git vouch for the edited bytes in a
# way an unguarded check would believe (asserted first, so the case cannot pass vacuously).
head_blob=$(git -C "$co" rev-parse HEAD:shim/.buckconfig)
# only_shim_refused NAME: the refusal named shim/.buckconfig and nothing else.
only_shim_refused() {
    grep -qF "not this commit's ($co/shim/.buckconfig; see" "$work/case.log" ||
        { cat "$work/case.log" >&2; fail "$1: the refusal does not name exactly shim/.buckconfig"; }
}
vouches() { [[ $(git -C "$co/shim" ls-tree HEAD -- .buckconfig | awk '{print $3}') == "$(git -C "$co/shim" hash-object -- .buckconfig)" ]]; }

# A clean filter that turns the edited bytes back into HEAD's: the bytes are hashed raw.
echo '# edited' >>"$co/shim/.buckconfig"
printf 'shim/.buckconfig filter=same\n' >"$co/.git/info/attributes"
git -C "$co" config filter.same.clean "cat '$work/shim.bak'"
[[ $(git -C "$co/shim" hash-object -- .buckconfig) == "$head_blob" ]] || fail "the clean filter setup does not make the edited file hash to HEAD's blob"
expect 2 "tracked file edited behind a clean filter" "$work/clean" true
only_shim_refused "tracked file edited behind a clean filter"
git -C "$co" config --unset filter.same.clean
rm -f -- "$co/.git/info/attributes"
cp "$work/shim.bak" "$co/shim/.buckconfig"

# A refs/replace entry that swaps HEAD for a commit holding the edited bytes: HEAD is read
# without replacements.
echo '# edited' >>"$co/shim/.buckconfig"
head_sha=$(git -C "$co" rev-parse HEAD)
edited_blob=$(git -C "$co" hash-object -w --no-filters -- shim/.buckconfig)
GIT_INDEX_FILE=$work/replace.index git -C "$co" read-tree HEAD
GIT_INDEX_FILE=$work/replace.index git -C "$co" update-index --cacheinfo "100644,$edited_blob,shim/.buckconfig"
replaced_tree=$(GIT_INDEX_FILE=$work/replace.index git -C "$co" write-tree)
replacement=$(git -C "$co" -c user.name=t -c user.email=t@t commit-tree "$replaced_tree" -m replacement)
git -C "$co" replace "$head_sha" "$replacement"
vouches || fail "the refs/replace setup does not make HEAD name the edited bytes"
expect 2 "a refs/replace entry for HEAD" "$work/clean" true
only_shim_refused "a refs/replace entry for HEAD"
git -C "$co" replace -d "$head_sha" >/dev/null
cp "$work/shim.bak" "$co/shim/.buckconfig"

# A nested repository that HEAD does not record as a submodule, whose own HEAD holds the
# edited bytes.
echo '# edited' >>"$co/shim/.buckconfig"
git -C "$co/shim" init -q
git -C "$co/shim" add .buckconfig
git -C "$co/shim" -c user.name=t -c user.email=t@t commit -q -m nested
vouches || fail "the nested repository setup does not hold the edited bytes at its HEAD"
expect 2 "a file of a nested repository that is not a submodule" "$work/clean" true
only_shim_refused "a file of a nested repository that is not a submodule"
rm -rf -- "$co/shim/.git"
cp "$work/shim.bak" "$co/shim/.buckconfig"

# A location line counted as one but not of the form the parser reads.
{ clean_dump; printf '[z]\n    w = 1\n  (defined at shim/.buckconfig, line 2)\n'; } >"$work/unparsed"
expect 2 "a location line the parser cannot read" "$work/unparsed" true

expect 2 "a prelude/ directory in the checkout" "$work/clean" mkdir "$co/prelude"
rmdir -- "$co/prelude"

sed "s|  (defined at $stock:2)|  (defined at .buckconfig:9)|" "$work/clean" >"$work/movedkey"
expect 2 "sqlite_dep_file_state not from the stock file" "$work/movedkey" true

sed 's/status = if_available/status = off/' "$work/clean" >"$work/offkey"
expect 2 "status not if_available" "$work/offkey" true

expect 2 "a home settings file" "$work/clean" sh -c "mkdir -p '$fake_home' && : >'$fake_home/.bucksettings.local.toml'"
rm -f -- "$fake_home/.bucksettings.local.toml"

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
mkdir -p "$fake_home/.buckconfig.d"
printf '[x]\ny = 1\n' >"$fake_home/.buckconfig.d/rollout"
(
    root=$co HOME=$fake_home
    # shellcheck source-path=SCRIPTDIR source=buckconfig-sources.sh
    source "$here/buckconfig-sources.sh"
    record_external_buckconfigs
) >"$work/external.tsv"
tab=$'\t'
grep -q "^$fake_home/.buckconfig.d/rollout$tab$(sha256sum <"$fake_home/.buckconfig.d/rollout" | cut -c1-64)$tab$(stat -c %s "$fake_home/.buckconfig.d/rollout")$" "$work/external.tsv" ||
    fail "record_external_buckconfigs: no sha256 line for a present file: $(cat "$work/external.tsv")"
grep -q "^$fake_home/.buckconfig.local${tab}absent$" "$work/external.tsv" ||
    fail "record_external_buckconfigs: no absent line for a missing file"
pass=$((pass + 1))

echo "test-buckconfig-sources.sh: $pass checks passed"
