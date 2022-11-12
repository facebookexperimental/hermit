#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -Eeuo pipefail
# This script is a "preflight" checklist: a series of tests that should run locally BEFORE publishing a diff.

# Switch to the Detcore root directory:
cd "$(dirname "$0")/../"

# For cargo-autocargo:
export PATH="$PATH:$HOME/fbsource/fbcode/common/rust/cargo_from_buck/bin/"
CARGO1="$HOME/fbsource/fbcode/third-party-buck/platform007/build/rust/bin/cargo"
CARGO2="$HOME/.cargo/bin/cargo"

function banner {
    echo
    echo "================================================================================"
    echo ">>> $*"
    echo "================================================================================"
}

function bail_out {
    echo
    echo "!!!!! Local validation failed or was interrupted !!!!";
    exit 1;
}
trap bail_out ERR

function build_n_test() {
    local mode=$1
    banner "Build and run tests through Buck..."
    buck build "$mode" ...
    buck test "$mode" ... -- --timeout 60
}

# Separate functions just to make the --tag look nice:
function opt_mode() {
    build_n_test @mode/opt
}

function dev-nosan_mode() {
    build_n_test @mode/dev-nosan
}

function cargo-fbcode() {
    banner "Test FB buck/autocargo setup..."
    $CARGO1 autocargo detcore
    $CARGO1 fbcode build detcore
    $CARGO1 fbcode doc detcore
}

function cargo-test() {
    banner "Test with vanilla Cargo..."
    $CARGO2 test
}

function arc-lint() {
    banner "Run linters..."
    arc lint
    # Disabling due to a problem with #[cfg]-activated dependencies:
    # arc lint-rust
}

# Unlike the analogous Reverie script, here we run these sequentially.
# This is due to additional problems encountered with parallel execution.
cargo-fbcode
opt_mode
dev-nosan_mode
arc-lint
cargo-test

banner "All done.  Local validation of Detcore passed."
