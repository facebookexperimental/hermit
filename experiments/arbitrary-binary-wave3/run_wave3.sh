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

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$script_dir/../.." && pwd)
hermit_bin=${HERMIT_BIN:-$repo_root/target/debug/hermit}
case_timeout=${CASE_TIMEOUT_SECONDS:-20}
runs=3
timestamp=$(date -u +%Y%m%dT%H%M%SZ)
artifact_root=${ARTIFACT_ROOT:-$script_dir/artifacts/$timestamp}
results=${RESULTS_FILE:-$script_dir/results.tsv}
summary=${SUMMARY_FILE:-$script_dir/summary.tsv}
metadata=${METADATA_FILE:-$script_dir/metadata.txt}

[[ -x $hermit_bin ]] || fail "Hermit binary is not executable: $hermit_bin"
[[ $case_timeout =~ ^[1-9][0-9]*$ ]] || \
  fail "CASE_TIMEOUT_SECONDS must be a positive integer"
[[ ! -e $artifact_root ]] || fail "artifact path already exists: $artifact_root"
for command in awk curl git nginx python3 redis-cli redis-server sha256sum sort timeout; do
  command -v "$command" >/dev/null || fail "required command not found: $command"
done

curl_bin=$(resolve_program curl)
git_bin=$(resolve_program git)
nginx_bin=$(resolve_program nginx)
redis_bin=$(resolve_program redis-server)
mkdir -p "$artifact_root"
export LC_ALL=C

{
  printf 'schema_version=1\n'
  printf 'started_at_utc=%s\n' "$timestamp"
  printf 'repository_commit=%s\n' "$(git -C "$repo_root" rev-parse HEAD)"
  printf 'repository_branch=%s\n' "$(git -C "$repo_root" branch --show-current)"
  printf 'hermit=%s\n' "$hermit_bin"
  printf 'hermit_sha256=%s\n' "$(sha256_file "$hermit_bin")"
  printf 'runs_per_program=%s\n' "$runs"
  printf 'case_timeout_seconds=%s\n' "$case_timeout"
  printf 'host_kernel=%s\n' "$(uname -srmo)"
  printf 'cpu_model=%s\n' "$(awk -F ': ' '/model name/{print $2; exit}' /proc/cpuinfo)"
  printf 'curl=%s\n' "$curl_bin"
  printf 'curl_sha256=%s\n' "$(sha256_file "$curl_bin")"
  printf 'curl_version=%s\n' "$(curl --version | head -n 1)"
  printf 'git=%s\n' "$git_bin"
  printf 'git_sha256=%s\n' "$(sha256_file "$git_bin")"
  printf 'git_version=%s\n' "$(git --version)"
  printf 'nginx=%s\n' "$nginx_bin"
  printf 'nginx_sha256=%s\n' "$(sha256_file "$nginx_bin")"
  printf 'nginx_version=%s\n' "$(nginx -v 2>&1)"
  printf 'redis_server=%s\n' "$redis_bin"
  printf 'redis_server_sha256=%s\n' "$(sha256_file "$redis_bin")"
  printf 'redis_server_version=%s\n' "$(redis-server --version)"
  printf 'artifact_root=%s\n' "$artifact_root"
} >"$metadata"

printf 'program\trun\toutcome\texit_code\tstdout_sha256\tstderr_sha256\tfingerprint_sha256\n' \
  >"$results"
printf 'program\toutcome\tdeterministic\tunique_fingerprints\texit_codes\tstdout_sha256\tstderr_sha256\n' \
  >"$summary"

run_case() {
  local name=$1
  local fixture=$2
  local case_dir=$artifact_root/$name
  local run run_name run_dir status outcome stdout_hash stderr_hash fingerprint
  local all_pass=yes all_timeout=yes deterministic=yes
  local reference_fingerprint='' reference_stdout='' reference_stderr=''
  local -a fingerprints=()
  local -a statuses=()

  mkdir -p "$case_dir"
  for ((run = 1; run <= runs; run++)); do
    run_name=$(printf 'run-%04d' "$run")
    run_dir=$case_dir/$run_name
    mkdir "$run_dir"

    set +e
    timeout --signal=KILL "${case_timeout}s" \
      "$hermit_bin" --log off run --strict -- /bin/sh "$fixture" \
      >"$run_dir/stdout" 2>"$run_dir/stderr"
    status=$?
    set -e

    case $status in
      0)
        outcome=pass
        all_timeout=no
        ;;
      137)
        outcome=timeout
        all_pass=no
        ;;
      *)
        outcome=fail
        all_pass=no
        all_timeout=no
        ;;
    esac

    stdout_hash=$(sha256_file "$run_dir/stdout")
    stderr_hash=$(sha256_file "$run_dir/stderr")
    fingerprint=$(
      printf 'exit_code=%s\nstdout_sha256=%s\nstderr_sha256=%s\n' \
        "$status" "$stdout_hash" "$stderr_hash" |
        sha256sum |
        awk '{print $1}'
    )
    fingerprints+=("$fingerprint")
    statuses+=("$status")
    if [[ -z $reference_fingerprint ]]; then
      reference_fingerprint=$fingerprint
      reference_stdout=$stdout_hash
      reference_stderr=$stderr_hash
    elif [[ $fingerprint != "$reference_fingerprint" ]]; then
      deterministic=no
    fi

    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
      "$name" "$run_name" "$outcome" "$status" \
      "$stdout_hash" "$stderr_hash" "$fingerprint" >>"$results"
  done

  local overall=fail
  if [[ $all_pass == yes ]]; then
    overall=pass
  elif [[ $all_timeout == yes ]]; then
    overall=timeout
  fi

  local unique_fingerprints exit_codes
  unique_fingerprints=$(printf '%s\n' "${fingerprints[@]}" | sort -u | wc -l)
  exit_codes=$(IFS=,; printf '%s' "${statuses[*]}")
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$name" "$overall" "$deterministic" "$unique_fingerprints" \
    "$exit_codes" "$reference_stdout" "$reference_stderr" >>"$summary"
  printf '%-14s outcome=%-7s deterministic=%s exit_codes=%s\n' \
    "$name" "$overall" "$deterministic" "$exit_codes"
}

run_case curl "$script_dir/fixtures/curl.sh"
run_case git "$script_dir/fixtures/git.sh"
run_case nginx "$script_dir/fixtures/nginx.sh"
run_case redis-server "$script_dir/fixtures/redis-server.sh"

printf 'Results: %s\nSummary: %s\nArtifacts: %s\n' \
  "$results" "$summary" "$artifact_root"
