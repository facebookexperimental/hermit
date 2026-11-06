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

# Fabricated-root refusals: a missing pointer, an early binary that
# differs from the final one, and a bundle with no resource manifest
# must each refuse by name. Run by lint-check-early-artifact-gate.
function self_test {
    local work early_hash identity bundle output status
    work=$(mktemp -d)
    trap 'rm -rf "$work"' RETURN
    mkdir -p "$work/ci" "$work/target/ci" "$work/target/validate" \
        "$work/target/install_pkg"
    cp "$ROOT_DIR/ci/verify-early-hermit-e2e-artifact.sh" \
        "$ROOT_DIR/ci/verify-hermit-e2e-artifact.sh" "$work/ci/"

    function expect_refusal {
        local want=$1
        set +e
        output=$("$work/ci/verify-early-hermit-e2e-artifact.sh" 2>&1)
        status=$?
        set -e
        [[ $status == 2 ]] ||
            fail "self-test: expected exit 2, got $status: $output"
        [[ $output == *"$want"* ]] ||
            fail "self-test: expected refusal naming '$want', got: $output"
    }

    expect_refusal "early artifact pointer is missing"

    printf '#!/bin/sh\nexit 0\n' >"$work/early-hermit"
    chmod 755 "$work/early-hermit"
    early_hash=$(sha256sum "$work/early-hermit" | cut -d' ' -f1)
    identity=$(printf '%s\n%s\n%s\n%s\n' binary-only "$early_hash" none none |
        sha256sum | cut -d' ' -f1)
    bundle="$work/bundles/$identity"
    mkdir -p "$bundle"
    install -m 755 "$work/early-hermit" "$bundle/hermit"
    printf '%s\n' "$early_hash" >"$bundle/hermit.sha256"
    printf 'binary-only\n' >"$bundle/kind"
    printf '%s\n' "$bundle" >"$work/target/ci/hermit-e2e-artifact.path"

    printf '#!/bin/sh\nexit 9\n' >"$work/target/validate/hermit"
    chmod 755 "$work/target/validate/hermit"
    expect_refusal "early-published Hermit differs from the final validate-profile binary"

    install -m 755 "$work/early-hermit" "$work/target/validate/hermit"
    expect_refusal "early-published resources differ from the final staged install tree"

    printf 'verify-early-hermit-e2e-artifact self-test: all refusals fire\n'
}

if [[ ${1:-} == --self-test ]]; then
    self_test
    exit 0
fi

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
