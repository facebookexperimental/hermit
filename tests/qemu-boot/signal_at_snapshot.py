#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Stop QEMU and save a snapshot when the run's root process (this
controller) receives SIGUSR1, which `hermit run --signal-at turn=N:SIGUSR1`
sends after scheduler turn N.

usage: signal_at_snapshot.py QEMU WORKDIR
Prints "snapshot bytes=<n> sha256=<hex>" and exits 0 only when every QMP
command succeeded, the migration completed, QEMU exited 0 and the snapshot is
not empty; otherwise it says why on stderr and exits 1. It writes "waiting for
SIGUSR1" to stderr once QMP is ready. QEMU is given paths relative to WORKDIR,
so the snapshot does not depend on where WORKDIR is.
"""

import hashlib
import json
import os
import signal
import socket
import subprocess
import sys
import time

signalled = False


def on_usr1(signo, frame):
    global signalled
    signalled = True


def fail(message):
    print("signal_at_snapshot: " + message, file=sys.stderr, flush=True)
    sys.exit(1)


def qmp(sock_file, command, arguments=None):
    """Send a QMP command and return its "return" value; fail on an error."""
    message = {"execute": command}
    if arguments is not None:
        message["arguments"] = arguments
    sock_file.write((json.dumps(message) + "\n").encode())
    sock_file.flush()
    while True:
        line = sock_file.readline()
        if not line:
            fail("QMP {} got no reply: QEMU closed the socket".format(command))
        reply = json.loads(line)
        if "error" in reply:
            fail("QMP {} failed: {}".format(command, reply["error"]))
        if "return" in reply:
            return reply["return"]


def main():
    signal.signal(signal.SIGUSR1, on_usr1)
    qemu, workdir = sys.argv[1], sys.argv[2]
    os.chdir(workdir)
    qmp_path = "qmp.sock"
    snapshot = "snapshot.bin"
    for path in (qmp_path, snapshot):
        if os.path.exists(path):
            os.unlink(path)
    proc = subprocess.Popen(
        [
            qemu, "-machine", "q35", "-cpu", "qemu64", "-smp", "1", "-m", "64M",
            "-display", "none", "-monitor", "none", "-nodefaults",
            "-qmp", "unix:{},server=on,wait=off".format(qmp_path),
            "-icount", "shift=0,sleep=off",
            "-rtc", "base=2022-01-01T00:00:00,clock=vm", "-no-reboot",
        ],
        stdin=subprocess.DEVNULL,
    )
    while not os.path.exists(qmp_path):
        time.sleep(0.001)
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.connect(qmp_path)
    sock_file = sock.makefile("rwb")
    json.loads(sock_file.readline())
    qmp(sock_file, "qmp_capabilities")
    print("waiting for SIGUSR1", file=sys.stderr, flush=True)
    while not signalled:
        time.sleep(0.001)
    qmp(sock_file, "stop")
    qmp(sock_file, "migrate", {"uri": "file:" + snapshot})
    while True:
        status = qmp(sock_file, "query-migrate").get("status")
        if status in ("completed", "failed", "cancelled"):
            break
        time.sleep(0.001)
    if status != "completed":
        fail("the migration did not complete: status={}".format(status))
    qmp(sock_file, "quit")
    exit_status = proc.wait()
    if exit_status != 0:
        fail("QEMU exited with status {}".format(exit_status))
    data = open(snapshot, "rb").read()
    if not data:
        fail("the snapshot is empty")
    print("snapshot bytes={} sha256={}".format(len(data), hashlib.sha256(data).hexdigest()))


if __name__ == "__main__":
    main()
