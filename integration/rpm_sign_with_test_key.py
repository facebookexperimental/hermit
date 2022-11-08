#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

import importlib.resources
import subprocess
import sys
import tempfile


class GnupgImportFailure(Exception):
    pass


def gpg_import_private_key(gnupg_home: str, pkey: str) -> None:
    # gpg launches gpg-agent which is a daemon, the daemon may
    # never exit which confuses tracer and causes the tracer
    # to never exit as well. As a work around we use timeout here
    # and force gpg to exit after timeout value.
    try:
        proc = subprocess.run(
            ["gpg", "--ignore-time-conflict", "--import", pkey],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env={"GNUPGHOME": gnupg_home},
            timeout=30,
            check=True,
        )
    except subprocess.TimeoutExpired:
        subprocess.run(
            ["gpgconf", "--kill", "gpg-agent"],
            env={"GNUPGHOME": gnupg_home},
        )
    if proc.returncode == 0 or "secret keys imported: 1" in proc.stderr:
        return
    else:
        raise GnupgImportFailure(proc.stderr)


def sign_with_test_key(rpm: str) -> None:
    # Since we're using a test key, create a temporary directory to house the
    # gpg configuration and trust data so as not to pollute the user's host
    # data.
    with tempfile.TemporaryDirectory() as gnupg_home, importlib.resources.path(
        __package__, "gpg-test-signing-key"
    ) as signing_key:
        gpg_import_private_key(gnupg_home, signing_key)
        subprocess.run(
            [
                "rpmsign",
                "--addsign",
                "--define",
                "_gpg_name Test Key",
                "--define",
                "_gpg_digest_algo sha256",
                "--define",
                "__gpg_sign_cmd %{__gpg} gpg --batch --no-verbose --ignore-time-conflict --no-armor --no-secmem-warning -u '%{_gpg_name}' -sbo %{__signature_filename} %{__plaintext_filename}",
                rpm,
            ],
            env={"GNUPGHOME": gnupg_home},
            check=True,
        )


if __name__ == "__main__":
    sign_with_test_key(sys.argv[1])
