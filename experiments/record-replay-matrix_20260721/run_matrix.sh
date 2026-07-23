#!/usr/bin/env bash

set -euo pipefail

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 2
}

resolve_program() {
  type -P -- "$1" || return 1
}

sha256_file() {
  sha256sum "$1" | awk '{print $1}'
}

normalize_stderr() {
  python3 -c 'from pathlib import Path; import sys; data=Path(sys.argv[1]).read_bytes().split(b"\nRECORDING COMPLETE!", 1)[0]; Path(sys.argv[2]).write_bytes(b"".join(line for line in data.splitlines(keepends=True) if not line.startswith(b"timeout: ")))' "$1" "$2"
}

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$script_dir/../.." && pwd)
fixtures=$script_dir/fixtures
hermit_bin=${HERMIT_BIN:-$repo_root/target/debug/hermit}
timestamp=$(date -u +%Y%m%dT%H%M%SZ)
artifact_root=${ARTIFACT_ROOT:-$script_dir/artifacts/$timestamp}
results=${RESULTS_FILE:-$script_dir/results.tsv}
metadata=${METADATA_FILE:-$script_dir/metadata.txt}
build_dir=$repo_root/target/record-replay-matrix
case_timeout=${CASE_TIMEOUT_SECONDS:-60}

[[ -x $hermit_bin ]] || fail "Hermit binary is not executable: $hermit_bin"
[[ ! -e $artifact_root ]] || fail "artifact directory already exists: $artifact_root"
[[ $case_timeout =~ ^[1-9][0-9]*$ ]] || fail "CASE_TIMEOUT_SECONDS must be a positive integer"
for command in awk cc cmp find python3 sha256sum timeout; do
  command -v "$command" >/dev/null || fail "required command not found: $command"
done

mkdir -p "$artifact_root" "$build_dir"
cc -O2 -g -pthread "$fixtures/pthread_create.c" -o "$build_dir/pthread_create" ||
  fail 'failed to compile pthread_create fixture'
cc -O2 -g -pthread "$fixtures/producer_consumer.c" -o "$build_dir/producer_consumer" ||
  fail 'failed to compile producer_consumer fixture'

export LC_ALL=C
repository_commit=$(git -C "$repo_root" rev-parse HEAD 2>/dev/null || printf unknown)
{
  printf 'schema_version=1\n'
  printf 'started_at_utc=%s\n' "$timestamp"
  printf 'repository_commit=%s\n' "$repository_commit"
  printf 'repository_branch=%s\n' "$(git -C "$repo_root" branch --show-current 2>/dev/null || printf unknown)"
  printf 'hermit=%s\n' "$hermit_bin"
  printf 'hermit_sha256=%s\n' "$(sha256_file "$hermit_bin")"
  printf 'artifact_root=%s\n' "$artifact_root"
  printf 'case_timeout_seconds=%s\n' "$case_timeout"
  printf 'host_kernel=%s\n' "$(uname -srmo)"
  printf 'cpu_model=%s\n' "$(awk -F ': ' '/model name/{print $2; exit}' /proc/cpuinfo)"
} >"$metadata"

printf 'program\trecord_success\treplay_success\toutput_match\texit_match\trecord_exit\treplay_exit\tevent_streams\tevent_bytes\tstdout_sha256\tstderr_sha256\tcommand\n' >"$results"

total=0
compatible=0

run_case() {
  local name=$1
  local program=$2
  shift 2
  local args=("$@")
  local case_dir=$artifact_root/$name
  local data_dir=$case_dir/data
  local record_status replay_status id recording_dir
  local recording_available=no
  local record_success=fail replay_success=fail output_match=fail exit_match=fail
  local event_streams=0 event_bytes=0 stdout_hash=- stderr_hash=-
  local command_line

  mkdir -p "$case_dir" "$data_dir"
  printf -v command_line '%q ' "$program" "${args[@]}"
  command_line=${command_line% }
  printf '%s\n' "$command_line" >"$case_dir/command.txt"

  set +e
  timeout --signal=TERM "${case_timeout}s" env HERMIT_MODE=record \
    "$hermit_bin" --log off record start \
    --data-dir="$data_dir" -- "$program" "${args[@]}" \
    >"$case_dir/record.stdout" 2>"$case_dir/record.stderr"
  record_status=$?
  set -e
  printf '%s\n' "$record_status" >"$case_dir/record.status"

  id=
  if [[ -f $data_dir/last ]]; then
    id=$(tr -d '\n' <"$data_dir/last")
  fi
  recording_dir=$data_dir/$id
  if [[ -n $id && -f $recording_dir/metadata.json ]]; then
    recording_available=yes
  fi
  if [[ $record_status -eq 0 && $recording_available == yes ]]; then
    record_success=pass
  fi

  normalize_stderr "$case_dir/record.stderr" "$case_dir/record.guest.stderr"

  replay_status=not_run
  : >"$case_dir/replay.stdout"
  : >"$case_dir/replay.stderr"
  if [[ $recording_available == yes ]]; then
    set +e
    timeout --signal=TERM "${case_timeout}s" env HERMIT_MODE=replay \
      "$hermit_bin" --log off replay --autopilot \
      --data-dir="$data_dir" "$id" \
      >"$case_dir/replay.stdout" 2>"$case_dir/replay.stderr"
    replay_status=$?
    set -e
    normalize_stderr "$case_dir/replay.stderr" "$case_dir/replay.guest.stderr"
    [[ $replay_status -eq 0 ]] && replay_success=pass
    [[ $replay_status -eq $record_status ]] && exit_match=pass
    if cmp -s "$case_dir/record.stdout" "$case_dir/replay.stdout" &&
      cmp -s "$case_dir/record.guest.stderr" "$case_dir/replay.guest.stderr"; then
      output_match=pass
    fi
  fi
  printf '%s\n' "$replay_status" >"$case_dir/replay.status"

  if [[ -d $recording_dir/thread ]]; then
    event_streams=$(find "$recording_dir/thread" -maxdepth 1 -type f ! -name '*.debug' | wc -l)
    event_bytes=$(find "$recording_dir/thread" -maxdepth 1 -type f ! -name '*.debug' -printf '%s\n' |
      awk '{sum += $1} END {print sum + 0}')
  fi
  if [[ $output_match == pass ]]; then
    stdout_hash=$(sha256_file "$case_dir/record.stdout")
    stderr_hash=$(sha256_file "$case_dir/record.guest.stderr")
  fi

  ((total += 1))
  if [[ $record_success == pass && $replay_success == pass &&
        $output_match == pass && $exit_match == pass ]]; then
    ((compatible += 1))
  fi

  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$name" "$record_success" "$replay_success" "$output_match" "$exit_match" \
    "$record_status" "$replay_status" "$event_streams" "$event_bytes" \
    "$stdout_hash" "$stderr_hash" "$command_line" >>"$results"
  printf '%-20s record=%-4s replay=%-4s output=%-4s exit=%-4s streams=%s\n' \
    "$name" "$record_success" "$replay_success" "$output_match" "$exit_match" "$event_streams"
}

echo_bin=$(resolve_program echo) || fail 'echo not found'
ls_bin=$(resolve_program ls) || fail 'ls not found'
cat_bin=$(resolve_program cat) || fail 'cat not found'
grep_bin=$(resolve_program grep) || fail 'grep not found'
find_bin=$(resolve_program find) || fail 'find not found'
sort_bin=$(resolve_program sort) || fail 'sort not found'
wc_bin=$(resolve_program wc) || fail 'wc not found'
python_bin=$(resolve_program python3) || fail 'python3 not found'
gcc_bin=$(resolve_program gcc) || fail 'gcc not found'

run_case echo "$echo_bin" record-replay-matrix
run_case ls "$ls_bin" -1 "$fixtures/tree"
run_case cat "$cat_bin" "$fixtures/input.txt"
run_case grep "$grep_bin" beta "$fixtures/input.txt"
run_case find "$find_bin" "$fixtures/tree" -type f -print
run_case sort "$sort_bin" "$fixtures/unsorted.txt"
run_case wc "$wc_bin" -l "$fixtures/input.txt"
run_case python3 "$python_bin" -c 'print(sum(value * value for value in range(10)))'
run_case gcc "$gcc_bin" -c -O0 "$fixtures/compile_input.c" -o /dev/null
run_case pthread_create "$build_dir/pthread_create"
run_case producer_consumer "$build_dir/producer_consumer"

{
  printf 'total=%s\n' "$total"
  printf 'compatible=%s\n' "$compatible"
  printf 'incompatible=%s\n' "$((total - compatible))"
} >>"$metadata"

printf 'Matrix complete: %s/%s workloads recorded and replayed identically.\n' "$compatible" "$total"
printf 'Results: %s\nArtifacts: %s\n' "$results" "$artifact_root"
