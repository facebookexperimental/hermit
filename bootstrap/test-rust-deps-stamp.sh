#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Tests bootstrap/rust-deps-stamp.py in a scratch Git repository: a written stamp
# checks clean; a changed Cargo.lock, an added Cargo.toml, a changed fixup, a
# changed Reindeer pin (REINDEER_REVISION in bootstrap/tool-pins.sh), a Reindeer
# patch added untracked and then tracked, a changed rust-toolchain.toml, a moved
# submodule pin, a tracked input deleted from the working tree and a missing stamp
# are each refused and named; a stamp written with a tracked input missing records
# it as missing; -h and --help print the help with exit 0; an unrelated
# file, a bootstrap test script, an ignored file under bootstrap/patches and an
# untracked file elsewhere in bootstrap/ are not inputs. Run by hand: bootstrap/test-rust-deps-stamp.sh
set -euo pipefail
stamp=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/rust-deps-stamp.py
repo=$(mktemp -d)
trap 'rm -rf -- "$repo"' EXIT
git -C "$repo" init -q
mkdir -p "$repo/shim/third-party/rust/fixups/foo" "$repo/bootstrap" "$repo/crate"
echo 'lock 1' >"$repo/Cargo.lock"
echo '[workspace]' >"$repo/Cargo.toml"
echo '[package]' >"$repo/crate/Cargo.toml"
echo 'vendor = true' >"$repo/shim/third-party/rust/reindeer.toml"
echo '[[buildscript]]' >"$repo/shim/third-party/rust/fixups/foo/fixups.toml"
echo 'generator' >"$repo/bootstrap/regenerate-rust-deps"
echo 'REINDEER_REVISION=aaaaaaaa' >"$repo/bootstrap/tool-pins.sh"
echo '# a test' >"$repo/bootstrap/test-x.sh"
echo '[toolchain]' >"$repo/rust-toolchain.toml"
echo 'readme' >"$repo/README.md"
git -C "$repo" add -A
git -C "$repo" update-index --add --cacheinfo 160000,1111111111111111111111111111111111111111,reverie
pass=0
expect() { # $1: 0 or 1, $2: what, $3: a phrase the refusal must name ("" for none)
    local status=0 out
    out=$("$stamp" check "$repo" 2>&1) || status=$?
    if [[ $status != "$1" ]] || [[ -n $3 && $out != *"$3"* ]]; then
        echo "FAIL: $2: exit $status, wanted $1 naming '$3': $out" >&2
        exit 1
    fi
    pass=$((pass + 1))
}
expect 1 "no stamp yet" "is missing"
"$stamp" write "$repo" >/dev/null
expect 0 "a fresh stamp" ""
grep -q '^gitlink 1111111111111111111111111111111111111111  reverie$' "$repo/shim/third-party/rust/BUCK.inputs"
echo 'readme 2' >"$repo/README.md"
expect 0 "an unrelated file changed" ""
echo 'lock 2' >"$repo/Cargo.lock"
expect 1 "Cargo.lock changed" "changed: Cargo.lock"
echo 'lock 1' >"$repo/Cargo.lock"
expect 0 "Cargo.lock restored" ""
mkdir -p "$repo/other" && echo '[package]' >"$repo/other/Cargo.toml" && git -C "$repo" add other/Cargo.toml
expect 1 "a Cargo.toml added" "added: other/Cargo.toml"
git -C "$repo" rm -q --cached other/Cargo.toml
echo 'changed' >>"$repo/shim/third-party/rust/fixups/foo/fixups.toml"
expect 1 "a fixup changed" "changed: shim/third-party/rust/fixups/foo/fixups.toml"
git -C "$repo" checkout -q -- shim/third-party/rust/fixups/foo/fixups.toml
rm -- "$repo/shim/third-party/rust/fixups/foo/fixups.toml"
expect 1 "a tracked input deleted from the working tree" "changed: shim/third-party/rust/fixups/foo/fixups.toml"
git -C "$repo" checkout -q -- shim/third-party/rust/fixups/foo/fixups.toml
echo 'REINDEER_REVISION=bbbbbbbb' >"$repo/bootstrap/tool-pins.sh"
expect 1 "the Reindeer pin changed" "changed: bootstrap/tool-pins.sh"
git -C "$repo" checkout -q -- bootstrap/tool-pins.sh
mkdir -p "$repo/bootstrap/patches/reindeer"
echo '*.orig' >"$repo/.git/info/exclude"
echo 'a leftover' >"$repo/bootstrap/patches/reindeer/0001-x.patch.orig"
expect 0 "an ignored file under bootstrap/patches" ""
echo 'notes' >"$repo/bootstrap/notes.txt"
expect 0 "an untracked file elsewhere in bootstrap/" ""
echo 'a patch' >"$repo/bootstrap/patches/reindeer/0001-x.patch"
expect 1 "an untracked Reindeer patch, which run-pinned-tool applies" "added: bootstrap/patches/reindeer/0001-x.patch"
git -C "$repo" add bootstrap/patches/reindeer/0001-x.patch
expect 1 "a Reindeer patch added" "added: bootstrap/patches/reindeer/0001-x.patch"
git -C "$repo" rm -q --cached bootstrap/patches/reindeer/0001-x.patch
rm -f -- "$repo/bootstrap/patches/reindeer/0001-x.patch"
expect 0 "the patch removed" ""
echo 'channel = "x"' >>"$repo/rust-toolchain.toml"
expect 1 "rust-toolchain.toml changed" "changed: rust-toolchain.toml"
git -C "$repo" checkout -q -- rust-toolchain.toml
echo '# changed' >>"$repo/bootstrap/test-x.sh"
expect 0 "a bootstrap test script changed" ""
git -C "$repo" checkout -q -- bootstrap/test-x.sh
git -C "$repo" update-index --cacheinfo 160000,2222222222222222222222222222222222222222,reverie
expect 1 "the Reverie pin moved" "changed: reverie"
git -C "$repo" update-index --cacheinfo 160000,1111111111111111111111111111111111111111,reverie
expect 0 "the pin restored" ""
rm -- "$repo/Cargo.lock"
"$stamp" write "$repo" >/dev/null
grep -q '^missing  Cargo.lock$' "$repo/shim/third-party/rust/BUCK.inputs" || { echo "FAIL: a stamp written with Cargo.lock missing does not record it as missing" >&2; exit 1; }
expect 0 "a stamp written with a tracked input missing" ""
git -C "$repo" checkout -q -- Cargo.lock
expect 1 "the missing input restored" "changed: Cargo.lock"
for flag in -h --help; do
    help=$("$stamp" "$flag") || { echo "FAIL: $flag exits $?" >&2; exit 1; }
    [[ $help == *"usage: rust-deps-stamp.py write REPO_ROOT"* ]] || { echo "FAIL: $flag prints no usage: $help" >&2; exit 1; }
    pass=$((pass + 1))
done
echo "test-rust-deps-stamp.sh: $pass checks passed"
