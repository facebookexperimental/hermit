#!/usr/bin/env bash

set -euo pipefail

redis_version=7.2.4
redis_sha256=0a62b9ae89b4be4e8d40c0035c83a72cb6776f4b62fe53553981a57f0f4ff73d
redis_url=https://github.com/redis/redis/archive/refs/tags/${redis_version}.tar.gz

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$script_dir/../.." && pwd)
artifact_root=${ARTIFACT_ROOT:-$repo_root/target/redis-strict}
archive=$artifact_root/downloads/redis-${redis_version}.tar.gz
source_root=$artifact_root/source/redis-${redis_version}
hermit_bin=${HERMIT_BIN:-$repo_root/target/debug/hermit}
jobs=${JOBS:-$(nproc)}

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 2
}

for command in cmp curl grep make nproc sha256sum tar timeout with-proxy; do
  command -v "$command" >/dev/null || fail "required command not found: $command"
done

if [[ ! -x $hermit_bin ]]; then
  cargo build -p hermit
fi

mkdir -p "$(dirname "$archive")" "$(dirname "$source_root")"
if [[ ! -f $archive ]]; then
  partial=$archive.partial.$$
  trap 'rm -f "$partial"' EXIT
  with-proxy curl --fail --location --retry 3 --output "$partial" "$redis_url"
  mv "$partial" "$archive"
  trap - EXIT
fi

printf '%s  %s\n' "$redis_sha256" "$archive" | sha256sum --check --status || \
  fail "Redis archive checksum mismatch: $archive"

if [[ ! -f $source_root/Makefile ]]; then
  tar -xzf "$archive" -C "$(dirname "$source_root")"
fi

make -C "$source_root" -j"$jobs" MALLOC=libc BUILD_TLS=no

redis_server=$source_root/src/redis-server
redis_cli=$source_root/src/redis-cli
[[ -x $redis_server && -x $redis_cli ]] || fail "Redis build did not produce server and CLI"

run_workload() {
  local run=$1
  local port=$((20000 + ((BASHPID * 17 + run * 131) % 30000)))
  local stdout=$artifact_root/strict-extended-$run.stdout
  local stderr=$artifact_root/strict-extended-$run.stderr

  if ! timeout 120 "$hermit_bin" --log off run --strict -- \
    /bin/sh "$script_dir/workload.sh" \
    "$redis_server" "$redis_cli" extended "source-$BASHPID-$run" "$port" \
    >"$stdout" 2>"$stderr"; then
    cat "$stdout" >&2
    cat "$stderr" >&2
    fail "strict Redis workload run $run failed"
  fi
}

run_workload 1
run_workload 2
cmp "$artifact_root/strict-extended-1.stdout" \
  "$artifact_root/strict-extended-2.stdout" || \
  fail "strict Redis output changed between runs"
cmp "$artifact_root/strict-extended-1.stderr" \
  "$artifact_root/strict-extended-2.stderr" || \
  fail "strict Redis diagnostics changed between runs"

memory_log=$artifact_root/strict-memory-test.log
if ! timeout 120 "$hermit_bin" --log off run --strict -- \
  "$redis_server" --test-memory 2 >"$memory_log" 2>&1; then
  tail -n 40 "$memory_log" >&2
  fail "Redis built-in memory test failed under strict Hermit"
fi
grep -a -q 'Your memory passed this test' "$memory_log" || \
  fail "Redis memory test did not report success"

cat "$artifact_root/strict-extended-1.stdout"
printf 'redis-source-build-strict-ok\n'

if [[ ${REDIS_RUN_UPSTREAM_PROBE:-0} == 1 ]]; then
  printf 'Running the diagnostic upstream Tcl probe (expected to time out).\n'
  (
    cd "$source_root"
    timeout 90 "$hermit_bin" --log off run --strict -- \
      ./runtest --single unit/printver --clients 1 --no-latency \
      --timeout 60 --verbose
  )
fi
