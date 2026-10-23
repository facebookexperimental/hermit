#!/usr/bin/env bash
#
# Demo 3: chaos concurrency testing.
#
# hello_race contains an intentional data race. Chaos mode makes scheduler
# choices with a seeded PRNG, so different seeds explore different interleavings
# and the same seed reproduces the same result. A schedule recorded from a
# failing seed reproduces that failure when it is replayed under a seed that
# passes without it.

set -euo pipefail

# shellcheck source=demos/lib/display.sh
source "$(dirname "${BASH_SOURCE[0]}")/../lib/display.sh"

# shellcheck disable=SC2034  # consumed by common.sh demo_success/demo_failure
DEMO_LABEL="Demo 3: Chaos Concurrency Testing"
demo_header "$DEMO_LABEL"
echo 'hello_race contains an intentional data race. Chaos mode makes scheduler'
echo 'choices with a seeded PRNG, so different seeds explore different interleavings'
echo 'and the same seed reproduces the same result. Seed 1 passes; seed 0 reaches the'
echo "antagonistic schedule and returns the guest's expected failure status. The demo"
echo "surveys seeds 0-15, then records seed 0's schedule to a file and replays the"
echo 'file under seed 1: the replay fails like the recording, with identical output.'
echo ''
echo '=========================================='

# shellcheck source=demos/lib/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/../lib/common.sh"

# hello_race's two outcomes. It exits 0 after printing PASS_LINE when the
# second thread's store lands last, and exits 1 after printing FAIL_LINE when
# the first thread's store lands last. Any other status or output means the run
# did not reach either outcome (for example Hermit failed), and the demo stops.
PASS_LINE='Did not find antagonistic schedule. Succeeding.'
FAIL_LINE='Antagonistic schedule reached, failing.'
# The seeds among 0-15 that reach the failing order with this demo's flags.
EXPECTED_FAILING_SEEDS='0 5 6 12 15'

# check_outcome LABEL STATUS OUTPUT_FILE EXPECTED_STATUS EXPECTED_LINE
check_outcome() {
  local label="$1" status="$2" output="$3" want_status="$4" want_line="$5"
  if [ ! -s "$output" ]; then
    echo "$label: produced no output (exit status $status)" >&2
    exit 1
  fi
  if [ "$status" -ne "$want_status" ]; then
    echo "$label: exit status $status, expected $want_status" >&2
    cat "$output" >&2
    exit 1
  fi
  if ! grep -Fxq -- "$want_line" "$output"; then
    echo "$label: output lacks the line: $want_line" >&2
    cat "$output" >&2
    exit 1
  fi
}

demo_banner "Seed 1 passes; seed 0 reproduces the expected failure"
status=0
chaos_run 1 >"$DEMO_TMP/seed-1.txt" || status=$?
cat "$DEMO_TMP/seed-1.txt"
check_outcome "seed 1" "$status" "$DEMO_TMP/seed-1.txt" 0 "$PASS_LINE"

status=0
chaos_run 0 >"$DEMO_TMP/seed-0.txt" || status=$?
cat "$DEMO_TMP/seed-0.txt"
check_outcome "seed 0" "$status" "$DEMO_TMP/seed-0.txt" 1 "$FAIL_LINE"
echo 'seed 0 reproduced the expected concurrency failure'

demo_banner "Survey seeds 0..15, retaining each run's output"
failing_seeds=()
for seed in $(seq 0 15); do
  status=0
  chaos_run "$seed" >"$DEMO_TMP/chaos-$seed.txt" || status=$?
  case "$status" in
    0)
      check_outcome "seed $seed" "$status" "$DEMO_TMP/chaos-$seed.txt" 0 "$PASS_LINE"
      result=pass
      ;;
    1)
      check_outcome "seed $seed" "$status" "$DEMO_TMP/chaos-$seed.txt" 1 "$FAIL_LINE"
      result=fail
      failing_seeds+=("$seed")
      ;;
    *)
      echo "seed $seed: exit status $status, expected 0 (pass) or 1 (fail)" >&2
      cat "$DEMO_TMP/chaos-$seed.txt" >&2
      exit 1
      ;;
  esac
  printf 'seed=%s result=%s\n' "$seed" "$result"
done
if [ "${failing_seeds[*]}" != "$EXPECTED_FAILING_SEEDS" ]; then
  echo "failing seeds were: ${failing_seeds[*]}; expected: $EXPECTED_FAILING_SEEDS" >&2
  exit 1
fi
# The same seed must give the same output as it did in the first step.
for seed in 0 1; do
  if ! cmp -s "$DEMO_TMP/seed-$seed.txt" "$DEMO_TMP/chaos-$seed.txt"; then
    echo "seed $seed gave different output on its second run" >&2
    exit 1
  fi
done
echo "failing seeds: ${failing_seeds[*]}; seeds 0 and 1 repeated their first output byte for byte"

demo_banner "Save and replay the failing schedule"
export CHAOS_SCHEDULE="$DEMO_ARTIFACTS/hello-race-schedule.json"

# virtual_time_fields STDERR_FILE: print "EPOCH SOURCE" from the line
# "hermit: virtual-time epoch=EPOCH source=SOURCE; reproduce with --epoch=EPOCH"
# that Hermit writes to stderr when a run starts.
virtual_time_fields() {
  sed -n -E 's/^hermit: virtual-time epoch=([^ ;]+) source=([a-z-]+); reproduce with --epoch=\1$/\1 \2/p' "$1"
}

# Record seed 0's failing run, then replay that file under seed 1. Seed 1
# passes without the file (first step), so a failing replay shows that the
# file, not the seed, chose the failing order. Both runs must reach the failing
# order: exit status 1 and FAIL_LINE.
#
# Both runs keep Hermit's stderr in a file, to check that the replay started
# its virtual clock at the instant stored in the file. HERMIT_EPOCH and
# HERMIT_LOG_FILE are unset for them: a set HERMIT_EPOCH would give both runs
# that start, so the replay would not take it from the file, and a set
# HERMIT_LOG_FILE would move the virtual-time line off stderr.
status=0
env -u HERMIT_EPOCH -u HERMIT_LOG_FILE hermit --log=error run \
  "${HERMIT_TMP_FLAGS[@]}" \
  --chaos --seed=0 \
  --base-env=minimal \
  --no-virtualize-cpuid \
  --max-timeslice=disabled \
  --env=HERMIT_MODE=chaos \
  --record-preemptions-to="$CHAOS_SCHEDULE" \
  -- "$HELLO_RACE" >"$DEMO_TMP/chaos-recorded.txt" 2>"$DEMO_TMP/chaos-recorded.err" || status=$?
cat "$DEMO_TMP/chaos-recorded.err" >&2
check_outcome "recording the failing schedule (seed 0)" "$status" \
  "$DEMO_TMP/chaos-recorded.txt" 1 "$FAIL_LINE"
test -s "$CHAOS_SCHEDULE"

status=0
env -u HERMIT_EPOCH -u HERMIT_LOG_FILE hermit --log=error run \
  "${HERMIT_TMP_FLAGS[@]}" \
  --chaos --seed=1 \
  --base-env=minimal \
  --no-virtualize-cpuid \
  --max-timeslice=disabled \
  --env=HERMIT_MODE=chaos \
  --replay-preemptions-from="$CHAOS_SCHEDULE" \
  -- "$HELLO_RACE" >"$DEMO_TMP/chaos-replayed.txt" 2>"$DEMO_TMP/chaos-replayed.err" || status=$?
cat "$DEMO_TMP/chaos-replayed.err" >&2
check_outcome "replaying the failing schedule under seed 1" "$status" \
  "$DEMO_TMP/chaos-replayed.txt" 1 "$FAIL_LINE"
cmp "$DEMO_TMP/chaos-recorded.txt" "$DEMO_TMP/chaos-replayed.txt"

recorded_clock="$(virtual_time_fields "$DEMO_TMP/chaos-recorded.err")"
replayed_clock="$(virtual_time_fields "$DEMO_TMP/chaos-replayed.err")"
if [ -z "$recorded_clock" ] || [ "$replayed_clock" != "${recorded_clock%% *} recording" ]; then
  echo "the replay did not start its virtual clock at the instant stored in $CHAOS_SCHEDULE" >&2
  echo "recording: ${recorded_clock:-no virtual-time line}; replay: ${replayed_clock:-no virtual-time line}" >&2
  exit 1
fi
echo 'Seed 1 passes without the file. Replayed under seed 1, the file reproduced the'
echo "recording's failure with identical output, starting the virtual clock at the"
echo "recording's instant (${recorded_clock%% *}):"
cat "$DEMO_TMP/chaos-replayed.txt"

demo_success
