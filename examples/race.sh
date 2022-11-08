#!/bin/bash
# (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

function prnt {
    for ((i=0; i<200; i++)); do
    echo -n "$1";
    done;
    echo;
}

prnt a &
prnt b
wait
echo
