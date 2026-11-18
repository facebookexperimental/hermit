#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Stamp, and check, the inputs shim/third-party/rust/BUCK was generated from.

usage: rust-deps-stamp.py write REPO_ROOT   record the inputs in shim/third-party/rust/BUCK.inputs
       rust-deps-stamp.py check REPO_ROOT   exit 0 when the recorded inputs are the checkout's
       rust-deps-stamp.py -h | --help       print this help

bootstrap/regenerate-rust-deps writes the stamp once it has generated (or restored)
the rules. ci/buck-e2e/validate-node checks it before a Buck-only run
(scripts/validate.rs --buck-s0), which uses the rules it finds, because generating
them runs cargo metadata: rules generated for other inputs would build another
commit's crates while every row records this commit.

The inputs, one definition for both modes: Cargo.lock; every tracked Cargo.toml;
every tracked file under shim/third-party/rust (reindeer.toml, the fixups,
shared-cell-aliases.txt, top/); every tracked file under bootstrap/ but its test-*
scripts: the generator, the helpers it sources, and the Reindeer it builds and runs
(bootstrap/reindeer, its launcher bootstrap/run-pinned-tool, the pin in
bootstrap/tool-pins.sh, and any local patch under bootstrap/patches/, tracked or
not: run-pinned-tool applies an untracked patch too, and Git's ignore rules keep
out only its leftovers, *.orig and *.rej); what picks
the cargo that Reindeer's `cargo metadata` runs (rust-toolchain.toml and the root
.cargo configuration); and every submodule's pinned commit (Reverie's crates are
path dependencies). Files are hashed as they are in the working tree, so an
uncommitted edit counts; a tracked input missing from the working tree is recorded
as missing, so a check names it and a write still stamps.

Exit status: 0 for a written stamp, one that matches, or the help; 1 for a check that
does not match or finds no stamp (the differences are named); 2 for a usage error.
"""

import hashlib
import subprocess
import sys
from pathlib import Path

STAMP = "shim/third-party/rust/BUCK.inputs"
HEADER = "# rust-deps-stamp v1: the inputs shim/third-party/rust/BUCK was generated from"
THIRD_PARTY = "shim/third-party/rust/"
BOOTSTRAP = "bootstrap/"
TOOLCHAIN = {"rust-toolchain.toml", "rust-toolchain", ".cargo/config.toml", ".cargo/config"}
PATCHES = "bootstrap/patches"
GITLINK = "160000"


def is_input(path):
    return (
        path == "Cargo.lock"
        or path == "Cargo.toml"
        or path.endswith("/Cargo.toml")
        or path.startswith(THIRD_PARTY)
        or (path.startswith(BOOTSTRAP) and not path.rsplit("/", 1)[1].startswith("test-"))
        or path in TOOLCHAIN
    )


def inputs(root):
    """The stamp's lines for the checkout at `root`, sorted by path."""
    listing = subprocess.run(
        ["git", "-C", str(root), "ls-files", "-s", "-z"],
        check=True,
        capture_output=True,
    ).stdout
    untracked_patches = subprocess.run(
        ["git", "-C", str(root), "ls-files", "-z", "--others", "--exclude-standard", "--", PATCHES],
        check=True,
        capture_output=True,
    ).stdout
    lines = []
    for path in untracked_patches.split(b"\0"):
        if path and is_input(path.decode()):
            path = path.decode()
            digest = hashlib.sha256((root / path).read_bytes()).hexdigest()
            lines.append((path, f"{digest}  {path}"))
    for entry in listing.split(b"\0"):
        if not entry:
            continue
        meta, path = entry.split(b"\t", 1)
        mode, object_id, _stage = meta.decode().split()
        path = path.decode()
        if mode == GITLINK:
            lines.append((path, f"gitlink {object_id}  {path}"))
        elif is_input(path):
            try:
                digest = hashlib.sha256((root / path).read_bytes()).hexdigest()
            except FileNotFoundError:
                digest = "missing"  # tracked, but deleted from the working tree
            lines.append((path, f"{digest}  {path}"))
    return [line for _, line in sorted(lines)]


def main(argv):
    if len(argv) == 2 and argv[1] in ("-h", "--help"):
        print(__doc__.strip())
        return 0
    if len(argv) != 3 or argv[1] not in ("write", "check"):
        print(__doc__.split("\n\n")[1], file=sys.stderr)
        return 2
    root = Path(argv[2]).resolve()
    current = inputs(root)
    stamp = root / STAMP
    if argv[1] == "write":
        stamp.write_text("\n".join([HEADER, *current]) + "\n")
        print(f"rust-deps-stamp: recorded {len(current)} inputs in {STAMP}")
        return 0
    if not stamp.is_file():
        print(
            f"rust-deps-stamp: {STAMP} is missing: the rules were generated before "
            "bootstrap/regenerate-rust-deps stamped them, or not at all",
            file=sys.stderr,
        )
        return 1
    recorded = [line for line in stamp.read_text().splitlines() if not line.startswith("#")]
    if recorded == current:
        return 0
    def by_path(lines):
        return {line.rsplit("  ", 1)[1]: line for line in lines}

    was, now = by_path(recorded), by_path(current)
    changed = sorted(p for p in was.keys() & now.keys() if was[p] != now[p])
    added = sorted(now.keys() - was.keys())
    removed = sorted(was.keys() - now.keys())
    print(
        f"rust-deps-stamp: the rules were generated from other inputs: {len(changed)} "
        f"changed, {len(added)} added, {len(removed)} removed",
        file=sys.stderr,
    )
    for kind, paths in (("changed", changed), ("added", added), ("removed", removed)):
        for path in paths[:10]:
            print(f"  {kind}: {path}", file=sys.stderr)
        if len(paths) > 10:
            print(f"  ... and {len(paths) - 10} more {kind}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
