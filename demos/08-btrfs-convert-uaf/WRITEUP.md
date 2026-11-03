# Finding and reproducing a schedule-dependent use-after-free

This is the story behind [demo 8](README.md): a real, historical concurrency
bug in btrfs-progs that an ordinary run cannot reproduce on demand, and that
Hermit reproduced with one seed, seed 7, in every run counted on one 316-thread
host: all 44 runs of the demo's command, during which the host's load was not
recorded, and three later runs of `run.sh`, with the 1-minute load average
between 18.18 and 38.72 at their starts and ends. On a shared or heavily loaded
machine it may not reproduce reliably. Chaos mode places each thread switch by
counting retired branches: Hermit arms the counter's interrupt a safety margin
of branches early and single-steps the rest of the way to the exact switch
point. If the interrupt arrives later than that margin, which is more likely
under heavy host load, Reverie, the library Hermit uses to trace the program,
prints a line starting with `HERMIT_SKID_OVERSHOOT` and Hermit refuses the run:
it prints a line starting with `HERMIT_POLICY_REFUSAL class=policy-refusal
cause=skid-overshoot` and exits with status 122 instead of reporting the crash.

## The bug

`btrfs-convert` turns an ext4 file system into a btrfs file system in place.
While it copies inodes, a second thread prints a progress spinner. The two
threads share a small structure, `struct task_info`, which the main thread
allocates before starting the progress thread and frees when the conversion is
done.

Until 2015 the progress thread was started detached, and the shutdown code
never waited for it to exit. So at the end of a conversion there was a short
window in which the main thread had already freed `task_info` while the progress
thread was about to read it one last time. If the progress thread's read came
first, nothing happened. If the free came first, the progress thread read freed
memory: a heap use-after-free. Which one came first depended only on how the
operating system happened to schedule the two threads at that moment.

Upstream fixed it in btrfs-progs commit
[73e211a7](https://github.com/kdave/btrfs-progs/commit/73e211a7a8ff3d2395783daaed71bf3792bd753f)
(Zhao Lei, 2015-07-27, "btrfs-progs: Fix wrong address accessing by subthread
in btrfs-convert"): stop detaching the thread, and join it before freeing its
data. The symptom reported upstream was an assertion failure in
`btrfs_cow_block`, far from the real cause, which is typical of use-after-free
bugs.

Hermit did not find a new bug here. The demo brings the race back into
btrfs-progs v7.1, with the progress thread detached and never joined as before
2015 but with its own shutdown code in place of the 2015 code (see the setup
below), and asks a simple question: can a tool reliably expose a real race
that ordinary runs hit only by chance, and then reproduce it exactly?

## The setup

Two builds of btrfs-progs v7.1, both with AddressSanitizer, which turns a read
of freed memory into an immediate abort with a detailed report:

- **buggy**: the progress thread is detached and never joined (before
  73e211a7).
- **fixed**: the progress thread is joined before its data is freed
  (73e211a7).

Neither build's shutdown code is the 2015 code. Back then, shutdown asked the
progress thread to stop with `pthread_cancel`, which takes effect only when
the thread reaches a cancellation point, and `task_info` was freed without
waiting for the thread. The buggy build instead sets a stop flag and writes
one byte to a pipe to wake the thread (the harness below explains the pipe),
and likewise does not wait, so it has the same kind of race through different
code. The fixed build joins the thread before the free, as 73e211a7 does, but
without 73e211a7's `pthread_cancel`: its thread leaves its loop when it sees
the flag.

The input is a 256 MiB ext4 image holding about 100 small files. Every run
converts a fresh copy.

Both builds carry the same small harness, so the only difference in behaviour
between them is the fix:

- The original progress thread woke once a second from a real-time
  (`CLOCK_MONOTONIC`) timer. Under Hermit, time advances with the program's own
  progress rather than with the wall clock, and during this input-and-output
  heavy conversion it advanced so little that the timer never fired: the
  progress thread slept through the whole run and the window never opened. The
  harness replaces the timer with a pipe. The progress thread blocks reading
  it, and shutdown writes a single byte to wake it for one final loop
  iteration, the one that races the free. Hermit has since made timers follow
  its virtual clock
  (<https://github.com/rrnewton/hermit/pull/1169>, merged 2026-07-30), so the
  unmodified timer might work today; that has not been measured.
- btrfs-convert normally starts by checking that its target is not mounted,
  which reads the whole mount table. Under Hermit that table comes from the
  host's current mounts, and Hermit passes it through unchanged, so on a host
  where other software mounts and unmounts file systems, that check made the
  schedule differ from run to run. Giving the program a mount table that does
  not depend on the host's mounts is tracked in
  <https://github.com/rrnewton/hermit/issues/3627>;
  <https://github.com/rrnewton/hermit/issues/1820> is a narrower, related
  issue. The harness skips the check to work around this gap in Hermit; it is
  not part of the bug. As a result the demo does not exercise btrfs-convert's
  refusal to convert a mounted file system, and the patched builds would go on
  to convert a mounted target, so use them only on the demo's image, a plain
  file that is never mounted.
- The demo runs the program with `hermit run --base-env=minimal`, so
  AddressSanitizer options from your environment do not reach it. Both builds
  compile in their options instead: abort on the first error, no leak
  checking, and no attempt to change the core-file size limit.

## The results

Measured on 2026-09-30 with Hermit commit `dc92644f96f4` on a Linux 7.1.3 host
with 316 CPUs, buggy binary SHA-256 `b4a66dd45185...` and fixed binary
`605d9b8a3ee8...`. A chaos run is
`hermit --log=error run --chaos --sched-seed <seed> --no-virtualize-cpuid --base-env=minimal --epoch=2026-01-01T00:00:00Z -- <build>/btrfs-convert <image copy>`,
with the image copy at a 40- or 41-character path under `/var/tmp`, and each
run was limited to 90 seconds.

| How it ran | Runs | Complete use-after-free reports | Other outcomes |
| --- | --- | --- | --- |
| buggy build, run normally | 40 | **0** | 29 clean; 11 printed the start of a report, then exited 0 before it was complete |
| fixed build, run normally | 20 | 0 | 20 clean |
| buggy build, Hermit chaos seeds 0 to 31 | 32 | **5** (seeds 7, 15, 23, 27, and 31), each exit 134 | 22 clean; 2 printed the start of a report and exited 0 (seeds 14 and 18); 3 stopped at the 90-second limit (seeds 16, 25, and 28) |
| fixed build, Hermit chaos seeds 0 to 31 | 32 | 0 | 29 clean, the same 3 slow seeds |

In the native runs that caught part of the use-after-free, the main thread
finished the conversion and exited while the progress thread was still
printing AddressSanitizer's report, so the process exited with status 0 and the
report stopped before its `SUMMARY:` line. A test that checks the exit status
would have passed all 40 runs.

Seeds 16, 25, and 28 were slow in both builds, so they are slow thread
schedules for this workload rather than a sign of the bug. Without the limit,
seed 16 finished correctly after 555 seconds (buggy) and 564 seconds (fixed).
While it was slow, a newly created thread never ran and the thread that created
it spun in `sched_yield` inside AddressSanitizer's `pthread_create`
(<https://github.com/rrnewton/hermit/issues/3444>).

The five crashing seeds printed the same report lines. Seed 7, run again with
the same command and the image at 36- to 39-character paths under `/var/tmp`,
crashed with those same report lines in all 90 reruns, from 21:09 UTC on
2026-09-30 to 00:19 UTC on 2026-10-01. The complete reports of runs at the same
path length were byte-identical; between path lengths they differed only in one
line of the shadow-memory map.

The report below comes from the demo's single-trial command in the
[walkthrough](README.md), with seed 7 and the image under
`target/demos/08-btrfs-convert-uaf/`. The seed, the addresses, and the line
numbers belong to that build and command line; the functions are the ones the
bug involves.

```text
==3==ERROR: AddressSanitizer: heap-use-after-free on address 0x606000000210 at pc 0x0000004e68e1 bp 0x7ffff3ffeaf0 sp 0x7ffff3ffeae0
READ of size 8 at 0x606000000210 thread T1
    #0 0x4e68e0 in task_period_wait common/task-utils.c:154
    #1 0x41215a in print_copied_inodes convert/main.c:169
    ...

0x606000000210 is located 16 bytes inside of 56-byte region [0x606000000200,0x606000000238)
freed by thread T0 here:
    #0 0x7ffff74b46b7 in free (/lib64/libasan.so.6+0xb46b7)
    #1 0x4e65a6 in task_deinit common/task-utils.c:100
    #2 0x418691 in do_convert convert/main.c:1354
    #3 0x418691 in main convert/main.c:2116
    ...

previously allocated by thread T0 here:
    #0 0x7ffff74b4bd7 in calloc (/lib64/libasan.so.6+0xb4bd7)
    #1 0x4e621a in task_init common/task-utils.c:29
    #2 0x4185b1 in do_convert convert/main.c:1343
    ...

Thread T1 created by T0 here:
    #0 0x7ffff74587d5 in pthread_create (/lib64/libasan.so.6+0x587d5)
    #1 0x4e6346 in task_start common/task-utils.c:56
    #2 0x4185e5 in do_convert convert/main.c:1345
    ...

SUMMARY: AddressSanitizer: heap-use-after-free common/task-utils.c:154 in task_period_wait
...
==3==ABORTING
```

Elided: the C library frames (`start_thread`, `clone3`,
`__libc_start_call_main`), the `main` frames at the bottom of the last two
stacks, and the shadow-memory map. Five outputs of seed-7 runs at the demo's
path, from 2026-09-30 and 2026-10-01, are still on the reference host, and all
five contain the same complete report, shadow-memory map included; the
[README](README.md#run-it) says which runs they came from.
`==3==` is the process ID the program saw, which under Hermit is a
virtual, repeatable number.

The report reads exactly like the bug: thread T1, the progress thread, read 8
bytes inside the 56-byte `task_info` that `task_init` allocated, after
`task_deinit` on the main thread had freed it.

## What this shows, and what it does not

- **Ordinary runs cannot reproduce it on demand.** In 40 native runs of the
  buggy build, 11 caught part of the use-after-free and 29 did not, all with
  exit status 0 and none with a complete report. Nothing about a native run
  lets you ask for the crash again.
- **Chaos scheduling finds it.** Hermit still runs one thread at a time, but in
  chaos mode it chooses where to switch threads pseudo-randomly from a seed. 5
  of 32 seeds gave a complete report.
- **The fix holds on the same seeds.** The fixed build ran every seed without
  a use-after-free. A seed names a schedule for one binary, so this is the
  same seeds, not the same interleavings.
- **A crash becomes a repeatable test case.** On the 316-thread host used for
  the demo, the same seed on the same build and command line gave the same
  interleaving and the same report; the last limit below gives the host's load.

Three limits are worth knowing:

- **The crashing seed depends on the build and the command line.** A seed
  names a schedule for one particular binary under one particular Hermit. With
  one earlier buggy binary, Hermit commit `103657d48a99` crashed on seeds 15
  and 19 of 0 to 31, and Hermit commit `00ed139b` crashed on seeds 3, 6, 10,
  and 13 instead, and not on 15, 17, or 19 (reported in
  <https://github.com/rrnewton/hermit/pull/2907>). A CI build of the demo found
  seed 47 (<https://github.com/rrnewton/hermit/actions/runs/34363123629>). The
  image path and the program's environment count too: with the image at the
  demo's own path rather than under `/var/tmp`, seed 7 crashed at a different
  heap address, and seed 1 printed the start of a report where under `/var/tmp`
  it printed none; without `--base-env=minimal`, seed 7 did not crash at all in
  three runs. That is why the demo's `prepare-assets.sh` searches for a crashing
  seed with the demo's own command line and records it next to the binary's
  hash, rather than hard-coding one.
- **The printed UUID repeated only once the mount table was out of the
  picture, and `--strict` was not what fixed it.** btrfs-convert prints the new
  file system's UUID. libuuid builds it from random bytes, which Hermit makes
  repeatable, and a mask seeded from `gettimeofday`, which reads Hermit's
  virtual clock, so anything that changes how far that clock has advanced
  changes the UUID. A reviewer measured this in
  <https://github.com/rrnewton/hermit/pull/2907>: 12 `--strict` runs of one
  seed printed two different UUIDs, and so did 12 runs without `--strict`. In a
  follow-up of 24 `--strict` runs, the random bytes were identical in all 24,
  23 runs printed one UUID and one run another, and that run's virtual-clock
  readings differed from its ninth reading on, by 10,180 ns. Across 96 runs in
  five configurations the reviewer counted 7 such runs, and in one batch the
  odd run also missed the use-after-free. All of those runs used a harness that
  still ran btrfs-convert's mount check, so the program's clock depended on
  the length of the mount table. Changing that length, by adding eight
  `--bind` options, changed the UUID: with the older harness and seed 7, a
  101-line guest mount table printed one UUID in three runs and a 117-line
  table another in three runs; with the current harness, both tables printed
  the same UUID in all six. Over more than three hours, all 110 kept seed-7
  runs of the current harness printed one UUID per image path, with no
  exception: 91 runs at `/var/tmp` paths of 36 to 40 characters (60 of them in
  five rounds 35 minutes apart), 13 at the demo's path, and 3 at each of two
  other paths. If runs still went
  astray at the reviewer's rate of 7 in 96, the chance that none of 110 would
  is (89/96)^110, about 1 in 4,100. The evidence is consistent with the mount
  table being the whole cause, and does not exclude a rarer one.
- **Heavy host load can make Hermit refuse a run.** Hermit arms the counter
  interrupt for each chaos-mode thread switch a safety margin of branches
  early and single-steps the rest of the way. An interrupt that arrives later
  than that margin, which is more likely when the host is busy, makes Reverie
  print a line starting with `HERMIT_SKID_OVERSHOOT`. Hermit then refuses the
  run instead of treating it as deterministic: it prints a line starting with
  `HERMIT_POLICY_REFUSAL class=policy-refusal cause=skid-overshoot` and exits
  with status 122, and `run.sh` reports rc=122 instead of the expected crash.
  All 44 seed-7 runs counted in the
  [demo's README](README.md#what-to-notice) crashed. The host's load was not
  recorded during them; the README gives the 1-minute load average during
  three later passing runs of `run.sh` on 2026-10-01, 18.18 to 38.72 on 316
  hardware threads.

## Try it

```bash
demos/08-btrfs-convert-uaf/prepare-assets.sh
demos/08-btrfs-convert-uaf/run.sh
```

See the [demo 8 walkthrough](README.md) for prerequisites and what the output
means.
