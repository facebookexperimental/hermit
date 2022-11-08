#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

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
