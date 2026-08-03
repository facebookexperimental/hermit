#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# check-script-sigpipe.sh — regression guard for shared SIGPIPE handling.
#
# Every standalone Hermit rust-script calls `rust_script_prelude::init` so that
# a downstream reader closing the pipe early (`prog | head`) terminates the
# producer cleanly instead of panicking or exiting 141 (which would fail any
# `set -o pipefail` pipeline). This guard compiles a tiny fixture that uses the
# real prelude with a plain `rustc` (no rust-script dependency in CI) and
# asserts the pipeline is clean.
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

fixture="scripts/lib/tests/sigpipe_smoke.rs"
[[ -f $fixture ]] || { echo "check-script-sigpipe.sh: missing $fixture" >&2; exit 2; }
command -v rustc >/dev/null 2>&1 || { echo "check-script-sigpipe.sh: rustc is required" >&2; exit 2; }

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
bin="$tmp/sigpipe_smoke"

RUSTUP_TOOLCHAIN="${RUSTUP_TOOLCHAIN:-stable}" rustc --edition=2021 -O "$fixture" -o "$bin"

# The producer writes 1,000,000 lines; `head -n1` closes the pipe after one.
# Under `pipefail` the pipeline status is the producer's unless it exits 0.
status=0
out="$(set -o pipefail; "$bin" 2>"$tmp/err" | head -n1)" || status=$?

if [[ $status -ne 0 ]]; then
    echo "check-script-sigpipe.sh: FAIL — 'sigpipe_smoke | head' exited $status (want 0)" >&2
    echo "  (a SIGPIPE from an early consumer must be a clean exit, not 141/panic)" >&2
    echo "--- stderr ---" >&2
    cat "$tmp/err" >&2 || true
    exit 1
fi

if grep -qiE 'panic|Broken pipe|backtrace' "$tmp/err"; then
    echo "check-script-sigpipe.sh: FAIL — producer emitted a panic/EPIPE error on stderr:" >&2
    cat "$tmp/err" >&2
    exit 1
fi

if [[ $out != "line 0" ]]; then
    echo "check-script-sigpipe.sh: FAIL — unexpected first line: '$out' (want 'line 0')" >&2
    exit 1
fi

echo "check-script-sigpipe.sh: OK — SIGPIPE from an early consumer exits cleanly (0)"
