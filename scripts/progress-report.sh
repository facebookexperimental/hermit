#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# Generate a Hermit progress report with LIVE test numbers.
#
# Runs the strict/fail-closed ratchet, the working-envelope vector (L1-L4+rr),
# the record_replay suite, and the per-app e2e suites, then writes a dated
# report to docs/progress-reports/vN-YYYY-MM-DD.md. Every number in the report
# is measured, never estimated. Suites that cannot run are recorded with the
# exact reason. See .llms/skills/progress-rubric/SKILL.md for the rubric.
#
# Usage:
#   scripts/progress-report.sh                 # version defaults to v3
#   REPORT_VERSION=v4 scripts/progress-report.sh
#   NO_PULL=1 scripts/progress-report.sh       # skip the git pull step
#
# Idempotent: re-running overwrites today's report and the /tmp logs.

set -uo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR" || exit 1

# shellcheck source=scripts/lib/test_results.sh
source "$ROOT_DIR/scripts/lib/test_results.sh"

REPORT_VERSION=${REPORT_VERSION:-v3}
DATE_UTC=$(date -u +%Y-%m-%d)
REPORT_DIR="$ROOT_DIR/docs/progress-reports"
REPORT="$REPORT_DIR/${REPORT_VERSION}-${DATE_UTC}.md"
mkdir -p "$REPORT_DIR"

STRICT_LOG=/tmp/progress-strict.log
STRICT_RESULTS=/tmp/progress-strict-results.json
ENVELOPE_LOG=/tmp/progress-envelope.log
RECORD_LOG=/tmp/progress-record.log
RECORD_RESULTS=/tmp/progress-record-results.json
APPS_LOG=/tmp/progress-apps.log
APPS_RESULTS_DIR=/tmp/progress-app-results

# with-proxy wrapper: use it when present (Meta devserver), else run bare.
proxy() {
  if command -v with-proxy >/dev/null 2>&1; then
    with-proxy "$@"
  else
    "$@"
  fi
}

test_result_counts() { # <result-file> <command-status>
  local result_file=$1 command_status=$2
  if ! load_test_results "$result_file"; then
    printf 'unknown (typed test result unavailable; command exit %s)' "$command_status"
    return
  elif ((command_status != 0 && TEST_RESULTS_FAILED > 0)); then
    printf 'ABORTED at exit %s after %s passed; first failure: %s' \
      "$command_status" "$TEST_RESULTS_PASSED" "$TEST_RESULTS_FIRST_FAILURE"
  elif ((command_status != 0)); then
    printf 'ABORTED at exit %s after %s passed; no typed individual-test failure was recorded' \
      "$command_status" "$TEST_RESULTS_PASSED"
  elif ((TEST_RESULTS_FAILED > 0)); then
    printf 'UNKNOWN (exit 0 disagrees with %s typed failure(s))' "$TEST_RESULTS_FAILED"
  else
    printf '%s passed, %s failed, %s filtered' \
      "$TEST_RESULTS_PASSED" "$TEST_RESULTS_FAILED" "$TEST_RESULTS_FILTERED"
  fi
}

strict_result() { # <result-file> <command-status>
  local result_file=$1 command_status=$2
  if ! load_test_results "$result_file"; then
    printf 'UNKNOWN (typed test result unavailable; command exit %s)' "$command_status"
  elif ((command_status == 0 && TEST_RESULTS_FAILED == 0)); then
    printf 'passed (%s enabled, %s filtered)' \
      "$TEST_RESULTS_PASSED" "$TEST_RESULTS_FILTERED"
  elif ((command_status == 0)); then
    printf 'UNKNOWN (exit 0 disagrees with %s typed failure(s))' "$TEST_RESULTS_FAILED"
  elif ((TEST_RESULTS_FAILED > 0)); then
    printf 'ABORTED at exit %s after %s passed; first failure: %s' \
      "$command_status" "$TEST_RESULTS_PASSED" "$TEST_RESULTS_FIRST_FAILURE"
  else
    printf 'ABORTED at exit %s after %s passed; no typed individual-test failure was recorded' \
      "$command_status" "$TEST_RESULTS_PASSED"
  fi
}

self_test() {
  local scratch fixed_log_hash first second missing_status=0
  scratch=$(mktemp -d)
  trap 'rm -rf -- "$scratch"' RETURN
  printf 'running 999 tests\ntest result: ok. 999 passed; 0 failed; 0 ignored\n' \
    >"$scratch/fixed-human.log"
  fixed_log_hash=$(sha256sum "$scratch/fixed-human.log")

  # shellcheck disable=SC2016 # `$` is part of the stable test identity.
  DAGRUN_TEST_COUNTS_PATH="$scratch/results.json" \
    "$ROOT_DIR/ci/write-structured-test-counts.sh" 2 7 \
      'suite$passes' pass 1 'suite$fails' fail 1 || return 1
  first=$(test_result_counts "$scratch/results.json" 1)
  # shellcheck disable=SC2016 # `$` is part of the expected failure identity.
  [[ $first == 'ABORTED at exit 1 after 1 passed; first failure: suite$fails' ]] \
    || return 1
  [[ $(test_result_counts "$scratch/results.json" 0) == \
    'UNKNOWN (exit 0 disagrees with 1 typed failure(s))' ]] || return 1
  # shellcheck disable=SC2016 # `$` is part of the expected failure identity.
  [[ $(strict_result "$scratch/results.json" 1) == \
    'ABORTED at exit 1 after 1 passed; first failure: suite$fails' ]] || return 1

  # shellcheck disable=SC2016 # `$` is part of the stable test identity.
  DAGRUN_TEST_COUNTS_PATH="$scratch/results.json" \
    "$ROOT_DIR/ci/write-structured-test-counts.sh" 3 4 \
      'suite$one' pass 1 'suite$two' pass 1 'suite$three' pass 1 || return 1
  second=$(test_result_counts "$scratch/results.json" 0)
  [[ $second == '3 passed, 0 failed, 4 filtered' ]] || return 1
  [[ $(strict_result "$scratch/results.json" 0) == \
    'passed (3 enabled, 4 filtered)' ]] || return 1
  [[ $(strict_result "$scratch/results.json" 7) == \
    'ABORTED at exit 7 after 3 passed; no typed individual-test failure was recorded' ]] \
    || return 1
  [[ $(test_result_counts "$scratch/results.json" 7) == \
    'ABORTED at exit 7 after 3 passed; no typed individual-test failure was recorded' ]] \
    || return 1

  DAGRUN_TEST_COUNTS_PATH="$scratch/results.json" \
    "$ROOT_DIR/ci/write-structured-test-counts.sh" 0 0 || return 1
  [[ $(test_result_counts "$scratch/results.json" 100) == \
    'ABORTED at exit 100 after 0 passed; no typed individual-test failure was recorded' ]] \
    || return 1
  [[ $(sha256sum "$scratch/fixed-human.log") == "$fixed_log_hash" ]] || return 1

  rm -f -- "$scratch/results.json"
  test_result_counts "$scratch/results.json" 0 >"$scratch/missing" || missing_status=$?
  [[ $missing_status == 0 ]] || return 1
  [[ $(<"$scratch/missing") == \
    'unknown (typed test result unavailable; command exit 0)' ]] || return 1
  printf 'progress-report: typed test-result self-test PASS\n'
}

if [[ ${1:-} == --self-test && $# -eq 1 ]]; then
  self_test
  exit
fi

echo "== Hermit progress report ${REPORT_VERSION} (${DATE_UTC}) =="

# ---------------------------------------------------------------------------
# 0. Context + pull
# ---------------------------------------------------------------------------
PULL_RESULT="skipped (NO_PULL=1)"
if [[ -z ${NO_PULL:-} ]]; then
  echo "-- git pull origin main"
  if PULL_OUT=$(proxy git pull origin main 2>&1); then
    PULL_RESULT=$(printf '%s' "$PULL_OUT" | tail -n1)
  else
    PULL_RESULT="FAILED (kept current HEAD): $(printf '%s' "$PULL_OUT" | tail -n1)"
  fi
fi

COMMIT=$(git rev-parse HEAD)
SHORT=$(git rev-parse --short HEAD)
KERNEL=$(uname -r)
CPU=$(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | sed 's/^ //')
PARANOID=$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo unknown)
RUSTC=$(rustc --version 2>/dev/null)
CARGO=$(cargo --version 2>/dev/null)
NEXTEST=$(cargo nextest --version 2>/dev/null | head -n1)
RUNTIMES=""
for t in python3 node redis-server sqlite3 java; do
  if command -v "$t" >/dev/null 2>&1; then RUNTIMES+="$t "; else RUNTIMES+="$t(MISSING) "; fi
done

# ---------------------------------------------------------------------------
# 1. Strict / fail-closed ratchet (fail-fast; may abort)
# ---------------------------------------------------------------------------
echo "-- strict / fail-closed ratchet"
rm -f -- "$STRICT_RESULTS"
DAGRUN_TEST_COUNTS_PATH="$STRICT_RESULTS" \
  ./scripts/test-fail-closed.sh >"$STRICT_LOG" 2>&1
STRICT_EXIT=$?
STRICT_STATUS=$(strict_result "$STRICT_RESULTS" "$STRICT_EXIT")

# ---------------------------------------------------------------------------
# 2. Working-envelope vector (L1-L4 + rr)
# ---------------------------------------------------------------------------
echo "-- working-envelope vector"
./scripts/validate.rs --envelope-only >"$ENVELOPE_LOG" 2>&1
ENV_JSON=$(grep -E '^\{"l1_pass"' "$ENVELOPE_LOG" | tail -n1)
[[ -z "$ENV_JSON" && -f "$ROOT_DIR/envelope.json" ]] && ENV_JSON=$(cat "$ROOT_DIR/envelope.json")

# ---------------------------------------------------------------------------
# 3. Record / replay
# ---------------------------------------------------------------------------
echo "-- record_replay suite"
rm -f -- "$RECORD_RESULTS"
REC_EXIT=0
DAGRUN_TEST_COUNTS_PATH="$RECORD_RESULTS" \
  ./ci/run-nextest-counted.sh -p hermit --test record_replay >"$RECORD_LOG" 2>&1 \
  || REC_EXIT=$?
RECORD_STATUS=$(test_result_counts "$RECORD_RESULTS" "$REC_EXIT")

# ---------------------------------------------------------------------------
# 4. App e2e suites
# ---------------------------------------------------------------------------
echo "-- app e2e suites"
: >"$APPS_LOG"
mkdir -p "$APPS_RESULTS_DIR"
declare -A APP_STATUS
for t in sqlite_veryquick redis_strict python_stdlib language_runtime_determinism; do
  result_file="$APPS_RESULTS_DIR/$t.json"
  rm -f -- "$result_file"
  echo "########## TARGET: $t ##########" >>"$APPS_LOG"
  app_exit=0
  DAGRUN_TEST_COUNTS_PATH="$result_file" \
    ./ci/run-nextest-counted.sh -p hermit --test "$t" >>"$APPS_LOG" 2>&1 \
    || app_exit=$?
  APP_STATUS[$t]=$(test_result_counts "$result_file" "$app_exit")
done

# ---------------------------------------------------------------------------
# 5. Recently landed PRs
# ---------------------------------------------------------------------------
PRS=$(git log --oneline -40 | grep -iE 'Merge pull request' | head -12 \
  | sed -E 's/^[0-9a-f]+ Merge pull request (#[0-9]+) from [^ ]+/\1/' | paste -sd, - | sed 's/,/, /g')

# ---------------------------------------------------------------------------
# 6. Emit report
# ---------------------------------------------------------------------------
{
  echo "# Hermit Progress Report ${REPORT_VERSION} — ${DATE_UTC}"
  echo
  echo "Generated by \`scripts/progress-report.sh\`. All numbers are live measurements."
  echo "Suites that cannot run are recorded with the exact reason (see rubric:"
  echo "\`.llms/skills/progress-rubric/SKILL.md\`)."
  echo
  echo "## Test context"
  echo
  echo "| Field | Value |"
  echo "| --- | --- |"
  echo "| Commit tested | \`$COMMIT\` (\`$SHORT\`) |"
  echo "| Branch | main (pull: $PULL_RESULT) |"
  echo "| Date (UTC) | $DATE_UTC |"
  echo "| Backend | ptrace |"
  echo "| Host CPU | $CPU |"
  echo "| Kernel | $KERNEL |"
  echo "| perf_event_paranoid | $PARANOID |"
  echo "| Toolchain | $RUSTC; $CARGO; $NEXTEST |"
  echo "| Guest runtimes | $RUNTIMES |"
  echo
  echo "## Summary table"
  echo
  echo "| Suite | Command | Result |"
  echo "| --- | --- | --- |"
  echo "| Strict / fail-closed | scripts/test-fail-closed.sh | $STRICT_STATUS |"
  echo "| Working-envelope L1-L4+rr | scripts/validate.rs --envelope-only | $ENV_JSON |"
  echo "| Record/replay | ci/run-nextest-counted.sh -p hermit --test record_replay | $RECORD_STATUS |"
  for t in sqlite_veryquick redis_strict python_stdlib language_runtime_determinism; do
    echo "| App: $t | ci/run-nextest-counted.sh -p hermit --test $t | ${APP_STATUS[$t]} |"
  done
  echo
  echo "## rr suite"
  echo
  echo "No \`rr_suite\` Cargo target and no \`third-party/rr\` submodule exist in the OSS"
  echo "repo; Meta's Buck rr matrix is not ported. OSS rr coverage = working-envelope rr"
  echo "probes + the record_replay target above."
  echo
  echo "## Recently landed PRs"
  echo
  echo "$PRS"
  echo
  echo "## Logs"
  echo
  echo "- Strict: \`$STRICT_LOG\`"
  echo "- Envelope: \`$ENVELOPE_LOG\`"
  echo "- Record/replay: \`$RECORD_LOG\`"
  echo "- Apps: \`$APPS_LOG\`"
  echo
  echo "> If the strict ratchet aborted, it is fail-fast: later targets did not run."
  echo "> Review the log and classify each failure as host limitation vs product"
  echo "> regression before publishing."
} >"$REPORT"

echo
echo "Report written: $REPORT"
echo "Strict: $STRICT_STATUS"
echo "Envelope: $ENV_JSON"
echo "Record/replay: $RECORD_STATUS"
