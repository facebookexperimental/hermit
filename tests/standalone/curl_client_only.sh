#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -eu

if [ "$*" == "" ]; then
    hermit="hermit"
else
    hermit="$1"
fi

dir=$(dirname "$0")

# Run the server asynchronously, with an open file descriptor to its stdout,
# which is needed to read the address and port of the server.
# exec: the coprocess is the python3 process itself, not a bash
# wrapper, so $server_PID names the process to stop. Where python3
# is a forking launcher that process is the launcher, and the trap
# below relies on SIGTERM reaching the interpreter through it.
coproc server { exec python3 "$dir/../util/simplest_server.py" 1; }
server_pid=${server_PID:-}
# Stop the server however this script ends: under set -e a refused or
# failed Hermit run exits before curl sends the one request the server
# waits for, and the orphaned server would hold this test's stderr
# open until the runner's timeout. The trap keeps the script's exit
# status. It must stay SIGTERM (kill's default): a python3 launcher
# forwards SIGTERM to its interpreter but cannot forward SIGKILL,
# so kill -9 would orphan the interpreter and restore the hang.
trap '[ -z "$server_pid" ] || kill "$server_pid" 2>/dev/null || true' EXIT

# Read the address:port combo that the server itself writes to stdout. This
# also ensures the server has fully started up and is ready to receive incoming
# connections.
IFS= read -r -u "${server[0]}" address
echo "Server address: $address"

echo "Running curl with hermit command: $hermit"
echo "Now run curl from $(command -v curl)..."
"$hermit" --log=info run --network=host --base-env=minimal curl "$address"

echo "Waiting for server shutdown"
wait
trap - EXIT
echo "Done."
