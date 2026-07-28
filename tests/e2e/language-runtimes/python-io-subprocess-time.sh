#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

case ${1:-} in
    --prepare)
        command -v python3 >/dev/null
        test -x /usr/bin/tr
        ;;
    --run)
        exec python3 - <<'PYTHON'
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time

root = Path(os.environ.get("E2E_TMPDIR", "/tmp")) / "hermit-python-interpreter-batch"
shutil.rmtree(root, ignore_errors=True)
root.mkdir(parents=True)

payload = "alpha\nbeta\ngamma\n"
input_path = root / "input.txt"
output_path = root / "output.txt"
input_path.write_text(payload, encoding="utf-8")

child = subprocess.run(
    ["/usr/bin/tr", "a-z", "A-Z"],
    input=input_path.read_text(encoding="utf-8"),
    text=True,
    capture_output=True,
    check=True,
)
output_path.write_text(child.stdout, encoding="utf-8")
observed = output_path.read_bytes()

print(
    "PYTHON "
    f"version={sys.version.split()[0]} "
    f"bytes={len(observed)} "
    f"sha256={hashlib.sha256(observed).hexdigest()} "
    f"child={child.returncode} "
    f"wall_ns={time.time_ns()} "
    f"monotonic_ns={time.monotonic_ns()}"
)
PYTHON
        ;;
    *)
        echo "usage: $0 --prepare|--run" >&2
        exit 2
        ;;
esac
