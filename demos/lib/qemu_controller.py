#!/usr/bin/env python3
"""Run QEMU plus its serial/QMP controller inside Hermit's boundary."""

import argparse
from pathlib import Path
import re
import subprocess
import sys
import time
from typing import List, NamedTuple, Optional, Tuple

from demo_common import qmp_command, stop_process, wait_for_socket


# Launch QEMU with fork()+execve, never vfork(). CPython 3.11+ uses vfork() by
# default, which suspends the parent until the child execs. This controller runs
# inside Hermit alongside QEMU, so both share one deterministic scheduler. Under
# `hermit run --no-rcb-time`, QEMU's `-icount sleep=off` main loop starts
# busy-polling as soon as it execs and the scheduler never resumes the suspended
# vfork parent, so the controller would wait forever. Plain fork() leaves the
# parent runnable. The attribute exists on CPython 3.11+; older versions already
# use fork().
if hasattr(subprocess, "_USE_VFORK"):
    subprocess._USE_VFORK = False


BOOT_MARKER = "HERMIT-QEMU-BASELINE-BOOT-OK"
# Emitted by the guest once it is polling the command disk; this is where the
# boot snapshot is taken, so the resumed guest has not yet read the device.
COMMAND_DISK_MARKER = "HERMIT-QEMU-COMMAND-DISK-READY"
BEGIN_MARKER = "__HERMIT_COMMAND_BEGIN__"
END_MARKER = "__HERMIT_COMMAND_END__"

# The guest's /init (demos/lib/qemu-assets.sh) frames the command's output: a
# BEGIN line naming the frame format, the command's stdout and stderr with
# OUTPUT_PREFIX in front of every line, and an END line carrying the command's
# exit status after END_SEPARATOR. /init runs the command as an unprivileged
# user that holds no descriptor for the console and cannot open it, so the
# command's output reaches the console only through /init, which prefixes
# every line and removes every END_SEPARATOR byte from it. Neither a line of
# output nor the tail of one that a kernel message splits off can therefore be
# an END line, and no kernel line can be one either: KERNEL_COMMAND_LINE sets
# printk.time=1, so every line the kernel prints starts with "[". The framing
# comment in qemu-assets.sh lists what this rests on.
COMMAND_FRAME_FORMAT = 3
BEGIN_LINE = "{} format={}".format(BEGIN_MARKER, COMMAND_FRAME_FORMAT).encode()
END_SEPARATOR = b"\x01"
END_LINE_RE = re.compile(
    re.escape(END_MARKER.encode() + END_SEPARATOR) + rb"status=(0|[1-9][0-9]{0,2})"
)
OUTPUT_PREFIX = b"| "
# Marks a line inside the frame that /init did not prefix (a kernel message)
# where it appears in the output.
CONSOLE_LINE_MARK = b"[console] "
# A whole line matching this, other than BEGIN_LINE, is the BEGIN line of an
# /init with another frame format: before format 2 /init printed the bare
# marker, and format 2 printed "format=2". Only whole lines count, so a kernel
# message printed onto the end of the current BEGIN line does not look stale.
STALE_BEGIN_RE = re.compile(re.escape(BEGIN_MARKER.encode()) + rb"(?: format=[0-9]+)?")
# A whole BEGIN line of some /init, then at once a kernel message, which starts
# with "[" (the console may already have sent the CR that goes before the
# line's LF). The current /init sends CR LF right after "format=3", so only an
# /init with another frame format prints a line starting with the bare marker
# and CR, or with "format=N" where N is not 3, before the "[". Any other split
# BEGIN line, such as "...BEGIN__[", "format=[" or "format=3[", can be the
# current BEGIN line split by a kernel message; it is ignored, the frame then
# never starts, and the run stops at a bound. What keeps an older /init out of
# a resume is that demo 6 restores only a boot snapshot booted from the current
# initramfs (verify_boot_snapshot in demo_common).
STALE_BEGIN_PREFIX_RE = re.compile(
    re.escape(BEGIN_MARKER.encode()) + rb"(?: format=([0-9]+)\r?|\r)\["
)
# The guest kernel's command line, for the boot and for every restore of the
# boot snapshot. printk.time=1 makes the kernel start every line it prints with
# a "[seconds]" timestamp, whatever its configuration, so no kernel line can
# start with END_MARKER.
KERNEL_COMMAND_LINE = "console=ttyS0 reboot=t printk.time=1"


def stale_guest_init_message(line: bytes) -> str:
    """Why a run stopped at an old BEGIN line, and how to rebuild."""
    return (
        'the guest printed "{}" where an /init of command frame format {} prints '
        '"{}", so the boot snapshot was built from an initramfs with another '
        "/init. Run demos/clean.sh, then run demo 5 again to rebuild the boot "
        "snapshot".format(
            line.decode("ascii", "replace"),
            COMMAND_FRAME_FORMAT,
            BEGIN_LINE.decode(),
        )
    )


# QEMU's initial process state influences the VM snapshot. Do not leak harness
# settings such as QEMU_TIMEOUT or proxy variables into its initial stack.
QEMU_ENV = {
    "LC_ALL": "C",
    "TZ": "UTC",
}


class StaleGuestInitError(ValueError):
    """The guest runs an /init that predates the current command frame."""


class CommandResult(NamedTuple):
    """What the guest's /init reported for one command."""

    # The command's stdout and stderr with OUTPUT_PREFIX removed, one line per
    # prefixed line, plus any unprefixed console line inside the frame after
    # CONSOLE_LINE_MARK, all in transcript order.
    output: bytes
    # The command's exit status, from the END line.
    exit_status: int
    # The unprefixed lines inside the frame, without CONSOLE_LINE_MARK.
    console_lines: Tuple[bytes, ...]


def end_line_status(line: bytes) -> Optional[int]:
    """The exit status an END line carries, or None if ``line`` is not one."""
    match = END_LINE_RE.fullmatch(line)
    if match is None:
        return None
    status = int(match.group(1))
    return status if status <= 255 else None


class CommandTranscriptParser:
    """Read the guest's command frame from a serial transcript, chunk by chunk.

    Only complete lines count: a line is read once its newline has arrived, so
    once a transcript yields a result, every longer transcript that starts with
    it yields the same result. The guest's console ends lines with CR LF; one
    CR before the LF is removed and any other CR is kept.

    Lines before BEGIN_LINE are ignored, except a whole line matching
    STALE_BEGIN_RE, the BEGIN line of an /init with another frame format, and a
    line in which a kernel message follows such a BEGIN line
    (STALE_BEGIN_PREFIX_RE); either raises StaleGuestInitError at once. After
    BEGIN_LINE, a line starting with OUTPUT_PREFIX is a line of the command's
    output, a line that is exactly an END line ends the frame, and any other
    line, including an END line of an older frame format or a bare END_MARKER,
    is kept in the output after CONSOLE_LINE_MARK and listed in console_lines,
    so it is shown and compared rather than dropped. Frame lines match only as
    whole lines. Do not feed a parser again after it has raised.
    """

    def __init__(self) -> None:
        self._pending = bytearray()
        self._started = False
        self._output = bytearray()
        self._console_lines: List[bytes] = []
        self.result: Optional[CommandResult] = None

    @property
    def started(self) -> bool:
        """Whether the current frame format's BEGIN line has been read."""
        return self._started

    def feed(self, data: bytes) -> Optional[CommandResult]:
        """Add transcript bytes; return the result once the END line is in."""
        if self.result is not None:
            return self.result
        search_from = len(self._pending)
        self._pending.extend(data)
        line_start = 0
        while self.result is None:
            newline = self._pending.find(b"\n", search_from)
            if newline < 0:
                break
            self._take_line(bytes(self._pending[line_start:newline]))
            line_start = search_from = newline + 1
        del self._pending[:line_start]
        return self.result

    def _take_line(self, line: bytes) -> None:
        if line.endswith(b"\r"):
            line = line[:-1]
        if not self._started:
            if line == BEGIN_LINE:
                self._started = True
                return
            if STALE_BEGIN_RE.fullmatch(line):
                raise StaleGuestInitError(stale_guest_init_message(line))
            interrupted = STALE_BEGIN_PREFIX_RE.match(line)
            if interrupted is None:
                return
            frame_format = interrupted.group(1)
            if frame_format is None:
                raise StaleGuestInitError(
                    stale_guest_init_message(line[: len(BEGIN_MARKER)])
                )
            if frame_format != str(COMMAND_FRAME_FORMAT).encode():
                raise StaleGuestInitError(
                    stale_guest_init_message(line[: interrupted.end(1)])
                )
            return
        if line.startswith(OUTPUT_PREFIX):
            self._output.extend(line[len(OUTPUT_PREFIX) :] + b"\n")
            return
        exit_status = end_line_status(line)
        if exit_status is not None:
            self.result = CommandResult(
                bytes(self._output), exit_status, tuple(self._console_lines)
            )
            return
        self._console_lines.append(line)
        self._output.extend(CONSOLE_LINE_MARK + line + b"\n")


def parse_command_transcript(transcript: bytes) -> Optional[CommandResult]:
    """The command's result in a whole transcript, or None if it has not ended."""
    return CommandTranscriptParser().feed(transcript)


class FileSerial:
    """Read-only serial transport that tails QEMU's `-serial file:` transcript.

    The controller only reads the console, so QEMU writes the serial stream
    straight to a file and this class tails it. Unlike a unix-socket chardev, a
    file sink puts no pollable descriptor in QEMU's main loop, so it cannot
    starve the -icount vCPU under `hermit --no-rcb-time`. There is no internal
    deadline: time is virtual inside Hermit, so the demo process outside the
    container enforces the real wall-clock timeout.
    """

    def __init__(self, transcript: Path) -> None:
        self.path = Path(transcript)
        self.handle = None
        self.buffer = bytearray()

    def _ensure_open(self) -> None:
        # QEMU creates the file when it opens the chardev; wait for it to appear.
        while self.handle is None:
            try:
                self.handle = self.path.open("rb")
            except FileNotFoundError:
                time.sleep(0.05)

    def wait_for(self, marker: str, count: int = 1) -> None:
        self._ensure_open()
        marker_bytes = marker.encode()
        while self.buffer.count(marker_bytes) < count:
            chunk = self.handle.read()
            if chunk:
                self.buffer.extend(chunk)
            else:
                time.sleep(0.05)

    def wait_for_command_result(self) -> CommandResult:
        """Tail the transcript until the guest's END line; return the result.

        Raises StaleGuestInitError as soon as the transcript shows an /init
        from before the current command frame.
        """
        self._ensure_open()
        parser = CommandTranscriptParser()
        result = parser.feed(bytes(self.buffer))
        while result is None:
            chunk = self.handle.read()
            if chunk:
                self.buffer.extend(chunk)
                result = parser.feed(chunk)
            else:
                time.sleep(0.05)
        return result

    def close(self) -> None:
        if self.handle is not None:
            self.handle.close()


def build_qemu_command(
    qemu: str,
    qmp_socket: Path,
    serial_endpoint: Path,
    disk: Path,
    kernel: Path,
    initrd: Path,
    load_snapshot: Optional[str] = None,
    command_image: Optional[Path] = None,
) -> List[str]:
    command = [
        qemu,
        "-machine",
        "q35",
        "-cpu",
        "max",
        "-smp",
        "1",
        "-m",
        "512M",
        "-display",
        "none",
        "-monitor",
        "none",
    ]
    # The console is a plain file chardev: QEMU only writes to it, and nothing is
    # ever typed into the guest (the command arrives on a disk). A listening
    # unix-socket chardev would add a descriptor whose readiness depends on host
    # timing to QEMU's main-loop poll set; under `hermit run --no-rcb-time` that
    # lets the main loop monopolize the scheduler and starve the vCPU thread.
    command.extend(["-serial", "file:{}".format(serial_endpoint)])
    command.extend(
        [
            "-qmp",
            "unix:{},server=on,wait=off".format(qmp_socket),
            "-drive",
            "if=none,id=hermit-snapshot-store,file={},format=qcow2".format(disk),
        ]
    )
    if command_image is not None:
        # Present at BOOT as well as resume: vmstate records the device model, so a
        # device absent at snapshot cannot appear at resume. Only the backing FILE
        # differs between the two, and its size is fixed so the geometry matches.
        # readonly=on is required, not cosmetic: a writable raw drive is not
        # snapshottable and `savevm` refuses the save.
        command.extend(
            [
                "-drive",
                "if=none,id=hermit-command,file={},format=raw,readonly=on".format(
                    command_image
                ),
                "-device",
                "virtio-blk-pci,drive=hermit-command",
            ]
        )
    if load_snapshot is not None:
        command.extend(["-loadvm", load_snapshot])
    command.extend(
        [
            "-icount",
            "shift=0,sleep=off",
            "-rtc",
            "base=2022-01-01T00:00:00,clock=vm",
            "-kernel",
            str(kernel),
            "-initrd",
            str(initrd),
            "-append",
            KERNEL_COMMAND_LINE,
        ]
    )
    return command


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("boot", "resume"))
    parser.add_argument("--qemu", required=True)
    parser.add_argument("--qmp-socket", type=Path, required=True)
    # Fixed-size raw image holding the guest command; see build_qemu_command.
    parser.add_argument("--command-image", type=Path)
    parser.add_argument("--serial-log", type=Path, required=True)
    parser.add_argument("--disk", type=Path, required=True)
    parser.add_argument("--kernel", type=Path, required=True)
    parser.add_argument("--initrd", type=Path, required=True)
    parser.add_argument("--snapshot-name", default="hermit-boot")
    parser.add_argument("--timeout", type=float, required=True)
    parser.add_argument("--post-snapshot-name")
    parser.add_argument("--no-save-snapshot", action="store_true")
    return parser.parse_args()


def run_controller(arguments: argparse.Namespace) -> int:
    load_snapshot = arguments.snapshot_name if arguments.mode == "resume" else None
    serial_endpoint = arguments.serial_log
    if arguments.mode == "resume":
        if arguments.command_image is None:
            raise ValueError("resume mode requires --command-image")
    command = build_qemu_command(
        arguments.qemu,
        arguments.qmp_socket,
        serial_endpoint,
        arguments.disk,
        arguments.kernel,
        arguments.initrd,
        load_snapshot,
        arguments.command_image,
    )
    process = None
    serial = None
    try:
        process = subprocess.Popen(command, env=QEMU_ENV)
        wait_for_socket(arguments.qmp_socket, process, arguments.timeout)

        if arguments.mode == "boot":
            # QEMU writes the console straight to arguments.serial_log; tail it.
            serial = FileSerial(arguments.serial_log)
            serial.wait_for(BOOT_MARKER)
            # Snapshot the guest while it is polling the command disk, NOT at a
            # shell prompt: the prompt only existed to be typed into.
            serial.wait_for(COMMAND_DISK_MARKER)
            qmp_command(
                arguments.qmp_socket,
                "human-monitor-command",
                "command-line",
                "savevm {}".format(arguments.snapshot_name),
                blocking=True,
            )
            qmp_command(arguments.qmp_socket, "quit", blocking=True)
        else:
            # Nothing is written into the guest. The command image was populated
            # before QEMU launched, so the guest reads it and frames the output
            # itself; the controller only tails the transcript. It waits for the
            # END line, which the command's own output cannot produce, and
            # leaves the exit status on that line for the demo to report.
            serial = FileSerial(arguments.serial_log)
            serial.wait_for_command_result()
            if arguments.no_save_snapshot:
                # There is no input channel to tell the guest to shut down, so
                # stop QEMU from the control side.
                qmp_command(arguments.qmp_socket, "quit", blocking=True)
            else:
                if not arguments.post_snapshot_name:
                    raise ValueError("resume snapshot requires --post-snapshot-name")
                qmp_command(
                    arguments.qmp_socket,
                    "human-monitor-command",
                    "command-line",
                    "savevm {}".format(arguments.post_snapshot_name),
                    blocking=True,
                )
                qmp_command(arguments.qmp_socket, "quit", blocking=True)

        return process.wait(timeout=arguments.timeout)
    finally:
        if serial is not None:
            serial.close()
        stop_process(process)


if __name__ == "__main__":
    try:
        sys.exit(run_controller(parse_args()))
    except Exception as error:
        print("QEMU controller failed: {}".format(error), file=sys.stderr)
        sys.exit(1)
