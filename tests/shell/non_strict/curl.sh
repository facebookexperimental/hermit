#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -eu

# Make sure the proxy doesn't get in the way. Otherwise, we will get a
# "kErrorAddressPrivate" error.
unset http_proxy

dir=$(dirname "$0")

# Optionally take the name of the server script in an env var.
if [[ ! -v PYTHON_SIMPLEST_SERVER ]]; then
    pyserver="$dir/../../util/simplest_server.py"
else
    pyserver="$PYTHON_SIMPLEST_SERVER"
fi
echo "Running server from $pyserver"

# This is set by coproc below. Declaring this to keep the linter happy.
declare server_PID

# Run the server asynchronously, with an open file descriptor to its stdout,
# which is needed to read the address and port of the server.
coproc server { python3 "$pyserver" 1; }

echo "Child process in pid $server_PID"

echo "Waiting for server to start up..."

# Read the address:port combo that the server itself writes to stdout. This
# also ensures the server has fully started up and is ready to receive incoming
# connections.
IFS= read -r -u "${server[0]}" address

echo "Server address: $address"

echo "Now run curl from $(command -v curl)..."
curl "$address"

echo "Waiting for server shutdown"
wait
echo "Finished."
