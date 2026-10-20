# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Show how the guest kernel's task list changes across a fixed command.

run.sh passes this file to drgn, which reads the task list from the paused
guest's memory without running any code inside the guest.
"""

import os
import sys
from pathlib import Path

from drgn.helpers.linux.list import list_for_each_entry

DEMO_DIR = Path(__file__).resolve().parent
DEMOS_DIR = DEMO_DIR.parent
ROOT = DEMOS_DIR.parent
sys.path.insert(0, str(DEMOS_DIR / "lib"))

from demo_common import hermit_binary  # noqa: E402
from drgn_hermit import GuestConfig, program_from_hermit  # noqa: E402


ADVANCE_MARKER = b"__HERMIT_DEMO07_ADVANCE_DONE__"
# The sleep the command asks the guest for, in microseconds. It is a request,
# not a measured advance: the guest also runs the rest of the command, and it
# keeps running until this process sees the marker and pauses QEMU. Nothing
# here measures how far guest time moves between the two reads.
REQUESTED_SLEEP_US = 1000
DEFAULT_ADVANCE_COMMAND = (
    "for n in 1 2; do sleep 1000 & done; "
    + "usleep {}; ".format(REQUESTED_SLEEP_US)
    # Split the marker in the command text so terminal echo cannot satisfy the
    # wait; only the shell's post-timer output contains the complete marker.
    + 'echo __HERMIT_DEMO07_ADVANCE_"DONE__"'
)


def _required_path(variable: str) -> Path:
    value = os.environ.get(variable)
    if not value:
        raise RuntimeError(
            "{} is not set; run demos/07-drgn-kernel/run.sh".format(variable)
        )
    return Path(value).resolve()


def _optional_path(variable: str):
    value = os.environ.get(variable)
    return Path(value).resolve() if value else None


def _config() -> GuestConfig:
    return GuestConfig(
        root=ROOT,
        hermit=Path(hermit_binary()),
        qemu=_required_path("QEMU_BIN"),
        kernel=_required_path("DEMO07_KERNEL"),
        initrd=_required_path("DEMO07_INITRD"),
        vmlinux=_required_path("DEMO07_VMLINUX"),
        snapshot_disk=_required_path("DEMO07_SNAPSHOT_DISK"),
        snapshot_name=os.environ.get("DEMO07_SNAPSHOT_NAME", "hermit-boot"),
        advance_command=DEFAULT_ADVANCE_COMMAND,
        artifact_dir=_required_path("DEMO07_ARTIFACTS"),
        qemu_bios=_optional_path("DEMO07_QEMU_BIOS"),
        qemu_library_path=_optional_path("DEMO07_QEMU_LIBRARY_PATH"),
        timeout=float(os.environ.get("DEMO07_TIMEOUT", "240")),
    )


def _task_list(program):
    init_task = program["init_task"]
    rows = [
        (
            init_task.pid.value_(),
            init_task.comm.string_().decode("utf-8", "replace"),
        )
    ]
    rows.extend(
        (task.pid.value_(), task.comm.string_().decode("utf-8", "replace"))
        for task in list_for_each_entry(
            "struct task_struct", init_task.tasks.address_of_(), "tasks"
        )
    )
    return rows


def _canonical(rows):
    return "\n".join("{} {}".format(pid, comm) for pid, comm in rows)


def _task_diff(before, after):
    before_set = set(before)
    after_set = set(after)
    removed = sorted(before_set - after_set)
    added = sorted(after_set - before_set)
    return removed, added


def _run_once(config, command):
    with program_from_hermit(config) as guest:
        with guest.observation() as program:
            before = _task_list(program)
        before_metrics = guest.metrics[-1]

        guest.advance(command, ADVANCE_MARKER)

        with guest.observation() as program:
            after = _task_list(program)
        after_metrics = guest.metrics[-1]

    removed, added = _task_diff(before, after)
    if not removed and not added:
        raise RuntimeError("task list did not change during deterministic advance")
    return before, after, removed, added, before_metrics, after_metrics


def _print_tasks(label, rows, limit):
    shown = rows[:limit]
    print(
        "{} tasks ({} total; first {} shown, pid comm):".format(
            label, len(rows), len(shown)
        )
    )
    for pid, comm in shown:
        print("  {:5d} {}".format(pid, comm))
    if len(rows) > limit:
        print("  ... {} unchanged rows omitted from display".format(len(rows) - limit))


def _print_diff(removed, added):
    print("task-list diff (- before, + after):")
    for pid, comm in removed:
        print("  - {:5d} {}".format(pid, comm))
    for pid, comm in added:
        print("  + {:5d} {}".format(pid, comm))


def main() -> int:
    config = _config()
    runs = int(os.environ.get("DEMO07_RUNS", "2"))
    task_limit = int(os.environ.get("DEMO07_TASK_LIMIT", "16"))
    command = DEFAULT_ADVANCE_COMMAND
    if runs < 2:
        raise ValueError("DEMO07_RUNS must be at least 2 to prove reproducibility")
    if task_limit < 1:
        raise ValueError("DEMO07_TASK_LIMIT must be positive")

    baseline = None
    all_metrics = []
    for run in range(1, runs + 1):
        current = _run_once(config, command)
        before, after, removed, added, before_metrics, after_metrics = current
        canonical = (
            _canonical(before),
            _canonical(after),
            _canonical(removed),
            _canonical(added),
        )
        if baseline is None:
            baseline = current
            expected = canonical
        elif canonical != expected:
            raise RuntimeError(
                "before/after task snapshots or their diff changed between restores"
            )
        all_metrics.extend((before_metrics, after_metrics))
        print(
            "evolution {}: before_tasks={} after_tasks={} removed={} added={} "
            "read_states={}/{},{}/{} serial_delta={}/{}".format(
                run,
                len(before),
                len(after),
                len(removed),
                len(added),
                before_metrics.qemu_state,
                before_metrics.tracer_state,
                after_metrics.qemu_state,
                after_metrics.tracer_state,
                before_metrics.serial_bytes_delta,
                after_metrics.serial_bytes_delta,
            ),
            flush=True,
        )

    before, after, removed, added, _, _ = baseline
    _print_tasks("before", before, task_limit)
    _print_tasks("after", after, task_limit)
    _print_diff(removed, added)
    if any(item.serial_bytes_delta != 0 for item in all_metrics):
        raise RuntimeError("guest serial output advanced during a drgn read")
    print(
        "RESULT: restored demo 5 boot snapshot; requested_sleep_us={}; "
        "task_lists_differ=yes; evolution_reproducible=yes; "
        "read_virtual_time_advanced=no".format(REQUESTED_SLEEP_US)
    )
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as error:
        print("Demo 7 failed: {}".format(error), file=sys.stderr)
        sys.exit(1)
