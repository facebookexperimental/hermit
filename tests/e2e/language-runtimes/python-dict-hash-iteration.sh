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
        test -x /usr/bin/sort
        ;;
    --run)
        exec python3 - <<'PYTHON'
import hashlib
import os
import shutil
import subprocess
import sys
from pathlib import Path

# Python seeds its SipHash string-hash secret from the OS RNG
# (getrandom/AT_RANDOM), governed by PYTHONHASHSEED. That secret controls
# hash() of str/bytes and therefore the iteration order of sets and dicts
# built from string keys. Natively this varies from run to run; under Hermit
# --strict the underlying entropy is determinized, so every observation below
# must be bitwise reproducible.
words = ["alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta"]

# Set iteration order is hash-seed sensitive.
set_order = ",".join(set(words))
# frozenset hashing combines the per-element str hashes.
frozen = hash(frozenset(words))
# Raw builtin hashes of individual strings.
str_hashes = ",".join(str(hash(w)) for w in words)
# Dict built by inserting in hash-bucket order, then read back by iteration.
by_hash = {w: hash(w) & 0xFFFF for w in sorted(words, key=hash)}
dict_iter = ",".join(by_hash)

# File I/O: persist the hash-derived material and read it back as bytes.
root = Path(os.environ.get("E2E_TMPDIR", "/tmp")) / "hermit-python-dict-hash"
shutil.rmtree(root, ignore_errors=True)
root.mkdir(parents=True)
payload = f"{set_order}\n{dict_iter}\n{str_hashes}\n{frozen}\n"
seed_path = root / "seed.txt"
seed_path.write_text(payload, encoding="utf-8")
observed = seed_path.read_bytes()

# Subprocess: canonicalize the set-iteration material through a child process
# and capture its output.
child = subprocess.run(
    ["/usr/bin/sort"],
    input=set_order.replace(",", "\n") + "\n",
    text=True,
    capture_output=True,
    check=True,
)
sorted_words = child.stdout.strip().replace("\n", ",")

print(
    "PYDICTHASH "
    f"version={sys.version.split()[0]} "
    f"set_order={set_order} "
    f"dict_iter={dict_iter} "
    f"frozen={frozen} "
    f"sorted={sorted_words} "
    f"child={child.returncode} "
    f"bytes={len(observed)} "
    f"sha256={hashlib.sha256(observed).hexdigest()}"
)
PYTHON
        ;;
    *)
        echo "usage: $0 --prepare|--run" >&2
        exit 2
        ;;
esac
