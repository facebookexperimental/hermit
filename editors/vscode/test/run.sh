#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# Runs the Hermit debug type in a real VS Code, headless (no display needed),
# against two sessions:
#   forward  VS Code starts hermit-dap on stdio and attaches it to
#            `hermit run --gdbserver`.
#   reverse  VS Code connects to `hermit replay --dap-listen` and steps back.
# See README.md in this directory.

set -euo pipefail

usage() {
    cat <<'USAGE'
Usage: editors/vscode/test/run.sh --code PATH [--hermit 'CMD ...'] [--hermit-dap PATH] [--work DIR]

  --code PATH        VS Code's Electron executable, for example
                     VSCode-linux-x64/code from the Linux .tar.gz. The
                     bin/code launcher detaches and returns at once, so a
                     path to it is replaced by the executable beside bin/.
  --hermit 'CMD'     Command that runs hermit, split on spaces
                     (default: target/debug/hermit). A wrapper such as
                     'safehermit --sh-deadline 600 target/debug/hermit' works.
  --hermit-dap PATH  hermit-dap for the stdio session
                     (default: target/debug/hermit-dap).
  --work DIR         Scratch directory, not under /tmp, where Hermit refuses
                     to run guests (default: target/vscode-test).
  --timeout SECONDS  Bound on each VS Code run and on recording the guest
                     (default: 300). A step that takes longer is killed and
                     counts as a failure. A --hermit wrapper with its own
                     deadline (safehermit's --sh-deadline) should keep it
                     shorter than this, so the wrapper ends its run first.
  -h, --help         Show this help.
USAGE
}

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd -- "$here/../../.." && pwd)
code=""
hermit="$repo/target/debug/hermit"
hermit_dap="$repo/target/debug/hermit-dap"
work="$repo/target/vscode-test"
trial_timeout=300
while [ "$#" -gt 0 ]; do
    case "$1" in
        --code) code=$2; shift 2 ;;
        --hermit) hermit=$2; shift 2 ;;
        --hermit-dap) hermit_dap=$2; shift 2 ;;
        --work) work=$2; shift 2 ;;
        --timeout) trial_timeout=$2; shift 2 ;;
        -h | --help) usage; exit 0 ;;
        *) echo "run.sh: unknown argument $1" >&2; usage >&2; exit 2 ;;
    esac
done
if [ -z "$code" ] || [ ! -x "$code" ]; then
    echo "run.sh: --code must name a VS Code executable; download one from" \
        "https://code.visualstudio.com/download (the Linux .tar.gz needs no install)" >&2
    exit 2
fi
if [ "$(basename -- "$(dirname -- "$code")")" = bin ] && [ -x "$(dirname -- "$code")/../code" ]; then
    code="$(dirname -- "$code")/../code"
fi
read -r -a hermit_cmd <<<"$hermit"

mkdir -p "$work"
work=$(cd -- "$work" && pwd)
cc -g -O0 -fno-pie -no-pie "$here/fixtures/squares.c" -o "$work/squares"

free_port() {
    python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'
}

background=()
# shellcheck disable=SC2317  # called by the EXIT trap
cleanup() {
    for pid in "${background[@]}"; do
        kill "$pid" 2>/dev/null || true
    done
}
trap cleanup EXIT

# stop_server PID GRACE: wait up to GRACE tenths of a second for a session's
# Hermit process to exit on its own, as it does when the debug session ends;
# then kill it. After a failed session GRACE is 0: the process is killed at
# once. Fails if a process with a grace period had to be killed, or if one
# would not exit even then, so no wait is unbounded.
stop_server() {
    local pid=$1 grace=$2
    if [ "$grace" -eq 0 ]; then
        kill "$pid" 2>/dev/null || true
    fi
    for _ in $(seq "$grace"); do
        if ! kill -0 "$pid" 2>/dev/null; then
            wait "$pid" 2>/dev/null || true
            return 0
        fi
        sleep 0.1
    done
    kill "$pid" 2>/dev/null || true
    for _ in $(seq 100); do
        if ! kill -0 "$pid" 2>/dev/null; then
            wait "$pid" 2>/dev/null || true
            if [ "$grace" -gt 0 ]; then
                echo "run.sh: Hermit process $pid did not exit after the session; killed it" >&2
                return 1
            fi
            return 0
        fi
        sleep 0.1
    done
    echo "run.sh: Hermit process $pid did not exit even when killed" >&2
    return 1
}

# check_result NAME EXPECT NONCE: the result file this run wrote, for this
# scenario, with every required step and no failures.
check_result() {
    node - "$work/$1.json" "$2" "$3" <<'NODE'
const fs = require('fs');
const [file, expect, nonce] = process.argv.slice(2);
let result;
try {
  result = JSON.parse(fs.readFileSync(file, 'utf8'));
} catch (error) {
  console.error(`no result from this run: ${error.message}`);
  process.exit(1);
}
const required = expect === 'reverse'
  ? ['attach', 'continue', 'continue', 'stepBack', 'reverseContinue']
  : ['attach', 'continue', 'continue'];
const steps = (result.steps || []).map((step) => step.action);
const problems = [];
if (result.nonce !== nonce) problems.push(`result is not from this run (nonce ${result.nonce})`);
if (result.expect !== expect) problems.push(`result is for scenario ${result.expect}`);
if (JSON.stringify(steps) !== JSON.stringify(required)) problems.push(`steps ${steps.join(',')}`);
if (!Array.isArray(result.failures) || result.failures.length) {
  problems.push(`failures: ${JSON.stringify(result.failures)}`);
}
if (problems.length) {
  console.error(problems.join('\n'));
  process.exit(1);
}
NODE
}

# run_code NAME EXPECT CONFIG_JSON: one bounded, headless VS Code run of
# test/index.js, judged only by the result file it writes.
run_code() {
    local name=$1 expect=$2 config=$3 status=0 nonce
    nonce="$name-$$-$RANDOM-$(date +%s%N)"
    rm -f "$work/$name.json"
    HERMIT_SOURCE="$here/fixtures/squares.c" \
    HERMIT_VSCODE_EXPECT="$expect" \
    HERMIT_VSCODE_NONCE="$nonce" \
    HERMIT_VSCODE_RESULT="$work/$name.json" \
    HERMIT_DEBUG_CONFIG="$config" \
        timeout --kill-after=10 "$trial_timeout" \
        "$code" --no-sandbox --disable-gpu --ozone-platform=headless \
        --user-data-dir="$work/$name-user-data" --extensions-dir="$work/$name-extensions" \
        --disable-extensions --disable-workspace-trust --skip-welcome --skip-release-notes \
        --extensionDevelopmentPath="$here/.." --extensionTestsPath="$here/index.js" \
        "$work" >"$work/$name-vscode.log" 2>&1 || status=$?
    if [ "$status" -ne 0 ]; then
        echo "FAIL $name (VS Code exit $status$([ "$status" -ge 124 ] && echo ", timed out after ${trial_timeout} s or killed")); see $work/$name-vscode.log" >&2
        return 1
    fi
    if ! check_result "$name" "$expect" "$nonce" 2>"$work/$name-check.log"; then
        echo "FAIL $name: $(tr '\n' ' ' <"$work/$name-check.log"); see $work/$name.json" >&2
        return 1
    fi
    echo "PASS $name"
}

# Wait until FILE contains PATTERN, or fail after 60 s.
wait_for_line() {
    local file=$1 pattern=$2
    for _ in $(seq 600); do
        grep -q -- "$pattern" "$file" 2>/dev/null && return 0
        sleep 0.1
    done
    echo "run.sh: '$pattern' never appeared in $file" >&2
    return 1
}

failed=0

# forward: VS Code runs hermit-dap on stdio against a deterministic run.
gdb_port=$(free_port)
"${hermit_cmd[@]}" run --gdbserver --gdbserver-port="$gdb_port" -- "$work/squares" \
    </dev/null >"$work/forward-hermit.log" 2>&1 &
server=$!
background+=("$server")
sleep 2
if run_code forward forward "{\"type\": \"hermit\", \"request\": \"attach\", \"name\": \"forward\",
    \"program\": \"$work/squares\", \"target\": \"127.0.0.1:$gdb_port\",
    \"dapPath\": \"$hermit_dap\"}"; then
    grace=300
else
    grace=0
    failed=1
fi
stop_server "$server" "$grace" || failed=1

# reverse: VS Code connects to hermit replay --dap-listen.
rm -rf "${work:?}/recordings"
record_status=0
timeout --kill-after=10 "$trial_timeout" \
    "${hermit_cmd[@]}" record start --data-dir "$work/recordings" -- "$work/squares" \
    </dev/null >"$work/record.log" 2>&1 || record_status=$?
if [ "$record_status" -ne 0 ] || [ ! -s "$work/recordings/last" ]; then
    echo "FAIL reverse: hermit record start exited $record_status$([ "$record_status" -ge 124 ] && echo ", timed out after ${trial_timeout} s or killed"); see $work/record.log" >&2
    exit 1
fi
recording=$(cat "$work/recordings/last")
gdb_port=$(free_port)
dap_port=$(free_port)
"${hermit_cmd[@]}" replay "$recording" --data-dir "$work/recordings" \
    --gdbserver-port="$gdb_port" --dap-listen="$dap_port" \
    </dev/null >"$work/reverse-hermit.log" 2>&1 &
server=$!
background+=("$server")
if wait_for_line "$work/reverse-hermit.log" "hermit-dap: listening on"; then
    if run_code reverse reverse "{\"type\": \"hermit\", \"request\": \"attach\", \"name\": \"reverse\",
        \"port\": $dap_port, \"program\": \"$work/recordings/$recording/exe\",
        \"target\": \"127.0.0.1:$gdb_port\"}"; then
        grace=300
    else
        grace=0
        failed=1
    fi
else
    echo "FAIL reverse: hermit replay --dap-listen never listened; see $work/reverse-hermit.log" >&2
    grace=0
    failed=1
fi
stop_server "$server" "$grace" || failed=1

exit "$failed"
