#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Refuse preparation if the early-published Hermit artifact is not the final
# validate-profile binary and resource tree. The E2E publisher snapshots the
# artifact while the workspace compile is still running; this gate runs in
# the prepare node after that compile and the LiteInst finalizer finish.
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
pointer="$ROOT_DIR/target/ci/hermit-e2e-artifact.path"
source_binary="$ROOT_DIR/target/validate/hermit"
source_install="$ROOT_DIR/target/install_pkg"

function fail {
    echo "verify-early-hermit-e2e-artifact.sh: $*" >&2
    exit 2
}

function tree_manifest {
    local root=$1 relative hash
    while IFS= read -r -d '' relative; do
        hash=$(sha256sum "$root/$relative" | cut -d' ' -f1)
        printf '%s  %s\n' "$hash" "$relative"
    done < <(cd "$root" && find -L . -type f -printf '%P\0' | LC_ALL=C sort -z)
}

[[ -f $pointer && -s $pointer ]] || fail "early artifact pointer is missing: $pointer"
bundle=$("$ROOT_DIR/ci/verify-hermit-e2e-artifact.sh" "$pointer")
[[ -x $source_binary ]] || fail "final Hermit is missing or non-executable: $source_binary"
cmp -s "$source_binary" "$bundle/hermit" ||
    fail "early-published Hermit differs from the final validate-profile binary"

current_manifest=$(mktemp)
trap 'rm -f "$current_manifest"' EXIT
tree_manifest "$source_install" >"$current_manifest"
cmp -s "$current_manifest" "$bundle/resources.sha256" ||
    fail "early-published resources differ from the final staged install tree"

printf 'verify-early-hermit-e2e-artifact: final binary and resources match %s\n' "$bundle"
