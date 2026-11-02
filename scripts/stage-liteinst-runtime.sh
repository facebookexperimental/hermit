#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

if (( $# != 3 && $# != 4 )) || { (( $# == 4 )) && [[ $4 != --allocator-fixture ]]; }; then
    echo "Usage: $0 <cargo-profile> <stable-runtime-path> <runtime-target-root> [--allocator-fixture]" >&2
    exit 2
fi

root_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
liteinst_profile=$1
liteinst_stable_input=$2
# A caller that already knows the pin it will compare the staged marker
# against may supply it. The hermit-cli integration tests pass the pin embedded
# in the Hermit binary under test (HERMIT_REVERIE_PIN), which is exactly the
# value the loader will require of this marker. Fixed runtime qualification
# also checks the declared source contents and exact Cargo dependency identities,
# including in copied sources without Git metadata
# (https://github.com/rrnewton/hermit/issues/3419). Repository-wide pin uniformity
# remains enforced by ci/run-reverie-pin-check.sh in the independent preflight.
if [[ -n ${HERMIT_LITEINST_REVERIE_PIN:-} ]]; then
    if [[ ! $HERMIT_LITEINST_REVERIE_PIN =~ ^[0-9a-f]{40}$ ]]; then
        echo "HERMIT_LITEINST_REVERIE_PIN must be a 40-hex Reverie revision: $HERMIT_LITEINST_REVERIE_PIN" >&2
        exit 2
    fi
fi
# Derive the marker from the actual nested build inputs, never the containing
# worktree. Require both manifests and the lock to name the same full revision;
# Cargo and the content qualifier independently check the resolved package graph.
manifest_revisions=
for manifest in "$root_dir/liteinst-runtime-build/Cargo.toml" \
    "$root_dir/liteinst-runtime-build/runtime/Cargo.toml"; do
    revisions=$(
        {
            grep 'rrnewton/reverie' "$manifest" |
                grep -oE 'rev = "[0-9a-f]{40}"' | grep -oE '[0-9a-f]{40}' || true
        } | sort -u
    )
    if [[ ! $revisions =~ ^[0-9a-f]{40}$ ||
        ( -n $manifest_revisions && $revisions != "$manifest_revisions" ) ]]; then
        echo "liteinst-runtime-build manifests must name one identical full Reverie revision" >&2
        exit 2
    fi
    manifest_revisions=$revisions
done
lock_revisions=$(
    {
        grep -h '^source = "git+https://github.com/rrnewton/reverie' \
            "$root_dir/liteinst-runtime-build/Cargo.lock" |
            grep -oE '[0-9a-f]{40}' || true
    } | sort -u
)
if [[ ! $manifest_revisions =~ ^[0-9a-f]{40}$ || $lock_revisions != "$manifest_revisions" ]]; then
    echo "liteinst-runtime-build manifests and lock must name one identical full Reverie revision" >&2
    exit 2
fi
reverie_pin=$manifest_revisions
if [[ -n ${HERMIT_LITEINST_REVERIE_PIN:-} && $reverie_pin != "$HERMIT_LITEINST_REVERIE_PIN" ]]; then
    echo "HERMIT_LITEINST_REVERIE_PIN=$HERMIT_LITEINST_REVERIE_PIN, but liteinst-runtime-build builds Reverie at: $reverie_pin" >&2
    exit 2
fi
liteinst_target_dir=$(realpath -m -- "$3-${reverie_pin:0:8}")
liteinst_stage_dir=$(dirname -- "$liteinst_stable_input")
liteinst_stage_name=$(basename -- "$liteinst_stable_input")

if [[ -z $liteinst_stable_input || $liteinst_stage_name == . || $liteinst_stage_name == / ]]; then
    echo "Stable LiteInst runtime path must name a file: $liteinst_stable_input" >&2
    exit 2
fi

mkdir -p -- "$liteinst_stage_dir"
liteinst_stage_dir=$(realpath -e -- "$liteinst_stage_dir")
liteinst_stable_stage=$liteinst_stage_dir/$liteinst_stage_name
liteinst_stable_marker=${liteinst_stable_stage}.revision
liteinst_temp_dir=$(
    mktemp -d --tmpdir="$liteinst_stage_dir" ".${liteinst_stage_name}.stage.XXXXXX"
)
liteinst_temp_stage=$liteinst_temp_dir/runtime.so
liteinst_temp_marker=${liteinst_temp_dir}/runtime.so.revision
cleanup_liteinst_temp_stage() {
    if [[ -n ${liteinst_temp_stage:-} ]]; then
        rm -f -- "$liteinst_temp_stage"
    fi
    if [[ -n ${liteinst_temp_marker:-} ]]; then
        rm -f -- "$liteinst_temp_marker"
    fi
    if [[ -n ${liteinst_temp_dir:-} ]]; then
        rmdir -- "$liteinst_temp_dir"
    fi
}
trap cleanup_liteinst_temp_stage EXIT

liteinst_features=()
if [[ ${4:-} == --allocator-fixture ]]; then
    liteinst_features=(--features allocator-fixture)
fi
HERMIT_LITEINST_STAGE=$liteinst_temp_stage HERMIT_LITEINST_REVERIE_PIN=$reverie_pin "${CARGO:-cargo}" build \
    --locked \
    --manifest-path liteinst-runtime-build/Cargo.toml \
    --profile "$liteinst_profile" \
    --target-dir "$liteinst_target_dir" \
    "${liteinst_features[@]}"

if [[ ! -s $liteinst_temp_stage || ! -f $liteinst_temp_stage || -L $liteinst_temp_stage ]]; then
    echo "LiteInst runtime build did not stage a non-empty regular file: $liteinst_temp_stage" >&2
    exit 1
fi

# Record the same pin that selected the target directory. Hermit requires this
# sibling marker before it will load the runtime, so the standalone staging
# command must produce both halves of that contract.
printf '%s\n' "$reverie_pin" >"$liteinst_temp_marker"

# The unique destinations above force Cargo to rerun the staging build script.
# They are adjacent to the stable paths, so each rename is atomic. Move the DSO
# first: while the two renames are in flight, a new DSO with an old or absent
# marker is refused. Moving the marker first could make an old DSO look current.
mv -fT -- "$liteinst_temp_stage" "$liteinst_stable_stage"
liteinst_temp_stage=
mv -fT -- "$liteinst_temp_marker" "$liteinst_stable_marker"
liteinst_temp_marker=
rmdir -- "$liteinst_temp_dir"
liteinst_temp_dir=
