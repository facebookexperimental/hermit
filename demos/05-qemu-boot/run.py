#!/usr/bin/env python3
"""Boot Linux under Hermit, save a QEMU snapshot, and auto-verify repeats."""

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
    archive_result_dir,
    banner,
    booted_initramfs_producer,
    canonicalize_qcow2_snapshot_timestamp,
    canonicalize_qemu_runtime_path,
    canonicalize_qemu_runtime_paths_in_file,
    check_dependencies,
    check_qemu_dependencies,
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
    load_committed_anchor,
    make_temp_result_dir,
    print_comparison,
    print_header,
    publish_anchor,
    publish_file_atomic,
    run_checked,
    save_metadata,
    stage_boot_assets,
    stage_guest_controller,
    stop_process,
    stop_process_group,
    wait_for_process,
    write_boot_snapshot_record,
)
from qemu_controller import build_qemu_command  # noqa: E402
from signal_33 import settle_signal_33_disposition  # noqa: E402


DEMO_LABEL = "Demo 5: QEMU Linux Snapshot"
ASSETS = Path(os.environ.get("QEMU_ASSETS", default_qemu_assets(ROOT)))
QEMU = os.environ.get("QEMU_BIN", shutil.which("qemu-system-x86_64") or "")
TIMEOUT = int(os.environ.get("QEMU_TIMEOUT", "600"))
# Cap on hermit-info.log. Eight healthy boots on 2026-09-30 wrote 457,618,652
# bytes each, and one `make -C demos demo5` took 75.7 s of wall time (Hermit
# 0.2.0 gdc92644f96f4, QEMU 10.1.2; Hermit 0.2.0 g770b95c505fa wrote about
# 253 MB), so 768 MiB is about 1.76 times a healthy run. A runaway log is
# stopped at the cap instead of filling the disk.
MAX_LOG_BYTES = int(os.environ.get("QEMU_MAX_LOG_BYTES", str(768 * 1024 * 1024)))
SNAPSHOT_NAME = os.environ.get("QEMU_SNAPSHOT_NAME", "hermit-boot")
# By default each concurrent run keeps its snapshot disk inside its own private
# working directory (computed in main); QEMU_SNAPSHOT_DISK forces a fixed path.
SNAPSHOT_DISK_OVERRIDE = os.environ.get("QEMU_SNAPSHOT_DISK")
SNAPSHOT_SIZE = os.environ.get("QEMU_SNAPSHOT_SIZE", "64M")
LOG_FILTER = os.environ.get(
    "QEMU_LOG_FILTER",
    "warn,detcore=info,reverie_ptrace::task=info",
)
# Without --epoch, `hermit run` starts virtual time at the host's current time
# (and logs that epoch so a run can be reproduced). Two boots started a minute
# apart then differ: guest kernel timestamps drift by a microsecond and the
# snapshot changes. Every boot therefore starts from the same fixed epoch,
# DetCore's own library default.
EPOCH = "2026-01-01T00:00:00Z"
# The guest (the controller and QEMU) sees this run's private working directory
# at this fixed path, bind-mounted inside Hermit's private /tmp. The host
# directory has a random name (see make_temp_result_dir), and the controller's
# branch count depends on the characters of the paths it parses: two boots whose
# working directories differed only in that random suffix wrote the same
# snapshot but different Hermit logs.
GUEST_RUN_DIR = Path("/tmp/hermit-demo5-run")
# The controller script and the kernel and initramfs reach the guest at fixed
# paths too. Their host paths depend on where the checkout is, and they appear
# on the guest's command lines (the controller's and QEMU's), whose lengths
# change how many branches the controller and QEMU execute while parsing them.
# Passed as host paths, a fresh clone in another directory booted to a
# different snapshot, 198360 scheduler turns instead of 198340, and a longer
# Hermit log. The controller directory holds this run's private copy of the
# controller's sources (see stage_guest_controller), not demos/lib itself.
GUEST_CONTROLLER_DIR = Path("/tmp/hermit-demo-controller")
GUEST_ASSETS_DIR = Path("/tmp/hermit-demo-assets")


def guest_environment_args() -> list:
    """Hermit flags that give the controller the same environment on every run.

    By default the guest inherits the caller's whole environment, and the
    controller's branch count depends on it: a boot started through
    `make -C demos demo5` (which adds MAKEFLAGS and MAKELEVEL) would not match
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
    reached the file so far. That count depends on host timing (a wrapper that
    relays Hermit's stderr delivers it late), so two boots wrote the same
    snapshot but different Hermit logs. The guest now reads /dev/null and writes
    to a pipe, whose metadata never changes, and this thread copies the pipe
    into the log.
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


def snapshot_exists(path: Path, name: str) -> bool:
    result = subprocess.run(
        ["qemu-img", "snapshot", "-l", str(path)],
        stdout=subprocess.PIPE,
        check=True,
        text=True,
    )
    return any(line.split()[1:2] == [name] for line in result.stdout.splitlines())


COMMAND_IMAGE_BYTES = 4096
PLACEHOLDER_COMMAND = "WAIT"


def write_placeholder_command_image(path: Path) -> None:
    """The image the guest polls before resume supplies a real command.

    Same fixed size as the resume image (demo 6 writes that one): the drive is
    attached at boot and at resume and only the backing file differs, so a geometry
    change between the two would not match the device state in the snapshot.
    """
    payload = PLACEHOLDER_COMMAND.encode() + b"\n"
    path.write_bytes(payload + b"\0" * (COMMAND_IMAGE_BYTES - len(payload)))


def boot_once() -> str:
    """Boot once, compare with the reference run if there is one, return the verdict."""
    os.environ["QEMU_BIN"] = QEMU
    os.environ["QEMU_DEMO_PYTHON"] = sys.executable
    qemu_dependency = check_qemu_dependencies(ROOT)
    dependency = check_dependencies(ROOT)
    hermit = hermit_binary()
    print_header(
        DEMO_LABEL,
        (
            "Hermit boots QEMU/Linux, streams the serial console, saves a live snapshot,",
            "and compares every repeat run with the first run.",
        ),
        dependency + "\n" + qemu_dependency,
    )
    banner("Verify QEMU kernel and initramfs")
    run_checked([str(DEMOS_DIR / "lib/qemu-assets.sh")], cwd=ROOT)
    if not QEMU:
        raise RuntimeError("qemu-system-x86_64 is required")

    ASSETS.mkdir(parents=True, exist_ok=True)
    anchor_dir = ASSETS / "boot-anchor"
    # Everything for this run lives in a private working directory so any number
    # of runs can boot QEMU concurrently without sharing sockets, disks, or logs.
    run_dir = make_temp_result_dir(ASSETS, "boot")
    controller_dir = stage_guest_controller(run_dir / "controller")
    # The kernel and initramfs this boot runs are private copies, bound at
    # GUEST_ASSETS_DIR in place of the shared asset directory, so the guest
    # paths QEMU is given do not change. Another checkout may replace the shared
    # initramfs at any time, so QEMU must not read it: the record next to the
    # snapshot names the SHA-256 of the copy QEMU boots, and the
    # INITRAMFS_VERSION that qemu-assets.sh's build record names for exactly
    # those bytes (booted_initramfs_producer), so demo 6 can tell whether the
    # /init in the snapshot's memory came from the initramfs that qemu-assets.sh
    # builds now (see verify_boot_snapshot).
    boot_assets = run_dir / "boot-assets"
    # Only the guest uses the QMP socket, through GUEST_RUN_DIR, so its path is
    # always short enough for AF_UNIX; the host path is kept for cleanup.
    qmp_socket = run_dir / "qmp.sock"
    # The serial console is a `-serial file:` transcript that QEMU writes and the
    # controller tails for boot markers (see build_qemu_command for why it is not
    # a socket).
    serial_log = run_dir / "serial.log"
    info_log = run_dir / "hermit-info.log"
    snapshot_disk = (
        Path(SNAPSHOT_DISK_OVERRIDE)
        if SNAPSHOT_DISK_OVERRIDE
        else run_dir / "hermit-snapshot.qcow2"
    )

    def guest_path(host_path: Path) -> Path:
        """The path at which the guest sees a file in boot_assets, run_dir or
        ASSETS. boot_assets is inside run_dir, so it is matched first."""
        for host_dir, guest_dir in (
            (boot_assets, GUEST_ASSETS_DIR),
            (run_dir, GUEST_RUN_DIR),
            (ASSETS, GUEST_ASSETS_DIR),
        ):
            try:
                return guest_dir / Path(host_path).relative_to(host_dir)
            except ValueError:
                continue
        return Path(host_path)

    process = None

    try:
        stage_boot_assets(ASSETS, boot_assets)
        booted_initramfs = boot_assets / "initramfs.cpio.gz"
        # Refuses, before the boot, a copy that qemu-assets.sh did not build at
        # the version this checkout builds.
        producer = booted_initramfs_producer(ROOT, ASSETS, booted_initramfs)
        # GUEST_ASSETS_DIR now shows boot_assets, so a QEMU_SNAPSHOT_DISK in the
        # asset directory is bound at the guest path it had when the whole
        # asset directory was bound there.
        snapshot_disk_binds = []
        if snapshot_disk.is_relative_to(ASSETS) and not snapshot_disk.is_relative_to(
            run_dir
        ):
            mount_point = boot_assets / snapshot_disk.relative_to(ASSETS)
            mount_point.parent.mkdir(parents=True, exist_ok=True)
            mount_point.touch()
            snapshot_disk_binds = [
                "--bind",
                "{}:{}".format(snapshot_disk, guest_path(snapshot_disk)),
            ]
        run_checked(
            [
                "qemu-img",
                "create",
                "-q",
                "-f",
                "qcow2",
                str(snapshot_disk),
                SNAPSHOT_SIZE,
            ]
        )

        # Boot attaches the command drive with a PLACEHOLDER, so the device exists in
        # the snapshot. Resume swaps only its backing file; a device absent here could
        # not appear there, because vmstate records the device model.
        command_image = run_dir / "guest-command.img"
        write_placeholder_command_image(command_image)
        qemu_argv = build_qemu_command(
            QEMU,
            guest_path(qmp_socket),
            guest_path(serial_log),
            guest_path(snapshot_disk),
            guest_path(boot_assets / "bzImage"),
            guest_path(booted_initramfs),
            None,
            guest_path(command_image),
        )
        command = [
            hermit,
            "run",
            *hermit_tmp_args(ROOT),
            "--bind",
            "{}:{}".format(controller_dir, GUEST_CONTROLLER_DIR),
            "--bind",
            "{}:{}".format(boot_assets, GUEST_ASSETS_DIR),
            *snapshot_disk_binds,
            "--bind",
            "{}:{}".format(run_dir, GUEST_RUN_DIR),
            # The guest would otherwise start in the checkout directory.
            "--workdir",
            str(GUEST_RUN_DIR),
            *guest_environment_args(),
            "--strict",
            "--epoch",
            EPOCH,
            # Keep branch-count (PMU) preemption armed with a large but finite
            # --max-timeslice. With preemption disabled and --no-rcb-time, idle
            # poll-yields kept the run queue busy, virtual time never jumped
            # forward, and the boot stalled at HPET calibration.
            "--target-timeslice",
            "100000",
            "--max-timeslice",
            "2000000000",
            "--",
            sys.executable,
            str(GUEST_CONTROLLER_DIR / "qemu_controller.py"),
            "boot",
            "--qemu",
            QEMU,
            "--qmp-socket",
            str(guest_path(qmp_socket)),
            "--serial-log",
            str(guest_path(serial_log)),
            "--disk",
            str(guest_path(snapshot_disk)),
            "--kernel",
            str(guest_path(boot_assets / "bzImage")),
            "--initrd",
            str(guest_path(booted_initramfs)),
            "--command-image",
            str(guest_path(command_image)),
            "--snapshot-name",
            SNAPSHOT_NAME,
            "--timeout",
            str(TIMEOUT),
        ]
        environment = hermit_log_environment(LOG_FILTER)
        # The guest controller imports demo_common. Suppress CPython's bytecode
        # write: the guest compiles demo_common from source on every run and
        # leaves the staged copy exactly as stage_guest_controller wrote it.
        # (When the guest imported from the shared demos/lib, concurrent runs
        # also raced on the same .pyc: one won openat(O_CREAT|O_EXCL), the other
        # saw EEXIST, and their traces diverged.) guest_environment_args passes
        # this variable through to the guest.
        environment["PYTHONDONTWRITEBYTECODE"] = "1"
        banner("Boot Linux to its serial shell (1st line takes a while to appear)")
        with info_log.open("wb", buffering=0) as log:
            process = subprocess.Popen(
                command,
                # The guest inherits these streams; see start_output_copier.
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                env=environment,
                cwd=str(ROOT),
                # Own process group, so stop_process can take the whole tree down:
                # a signal to the hermit PID alone can leave a second Hermit
                # process running and writing hermit-info.log.
                start_new_session=True,
            )
            launched = time.monotonic()
            copier = start_output_copier(process, log)
            try:
                return_code = wait_for_process(
                    process,
                    TIMEOUT,
                    stream_path=serial_log,
                    first_output_label="Waiting for first serial line",
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
            finally:
                # Whatever ended the wait, stop everything left in Hermit's
                # process group, so nothing it started keeps writing the log.
                # The pipe's last bytes then arrive at once. A copier still
                # running after 10 seconds is reading from a process outside
                # the group; closing the log at the end of this block stops it
                # at its next write.
                stop_process_group(process)
                copier.join(10)
        if return_code != 0:
            raise RuntimeError("Hermit/QEMU exited with status {}".format(return_code))
        if hash_file(booted_initramfs) != producer["initramfs_sha256"]:
            raise RuntimeError(
                "the initramfs copy {} changed during the boot".format(booted_initramfs)
            )
        # The copies are not part of the run's results.
        shutil.rmtree(str(boot_assets))
        if not snapshot_exists(snapshot_disk, SNAPSHOT_NAME):
            raise RuntimeError("snapshot {} was not saved".format(SNAPSHOT_NAME))
        canonicalize_qcow2_snapshot_timestamp(snapshot_disk, SNAPSHOT_NAME)
        # Every check of the boot itself comes before the snapshot's .id file,
        # its records and its publication, so a published snapshot with a
        # matching record means the boot passed them. Demos 6 and 7 rely on
        # that when demo 5 exits non-zero after a rebuild
        # (accept_snapshot_after_failed_rebuild): after publication come only
        # the archive copy, the display, the run's metadata and the comparison
        # with the reference run. Review finding R7-6 on
        # https://github.com/rrnewton/hermit/pull/3703.
        serial_text = serial_log.read_text(errors="replace")
        if "2022-01-01T" not in serial_text:
            raise RuntimeError("serial transcript lacks the fixed RTC epoch")

        snapshot_sha = hash_file(snapshot_disk)
        (snapshot_disk.with_suffix(snapshot_disk.suffix + ".id")).write_text(
            snapshot_sha + "\n"
        )
        if SNAPSHOT_DISK_OVERRIDE:
            # Demo 6 restores an explicit QEMU_BOOT_SNAPSHOT_DISK only with this
            # record next to it.
            write_boot_snapshot_record(snapshot_disk, snapshot_sha, producer)
        # Shared handoff artifact consumed by demo 6; publish atomically so a
        # concurrent reader never sees a half-written qcow2. Its record follows
        # it; a reader that finds the new snapshot with the previous record sees
        # a SHA-256 that does not match and does not restore it.
        baseline_disk = ASSETS / "hermit-boot.qcow2"
        publish_file_atomic(snapshot_disk, baseline_disk)
        write_boot_snapshot_record(baseline_disk, snapshot_sha, producer)
        archived_disk = run_dir / "boot-snapshot.qcow2"
        copy_file(snapshot_disk, archived_disk)
        # serial_log already lives inside run_dir, so it is published with the
        # anchor/archive directly; no separate copy step is needed.

        banner("Snapshot ready")
        snapshot_display = display_path(snapshot_disk, ROOT)
        print(
            "Snapshot disk: {} (internal tag: {})".format(snapshot_display, SNAPSHOT_NAME)
        )
        run_checked(["qemu-img", "snapshot", "-l", str(snapshot_disk)])
        print("Snapshot SHA-256: {}".format(snapshot_sha))

        banner("Hermit INFO tail (wall-clock timestamps stripped)")
        for line in extract_info_tail(info_log):
            print(line)

        # The qemu argv and the Hermit INFO log both embed this run's private
        # working directory (sockets and snapshot disk), which differs between
        # concurrent runs by design. Fold that path to a stable token so the
        # anchor comparison reflects genuine differences, not the per-run temp
        # directory name.
        canonical_argv = [
            canonicalize_qemu_runtime_path(arg, run_dir, qmp_socket)
            for arg in qemu_argv
        ]
        canonicalize_qemu_runtime_paths_in_file(info_log, run_dir, qmp_socket)
        current = save_metadata(
            run_dir,
            archived_disk,
            info_log,
            {
                "kind": "qemu-boot",
                "snapshot_name": SNAPSHOT_NAME,
                "snapshot_date_nsec_canonicalized": True,
                "qemu_argv": canonical_argv,
                "serial_log": str(serial_log.resolve()),
                "serial_sha256": hash_file(serial_log),
            },
        )
        # Remove the run's QMP socket before publishing so it never pollutes the
        # anchor (or the archived run) directory. (Serial is a file, not a
        # socket, and lives on as the transcript.)
        qmp_socket.unlink(missing_ok=True)
        # Drop the working snapshot copy before publishing: the archived
        # boot-snapshot.qcow2 plus the metadata's qcow2_sha256 retain everything
        # needed, so keeping it too would double the per-run disk footprint. An
        # explicitly overridden QEMU_SNAPSHOT_DISK is left in place.
        if not SNAPSHOT_DISK_OVERRIDE:
            snapshot_disk.unlink(missing_ok=True)
            snapshot_disk.with_suffix(snapshot_disk.suffix + ".id").unlink(
                missing_ok=True
            )

        banner("Automatic repeat verification")
        # The first run on this machine becomes the reference run, claimed with
        # one atomic, no-clobber rename; every later run compares against it.
        won_anchor = publish_anchor(run_dir, anchor_dir)
        if won_anchor:
            final_dir = anchor_dir
            result = "FIRST RUN SAVED"
            print(
                "Saved this run as the reference run at {}".format(
                    display_path(anchor_dir, ROOT)
                )
            )
        else:
            anchor = load_committed_anchor(anchor_dir)
            if anchor is None:
                raise RuntimeError(
                    "published boot anchor has no readable run-metadata.json"
                )
            # Compare while the run dir is still in place (its info_log path is
            # valid), then archive it into run-history.
            passed, report = compare_runs(anchor, current)
            final_dir = archive_result_dir(run_dir, ASSETS, "boot")
            print("Comparing with the reference run.")
            print_comparison(passed, report, current.qcow2_sha256, "Boot")
            result = "SUCCESS" if passed else "PARTIAL"
        print(
            "Run metadata: {}".format(
                display_path(final_dir / "run-metadata.json", ROOT)
            )
        )
        print(
            "Archived snapshot: {}".format(
                display_path(final_dir / "boot-snapshot.qcow2", ROOT)
            )
        )
        print("\n=== {}: {} ===".format(DEMO_LABEL, result))
        return result
    finally:
        stop_process(process)
        qmp_socket.unlink(missing_ok=True)
        shutil.rmtree(str(boot_assets), ignore_errors=True)


def main() -> int:
    # Before the first `hermit`; see settle_signal_33_disposition.
    settle_signal_33_disposition()
    result = boot_once()
    # On a fresh machine the first boot only records the reference run. Boot a
    # second time so a single invocation always performs a comparison. Set
    # QEMU_BOOT_REPEAT=0 to skip the second boot.
    if result == "FIRST RUN SAVED" and os.environ.get("QEMU_BOOT_REPEAT", "1") != "0":
        banner("Boot again and compare with the reference run just saved")
        result = boot_once()
    return 1 if result == "PARTIAL" else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as error:
        print("WARN: {}: FAILURE: {}".format(DEMO_LABEL, error), file=sys.stderr)
        sys.exit(1)
