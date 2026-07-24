#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$script_dir/../.." && pwd)
dependency_root=${BACKEND_BENCHMARK_DEPENDENCIES:-$repo_root/target/backend-benchmarks/deps}

ninja_tag=v1.13.1
ninja_commit=79feac0f3e3bc9da9effc586cd5fea41e7550051
leveldb_tag=1.23
leveldb_commit=99b3c03b3284f5886f9ef9a4ef703d57373e61be

for tool in cmake c++ git python3; do
  command -v "$tool" >/dev/null || {
    printf 'error: required command not found: %s\n' "$tool" >&2
    exit 2
  }
done

proxy=()
if command -v with-proxy >/dev/null; then
  proxy=(with-proxy)
fi

clone_pinned() {
  local url=$1
  local tag=$2
  local commit=$3
  local destination=$4

  if [[ ! -d $destination/.git ]]; then
    mkdir -p "$(dirname "$destination")"
    "${proxy[@]}" git clone --depth 1 --branch "$tag" "$url" "$destination"
  fi

  local actual
  actual=$(git -C "$destination" rev-parse HEAD)
  if [[ $actual != "$commit" ]]; then
    printf 'error: %s is at %s; expected %s\n' "$destination" "$actual" "$commit" >&2
    exit 2
  fi
}

ninja_source=$dependency_root/ninja
leveldb_source=$dependency_root/leveldb
leveldb_build=$dependency_root/leveldb-build-bench

clone_pinned \
  https://github.com/ninja-build/ninja.git \
  "$ninja_tag" "$ninja_commit" "$ninja_source"
clone_pinned \
  https://github.com/google/leveldb.git \
  "$leveldb_tag" "$leveldb_commit" "$leveldb_source"

if [[ ! -x $ninja_source/ninja ]]; then
  (
    cd "$ninja_source"
    python3 configure.py --bootstrap
  )
fi

"${proxy[@]}" git -C "$leveldb_source" submodule update --init --depth 1
cmake -S "$leveldb_source" -B "$leveldb_build" \
  -DCMAKE_BUILD_TYPE=Release \
  -DLEVELDB_BUILD_TESTS=ON \
  -DLEVELDB_BUILD_BENCHMARKS=ON \
  -DLEVELDB_INSTALL=OFF \
  -DBUILD_SHARED_LIBS=OFF \
  -DBENCHMARK_ENABLE_TESTING=OFF
cmake --build "$leveldb_build" --target db_bench -j "${BUILD_JOBS:-4}"

[[ -x $ninja_source/ninja ]] || {
  printf 'error: Ninja build did not produce %s\n' "$ninja_source/ninja" >&2
  exit 2
}
[[ -x $leveldb_build/db_bench ]] || {
  printf 'error: LevelDB build did not produce %s\n' "$leveldb_build/db_bench" >&2
  exit 2
}

printf 'Ninja:  %s (%s)\n' "$ninja_source/ninja" "$ninja_commit"
printf 'LevelDB: %s (%s)\n' "$leveldb_build/db_bench" "$leveldb_commit"
