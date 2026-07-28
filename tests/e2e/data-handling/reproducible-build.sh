#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../../.." && pwd)
readonly SCRIPT_DIR REPO_ROOT

function write_crate {
    local directory=$1
    mkdir -p "$directory/src"
    cat >"$directory/Cargo.toml" <<'EOF'
[package]
name = "hermit-build-timestamp-fixture"
version = "0.1.0"
edition = "2018"

[dependencies]
build_timestamp = "=0.1.0"

[workspace]
EOF
    cat >"$directory/src/main.rs" <<'EOF'
use build_timestamp::build_time;

build_time!("%Y-%m-%dT%H:%M:%S");

fn main() {
    println!("{BUILD_TIME}");
}
EOF
}

if [[ ${1:-} == --guest ]]; then
    proc_macro=${2:?build_timestamp proc-macro path is required}
    mkdir -p "$REPO_ROOT/target/e2e-data-handling"
    work=$(mktemp -d "$REPO_ROOT/target/e2e-data-handling/build-crate.XXXXXX")
    trap 'rm -rf -- "$work"' EXIT
    write_crate "$work"

    # The cached proc-macro executes inside this rustc process. Consequently,
    # build_timestamp's clock read and the final compiler/linker run are all
    # controlled by Hermit, without involving Cargo's process supervisor.
    export RUSTC_BOOTSTRAP=1
    rustc -Zthreads=1 -Ccodegen-units=1 -Cdebuginfo=0 \
        --edition=2018 "$work/src/main.rs" \
        --extern "build_timestamp=$proc_macro" \
        -L "dependency=$(dirname -- "$proc_macro")" \
        -o "$work/hermit-build-timestamp-fixture"

    "$work/hermit-build-timestamp-fixture"
    sha256sum "$work/hermit-build-timestamp-fixture" |
        awk '{ print "binary=" $1 }'
    exit 0
fi

# shellcheck source=tests/e2e/data-handling/lib/common.bash
source "$(dirname -- "$0")/lib/common.bash"
require_tools cargo rustc cc sha256sum awk dirname

# Compile the dependency graph outside Hermit as a normal warm Cargo cache.
# Guest rustc then invokes the published build_timestamp proc-macro while
# compiling the final application; that invocation is the nondeterministic
# operation this test places under strict verification.
mkdir -p "$REPO_ROOT/target/e2e-data-handling"
fixture_dir=$(mktemp -d "${TMPDIR:-/tmp}/hermit-build-fetch.XXXXXX")
dependency_dir=$(mktemp -d \
    "$REPO_ROOT/target/e2e-data-handling/build-deps.XXXXXX")
trap 'rm -rf -- "$fixture_dir" "$dependency_dir"' EXIT
write_crate "$fixture_dir"
CARGO_TARGET_DIR=$dependency_dir \
    cargo build --quiet --manifest-path "$fixture_dir/Cargo.toml"

shopt -s nullglob
proc_macros=("$dependency_dir"/debug/deps/libbuild_timestamp-*.so)
if ((${#proc_macros[@]} != 1)); then
    echo "expected one build_timestamp proc macro, found ${#proc_macros[@]}" >&2
    exit 1
fi

export NATIVE_ATTEMPTS=3
export NATIVE_RETRY_DELAY=1.1
assert_nondeterminism_removed crates-io-build-timestamp-0.1.0 \
    "$(readlink -f "$0")" --guest "${proc_macros[0]}"
