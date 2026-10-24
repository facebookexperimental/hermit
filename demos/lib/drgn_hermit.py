#!/usr/bin/env python3
"""Read a paused Hermit/QEMU Linux guest through drgn without ptrace attach.

The helper restores demo 5's QEMU boot snapshot with its CPUs paused.  Hermit's
exact tracer thread group is then stopped while QEMU is in ptrace-stop, and
guest physical reads are served from QEMU's RAM mmap through
``/proc/<qemu-pid>/mem``.  Only :meth:`advance` resumes the tracer and guest.
"""

from contextlib import contextmanager
from dataclasses import dataclass
import json
import lzma
import os
from pathlib import Path
import re
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time
from typing import Callable, Dict, Iterator, List, Optional, Tuple

from demo_common import (
    accept_snapshot_after_failed_rebuild,
    BootSnapshotMismatch,
    hermit_tmp_args,
    make_socket_path,
    verify_boot_snapshot,
)
from qemu_controller import KERNEL_COMMAND_LINE


XZ_MAGIC = b"\xfd7zXZ\x00"
ELF_MAGIC = b"\x7fELF"
VMCOREINFO_MARKER = b"OSRELEASE="
COMMAND_IMAGE_BYTES = 4096
# The guest's init reads only the first 512 bytes of the command disk
# (`dd bs=512 count=1` in demos/lib/qemu-assets.sh), so the command and the
# newline that ends it must fit in those 512 bytes; a longer command would run
# cut off at byte 512.
GUEST_COMMAND_READ_BYTES = 512
MAX_GUEST_COMMAND_BYTES = GUEST_COMMAND_READ_BYTES - 1
# A failed pass prints at most this much of the end of its Hermit log: the
# error that ends the pass names only the step that failed, and Hermit's or
# QEMU's own reason is in the log.
FAILED_LOG_TAIL_LINES = 40
FAILED_LOG_TAIL_BYTES = 64 << 10
# close() kills QEMU and Hermit's tracer one by one, each through a pidfd, and
# waits at most this long for both to exit, then at most HERMIT_EXIT_SECONDS
# for the process the demo started (Hermit, or a wrapper that runs it) to exit
# by itself.
OWNED_PROCESS_EXIT_SECONDS = 10.0
HERMIT_EXIT_SECONDS = 10.0
# The demo's virtual-time epoch, as demos 05 and 06 pass theirs: `hermit run
# --strict` refuses an epoch drawn from the host clock.
EPOCH = "2026-01-01T00:00:00Z"


def _write_command_image(path: Path, command: str) -> None:
    """Write the fixed-size command disk consumed by the resumed guest."""
    if "\n" in command or "\r" in command:
        raise ValueError("command-disk command must be one line")
    encoded = command.encode("utf-8")
    if len(encoded) > MAX_GUEST_COMMAND_BYTES:
        raise ValueError(
            "guest command is {} bytes; the guest reads only the first {} bytes "
            "of its command disk, so a command can be at most {} bytes (UTF-8) "
            "plus the newline after it".format(
                len(encoded), GUEST_COMMAND_READ_BYTES, MAX_GUEST_COMMAND_BYTES
            )
        )
    payload = encoded + b"\n"
    path.write_bytes(payload + b"\0" * (COMMAND_IMAGE_BYTES - len(payload)))


@dataclass(frozen=True)
class GuestConfig:
    root: Path
    hermit: Path
    qemu: Path
    kernel: Path
    initrd: Path
    vmlinux: Path
    # The boot snapshot, by the name ensure_boot_snapshot checked. Demo 5's
    # record of it is read next to this name (verify_boot_snapshot reads
    # <snapshot_disk>.producer.json), so a symlinked name must not be resolved:
    # the record next to the symlink's target is not the one that was checked.
    snapshot_disk: Path
    snapshot_name: str
    advance_command: str
    artifact_dir: Path
    # The asset directory whose initramfs.cpio.gz demo 5's record of
    # snapshot_disk must name (verify_boot_snapshot).
    assets: Path
    qemu_bios: Optional[Path] = None
    qemu_library_path: Optional[Path] = None
    timeout: float = 240.0
    ram_bytes: int = 512 << 20


@dataclass(frozen=True)
class ObservationMetrics:
    physical_reads: int
    physical_bytes: int
    qemu_state: str
    tracer_state: str
    serial_bytes_delta: int


class QmpClient:
    """Small synchronous QMP client tolerant of interleaved events."""

    def __init__(self, connection: socket.socket) -> None:
        self.connection = connection
        self.stream = connection.makefile("rwb", buffering=0)
        greeting = self._read_message()
        if "QMP" not in greeting:
            raise RuntimeError("QMP greeting was not received")
        self.execute("qmp_capabilities")

    @classmethod
    def connect(
        cls, path: Path, process: subprocess.Popen, timeout: float
    ) -> "QmpClient":
        deadline = time.monotonic() + timeout
        last_error = None
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise RuntimeError(
                    "Hermit exited before QMP connected (status {})".format(
                        process.returncode
                    )
                )
            connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            connection.settimeout(min(5.0, timeout))
            try:
                connection.connect(str(path))
                return cls(connection)
            except (FileNotFoundError, ConnectionRefusedError, socket.timeout) as error:
                last_error = error
                connection.close()
                time.sleep(0.05)
        raise TimeoutError("QMP socket did not become ready: {} ({})".format(path, last_error))

    def _read_message(self):
        while True:
            line = self.stream.readline()
            if not line:
                raise RuntimeError("QMP disconnected")
            try:
                return json.loads(line.decode("utf-8"))
            except json.JSONDecodeError:
                continue

    def execute(self, command: str, arguments=None):
        request = {"execute": command}
        if arguments:
            request["arguments"] = arguments
        self.stream.write(json.dumps(request, separators=(",", ":")).encode() + b"\n")
        while True:
            response = self._read_message()
            if "event" in response:
                continue
            if "error" in response:
                raise RuntimeError("QMP {} failed: {}".format(command, response["error"]))
            if "return" in response:
                return response["return"]

    def status(self) -> str:
        result = self.execute("query-status")
        return str(result.get("status", ""))

    def close(self) -> None:
        try:
            self.stream.close()
        finally:
            self.connection.close()


def _atomic_write(path: Path, data: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary = tempfile.mkstemp(prefix=".vmlinux.", dir=str(path.parent))
    try:
        with os.fdopen(descriptor, "wb") as output:
            output.write(data)
        os.chmod(temporary, 0o644)
        os.replace(temporary, path)
    finally:
        try:
            os.unlink(temporary)
        except FileNotFoundError:
            pass


def ensure_boot_snapshot(
    snapshot: Path, root: Path, assets: Path, rebuild: Callable[[], None]
) -> None:
    """Use demo 5's boot snapshot only if it was booted from the current initramfs.

    These are demo 6's rules (ensure_boot_snapshot in
    demos/06-qemu-resume/run.py). The snapshot's memory holds the guest /init
    that runs demo 7's command, so a snapshot booted from another initramfs runs
    another /init (see verify_boot_snapshot). The default snapshot,
    ``assets``/hermit-boot.qcow2, is built, or rebuilt, by ``rebuild``, which
    runs demo 5. A snapshot named by DEMO07_SNAPSHOT_DISK is never built or
    replaced here: if it is missing or does not match, this refuses and says how
    to rebuild it. Its existence alone is never enough.
    """
    snapshot = Path(snapshot)
    default_snapshot = Path(assets) / "hermit-boot.qcow2"
    custom = snapshot != default_snapshot
    stale = False
    if not snapshot.is_file():
        if custom:
            raise RuntimeError(
                "missing custom boot snapshot: {}; produce it before demo 7".format(
                    snapshot
                )
            )
        print("Demo 5 boot snapshot missing; running demo 5 first...", flush=True)
    else:
        try:
            verify_boot_snapshot(snapshot, root, assets)
            return
        except BootSnapshotMismatch as mismatch:
            if custom:
                raise RuntimeError(
                    "refusing to restore the custom boot snapshot {} "
                    "(DEMO07_SNAPSHOT_DISK): {}. The snapshot's memory holds the "
                    "guest /init that runs demo 7's command, so a snapshot booted "
                    "from another initramfs runs another /init; an /init from an "
                    "older initramfs runs the command as root with the console as "
                    "its standard input. Rebuild it by running demo 5 with "
                    "QEMU_SNAPSHOT_DISK={} and the same QEMU_ASSETS, or unset "
                    "DEMO07_SNAPSHOT_DISK to use the default snapshot, which demo 7 "
                    "rebuilds itself".format(snapshot, mismatch, snapshot)
                ) from mismatch
            print(
                "Demo 5 boot snapshot {} is not from the current initramfs: {}; "
                "running demo 5 again to rebuild it...".format(snapshot, mismatch),
                flush=True,
            )
            stale = True
    try:
        rebuild()
    except subprocess.CalledProcessError as failure:
        if not stale:
            raise
        # As in demo 6: demo 5 can exit non-zero after it saved a current
        # snapshot, when its own reference run was made from an older
        # initramfs. Its record, not its exit status, is the verdict.
        accept_snapshot_after_failed_rebuild(snapshot, root, assets, failure, "demo 7")
        return
    if not snapshot.is_file():
        raise RuntimeError("Demo 5 did not produce {}".format(snapshot))
    try:
        verify_boot_snapshot(snapshot, root, assets)
    except BootSnapshotMismatch as mismatch:
        raise RuntimeError(
            "Demo 5 ran, but {} still does not match the current initramfs: "
            "{}".format(snapshot, mismatch)
        ) from mismatch


def ensure_vmlinux(kernel: Path, vmlinux: Path) -> Path:
    """Extract the ELF-with-BTF payload from the fixed XZ bzImage if needed."""
    if vmlinux.is_file():
        with vmlinux.open("rb") as source:
            if source.read(4) == ELF_MAGIC:
                return vmlinux
        raise RuntimeError("cached vmlinux is not an ELF file: {}".format(vmlinux))

    compressed = kernel.read_bytes()
    offset = compressed.find(XZ_MAGIC)
    if offset < 0:
        raise RuntimeError(
            "kernel has no XZ-compressed vmlinux; set DEMO07_VMLINUX to matching debug info"
        )
    try:
        extracted = lzma.decompress(compressed[offset:])
    except lzma.LZMAError as error:
        raise RuntimeError("could not extract vmlinux from {}: {}".format(kernel, error))
    if not extracted.startswith(ELF_MAGIC):
        raise RuntimeError("extracted kernel payload is not ELF")
    _atomic_write(vmlinux, extracted)
    return vmlinux


def _elf_build_id(path: Path) -> str:
    try:
        result = subprocess.run(
            ["readelf", "-n", str(path)],
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
    except FileNotFoundError:
        raise RuntimeError("readelf is required to verify the kernel BuildID")
    except subprocess.CalledProcessError as error:
        raise RuntimeError("readelf failed for {}: {}".format(path, error.stderr.strip()))
    match = re.search(r"Build ID:\s*([0-9a-fA-F]+)", result.stdout)
    if match is None:
        raise RuntimeError("vmlinux has no GNU BuildID: {}".format(path))
    return match.group(1).lower()


def _register_vmcoreinfo_symbols(program, vmcoreinfo: bytes, drgn) -> None:
    """Expose runtime kernel symbols recorded in the guest's VMCOREINFO."""
    symbols = tuple(
        drgn.Symbol(
            match.group(1).decode("ascii"),
            int(match.group(2), 16),
            1,
            drgn.SymbolBinding.GLOBAL,
            drgn.SymbolKind.UNKNOWN,
        )
        for match in re.finditer(
            rb"(?m)^SYMBOL\(([^)]+)\)=([0-9a-fA-F]+)$", vmcoreinfo
        )
    )
    if not symbols:
        raise RuntimeError("guest VMCOREINFO contains no SYMBOL entries")

    def find_symbols(_program, name, address, one):
        matches = [
            symbol
            for symbol in symbols
            if (name is None or symbol.name == name)
            and (
                address is None
                or symbol.address <= address < symbol.address + symbol.size
            )
        ]
        return matches[:1] if one else matches

    program.register_symbol_finder(
        "guest-vmcoreinfo", find_symbols, enable_index=0
    )


def _btf_dwarf_cache(vmlinux: Path, artifact_dir: Path) -> Path:
    """Convert the public kernel BTF to DWARF for drgn builds without BTF."""
    build_id = _elf_build_id(vmlinux)
    cache_dir = artifact_dir / "type-cache"
    cache_dir.mkdir(parents=True, exist_ok=True)
    output = cache_dir / "vmlinux-types-{}.o".format(build_id)
    if output.is_file():
        with output.open("rb") as cached:
            if cached.read(4) == ELF_MAGIC:
                return output
        raise RuntimeError("cached drgn type object is not ELF: {}".format(output))

    bpftool = shutil.which("bpftool")
    compiler = shutil.which("gcc")
    if bpftool is None or compiler is None:
        raise RuntimeError(
            "bpftool and gcc are required to bridge kernel BTF into public drgn"
        )

    descriptor, source_name = tempfile.mkstemp(
        prefix=".vmlinux-types.", suffix=".c", dir=str(cache_dir)
    )
    os.close(descriptor)
    source = Path(source_name)
    temporary_output = source.with_suffix(".o")
    try:
        with source.open("wb") as generated:
            dumped = subprocess.run(
                [bpftool, "btf", "dump", "file", str(vmlinux), "format", "c"],
                stdout=generated,
                stderr=subprocess.PIPE,
            )
        if dumped.returncode != 0:
            raise RuntimeError(
                "bpftool could not decode kernel BTF: {}".format(
                    dumped.stderr.decode("utf-8", "replace").strip()
                )
            )
        with source.open("ab") as generated:
            generated.write(b"\nstruct task_struct demo07_init_task_type;\n")
        compiled = subprocess.run(
            [
                compiler,
                "-w",
                "-g",
                "-gdwarf-4",
                "-fno-eliminate-unused-debug-types",
                "-c",
                str(source),
                "-o",
                str(temporary_output),
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        if compiled.returncode != 0:
            raise RuntimeError(
                "gcc could not compile BTF-derived types: {}".format(
                    compiled.stderr.decode("utf-8", "replace").strip()
                )
            )
        os.chmod(temporary_output, 0o644)
        os.replace(temporary_output, output)
        return output
    finally:
        for temporary in (source, temporary_output):
            try:
                temporary.unlink()
            except FileNotFoundError:
                pass


def _load_btf_dwarf(program, vmlinux: Path, artifact_dir: Path) -> None:
    debug_object = _btf_dwarf_cache(vmlinux, artifact_dir)
    module = program.extra_module(str(debug_object), create=True)
    # An extra module needs a nonempty range even though only its types are used.
    module.address_range = (0, 1)
    module.try_file(str(debug_object), force=True)
    program.load_module_debug_info(module)


def _vmcoreinfo_symbol_addresses(vmcoreinfo: bytes) -> dict[str, int]:
    return {
        match.group(1).decode("ascii"): int(match.group(2), 16)
        for match in re.finditer(
            rb"(?m)^SYMBOL\(([^)]+)\)=([0-9a-fA-F]+)$", vmcoreinfo
        )
    }


def _kallsyms_address(program, vmcoreinfo: bytes, wanted: str) -> int:
    """Resolve one symbol from the stopped guest's compressed kallsyms table."""
    addresses = _vmcoreinfo_symbol_addresses(vmcoreinfo)
    required = (
        "kallsyms_names",
        "kallsyms_num_syms",
        "kallsyms_token_table",
        "kallsyms_token_index",
        "kallsyms_offsets",
        "kallsyms_relative_base",
    )
    missing = [name for name in required if name not in addresses]
    if missing:
        raise RuntimeError(
            "guest VMCOREINFO lacks kallsyms metadata: {}".format(", ".join(missing))
        )

    number = struct.unpack(
        "<I", program.read(addresses["kallsyms_num_syms"], 4)
    )[0]
    if not 0 < number <= 4_000_000:
        raise RuntimeError("invalid guest kallsyms count: {}".format(number))

    names_size = addresses["kallsyms_token_table"] - addresses["kallsyms_names"]
    token_table_size = (
        addresses["kallsyms_token_index"] - addresses["kallsyms_token_table"]
    )
    if not 0 < names_size <= 64 << 20 or not 0 < token_table_size <= 1 << 20:
        raise RuntimeError("invalid guest kallsyms table layout")

    names = program.read(addresses["kallsyms_names"], names_size)
    token_table = program.read(addresses["kallsyms_token_table"], token_table_size)
    token_index = struct.unpack(
        "<256H", program.read(addresses["kallsyms_token_index"], 512)
    )
    offsets = struct.unpack(
        "<{}i".format(number),
        program.read(addresses["kallsyms_offsets"], number * 4),
    )
    relative_base = struct.unpack(
        "<Q", program.read(addresses["kallsyms_relative_base"], 8)
    )[0]

    position = 0
    for index in range(number):
        if position >= len(names):
            raise RuntimeError("guest kallsyms names table ended early")
        length = names[position]
        position += 1
        if length & 0x80:
            if position >= len(names):
                raise RuntimeError("guest kallsyms name length is truncated")
            length = (length & 0x7F) | (names[position] << 7)
            position += 1
        if position + length > len(names):
            raise RuntimeError("guest kallsyms name is truncated")

        expanded = bytearray()
        for token in names[position : position + length]:
            start = token_index[token]
            end = token_table.find(b"\0", start)
            if end < 0:
                raise RuntimeError("guest kallsyms token table is truncated")
            expanded.extend(token_table[start:end])
        position += length
        # The first expanded character is the kallsyms type code.
        if expanded[1:].decode("ascii", "replace") == wanted:
            offset = offsets[index]
            return (
                relative_base + offset
                if offset >= 0
                else relative_base - 1 - offset
            )
    raise RuntimeError("guest kallsyms has no {} symbol".format(wanted))


def _register_init_task_object(program, address: int, drgn) -> None:
    task_type = program.type("struct task_struct")

    def find_object(_program, name, _flags, _filename):
        if name == "init_task":
            return drgn.Object(program, task_type, address=address)
        return None

    program.register_object_finder(
        "guest-init-task", find_object, enable_index=0
    )


def _log_tail(path: Path, lines: int, max_bytes: int) -> Optional[List[str]]:
    """Return the last ``lines`` lines among the last ``max_bytes`` bytes of ``path``.

    Returns None when the file does not exist. A line cut by the byte limit is
    left out.
    """
    try:
        with path.open("rb") as log:
            size = log.seek(0, os.SEEK_END)
            log.seek(max(0, size - max_bytes))
            data = log.read()
    except FileNotFoundError:
        return None
    text = data.decode("utf-8", errors="replace").splitlines()
    if size > max_bytes:
        text = text[1:]
    return text[-lines:] if lines > 0 else []


def _proc_status_value(pid: int, key: str) -> str:
    with open("/proc/{}/status".format(pid)) as status:
        for line in status:
            if line.startswith(key + ":"):
                return line.split()[1]
    raise RuntimeError("no {} in /proc/{}/status".format(key, pid))


def _proc_state(pid: int) -> str:
    return _proc_status_value(pid, "State")


def _proc_identity(pid: int) -> Optional[Tuple[str, int]]:
    """Return the state and start time of process ``pid``, or None if there is none.

    They are fields 3 and 22 of /proc/<pid>/stat. The start time, in clock
    ticks since boot, tells a process apart from a later one that reuses its pid.
    """
    try:
        with open("/proc/{}/stat".format(pid), "rb") as stat:
            data = stat.read()
    except (FileNotFoundError, ProcessLookupError):
        return None
    # Field 2, the command name, is in parentheses and may itself contain
    # spaces and parentheses, so the remaining fields follow the last ")".
    fields = data[data.rindex(b")") + 1 :].split()
    return fields[0].decode("ascii"), int(fields[19])


def _is_running(pid: int, start_time: int) -> bool:
    """Whether ``pid`` is still the process that started at ``start_time`` and has not exited."""
    identity = _proc_identity(pid)
    return (
        identity is not None
        and identity[0] not in ("Z", "X", "x")
        and identity[1] == start_time
    )


class _NoSignalSent(Exception):
    """A process that needed SIGKILL could not be signalled through a pidfd, so got no signal."""


def _kill_if_running(pid: int, start_time: int, pidfd: Optional[int] = None) -> None:
    """Send SIGKILL to ``pid`` only if it is still the process that started at ``start_time``.

    The signal goes only through a pidfd: ``pidfd`` when the caller holds one
    for this process (see _HeldTracer), else one opened here. A process that has
    exited, even if not yet reaped, or whose pid now belongs to a process with a
    different start time, is not signalled. Any other failure to open or use
    the pidfd raises _NoSignalSent, and no signal is sent by pid instead: these
    processes are not this process's children, so nothing keeps their pids from
    being given to another process between a check and a signal by pid.
    """
    if not _is_running(pid, start_time):
        return
    held = pidfd is not None
    if pidfd is None:
        try:
            # A pidfd refers to the process that had the pid when it was opened, so
            # a start time that still matches after opening it identifies the
            # process the signal reaches, even if the pid is reused meanwhile.
            pidfd = os.pidfd_open(pid)
        except ProcessLookupError:
            return  # ESRCH: it has exited.
        except (AttributeError, OSError) as error:
            raise _NoSignalSent("pidfd_open failed: {}".format(error)) from error
    try:
        if not _is_running(pid, start_time):
            return
        try:
            signal.pidfd_send_signal(pidfd, signal.SIGKILL)
        except ProcessLookupError:
            return  # ESRCH: it exited after the check.
        except (AttributeError, OSError) as error:
            raise _NoSignalSent("pidfd_send_signal failed: {}".format(error)) from error
    finally:
        if not held:
            os.close(pidfd)


def _tracer_of(pid: int, start_time: int) -> Optional[int]:
    """The thread-group id of the process ptrace-tracing ``pid``, or None.

    None also when ``pid`` is no longer the process that started at
    ``start_time``, checked after the read, so a reused pid's tracer is never
    returned.
    """
    try:
        tracer = int(_proc_status_value(pid, "TracerPid"))
        tracer_tgid = int(_proc_status_value(tracer, "Tgid")) if tracer else 0
    except (OSError, RuntimeError, ValueError):
        return None
    if not tracer_tgid or not _is_running(pid, start_time):
        return None
    return tracer_tgid


def _kill_and_wait(
    processes: List[Tuple[str, int, int]],
    timeout: float,
    pidfds: Optional[Dict[Tuple[str, int], int]] = None,
) -> Tuple[List[Tuple[str, int, int]], List[Tuple[str, int, int, str]]]:
    """SIGKILL each (name, pid, start time) and wait up to ``timeout`` seconds for all to exit.

    Returns the processes still running when the wait ends, and, each with the
    reason, those that could not be signalled through a pidfd and so were sent
    no signal; the wait is not spent on these. A process that has exited, even
    if not yet reaped, or whose pid now belongs to a process with a different
    start time, counts as gone and is not signalled. ``pidfds`` maps (name,
    pid) to a pidfd already held for that process, which is used instead of a
    new one.
    """
    signalled = []  # type: List[Tuple[str, int, int]]
    not_signalled = []  # type: List[Tuple[str, int, int, str]]
    for name, pid, start_time in processes:
        held = (pidfds or {}).get((name, pid))
        try:
            if held is None:
                _kill_if_running(pid, start_time)
            else:
                _kill_if_running(pid, start_time, held)
        except _NoSignalSent as error:
            not_signalled.append((name, pid, start_time, str(error)))
        else:
            signalled.append((name, pid, start_time))
    deadline = time.monotonic() + timeout
    while True:
        running = [process for process in signalled if _is_running(process[1], process[2])]
        if not running or time.monotonic() >= deadline:
            return running, not_signalled
        time.sleep(0.02)


def _find_qemu(qmp_socket: Path) -> Optional[int]:
    qmp_argument = "unix:{},server=on,wait=off".format(qmp_socket)
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        pid = int(entry)
        try:
            with open("/proc/{}/comm".format(pid)) as comm_file:
                comm = comm_file.read().strip()
            if not (comm.startswith("qemu-system") or comm == "qemu-kvm"):
                continue
            with open("/proc/{}/cmdline".format(pid), "rb") as command_file:
                arguments = [part.decode(errors="replace") for part in command_file.read().split(b"\0")]
            if qmp_argument in arguments:
                return pid
        except OSError:
            continue
    return None


def _wait_for_qemu(process: subprocess.Popen, qmp_socket: Path, timeout: float) -> int:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(
                "Hermit exited before QEMU appeared (status {})".format(process.returncode)
            )
        qemu_pid = _find_qemu(qmp_socket)
        if qemu_pid is not None:
            return qemu_pid
        time.sleep(0.05)
    raise TimeoutError("QEMU process for {} was not found".format(qmp_socket))


def _open_serial_pipe(
    base: Path, process: subprocess.Popen, timeout: float
) -> Tuple[int, int]:
    """Open a QEMU `-serial pipe:` FIFO pair (``<base>.in``/``<base>.out``).

    A connected unix-socket serial chardev keeps a descriptor whose readiness
    depends on host timing in QEMU's main loop and starves the -icount vCPU
    under `hermit --no-rcb-time`. A pipe's input FIFO is poll-ready only while a
    command is queued, so QEMU's main loop blocks in poll() between commands.
    QEMU opens both ends O_RDWR at chardev init, so these opens do not block once
    QEMU is up (guaranteed by the preceding QMP connect). The read end is
    non-blocking so the advance loop can poll for guest liveness.
    """
    read_fd = os.open(str(base) + ".out", os.O_RDONLY | os.O_NONBLOCK)
    deadline = time.monotonic() + timeout
    while True:
        try:
            write_fd = os.open(str(base) + ".in", os.O_WRONLY | os.O_NONBLOCK)
            return read_fd, write_fd
        except OSError as error:
            if process.poll() is not None:
                os.close(read_fd)
                raise RuntimeError(
                    "Hermit exited before serial pipe opened (status {})".format(
                        process.returncode
                    )
                )
            if time.monotonic() >= deadline:
                os.close(read_fd)
                raise TimeoutError(
                    "serial input pipe did not open: {}.in ({})".format(base, error)
                )
            time.sleep(0.05)


def _qemu_tracer_tgid(qemu_pid: int) -> int:
    """The thread-group id of the process that ptrace-traces QEMU."""
    tracer_tid = int(_proc_status_value(qemu_pid, "TracerPid"))
    if tracer_tid == 0:
        raise RuntimeError("QEMU is not ptrace-traced by Hermit")
    return int(_proc_status_value(tracer_tid, "Tgid"))


class _HeldTracer:
    """Hermit's tracer, found as QEMU's tracer, held through a pidfd opened once.

    The demo did not start the tracer, so nothing keeps its pid from being given
    to another process once it exits. Every signal the demo sends it, SIGSTOP,
    SIGCONT and SIGKILL, goes through this pidfd, which names the process that
    had the pid when it was opened; none is ever sent by pid.
    """

    def __init__(self, tgid: int, start_time: int, pidfd: int) -> None:
        self.tgid = tgid
        self.start_time = start_time
        self.pidfd = pidfd  # type: Optional[int]

    def send(self, sig: int) -> bool:
        """Send ``sig`` through the pidfd; False if the tracer has exited (ESRCH).

        Any other failure raises, and no signal is sent by pid instead.
        """
        if self.pidfd is None:
            raise RuntimeError("Hermit's tracer (pid {}) is no longer held".format(self.tgid))
        try:
            signal.pidfd_send_signal(self.pidfd, sig)
        except ProcessLookupError:
            return False
        except (AttributeError, OSError) as error:
            raise RuntimeError(
                "could not send {} to Hermit's tracer (pid {}) through its pidfd: {}; "
                "no signal was sent by pid".format(signal.Signals(sig).name, self.tgid, error)
            ) from error
        return True

    def close(self) -> None:
        if self.pidfd is not None:
            os.close(self.pidfd)
            self.pidfd = None


def _hold_exact_tracer(qemu_pid: int) -> _HeldTracer:
    """Open a pidfd for QEMU's tracer and check that it names the tracer found.

    The start time is read before the pidfd is opened, and checked again after,
    with the tracer still QEMU's: a pid given to another process meanwhile has
    a different start time, so the pidfd is then closed unused.
    """
    tracer_tgid = _qemu_tracer_tgid(qemu_pid)
    identity = _proc_identity(tracer_tgid)
    if identity is None:
        raise RuntimeError("Hermit's tracer (pid {}) exited before it was held".format(tracer_tgid))
    try:
        pidfd = os.pidfd_open(tracer_tgid)
    except ProcessLookupError as error:
        raise RuntimeError(
            "Hermit's tracer (pid {}) exited before it was held".format(tracer_tgid)
        ) from error
    except (AttributeError, OSError) as error:
        raise RuntimeError(
            "could not open a pidfd for Hermit's tracer (pid {}): {}; it is not signalled "
            "by pid".format(tracer_tgid, error)
        ) from error
    tracer = _HeldTracer(tracer_tgid, identity[1], pidfd)
    try:
        current = _qemu_tracer_tgid(qemu_pid)
        held = _is_running(tracer_tgid, identity[1])
    except BaseException:
        tracer.close()
        raise
    if current != tracer_tgid or not held:
        tracer.close()
        raise RuntimeError(
            "Hermit's tracer changed while it was being held: pid {} started at {}, and QEMU's "
            "tracer is now pid {}".format(tracer_tgid, identity[1], current)
        )
    return tracer


def _freeze_exact_tracer(qemu_pid: int, tracer: _HeldTracer, timeout: float = 20.0) -> None:
    """Stop ``tracer``, QEMU's tracer, while QEMU is in a trace-stop."""
    current = _qemu_tracer_tgid(qemu_pid)
    if current != tracer.tgid:
        raise RuntimeError(
            "QEMU's tracer is now pid {}, not Hermit's tracer pid {} that the demo "
            "holds".format(current, tracer.tgid)
        )
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if _proc_state(qemu_pid) != "t":
            time.sleep(0.002)
            continue
        if not tracer.send(signal.SIGSTOP):
            raise RuntimeError("Hermit's tracer (pid {}) exited".format(tracer.tgid))
        for _ in range(1000):
            if _proc_state(tracer.tgid) == "T":
                break
            time.sleep(0.001)
        if _proc_state(qemu_pid) == "t" and _proc_state(tracer.tgid) == "T":
            return
        if not tracer.send(signal.SIGCONT):
            raise RuntimeError("Hermit's tracer (pid {}) exited".format(tracer.tgid))
    raise TimeoutError("could not freeze Hermit's exact tracer at a QEMU trace-stop")


def _ram_region(qemu_pid: int, wanted: int) -> Tuple[int, int]:
    candidates = []
    with open("/proc/{}/maps".format(qemu_pid)) as maps:
        for line in maps:
            fields = line.split()
            if "r" not in fields[1] or "w" not in fields[1]:
                continue
            first, last = (int(value, 16) for value in fields[0].split("-"))
            size = last - first
            if wanted // 2 <= size <= wanted * 2:
                candidates.append((abs(size - wanted), first, last))
    if not candidates:
        raise RuntimeError("could not identify the {} MiB QEMU RAM mmap".format(wanted >> 20))
    _, first, last = min(candidates)
    return first, last


def _scan_vmcoreinfo(
    descriptor: int, first: int, last: int, chunk: int = 8 << 20
) -> Tuple[int, bytes]:
    offset = first
    tail = b""
    while offset < last:
        count = min(chunk, last - offset)
        try:
            data = os.pread(descriptor, count, offset)
        except OSError:
            offset += 4096
            tail = b""
            continue
        if not data:
            offset += 4096
            tail = b""
            continue
        combined = tail + data
        search_from = 0
        while True:
            index = combined.find(VMCOREINFO_MARKER, search_from)
            if index < 0:
                break
            address = offset - len(tail) + index
            candidate = os.pread(descriptor, 65536, address).split(b"\0", 1)[0]
            if (
                re.match(rb"OSRELEASE=[0-9]", candidate)
                and b"\nPAGESIZE=" in candidate
                and b"\nSYMBOL(_stext)=" in candidate
            ):
                return address, candidate
            search_from = index + len(VMCOREINFO_MARKER)
        tail = data[-len(VMCOREINFO_MARKER) :]
        offset += len(data)
    raise RuntimeError("VMCOREINFO was not found in guest RAM")


class HermitGuestProgram:
    def __init__(self, config: GuestConfig) -> None:
        self.config = config
        self._process = None  # type: Optional[subprocess.Popen]
        self._process_group = None  # type: Optional[int]
        self._qemu_pid = None  # type: Optional[int]
        self._tracer_tgid = None  # type: Optional[int]
        self._tracer = None  # type: Optional[_HeldTracer]
        # (name, pid, start time) of each process close() must kill through a pidfd.
        self._owned_processes = []  # type: List[Tuple[str, int, int]]
        self._memory = None  # type: Optional[int]
        self._qmp = None  # type: Optional[QmpClient]
        self._serial_read_fd = None  # type: Optional[int]
        self._serial_write_fd = None  # type: Optional[int]
        self._ram_first = 0
        self._ram_size = 0
        self._vmcoreinfo = b""
        self._vmlinux = None  # type: Optional[Path]
        self._frozen = False
        self._serial_bytes = 0
        self._reads = 0
        self._bytes = 0
        self._init_task_address = None  # type: Optional[int]
        self.metrics = []  # type: list[ObservationMetrics]
        self.run_dir = None  # type: Optional[Path]
        self.serial_log = None  # type: Optional[Path]
        self.qmp_socket = None  # type: Optional[Path]

    def start(self) -> "HermitGuestProgram":
        for path in (
            self.config.hermit,
            self.config.qemu,
            self.config.kernel,
            self.config.initrd,
            self.config.snapshot_disk,
        ):
            if not path.is_file():
                raise FileNotFoundError(str(path))
        self._vmlinux = ensure_vmlinux(self.config.kernel, self.config.vmlinux)
        self.config.artifact_dir.mkdir(parents=True, exist_ok=True)
        self.run_dir = Path(tempfile.mkdtemp(prefix="run.", dir=str(self.config.artifact_dir)))
        self.serial_log = self.run_dir / "serial.log"
        hermit_log = self.run_dir / "hermit.log"
        # QEMU, under Hermit, creates the QMP socket at this path and this
        # process connects to it from outside, so the path must fit the 107
        # bytes AF_UNIX allows. In run.sh's default artifact directory the path
        # in the run directory fits only from a checkout path of at most 57
        # bytes (from a 60-byte one it is 110 bytes). make_socket_path keeps it
        # where it fits and otherwise moves it to a short directory outside
        # host /tmp; close() removes a moved socket. A run directory under host
        # /tmp keeps its socket there: hermit_tmp_args, below, then gives QEMU
        # the host's /tmp.
        qmp_socket = make_socket_path(self.run_dir / "qmp.sock", "drgn")
        self.qmp_socket = qmp_socket
        # Bidirectional serial over a `-serial pipe:` FIFO pair, not a unix
        # socket: a socket chardev's always-pollable descriptor lets QEMU's main
        # loop starve the -icount vCPU under `hermit --no-rcb-time`. QEMU opens
        # (does not create) the FIFOs, so make them before launch.
        serial_pipe = self.run_dir / "serial"
        for suffix in (".in", ".out"):
            os.mkfifo(str(serial_pipe) + suffix)
        working_snapshot = self.run_dir / "snapshot.qcow2"
        shutil.copyfile(str(self.config.snapshot_disk), str(working_snapshot))
        # Demo 5 does not take a lock, so it may have replaced the boot
        # snapshot, or its record, after ensure_boot_snapshot checked them.
        # Check the copy that QEMU restores, as demo 6 does, against the record
        # next to config.snapshot_disk, the name ensure_boot_snapshot checked.
        try:
            verify_boot_snapshot(
                self.config.snapshot_disk,
                self.config.root,
                self.config.assets,
                disk=working_snapshot,
            )
        except BootSnapshotMismatch as mismatch:
            raise RuntimeError(
                "the copy {} of the boot snapshot {} does not match demo 5's "
                "record: {}; demo 5 may have replaced the boot snapshot after "
                "demo 7 checked it, so run demo 7 again".format(
                    working_snapshot, self.config.snapshot_disk, mismatch
                )
            ) from mismatch
        # Demo 5 records this virtio-blk device in the saved VM topology, and the
        # current initramfs waits for its command here rather than on serial. Supply
        # the real deterministic advance command before -loadvm, with the same
        # fixed geometry used by Demos 5 and 6.
        command_image = self.run_dir / "guest-command.img"
        _write_command_image(command_image, self.config.advance_command)

        qemu_command = [
            str(self.config.qemu),
            "-machine", "q35",
            "-accel", "tcg",
            "-cpu", "max",
            "-smp", "1",
            "-m", "512M",
            "-display", "none",
            "-monitor", "none",
            "-serial", "pipe:{}".format(serial_pipe),
            "-qmp", "unix:{},server=on,wait=off".format(qmp_socket),
            "-drive", "if=none,id=hermit-snapshot-store,file={},format=qcow2".format(
                working_snapshot
            ),
            "-drive", "if=none,id=hermit-command,file={},format=raw,readonly=on".format(
                command_image
            ),
            "-device", "virtio-blk-pci,drive=hermit-command",
            "-loadvm", self.config.snapshot_name,
            "-S",
            "-icount", "shift=0,sleep=off",
            "-rtc", "base=2022-01-01T00:00:00,clock=vm",
            "-kernel", str(self.config.kernel),
            "-initrd", str(self.config.initrd),
            "-append", KERNEL_COMMAND_LINE,
        ]
        if self.config.qemu_bios is not None:
            qemu_command[1:1] = ["-L", str(self.config.qemu_bios)]
        # Hermit gives QEMU a private /tmp unless it is passed --tmp=/tmp, so a
        # path under host /tmp is invisible to QEMU. Decide from every host
        # path QEMU opens: the run directory (QMP socket, serial FIFOs and both
        # disks), its own binary, firmware and libraries, the kernel and
        # initramfs, and the checkout it runs in. Deciding from the checkout
        # alone made a DEMO07_ARTIFACTS under host /tmp fail at the QMP socket.
        # The library path is prepended to LD_LIBRARY_PATH, so it may be a list.
        library_dirs = []  # type: List[Path]
        if self.config.qemu_library_path is not None:
            library_dirs = [
                Path(part)
                for part in str(self.config.qemu_library_path).split(os.pathsep)
                if part
            ]
        tmp_args = hermit_tmp_args(
            self.config.root,
            self.run_dir,
            qmp_socket,
            self.config.qemu,
            self.config.qemu_bios,
            self.config.kernel,
            self.config.initrd,
            *library_dirs,
        )
        command = [
            str(self.config.hermit),
            "run",
            *tmp_args,
            "--strict",
            "--epoch",
            EPOCH,
            "--no-rcb-time",
            "--target-timeslice", "100000",
            "--max-timeslice", "disabled",
            "--",
        ] + qemu_command
        environment = os.environ.copy()
        environment["LC_ALL"] = "C"
        environment["TZ"] = "UTC"
        if self.config.qemu_library_path is not None:
            old_path = environment.get("LD_LIBRARY_PATH", "")
            environment["LD_LIBRARY_PATH"] = str(self.config.qemu_library_path) + (
                ":" + old_path if old_path else ""
            )

        with hermit_log.open("wb") as output:
            self._process = subprocess.Popen(
                command,
                cwd=str(self.config.root),
                env=environment,
                start_new_session=True,
                stdout=output,
                stderr=subprocess.STDOUT,
            )
        self._process_group = os.getpgid(self._process.pid)
        self._qmp = QmpClient.connect(qmp_socket, self._process, self.config.timeout)
        self._serial_read_fd, self._serial_write_fd = _open_serial_pipe(
            serial_pipe, self._process, self.config.timeout
        )
        initial_status = self._qmp.status()
        if initial_status not in ("paused", "prelaunch"):
            self._qmp.execute("stop")
            if self._qmp.status() != "paused":
                raise RuntimeError("QEMU did not start with guest CPUs paused")

        self._qemu_pid = _wait_for_qemu(self._process, qmp_socket, self.config.timeout)
        self._own_process("QEMU", self._qemu_pid)
        self._tracer = _hold_exact_tracer(self._qemu_pid)
        self._tracer_tgid = self._tracer.tgid
        self._owned_processes.append(
            ("Hermit's tracer", self._tracer.tgid, self._tracer.start_time)
        )
        _freeze_exact_tracer(self._qemu_pid, self._tracer)
        self._frozen = True
        first, last = _ram_region(self._qemu_pid, self.config.ram_bytes)
        self._ram_first = first
        self._ram_size = last - first
        self._memory = os.open("/proc/{}/mem".format(self._qemu_pid), os.O_RDONLY)
        _, self._vmcoreinfo = _scan_vmcoreinfo(self._memory, first, last)
        build_match = re.search(rb"(?m)^BUILD-ID=([0-9a-fA-F]+)$", self._vmcoreinfo)
        if build_match is None:
            raise RuntimeError("guest VMCOREINFO has no BuildID")
        guest_build_id = build_match.group(1).decode().lower()
        debug_build_id = _elf_build_id(self._vmlinux)
        if guest_build_id != debug_build_id:
            raise RuntimeError(
                "kernel/vmlinux BuildID mismatch: guest {} debug {}".format(
                    guest_build_id, debug_build_id
                )
            )
        return self

    def _program(self):
        if not self._frozen or self._memory is None or self._vmlinux is None:
            raise RuntimeError("guest must be frozen before a drgn observation")
        self._reads = 0
        self._bytes = 0

        import drgn

        def read_physical(address, count, offset, physical):
            if not physical:
                raise ValueError("guest memory callback received a virtual read")
            data = os.pread(self._memory, count, self._ram_first + offset)
            if len(data) != count:
                raise OSError(
                    "short guest RAM read at {:#x}: {}/{}".format(
                        address, len(data), count
                    )
                )
            self._reads += 1
            self._bytes += count
            return data

        program = drgn.Program(drgn.Platform(drgn.Architecture.X86_64))
        program.add_memory_segment(0, self._ram_size, read_physical, physical=True)
        program.set_linux_kernel_custom(self._vmcoreinfo, True)
        _register_vmcoreinfo_symbols(program, self._vmcoreinfo, drgn)
        if "btf" not in program.registered_type_finders():
            # Public drgn 0.2.0 does not read BTF, while Meta's build does.
            # Convert the pinned public BTF to cached DWARF types on that path.
            _load_btf_dwarf(program, self._vmlinux, self.config.artifact_dir)
        else:
            try:
                program.load_debug_info([str(self._vmlinux)], main=True)
            except drgn.MissingDebugInfoError:
                _load_btf_dwarf(program, self._vmlinux, self.config.artifact_dir)
        if self._init_task_address is None:
            self._init_task_address = _kallsyms_address(
                program, self._vmcoreinfo, "init_task"
            )
        _register_init_task_object(program, self._init_task_address, drgn)
        return program

    @contextmanager
    def observation(self):
        if self._qemu_pid is None or self._tracer_tgid is None:
            raise RuntimeError("guest was not started")
        # Bytes already in the serial pipe reached it before this read began,
        # for example output that followed the advance marker: log them and
        # leave them out of the count.
        self._drain_serial()
        serial_before = self._serial_bytes
        program = self._program()
        yield program
        # Whatever the pipe holds now reached it while the read ran.
        self._drain_serial()
        qemu_state = _proc_state(self._qemu_pid)
        tracer_state = _proc_state(self._tracer_tgid)
        serial_delta = self._serial_bytes - serial_before
        current = ObservationMetrics(
            physical_reads=self._reads,
            physical_bytes=self._bytes,
            qemu_state=qemu_state,
            tracer_state=tracer_state,
            serial_bytes_delta=serial_delta,
        )
        self.metrics.append(current)
        if qemu_state != "t" or tracer_state != "T" or serial_delta != 0:
            raise RuntimeError(
                "guest advanced during read: qemu={} tracer={} serial_delta={}".format(
                    qemu_state, tracer_state, serial_delta
                )
            )

    def _drain_serial(self) -> None:
        """Read every byte the serial pipe holds now, without waiting for more.

        The bytes are appended to the serial transcript and counted in
        ``_serial_bytes``. QEMU keeps the pipe's write end open, so an empty
        pipe raises BlockingIOError rather than returning end of file.
        """
        if self._serial_read_fd is None or self.serial_log is None:
            raise RuntimeError("serial transport is unavailable")
        with self.serial_log.open("ab") as output:
            while True:
                try:
                    chunk = os.read(self._serial_read_fd, 65536)
                except BlockingIOError:
                    return
                if not chunk:
                    raise RuntimeError("guest serial disconnected during a read")
                self._serial_bytes += len(chunk)
                output.write(chunk)

    def _wait_for_serial(self, marker: bytes) -> None:
        if (
            self._serial_read_fd is None
            or self.serial_log is None
            or self._process is None
        ):
            raise RuntimeError("serial transport is unavailable")
        deadline = time.monotonic() + self.config.timeout
        transcript = bytearray()
        with self.serial_log.open("ab") as output:
            while time.monotonic() < deadline:
                if self._process.poll() is not None:
                    raise RuntimeError(
                        "Hermit exited during deterministic advance (status {})".format(
                            self._process.returncode
                        )
                    )
                try:
                    chunk = os.read(self._serial_read_fd, 65536)
                except BlockingIOError:
                    # No guest output yet; QEMU still holds the pipe's write end
                    # open (O_RDWR), so this is a wait, not EOF.
                    time.sleep(0.02)
                    continue
                if not chunk:
                    raise RuntimeError("guest serial disconnected during advance")
                transcript.extend(chunk)
                self._serial_bytes += len(chunk)
                output.write(chunk)
                output.flush()
                if marker in transcript:
                    return
        raise TimeoutError("guest advance marker was not seen")

    def advance(self, command: str, marker: bytes) -> None:
        """Run the preloaded guest command, then freeze the guest at its completion marker."""
        if not self._frozen or self._qmp is None:
            raise RuntimeError("guest is not ready for deterministic advance")
        if self._tracer is None or self._qemu_pid is None:
            raise RuntimeError("traced processes are unavailable")
        if b"\n" in marker or "\n" in command or "\r" in command:
            raise ValueError("advance command and marker must each be one line")
        if command != self.config.advance_command:
            raise ValueError("advance command differs from the preloaded command disk")

        # The deterministic input was preloaded on the command disk before QEMU
        # restored the snapshot. Resume the frozen tracee, then wait on serial only
        # for the guest's completion marker.
        if not self._tracer.send(signal.SIGCONT):
            raise RuntimeError(
                "Hermit's tracer (pid {}) exited before the advance".format(self._tracer.tgid)
            )
        self._frozen = False
        self._qmp.execute("cont")
        self._wait_for_serial(marker)
        self._qmp.execute("stop")
        if self._qmp.status() != "paused":
            raise RuntimeError("QEMU did not pause after deterministic advance")
        _freeze_exact_tracer(self._qemu_pid, self._tracer)
        self._frozen = True

    def close(self, failed: bool = False) -> None:
        """Stop Hermit and QEMU; after a failed pass, also report and tidy it.

        ``failed`` says that the pass did not finish: its Hermit log's end is
        printed, and its snapshot copy is removed (demo 5's is 94 MB).

        QEMU or Hermit's tracer still running after close() has killed it and
        waited is reported, and so is one that close() could not signal through
        a pidfd, which it then sends no signal at all; after a pass that
        finished, close() then raises.
        """
        if self._memory is not None:
            os.close(self._memory)
            self._memory = None
        if self._qmp is not None:
            try:
                self._qmp.close()
            except OSError:
                pass
            self._qmp = None
        for attribute in ("_serial_read_fd", "_serial_write_fd"):
            descriptor = getattr(self, attribute)
            if descriptor is not None:
                try:
                    os.close(descriptor)
                except OSError:
                    pass
                setattr(self, attribute, None)
        # Kill QEMU and Hermit's tracer one by one. The process-group SIGKILL
        # below does not reach them when `hermit` is a wrapper that runs Hermit
        # elsewhere: safehermit runs it as a systemd user unit, where the
        # tracer, stopped since the last observation, and QEMU outlived every
        # pass. Each is signalled only through a pidfd, and only while its pid
        # still has the start time recorded when start(), or here a pass that
        # failed first, found it, so a reused pid is left alone. One that cannot
        # be signalled through a pidfd is sent no signal, and that is reported.
        if self._process is not None and self._process.poll() is None:
            self._own_unrecorded_processes()
        stopped = []  # type: List[str]
        unstopped = []  # type: List[Tuple[str, int, int]]
        if self._owned_processes:
            running = [
                (name, pid)
                for name, pid, start_time in self._owned_processes
                if _is_running(pid, start_time)
            ]
            held = {}  # type: Dict[Tuple[str, int], int]
            if self._tracer is not None and self._tracer.pidfd is not None:
                held[("Hermit's tracer", self._tracer.tgid)] = self._tracer.pidfd
            survivors, not_signalled = _kill_and_wait(
                self._owned_processes, OWNED_PROCESS_EXIT_SECONDS, held
            )
            reasons = {(name, pid): reason for name, pid, _, reason in not_signalled}
            stopped = [name for name, pid in running if (name, pid) not in reasons]
            unstopped = [
                process
                for process in self._owned_processes
                if process in survivors or process[:2] in reasons
            ]
            self._owned_processes = unstopped
            for name, pid, _ in unstopped:
                if (name, pid) in reasons:
                    message = (
                        "Could not stop {} (pid {}): {}. It was sent no signal: a "
                        "signal sent by pid alone could reach another process given "
                        "that pid.".format(name, pid, reasons[(name, pid)])
                    )
                else:
                    message = "{} (pid {}) is still running {:g} s after SIGKILL.".format(
                        name, pid, OWNED_PROCESS_EXIT_SECONDS
                    )
                print(message, file=sys.stderr)
            # Without its tracer Hermit exits, and a wrapper then finishes its
            # own cleanup (safehermit stops and resets its unit), which the
            # process-group SIGKILL below would cut short.
            if self._process is not None:
                try:
                    self._process.wait(timeout=HERMIT_EXIT_SECONDS)
                except subprocess.TimeoutExpired:
                    print(
                        "Hermit (pid {}) is still running {:g} s after its tracer "
                        "and QEMU stopped; killing its process group.".format(
                            self._process.pid, HERMIT_EXIT_SECONDS
                        ),
                        file=sys.stderr,
                    )
        if self._tracer is not None:
            self._tracer.close()
        # Hermit runs in its own process group (start_new_session); this stops
        # whatever is still running in it, Hermit included.
        if (
            self._process_group is not None
            and self._process is not None
            and self._process.poll() is None
        ):
            try:
                os.killpg(self._process_group, signal.SIGKILL)
            except ProcessLookupError:
                pass
        if self._process is not None:
            try:
                self._process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self._process.kill()
                self._process.wait(timeout=10)
        # A socket that make_socket_path moved out of the run directory would
        # otherwise stay behind in the shared relocation directory.
        if (
            self.qmp_socket is not None
            and self.run_dir is not None
            and self.qmp_socket.parent != self.run_dir
        ):
            self.qmp_socket.unlink(missing_ok=True)
        if failed:
            self._report_failed_pass(stopped)
        elif unstopped:
            # A failed pass is already raising; this would replace its error.
            raise RuntimeError(
                "could not stop {} after the pass".format(
                    ", ".join("{} (pid {})".format(name, pid) for name, pid, _ in unstopped)
                )
            )

    def _own_process(self, name: str, pid: int) -> None:
        """Record ``pid``, found just now, and its start time for close() to kill."""
        identity = _proc_identity(pid)
        if identity is not None:
            self._owned_processes.append((name, pid, identity[1]))

    def _own_unrecorded_processes(self) -> None:
        """Find QEMU and Hermit's tracer when the pass failed before start() recorded them.

        A pass can fail while Hermit still runs, for instance waiting for QMP,
        before start() looks for QEMU. QEMU's command line names this pass's
        QMP socket, and QEMU's tracer is Hermit's, so neither can belong to
        another run.
        """
        recorded = {name: (pid, start) for name, pid, start in self._owned_processes}
        if "QEMU" not in recorded and self.qmp_socket is not None:
            qemu_pid = _find_qemu(self.qmp_socket)
            if qemu_pid is not None:
                self._own_process("QEMU", qemu_pid)
                recorded = {name: (pid, start) for name, pid, start in self._owned_processes}
        if "QEMU" in recorded and "Hermit's tracer" not in recorded:
            tracer_tgid = _tracer_of(*recorded["QEMU"])
            if tracer_tgid is not None:
                self._own_process("Hermit's tracer", tracer_tgid)

    def _report_failed_pass(self, stopped: Optional[List[str]] = None) -> None:
        """Print the end of a failed pass's Hermit log and remove its snapshot copy.

        ``stopped`` names the processes close() killed. Hermit logs the SIGKILL
        of its tracer as its own failure, so the report says where it came from.
        """
        if self.run_dir is None:
            return
        hermit_log = self.run_dir / "hermit.log"
        tail = _log_tail(hermit_log, FAILED_LOG_TAIL_LINES, FAILED_LOG_TAIL_BYTES)
        if tail is None:
            print("The failed pass has no Hermit log at {}.".format(hermit_log), file=sys.stderr)
        elif not tail:
            print("The failed pass's Hermit log {} is empty.".format(hermit_log), file=sys.stderr)
        else:
            print("End of the failed pass's Hermit log, {}:".format(hermit_log), file=sys.stderr)
            for line in tail:
                print("  " + line, file=sys.stderr)
        if stopped:
            print(
                "The demo stopped {} with SIGKILL before reading the log; a SIGKILL that "
                "the log reports is this cleanup, not the pass's failure.".format(
                    " and ".join(stopped)
                ),
                file=sys.stderr,
            )
        snapshot = self.run_dir / "snapshot.qcow2"
        try:
            size = snapshot.stat().st_size
            snapshot.unlink()
        except FileNotFoundError:
            return
        print(
            "Removed the failed pass's {}-byte snapshot copy {}".format(size, snapshot),
            file=sys.stderr,
        )


@contextmanager
def program_from_hermit(config: GuestConfig) -> Iterator[HermitGuestProgram]:
    """Yield a restored snapshot guest, initially frozen for observation.

    A pass that fails, in start() or in the caller's block, prints the end of
    its Hermit log and loses its snapshot copy; a pass that finishes keeps both.
    """
    guest = HermitGuestProgram(config)
    failed = True
    try:
        yield guest.start()
        failed = False
    finally:
        guest.close(failed=failed)
