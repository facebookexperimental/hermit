# Demo 8: Find and reproduce a schedule-dependent use-after-free (btrfs-progs)

`btrfs-convert`, the tool that converts an ext4 file system to btrfs in place,
once had a use-after-free: a background progress thread could read memory that
the main thread had already freed. Whether it happens depends only on how the
two threads interleave at shutdown, and an ordinary run cannot choose that
interleaving. On the reference host, 29 of 40 native runs of the buggy build
showed no sign of the bug, and the other 11 printed the first lines of an
AddressSanitizer report and then exited with status 0 before the report was
complete. This demo runs two AddressSanitizer builds of btrfs-progs v7.1: a
buggy build that brings the race back, with the progress thread detached and
never joined as before the upstream fix but with the demo's own shutdown code
(see [How it works](#how-it-works)), and a fixed build that joins the thread
as the fix does. Hermit's chaos mode finds a seed whose thread schedule crashed
the buggy build every time it was run on the reference host, the fixed build
run with that seed does not crash, and running the seed again reproduced the
crash report byte for byte. On a shared or heavily loaded machine the crash may
not reproduce reliably. Chaos mode places each thread switch by counting the
CPU's retired branches: Hermit arms the counter's interrupt a safety margin of
branches early and single-steps the rest of the way to the exact switch point.
If the interrupt arrives later than that margin, which is more likely under
heavy host load, Reverie, the library Hermit uses to trace the program, prints
a line starting with `HERMIT_SKID_OVERSHOOT` and Hermit refuses the run: it
prints a line starting with `HERMIT_POLICY_REFUSAL class=policy-refusal
cause=skid-overshoot` and exits with status 122, so `run.sh` reports rc=122
instead of the expected crash. The story of the bug is in
[WRITEUP.md](WRITEUP.md).

## Prerequisites

- `hermit` on your `PATH`, built from this checkout (see
  [Setup](../README.md#setup)).
- A one-time build of the two `btrfs-convert` variants and a small ext4 image,
  done by `prepare-assets.sh` (below). It needs `autoconf`, `automake`, `file`,
  `git`, `make`, `mkfs.ext4` (e2fsprogs), `patch`, `pkg-config`, `truncate`,
  a C compiler with AddressSanitizer support, network access to github.com to
  clone btrfs-progs, and the development libraries that btrfs-progs'
  `configure` checks for: libuuid, libblkid, zlib, LZO, zstd, and, for the
  ext4 converter, libext2fs and libcom_err. `configure` stops at the first
  one it cannot find and names it, for example
  `configure: error: Package requirements (ext2fs) were not met`. On CentOS
  Stream 9 the packages are `libuuid-devel`, `libblkid-devel`, `zlib-devel`,
  `lzo-devel`, `libzstd-devel`, `e2fsprogs-devel`, and `libcom_err-devel`.
  If you unpack the headers and libraries somewhere other than `/usr`, set
  `PKG_CONFIG_PATH` and `LDFLAGS` for them, and pass the include directory in
  `CFLAGS` rather than `CPPFLAGS`: the btrfs-progs Makefile does not read
  `CPPFLAGS`.
- About 256 MB of disk for the image and a few hundred megabytes more for the
  btrfs-progs build under `ignored/`.

Without the prepared assets, `run.sh` prints `SKIPPED` and exits 77, the
conventional "skipped" status, so a skip run on its own, or through
`make -C demos demo8` (which then exits 2), is never read as a pass.
`demos/run-all.sh` sets `DEMO_SKIP_EXIT_STATUS=0` for the demos it runs and
counts this one as skipped rather than failed from its `SKIPPED` line. A
skipped demo produced no result, so the sweep then exits 3 instead of 0.

## Run it

From the repository root, build the assets once and then run the demo:

```bash
demos/08-btrfs-convert-uaf/prepare-assets.sh
demos/08-btrfs-convert-uaf/run.sh
```

`make -C demos demo8` runs only the second step, and skips when the assets
have not been prepared. The preparation clones btrfs-progs v7.1
(commit `4ab0e80be9e3bb1db2e6038e6d4316d35fb7ba8b` from
<https://github.com/kdave/btrfs-progs>), builds a `buggy` and a `fixed` variant
with AddressSanitizer, writes a 256 MiB ext4 image holding 100 small files, and
then tries chaos seeds until it finds one that crashes the buggy build,
crashes it again with the same complete AddressSanitizer report, and leaves
the fixed build clean. It records that seed in
`ignored/demo08-btrfs/.crash-seed`. Running it again only rechecks the
recorded seed, with those same checks.

A run that Hermit refuses with exit status 122 (the overshoot refusal described
under [What to notice](#what-to-notice)) did not test its seed, so the seed
search does not count it as a seed that failed to crash. It runs that seed
again, up to `DEMO08_REFUSAL_RETRIES` more times (default 2, so 3 attempts in
all); this applies to the first run of a seed, its repeat, and the fixed
build's run. The one exception is a refused fixed-build run that printed a
use-after-free report: that report stops the search as a regression, as it
would with any other exit status. When a refused attempt is followed by another
one, its output is kept in a file ending in `-refused-1.out`, `-refused-2.out`,
and so on, and every refused attempt gets a row in `calibration.tsv` whose
`qualifies` column says `refused`. A seed that is refused on every attempt is
reported as refused and skipped. The summary line's `refused=` counts only the
seeds whose first run was refused on every attempt. A seed whose first run
crashed but whose repeat or fixed-build run was refused on every attempt is
counted under `unconfirmed=`, together with the seeds whose repeat or
fixed-build run went past the per-run time limit. `refused_runs=` counts every
refused attempt, including those of repeats and fixed-build runs. The script
fails, saying that no seed was tested, if the first run of every seed it tried
was refused on every attempt.

To run one chaos trial yourself, with the seed that `prepare-assets.sh`
recorded (the tool rewrites its input, so give it a fresh copy each time):

```bash
mkdir -p target/demos/08-btrfs-convert-uaf
cp --reflink=auto ignored/demo08-btrfs/pop-tiny.img target/demos/08-btrfs-convert-uaf/chaos-buggy.img
seed=$(cut -d' ' -f1 ignored/demo08-btrfs/.crash-seed)
hermit --log=error run --chaos --sched-seed "$seed" --no-virtualize-cpuid \
  --base-env=minimal --epoch=2026-01-01T00:00:00Z \
  -- "$PWD/ignored/demo08-btrfs/buggy/btrfs-convert" \
  "$PWD/target/demos/08-btrfs-convert-uaf/chaos-buggy.img"
echo "exit status: $?"
```

For a checkout outside `/tmp`, this is exactly the command `run.sh` runs in
its step 2, and it must stay that way for the recorded seed to apply:

- Keep the image out of `/tmp`, or bind it as below. Hermit gives the program
  a private, empty `/tmp`, so it cannot see an image you copied there;
  btrfs-convert then prints `ERROR: ext2fs_open: No such file or directory`
  and `ERROR: no file system found to convert`, and exits with status 1. For
  the same reason Hermit refuses to start a converter under `/tmp`. In a
  checkout under `/tmp`, `run.sh` and `prepare-assets.sh` therefore add one
  `--bind` for the directory that holds the converter and one for the image,
  which mount exactly those host paths at the same paths inside the private
  `/tmp`; the manual equivalent adds these two lines before `--`:

  ```bash
    --bind "$PWD/ignored/demo08-btrfs/buggy" \
    --bind "$PWD/target/demos/08-btrfs-convert-uaf/chaos-buggy.img" \
  ```

  For a checkout outside `/tmp` the scripts add nothing, so their command line
  and the seeds recorded with it are unchanged. The converter is bound through
  its directory because Hermit cannot start a program whose path is itself a
  `--bind` target: it reports that the program does not exist
  (<https://github.com/rrnewton/hermit/issues/3626>). `--tmp=/tmp` would also
  work, but it would show the converter everything in the host's
  `/tmp` and make all of it an input to the run. The binds add entries to the
  guest's mount table and the directories leading to them inside the private
  `/tmp`. The patched build reads no mount table (see
  [How it works](#how-it-works)). The 117-line mount table in the UUID
  measurement under [What to notice](#what-to-notice) came from eight extra
  `--bind` options ([WRITEUP.md](WRITEUP.md)), and with the current harness
  it printed the same UUID as the 101-line table in all six runs. No run has
  measured whether the binds change which seeds crash. `prepare-assets.sh`
  calibrates with the same binds that
  `run.sh` replays, so moving a checkout into or out of `/tmp` calls for
  running it again, as any change of path does. The scripts stop before they
  create a directory, build, or run anything, naming the path and the reason,
  when a path under `/tmp` cannot be shown this way: for example a path that
  reaches `/tmp` through a symbolic link, a converter that is itself a
  symbolic link or is given as a relative path, or a path that begins with
  `/tmp/` and contains `:` or a `..` component.
- Keep the same absolute paths. The paths are the program's arguments, and a
  different argument length shifts the program's memory layout. With seed 7 on
  the reference build, image paths of 75, 85 (the path above), and 104
  characters each crashed three times out of three with the same heap
  address, program counter, program frames, and `SUMMARY:` line, the lines
  that step 2 prints. With the image at paths of 36 to 40 characters under
  `/var/tmp`, seed 7 still crashed, but at heap address `0x6060000002d0`
  instead of `0x606000000210`. The rest of the report can depend on the path
  even when those lines do not. Of the seed-7 runs under `/var/tmp` whose
  output is still on the reference host, the 9 with a 38-character image path
  all printed one complete report, the 21 with a 39-character path all printed
  a second, and the one with a 40-character path printed a third; the three
  differ only in one row of the shadow memory map that ends the report. A path
  change can also change whether a seed reaches the use-after-free at all:
  seed 1 printed the start of a report with the image at the path above, and
  no report with it under `/var/tmp`.
- Keep `--base-env=minimal`. Without it the program inherits your shell's
  whole environment, which also shifts its memory layout: with the path above
  and seed 7, three runs that inherited the environment all exited 0 without a
  use-after-free.
- `--epoch` fixes the virtual clock's starting time. Without it Hermit starts
  the clock at the host's current time and prints
  `virtual-time epoch=... source=host-now`.

The end of the output, observed on 2026-09-30 with seed 7:

```text
hermit: virtual-time epoch=2026-01-01T00:00:00+00:00 source=explicit; reproduce with --epoch=2026-01-01T00:00:00+00:00
btrfs-convert from btrfs-progs v7.1
...
Copy inodes [o] [         0/       111]
=================================================================
==3==ERROR: AddressSanitizer: heap-use-after-free on address 0x606000000210 at pc 0x0000004e68e1 bp 0x7ffff3ffeaf0 sp 0x7ffff3ffeae0
READ of size 8 at 0x606000000210 thread T1
    #0 0x4e68e0 in task_period_wait common/task-utils.c:154
    #1 0x41215a in print_copied_inodes convert/main.c:169
...
SUMMARY: AddressSanitizer: heap-use-after-free common/task-utils.c:154 in task_period_wait
...
==3==ABORTING
... ERROR reverie_ptrace::lifecycle: guest terminated by signal tid=3 pid=3 signal=SIGABRT core_dumped=true
... ERROR reverie_ptrace::lifecycle: guest terminated by signal tid=5 pid=3 signal=SIGABRT core_dumped=true
exit status: 134
```

Elided: the description of the source and target file systems, the rest of
the AddressSanitizer report (the freeing and allocating stacks and the shadow
memory map), and the host wall-clock timestamps that begin Hermit's two
`ERROR` lines. Those two lines are Hermit's own log, printed as it sees each
thread end, and their order is not fixed: in the 13 saved outputs of seed 7 at
this path, `tid=3`'s line came first in 10 and `tid=5`'s in 3. The demo
leaves Hermit's log lines out and compares everything else from the report's
`ERROR:` line through `==3==ABORTING`, byte for byte: the complete
AddressSanitizer report, 63 lines here, including the parts elided above.
Five outputs of seed 7 at this path are still on the reference host, from
2026-09-30 and 2026-10-01, and all five contain the same complete report;
two of them are a run of `run.sh` and its repeat, and two are a recheck by
`prepare-assets.sh` and its repeat. A run and its repeat by `run.sh` in
another checkout, with a 109-character image path, printed that same report
too.

## What you will see

Observed on 2026-09-30 with Hermit `dc92644f96f4`:

```text
=== Demo 8: schedule-dependent btrfs-convert use-after-free ===
Using hermit 0.2.0 (...) (...)
seed=7 timeout=90s
buggy=.../ignored/demo08-btrfs/buggy/btrfs-convert
fixed=.../ignored/demo08-btrfs/fixed/btrfs-convert

--- Step 1: native buggy btrfs-convert ---
native buggy: rc=0 with no use-after-free report

--- Step 2: chaos buggy, --sched-seed 7 (expect an ASAN use-after-free) ---
==3==ERROR: AddressSanitizer: heap-use-after-free on address 0x606000000210 at pc 0x0000004e68e1 bp 0x7ffff3ffeaf0 sp 0x7ffff3ffeae0
    #0 0x4e68e0 in task_period_wait common/task-utils.c:154
    #1 0x41215a in print_copied_inodes convert/main.c:169
SUMMARY: AddressSanitizer: heap-use-after-free common/task-utils.c:154 in task_period_wait
chaos buggy: reproduced the use-after-free

--- Step 3: chaos fixed, --sched-seed 7 (expect a clean exit) ---
chaos fixed: completed rc=0 with no use-after-free (73e211a7 closes the window)

--- Step 4: run --sched-seed 7 again and compare the crash ---
replay: ASAN report byte-identical (same heap address, PC, and frames)

=== Demo 8: btrfs-convert Use-After-Free: SUCCESS ===
the native run showed no use-after-free; chaos crashed on seed 7,
the fix closed the window, and the crash reproduced exactly.
```

Elided: your Hermit version and path, and the asset paths. The seed, the heap
address, the program counters, and the source line numbers depend on your
compiler, the exact binaries, and the checkout's path, so yours may differ.

Step 1 runs the buggy build natively, so its line changes from run to run. In
2 of 11 runs of `run.sh` between 2026-09-30 and 2026-10-01 the native run
caught part of the use-after-free, and step 1 and the last lines read instead:

```text
--- Step 1: native buggy btrfs-convert ---
native buggy: rc=0; ASAN started a use-after-free report, but the process exited before the report's SUMMARY:
==3617152==ERROR: AddressSanitizer: heap-use-after-free on address 0x606000000210 at pc 0x0000004e68e1 bp 0x7f0fca7feaf0 sp 0x7f0fca7feae0
...
=== Demo 8: btrfs-convert Use-After-Free: SUCCESS ===
the native run started an ASAN report but exited rc=0 before it completed; chaos crashed on seed 7,
the fix closed the window, and the crash reproduced exactly.
```

Steps 2 to 4 printed the same lines in all 11 runs of `run.sh`, including
one whose environment carried 3,000 extra bytes.

The runs above used the earlier `run.sh`, which compared only the four report
lines that step 2 prints. This version compares the complete report, and two
of its lines read differently: step 2 ends with the first line below instead
of `chaos buggy: reproduced the use-after-free`, and step 4 prints the second
instead of `replay: ASAN report byte-identical (same heap address, PC, and
frames)`. Both lines are written here from the script and from the 63-line
reports in the saved outputs, not copied from a run:

```text
chaos buggy: reproduced the use-after-free; the complete 63-line ASAN report is in .../target/demos/08-btrfs-convert-uaf/asan-report.txt
replay: ASAN report byte-identical: all 63 lines from the ERROR line through ABORTING, including the heap address, PC, every stack, and the shadow memory
```

## What to notice

- The native run cannot choose its interleaving. In 40 native runs of the
  buggy build, 29 showed no use-after-free and 11 printed the start of an
  AddressSanitizer report, then exited with status 0 before the report's
  `SUMMARY:` line: the main thread finished the conversion and exited while the
  progress thread was still writing the report. None of the 40 gave a complete
  report or a nonzero exit status, and 20 native runs of the fixed build showed
  nothing. The chaos run in step 2 crashed with a complete report in every one
  of 141 runs of seed 7 on this build: 44 with the demo's own command and image
  path, and 97 with the image at other paths.
- `==3==` is the process ID AddressSanitizer saw. Under Hermit that is a
  virtual process ID, so it too is the same on every run; the native report
  shows the host's process ID instead.
- Step 3 runs the fixed build under the same seed. The only difference between
  the two builds is the fix, so its clean exit shows the crash came from the bug
  and not from the scheduler.
- Step 4 compares the saved reports, `asan-report.txt` and
  `asan-report-replay.txt` under `target/demos/08-btrfs-convert-uaf/`, byte
  for byte. Each holds the complete AddressSanitizer report from its `ERROR:`
  line through `==3==ABORTING`: the faulting address and program counter, the
  stacks of the faulting read, of the free, of the allocation, and of the
  thread's creation, and the shadow memory map. Only Hermit's own log lines,
  which begin with a wall-clock timestamp, are left out. Nothing inside the
  report is changed or skipped, and a run whose report stops before
  `==3==ABORTING` fails the demo. `prepare-assets.sh` compares a seed's first
  run and its repeat the same way before it records the seed, and stops if
  the two reports differ.
- The crashing seed is a property of the build and of the exact command line.
  A different compiler or Hermit version can move the crash to other seeds,
  which is why `prepare-assets.sh` searches for one and ties it to the buggy
  binary's SHA-256; `run.sh` refuses a recorded seed whose binary hash does not
  match. The image path and the program's environment matter too, which is why
  both scripts and the trial command above use the same paths,
  `--base-env=minimal`, and a fixed `--epoch`. With those flags, seed 7
  crashed in all 44 runs at the demo's path from 21:08 UTC on 2026-09-30 to
  00:19 UTC on 2026-10-01: one seed search and six rechecks by
  `prepare-assets.sh`, 11 runs of `run.sh`, and 8 single trials. One recheck
  and one `run.sh` run had 3,000 extra bytes in their environment.
- The host's load was not recorded during those 44 runs. Three later runs of
  `run.sh` on 2026-10-01, at 19:59, 20:10, and 20:52 UTC, all passed with seed
  7 on the same 316-thread host. They used Hermit built from hermit main
  `1139c661ede3` plus commits that change no Hermit source, took 9.1, 9.3, and
  8.9 seconds, and the host's 1-minute load average, read from `/proc/loadavg`
  at the start and end of each run, was between 18.18 and 38.72. On a shared or
  heavily loaded machine the crash may not reproduce reliably, even with the
  same build, seed, and command line. Chaos mode places each thread switch by
  counting the CPU's retired branches. This demo's command does not pass
  `--imprecise-timers`, so Hermit uses precise timers: it arms the counter's
  interrupt a safety margin of branches early and single-steps the rest of the
  way to the exact switch point. If the interrupt arrives later than that
  margin, which is more likely under heavy host load, Reverie prints a line
  starting with `HERMIT_SKID_OVERSHOOT` and Hermit refuses the run instead of
  treating it as deterministic: it prints a line starting with
  `HERMIT_POLICY_REFUSAL class=policy-refusal cause=skid-overshoot count=` and
  exits with status 122, whether or not the program crashed. Steps 2, 3, and 4
  then report rc=122 instead of the result they expected, add a line that
  begins `rc=122 means Hermit refused the run` and names the file to look in,
  and exit 1. `run.sh` saves each Hermit run's standard error with its output
  in its artifacts directory, `target/demos/08-btrfs-convert-uaf/` by default.
  Both lines are in `chaos-buggy.out` when step 2 reports rc=122, in
  `chaos-fixed.out` for step 3, and in `chaos-buggy-replay.out` for step 4.
- The program no longer reads the mount table. Under Hermit,
  `/proc/self/mounts` lists mounts that come from the host at the time of the
  run, and Hermit passes it through unchanged
  (<https://github.com/rrnewton/hermit/issues/1820>); btrfs-convert's check
  that its target is not mounted reads that whole table. On a host where
  other software mounts and unmounts file systems, that made the schedule, and
  with it the crashing seed and the printed file system UUID, change from one
  run to the next. The demo's harness skips that check to work around this gap
  in Hermit's determinism, so the demo does not test btrfs-convert's refusal to
  convert a mounted file system (see [How it works](#how-it-works)).
- The target file system UUID that btrfs-convert prints repeats too. libuuid
  builds it from random bytes, which Hermit makes repeatable, and a mask
  seeded from the clock, which is Hermit's virtual clock, so anything that
  changes how far that clock has advanced changes the UUID. The mount table
  was such an input: with a harness that still ran the mount check, a
  101-line guest mount table gave UUID `f10affb6-...` in three runs and a
  117-line table gave `73a3c4ee-...` in three runs. With the current harness
  both tables gave `934405ce-...` in all six runs. In all 110 kept seed-7
  runs of the current harness, over more than three hours, each image path
  printed one UUID and no run differed from its path's UUID.
  `--strict` does not address this: in a 12-run comparison with the older
  harness, `--strict` and non-strict runs each printed two different UUIDs
  (<https://github.com/rrnewton/hermit/pull/2907>). The UUID also depends on
  the image path, like the heap layout: the 75-, 85-, and 104-character paths
  above gave three different UUIDs, each the same in every run at that path
  (3, 13, and 3 runs), and the paths of 36 to 40 characters under `/var/tmp`
  all gave `934405ce-...` (91 runs).

## How it works

btrfs-convert starts a progress thread that prints a spinner while the main
thread copies inodes. Before upstream commit
[73e211a7](https://github.com/kdave/btrfs-progs/commit/73e211a7a8ff3d2395783daaed71bf3792bd753f)
the thread was detached and never joined, so when the conversion finished, the
main thread could free the thread's shared `struct task_info` while the thread
was still reading it. The `buggy` variant has the detached, unjoined thread; the
`fixed` variant joins it before freeing, which is the upstream fix.

Under `hermit run --chaos`, Hermit still runs one thread at a time, but it
picks thread switches pseudo-randomly from `--sched-seed`. Some seeds happen to
let the main thread free the memory just before the progress thread's last
read, and AddressSanitizer turns that read into an abort with exit status 134.
Because the schedule is a function of the seed, running the same seed on the
same input reproduced the same interleaving and the same report on the host
described under [What to notice](#what-to-notice), which also gives the host's
load where it was recorded. Under heavy host load a late counter interrupt
makes Hermit refuse the run with exit status 122.

The two variants carry a small harness, applied identically to both, in
[`fixtures/`](fixtures/):

- The original progress thread wakes once a second from a `CLOCK_MONOTONIC`
  timer. Hermit's clock advances with the program's own progress rather than
  with real time, and this input-and-output-bound conversion does little
  computation, so when the demo was built the timer never fired under Hermit
  and the thread slept through the race. The harness replaces the timer with a
  pipe: the thread blocks reading it, and the shutdown path writes one byte to
  wake it for one last loop iteration, the iteration that races the free.
- The harness removes btrfs-convert's check that its target is not mounted.
  This is a workaround for a gap in Hermit's determinism, not part of the bug:
  Hermit lets the host's mount table reach the program. The check reads
  `/proc/self/mounts`, calls `stat` on each entry, and reads
  `/sys/block/loop*/loop/backing_file`. Under Hermit, `/proc/self/mounts`
  lists the mounts the host has at the time of the run, passed through
  unchanged, and the loop devices are the host's. So the number of entries,
  and with it the number of system calls and branch instructions in the check
  and the chaos schedule after it, followed whatever the host had mounted. The
  harness replaces `ret = check_mounted(file);` with `ret = 0;`. As a result,
  the demo does not exercise btrfs-convert's refusal to convert a mounted file
  system: the patched builds would go on to convert a mounted target, so use
  them only on the demo's image. That image is a private copy that is never
  mounted, so the check had nothing to find. Run natively under `strace`, the
  patched build opens neither `/proc/self/mounts` nor any `/sys/block` file;
  the unpatched build opened `/proc/self/mounts` once and
  `/sys/block/loop*/loop/backing_file` 14 times. Giving the program a mount
  table that does not depend on the host's mounts is tracked in
  <https://github.com/rrnewton/hermit/issues/3627>.
  <https://github.com/rrnewton/hermit/issues/1820> is a narrower, related
  issue: a short-lived host mount changed `findmnt` output between the two
  runs of a Hermit `--verify`.
- The demo runs the program with `--base-env=minimal`, so AddressSanitizer
  settings such as `ASAN_OPTIONS` in your environment do not reach it; instead
  `convert/main.c` defines `__asan_default_options()` with
  `abort_on_error=1`, leak detection off, and `disable_coredump=0` (which avoids
  a core-size `setrlimit` call that Hermit rejects).

The files are in [`fixtures/`](fixtures/): `buggy/common/` and `fixed/common/`
hold each variant's `task-utils.c` and `task-utils.h`, both with the pipe, and
[`convert-main-v7.1.patch`](fixtures/convert-main-v7.1.patch) makes the
`convert/main.c` changes, the same for both variants. None of these changes
decides which shutdown order is safe. The race is the same kind as the
original bug, a detached thread that can read `task_info` after the main
thread has freed it, but the buggy build reaches it through the demo's own
shutdown code rather than the 2015 code. Before upstream commit 73e211a7,
`task_stop()` asked the thread to stop with `pthread_cancel()`, which takes
effect only when the thread reaches a cancellation point, and returned
without waiting, after which the caller freed `task_info` with
`task_deinit()`. The buggy build's `task_stop()` sets a stop flag and writes
the one byte to the pipe instead, and it too returns without waiting. The
fixed build joins the thread before the free, as 73e211a7 does, but without
73e211a7's `pthread_cancel()`: its thread leaves its loop when it sees the
flag.

Each run gets a fresh copy of the image, because btrfs-convert converts it in
place. Step 4 reuses the image path of step 2: the path is part of the
program's arguments, and a different argument length shifts the initial heap
layout, which can produce a different (but equally repeatable) heap address or
shadow memory map; the measurements are under [Run it](#run-it).

Controls (environment variables):

| Variable | Default | Meaning |
| --- | --- | --- |
| `DEMO08_DIR` | `ignored/demo08-btrfs` | Directory holding `buggy/`, `fixed/`, `pop-tiny.img`, and `.crash-seed`. Under `/tmp`, give an absolute path that begins with `/tmp/`; see [Run it](#run-it). |
| `DEMO08_ARTIFACTS` | `target/demos/08-btrfs-convert-uaf` | Scratch images and saved reports. Under `/tmp`, give an absolute path that begins with `/tmp/`; see [Run it](#run-it). `demos/clean.sh` removes only the default directory and names an override instead. |
| `DEMO08_CRASH_SEED` | from `.crash-seed`, else `7` | The chaos seed to use. |
| `DEMO08_TIMEOUT` | `90` | Seconds allowed per run. `prepare-assets.sh` applies the same limit, so it only records a seed that fits. |
| `DEMO08_REQUIRE_ASSETS` | `0` | Set to `1` to fail (exit 1) instead of skipping when the assets are missing. With `0`, a missing asset is a skip, which exits with `DEMO_SKIP_EXIT_STATUS`. |
| `DEMO_SKIP_EXIT_STATUS` | `77` | The exit status of a skip, from 0 to 255; any other value is refused with exit 2. `demos/run-all.sh` sets `0`, because it reads the `SKIPPED` line and counts the demo as skipped. |
| `DEMO08_CALIBRATION_SEEDS` | `64` | How many seeds `prepare-assets.sh` tries. |
| `DEMO08_REFUSAL_RETRIES` | `2` | How many more times `prepare-assets.sh` runs a seed after Hermit refuses a run with exit status 122. A seed refused on every attempt is reported as refused, not as one that did not crash. |
| `DEMO08_BUILD_ROOT`, `DEMO08_BUILD_JOBS`, `DEMO08_BTRFS_REPO` | `ignored/demo08-build`, all CPUs, the GitHub URL | Where and how the btrfs-progs build runs. |

How long it takes, measured on 2026-09-30 and 2026-10-01 (UTC) with Hermit
`dc92644f96f4` on a 316-thread AMD EPYC 9D85 host. The host's load was not
recorded during these measurements; the load during three later runs of
`run.sh` is given under [What to notice](#what-to-notice).

| Step | Time |
| --- | --- |
| `prepare-assets.sh` from scratch, with the btrfs-progs clone already present (both builds, image, seed search over seeds 0 to 7) | 131 s |
| `prepare-assets.sh` when the recorded seed still crashes (recheck only) | 7.3 to 12.5 s (6 runs) |
| One chaos run of the buggy build, seeds 0 to 31 | 1 to 66 s, median 6 s, for 29 seeds; seeds 16, 25, and 28 were stopped at the 90 s limit |
| `run.sh` | 7.9 to 14.7 s (10 timed runs) |
| The single trial above | 2.7 to 3.1 s (5 runs) |

Seeds 16, 25, and 28 were slow in the fixed build too, so they are slow
schedules for this workload, not a sign of the bug. Without the limit, seed 16
finished correctly after 555 s (buggy) and 564 s (fixed); while it was slow,
a newly created thread never ran and the thread that created it spun in
`sched_yield` inside AddressSanitizer's `pthread_create`
(<https://github.com/rrnewton/hermit/issues/3444>). `prepare-assets.sh` moves
past a seed that reaches the limit.
