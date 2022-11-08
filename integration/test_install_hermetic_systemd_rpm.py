#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

import os
import subprocess
import unittest
from datetime import datetime, timedelta, timezone

from dateutil.parser import parse

# detcore epoch (utc)
EPOCH = parse("1999-12-31T23:59:59Z")


def file_mtime_utc(file: str):
    return datetime.fromtimestamp(os.stat(file).st_mtime, timezone.utc)


class InstallHermeticSystemdRpmTestCase(unittest.TestCase):
    def test_contents(self):
        self.assertFalse(os.path.exists("/antlir-custom-rpm-gpg-keys"))
        self.assertTrue(os.path.exists("/usr/sbin/init"))

        start_time = EPOCH
        end_time = start_time + timedelta(days=1)

        mtime = file_mtime_utc("/usr/sbin/init")

        self.assertGreaterEqual(mtime, start_time)
        self.assertLessEqual(mtime, end_time)

    def test_signing(self):
        info = subprocess.check_output(
            ["rpm", "-qip", "/antlir/systemd.rpm"], text=True
        )

        keys = subprocess.check_output(
            ["rpm", "-q", "gpg-pubkey", "--queryformat", "%{SUMMARY}"],
            text=True,
        )

        self.assertIn("Hermetic Test Key <key@example.com>", keys)

        self.assertIn("Key ID d1e0a52c340ffdf3", info)
