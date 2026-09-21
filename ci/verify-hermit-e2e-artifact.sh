#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Verify and resolve one content-addressed Hermit E2E artifact.
set -euo pipefail

function fail {
    echo "verify-hermit-e2e-artifact.sh: $*" >&2
    exit 2
}

function tree_manifest {
    local root=$1 relative hash
    while IFS= read -r -d '' relative; do
        hash=$(sha256sum "$root/$relative" | cut -d' ' -f1)
        printf '%s  %s\n' "$hash" "$relative"
    done < <(cd "$root" && find -L . -type f -printf '%P\0' | LC_ALL=C sort -z)
}

function require_complete_resources {
    local install=$1 path
    [[ -d $install/rsrcs ]] || fail "resource bundle has no rsrcs directory: $install"
    for path in \
        libdetcore_dbt.so \
        libdetcore_sabre.so \
        libreverie_dbt_client.so \
        libreverie_liteinst.so; do
        [[ -f $install/rsrcs/$path && ! -L $install/rsrcs/$path && -s $install/rsrcs/$path ]] ||
            fail "resource bundle is missing or empty: $install/rsrcs/$path"
    done
    for path in dynamorio/bin64/drrun sabre e9patch e9tool; do
        [[ -f $install/rsrcs/$path && ! -L $install/rsrcs/$path && -s $install/rsrcs/$path && -x $install/rsrcs/$path ]] ||
            fail "resource bundle executable is missing, empty, or non-executable: $install/rsrcs/$path"
    done
}

[[ $# == 1 ]] || fail "usage: $0 BUNDLE-OR-POINTER"
input=$1
if [[ -d $input ]]; then
    bundle=$(cd "$input" && pwd -P)
else
    [[ -f $input && -s $input ]] || fail "artifact pointer is missing or empty: $input"
    IFS= read -r bundle <"$input"
    [[ -n $bundle && $bundle == /* ]] || fail "artifact pointer must contain one absolute path: $input"
    [[ $(wc -l <"$input") == 1 ]] || fail "artifact pointer must contain exactly one line: $input"
fi

[[ -d $bundle && ! -L $bundle ]] || fail "published artifact directory is missing or not a regular directory: $bundle"
[[ -f $bundle/kind && -s $bundle/kind ]] || fail "published artifact has no kind marker: $bundle"
kind=$(<"$bundle/kind")
[[ $kind == complete || $kind == binary-only ]] || fail "unknown artifact kind '$kind': $bundle"
[[ -f $bundle/hermit && ! -L $bundle/hermit && -s $bundle/hermit && -x $bundle/hermit ]] ||
    fail "published Hermit is missing, empty, or non-executable: $bundle/hermit"
[[ -f $bundle/hermit.sha256 && -s $bundle/hermit.sha256 ]] ||
    fail "published Hermit hash is missing: $bundle/hermit.sha256"
expected_binary_hash=$(<"$bundle/hermit.sha256")
actual_binary_hash=$(sha256sum "$bundle/hermit" | cut -d' ' -f1)
[[ $actual_binary_hash == "$expected_binary_hash" ]] ||
    fail "published Hermit hash mismatch: expected $expected_binary_hash, got $actual_binary_hash"

resource_hash=none
if [[ $kind == complete ]]; then
    require_complete_resources "$bundle/install"
    [[ -f $bundle/resources.sha256 ]] || fail "complete artifact has no resource manifest: $bundle"
    generated=$(mktemp)
    trap 'rm -f "$generated"' EXIT
    tree_manifest "$bundle/install" >"$generated"
    cmp -s "$bundle/resources.sha256" "$generated" || fail "published resource hash manifest does not match: $bundle"
    resource_hash=$(sha256sum "$bundle/resources.sha256" | cut -d' ' -f1)
elif [[ -e $bundle/install || -e $bundle/resources.sha256 ]]; then
    fail "binary-only artifact unexpectedly contains an unverified resource bundle: $bundle"
fi

identity=$(printf '%s\n%s\n%s\n' "$kind" "$actual_binary_hash" "$resource_hash" | sha256sum | cut -d' ' -f1)
[[ ${bundle##*/} == "$identity" ]] ||
    fail "content-addressed artifact identity mismatch: expected directory $identity, got ${bundle##*/}"
printf '%s\n' "$bundle"
