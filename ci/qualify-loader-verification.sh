#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved. Licensed under the BSD-style license in LICENSE.

# Opt-in physical package tests. Run under the existing validation resource
# owner; this script is not an ad-hoc Hermit launcher or a package producer.
set -euo pipefail
ROOT_DIR=$(realpath -e -- "${BASH_SOURCE[0]%/*}/..")

fail() { printf 'qualify-loader-verification: %s\n' "$*" >&2; exit 2; }
[[ $# == 2 && $1 == --standalone-build ]] ||
    fail "usage: $0 --standalone-build NEW_OUTPUT_DIRECTORY (requires qualified HERMIT_INSTALL_DIR)"
# These optional feature selections do not occur in the committed prepared
# profiles. Select the existing standalone counted path explicitly; never
# unset a caller's prepared requirement or manufacture a prepared stamp.
[[ ${HERMIT_PREPARED_NEXTEST_REQUIRED:-0} == 0 ]] ||
    fail "the prepared profile has no loader qualification selection; standalone mode cannot bypass its requirement"
[[ ${HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED:-0} == 1 ]] ||
    fail "prepare the canonical Rust-script executables, then set HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1"
[[ ! ${HERMIT_LOADER_QUALIFICATION_RECORD+x} && ! ${HERMIT_RETAINED_PACKAGE_TEST_CHILD+x} ]] ||
    fail "qualification record/child selectors must not be inherited"
[[ -n ${HERMIT_INSTALL_DIR:-} ]] || fail "HERMIT_INSTALL_DIR must name the actual qualified installation"
install=$(realpath -e -- "$HERMIT_INSTALL_DIR")
for member in detcore_private_runtime.elf detcore_private_runtime.package.json libdetcore_liteinst.so; do
    [[ -f $install/rsrcs/$member && -s $install/rsrcs/$member && ! -L $install/rsrcs/$member ]] ||
        fail "missing regular package member $install/rsrcs/$member"
done
# The producer's source and machine-code qualification remains a prerequisite;
# neither the descriptor nor these pathname checks confer source readiness.
export HERMIT_INSTALL_DIR=$install
export HERMIT_RETAINED_PACKAGE_TEST_INSTALL=$install
mkdir -- "$2"
output=$(realpath -e -- "$2")
export HERMIT_LOADER_QUALIFICATION_OUTPUT=$output
export CARGO_BUILD_JOBS=4
export CARGO_INCREMENTAL=0

run_one() {
    local label=$1 expected_id=$2 status=0
    shift 2
    # The official wrapper owns typed results, per-test CPU/wall bounds and
    # CPU reconciliation. Every selected test must really execute once.
    DAGRUN_TEST_COUNTS_PATH="$output/$label.counts.json" \
        HERMIT_NEXTEST_CPU_REPORT_PATH="$output/$label.cpu.json" \
        NEXTEST_EXPECTED_EXECUTED=1 \
        env -C "$ROOT_DIR" "$ROOT_DIR/ci/run-nextest-counted.sh" --locked "$@" \
        >"$output/$label.stdout" 2>"$output/$label.stderr" || status=$?
    printf '%s\n' "$status" >"$output/$label.status"
    ((status == 0)) || return "$status"
    python3 -I - "$output/$label.counts.json" "$expected_id" <<'PY'
import json
import sys
with open(sys.argv[1], encoding="utf-8") as stream:
    report = json.load(stream)
assert report["schema"] == 2, report
assert report["executed_tests"] == 1, report
assert report["results"] == [{"id": sys.argv[2], "result": "pass", "attempts": 1}], report
PY
}

HERMIT_LOADER_QUALIFICATION_RECORD="$output/inline-inputs.json" \
    run_one inline 'hermit-run::cli$loader_public_verify_replays_stdin_bytes_and_eof_at_bitwise_parity' \
    -p hermit-run --features exec-stop-launcher,liteinst --test cli \
    loader_public_verify_replays_stdin_bytes_and_eof_at_bitwise_parity \
    -j 1 --retries 0 -- --exact

run_one default 'hermit-run::cli$in_guest_trap_without_loader_opt_in_keeps_strict_stdin_verification' \
    -p hermit-run --features exec-stop-launcher,liteinst --test cli \
    in_guest_trap_without_loader_opt_in_keeps_strict_stdin_verification \
    -j 1 --retries 0 -- --exact

export HERMIT_LOADER_QUALIFICATION_RECORD="$output/inline-inputs.json"
program=$(python3 -I - "$HERMIT_LOADER_QUALIFICATION_RECORD" <<'PY'
import hashlib
import json
import pathlib
import sys
with open(sys.argv[1], encoding="utf-8") as stream:
    record = json.load(stream)
assert record["schema"] == 1
path = pathlib.Path(record["cat"]["path"])
assert path.is_absolute() and path.is_file()
assert "\n" not in str(path) and "\r" not in str(path)
assert hashlib.sha256(path.read_bytes()).hexdigest() == record["cat"]["sha256"]
print(path)
PY
)
export HERMIT_RETAINED_PACKAGE_TEST_PROGRAM=$program

run_one saved-pair 'hermit-manifest-plan$runner::tests::mapped_saved_pair_uses_actual_holder_and_matches_inline_verification' \
    -p hermit-manifest-plan --lib \
    runner::tests::mapped_saved_pair_uses_actual_holder_and_matches_inline_verification \
    -j 1 --retries 0 -- --exact --ignored

run_one retained 'hermit-run::hermit$run::tests::mapped_verification_retains_actual_package_after_parent_rename' \
    -p hermit-run --features exec-stop-launcher,liteinst --bin hermit \
    run::tests::mapped_verification_retains_actual_package_after_parent_rename \
    -j 1 --retries 0 -- --exact --ignored

printf '%s\n' 'loader qualification: four exact tests executed once and passed; source qualification remains separately bound'
