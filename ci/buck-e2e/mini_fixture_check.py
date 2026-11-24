#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Refuse a mini-image cell whose prepared fixtures the image cannot run.

The mini images keep their libraries under /nix/store, where the CentOS
fixture_glibc_check.sh (which reads usr/lib64 from a rootfs tar) cannot
see them. This is the same check for a Nix docker archive: read the
layers in manifest order without extracting the rootfs, collect every
versioned symbol and library name the image offers, then require each
dynamic fixture's undefined versioned symbols and NEEDED libraries to
be among them. The verdict TSV has the v2 checker's shape ("schema 1",
a status line, then one "missing PATH ITEM" line per unmet need).
Exit 0 writes the checked verdict (NEEDS rows included; like v2, unmet
needs are findings, not a failure). Exit 2 is a usage error or a refusal
(an input the check cannot read); a refusal overwrites the verdict with
"status unchecked REASON", so a stale checked verdict never survives it.
"""

from __future__ import annotations

import argparse
import io
import json
import pathlib
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile

VERSIONED = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*_[0-9][0-9.]*$|GLIBC_PRIVATE")
OBJDUMP_TIMEOUT = 30


class Refusal(Exception):
    """An input the check cannot read: refuse by name (unchecked verdict)."""


def offered_symbols(objdump: str, library: pathlib.Path, display: str) -> set[str]:
    # A .so that is not an ELF (a linker script) legitimately offers no
    # symbols. An ELF library objdump cannot read must never read as "no
    # symbols": that would fabricate missing rows under a checked verdict.
    # Refuse by name instead, with the error and objdump's stderr kept.
    with open(library, "rb") as handle:
        if handle.read(4) != b"\x7fELF":
            return set()
    try:
        out = subprocess.run(
            [objdump, "-T", str(library)], text=True,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            timeout=OBJDUMP_TIMEOUT)
    except (OSError, subprocess.TimeoutExpired) as error:
        raise Refusal(f"cannot read library {display}: {error}") from error
    if out.returncode != 0:
        detail = out.stderr.strip() or f"exit {out.returncode}"
        raise Refusal(f"{objdump} cannot read library {display}: {detail}")
    symbols: set[str] = set()
    for line in out.stdout.splitlines():
        if "*UND*" in line:
            continue
        fields = line.split()
        if len(fields) >= 2:
            version = fields[-2].strip("()")
            if VERSIONED.match(version):
                symbols.add(f"{fields[-1]}@{version}")
    return symbols


def needs(objdump: str, elf: pathlib.Path) -> tuple[set[str], set[str], bool]:
    try:
        dynamic = subprocess.run(
            [objdump, "-T", str(elf)], text=True,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30)
        private = subprocess.run(
            [objdump, "-p", str(elf)], text=True,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30)
    except (OSError, subprocess.TimeoutExpired) as error:
        raise Refusal(f"cannot run {objdump} on {elf}: {error}") from error
    # Like v2 (fixture_glibc_check.sh:135-138): a fixture objdump -T cannot
    # read as a dynamic object needs nothing. Static programs and .o files
    # land here - objdump prints "not a dynamic object" and exits 1 - and
    # the :fixtures tree holds -static fixtures, so refusing would refuse
    # every wired run.
    if dynamic.returncode != 0 or not dynamic.stdout.strip():
        return set(), set(), True
    if private.returncode != 0:
        raise Refusal(f"{objdump} cannot read fixture {elf}")
    symbols: set[str] = set()
    for line in dynamic.stdout.splitlines():
        if "*UND*" not in line:
            continue
        fields = line.split()
        if len(fields) >= 2:
            version = fields[-2].strip("()")
            if VERSIONED.match(version):
                symbols.add(f"{fields[-1]}@{version}")
    libraries = {line.split()[1] for line in private.stdout.splitlines()
                 if line.split()[:1] == ["NEEDED"] and len(line.split()) >= 2}
    return symbols, libraries, False


def image_offer(archive: pathlib.Path, objdump: str,
                scratch: pathlib.Path) -> tuple[set[str], set[str]]:
    """Versioned symbols and library names one docker archive offers.

    Only regular *.so* members are materialized (one at a time, under a
    scratch name), so the absolute symlinks a Nix layer is full of are
    never extracted.
    """
    offered: set[str] = set()
    names: set[str] = set()
    with tarfile.open(archive) as outer:
        manifest_member = outer.extractfile("manifest.json")
        if manifest_member is None:
            raise Refusal("docker archive has no manifest.json")
        manifest = json.loads(manifest_member.read())
        if not isinstance(manifest, list) or not manifest:
            raise Refusal("docker archive manifest.json is empty")
        layers = manifest[0].get("Layers") if isinstance(manifest[0], dict) else None
        if not layers:
            raise Refusal("docker archive manifest.json names no layers")
        for layer_name in layers:
            member = outer.extractfile(layer_name)
            if member is None:
                raise Refusal(f"docker archive has no layer {layer_name}")
            with tarfile.open(fileobj=member) as layer:
                for item in layer.getmembers():
                    base = pathlib.PurePosixPath(item.name).name
                    if ".so" not in base:
                        continue
                    names.add(base)
                    if not item.isfile():
                        continue
                    source = layer.extractfile(item)
                    if source is None:
                        continue
                    scratch_path = scratch / "library"
                    with open(scratch_path, "wb") as handle:
                        while chunk := source.read(1024 * 1024):
                            handle.write(chunk)
                    offered |= offered_symbols(objdump, scratch_path, item.name)
    return offered, names


_FAKE_OBJDUMP = """import pathlib, sys
data = pathlib.Path(sys.argv[-1]).read_bytes()
if b"BROKENLIB" in data:
    print("objdump: library: file format not recognized", file=sys.stderr)
    sys.exit(1)
elif b"STATICED" in data:
    if "-p" in sys.argv:
        print("static: file format elf64-x86-64")
    else:
        print("objdump: static: not a dynamic object", file=sys.stderr)
        sys.exit(1)
elif b"FIXTURE" in data:
    if "-p" in sys.argv:
        print("  NEEDED               libfoo.so.1")
        print("  NEEDED               libmissing.so.9")
    else:
        print("0000 w DF *UND* 0000 (GLIBC_2.34) puts")
        print("0000 w DF *UND* 0000 (GLIBC_9.99) nosuch")
elif b"GLIBCDEF" in data:
    print("0000 g DF .text 0000 (GLIBC_2.34) puts")
else:
    print("0000 g DF .text 0000 plain")
"""


def _write_image(path: pathlib.Path, libc: bytes,
                 libfoo: bytes = b"\x7fELFGLIBCDEF") -> None:
    layer_bytes = io.BytesIO()
    with tarfile.open(fileobj=layer_bytes, mode="w") as layer:
        for name, data in (("nix/store/libc/libc.so.6", libc),
                           ("nix/store/libfoo/libfoo.so.1", libfoo)):
            info = tarfile.TarInfo(name)
            info.size = len(data)
            layer.addfile(info, io.BytesIO(data))
    with tarfile.open(path, "w") as outer:
        manifest = json.dumps([{"Layers": ["layer.tar"]}]).encode()
        info = tarfile.TarInfo("manifest.json")
        info.size = len(manifest)
        outer.addfile(info, io.BytesIO(manifest))
        data = layer_bytes.getvalue()
        info = tarfile.TarInfo("layer.tar")
        info.size = len(data)
        outer.addfile(info, io.BytesIO(data))


def run_self_test() -> int:
    with tempfile.TemporaryDirectory(prefix="mini-fixture-self-test-") as tmp:
        root = pathlib.Path(tmp)
        objdump = root / "objdump"
        objdump.write_text(f"#!{sys.executable}\n" + _FAKE_OBJDUMP)
        objdump.chmod(0o755)
        fixtures = root / "fixtures"
        fixtures.mkdir()
        (fixtures / "guest").write_bytes(b"\x7fELFFIXTURE")
        (fixtures / "static-guest").write_bytes(b"\x7fELFSTATICED")
        image = root / "image.tar"
        _write_image(image, b"\x7fELFGLIBCDEF")
        verdict = root / "verdict.tsv"
        code = main(["--image-tar", str(image), "--fixtures", str(fixtures),
                     "--verdict", str(verdict), "--objdump", str(objdump)])
        assert code == 0, code
        text = verdict.read_text()
        assert "status\tchecked" in text, text
        assert "nosuch@GLIBC_9.99" in text, text
        assert "libmissing.so.9 (library)" in text, text
        assert "static-guest" not in text, text

        empty = root / "empty.tar"
        with tarfile.open(empty, "w") as outer:
            data = b"[]"
            info = tarfile.TarInfo("manifest.json")
            info.size = len(data)
            outer.addfile(info, io.BytesIO(data))
        verdict2 = root / "verdict2.tsv"
        assert main(["--image-tar", str(empty), "--fixtures", str(fixtures),
                     "--verdict", str(verdict2), "--objdump", str(objdump)]) == 2
        assert "status\tunchecked\t" in verdict2.read_text(), verdict2.read_text()
        # A refusal overwrites a stale checked verdict at the same path.
        assert main(["--image-tar", str(empty), "--fixtures", str(fixtures),
                     "--verdict", str(verdict), "--objdump", str(objdump)]) == 2
        stale = verdict.read_text()
        assert "status\tunchecked\t" in stale, stale
        assert "status\tchecked" not in stale, stale

        # Planted: an ELF library objdump cannot read must FAIL BY NAME
        # (unchecked verdict naming it, stderr kept), never a checked
        # verdict over fabricated missing rows.
        badlib = root / "badlib.tar"
        _write_image(badlib, b"\x7fELFGLIBCDEF", b"\x7fELFBROKENLIB")
        verdict6 = root / "v6"
        assert main(["--image-tar", str(badlib), "--fixtures", str(fixtures),
                     "--verdict", str(verdict6), "--objdump", str(objdump)]) == 2
        bad_text = verdict6.read_text()
        assert "status\tunchecked\t" in bad_text, bad_text
        assert "libfoo.so.1" in bad_text, bad_text
        assert "file format not recognized" in bad_text, bad_text
        assert "status\tchecked" not in bad_text, bad_text

        nog = root / "noglibc.tar"
        _write_image(nog, b"\x7fELFPLAINX", b"\x7fELFPLAIN")
        assert main(["--image-tar", str(nog), "--fixtures", str(fixtures),
                     "--verdict", str(root / "v3"), "--objdump", str(objdump)]) == 2
        assert main(["--image-tar", str(image), "--fixtures", str(root / "absent"),
                     "--verdict", str(root / "v4"), "--objdump", str(objdump)]) == 2
        assert main(["--image-tar", str(image), "--fixtures", str(fixtures),
                     "--verdict", str(root / "v5"),
                     "--objdump", str(root / "no-objdump")]) == 2
    print("mini_fixture_check self-test: ok", file=sys.stderr)
    return 0


def refuse(verdict: pathlib.Path, reason: str) -> int:
    """Exit-2 refusal: overwrite any stale verdict as unchecked, like v2."""
    clean = reason.replace("\t", " ").replace("\n", " ")
    print(f"mini_fixture_check: {reason}", file=sys.stderr)
    try:
        verdict.write_text(f"schema\t1\nstatus\tunchecked\t{clean}\n")
    except OSError as error:
        print(f"mini_fixture_check: cannot write verdict: {error}", file=sys.stderr)
    return 2


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--image-tar", type=pathlib.Path)
    parser.add_argument("--fixtures", type=pathlib.Path)
    parser.add_argument("--verdict", type=pathlib.Path)
    parser.add_argument("--objdump", default="objdump")
    args = parser.parse_args(argv)
    if args.self_test:
        return run_self_test()
    missing = [name for name in ("image_tar", "fixtures", "verdict")
               if getattr(args, name) is None]
    if missing:
        parser.error("--image-tar, --fixtures and --verdict are required")

    if not args.image_tar.is_file():
        return refuse(args.verdict, f"no image tar at {args.image_tar}")
    if not args.fixtures.is_dir():
        return refuse(args.verdict, f"no fixtures directory at {args.fixtures}")
    objdump = pathlib.Path(args.objdump)
    if "/" in args.objdump:
        if not (objdump.is_file() and objdump.stat().st_mode & 0o111):
            return refuse(args.verdict, f"no executable objdump at {args.objdump}")
    elif shutil.which(args.objdump) is None:
        return refuse(args.verdict, f"no {args.objdump} on PATH")

    missing: list[tuple[str, str]] = []
    checked = static_count = 0
    try:
        with tempfile.TemporaryDirectory(prefix="mini-fixture-check-") as tmp:
            offered, names = image_offer(
                args.image_tar, args.objdump, pathlib.Path(tmp))
            if "libc.so.6" not in names or not any(
                    symbol.rsplit("@", 1)[-1].startswith("GLIBC_")
                    for symbol in offered):
                raise Refusal("image has no libc.so.6 defining GLIBC_ versions")
            for fixture in sorted(args.fixtures.rglob("*")):
                if not fixture.is_file():
                    continue
                try:
                    if fixture.read_bytes()[:4] != b"\x7fELF":
                        continue
                except OSError:
                    continue
                checked += 1
                symbols, libs, is_static = needs(args.objdump, fixture)
                if is_static:
                    static_count += 1
                    continue
                rel = str(fixture.relative_to(args.fixtures))
                for item in sorted(symbols - offered):
                    missing.append((rel, item))
                for item in sorted(libs - names):
                    missing.append((rel, f"{item} (library)"))
    except Refusal as error:
        return refuse(args.verdict, str(error))
    except (OSError, ValueError, KeyError, IndexError, TypeError,
            tarfile.TarError, json.JSONDecodeError) as error:
        return refuse(args.verdict, f"cannot read an input: {error}")

    with open(args.verdict, "w") as verdict:
        verdict.write("schema\t1\nstatus\tchecked\n")
        for rel, item in missing:
            verdict.write(f"missing\t{rel}\t{item}\n")
    print(
        f"mini_fixture_check: {checked} ELF fixture(s), {static_count} static, "
        f"{len(missing)} missing item(s); verdict at {args.verdict}",
        file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
