#!/usr/bin/env bash

set -euo pipefail

readonly LEVELDB_REPOSITORY="https://github.com/google/leveldb.git"
readonly LEVELDB_REVISION="7ee830d02b623e8ffe0b95d59a74db1e58da04c5"

if [[ $# -ne 2 ]]; then
  echo "usage: $0 SOURCE_DIR BUILD_DIR" >&2
  exit 2
fi

readonly source_dir=$1
readonly build_dir=$2

if [[ -e "$source_dir" || -e "$build_dir" ]]; then
  echo "source and build destinations must not already exist" >&2
  exit 2
fi

git clone --filter=blob:none --no-checkout "$LEVELDB_REPOSITORY" "$source_dir"
git -C "$source_dir" checkout --detach "$LEVELDB_REVISION"
git -C "$source_dir" submodule update --init --depth=1 third_party/googletest

cmake \
  -S "$source_dir" \
  -B "$build_dir" \
  -G "Unix Makefiles" \
  -DCMAKE_BUILD_TYPE=Release \
  -DLEVELDB_BUILD_TESTS=ON \
  -DLEVELDB_BUILD_BENCHMARKS=OFF

cmake --build "$build_dir" --parallel "${LEVELDB_BUILD_JOBS:-2}" --target \
  c_test env_posix_test leveldb_tests

echo "$build_dir"
