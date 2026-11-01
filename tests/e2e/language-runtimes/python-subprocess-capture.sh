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
# The program exits 1 unless every spawn captured exactly its child's line
# from a child that exited 0, and no worker raised.

set -euo pipefail

case ${1:-} in
    --prepare)
        test -x /usr/bin/python3
        test -x /bin/echo
        ;;
    --run)
        exec /usr/bin/python3 - <<'PYTHON'
import subprocess
import sys
import threading

THREADS = 4
SPAWNS = 5
lock = threading.Lock()
completed = [0]
done = [0]
failures = []


def work(thread):
    try:
        for spawn in range(SPAWNS):
            expected = f"child {thread}.{spawn}"
            child = subprocess.run(
                ["/bin/echo", expected],
                capture_output=True,
                text=True,
            )
            correct = child.returncode == 0 and child.stdout == expected + "\n"
            # Report each spawn as it completes, as a worker pool would: the
            # write runs while the other threads' pipes are open.
            with lock:
                completed[0] += 1
                if correct:
                    done[0] += 1
                else:
                    failures.append(f"thread {thread} spawn {spawn}")
                print(
                    f"spawn {completed[0]}: thread {thread} read {child.stdout.strip()!r} "
                    f"to EOF, rc={child.returncode}{'' if correct else ' (WRONG)'}",
                    flush=True,
                )
    except BaseException as error:
        with lock:
            failures.append(f"thread {thread} raised {error!r}")


threads = [threading.Thread(target=work, args=(thread,)) for thread in range(THREADS)]
for thread in threads:
    thread.start()
for thread in threads:
    thread.join()
if failures or done[0] != THREADS * SPAWNS:
    print(
        f"python-subprocess-capture: FAILED, {done[0]} of {THREADS * SPAWNS} spawns "
        f"captured their child's line; failures: {failures}",
        flush=True,
    )
    sys.exit(1)
print(
    f"python-subprocess-capture: {done[0]} spawns from {THREADS} threads, "
    "every pipe reached EOF with its child's line",
    flush=True,
)
PYTHON
        ;;
    *)
        echo "usage: $0 --prepare|--run" >&2
        exit 2
        ;;
esac
