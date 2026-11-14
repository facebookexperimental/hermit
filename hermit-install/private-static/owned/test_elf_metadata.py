# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Synthetic ELF header controls for PT_LOAD alignment; no producer runs."""
import importlib.util
from pathlib import Path
import struct
import tempfile
import unittest

D = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location(
    'test_elf_metadata_module', D / 'assets/elf_metadata.py')
headers = importlib.util.module_from_spec(spec)
spec.loader.exec_module(headers)

LOW = 0x6000E1000000


class PtLoadAlignment(unittest.TestCase):
    def image(self, align, offset=0x1573000, vaddr=0x6000E2573000):
        raw = bytearray(64 + 2 * 56)
        raw[:7] = b'\x7fELF\x02\x01\x01'
        struct.pack_into('<HHI', raw, 16, 2, 62, 1)
        struct.pack_into('<QQ', raw, 24, LOW + 0x1000, 64)
        struct.pack_into('<HH', raw, 54, 56, 2)
        segments = [
            (1, 5, 0x1000, LOW + 0x1000, LOW + 0x1000, 0x1000, 0x1000, 4096),
            (1, 6, offset, vaddr, vaddr, 0, 0x1000, align),
        ]
        for index, segment in enumerate(segments):
            struct.pack_into('<IIQQQQQQ', raw, 64 + index * 56, *segment)
        directory = tempfile.TemporaryDirectory(prefix='elf-metadata-')
        self.addCleanup(directory.cleanup)
        path = Path(directory.name) / 'image.elf'
        with path.open('wb') as stream:
            stream.write(raw)
            stream.truncate(offset)
        return path

    def test_real_produced_rw_header_values_accepted(self):
        result = headers.elf_headers(self.image(0x10000))
        loads = [s for s in result['segments'] if s['type'] == 1]
        self.assertEqual(loads[1]['align'], 0x10000)
        self.assertEqual(loads[1]['vaddr'] % loads[1]['align'], 0x3000)
        self.assertEqual(loads[1]['offset'] % loads[1]['align'], 0x3000)

    def test_oversized_and_non_power_of_two_alignment_refused(self):
        for align in (131072, 12288):
            with self.subTest(align=align):
                with self.assertRaisesRegex(RuntimeError, 'unexpected PT_LOAD alignment'):
                    headers.elf_headers(self.image(align))

    def test_incongruent_alignment_refused(self):
        path = self.image(0x10000, vaddr=0x6000E2574000)
        with self.assertRaisesRegex(RuntimeError, 'unexpected PT_LOAD alignment'):
            headers.elf_headers(path)


if __name__ == '__main__':
    unittest.main()
