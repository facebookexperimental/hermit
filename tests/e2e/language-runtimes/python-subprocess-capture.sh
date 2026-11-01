#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Python's subprocess.run with captured output, from four threads at once.
# Each spawn makes a pipe for the child's output and one through which the
# child reports a failed exec, and the parent reads both to EOF. With threads,
# every other thread shares those write ends (CLONE_FILES), so each must go
# with the close of the thread that made it, or a reader never sees EOF.

set -euo pipefail

case ${1:-} in
    --prepare)
        test -x /usr/bin/python3
        test -x /bin/echo
        ;;
    --run)
        exec /usr/bin/python3 - <<'PYTHON'
import subprocess
import threading

THREADS = 4
SPAWNS = 5
lock = threading.Lock()
done = [0]


def work(thread):
    for spawn in range(SPAWNS):
        child = subprocess.run(
            ["/bin/echo", f"child {thread}.{spawn}"],
            capture_output=True,
            text=True,
            check=True,
        )
        # Report each spawn as it completes, as a worker pool would: the
        # write runs while the other threads' pipes are open.
        with lock:
            done[0] += 1
            print(
                f"spawn {done[0]}: thread {thread} read {child.stdout.strip()!r} "
                f"to EOF, rc={child.returncode}",
                flush=True,
            )


threads = [threading.Thread(target=work, args=(thread,)) for thread in range(THREADS)]
for thread in threads:
    thread.start()
for thread in threads:
    thread.join()
print(
    f"python-subprocess-capture: {done[0]} spawns from {THREADS} threads, "
    "every pipe reached EOF",
    flush=True,
)
PYTHON
        ;;
    *)
        echo "usage: $0 --prepare|--run" >&2
        exit 2
        ;;
esac
