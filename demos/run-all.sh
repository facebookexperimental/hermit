#!/usr/bin/env bash
# Run the selected Hermit demos, with one log and one summary row per demo.
# Exits 1 if any selected demo fails; a demo that exits 0 without printing its
# own SUCCESS line has failed. Otherwise it exits 4 if it could not
# create, write, or read its log directory, a demo's log, or its summary, since
# a result is then unknown or unrecorded, and 3 if at least one demo was
# skipped or only saved its first run (and so produced no result). `--help`
# lists the exit statuses.

set -uo pipefail

DEMO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$DEMO_DIR/.." && pwd)"
LOG_DIR="${DEMO_SWEEP_LOG_DIR:-$ROOT/target/demo-sweep}"
SUMMARY="$LOG_DIR/summary.tsv"

usage() {
  cat <<'EOF'
Usage: demos/run-all.sh [--with-analyze] [--with-qemu] [--all] [--group N]

With no option, run the quick demos 1-3.
  --with-analyze  add demo 4 (schedule bisection with `hermit analyze`)
  --with-qemu     add demos 5, 6, and 9 (QEMU/Linux)
  --all           run demos 1-9
  --group N       run one verification group:
                    1  demos 1, 2, 3        (quick, process level)
                    2  demos 4, 8, 9        (analyze, btrfs, QEMU BusyBox)
                    3  demos 5, 6, 7        (QEMU snapshot, resume, drgn)

A demo passes only when it exits 0 and its own last result line,
`=== Demo N: <title>: SUCCESS ===` with N its own number, says SUCCESS. A demo
that exits 0 without that line, for example after stopping silently or after
printing only another demo's result, is recorded as FAIL.

A demo that cannot run on this host prints a SKIPPED line and is recorded as
SKIP; demo 8 does this until its prepare-assets.sh has been run. Run on its
own, such a demo exits 77, so that a skip is never read as a pass; the sweep
sets DEMO_SKIP_EXIT_STATUS=0, so that the demo exits 0 and the SKIPPED line
tells the skip from a failure.

Demos 5 and 6 compare each run with a reference run that their first run
saves. The sweep sets QEMU_BOOT_REPEAT=1 and QEMU_RESUME_REPEAT=1, so a demo
that has no reference run yet saves one and then runs again to compare. A
demo whose last result line still says FIRST RUN SAVED compared nothing and
is recorded as UNCOMPARED. Neither a skipped nor an uncompared demo produced
a result, so neither counts as a pass. The sweep tells them from a pass by
reading the demo's log, so a demo whose log could not be written or read is
recorded as ERROR: its result is unknown.

Exit status:
  0  every selected demo passed
  1  at least one selected demo failed
     (including one that exited 0 without its own SUCCESS line)
  2  usage error
  3  no selected demo failed, but at least one was skipped
     or saved its first run and compared nothing
  4  no selected demo failed, but the sweep could not create, write, or read
     its log directory, a demo's log, or summary.tsv, so a result is unknown
     or unrecorded (with an unusable log directory, no demo is run)
When more than one applies, 1 comes before 4, and 4 before 3. A caller that
accepts skipped or uncompared demos can check for exit status 3 itself.

Logs and summary.tsv go to target/demo-sweep/ (override: DEMO_SWEEP_LOG_DIR).
EOF
}

with_analyze=0
with_qemu=0
with_all=0
group=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --with-analyze) with_analyze=1 ;;
    --with-qemu) with_qemu=1 ;;
    --all) with_analyze=1; with_qemu=1; with_all=1 ;;
    --group)
      [ "$#" -ge 2 ] || { usage >&2; exit 2; }
      group="$2"
      shift
      ;;
    -h|--help) usage; exit 0 ;;
    *) usage >&2; exit 2 ;;
  esac
  shift
done

if [ -n "${DEMO_SWEEP_TARGETS:-}" ]; then
  # Test and debugging override: an explicit list of make targets.
  read -r -a demos <<<"$DEMO_SWEEP_TARGETS"
elif [ -n "$group" ]; then
  case "$group" in
    1) demos=(demo1 demo2 demo3) ;;
    2) demos=(demo4 demo8 demo9) ;;
    3) demos=(demo5 demo6 demo7) ;;
    *) echo "error: --group must be 1, 2, or 3" >&2; exit 2 ;;
  esac
else
  demos=(demo1 demo2 demo3)
  [ "$with_analyze" -eq 0 ] || demos+=(demo4)
  [ "$with_qemu" -eq 0 ] || demos+=(demo5 demo6)
  [ "$with_all" -eq 0 ] || demos+=(demo7 demo8)
  [ "$with_qemu" -eq 0 ] || demos+=(demo9)
fi

if [ "${#demos[@]}" -eq 0 ]; then
  echo "error: no demos selected" >&2
  exit 2
fi

# A demo passes only on its own result line, `=== Demo N: ...`, so the sweep
# must know each target's demo number. Only DEMO_SWEEP_TARGETS can name another
# target, and it is refused before anything runs.
for demo in "${demos[@]}"; do
  if ! [[ "$demo" =~ ^demo[1-9][0-9]*$ ]]; then
    echo "error: $demo is not a demo target (demo1, demo2, ...); the sweep cannot tell whether it passed" >&2
    exit 2
  fi
done

# Without a demo's log the sweep cannot tell a skip from a pass, so refuse to
# start, before any demo runs, when the logs and the summary have nowhere to go.
refuse_to_start() {
  printf 'error: %s\n' "$1" >&2
  printf 'Set DEMO_SWEEP_LOG_DIR to a directory you can write, or unset it to use %s.\n' \
    "$ROOT/target/demo-sweep" >&2
  printf '\n=== Demo suite: ERROR — no demo was run, because the sweep cannot keep its logs and summary ===\n' >&2
  exit 4
}

read -r -a make_command <<<"${MAKE:-make}"
mkdir -p "$LOG_DIR" ||
  refuse_to_start "cannot create the log directory $LOG_DIR"
printf 'demo\tstatus\texit\tduration_seconds\tlog\n' >"$SUMMARY" ||
  refuse_to_start "cannot write the summary $SUMMARY"

# Demos 1-4 share one scratch directory and one build of their guest programs.
export DEMO_TMP="${DEMO_TMP:-$(mktemp -d -t hermit-demo.XXXXXX)}"

# Demos 5 and 6 compare a run with a saved reference run. With no reference
# run yet, a run saves itself as the reference and compares nothing; the demo
# then runs a second time to compare unless QEMU_BOOT_REPEAT (demo 5) or
# QEMU_RESUME_REPEAT (demo 6) is 0. The sweep exists to measure, so it always
# asks for the second run, whatever the caller's environment says.
export QEMU_BOOT_REPEAT=1 QEMU_RESUME_REPEAT=1

# A demo that cannot run here exits 77 on its own, because a skip is not a
# pass. Behind make, which turns any failing status into 2, the sweep could not
# tell that 77 from a failure, so it asks for exit 0 and reads the SKIPPED line
# instead, whatever the caller's environment says.
export DEMO_SKIP_EXIT_STATUS=0

failures=0
passes=0
skips=0
skipped=()
# Demos whose last result line says FIRST RUN SAVED: they compared nothing.
uncompared=0
uncompared_demos=()
errored=()
# One line for each log or summary that could not be written or read. Any
# entry makes the sweep exit 4 unless a demo failed.
record_errors=()
rows=()
for demo in "${demos[@]}"; do
  log="$LOG_DIR/$demo.log"
  started=$SECONDS
  printf '\n=== %s: START ===\n' "$demo"

  "${make_command[@]}" -C "$DEMO_DIR" --no-print-directory "$demo" \
    2>&1 | tee "$log"
  pipe_status=("${PIPESTATUS[@]}")
  rc=${pipe_status[0]}
  tee_rc=${pipe_status[1]}
  duration=$((SECONDS - started))

  log_error=""
  if [ "$tee_rc" -ne 0 ]; then
    log_error="tee could not write the log $log (exit $tee_rc)"
  fi

  if [ "$rc" -ne 0 ]; then
    status=FAIL
    failures=$((failures + 1))
    printf '=== %s: FAIL (exit %s, %ss; log %s) ===\n' \
      "$demo" "$rc" "$duration" "$log" >&2
    if [ "${GITHUB_ACTIONS:-}" = true ]; then
      printf '::error title=%s failed::exit %s after %ss; inspect %s\n' \
        "$demo" "$rc" "$duration" "$log"
    fi
  else
    # A demo exits 0 in cases that only its log separates. It passed: its own
    # last result line, `=== Demo N: <title>: <result> ===` with N its own
    # number, says SUCCESS. Or it cannot run here and printed a SKIPPED line
    # (demo 8 does this when its prepared assets are absent). Or it saved its
    # first run as its reference run and compared nothing: its last result
    # line says FIRST RUN SAVED. A demo that then ran again and compared
    # prints a later SUCCESS or PARTIAL line, so only the last result line
    # counts. Anything else -- no result line at all, a PARTIAL, or only the
    # lines of another demo, such as demo 5 run first to make the boot
    # snapshot that demos 6 and 7 need -- is not a pass, and the demo failed.
    # grep exits 0 when a line matches, 1 when none does, and 2 or more when
    # it cannot read the log; -a makes it print the matching lines even when
    # the log holds binary bytes, such as a guest's serial output. A log that
    # tee could not write completely is not read at all.
    status=""
    if [ -n "$log_error" ]; then
      status=ERROR
    else
      grep -qE '^=== Demo [0-9]+: SKIPPED' "$log"
      grep_rc=$?
      if [ "$grep_rc" -eq 0 ]; then
        status=SKIP
      elif [ "$grep_rc" -eq 1 ]; then
        result_lines="$(grep -aE '^=== Demo [0-9]+: .+: (FIRST RUN SAVED|SUCCESS|PARTIAL) ===$' "$log")"
        grep_rc=$?
        if [ "$grep_rc" -le 1 ]; then
          own_result=""
          while IFS= read -r result_line; do
            case "$result_line" in
              "=== Demo ${demo#demo}: "*) own_result=$result_line ;;
            esac
          done <<<"$result_lines"
          if [[ "$own_result" != *": SUCCESS ===" &&
            "$own_result" != *": FIRST RUN SAVED ===" ]]; then
            status=NO_SUCCESS_LINE
          elif [[ "$own_result" == *": FIRST RUN SAVED ===" ]] ||
            [[ "${result_lines##*$'\n'}" == *": FIRST RUN SAVED ===" ]]; then
            status=UNCOMPARED
          else
            status=PASS
          fi
        fi
      fi
      if [ "$grep_rc" -gt 1 ]; then
        status=ERROR
        log_error="grep could not read the log $log (exit $grep_rc)"
      fi
    fi
    case "$status" in
      NO_SUCCESS_LINE)
        status=FAIL
        failures=$((failures + 1))
        printf '=== %s: FAIL (exit 0 without its own "=== Demo %s: <title>: SUCCESS ===" line, %ss; log %s) ===\n' \
          "$demo" "${demo#demo}" "$duration" "$log" >&2
        if [ "${GITHUB_ACTIONS:-}" = true ]; then
          printf '::error title=%s failed::exit 0 without its SUCCESS line after %ss; inspect %s\n' \
            "$demo" "$duration" "$log"
        fi
        ;;
      SKIP)
        skips=$((skips + 1))
        skipped+=("$demo")
        printf '=== %s: SKIP (%ss; log %s) ===\n' "$demo" "$duration" "$log"
        ;;
      UNCOMPARED)
        uncompared=$((uncompared + 1))
        uncompared_demos+=("$demo")
        printf '=== %s: UNCOMPARED (%ss; it saved its first run and compared nothing; log %s) ===\n' \
          "$demo" "$duration" "$log"
        ;;
      PASS)
        passes=$((passes + 1))
        printf '=== %s: PASS (%ss) ===\n' "$demo" "$duration"
        ;;
      *)
        errored+=("$demo")
        printf '=== %s: ERROR (exit 0, %ss; %s, so a pass cannot be told from a skip or an uncompared run) ===\n' \
          "$demo" "$duration" "$log_error" >&2
        if [ "${GITHUB_ACTIONS:-}" = true ]; then
          printf '::error title=%s result unknown::%s\n' "$demo" "$log_error"
        fi
        ;;
    esac
    case "$demo" in
      demo1|demo2|demo3|demo4) export DEMO_SKIP_BUILD=1 ;;
    esac
  fi
  [ -z "$log_error" ] || record_errors+=("$demo: $log_error")

  row="$(printf '%s\t%s\t%s\t%s\t%s' "$demo" "$status" "$rc" "$duration" "$log")"
  rows+=("$row")
  if ! printf '%s\n' "$row" >>"$SUMMARY"; then
    record_errors+=("$demo: could not append its row to the summary $SUMMARY")
  fi
done

# Print the summary from memory, so that it is complete even when summary.tsv
# is not.
printf '\n=== Demo sweep summary ===\n'
for row in "${rows[@]}"; do
  IFS=$'\t' read -r demo status rc duration log <<<"$row"
  printf '%-8s %-10s exit=%-3s duration=%4ss log=%s\n' \
    "$demo" "$status" "$rc" "$duration" "$log"
done

if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  {
    echo '## Hermit demo sweep'
    echo
    echo '| Demo | Status | Exit | Duration |'
    echo '|---|---:|---:|---:|'
    for row in "${rows[@]}"; do
      IFS=$'\t' read -r demo status rc duration _log <<<"$row"
      printf '| `%s` | **%s** | %s | %ss |\n' "$demo" "$status" "$rc" "$duration"
    done
  } >>"$GITHUB_STEP_SUMMARY"
  # Read the status directly: bash 5.1 does not apply `if !` to a group
  # command whose redirection fails.
  step_summary_rc=$?
  if [ "$step_summary_rc" -ne 0 ]; then
    record_errors+=("could not write the GitHub step summary $GITHUB_STEP_SUMMARY")
  fi
fi

report_record_errors() {
  [ "${#record_errors[@]}" -ne 0 ] || return 0
  printf 'Could not write or read:\n' >&2
  printf '  %s\n' "${record_errors[@]}" >&2
  printf 'Make these paths writable and readable, or set DEMO_SWEEP_LOG_DIR to another directory, and run the sweep again.\n' >&2
}

# Added to the closing line only when some demo compared nothing.
uncompared_note=""
if [ "$uncompared" -ne 0 ]; then
  uncompared_note=", $uncompared saved a first run and compared nothing"
fi
report_uncompared() {
  [ "$uncompared" -ne 0 ] || return 0
  printf 'Saved a first run and compared nothing: %s\n' "${uncompared_demos[*]}" >&2
}

if [ "$failures" -ne 0 ]; then
  printf '\n=== Demo suite: FAILURE — %s demo(s) failed, %s passed, %s skipped%s ===\n' \
    "$failures" "$passes" "$skips" "$uncompared_note" >&2
  if [ "$skips" -ne 0 ]; then
    printf 'Skipped, with no result: %s\n' "${skipped[*]}" >&2
  fi
  report_uncompared
  if [ "${#errored[@]}" -ne 0 ]; then
    printf 'Unknown, because the log could not be read: %s\n' "${errored[*]}" >&2
  fi
  report_record_errors
  exit 1
fi

# A demo whose log could not be read may have skipped, and a row missing from
# summary.tsv leaves the record incomplete, so neither may end as SUCCESS or
# as INCOMPLETE.
if [ "${#record_errors[@]}" -ne 0 ]; then
  printf '\n=== Demo suite: ERROR — %s of %s requested demos passed, %s skipped, %s unknown%s; the sweep could not write or read its records ===\n' \
    "$passes" "${#demos[@]}" "$skips" "${#errored[@]}" "$uncompared_note" >&2
  if [ "${#errored[@]}" -ne 0 ]; then
    printf 'Unknown, because the log could not be read: %s\n' "${errored[*]}" >&2
  fi
  if [ "$skips" -ne 0 ]; then
    printf 'Skipped, with no result: %s\n' "${skipped[*]}" >&2
  fi
  report_uncompared
  report_record_errors
  exit 4
fi

# A skipped demo, and a demo that saved its first run and compared nothing,
# produced no result, so never report either as passed, and give the sweep its
# own nonzero status so that a caller that reads only the exit status does not
# take it for a success.
if [ "$skips" -ne 0 ] || [ "$uncompared" -ne 0 ]; then
  printf '\n=== Demo suite: INCOMPLETE — %s of %s requested demos passed, %s skipped and unmeasured%s ===\n' \
    "$passes" "${#demos[@]}" "$skips" "$uncompared_note" >&2
  if [ "$skips" -ne 0 ]; then
    printf 'Skipped, with no result: %s\n' "${skipped[*]}" >&2
  fi
  report_uncompared
  exit 3
fi

printf '\n=== Demo suite: SUCCESS — all %s requested demos passed ===\n' \
  "${#demos[@]}"
