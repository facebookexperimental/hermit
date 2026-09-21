#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Compile the canonical Reverie ancestor-and-monotonic checker with the installed Rust
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

# The Reverie checker above enforces uniformity for REVERIE ONLY. Hermit pins
# three git dependencies, and until this ran, two of them -- liteinst2 and
# rust-shed -- could have been split at two revisions with nothing detecting it.
# A split pin lets a mechanism be half present: the build succeeds, the tests
# pass, and the half that actually runs is the wrong one.
#
# Run in the SAME preflight node rather than as a new one, because
# scripts/validate.rs asserts the preflight node set by exact tag and a new node
# would change that contract for a check that shares this one's subject.
#
# Compiled the same dependency-free way, for the same reason: CI images provide
# rustc but not rust-script.
if [[ $mode == run ]]; then
    uniformity="$compile_dir/uniformity"
    RUSTUP_TOOLCHAIN=stable rustc --edition=2021 \
        scripts/check-git-pin-uniformity.rs -o "$uniformity"
    # STDOUT OF THIS SCRIPT IS MACHINE-READABLE: `--print-pin` callers capture it
    # with $(...) and compare the result to a 40-hex sha. The uniformity check
    # reports with `println!`, so running it on stdout appended eight lines of
    # scope commentary to that value and made every such comparison
    # unsatisfiable -- the pin was never the string being compared. Its findings
    # belong on stderr, where they are still shown and still gate via the exit
    # status, without corrupting a value another program parses.
    "$uniformity" >&2
fi
