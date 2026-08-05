#!/usr/bin/python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# pyre-unsafe

import datetime
import sys


def millis():
    return datetime.datetime.now().timestamp() * 1000


# `numdots` dots are printed, each after `step` milliseconds of Hermit's
# deterministic virtual time have elapsed in the busy-wait below. Both are
# overridable on the command line (numdots, then step) so a caller running under
# an expensive backend -- e.g. KVM, where every clock read is a VM exit -- can
# render a shorter bar without any change to the clock's granularity. The
# defaults reproduce the original 50-dot, 20ms bar for every other caller.
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
