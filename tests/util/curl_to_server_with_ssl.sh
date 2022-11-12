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
coproc server { python3 "${dir}/../util/ssl_server.py" 1; }

# Read the address:port combo that the server itself writes to stdout. This
# also ensures the server has fully started up and is ready to receive incoming
# connections.
IFS= read -r -u "${server[0]}" address
echo "Server address: ${address}"

echo "Running curl with hermit command: ${hermit}"
echo "Now run curl from $(command -v curl)..."
"${hermit}" run --epoch "$(date --iso-8601=seconds)" --no-sequentialize-threads --no-deterministic-io  -- "$(command -v curl)" -vL --resolve "${address}:127.0.0.1" "https://${address}"

echo "Waiting for server shutdown"
wait
echo "Done."
