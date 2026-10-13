#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# Negative controls for run.sh, with stand-ins for VS Code and Hermit, so it
# needs neither: each broken editor below must make run.sh fail, in bounded
# time, even when a result file from an earlier run is lying in --work.
#   editors/vscode/test/selftest.sh

set -euo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd -- "$here/../../.." && pwd)
stubs=$(mktemp -d)
trap 'rm -rf -- "$stubs"' EXIT

# A Hermit that never finishes a session: `run` and `replay` wait like a
# GDB server with no client, and `record` leaves a recording ID behind.
cat >"$stubs/hermit" <<'STUB'
#!/usr/bin/env bash
case "$1" in
    record)
        data_dir=$4
        mkdir -p "$data_dir/0123456789abcdef0123456789abcdef"
        echo 0123456789abcdef0123456789abcdef >"$data_dir/last"
        ;;
    replay)
        echo "hermit-dap: listening on 127.0.0.1:1" >&2
        exec sleep 600
        ;;
    *)
        exec sleep 600
        ;;
esac
STUB
printf '#!/bin/sh\nexit 0\n' >"$stubs/editor-exits-0"
printf '#!/bin/sh\nexit 1\n' >"$stubs/editor-exits-1"
printf '#!/bin/sh\nexec sleep 600\n' >"$stubs/editor-hangs"
chmod +x "$stubs"/*

failed=0
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
    start=$(date +%s)
    status=0
    "$here/run.sh" --code "$stubs/$editor" --hermit "$stubs/hermit" --work "$work" \
        --timeout 5 >"$work/run.log" 2>&1 || status=$?
    elapsed=$(($(date +%s) - start))
    passes=$(grep -c '^PASS' "$work/run.log" || true)
    if [ "$status" -eq 0 ] || [ "$passes" -ne 0 ] || [ "$elapsed" -gt 60 ]; then
        echo "FAIL $editor: run.sh exit $status, $passes PASS line(s), ${elapsed} s; see $work/run.log"
        failed=1
    else
        echo "ok   $editor: run.sh exit $status in ${elapsed} s"
    fi
done
exit "$failed"
