#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# Controls for run.sh, with stand-ins for VS Code and Hermit, so it needs
# neither. Each broken editor below must make run.sh fail, in bounded time,
# even when a result file from an earlier run is lying in --work; a stand-in
# editor that writes a correct result for this run, and ends the session,
# must make it pass. A recording that hangs must fail too.
#   editors/vscode/test/selftest.sh

set -euo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd -- "$here/../../.." && pwd)
stubs=$(mktemp -d)
trap 'rm -rf -- "$stubs"' EXIT

# A Hermit whose `run` and `replay` wait like a GDB server until the session
# ends, which only the good editor signals (by creating $STUB_SESSION_ENDED),
# and whose `record` leaves a recording ID behind, or hangs if
# $STUB_RECORD_HANGS is set.
cat >"$stubs/hermit" <<'STUB'
#!/usr/bin/env bash
wait_for_session_end() {
    for _ in $(seq 6000); do
        if [ -e "$STUB_SESSION_ENDED" ]; then
            rm -f "$STUB_SESSION_ENDED"
            exit 0
        fi
        sleep 0.1
    done
}
case "$1" in
    record)
        [ -n "${STUB_RECORD_HANGS:-}" ] && exec sleep 600
        data_dir=$4
        mkdir -p "$data_dir/0123456789abcdef0123456789abcdef"
        echo 0123456789abcdef0123456789abcdef >"$data_dir/last"
        ;;
    replay)
        echo "hermit-dap: listening on 127.0.0.1:1" >&2
        wait_for_session_end
        ;;
    *)
        wait_for_session_end
        ;;
esac
STUB
# The good editor: writes the result run.sh asks for, for this run's nonce
# and scenario, then ends the session as a real debug session would.
cat >"$stubs/editor-good" <<'STUB'
#!/usr/bin/env bash
if [ "$HERMIT_VSCODE_EXPECT" = reverse ]; then
    steps='[{"action":"attach"},{"action":"continue"},{"action":"continue"},{"action":"stepBack"},{"action":"reverseContinue"}]'
else
    steps='[{"action":"attach"},{"action":"continue"},{"action":"continue"}]'
fi
printf '{"expect": "%s", "nonce": "%s", "failures": [], "steps": %s}\n' \
    "$HERMIT_VSCODE_EXPECT" "$HERMIT_VSCODE_NONCE" "$steps" >"$HERMIT_VSCODE_RESULT"
touch "$STUB_SESSION_ENDED"
STUB
printf '#!/bin/sh\nexit 0\n' >"$stubs/editor-exits-0"
printf '#!/bin/sh\nexit 1\n' >"$stubs/editor-exits-1"
printf '#!/bin/sh\nexec sleep 600\n' >"$stubs/editor-hangs"
chmod +x "$stubs"/*

failed=0
export STUB_SESSION_ENDED="$stubs/session-ended"

# check NAME WANT_STATUS WANT_PASSES: run.sh as configured below must exit
# WANT_STATUS (0, or nonzero for any failure) with WANT_PASSES PASS lines,
# within 60 s.
check() {
    local name=$1 want_status=$2 want_passes=$3 start status=0 elapsed passes
    start=$(date +%s)
    "$here/run.sh" --code "$stubs/$editor" --hermit "$stubs/hermit" --work "$work" \
        --timeout 5 >"$work/run.log" 2>&1 || status=$?
    elapsed=$(($(date +%s) - start))
    passes=$(grep -c '^PASS' "$work/run.log" || true)
    if { [ "$want_status" -eq 0 ] && [ "$status" -ne 0 ]; } ||
        { [ "$want_status" -ne 0 ] && [ "$status" -eq 0 ]; } ||
        [ "$passes" -ne "$want_passes" ] || [ "$elapsed" -gt 60 ]; then
        echo "FAIL $name: run.sh exit $status, $passes PASS line(s), ${elapsed} s; see $work/run.log"
        failed=1
    else
        echo "ok   $name: run.sh exit $status, $passes PASS line(s), in ${elapsed} s"
    fi
}

editor="editor-good"
work="$repo/target/vscode-selftest-$editor"
mkdir -p "$work"
check "$editor" 0 2

editor="editor-good"
work="$repo/target/vscode-selftest-record-hangs"
mkdir -p "$work"
STUB_RECORD_HANGS=1 check record-hangs 1 1

for editor in editor-exits-0 editor-exits-1 editor-hangs; do
    work="$repo/target/vscode-selftest-$editor"
    mkdir -p "$work"
    # A result from an earlier run that passed every check but this run's
    # nonce must not count.
    printf '%s\n' '{"expect": "forward", "nonce": "stale", "failures": [],
      "steps": [{"action": "attach"}, {"action": "continue"}, {"action": "continue"}]}' \
        >"$work/forward.json"
    printf '%s\n' '{"expect": "reverse", "nonce": "stale", "failures": [],
      "steps": [{"action": "attach"}, {"action": "continue"}, {"action": "continue"},
                {"action": "stepBack"}, {"action": "reverseContinue"}]}' >"$work/reverse.json"
    check "$editor" 1 0
done
exit "$failed"
