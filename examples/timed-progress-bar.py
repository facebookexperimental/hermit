#!/usr/bin/python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.


import datetime
import sys


def millis():
    return datetime.datetime.now().timestamp() * 1000


# `numdots` dots are printed, each after `step` milliseconds of Hermit's
# deterministic virtual time have elapsed in the busy-wait below. Keep the
# original defaults for every caller, while allowing expensive backends to
# exercise several complete intervals without multiplying VM exits needlessly.
numdots = int(sys.argv[1]) if len(sys.argv) > 1 else 50
step = int(sys.argv[2]) if len(sys.argv) > 2 else 20

start = millis()
prev = start

print("[", end="", flush=True)
for _x in range(numdots):
    current = millis()
    while current - prev < step:
        current = millis()
    print(".", end="", flush=True)
    prev = current

print("]", flush=True)
