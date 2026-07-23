#!/usr/bin/env bash

set -euo pipefail

readonly SQLITE_VERSION=3.51.2
readonly SQLITE_ARCHIVE=sqlite-src-3510200.zip
readonly SQLITE_URL=https://www.sqlite.org/2026/sqlite-src-3510200.zip
readonly SQLITE_SHA256=85110f762d5079414d99dd5d7917bc3ff7e05876e6ccbd13d8496a3817f20829
readonly EXPECTED_TESTS=330900
readonly STALL_MARKER='Time: lock3.test '
readonly -a KNOWN_FAILURES=(
  backup2-6
  busy2-1.1.3
  busy2-2.1.3
  busy2-2.1.5
  delete-8.1
  delete-8.2
  delete-8.3
  delete-8.4
  delete-8.5
  delete-8.6
  extension01-1.6
  like-14.1
  like-14.2
)

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 2
}

sha256_file() {
  sha256sum "$1" | awk '{print $1}'
}

normalize_sqlite_stdout() {
  local input=$1
  local output=$2
  sed -E \
    -e '/^Time: .* -?[0-9]+ ms$/d' \
    -e 's/\([0-9]+ ms - want /(<elapsed> ms - want /g' "$input" >"$output"
}

run_strict_suite() {
  local run_dir=$1
  local process lines_before lines_after
  local stopped_at_known_stall=no

  timeout --signal=KILL "${timeout_seconds}s" \
    "$hermit_bin" --log off run --workdir="$run_dir" --strict -- \
    "$testfixture" "$test_script" --verbose=0 \
    >"$run_dir/stdout" 2>"$run_dir/stderr" &
  process=$!

  while kill -0 "$process" 2>/dev/null; do
    if grep -Fq "$STALL_MARKER" "$run_dir/stdout"; then
      lines_before=$(wc -l <"$run_dir/stdout")
      sleep "$stall_grace_seconds"
      lines_after=$(wc -l <"$run_dir/stdout")
      if kill -0 "$process" 2>/dev/null && [[ $lines_after -eq $lines_before ]]; then
        stopped_at_known_stall=yes
        kill -TERM "$process" 2>/dev/null || true
      fi
      break
    fi
    sleep 1
  done

  set +e
  wait "$process"
  suite_status=$?
  set -e
  if [[ $stopped_at_known_stall == yes ]]; then
    suite_outcome=known_lock4_stall
  else
    suite_outcome=completed
  fi
}

require_positive_integer() {
  local name=$1
  local value=$2
  [[ $value =~ ^[1-9][0-9]*$ ]] || fail "$name must be a positive integer"
}

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$script_dir/../.." && pwd)
hermit_bin=${HERMIT_BIN:-$repo_root/target/release/hermit}
case $hermit_bin in
  /*) ;;
  *) hermit_bin=$PWD/$hermit_bin ;;
esac
artifact_root=${SQLITE_VERYQUICK_ARTIFACT_ROOT:-$repo_root/target/sqlite-veryquick}
case $artifact_root in
  /*) ;;
  *) artifact_root=$PWD/$artifact_root ;;
esac
timeout_seconds=${SQLITE_VERYQUICK_TIMEOUT_SECONDS:-7200}
stall_grace_seconds=${SQLITE_VERYQUICK_STALL_GRACE_SECONDS:-30}
build_jobs=${SQLITE_VERYQUICK_BUILD_JOBS:-8}
archive=$artifact_root/downloads/$SQLITE_ARCHIVE
source_parent=$artifact_root/source
source_dir=$source_parent/sqlite-src-3510200
build_dir=$artifact_root/build
testfixture=$build_dir/testfixture
test_script=$source_dir/test/veryquick.test
root_patch=$script_dir/root-userns.patch
timestamp=$(date -u +%Y%m%dT%H%M%SZ)
evidence_dir=$artifact_root/evidence/$timestamp

require_positive_integer SQLITE_VERYQUICK_TIMEOUT_SECONDS "$timeout_seconds"
require_positive_integer SQLITE_VERYQUICK_STALL_GRACE_SECONDS "$stall_grace_seconds"
require_positive_integer SQLITE_VERYQUICK_BUILD_JOBS "$build_jobs"
[[ -x $hermit_bin ]] || fail "Hermit binary is not executable: $hermit_bin"

for command in awk cmp curl date find grep make patch sed sha256sum sleep sort timeout unzip wc; do
  command -v "$command" >/dev/null || fail "required command not found: $command"
done
cc=${CC:-cc}
command -v "$cc" >/dev/null || fail "C compiler not found: $cc"
command -v tclsh >/dev/null || fail 'tclsh not found; install the Tcl development package'

export LC_ALL=C
mkdir -p "$artifact_root/downloads" "$source_parent" "$build_dir"

if [[ ! -f $archive ]]; then
  printf 'Downloading SQLite %s from %s\n' "$SQLITE_VERSION" "$SQLITE_URL"
  curl --fail --location --retry 3 --output "$archive.part" "$SQLITE_URL"
  mv "$archive.part" "$archive"
fi

actual_archive_sha256=$(sha256_file "$archive")
[[ $actual_archive_sha256 == "$SQLITE_SHA256" ]] ||
  fail "source checksum mismatch: expected $SQLITE_SHA256, got $actual_archive_sha256"

if [[ ! -d $source_dir ]]; then
  if [[ -n $(find "$source_parent" -mindepth 1 -maxdepth 1 -print -quit) ]]; then
    fail "source directory is incomplete or unexpected: $source_parent"
  fi
  unzip -q "$archive" -d "$source_parent"
fi
[[ -x $source_dir/configure ]] || fail "SQLite configure script not found: $source_dir/configure"
[[ -f $test_script ]] || fail "SQLite veryquick suite not found: $test_script"

if ! grep -Fq 'root-mapped user namespace' "$source_dir/test/attach.test"; then
  patch --batch --forward -d "$source_dir" -p1 <"$root_patch" ||
    fail 'failed to apply the root-user-namespace compatibility patch'
fi
grep -Fq 'root-mapped user namespace' "$source_dir/test/attach.test" ||
  fail 'root-user-namespace compatibility patch is missing'
root_patch_sha256=$(sha256_file "$root_patch")

if [[ ! -f $build_dir/Makefile ]]; then
  printf 'Configuring SQLite %s\n' "$SQLITE_VERSION"
  (
    cd "$build_dir"
    "$source_dir/configure" --disable-shared --enable-static CC="$cc"
  ) >"$artifact_root/configure.log" 2>&1 ||
    fail "SQLite configure failed; see $artifact_root/configure.log"
fi

printf 'Building SQLite testfixture\n'
make -C "$build_dir" -j"$build_jobs" testfixture \
  >"$artifact_root/build.log" 2>&1 ||
  fail "testfixture build failed; see $artifact_root/build.log"
[[ -x $testfixture ]] || fail "testfixture was not built: $testfixture"

[[ ! -e $evidence_dir ]] || fail "evidence directory already exists: $evidence_dir"
mkdir -p "$evidence_dir"
results=$evidence_dir/results.tsv
metadata=$evidence_dir/metadata.txt

repository_commit=$(git -C "$repo_root" rev-parse HEAD 2>/dev/null || printf unknown)
repository_branch=$(git -C "$repo_root" branch --show-current 2>/dev/null || printf unknown)
hermit_sha256=$(sha256_file "$hermit_bin")
testfixture_sha256=$(sha256_file "$testfixture")

{
  printf 'schema_version=1\n'
  printf 'started_at_utc=%s\n' "$timestamp"
  printf 'repository_commit=%s\n' "$repository_commit"
  printf 'repository_branch=%s\n' "$repository_branch"
  printf 'host_kernel=%s\n' "$(uname -srmo)"
  printf 'sqlite_version=%s\n' "$SQLITE_VERSION"
  printf 'sqlite_url=%s\n' "$SQLITE_URL"
  printf 'sqlite_archive_sha256=%s\n' "$actual_archive_sha256"
  printf 'sqlite_expected_tests=%s\n' "$EXPECTED_TESTS"
  printf 'configure_flags=--disable-shared --enable-static\n'
  printf 'root_userns_patch=%s\n' "$root_patch"
  printf 'root_userns_patch_sha256=%s\n' "$root_patch_sha256"
  printf 'hermit=%s\n' "$hermit_bin"
  printf 'hermit_sha256=%s\n' "$hermit_sha256"
  printf 'testfixture=%s\n' "$testfixture"
  printf 'testfixture_sha256=%s\n' "$testfixture_sha256"
  printf 'timeout_seconds_per_run=%s\n' "$timeout_seconds"
  printf 'stall_grace_seconds=%s\n' "$stall_grace_seconds"
  printf 'stall_marker=%s\n' "$STALL_MARKER"
  printf 'known_pre_stall_failures=%s\n' "${#KNOWN_FAILURES[@]}"
  printf 'command=%q --log off run --workdir=RUN_DIR --strict -- %q %q --verbose=0\n' \
    "$hermit_bin" "$testfixture" "$test_script"
} >"$metadata"

printf 'run\toutcome\texit_status\traw_stdout_sha256\tsemantic_stdout_sha256\tstderr_sha256\tsuite_assertions\n' >"$results"

for run in 1 2; do
  run_dir=$evidence_dir/run-$run
  mkdir "$run_dir"
  printf 'Running SQLite veryquick under strict Hermit (%s/2)\n' "$run"
  run_strict_suite "$run_dir"
  status=$suite_status
  outcome=$suite_outcome

  printf '%s\n' "$status" >"$run_dir/exit-status"
  normalize_sqlite_stdout "$run_dir/stdout" "$run_dir/stdout.semantic"
  stdout_sha256=$(sha256_file "$run_dir/stdout")
  semantic_stdout_sha256=$(sha256_file "$run_dir/stdout.semantic")
  stderr_sha256=$(sha256_file "$run_dir/stderr")
  printf '%s  stdout\n' "$stdout_sha256" >"$run_dir/stdout.sha256"
  printf '%s  stdout.semantic\n' "$semantic_stdout_sha256" >"$run_dir/stdout.semantic.sha256"
  printf '%s  stderr\n' "$stderr_sha256" >"$run_dir/stderr.sha256"
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$run" "$outcome" "$status" "$stdout_sha256" "$semantic_stdout_sha256" \
    "$stderr_sha256" "$EXPECTED_TESTS" >>"$results"

  [[ $outcome == known_lock4_stall ]] ||
    fail "run $run did not reproduce the known lock4 stall (outcome: $outcome)"
  [[ $status -eq 143 ]] || fail "run $run stopped with unexpected status $status"
  printf '%s\n' "${KNOWN_FAILURES[@]}" | sort >"$run_dir/expected-failures"
  sed -n 's/^! \([^ ]*\) \(expected\|got\):.*/\1/p' "$run_dir/stdout" | \
    sort -u >"$run_dir/observed-failures"
  cmp "$run_dir/expected-failures" "$run_dir/observed-failures" >/dev/null ||
    fail "run $run produced an unexpected pre-stall failure set"
  [[ ! -s $run_dir/stderr ]] || fail "run $run produced unexpected stderr"
done

if cmp "$evidence_dir/run-1/stdout" "$evidence_dir/run-2/stdout" >/dev/null; then
  raw_stdout_match=yes
else
  raw_stdout_match=no
fi
cmp "$evidence_dir/run-1/stdout.semantic" "$evidence_dir/run-2/stdout.semantic" >/dev/null ||
  fail 'strict runs produced different semantic stdout'
cmp "$evidence_dir/run-1/stderr" "$evidence_dir/run-2/stderr" >/dev/null ||
  fail 'strict runs produced different stderr'

{
  printf 'classification=REPRODUCIBLE_KNOWN_LIMITATION\n'
  printf 'runs=2\n'
  printf 'outcome=known_lock4_stall\n'
  printf 'suite_assertions=%s\n' "$EXPECTED_TESTS"
  printf 'known_pre_stall_failures=%s\n' "${#KNOWN_FAILURES[@]}"
  printf 'raw_stdout_match=%s\n' "$raw_stdout_match"
  printf 'semantic_stdout_sha256=%s\n' "$(sha256_file "$evidence_dir/run-1/stdout.semantic")"
  printf 'stderr_sha256=%s\n' "$(sha256_file "$evidence_dir/run-1/stderr")"
} >"$evidence_dir/summary.txt"

printf 'SQLite %s veryquick: reproduced lock4 stall with %s identical pre-stall failures.\n' \
  "$SQLITE_VERSION" "${#KNOWN_FAILURES[@]}"
printf 'Evidence: %s\n' "$evidence_dir"
