#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Snapshot mutable Cargo outputs into one verified content-addressed artifact.
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
VERIFY="$ROOT_DIR/ci/verify-hermit-e2e-artifact.sh"

function fail {
    echo "publish-hermit-e2e-artifact.sh: $*" >&2
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
    for path in libdetcore_dbt.so libdetcore_sabre.so libreverie_dbt_client.so libreverie_liteinst.so; do
        [[ -f $install/rsrcs/$path && -s $install/rsrcs/$path ]] ||
            fail "resource bundle is missing or empty: $install/rsrcs/$path"
    done
    for path in dynamorio/bin64/drrun sabre e9patch e9tool; do
        [[ -f $install/rsrcs/$path && -s $install/rsrcs/$path && -x $install/rsrcs/$path ]] ||
            fail "resource bundle executable is missing, empty, or non-executable: $install/rsrcs/$path"
    done
}

[[ $# == 3 || $# == 4 ]] ||
    fail "usage: $0 SOURCE-BINARY BUNDLE-ROOT POINTER [SOURCE-INSTALL-DIR]"
source_binary=$1
bundle_root=$2
pointer=$3
source_install=${4:-}
kind=binary-only
[[ -z $source_install ]] || kind=complete

[[ -f $source_binary && ! -L $source_binary && -s $source_binary && -x $source_binary ]] ||
    fail "source Hermit is missing, empty, or non-executable: $source_binary"
mkdir -p "$bundle_root" "$(dirname "$pointer")"
bundle_root=$(cd "$bundle_root" && pwd -P)
pointer_dir=$(cd "$(dirname "$pointer")" && pwd -P)
pointer="$pointer_dir/$(basename "$pointer")"
stage="$bundle_root/.tmp-$$"
pointer_tmp="$pointer.tmp-$$"
before_manifest=$(mktemp)
after_manifest=$(mktemp)
function cleanup {
    rm -rf "$stage"
    rm -f "$pointer_tmp" "$before_manifest" "$after_manifest"
}
trap cleanup EXIT
[[ ! -e $stage ]] || fail "staging path already exists: $stage"
mkdir -p "$stage"

binary_hash_before=$(sha256sum "$source_binary" | cut -d' ' -f1)
install -m 755 "$source_binary" "$stage/hermit"
binary_hash_after=$(sha256sum "$source_binary" | cut -d' ' -f1)
published_binary_hash=$(sha256sum "$stage/hermit" | cut -d' ' -f1)
[[ -f $source_binary && ! -L $source_binary && -s $source_binary && -x $source_binary ]] ||
    fail "source Hermit changed type, size, or mode during publication: $source_binary"
[[ $binary_hash_before == "$binary_hash_after" && $binary_hash_before == "$published_binary_hash" ]] ||
    fail "source Hermit changed bytes during publication: before=$binary_hash_before after=$binary_hash_after copy=$published_binary_hash"
printf '%s\n' "$published_binary_hash" >"$stage/hermit.sha256"
printf '%s\n' "$kind" >"$stage/kind"

resource_hash=none
if [[ $kind == complete ]]; then
    require_complete_resources "$source_install"
    tree_manifest "$source_install" >"$before_manifest"
    [[ -s $before_manifest ]] || fail "source install bundle contains no regular files: $source_install"
    mkdir -p "$stage/install"
    cp -aL "$source_install/." "$stage/install/"
    tree_manifest "$source_install" >"$after_manifest"
    cmp -s "$before_manifest" "$after_manifest" || fail "source install bundle changed during publication: $source_install"
    tree_manifest "$stage/install" >"$stage/resources.sha256"
    cmp -s "$before_manifest" "$stage/resources.sha256" || fail "published resource bytes do not match source bundle: $source_install"
    require_complete_resources "$stage/install"
    [[ -z $(find "$stage/install" -type l -print -quit) ]] ||
        fail "published resource bundle retained a symlink instead of an immutable copy: $stage/install"
    resource_hash=$(sha256sum "$stage/resources.sha256" | cut -d' ' -f1)
fi

identity=$(printf '%s\n%s\n%s\n' "$kind" "$published_binary_hash" "$resource_hash" | sha256sum | cut -d' ' -f1)
published="$bundle_root/$identity"
if [[ -e $published ]]; then
    "$VERIFY" "$published" >/dev/null
    rm -rf "$stage"
else
    mv "$stage" "$published"
    "$VERIFY" "$published" >/dev/null
fi
printf '%s\n' "$published" >"$pointer_tmp"
mv -f "$pointer_tmp" "$pointer"
resolved=$("$VERIFY" "$pointer")
[[ $resolved == "$published" ]] || fail "published pointer resolved to $resolved, expected $published"
printf 'published Hermit E2E artifact kind=%s identity=%s path=%s\n' "$kind" "$identity" "$published"
