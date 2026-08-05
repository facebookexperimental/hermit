#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Compile the canonical latest-Reverie checker with the installed Rust
# toolchain instead of relying on its developer-friendly rust-script shebang.
# GitHub's portable images provide rustc but intentionally do not install
# rust-script.  Keep the wrapper dependency-free so every CI DAG lane can run
# the same checker source on a pristine image.

set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

mode='run'
if [[ ${1:-} == --self-test ]]; then
    mode='test'
    shift
fi

mkdir -p target/ci
compile_dir=$(mktemp -d "$ROOT_DIR/target/ci/check-reverie-pin.XXXXXX")
checker="$compile_dir/checker"
trap 'rm -rf -- "$compile_dir"' EXIT
if [[ $mode == test ]]; then
    if (($# != 0)); then
        echo "usage: ci/run-reverie-pin-check.sh --self-test" >&2
        exit 2
    fi
    RUSTUP_TOOLCHAIN=stable rustc --edition=2021 --test \
        scripts/check-reverie-pin.rs -o "$checker"
else
    RUSTUP_TOOLCHAIN=stable rustc --edition=2021 \
        scripts/check-reverie-pin.rs -o "$checker"
fi

"$checker" "$@"
