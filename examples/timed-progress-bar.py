#!/usr/bin/env python3
# (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

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
