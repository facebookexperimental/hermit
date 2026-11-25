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
# Name the marker pin the way the build itself identifies its Reverie
# sources, mirroring reverie_pin_at in hermit-cli/reverie_gitlink.rs: the
# submodule checkout's HEAD when one is initialized, else the Eden marker
# file, else the recorded gitlink, else the tree's release values.
# Manifests may spell Reverie by path (no revision to grep), and Cargo
# plus the content qualifier independently bind the resolved package
# graph either way; repository-wide pin uniformity stays with
# ci/run-reverie-pin-check.sh.
reverie_pin=
if [[ -e $root_dir/reverie/.git ]]; then
    reverie_pin=$(git -C "$root_dir/reverie" rev-parse HEAD 2>/dev/null || true)
fi
if [[ ! $reverie_pin =~ ^[0-9a-f]{40}$ && -f $root_dir/reverie.submodule.txt ]]; then
    marker_line=$(cat -- "$root_dir/reverie.submodule.txt")
    if [[ $marker_line =~ ^Subproject\ commit\ ([0-9a-f]{40})$ ]]; then
        reverie_pin=${BASH_REMATCH[1]}
    fi
fi
if [[ ! $reverie_pin =~ ^[0-9a-f]{40}$ ]]; then
    reverie_pin=$(git -C "$root_dir" rev-parse HEAD:reverie 2>/dev/null || true)
fi
if [[ ! $reverie_pin =~ ^[0-9a-f]{40}$ ]]; then
    release_pin=
    release_pin_source=
    release_pin_problem=
    for release_file in \
        "$root_dir/shim/hermetic_infra/hermit/fb-only/release.bzl" \
        "$root_dir/fb-only/release.bzl"; do
        [[ -f $release_file ]] || continue
        if [[ ! -r $release_file ]]; then
            [[ -n $release_pin_problem ]] || release_pin_problem="$release_file: release values are unreadable"
            continue
        fi
        release_file_count=0
        release_file_pin=
        while IFS= read -r release_line || [[ -n $release_line ]]; do
            release_code=${release_line%%#*}
            [[ $release_code == *HERMIT_REVERIE_PIN* ]] || continue
            if [[ $release_code =~ ^[[:space:]]*HERMIT_REVERIE_PIN[[:space:]]*=[[:space:]]*\"([0-9a-f]{40})\"[[:space:]]*$ ]]; then
                release_file_count=$((release_file_count + 1))
                release_file_pin=${BASH_REMATCH[1]}
            else
                [[ -n $release_pin_problem ]] || release_pin_problem="$release_file: active HERMIT_REVERIE_PIN is not one exact lowercase 40-hex assignment: $release_line"
            fi
        done <"$release_file"
        if ((release_file_count == 0)); then
            [[ -n $release_pin_problem ]] || release_pin_problem="$release_file: no active HERMIT_REVERIE_PIN assignment"
        elif ((release_file_count > 1)); then
            [[ -n $release_pin_problem ]] || release_pin_problem="$release_file: duplicate active HERMIT_REVERIE_PIN assignments ($release_file_count)"
        elif [[ -n $release_pin && $release_pin != "$release_file_pin" ]]; then
            [[ -n $release_pin_problem ]] || release_pin_problem="$release_file: HERMIT_REVERIE_PIN $release_file_pin disagrees with $release_pin_source ($release_pin)"
        else
            release_pin=$release_file_pin
            release_pin_source=$release_file
        fi
    done
    if [[ -z $release_pin_problem && $release_pin =~ ^[0-9a-f]{40}$ ]]; then
        reverie_pin=$release_pin
        echo "stage-liteinst-runtime: Reverie pin from release values: $release_pin_source" >&2
    elif [[ -n $release_pin_problem ]]; then
        echo "stage-liteinst-runtime: $release_pin_problem" >&2
    fi
fi
if [[ ! $reverie_pin =~ ^[0-9a-f]{40}$ ]]; then
    echo "cannot name the Reverie pin: no initialized reverie submodule checkout, reverie.submodule.txt marker, gitlink, or release values at $root_dir" >&2
    exit 2
fi
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
