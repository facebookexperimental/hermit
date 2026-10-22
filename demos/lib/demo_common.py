#!/usr/bin/env python3
"""Shared utilities for the Python QEMU snapshot demos."""

import ctypes
import datetime as dt
import errno
import fcntl
import hashlib
import itertools
import json
import os
from pathlib import Path
import re
import shutil
import socket
import stat
import struct
import signal
import subprocess
import sys
import tempfile
import threading
import time
from dataclasses import dataclass
from enum import Enum
from typing import Any, Dict, List, Mapping, Optional, Sequence, Tuple, TypedDict, cast


WALLCLOCK_RE = re.compile(
    r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z[ \t]+"
)

# The repeat check of demos 5 and 6 compares the two runs' Hermit INFO logs
# (hermit-info.log, compare_hermit_logs). That file holds everything the demo
# captured from Hermit's standard output and error: Hermit's tracing records,
# the continuation lines of a multi-line record and of the run report, and what
# the guest controller and QEMU printed. Every one of those lines is compared,
# and only two things are normalized, both borrowed from Hermit's own canonical
# log comparison, BitwiseInfoV1 (detcore/src/logdiff.rs):
#
# 1. WALLCLOCK_RE removes the real wall-clock timestamp that starts a tracing
#    record (STRIP_WALL_CLOCK_PREFIX_V1). Whether a line starts with one is
#    still compared.
# 2. HOST_ADDR_RE matches the one marker Hermit puts around a host-side address
#    it prints, `<hostaddr 0x...>` (`host_addr` in logdiff.rs). Each marked value
#    becomes its first-appearance ordinal within its own log, exactly as
#    `canonicalize_addresses_in_line` does (CANON_ADDRESS_ORDINAL_V1), so which
#    marked values repeat, and in what order they first appear, still has to
#    match.
#
# Before saving its log, demo 5 also replaces its private run directory and QMP
# socket path with fixed tokens (canonicalize_qemu_runtime_paths_in_file).
#
# This is not BitwiseInfoV1 itself. It is stricter in three ways: it compares
# every captured line, where BitwiseInfoV1 compares only INFO records; it
# compares bytes, so a carriage return, a line ending or a byte that is not
# UTF-8 counts like any other byte, where Hermit refuses a log that is not UTF-8
# and trims each record; and it removes a timestamp only at the start of a line,
# where Hermit starts a new record at a timestamp anywhere. It does not check,
# as BitwiseInfoV1 does, that DETLOG records are in Hermit's current structured
# form, a property of the log's format rather than of whether the two runs
# agree. Like BitwiseInfoV1 (log_was_truncated and matched_with_evidence in
# logdiff.rs), it refuses a log that ends with the truncation marker of
# Hermit's bounded log writer, and it counts a match only when each log holds at
# least one INFO record. The demos start Hermit without HERMIT_LOG and
# HERMIT_LOG_FILE (hermit_log_environment), so neither can change or redirect
# what is captured.
#
# Every other byte must match, including the bare `0x7f...` guest addresses that
# DETLOG prints for syscall pointer arguments. The two logs come from separate
# `hermit run` invocations with identical inputs: the launchers give the guest
# Hermit's minimal fixed environment (guest_environment_args), fixed guest-side
# paths for the controller, the assets and the run directory, a fixed working
# directory and a fixed --epoch, and the ptrace backend turns off address-space
# randomization in the guest. Guest addresses are then reproducible, and the
# retained logs agree: two demo 5 boot pairs (2026-09-30 and 2026-10-01;
# 2,332,583 lines and 817,167 bare 0x7f... values per log) and one demo 6 resume
# pair (2026-10-01; 1,604,193 lines, 320,244 values) differed in no line once the
# wall-clock prefix was removed. The demo 5 and demo 6 READMEs give the counts
# this comparison reported for a pair of each on 2026-10-03. A guest address
# that still differs means the inputs differed or the execution diverged, so the
# repeat fails.
#
# Logs also carry a file's resource id, `FileContents(DetInode(N))`. Hermit
# derives N deterministically (determinize_inode in detcore/src/tool_global.rs),
# so it too is compared exactly.
HOST_ADDR_RE = re.compile(r"<hostaddr (0[xX][A-Fa-f0-9]+)>")
# Schema 2 made qemu_binary_sha256 required. Schema 3 adds guest_exit_status,
# the guest command's exit status, to qemu-resume rows and requires it there.
RUN_METADATA_SCHEMA_VERSION = 3
SUPPORTED_RUN_METADATA_SCHEMA_VERSIONS = (1, 2, RUN_METADATA_SCHEMA_VERSION)


class QemuRunKind(str, Enum):
    BOOT = "qemu-boot"
    RESUME = "qemu-resume"


class QemuRunMetadataRecord(TypedDict, total=False):
    schema_version: int
    created_at: str
    kind: str
    info_log: str
    info_log_sha256: str
    hermit_version: str
    qemu_version: str
    qemu_binary_sha256: str
    qemu_argv: List[str]
    serial_log: str
    serial_sha256: str
    qcow2_path: str
    qcow2_sha256: str
    qcow2_size: int
    snapshot_name: str
    snapshot_date_nsec_canonicalized: bool
    command: str
    command_sha256: str
    guest_output: str
    guest_output_sha256: str
    guest_exit_status: int
    snapshot_saved: bool


@dataclass(frozen=True)
class QemuRunMetadata:
    schema_version: int
    kind: QemuRunKind
    created_at: str
    info_log: str
    info_log_sha256: str
    hermit_version: str
    qemu_version: str
    qemu_binary_sha256: Optional[str]
    qemu_argv: Tuple[str, ...]
    serial_log: str
    serial_sha256: Optional[str]
    qcow2_path: Optional[str]
    qcow2_sha256: Optional[str]
    qcow2_size: Optional[int]
    snapshot_name: Optional[str]
    snapshot_date_nsec_canonicalized: Optional[bool]
    command: Optional[str]
    command_sha256: Optional[str]
    guest_output: Optional[str]
    guest_output_sha256: Optional[str]
    snapshot_saved: Optional[bool]
    guest_exit_status: Optional[int]
    raw: QemuRunMetadataRecord

    def with_info_log(self, path: Path) -> "QemuRunMetadata":
        value = dict(self.raw)
        value["info_log"] = str(Path(path).resolve())
        return parse_run_metadata(value)


COMMON_METADATA_FIELDS = frozenset(
    {
        "schema_version",
        "created_at",
        "kind",
        "info_log",
        "info_log_sha256",
        "hermit_version",
        "qemu_version",
        "qemu_binary_sha256",
        "qemu_argv",
        "serial_log",
    }
)
BOOT_METADATA_FIELDS = COMMON_METADATA_FIELDS | frozenset(
    {
        "serial_sha256",
        "qcow2_path",
        "qcow2_sha256",
        "qcow2_size",
        "snapshot_name",
        "snapshot_date_nsec_canonicalized",
    }
)
RESUME_METADATA_FIELDS = COMMON_METADATA_FIELDS | frozenset(
    {
        "command",
        "command_sha256",
        "guest_output",
        "guest_output_sha256",
        "guest_exit_status",
        "snapshot_saved",
        "qcow2_path",
        "qcow2_sha256",
        "qcow2_size",
        "snapshot_date_nsec_canonicalized",
    }
)
METADATA_FIELDS_BY_KIND = {
    QemuRunKind.BOOT: BOOT_METADATA_FIELDS,
    QemuRunKind.RESUME: RESUME_METADATA_FIELDS,
}


def _metadata_error(field: str, detail: str) -> ValueError:
    return ValueError("qemu-run-metadata-{}: {}".format(field, detail))


def _metadata_text(value: Mapping[str, Any], field: str) -> str:
    raw = value.get(field)
    if not isinstance(raw, str) or not raw.strip():
        raise _metadata_error(field, "must be a nonempty string")
    return raw


def _metadata_sha256(value: Mapping[str, Any], field: str) -> str:
    raw = _metadata_text(value, field)
    if len(raw) != 64 or any(character not in "0123456789abcdef" for character in raw):
        raise _metadata_error(field, "must be a lowercase 64-hex SHA-256")
    return raw


def _metadata_optional_text(value: Mapping[str, Any], field: str) -> Optional[str]:
    if field not in value:
        return None
    return _metadata_text(value, field)


def _metadata_optional_sha256(value: Mapping[str, Any], field: str) -> Optional[str]:
    if field not in value:
        return None
    return _metadata_sha256(value, field)


def _metadata_qemu_argv(value: Mapping[str, Any]) -> Tuple[str, ...]:
    raw = value.get("qemu_argv")
    if (
        not isinstance(raw, list)
        or not raw
        or any(not isinstance(argument, str) or not argument for argument in raw)
    ):
        raise _metadata_error("qemu_argv", "must be a nonempty list of strings")
    return tuple(raw)


def _metadata_optional_exit_status(value: Mapping[str, Any]) -> Optional[int]:
    if "guest_exit_status" not in value:
        return None
    raw = value.get("guest_exit_status")
    if not isinstance(raw, int) or isinstance(raw, bool) or not 0 <= raw <= 255:
        raise _metadata_error(
            "guest_exit_status", "must be an integer from 0 to 255"
        )
    return raw


def _metadata_optional_size(value: Mapping[str, Any]) -> Optional[int]:
    if "qcow2_size" not in value:
        return None
    raw = value.get("qcow2_size")
    if not isinstance(raw, int) or isinstance(raw, bool) or raw < 0:
        raise _metadata_error("qcow2_size", "must be a nonnegative integer")
    return raw


def parse_run_metadata(value: Mapping[str, Any]) -> QemuRunMetadata:
    """Read the version before requiring its complete kind-specific shape."""
    schema_version = value.get("schema_version")
    if (
        not isinstance(schema_version, int)
        or isinstance(schema_version, bool)
        or schema_version not in SUPPORTED_RUN_METADATA_SCHEMA_VERSIONS
    ):
        raise _metadata_error(
            "schema_version", "unsupported value {!r}".format(schema_version)
        )
    try:
        kind = QemuRunKind(_metadata_text(value, "kind"))
    except ValueError as error:
        if str(error).startswith("qemu-run-metadata-"):
            raise
        raise _metadata_error(
            "kind", "unsupported value {!r}".format(value.get("kind"))
        )

    allowed = METADATA_FIELDS_BY_KIND.get(kind)
    if allowed is None:
        raise _metadata_error(
            "kind", "has no field contract for {!r}".format(kind.value)
        )
    unknown = sorted(set(value) - allowed)
    if unknown:
        raise _metadata_error(
            "field",
            "unknown field(s) for kind {!r}: {}".format(kind.value, ", ".join(unknown)),
        )

    created_at = _metadata_text(value, "created_at")
    info_log = _metadata_text(value, "info_log")
    info_log_sha256 = _metadata_sha256(value, "info_log_sha256")
    hermit_version = _metadata_text(value, "hermit_version")
    qemu_version = _metadata_text(value, "qemu_version")
    qemu_binary_sha256 = _metadata_optional_sha256(value, "qemu_binary_sha256")
    qemu_argv = _metadata_qemu_argv(value)
    serial_log = _metadata_text(value, "serial_log")

    serial_sha256 = _metadata_optional_sha256(value, "serial_sha256")
    qcow2_path = _metadata_optional_text(value, "qcow2_path")
    qcow2_sha256 = _metadata_optional_sha256(value, "qcow2_sha256")
    qcow2_size = _metadata_optional_size(value)
    snapshot_name = _metadata_optional_text(value, "snapshot_name")
    canonicalized = value.get("snapshot_date_nsec_canonicalized")
    if canonicalized is not None and not isinstance(canonicalized, bool):
        raise _metadata_error("snapshot_date_nsec_canonicalized", "must be a boolean")
    command = _metadata_optional_text(value, "command")
    command_sha256 = _metadata_optional_sha256(value, "command_sha256")
    guest_output = _metadata_optional_text(value, "guest_output")
    guest_output_sha256 = _metadata_optional_sha256(value, "guest_output_sha256")
    guest_exit_status = _metadata_optional_exit_status(value)
    snapshot_saved = value.get("snapshot_saved")
    if snapshot_saved is not None and not isinstance(snapshot_saved, bool):
        raise _metadata_error("snapshot_saved", "must be a boolean")

    if kind is QemuRunKind.BOOT:
        if qemu_binary_sha256 is None:
            raise _metadata_error("qemu_binary_sha256", "is required for qemu-boot")
        for field, parsed in (
            ("serial_sha256", serial_sha256),
            ("qcow2_path", qcow2_path),
            ("qcow2_sha256", qcow2_sha256),
            ("qcow2_size", qcow2_size),
            ("snapshot_name", snapshot_name),
        ):
            if parsed is None:
                raise _metadata_error(field, "is required for qemu-boot")
        if canonicalized is not True:
            raise _metadata_error(
                "snapshot_date_nsec_canonicalized", "must be true for qemu-boot"
            )
    elif kind is QemuRunKind.RESUME:
        for field, parsed in (
            ("command", command),
            ("command_sha256", command_sha256),
            ("guest_output", guest_output),
            ("guest_output_sha256", guest_output_sha256),
        ):
            if parsed is None:
                raise _metadata_error(field, "is required for qemu-resume")
        if schema_version >= 3 and guest_exit_status is None:
            raise _metadata_error(
                "guest_exit_status",
                "is required for qemu-resume from schema 3 on",
            )
        if schema_version < 3 and guest_exit_status is not None:
            raise _metadata_error(
                "guest_exit_status",
                "is not part of schema {}".format(schema_version),
            )
        if snapshot_saved is None:
            raise _metadata_error("snapshot_saved", "is required for qemu-resume")
        if qemu_binary_sha256 is None and not (
            schema_version == 1 and snapshot_saved is False
        ):
            raise _metadata_error(
                "qemu_binary_sha256",
                "is required except on schema-1 rows without a saved snapshot",
            )
        snapshot_fields = {
            "qcow2_path": qcow2_path,
            "qcow2_sha256": qcow2_sha256,
            "qcow2_size": qcow2_size,
            "snapshot_date_nsec_canonicalized": canonicalized,
        }
        if snapshot_saved:
            for field, parsed in snapshot_fields.items():
                if parsed is None:
                    raise _metadata_error(
                        field, "is required when snapshot_saved is true"
                    )
            if canonicalized is not True:
                raise _metadata_error(
                    "snapshot_date_nsec_canonicalized",
                    "must be true when snapshot_saved is true",
                )
        else:
            present = sorted(
                field for field, parsed in snapshot_fields.items() if parsed is not None
            )
            if present:
                raise _metadata_error(
                    "snapshot_saved",
                    "is false but snapshot field(s) are present: {}".format(
                        ", ".join(present)
                    ),
                )
    else:
        raise _metadata_error(
            "kind", "has no value contract for {!r}".format(kind.value)
        )

    return QemuRunMetadata(
        schema_version=schema_version,
        kind=kind,
        created_at=created_at,
        info_log=info_log,
        info_log_sha256=info_log_sha256,
        hermit_version=hermit_version,
        qemu_version=qemu_version,
        qemu_binary_sha256=qemu_binary_sha256,
        qemu_argv=qemu_argv,
        serial_log=serial_log,
        serial_sha256=serial_sha256,
        qcow2_path=qcow2_path,
        qcow2_sha256=qcow2_sha256,
        qcow2_size=qcow2_size,
        snapshot_name=snapshot_name,
        snapshot_date_nsec_canonicalized=canonicalized,
        command=command,
        command_sha256=command_sha256,
        guest_output=guest_output,
        guest_output_sha256=guest_output_sha256,
        snapshot_saved=snapshot_saved,
        guest_exit_status=guest_exit_status,
        raw=cast(QemuRunMetadataRecord, dict(value)),
    )


def _under_host_tmp(root: Path) -> bool:
    """Whether ``root`` resolves inside the host's ``/tmp`` tree."""
    try:
        Path(root).resolve().relative_to("/tmp")
    except ValueError:
        return False
    return True


def default_qemu_assets(root: Path) -> Path:
    """Return a host-visible, checkout-scoped default for persistent QEMU assets."""
    root = Path(root).resolve()
    if not _under_host_tmp(root):
        return root / "ignored/qemu-linux"
    # Hermit mounts a private tmpfs over /tmp. Keep persistent QEMU inputs outside it,
    # and include the canonical checkout identity so concurrent clones cannot share or
    # clean each other's snapshots.
    digest = hashlib.sha256(str(root).encode("utf-8")).hexdigest()[:12]
    return Path("/var/tmp") / "hermit-demo-qemu-{}-{}".format(
        os.getuid(), digest
    )


def hermit_tmp_args(root: Path, *paths: Optional[Path]) -> List[str]:
    """Return ``--tmp=/tmp`` when a host path the traced guest opens is under host /tmp.

    Without ``--tmp``, Hermit mounts a private tmpfs over the guest's /tmp, which
    hides everything under the host's /tmp. ``root`` is the checkout, whose QEMU
    controller and runtime paths a guest may open; ``paths`` are any other host
    paths it opens, such as a run directory or a kernel. ``None`` entries are
    skipped.
    """
    candidates = [root, *(path for path in paths if path is not None)]
    return ["--tmp=/tmp"] if any(_under_host_tmp(path) for path in candidates) else []


# The files the guest's QEMU controller runs: the script and the one module it
# imports. See stage_guest_controller.
GUEST_CONTROLLER_SOURCES = ("qemu_controller.py", "demo_common.py")
# The modification time given to the staged copies and their directory:
# 2026-01-01T00:00:00Z, the fixed epoch the QEMU demos pass to Hermit.
GUEST_CONTROLLER_MTIME = 1767225600


def stage_guest_controller(destination: Path) -> Path:
    """Copy the controller's sources into a new directory for the guest to run.

    The guest's Python used to import them straight from demos/lib. It then
    also read the bytecode cache that the host's Python leaves in
    demos/lib/__pycache__, and a cached ``.pyc`` records the absolute path of
    its source file: from a checkout whose path was 6 characters shorter, the
    guest read a ``demo_common`` cache 6 bytes shorter, and the boot ended at a
    different virtual time with a different Hermit log. Anything else in
    demos/lib (an editor's swap file, a stale cache) would also change the
    directory listing the guest reads.

    The new directory holds only the two sources, with fixed contents, file
    modes, and modification times, and no cache. The guest runs with
    PYTHONDONTWRITEBYTECODE set, so it compiles demo_common from source and
    writes nothing back. Returns ``destination``.
    """
    destination = Path(destination)
    destination.mkdir(mode=0o755)
    os.chmod(destination, 0o755)
    library = Path(__file__).resolve().parent
    for name in GUEST_CONTROLLER_SOURCES:
        staged = destination / name
        shutil.copyfile(library / name, staged)
        os.chmod(staged, 0o644)
        os.utime(staged, (GUEST_CONTROLLER_MTIME, GUEST_CONTROLLER_MTIME))
    os.utime(destination, (GUEST_CONTROLLER_MTIME, GUEST_CONTROLLER_MTIME))
    return destination


def display_path(path: Path, root: Path) -> str:
    """Render a repo-relative path when possible, otherwise a stable absolute path."""
    path = Path(path).resolve()
    try:
        return str(path.relative_to(Path(root).resolve()))
    except ValueError:
        return str(path)


def hash_file(path: Path) -> str:
    """Return the SHA-256 digest of a file."""
    digest = hashlib.sha256()
    with Path(path).open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def canonicalize_qcow2_snapshot_timestamp(path: Path, snapshot_name: str) -> None:
    """Zero a qcow2 snapshot's non-guest subsecond creation timestamp."""
    path = Path(path)
    with path.open("r+b") as image:
        header = image.read(72)
        if len(header) != 72 or header[:4] != b"QFI\xfb":
            raise ValueError("not a qcow2 image: {}".format(path))
        version = struct.unpack_from(">I", header, 4)[0]
        if version < 3:
            raise ValueError(
                "qcow2 version {} lacks v3 snapshot metadata".format(version)
            )
        snapshot_count = struct.unpack_from(">I", header, 60)[0]
        snapshot_offset = struct.unpack_from(">Q", header, 64)[0]
        image.seek(snapshot_offset)
        for _ in range(snapshot_count):
            entry_offset = image.tell()
            entry = image.read(40)
            if len(entry) != 40:
                raise ValueError("truncated qcow2 snapshot table: {}".format(path))
            _, _, id_size, name_size, _, _, _, _, extra_size = struct.unpack(
                ">QIHHIIQII", entry
            )
            extra_and_strings = image.read(extra_size + id_size + name_size)
            if len(extra_and_strings) != extra_size + id_size + name_size:
                raise ValueError("truncated qcow2 snapshot entry: {}".format(path))
            name_start = extra_size + id_size
            name = extra_and_strings[name_start:].decode("utf-8")
            if name == snapshot_name:
                image.seek(entry_offset + 20)
                image.write(struct.pack(">I", 0))
                image.flush()
                return
            entry_size = 40 + extra_size + id_size + name_size
            image.seek(entry_offset + ((entry_size + 7) // 8) * 8)
    raise ValueError("snapshot {!r} not found in {}".format(snapshot_name, path))


def _tool_version(command: Sequence[str]) -> str:
    try:
        result = subprocess.run(
            list(command),
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
            text=True,
            timeout=20,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        return "unavailable: {}".format(error)
    first_line = result.stdout.splitlines()
    return first_line[0] if first_line else "unknown"


def _tool_sha256(executable: str) -> str:
    path = Path(shutil.which(executable) or executable)
    if not path.is_file():
        raise _metadata_error(
            "qemu_binary_sha256", "cannot hash {}: not a file".format(path)
        )
    try:
        return hash_file(path)
    except OSError as error:
        raise _metadata_error(
            "qemu_binary_sha256", "cannot hash {}: {}".format(path, error)
        ) from error


def _write_json(path: Path, value: Dict[str, Any]) -> None:
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name("{}.tmp.{}".format(path.name, os.getpid()))
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")
    os.replace(str(temporary), str(path))


def save_metadata(
    run_dir: Path,
    qcow2_path: Optional[Path],
    info_log: Path,
    extra: Optional[Dict[str, Any]] = None,
) -> QemuRunMetadata:
    """Save machine-readable metadata for one run and return it."""
    run_dir = Path(run_dir)
    info_log = Path(info_log)
    run_dir.mkdir(parents=True, exist_ok=True)
    qemu = os.environ.get("QEMU_BIN", "qemu-system-x86_64")
    metadata: Dict[str, Any] = {
        "schema_version": RUN_METADATA_SCHEMA_VERSION,
        "created_at": dt.datetime.now(dt.timezone.utc).isoformat(),
        "info_log": str(info_log.resolve()),
        "info_log_sha256": hash_file(info_log),
        "hermit_version": _tool_version([hermit_binary(), "--version"]),
        "qemu_version": _tool_version([qemu, "--version"]),
        "qemu_binary_sha256": _tool_sha256(qemu),
    }
    if qcow2_path is not None:
        qcow2_path = Path(qcow2_path)
        metadata.update(
            {
                "qcow2_path": str(qcow2_path.resolve()),
                "qcow2_sha256": hash_file(qcow2_path),
                "qcow2_size": qcow2_path.stat().st_size,
            }
        )
    if extra:
        overlap = sorted(set(metadata) & set(extra))
        if overlap:
            raise _metadata_error(
                "field",
                "extra fields replace common field(s): {}".format(", ".join(overlap)),
            )
        metadata.update(extra)
    typed = parse_run_metadata(metadata)
    _write_json(run_dir / "run-metadata.json", dict(typed.raw))
    return typed


def load_anchor(run_dir: Path) -> Optional[QemuRunMetadata]:
    """Load the first-run metadata anchor, if present."""
    anchor_path = Path(run_dir) / "run-metadata.json"
    if not anchor_path.is_file():
        return None
    return parse_run_metadata(json.loads(anchor_path.read_text()))


def save_anchor(run_dir: Path, metadata: QemuRunMetadata) -> Path:
    """Persist a metadata object as the first-run anchor."""
    anchor_path = Path(run_dir) / "run-metadata.json"
    _write_json(anchor_path, dict(metadata.raw))
    return anchor_path


# ---------------------------------------------------------------------------
# Concurrent-safe anchor claim.
#
# Multiple demo runs may execute simultaneously. Each builds its whole result in
# a private working directory (make_temp_result_dir) and then races to publish it
# as THE anchor with a single atomic, no-clobber rename. Exactly one run wins and
# becomes the first-run anchor; every other run loses cleanly (EEXIST), archives
# its own result, and compares against the fully-committed anchor. Because the
# entire result directory is moved in one step, a loser never observes a
# half-written anchor.
# ---------------------------------------------------------------------------

# renameat2(2) with RENAME_NOREPLACE is the atomic, no-clobber primitive.
_AT_FDCWD = -100
_RENAME_NOREPLACE = 1
# x86-64 renameat2 syscall number; used only if the glibc wrapper is missing.
_SYS_RENAMEAT2 = 316


def _rename_noreplace(src: Path, dst: Path) -> None:
    """Atomically rename ``src`` to ``dst``, refusing to clobber an existing dst.

    Wraps Linux ``renameat2(RENAME_NOREPLACE)``: the move either creates ``dst``
    atomically or raises ``OSError(EEXIST)`` because ``dst`` already exists. It
    never overwrites ``dst``, and there is no check-then-move window (no TOCTOU).
    Raises ``OSError(ENOSYS)``/``OSError(EINVAL)`` when the kernel or filesystem
    lacks RENAME_NOREPLACE, so callers can fall back to a lock-guarded rename.
    """
    libc = ctypes.CDLL(None, use_errno=True)
    src_b = os.fsencode(str(src))
    dst_b = os.fsencode(str(dst))
    wrapper = getattr(libc, "renameat2", None)
    if wrapper is not None:
        wrapper.restype = ctypes.c_int
        wrapper.argtypes = [
            ctypes.c_int,
            ctypes.c_char_p,
            ctypes.c_int,
            ctypes.c_char_p,
            ctypes.c_uint,
        ]
        result = wrapper(
            _AT_FDCWD, src_b, _AT_FDCWD, dst_b, ctypes.c_uint(_RENAME_NOREPLACE)
        )
    else:
        result = libc.syscall(
            ctypes.c_long(_SYS_RENAMEAT2),
            ctypes.c_int(_AT_FDCWD),
            ctypes.c_char_p(src_b),
            ctypes.c_int(_AT_FDCWD),
            ctypes.c_char_p(dst_b),
            ctypes.c_uint(_RENAME_NOREPLACE),
        )
    if result != 0:
        code = ctypes.get_errno()
        raise OSError(code, os.strerror(code), str(dst))


def _publish_anchor_locked(work_dir: Path, anchor_dir: Path) -> bool:
    """Portable fallback claim: serialize with an exclusive lock, then rename.

    Used only when the filesystem lacks ``renameat2(RENAME_NOREPLACE)``. The lock
    removes the check-then-rename race: a loser cannot rename while the winner
    holds the lock, so the plain ``os.rename`` here is safe and never clobbers a
    published anchor.
    """
    lock_path = anchor_dir.with_name(anchor_dir.name + ".claim.lock")
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    handle = lock_path.open("a+")
    try:
        fcntl.flock(handle.fileno(), fcntl.LOCK_EX)
        if anchor_dir.exists():
            return False
        os.rename(str(work_dir), str(anchor_dir))
        return True
    finally:
        fcntl.flock(handle.fileno(), fcntl.LOCK_UN)
        handle.close()


def make_temp_result_dir(assets: Path, prefix: str) -> Path:
    """Create a private, unique per-run working directory under ``<assets>/.work``.

    Everything for one concurrent run (sockets, snapshot, logs, metadata) is
    built here in isolation so simultaneous runs never share a path. The
    directory is later published atomically as the anchor (winner) or archived
    into run-history (loser).
    """
    work_root = Path(assets) / ".work"
    work_root.mkdir(parents=True, exist_ok=True)
    return Path(tempfile.mkdtemp(prefix="{}-".format(prefix), dir=str(work_root)))


# Linux's sockaddr_un.sun_path is 108 bytes INCLUDING the NUL terminator, so a
# bind or connect fails once the pathname reaches 108. This is a kernel ABI
# constant, not a tunable.
AF_UNIX_PATH_MAX = 107


def make_socket_path(preferred: Path, prefix: str) -> Path:
    """Return a socket pathname for ``preferred``, relocating only if it must.

    Linux rejects a Unix-domain socket path at or over AF_UNIX_PATH_MAX. The
    natural home for these sockets is the per-run working directory, alongside
    the run's other files, and from most checkouts that fits. A deeply nested
    checkout can exceed the bound, and QEMU then refuses to create the socket.

    Relocation is the fallback, not the default. QEMU runs under Hermit, so a
    socket outside the checkout is not necessarily visible to both the guest
    that creates it and the controller that connects to it. Keep the working
    path wherever it fits, and move only when the bound forces it.
    """
    preferred = Path(preferred)
    if len(str(preferred).encode()) <= AF_UNIX_PATH_MAX:
        return preferred

    # Do not use host /tmp here: Hermit normally mounts a private
    # tmpfs over /tmp, which would hide an outer controller's socket from QEMU
    # running under Hermit.  /var/tmp is deliberately also used by
    # default_qemu_assets for this namespace-visibility reason.
    base = Path(os.environ.get("QEMU_SOCKET_DIR", "/var/tmp"))
    if _under_host_tmp(base):
        raise RuntimeError(
            "socket path {} is {} bytes, over the {}-byte AF_UNIX limit, but "
            "QEMU_SOCKET_DIR={} is under host /tmp, which Hermit normally hides "
            "with a private tmpfs. Choose a short host-visible directory outside "
            "/tmp, or set QEMU_ASSETS to a short path so relocation is "
            "unnecessary.".format(
                preferred, len(str(preferred).encode()), AF_UNIX_PATH_MAX, base
            )
        )
    root = base / "hermit-qmp-{}".format(os.getuid())
    try:
        root.mkdir(parents=True, mode=0o700, exist_ok=True)
    except OSError as error:
        raise RuntimeError(
            "socket path {} is {} bytes, over the {}-byte AF_UNIX limit, and the "
            "host-visible fallback root {} is unusable: {}. Set QEMU_SOCKET_DIR "
            "to a short writable directory outside /tmp, or set QEMU_ASSETS to "
            "a short path so relocation is unnecessary.".format(
                preferred, len(str(preferred).encode()), AF_UNIX_PATH_MAX, root, error
            )
        ) from error

    # Hash the complete preferred path.  Demo 5's preferred path includes its
    # unique run directory, so concurrent runs remain isolated.  Demo 6's
    # preferred path is stable and protected by its existing lock, so repeats
    # reuse the same spelling.  Unlike mkdtemp this mapping is deterministic:
    # callers can canonicalize the one known relocated socket without masking
    # any other QEMU argument.
    identity = hashlib.sha256(os.fsencode(str(preferred.resolve()))).hexdigest()[:16]
    path = root / "{}-{}.sock".format(prefix, identity)
    length = len(str(path).encode())
    if length > AF_UNIX_PATH_MAX:
        raise RuntimeError(
            "socket path is {} bytes even after relocating under {}, over the "
            "{}-byte AF_UNIX limit: {}. Set QEMU_SOCKET_DIR to a shorter "
            "host-visible directory outside /tmp.".format(
                length, root, AF_UNIX_PATH_MAX, path
            )
        )
    return path


def canonicalize_qemu_runtime_path(text: str, run_dir: Path, qmp_socket: Path) -> str:
    """Fold only the private runtime paths whose spelling varies between runs."""
    run_dir = Path(run_dir)
    qmp_socket = Path(qmp_socket)
    normalized = text.replace(str(run_dir), "<run-dir>")
    try:
        qmp_socket.relative_to(run_dir)
    except ValueError:
        normalized = normalized.replace(str(qmp_socket), "<qmp-socket>")
    return normalized


def publish_anchor(work_dir: Path, anchor_dir: Path) -> bool:
    """Atomically publish ``work_dir`` as THE anchor directory.

    Returns ``True`` if this run won the anchor (``work_dir`` became
    ``anchor_dir``), or ``False`` if an anchor already existed, in which case the
    caller is a loser and must compare against the committed anchor. The whole
    result directory is moved in one atomic, no-clobber step, so no reader ever
    observes a half-written anchor.
    """
    work_dir = Path(work_dir)
    anchor_dir = Path(anchor_dir)
    anchor_dir.parent.mkdir(parents=True, exist_ok=True)
    try:
        _rename_noreplace(work_dir, anchor_dir)
        return True
    except OSError as error:
        if error.errno == errno.EEXIST:
            return False
        if error.errno in (errno.ENOSYS, errno.EINVAL):
            return _publish_anchor_locked(work_dir, anchor_dir)
        raise


def load_committed_anchor(anchor_dir: Path) -> Optional[QemuRunMetadata]:
    """Load the committed anchor metadata, resolving its bundled INFO log path.

    Returns ``None`` when no anchor exists yet. The anchor's Hermit INFO log is
    bundled inside the anchor directory; the ``info_log`` field recorded before
    publication points at the pre-publish working path, so rewrite it to the
    bundled copy for log comparison.
    """
    anchor_dir = Path(anchor_dir)
    anchor_meta = anchor_dir / "run-metadata.json"
    if not anchor_meta.is_file():
        return None
    metadata = parse_run_metadata(json.loads(anchor_meta.read_text()))
    bundled_log = anchor_dir / "hermit-info.log"
    if bundled_log.is_file():
        metadata = metadata.with_info_log(bundled_log)
    return metadata


def archive_result_dir(work_dir: Path, assets: Path, prefix: str) -> Path:
    """Move a completed non-anchor run into run-history under a unique name."""
    work_dir = Path(work_dir)
    timestamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")
    history = Path(assets) / "run-history"
    history.mkdir(parents=True, exist_ok=True)
    # work_dir.name carries the mkdtemp random suffix, guaranteeing uniqueness.
    destination = history / "{}-{}-{}".format(prefix, timestamp, work_dir.name)
    os.rename(str(work_dir), str(destination))
    return destination


def publish_file_atomic(src: Path, dst: Path) -> None:
    """Copy ``src`` onto ``dst`` atomically (temp copy + ``os.replace``).

    Concurrent-safe replacement for a plain copy of a shared handoff artifact:
    readers always see either the old or the new complete file, never a partial.
    """
    src = Path(src)
    dst = Path(dst)
    dst.parent.mkdir(parents=True, exist_ok=True)
    temporary = dst.with_name("{}.tmp.{}".format(dst.name, os.getpid()))
    shutil.copy2(str(src), str(temporary))
    os.replace(str(temporary), str(dst))


# Demo 6 restores demo 5's boot snapshot instead of booting, and the snapshot's
# memory holds the guest's /init, which reads demo 6's command, runs it (as
# which user, with which standard input) and frames its output. That /init came
# from the initramfs demo 5 booted, so a snapshot saved before an /init change
# keeps running the old /init after demos/lib/qemu-assets.sh builds a new
# initramfs. Demo 5 therefore writes a record next to each boot snapshot it
# saves, naming the snapshot's SHA-256 and the INITRAMFS_VERSION and SHA-256 of
# the initramfs it booted, and demo 6 restores a snapshot only when that record
# matches the snapshot and the initramfs it would use now (verify_boot_snapshot).
BOOT_SNAPSHOT_RECORD_FORMAT = 1
INITRAMFS_VERSION_RE = re.compile(r"^INITRAMFS_VERSION=([0-9]+)$", re.MULTILINE)
SHA256_RE = re.compile(r"[0-9a-f]{64}")


class BootSnapshotMismatch(RuntimeError):
    """A boot snapshot has no record that matches it and the current initramfs."""


def boot_snapshot_record_path(snapshot: Path) -> Path:
    """The record demo 5 writes next to the boot snapshot ``snapshot``."""
    snapshot = Path(snapshot)
    return snapshot.with_name(snapshot.name + ".producer.json")


def current_initramfs_version(root: Path) -> int:
    """The INITRAMFS_VERSION that demos/lib/qemu-assets.sh builds now."""
    script = Path(root) / "demos/lib/qemu-assets.sh"
    versions = INITRAMFS_VERSION_RE.findall(script.read_text(encoding="utf-8"))
    if len(versions) != 1:
        raise RuntimeError(
            "expected one INITRAMFS_VERSION= line in {}, found {}".format(
                script, len(versions)
            )
        )
    return int(versions[0])


def initramfs_producer(root: Path, assets: Path) -> Dict[str, Any]:
    """The initramfs a boot uses now: the version qemu-assets.sh builds, and the
    SHA-256 of ``assets``/initramfs.cpio.gz."""
    return {
        "initramfs_version": current_initramfs_version(root),
        "initramfs_sha256": hash_file(Path(assets) / "initramfs.cpio.gz"),
    }


# The build record demos/lib/qemu-assets.sh writes next to the initramfs it
# builds: one line, "<INITRAMFS_VERSION> <SHA-256>", naming the archive it built
# by its SHA-256 and the version it built it at.
INITRAMFS_BUILD_RECORD = ".initramfs-build"


def stage_boot_assets(assets: Path, destination: Path) -> Path:
    """Copy the kernel and initramfs from ``assets`` into ``destination``.

    ``destination`` is a new directory private to one boot. Checkouts of
    different versions share ``assets`` and replace its initramfs.cpio.gz when
    they build theirs, so a boot that QEMU starts from the shared file can read
    bytes other than the ones that were hashed for its record. Demo 5 boots the
    copy, hashes the copy, and records that hash.
    """
    destination = Path(destination)
    destination.mkdir(parents=True)
    for name in ("bzImage", "initramfs.cpio.gz"):
        shutil.copy2(str(Path(assets) / name), str(destination / name))
    return destination


def booted_initramfs_producer(
    root: Path, assets: Path, initramfs: Path
) -> Dict[str, Any]:
    """The initramfs a boot runs: ``initramfs``, the private copy QEMU boots.

    Returns the SHA-256 of that copy, and the INITRAMFS_VERSION that
    qemu-assets.sh under ``root`` builds, but only when the build record in
    ``assets`` names exactly that version and that SHA-256. qemu-assets.sh
    writes the record after it builds an archive, naming the SHA-256 of the
    archive it built and the version it built it at, so a record is true of the
    bytes it names however old it is; requiring its SHA-256 to be the copy's
    binds the version to the bytes booted. Raises RuntimeError otherwise: the
    shared initramfs was replaced after qemu-assets.sh ran, by another checkout
    or by a version of qemu-assets.sh that writes no record.
    """
    sha256 = hash_file(initramfs)
    version = current_initramfs_version(root)
    record_path = Path(assets) / INITRAMFS_BUILD_RECORD
    try:
        record = record_path.read_text(encoding="utf-8")
    except OSError as error:
        record = "unreadable ({})".format(error)
    if record.split() != [str(version), sha256]:
        raise RuntimeError(
            "the initramfs copied for this boot (SHA-256 {}) is not one that "
            "demos/lib/qemu-assets.sh built at INITRAMFS_VERSION {}: its build "
            "record {} says {!r}. Another checkout replaced the shared initramfs "
            "after qemu-assets.sh ran; run demo 5 again.".format(
                sha256, version, record_path, record.strip()
            )
        )
    return {"initramfs_version": version, "initramfs_sha256": sha256}


def write_boot_snapshot_record(
    snapshot: Path, snapshot_sha256: str, producer: Mapping[str, Any]
) -> Path:
    """Record, next to ``snapshot``, its SHA-256 and the initramfs it booted.

    ``producer`` gives the initramfs_version and initramfs_sha256 to record.
    Demo 5 passes what booted_initramfs_producer returned before the boot for
    the private copy of the initramfs that QEMU boots, and checks after the boot
    that the copy is unchanged. The record is renamed into place whole.
    """
    record = boot_snapshot_record_path(snapshot)
    _write_json(
        record,
        {
            "format": BOOT_SNAPSHOT_RECORD_FORMAT,
            "snapshot_sha256": snapshot_sha256,
            "initramfs_version": producer["initramfs_version"],
            "initramfs_sha256": producer["initramfs_sha256"],
        },
    )
    return record


def _boot_snapshot_record_problem(record: Any) -> Optional[str]:
    """Why ``record`` is not a record write_boot_snapshot_record writes, or None."""
    if not isinstance(record, dict):
        return "it is not a JSON object"
    record_format = record.get("format")
    if type(record_format) is not int or record_format != BOOT_SNAPSHOT_RECORD_FORMAT:
        return "its format is {!r}, not {}".format(record_format, BOOT_SNAPSHOT_RECORD_FORMAT)
    if type(record.get("initramfs_version")) is not int:
        return "its initramfs_version {!r} is not a whole number".format(
            record.get("initramfs_version")
        )
    for key in ("initramfs_sha256", "snapshot_sha256"):
        value = record.get(key)
        if not isinstance(value, str) or SHA256_RE.fullmatch(value) is None:
            return "its {} {!r} is not a SHA-256 digest".format(key, value)
    return None


def verify_boot_snapshot(
    snapshot: Path, root: Path, assets: Path, disk: Optional[Path] = None
) -> None:
    """Raise BootSnapshotMismatch unless demo 5's record of ``snapshot`` matches.

    The record must name the INITRAMFS_VERSION that qemu-assets.sh under
    ``root`` builds now, the SHA-256 that ``assets``/initramfs.cpio.gz has now,
    and the SHA-256 of ``disk``: ``snapshot`` itself, or the copy of it that
    QEMU will restore. The message says what did not match.
    """
    snapshot = Path(snapshot)
    disk = snapshot if disk is None else Path(disk)
    initramfs = Path(assets) / "initramfs.cpio.gz"
    if not initramfs.is_file():
        raise BootSnapshotMismatch(
            "there is no initramfs at {} to compare it with".format(initramfs)
        )
    record_path = boot_snapshot_record_path(snapshot)
    try:
        record = json.loads(record_path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        raise BootSnapshotMismatch(
            "it has no record of the initramfs it was booted from ({} is missing): "
            "demo 5 saved it before it wrote such records, or demo 5 did not save "
            "it".format(record_path)
        ) from None
    except (OSError, ValueError) as error:
        raise BootSnapshotMismatch(
            "its record {} cannot be read: {}".format(record_path, error)
        ) from None
    problem = _boot_snapshot_record_problem(record)
    if problem is not None:
        raise BootSnapshotMismatch(
            "its record {} is not one demo 5 wrote: {}".format(record_path, problem)
        )
    current = initramfs_producer(root, assets)
    if record["initramfs_version"] != current["initramfs_version"]:
        raise BootSnapshotMismatch(
            "it was booted from initramfs version {}, and demos/lib/qemu-assets.sh "
            "now builds version {}".format(
                record["initramfs_version"], current["initramfs_version"]
            )
        )
    if record["initramfs_sha256"] != current["initramfs_sha256"]:
        raise BootSnapshotMismatch(
            "it was booted from an initramfs with SHA-256 {}, and {} now has "
            "SHA-256 {}".format(
                record["initramfs_sha256"], initramfs, current["initramfs_sha256"]
            )
        )
    disk_sha256 = hash_file(disk)
    if disk_sha256 != record["snapshot_sha256"]:
        raise BootSnapshotMismatch(
            "{} has SHA-256 {}, not the {} that demo 5 recorded for it".format(
                disk, disk_sha256, record["snapshot_sha256"]
            )
        )


def accept_snapshot_after_failed_rebuild(
    snapshot: Path,
    root: Path,
    assets: Path,
    failure: subprocess.CalledProcessError,
    demo: str,
) -> None:
    """Decide whether the default boot snapshot is usable after demo 5 failed.

    Demo 6 and demo 7 run demo 5 to rebuild a default boot snapshot that was
    booted from another initramfs. Demo 5 publishes the snapshot and its record
    before it compares the boot with its saved reference run, and that reference
    no longer applies once the initramfs has changed, so demo 5 can end PARTIAL
    and exit non-zero although the snapshot it has just saved is current
    (demos/05-qemu-boot/README.md). Its exit status therefore does not say
    whether the snapshot is usable; demo 5's record does. Return, after a note,
    when ``snapshot`` now matches its record and the current initramfs
    (verify_boot_snapshot); otherwise raise a RuntimeError that says to run
    demos/clean.sh and then ``demo`` again.
    """
    snapshot = Path(snapshot)
    command = failure.cmd
    if not isinstance(command, str):
        command = " ".join(str(part) for part in command)
    if not snapshot.is_file():
        problem = "{} does not exist".format(snapshot)
    else:
        try:
            verify_boot_snapshot(snapshot, root, assets)
            problem = None
        except BootSnapshotMismatch as mismatch:
            problem = "{} does not match the current initramfs: {}".format(
                snapshot, mismatch
            )
    if problem is not None:
        raise RuntimeError(
            "demo 5 failed while rebuilding the boot snapshot (`{}` exited with "
            "status {}; its output is above), and {}. Run demos/clean.sh, then {} "
            "again".format(command, failure.returncode, problem, demo)
        ) from failure
    print(
        "NOTE: demo 5 failed (`{}` exited with status {}), but it saved {} with a "
        "record that matches the current initramfs, so {} uses that snapshot. If "
        "the initramfs changed since demo 5 saved its reference run, that "
        "reference no longer applies and demo 5 ends PARTIAL against it until you "
        "run demos/clean.sh; its WARN lines above say what differed.".format(
            command, failure.returncode, snapshot, demo
        ),
        flush=True,
    )


def canonicalize_qemu_runtime_paths_in_file(
    path: Path, run_dir: Path, qmp_socket: Path
) -> None:
    """Apply canonicalize_qemu_runtime_path to the bytes of a file, in place.

    The file is read and written as bytes, so every other byte, a carriage
    return or a byte that is not UTF-8 included, is kept exactly. The rewritten
    file replaces the old one in one rename.
    """
    path = Path(path)
    data = path.read_bytes().replace(os.fsencode(str(run_dir)), b"<run-dir>")
    try:
        Path(qmp_socket).relative_to(run_dir)
    except ValueError:
        data = data.replace(os.fsencode(str(qmp_socket)), b"<qmp-socket>")
    temporary = path.with_name("{}.tmp.{}".format(path.name, os.getpid()))
    try:
        temporary.write_bytes(data)
        os.replace(str(temporary), str(path))
    except BaseException:
        try:
            temporary.unlink()
        except FileNotFoundError:
            pass
        raise


# Hermit reads these two variables as its --log and --log-file options
# (hermit-cli/src/bin/hermit/global_opts.rs). HERMIT_LOG is a level that Hermit
# applies to every target RUST_LOG does not name, in place of RUST_LOG's own
# default level (EffectiveFilter in hermit-cli/src/liteinst_bootstrap.rs), so it
# adds or removes records. HERMIT_LOG_FILE sends Hermit's tracing to that file
# instead of standard error, so the captured log would hold no tracing record
# at all. Either would change what the repeat check compares, so the demos
# clear both. HERMIT_LOG_MAX_BYTES is left alone: it bounds only a log file
# (and the --verify and run-evidence logs), never standard error.
HERMIT_LOG_ENVIRONMENT = ("HERMIT_LOG", "HERMIT_LOG_FILE")


def hermit_log_environment(log_filter: str) -> Dict[str, str]:
    """The environment for a Hermit run whose INFO log a demo compares.

    The caller's environment, with RUST_LOG set to ``log_filter`` and without
    the variables in HERMIT_LOG_ENVIRONMENT.
    """
    environment = dict(os.environ)
    for name in HERMIT_LOG_ENVIRONMENT:
        environment.pop(name, None)
    environment["RUST_LOG"] = log_filter
    return environment


# The line Hermit's bounded log writer ends a log with once the log reaches
# HERMIT_LOG_MAX_BYTES (TRUNCATION_MARKER in detcore/src/logdiff.rs).
HERMIT_LOG_TRUNCATION_MARKER = (
    "=== HERMIT LOG TRUNCATED: reached the configured size bound "
    "(HERMIT_LOG_MAX_BYTES). Output beyond this point was DISCARDED. The run "
    "itself continued and was NOT affected. ==="
)


def hermit_log_was_truncated(path: Path) -> bool:
    """Whether the log at ``path`` ends with Hermit's truncation marker.

    The test Hermit's own comparison applies (log_was_truncated in
    detcore/src/logdiff.rs): once trailing newlines and carriage returns are
    set aside, the file ends with the marker, and the marker is a whole line.
    Hermit writes the marker only into a log file it bounds, never to the
    standard error the demos capture; a log that carries it is incomplete
    however it got there, so the comparison refuses it, as Hermit's does.
    """
    marker = HERMIT_LOG_TRUNCATION_MARKER.encode()
    with Path(path).open("rb") as source:
        end = source.seek(0, os.SEEK_END)
        # Set aside the trailing line breaks, a block at a time.
        while end > 0:
            start = max(0, end - 4096)
            source.seek(start)
            content = source.read(end - start).rstrip(b"\r\n")
            if content:
                end = start + len(content)
                break
            end = start
        if end < len(marker):
            return False
        start = max(0, end - len(marker) - 1)
        source.seek(start)
        tail = source.read(end - start)
    return tail.endswith(marker) and (len(tail) == len(marker) or tail[:1] == b"\n")


class _HostAddressOrdinals:
    """Number each Hermit-marked host address by first appearance in one log.

    This is the canonical policy's only address rule (CANON_ADDRESS_ORDINAL_V1,
    `canonicalize_addresses_in_line` in detcore/src/logdiff.rs): a value inside
    `<hostaddr ...>` becomes `<addrN>`, where N counts distinct marked values in
    order of first appearance. Use one instance per log, never shared between
    the two logs being compared. Which marked values repeat, and their order,
    must still agree. Bare hex values, including every guest address, are not
    touched.
    """

    def __init__(self) -> None:
        self._seen: Dict[str, int] = {}

    def substitute(self, line: str) -> str:
        def _ordinal(match: "re.Match[str]") -> str:
            raw = match.group(1)
            ordinal = self._seen.get(raw)
            if ordinal is None:
                ordinal = len(self._seen) + 1
                self._seen[raw] = ordinal
            return "<addr{}>".format(ordinal)

        return HOST_ADDR_RE.sub(_ordinal, line)


@dataclass(frozen=True)
class HermitLogComparison:
    """What compare_hermit_logs found.

    ``difference`` is empty when the logs matched, and otherwise describes the
    first line that differs. ``lines`` and ``info_records`` count, for the
    first and the second log, every line and the lines that start a Hermit INFO
    record (a wall-clock timestamp followed by ``INFO``).
    """

    difference: str
    lines: Tuple[int, int]
    info_records: Tuple[int, int]


def _split_wallclock_prefix(line: str) -> Tuple[bool, str]:
    """Whether ``line`` starts with a wall-clock timestamp, and the rest of it."""
    match = WALLCLOCK_RE.match(line)
    if match is None:
        return False, line
    return True, line[match.end():]


def _shown_log_line(line: Optional[Tuple[bool, str]]) -> str:
    """A compared line as a divergence report shows it; see compare_hermit_logs."""
    if line is None:
        return repr("")
    stamped, text = line
    return repr("<wall-clock> " + text if stamped else text)


def compare_hermit_logs(log1: Path, log2: Path) -> HermitLogComparison:
    """Compare two captured Hermit logs line by line; see HOST_ADDR_RE.

    A line ends only at a newline byte and is decoded without loss, so a
    carriage return or a byte that is not UTF-8 is compared like any other
    byte. Only a wall-clock timestamp at the start of a line is removed
    (whether the line had one is still compared) and only Hermit-marked host
    addresses are numbered. Both logs are read to the end, so the counts cover
    every line even after a difference.
    """
    lines = [0, 0]
    info_records = [0, 0]
    addresses = (_HostAddressOrdinals(), _HostAddressOrdinals())
    before: List[str] = []
    difference = ""
    with Path(log1).open("rb") as first, Path(log2).open("rb") as second:
        for line_number, raw_lines in enumerate(
            itertools.zip_longest(first, second), start=1
        ):
            compared: List[Optional[Tuple[bool, str]]] = []
            for side, raw in enumerate(raw_lines):
                if raw is None:
                    compared.append(None)
                    continue
                lines[side] += 1
                stamped, text = _split_wallclock_prefix(
                    raw.decode("utf-8", "surrogateescape")
                )
                if stamped and text.startswith("INFO "):
                    info_records[side] += 1
                if not difference:
                    compared.append((stamped, addresses[side].substitute(text)))
            if difference:
                continue
            if compared[0] == compared[1]:
                before.append(_shown_log_line(compared[0]))
                before = before[-3:]
                continue
            context = ["  {}".format(item) for item in before]
            context.extend(
                (
                    "- {}".format(_shown_log_line(compared[0])),
                    "+ {}".format(_shown_log_line(compared[1])),
                )
            )
            difference = (
                "first divergence at line {} (only the wall-clock prefix "
                "removed and Hermit-marked host addresses numbered):\n{}"
            ).format(line_number, "\n".join(context))
    return HermitLogComparison(
        difference=difference,
        lines=(lines[0], lines[1]),
        info_records=(info_records[0], info_records[1]),
    )


# The run artifacts compare_runs compares exactly, in report order, each with
# the label its report line uses.
COMPARED_METADATA_FIELDS = (
    ("qemu_version", "QEMU version"),
    ("qemu_binary_sha256", "QEMU binary SHA-256"),
    ("qcow2_sha256", "qcow2 SHA-256"),
    ("serial_sha256", "serial output SHA-256"),
    ("guest_output_sha256", "guest output SHA-256"),
    ("guest_exit_status", "guest command exit status"),
)


def metadata_field_required(
    kind: QemuRunKind,
    schema_version: int,
    snapshot_saved: Optional[bool],
    field: str,
) -> bool:
    """Whether parse_run_metadata requires one of COMPARED_METADATA_FIELDS.

    This restates, for the compared fields only, the rules parse_run_metadata
    enforces for a row of this kind, schema and snapshot state; a test checks
    the two against each other.
    """
    if field == "qemu_version":
        return True
    if kind is QemuRunKind.BOOT:
        return field in ("qemu_binary_sha256", "qcow2_sha256", "serial_sha256")
    if kind is QemuRunKind.RESUME:
        if field == "qemu_binary_sha256":
            return not (schema_version == 1 and snapshot_saved is False)
        if field == "qcow2_sha256":
            return bool(snapshot_saved)
        if field == "guest_output_sha256":
            return True
        if field == "guest_exit_status":
            return schema_version >= 3
    return False


def _row_description(kind: QemuRunKind, schema_version: int, snapshot_saved: Optional[bool]) -> str:
    if kind is QemuRunKind.RESUME:
        return "{} rows of schema {} with snapshot_saved {}".format(
            kind.value, schema_version, "true" if snapshot_saved else "false"
        )
    return "{} rows of schema {}".format(kind.value, schema_version)


def _uncompared_field(
    anchor: QemuRunMetadata, current: QemuRunMetadata, field: str, label: str
) -> Tuple[bool, str]:
    """Report one of COMPARED_METADATA_FIELDS that neither run recorded.

    Such a field is never passed over in silence. When a row of either run's
    shape must record it, or a row the current code writes would, the repeat
    cannot vouch for it and fails. Otherwise the field does not exist for these
    runs and the line says why it was not compared.
    """
    for row in (anchor, current):
        if metadata_field_required(row.kind, row.schema_version, row.snapshot_saved, field):
            return False, (
                "WARN: {} was not compared: neither run recorded it, although {} "
                "must record it".format(
                    label,
                    _row_description(row.kind, row.schema_version, row.snapshot_saved),
                )
            )
    if metadata_field_required(
        current.kind, RUN_METADATA_SCHEMA_VERSION, current.snapshot_saved, field
    ):
        return False, (
            "WARN: {} was not compared: neither run recorded it (first run schema "
            "{}, current run schema {}; {} record it), so this repeat cannot vouch "
            "for it".format(
                label,
                anchor.schema_version,
                current.schema_version,
                _row_description(
                    current.kind, RUN_METADATA_SCHEMA_VERSION, current.snapshot_saved
                ),
            )
        )
    kinds = {anchor.kind, current.kind}
    reason = "neither run recorded it"
    if kinds == {QemuRunKind.RESUME} and field == "qcow2_sha256":
        reason = "neither run saved a snapshot"
    elif kinds == {QemuRunKind.RESUME} and field == "serial_sha256":
        reason = (
            "qemu-resume rows do not record it; the guest command's output, taken "
            "from the serial log, is compared instead"
        )
    elif kinds == {QemuRunKind.BOOT} and field in (
        "guest_output_sha256",
        "guest_exit_status",
    ):
        reason = "qemu-boot runs start no guest command"
    return True, "NOT COMPARED: {}: {}".format(label, reason)


def compare_runs(
    anchor: QemuRunMetadata, current: QemuRunMetadata
) -> Tuple[bool, List[str]]:
    """Compare exact artifacts, and the INFO logs byte for byte apart from the
    two normalizations borrowed from Hermit's canonical comparison (see the
    comment above HOST_ADDR_RE)."""
    passed = True
    report: List[str] = []
    if anchor.kind is not current.kind:
        passed = False
        report.append(
            "WARN: run kind differs from first run: first={} current={}".format(
                anchor.kind.value, current.kind.value
            )
        )
    if anchor.qemu_argv == current.qemu_argv:
        report.append("PASS: QEMU argv matches first run")
    else:
        passed = False
        report.append(
            "WARN: QEMU argv differs from first run; executable path or arguments changed"
        )
    for field, label in COMPARED_METADATA_FIELDS:
        anchor_value = getattr(anchor, field)
        current_value = getattr(current, field)
        if anchor_value is None and current_value is None:
            uncompared_passed, line = _uncompared_field(anchor, current, field, label)
            passed = passed and uncompared_passed
            report.append(line)
        elif anchor_value == current_value:
            report.append("PASS: {} matches ({})".format(label, current_value))
        else:
            passed = False
            report.append(
                "WARN: {} differs from first run: first={} current={}".format(
                    label, anchor_value, current_value
                )
            )

    # Compare the logs with the two canonical normalizations only (see the
    # comment above HOST_ADDR_RE). Any remaining difference, a guest address
    # included, is execution evidence and must fail the repeat, even when the VM
    # artifacts happen to be byte-identical. In particular, a difference that
    # begins during Python startup can propagate into virtual clock values and
    # the QEMU execution; its origin does not make the later guest-visible log
    # evidence optional.
    anchor_log = anchor.info_log
    current_log = current.info_log
    logs = (("first-run", anchor_log), ("current", current_log))
    if not (
        anchor_log
        and current_log
        and Path(anchor_log).is_file()
        and Path(current_log).is_file()
    ):
        passed = False
        report.append(
            "WARN: Hermit INFO logs not compared because the first-run or current "
            "log is unavailable; canonical repeat verification requires both logs"
        )
        return passed, report
    truncated = [name for name, log in logs if hermit_log_was_truncated(Path(log))]
    if truncated:
        passed = False
        for name in truncated:
            report.append(
                "WARN: Hermit INFO logs not compared because the {} log ends with "
                "Hermit's truncation marker (HERMIT_LOG_MAX_BYTES), so part of it "
                "was discarded; canonical repeat verification requires complete "
                "logs".format(name)
            )
        return passed, report
    comparison = compare_hermit_logs(Path(anchor_log), Path(current_log))
    if comparison.difference:
        passed = False
        report.append(
            "WARN: Hermit INFO log differs from first run with only the "
            "wall-clock prefix removed and Hermit-marked host addresses "
            "numbered; canonical repeat verification failed\n{}".format(
                comparison.difference
            )
        )
    for (name, _), count in zip(logs, comparison.info_records):
        if count == 0:
            passed = False
            report.append(
                "WARN: the {} Hermit INFO log holds no Hermit INFO record, so it is "
                "no evidence that the run repeated; QEMU_LOG_FILTER must keep "
                "Hermit's INFO records (the default does){}".format(
                    name,
                    "; remove the saved first run with demos/clean.sh and run again"
                    if name == "first-run"
                    else "",
                )
            )
    if not comparison.difference and all(comparison.info_records):
        report.append(
            "PASS: Hermit INFO log matches first run exactly apart from the "
            "wall-clock prefix (Hermit-marked host addresses compared by "
            "first appearance); compared {:,} lines, {:,} of which start a "
            "Hermit INFO record".format(
                comparison.lines[1], comparison.info_records[1]
            )
        )
    return passed, report


def print_comparison(
    passed: bool,
    report: Sequence[str],
    snapshot_sha256: Optional[str] = None,
    subject: str = "Run",
) -> None:
    for line in report:
        print(line)
    if passed:
        print("PASS: all repeat checks match the first run")
    else:
        print(
            "PARTIAL: workload completed, but repeat verification differs from the first run."
        )
        print("Review the WARN lines above before sharing this artifact.")
    if passed and snapshot_sha256 is not None:
        print()
        print("DETERMINISTIC: snapshot SHA-256 matches the previous run:")
        print("   {}".format(snapshot_sha256))
        print("   {} is bitwise-reproducible under Hermit.".format(subject))


HERMIT_NOT_ON_PATH = (
    "hermit is not on PATH. Build it from this checkout with `make release-core` "
    "and run `export PATH=\"$PWD/target/release:$PATH\"` (see demos/README.md)."
)


def hermit_binary() -> str:
    """Return the `hermit` found on PATH, which is the binary every demo runs."""
    found = shutil.which("hermit")
    if found is None:
        raise RuntimeError(HERMIT_NOT_ON_PATH)
    return found


def check_dependencies(root: Path) -> str:
    """Confirm that a working `hermit` is on PATH and return a summary line.

    ``root`` is the checkout the demo belongs to; it is accepted for symmetry with
    check_qemu_dependencies and is not otherwise needed.
    """
    del root
    hermit = hermit_binary()
    result = subprocess.run(
        [hermit, "--version"],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if result.returncode != 0:
        sys.stderr.write(result.stderr)
        sys.stderr.write(result.stdout)
        raise subprocess.CalledProcessError(result.returncode, result.args)
    lines = [line.strip() for line in result.stdout.splitlines() if line.strip()]
    if not lines or not lines[0].startswith("hermit "):
        raise RuntimeError(
            "unexpected `hermit --version` output: {!r}".format(result.stdout)
        )
    return "Dependency check passed: {} ({})".format(lines[0], hermit)


def check_qemu_dependencies(root: Path) -> str:
    """Run the zero-build QEMU demo preflight and return its summary."""
    result = subprocess.run(
        [str(root / "demos/lib/qemu-assets.sh"), "--check"],
        cwd=str(root),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if result.returncode != 0:
        sys.stderr.write(result.stderr)
        sys.stderr.write(result.stdout)
        raise subprocess.CalledProcessError(result.returncode, result.args)
    lines = [line for line in result.stdout.splitlines() if line]
    if len(lines) != 1 or not lines[0].startswith(
        "QEMU dependency check passed:"
    ):
        raise RuntimeError(
            "unexpected QEMU dependency-check output: {!r}".format(result.stdout)
        )
    return lines[0]


def print_header(title: str, description: Sequence[str], dependency: str) -> None:
    width = 80
    title_width = width - 10
    if len(title) > title_width:
        raise ValueError("demo title is too wide: {}".format(title))
    print("=" * width)
    print("=====" + title.center(title_width) + "=====")
    print()
    for line in description:
        if len(line) > width:
            raise ValueError("demo description is too wide: {}".format(line))
        print(line)
    print(dependency)
    print()
    print("=" * width)


def banner(title: str) -> None:
    print("\n=== {} ===".format(title), flush=True)


def run_checked(command: Sequence[str], cwd: Optional[Path] = None) -> None:
    subprocess.run(list(command), cwd=str(cwd) if cwd else None, check=True)


def make_run_dir(parent: Path, prefix: str) -> Path:
    timestamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")
    run_dir = (
        Path(parent) / "run-history" / "{}-{}-{}".format(prefix, timestamp, os.getpid())
    )
    run_dir.mkdir(parents=True, exist_ok=False)
    return run_dir


class LogCapExceeded(RuntimeError):
    """A run's log grew past its cap, and the run's process group was signalled.

    wait_for_process raises it while the launched process runs. drain_output
    raises it after that process exited while processes it left behind still
    wrote to the log; ``exit_status`` is then the launched process's exit status.

    Each also checks the size once more before it returns: wait_for_process
    once the launched process has exited, drain_output once the copy of its
    output has ended. A log past the cap then raises this with
    ``final_check`` set and ``exit_status`` the launched process's exit
    status. Nothing was then seen still writing to the log, so the message
    says only when the size was checked.

    Either raises it after stop_process_group, which signals the launched
    process's process group (SIGTERM, then SIGKILL) while the launched process
    is unreaped, as it is in both. The message says that the demo signalled
    the group and no more: stop_process_group does not report whether the
    group emptied, and its signals reach no process outside that group, such
    as one a wrapper started in a session of its own.

    ``elapsed`` is read when the size is found past the cap, before the group
    is signalled, so the time the signals and their waits take is not counted
    in it.
    """

    def __init__(
        self,
        log_path: Path,
        log_size: int,
        max_log_bytes: int,
        elapsed: float,
        exit_status: Optional[int] = None,
        final_check: bool = False,
    ):
        if final_check:
            message = (
                "{} was {} bytes, past the {}-byte log cap, when checked after the "
                "launched process exited with status {}; the demo then signalled "
                "its process group".format(
                    log_path, log_size, max_log_bytes, exit_status
                )
            )
        elif exit_status is None:
            message = (
                "{} grew to {} bytes, past the {}-byte log cap; the demo then "
                "signalled the launched process's process group".format(
                    log_path, log_size, max_log_bytes
                )
            )
        else:
            message = (
                "{} grew to {} bytes, past the {}-byte log cap, after the launched "
                "process exited with status {}, while processes still wrote to it; "
                "the demo then signalled the launched process's process "
                "group".format(log_path, log_size, max_log_bytes, exit_status)
            )
        super().__init__(message)
        self.log_path = Path(log_path)
        self.log_size = log_size
        self.max_log_bytes = max_log_bytes
        # Seconds from the start of the run (or of the wait) until the size was
        # found past the cap, read before the group was signalled.
        self.elapsed = elapsed
        self.exit_status = exit_status
        self.final_check = final_check


def _log_size(log_path: Path) -> int:
    """The size of ``log_path`` in bytes, or 0 while it does not exist."""
    try:
        return Path(log_path).stat().st_size
    except FileNotFoundError:
        return 0


def wait_for_process(
    process: subprocess.Popen,
    timeout: float,
    stream_path: Optional[Path] = None,
    progress_label: Optional[str] = None,
    first_output_label: Optional[str] = None,
    log_path: Optional[Path] = None,
    max_log_bytes: Optional[int] = None,
) -> int:
    """Wait for a process, optionally streaming a growing file or showing progress.

    ``timeout`` bounds the wall time: past it, the process group is signalled
    (stop_process_group) and TimeoutError is raised. When ``log_path`` and
    ``max_log_bytes`` are both set, the file is also watched: if it grows past
    the cap, the process group is signalled and LogCapExceeded (a RuntimeError)
    names the cap, so a runaway log cannot fill the disk. The size is checked
    once more after the process exits, before its exit status is returned, so
    output written since the previous check is held to the cap too. Either way
    the group is signalled before the error is raised, so that what is in it
    stops writing before the caller cleans up; whether it emptied is not
    reported, and a process outside the group is not signalled. The caller
    is expected to have started the process with ``start_new_session=True``;
    once the process exits, drain_output keeps the same cap while the rest of
    its output is copied.

    The exit status is returned without reaping the process: until it is
    reaped, Linux gives its PID, and the process group ID it leads, to no other
    process, so a later stop_process_group cannot signal one that reused them.
    The caller reaps it with stop_process_group (or stop_process) when it is
    done with the group.

    When ``first_output_label`` is set (used with ``stream_path``), a live
    seconds-counter ticks until the very first byte of streamed output appears,
    then freezes as ``(N.Ns to first output)``. For the QEMU boot demo this makes
    the healthy ~10-20s time-to-first-serial-line obvious at a glance and turns a
    wedged boot (counter climbing toward the timeout with no output) into an
    immediately visible symptom.
    """
    deadline = time.monotonic() + timeout
    started = time.monotonic()
    stream = None
    last_progress = -1
    last_wait_tick = -1.0
    first_output_at: Optional[float] = None

    def note_first_output() -> None:
        nonlocal first_output_at
        if first_output_label is not None and first_output_at is None:
            first_output_at = time.monotonic() - started
            # Freeze the ticking counter on its own line, then let the streamed
            # output follow on the next line.
            print(
                "\r{}: {:.1f}s  ({:.1f}s to first output)".format(
                    first_output_label, first_output_at, first_output_at
                ),
                flush=True,
            )
            # Stable, greppable marker so timing harnesses can recover the
            # frozen time-to-first-output from a log without parsing the
            # human-facing counter line (which carries a leading '\r').
            print(
                "FIRST_OUTPUT_ELAPSED={:.1f}s".format(first_output_at),
                flush=True,
            )

    try:
        while True:
            if (
                stream is None
                and stream_path is not None
                and Path(stream_path).is_file()
            ):
                stream = Path(stream_path).open("rb")
            if stream is not None:
                chunk = stream.read()
                if chunk:
                    note_first_output()
                    sys.stdout.buffer.write(chunk)
                    sys.stdout.buffer.flush()

            return_code = _exit_status_without_reaping(process)
            if return_code is not None:
                if stream is not None:
                    chunk = stream.read()
                    if chunk:
                        note_first_output()
                        sys.stdout.buffer.write(chunk)
                        sys.stdout.buffer.flush()
                # The process may have written past the cap, and exited, since
                # the previous check; that output is checked here, before the
                # exit status is returned.
                if log_path is not None and max_log_bytes is not None:
                    log_size = _log_size(log_path)
                    if log_size > max_log_bytes:
                        elapsed = time.monotonic() - started
                        stop_process_group(process)
                        raise LogCapExceeded(
                            Path(log_path),
                            log_size,
                            max_log_bytes,
                            elapsed,
                            exit_status=return_code,
                            final_check=True,
                        )
                if progress_label is not None:
                    done_elapsed = time.monotonic() - started
                    print(
                        "\r{}: done ({:.1f}s)".format(
                            progress_label, done_elapsed
                        )
                    )
                    # Stable, greppable end-of-timer marker.
                    print(
                        "TIMER_DONE label={} elapsed={:.1f}s".format(
                            progress_label, done_elapsed
                        ),
                        flush=True,
                    )
                return return_code

            if log_path is not None and max_log_bytes is not None:
                log_size = _log_size(log_path)
                if log_size > max_log_bytes:
                    # Read when the cap was seen, not after the group is signalled.
                    elapsed = time.monotonic() - started
                    stop_process_group(process)
                    raise LogCapExceeded(
                        Path(log_path),
                        log_size,
                        max_log_bytes,
                        elapsed,
                    )

            now = time.monotonic()
            if now >= deadline:
                stop_process_group(process)
                raise TimeoutError("process exceeded timeout of {}s".format(timeout))
            if progress_label is not None:
                elapsed = int(now - started)
                if elapsed != last_progress:
                    print(
                        "\r{}: {}s".format(progress_label, elapsed), end="", flush=True
                    )
                    last_progress = elapsed
            if first_output_label is not None and first_output_at is None:
                waited = now - started
                if waited - last_wait_tick >= 0.1:
                    print(
                        "\r{}: {:.1f}s".format(first_output_label, waited),
                        end="",
                        flush=True,
                    )
                    last_wait_tick = waited
            time.sleep(0.1)
    finally:
        if stream is not None:
            stream.close()


def acquire_demo_lock(path: Path) -> Any:
    """Acquire the single-writer lock for fixed QEMU runtime paths."""
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    handle = path.open("a+")
    try:
        fcntl.flock(handle.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        handle.close()
        raise RuntimeError("another QEMU demo is already using {}".format(path))
    return handle


def release_demo_lock(handle: Any) -> None:
    if handle is None:
        return
    fcntl.flock(handle.fileno(), fcntl.LOCK_UN)
    handle.close()


def wait_for_socket(path: Path, process: subprocess.Popen, timeout: float) -> None:
    deadline = time.monotonic() + timeout
    path = Path(path)
    while time.monotonic() < deadline:
        if path.exists() and stat.S_ISSOCK(path.stat().st_mode):
            return
        if process.poll() is not None:
            raise RuntimeError("Hermit exited before socket appeared: {}".format(path))
        time.sleep(0.1)
    raise TimeoutError("timed out waiting for socket: {}".format(path))


def qmp_command(
    socket_path: Path,
    execute: str,
    argument_name: Optional[str] = None,
    argument_value: Optional[str] = None,
    blocking: bool = False,
) -> Any:
    connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    if not blocking:
        connection.settimeout(float(os.environ.get("DEMO_QMP_TIMEOUT", "300")))
    connection.connect(str(socket_path))
    stream = connection.makefile("rwb", buffering=0)

    def receive(message_id: str) -> Any:
        while True:
            line = stream.readline()
            if not line:
                raise RuntimeError("QMP disconnected before replying")
            message = json.loads(line)
            if message.get("id") != message_id:
                continue
            if "error" in message:
                raise RuntimeError(str(message["error"]))
            return message.get("return")

    greeting = json.loads(stream.readline())
    if "QMP" not in greeting:
        raise RuntimeError("invalid QMP greeting: {!r}".format(greeting))
    stream.write(
        json.dumps({"execute": "qmp_capabilities", "id": "caps"}).encode() + b"\n"
    )
    receive("caps")
    request: Dict[str, Any] = {"execute": execute, "id": "command"}
    if argument_name:
        request["arguments"] = {argument_name: argument_value}
    stream.write(json.dumps(request).encode() + b"\n")
    result = receive("command")
    stream.close()
    connection.close()
    return result


class SerialSession:
    """Bidirectional Unix serial connection with streaming transcript capture."""

    def __init__(
        self, socket_path: Path, transcript: Path, stream_output: bool
    ) -> None:
        self.socket = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.socket.connect(str(socket_path))
        self.socket.settimeout(0.5)
        self.transcript_path = Path(transcript)
        self.transcript = self.transcript_path.open("wb")
        self.stream_output = stream_output
        self.buffer = bytearray()
        self.condition = threading.Condition()
        self.stopped = False
        self.thread = threading.Thread(target=self._read, daemon=True)
        self.thread.start()

    def _read(self) -> None:
        try:
            while not self.stopped:
                try:
                    chunk = self.socket.recv(65536)
                except socket.timeout:
                    continue
                except OSError:
                    break
                if not chunk:
                    break
                self.transcript.write(chunk)
                self.transcript.flush()
                if self.stream_output:
                    sys.stdout.buffer.write(chunk)
                    sys.stdout.buffer.flush()
                with self.condition:
                    self.buffer.extend(chunk)
                    self.condition.notify_all()
        finally:
            with self.condition:
                self.condition.notify_all()

    def wait_for(self, marker: str, timeout: float, count: int = 1) -> None:
        marker_bytes = marker.encode()
        deadline = time.monotonic() + timeout
        with self.condition:
            while self.buffer.count(marker_bytes) < count:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError(
                        "timed out waiting for serial marker: {!r}".format(marker)
                    )
                self.condition.wait(min(remaining, 0.5))

    def send_line(self, line: str) -> None:
        self.socket.sendall(line.encode() + b"\n")

    def bytes(self) -> bytes:
        with self.condition:
            return bytes(self.buffer)

    def close(self) -> None:
        self.stopped = True
        try:
            self.socket.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        self.socket.close()
        self.thread.join(timeout=5)
        self.transcript.close()


def _exit_status_without_reaping(process: subprocess.Popen) -> Optional[int]:
    """The exit status of ``process`` once it has exited, else None; never reaps it.

    The status is what Popen.returncode would hold: the exit code, or minus the
    number of the signal that ended the process. Popen.poll() and Popen.wait()
    reap the child, after which Linux may give its PID, and the process group
    ID it led, to a new process. This asks with waitid(WNOWAIT) instead, which
    leaves an exited child a zombie: until it is reaped, its PID and its group
    ID remain its own, so a signal sent to either cannot reach another process.
    """
    if process.returncode is not None:
        return process.returncode
    try:
        info = os.waitid(os.P_PID, process.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)
    except ChildProcessError:
        # Reaped elsewhere, not through this Popen; let it record that.
        return process.poll()
    if info is None:
        return None
    if info.si_code == os.CLD_EXITED:
        return info.si_status
    return -info.si_status


def _wait_without_reaping(process: subprocess.Popen, timeout: float) -> Optional[int]:
    """Wait up to ``timeout`` seconds for ``process`` to exit, without reaping it.

    Returns its exit status, or None if it is still running.
    """
    deadline = time.monotonic() + timeout
    delay = 0.0005
    while True:
        status = _exit_status_without_reaping(process)
        if status is not None:
            return status
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return None
        delay = min(delay * 2, remaining, 0.05)
        time.sleep(delay)


def _signal_unreaped(process: subprocess.Popen, group: Optional[int], sig: int) -> None:
    """Send ``sig`` to process group ``group``, or else to ``process`` alone.

    Only for a ``process`` that has not been reaped, so that neither number can
    belong to anything else. ``group`` is ``process``'s PID when it leads its
    own group, else None.
    """
    if group is not None:
        try:
            os.killpg(group, sig)
            return
        except (ProcessLookupError, PermissionError):
            pass
    try:
        os.kill(process.pid, sig)
    except OSError:
        pass


def _stop_unreaped(process: subprocess.Popen) -> None:
    """stop_process without the reaping: the child is left for the caller to reap.

    Sends nothing if ``process`` had already exited when this was called, and
    nothing more once it turns out to have been reaped elsewhere (its numbers
    may then belong to another process already).

    The SIGKILL is sent even if the SIGTERM or the wait after it raises (a
    KeyboardInterrupt during the wait, for example): that exception is raised
    again once the SIGKILL has been sent and waited for.
    """
    if _exit_status_without_reaping(process) is not None:
        return
    group: Optional[int] = None
    try:
        if os.getpgid(process.pid) == process.pid:
            group = process.pid
    except OSError:
        group = None
    try:
        _signal_unreaped(process, group, signal.SIGTERM)
        _wait_without_reaping(process, 10)
    finally:
        # Asks again first: the wait may have stopped before it learned that
        # the child was reaped elsewhere, after which nothing more is sent.
        _exit_status_without_reaping(process)
        if process.returncode is None:
            # Sent even if the child has exited meanwhile: the rest of its group
            # may still be running. The child is still unreaped, so the group ID
            # is its own.
            _signal_unreaped(process, group, signal.SIGKILL)
            _wait_without_reaping(process, 10)


def stop_process(process: Optional[subprocess.Popen]) -> None:
    """Stop a launched child, and its descendants when it leads its own group.

    Signalling one PID is not enough for a Hermit run: Hermit supervises a traced
    process tree, and a surviving Hermit process can keep writing its log after
    the one we were handed has exited. Killing the child's process group reaches
    the whole tree. That is only safe when the child leads a group of its own
    (otherwise the group is ours and we would kill the demo), so callers launch
    long-running children with ``start_new_session=True``.

    SIGTERM, up to 10 seconds for the child to exit, then SIGKILL and up to 10
    more. The child is reaped only after the SIGKILL has been sent: until then
    Linux gives its PID, and the group ID it leads, to no other process, so
    neither signal can reach a process that reused them. Nothing is sent to a
    child that has already exited; it is only reaped. If a wait raises (a
    KeyboardInterrupt, for example), the SIGKILL is still sent and the child
    still reaped after it, and then the exception is raised again.
    """
    if process is None or process.returncode is not None:
        return
    try:
        _stop_unreaped(process)
    finally:
        process.poll()


def _may_signal(pid: int) -> bool:
    """Whether this process may signal ``pid``, asked through a pidfd with signal 0.

    Signal 0 delivers nothing. The answer is False when ``pid`` no longer
    exists or the signal is refused for permission, and True when it is
    accepted. It is also True when the question cannot be asked (no pidfd_open,
    too many open files) or fails some other way: a failed observation is not
    an absence.
    """
    try:
        pidfd = os.pidfd_open(pid)
    except ProcessLookupError:
        return False
    except (AttributeError, OSError):
        return True  # Cannot ask; count it.
    try:
        signal.pidfd_send_signal(pidfd, 0)
    except (ProcessLookupError, PermissionError):
        return False
    except OSError:
        return True
    finally:
        os.close(pidfd)
    return True


def _unreadable_stat_counts(pid: int, error: OSError) -> bool:
    """Whether a process whose /proc/<pid>/stat could not be opened or read may be a member.

    ``error`` is what the open or the read raised. A process that no longer
    exists (ENOENT, ESRCH) does not count. EPERM or EACCES is what /proc
    mounted with hidepid=1, or a security module, answers for another user's
    process: its group cannot be read, but whether this process may signal it
    can still be asked (_may_signal), and a process it may not signal does not
    count, whatever its group. Any other failure counts: the group is unknown.
    """
    if error.errno in (errno.ENOENT, errno.ESRCH):
        return False
    if error.errno in (errno.EPERM, errno.EACCES):
        return _may_signal(pid)
    return True  # Cannot tell; count it.


def _other_group_members(group: int, leader: int) -> bool:
    """Whether a process other than ``leader`` may be in process group ``group``.

    Reads the group ID of every process in /proc. A member that has exited but
    has not been reaped counts, as it does for kill(). A member this process may
    not signal does not count: nothing more can be done about it from here.
    Permission is asked through a pidfd with signal 0, which delivers nothing.
    A process whose stat cannot be read counts, unless the read failed because
    the process no longer exists, or because the stat is refused (EPERM or
    EACCES: hidepid=1 on /proc, or a security module) and this process may not
    signal it either (see _unreadable_stat_counts): a failed observation is not
    an absence. For the same reason, if /proc itself cannot be opened or listed
    to the end (too many open files, for example), the answer is that a member
    may remain.

    The answer is a sample of a moment: it only bounds a wait, and never decides
    whether a signal is sent (see stop_process_group).
    """
    wanted = str(group).encode()
    try:
        entries = os.scandir("/proc")
    except OSError:
        return True  # /proc could not be opened: cannot tell; count it.
    while True:
        try:
            entry = next(entries, None)
        except OSError:
            return True  # The listing failed part-way: cannot tell; count it.
        if entry is None:
            return False
        name = entry.name
        if not name.isdigit() or int(name) == leader:
            continue
        try:
            descriptor = os.open("/proc/{}/stat".format(name), os.O_RDONLY)
        except OSError as error:
            if _unreadable_stat_counts(int(name), error):
                return True
            continue
        try:
            data = os.read(descriptor, 4096)
        except OSError as error:
            if _unreadable_stat_counts(int(name), error):
                return True
            continue
        finally:
            os.close(descriptor)
        # The command name (field 2) is in parentheses and may contain spaces
        # and parentheses; state, parent and process group follow the last ")".
        fields = data[data.rfind(b")") + 2 :].split(b" ", 3)
        if len(fields) < 3 or fields[2] != wanted:
            continue
        if _may_signal(int(name)):
            return True


def _group_empty(process: subprocess.Popen, group: int, timeout: float) -> bool:
    """Whether ``process`` exits and its process group ``group`` empties within ``timeout`` seconds.

    ``process`` leads the group and counts until it has exited; it is not
    reaped here, which keeps the group ID its own. Any other member that has
    exited but not yet been reaped still counts; a member this process may not
    signal does not (see _other_group_members).
    """
    deadline = time.monotonic() + timeout
    while True:
        if _exit_status_without_reaping(process) is not None and not _other_group_members(
            group, process.pid
        ):
            return True
        if time.monotonic() >= deadline:
            return False
        time.sleep(0.05)


def stop_process_group(process: Optional[subprocess.Popen]) -> None:
    """Stop a launched child and everything left in its process group, then reap the child.

    stop_process does nothing once the child has exited, but processes the
    child started stay in its group and can keep running, still writing to the
    output they inherited from it. This stops the child if it is still running
    (as stop_process does), then signals the group itself: SIGTERM, up to 10
    seconds for the group to empty, then SIGKILL and up to 10 more. The
    SIGKILL is sent whenever the child is still unreaped, whatever the wait
    saw: the wait reads /proc, which is a sample and can miss a member, so it
    only shortens the wait. ESRCH from the SIGKILL means the group was empty.

    It is meant for children started with ``start_new_session=True``, whose
    group ID is the child's PID. Linux gives that number to no other process
    while the child is unreaped, even after it has exited, and the child is
    reaped only here, after the last signal (wait_for_process and drain_output
    leave it unreaped), so every signal reaches only the child's own group.
    A child that was reaped before this call may have had its PID and group ID
    given to another process since, so nothing is sent then. The caller's own
    group is never signalled.

    No exception from the steps before it can skip the group's SIGKILL or the
    reap after it: if stopping the child, the SIGTERM or the wait after it
    raises (a KeyboardInterrupt, for example), the SIGKILL is still sent while
    the child is unreaped, the child is still reaped after it, and then the
    exception is raised again. An exception in the wait after the SIGKILL
    does not skip the reap either.
    """
    if process is None or process.returncode is not None:
        return
    group = process.pid
    # Never the caller's own group.
    own_group = group == os.getpgrp()
    try:
        try:
            _stop_unreaped(process)
            if not own_group:
                _signal_group_while_unreaped(process, group, signal.SIGTERM)
        finally:
            if not own_group:
                _signal_group_while_unreaped(process, group, signal.SIGKILL)
    finally:
        process.poll()


def _signal_group_while_unreaped(process: subprocess.Popen, group: int, sig: int) -> None:
    """Send ``sig`` to ``process``'s group ``group``, then wait up to 10 seconds for it to empty.

    One step of stop_process_group. Nothing is sent once ``process`` turns out
    to have been reaped elsewhere.
    """
    # Learns whether the child was reaped elsewhere meanwhile, after which its
    # group ID may be another process's: nothing more is sent.
    _exit_status_without_reaping(process)
    if process.returncode is not None:
        return
    try:
        os.killpg(group, sig)
    except ProcessLookupError:
        pass  # ESRCH: nothing is left in the group.
    except PermissionError:
        pass  # No member may be signalled from here.
    _group_empty(process, group, 10)


def drain_output(
    copier: threading.Thread,
    process: subprocess.Popen,
    timeout: float,
    log_path: Optional[Path] = None,
    max_log_bytes: Optional[int] = None,
    started: Optional[float] = None,
    label: str = "the launched process",
) -> None:
    """After ``process`` exited, wait for ``copier`` to copy the rest of its output.

    ``copier`` is the thread that copies the process's output pipe into a log.
    Processes the child left in its process group inherit that pipe, so they
    can keep it open, and keep writing to it, after the child exits. While
    waiting, the log is held to the cap wait_for_process applies: once it
    grows past ``max_log_bytes``, the child's group is signalled
    (stop_process_group) and LogCapExceeded is raised, with ``elapsed``
    counted from ``started`` (a time.monotonic() value; by default, when this
    call began). The size is checked once more when the copy ends, before this
    returns, so output that was all copied between two checks is held to the
    cap too. If the output is still open after ``timeout`` seconds, the group
    is signalled and RuntimeError is raised, naming ``label``. Neither message
    says the processes stopped: stop_process_group does not report it, and a
    process outside the group, which may be the one holding the output, is not
    signalled.

    ``process`` must not have been reaped yet: wait for it with
    wait_for_process, not Popen.wait() or Popen.poll(). The group can only be
    signalled safely while its leader is unreaped (see stop_process_group), so
    a reaped ``process`` raises ValueError before anything is waited for.
    """
    if process.returncode is not None:
        raise ValueError(
            "drain_output needs {} unreaped, but it was already reaped (exit status "
            "{}), so the processes it left in its group can no longer be signalled "
            "safely; wait for it with wait_for_process".format(label, process.returncode)
        )
    began = time.monotonic()
    if started is None:
        started = began
    deadline = began + timeout
    while True:
        copier.join(0.1)
        if not copier.is_alive():
            # The copy has ended, so nothing more reaches the log through it.
            # Output copied since the previous check is checked here.
            if log_path is not None and max_log_bytes is not None:
                log_size = _log_size(log_path)
                if log_size > max_log_bytes:
                    elapsed = time.monotonic() - started
                    stop_process_group(process)
                    raise LogCapExceeded(
                        Path(log_path),
                        log_size,
                        max_log_bytes,
                        elapsed,
                        exit_status=process.returncode,
                        final_check=True,
                    )
            return
        if log_path is not None and max_log_bytes is not None:
            log_size = _log_size(log_path)
            if log_size > max_log_bytes:
                # Read when the cap was seen, not after the group is signalled.
                elapsed = time.monotonic() - started
                stop_process_group(process)
                raise LogCapExceeded(
                    Path(log_path),
                    log_size,
                    max_log_bytes,
                    elapsed,
                    exit_status=process.returncode,
                )
        if time.monotonic() >= deadline:
            stop_process_group(process)
            raise RuntimeError(
                "{}'s output was still open {}s after it exited, so processes "
                "still held it; the demo then signalled {}'s process group".format(
                    label, timeout, label
                )
            )


def extract_info_tail(log_path: Path) -> List[str]:
    """Extract the final COMMIT, shutdown sequence, and compact run report."""
    values: Dict[str, str] = {}
    kills: List[str] = []
    report: List[str] = []
    with Path(log_path).open(errors="replace") as source:
        for raw_line in source:
            line = WALLCLOCK_RE.sub("", raw_line.rstrip("\n"))
            if line.startswith(" COMMIT turn "):
                values["commit"] = line
            elif "Scheduler authorized" in line:
                values["authorized"] = line
            elif "tail_inject of syscall:" in line:
                values["tail_inject"] = line
            elif "logically_kill:" in line:
                kills.append(line)
                kills = kills[-2:]
            elif "scheduler (step2_process_blocked):" in line:
                values["blocked"] = line
            elif "[scheduler] run queue empty" in line:
                values["empty"] = line
            elif "detcore shut down" in line:
                values["shutdown"] = line
            elif (
                "hermit run report" in line
                or line.startswith("Final thread-tree")
                or line.startswith("There were ")
                or line.startswith("Internally,")
                or line.startswith("Final virtual global (cpu) time:")
                or line.startswith("Elapsed virtual global (cpu) time:")
                or line.startswith("Timeslice stats:")
            ):
                report.append(line)
    result = [
        values[key] for key in ("commit", "authorized", "tail_inject") if key in values
    ]
    result.extend(kills)
    result.extend(
        values[key] for key in ("blocked", "empty", "shutdown") if key in values
    )
    result.extend(report)
    return result


def copy_file(source: Path, destination: Path) -> None:
    Path(destination).parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(str(source), str(destination))
