#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# RUN: %hermit run --bind /tmp --chaos --verify -- %s
# RUN: %hermit run --bind /tmp --chaos --sched-heuristic=random --verify -- %s
# RUN: %hermit run --bind /tmp --chaos --seed-from=Args --sched-heuristic=random --verify -- %s
# RUN: %hermit run --bind /tmp --chaos --seed-from=Args --sched-heuristic=stickyrandom --verify -- %s

function prnt {
    for ((i=0; i<500; i++)); do
    echo -n "$1";
    done;
    echo;
}

prnt a &
prnt b
wait
echo
