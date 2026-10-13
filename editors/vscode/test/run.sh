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
  -h, --help         Show this help.
USAGE
}

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd -- "$here/../../.." && pwd)
code=""
hermit="$repo/target/debug/hermit"
hermit_dap="$repo/target/debug/hermit-dap"
work="$repo/target/vscode-test"
while [ "$#" -gt 0 ]; do
    case "$1" in
        --code) code=$2; shift 2 ;;
        --hermit) hermit=$2; shift 2 ;;
        --hermit-dap) hermit_dap=$2; shift 2 ;;
        --work) work=$2; shift 2 ;;
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

# run_code NAME EXPECT CONFIG_JSON: one headless VS Code run of test/index.js.
run_code() {
    local name=$1 expect=$2 config=$3 status=0
    HERMIT_SOURCE="$here/fixtures/squares.c" \
    HERMIT_VSCODE_EXPECT="$expect" \
    HERMIT_VSCODE_RESULT="$work/$name.json" \
    HERMIT_DEBUG_CONFIG="$config" \
        "$code" --no-sandbox --disable-gpu --ozone-platform=headless \
        --user-data-dir="$work/$name-user-data" --extensions-dir="$work/$name-extensions" \
        --disable-extensions --disable-workspace-trust --skip-welcome --skip-release-notes \
        --extensionDevelopmentPath="$here/.." --extensionTestsPath="$here/index.js" \
        "$work" >"$work/$name-vscode.log" 2>&1 || status=$?
    if [ "$status" -ne 0 ] || [ ! -s "$work/$name.json" ]; then
        echo "FAIL $name (VS Code exit $status); see $work/$name.json and $work/$name-vscode.log" >&2
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
background+=("$!")
sleep 2
run_code forward forward "{\"type\": \"hermit\", \"request\": \"attach\", \"name\": \"forward\",
    \"program\": \"$work/squares\", \"target\": \"127.0.0.1:$gdb_port\",
    \"dapPath\": \"$hermit_dap\"}" || failed=1
wait "${background[-1]}" || true

# reverse: VS Code connects to hermit replay --dap-listen.
rm -rf "${work:?}/recordings"
"${hermit_cmd[@]}" record start --data-dir "$work/recordings" -- "$work/squares" \
    </dev/null >"$work/record.log" 2>&1
recording=$(cat "$work/recordings/last")
gdb_port=$(free_port)
dap_port=$(free_port)
"${hermit_cmd[@]}" replay "$recording" --data-dir "$work/recordings" \
    --gdbserver-port="$gdb_port" --dap-listen="$dap_port" \
    </dev/null >"$work/reverse-hermit.log" 2>&1 &
background+=("$!")
wait_for_line "$work/reverse-hermit.log" "hermit-dap: listening on"
run_code reverse reverse "{\"type\": \"hermit\", \"request\": \"attach\", \"name\": \"reverse\",
    \"port\": $dap_port, \"program\": \"$work/recordings/$recording/exe\",
    \"target\": \"127.0.0.1:$gdb_port\"}" || failed=1
wait "${background[-1]}" || true

exit "$failed"
