#!/usr/bin/env python3
"""Resume the QEMU boot snapshot, run a command, and auto-verify repeats."""

import argparse
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import sys
import threading
import time


DEMO_DIR = Path(__file__).resolve().parent
DEMOS_DIR = DEMO_DIR.parent
ROOT = DEMOS_DIR.parent
sys.path.insert(0, str(DEMOS_DIR / "lib"))

from demo_common import (  # noqa: E402
    accept_snapshot_after_failed_rebuild,
    acquire_demo_lock,
    banner,
    BootSnapshotMismatch,
    canonicalize_qcow2_snapshot_timestamp,
    check_dependencies,
    compare_runs,
    copy_file,
    default_qemu_assets,
    display_path,
    drain_output,
    extract_info_tail,
    hash_file,
    hermit_binary,
    hermit_log_environment,
    hermit_tmp_args,
    LogCapExceeded,
    load_anchor,
    make_run_dir,
    print_comparison,
    print_header,
    release_demo_lock,
    run_checked,
    save_anchor,
    save_metadata,
    stage_guest_controller,
    stop_process,
    stop_process_group,
    verify_boot_snapshot,
    wait_for_process,
)
from qemu_controller import (  # noqa: E402
    BEGIN_LINE,
    CommandTranscriptParser,
    StaleGuestInitError,
    build_qemu_command,
    parse_command_transcript,
)
from signal_33 import settle_signal_33_disposition  # noqa: E402


DEMO_LABEL = "Demo 6: QEMU Snapshot Resume"
ASSETS = Path(os.environ.get("QEMU_ASSETS", default_qemu_assets(ROOT)))
QEMU = os.environ.get("QEMU_BIN", shutil.which("qemu-system-x86_64") or "")
TIMEOUT = int(os.environ.get("QEMU_TIMEOUT", "120"))
# Stop the run if Hermit's info log grows past this many bytes. Healthy resumes
# measured on 2026-09-30 on Hermit 0.2.0 gdc92644f96f4 with QEMU 10.1.2 wrote
# 169,751,813 bytes (`uname -a` without saving a snapshot, 10.0-11.4 s) and
# 258,227,125 to 259,021,561 bytes (three commands with a saved snapshot,
# 15.7-16.5 s); the cap is about 2.1 times the larger figure. (Hermit 0.2.0
# g770b95c505fa wrote 80 to 153 MB.)
MAX_LOG_BYTES = int(os.environ.get("QEMU_MAX_LOG_BYTES", str(512 * 1024 * 1024)))
SNAPSHOT_NAME = os.environ.get("QEMU_SNAPSHOT_NAME", "hermit-boot")
SNAPSHOT_DISK = Path(
    os.environ.get("QEMU_SNAPSHOT_DISK", ASSETS / "hermit-snapshot.qcow2")
)
BOOT_SNAPSHOT_DISK = Path(
    os.environ.get("QEMU_BOOT_SNAPSHOT_DISK", ASSETS / "hermit-boot.qcow2")
)
LOG_FILTER = os.environ.get(
    "QEMU_LOG_FILTER",
    "warn,detcore=info,reverie_ptrace::task=info",
)
# Without --epoch, `hermit run` starts virtual time at the host's current time
# (and logs that epoch so a run can be reproduced), so two resumes started at
# different times would not match. Every resume starts from the same fixed
# epoch, DetCore's own library default, as demo 5's boot does.
EPOCH = "2026-01-01T00:00:00Z"
# The guest sees the controller script and the assets directory (kernel,
# initramfs, snapshot disk, command image, serial log, QMP socket) at fixed
# paths, the same ones demo 5's boot uses. Their host paths depend on where the
# checkout is, and they appear on the guest's command lines, whose lengths change
# how many branches the controller and QEMU execute: passed as host paths, a
# fresh clone in another directory resumed to a different post-command snapshot
# after 288657 scheduler turns instead of 287149. The controller directory holds
# this run's private copy of the controller's sources (see
# stage_guest_controller), not demos/lib itself.
GUEST_CONTROLLER_DIR = Path("/tmp/hermit-demo-controller")
GUEST_ASSETS_DIR = Path("/tmp/hermit-demo-assets")
# The guest's working directory, instead of the checkout directory.
GUEST_WORKDIR = Path("/tmp")


def guest_path(host_path: Path) -> Path:
    """The path at which the guest sees ``host_path``.

    Files in ASSETS appear under GUEST_ASSETS_DIR; any other path (for example a
    QEMU_SNAPSHOT_DISK outside ASSETS) is passed unchanged.
    """
    try:
        return GUEST_ASSETS_DIR / Path(host_path).relative_to(ASSETS)
    except ValueError:
        return Path(host_path)


def guest_environment_args() -> list:
    """Hermit flags that give the controller the same environment on every run.

    By default the guest inherits the caller's whole environment, and the
    controller's branch count depends on it: a resume started through
    `make -C demos demo6` (which adds MAKEFLAGS and MAKELEVEL) would not match
    one started directly. The guest gets Hermit's minimal fixed environment plus
    only the variables the controller uses.
    """
    arguments = ["--base-env=minimal", "--env", "PYTHONDONTWRITEBYTECODE"]
    if "DEMO_QMP_TIMEOUT" in os.environ:
        arguments += ["--env", "DEMO_QMP_TIMEOUT"]
    return arguments


# Seconds to wait, after Hermit exits, for its last output to reach the log.
# Processes Hermit left running can hold its output open meanwhile;
# drain_output keeps QEMU_MAX_LOG_BYTES in force and, when this runs out,
# signals Hermit's process group, which reaches those still in it.
OUTPUT_DRAIN_TIMEOUT = 60


def start_output_copier(process: subprocess.Popen, log) -> threading.Thread:
    """Copy Hermit's output pipe into ``log`` on a thread; return the thread.

    The guest inherits Hermit's standard streams. When they were the log file
    itself, the controller's start-up fstat(1) reported how many log bytes had
    reached the file so far, which depends on host timing (a wrapper that relays
    Hermit's stderr delivers it late); demo 5's boot hit exactly this. The guest
    now reads /dev/null and writes to a pipe, whose metadata never changes, and
    a thread copies the pipe into the log.
    """

    def copy_output() -> None:
        try:
            while True:
                chunk = os.read(process.stdout.fileno(), 1 << 16)
                if not chunk:
                    break
                log.write(chunk)
        except (OSError, ValueError):
            pass  # the log was closed after a failed run; nothing more to keep
        finally:
            process.stdout.close()

    copier = threading.Thread(target=copy_output, daemon=True)
    copier.start()
    return copier


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--no-save-snapshot",
        action="store_true",
        help="run the command without saving a post-command snapshot",
    )
    parser.add_argument("command", nargs=argparse.REMAINDER, help="guest shell command")
    return parser.parse_args()


COMMAND_IMAGE_BYTES = 4096
# The guest's init reads only the first 512 bytes of the command disk
# (`dd bs=512 count=1` in demos/lib/qemu-assets.sh) and runs the first line of
# what it read. A longer command would reach the guest cut off at byte 512 and
# run in that cut-off form, so the host refuses it before starting anything.
GUEST_COMMAND_READ_BYTES = 512
# The command and the newline that ends it must both fit in what the guest reads.
MAX_GUEST_COMMAND_BYTES = GUEST_COMMAND_READ_BYTES - 1


def check_guest_command(command: str) -> bytes:
    """Return the command's UTF-8 bytes; refuse one the guest would not run whole."""
    if "\n" in command or "\r" in command:
        raise ValueError("guest command must be a single line")
    encoded = command.encode()
    if len(encoded) > MAX_GUEST_COMMAND_BYTES:
        raise ValueError(
            "guest command is {} bytes; the guest reads only the first {} bytes "
            "of its command disk, so a command can be at most {} bytes (UTF-8) "
            "plus the newline after it".format(
                len(encoded), GUEST_COMMAND_READ_BYTES, MAX_GUEST_COMMAND_BYTES
            )
        )
    return encoded


def write_command_image(path: Path, command: str) -> None:
    """Fixed-size raw image holding the guest command.

    The size is fixed because the same drive is attached at boot and at resume and
    only its backing file differs; a geometry change between the two would not
    match the device state recorded in the snapshot.
    """
    payload = check_guest_command(command) + b"\n"
    path.write_bytes(payload + b"\0" * (COMMAND_IMAGE_BYTES - len(payload)))


def failed_run_message(return_code: int, serial_log: Path) -> str:
    """Describe a failed resume, naming a stale guest /init when the log shows one."""
    message = "Hermit/QEMU exited with status {}".format(return_code)
    try:
        parse_command_transcript(serial_log.read_bytes())
    except OSError:
        # A run stopped early may not have written the serial log.
        pass
    except StaleGuestInitError as error:
        message += ": {}".format(error)
    return message


def guest_command_progress(serial_log: Path, hermit_exited: bool = False) -> str:
    """Say how far the guest's command had got, from the serial log.

    ``hermit_exited`` says Hermit had exited (the demo signalled Hermit's
    process group later, for processes it left running), so a finished
    command is not reported as one whose Hermit had not exited. A BEGIN line
    without an END line shows only that the command was not seen to finish: a
    command still running and one whose END line a kernel message hid leave
    the same serial log. In the same way, no
    BEGIN line shows only that the command was not seen to start: a BEGIN line
    that a kernel message split does not start the frame (see
    STALE_BEGIN_PREFIX_RE in qemu_controller).
    """
    try:
        transcript = serial_log.read_bytes()
    except OSError:
        return "QEMU had not written the serial log {}".format(serial_log)
    parser = CommandTranscriptParser()
    try:
        result = parser.feed(transcript)
    except StaleGuestInitError as error:
        return str(error)
    if result is not None:
        if hermit_exited:
            return "the guest command had finished with exit status {}".format(
                result.exit_status
            )
        return (
            "the guest command had finished with exit status {}, but Hermit/QEMU "
            "had not exited".format(result.exit_status)
        )
    if parser.started:
        return (
            "the guest command was not seen to finish: the serial log has the {} "
            "line but no END line".format(BEGIN_LINE.decode())
        )
    return "the guest was not seen to start the command: the serial log has no {} line".format(
        BEGIN_LINE.decode()
    )


def stopped_run_message(error: Exception, serial_log: Path) -> str:
    """Name the bound an unfinished resume reached and how far the guest got.

    A command that never exits, or one whose END line a kernel message hid,
    keeps the run going until it reaches a bound. With the default settings the
    INFO log cap is reached first: on 2026-10-03 Hermit wrote 18.5 to 19.2 MB
    of INFO log per second of resume, so a `sleep 1000000` reached the 512 MiB
    cap after about 29 seconds, well before the 120-second QEMU_TIMEOUT.
    Neither bound can end in SUCCESS. The cap also holds after Hermit exits,
    while processes it left running still write to its output (see
    drain_output); LogCapExceeded then carries Hermit's exit status. It
    carries it too when the log was found past the cap by the check made once
    Hermit had exited, or once the copy of its output had ended
    (``final_check``); nothing was then seen still writing.

    The time a cap message gives is when the size was found past the cap, read
    before anything was signalled. The message names QEMU_TIMEOUT without saying
    whether it had passed by then: the size is checked before the deadline, so
    a log past the cap is what is reported even when both bounds were passed.

    By the time this runs, wait_for_process or drain_output has signalled
    Hermit's process group, the one the demo started Hermit in (see
    stop_process_group), and that is all a cap message says about it: nothing
    reports whether the group emptied, and a process outside it is not
    signalled. Under a wrapper such as bin/safehermit, Hermit, its tracer and
    QEMU can run outside that group.
    """
    if isinstance(error, LogCapExceeded) and error.final_check:
        cause = (
            "Hermit's INFO log {} was {} bytes, past the {}-byte cap "
            "(QEMU_MAX_LOG_BYTES), when checked {:.1f}s into the resume, after "
            "Hermit had exited with status {}, and the demo then signalled "
            "Hermit's process group".format(
                error.log_path,
                error.log_size,
                error.max_log_bytes,
                error.elapsed,
                error.exit_status,
            )
        )
        return "{}; {}".format(cause, guest_command_progress(serial_log, hermit_exited=True))
    if isinstance(error, LogCapExceeded) and error.exit_status is not None:
        cause = (
            "Hermit's INFO log {} grew to {} bytes, past the {}-byte cap "
            "(QEMU_MAX_LOG_BYTES), {:.1f}s into the resume, after Hermit had "
            "exited with status {}: processes it left running still wrote to its "
            "output, and the demo then signalled Hermit's process group".format(
                error.log_path,
                error.log_size,
                error.max_log_bytes,
                error.elapsed,
                error.exit_status,
            )
        )
        return "{}; {}".format(cause, guest_command_progress(serial_log, hermit_exited=True))
    if isinstance(error, LogCapExceeded):
        cause = (
            "Hermit's INFO log {} grew to {} bytes, past the {}-byte cap "
            "(QEMU_MAX_LOG_BYTES), when checked {:.1f}s into the resume "
            "(QEMU_TIMEOUT is {}s), so the demo signalled Hermit's process "
            "group".format(
                error.log_path,
                error.log_size,
                error.max_log_bytes,
                error.elapsed,
                TIMEOUT,
            )
        )
    else:
        cause = "Hermit/QEMU did not exit within QEMU_TIMEOUT ({}s)".format(TIMEOUT)
    return "{}; {}".format(cause, guest_command_progress(serial_log))


def ensure_boot_snapshot() -> None:
    """Use demo 5's boot snapshot only if it was booted from the current initramfs.

    The snapshot's memory holds the guest /init that runs the command, so a
    snapshot booted from another initramfs runs another /init (see
    verify_boot_snapshot). The default snapshot is built, or rebuilt, by running
    demo 5. A custom QEMU_BOOT_SNAPSHOT_DISK is never built or replaced here: if
    it is missing or does not match, the demo stops and says how to rebuild it.
    """
    default_snapshot = ASSETS / "hermit-boot.qcow2"
    custom = BOOT_SNAPSHOT_DISK != default_snapshot
    stale = False
    if not BOOT_SNAPSHOT_DISK.is_file():
        if custom:
            raise RuntimeError(
                "missing custom boot snapshot: {}; produce it before Demo 6".format(
                    BOOT_SNAPSHOT_DISK
                )
            )
        print("Demo 5 boot snapshot missing; running demo 5 first...", flush=True)
    else:
        try:
            verify_boot_snapshot(BOOT_SNAPSHOT_DISK, ROOT, ASSETS)
            return
        except BootSnapshotMismatch as mismatch:
            if custom:
                raise RuntimeError(
                    "refusing to restore the custom boot snapshot {} "
                    "(QEMU_BOOT_SNAPSHOT_DISK): {}. The snapshot's memory holds "
                    "the guest /init that runs the command and frames its output, "
                    "so a snapshot booted from another initramfs runs another "
                    "/init; an /init from an older initramfs runs the command as "
                    "root with the console as its standard input. Rebuild it by "
                    "running demo 5 with QEMU_SNAPSHOT_DISK={} and the same "
                    "QEMU_ASSETS, or unset QEMU_BOOT_SNAPSHOT_DISK to use the "
                    "default snapshot, which demo 6 rebuilds itself".format(
                        BOOT_SNAPSHOT_DISK, mismatch, BOOT_SNAPSHOT_DISK
                    )
                ) from mismatch
            print(
                "Demo 5 boot snapshot {} is not from the current initramfs: {}; "
                "running demo 5 again to rebuild it...".format(
                    BOOT_SNAPSHOT_DISK, mismatch
                ),
                flush=True,
            )
            stale = True
    try:
        run_checked(
            ["make", "--no-print-directory", "-C", str(DEMOS_DIR), "demo5"],
            cwd=ROOT,
        )
    except subprocess.CalledProcessError as failure:
        if not stale:
            raise
        # The snapshot did not match the current initramfs, so demo 5's saved
        # reference run may not either, and demo 5 then exits non-zero after
        # saving a current snapshot. Its record, not its exit status, is the
        # verdict.
        accept_snapshot_after_failed_rebuild(
            BOOT_SNAPSHOT_DISK, ROOT, ASSETS, failure, "demo 6"
        )
        return
    if not BOOT_SNAPSHOT_DISK.is_file():
        raise RuntimeError("Demo 5 did not produce {}".format(BOOT_SNAPSHOT_DISK))
    try:
        verify_boot_snapshot(BOOT_SNAPSHOT_DISK, ROOT, ASSETS)
    except BootSnapshotMismatch as mismatch:
        raise RuntimeError(
            "Demo 5 ran, but {} still does not match the current initramfs: "
            "{}".format(BOOT_SNAPSHOT_DISK, mismatch)
        ) from mismatch


def resume_once(guest_command: str, save_snapshot: bool) -> str:
    """Resume once, compare with this command's reference run, return the verdict."""

    os.environ["QEMU_BIN"] = QEMU
    os.environ["QEMU_ASSETS"] = str(ASSETS)
    dependency = check_dependencies(ROOT)
    print_header(
        DEMO_LABEL,
        (
            "QEMU restores the live shell, runs one command, saves a post-command",
            "snapshot by default, and compares repeats keyed by the command string.",
        ),
        dependency,
    )
    hermit = hermit_binary()
    if not QEMU:
        raise RuntimeError("qemu-system-x86_64 is required")
    ensure_boot_snapshot()

    command_digest = hashlib.sha256(guest_command.encode()).hexdigest()
    # A --no-save-snapshot run writes no post-command snapshot and runs a
    # different controller command line (so a different Hermit log), so it can
    # never match a reference that saved one. It keeps its own reference.
    reference_key = command_digest if save_snapshot else command_digest + "-no-save-snapshot"
    command_root = ASSETS / "resume-metadata" / reference_key
    lock = acquire_demo_lock(ASSETS / ".qemu-demo.lock")
    run_dir = make_run_dir(command_root, "resume")
    controller_dir = stage_guest_controller(run_dir / "controller")
    # Only the guest uses the QMP socket, through GUEST_ASSETS_DIR, so its path is
    # always short enough for AF_UNIX; the host path is kept for cleanup.
    qmp_socket = ASSETS / "qmp.sock"
    serial_log = ASSETS / "serial.log"
    archived_serial_log = run_dir / "serial.log"
    info_log = run_dir / "hermit-info.log"
    output_path = run_dir / "guest-output.txt"
    archived_disk = run_dir / "post-command.qcow2"
    copy_file(BOOT_SNAPSHOT_DISK, SNAPSHOT_DISK)
    process = None
    saved_snapshot = save_snapshot

    try:
        # Demo 5 does not take the demo lock, so it may have replaced the boot
        # snapshot, or its record, since ensure_boot_snapshot checked them.
        # Check the copy that QEMU restores.
        try:
            verify_boot_snapshot(BOOT_SNAPSHOT_DISK, ROOT, ASSETS, disk=SNAPSHOT_DISK)
        except BootSnapshotMismatch as mismatch:
            raise RuntimeError(
                "the copy {} of the boot snapshot {} does not match demo 5's "
                "record: {}; demo 5 may have replaced the boot snapshot after "
                "this demo checked it, so run demo 6 again".format(
                    SNAPSHOT_DISK, BOOT_SNAPSHOT_DISK, mismatch
                )
            ) from mismatch
        for runtime_path in (qmp_socket, serial_log):
            runtime_path.unlink(missing_ok=True)
        # The command reaches the guest on a disk, not through the console. The
        # fixed-size image is written before QEMU starts, so its contents are
        # present the instant the guest resumes and nothing depends on when the
        # host sends bytes. The path is stable rather than per-run because it
        # appears in the QEMU argv that the repeat check compares; the demo lock
        # serializes runs, so sharing it is safe.
        command_image = ASSETS / "guest-command.img"
        write_command_image(command_image, guest_command)
        qemu_argv = build_qemu_command(
            QEMU,
            guest_path(qmp_socket),
            guest_path(serial_log),
            guest_path(SNAPSHOT_DISK),
            guest_path(ASSETS / "bzImage"),
            guest_path(ASSETS / "initramfs.cpio.gz"),
            SNAPSHOT_NAME,
            guest_path(command_image),
        )
        post_name = "command-{}".format(command_digest[:16])
        command = [
            hermit,
            "run",
            *hermit_tmp_args(ROOT),
            "--bind",
            "{}:{}".format(controller_dir, GUEST_CONTROLLER_DIR),
            "--bind",
            "{}:{}".format(ASSETS, GUEST_ASSETS_DIR),
            "--workdir",
            str(GUEST_WORKDIR),
            *guest_environment_args(),
            "--strict",
            "--epoch",
            EPOCH,
            "--no-rcb-time",
            "--target-timeslice",
            "100000",
            "--max-timeslice",
            "disabled",
            "--",
            sys.executable,
            str(GUEST_CONTROLLER_DIR / "qemu_controller.py"),
            "resume",
            "--qemu",
            QEMU,
            "--qmp-socket",
            str(guest_path(qmp_socket)),
            "--command-image",
            str(guest_path(command_image)),
            "--serial-log",
            str(guest_path(serial_log)),
            "--disk",
            str(guest_path(SNAPSHOT_DISK)),
            "--kernel",
            str(guest_path(ASSETS / "bzImage")),
            "--initrd",
            str(guest_path(ASSETS / "initramfs.cpio.gz")),
            "--snapshot-name",
            SNAPSHOT_NAME,
            "--timeout",
            str(TIMEOUT),
            "--post-snapshot-name",
            post_name,
        ]
        if not saved_snapshot:
            command.append("--no-save-snapshot")
        environment = hermit_log_environment(LOG_FILTER)
        # The guest controller imports demo_common. Suppress CPython's bytecode
        # write: the guest compiles demo_common from source on every run and
        # leaves the staged copy exactly as stage_guest_controller wrote it.
        # (guest_environment_args passes this variable through to the guest.)
        environment["PYTHONDONTWRITEBYTECODE"] = "1"
        banner("Resume {} and run: {}".format(SNAPSHOT_NAME, guest_command))
        print("Restoring snapshot (timeout: {}s)...".format(TIMEOUT), flush=True)
        with info_log.open("wb", buffering=0) as log:
            process = subprocess.Popen(
                command,
                # The guest inherits these streams; see start_output_copier.
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                env=environment,
                cwd=str(ROOT),
                # Own process group, so stop_process_group can signal every
                # process in it, not only the first one.
                start_new_session=True,
            )
            launched = time.monotonic()
            copier = start_output_copier(process, log)
            try:
                return_code = wait_for_process(
                    process,
                    TIMEOUT,
                    progress_label="Hermit/QEMU resume",
                    log_path=info_log,
                    max_log_bytes=MAX_LOG_BYTES,
                )
                drain_output(
                    copier,
                    process,
                    OUTPUT_DRAIN_TIMEOUT,
                    log_path=info_log,
                    max_log_bytes=MAX_LOG_BYTES,
                    started=launched,
                    label="Hermit",
                )
            except (LogCapExceeded, TimeoutError) as error:
                print(flush=True)
                raise RuntimeError(stopped_run_message(error, serial_log)) from error
            finally:
                # Whatever ended the wait, stop everything left in Hermit's
                # process group, so nothing it started keeps writing the log.
                # The pipe's last bytes then arrive at once. A copier still
                # running after 10 seconds is reading from a process outside
                # the group; closing the log at the end of this block stops it
                # at its next write.
                stop_process_group(process)
                copier.join(10)
        # Check the exit status before reading any artifact: a run stopped early
        # may not have written the serial log yet.
        if return_code != 0:
            raise RuntimeError(failed_run_message(return_code, serial_log))
        transcript = serial_log.read_bytes()

        copy_file(serial_log, archived_serial_log)
        command_result = parse_command_transcript(transcript)
        if command_result is None:
            raise RuntimeError(
                "Hermit/QEMU exited 0, but the serial log {} holds no complete "
                "command frame: a {} line, the command's output, then an END "
                "line with the command's exit status".format(
                    display_path(archived_serial_log, ROOT),
                    BEGIN_LINE.decode(),
                )
            )
        # The command's stdout and stderr without the guest's "| " prefix, plus
        # any console line printed inside the frame, marked "[console] ".
        guest_output = command_result.output
        output_path.write_bytes(guest_output)
        banner("Guest serial output")
        sys.stdout.buffer.write(guest_output)
        sys.stdout.buffer.flush()
        print("Guest command exit status: {}".format(command_result.exit_status))

        banner("Hermit INFO tail (wall-clock timestamps stripped)")
        for line in extract_info_tail(info_log):
            print(line)

        extra = {
            "kind": "qemu-resume",
            "command": guest_command,
            "command_sha256": command_digest,
            "guest_output": str(output_path.resolve()),
            "guest_output_sha256": hashlib.sha256(guest_output).hexdigest(),
            # Reported and compared between runs; a nonzero status is the
            # command's own result, not a demo failure.
            "guest_exit_status": command_result.exit_status,
            "qemu_argv": qemu_argv,
            "serial_log": str(archived_serial_log.resolve()),
            "snapshot_saved": saved_snapshot,
        }
        if saved_snapshot:
            canonicalize_qcow2_snapshot_timestamp(SNAPSHOT_DISK, post_name)
            copy_file(SNAPSHOT_DISK, archived_disk)
            extra["snapshot_date_nsec_canonicalized"] = True
        current = save_metadata(
            run_dir, archived_disk if saved_snapshot else None, info_log, extra
        )
        anchor = load_anchor(command_root)
        banner("Automatic repeat verification")
        result = "FIRST RUN SAVED"
        if anchor is None:
            anchor_path = save_anchor(command_root, current)
            print(
                "Saved this run as the reference run for this command at {}".format(
                    display_path(anchor_path, ROOT)
                )
            )
        else:
            passed, report = compare_runs(anchor, current)
            print_comparison(
                passed,
                report,
                current.qcow2_sha256,
                "Resume",
            )
            result = "SUCCESS" if passed else "PARTIAL"

        if saved_snapshot:
            print("Post-command snapshot: {}".format(display_path(archived_disk, ROOT)))
            print("Post-command SHA-256: {}".format(hash_file(archived_disk)))
        print(
            "Run metadata: {}".format(display_path(run_dir / "run-metadata.json", ROOT))
        )
        print("\n=== {}: {} ===".format(DEMO_LABEL, result))
        return result
    finally:
        stop_process(process)
        qmp_socket.unlink(missing_ok=True)
        release_demo_lock(lock)


def main() -> int:
    # Before the first `hermit`; see settle_signal_33_disposition.
    settle_signal_33_disposition()
    arguments = parse_args()
    guest_command = " ".join(arguments.command).strip() or "uname -a"
    # Refuse a command the guest cannot run whole before taking the demo lock,
    # copying the boot snapshot, or starting Hermit.
    check_guest_command(guest_command)
    save_snapshot = not arguments.no_save_snapshot
    result = resume_once(guest_command, save_snapshot)
    # The first run of a command only records its reference run. Resume a second
    # time so a single invocation always performs a comparison. Set
    # QEMU_RESUME_REPEAT=0 to skip the second resume.
    if result == "FIRST RUN SAVED" and os.environ.get("QEMU_RESUME_REPEAT", "1") != "0":
        banner("Resume again and compare with the reference run just saved")
        result = resume_once(guest_command, save_snapshot)
    return 1 if result == "PARTIAL" else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as error:
        print("WARN: {}: FAILURE: {}".format(DEMO_LABEL, error), file=sys.stderr)
        sys.exit(1)
