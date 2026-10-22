#!/usr/bin/env bash
# Build demo 8's AddressSanitizer btrfs-convert variants and input image from
# pinned public btrfs-progs sources, then find a chaos seed that crashes.

set -euo pipefail

# Show usage and exit before any tool checks or network access.
for arg in "$@"; do
    case "$arg" in
        -h | --help)
            cat <<'EOF'
prepare-assets.sh -- build demo 8's AddressSanitizer btrfs-convert variants

USAGE:
  demos/08-btrfs-convert-uaf/prepare-assets.sh            build the assets and find a crashing seed
  demos/08-btrfs-convert-uaf/prepare-assets.sh -h|--help  show this help and exit

A bare invocation clones btrfs-progs v7.1, builds the buggy and fixed variants
with AddressSanitizer, creates a small ext4 image, and then runs `hermit run
--chaos` over seeds until one crashes, crashes again with the same complete
AddressSanitizer report, and is clean with the fixed variant. It records that
seed in <assets>/.crash-seed. It is idempotent: when the cached assets match
the sources it only re-checks the recorded seed.

Needs: hermit on PATH, autoconf, automake, file, git, make, mkfs.ext4 (e2fsprogs),
patch, pkg-config, truncate, a C compiler with AddressSanitizer, and network
access to github.com.

Env: DEMO08_DIR (assets), DEMO08_BUILD_ROOT, DEMO08_ARTIFACTS, DEMO08_BTRFS_REPO,
DEMO08_BUILD_JOBS, DEMO08_CALIBRATION_SEEDS (default 64), DEMO08_TIMEOUT (the
demo's per-run timeout, default 90, which also caps each calibration run),
DEMO08_CALIBRATION_TIMEOUT (must not exceed DEMO08_TIMEOUT), and
DEMO08_REFUSAL_RETRIES (default 2: how many more times to run a seed when
Hermit refuses a run with exit status 122; a seed refused on every attempt is
reported as refused and skipped, not counted as tested).
EOF
            exit 0
            ;;
    esac
done

DEMO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# asan_report: the complete AddressSanitizer report in a run's output.
# shellcheck source=demos/08-btrfs-convert-uaf/asan-report.sh
source "$DEMO_DIR/asan-report.sh"
# hermit_tmp_check_paths, hermit_tmp_bind_args: show the converter its program
# and image when they are under /tmp, which Hermit hides from it.
# shellcheck source=demos/lib/hermit-tmp-binds.sh
source "$DEMO_DIR/../lib/hermit-tmp-binds.sh"
ROOT="$(cd "$DEMO_DIR/../.." && pwd)"
ASSETS="${DEMO08_DIR:-$ROOT/ignored/demo08-btrfs}"
BUILD_ROOT="${DEMO08_BUILD_ROOT:-$ROOT/ignored/demo08-build}"
ARTIFACTS="${DEMO08_ARTIFACTS:-$ROOT/target/demos/08-btrfs-convert-uaf}"
SOURCE="$BUILD_ROOT/btrfs-progs-v7.1"
STAGING="$BUILD_ROOT/staging"
PATCH="$DEMO_DIR/fixtures/convert-main-v7.1.patch"
VARIANT_SOURCE="$DEMO_DIR/fixtures"
BTRFS_REPO="${DEMO08_BTRFS_REPO:-https://github.com/kdave/btrfs-progs.git}"
BTRFS_TAG=v7.1
BTRFS_COMMIT=4ab0e80be9e3bb1db2e6038e6d4316d35fb7ba8b
PREP_VERSION=2
STAMP="$ASSETS/.nightly-prep-version"
JOBS="${DEMO08_BUILD_JOBS:-$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 2)}"
CALIBRATION_SEEDS="${DEMO08_CALIBRATION_SEEDS:-64}"
# The demo's per-run timeout. run.sh reads DEMO08_TIMEOUT with the same default,
# and a seed is only usable there if it crashes within that time.
DEMO_TIMEOUT="${DEMO08_TIMEOUT:-90}"
# Each calibration run is capped by the demo's own timeout. On a 176-core AMD
# EPYC 9D64 host, seeds 0-15 of the freshly built v7.1 variant took 6 s minimum,
# 11 s median, and 103 s maximum per run. A shorter cap would miss slow crashing
# seeds; a longer one would record seeds that run.sh then cuts off.
CALIBRATION_TIMEOUT="${DEMO08_CALIBRATION_TIMEOUT:-$DEMO_TIMEOUT}"
# How many more times to run a seed when Hermit refuses the run (exit status
# 122); see run_variant_unrefused.
REFUSAL_RETRIES="${DEMO08_REFUSAL_RETRIES:-2}"

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

BUILD_TOOLS=(autoconf automake file git make mkfs.ext4 patch pkg-config truncate)

require_build_tools() {
  local command
  for command in "${BUILD_TOOLS[@]}"; do
    command -v "$command" >/dev/null 2>&1 || fail "$command is required to build the demo 8 assets"
  done
}

command -v sha256sum >/dev/null 2>&1 || fail "sha256sum is required to prepare demo 8"
command -v timeout >/dev/null 2>&1 || fail "timeout is required to prepare demo 8"
[[ $JOBS =~ ^[1-9][0-9]*$ ]] || fail "DEMO08_BUILD_JOBS must be a positive integer"
[[ $CALIBRATION_SEEDS =~ ^[1-9][0-9]*$ ]] || \
  fail "DEMO08_CALIBRATION_SEEDS must be a positive integer"
[[ $DEMO_TIMEOUT =~ ^[1-9][0-9]*$ ]] || fail "DEMO08_TIMEOUT must be a positive integer"
[[ $CALIBRATION_TIMEOUT =~ ^[1-9][0-9]*$ ]] || \
  fail "DEMO08_CALIBRATION_TIMEOUT must be a positive integer"
# A seed found with a longer cap than the demo's timeout could be one the demo
# cuts off, so refuse that configuration outright.
[ "$CALIBRATION_TIMEOUT" -le "$DEMO_TIMEOUT" ] || \
  fail "DEMO08_CALIBRATION_TIMEOUT=$CALIBRATION_TIMEOUT exceeds the demo's per-run budget" \
    "DEMO08_TIMEOUT=$DEMO_TIMEOUT; a seed calibrated above that budget is cut off by the demo"
[[ $REFUSAL_RETRIES =~ ^[0-9]+$ ]] || \
  fail "DEMO08_REFUSAL_RETRIES must be a non-negative integer"
# Calibration runs each converter on its image under Hermit, which gives the
# converter a private /tmp; run_variant binds the converter's directory and the
# image into it when they are under /tmp. Refuse a path it cannot show the
# converter now, before the build, rather than after.
TMP_HINT="Run demo 8 from a checkout outside /tmp or through a path that begins with /tmp/, or set DEMO08_DIR and DEMO08_ARTIFACTS to directories outside /tmp or to absolute paths that begin with /tmp/."
hermit_tmp_check_paths "$TMP_HINT" \
  "$ASSETS/buggy/btrfs-convert" "$ARTIFACTS/chaos-buggy.img" || exit 1
hermit_tmp_check_paths "$TMP_HINT" \
  "$ASSETS/fixed/btrfs-convert" "$ARTIFACTS/chaos-fixed.img" || exit 1

# A crashing seed belongs to the exact buggy binary it was found with, so it is
# stored together with that binary's sha256. A recorded seed whose hash does not
# match the binary present is discarded.
fixture_identity() {
  sha256sum "$ASSETS/buggy/btrfs-convert" | cut -d' ' -f1
}

# A digest of every source the build consumes: the patch and each variant file,
# by relative name and content (so it does not change when the checkout moves).
# The cache stamp includes it, so editing any of them forces a rebuild.
# tests/test_btrfs_demo_controls.py computes the same digest.
fixture_source_identity() {
  {
    sha256sum <"$PATCH"
    find "$VARIANT_SOURCE" -type f -printf '%P\n' | LC_ALL=C sort | while read -r relative; do
      printf '%s ' "$relative"
      sha256sum <"$VARIANT_SOURCE/$relative"
    done
  } | sha256sum | cut -d' ' -f1
}

# A run in which the guest executed ends in a clean conversion (0), an ASAN
# abort (134), or the timeout (124), and leaves output. Any other status means
# Hermit or the environment failed around the guest, which is not the same fact
# as "this seed did not crash". Exit status 122 is Hermit refusing the run; it
# is not counted here either, and run_variant_unrefused retries it and the
# callers report it as refused rather than as "not executed".
seed_executed() {
  local rc=$1 output=$2
  case "$rc" in
    0 | 124 | 134) [ -s "$output" ] && return 0 ;;
  esac
  return 1
}

# Result of the most recent run_variant call. These are globals rather than
# echoed values so the run stays in the main shell, where `set -e` still applies.
RUN_RC=
RUN_ELAPSED=
RUN_ENGAGEMENT=
RUN_UAF=

# Run one variant ("buggy" or "fixed") on one seed. The combined guest output
# goes to OUTPUT and the classification to the RUN_* globals. RUN_ELAPSED is in
# whole seconds of wall time, the quantity the demo's timeout bounds.
run_variant() {
  local variant=$1 seed=$2 image=$3 output=$4
  local rc start engagement=did-not-reach uaf=none
  local -a binds=()

  # Checked explicitly because errexit is suspended here: confirm_seed runs as
  # `if confirm_seed ...`. A failed copy would leave the previous run's
  # converted image in place and make two runs of one seed look different.
  cp --reflink=auto "$ASSETS/pop-tiny.img" "$image" ||
    fail "demo 8 could not stage a fresh image for the $variant run on seed $seed:" \
      "copying $ASSETS/pop-tiny.img to $image failed. This is an I/O or environment fault," \
      "not a disagreement between two runs of the same seed."
  hermit_tmp_bind_args binds "$ASSETS/$variant/btrfs-convert" "$image"
  start=$SECONDS
  set +e
  # The same flags as run.sh's chaos_convert, which explains --base-env=minimal
  # and --epoch: without them the seed recorded here depends on this shell's
  # environment and on the host clock, and run.sh may not reproduce it. It also
  # explains the --bind options, which are present only for paths under /tmp.
  timeout "$CALIBRATION_TIMEOUT" hermit --log=error run \
    --chaos --sched-seed "$seed" --no-virtualize-cpuid \
    --base-env=minimal --epoch=2026-01-01T00:00:00Z \
    "${binds[@]}" \
    -- "$ASSETS/$variant/btrfs-convert" "$image" >"$output" 2>&1
  rc=$?
  set -e
  RUN_ELAPSED=$((SECONDS - start))

  # "Copy inodes [" is the progress line, printed only once the progress thread
  # runs; without it the run never reached the code with the bug.
  if grep -qa 'Copy inodes \[' "$output"; then
    engagement=reached
  fi
  # The report text alone does not qualify a seed: the demo requires the ASAN
  # abort (exit 134). A run can print the start of a report and still exit 0,
  # or be cut off at 124 mid-report. A report that reaches its SUMMARY line is
  # classified complete here. The SUMMARY is not ASAN's last line: the
  # shadow-memory map and the closing ==PID==ABORTING line follow it, and
  # confirm_seed requires that closing line in both runs before it compares
  # their reports.
  if grep -qa 'AddressSanitizer: heap-use-after-free' "$output"; then
    uaf=hit
    if grep -qa 'SUMMARY: AddressSanitizer' "$output"; then
      uaf=complete
    fi
  fi
  RUN_RC=$rc
  RUN_ENGAGEMENT=$engagement
  RUN_UAF=$uaf
}

# Hermit exits 122 when it refuses a run under a fail-closed policy and prints a
# HERMIT_POLICY_REFUSAL line naming the cause, for example cause=skid-overshoot
# after a performance-counter interrupt arrived later than its safety margin,
# which heavy host load makes more likely. A refused run did not test the seed:
# it is neither "this seed did not crash" nor a guest failure. So run_variant is
# repeated, up to REFUSAL_RETRIES more times. Each refused attempt that is
# retried keeps its output as <output>-refused-<n>.out and gets a calibration.tsv
# row whose qualifies column says "refused". The RUN_* globals describe the last
# attempt, so RUN_RC=122 afterwards means every attempt was refused, and the
# caller must report that seed as refused.
#
# A refused fixed-variant run that printed a use-after-free is not retried:
# confirm_seed treats that report as a regression whatever the exit status, and
# a retry must not replace it with a cleaner run.
REFUSED_RUNS=0

run_variant_unrefused() {
  local variant=$1 seed=$2 image=$3 output=$4 report=$5 source=$6 label=$7
  local attempt kept attempts=$((REFUSAL_RETRIES + 1))
  for ((attempt = 1; ; attempt++)); do
    run_variant "$variant" "$seed" "$image" "$output"
    [ "$RUN_RC" = 122 ] || return 0
    REFUSED_RUNS=$((REFUSED_RUNS + 1))
    if [ "$variant" = fixed ] && [ "$RUN_UAF" != none ]; then
      return 0
    fi
    if [ "$attempt" -ge "$attempts" ]; then
      echo "demo 8: Hermit refused the $label run on seed $seed (rc=122) on all $attempts" \
        "attempt(s); its HERMIT_POLICY_REFUSAL line in $output names the cause." >&2
      return 0
    fi
    kept="${output%.out}-refused-${attempt}.out"
    # Checked explicitly: errexit is suspended when this runs under confirm_seed.
    mv -f -- "$output" "$kept" ||
      fail "demo 8 could not keep the refused $label run's output: moving $output to $kept failed"
    record_run "$report" "$seed" "$source" "$label" "$RUN_ENGAGEMENT" "$RUN_UAF" "$RUN_RC" \
      "$RUN_ELAPSED" refused "$kept"
    echo "demo 8: Hermit refused the $label run on seed $seed (rc=122, attempt $attempt of" \
      "$attempts); its HERMIT_POLICY_REFUSAL line in $kept names the cause. Running it again." >&2
  done
}

# Is this buggy-variant run the crash the demo requires: the guest executed,
# reached the progress thread, printed a complete report, and aborted?
run_qualifies() {
  local rc=$1 engagement=$2 uaf=$3 output=$4
  seed_executed "$rc" "$output" || return 1
  [ "$engagement" = reached ] || return 1
  [ "$uaf" = complete ] || return 1
  [ "$rc" = 134 ]
}

# Append one row to calibration.tsv and echo the same fields to the log.
record_run() {
  local report=$1 seed=$2 source=$3 variant=$4 engagement=$5 uaf=$6 rc=$7 elapsed=$8 \
    qualifies=$9 output=${10}
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$seed" "$source" "$variant" "$engagement" "$uaf" "$rc" "$elapsed" "$qualifies" \
    "$output" >>"$report"
  printf '  seed=%s source=%s variant=%s engagement=%s uaf=%s rc=%s elapsed=%ss qualifies=%s output=%s\n' \
    "$seed" "$source" "$variant" "$engagement" "$uaf" "$rc" "$elapsed" "$qualifies" "$output"
}

# Save the complete ASAN report in the run output $1 to $2, or stop the
# calibration. The run qualified, so it exited 134 with the report's SUMMARY
# line, but a report without ASAN's closing ==PID==ABORTING line cannot be
# compared whole, and run.sh refuses such a run.
save_asan_report() {
  local output=$1 saved=$2 label=$3 seed=$4 report=$5
  asan_report "$output" >"$saved" ||
    fail "demo 8 seed $seed: the $label run exited 134 with the report's SUMMARY line, but" \
      "no complete ASAN report could be saved from $output to $saved. The report must run" \
      "through ASAN's closing ==PID==ABORTING line, because two runs of one seed are" \
      "compared on their complete reports (report: $report)"
}

# Check that a candidate seed passes the same checks as run.sh before recording
# it: the buggy variant crashes again on the same seed with the same complete
# ASAN report, from its ERROR line through its closing ABORTING line with only
# Hermit's own log lines left out, and the fixed variant on that seed completes
# cleanly. $5 is the output of the seed's first, qualifying run.
#
# Return 0 to accept, 1 to reject this seed and keep searching. Only a timeout
# (rc=124) or a run that Hermit refused on every attempt (rc=122) rejects a
# seed: the first means the seed does not fit the demo's timeout, the second
# that the check was not run. Any other disagreement between two runs of one
# seed, including two crash reports that differ in any line, is a determinism or
# environment failure, and searching for a friendlier seed would hide it, so
# those stop the whole calibration.
#
# The two timeout rejections are counted separately: one says the seed is too
# slow, the other says the fixed control is.
REJECTED_REPLAY_BUDGET=0
REJECTED_FIXED_BUDGET=0
REJECTED_REFUSED=0

confirm_seed() {
  local report=$1 artifacts=$2 seed=$3 source=$4 first_output=$5
  local rc elapsed engagement uaf output qualifies first_asan replay_asan

  first_asan="${first_output%.out}.asan.txt"
  save_asan_report "$first_output" "$first_asan" first "$seed" "$report"

  output="$artifacts/calibration-confirm-replay-seed-${seed}.out"
  run_variant_unrefused buggy "$seed" "$artifacts/chaos-buggy.img" "$output" \
    "$report" "$source" buggy-replay
  rc=$RUN_RC elapsed=$RUN_ELAPSED engagement=$RUN_ENGAGEMENT uaf=$RUN_UAF
  qualifies=no
  if [ "$rc" = 122 ]; then
    qualifies=refused
  elif run_qualifies "$rc" "$engagement" "$uaf" "$output"; then
    qualifies=yes
  fi
  record_run "$report" "$seed" "$source" buggy-replay "$engagement" "$uaf" "$rc" "$elapsed" \
    "$qualifies" "$output"
  if [ "$qualifies" != yes ]; then
    if [ "$rc" = 122 ]; then
      echo "demo 8 seed $seed crashed on its first run, but Hermit refused every replay" \
        "attempt (rc=122), so the replay was not tested and this seed is not accepted." >&2
      REJECTED_REFUSED=$((REJECTED_REFUSED + 1))
      return 1
    fi
    if [ "$rc" = 124 ]; then
      echo "demo 8 seed $seed replayed past the ${CALIBRATION_TIMEOUT}s budget (rc=124);" \
        "the demo would cut the same run off, so this seed is not accepted." >&2
      REJECTED_REPLAY_BUDGET=$((REJECTED_REPLAY_BUDGET + 1))
      return 1
    fi
    fail "demo 8 seed $seed crashed on its first run and did not on its replay" \
      "(rc=$rc engagement=$engagement uaf=$uaf). Two runs of one seed must agree; this is a" \
      "determinism or environment failure and must not be worked around by choosing another" \
      "seed (report: $report)"
  fi

  # The replay crashed too, and it must print the same complete report: the
  # comparison run.sh's Step 4 makes.
  replay_asan="${output%.out}.asan.txt"
  save_asan_report "$output" "$replay_asan" replay "$seed" "$report"
  if ! cmp -s "$first_asan" "$replay_asan"; then
    diff "$first_asan" "$replay_asan" >&2 || true
    fail "demo 8 seed $seed crashed on its first run and on its replay, but the two ASAN" \
      "reports differ ($first_asan and $replay_asan; the differing lines are above). Two runs" \
      "of one seed must print the same complete report; this is a determinism or environment" \
      "failure and must not be worked around by choosing another seed (report: $report)"
  fi
  echo "  seed=$seed replay printed the same complete ASAN report as the first run" \
    "($(wc -l <"$replay_asan") lines: $replay_asan)"

  output="$artifacts/calibration-confirm-fixed-seed-${seed}.out"
  run_variant_unrefused fixed "$seed" "$artifacts/chaos-fixed.img" "$output" \
    "$report" "$source" fixed
  rc=$RUN_RC elapsed=$RUN_ELAPSED engagement=$RUN_ENGAGEMENT uaf=$RUN_UAF
  qualifies=n/a
  [ "$rc" != 122 ] || qualifies=refused
  record_run "$report" "$seed" "$source" fixed "$engagement" "$uaf" "$rc" "$elapsed" \
    "$qualifies" "$output"
  if [ "$uaf" != none ]; then
    fail "demo 8 fixed variant reported a use-after-free on seed $seed (rc=$rc): the fix does" \
      "not close the window on this schedule (report: $report)"
  fi
  if [ "$rc" = 122 ]; then
    echo "demo 8 fixed control on seed $seed was refused by Hermit on every attempt" \
      "(rc=122), so the control was not tested and this seed is not accepted." >&2
    REJECTED_REFUSED=$((REJECTED_REFUSED + 1))
    return 1
  fi
  if [ "$rc" = 124 ]; then
    echo "demo 8 fixed control on seed $seed ran past the ${CALIBRATION_TIMEOUT}s budget" \
      "(rc=124); the demo's comparison would be cut off, so this seed is not accepted." >&2
    REJECTED_FIXED_BUDGET=$((REJECTED_FIXED_BUDGET + 1))
    return 1
  fi
  if [ "$rc" != 0 ]; then
    fail "demo 8 fixed control on seed $seed did not complete: rc=$rc. The demo requires a" \
      "clean fixed run to show the fix closes the window (report: $report)"
  fi
  return 0
}

calibrate_crash_seed() {
  local artifacts="$ARTIFACTS"
  local image="$artifacts/chaos-buggy.img"
  local report="$artifacts/calibration.tsv"
  local output seed source rc elapsed fixture cached_fixture engagement uaf qualifies i
  # complete_reports counts the subset of uaf_hits whose report reached its
  # SUMMARY line, which separates "the report was cut off" from "the report
  # finished and the guest never aborted".
  # refused counts seeds whose search run Hermit refused on every attempt; they
  # were not tested and are not in executed.
  local executed=0 attempted=0 engaged=0 uaf_hits=0 complete_reports=0 qualified=0 rejected=0
  local refused=0
  local last_rc="" found_seed="" found_source=""
  local cached_seed=""
  local -a seeds=() sources=()

  command -v hermit >/dev/null 2>&1 || \
    fail "hermit is not on PATH -- build it with 'make -C $ROOT release-core' and run: export PATH=\"$ROOT/target/release:\$PATH\""

  fixture="$(fixture_identity)"

  if [ -r "$ASSETS/.crash-seed" ]; then
    seed="$(cut -d' ' -f1 <"$ASSETS/.crash-seed")"
    cached_fixture="$(cut -s -d' ' -f2 <"$ASSETS/.crash-seed")"
    if [[ $seed =~ ^[0-9]+$ ]] && [ "$cached_fixture" = "$fixture" ]; then
      cached_seed="$seed"
      echo "Replaying recorded demo 8 crash seed $seed for buggy binary ${fixture:0:12}."
    elif [ -z "$cached_fixture" ]; then
      echo "Recorded demo 8 crash seed names no buggy binary; searching again." >&2
    else
      echo "Recorded demo 8 crash seed was found with buggy binary ${cached_fixture:0:12}," \
        "but this binary is ${fixture:0:12}; searching again." >&2
    fi
  fi

  if [ -n "$cached_seed" ]; then
    seeds+=("$cached_seed")
    sources+=(cached)
  fi
  for ((seed = 0; seed < CALIBRATION_SEEDS; seed++)); do
    seeds+=("$seed")
    sources+=(cold)
  done

  mkdir -p "$artifacts"
  # `qualifies` only answers "is this a qualifying buggy crash", so the fixed
  # confirmation row carries n/a; its outcome is in the exit and uaf columns.
  printf 'seed\tsource\tvariant\tengagement\tuaf\texit\telapsed\tqualifies\toutput\n' >"$report"

  echo "Searching for a crashing seed for buggy binary ${fixture:0:12}" \
    "(up to $CALIBRATION_SEEDS seeds, ${CALIBRATION_TIMEOUT}s each," \
    "demo budget ${DEMO_TIMEOUT}s)..."
  for ((i = 0; i < ${#seeds[@]}; i++)); do
    seed="${seeds[$i]}"
    source="${sources[$i]}"
    output="$artifacts/calibration-${source}-seed-${seed}.out"
    run_variant_unrefused buggy "$seed" "$image" "$output" "$report" "$source" buggy
    rc=$RUN_RC elapsed=$RUN_ELAPSED engagement=$RUN_ENGAGEMENT uaf=$RUN_UAF
    attempted=$((attempted + 1))
    last_rc=$rc
    if [ "$rc" = 122 ]; then
      refused=$((refused + 1))
      record_run "$report" "$seed" "$source" buggy "$engagement" "$uaf" "$rc" "$elapsed" \
        refused "$output"
      echo "demo 8 seed $seed was not tested: Hermit refused every attempt; trying the next seed." >&2
      continue
    fi
    if seed_executed "$rc" "$output"; then
      executed=$((executed + 1))
    fi
    if [ "$engagement" = reached ]; then
      engaged=$((engaged + 1))
    fi
    if [ "$uaf" != none ]; then
      uaf_hits=$((uaf_hits + 1))
    fi
    if [ "$uaf" = complete ]; then
      complete_reports=$((complete_reports + 1))
    fi
    qualifies=no
    if run_qualifies "$rc" "$engagement" "$uaf" "$output"; then
      qualifies=yes
    fi
    record_run "$report" "$seed" "$source" buggy "$engagement" "$uaf" "$rc" "$elapsed" \
      "$qualifies" "$output"
    if [ "$qualifies" = yes ]; then
      qualified=$((qualified + 1))
      # One qualifying run is a candidate. Only a seed that also passes the
      # replay and fixed-variant checks is recorded.
      if confirm_seed "$report" "$artifacts" "$seed" "$source" "$output"; then
        found_seed="$seed"
        found_source="$source"
        break
      fi
      rejected=$((rejected + 1))
    fi
  done

  # uaf_hits counts report text; qualified counts runs that also aborted with a
  # complete report; unconfirmed counts qualifying seeds that failed the replay
  # or fixed-variant check.
  printf 'Demo 8 calibration summary: engagement=%s/%s uaf_hits=%s/%s qualified=%s/%s executed=%s/%s unconfirmed=%s refused=%s/%s refused_runs=%s report=%s\n' \
    "$engaged" "$attempted" "$uaf_hits" "$attempted" "$qualified" "$attempted" \
    "$executed" "$attempted" "$rejected" "$refused" "$attempted" "$REFUSED_RUNS" "$report"
  if [ "$refused" -gt 0 ]; then
    echo "note: $refused of $attempted seeds were not tested, because Hermit refused every" \
      "attempt on them (rc=122; rows marked refused in $report)." >&2
  fi
  rm -f -- "$image" "$artifacts/chaos-fixed.img"
  if [ -n "$found_seed" ]; then
    printf '%s %s\n' "$found_seed" "$fixture" >"$ASSETS/.crash-seed"
    if [ "$found_source" = cached ]; then
      echo "Demo 8 crash seed replayed: cached seed $found_seed" \
        "(guest exit $last_rc, buggy binary ${fixture:0:12})"
    else
      echo "Demo 8 crash seed calibrated: $found_seed" \
        "(guest exit $last_rc, buggy binary ${fixture:0:12})"
    fi
    return
  fi

  # "No seed crashed" says something about the program only once the guest has
  # both executed and reached the progress thread.
  if [ "$refused" -eq "$attempted" ]; then
    fail "demo 8 calibration was refused by Hermit on every seed: each of the $attempted" \
      "attempted seeds exited 122 on all $((REFUSAL_RETRIES + 1)) attempt(s). The" \
      "HERMIT_POLICY_REFUSAL line in each output names the cause; cause=skid-overshoot, a late" \
      "performance-counter interrupt, is more likely under heavy host load. No seed was tested," \
      "so this says nothing about the use-after-free. Re-run on a quieter host or raise" \
      "DEMO08_REFUSAL_RETRIES (report: $report)"
  fi
  if [ "$executed" -eq 0 ]; then
    fail "demo 8 calibration never executed the guest: 0 of $attempted seeds produced a guest" \
      "exit status (0/124/134) with output; last rc=$last_rc. This is an environment failure" \
      "(hermit or the btrfs-convert binary), NOT an absence of the use-after-free."
  fi
  if [ "$qualified" -gt 0 ]; then
    fail "demo 8 calibration found $qualified qualifying seed(s) but confirmed none:" \
      "$REJECTED_REPLAY_BUDGET seed(s) replayed past the ${CALIBRATION_TIMEOUT}s per-run budget," \
      "$REJECTED_FIXED_BUDGET had a fixed control that ran past it, and $REJECTED_REFUSED had a" \
      "replay or fixed control that Hermit refused on every attempt (rc=122). The first means" \
      "the seed is too slow for the demo, the second that the fixed variant is, and the third" \
      "that the check was not run. No seed is confirmed at DEMO08_TIMEOUT=$DEMO_TIMEOUT" \
      "(report: $report)"
  fi
  if [ "$engaged" -eq 0 ]; then
    fail "demo 8 calibration NO-RESULT: path engagement 0/$attempted; $uaf_hits UAF hits" \
      "cannot qualify without the progress-thread line (report: $report)"
  fi
  # "No use-after-free" is only true when uaf_hits is zero. A complete report
  # from a guest that did not abort (a binary built without abort_on_error)
  # lands here.
  if [ "$uaf_hits" -gt 0 ]; then
    fail "demo 8 calibration found $uaf_hits use-after-free report(s) in $attempted attempted" \
      "seeds and qualified none: $complete_reports were COMPLETE but the guest did not abort" \
      "with 134, which is what a binary built without abort_on_error produces, and" \
      "$((uaf_hits - complete_reports)) stopped before the report's SUMMARY line, which is a run" \
      "cut off or interleaved with another thread. A qualifying run needs the complete report" \
      "AND the abort. This is not an absent use-after-free (report: $report)"
  fi
  fail "no ASAN UAF found after $attempted attempted seeds; path engagement" \
    "$engaged/$attempted ($executed seeds executed; report: $report)"
}

expected_stamp="prep=$PREP_VERSION btrfs=$BTRFS_COMMIT fixture-src=$(fixture_source_identity)"
if [ "$(cat "$STAMP" 2>/dev/null || true)" = "$expected_stamp" ] \
   && [ -x "$ASSETS/buggy/btrfs-convert" ] \
   && [ -x "$ASSETS/fixed/btrfs-convert" ] \
   && [ -r "$ASSETS/pop-tiny.img" ]; then
  calibrate_crash_seed
  echo "Demo 8 assets ready (cached at $ASSETS)"
  exit 0
fi

require_build_tools
mkdir -p "$BUILD_ROOT"
if [ ! -d "$SOURCE/.git" ]; then
  tmp="$BUILD_ROOT/.btrfs-progs-v7.1.$$"
  rm -rf -- "${tmp:?}"
  echo "Fetching btrfs-progs $BTRFS_TAG..."
  if timeout 20 git ls-remote --exit-code "$BTRFS_REPO" "refs/tags/$BTRFS_TAG" \
       >/dev/null 2>&1; then
    git clone --depth 1 --branch "$BTRFS_TAG" "$BTRFS_REPO" "$tmp"
  elif command -v with-proxy >/dev/null 2>&1; then
    # Some networks reach github.com only through a forward proxy, available
    # here as a `with-proxy` wrapper command.
    echo "  direct connection failed; retrying through with-proxy..." >&2
    with-proxy git clone --depth 1 --branch "$BTRFS_TAG" "$BTRFS_REPO" "$tmp"
  else
    fail "cannot fetch $BTRFS_REPO (set DEMO08_BTRFS_REPO to a reachable mirror)"
  fi
  [ "$(git -C "$tmp" rev-parse HEAD)" = "$BTRFS_COMMIT" ] || \
    fail "btrfs-progs $BTRFS_TAG did not resolve to pinned commit $BTRFS_COMMIT"
  mv "$tmp" "$SOURCE"
fi
[ "$(git -C "$SOURCE" rev-parse HEAD)" = "$BTRFS_COMMIT" ] || \
  fail "$SOURCE is not pinned btrfs-progs $BTRFS_COMMIT"

rm -rf -- "${STAGING:?}"
mkdir -p "$STAGING"

build_variant() {
  local name="$1"
  local tree="$BUILD_ROOT/$name"

  rm -rf -- "${tree:?}"
  cp -a --reflink=auto "$SOURCE" "$tree"
  cp "$VARIANT_SOURCE/$name/common/task-utils.c" "$tree/common/task-utils.c"
  cp "$VARIANT_SOURCE/$name/common/task-utils.h" "$tree/common/task-utils.h"
  patch --directory "$tree" --strip=1 --forward <"$PATCH"

  (
    cd "$tree"
    ./autogen.sh >/dev/null
    ./configure --disable-documentation --disable-python --disable-libudev \
      --disable-zoned --disable-backtrace --with-convert=ext2 \
      --with-crypto=builtin >/dev/null
    make -j "$JOBS" \
      EXTRA_CFLAGS='-fsanitize=address -fno-omit-frame-pointer -g -O1 -D_FORTIFY_SOURCE=0' \
      EXTRA_LDFLAGS='-fsanitize=address' btrfs-convert
  )
  install -D -m 0755 "$tree/btrfs-convert" "$STAGING/$name/btrfs-convert"
}

echo "Building demo 8 buggy and fixed AddressSanitizer variants..."
build_variant buggy
build_variant fixed

populate="$BUILD_ROOT/populate"
image="$STAGING/pop-tiny.img"
rm -rf -- "${populate:?}"
mkdir -p "$populate"
for n in $(seq 1 100); do
  printf 'Hermit Demo 8 fixture %03d\n' "$n" >"$populate/file-$n.txt"
done
truncate -s 256M "$image"
mkfs.ext4 -F -q -b 4096 -N 200 -d "$populate" "$image"

mkdir -p "$ASSETS/buggy" "$ASSETS/fixed"
install -m 0755 "$STAGING/buggy/btrfs-convert" "$ASSETS/buggy/btrfs-convert"
install -m 0755 "$STAGING/fixed/btrfs-convert" "$ASSETS/fixed/btrfs-convert"
install -m 0644 "$image" "$ASSETS/pop-tiny.img"
printf '%s\n' "$expected_stamp" >"$STAMP"
rm -f -- "$ASSETS/.crash-seed"
calibrate_crash_seed

printf 'Demo 8 assets prepared at %s\n' "$ASSETS"
printf '  buggy_sha256=%s\n' "$(sha256sum "$ASSETS/buggy/btrfs-convert" | cut -d' ' -f1)"
printf '  fixed_sha256=%s\n' "$(sha256sum "$ASSETS/fixed/btrfs-convert" | cut -d' ' -f1)"
printf '  image_sha256=%s\n' "$(sha256sum "$ASSETS/pop-tiny.img" | cut -d' ' -f1)"
printf '  crash_seed=%s\n' "$(cut -d' ' -f1 <"$ASSETS/.crash-seed")"
printf '  crash_seed_fixture=%s\n' "$(cut -s -d' ' -f2 <"$ASSETS/.crash-seed")"
