#!/bin/sh
# make's SHELL for the lint-check-* targets that ci/lint-checks-node.sh runs.
#
# WHY: the node runs `make -Otarget`, which holds each checker's output until the
# checker finishes. A checker that hangs therefore prints nothing at all, and the
# node's log cannot say which of the 44 checkers the wall guard killed. This
# wrapper runs each recipe line with /bin/sh exactly as make would, and writes one
# line when the line starts and one when it ends to the file descriptor named by
# LINT_CHECKS_TRACE_FD. That descriptor is the node's stderr, outside make's
# buffering and outside the capture that classify_run reads, so the lines appear
# as they happen. A started line with no finished line names a checker that was
# still running when the node stopped.
#
# ci/lint-checks-node.sh exports LINT_CHECK_TARGET=$@ to each lint-check-*
# recipe. A command without it (a $(shell ...) make evaluates, or any run outside
# the node) is executed without trace lines.
if [ -z "${LINT_CHECK_TARGET:-}" ] || [ -z "${LINT_CHECKS_TRACE_FD:-}" ]; then
    exec /bin/sh "$@"
fi
case "$LINT_CHECKS_TRACE_FD" in
    *[!0-9]* | '') exec /bin/sh "$@" ;;
esac

# The last argument is the recipe line; the ones before it are .SHELLFLAGS.
for command_text; do :; done
command_text=$(printf '%s' "$command_text" | tr '\n\t' '  ' | cut -c1-100)
# A trace line is advisory. Each one is written from a subshell, so a reader that
# has gone away (SIGPIPE) or a closed descriptor ends that subshell, never this
# wrapper, and the recipe still runs and still decides the exit status.
trace() {
    ( printf '%s\n' "$1" >&"$LINT_CHECKS_TRACE_FD" ) 2>/dev/null || :
}
start=$(date +%s.%N)
trace "lint-checks: started  $LINT_CHECK_TARGET: $command_text"
# The recipe sees what it saw without this wrapper: neither the trace descriptor
# nor the two variables that name it are passed on to the checker.
fd=$LINT_CHECKS_TRACE_FD
(
    unset LINT_CHECKS_TRACE_FD LINT_CHECK_TARGET
    eval "exec $fd>&-"
    exec /bin/sh "$@"
)
rc=$?
elapsed=$(awk -v a="$start" -v b="$(date +%s.%N)" 'BEGIN { printf "%.1f", b - a }')
trace "lint-checks: finished $LINT_CHECK_TARGET: exit $rc after $elapsed s"
exit "$rc"
