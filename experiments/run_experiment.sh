#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: run_experiment.sh [OPTIONS] PROGRAM RUNS [ARG ...]

Run PROGRAM repeatedly under Hermit and compare stdout, stderr, and exit code.

Options:
  --hermit PATH      Hermit binary or command (default: target/debug/hermit, then PATH)
  --hermit-log LEVEL Hermit log threshold (default: error)
  --output DIR       Evidence directory (default: experiments/PROGRAM_TIMESTAMP)
  -h, --help         Show this help

The script exits 0 for DETERMINISTIC, 1 for NON-DETERMINISTIC, and 2 for a
usage or setup error. A consistent nonzero program exit is still deterministic.
USAGE
}

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 2
}

resolve_executable() {
  local candidate=$1
  if [[ $candidate == */* ]]; then
    [[ -x $candidate ]] || return 1
    printf '%s\n' "$candidate"
  else
    command -v -- "$candidate"
  fi
}

sha256_file() {
  sha256sum "$1" | awk '{print $1}'
}

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$script_dir/.." && pwd)
hermit_input=${HERMIT_BIN:-}
hermit_log=${HERMIT_LOG_LEVEL:-error}
output_dir=

while (($# > 0)); do
  case $1 in
    --hermit)
      (($# >= 2)) || fail '--hermit requires a value'
      hermit_input=$2
      shift 2
      ;;
    --hermit-log)
      (($# >= 2)) || fail '--hermit-log requires a value'
      hermit_log=$2
      shift 2
      ;;
    --output)
      (($# >= 2)) || fail '--output requires a value'
      output_dir=$2
      shift 2
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    --)
      shift
      break
      ;;
    -*)
      fail "unknown option: $1"
      ;;
    *)
      break
      ;;
  esac
done

(($# >= 2)) || {
  usage >&2
  exit 2
}

program_input=$1
runs=$2
shift 2
program_args=("$@")

[[ $runs =~ ^[1-9][0-9]*$ ]] || fail "RUNS must be a positive integer: $runs"
case $hermit_log in
  off | error | warn | info | debug | trace) ;;
  *) fail "invalid Hermit log threshold: $hermit_log" ;;
esac
command -v sha256sum >/dev/null || fail 'sha256sum is required'
command -v awk >/dev/null || fail 'awk is required'

if [[ -z $hermit_input ]]; then
  if [[ -x $repo_root/target/debug/hermit ]]; then
    hermit_input=$repo_root/target/debug/hermit
  else
    hermit_input=hermit
  fi
fi

hermit_bin=$(resolve_executable "$hermit_input") ||
  fail "Hermit executable not found: $hermit_input"
program=$(resolve_executable "$program_input") ||
  fail "program executable not found: $program_input"

if [[ -z $output_dir ]]; then
  program_name=$(basename "$program")
  program_slug=${program_name//[^[:alnum:]._-]/_}
  timestamp=$(date -u +%Y%m%dT%H%M%SZ)
  output_dir=$script_dir/${program_slug}_${timestamp}
fi

[[ ! -e $output_dir ]] || fail "evidence path already exists: $output_dir"
mkdir -p "$output_dir/runs"

export LC_ALL=C
started_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)
repository_commit=$(git -C "$repo_root" rev-parse HEAD 2>/dev/null || printf 'unknown')
hermit_hash=$(sha256_file "$hermit_bin")
if [[ -f $program ]]; then
  program_hash=$(sha256_file "$program")
else
  program_hash=unavailable
fi
printf -v command_line '%q ' "$program" "${program_args[@]}"
command_line=${command_line% }

{
  printf 'schema_version=1\n'
  printf 'started_at_utc=%s\n' "$started_at"
  printf 'repository_commit=%s\n' "$repository_commit"
  printf 'hermit=%s\n' "$hermit_bin"
  printf 'hermit_log=%s\n' "$hermit_log"
  printf 'hermit_sha256=%s\n' "$hermit_hash"
  printf 'program=%s\n' "$program"
  printf 'program_sha256=%s\n' "$program_hash"
  printf 'runs=%s\n' "$runs"
  printf 'command=%s\n' "$command_line"
} >"$output_dir/metadata.txt"

manifest=$output_dir/runs.tsv
printf 'run\texit_code\tstdout_sha256\tstderr_sha256\tfingerprint_sha256\n' >"$manifest"

declare -A seen_fingerprints=()
reference_fingerprint=
result=DETERMINISTIC
unique_fingerprints=0

for ((run = 1; run <= runs; run++)); do
  run_name=$(printf 'run-%04d' "$run")
  run_dir=$output_dir/runs/$run_name
  mkdir "$run_dir"

  set +e
  "$hermit_bin" --log "$hermit_log" run -- "$program" "${program_args[@]}" \
    >"$run_dir/stdout" 2>"$run_dir/stderr"
  exit_code=$?
  set -e

  stdout_hash=$(sha256_file "$run_dir/stdout")
  stderr_hash=$(sha256_file "$run_dir/stderr")
  fingerprint=$(
    printf 'exit_code=%s\nstdout_sha256=%s\nstderr_sha256=%s\n' \
      "$exit_code" "$stdout_hash" "$stderr_hash" |
      sha256sum |
      awk '{print $1}'
  )

  printf '%s  stdout\n' "$stdout_hash" >"$run_dir/stdout.sha256"
  printf '%s  stderr\n' "$stderr_hash" >"$run_dir/stderr.sha256"
  {
    printf 'exit_code=%s\n' "$exit_code"
    printf 'stdout_sha256=%s\n' "$stdout_hash"
    printf 'stderr_sha256=%s\n' "$stderr_hash"
    printf 'fingerprint_sha256=%s\n' "$fingerprint"
  } >"$run_dir/observation.txt"

  printf '%s\t%s\t%s\t%s\t%s\n' \
    "$run_name" "$exit_code" "$stdout_hash" "$stderr_hash" "$fingerprint" \
    >>"$manifest"

  if [[ -z ${seen_fingerprints[$fingerprint]+present} ]]; then
    seen_fingerprints[$fingerprint]=1
    ((unique_fingerprints += 1))
  fi
  if [[ -z $reference_fingerprint ]]; then
    reference_fingerprint=$fingerprint
  elif [[ $fingerprint != "$reference_fingerprint" ]]; then
    result=NON-DETERMINISTIC
  fi
done

{
  printf 'result=%s\n' "$result"
  printf 'runs=%s\n' "$runs"
  printf 'unique_fingerprints=%s\n' "$unique_fingerprints"
  printf 'reference_fingerprint=%s\n' "$reference_fingerprint"
  printf 'manifest=runs.tsv\n'
} >"$output_dir/summary.txt"

printf '%s\n' "$result"
printf 'Evidence: %s\n' "$output_dir"

if [[ $result == NON-DETERMINISTIC ]]; then
  exit 1
fi
