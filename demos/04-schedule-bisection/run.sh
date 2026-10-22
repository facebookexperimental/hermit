#!/usr/bin/env bash
#
# Demo 4: schedule bisection.
#
# hermit analyze first finds passing and failing schedules, then bisects their
# event streams to identify the ordering that changes the outcome. This step
# runs the guest many times, and gives up after 10 minutes. The default uses
# syscall-boundary chaos so it is
# portable across hosts; set ANALYZE_MAX_TIMESLICE=400000 to add precise
# PMU preemption. A successful run ends with "Completed analysis successfully".
#
# Verbosity: by default this demo shows only the evolving per-pass search
# progress lines and the final race localization, filtering out hermit
# analyze's convergence diagnostics. Set DEMO_VERBOSE=1 to see the full,
# unfiltered analyze output.

set -euo pipefail

# shellcheck source=demos/lib/display.sh
source "$(dirname "${BASH_SOURCE[0]}")/../lib/display.sh"

# shellcheck disable=SC2034  # consumed by common.sh demo_success/demo_failure
DEMO_LABEL="Demo 4: Schedule Bisection"
demo_header "$DEMO_LABEL"
echo 'hermit analyze first finds passing and failing schedules, then bisects their'
echo 'event streams to identify the ordering that changes the outcome. It runs a'
echo 'debug guest so the report can resolve source locations. It runs the guest'
echo 'many times, and gives up after 10 minutes. The portable default explores'
echo 'syscall-boundary schedules; set ANALYZE_MAX_TIMESLICE=400000 to also switch'
echo 'threads mid-computation using hardware performance counters. A successful run'
echo 'ends with "Completed analysis successfully".'
echo ''
echo 'By default only the per-pass search progress and the final result are shown;'
echo 'run with DEMO_VERBOSE=1 for the full analyze diagnostics.'
echo ''
echo '=========================================='

# shellcheck source=demos/lib/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/../lib/common.sh"

export PYTHON="${PYTHON:-python3}"

# lib/common.sh already built hello_race as a debug binary, so the report can
# resolve its source locations.
export ANALYSIS_REPORT="$DEMO_ARTIFACTS/hello-race-analysis.json"
export ANALYZE_MAX_TIMESLICE="${ANALYZE_MAX_TIMESLICE:-disabled}"

demo_banner "Search and bisect schedules (gives up after 10 minutes)"
# hermit analyze writes its search progress and its convergence diagnostics
# straight to stderr, not through the log framework, so --log=error and
# RUST_LOG do not quiet them. To keep the demo
# readable we filter that stderr stream down to the evolving per-pass progress
# lines plus the final race localization. Set DEMO_VERBOSE=1 to bypass the
# filter and see everything.
#
# When the host lacks CPUID faulting, add --no-virtualize-cpuid to the inner
# runs so CPUID does not become a host input that desyncs record/replay
# ("Expected match before pop").
analyze_run_args=(--base-env=host)
if ! hermit_supports_cpuid_faulting; then
  echo "note: host lacks CPUID faulting; adding --no-virtualize-cpuid to analyze runs" >&2
  analyze_run_args+=(--no-virtualize-cpuid)
fi
analyze_run_flags=()
for arg in "${analyze_run_args[@]}"; do
  analyze_run_flags+=("--run-arg=$arg")
done

# Allowlist of analyze stderr lines to keep in the default (quiet) view: the
# per-pass search progress and the final race localization. NO_COLOR=1 forces
# these markers to plain text so the match is stable regardless of whether
# stderr is a TTY.
demo_analyze_keep='^:: Event-Level Search Pass |^:: Completed analysis successfully|^:: Critical events found|^:: Critical branch boundary|^Critical event index '

run_analyze() {
  NO_COLOR=1 timeout 600 hermit --log=error analyze \
    "${HERMIT_ANALYZE_TMP_FLAGS[@]}" \
    "${analyze_run_flags[@]}" \
    --report-file="$ANALYSIS_REPORT" \
    --analyze-seed=0 \
    --search -- \
    --chaos --summary --max-timeslice="$ANALYZE_MAX_TIMESLICE" -- \
    "$HELLO_RACE"
}

if [ "${DEMO_VERBOSE:-0}" = "1" ]; then
  run_analyze
else
  # Filter only stderr (the noisy stream). stdout carries the human-readable
  # report, which analyze prints after its last stderr line. Hold stdout in a
  # file and print it once analyze exits: a filter running beside a live stdout
  # lets the kept stderr lines land in the middle of the report's stack traces.
  analyze_stdout="$DEMO_TMP/analyze-stdout.txt"
  set +e
  run_analyze 2>&1 >"$analyze_stdout" | grep --line-buffered -E "$demo_analyze_keep" >&2
  analyze_rc=${PIPESTATUS[0]}
  set -e
  cat "$analyze_stdout"
  if [ "$analyze_rc" -ne 0 ]; then
    echo "hermit analyze failed; rerun with DEMO_VERBOSE=1 to see all of its output" >&2
    (exit "$analyze_rc")
  fi
fi

demo_banner "Report the two critical adjacent events"
"$PYTHON" -c 'import json,sys; d=json.load(open(sys.argv[1])); print(d["header"]); print("critical events:", d["critical_event1"]["event_index"], d["critical_event2"]["event_index"])' "$ANALYSIS_REPORT"

echo
echo "Event numbers can vary with the binary and Hermit revision. The default"
echo "localizes the race to the order of two system calls; on a host with hardware"
echo "performance counters, ANALYZE_MAX_TIMESLICE=400000 localizes it more finely."
demo_success
