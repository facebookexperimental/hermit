#!/usr/bin/env python3
"""Refresh ci/buck-e2e/stage's kept copy of a checkout from the files Git tracks.

usage: refresh_copy.py ROOT DEST RECORD JOURNAL

DEST is the stage's fixed source copy, kept between stages with its build output
(see stage, "REPRODUCIBLE STAGING"). It is refreshed to hold exactly the files Git
tracks in ROOT: the checkout's tracked files, each initialized submodule's
(recursively) and the .git links of the checkout and its submodules, which
hermit-install's build script reads through `git -C`. Nothing untracked in ROOT
is read: the validation's other nodes build into it while the stage runs.

RECORD names, NUL-separated, every path the last completed refresh copied. A path
it names that Git no longer tracks is removed; a file the stage's own builds
wrote into DEST stays, unless it stands where a tracked path now needs to be.

The refresh is journaled. JOURNAL is written, naming every path this refresh may
change, before DEST is touched; RECORD is replaced atomically, and JOURNAL removed,
only once DEST matches ROOT. A stage that finds JOURNAL (a refresh that did not
finish: interrupted, killed, or failed in rsync) wipes DEST and builds cold, so a
path an unfinished refresh copied can never outlive it unrecorded.

Order, so a path that changes between a file and a directory (foo tracked, then
foo/bar, or the reverse; a checkout's .git directory, then a .git file) is never
deleted after its new form is copied, and rsync never has to replace a directory
with a file:
  1. remove each recorded path Git no longer tracks;
  2. for each tracked path, remove what stands in its way in DEST: a file or
     symbolic link where one of its ancestors must be a directory, and a
     directory where it is a file or link (or the reverse, for a .git directory);
  3. rsync the tracked paths, by content (--checksum), never preserving times.

Exit status: 0 refreshed; 2 the tracked files could not be listed (any git
failure at any submodule depth stops the listing); otherwise rsync's status.
"""
from __future__ import annotations

import os
import shutil
import subprocess
import sys
from pathlib import Path


class ListingError(Exception):
    pass


def list_tracked(root: Path) -> list[str]:
    """The paths to copy, relative to ROOT: '.git', then every tracked file, with
    each initialized submodule's '.git' link and its own tracked files in place of
    its gitlink. A git failure anywhere raises ListingError."""

    paths = [".git"]

    def walk(directory: Path, prefix: str) -> None:
        listed = subprocess.run(
            ["git", "-C", str(directory), "ls-files", "-z", "--stage"],
            capture_output=True,
            check=False,
        )
        if listed.returncode != 0:
            raise ListingError(
                f"git ls-files in {directory} failed (exit {listed.returncode}): "
                f"{listed.stderr.decode(errors='replace').strip()}"
            )
        for entry in listed.stdout.split(b"\0"):
            if not entry:
                continue
            meta, _, name = entry.partition(b"\t")
            path = os.fsdecode(name)
            if meta.startswith(b"160000 "):
                if os.path.lexists(directory / path / ".git"):
                    paths.append(f"{prefix}{path}/.git")
                    walk(directory / path, f"{prefix}{path}/")
            else:
                paths.append(f"{prefix}{path}")

    walk(root, "")
    return paths


def read_record(record: Path) -> list[str]:
    try:
        data = record.read_bytes()
    except FileNotFoundError:
        return []
    return [os.fsdecode(p) for p in data.split(b"\0") if p]


def write_paths(path: Path, paths: list[str]) -> None:
    """Write PATHS NUL-separated to PATH atomically, and make it durable."""

    temporary = path.with_name(path.name + ".tmp")
    with open(temporary, "wb") as handle:
        handle.write(b"".join(os.fsencode(p) + b"\0" for p in paths))
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(temporary, path)


def is_real_dir(path: Path) -> bool:
    return path.is_dir() and not path.is_symlink()


def remove(path: Path) -> None:
    """Remove PATH whatever it is: a directory with its contents, or a file or link."""

    if is_real_dir(path):
        shutil.rmtree(path)
    elif os.path.lexists(path):
        path.unlink()


def reconcile(root: Path, dest: Path, recorded: list[str], wanted: list[str]) -> None:
    """Steps 1 and 2 of the module docstring: clear DEST of obsolete recorded paths
    and of anything standing where a tracked path must go."""

    keep = set(wanted)
    # Deepest first, so a removed directory's recorded children go before it.
    for path in sorted(set(recorded) - keep, key=lambda p: p.count("/"), reverse=True):
        remove(dest / path)
    for path in wanted:
        parts = path.split("/")
        for depth in range(1, len(parts)):
            ancestor = dest.joinpath(*parts[:depth])
            if os.path.lexists(ancestor) and not is_real_dir(ancestor):
                ancestor.unlink()
        target = dest / path
        if not os.path.lexists(target):
            continue
        if is_real_dir(root / path) != is_real_dir(target):
            remove(target)


def refresh(root: Path, dest: Path, record: Path, journal: Path) -> int:
    try:
        wanted = list_tracked(root)
    except ListingError as error:
        print(f"ci/buck-e2e/stage: cannot list the files Git tracks in {root}: {error}", file=sys.stderr)
        return 2
    recorded = read_record(record)
    # Every path this refresh may change, durable before DEST is touched.
    write_paths(journal, sorted(set(recorded) | set(wanted)))
    reconcile(root, dest, recorded, wanted)
    staged = record.with_name(record.name + ".wanted")
    write_paths(staged, wanted)
    copied = subprocess.run(
        ["rsync", "-rlp", "--checksum", "--from0", f"--files-from={staged}", f"{root}/", f"{dest}/"],
        check=False,
    )
    if copied.returncode != 0:
        return copied.returncode
    os.replace(staged, record)
    journal.unlink()
    return 0


def main(argv: list[str]) -> int:
    if len(argv) != 4:
        print(__doc__.splitlines()[2], file=sys.stderr)
        return 2
    root, dest, record, journal = (Path(a) for a in argv)
    return refresh(root, dest, record, journal)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
