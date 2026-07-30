#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

if (( $# != 3 )); then
    echo "Usage: $0 <cargo-profile> <stable-runtime-path> <runtime-target-dir>" >&2
    exit 2
fi

liteinst_profile=$1
liteinst_stable_input=$2
liteinst_target_dir=$(realpath -m -- "$3")
liteinst_stage_dir=$(dirname -- "$liteinst_stable_input")
liteinst_stage_name=$(basename -- "$liteinst_stable_input")

if [[ -z $liteinst_stable_input || $liteinst_stage_name == . || $liteinst_stage_name == / ]]; then
    echo "Stable LiteInst runtime path must name a file: $liteinst_stable_input" >&2
    exit 2
fi

mkdir -p -- "$liteinst_stage_dir"
liteinst_stage_dir=$(realpath -e -- "$liteinst_stage_dir")
liteinst_stable_stage=$liteinst_stage_dir/$liteinst_stage_name
liteinst_temp_dir=$(
    mktemp -d --tmpdir="$liteinst_stage_dir" ".${liteinst_stage_name}.stage.XXXXXX"
)
liteinst_temp_stage=$liteinst_temp_dir/runtime.so
cleanup_liteinst_temp_stage() {
    if [[ -n ${liteinst_temp_stage:-} ]]; then
        rm -f -- "$liteinst_temp_stage"
    fi
    if [[ -n ${liteinst_temp_dir:-} ]]; then
        rmdir -- "$liteinst_temp_dir"
    fi
}
trap cleanup_liteinst_temp_stage EXIT

HERMIT_LITEINST_STAGE=$liteinst_temp_stage "${CARGO:-cargo}" build \
    --locked \
    --manifest-path liteinst-runtime-build/Cargo.toml \
    --profile "$liteinst_profile" \
    --target-dir "$liteinst_target_dir"

if [[ ! -s $liteinst_temp_stage || ! -f $liteinst_temp_stage || -L $liteinst_temp_stage ]]; then
    echo "LiteInst runtime build did not stage a non-empty regular file: $liteinst_temp_stage" >&2
    exit 1
fi

# The unique destination above forces Cargo to rerun the staging build script.
# It is adjacent to the stable path, so this rename is an atomic replacement.
mv -fT -- "$liteinst_temp_stage" "$liteinst_stable_stage"
liteinst_temp_stage=
rmdir -- "$liteinst_temp_dir"
liteinst_temp_dir=
