#!/usr/bin/env bash

set -euo pipefail

stress_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "$stress_dir/../../.." && pwd)
hermit_bin=${HERMIT_BIN:-$repo_root/target/release/hermit}
cc_bin=${CC:-cc}
verify_repetitions=${DETERMINISM_STRESS_REPETITIONS:-1}
native_repetitions=${NATIVE_STRESS_REPETITIONS:-3}
verify_timeout=${DETERMINISM_STRESS_TIMEOUT:-180}
verify_index=0

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

[[ -x $hermit_bin ]] || fail \
  "Hermit release binary not found: $hermit_bin (run: cargo build --release -p hermit --bin hermit)"
command -v "$cc_bin" >/dev/null 2>&1 || fail "C compiler not found: $cc_bin"
command -v sha256sum >/dev/null 2>&1 || fail "sha256sum is required"
command -v timeout >/dev/null 2>&1 || fail "timeout is required"
[[ $verify_repetitions =~ ^[1-9][0-9]*$ ]] || fail \
  "DETERMINISM_STRESS_REPETITIONS must be a positive integer"
if [[ ! $native_repetitions =~ ^[1-9][0-9]*$ ]] || ((native_repetitions < 2)); then
  fail "NATIVE_STRESS_REPETITIONS must be an integer of at least 2"
fi
[[ $verify_timeout =~ ^[1-9][0-9]*$ ]] || fail \
  "DETERMINISM_STRESS_TIMEOUT must be a positive integer"

mkdir -p "$repo_root/target"
stress_workdir=$(mktemp -d "$repo_root/target/determinism-stress.XXXXXX")

cleanup_stress_workdir() {
  if [[ ${KEEP_DETERMINISM_STRESS_ARTIFACTS:-0} == 1 ]]; then
    printf 'artifacts retained: %s\n' "$stress_workdir"
  else
    rm -rf -- "$stress_workdir"
  fi
}
trap cleanup_stress_workdir EXIT

compile_c() {
  local source=$1
  local output_name=$2
  shift 2
  local output=$stress_workdir/$output_name

  if ! "$cc_bin" -std=gnu11 -O2 -g -Wall -Wextra -pthread \
    "$repo_root/$source" -o "$output" "$@"; then
    fail "failed to compile $source"
  fi
  printf '%s\n' "$output"
}

show_native_variation() {
  local label=$1
  shift
  local digest_file=$stress_workdir/native-digests.txt
  : >"$digest_file"

  printf '\n[native] %s (%s runs)\n' "$label" "$native_repetitions"
  for ((attempt = 1; attempt <= native_repetitions; attempt++)); do
    local output=$stress_workdir/native-$attempt.out
    if ! timeout --kill-after=5 "${verify_timeout}s" "$@" >"$output" 2>&1; then
      cat "$output" >&2
      fail "native $label run $attempt failed"
    fi
    sha256sum "$output" | cut -d' ' -f1 | tee -a "$digest_file"
  done

  local unique
  unique=$(sort -u "$digest_file" | wc -l)
  printf '[native] %s unique output digests: %s/%s\n' \
    "$label" "$unique" "$native_repetitions"
}

verify_guest() {
  local label=$1
  shift

  for ((attempt = 1; attempt <= verify_repetitions; attempt++)); do
    verify_index=$((verify_index + 1))
    local stdout=$stress_workdir/verify-$verify_index.stdout
    local stderr=$stress_workdir/verify-$verify_index.stderr

    printf '[hermit L2] %s (attempt %s/%s)\n' \
      "$label" "$attempt" "$verify_repetitions"
    if ! timeout --kill-after=10 "${verify_timeout}s" \
      "$hermit_bin" --log info run --strict --verify -- "$@" \
      >"$stdout" 2>"$stderr"; then
      cat "$stdout" >&2
      tail -200 "$stderr" >&2
      printf 'error: %s failed under hermit run --strict --verify\n' "$label" >&2
      return 1
    fi
    if ! grep -Fq 'Determinism verified' "$stdout" "$stderr"; then
      cat "$stdout" >&2
      tail -200 "$stderr" >&2
      printf 'error: %s exited without the determinism marker\n' "$label" >&2
      return 1
    fi
  done
}

stress_success() {
  printf '\nPASS: %s (ptrace backend, strict L2, log=info, relaxations=none)\n' "$1"
}
