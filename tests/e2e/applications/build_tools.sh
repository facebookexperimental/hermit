#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail
export LC_ALL=C
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
export TZ=UTC

function run_build_workload {
    local work_dir=$1
    local stamp artifact_hash artifact

    rm -rf -- "$work_dir"
    mkdir -p -- "$work_dir"
    stamp="$(date +%s%N)-$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')"

    cat >"$work_dir/Generate.cmake" <<'EOF'
file(WRITE "${OUTPUT}" "${STAMP}\n")
EOF
    cat >"$work_dir/Makefile" <<'EOF'
.PHONY: all
all:
	mkdir -p build
	cmake -DOUTPUT=build/application.txt -DSTAMP="$(BUILD_STAMP)" -P Generate.cmake
EOF

    BUILD_STAMP=$stamp make --silent -C "$work_dir" >"$work_dir/build.log"
    artifact="$work_dir/build/application.txt"
    [[ $(<"$artifact") == "$stamp" ]]
    artifact_hash=$(sha256sum "$artifact" | cut -d' ' -f1)
    printf 'build-tools:%s:%s\n' "$stamp" "$artifact_hash"
}

if [[ ${1:-} == --guest ]]; then
    run_build_workload "$2"
    exit
fi

# shellcheck source=tests/e2e/applications/common.sh
source "$(dirname -- "$0")/common.sh"
require_commands cmake date make od sha256sum timeout tr

work_root=$(mktemp -d "${TMPDIR:-/tmp}/hermit-build-e2e.XXXXXX")
trap 'rm -rf -- "$work_root"' EXIT

native_first=$(run_build_workload "$work_root/native")
native_second=$(run_build_workload "$work_root/native")
assert_native_nondeterminism 'make/CMake workload' "$native_first" "$native_second"

run_hermit_verify 'make/CMake workload' \
    /bin/bash "$0" --guest "$work_root/verified" >/dev/null
printf 'build-tools:verified\n'
