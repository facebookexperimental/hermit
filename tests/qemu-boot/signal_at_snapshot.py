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
Prints "snapshot bytes=<n> sha256=<hex>". QEMU is given paths relative to
WORKDIR, so the snapshot does not depend on where WORKDIR is.
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


def qmp(sock_file, command, arguments=None):
    message = {"execute": command}
    if arguments is not None:
        message["arguments"] = arguments
    sock_file.write((json.dumps(message) + "\n").encode())
    sock_file.flush()
    while True:
        reply = json.loads(sock_file.readline())
        if "return" in reply or "error" in reply:
            return reply


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
    while not signalled:
        time.sleep(0.001)
    qmp(sock_file, "stop")
    qmp(sock_file, "migrate", {"uri": "file:" + snapshot})
    while qmp(sock_file, "query-migrate")["return"].get("status") not in (
        "completed", "failed", "cancelled"):
        time.sleep(0.001)
    qmp(sock_file, "quit")
    proc.wait()
    data = open(snapshot, "rb").read()
    print("snapshot bytes={} sha256={}".format(len(data), hashlib.sha256(data).hexdigest()))


if __name__ == "__main__":
    main()
