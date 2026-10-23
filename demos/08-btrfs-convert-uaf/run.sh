#!/usr/bin/env bash
# Demo 8: a real, schedule-dependent use-after-free in btrfs-convert.
#
# btrfs-convert runs a background "progress" thread that reads a shared
# `struct task_info *info` while the main thread copies inodes. Before upstream
# commit 73e211a7, task_start() detached that thread and task_stop() never
# joined it, so task_deinit() could free(info) while the thread was still
# reading it. Whether that happens depends only on the teardown interleaving,
# which an ordinary run cannot choose. Hermit's chaos scheduler reaches the
# crashing interleaving on specific seeds and reproduced the crash in every run
# counted in README.md.
#
# The demo runs AddressSanitizer builds of two btrfs-convert variants. Both are
# btrfs-progs v7.1 with the demo's own shutdown code for the progress thread,
# not the upstream code before or at 73e211a7 (README.md, "How it works"):
# `buggy` returns from task_stop() without joining the detached thread, as the
# code before 73e211a7 did, and `fixed` joins the thread before its data is
# freed, as 73e211a7 does. The demo reports what one native buggy run
# showed, then shows that the chaos buggy run crashes on a known seed, the chaos
# fixed run on the same seed is clean, and a second run of the seed prints the
# same complete AddressSanitizer report, byte for byte, from its ERROR line
# through its closing ABORTING line. That held on the host described in
# README.md, which gives the host's load where it was recorded. If a
# performance-counter interrupt arrives later than Hermit's safety margin, which
# heavy host load makes more likely, Reverie prints a HERMIT_SKID_OVERSHOOT line
# and Hermit refuses the run: it prints "HERMIT_POLICY_REFUSAL
# class=policy-refusal cause=skid-overshoot count=N" and exits 122, so this
# script reports rc=122 instead of the expected crash. README.md explains this.
# prepare-assets.sh builds the binaries and the input image; WRITEUP.md tells
# the story of the bug.

set -euo pipefail

DEMO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$DEMO_DIR/../.." && pwd)"
ASSETS="${DEMO08_DIR:-$ROOT/ignored/demo08-btrfs}"
ARTIFACTS="${DEMO08_ARTIFACTS:-$ROOT/target/demos/08-btrfs-convert-uaf}"
# asan_report: the complete AddressSanitizer report in a run's output.
# shellcheck source=demos/08-btrfs-convert-uaf/asan-report.sh
source "$DEMO_DIR/asan-report.sh"
# hermit_tmp_check_paths, hermit_tmp_bind_args: show the converter its program
# and image when they are under /tmp, which Hermit hides from it.
# shellcheck source=demos/lib/hermit-tmp-binds.sh
source "$DEMO_DIR/../lib/hermit-tmp-binds.sh"

usage() {
  cat <<'EOF'
Usage: demos/08-btrfs-convert-uaf/run.sh

Show a schedule-dependent btrfs-convert use-after-free that native execution
cannot reproduce on demand and `hermit run --chaos` finds and reproduces; the
README counts the runs in which it did and gives the host's load where it was
recorded. Under heavy host load a late performance-counter interrupt makes
Hermit refuse a run (HERMIT_POLICY_REFUSAL ... cause=skid-overshoot, exit 122),
and this script then reports rc=122; see demos/08-btrfs-convert-uaf/README.md.

Needs AddressSanitizer btrfs-convert binaries and a populated ext4 image, which
demos/08-btrfs-convert-uaf/prepare-assets.sh builds:
  ignored/demo08-btrfs/buggy/btrfs-convert
  ignored/demo08-btrfs/fixed/btrfs-convert
  ignored/demo08-btrfs/pop-tiny.img
When those assets are absent the demo prints SKIPPED and exits 77, the
conventional "skipped" status, because a skip produced no result and is not a
pass. demos/run-all.sh sets DEMO_SKIP_EXIT_STATUS=0 and records the demo as
skipped from its SKIPPED line; the sweep then exits 3.

Useful overrides:
  DEMO08_DIR=/path        asset directory (buggy/, fixed/, pop-tiny.img)
  DEMO08_ARTIFACTS=/path  per-run scratch images and saved ASAN reports
  DEMO08_CRASH_SEED=N     use this seed instead of the one prepare-assets.sh recorded
  DEMO08_TIMEOUT=90       per-run timeout in seconds; prepare-assets.sh reads the
                          same variable, so it only records a seed that fits here
  DEMO08_REQUIRE_ASSETS=1 fail (exit 1) instead of skipping when assets are absent
  DEMO_SKIP_EXIT_STATUS=77
                          exit status of a skip, 0 to 255; run-all.sh sets 0
EOF
}

case "${1:-}" in
  "") ;;
  -h|--help) usage; exit 0 ;;
  *) usage >&2; exit 2 ;;
esac

BUGGY="$ASSETS/buggy/btrfs-convert"
FIXED="$ASSETS/fixed/btrfs-convert"
IMAGE="$ASSETS/pop-tiny.img"
REQUIRE_ASSETS="${DEMO08_REQUIRE_ASSETS:-0}"
if [ "$REQUIRE_ASSETS" != 0 ] && [ "$REQUIRE_ASSETS" != 1 ]; then
  echo "error: DEMO08_REQUIRE_ASSETS must be 0 or 1" >&2
  exit 2
fi
# A skip produced no result, so on its own it must not exit 0 like a pass.
# run-all.sh reads the SKIPPED line itself and sets this to 0.
SKIP_EXIT_STATUS="${DEMO_SKIP_EXIT_STATUS:-77}"
if ! [[ "$SKIP_EXIT_STATUS" =~ ^(0|[1-9][0-9]{0,2})$ ]] ||
  [ "$SKIP_EXIT_STATUS" -gt 255 ]; then
  echo "error: DEMO_SKIP_EXIT_STATUS must be an exit status from 0 to 255" >&2
  exit 2
fi

# The ASAN binaries and the image are large and host-specific, so they live in
# an ignored directory rather than the repository. When they are absent, skip
# with SKIP_EXIT_STATUS (77 unless the caller set DEMO_SKIP_EXIT_STATUS), so a
# standalone run is never read as a pass. run-all.sh sets it to 0 and reports
# the demo as skipped, not failed, from the SKIPPED line; the sweep still exits
# 3 for the skip.
for f in "$BUGGY" "$FIXED" "$IMAGE"; do
  if [ ! -r "$f" ]; then
    if [ "$REQUIRE_ASSETS" = 1 ]; then
      echo "=== Demo 8: FAILURE -- required asset is missing: $f ===" >&2
    else
      echo "=== Demo 8: SKIPPED -- missing asset: $f ==="
    fi
    echo "Build the ASAN btrfs-convert variants and the image first:"
    echo "  demos/08-btrfs-convert-uaf/prepare-assets.sh"
    [ "$REQUIRE_ASSETS" = 0 ] && exit "$SKIP_EXIT_STATUS"
    exit 1
  fi
done

if ! command -v hermit >/dev/null 2>&1; then
  echo "error: hermit is not on PATH -- build it with 'make -C $ROOT release-core' and run: export PATH=\"$ROOT/target/release:\$PATH\"" >&2
  exit 1
fi

if [ -n "${DEMO08_CRASH_SEED:-}" ]; then
  CRASH_SEED="$DEMO08_CRASH_SEED"
elif [ -r "$ASSETS/.crash-seed" ]; then
  # The file holds `<seed> <buggy-binary-sha256>`. A crashing schedule belongs
  # to one exact binary, so a seed recorded for a different binary is refused
  # rather than silently used.
  CRASH_SEED="$(cut -d' ' -f1 <"$ASSETS/.crash-seed")"
  SEED_FIXTURE="$(cut -s -d' ' -f2 <"$ASSETS/.crash-seed")"
  HAVE_FIXTURE="$(sha256sum "$BUGGY" | cut -d' ' -f1)"
  if [ -n "$SEED_FIXTURE" ] && [ "$SEED_FIXTURE" != "$HAVE_FIXTURE" ]; then
    echo "error: Demo 8 crash seed $CRASH_SEED was recorded for buggy binary" \
      "${SEED_FIXTURE:0:12}, but the binary present is ${HAVE_FIXTURE:0:12}." \
      "Re-run demos/08-btrfs-convert-uaf/prepare-assets.sh to find a new seed." >&2
    exit 2
  fi
else
  # On the reference build (Hermit dc92644f96f4, Linux 7.1.3, 316 CPUs,
  # 2026-09-30), seed 7 was the first of seeds 0-7 to give a complete ASAN
  # report with the image at this script's path. With the image at a shorter
  # path under /var/tmp, seeds 7, 15, 23, 27, and 31 of 0-31 did. The crashing
  # seeds depend on the exact binary and on the image path, which is why the
  # recorded .crash-seed above takes precedence.
  CRASH_SEED=7
fi
[[ $CRASH_SEED =~ ^[0-9]+$ ]] || {
  echo "error: Demo 8 crash seed must be a non-negative integer" >&2
  exit 2
}
TIMEOUT="${DEMO08_TIMEOUT:-90}"
# The images that the chaos runs of steps 2 to 4 convert. Hermit gives the
# converter a private /tmp, so from a checkout under /tmp chaos_convert binds
# the converter's directory and its image into it; refuse a path it cannot
# show the converter now, before anything is created or run, rather than
# letting Hermit fail on it.
CHAOS_IMG="$ARTIFACTS/chaos-buggy.img"
FIXED_IMG="$ARTIFACTS/chaos-fixed.img"
TMP_HINT="Run demo 8 from a checkout outside /tmp or through a path that begins with /tmp/, or set DEMO08_DIR and DEMO08_ARTIFACTS to directories outside /tmp or to absolute paths that begin with /tmp/."
hermit_tmp_check_paths "$TMP_HINT" "$BUGGY" "$CHAOS_IMG" || exit 2
hermit_tmp_check_paths "$TMP_HINT" "$FIXED" "$FIXED_IMG" || exit 2
mkdir -p "$ARTIFACTS"

# btrfs-convert rewrites its image in place, so every run gets a fresh copy.
fresh_image() {
  local dst="$1"
  cp --reflink=auto "$IMAGE" "$dst"
}

# The lines of a saved ASAN report that this script prints for a reader: the
# error line (faulting heap address and PC), the two program frames, and the
# SUMMARY. Display only: Step 4 compares the complete report, not these lines.
asan_highlights() {
  grep -aE 'AddressSanitizer: heap-use-after-free|task_period_wait|print_copied_inodes|SUMMARY: AddressSanitizer' "$1" || true
}

# The run reported a use-after-free and its report reached the SUMMARY line. A
# report without its SUMMARY shows ASAN started describing a fault, not that the
# program reached the abort. prepare-assets.sh uses the same definition. The
# SUMMARY is not ASAN's last line: the shadow-memory map and the closing
# ==PID==ABORTING line follow it, and Steps 2 and 4 also require that closing
# line, through asan_report, before they compare two reports.
complete_asan_uaf() {
  local output="$1"
  grep -qa 'AddressSanitizer: heap-use-after-free' "$output" \
    && grep -qa 'SUMMARY: AddressSanitizer' "$output"
}

# One chaos run. --sched-seed selects the interleaving. --no-virtualize-cpuid
# lets the demo also run on hosts without CPUID faulting; with it the guest
# sees the host's real CPUID results on every host, so CPUID is a host input
# in this command.
#
# A seed names one interleaving only for one set of guest inputs, so two more
# host inputs are pinned here, exactly as in prepare-assets.sh:
#   --base-env=minimal  By default the guest inherits the caller's whole
#       environment, and its size and contents shift the guest's stack and heap
#       layout, so a seed calibrated by prepare-assets.sh would name a different
#       interleaving when run.sh or a user's shell ran it.
#   --epoch=...         Without it the virtual clock starts at the host's
#       current time (Hermit prints "source=host-now"), so every run starts
#       from a different clock.
#
# --bind is added only for a converter or image path that begins with /tmp/,
# so the command line for a checkout outside /tmp has no --bind at all. Hermit
# gives the converter a private /tmp, and each --bind mounts one host path at
# the same path inside it: the directory that holds the converter (Hermit
# cannot start a program that is itself a bind target) and the image file (see
# demos/lib/hermit-tmp-binds.sh). prepare-assets.sh adds the same binds, so a
# seed it calibrates in a checkout under /tmp was found with the binds this
# script replays.
chaos_convert() {
  local conv="$1" seed="$2" img="$3" out="$4"
  local -a binds=()
  hermit_tmp_bind_args binds "$conv" "$img"
  timeout "$TIMEOUT" hermit --log=error run \
    --chaos --sched-seed "$seed" --no-virtualize-cpuid \
    --base-env=minimal --epoch=2026-01-01T00:00:00Z \
    "${binds[@]}" \
    -- "$conv" "$img" >"$out" 2>&1
}

# Explain exit status 122 for the chaos run whose output is in $1. Every chaos
# step uses it, so a refusal reads the same whichever step it happens in.
explain_refusal() {
  local out="$1"
  echo "rc=122 means Hermit refused the run: a performance-counter" \
    "interrupt arrived later than its safety margin, so it printed" \
    "HERMIT_SKID_OVERSHOOT and HERMIT_POLICY_REFUSAL ..." \
    "cause=skid-overshoot instead of treating the run as deterministic" \
    "evidence. Heavy host load makes this more likely; see both lines" \
    "in $out and re-run on a quieter host" >&2
}

echo "=== Demo 8: schedule-dependent btrfs-convert use-after-free ==="
echo "Using $(hermit --version 2>/dev/null || echo 'hermit (version unavailable)') ($(command -v hermit))"
echo "seed=$CRASH_SEED timeout=${TIMEOUT}s"
echo "buggy=$BUGGY"
echo "fixed=$FIXED"
echo

# --- Step 1: one native buggy run ---------------------------------------------
# A native run cannot choose its interleaving, so its outcome varies from run to
# run. On the reference build, 29 of 40 native runs showed no use-after-free and
# 11 printed the first lines of an ASAN report, then exited 0 without its
# SUMMARY: the main thread finished and exited while the progress thread was
# still reporting. The demo reports which outcome this run had and continues;
# its point is that the chaos run below crashes on a chosen seed, which it did
# in every run counted in README.md.
echo "--- Step 1: native buggy btrfs-convert ---"
NATIVE_IMG="$ARTIFACTS/native-buggy.img"
fresh_image "$NATIVE_IMG"
native_rc=0
"$BUGGY" "$NATIVE_IMG" >"$ARTIFACTS/native-buggy.out" 2>&1 || native_rc=$?
if complete_asan_uaf "$ARTIFACTS/native-buggy.out"; then
  echo "native buggy: rc=$native_rc with a complete ASAN use-after-free report"
  native_summary="the native run also crashed this time (rc=$native_rc)"
elif grep -qa 'AddressSanitizer: heap-use-after-free' "$ARTIFACTS/native-buggy.out"; then
  echo "native buggy: rc=$native_rc; ASAN started a use-after-free report, but the" \
    "process exited before the report's SUMMARY:"
  grep -a 'AddressSanitizer: heap-use-after-free' "$ARTIFACTS/native-buggy.out"
  native_summary="the native run started an ASAN report but exited rc=$native_rc before it completed"
else
  echo "native buggy: rc=$native_rc with no use-after-free report"
  native_summary="the native run showed no use-after-free"
fi
echo

# --- Step 2: the chaos buggy run crashes on a known seed ----------------------
echo "--- Step 2: chaos buggy, --sched-seed $CRASH_SEED (expect an ASAN use-after-free) ---"
fresh_image "$CHAOS_IMG"
buggy_rc=0
chaos_convert "$BUGGY" "$CRASH_SEED" "$CHAOS_IMG" "$ARTIFACTS/chaos-buggy.out" \
  || buggy_rc=$?
if [ "$buggy_rc" -ne 134 ]; then
  echo "chaos buggy seed $CRASH_SEED did not complete with the ASAN abort:" \
    "rc=$buggy_rc, expected 134" >&2
  case "$buggy_rc" in
    0)   echo "rc=0 means the crash did not occur. If the seed came from an earlier" \
           "prepare-assets.sh, re-run it: it re-checks the recorded seed and" \
           "searches again when that seed no longer crashes" >&2 ;;
    122) explain_refusal "$ARTIFACTS/chaos-buggy.out" ;;
    124) echo "rc=124 means the ${TIMEOUT}s timeout cut the run off" >&2 ;;
    125) echo "rc=125 is a wrapper failure, not a guest crash" >&2 ;;
  esac
  exit 1
fi
if ! complete_asan_uaf "$ARTIFACTS/chaos-buggy.out"; then
  echo "chaos buggy seed $CRASH_SEED exited 134 without a complete ASAN use-after-free report" >&2
  echo "both the use-after-free line and the report's SUMMARY line are required" >&2
  exit 1
fi
# Save the complete report for Step 4 to compare against.
if ! asan_report "$ARTIFACTS/chaos-buggy.out" >"$ARTIFACTS/asan-report.txt"; then
  echo "chaos buggy seed $CRASH_SEED exited 134, but its ASAN report stops" \
    "before ASAN's closing ==PID==ABORTING line, so the complete report" \
    "cannot be compared" >&2
  exit 1
fi
asan_highlights "$ARTIFACTS/asan-report.txt"
echo "chaos buggy: reproduced the use-after-free; the complete" \
  "$(wc -l <"$ARTIFACTS/asan-report.txt")-line ASAN report is in" \
  "$ARTIFACTS/asan-report.txt"
echo

# --- Step 3: the chaos fixed run on the same seed is clean --------------------
echo "--- Step 3: chaos fixed, --sched-seed $CRASH_SEED (expect a clean exit) ---"
fresh_image "$FIXED_IMG"
# This control shows the fix closes the window, so it must both finish and stay
# clean. Read the output first, which catches a use-after-free at any exit
# status, and only then require a clean exit: a timeout, a wrapper failure, or
# an exec failure did not run the control to completion.
fixed_rc=0
chaos_convert "$FIXED" "$CRASH_SEED" "$FIXED_IMG" "$ARTIFACTS/chaos-fixed.out" \
  || fixed_rc=$?
if grep -qaE 'AddressSanitizer: heap-use-after-free' "$ARTIFACTS/chaos-fixed.out"; then
  echo "regression: the fixed variant reported a use-after-free (rc=$fixed_rc)" >&2
  echo "the fix does not close the window on seed $CRASH_SEED" >&2
  exit 1
fi
if [ "$fixed_rc" -ne 0 ]; then
  echo "the fixed control did not complete: rc=$fixed_rc, no use-after-free reported" >&2
  case "$fixed_rc" in
    122) explain_refusal "$ARTIFACTS/chaos-fixed.out" ;;
    124) echo "rc=124 is the ${TIMEOUT}s timeout: the control was cut off" >&2 ;;
    125) echo "rc=125 is a wrapper failure, not a guest result" >&2 ;;
    *)   echo "rc=$fixed_rc is an execution or environment failure" >&2 ;;
  esac
  echo "a control that did not run to completion cannot show that the fix closed" >&2
  echo "the window, so the demo refuses rather than counting it as a clean run" >&2
  exit 1
fi
echo "chaos fixed: completed rc=0 with no use-after-free (73e211a7 closes the window)"
echo

# --- Step 4: the crash reproduces byte-for-byte -------------------------------
# The replay must print the same complete ASAN report as Step 2: every line from
# the ERROR line through the closing ABORTING line, with only Hermit's own log
# lines left out (asan_report). That includes the heap address, the PC, every
# stack, and the shadow-memory map.
#
# Reuse the exact image path from Step 2 so the command line is byte-identical.
# Hermit's determinism is per input, and the report depends on argv: a different
# path length shifts the initial heap layout, which can give a different (but
# still repeatable) heap address or shadow-memory map. README.md gives the
# measurements.
echo "--- Step 4: run --sched-seed $CRASH_SEED again and compare the crash ---"
fresh_image "$CHAOS_IMG"
replay_rc=0
chaos_convert "$BUGGY" "$CRASH_SEED" "$CHAOS_IMG" "$ARTIFACTS/chaos-buggy-replay.out" \
  || replay_rc=$?
if [ "$replay_rc" -ne 134 ]; then
  echo "replay did not complete with the ASAN abort: rc=$replay_rc, expected 134" >&2
  case "$replay_rc" in
    122) explain_refusal "$ARTIFACTS/chaos-buggy-replay.out" ;;
    124) echo "rc=124 means the ${TIMEOUT}s timeout cut the run off" >&2 ;;
    125) echo "rc=125 is a wrapper failure, not a guest crash" >&2 ;;
  esac
  exit 1
fi
if ! complete_asan_uaf "$ARTIFACTS/chaos-buggy-replay.out"; then
  echo "replay exited 134 without a complete ASAN use-after-free report" >&2
  echo "both the use-after-free line and the report's SUMMARY line are required" >&2
  exit 1
fi
if ! asan_report "$ARTIFACTS/chaos-buggy-replay.out" >"$ARTIFACTS/asan-report-replay.txt"; then
  echo "replay exited 134, but its ASAN report stops before ASAN's closing" \
    "==PID==ABORTING line, so the complete report cannot be compared" >&2
  exit 1
fi
if cmp -s "$ARTIFACTS/asan-report.txt" "$ARTIFACTS/asan-report-replay.txt"; then
  echo "replay: ASAN report byte-identical: all" \
    "$(wc -l <"$ARTIFACTS/asan-report.txt") lines from the ERROR line through" \
    "ABORTING, including the heap address, PC, every stack, and the shadow memory"
else
  echo "replay: ASAN reports differ between runs" >&2
  diff "$ARTIFACTS/asan-report.txt" "$ARTIFACTS/asan-report-replay.txt" >&2 || true
  exit 1
fi

echo
echo "=== Demo 8: btrfs-convert Use-After-Free: SUCCESS ==="
echo "$native_summary; chaos crashed on seed $CRASH_SEED,"
echo "the fix closed the window, and the crash reproduced exactly."
