#!/usr/bin/env bash
# Common Cargo diagnostics; does not decide source policy or grant resources.
set -euo pipefail
ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
if (($# == 0)); then
    echo "usage: ci/run-build-command.sh COMMAND [ARG...]" >&2
    exit 2
fi

if [[ -n ${HERMIT_CARGO_TIMINGS_DIR:-} && ${1:-} == cargo && ${2:-} == build ]]; then
    timings_started=$(date +%s)
    timings_status=0
    "$ROOT_DIR/ci/retry-on-compiler-crash.sh" "$@" --timings || timings_status=$?
    timings_reports=${CARGO_TARGET_DIR:-$PWD/target}/cargo-timings
    if ! mkdir -p -- "$HERMIT_CARGO_TIMINGS_DIR" ||
        ! find "$timings_reports" -maxdepth 1 -name 'cargo-timing-*.html' -newermt "@$timings_started" \
            -exec cp -- {} "$HERMIT_CARGO_TIMINGS_DIR/" \; ; then
        echo "run-with-reverie-dbt-budget.sh: could not keep the cargo --timings report in $HERMIT_CARGO_TIMINGS_DIR (diagnostics only)" >&2
    fi
    exit "$timings_status"
fi
exec "$ROOT_DIR/ci/retry-on-compiler-crash.sh" "$@"
