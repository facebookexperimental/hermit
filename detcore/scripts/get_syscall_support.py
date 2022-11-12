#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

import re
import subprocess
import sys

hermit_cli = sys.argv[1]
syscaller = sys.argv[2]

total_syscalls = 332
failures = 0
for syscall_id in range(0, total_syscalls + 1):
    try:
        res = subprocess.run(
            [
                hermit_cli,
                "run",
                "--panic-on-unsupported-syscalls",
                "--",
                syscaller,
                str(syscall_id),
            ],
            capture_output=True,
            timeout=2,
        )
    except subprocess.TimeoutExpired:
        print(f"Pass || syscall: {syscall_id}")
        continue

    if res.returncode == 0:
        print(f"Pass || syscall: {syscall_id}")
    else:
        failed_at_syscall = None
        for line in res.stderr.split(b"\n"):
            if b"unsupported syscall:" in line:
                m = re.search(b"unsupported syscall: (.+?)\\(", line)
                if m:
                    failed_at_syscall = m.group(1).decode("utf-8")

        if failed_at_syscall:
            failures += 1
            print(f"Fail || syscall: {syscall_id} -> {failed_at_syscall}")

print(f"Failed syscalls: {failures}")
print(f"Handled syscalls: {total_syscalls-failures}")
