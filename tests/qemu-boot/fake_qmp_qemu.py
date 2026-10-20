#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""A stand-in for qemu-system-x86_64 that speaks just enough QMP for
signal_at_snapshot.py, so a test can make each step of a snapshot fail.

It serves the `-qmp unix:PATH,...` socket it is given. Environment:
  FAKE_QEMU_MIGRATION_STATUS  what query-migrate reports (default completed)
  FAKE_QEMU_ERROR_ON          a command answered with a QMP error instead
  FAKE_QEMU_EXIT              the status it exits with after quit (default 0)
  FAKE_QEMU_EMPTY_SNAPSHOT    if set, `migrate` writes an empty file
`migrate` writes the bytes FAKE-SNAPSHOT to its file: URI.
"""

import json
import os
import socket
import sys


def main():
    endpoint = sys.argv[sys.argv.index("-qmp") + 1]
    path = endpoint[len("unix:"):].split(",", 1)[0]
    status = os.environ.get("FAKE_QEMU_MIGRATION_STATUS", "completed")
    error_on = os.environ.get("FAKE_QEMU_ERROR_ON")
    exit_status = int(os.environ.get("FAKE_QEMU_EXIT", "0"))
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(path)
    listener.listen(1)
    connection, _ = listener.accept()
    stream = connection.makefile("rwb")
    stream.write(b'{"QMP": {"version": {}, "capabilities": []}}\n')
    stream.flush()
    for line in stream:
        command = json.loads(line)["execute"]
        if command == error_on:
            reply = {"error": {"class": "GenericError", "desc": "fake failure"}}
        elif command == "migrate":
            uri = json.loads(line)["arguments"]["uri"]
            with open(uri[len("file:"):], "wb") as snapshot:
                if not os.environ.get("FAKE_QEMU_EMPTY_SNAPSHOT"):
                    snapshot.write(b"FAKE-SNAPSHOT")
            reply = {"return": {}}
        elif command == "query-migrate":
            reply = {"return": {"status": status}}
        else:
            reply = {"return": {}}
        stream.write((json.dumps(reply) + "\n").encode())
        stream.flush()
        if command == "quit":
            sys.exit(exit_status)
    sys.exit(exit_status)


if __name__ == "__main__":
    main()
