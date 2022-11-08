#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

import datetime


def millis():
    return datetime.datetime.now().timestamp() * 1000


start = millis()
prev = start
step = 20
numdots = 50

print("[", end="", flush=True)
for _x in range(numdots):
    current = millis()
    while current - prev < step:
        current = millis()
    print(".", end="", flush=True)
    prev = current

print("]", flush=True)
