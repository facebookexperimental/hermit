#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Compile the Tier 1 Cargo.lock freshness checker with the installed Rust
# toolchain instead of relying on its developer-friendly rust-script
# shebang, exactly like ci/run-reverie-pin-check.sh: preflight DAG nodes
# run before the prebuilt rust-scripts exist, and portable images
# provide rustc but intentionally do not install rust-script.

set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

mode='run'
if [[ ${1:-} == --self-test ]]; then
    mode='test'
    shift
fi

compile_root="${CARGO_TARGET_DIR:-$ROOT_DIR/target}/ci"
mkdir -p -- "$compile_root"
compile_dir=$(mktemp -d -- "$compile_root/check-cargo-lock.XXXXXX")
checker="$compile_dir/checker"
trap 'rm -rf -- "$compile_dir"' EXIT
if [[ $mode == test ]]; then
    if (($# != 0)); then
        echo "usage: ci/run-cargo-lock-check.sh --self-test" >&2
        exit 2
    fi
    RUSTUP_TOOLCHAIN=stable rustc --edition=2021 --test \
        scripts/check-cargo-lock-freshness.rs -o "$checker"
else
    RUSTUP_TOOLCHAIN=stable rustc --edition=2021 \
        scripts/check-cargo-lock-freshness.rs -o "$checker"
fi

"$checker" "$@"
