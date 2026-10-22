# shellcheck shell=bash
# Sourced by run.sh and prepare-assets.sh, which both require two chaos runs of
# one seed to print the same AddressSanitizer report. Defines asan_report and
# nothing else.

# Print the complete AddressSanitizer report in the run output $1: every line
# from the first one that contains "ERROR: AddressSanitizer" through ASAN's
# closing "==PID==ABORTING" line, which ASAN prints just before it aborts the
# process. That covers the faulting access, the faulting, freeing,
# allocating, and thread-creation stacks, the SUMMARY, and the shadow-memory
# map.
#
# Only Hermit's own log lines are left out, wherever they fall: Hermit's
# tracing events start with a UTC timestamp and a level, for example
# "2026-10-01T21:20:16.078060Z ERROR reverie_ptrace::lifecycle: ...", and are
# Hermit's account of the run, not the program's output. Nothing inside the
# report is rewritten or skipped. Lines after ABORTING are not part of the
# report: there the outputs hold Hermit's two "guest terminated by signal"
# events, whose order is not fixed, and, when the run went through a wrapper
# such as bin/safehermit, the wrapper's per-run summary lines.
#
# Returns 0 when the report reaches the ABORTING line and 1 otherwise: an
# output with no report, or with a report that stops before ABORTING, holds no
# complete report to compare.
asan_report() {
  LC_ALL=C awk '
    /^[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9](\.[0-9]+)?Z +(TRACE|DEBUG|INFO|WARN|ERROR) / { next }
    !started && index($0, "ERROR: AddressSanitizer") { started = 1 }
    !started { next }
    { print }
    /^==[0-9]+==ABORTING$/ { closed = 1; exit }
    END { if (!closed) exit 1 }
  ' "$1"
}
