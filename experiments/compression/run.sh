#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 2
}

require_positive_integer() {
  local name=$1
  local value=$2
  [[ $value =~ ^[1-9][0-9]*$ ]] || fail "$name must be a positive integer: $value"
}

sha256_file() {
  sha256sum "$1" | awk '{print $1}'
}

run_strict() {
  local label=$1
  local workdir=$2
  local stdout_file=$3
  local stderr_file=$4
  shift 4

  if ! "$hermit_bin" --log off run --strict \
    --base-env=minimal --env=LC_ALL=C --workdir="$workdir" -- \
    "$@" >"$stdout_file" 2>"$stderr_file"; then
    printf '%s failed under strict Hermit:\n' "$label" >&2
    cat "$stderr_file" >&2
    return 1
  fi
}

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$script_dir/../.." && pwd)
hermit_bin=${HERMIT_BIN:-$repo_root/target/release/hermit}
artifact_root=${COMPRESSION_ARTIFACT_ROOT:-$repo_root/target/compression}
runs=${COMPRESSION_RUNS:-3}
input_lines=${COMPRESSION_INPUT_LINES:-24000}

require_positive_integer COMPRESSION_RUNS "$runs"
require_positive_integer COMPRESSION_INPUT_LINES "$input_lines"
((runs == 3)) || fail "COMPRESSION_RUNS must be 3 for the determinism check"

for tool in awk basename bzip2 bzip2recover cargo cat cmp cp date git gzip mkdir seq sha256sum uname wc; do
  command -v "$tool" >/dev/null || fail "required command not found: $tool"
done

bzip2_bin=$(command -v bzip2)
bzip2recover_bin=$(command -v bzip2recover)
gzip_bin=$(command -v gzip)

if [[ ! -x $hermit_bin ]]; then
  cargo build --manifest-path "$repo_root/Cargo.toml" --release -p hermit --bin hermit
fi
[[ -x $hermit_bin ]] || fail "Hermit binary is not executable: $hermit_bin"

export LC_ALL=C
timestamp=$(date -u +%Y%m%dT%H%M%SZ)
evidence_dir=$artifact_root/evidence-$timestamp
[[ ! -e $evidence_dir ]] || fail "evidence path already exists: $evidence_dir"
mkdir -p "$evidence_dir/runs"

input_file=$evidence_dir/input.txt
awk -v lines="$input_lines" 'BEGIN {
  for (i = 0; i < lines; i++) {
    printf "%08d|the quick brown fox jumps over the deterministic dog|%08x|compress me reproducibly\n", i, i
  }
}' >"$input_file"

input_sha256=$(sha256_file "$input_file")
input_bytes=$(wc -c <"$input_file")
results=$evidence_dir/results.tsv
metadata=$evidence_dir/metadata.txt

{
  printf 'schema_version=1\n'
  printf 'started_at_utc=%s\n' "$timestamp"
  printf 'repository_commit=%s\n' "$(git -C "$repo_root" rev-parse HEAD)"
  printf 'host_kernel=%s\n' "$(uname -srmo)"
  printf 'runs=%s\n' "$runs"
  printf 'input_lines=%s\n' "$input_lines"
  printf 'input_bytes=%s\n' "$input_bytes"
  printf 'input_sha256=%s\n' "$input_sha256"
  printf 'hermit=%s\n' "$hermit_bin"
  printf 'hermit_sha256=%s\n' "$(sha256_file "$hermit_bin")"
  printf 'bzip2=%s\n' "$bzip2_bin"
  printf 'gzip=%s\n' "$gzip_bin"
  printf 'gzip_flags=-n -9 -c\n'
} >"$metadata"

printf 'run\tinput_sha256\tbzip2_sha256\tgzip_sha256\trecovery_manifest_sha256\trecovered_blocks\n' >"$results"

reference_bzip2_sha256=
reference_gzip_sha256=
reference_recovery_manifest_sha256=

shopt -s nullglob
for run in $(seq 1 "$runs"); do
  run_dir=$evidence_dir/runs/run-$run
  recover_dir=$run_dir/recover
  mkdir -p "$recover_dir"
  printf 'Running compression workload under strict Hermit (%s/%s)\n' "$run" "$runs"

  bzip2_output=$run_dir/output.bz2
  bzip2_decoded=$run_dir/bzip2-decoded.txt
  gzip_output=$run_dir/output.gz
  gzip_decoded=$run_dir/gzip-decoded.txt

  run_strict bzip2-compress "$run_dir" "$bzip2_output" "$run_dir/bzip2-compress.stderr" \
    "$bzip2_bin" -9 -c "$input_file"
  run_strict bzip2-decompress "$run_dir" "$bzip2_decoded" "$run_dir/bzip2-decompress.stderr" \
    "$bzip2_bin" -d -c "$bzip2_output"
  run_strict gzip-compress "$run_dir" "$gzip_output" "$run_dir/gzip-compress.stderr" \
    "$gzip_bin" -n -9 -c "$input_file"
  run_strict gzip-decompress "$run_dir" "$gzip_decoded" "$run_dir/gzip-decompress.stderr" \
    "$gzip_bin" -d -c "$gzip_output"

  cmp "$input_file" "$bzip2_decoded" >/dev/null || fail "bzip2 run $run changed the input"
  cmp "$input_file" "$gzip_decoded" >/dev/null || fail "gzip run $run changed the input"

  cp "$bzip2_output" "$recover_dir/archive.bz2"
  run_strict bzip2recover "$recover_dir" "$run_dir/bzip2recover.stdout" \
    "$run_dir/bzip2recover.stderr" "$bzip2recover_bin" archive.bz2

  recovered_files=("$recover_dir"/rec*archive.bz2)
  ((${#recovered_files[@]} >= 2)) ||
    fail "bzip2recover run $run produced fewer than two recovered blocks"
  "$bzip2_bin" -d -c "${recovered_files[@]}" >"$run_dir/recovered-input.txt"
  cmp "$input_file" "$run_dir/recovered-input.txt" >/dev/null ||
    fail "bzip2recover run $run did not reconstruct the input"

  recovery_manifest=$run_dir/recovery-manifest.sha256
  : >"$recovery_manifest"
  for recovered in "${recovered_files[@]}"; do
    printf '%s  %s\n' "$(sha256_file "$recovered")" "$(basename "$recovered")" \
      >>"$recovery_manifest"
  done

  bzip2_sha256=$(sha256_file "$bzip2_output")
  gzip_sha256=$(sha256_file "$gzip_output")
  recovery_manifest_sha256=$(sha256_file "$recovery_manifest")

  if [[ -z $reference_bzip2_sha256 ]]; then
    reference_bzip2_sha256=$bzip2_sha256
    reference_gzip_sha256=$gzip_sha256
    reference_recovery_manifest_sha256=$recovery_manifest_sha256
  else
    [[ $bzip2_sha256 == "$reference_bzip2_sha256" ]] ||
      fail "bzip2 SHA-256 changed on run $run"
    [[ $gzip_sha256 == "$reference_gzip_sha256" ]] ||
      fail "gzip SHA-256 changed on run $run"
    [[ $recovery_manifest_sha256 == "$reference_recovery_manifest_sha256" ]] ||
      fail "bzip2recover output SHA-256 changed on run $run"
  fi

  printf '%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$run" "$input_sha256" "$bzip2_sha256" "$gzip_sha256" \
    "$recovery_manifest_sha256" "${#recovered_files[@]}" >>"$results"
done

{
  printf 'classification=DETERMINISTIC\n'
  printf 'runs=%s\n' "$runs"
  printf 'input_sha256=%s\n' "$input_sha256"
  printf 'bzip2_sha256=%s\n' "$reference_bzip2_sha256"
  printf 'gzip_sha256=%s\n' "$reference_gzip_sha256"
  printf 'bzip2recover_manifest_sha256=%s\n' "$reference_recovery_manifest_sha256"
} >"$evidence_dir/summary.txt"

printf 'Compression determinism: %s/%s strict runs produced SHA-identical bzip2, gzip, and bzip2recover output.\n' \
  "$runs" "$runs"
printf 'Evidence: %s\n' "$evidence_dir"
