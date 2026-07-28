#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

import datetime
import hashlib
import os
import random
import threading
import time


WORDS = (
    "alpha",
    "bravo",
    "charlie",
    "delta",
    "echo",
    "foxtrot",
    "golf",
    "hotel",
)
THREADS = 4
ITERATIONS = 64


def random_probe():
    token = os.urandom(16).hex()
    value = random.getrandbits(64)
    hash_order = ",".join(set(WORDS))
    print(f"RANDOM token={token} prng={value} hash_order={hash_order}")


def time_probe():
    os.environ["TZ"] = "UTC"
    time.tzset()
    now_ns = time.time_ns()
    now = datetime.datetime.now(datetime.timezone.utc)
    print(f"TIME unix_ns={now_ns} utc={now.isoformat()} zone={time.tzname[0]}")


def thread_probe():
    lock = threading.Lock()
    ready = threading.Barrier(THREADS + 1)
    schedule = []
    counter = 0

    def worker(worker_id):
        nonlocal counter
        ready.wait()
        for _ in range(ITERATIONS):
            with lock:
                counter += 1
                schedule.append(worker_id)
            time.sleep(0)

    workers = [threading.Thread(target=worker, args=(index,)) for index in range(THREADS)]
    for worker in workers:
        worker.start()
    ready.wait()
    for worker in workers:
        worker.join()

    expected = THREADS * ITERATIONS
    if counter != expected or len(schedule) != expected:
        raise RuntimeError(f"thread counter mismatch: {counter} != {expected}")
    digest = hashlib.sha256(bytes(schedule)).hexdigest()
    print(f"THREAD workers={THREADS} counter={counter} schedule_sha256={digest}")


def system_probe():
    uname = os.uname()
    sentinel = os.environ["HERMIT_RUNTIME_SENTINEL"]
    with open("/proc/self/status", encoding="utf-8") as status_file:
        status = {
            key: value.strip()
            for key, value in (
                line.split(":", 1)
                for line in status_file
                if line.startswith(("Name:", "Threads:"))
            )
        }
    with open("/proc/sys/kernel/hostname", encoding="utf-8") as hostname_file:
        proc_hostname = hostname_file.read().strip()
    print(
        "SYSTEM "
        f"uname={uname.sysname}/{uname.machine}/{uname.nodename} "
        f"env={sentinel} proc={status['Name']}/{status['Threads']}/{proc_hostname}"
    )


random_probe()
time_probe()
thread_probe()
system_probe()
