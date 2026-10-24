# Demo 6: Resume the Linux snapshot and run a command

Booting Linux under Hermit took 75.7 seconds for one `make -C demos demo5` on
a shared 316-CPU host (2026-09-30, Hermit 0.2.0 gdc92644f96f4, QEMU 10.1.2; see
demo 5). On the same host, restoring the saved machine and running one command
took 15.7 to 16.5 seconds of Hermit/QEMU time over eleven resumes, or 10.0 to
11.4 seconds over three resumes that did not save a post-command snapshot. One
`make -C demos demo6` took 25.3 seconds (demo 6 took 26 seconds inside a
`make -C demos group3`), and four first invocations of a command, which resume
twice, took 46.0 to 46.4 seconds. This demo restores the snapshot that
[demo 5](../05-qemu-boot/README.md) saved, has the already-running guest shell
run one command, prints the command's output, and saves the machine again after
the command. Because the resumed machine runs under Hermit, running the same
command again produces identical guest output and a byte-identical
post-command snapshot.

## Prerequisites

- Everything [demo 5](../05-qemu-boot/README.md#prerequisites) needs.
- The boot snapshot from demo 5, booted from the initramfs that
  `demos/lib/qemu-assets.sh` builds now. If it is missing, or demo 5's record
  of it does not show that, this demo runs demo 5 first to rebuild it. If the
  snapshot was missing and demo 5 exits non-zero, this demo stops with demo 5's
  failure, even if demo 5 saved a snapshot first. If a snapshot was there but
  its record did not show that, demo 5 may end `PARTIAL` and exit non-zero
  after it has saved a current snapshot, because after an initramfs change its
  own reference run no longer applies either. In that case only, this demo
  checks demo 5's record instead of its exit status: it uses the rebuilt
  snapshot, with a note, when the record matches, and otherwise stops and asks
  you to run `demos/clean.sh` and then this demo again.

The resume itself does not use the hardware performance counters (see "How it
works"), but demo 5 does if it has to run first.

## Run it

From the repository root:

```bash
demos/06-qemu-resume/run.py
```

With no argument the guest runs `uname -a`. Pass any single-line shell command
of up to 511 bytes to run it inside the guest instead:

```bash
demos/06-qemu-resume/run.py 'ls /'
demos/06-qemu-resume/run.py 'cat /proc/meminfo | head -n 3'
```

The guest reads only the first 512 bytes of the disk that carries its command,
and the newline after the command has to fit in them too, so `run.py` refuses a
longer command (counted in UTF-8 bytes) before it resumes anything.

`make -C demos demo6` runs the default command, and
`make -C demos demo6 DEMO6_COMMAND='ls /'` runs another simple one. Quote a
command that contains shell syntax such as `|` and pass it to `run.py`
directly, because Make would otherwise hand the `|` to your own shell.

Each distinct command has its own reference run. On the first run of a command,
one invocation resumes twice: the first resume is saved as that command's
reference and the second is compared with it. Set `QEMU_RESUME_REPEAT=0` to
resume only once; that invocation then compares nothing, ends with
`FIRST RUN SAVED`, and still exits 0. `demos/run-all.sh`, which
`make -C demos all` and the group targets use, sets `QEMU_RESUME_REPEAT=1`, so
it always resumes a second time to compare.

## What you will see

Captured on 2026-09-30: the second resume of a first `uname -a` invocation,
with the tail of Hermit's log cut (marked `...`).

This capture and the 2026-09-30 measurements below predate the 2026-10-02
change to the guest's `/init` that frames the command's output (see "How it
works"). A run of the current demo also prints `Guest command exit status: 0`
after the guest output and `PASS: guest command exit status matches (0)` in
the repeat check, and its post-command snapshot hash, scheduler turns, virtual
times, and log size are expected to differ from the figures here. The sample
is to be refreshed from the next verified run.

```text
================================================================================
=====                     Demo 6: QEMU Snapshot Resume                     =====

QEMU restores the live shell, runs one command, saves a post-command
snapshot by default, and compares repeats keyed by the command string.
Dependency check passed: hermit 0.2.0 (...) (...)

================================================================================

=== Resume hermit-boot and run: uname -a ===
Restoring snapshot (timeout: 120s)...
Hermit/QEMU resume: done (16.3s)
TIMER_DONE label=Hermit/QEMU resume elapsed=16.3s

=== Guest serial output ===
Linux (none) 6.17.13-0_fbk0_crackerjackhost_0_g2b4321c50d79 #1 SMP Thu Dec 18 12:27:13 PST 2025 x86_64 x86_64 x86_64 GNU/Linux

=== Hermit INFO tail (wall-clock timestamps stripped) ===
...
  ------------------------------ hermit run report ------------------------------
Final thread-tree was: [3 [5 7 9 11 13 15 17 19]]
There were 2 group leaders of 9 thread(s) total.
Internally, the hermit scheduler ran 288022 turns, recorded 0 events (0 desynced)
Final virtual global (cpu) time: 1_767_226_582.454_550_000s
Elapsed virtual global (cpu) time: 982.454_550_000s
Timeslice stats: min=0ns max=125000000ns mean=4006895ns count=211951

=== Automatic repeat verification ===
PASS: QEMU argv matches first run
PASS: QEMU version matches (QEMU emulator version 10.1.2 (...))
PASS: QEMU binary SHA-256 matches (...)
PASS: qcow2 SHA-256 matches (9f2cc4675b3e641386c358d8ef8234e100ec829ad4de63577fb3e98cf8a79c66)
NOT COMPARED: serial output SHA-256: qemu-resume rows do not record it; the guest command's output, taken from the serial log, is compared instead
PASS: guest output SHA-256 matches (42a2dfb7b01dab06b2fc0b36483d3f75aaa7d33d239a636b3ae2daf9b0f81f06)
PASS: Hermit INFO log matches first run exactly apart from the wall-clock prefix (Hermit-marked host addresses compared by first appearance); compared ... lines, ... of which start a Hermit INFO record
PASS: all repeat checks match the first run

DETERMINISTIC: snapshot SHA-256 matches the previous run:
   9f2cc4675b3e641386c358d8ef8234e100ec829ad4de63577fb3e98cf8a79c66
   Resume is bitwise-reproducible under Hermit.
Post-command snapshot: ignored/qemu-linux/resume-metadata/28ba533b0f3c4df63d6b4a5ead73860697bdf735bb353e4ca928474889eb8a15/run-history/resume-...Z-.../post-command.qcow2
Post-command SHA-256: 9f2cc4675b3e641386c358d8ef8234e100ec829ad4de63577fb3e98cf8a79c66
Run metadata: ignored/qemu-linux/resume-metadata/28ba533b0f3c4df63d6b4a5ead73860697bdf735bb353e4ca928474889eb8a15/run-history/resume-...Z-.../run-metadata.json

=== Demo 6: QEMU Snapshot Resume: SUCCESS ===
```

Elided (`...`): your Hermit version and path, the tail of Hermit's log, the
QEMU package name and binary hash, and the per-run directory names (a
timestamp and a process ID). The wall-clock times (`16.3s`) depend on host
load; in a terminal the `Hermit/QEMU resume` counter updates in place until it
reads `done`. The `NOT COMPARED:` line and the `PASS: Hermit INFO log` line
are shown in the current wording, with the two counts elided; the capture
printed an earlier comparator's wording, which had neither. That comparator
read the logs as text, and by it the capture's logs differed in no line once
the wall-clock prefix was removed (see below). The current comparison reads
them as bytes; its counts for a pair of resumes on 2026-10-03 follow. The
directory named `28ba533b...`
is the SHA-256 of the command string `uname -a`. Your hashes and virtual times
will match these only with the same QEMU build, kernel and demo 5 snapshot. As
in demo 5, a saved reference run no longer applies after you rebuild Hermit,
change QEMU or the kernel, replace demo 5's snapshot, change the guest's
`/init` or initramfs (`demos/lib/qemu-assets.sh`; demo 5 must then save a new
boot snapshot), edit `demos/lib/demo_common.py` or
`demos/lib/qemu_controller.py`, or run the demo with a different Python
interpreter: the interpreter that runs `run.py` is also the guest's controller
program. Run `demos/clean.sh` to start over.

On 2026-09-30 six `uname -a` resumes ran: two in a first invocation in this
checkout, one `make -C demos demo6`, one `make -C demos group3`, and two in a
first invocation from a copy of the `demos` directory under `/var/tmp`. In all
six the post-command snapshot, the guest output, the scheduler's 288022 turns,
the virtual times, and the whole Hermit log (258,506,138 bytes, 1,604,193
lines) were identical;
the logs differed in no line once each line's leading wall-clock timestamp was
removed. The other two commands behaved the same way: `ls /` (three resumes)
gave post-command snapshot `146f380f...` and guest output `81acdc95...`, and
`cat /proc/meminfo | head -n 3` (two resumes) gave `642d32db...` and
`a78a631c...`.

On 2026-10-03 the two `uname -a` resumes of a first invocation of the current
demo (Hermit 0.4.0 gb82e83b510a6) each ran 287703 scheduler turns and saved,
exactly as captured, a log of 258,316,724 bytes holding no carriage return and
no line that is not UTF-8. The repeat check compared 1,602,765 lines, 1,027,339
of which start a Hermit INFO record, and found no difference. Comparing that
pair again outside the demo took 6.4 seconds, against 3.9 seconds for the
earlier comparator, with the host's load average near 200.

The first resume of a new command ends instead with:

```text
=== Automatic repeat verification ===
Saved this run as the reference run for this command at ignored/qemu-linux/resume-metadata/28ba533b0f3c4df63d6b4a5ead73860697bdf735bb353e4ca928474889eb8a15/run-metadata.json
Post-command snapshot: ignored/qemu-linux/resume-metadata/28ba533b0f3c4df63d6b4a5ead73860697bdf735bb353e4ca928474889eb8a15/run-history/resume-...Z-.../post-command.qcow2
Post-command SHA-256: 9f2cc4675b3e641386c358d8ef8234e100ec829ad4de63577fb3e98cf8a79c66
Run metadata: ignored/qemu-linux/resume-metadata/28ba533b0f3c4df63d6b4a5ead73860697bdf735bb353e4ca928474889eb8a15/run-history/resume-...Z-.../run-metadata.json

=== Demo 6: QEMU Snapshot Resume: FIRST RUN SAVED ===

=== Resume again and compare with the reference run just saved ===
```

## What to notice

- The guest did not boot. The kernel messages from demo 5 do not appear; the
  guest continues from the exact moment the snapshot was taken and runs the
  command straight away.
- Where demo 5's repeat check has a `serial output SHA-256` line, this one has
  `guest output SHA-256`: the output of your command, taken from the serial
  transcript between the guest's BEGIN and END lines with the `| ` in front of
  each line removed (see "How it works"). The demo does not hash the rest of
  the transcript; we compared each run's archived `serial.log` by hand, and it
  was byte-identical across the repeats of each command above.
- `Guest command exit status:` is the command's own exit status, read from the
  guest's END line. It is saved in `run-metadata.json` as `guest_exit_status`
  and compared with the reference run (`PASS: guest command exit status
  matches`). A nonzero status does not fail the demo, because it is the
  command's result; a status that differs from the reference run does.
- The reference run is stored per command (under a directory named after the
  command's SHA-256), so `uname -a` and `ls /` are checked independently.
- The Hermit log is compared as in demo 5 (see its "What to notice"): every
  captured line, as bytes, with only the wall-clock prefix removed and the
  host addresses Hermit marks numbered by first appearance. A log that ends
  with Hermit's truncation marker, or that holds no Hermit INFO record, fails
  the check, so `QEMU_LOG_FILTER` must keep Hermit's INFO records; the demo
  starts Hermit without `HERMIT_LOG` and `HERMIT_LOG_FILE`. A `NOT COMPARED:`
  line names a check that does not apply to these runs and says why.
- As in demo 5, one value in the Hermit log still depends on how `hermit` was
  started. QEMU reads `/proc/self/status`, and Hermit passes that file's
  `SigIgn` line, the set of signals the process ignores, through from the host
  unchanged: https://github.com/rrnewton/hermit/issues/3441. On 2026-09-30, in
  two invocations from a fresh clone, the only failed comparison was the second
  resume of the first invocation: QEMU's two reads of `/proc/self/status`
  differed in the bit for signal 33, while the post-command snapshot and the
  guest output matched. The second resume came after the demo's first thread,
  and glibc changes signal 33 when a program creates its first thread;
  [demo 5's "How it works"](../05-qemu-boot/README.md#how-it-works) explains
  this. The demo now starts and waits for one thread before its first resume,
  so every resume sees signal 33 in the same state. Other signals that a
  launcher leaves ignored still pass through to the guest. `nohup`, for
  example, ignores `SIGHUP`, and GNU `make` also leaves signal 32, glibc's other
  internal signal, ignored. glibc resets signal 32 only when a program cancels
  a thread, which the demo never does, so on a host where `python3` is a plain
  binary, `make -C demos demo6` and `demos/06-qemu-resume/run.py` from a shell
  can still start `hermit` with different values. So start the reference run
  and the repeats the
  same way (for example, always `make -C demos demo6`, or always
  `demos/06-qemu-resume/run.py` from a shell), and run `demos/clean.sh` when you
  switch. The comparison does not ignore this field. If a `PARTIAL` run's only
  `WARN:` is the Hermit log, check whether the differing lines are QEMU's reads
  of `/proc/self/status`.
- `--no-save-snapshot` skips saving the post-command snapshot; the guest output
  and the Hermit log are still compared, the repeat check prints
  `NOT COMPARED: qcow2 SHA-256: neither run saved a snapshot`, and the
  `DETERMINISTIC:` lines do not appear. Such a run keeps its own reference, in
  the command's directory name with `-no-save-snapshot` appended, because it
  executes differently (194491 scheduler turns for `uname -a` instead of
  288022, a 169,751,813-byte Hermit log, identical in three resumes). For
  example:

  ```bash
  demos/06-qemu-resume/run.py --no-save-snapshot 'uname -a'
  ```

## How it works

The demo copies the boot snapshot and starts QEMU with the same machine
configuration as demo 5, plus an option to load the `hermit-boot` snapshot at
start-up. The command travels to the guest on a small raw disk, `/dev/vda`,
that demo 5 attached (holding `WAIT`) before it saved the snapshot. Before QEMU
starts, the demo writes the command into that disk image. When the guest
resumes, its `/init` loop reads the disk's first 512 bytes, finds a command
instead of `WAIT`, and runs it. The command is on disk before the guest runs,
so nothing depends on when the host sends it.

`/init` frames the command's output so that nothing the command prints can be
taken for the frame. It prints `__HERMIT_COMMAND_BEGIN__ format=3`, then runs
the command as user and group 1000 (BusyBox `chpst -u 1000:1000`), with its
standard input from `/dev/null` and its standard output and standard error
going to a file. It prints that file with `| ` in front of every line and every
byte 0x01 (ASCII SOH) removed, and then prints `__HERMIT_COMMAND_END__`, one
0x01 byte, and `status=N`, where `N` is the command's exit status. The
controller ([`lib/qemu_controller.py`](../lib/qemu_controller.py)) reads the
transcript line by line and accepts only these exact whole lines as the frame.
A command that prints the end marker itself, for example
`echo __HERMIT_COMMAND_END__; echo FINISHED; exit 3`, shows it as the output
line `| __HERMIT_COMMAND_END__`, followed by `| FINISHED`, and the demo
reports exit status 3 from the real END line after them. A command that never
exits, for example `echo __HERMIT_COMMAND_END__; sleep 1000000`, shows
nothing: `/init` prints the output only after the command exits, so neither
its output nor an END line reaches the serial port, and the run fails when it
reaches a bound (see "A run that does not finish" below).

The command cannot print a frame line by another route either:

- It cannot write to the serial port. `/dev/console` and `/dev/ttyS0` belong to
  root with mode 0600, and the command is not root. Its standard input is
  `/dev/null`, so it holds no console file descriptor: before this change the
  command inherited `/init`'s standard input, the console opened for reading
  and writing, and `echo ... >&0` put a forged END line straight on the serial
  port.
- It cannot make the kernel print one. `/dev/kmsg` has mode 0644, so user 1000
  cannot write to it, and the kernel command line holds `printk.time=1`, so
  every line the kernel prints starts with a `[` timestamp.
- It cannot change what `/init` prints. No output line holds a 0x01 byte, so
  neither an output line nor the tail of one that a kernel message split in
  two is an END line. User 1000 cannot signal or trace `/init`, and because
  `/tmp` is sticky (mode 1777) it cannot remove or rename the output file,
  which `/init` creates as root; writing into that file only changes its own
  output.

This rests on the guest kernel. A kernel bug or privilege escalation that
gives user 1000 root, or a kernel or initramfs other than demo 5's with
different device modes, would reopen these routes; the demo does not defend
against the guest kernel. When the real END line arrives, the controller asks QEMU to save a
snapshot named `command-<first 16 hex digits of the command's SHA-256>`. The
demo removes the `| ` prefixes to get the guest output. Any other line inside
the frame, such as a kernel message, is kept in the guest output with
`[console] ` in front of it, so it is shown and compared rather than dropped.

The framing has these consequences and limits:

- The output appears after the command has exited, not while it runs.
- The command's standard output is a file, not the console. Programs that
  format for a terminal print differently (BusyBox `ls`, for example, prints
  one name per line instead of columns), and C programs buffer their standard
  output, so its lines can come out in a different order relative to standard
  error than they would at a terminal.
- The command runs as user and group 1000, not root, and reads `/dev/null` as
  its standard input. A command that needs root, such as `mount`, reading
  `/dev/vda`, or writing under `/proc/sys`, fails with a permission error. It
  can still create files in `/tmp`.
- The file is `/tmp/.hermit-command-output` in the guest's memory. A command
  that reads or writes it interferes with its own output.
- A last line without a newline is printed with one, and NUL bytes and 0x01
  bytes are not preserved. Output that a background job writes after the
  command has exited is not shown.
- A kernel message printed in the middle of the END line hides that line, so
  the controller keeps waiting until the run reaches a bound and fails (see
  "A run that does not finish" below); it never ends with a cut-off
  output.
- A boot snapshot saved before this change still runs an old `/init`, which
  runs the command as root with the console as its standard input, so the
  command could print a frame of its own. `demos/lib/qemu-assets.sh` rebuilds
  the initramfs by itself (its `.initramfs-version` is now 9), but only demo 5
  saves a new boot snapshot. Demo 5 therefore writes a record next to each boot
  snapshot it saves, `hermit-boot.qcow2.producer.json`, naming the snapshot's
  SHA-256 and the `INITRAMFS_VERSION` and SHA-256 of the initramfs it booted.
  This demo restores the default snapshot only when that record matches the
  snapshot and the current initramfs, and otherwise runs demo 5 again to
  rebuild it; if demo 5 then exits non-zero, the rebuilt snapshot is used only
  when its record matches. That allowance is only for a snapshot that was
  there and did not match: when the snapshot is missing and demo 5 exits
  non-zero, the demo stops with demo 5's failure (see Prerequisites). A
  snapshot named by `QEMU_BOOT_SNAPSHOT_DISK` without such a record is refused,
  with the reason and how to rebuild it. Because demo 5 does not take this demo's lock, the
  copy that QEMU restores is checked against the record too. As a second line
  of defence, the controller stops as soon as it sees the BEGIN line of
  another `/init`, a bare `__HERMIT_COMMAND_BEGIN__` line or one ending in
  `format=2`, also when a kernel message is printed right after it, names it,
  and the demo says to run `demos/clean.sh` and then demo 5 again.
- A reference run saved before this change was started with another kernel
  command line, without `printk.time=1`, so its comparison reports
  `WARN: QEMU argv differs from first run` and the run ends `PARTIAL`, never
  `SUCCESS`. This holds for demo 5's saved first boot too. `demos/clean.sh`
  removes both.

The resume runs under `hermit run --strict --epoch 2026-01-01T00:00:00Z
--no-rcb-time --target-timeslice 100000 --max-timeslice disabled`. It switches
QEMU's threads only at system calls and advances virtual time without counting
branches, so it needs no hardware performance counters. As in demo 5,
`--epoch` fixes the date the virtual clocks start from, `--base-env=minimal`
(plus `PYTHONDONTWRITEBYTECODE`) replaces your shell's environment, and the
guest reads `/dev/null` and writes to a pipe that the demo copies into the
Hermit log, so neither the host clock, your environment, nor the log file's
growing size reaches the guest. The controller runs from
`/tmp/hermit-demo-controller`, a copy of its source files made for each run,
and the assets directory (snapshot, command disk, serial transcript) is bound
at `/tmp/hermit-demo-assets`, so the checkout's location does not reach the
guest's command lines either. Before these paths were fixed, a fresh clone in
another directory resumed to a different post-command snapshot after 288657
scheduler turns instead of 287149.

A run that does not finish. The guest-side controller has no deadline of its
own, because time inside Hermit is virtual. Two bounds outside Hermit end the
demo's wait for a resume that does not end by itself (a command that never
exits, or an END line that a kernel message hid). The demo checks both
repeatedly, the size first, and the first check that finds a bound passed
ends the wait, and the run fails:

- `QEMU_MAX_LOG_BYTES`, the size of Hermit's INFO log. At the default log
  filter Hermit wrote 18.5 to 19.2 MB of INFO log per second of resume on
  2026-10-03, so a run reaches the 512 MiB cap after about 29 seconds. With
  the defaults this is the bound that ends the wait for a command that runs
  longer than that: a `sleep 1000000` failed after 31.1 seconds in one run
  and 30.9 seconds in another.
- `QEMU_TIMEOUT`, the wall-clock seconds of the resume (120 by default). With
  the default cap it is reached only by a resume that logs more slowly.

Either way the demo prints a FAILURE line that names the bound, the size or
time it reached, and how far the guest had got, read from the serial log: no
BEGIN line (the command was not seen to start; a BEGIN line that a kernel
message split looks the same), a BEGIN line with no END line (the command was
not seen to finish; an END line that a kernel message hid looks the same), or
an END line while Hermit and QEMU had not exited. Neither ends in
`SUCCESS`, and the run exits 1. For example, the command
`printf '__HERMIT_COMMAND_END__\001status=0\n' >&0; echo __HERMIT_COMMAND_END__ status=0; sleep 1000000`
ended on 2026-10-03, 30.9 seconds after the demo started, when Hermit's INFO
log had reached 538851401 bytes, 29.2 seconds into the resume. The FAILURE line
that run printed was captured before four corrections, so it is not quoted
here. For a run like it, the demo now prints (path shortened; `N` is the time
the size was found past the cap):

```text
WARN: Demo 6: QEMU Snapshot Resume: FAILURE: Hermit's INFO log .../run-history/resume-<time>-<pid>/hermit-info.log grew to 538851401 bytes, past the 536870912-byte cap (QEMU_MAX_LOG_BYTES), when checked Ns into the resume (QEMU_TIMEOUT is 120s), so the demo signalled Hermit's process group; the guest command was not seen to finish: the serial log has the __HERMIT_COMMAND_BEGIN__ format=3 line but no END line
```

The four corrections: the time in a cap message is now read when the size is
found past the cap, before the demo signals anything. The message says `when
checked Ns into the resume (QEMU_TIMEOUT is 120s)` rather than placing the end
of the run before QEMU_TIMEOUT: when a check finds both bounds passed, the size
is checked first and is what is reported. A BEGIN line without an END line is
reported as `the guest command was not seen to finish`, and a serial log
without a BEGIN line as `the guest was not seen to start the command`. And the
message says what the demo did, `so the demo signalled Hermit's process group`,
instead of claiming that the run had ended, which nothing checked.

At either bound, before it reports, the demo signals Hermit's process group,
the process group it started the `hermit` command in: SIGTERM and then
SIGKILL, at most 10 seconds apart. It waits up to 10 seconds after each signal
for the group to empty, but it does not report whether it did, so no FAILURE
line says that anything stopped. Only that group is signalled. A wrapper such
as `bin/safehermit` can run Hermit, its tracer and QEMU outside it: on
2026-10-04, with Hermit started through `bin/safehermit`, which runs it in a
systemd user unit, the unit holding all three was still active a minute after
the demo printed its FAILURE line. On 2026-10-03, with `QEMU_TIMEOUT=20`, the
command `sleep 1000000` ended 22.0 seconds after the demo started, and no
Hermit, QEMU, or `sleep` process was left running.

The cap also holds after Hermit exits. The demo then waits up to 60 seconds
for the rest of Hermit's output to reach the INFO log. If a process Hermit
left running writes the log past the cap, or still holds Hermit's output open
after those 60 seconds, the demo signals Hermit's process group the same way
and fails, with a cap message that names Hermit's exit status or with
`Hermit's output was still open 60s after it exited, so processes still held it; the demo then signalled Hermit's process group`.

Controls (environment variables):

| Variable | Default | Meaning |
| --- | --- | --- |
| `QEMU_RESUME_REPEAT` | `1` | Set to `0` to skip the second resume of a new command, which then compares nothing. `demos/run-all.sh` always sets it to `1`. |
| `QEMU_TIMEOUT` | `120` | Seconds the demo waits for the resume before it signals Hermit's process group and fails, unless the log reached `QEMU_MAX_LOG_BYTES` first. With the default cap, a resume that keeps running reaches the cap after about 29 seconds, before this timeout; see "A run that does not finish". |
| `QEMU_ASSETS` | `ignored/qemu-linux` | Where demo 5's snapshot and this demo's results are kept. |
| `QEMU_BOOT_SNAPSHOT_DISK` | `$QEMU_ASSETS/hermit-boot.qcow2` | The boot snapshot to restore. Another path is restored only with the record that demo 5 writes next to it (`<path>.producer.json`) for the current initramfs; run demo 5 with `QEMU_SNAPSHOT_DISK=<path>` and the same `QEMU_ASSETS` to save one. This demo never runs demo 5 for it. |
| `QEMU_BIN` | `qemu-system-x86_64` on `PATH` | The QEMU binary. It must be the one demo 5 used. |
| `QEMU_LOG_FILTER` | `warn,detcore=info,reverie_ptrace::task=info` | Hermit's log filter, passed to Hermit as `RUST_LOG`; the demo removes `HERMIT_LOG` and `HERMIT_LOG_FILE` from Hermit's environment. The repeat check needs Hermit's INFO records, so a filter that keeps none fails it. A different filter also changes the log, its size and so how soon `QEMU_MAX_LOG_BYTES` is reached, and a saved reference run no longer applies; run `demos/clean.sh`. |
| `QEMU_MAX_LOG_BYTES` | 512 MiB | Signal Hermit's process group and fail if Hermit's event log grows past this size. Healthy resumes on 2026-09-30 wrote 169,751,813 bytes (`uname -a` with `--no-save-snapshot`, 10.0 to 11.4 seconds) and 258,227,125 to 259,021,561 bytes (the three commands above, saving a snapshot, 15.7 to 16.5 seconds) (Hermit 0.2.0 gdc92644f96f4, QEMU 10.1.2); Hermit 0.2.0 g770b95c505fa wrote 80 to 153 MB. A resume that keeps running reaches this cap after about 29 seconds (18.5 to 19.2 MB per second on 2026-10-03), so with the defaults it, not `QEMU_TIMEOUT`, is what ends the wait for a command that does not finish. The cap also holds while the demo waits, for up to 60 seconds after Hermit exits, for the rest of Hermit's output. |
