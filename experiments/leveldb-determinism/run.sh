#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# LevelDB concurrent-workload (non)determinism probe with a dual assertion:
#   1. NATIVE runs must DIFFER            -> nondeterminism exists (thread sched).
#   2. HERMIT runs must be IDENTICAL      -> determinism achieved.
#
# The workload (leveldb_concurrent.cc) runs N threads doing the same tiny,
# fixed sequence of LevelDB Put/Get operations against a shared DB, then prints
# a per-thread summary line in thread-completion order. The line CONTENT is
# fixed; only the ORDER depends on scheduling. LevelDB itself also runs
# background compaction threads, so this exercises a real storage engine.
#
# Everything downloaded/built lives under .build/ (git-ignored). LevelDB is
# fetched via `with-proxy` on Meta hosts; set NO_PROXY_FETCH=1 to use a bare
# curl instead.

set -uo pipefail
cd "$(dirname -- "${BASH_SOURCE[0]}")"

# ---- Configuration (env-overridable; defaults kept SMALL for CI) -------------
HERMIT=${HERMIT:-../../target/release/hermit}
NTHREADS=${NTHREADS:-8}
OPS=${OPS:-50}
NRUNS=${NRUNS:-6}
HERMIT_RUNS=${HERMIT_RUNS:-4}
HERMIT_TIMEOUT=${HERMIT_TIMEOUT:-180}
LEVELDB_VERSION=${LEVELDB_VERSION:-1.23}
LEVELDB_SHA256=${LEVELDB_SHA256:-9a37f8a6174f09bd622bc723b55881dc541cd50747cbd08831c2a82d620f6d76}

build_dir=.build
leveldb_src=$build_dir/leveldb-$LEVELDB_VERSION
leveldb_lib=$leveldb_src/build/libleveldb.a
program=$build_dir/leveldb_concurrent

fail() { printf 'error: %s\n' "$*" >&2; exit 2; }

fetch() {
  local url=$1 out=$2
  if [[ ${NO_PROXY_FETCH:-0} == 1 ]]; then
    curl -fsSL -o "$out" "$url"
  else
    with-proxy curl -fsSL -o "$out" "$url"
  fi
}

# ---- Build LevelDB + the workload (idempotent) -------------------------------
mkdir -p "$build_dir"
if [[ ! -f $leveldb_lib ]]; then
  echo ":: fetching + building LevelDB $LEVELDB_VERSION"
  tarball=$build_dir/leveldb-$LEVELDB_VERSION.tar.gz
  [[ -f $tarball ]] ||
    fetch "https://github.com/google/leveldb/archive/refs/tags/$LEVELDB_VERSION.tar.gz" "$tarball" ||
    fail "download failed (need network / with-proxy)"
  got=$(sha256sum "$tarball" | awk '{print $1}')
  [[ $got == "$LEVELDB_SHA256" ]] || fail "LevelDB checksum mismatch: $got"
  tar -C "$build_dir" -xzf "$tarball"
  cmake -S "$leveldb_src" -B "$leveldb_src/build" \
    -DCMAKE_BUILD_TYPE=Release -DLEVELDB_BUILD_TESTS=OFF \
    -DLEVELDB_BUILD_BENCHMARKS=OFF -DBUILD_SHARED_LIBS=OFF >/dev/null
  cmake --build "$leveldb_src/build" --target leveldb -j"$(nproc)" >/dev/null
fi
[[ -f $leveldb_lib ]] || fail "libleveldb.a missing after build"

if [[ ! -x $program || $program -ot leveldb_concurrent.cc ]]; then
  echo ":: compiling leveldb_concurrent"
  g++ -O2 -std=c++17 -I "$leveldb_src/include" \
    leveldb_concurrent.cc "$leveldb_lib" -lpthread -o "$program"
fi

# ---- Run harness ------------------------------------------------------------
# Each invocation gets a FRESH, empty database directory so the key set (and
# thus the deterministic total_keys line) is identical across runs. We hand
# LevelDB an already-created empty dir rather than a non-existent nested path:
# under `hermit run`, LevelDB's creation of a brand-new subdirectory currently
# fails (see README), whereas opening an existing empty dir works everywhere.
#
# Prints the md5 of stdout on success, or the literal token FAILED(rc=...) if
# the program errored or produced no output -- so that empty output can never be
# mistaken for "identical and therefore deterministic".
run_once() {           # run_once <launcher...>
  local dbdir out rc
  dbdir=$(mktemp -d "${TMPDIR:-/tmp}/ldbdet.XXXXXX")
  out=$(mktemp "${TMPDIR:-/tmp}/ldbout.XXXXXX")
  "$@" "$program" "$dbdir" "$NTHREADS" "$OPS" >"$out" 2>/dev/null
  rc=$?
  if [[ $rc -ne 0 ]] || ! grep -q '^total_keys=' "$out"; then
    printf 'FAILED(rc=%s)\n' "$rc"
  else
    md5sum <"$out" | cut -d' ' -f1
  fi
  rm -rf "$dbdir" "$out"
}

hermit_bin=$HERMIT
command -v -- "$hermit_bin" >/dev/null 2>&1 || [[ -x $hermit_bin ]] ||
  fail "hermit not found at '$hermit_bin' (build it or set HERMIT=...)"

echo "leveldb: $LEVELDB_VERSION   threads: $NTHREADS   ops/thread: $OPS"
echo "hermit:  $hermit_bin"
echo

echo "== native (expect DIFFERING hashes) =="
declare -A native_seen=()
native_ok=1
for _ in $(seq 1 "$NRUNS"); do
  h=$(run_once); echo "  $h"; native_seen[$h]=1
  [[ $h == FAILED* ]] && native_ok=0
done
native_distinct=${#native_seen[@]}
echo "distinct native outputs: $native_distinct / $NRUNS"
assert1="FAIL"
if [[ $native_ok -eq 1 && $native_distinct -ge 2 ]]; then
  assert1="PASS"; echo "ASSERT-1 PASS: native is nondeterministic"
elif [[ $native_ok -eq 0 ]]; then
  echo "ASSERT-1 FAIL: a native run failed to produce output"
else
  echo "ASSERT-1 FAIL: native output did not vary"
fi
echo

echo "== hermit (want IDENTICAL hashes; each run bounded to ${HERMIT_TIMEOUT}s) =="
declare -A hermit_seen=()
hermit_ok=1
for _ in $(seq 1 "$HERMIT_RUNS"); do
  h=$(run_once timeout "$HERMIT_TIMEOUT" "$hermit_bin" run --)
  echo "  $h"
  hermit_seen[$h]=1
  [[ $h == FAILED* ]] && hermit_ok=0
done
hermit_distinct=${#hermit_seen[@]}
echo "distinct hermit outputs: $hermit_distinct / $HERMIT_RUNS"
assert2="FAIL"
if [[ $hermit_ok -eq 1 && $hermit_distinct -eq 1 ]]; then
  assert2="PASS"; echo "ASSERT-2 PASS: hermit is deterministic"
elif [[ $hermit_ok -eq 0 ]]; then
  echo "ASSERT-2 FAIL: a hermit run failed to produce output"
else
  echo "ASSERT-2 FAIL: hermit outputs differ"
fi
echo

{
  echo "field,value"
  echo "leveldb_version,$LEVELDB_VERSION"
  echo "threads,$NTHREADS"
  echo "ops_per_thread,$OPS"
  echo "native_runs,$NRUNS"
  echo "native_distinct,$native_distinct"
  echo "hermit_runs,$HERMIT_RUNS"
  echo "hermit_distinct,$hermit_distinct"
  echo "assert1_native_nondeterministic,$assert1"
  echo "assert2_hermit_deterministic,$assert2"
} > results.csv
echo "wrote results.csv"

[[ $assert1 == PASS && $assert2 == PASS ]] || exit 1
echo "OVERALL: PASS (native nondeterministic, hermit deterministic)"
