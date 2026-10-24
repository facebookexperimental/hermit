# Demo 5: Boot Linux in QEMU and save a snapshot

Hermit can run an entire virtual machine, not only a single program. This demo
runs the QEMU emulator under Hermit. QEMU boots a Linux kernel with a small
BusyBox userland and, once the guest's shell is ready, saves the live machine
(memory, CPU and device state) as a snapshot inside a qcow2 disk image. Hermit
controls every clock reading and every thread switch that QEMU sees, so a
second boot produces the same serial console output and a byte-identical
snapshot file. Hermit's event log matches too when both boots are started the
same way, because one value QEMU reads, the set of signals it ignores, still
comes from whatever started `hermit`
(https://github.com/rrnewton/hermit/issues/3441; see "What to notice"). Demos 6
and 7 start from this snapshot.

## Prerequisites

- `hermit` on your `PATH`, built from this checkout (see
  [Setup](../README.md#setup)).
- `python3`, `qemu-system-x86_64` and `qemu-img`, a statically linked BusyBox,
  `cpio`, `gzip`, `file`, `sha256sum`, and `curl` for the one-time kernel
  download. To check for them without booting anything:

  ```bash
  demos/lib/qemu-assets.sh --check
  ```

  When something is missing, the check prints install commands such as:

  ```bash
  sudo apt install python3 qemu-system-x86 qemu-utils busybox-static cpio gzip curl file
  sudo dnf install python3 qemu-system-x86-core qemu-img busybox cpio gzip curl file
  ```

- User-space access to the CPU's hardware performance counters. Hermit uses
  them to advance virtual time and to switch between QEMU's threads at
  repeatable points.
- `ptrace` must be allowed (it is blocked by some container runtimes and by
  strict Yama settings).
- Disk space: each boot keeps its Hermit event log, about 460 MB
  (457,618,652 bytes in each of eight boots measured on 2026-09-30), next to
  its archived snapshot of about 90 MB. `demos/clean.sh` removes them.
- A checkout under `/tmp` works, but each boot leaves three empty directories
  on the host, `/tmp/hermit-demo5-run`, `/tmp/hermit-demo-controller`, and
  `/tmp/hermit-demo-assets` (see "How it works").

## Run it

From the repository root:

```bash
demos/05-qemu-boot/run.py
```

The same demo runs with `make -C demos demo5`.

The first run downloads a pinned Linux kernel from the Hermit GitHub releases,
checks its SHA-256, and builds a BusyBox initramfs; later runs reuse both. On a
machine that has never run the demo, one invocation boots twice: the first boot
is saved as the reference run and the second is compared with it. Set
`QEMU_BOOT_REPEAT=0` to boot only once; that invocation then compares nothing,
ends with `FIRST RUN SAVED`, and still exits 0. `demos/run-all.sh`, which
`make -C demos all` and the group targets use, sets `QEMU_BOOT_REPEAT=1`, so it
always boots a second time to compare.

How long a boot takes depends on host load. On a shared 316-CPU host
(2026-09-30, Hermit 0.2.0 gdc92644f96f4, QEMU 10.1.2), three first invocations,
which boot twice, took 136.8 to 147.3 seconds of wall time. With a saved
reference run (one boot), `make -C demos demo5` took 75.7 seconds, and demo 5
took 71 seconds inside a `make -C demos group3`. The first serial line appeared
after 12.4 to 13.5 seconds.

## What you will see

Captured on 2026-09-30: the second boot of a first invocation, with most of the
kernel messages and the tail of Hermit's log cut (marked `...`).

This capture and the 2026-09-30 measurements below predate the 2026-10-02
change to the guest's `/init`, which changed the initramfs (see demo 6's "How
it works"). The boot's output has the same form, but the snapshot and serial
hashes, kernel timestamps, scheduler turns, virtual times, and log size are
expected to differ from the figures here. The sample is to be refreshed from
the next verified run.

```text
================================================================================
=====                     Demo 5: QEMU Linux Snapshot                      =====

Hermit boots QEMU/Linux, streams the serial console, saves a live snapshot,
and compares every repeat run with the first run.
Dependency check passed: hermit 0.2.0 (...) (...)
QEMU dependency check passed: qemu-system-x86_64 qemu-img python3 static-busybox file cpio gzip sed sha256sum touch kernel-source

================================================================================

=== Verify QEMU kernel and initramfs ===
Kernel ready (12.7MB, cached)
Initramfs ready (0.8MB, cached)
QEMU assets ready.

=== Boot Linux to its serial shell (1st line takes a while to appear) ===
Waiting for first serial line: 12.6s  (12.6s to first output)
FIRST_OUTPUT_ELAPSED=12.6s
[    0.000000] Linux version 6.17.13-0_fbk0_crackerjackhost_0_g2b4321c50d79 ...
...
[    1.819107] Run /init as init process
==========================================
HERMIT-QEMU-BASELINE-BOOT-OK
kernel: 6.17.13-0_fbk0_crackerjackhost_0_g2b4321c50d79
rtc: 2022-01-01T00:00:07Z
==========================================
HERMIT-QEMU-COMMAND-DISK-READY

=== Snapshot ready ===
Snapshot disk: ignored/qemu-linux/.work/boot-.../hermit-snapshot.qcow2 (internal tag: hermit-boot)
Snapshot list:
ID      TAG               VM_SIZE                DATE        VM_CLOCK     ICOUNT
1       hermit-boot      89.4 MiB 2025-12-31 16:04:37  0000:00:07.398         --
Snapshot SHA-256: a5813ecc0c1f0f616da1802980a810cf4590834881dacd996eada1e87efe7624

=== Hermit INFO tail (wall-clock timestamps stripped) ===
...
  ------------------------------ hermit run report ------------------------------
Final thread-tree was: [3 [5 7 9 11 13 15 17 19]]
There were 2 group leaders of 9 thread(s) total.
Internally, the hermit scheduler ran 198345 turns, recorded 0 events (0 desynced)
Final virtual global (cpu) time: 1_767_225_883.435_825_230s
Elapsed virtual global (cpu) time: 283.435_825_230s
Timeslice stats: min=20940ns max=2000000000ns mean=1323465ns count=141483

=== Automatic repeat verification ===
Comparing with the reference run.
PASS: QEMU argv matches first run
PASS: QEMU version matches (QEMU emulator version 10.1.2 (...))
PASS: QEMU binary SHA-256 matches (...)
PASS: qcow2 SHA-256 matches (a5813ecc0c1f0f616da1802980a810cf4590834881dacd996eada1e87efe7624)
PASS: serial output SHA-256 matches (bf41d127c488b7fd8df9ba85c0af2c615a7973e3cc0a1e861054e5bfd5e8805f)
NOT COMPARED: guest output SHA-256: qemu-boot runs start no guest command
NOT COMPARED: guest command exit status: qemu-boot runs start no guest command
PASS: Hermit INFO log matches first run exactly apart from the wall-clock prefix (Hermit-marked host addresses compared by first appearance); compared ... lines, ... of which start a Hermit INFO record
PASS: all repeat checks match the first run

DETERMINISTIC: snapshot SHA-256 matches the previous run:
   a5813ecc0c1f0f616da1802980a810cf4590834881dacd996eada1e87efe7624
   Boot is bitwise-reproducible under Hermit.
Run metadata: ignored/qemu-linux/run-history/boot-...Z-boot-.../run-metadata.json
Archived snapshot: ignored/qemu-linux/run-history/boot-...Z-boot-.../boot-snapshot.qcow2

=== Demo 5: QEMU Linux Snapshot: SUCCESS ===
```

Elided (`...`): your Hermit version and path, the kernel's boot messages
between its first line and the start of `/init`, the tail of Hermit's log, the
QEMU package name and binary hash (they depend on your installation), and the
random per-run directory names. In a terminal the `Waiting for first serial
line` counter updates in place; the line shown is its final state, and the
wait depends on host load. The two `NOT COMPARED:` lines and the
`PASS: Hermit INFO log` line are shown in the current wording, with the two
counts elided; the capture printed an earlier comparator's wording, which had
neither. That comparator read the logs as text, and by it the capture's logs
differed in no line once the wall-clock prefix was removed (see below). The
current comparison reads them as bytes; its counts for a pair of boots on
2026-10-03 follow.

The numbers that are the same on every boot are the point of the demo. On
2026-09-30 a first invocation (two boots) ran from this checkout, from a copy
of its `demos` directory under `/var/tmp`, and from a copy under `/tmp`, and
one `make -C demos demo5` and one `make -C demos group3` ran in this checkout.
In all eight boots the snapshot SHA-256, the serial SHA-256, the scheduler's
198345 turns, the virtual times,
the timeslice statistics, and the whole Hermit log (457,618,652 bytes,
2,332,583 lines) were identical; the logs differed in no line once each line's
leading wall-clock timestamp was removed. On 2026-10-03 a first invocation of
the current demo (Hermit 0.4.0 gb82e83b510a6) saved two logs of 457,666,750
bytes, holding no carriage return and no line that is not UTF-8; its repeat
check compared 2,332,846 lines, 1,936,090 of which start a Hermit INFO record,
and found no difference. Comparing that pair again outside the demo took 10.1
seconds, against 6.3 seconds for the earlier comparator, with the host's load
average near 200. The snapshot's `DATE` column is
virtual time too (`--epoch 2026-01-01T00:00:00Z` plus the boot), printed by
`qemu-img` in the host's time zone, so it reads differently outside US Pacific
time. The snapshot hash also depends on the QEMU build and the kernel, and the
guest's command line carries the host paths of `python3` and
`qemu-system-x86_64`; all eight boots ran on one host, and no other host was
tried.

The first boot of a first invocation ends instead with:

```text
=== Automatic repeat verification ===
Saved this run as the reference run at ignored/qemu-linux/boot-anchor
Run metadata: ignored/qemu-linux/boot-anchor/run-metadata.json
Archived snapshot: ignored/qemu-linux/boot-anchor/boot-snapshot.qcow2

=== Demo 5: QEMU Linux Snapshot: FIRST RUN SAVED ===

=== Boot again and compare with the reference run just saved ===
```

## What to notice

- The guest prints `rtc: 2022-01-01T...`. QEMU starts the guest's real-time
  clock at a fixed date and advances it with the number of instructions the
  guest has executed, so the guest sees the same time on every boot.
- The repeat check compares five things with the reference run: QEMU's
  command line, the QEMU version and binary hash, the SHA-256 of the whole
  qcow2 file (which contains the saved memory and device state), the SHA-256 of
  the serial console transcript, and Hermit's own event log. The logs are
  compared with the two normalizations of Hermit's own canonical log
  comparison (`BitwiseInfoV1` in `detcore/src/logdiff.rs`) and no others: the
  wall-clock timestamp that starts a line is removed, and host addresses that
  Hermit marks as `<hostaddr ...>` are numbered in order of first appearance.
  Everything else, including virtual time and every guest address, must match
  exactly; both boots get the same environment, paths and epoch, and Hermit
  turns off address randomization in the guest, so the same boot prints the
  same addresses. The comparison is stricter than Hermit's: it compares every
  captured line, not only INFO records, and it compares bytes, so a carriage
  return or a byte that is not UTF-8 counts. Like Hermit's, it refuses a log
  that ends with the marker Hermit writes when a log reaches
  `HERMIT_LOG_MAX_BYTES`, and it requires at least one Hermit INFO record in
  each log, so `QEMU_LOG_FILTER` must keep them. The demo starts Hermit without
  `HERMIT_LOG` and `HERMIT_LOG_FILE`, which would change or move the log.
- A difference fails the run: the headline becomes `PARTIAL`, the demo exits
  with status 1, and `WARN:` lines name what differed, which log was refused,
  or which required value neither run recorded. A `NOT COMPARED:` line names a
  check that does not apply to these runs and says why. After you rebuild
  Hermit, change QEMU, change the kernel, change the guest's `/init` or
  initramfs (`demos/lib/qemu-assets.sh`), edit `demos/lib/demo_common.py`
  or `demos/lib/qemu_controller.py` (the guest runs copies of both), or run
  the demo with a different Python interpreter (the interpreter that runs
  `run.py` is also the guest's controller program, so a different one changes
  the snapshot, the console output, and the log), the saved reference no
  longer applies; run `demos/clean.sh` to start over. The repeat check compares
  QEMU's command line but not the interpreter, so a `PARTIAL` run after
  switching interpreters does not name the interpreter as the cause.
- One value in the Hermit log still depends on how `hermit` was started. QEMU
  reads `/proc/self/status`, and Hermit passes that file's `SigIgn` line, the
  set of signals the process ignores, through from the host unchanged:
  https://github.com/rrnewton/hermit/issues/3441. The demo now fixes the one
  signal that made boots differ on 2026-09-30, signal 33 (see "How it works").
  Other signals that a launcher leaves ignored still pass through to the guest.
  `nohup`, for example, ignores `SIGHUP`, and GNU `make` also leaves signal 32,
  glibc's other internal signal, ignored. glibc resets signal 32 only when a
  program cancels a thread, which the demo never does, so on a host where
  `python3` is a plain binary, `make -C demos demo5` and
  `demos/05-qemu-boot/run.py` from a shell can still start `hermit` with
  different values. So start the reference run and the
  repeats the same way (for example, always `make -C demos demo5`, or always
  `demos/05-qemu-boot/run.py` from a shell), and run `demos/clean.sh` when you
  switch. The comparison does not ignore this field. If a `PARTIAL` run's only
  `WARN:` is the Hermit log, check whether the differing lines are QEMU's reads
  of `/proc/self/status`.
- The snapshot is published as `ignored/qemu-linux/hermit-boot.qcow2` (or under
  `QEMU_ASSETS`). [Demo 6](../06-qemu-resume/README.md) and
  [demo 7](../07-drgn-kernel/README.md) restore it instead of booting again.
  Next to it, `hermit-boot.qcow2.producer.json` records the snapshot's SHA-256
  and the `INITRAMFS_VERSION` and SHA-256 of the initramfs this boot started
  from; demo 6 restores the snapshot only when that record matches the current
  initramfs. A snapshot saved at `QEMU_SNAPSHOT_DISK` gets the same record.
  The boot runs a copy of the kernel and initramfs made in its working
  directory, and the SHA-256 recorded is the copy's, so a checkout that
  rebuilds the shared initramfs during the boot cannot change what the record
  names. The version recorded is the one that `demos/lib/qemu-assets.sh`
  names for exactly those bytes in its build record, `.initramfs-build` next
  to the initramfs (`<INITRAMFS_VERSION> <SHA-256>`, written after each
  build); when that record names other bytes or another version, the demo
  stops before booting and asks you to run it again.

## How it works

QEMU runs with its software CPU emulator (no KVM), one virtual CPU, and
`-icount shift=0,sleep=off`, which drives the guest's clocks from the count of
executed instructions instead of from host time. Hermit runs QEMU together with
a small Python controller, [`lib/qemu_controller.py`](../lib/qemu_controller.py),
under `hermit run --strict`. The controller starts QEMU, watches the serial
transcript for the `HERMIT-QEMU-COMMAND-DISK-READY` line that the guest's
`/init` prints, and then asks QEMU over its control socket (QMP) to save a
snapshot named `hermit-boot`.

QEMU is multithreaded. Hermit runs its threads one at a time. It counts each
thread's retired branch instructions with the hardware performance counters,
advances virtual time by that count, and can preempt a thread at a repeatable
point, so the threads interact in the same order on every run. The full
command is `hermit run --strict --epoch 2026-01-01T00:00:00Z
--target-timeslice 100000 --max-timeslice 2000000000`, plus the options below:
`--target-timeslice` switches threads at system calls after 100 virtual
microseconds, and the large but finite `--max-timeslice` keeps branch-count
preemption armed for a thread that makes no system calls. Every time or random
value QEMU asks the host for comes from Hermit's virtual sources, and
`--epoch` fixes the date those clocks start from (without it, `hermit run`
starts them at the host's current time). The one field the demo adjusts before
hashing is the sub-second part of the snapshot's creation time in the qcow2
snapshot table, which is file metadata rather than guest state.

The demo also keeps host details out of what the guest sees, because each of
them once made two boots write different Hermit logs:

- The per-run working directory has a random name. It is bound into the guest
  at the fixed path `/tmp/hermit-demo5-run` with `--bind`, and the controller
  is given that path, so the random name never reaches a guest command line.
- The run's copies of the kernel and initramfs are bound at
  `/tmp/hermit-demo-assets`, the path at which the asset directory itself was
  bound before (a `QEMU_SNAPSHOT_DISK` in the asset directory is bound at its
  old path inside it), and the controller runs from
  `/tmp/hermit-demo-controller`, a copy of its two
  source files made for each run with fixed file modes and modification times.
  Their host paths depend on where the checkout is, and they appear on the
  controller's and QEMU's command lines, whose lengths change how many branches
  those programs execute. When they were passed as host paths, a fresh clone in
  another directory booted to a different snapshot, with 198360 scheduler turns
  instead of 198340 and a longer Hermit log. Importing the controller straight
  from `demos/lib` also made the guest read the bytecode cache that the host's
  Python leaves there, and a cached `.pyc` records its source file's absolute
  path: a copy of the checkout at a path 6 characters shorter read a
  `demo_common` cache 6 bytes shorter and ended at a different virtual time.
- Hermit creates the three empty mount points on the host; under a `/tmp`
  checkout they stay behind after the run.
- `--base-env=minimal` gives the guest a fixed environment (`PATH`,
  `HOSTNAME` and `HOME`) plus `PYTHONDONTWRITEBYTECODE`, instead of your shell's
  environment.
- The guest's standard input is `/dev/null` and its standard output and error
  are a pipe. The demo copies the pipe into the Hermit log on a separate
  thread. When the guest wrote straight to the log file, the controller's
  start-up `fstat` of its output saw the log's current size, which depends on
  host timing.
- Before its first boot, the demo starts one thread and waits for it to
  finish, so that every `hermit` it starts sees signal 33 in the same state.
  Signal 33 is one of glibc's two internal signals, and glibc does not let a
  program change it, so a program started with it ignored keeps it ignored. GNU
  `make` starts its commands that way, and so does the `python3` launcher at
  `/usr/local/bin/python3` on the host we measured. The exception is glibc
  itself: when a program creates its first thread, glibc installs its own
  handler for signal 33, and programs started after that see the default
  setting. The demo used to create its first thread, the one that copies the
  guest's output into the log, just after its first boot, so the second boot of
  an invocation could see signal 33 differently from the first. On 2026-09-30,
  in three invocations from a fresh clone run with that `python3` launcher,
  the only failed comparison was that second boot: QEMU's two 1,024-byte reads of
  `/proc/self/status` differed in the bit for signal 33, while the snapshot and
  the console transcript matched the reference. The two later invocations
  booted once each, saw the same setting as the reference, and matched.

The guest also gets a small second disk, `/dev/vda`, holding the placeholder
`WAIT`. The guest's `/init` polls that disk for a command to run. It is attached
before the snapshot is saved, so it is part of the saved machine, and demo 6
replaces its contents after restoring the snapshot.

Controls (environment variables):

| Variable | Default | Meaning |
| --- | --- | --- |
| `QEMU_BOOT_REPEAT` | `1` | Set to `0` to skip the second boot of a first invocation, which then compares nothing. `demos/run-all.sh` always sets it to `1`. |
| `QEMU_TIMEOUT` | `600` | Seconds the demo waits for the boot: it then signals Hermit's process group, the one it started the `hermit` command in (SIGTERM and then SIGKILL, at most 10 seconds apart), and fails. It does not report whether the group emptied, and a process outside the group is not signalled. |
| `QEMU_ASSETS` | `ignored/qemu-linux` | Where the kernel, initramfs, snapshot, and run history are kept. A checkout under `/tmp` uses a directory under `/var/tmp` instead. |
| `QEMU_BIN` | `qemu-system-x86_64` on `PATH` | The QEMU binary. |
| `KERNEL_IMAGE` | (download) | Use a local copy of the pinned kernel; its SHA-256 must still match. |
| `BUSYBOX` | `busybox` on `PATH` | A statically linked BusyBox for the initramfs. |
| `QEMU_SNAPSHOT_NAME` | `hermit-boot` | The snapshot's name inside the qcow2 file. |
| `QEMU_LOG_FILTER` | `warn,detcore=info,reverie_ptrace::task=info` | Hermit's log filter, passed to Hermit as `RUST_LOG`; the demo removes `HERMIT_LOG` and `HERMIT_LOG_FILE` from Hermit's environment. The repeat check needs Hermit's INFO records, so a filter that keeps none fails it. A different filter also changes the log, so a saved reference run no longer applies; run `demos/clean.sh`. |
| `QEMU_MAX_LOG_BYTES` | 768 MiB | Signal Hermit's process group and fail if Hermit's event log grows past this size. The cap still holds while the demo waits, for up to 60 seconds after Hermit exits, for the rest of Hermit's output; when the cap or those 60 seconds run out, the demo signals Hermit's process group the same way, which reaches the processes Hermit left running in that group. Eight healthy boots on 2026-09-30 wrote 457,618,652 bytes each (Hermit 0.2.0 gdc92644f96f4, QEMU 10.1.2); Hermit 0.2.0 g770b95c505fa wrote about 253 MB. |
