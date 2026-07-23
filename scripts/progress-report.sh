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
# exact reason. See .llms/skills/progress-rubric.md for the rubric.
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

REPORT_VERSION=${REPORT_VERSION:-v3}
DATE_UTC=$(date -u +%Y-%m-%d)
REPORT_DIR="$ROOT_DIR/docs/progress-reports"
REPORT="$REPORT_DIR/${REPORT_VERSION}-${DATE_UTC}.md"
mkdir -p "$REPORT_DIR"

STRICT_LOG=/tmp/progress-strict.log
ENVELOPE_LOG=/tmp/progress-envelope.log
RECORD_LOG=/tmp/progress-record.log
APPS_LOG=/tmp/progress-apps.log

# with-proxy wrapper: use it when present (Meta devserver), else run bare.
proxy() {
  if command -v with-proxy >/dev/null 2>&1; then
    with-proxy "$@"
  else
    "$@"
  fi
}

# Extract "N passed; M failed; K ignored" from a `cargo test` result line.
counts_from() { # <logfile> <target-marker-or-empty>
  local file=$1
  grep -E 'test result:' "$file" 2>/dev/null | tail -n1 \
    | sed -E 's/.*result: [a-zA-Z]+\. ([0-9]+) passed; ([0-9]+) failed; ([0-9]+) ignored.*/\1 \2 \3/'
}

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
./scripts/test-fail-closed.sh >"$STRICT_LOG" 2>&1
STRICT_EXIT=$?
STRICT_PASSED=$(grep -c '==> Fail-closed:' "$STRICT_LOG")
STRICT_RATCHET=$(grep -E 'Fail-closed ratchet passed' "$STRICT_LOG" | tail -n1)
STRICT_FAIL=$(grep -E '^failures:' -A2 "$STRICT_LOG" | grep -vE '^failures:|^--' | sed 's/^ *//' | grep -v '^$' | head -n1)
if [[ $STRICT_EXIT -eq 0 ]]; then
  STRICT_STATUS="passed ($STRICT_RATCHET)"
else
  STRICT_STATUS="ABORTED at exit $STRICT_EXIT after $STRICT_PASSED enabled tests; first failure: ${STRICT_FAIL:-see log}"
fi

# ---------------------------------------------------------------------------
# 2. Working-envelope vector (L1-L4 + rr)
# ---------------------------------------------------------------------------
echo "-- working-envelope vector"
./validate.sh --envelope-only >"$ENVELOPE_LOG" 2>&1
ENV_JSON=$(grep -E '^\{"l1_pass"' "$ENVELOPE_LOG" | tail -n1)
[[ -z "$ENV_JSON" && -f "$ROOT_DIR/envelope.json" ]] && ENV_JSON=$(cat "$ROOT_DIR/envelope.json")

# ---------------------------------------------------------------------------
# 3. Record / replay
# ---------------------------------------------------------------------------
echo "-- record_replay suite"
cargo test -p hermit --test record_replay >"$RECORD_LOG" 2>&1
read -r REC_P REC_F REC_I <<<"$(counts_from "$RECORD_LOG")"

# ---------------------------------------------------------------------------
# 4. App e2e suites
# ---------------------------------------------------------------------------
echo "-- app e2e suites"
: >"$APPS_LOG"
declare -A APP_P APP_F APP_I
for t in sqlite_veryquick redis_strict python_stdlib language_runtime_determinism; do
  echo "########## TARGET: $t ##########" >>"$APPS_LOG"
  cargo test -p hermit --test "$t" >>"$APPS_LOG" 2>&1
  line=$(grep -E 'test result:' "$APPS_LOG" | tail -n1)
  read -r p f i <<<"$(printf '%s' "$line" | sed -E 's/.*result: [a-zA-Z]+\. ([0-9]+) passed; ([0-9]+) failed; ([0-9]+) ignored.*/\1 \2 \3/')"
  APP_P[$t]=${p:-?}; APP_F[$t]=${f:-?}; APP_I[$t]=${i:-?}
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
  echo "\`.llms/skills/progress-rubric.md\`)."
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
  echo "| Working-envelope L1-L4+rr | validate.sh --envelope-only | $ENV_JSON |"
  echo "| Record/replay | cargo test -p hermit --test record_replay | ${REC_P:-?} passed, ${REC_F:-?} failed, ${REC_I:-?} ignored |"
  for t in sqlite_veryquick redis_strict python_stdlib language_runtime_determinism; do
    echo "| App: $t | cargo test -p hermit --test $t | ${APP_P[$t]} passed, ${APP_F[$t]} failed, ${APP_I[$t]} ignored |"
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
echo "Record/replay: ${REC_P:-?}/${REC_F:-?}/${REC_I:-?} (passed/failed/ignored)"
