# Hermit demos

Hermit runs Linux programs deterministically. Run a program twice under
`hermit run` and it sees the same random bytes, the same process IDs, the same
memory addresses, and the same order of events between its threads, so it
produces the same output. Its clock readings repeat too when both runs pass the
same `--epoch`, the instant at which Hermit's virtual clock starts; without it,
each run starts the clock at the host's current time. Hermit does this without
recompiling or modifying the program: it runs the program under `ptrace`,
intercepts its system calls, answers time and randomness requests from virtual
sources, and runs all of the program's threads one at a time in an order it
chooses. The same control lets Hermit do the opposite on purpose: explore many
different thread orders to shake out concurrency bugs, and then replay the one
that failed. Hermit is not a security sandbox, and it cannot make a changing
file system or network deterministic.

## What you can do with it

- **Run a program twice and get the same answer**, from `date` and
  `/dev/urandom` to Python's hash ordering and heap addresses
  ([demo 1](01-deterministic-run/README.md)).
- **Record an execution and replay it later**, including under GDB
  ([demo 2](02-record-replay/README.md)). Record/replay is experimental and
  narrower than `hermit run`.
- **Turn a rare race into a reproducible test case**: chaos mode
  tries different thread orders from a seed, and the same seed always gives the
  same order ([demo 3](03-chaos-concurrency/README.md)).
- **Narrow a failure down to two events whose order decides it**:
  `hermit analyze` bisects between a passing and a failing schedule down to one
  pair of adjacent events on different threads
  ([demo 4](04-schedule-bisection/README.md)). In the demo's default mode Hermit
  switches threads only at system calls, so the pair it reports is two `mmap`
  calls the threads make while starting up, not the racing stores themselves.
  With hardware performance counters and `ANALYZE_MAX_TIMESLICE=400000` the pair
  moves into the program, and its stack traces point at the `do_work` calls
  right after the two racing stores.
- **Run a whole virtual machine deterministically**: QEMU booting Linux under
  Hermit gives a byte-identical snapshot of the booted machine
  ([demos 5](05-qemu-boot/README.md) and [6](06-qemu-resume/README.md)), and
  the same console output on every boot
  ([demo 9](09-qemu-busybox/README.md), which saves no snapshot).
- **Inspect a running guest kernel without disturbing it**, and see the same
  kernel state on every run ([demo 7](07-drgn-kernel/README.md)).
- **Catch a real use-after-free that normal runs expose only by chance**, and
  reproduce it on request ([demo 8](08-btrfs-convert-uaf/README.md)). On one
  316-thread host, seed 7 crashed it in all 44 counted runs of the demo's
  command, during which the host's load was not recorded, and in three later
  runs of the demo, with the 1-minute load average between 18.18 and 38.72 at
  their starts and ends. On a shared or heavily loaded machine the crash may
  not reproduce reliably: Hermit arms the retired-branch counter interrupt for
  each chaos-mode thread switch a safety margin early and single-steps to the
  exact point, and if the interrupt arrives later than that margin, which is
  more likely under heavy host load, Hermit prints `HERMIT_SKID_OVERSHOOT` and
  refuses the run (`HERMIT_POLICY_REFUSAL ... cause=skid-overshoot`, exit
  status 122) instead of reporting the crash.
- **Build Debian packages bit for bit reproducibly**: in a 58-package sample
  measured with Hermit commit `1fadc03779f2`, 52 built byte-identically from two
  different root directories and none differed; an earlier, larger run found
  real differences in a few packages
  ([writeup](../ai_docs/reproducible-builds-highlights.md)).

## The demos

| # | Demo | What it shows |
| --- | --- | --- |
| 1 | [Deterministic run](01-deterministic-run/README.md) | Random bytes, Python hashing, heap addresses, and a two-process race all repeat exactly, and so does wall-clock time when both runs pass the same `--epoch`; `--verify` checks it for you. |
| 2 | [Record and replay](02-record-replay/README.md) | Record a run, list the recording, replay it unattended, and replay it under GDB. |
| 3 | [Chaos concurrency testing](03-chaos-concurrency/README.md) | Seeded chaos scheduling finds a failing thread order in a racy program and replays it exactly. |
| 4 | [Schedule bisection](04-schedule-bisection/README.md) | `hermit analyze` narrows a failure down to two adjacent events whose order decides it and prints their stack traces: by default two `mmap` system calls at thread start-up; with performance counters and `ANALYZE_MAX_TIMESLICE=400000`, points just after the racing stores. |
| 5 | [Boot Linux in QEMU and save a snapshot](05-qemu-boot/README.md) | QEMU boots Linux under Hermit; a second boot started the same way, with the same Python interpreter, gives the same console output, snapshot file, and event log (the interpreter runs inside the guest, and one value QEMU reads, the set of ignored signals, still comes from the host: https://github.com/rrnewton/hermit/issues/3441). |
| 6 | [Resume the snapshot and run a command](06-qemu-resume/README.md) | Restore the demo 5 snapshot, run any shell command in the guest, and get identical output and an identical post-command snapshot. |
| 7 | [Watch the guest kernel's task list with drgn](07-drgn-kernel/README.md) | Read the guest kernel's task list with the drgn debugger before and after a fixed command, with the same result on every run. |
| 8 | [Find and reproduce a schedule-dependent use-after-free](08-btrfs-convert-uaf/README.md) | Chaos mode crashes a `btrfs-convert` build with a real 2015 race put back, the fixed build survives the same seed, and on the 316-thread host where it was measured the crash report repeated byte for byte (on a shared or heavily loaded machine the crash may not reproduce reliably). |
| 9 | [Boot BusyBox in QEMU, from a single script](09-qemu-busybox/README.md) | The smallest whole-machine demo: one script boots a kernel and BusyBox under `hermit run --strict`. |

> **Catching a use-after-free that normal runs expose only by chance (btrfs-progs).**
> `btrfs-convert` turns an ext4 file system into btrfs while a background
> thread prints progress. In 2015 upstream fixed a race in how that thread
> shuts down (btrfs-progs commit
> [73e211a7](https://github.com/kdave/btrfs-progs/commit/73e211a7a8ff3d2395783daaed71bf3792bd753f)):
> the main thread could free the progress thread's state while the thread was
> still reading it. We rebuilt btrfs-progs v7.1 with that race put back (the
> thread detached and never joined, as before the fix, but with the demo's own
> shutdown code) and compiled it with AddressSanitizer, which turns a read of
> freed memory into an immediate abort with a report. Run natively 40 times,
> the buggy binary never produced a complete report or a failing exit status:
> 29 runs showed nothing, and 11 printed the start of a report and then exited
> with status 0, because
> the main thread finished before the report did. Under `hermit run --chaos`,
> 5 of 32 scheduler seeds crashed it with a complete report, while the fixed
> binary showed no use-after-free on any of the same 32 seeds. On that host
> a crashing seed crashed again every time: seed 7 crashed the buggy binary in
> all 44 runs of the demo's command over more than three hours, from 21:08 UTC on
> 2026-09-30 to 00:19 UTC on 2026-10-01, and every report we kept from them
> was byte-identical, with the same heap address and the same stacks.
> (Measured with Hermit commit `dc92644f96f4` on 316 hardware threads. The
> host's load was not recorded during those runs; demo 8's README gives it for
> three later passing runs. The writeup has the full table. On a shared or
> heavily loaded machine the crash may not reproduce reliably; see demo 8's
> README.)
> On that host, a crash that native runs showed only in part, and never on
> request, became one command you can hand to a colleague. Both
> binaries carry the same small test harness. Among other things it skips
> btrfs-convert's check that its target is not mounted, because the mount
> table it reads under Hermit comes from the host's current mounts
> (<https://github.com/rrnewton/hermit/issues/1820>), and that check made the
> crashing seed depend on what else was mounted on the host. The demo finds its
> own crashing seed for your build. Read
> [the story of the bug](08-btrfs-convert-uaf/WRITEUP.md) (with the
> measurements and where they come from) or
> [run the demo](08-btrfs-convert-uaf/README.md).

> **Bit-for-bit reproducible Debian package builds.** We took 58 packages
> from the Debian 7 (Wheezy) set studied in the ASPLOS 2020 paper
> *Reproducible Containers* and built each one twice natively, from two
> different root directories. All 58 produced different `.deb` files. Under
> `hermit run --strict --no-rcb-time`, 52 of the 58 produced byte-identical
> `.deb` files from both roots, and none differed. The other 6 gave no result
> under Hermit: four did not finish in the time allowed, one needs a group ID
> that Hermit's container does not map, and one failed for a reason not yet
> explained. (Measured with Hermit commit `1fadc03779f2` on 2026-08-07.) A
> package counts only if its own native builds
> differed, so each package is its own control. Hermit removes the variation a
> build *sees*, such as timing, thread order, process IDs, randomness, and
> directory order. It deliberately does not hide a real difference, such as a
> build that writes its own path into its output. The main limit today is
> speed: Hermit runs a build's threads one at a time, so large parallel builds
> are slow. The 58 packages are a sample of quick builds, not a random one: an
> earlier, unfinished run on the 8,688 packages that the paper made
> reproducible attempted 46: 37 gave byte-identical `.deb` files; in 5, only
> the `.deb` archive timestamps differed, by a second; one shipped different
> bytes; two crashed; and one was skipped by the harness. Read
> [the writeup](../ai_docs/reproducible-builds-highlights.md)
> for the method, the per-package table, and that earlier run.

## Setup

Hermit supports x86-64 Linux. You need:

- Rust through [rustup](https://rustup.rs/). The repository's
  `rust-toolchain.toml` selects the right nightly toolchain automatically.
- A C and C++ compiler, `cmake`, and the libunwind and LZMA development
  packages:

  ```bash
  sudo apt-get install -y build-essential cmake libunwind-dev liblzma-dev   # Debian or Ubuntu
  sudo dnf install -y gcc gcc-c++ make cmake libunwind-devel xz-devel       # Fedora or CentOS
  ```

- Permission to use `ptrace` and user namespaces, which some container
  runtimes block.
- For the demos that say so, user-space access to the CPU's hardware
  performance counters. Most bare-metal Linux hosts allow it; many virtual
  machines and containers do not.

Build Hermit from this checkout and put it on your `PATH`. From the repository
root:

```bash
git submodule update --init --recursive
make release-core
export PATH="$PWD/target/release:$PATH"
hermit --version
```

`hermit --version` prints the version, build date, and commit, for example
`hermit 0.4.0 (2026-10-03, gb82e83b510a6)`. Every demo runs the `hermit` it
finds on your `PATH`.

Each demo's README lists anything else it needs. To check the QEMU demos'
requirements without running anything:

```bash
demos/lib/qemu-assets.sh --check
```

## Run the demos

Run every demo and get a pass, fail, or skip line for each:

```bash
make -C demos all
```

This is the same as `demos/run-all.sh --all`. It writes one log per demo and a
`summary.tsv` to `target/demo-sweep/`. Demo 8 is skipped, which counts as
neither a pass nor a failure, until you have run its `prepare-assets.sh`.
Demos 5 and 6 compare each run with a reference run that their first run
saves. The script sets `QEMU_BOOT_REPEAT=1` and `QEMU_RESUME_REPEAT=1` for the
demos it runs, whatever your environment says, so a demo with no reference run
yet saves one and then runs again to compare. A demo whose last result line
still says `FIRST RUN SAVED` compared nothing; it is recorded as `UNCOMPARED`,
which also counts as neither a pass nor a failure. The script exits 1 if any
demo fails. Otherwise it exits 4 if it could not create, write, or read its log
directory, a demo's log, or `summary.tsv`, because it reads each log to tell a
skip or an uncompared run from a pass, and 3 if at least one demo was skipped
or compared nothing, because such a demo produced no result. `make` reports
any of these as its own exit status 2. A caller that accepts skipped or
uncompared demos can check for exit status 3 itself; `demos/run-all.sh --help`
lists every exit status.

Smaller selections:

```bash
demos/run-all.sh                  # demos 1-3 only
demos/run-all.sh --with-qemu      # demos 1-3, 5, 6, and 9
make -C demos demo5               # one demo
make -C demos group2              # one of the groups below
```

The demos fall into three groups. Group 1 runs ordinary programs, group 3 the
QEMU/Linux snapshot demos, and group 2 the rest, including demo 9's BusyBox
boot. The groups are not balanced in length:

| Group | Demos | Needs, beyond Hermit | How long |
| --- | --- | --- | --- |
| 1 | 1, 2, 3 | `cargo`, a C compiler, `python3`; `gdb` for demo 2; performance counters for demo 1's last step | 31.6 s. Demos 1, 2, and 3 took 9, 6, and 16 s. The first run in a checkout also builds the test programs, which demo 1's README puts at about 20 s more. |
| 2 | 4, 8, 9 | Demo 4: `python3`, `timeout`. Demo 8: the assets from its `prepare-assets.sh` (a compiler with AddressSanitizer, btrfs-progs build tools, network access). Demo 9: QEMU, a static BusyBox, `cpio`, `gzip`, `curl`, performance counters | 172.1 s. Demos 4, 8, and 9 took 5, 9, and 158 s. The bounds are much longer: demo 4 gives up after 10 minutes and demo 8 allows each of its three Hermit runs 90 s. With `VERIFY=1`, demo 9 boots twice; the run in its README took 385 s at a host load average near 230, and an earlier one 335 s at about 40. |
| 3 | 5, 6, 7 | QEMU with `qemu-img`, `python3`, a static BusyBox, `cpio`, `gzip`, `curl`, `file`; performance counters for demo 5; `drgn`, `bpftool`, `gcc`, and `readelf` for demo 7 | 111.1 s. Demos 5, 6, and 7 took 71, 26, and 15 s, with demo 5's and demo 6's reference runs already saved. Without them, demo 5 boots twice (three such runs took 136.8 to 147.3 s) and demo 6 resumes twice (46.0 to 46.4 s). |

Measured on 2026-09-30 by running `make -C demos group1`, `group2`, and
`group3` one after another, with Hermit 0.2.0 `dc92644f96f4`, QEMU 10.1.2, and
demo 8's assets prepared, on a shared 316-CPU AMD EPYC host. All nine demos
passed. The group times are wall-clock times of the `make` commands; the
per-demo times are the whole seconds that `demos/run-all.sh` writes to
`summary.tsv`.

The demo scripts have unit tests that need no QEMU and no Hermit run:

```bash
make -C demos test
```

Remove computed results (logs, snapshots, run history) with
`make -C demos clean`; `make -C demos distclean` also removes the downloaded
kernel and the built initramfs. Both call `demos/clean.sh`, which accepts
`--dry-run` to show what it would remove.
