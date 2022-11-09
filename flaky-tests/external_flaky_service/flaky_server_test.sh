#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# First argument is the hermit
if [ "$*" == "" ]; then
    hermit="hermit"
else
    hermit="$1"
fi

dir=$(dirname "$0")

# Second argument is the number of requests
NUMBER_OF_REQUESTS=$2

# Starting from 1025 as usually 0-1024 are reserved.
INITIAL_PORT=1025

port=$INITIAL_PORT
# Command to check if the port is free
is_port_free=$(netstat -taln | grep $port)

# Loop to find first free port
while [[ -n "$is_port_free" ]]; do
    port=$(( port+1 ))
    is_port_free=$(netstat -taln | grep $port)
done
echo "Usable Port: $port"

# Launch the flaky server on the specified port.
"$dir"/flaky_server.sh "$NUMBER_OF_REQUESTS" "$port" &

# Make requets to the flaky server
for (( c=0; c<"$NUMBER_OF_REQUESTS"; c++ ))
do
    # We are running `nc localhost $port <<< " " ` in hermit.
    # It sends a request with " " to the specified ip port.
    # Our ip is localhost and port is $port.
    output=$("$hermit" --log=error run --base-env=minimal nc -- localhost $port <<< " ")
    echo "$output"
done

# Kill the application using $port, to make sure we release the port
kill "$(lsof -t -i:"$port")"
wait
echo "End of Program"
exit "$output"
