#!/usr/bin/env python3
"""Install/check one emitted core engine and both source-owned ABI headers.

This consumes Cargo's recorded output; it never compiles, installs a toolchain,
discovers a backend, or grants runtime/private-image qualification. The caller
owns the normal build budget, lease, exit status and complete output records.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import struct
import subprocess
import sys
import tomllib

SCHEMA = 2
ENGINE = "libhermit_inguest_engine.so"
HEADER = "hermit-engine-v1.h"
NATIVE_HEADER = "hermit-engine-native-v1.h"
MANIFEST = "engine-artifact.json"
MAX_RECORD = 64 * 1024 * 1024
MAX_FILE = 2 * 1024 * 1024 * 1024
FORBIDDEN_PACKAGES = {
    "detcore-dbt", "detcore-sabre", "hermit-detcore-dbt",
    "hermit-detcore-sabre", "reverie-dbt", "reverie-sabre",
    "reverie-e9patch", "hermit-dynamorio", "reverie-dynamorio",
}


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ValueError(message)


def revision(value: str) -> str:
    require(re.fullmatch(r"[0-9a-f]{40}", value) is not None,
            "a full lowercase Git revision is required")
    return value


def identity(info: os.stat_result) -> tuple[int, ...]:
    return (info.st_dev, info.st_ino, info.st_size, info.st_mode,
            info.st_mtime_ns, info.st_ctime_ns)


def open_regular(path: Path):
    before = path.lstat()
    require(stat.S_ISREG(before.st_mode), f"not a real regular file: {path}")
    require(0 < before.st_size <= MAX_FILE, f"invalid file length: {path}")
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    file = os.fdopen(descriptor, "rb")
    try:
        require(identity(os.fstat(descriptor)) == identity(before),
                f"file changed while opening: {path}")
    except BaseException:
        file.close()
        raise
    return file, before


def recheck(path: Path, file, before: os.stat_result) -> None:
    require(identity(os.fstat(file.fileno())) == identity(before)
            and identity(path.lstat()) == identity(before),
            f"held or visible file identity changed: {path}")


def digest(path: Path) -> dict:
    file, before = open_regular(path)
    with file:
        checksum = hashlib.sha256()
        while block := file.read(65536):
            checksum.update(block)
        recheck(path, file, before)
    return {"file": path.name, "bytes": before.st_size,
            "sha256": checksum.hexdigest()}


def read_record(path: Path) -> bytes:
    file, before = open_regular(path)
    with file:
        require(before.st_size <= MAX_RECORD, f"record exceeds byte bound: {path}")
        raw = file.read(MAX_RECORD + 1)
        require(len(raw) == before.st_size, f"record changed length: {path}")
        recheck(path, file, before)
    return raw


def cargo_events(path: Path) -> list[dict]:
    return parse_cargo_events(read_record(path))


def parse_cargo_events(raw: bytes) -> list[dict]:
    records = [json.loads(line) for line in raw.splitlines()]
    require(all(isinstance(row, dict) for row in records), "non-object Cargo event")
    finished = [row for row in records if row.get("reason") == "build-finished"]
    require(len(finished) == 1 and finished[0].get("success") is True
            and records[-1] == finished[0],
            "Cargo events do not end in one successful build-finished record")
    return records


def git(root: Path, *arguments: str) -> str:
    command = ["git", "-C", str(root), *arguments]
    result = subprocess.run(command, capture_output=True, check=False, timeout=30)
    require(result.returncode == 0, f"Git source check refused: {arguments}")
    return result.stdout.decode().strip()


def committed_source(root: Path, expected: str) -> str:
    require(git(root, "rev-parse", "HEAD") == revision(expected),
            f"source HEAD differs from requested revision: {root}")
    # Cargo reads the tracked worktree, so compare those bytes to HEAD. This
    # deliberately makes no separate assertion about the index's contents.
    git(root, "diff", "--quiet", "HEAD", "--")
    return git(root, "rev-parse", "HEAD^{tree}")


def one(rows: list, message: str):
    require(len(rows) == 1, message)
    return rows[0]


def select_engine(records: list[dict], metadata: dict, root: Path) -> tuple[dict, dict]:
    package = one([p for p in metadata["packages"]
                   if p["name"] == "hermit-inguest-engine"
                   and Path(p["manifest_path"]).resolve() == root / "inguest-engine/Cargo.toml"],
                  "metadata must identify the exact local engine package")
    unit = one([row for row in records if row.get("reason") == "compiler-artifact"
                and row.get("package_id") == package["id"]
                and row.get("target", {}).get("name") == "hermit_inguest_engine"
                and row["target"].get("crate_types") == ["cdylib"]],
               "Cargo must emit exactly one concrete engine cdylib unit")
    require(not unit["profile"].get("test"), "a test unit is not the engine DSO")
    require(set(unit["features"]) <= {"default", "dispatch-observer", "native-first-instruction-observer"},
            "unexpected engine feature selection")
    return package, unit


def elf(path: Path, shared: bool) -> None:
    file, before = open_regular(path)
    with file:
        raw = file.read(64)
        recheck(path, file, before)
    require(len(raw) == 64 and raw[:7] == b"\x7fELF\x02\x01\x01",
            f"not a little-endian ELF64 object: {path}")
    kind, machine = struct.unpack_from("<HH", raw, 16)
    require(machine == 62 and (kind == 3 if shared else kind in (2, 3)),
            f"wrong ELF architecture/type: {path}")
    require(before.st_mode & 0o111 != 0, f"artifact is not executable: {path}")


def copy_exclusive(source: Path, destination: Path, mode: int) -> dict:
    file, before = open_regular(source)
    with file:
        fd = os.open(destination, os.O_WRONLY | os.O_CREAT | os.O_EXCL
                     | os.O_NOFOLLOW | os.O_CLOEXEC, mode)
        with os.fdopen(fd, "wb") as output:
            checksum = hashlib.sha256()
            total = 0
            while block := file.read(65536):
                output.write(block)
                checksum.update(block)
                total += len(block)
            output.flush()
            os.fsync(output.fileno())
        recheck(source, file, before)
        require(total == before.st_size, "source length changed during installation")
    result = {"file": destination.name, "bytes": total, "sha256": checksum.hexdigest()}
    require(digest(destination) == result, "installed artifact differs from held source")
    return result


def write_new(path: Path, record: dict) -> None:
    with path.open("x") as output:
        json.dump(record, output, indent=2, sort_keys=True)
        output.write("\n")
        output.flush()
        os.fsync(output.fileno())


def install(args) -> None:
    root = Path(__file__).resolve().parent.parent
    h = revision(args.hermit_revision)
    r = revision(args.reverie_revision)
    tree = committed_source(root, h)
    for name in ("Cargo.toml", "artifact.py", "include/" + HEADER,
                 "include/" + NATIVE_HEADER):
        git(root, "cat-file", "-e", "HEAD:inguest-engine/" + name)
    require(not git(root, "ls-files", "--others", "--exclude-standard", "--", "inguest-engine"),
            "uncommitted engine package sources cannot be labelled with HEAD")
    require(git(root, "rev-parse", "HEAD:reverie") == r,
            "core Reverie gitlink differs from requested engine source")
    manifest_path = root / "inguest-engine/Cargo.toml"
    source_manifest = tomllib.loads(manifest_path.read_text())
    for name in ("reverie", "reverie-inguest", "reverie-liteinst"):
        require(source_manifest["dependencies"][name]["rev"] == r,
                f"engine dependency pin mismatch: {name}")
    metadata_path = Path(args.cargo_metadata).resolve(strict=True)
    event_path = Path(args.cargo_events).resolve(strict=True)
    metadata_bytes = read_record(metadata_path)
    event_bytes = read_record(event_path)
    metadata = json.loads(metadata_bytes)
    require(Path(metadata["workspace_root"]).resolve() == root,
            "metadata belongs to another workspace")
    records = parse_cargo_events(event_bytes)
    package, unit = select_engine(records, metadata, root)
    packages = {p["id"]: p for p in metadata["packages"]}
    units = [row for row in records if row.get("reason") == "compiler-artifact"]
    ids = {row["package_id"] for row in units}
    require(ids <= packages.keys(), "compiled unit is absent from metadata")
    require(not {packages[p]["name"] for p in ids} & FORBIDDEN_PACKAGES,
            "backend package appeared in the neutral engine build")
    detcore = one([packages[p] for p in ids if packages[p]["name"] == "hermit-detcore"],
                  "engine build must contain one concrete Detcore package")
    require(Path(detcore["manifest_path"]).resolve() == root / "detcore/Cargo.toml",
            "Detcore is not the same source workspace as the engine")
    for name in ("reverie-core", "reverie-inguest", "reverie-liteinst"):
        dependency = one([packages[p] for p in ids if packages[p]["name"] == name],
                         f"engine build must contain one {name} package")
        require((dependency.get("source") or "").endswith("#" + r),
                f"actual compiled core revision mismatch: {name}")
        dependency_root = Path(dependency["manifest_path"]).resolve().parent
        committed_source(dependency_root, r)
    target = Path(args.target_dir).resolve(strict=True)
    require(Path(metadata["target_directory"]).resolve() == target,
            "metadata and requested target directory differ")
    emitted = one([Path(p) for p in unit["filenames"] if Path(p).name == ENGINE],
                  "actual engine artifact path is missing/ambiguous")
    require(emitted.is_absolute() and emitted.resolve(strict=True).is_relative_to(target),
            "engine artifact is outside the declared target")
    elf(emitted, shared=True)
    header = root / "inguest-engine/include" / HEADER
    native_header = root / "inguest-engine/include" / NATIVE_HEADER
    destination = Path(args.destination).absolute()
    os.mkdir(destination, 0o700)  # Existing and partial destinations refuse.
    artifacts = {"engine": copy_exclusive(emitted, destination / ENGINE, 0o755),
                 "header": copy_exclusive(header, destination / HEADER, 0o644),
                 "native_header": copy_exclusive(native_header, destination / NATIVE_HEADER, 0o644)}
    require(committed_source(root, h) == tree, "source tree changed during installation")
    event_record = {"file": event_path.name, "bytes": len(event_bytes),
                    "sha256": hashlib.sha256(event_bytes).hexdigest()}
    metadata_record = {"file": metadata_path.name, "bytes": len(metadata_bytes),
                       "sha256": hashlib.sha256(metadata_bytes).hexdigest()}
    require(digest(event_path) == event_record and digest(metadata_path) == metadata_record,
            "Cargo evidence changed during installation")
    record = {
        "schema": SCHEMA, "kind": "hermit-inguest-engine",
        "core_h_revision": h, "core_h_tree": tree, "core_r_revision": r,
        "artifacts": artifacts, "profile": unit["profile"], "features": unit["features"],
        "cargo_package_id": package["id"],
        "source_records": {"cargo_events": event_record,
                           "cargo_metadata": metadata_record,
                           "cargo_lock": digest(root / "Cargo.lock"),
                           "package_manifest": digest(manifest_path),
                           "installer": digest(Path(__file__).resolve())},
        "compiled_units": [{"package_id": u["package_id"], "target": u["target"]["name"],
                            "crate_types": u["target"]["crate_types"], "features": u["features"],
                            "profile": u["profile"], "fresh": u["fresh"]} for u in units],
        "qualification": "artifact installation only; no loader/runtime/private qualification",
    }
    write_new(destination / MANIFEST, record)  # Commit record is last.
    check(destination / MANIFEST)


def check(path: Path) -> dict:
    record = json.loads(read_record(path))
    require(set(record) == {"schema", "kind", "core_h_revision", "core_h_tree",
                            "core_r_revision", "artifacts", "profile", "features",
                            "cargo_package_id", "source_records", "compiled_units",
                            "qualification"},
            "unknown/missing neutral engine manifest fields")
    require(record.get("schema") == SCHEMA and record.get("kind") == "hermit-inguest-engine",
            "unsupported neutral engine artifact manifest")
    revision(record["core_h_revision"])
    revision(record["core_h_tree"])
    revision(record["core_r_revision"])
    require(isinstance(record["features"], list)
            and set(record["features"]) <= {"default", "dispatch-observer", "native-first-instruction-observer"},
            "unexpected engine features")
    require(isinstance(record["profile"], dict) and not record["profile"].get("test"),
            "invalid engine artifact profile")
    require(isinstance(record["compiled_units"], list) and record["compiled_units"],
            "compiled unit binding is missing")
    require(set(record["source_records"]) == {"cargo_events", "cargo_metadata", "cargo_lock",
                                             "package_manifest", "installer"},
            "neutral engine source record binding is incomplete")
    directory = path.resolve(strict=True).parent
    require(set(record["artifacts"]) == {"engine", "header", "native_header"},
            "unexpected artifact roles")
    for role, name in (("engine", ENGINE), ("header", HEADER),
                       ("native_header", NATIVE_HEADER)):
        expected = record["artifacts"][role]
        require(set(expected) == {"file", "bytes", "sha256"} and expected["file"] == name,
                f"unexpected artifact pathname/schema for {role}")
        require(type(expected["bytes"]) is int and 0 < expected["bytes"] <= MAX_FILE
                and re.fullmatch(r"[0-9a-f]{64}", expected["sha256"]) is not None,
                f"invalid artifact length/hash for {role}")
        require(digest(directory / name) == expected, f"installed {role} changed")
    elf(directory / ENGINE, shared=True)
    return record


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="operation", required=True)
    create = commands.add_parser("install")
    for name in ("hermit-revision", "reverie-revision", "cargo-events",
                 "cargo-metadata", "target-dir", "destination"):
        create.add_argument("--" + name, required=True)
    verify = commands.add_parser("check")
    verify.add_argument("manifest")
    args = parser.parse_args()
    if args.operation == "install":
        install(args)
    else:
        check(Path(args.manifest))


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        print(f"core engine artifact refused: {error}", file=sys.stderr)
        sys.exit(1)
