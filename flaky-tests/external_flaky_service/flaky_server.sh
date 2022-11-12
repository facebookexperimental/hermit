#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

NUMBER_OF_REQUESTS=$1
port=$2
function request_handler () {
    # Number of times to handle request
    for (( n=0; n<"$NUMBER_OF_REQUESTS"; n++ ))
    do
        # Read from stdin, we have redirected to read from REQUEST
        read -r
        # Generate an random number and store in output
        output=$(( RANDOM % 2 ))
        # Print the output
        echo $output
    done
}

# coproc takes 2 arguments. First is the buffer that stores stdin and stdout, second is a function or file to run.
# In this case, it runs request_handler function and stores the stdin and stdout in REQUEST
coproc REQUEST { request_handler; }

# Run nc (shortform of netcat), -4 specifies ipv4, -n is for not to use DNS. We do not need DNS as we are running on localhost.
# -l is to bind and listen at the specified port. It mainly means server mode.
# Between < > is for read from and write to REQUEST.
nc -4 -n 0.0.0.0 -l "$port" -k <&"${REQUEST[0]}" >&"${REQUEST[1]}"

# REQUEST is the buffer between function request_handler and nc.
