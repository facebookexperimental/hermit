# Demo 7: Watch the guest kernel's task list with drgn

A deterministic virtual machine can be inspected like a debugger target without
disturbing it. This demo restores the snapshot from
[demo 5](../05-qemu-boot/README.md), pauses the machine, and uses the
[drgn](https://github.com/osandov/drgn) kernel debugger to read the guest
kernel's list of tasks straight out of QEMU's memory. It then lets the guest run
a fixed command that starts two processes and asks for a 1 millisecond sleep,
pauses again, and reads the task list a second time. It does all of this
twice from fresh copies of the snapshot and checks that both passes see exactly
the same task lists and the same difference between them.

## Prerequisites

- Everything [demo 5](../05-qemu-boot/README.md#prerequisites) needs. If the
  boot snapshot is missing, this demo runs demo 5 first.
- `drgn` on your `PATH` (see the [drgn documentation](https://drgn.readthedocs.io/)
  for installation).
- `bpftool`, `gcc`, and `readelf`, used to turn the kernel's built-in type
  information (BTF) into a form drgn can load.

## Run it

From the repository root:

```bash
demos/07-drgn-kernel/run.sh
```

The same demo runs with `make -C demos demo7`. `demos/07-drgn-kernel/run.sh
--help` lists the settings.

## What you will see

Captured on 2026-09-30 from `demos/07-drgn-kernel/run.sh` (`make -C demos
demo7` prints the same, after one extra first line, `QEMU dependency check
passed: ...`):

```text
=== Demo 7: snapshot -> read-only kernel read -> deterministic advance -> read ===
Using hermit 0.2.0 (...) (...)
evolution 1: before_tasks=84 after_tasks=85 removed=2 added=3 read_states=t/T,t/T serial_delta=0/0
evolution 2: before_tasks=84 after_tasks=85 removed=2 added=3 read_states=t/T,t/T serial_delta=0/0
before tasks (84 total; first 16 shown, pid comm):
      0 swapper/0
      1 init
      2 kthreadd
      3 pool_workqueue_
      4 kworker/R-rcu_g
      5 kworker/R-sync_
      6 kworker/R-kvfre
      7 kworker/R-slub_
      8 kworker/R-netns
      9 kworker/0:0
     10 kworker/0:0H
     11 kworker/0:1
     12 kworker/u4:0
     13 kworker/R-mm_pe
     14 kworker/u4:1
     15 ksoftirqd/0
  ... 68 unchanged rows omitted from display
after tasks (85 total; first 16 shown, pid comm):
      0 swapper/0
      1 sh
      2 kthreadd
      3 pool_workqueue_
      4 kworker/R-rcu_g
      5 kworker/R-sync_
      6 kworker/R-kvfre
      7 kworker/R-slub_
      8 kworker/R-netns
      9 kworker/0:0
     10 kworker/0:0H
     11 kworker/0:1
     12 kworker/u4:0
     13 kworker/R-mm_pe
     14 kworker/u4:1
     15 ksoftirqd/0
  ... 69 unchanged rows omitted from display
task-list diff (- before, + after):
  -     1 init
  -    95 sleep
  +     1 sh
  +   101 sleep
  +   102 sleep
RESULT: restored demo 5 boot snapshot; requested_sleep_us=1000; task_lists_differ=yes; evolution_reproducible=yes; read_virtual_time_advanced=no

=== Demo 7: drgn Kernel Task Evolution: SUCCESS ===
```

Edited by hand: the `RESULT` field `requested_sleep_us` was named
`fixed_virtual_advance_us` when this output was captured and was renamed on
2026-10-02; its value 1000 and the rest of the block are as printed then, and
the block is to be refreshed from the next verified run of this demo.

Elided (`...`): your Hermit version and the path of the `hermit` on your
`PATH`. Everything from `evolution 1` to `RESULT` was byte-identical in ten
invocations (twenty passes) on 2026-09-30, from both `run.sh` and `make`, with
QEMU 10.1.2 and the pinned 6.17.13 kernel. A different QEMU, kernel, or boot
snapshot can change the counts and process IDs; the two passes of one
invocation must still agree. The whole demo took 14 to 18 seconds of wall time.

These are the same counts and the same difference that the walkthrough for an
earlier version of this demo reported (`demos/WALKTHROUGH.md` in
<https://github.com/rrnewton/hermit/pull/2904>).

The difference rows read as follows. Before the advance, process 1 is the
guest's start-up script (`/init`, named `init`) waiting for a command, and
process 95 is the `sleep 1` of its polling loop. During the advance `/init`
reads the command, runs it (the command's two `sleep 1000 &` become processes
101 and 102, and pass to process 1 when the command's shell exits), and then
replaces itself with an interactive shell (`exec setsid cttyhack sh`, the last
line of `/init`), so process 1 is now named `sh`. The polling `sleep 1` has
exited.

## What to notice

- `read_states=t/T,t/T`: for each of the two reads, QEMU was in the `t`
  (stopped under `ptrace`) state and Hermit's tracer was in the `T` (stopped)
  state. With both stopped, reading the task list cannot execute a single guest
  instruction. `serial_delta=0/0` counts, for each read, the bytes that reached
  the host end of the guest's serial pipe while the read ran: the demo takes
  whatever the pipe already holds just before the read, takes it again just
  after, and fails if anything arrived in between.
- The task list changes between the two reads (`task_lists_differ=yes`): the
  command started two `sleep` processes. The second pass, from a fresh copy of
  the snapshot, produced the same before list, after list, and difference
  (`evolution_reproducible=yes`).
- Only the first 16 rows of each list are printed; the comparison uses every
  row.

## How it works

The demo restores the snapshot with QEMU's virtual CPU paused, running QEMU
under `hermit run --strict --no-rcb-time --target-timeslice 100000
--max-timeslice disabled` (set in the shared
[`lib/drgn_hermit.py`](../lib/drgn_hermit.py)). Unlike demos 5 and 6 it passes
no `--epoch`, so Hermit's virtual clocks start at the host time of each run;
the guest's own clock comes from QEMU (`-rtc base=2022-01-01T00:00:00,clock=vm`),
and the demo compares the task lists rather than Hermit's log. Runs started at
different host times produced the same result. Before
QEMU starts, the fixed command
(`for n in 1 2; do sleep 1000 & done; usleep 1000; echo ...`) is written to the
guest's command disk, the same mechanism as
[demo 6](../06-qemu-resume/README.md).

drgn does not attach to QEMU as a debugger. The demo waits until QEMU is in a
`ptrace` stop, sends `SIGSTOP` to Hermit's tracer, opens `/proc/<qemu pid>/mem`
read-only, and gives drgn the guest's RAM as physical memory. Before reading
kernel structures it checks that the kernel's build ID, which the guest kernel
publishes in memory (VMCOREINFO), matches the `vmlinux` that drgn uses for type
information. The `vmlinux` is extracted from the pinned kernel image on first
use.

To advance, the demo resumes Hermit's tracer, tells QEMU to continue over its
control socket, and waits for the command's completion marker on the serial
port. Since 2026-10-02 the guest's `/init` saves the command's output in a
file and prints it, with `| ` in front of each line, only after the command's
shell has exited (see [demo 6's "How it works"](../06-qemu-resume/README.md#how-it-works)),
so the marker arrives as `| __HERMIT_DEMO07_ADVANCE_DONE__`; the demo looks for
the marker anywhere in the transcript. The sample above was captured before
that change and is to be refreshed from the next verified run. The demo then
pauses QEMU and stops the tracer again for the second read. Guest time advances
only between the two reads. The command asks for a 1 millisecond
sleep (`usleep 1000`, printed as `requested_sleep_us=1000`), but that is a
request, not a measurement: the guest also runs the rest of the command, and it
keeps running until the demo sees the marker and pauses QEMU. The advance is
the sleep plus everything else that runs before QEMU pauses, and the demo does
not measure it.

Controls (environment variables):

| Variable | Default | Meaning |
| --- | --- | --- |
| `DEMO07_RUNS` | `2` | Number of independent passes; at least 2. |
| `DEMO07_TASK_LIMIT` | `16` | Number of task rows printed (all rows are compared). |
| `DEMO07_TIMEOUT` | `240` | Seconds allowed for the restore and for the advance. |
| `DEMO07_SNAPSHOT_DISK` | `$QEMU_ASSETS/hermit-boot.qcow2` | The demo 5 snapshot to restore. |
| `DEMO07_SNAPSHOT_NAME` | `hermit-boot` | The snapshot's name inside the qcow2 file. |
| `DEMO07_VMLINUX` | extracted from `bzImage` | A kernel ELF image with type information matching the guest kernel. |
| `DEMO07_QEMU_BIOS`, `DEMO07_QEMU_LIBRARY_PATH` | unset | Firmware directory and library path for a QEMU installed in a non-standard location. |
| `DEMO07_ARTIFACTS` | `target/demos/07-drgn-kernel` | Directory that holds one run directory per pass. It may be under the host's `/tmp`; Hermit then gives QEMU the host's `/tmp` (`hermit run --tmp=/tmp`) instead of a private one, so that QEMU can open the run directory. |
| `QEMU_BIN`, `QEMU_ASSETS` | as in demo 5 | The QEMU binary and asset directory. |

Each pass keeps its working copy of the snapshot, serial transcript, and Hermit
log under `target/demos/07-drgn-kernel/`, or under `DEMO07_ARTIFACTS` when that
is set. `demos/clean.sh` removes the default directory. It does not remove a
`DEMO07_ARTIFACTS` directory, which can be any path; it prints that directory's
name instead, so that you can delete it yourself.

If a pass fails, the demo prints the last 40 lines of that pass's Hermit log,
which holds Hermit's and QEMU's own error messages, and removes the pass's
snapshot copy (94 MB for demo 5's snapshot). The log and the rest of the run
directory stay.
