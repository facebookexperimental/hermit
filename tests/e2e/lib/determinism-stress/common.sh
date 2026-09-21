#!/usr/bin/env bash

set -euo pipefail

stress_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "$stress_dir/../../../.." && pwd)
hermit_bin=${HERMIT_BIN:-$repo_root/target/release/hermit}
VERIFICATION_REPORT_BIN=${VERIFICATION_REPORT_BIN:-$(dirname -- "$hermit_bin")/verification-report}
cc_bin=${CC:-cc}
verify_repetitions=${DETERMINISM_STRESS_REPETITIONS:-1}
native_repetitions=${NATIVE_STRESS_REPETITIONS:-3}
verify_timeout=${DETERMINISM_STRESS_TIMEOUT:-180}
verify_index=0

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

comparison_display=""
comparison_relaxations=""
comparison_evidence_complete=true
comparison_evidence_observations=0
comparison_evidence_self_test_scratch=""

reset_comparison_evidence() {
  comparison_display=""
  comparison_relaxations=""
  comparison_evidence_complete=true
  comparison_evidence_observations=0
}

record_comparison_evidence() {
  local stderr_file=$1
  local -a evidence_lines=()
  local evidence comparison relaxations
  mapfile -t evidence_lines < <(
    grep -E '^:: comparison=[^[:space:]]+ relaxations=[^[:space:]]+$' \
      "$stderr_file" || true
  )
  if ((${#evidence_lines[@]} != 1)); then
    comparison_evidence_complete=false
    printf 'warning: expected one comparison-evidence line, found %s; reporting unknown\n' \
      "${#evidence_lines[@]}" >&2
    return 0
  fi

  evidence=${evidence_lines[0]#:: comparison=}
  comparison=${evidence%% relaxations=*}
  relaxations=${evidence#* relaxations=}
  comparison_evidence_observations=$((comparison_evidence_observations + 1))
  if [[ -z $comparison_display ]]; then
    comparison_display=$comparison
    comparison_relaxations=$relaxations
  elif [[ $comparison_display != "$comparison" || $comparison_relaxations != "$relaxations" ]]; then
    comparison_evidence_complete=false
    printf 'warning: verification attempts reported different comparison evidence; reporting unknown\n' >&2
  fi
}

stress_success() {
  local display=unknown relaxations=unknown
  if [[ $comparison_evidence_complete == true &&
        $comparison_evidence_observations -gt 0 ]]; then
    display=$comparison_display
    relaxations=$comparison_relaxations
  fi
  printf '\nPASS: %s (ptrace backend, comparison=%s, log=info, relaxations=%s)\n' \
    "$1" "$display" "$relaxations"
}

comparison_evidence_self_test() {
  local output
  comparison_evidence_self_test_scratch=$(mktemp -d \
    "${TMPDIR:-/tmp}/determinism-stress-evidence.XXXXXX")
  trap 'rm -rf -- "$comparison_evidence_self_test_scratch"' EXIT

  printf '%s\n' \
    ':: comparison=Stripped relaxations=unsafe-numeric-address-and-path-normalization/v1' \
    >"$comparison_evidence_self_test_scratch/stripped.stderr"
  reset_comparison_evidence
  record_comparison_evidence "$comparison_evidence_self_test_scratch/stripped.stderr"
  output=$(stress_success stripped-control)
  [[ $output == *'comparison=Stripped'* ]] ||
    fail "Stripped comparison display was not preserved"
  [[ $output == *'relaxations=unsafe-numeric-address-and-path-normalization/v1'* ]] ||
    fail "Stripped comparison did not report its lossy relaxation"
  [[ $output != *'relaxations=none'* ]] ||
    fail "Stripped comparison falsely reported relaxations=none"

  printf '%s\n' ':: comparison=BitwiseInfoV1 relaxations=none' \
    >"$comparison_evidence_self_test_scratch/canonical.stderr"
  reset_comparison_evidence
  record_comparison_evidence "$comparison_evidence_self_test_scratch/canonical.stderr"
  output=$(stress_success canonical-control)
  [[ $output == *'comparison=BitwiseInfoV1'* ]] ||
    fail "canonical comparison display was not preserved"
  [[ $output == *'relaxations=none'* ]] ||
    fail "a comparison without relaxations must still report relaxations=none"

  : >"$comparison_evidence_self_test_scratch/missing.stderr"
  reset_comparison_evidence
  record_comparison_evidence "$comparison_evidence_self_test_scratch/missing.stderr"
  output=$(stress_success missing-control)
  [[ $output == *'comparison=unknown'* && $output == *'relaxations=unknown'* ]] ||
    fail "missing comparison evidence must not invent a policy or relaxation claim"

  printf '%s\n' \
    ':: comparison=BitwiseInfoV1 relaxations=none' \
    ':: comparison=BitwiseInfoV1 relaxations=none' \
    >"$comparison_evidence_self_test_scratch/duplicate.stderr"
  reset_comparison_evidence
  record_comparison_evidence "$comparison_evidence_self_test_scratch/duplicate.stderr"
  output=$(stress_success duplicate-control)
  [[ $output == *'comparison=unknown'* && $output == *'relaxations=unknown'* ]] ||
    fail "duplicate comparison evidence must not invent a policy or relaxation claim"

  reset_comparison_evidence
  record_comparison_evidence "$comparison_evidence_self_test_scratch/stripped.stderr"
  record_comparison_evidence "$comparison_evidence_self_test_scratch/canonical.stderr"
  output=$(stress_success inconsistent-control)
  [[ $output == *'comparison=unknown'* && $output == *'relaxations=unknown'* ]] ||
    fail "inconsistent comparison evidence must not invent a policy or relaxation claim"

  printf 'determinism-stress comparison evidence self-test: positive and refusal directions pass\n'
}

if [[ ${DETERMINISM_STRESS_EVIDENCE_SELF_TEST:-0} == 1 ]]; then
  comparison_evidence_self_test
  exit 0
fi

[[ -x $hermit_bin ]] || fail \
  "Hermit release binary not found: $hermit_bin (run: cargo build --release -p hermit --bin hermit)"
[[ -x $VERIFICATION_REPORT_BIN ]] || fail \
  "typed verification-report reader not found: $VERIFICATION_REPORT_BIN (run: cargo build -p hermit --bin verification-report)"
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
    local verify_report=$stress_workdir/verify-$verify_index.json

    printf '[hermit verify] %s (attempt %s/%s)\n' \
      "$label" "$attempt" "$verify_repetitions"
    if ! timeout --kill-after=10 "${verify_timeout}s" \
      "$hermit_bin" --log info run --strict --verify \
      --verify-json "$verify_report" -- "$@" \
      >"$stdout" 2>"$stderr"; then
      cat "$stdout" >&2
      tail -200 "$stderr" >&2
      printf 'error: %s failed under hermit run --strict --verify\n' "$label" >&2
      return 1
    fi
    if ! "$VERIFICATION_REPORT_BIN" matched "$verify_report"; then
      cat "$stdout" >&2
      tail -200 "$stderr" >&2
      printf 'error: %s typed verification report did not match\n' "$label" >&2
      return 1
    fi
    record_comparison_evidence "$stderr"
  done
}
